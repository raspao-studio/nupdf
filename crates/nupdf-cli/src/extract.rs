// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! The `extract` command: pages' structured text with content ids, as
//! JSON, JSON lines, plain text or Markdown, within a budget
//! (DECISIONS D35).

use nupdf::Document;
use nupdf::content::Store;
use nupdf::extract::{self as ex, Span};
use nupdf::geometry::{Point, Quad, Rect};
use nupdf::object::Object;
use nupdf::stext::{self, Block, Options, StextLine, StextPage};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

/// How `extract` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Json,
    Jsonl,
    Text,
    Markdown,
    /// Each page's text as it stands on the page, rows across and down
    /// (DECISIONS D51).
    Layout,
}

/// What `extract` was asked for.
pub struct Args<'a> {
    pub format: Format,
    pub pages: Option<&'a str>,
    pub options: Options,
    pub max_chars: Option<usize>,
    pub max_items: Option<usize>,
    pub cursor: Option<&'a str>,
    pub fields: Option<&'a str>,
    pub threads: Option<usize>,
    /// What of each page the text comes from: the contents, or for text
    /// as `mutool draw -F txt` writes it, annotations and widgets too.
    pub parts: nupdf::page_run::Parts,
    /// Whether the JSON is laid out again: pages written on the threads
    /// are then kept as values, not as compact text.
    pub pretty: bool,
}

/// What `extract` gives back: for JSON, the report's body; for JSON
/// lines, a line per page and the body for the last; for text and
/// Markdown, the text and the body (without pages) for stderr.
pub enum Extracted {
    Json(Body),
    Lines(Vec<PageOut>, Value),
    Text(String, Value),
}

/// The JSON report's body: the page count, the pages, then whether the
/// budget cut them short and where to go on.
pub struct Body {
    page_count: Value,
    pages: Vec<PageOut>,
    tail: Map<String, Value>,
}

/// A JSON report: `schema`, `doc_id`, the body, `diagnostics`.
pub struct Report<'a> {
    pub schema: &'a str,
    pub doc_id: &'a str,
    pub body: &'a Body,
    pub diagnostics: Value,
}

impl Serialize for Report<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("schema", self.schema)?;
        m.serialize_entry("doc_id", self.doc_id)?;
        m.serialize_entry("page_count", &self.body.page_count)?;
        m.serialize_entry("pages", &self.body.pages)?;
        for (k, v) in &self.body.tail {
            m.serialize_entry(k, v)?;
        }
        m.serialize_entry("diagnostics", &self.diagnostics)?;
        m.end()
    }
}

/// A JSON line: `schema`, `doc_id`, then the page's fields.
pub struct Line<'a> {
    pub schema: &'a str,
    pub doc_id: &'a str,
    pub page: &'a PageOut,
}

impl Serialize for Line<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("schema", self.schema)?;
        m.serialize_entry("doc_id", self.doc_id)?;
        self.page.entries(&mut m)?;
        m.end()
    }
}

/// A page of JSON output: its id and page-level fields, then its blocks
/// as they were written on the thread that extracted the page.
pub struct PageOut {
    head: Map<String, Value>,
    blocks: Option<Vec<Piece>>,
}

impl PageOut {
    fn entries<M: SerializeMap>(&self, m: &mut M) -> Result<(), M::Error> {
        for (k, v) in &self.head {
            m.serialize_entry(k, v)?;
        }
        if let Some(blocks) = &self.blocks {
            m.serialize_entry("blocks", blocks)?;
        }
        Ok(())
    }
}

impl Serialize for PageOut {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        self.entries(&mut m)?;
        m.end()
    }
}

/// What a block is written as: its text, or its JSON, compact or (for
/// output laid out again) as a value.
enum Piece {
    None,
    Text(String),
    Value(Value),
    Raw(Box<RawValue>),
}

impl Serialize for Piece {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Piece::None => s.serialize_unit(),
            Piece::Text(t) => s.serialize_str(t),
            Piece::Value(v) => v.serialize(s),
            Piece::Raw(r) => r.serialize(s),
        }
    }
}

/// A page made ready on the thread that extracted it, so that the page
/// loop only fits blocks into the budget: each block's length and output,
/// and for Markdown the page's Markdown with every block (for layout, the
/// page laid out).
struct Prepared {
    page: Result<StextPage, String>,
    blocks: Vec<(usize, Piece)>,
    markdown: String,
}

fn prepare(n: usize, page: Result<StextPage, String>, f: &Fields, args: &Args) -> Prepared {
    let mut blocks = Vec::new();
    let mut markdown = String::new();
    if let Ok(p) = &page {
        for (k, b) in p.blocks.iter().enumerate() {
            let piece = match args.format {
                Format::Text => Piece::Text(blocks_text(&[b])),
                Format::Markdown | Format::Layout => Piece::None,
                Format::Json | Format::Jsonl if f.depth == 0 => {
                    if f.has("text") {
                        Piece::Text(blocks_text(&[b]))
                    } else {
                        Piece::None
                    }
                }
                Format::Json | Format::Jsonl => {
                    let v = block_value(b, &format!("p{n}.b{}", k + 1), f);
                    if args.pretty {
                        Piece::Value(v)
                    } else {
                        serde_json::value::to_raw_value(&v).map_or(Piece::Value(v), Piece::Raw)
                    }
                }
            };
            blocks.push((ex::block_len(b), piece));
        }
        if args.format == Format::Markdown {
            let all: Vec<&Block> = p.blocks.iter().collect();
            markdown = ex::markdown(&all, ex::body_size(&all));
        } else if args.format == Format::Layout {
            markdown = ex::layout(p);
        }
    }
    Prepared {
        page,
        blocks,
        markdown,
    }
}

/// How many pages `--threads` works on ahead when a budget may stop it.
const WINDOW: usize = 64;

/// Every field, in the order levels nest.
const FIELDS: &[&str] = &[
    "page", "bbox", "blocks", "type", "lines", "wmode", "dir", "spans", "font", "size", "bold",
    "italic", "color", "text", "chars",
];

struct Fields {
    keep: Vec<&'static str>,
    /// The deepest level kept: 0 pages, 1 blocks, 2 lines, 3 spans.
    depth: u8,
}

impl Fields {
    fn parse(spec: Option<&str>) -> Result<Fields, String> {
        let keep: Vec<&'static str> = match spec {
            None => FIELDS.iter().copied().filter(|f| *f != "chars").collect(),
            Some(s) => {
                let mut keep = Vec::new();
                for f in s.split(',').map(str::trim).filter(|f| !f.is_empty()) {
                    let known = FIELDS
                        .iter()
                        .find(|k| **k == f)
                        .ok_or_else(|| format!("unknown field: {f}"))?;
                    keep.push(*known);
                }
                keep
            }
        };
        let has = |f: &str| keep.contains(&f);
        let depth = match (has("blocks"), has("lines"), has("spans")) {
            (false, false, false) => 0,
            (true, false, false) => 1,
            (true, true, false) => 2,
            (true, true, true) => 3,
            _ => return Err("fields: lines need blocks, and spans need lines".into()),
        };
        if has("chars") && depth < 3 {
            return Err("fields: chars need spans".into());
        }
        Ok(Fields { keep, depth })
    }

    fn has(&self, f: &str) -> bool {
        self.keep.contains(&f)
    }
}

fn number(v: f32) -> Value {
    if v.is_finite() {
        json!(f64::from(v))
    } else {
        Value::Null
    }
}

fn rect(r: Rect) -> Value {
    json!([number(r.x0), number(r.y0), number(r.x1), number(r.y1)])
}

fn point(p: Point) -> Value {
    json!([number(p.x), number(p.y)])
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

/// `#rrggbb`, with the alpha after when it is not opaque.
fn color(argb: u32) -> String {
    let a = argb >> 24;
    if a == 0xff {
        format!("#{:06x}", argb & 0xff_ffff)
    } else {
        format!("#{:06x}{a:02x}", argb & 0xff_ffff)
    }
}

/// A cursor: where the next call starts, bound to the document.
fn cursor(page: usize, block: Option<usize>, doc_id: &str) -> String {
    match block {
        Some(b) => format!("p{page}.b{b}@{doc_id}"),
        None => format!("p{page}@{doc_id}"),
    }
}

/// The page and block (1-based) a cursor names.
fn parse_cursor(c: &str, doc_id: &str) -> Result<(usize, usize), String> {
    let (id, doc) = c
        .split_once('@')
        .ok_or("a cursor ends in @ and the document's id")?;
    if doc != doc_id {
        return Err("the cursor is for another document".into());
    }
    let mut parts = id.split('.');
    let page = parts
        .next()
        .and_then(|p| p.strip_prefix('p'))
        .and_then(|p| p.parse().ok())
        .filter(|&p: &usize| p > 0)
        .ok_or("a cursor starts with a page id, such as p3")?;
    let block = match parts.next() {
        None => 1,
        Some(b) => b
            .strip_prefix('b')
            .and_then(|b| b.parse().ok())
            .filter(|&b: &usize| b > 0)
            .ok_or("a cursor's block id is b and a number, such as p3.b2")?,
    };
    if parts.next().is_some() {
        return Err("a cursor names a page or a block".into());
    }
    Ok((page, block))
}

fn span_value(line: &StextLine, span: &Span, id: &str, f: &Fields) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), id.into());
    if f.has("text") {
        m.insert(
            "text".into(),
            ex::chars_text(&line.chars[span.start..span.end]).into(),
        );
    }
    if f.has("font") {
        m.insert(
            "font".into(),
            String::from_utf8_lossy(&span.style.font)
                .into_owned()
                .into(),
        );
    }
    if f.has("size") {
        m.insert("size".into(), number(span.style.size));
    }
    if f.has("bold") {
        m.insert("bold".into(), span.style.bold.into());
    }
    if f.has("italic") {
        m.insert("italic".into(), span.style.italic.into());
    }
    if f.has("color") {
        m.insert("color".into(), color(span.style.argb).into());
    }
    if f.has("bbox") {
        m.insert("bbox".into(), rect(span.bbox));
    }
    if f.has("chars") {
        let chars: Vec<Value> = line.chars[span.start..span.end]
            .iter()
            .map(|c| json!([stext::rune_char(c.c).to_string(), quad(c.quad)]))
            .collect();
        m.insert("chars".into(), chars.into());
    }
    Value::Object(m)
}

fn line_value(line: &StextLine, id: &str, f: &Fields) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), id.into());
    if f.has("bbox") {
        m.insert("bbox".into(), rect(line.bbox));
    }
    if f.has("wmode") {
        m.insert("wmode".into(), line.wmode.into());
    }
    if f.has("dir") {
        m.insert("dir".into(), point(line.dir));
    }
    if f.depth == 2 && f.has("text") {
        m.insert("text".into(), ex::chars_text(&line.chars).into());
    }
    if f.depth >= 3 {
        let spans: Vec<Value> = ex::spans(line)
            .iter()
            .enumerate()
            .map(|(i, s)| span_value(line, s, &format!("{id}.s{}", i + 1), f))
            .collect();
        m.insert("spans".into(), spans.into());
    }
    Value::Object(m)
}

fn block_value(block: &Block, id: &str, f: &Fields) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), id.into());
    let (kind, bbox) = match block {
        Block::Text(t) => ("text", t.bbox),
        Block::Image(i) => ("image", i.bbox),
        Block::Vector(v) => ("vector", v.bbox),
        Block::Struct(s) => ("struct", s.bbox),
        Block::Grid(g) => ("grid", g.bbox),
    };
    if f.has("type") {
        m.insert("type".into(), kind.into());
    }
    if f.has("bbox") {
        m.insert("bbox".into(), rect(bbox));
    }
    if let Block::Struct(s) = block {
        if f.has("type") {
            m.insert("standard".into(), s.standard.into());
        }
        let blocks: Vec<Value> = s
            .blocks
            .iter()
            .enumerate()
            .map(|(i, b)| block_value(b, &format!("{id}.b{}", i + 1), f))
            .collect();
        m.insert("blocks".into(), blocks.into());
    }
    if let Block::Text(t) = block {
        if f.depth == 1 && f.has("text") {
            m.insert("text".into(), ex::block_text(t).into());
        }
        if f.depth >= 2 {
            let lines: Vec<Value> = t
                .lines
                .iter()
                .enumerate()
                .map(|(i, l)| line_value(l, &format!("{id}.l{}", i + 1), f))
                .collect();
            m.insert("lines".into(), lines.into());
        }
    }
    Value::Object(m)
}

/// The text of blocks, as `fz_print_stext_page_as_text` writes them.
fn blocks_text(blocks: &[&Block]) -> String {
    let mut out = String::new();
    for b in blocks {
        stext::blocks_text(std::slice::from_ref(*b), &mut out);
    }
    out
}

/// Extracts the pages `args` asks for.
pub fn extract(doc: &Document, doc_id: &str, args: &Args) -> Result<Extracted, String> {
    let fields = Fields::parse(args.fields)?;
    // Counted before any page loads: loading one can repair the document.
    let page_count = doc.page_count();
    let count = page_count.clone().unwrap_or(0);
    let indices: Vec<i32> = match args.pages {
        Some(spec) => nupdf::rearrange::parse_page_ranges(spec, count, doc),
        None => (0..count).collect(),
    };
    let (mut pos, mut first_block) = (0usize, 0usize);
    if let Some(c) = args.cursor {
        let (page, block) = parse_cursor(c, doc_id)?;
        pos = indices
            .iter()
            .position(|&i| i as usize + 1 == page)
            .ok_or("the cursor's page is not among the pages asked for")?;
        first_block = block - 1;
    }
    let ready = Ready {
        doc,
        doc_id,
        args,
        fields: &fields,
        indices: &indices,
        page_count,
    };
    match args.threads {
        Some(threads) => {
            // Each page is made ready on the thread that extracted it.
            let job = |index: i32, doc: &Document, page: &Result<Object, String>, store: Store| {
                let (r, store) =
                    nupdf::parallel::run_page(doc, page, args.options, store, args.parts);
                (prepare(index as usize + 1, r, &fields, args), store)
            };
            nupdf::parallel::with_workers(doc, threads, &job, |run| {
                walk(&ready, pos, first_block, Some(run))
            })
        }
        None => walk(&ready, pos, first_block, None),
    }
}

/// What the page loop works from.
struct Ready<'a> {
    doc: &'a Document,
    doc_id: &'a str,
    args: &'a Args<'a>,
    fields: &'a Fields,
    indices: &'a [i32],
    page_count: Result<i32, nupdf::PageCountError>,
}

/// Runs a window of pages on the kept threads.
type Windows<'a> = dyn FnMut(&[i32]) -> Vec<Prepared> + 'a;

/// The page loop: from the page at `pos` (and its block `first_block`),
/// each page in turn while the budget lasts. With `run`, pages are worked
/// on ahead in windows by it; else one after another, as MuPDF does.
fn walk(
    ready: &Ready,
    mut pos: usize,
    mut first_block: usize,
    mut run: Option<&mut Windows>,
) -> Result<Extracted, String> {
    let Ready {
        doc,
        doc_id,
        args,
        fields,
        indices,
        ..
    } = *ready;
    let page_count = ready.page_count.clone();
    // Text and Markdown go block by block, as JSON with blocks does; a
    // laid-out page is one piece, its rows crossing blocks.
    let by_block = match args.format {
        Format::Layout => false,
        Format::Text | Format::Markdown => true,
        Format::Json | Format::Jsonl => fields.depth >= 1,
    };
    let (mut chars, mut items) = (0usize, 0usize);
    let mut truncated = false;
    let mut next = None;
    let mut pages: Vec<PageOut> = Vec::new();
    let mut text = String::new();
    let mut errors = Vec::new();
    let mut store = Store::default();
    let mut ahead = std::collections::VecDeque::new();
    // Whether a unit of len characters still fits; the first always does.
    let fits = |chars: usize, items: usize, len: usize| {
        (chars == 0 && items == 0)
            || (args.max_chars.is_none_or(|m| chars + len <= m)
                && args.max_items.is_none_or(|m| items < m))
    };
    'pages: while let Some(&index) = indices.get(pos) {
        let n = index as usize + 1;
        let id = format!("p{n}");
        let skip = std::mem::take(&mut first_block);
        pos += 1;
        let prepared = if let Some(run) = run.as_mut() {
            // Pages are worked on ahead in windows of the same size at
            // every thread count, so the report does not depend on it.
            if ahead.is_empty() {
                // Without a budget every page is wanted, so all go at once.
                let size = if args.max_chars.is_some() || args.max_items.is_some() {
                    WINDOW
                } else {
                    indices.len()
                };
                let window = &indices[pos - 1..indices.len().min(pos - 1 + size)];
                ahead.extend(run(window));
            }
            ahead.pop_front().unwrap_or_else(|| Prepared {
                page: Err("page was not extracted".to_string()),
                blocks: Vec::new(),
                markdown: String::new(),
            })
        } else {
            let extracted = match doc.page(index) {
                Ok(obj) if doc.resolve(&obj).as_dict().is_none() => {
                    Err("page object is not a dictionary".to_string())
                }
                Ok(obj) => {
                    let (result, next_store) =
                        stext::extract_page_parts(doc, &obj, args.options, store, args.parts);
                    store = next_store;
                    result.map_err(|e| nupdf::diag::report::strip(&e).to_string())
                }
                Err(e) => Err(e.0),
            };
            prepare(n, extracted, fields, args)
        };
        let Prepared {
            page,
            blocks: mut pieces,
            markdown,
        } = prepared;
        let page: StextPage = match page {
            Ok(p) => p,
            Err(e) => {
                if !fits(chars, items, 0) {
                    truncated = true;
                    next = Some(cursor(n, None, doc_id));
                    break;
                }
                items += 1;
                errors.push(json!({ "page": n, "error": e }));
                let mut head = Map::new();
                head.insert("id".into(), id.into());
                head.insert("error".into(), e.into());
                pages.push(PageOut { head, blocks: None });
                continue;
            }
        };
        let all: Vec<&Block> = page.blocks.iter().collect();
        let mut taken: Vec<usize> = Vec::new();
        let mut complete = true;
        if by_block {
            for (k, (len, _)) in pieces.iter().enumerate().skip(skip) {
                if !fits(chars, items, *len) {
                    truncated = true;
                    complete = false;
                    next = Some(cursor(n, Some(k + 1), doc_id));
                    break;
                }
                chars += len;
                items += 1;
                taken.push(k);
            }
        } else {
            let len: usize = pieces.iter().map(|(len, _)| len).sum();
            if !fits(chars, items, len) {
                truncated = true;
                next = Some(cursor(n, None, doc_id));
                break 'pages;
            }
            chars += len;
            items += 1;
            taken = (0..pieces.len()).collect();
        }
        let mut take = |k: usize| std::mem::replace(&mut pieces[k].1, Piece::None);
        let joined = |take: &mut dyn FnMut(usize) -> Piece| {
            let mut out = String::new();
            for &k in &taken {
                if let Piece::Text(t) = take(k) {
                    out.push_str(&t);
                }
            }
            out
        };
        match args.format {
            Format::Text => {
                text.push_str(&joined(&mut take));
                if complete {
                    text.push_str("\u{c}\n");
                }
            }
            Format::Layout => {
                text.push_str(&markdown);
                text.push_str("\u{c}\n");
            }
            Format::Markdown => {
                text.push_str(&format!("<!-- {id} -->\n\n"));
                let md = if taken.len() == all.len() {
                    markdown
                } else {
                    let blocks: Vec<&Block> = taken.iter().map(|&k| all[k]).collect();
                    ex::markdown(&blocks, ex::body_size(&all))
                };
                if !md.is_empty() {
                    text.push_str(&md);
                    text.push_str("\n\n");
                }
            }
            Format::Json | Format::Jsonl if !taken.is_empty() || complete => {
                let mut head = Map::new();
                head.insert("id".into(), id.clone().into());
                if fields.has("page") {
                    head.insert("page".into(), n.into());
                }
                if fields.has("bbox") {
                    head.insert("bbox".into(), rect(page.mediabox));
                }
                let mut blocks = None;
                if fields.depth == 0 {
                    if fields.has("text") {
                        head.insert("text".into(), joined(&mut take).into());
                    }
                } else {
                    blocks = Some(taken.iter().map(|&k| take(k)).collect());
                }
                pages.push(PageOut { head, blocks });
            }
            Format::Json | Format::Jsonl => {}
        }
        if !complete {
            break;
        }
    }
    let page_count = page_count.map_or(Value::Null, |c| json!(c));
    let mut body = Map::new();
    body.insert("page_count".into(), page_count.clone());
    match args.format {
        Format::Json => {
            let mut tail = Map::new();
            tail.insert("truncated".into(), truncated.into());
            tail.insert("next_cursor".into(), next.map_or(Value::Null, Value::from));
            Ok(Extracted::Json(Body {
                page_count,
                pages,
                tail,
            }))
        }
        Format::Jsonl => {
            body.insert("truncated".into(), truncated.into());
            body.insert("next_cursor".into(), next.map_or(Value::Null, Value::from));
            body.insert("page_errors".into(), errors.into());
            Ok(Extracted::Lines(pages, Value::Object(body)))
        }
        Format::Text | Format::Markdown | Format::Layout => {
            body.insert("truncated".into(), truncated.into());
            body.insert("next_cursor".into(), next.map_or(Value::Null, Value::from));
            body.insert("page_errors".into(), errors.into());
            Ok(Extracted::Text(text, Value::Object(body)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_name_a_page_or_block_of_one_document() {
        assert_eq!(parse_cursor("p3.b2@abc", "abc"), Ok((3, 2)));
        assert_eq!(parse_cursor("p3@abc", "abc"), Ok((3, 1)));
        assert!(parse_cursor("p3.b2@abc", "xyz").is_err());
        assert!(parse_cursor("p0@abc", "abc").is_err());
        assert!(parse_cursor("p3.b2.l1@abc", "abc").is_err());
    }

    #[test]
    fn fields_nest_from_pages_down() {
        assert_eq!(Fields::parse(None).map(|f| f.depth), Ok(3));
        assert_eq!(Fields::parse(Some("text")).map(|f| f.depth), Ok(0));
        assert_eq!(Fields::parse(Some("blocks,text")).map(|f| f.depth), Ok(1));
        assert!(Fields::parse(Some("lines")).is_err());
        assert!(Fields::parse(Some("blocks,chars")).is_err());
        assert!(Fields::parse(Some("bogus")).is_err());
    }

    #[test]
    fn colors_are_hex_with_alpha_when_not_opaque() {
        assert_eq!(color(0xff00_80ff), "#0080ff");
        assert_eq!(color(0x8000_0000), "#00000080");
    }
}
