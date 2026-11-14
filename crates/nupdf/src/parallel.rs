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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use crate::content::{SharedFonts, Store};
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

/// A page's structured text with `opts`, from the `parts` of it asked
/// for, on `store`, which it gives back.
pub fn run_page(
    doc: &Document,
    page: &Result<Object, String>,
    opts: Options,
    store: Store,
    parts: crate::page_run::Parts,
) -> (PageResult, Store) {
    match page {
        Ok(obj) => {
            let (result, store) = stext::extract_page_parts(doc, obj, opts, store, parts);
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
        let (r, next) = run_page(
            doc,
            &page_object(doc, i),
            opts,
            std::mem::take(&mut store),
            crate::page_run::Parts::CONTENTS,
        );
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
/// order. A repair during the run starts it again, one page after
/// another on the file opened afresh (DECISIONS D37).
pub fn run_pages<T: Send>(
    doc: &Document,
    indices: &[i32],
    threads: usize,
    job: &Job<'_, T>,
) -> Vec<T> {
    // The page tree is walked here, so its warnings come in order.
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

/// Runs every page with its diagnostics kept. Each thread keeps one store
/// for the pages it takes, as a run one after another keeps one for all,
/// and its results until it is done, so that finishing a page touches
/// nothing the other threads use.
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
    let mut done: Vec<Option<Done<T>>> = (0..pages.len()).map(|_| None).collect();
    if threads == 1 {
        let mut store = Store::default();
        for (i, slot) in done.iter_mut().enumerate() {
            let (r, next) = one(i, std::mem::take(&mut store));
            store = next;
            *slot = Some(r);
        }
    } else {
        let next = AtomicUsize::new(0);
        let fonts = SharedFonts::default();
        let interrupt = crate::interrupt::current();
        let worker = || {
            crate::interrupt::within(interrupt.clone(), || {
                let mut store = Store::sharing_fonts(&fonts);
                let mut mine = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= pages.len() || crate::interrupt::interrupted() {
                        break;
                    }
                    let (r, next_store) = one(i, std::mem::take(&mut store));
                    store = next_store;
                    mine.push((i, r));
                }
                mine
            })
        };
        std::thread::scope(|s| {
            let workers: Vec<_> = (0..threads)
                .filter_map(|_| {
                    std::thread::Builder::new()
                        .stack_size(STACK_SIZE)
                        .spawn_scoped(s, worker)
                        .ok()
                })
                .collect();
            // A thread that cannot start leaves its pages to the others.
            for w in workers {
                for (i, r) in w.join().unwrap_or_default() {
                    done[i] = Some(r);
                }
            }
        });
    }
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
    run_pages(doc, indices, threads, &|i, doc, page, store| {
        let (r, store) = run_page(doc, page, opts, store, crate::page_run::Parts::CONTENTS);
        (f(i, r), store)
    })
}

/// Reports `diagnostics` once each, in a fixed order.
fn report(doc: &Document, mut diagnostics: Vec<Diagnostic>) {
    diagnostics.sort_by(|a, b| (a.code, &a.message).cmp(&(b.code, &b.message)));
    diagnostics.dedup();
    doc.diagnostics().replay(diagnostics);
}

/// What a kept worker does with a page: given its index in the document,
/// the document, the page object (or the error finding it) and the
/// thread's store, which it gives back.
pub type PageJob<'j, T> =
    dyn Fn(i32, &Document, &Result<Object, String>, Store) -> (T, Store) + Sync + 'j;

/// The pages a round gives the workers, with their indices.
type Pages = Arc<Vec<(i32, Result<Object, String>)>>;

/// The round of pages the workers are on.
struct Round<T> {
    id: u64,
    pages: Pages,
    results: Vec<Option<Done<T>>>,
    /// Workers still on the round.
    busy: usize,
    stop: bool,
    /// Counts the rounds that repaired the document: a worker seeing it
    /// change starts its store afresh, as the caller does.
    repairs: u64,
    fonts: SharedFonts,
}

struct Shared<T> {
    round: Mutex<Round<T>>,
    /// Workers wait here for a round, or to stop.
    work: Condvar,
    /// The caller waits here for a round to be done.
    idle: Condvar,
    next: AtomicUsize,
}

impl<T> Shared<T> {
    fn lock(&self) -> MutexGuard<'_, Round<T>> {
        self.round.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `body`, given a way to run `job` on rounds of pages on up to `threads`
/// threads that are kept from one round to the next, each with its store:
/// a command that works on windows of pages in turn then neither starts
/// threads nor loads fonts again for each. Every round's results come in
/// page order, with diagnostics and repair handled as by [`run_pages`].
pub fn with_workers<T: Send, R>(
    doc: &Document,
    threads: usize,
    job: &PageJob<'_, T>,
    body: impl FnOnce(&mut dyn FnMut(&[i32]) -> Vec<T>) -> R,
) -> R {
    let shared = Shared {
        round: Mutex::new(Round {
            id: 0,
            pages: Arc::new(Vec::new()),
            results: Vec::new(),
            busy: 0,
            stop: false,
            repairs: 0,
            fonts: SharedFonts::default(),
        }),
        work: Condvar::new(),
        idle: Condvar::new(),
        next: AtomicUsize::new(0),
    };
    let shared = &shared;
    std::thread::scope(|s| {
        let mut workers = 0;
        if threads > 1 {
            for _ in 0..threads {
                let interrupt = crate::interrupt::current();
                let spawned = std::thread::Builder::new()
                    .stack_size(STACK_SIZE)
                    .spawn_scoped(s, move || {
                        crate::interrupt::within(interrupt, || worker(doc, shared, job));
                    });
                workers += usize::from(spawned.is_ok());
            }
        }
        let mut store = Store::default();
        let mut run = |indices: &[i32]| round(doc, shared, workers, job, &mut store, indices);
        let out = body(&mut run);
        shared.lock().stop = true;
        shared.work.notify_all();
        out
    })
}

/// A kept worker: each round, pages in turn until there are none left.
fn worker<T: Send>(doc: &Document, shared: &Shared<T>, job: &PageJob<'_, T>) {
    let mut store = Store::default();
    let mut seen = 0;
    let mut repairs = None;
    loop {
        let pages = {
            let mut r = shared.lock();
            while r.id == seen && !r.stop {
                r = shared.work.wait(r).unwrap_or_else(PoisonError::into_inner);
            }
            if r.stop {
                return;
            }
            seen = r.id;
            if repairs != Some(r.repairs) {
                repairs = Some(r.repairs);
                store = Store::sharing_fonts(&r.fonts);
            }
            Arc::clone(&r.pages)
        };
        let mut mine = Vec::new();
        loop {
            let i = shared.next.fetch_add(1, Ordering::Relaxed);
            let Some((index, page)) = pages.get(i) else {
                break;
            };
            // A page that panics is left for the caller to do again.
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                diag::capture(|| job(*index, doc, page, std::mem::take(&mut store)))
            }));
            if let Ok(((r, next), d)) = ran {
                store = next;
                mine.push((i, (r, d)));
            }
        }
        let mut r = shared.lock();
        for (i, done) in mine {
            if let Some(slot) = r.results.get_mut(i) {
                *slot = Some(done);
            }
        }
        r.busy = r.busy.saturating_sub(1);
        if r.busy == 0 {
            shared.idle.notify_all();
        }
    }
}

/// One round: the pages at `indices` on the workers (or here, with
/// `store`, when there are none).
fn round<T: Send>(
    doc: &Document,
    shared: &Shared<T>,
    workers: usize,
    job: &PageJob<'_, T>,
    store: &mut Store,
    indices: &[i32],
) -> Vec<T> {
    // The page tree is walked here, so its warnings come in order.
    let pages: Pages = Arc::new(indices.iter().map(|&i| (i, page_object(doc, i))).collect());
    let repaired = doc.repair_attempted();
    let mut done: Vec<Option<Done<T>>> = if workers == 0 {
        (0..pages.len()).map(|_| None).collect()
    } else {
        let mut r = shared.lock();
        r.pages = Arc::clone(&pages);
        r.results = (0..pages.len()).map(|_| None).collect();
        r.busy = workers;
        shared.next.store(0, Ordering::Relaxed);
        r.id += 1;
        shared.work.notify_all();
        while r.busy > 0 {
            r = shared.idle.wait(r).unwrap_or_else(PoisonError::into_inner);
        }
        std::mem::take(&mut r.results)
    };
    let mut results = Vec::with_capacity(pages.len());
    let mut diagnostics = Vec::new();
    for ((index, page), slot) in pages.iter().zip(done.iter_mut()) {
        let (r, d) = slot.take().unwrap_or_else(|| {
            let ((r, next), d) = diag::capture(|| job(*index, doc, page, std::mem::take(store)));
            *store = next;
            (r, d)
        });
        results.push(r);
        diagnostics.extend(d);
    }
    if !repaired && doc.repair_attempted() {
        // What the stores hold was loaded before the repair, by whichever
        // threads took the pages; from the next round on, all start afresh.
        *store = Store::default();
        let mut r = shared.lock();
        r.repairs += 1;
        r.fonts = SharedFonts::default();
    }
    if !repaired
        && doc.repair_attempted()
        && let Some(fresh) = doc.reopen()
    {
        drop(results);
        let ((results, mut diagnostics), tree) = diag::capture(|| {
            let mut store = Store::default();
            let mut results = Vec::with_capacity(indices.len());
            let mut diagnostics = Vec::new();
            for &index in indices {
                let page = page_object(&fresh, index);
                let ((r, next), d) =
                    diag::capture(|| job(index, &fresh, &page, std::mem::take(&mut store)));
                store = next;
                results.push(r);
                diagnostics.extend(d);
            }
            (results, diagnostics)
        });
        diagnostics.extend(tree);
        report(doc, diagnostics);
        return results;
    }
    report(doc, diagnostics);
    results
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

/// How deep a font dictionary written inside another object is looked for.
const MAX_FONT_NESTING: usize = 16;

/// The most glyph procedures looked through before giving up on telling.
const MAX_GLYPH_PROCS: usize = 4096;

/// Whether `doc`'s pages can be drawn each on its own, on threads, with
/// the pixels of drawing them one after another: whether no Type 3 font
/// draws with resources it takes from where it is first used.
///
/// A Type 3 font without /Resources borrows those in use where it is
/// loaded (`pdf_load_type3_font`), and stays loaded for the pages after,
/// so what its glyphs draw on one page can depend on which page came
/// before it. MuPDF draws such a document's pages differently in a run
/// than alone, and nupdf follows it; pages given out to threads as they
/// come free would draw it differently from run to run. Most such fonts
/// borrow nothing: their glyphs are paths and inline images, which name no
/// resource. Only one whose glyphs do (a form, an image, a graphics
/// state, a font, a colour space, a pattern or a shading by name) ties
/// the pages together.
///
/// Every object is looked at, in a copy of the document opened afresh, so
/// that nothing it loads or warns of shows in `doc`. False where it
/// cannot be told: a document made or changed in memory, a run that has
/// been interrupted, a glyph that does not parse.
pub fn pages_draw_apart(doc: &Document) -> bool {
    let Some(copy) = doc.reopen() else {
        return false;
    };
    let mut procs = 0;
    let (apart, _) = diag::capture(|| {
        for num in 1..copy.xref_len() {
            if num % 1024 == 0 && crate::interrupt::interrupted() {
                return false;
            }
            let Ok(num) = i32::try_from(num) else {
                return false;
            };
            if let Ok(Some(o)) = copy.load_object(num)
                && borrows(&copy, &o, 0, &mut procs)
            {
                return false;
            }
        }
        true
    });
    apart
}

/// Whether `o`, or a dictionary written inside it, is a Type 3 font
/// without resources of its own whose glyphs name one.
fn borrows(doc: &Document, o: &Object, depth: usize, procs: &mut usize) -> bool {
    let dict = match o {
        _ if depth > MAX_FONT_NESTING => return false,
        Object::Dict(d) => d,
        Object::Stream(s) => &s.dict,
        Object::Array(items) => return items.iter().any(|i| borrows(doc, i, depth + 1, procs)),
        _ => return false,
    };
    let type3 = dict.get(b"Subtype").is_some_and(|s| s.is_name(b"Type3"));
    let own = !matches!(dict.get(b"Resources"), None | Some(Object::Null));
    if type3 && !own {
        let Some(glyphs) = dict.get(b"CharProcs").map(|c| doc.resolve(c)) else {
            return false;
        };
        let Object::Dict(glyphs) = glyphs else {
            return false;
        };
        return glyphs.iter().any(|(_, glyph)| {
            *procs += 1;
            *procs > MAX_GLYPH_PROCS || names_a_resource(doc, glyph)
        });
    }
    dict.iter().any(|(_, v)| borrows(doc, v, depth + 1, procs))
}

/// Whether the glyph procedure `glyph` uses a resource by name, as far as
/// running it with no resources shows: an operator that takes one, or
/// anything that goes wrong.
fn names_a_resource(doc: &Document, glyph: &Object) -> bool {
    use crate::content::{Op, ProcState, Processor};

    #[derive(Default)]
    struct Probe {
        state: ProcState,
        named: bool,
    }

    impl Processor for Probe {
        fn op(&mut self, op: Op<'_>) -> Result<(), String> {
            let device = |name: &[u8]| {
                matches!(
                    name,
                    b"DeviceGray" | b"DeviceRGB" | b"DeviceCMYK" | b"G" | b"RGB" | b"CMYK"
                )
            };
            self.named |= match op {
                Op::GsBegin { .. }
                | Op::Font { .. }
                | Op::ColorPattern { .. }
                | Op::ColorShade { .. }
                | Op::Shade { .. }
                | Op::DrawImage { .. }
                | Op::DrawForm { .. } => true,
                Op::ColorSpace { name, .. } => !device(name) && name != b"Pattern",
                Op::InlineImage {
                    colorspace_name, ..
                } => colorspace_name.is_some_and(|n| !device(n)),
                Op::MarkedPointProperties { raw, .. } | Op::BeginMarkedProperties { raw, .. } => {
                    matches!(raw, Some(Object::Name(_)))
                }
                _ => false,
            };
            Ok(())
        }

        fn state(&mut self) -> &mut ProcState {
            &mut self.state
        }
    }

    if !matches!(doc.resolve(glyph), Object::Stream(_)) {
        return false;
    }
    let mut probe = Probe::default();
    let (ran, warnings) =
        diag::capture(|| crate::content::process_raw_contents(doc, glyph, &mut probe));
    probe.named || ran.is_err() || !warnings.is_empty()
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
    fn pages_draw_apart_unless_a_type3_font_borrows_its_resources() {
        // A document with a Type 3 font (4) whose one glyph is `proc`.
        let doc = |font_rest: &str, proc: &str| {
            let page =
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 9 9]/Resources<</Font<</F 4 0 R>>>>>>";
            let font = format!(
                "<</Type/Font/Subtype/Type3/FontMatrix[1 0 0 1 0 0]/FontBBox[0 0 1 1]/FirstChar 65\
                 /LastChar 65/Widths[1]/Encoding<</Differences[65/a]>>/CharProcs<</a 5 0 R>>{font_rest}>>"
            );
            let objects = [
                b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
                b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
                page.as_bytes().to_vec(),
                font.into_bytes(),
                crate::testutil::stream("", proc.as_bytes()),
            ];
            Document::open(crate::testutil::pdf_bytes(&objects)).unwrap()
        };
        // No Type 3 font at all.
        assert!(pages_draw_apart(&smoke("stext.pdf")));
        // Glyphs that name no resource: paths, colours, an inline image.
        for proc in [
            "1 0 0 0 1 1 d1 0 0 1 1 re f",
            "1 0 d0 1 0 0 rg /DeviceRGB cs 0 1 0 sc q 2 0 0 2 0 0 cm 0 0 m 1 1 l S Q",
            "1 0 d0 BI /W 1 /H 1 /BPC 8 /CS /G ID \x00 EI",
            "1 0 d0 /Span BMC 0 0 1 1 re f EMC /T <</K 1>> BDC EMC",
            "",
        ] {
            assert!(pages_draw_apart(&doc("", proc)), "{proc}");
        }
        // Glyphs that do, in a font without resources of its own: the
        // pages are tied together.
        for proc in [
            "1 0 d0 /X Do",
            "1 0 d0 /G0 gs 0 0 1 1 re f",
            "1 0 d0 /C0 cs 0 0 1 1 re f",
            "1 0 d0 /Pattern cs /P0 scn 0 0 1 1 re f",
            "1 0 d0 /S0 sh",
            "1 0 d0 BT /F0 1 Tf (a) Tj ET",
            "1 0 d0 BI /W 1 /H 1 /BPC 8 /CS /C0 ID \x00 EI",
            "1 0 d0 /OC /L0 BDC 0 0 1 1 re f EMC",
            "1 0 d0 0 0 1 1 re f nonsense",
        ] {
            assert!(!pages_draw_apart(&doc("", proc)), "{proc}");
            assert!(!pages_draw_apart(&doc("/Resources null", proc)), "{proc}");
            // With resources of its own the font takes none from a page.
            assert!(pages_draw_apart(&doc("/Resources<<>>", proc)), "{proc}");
        }
        // Nothing the look loads or warns of shows in the document.
        let quiet = doc("", "1 0 d0 /X Do nonsense");
        assert!(!pages_draw_apart(&quiet));
        assert!(quiet.diagnostics().snapshot().is_empty());
        // A document changed in memory has no file to look in.
        let edited = doc("/Resources<<>>", "");
        assert!(pages_draw_apart(&edited));
        edited.add_object(Object::Null);
        assert!(!pages_draw_apart(&edited));
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
    fn kept_workers_give_every_round_the_sequential_pages() {
        for name in [
            "stext.pdf",
            "multipage-inherit.pdf",
            "wrong-object-at-offset.pdf",
        ] {
            let doc = smoke(name);
            let count = doc.page_count().unwrap();
            let indices: Vec<i32> = (0..count).collect();
            let want = texts(&extract_sequential(&doc, &indices, Options::default()));
            // Rounds of every page, of each page alone, and of none.
            let mut rounds: Vec<Vec<i32>> = vec![indices.clone(), Vec::new()];
            rounds.extend(indices.iter().map(|&i| vec![i]));
            let mut reports = Vec::new();
            for threads in [1, 2, 8] {
                let doc = smoke(name);
                let job = |_, doc: &Document, page: &Result<Object, String>, store| {
                    run_page(
                        doc,
                        page,
                        Options::default(),
                        store,
                        crate::page_run::Parts::CONTENTS,
                    )
                };
                let got: Vec<Vec<PageResult>> = with_workers(&doc, threads, &job, |run| {
                    rounds.iter().map(|r| run(r)).collect()
                });
                assert_eq!(texts(&got[0]), want, "{name} on {threads} threads");
                assert!(got[1].is_empty());
                let alone: Vec<_> = got[2..].iter().flat_map(|r| texts(r)).collect();
                assert_eq!(alone, want, "{name} page by page on {threads} threads");
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
