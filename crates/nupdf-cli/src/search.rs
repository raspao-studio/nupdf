// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! The `search` command: hits of a needle in the pages' structured text,
//! each with its quads and the content ids of its first and last
//! characters (DECISIONS D36).

use nupdf::Document;
use nupdf::extract;
use nupdf::geometry::Quad;
use nupdf::search::{self, Hit, Position};
use nupdf::stext::{self, Block, Options, StextPage};
use serde_json::{Map, Value, json};

/// What `search` was asked for.
pub struct Args<'a> {
    pub needle: &'a str,
    pub options: u32,
    pub stext: Options,
    pub pages: Option<&'a str>,
    pub max_items: Option<usize>,
    pub threads: Option<usize>,
}

fn number(v: f32) -> Value {
    if v.is_finite() {
        json!(f64::from(v))
    } else {
        Value::Null
    }
}

fn quad(q: Quad) -> Value {
    json!([
        number(q.ul.x),
        number(q.ul.y),
        number(q.ur.x),
        number(q.ur.y),
        number(q.ll.x),
        number(q.ll.y),
        number(q.lr.x),
        number(q.lr.y)
    ])
}

/// The content id of a character: its span and its offset in the span.
fn char_id(page: &StextPage, n: usize, at: Position) -> Option<(String, usize)> {
    let Block::Text(t) = page.blocks.get(at.block)? else {
        return None;
    };
    let line = t.lines.get(at.line)?;
    let (i, span) = extract::spans(line)
        .into_iter()
        .enumerate()
        .find(|(_, s)| s.start <= at.ch && at.ch < s.end)?;
    Some((
        format!("p{n}.b{}.l{}.s{}", at.block + 1, at.line + 1, i + 1),
        at.ch - span.start,
    ))
}

/// The text of a hit, from its first character to its last.
fn hit_text(page: &StextPage, hit: &Hit) -> String {
    let mut out = String::new();
    for (bi, block) in page.blocks.iter().enumerate() {
        let Block::Text(t) = block else { continue };
        for (li, line) in t.lines.iter().enumerate() {
            for (ci, ch) in line.chars.iter().enumerate() {
                let here = (bi, li, ci);
                let from = (hit.begin.block, hit.begin.line, hit.begin.ch);
                let to = (hit.end.block, hit.end.line, hit.end.ch);
                if here >= from && here <= to {
                    out.push(stext::rune_char(ch.c));
                }
            }
            if (bi, li) >= (hit.begin.block, hit.begin.line)
                && (bi, li) < (hit.end.block, hit.end.line)
            {
                out.push(' ');
            }
        }
    }
    out
}

fn hit_value(page: &StextPage, n: usize, hit: &Hit) -> Value {
    let mut m = Map::new();
    m.insert("page".into(), n.into());
    let begin = char_id(page, n, hit.begin);
    let end = char_id(page, n, hit.end);
    match (&begin, &end) {
        (Some((a, x)), Some((b, y))) if a == b => {
            m.insert("id".into(), format!("{a}:{x}-{}", y + 1).into());
        }
        _ => {
            if let Some((a, x)) = &begin {
                m.insert("begin".into(), format!("{a}:{x}").into());
            }
            if let Some((b, y)) = &end {
                m.insert("end".into(), format!("{b}:{y}").into());
            }
        }
    }
    m.insert("text".into(), hit_text(page, hit).into());
    m.insert(
        "quads".into(),
        hit.quads
            .iter()
            .map(|&q| quad(q))
            .collect::<Vec<_>>()
            .into(),
    );
    Value::Object(m)
}

/// The pages a capped search works on at a time: 4, then twice as many
/// each time up to 64, whatever the threads, so that the pages run, and
/// with them the warnings reported, are the same on every machine. Hits
/// on the first pages, the usual case, cost four pages; a needle far in
/// is soon found 64 pages at a time.
fn windows(indices: &[i32]) -> impl Iterator<Item = &[i32]> {
    let mut rest = indices;
    let mut size = 4;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (window, after) = rest.split_at(size.min(rest.len()));
        rest = after;
        size = (size * 2).min(64);
        Some(window)
    })
}

/// Searches the pages `args` asks for.
pub fn search(doc: &Document, args: &Args) -> Result<Value, String> {
    let page_count = doc.page_count();
    let count = page_count.clone().unwrap_or(0);
    let indices: Vec<i32> = match args.pages {
        Some(spec) => nupdf::rearrange::parse_page_ranges(spec, count, doc),
        None => (0..count).collect(),
    };
    let mut hits = Vec::new();
    let mut errors = Vec::new();
    let mut truncated = false;
    let hits_of = |n: usize, page: nupdf::parallel::PageResult| {
        page.and_then(|p| {
            search::search_page(&p, args.needle, args.options).map(|h| {
                // As many as can be kept: those past the cap are dropped.
                let keep = args.max_items.map_or(h.len(), |m| h.len().min(m + 1));
                h[..keep]
                    .iter()
                    .map(|hit| hit_value(&p, n, hit))
                    .collect::<Vec<_>>()
            })
        })
    };
    // A page's hits added to the report; false once the cap is passed.
    let mut take = |index: i32, page: Result<Vec<Value>, String>| {
        match page {
            Ok(page_hits) => {
                for hit in page_hits {
                    if args.max_items.is_some_and(|m| hits.len() >= m) {
                        truncated = true;
                        return false;
                    }
                    hits.push(hit);
                }
            }
            Err(e) => errors.push(json!({ "page": index + 1, "error": e })),
        }
        true
    };
    let unasked = crate::threads_for(None, indices.len());
    match (args.threads, args.max_items, unasked) {
        // All the pages on the threads asked for, or on those a long
        // document takes unasked.
        (Some(threads), _, _) | (None, None, Some(threads)) => {
            let found = nupdf::parallel::map_pages(doc, &indices, args.stext, threads, &|i, p| {
                hits_of(indices[i] as usize + 1, p)
            });
            for (index, page) in indices.iter().zip(found) {
                if !take(*index, page) {
                    break;
                }
            }
        }
        // A long document with a cap on the hits: windows of pages on
        // threads, stopping at the window that passes the cap. Page by
        // page, a needle whose first hit is far in took longer than
        // searching every page on threads does.
        (None, Some(_), Some(threads)) => {
            let job = |index: i32,
                       doc: &Document,
                       page: &Result<nupdf::Object, String>,
                       store: nupdf::content::Store| {
                let parts = nupdf::page_run::Parts::CONTENTS;
                let (r, store) = nupdf::parallel::run_page(doc, page, args.stext, store, parts);
                (hits_of(index as usize + 1, r), store)
            };
            nupdf::parallel::with_workers(doc, threads, &job, |run| {
                for window in windows(&indices) {
                    for (index, page) in window.iter().zip(run(window)) {
                        if !take(*index, page) {
                            return;
                        }
                    }
                }
            });
        }
        // A short one: pages past the last hit kept are not run, as in
        // MuPDF's loop.
        (None, _, None) => {
            let found = nupdf::parallel::sequential(doc, &indices, args.stext);
            for (index, page) in indices.iter().zip(found) {
                if !take(*index, hits_of(*index as usize + 1, page)) {
                    break;
                }
            }
        }
    }
    let mut body = Map::new();
    body.insert(
        "page_count".into(),
        page_count.map_or(Value::Null, |c| json!(c)),
    );
    body.insert("hit_count".into(), hits.len().into());
    body.insert("hits".into(), hits.into());
    body.insert("page_errors".into(), errors.into());
    body.insert("truncated".into(), truncated.into());
    Ok(Value::Object(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capped_search_takes_pages_in_growing_windows() {
        let pages: Vec<i32> = (0..300).collect();
        let sizes: Vec<usize> = windows(&pages).map(<[i32]>::len).collect();
        assert_eq!(sizes, [4, 8, 16, 32, 64, 64, 64, 48]);
        let again: Vec<i32> = windows(&pages).flatten().copied().collect();
        assert_eq!(again, pages);
        assert_eq!(
            windows(&pages[..3]).map(<[i32]>::len).collect::<Vec<_>>(),
            [3]
        );
        assert_eq!(windows(&[]).count(), 0);
    }
}
