// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Pages worked on several threads, with results that do not depend on how
//! many (DECISIONS D37).
//!
//! Each thread keeps its own resource store, and each page's diagnostics
//! are kept apart while it runs. A document is shared by every thread, so a warning from
//! loading an object goes to whichever page loads it first; to keep the
//! report the same at every thread count, the pages' diagnostics are
//! reported once each, sorted by code and message. A page that makes the
//! document repair itself changes what the pages before it would have
//! read, so then every page is done again, one after another, on the file
//! opened afresh.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::content::Store;
use crate::diag::{self, Diagnostic};
use crate::document::Document;
use crate::object::Object;
use crate::stext::{self, Options, StextPage};

/// The stack each worker runs on; as the CLI's, since content streams nest
/// about a thousand forms deep.
pub const STACK_SIZE: usize = 1 << 30;

/// A page's structured text, or the error that stopped it, as the CLI
/// reports it.
pub type PageResult = Result<StextPage, String>;

/// What a page gave and the diagnostics it reported.
type Done<T> = (T, Vec<Diagnostic>);

/// The page object at `index`, ready to run.
fn page_object(doc: &Document, index: i32) -> Result<Object, String> {
    let obj = doc.page(index).map_err(|e| e.0)?;
    if doc.resolve(&obj).as_dict().is_none() {
        return Err("page object is not a dictionary".to_string());
    }
    Ok(obj)
}

fn run_page(
    doc: &Document,
    page: &Result<Object, String>,
    opts: Options,
    store: Store,
) -> (PageResult, Store) {
    match page {
        Ok(obj) => {
            let (result, store) = stext::extract_page(doc, obj, opts, store);
            (
                result.map_err(|e| crate::diag::report::strip(&e).to_string()),
                store,
            )
        }
        Err(e) => (Err(e.clone()), store),
    }
}

/// The pages at `indices`, one after another as they are asked for,
/// sharing one store and reporting as they go: MuPDF's order.
pub fn sequential<'a>(
    doc: &'a Document,
    indices: &'a [i32],
    opts: Options,
) -> impl Iterator<Item = PageResult> + 'a {
    let mut store = Store::default();
    indices.iter().map(move |&i| {
        let (r, next) = run_page(doc, &page_object(doc, i), opts, std::mem::take(&mut store));
        store = next;
        r
    })
}

/// [`sequential`], all at once.
pub fn extract_sequential(doc: &Document, indices: &[i32], opts: Options) -> Vec<PageResult> {
    sequential(doc, indices, opts).collect()
}

/// The pages at `indices` on up to `threads` threads, in page order.
pub fn extract_pages(
    doc: &Document,
    indices: &[i32],
    opts: Options,
    threads: usize,
) -> Vec<PageResult> {
    map_pages(doc, indices, opts, threads, &|_, r| r)
}

/// What a page job gets: its position among the pages, the document, the
/// page object (or the error finding it) and the thread's store, which it
/// gives back.
pub type Job<'j, T> =
    dyn Fn(usize, &Document, &Result<Object, String>, Store) -> (T, Store) + Sync + 'j;

/// `job` on each page at `indices`, on up to `threads` threads, in page
/// order, with diagnostics and repair handled as for [`map_pages`].
pub fn run_pages<T: Send>(
    doc: &Document,
    indices: &[i32],
    threads: usize,
    job: &Job<'_, T>,
) -> Vec<T> {
    let pages: Vec<Result<Object, String>> = indices.iter().map(|&i| page_object(doc, i)).collect();
    let repaired = doc.repair_attempted();
    let (results, diagnostics) = run_jobs(doc, &pages, threads, job);
    if !repaired
        && doc.repair_attempted()
        && let Some(fresh) = doc.reopen()
    {
        drop(results);
        let ((results, mut diagnostics), tree) = diag::capture(|| {
            let pages: Vec<_> = indices.iter().map(|&i| page_object(&fresh, i)).collect();
            run_jobs(&fresh, &pages, 1, job)
        });
        diagnostics.extend(tree);
        report(doc, diagnostics);
        return results;
    }
    report(doc, diagnostics);
    results
}

fn run_jobs<T: Send>(
    doc: &Document,
    pages: &[Result<Object, String>],
    threads: usize,
    job: &Job<'_, T>,
) -> (Vec<T>, Vec<Diagnostic>) {
    let one = |i: usize, store: Store| {
        let ((r, store), d) = diag::capture(|| job(i, doc, &pages[i], store));
        ((r, d), store)
    };
    let threads = threads.clamp(1, pages.len().max(1));
    let mut done: Vec<Option<Done<T>>> = if threads == 1 {
        let mut store = Store::default();
        (0..pages.len())
            .map(|i| {
                let (r, next) = one(i, std::mem::take(&mut store));
                store = next;
                Some(r)
            })
            .collect()
    } else {
        let next = AtomicUsize::new(0);
        let slots: Mutex<Vec<Option<Done<T>>>> =
            Mutex::new((0..pages.len()).map(|_| None).collect());
        std::thread::scope(|s| {
            for _ in 0..threads {
                let spawned = std::thread::Builder::new()
                    .stack_size(STACK_SIZE)
                    .spawn_scoped(s, || {
                        let mut store = Store::default();
                        loop {
                            let i = next.fetch_add(1, Ordering::SeqCst);
                            if i >= pages.len() {
                                break;
                            }
                            let (r, next_store) = one(i, std::mem::take(&mut store));
                            store = next_store;
                            if let Ok(mut slots) = slots.lock() {
                                slots[i] = Some(r);
                            }
                        }
                    });
                drop(spawned);
            }
        });
        slots.into_inner().unwrap_or_default()
    };
    let mut results = Vec::with_capacity(pages.len());
    let mut diagnostics = Vec::new();
    for (i, slot) in done.iter_mut().enumerate() {
        let (r, d) = slot.take().unwrap_or_else(|| one(i, Store::default()).0);
        results.push(r);
        diagnostics.extend(d);
    }
    (results, diagnostics)
}

/// `f` of each page at `indices` (its position among them and its
/// result), on up to `threads` threads, in page order. `f` runs on the
/// threads too, so the work of turning pages into output is shared.
pub fn map_pages<T: Send>(
    doc: &Document,
    indices: &[i32],
    opts: Options,
    threads: usize,
    f: &(dyn Fn(usize, PageResult) -> T + Sync),
) -> Vec<T> {
    // The page tree is walked here, so its warnings come in order.
    let pages: Vec<Result<Object, String>> = indices.iter().map(|&i| page_object(doc, i)).collect();
    let repaired = doc.repair_attempted();
    let (results, diagnostics) = run_all(doc, &pages, opts, threads, f);
    if !repaired
        && doc.repair_attempted()
        && let Some(fresh) = doc.reopen()
    {
        drop(results);
        let ((results, mut diagnostics), tree) = diag::capture(|| {
            let pages: Vec<_> = indices.iter().map(|&i| page_object(&fresh, i)).collect();
            run_all(&fresh, &pages, opts, 1, f)
        });
        diagnostics.extend(tree);
        report(doc, diagnostics);
        return results;
    }
    report(doc, diagnostics);
    results
}

/// Reports `diagnostics` once each, in a fixed order.
fn report(doc: &Document, mut diagnostics: Vec<Diagnostic>) {
    diagnostics.sort_by(|a, b| (a.code, &a.message).cmp(&(b.code, &b.message)));
    diagnostics.dedup();
    doc.diagnostics().replay(diagnostics);
}

/// Runs every page with its diagnostics kept. Each thread keeps one store
/// for the pages it takes, as a run one after another keeps one for all.
fn run_all<T: Send>(
    doc: &Document,
    pages: &[Result<Object, String>],
    opts: Options,
    threads: usize,
    f: &(dyn Fn(usize, PageResult) -> T + Sync),
) -> (Vec<T>, Vec<Diagnostic>) {
    let one = |i: usize, store: Store| {
        let ((r, store), d) = diag::capture(|| {
            let (r, store) = run_page(doc, &pages[i], opts, store);
            (f(i, r), store)
        });
        ((r, d), store)
    };
    let threads = threads.clamp(1, pages.len().max(1));
    let mut done: Vec<Option<Done<T>>> = if threads == 1 {
        let mut store = Store::default();
        (0..pages.len())
            .map(|i| {
                let (r, next) = one(i, std::mem::take(&mut store));
                store = next;
                Some(r)
            })
            .collect()
    } else {
        let next = AtomicUsize::new(0);
        let slots: Mutex<Vec<Option<Done<T>>>> =
            Mutex::new((0..pages.len()).map(|_| None).collect());
        std::thread::scope(|s| {
            for _ in 0..threads {
                let spawned = std::thread::Builder::new()
                    .stack_size(STACK_SIZE)
                    .spawn_scoped(s, || {
                        let mut store = Store::default();
                        loop {
                            let i = next.fetch_add(1, Ordering::SeqCst);
                            if i >= pages.len() {
                                break;
                            }
                            let (r, next_store) = one(i, std::mem::take(&mut store));
                            store = next_store;
                            if let Ok(mut slots) = slots.lock() {
                                slots[i] = Some(r);
                            }
                        }
                    });
                // A thread that cannot start leaves its pages to the others.
                drop(spawned);
            }
        });
        slots.into_inner().unwrap_or_default()
    };
    let mut results = Vec::with_capacity(pages.len());
    let mut diagnostics = Vec::new();
    for (i, slot) in done.iter_mut().enumerate() {
        // A page whose worker failed is done here.
        let (r, d) = slot.take().unwrap_or_else(|| one(i, Store::default()).0);
        results.push(r);
        diagnostics.extend(d);
    }
    (results, diagnostics)
}

/// The pages at `indices`: in MuPDF's way without `threads`, else on that
/// many threads, with the same results at every count.
pub fn extract(
    doc: &Document,
    indices: &[i32],
    opts: Options,
    threads: Option<usize>,
) -> Vec<PageResult> {
    map(doc, indices, opts, threads, &|_, r| r)
}

/// `f` of each page at `indices`, as [`extract`] runs them.
pub fn map<T: Send>(
    doc: &Document,
    indices: &[i32],
    opts: Options,
    threads: Option<usize>,
    f: &(dyn Fn(usize, PageResult) -> T + Sync),
) -> Vec<T> {
    match threads {
        None => sequential(doc, indices, opts)
            .enumerate()
            .map(|(i, r)| f(i, r))
            .collect(),
        Some(n) => map_pages(doc, indices, opts, n, f),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn smoke(name: &str) -> Document {
        let path = format!("{}/../../corpus/smoke/{name}", env!("CARGO_MANIFEST_DIR"));
        Document::open(std::fs::read(path).unwrap()).unwrap()
    }

    fn texts(results: &[PageResult]) -> Vec<Result<String, String>> {
        results
            .iter()
            .map(|r| r.as_ref().map(|p| p.text()).map_err(Clone::clone))
            .collect()
    }

    #[test]
    fn every_thread_count_gives_the_sequential_pages() {
        for name in [
            "stext.pdf",
            "multipage-inherit.pdf",
            "wrong-object-at-offset.pdf",
        ] {
            let doc = smoke(name);
            let indices: Vec<i32> = (0..doc.page_count().unwrap()).collect();
            let want = texts(&extract_sequential(&doc, &indices, Options::default()));
            let mut reports = Vec::new();
            for threads in [1, 2, 8] {
                let doc = smoke(name);
                let got = texts(&extract_pages(&doc, &indices, Options::default(), threads));
                assert_eq!(got, want, "{name} on {threads} threads");
                reports.push(doc.diagnostics().snapshot());
            }
            assert!(reports.windows(2).all(|w| w[0] == w[1]), "{name}");
        }
    }

    #[test]
    fn pages_out_of_range_are_errors_in_place() {
        let doc = smoke("stext.pdf");
        let got = extract_pages(&doc, &[0, 99, 1], Options::default(), 4);
        assert!(got[0].is_ok() && got[1].is_err() && got[2].is_ok());
    }
}
