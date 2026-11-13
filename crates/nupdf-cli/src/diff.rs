// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf diff`: two PDFs compared by their text, their structure and their
//! pixels (DECISIONS D44).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use nupdf::content::Store;
use nupdf::geometry::Matrix;
use nupdf::stext::{Block, Options};
use nupdf::{Document, Object};
use serde_json::{Map, Value, json};

/// What to compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum By {
    Text,
    Structure,
    Pixels,
}

pub struct Args<'a> {
    pub by: &'a [By],
    pub options: Options,
    pub dpi: f32,
    /// Most differences reported, of each kind.
    pub max_items: usize,
}

// ---- edit scripts --------------------------------------------------------

/// One step of turning one sequence into another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// The items at these positions are the same.
    Same(usize, usize),
    /// The first sequence's item here is gone.
    Delete(usize),
    /// The second sequence's item here is new.
    Insert(usize),
}

/// The shortest edit script from `a` to `b` (Myers' O((N+M)D) algorithm),
/// or None when it takes more than `max_d` edits.
fn edits<T: PartialEq>(a: &[T], b: &[T], max_d: usize) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let limit = ((n + m) as usize).min(max_d) as isize;
    let offset = limit + 1;
    let mut v = vec![0isize; (2 * limit + 3) as usize];
    // trace[d]: v after step d, for k in -d..=d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let at = |v: &[isize], k: isize| v[(k + offset) as usize];
    for d in 0..=limit {
        for k in (-d..=d).step_by(2) {
            let mut x = if k == -d || (k != d && at(&v, k - 1) < at(&v, k + 1)) {
                at(&v, k + 1)
            } else {
                at(&v, k - 1) + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[(k + offset) as usize] = x;
            if x >= n && y >= m {
                return Some(back(&trace, d, n, m));
            }
        }
        trace.push(v[(offset - d) as usize..=(offset + d) as usize].to_vec());
    }
    None
}

/// The edit script from the trace of [`edits`], which ended at step `d`.
fn back(trace: &[Vec<isize>], d: isize, n: isize, m: isize) -> Vec<Op> {
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..=d).rev() {
        let v = &trace[(d - 1) as usize];
        let at = |k: isize| v[(k + d - 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            ops.push(Op::Same(x as usize, y as usize));
        }
        if x == prev_x {
            ops.push(Op::Insert((y - 1) as usize));
        } else {
            ops.push(Op::Delete((x - 1) as usize));
        }
        (x, y) = (prev_x, prev_y);
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        ops.push(Op::Same(x as usize, y as usize));
    }
    ops.reverse();
    ops
}

/// Most edits looked for between two documents' pages, or two pages' lines.
const MAX_EDITS: usize = 2000;

// ---- text ----------------------------------------------------------------

/// A line of a page's text and its content id.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    id: String,
    text: String,
}

/// A page's lines, ids as `extract` gives them.
fn lines(prefix: &str, blocks: &[Block], out: &mut Vec<Line>) {
    for (i, b) in blocks.iter().enumerate() {
        let id = format!("{prefix}.b{}", i + 1);
        match b {
            Block::Text(t) => {
                for (j, l) in t.lines.iter().enumerate() {
                    let text = nupdf::extract::chars_text(&l.chars);
                    if !text.trim().is_empty() {
                        out.push(Line {
                            id: format!("{id}.l{}", j + 1),
                            text,
                        });
                    }
                }
            }
            Block::Struct(s) => lines(&id, &s.blocks, out),
            _ => {}
        }
    }
}

/// What `diff` reads of a document's text: every page's lines (or the
/// error extracting it), and the fonts the text is drawn in.
struct Text {
    pages: Vec<Result<Vec<Line>, String>>,
    fonts: BTreeSet<String>,
}

fn fonts_in(blocks: &[Block], out: &mut BTreeSet<String>) {
    for b in blocks {
        match b {
            Block::Text(t) => {
                for c in t.lines.iter().flat_map(|l| &l.chars) {
                    let name = crate::trace::script_str(&c.font.name);
                    if !name.is_empty() && !out.contains(name.as_ref()) {
                        out.insert(name.into_owned());
                    }
                }
            }
            Block::Struct(s) => fonts_in(&s.blocks, out),
            _ => {}
        }
    }
}

fn read_text(doc: &Document, opts: Options) -> Text {
    let count = doc.page_count().unwrap_or(0);
    let indices: Vec<i32> = (0..count).collect();
    let mut fonts = BTreeSet::new();
    let pages = nupdf::parallel::extract(doc, &indices, opts, None)
        .into_iter()
        .enumerate()
        .map(|(i, page)| {
            page.map(|p| {
                fonts_in(&p.blocks, &mut fonts);
                let mut out = Vec::new();
                lines(&format!("p{}", i + 1), &p.blocks, &mut out);
                out
            })
        })
        .collect();
    Text { pages, fonts }
}

/// The lines' texts, which is what pages and lines are compared by.
fn texts(lines: &[Line]) -> Vec<&str> {
    lines.iter().map(|l| l.text.as_str()).collect()
}

/// Pages in `a` and `b`, paired: by the edit script over whole pages'
/// text, a run of pages gone and one of pages new taken as pages changed,
/// in order, and what is left as pages removed or added.
fn pair_pages(a: &[Vec<&str>], b: &[Vec<&str>]) -> Vec<(Option<usize>, Option<usize>)> {
    let ops = edits(a, b, MAX_EDITS).unwrap_or_else(|| {
        // Too different to line up: page by page.
        (0..a.len().max(b.len()))
            .flat_map(|i| {
                let mut v = Vec::new();
                if i < a.len() {
                    v.push(Op::Delete(i));
                }
                if i < b.len() {
                    v.push(Op::Insert(i));
                }
                v
            })
            .collect()
    });
    let mut out = Vec::new();
    let (mut gone, mut new) = (Vec::new(), Vec::new());
    let flush = |gone: &mut Vec<usize>, new: &mut Vec<usize>, out: &mut Vec<_>| {
        let n = gone.len().max(new.len());
        for k in 0..n {
            out.push((gone.get(k).copied(), new.get(k).copied()));
        }
        gone.clear();
        new.clear();
    };
    for op in ops {
        match op {
            Op::Same(..) => flush(&mut gone, &mut new, &mut out),
            Op::Delete(i) => gone.push(i),
            Op::Insert(j) => new.push(j),
        }
    }
    flush(&mut gone, &mut new, &mut out);
    out
}

fn compare_text(a: &Text, b: &Text, args: &Args) -> Value {
    let (pa, pb) = (&a.pages, &b.pages);
    let empty = Vec::new();
    let la: Vec<&Vec<Line>> = pa.iter().map(|p| p.as_ref().unwrap_or(&empty)).collect();
    let lb: Vec<&Vec<Line>> = pb.iter().map(|p| p.as_ref().unwrap_or(&empty)).collect();
    let ta: Vec<Vec<&str>> = la.iter().map(|l| texts(l)).collect();
    let tb: Vec<Vec<&str>> = lb.iter().map(|l| texts(l)).collect();
    let mut pages = Vec::new();
    let mut changes = 0usize;
    let mut truncated = false;
    let page_no = |i: Option<usize>| i.map_or(Value::Null, |i| (i + 1).into());
    for (i, j) in pair_pages(&ta, &tb) {
        if changes >= args.max_items {
            truncated = true;
            break;
        }
        let mut entry = json!({"a_page": page_no(i), "b_page": page_no(j)});
        match (i, j) {
            (Some(i), Some(j)) => {
                let (x, y) = (la[i], lb[j]);
                let mut list = Vec::new();
                match edits(&ta[i], &tb[j], MAX_EDITS) {
                    Some(ops) => {
                        for op in ops {
                            let change = match op {
                                Op::Same(..) => continue,
                                Op::Delete(k) => {
                                    json!({"op": "delete", "a": x[k].id, "text": x[k].text})
                                }
                                Op::Insert(k) => {
                                    json!({"op": "insert", "b": y[k].id, "text": y[k].text})
                                }
                            };
                            if changes >= args.max_items {
                                truncated = true;
                                break;
                            }
                            changes += 1;
                            list.push(change);
                        }
                    }
                    None => {
                        entry["replaced"] = true.into();
                        changes += 1;
                    }
                }
                entry["changes"] = list.into();
            }
            (Some(_), None) => {
                entry["op"] = "delete".into();
                changes += 1;
            }
            (None, Some(_)) => {
                entry["op"] = "insert".into();
                changes += 1;
            }
            (None, None) => continue,
        }
        pages.push(entry);
    }
    let errors = |p: &[Result<Vec<Line>, String>]| -> Vec<Value> {
        p.iter()
            .enumerate()
            .filter_map(|(i, r)| r.as_ref().err().map(|e| json!({"page": i + 1, "error": e})))
            .collect()
    };
    json!({
        "same": pages.is_empty(),
        "pages": pages,
        "truncated": truncated,
        "a_errors": errors(pa),
        "b_errors": errors(pb),
    })
}

// ---- structure -----------------------------------------------------------

/// What `diff` compares of a document's structure, each part named; the
/// fonts are those its text is drawn in.
fn structure(doc: &Document, text: &Text) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    let count = doc.page_count().unwrap_or(0);
    out.insert("page_count".into(), count.into());
    let inv = nupdf::inventory::inventory(doc);
    out.insert(
        "fonts".into(),
        text.fonts.iter().cloned().collect::<Vec<_>>().into(),
    );
    let mut images = vec![0u32; count.max(0) as usize];
    for i in &inv.images {
        if let Some(n) = usize::try_from(i.page - 1)
            .ok()
            .and_then(|p| images.get_mut(p))
        {
            *n += 1;
        }
    }
    for p in 0..count {
        let key = |what: &str| format!("page.{what}.{}", p + 1);
        let Ok(page) = doc.page(p) else {
            out.insert(key("error"), "cannot load the page".into());
            continue;
        };
        if let Ok((bounds, _)) = doc.page_transform(&page) {
            let r = |v: f32| (f64::from(v) * 100.0).round() / 100.0;
            out.insert(
                key("size"),
                json!([r(bounds.x1 - bounds.x0), r(bounds.y1 - bounds.y0)]),
            );
        }
        let rotate = doc
            .inherited(&page, b"Rotate")
            .ok()
            .flatten()
            .map_or(0, |r| doc.resolve(&r).to_int());
        out.insert(key("rotate"), rotate.into());
        let annots = match doc.dict_get(&page, b"Annots") {
            Object::Array(a) => a.len(),
            _ => 0,
        };
        out.insert(key("annotations"), annots.into());
        out.insert(
            key("images"),
            images.get(p as usize).copied().unwrap_or(0).into(),
        );
    }
    let trailer = doc.trailer();
    if let Some(Object::Dict(info)) = trailer.get(b"Info").map(|i| doc.resolve(i)) {
        for (k, v) in info.iter() {
            let value = match doc.resolve(v) {
                Object::String(s) => nupdf::text_string::text_string(&s).into(),
                other => nupdf::print::to_string(&other, nupdf::print::Options::default()).into(),
            };
            out.insert(format!("info.{}", String::from_utf8_lossy(k)), value);
        }
    }
    out.insert("outline".into(), outline(doc).into());
    let root = trailer.get(b"Root").cloned().unwrap_or(Object::Null);
    let acroform = doc.dict_get(&root, b"AcroForm");
    let fields = match doc.dict_get(&acroform, b"Fields") {
        Object::Array(a) => a.len(),
        _ => 0,
    };
    out.insert("form_fields".into(), fields.into());
    out
}

/// Most outline items read.
const MAX_OUTLINE: usize = 100_000;

/// The outline's titles, indented two spaces a level.
fn outline(doc: &Document) -> Vec<String> {
    let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
    let outlines = doc.dict_get(&root, b"Outlines");
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![(doc.dict_get(&outlines, b"First"), 0usize)];
    while let Some((item, depth)) = stack.pop() {
        if out.len() >= MAX_OUTLINE {
            break;
        }
        let Object::Ref(r) = item else { continue };
        if !seen.insert(r.num) {
            continue;
        }
        let node = Object::Ref(r);
        let title = match doc.dict_get(&node, b"Title") {
            Object::String(s) => nupdf::text_string::text_string(&s),
            _ => String::new(),
        };
        out.push(format!("{}{title}", "  ".repeat(depth)));
        // Next sibling below the first child, so children come first.
        stack.push((doc.dict_get(&node, b"Next"), depth));
        stack.push((doc.dict_get(&node, b"First"), depth + 1));
    }
    out
}

fn compare_structure(a: (&Document, &Text), b: (&Document, &Text), args: &Args) -> Value {
    let (sa, sb) = (structure(a.0, a.1), structure(b.0, b.1));
    let keys: BTreeSet<&String> = sa.keys().chain(sb.keys()).collect();
    let mut differences = Vec::new();
    for k in keys {
        let (x, y) = (sa.get(k), sb.get(k));
        if x == y {
            continue;
        }
        if differences.len() >= args.max_items {
            break;
        }
        let mut d = Map::new();
        d.insert("what".into(), k.clone().into());
        if let (Some(Value::Array(x)), Some(Value::Array(y))) = (x, y)
            && (k == "fonts" || k == "outline")
        {
            let only = |p: &[Value], q: &[Value]| -> Vec<Value> {
                p.iter().filter(|v| !q.contains(v)).cloned().collect()
            };
            d.insert("only_a".into(), only(x, y).into());
            d.insert("only_b".into(), only(y, x).into());
        } else {
            d.insert("a".into(), x.cloned().unwrap_or(Value::Null));
            d.insert("b".into(), y.cloned().unwrap_or(Value::Null));
        }
        differences.push(Value::Object(d));
    }
    json!({"same": differences.is_empty(), "differences": differences})
}

// ---- pixels --------------------------------------------------------------

/// Largest page drawn, in pixels.
const MAX_PIXELS: u64 = 1 << 26;

fn render(doc: &Document, index: i32, dpi: f32) -> Result<nupdf::draw::paint::Pix, String> {
    let page = doc.page(index).map_err(|e| e.0)?;
    let scale = dpi / 72.0;
    let (r, _) = nupdf::draw::render_page_with(
        doc,
        &page,
        Matrix::scale(scale, scale),
        MAX_PIXELS,
        None,
        Store::default(),
        true,
        nupdf::draw::Parts::ALL,
    );
    r.map(|r| r.pix)
}

fn compare_pixels(a: &Document, b: &Document, args: &Args) -> Value {
    let count = a.page_count().unwrap_or(0).min(b.page_count().unwrap_or(0));
    let scale = f64::from(args.dpi) / 72.0;
    let mut pages = Vec::new();
    for p in 0..count {
        if pages.len() >= args.max_items {
            break;
        }
        let (x, y) = match (render(a, p, args.dpi), render(b, p, args.dpi)) {
            (Ok(x), Ok(y)) => (x, y),
            (x, y) => {
                pages.push(json!({"page": p + 1, "a_error": x.err(), "b_error": y.err()}));
                continue;
            }
        };
        if (x.w, x.h, x.n) != (y.w, y.h, y.n) {
            pages.push(json!({"page": p + 1, "a_size": [x.w, x.h], "b_size": [y.w, y.h]}));
            continue;
        }
        let (w, n) = (x.w.max(0) as usize, x.n.max(1));
        let (mut differing, mut max_delta) = (0usize, 0u8);
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for (i, (pa, pb)) in x.samples.chunks(n).zip(y.samples.chunks(n)).enumerate() {
            let delta = pa
                .iter()
                .zip(pb)
                .map(|(u, v)| u.abs_diff(*v))
                .max()
                .unwrap_or(0);
            if delta > 0 {
                differing += 1;
                max_delta = max_delta.max(delta);
                let (px, py) = (i % w.max(1), i / w.max(1));
                (x0, y0, x1, y1) = (x0.min(px), y0.min(py), x1.max(px + 1), y1.max(py + 1));
            }
        }
        if differing == 0 {
            continue;
        }
        let total = (w * x.h.max(0) as usize).max(1);
        let pt = |v: usize, origin: i32| {
            ((v as f64 + f64::from(origin)) / scale * 100.0).round() / 100.0
        };
        pages.push(json!({
            "page": p + 1,
            "differing": differing,
            "share": (differing as f64 / total as f64 * 1e6).round() / 1e6,
            "max_delta": max_delta,
            "bbox": [pt(x0, x.x), pt(y0, x.y), pt(x1, x.x), pt(y1, x.y)],
        }));
    }
    json!({"same": pages.is_empty(), "dpi": args.dpi, "pages": pages})
}

/// The comparison's body: each kind asked for, or null.
pub fn diff(a: &Document, b: &Document, args: &Args) -> Value {
    let has = |k: By| args.by.contains(&k);
    let read = has(By::Text) || has(By::Structure);
    let (ta, tb) = if read {
        (read_text(a, args.options), read_text(b, args.options))
    } else {
        let none = || Text {
            pages: Vec::new(),
            fonts: BTreeSet::new(),
        };
        (none(), none())
    };
    let text = has(By::Text).then(|| compare_text(&ta, &tb, args));
    let structure = has(By::Structure).then(|| compare_structure((a, &ta), (b, &tb), args));
    let pixels = has(By::Pixels).then(|| compare_pixels(a, b, args));
    let same = [&text, &structure, &pixels]
        .iter()
        .all(|v| v.as_ref().is_none_or(|v| v["same"] == true));
    json!({
        "identical": same,
        "text": text,
        "structure": structure,
        "pixels": pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(a: &[&str], b: &[&str], ops: &[Op]) -> Vec<String> {
        // The script must give b from a: same items agree, every item of
        // each side appears once, in order.
        let (mut i, mut j) = (0, 0);
        let mut out = Vec::new();
        for op in ops {
            match *op {
                Op::Same(x, y) => {
                    assert_eq!((x, y), (i, j));
                    assert_eq!(a[x], b[y]);
                    out.push(b[y].to_string());
                    i += 1;
                    j += 1;
                }
                Op::Delete(x) => {
                    assert_eq!(x, i);
                    i += 1;
                }
                Op::Insert(y) => {
                    assert_eq!(y, j);
                    out.push(b[y].to_string());
                    j += 1;
                }
            }
        }
        assert_eq!((i, j), (a.len(), b.len()));
        out
    }

    #[test]
    fn edit_scripts_are_shortest_and_turn_one_into_the_other() {
        let cases: [(&[&str], &[&str], usize); 6] = [
            (&[], &[], 0),
            (&["a"], &[], 1),
            (&[], &["a", "b"], 2),
            (&["a", "b", "c"], &["a", "b", "c"], 0),
            (
                &["a", "b", "c", "a", "b", "b", "a"],
                &["c", "b", "a", "b", "a", "c"],
                5,
            ),
            (&["x", "a", "y"], &["a", "z"], 3),
        ];
        for (a, b, d) in cases {
            let ops = edits(a, b, 100).unwrap();
            assert_eq!(
                apply(a, b, &ops),
                b.iter().map(|s| s.to_string()).collect::<Vec<_>>()
            );
            let n = ops.iter().filter(|o| !matches!(o, Op::Same(..))).count();
            assert_eq!(n, d, "{a:?} {b:?}");
        }
        assert!(edits(&["a", "b"], &["c", "d"], 3).is_none());
    }

    #[test]
    fn pages_pair_up_around_those_that_stayed() {
        let p = |t: &'static str| vec![t];
        let a = [p("one"), p("two"), p("three"), p("four")];
        let b = [p("one"), p("new"), p("two 2"), p("three"), p("four")];
        // "two" changed to "two 2", "new" was inserted before it: the run
        // of one page gone and two new pairs the first, adds the second.
        assert_eq!(pair_pages(&a, &b), [(Some(1), Some(1)), (None, Some(2))]);
        assert_eq!(pair_pages(&a, &a), []);
        assert_eq!(pair_pages(&a, &a[..3]), [(Some(3), None)]);
    }
}
