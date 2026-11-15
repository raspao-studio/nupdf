// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! The `stext` command: each page's structured text, as MuPDF's script
//! glue walks it (oracle/js/stext.js).

use nupdf::Document;
use nupdf::geometry::{Point, Quad, Rect};
use nupdf::parallel::PageResult;
use nupdf::stext::{Block, Options, StextChar, StextLine, StructBlock};
use serde::ser::{SerializeMap, SerializeSeq, SerializeTuple};
use serde::{Serialize, Serializer};

/// The `stext` report's body. For compact output each page is written as
/// JSON on the thread that ran it, so that turning a long document into
/// text is not left to one thread at the end; output laid out again
/// (`--pretty`) keeps the pages to write them at the end.
pub struct Body {
    page_count: Option<i32>,
    pages: Pages,
}

enum Pages {
    /// Each page as compact JSON; `None` only if it could not be written,
    /// which a page of finite numbers and strings never fails to be.
    Written(Vec<Option<Vec<u8>>>),
    Kept(Vec<PageResult>),
}

impl Body {
    /// Writes the report as one line of compact JSON, the pages copied as
    /// they were written: `schema`, `doc_id`, the body, `diagnostics`.
    pub fn write_compact(
        &self,
        out: &mut impl std::io::Write,
        schema: &str,
        doc_id: &str,
        diagnostics: &serde_json::Value,
    ) -> std::io::Result<()> {
        write!(
            out,
            "{{\"schema\":{},\"doc_id\":{},\"page_count\":{},\"pages\":[",
            serde_json::to_string(schema)?,
            serde_json::to_string(doc_id)?,
            serde_json::to_string(&self.page_count)?,
        )?;
        match &self.pages {
            Pages::Written(pages) => {
                for (i, page) in pages.iter().enumerate() {
                    if i > 0 {
                        out.write_all(b",")?;
                    }
                    out.write_all(page.as_deref().unwrap_or(b"null"))?;
                }
            }
            Pages::Kept(pages) => {
                for (i, page) in pages.iter().enumerate() {
                    if i > 0 {
                        out.write_all(b",")?;
                    }
                    serde_json::to_writer(&mut *out, &PageJson(page))?;
                }
            }
        }
        write!(
            out,
            "],\"diagnostics\":{}}}",
            serde_json::to_string(diagnostics)?
        )
    }

    /// Writes the fields into a map being serialized.
    pub fn serialize_into<M: SerializeMap>(&self, map: &mut M) -> Result<(), M::Error> {
        map.serialize_entry("page_count", &self.page_count)?;
        match &self.pages {
            Pages::Kept(pages) => {
                let pages: Vec<_> = pages.iter().map(PageJson).collect();
                map.serialize_entry("pages", &pages)
            }
            Pages::Written(pages) => {
                let pages: Vec<_> = pages
                    .iter()
                    .map(|p| {
                        p.as_ref()
                            .and_then(|p| String::from_utf8(p.clone()).ok())
                            .and_then(|p| serde_json::value::RawValue::from_string(p).ok())
                    })
                    .collect();
                map.serialize_entry("pages", &pages)
            }
        }
    }
}

/// Each page's blocks, or the error that stopped its run; written as JSON
/// on the threads unless `keep`.
pub fn stext(doc: &Document, opts: Options, threads: Option<usize>, keep: bool) -> Body {
    let count = doc.page_count().ok();
    let indices: Vec<i32> = (0..count.unwrap_or(0)).collect();
    let threads = crate::threads_for(threads, indices.len());
    let pages = if keep {
        Pages::Kept(nupdf::parallel::map(
            doc,
            &indices,
            opts,
            threads,
            &|_, r| r,
        ))
    } else {
        Pages::Written(nupdf::parallel::map(
            doc,
            &indices,
            opts,
            threads,
            &|_, r| write_page(&r),
        ))
    };
    Body {
        page_count: count,
        pages,
    }
}

/// A page as compact JSON. Each thread writes into one buffer it keeps,
/// and the page is copied out at its size: growing a fresh buffer for
/// every page copied the larger pages over and over.
fn write_page(page: &PageResult) -> Option<Vec<u8>> {
    thread_local! {
        static BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    BUF.with(|buf| {
        let mut buf = buf.try_borrow_mut().ok()?;
        buf.clear();
        serde_json::to_writer(&mut *buf, &PageJson(page)).ok()?;
        Some(buf.as_slice().to_vec())
    })
}

/// A page as the glue gives it: `{"blocks": [...]}` or `{"error": ...}`.
struct PageJson<'a>(&'a PageResult);

impl Serialize for PageJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(1))?;
        match self.0 {
            Ok(page) => map.serialize_entry("blocks", &Blocks(&page.blocks))?,
            Err(e) => map.serialize_entry("error", e)?,
        }
        map.end()
    }
}

/// A coordinate: `null` if not finite, as JSON has no such numbers.
struct Number(f32);

impl Serialize for Number {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.0.is_finite() {
            s.serialize_f64(f64::from(self.0))
        } else {
            s.serialize_unit()
        }
    }
}

/// Numbers as a JSON array.
struct Numbers<const N: usize>([f32; N]);

impl<const N: usize> Serialize for Numbers<N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut t = s.serialize_tuple(N)?;
        for v in self.0 {
            t.serialize_element(&Number(v))?;
        }
        t.end()
    }
}

fn rect(r: Rect) -> Numbers<4> {
    Numbers([r.x0, r.y0, r.x1, r.y1])
}

fn point(p: Point) -> Numbers<2> {
    Numbers([p.x, p.y])
}

fn quad(q: Quad) -> Numbers<8> {
    Numbers([
        q.ul.x, q.ul.y, q.ur.x, q.ur.y, q.ll.x, q.ll.y, q.lr.x, q.lr.y,
    ])
}

/// A character as the glue's `fz_runetochar` and string push give it:
/// U+FFFD past Unicode (NUL is written overlong, so it stays).
fn char_text(c: i32) -> char {
    u32::try_from(c)
        .ok()
        .and_then(char::from_u32)
        .unwrap_or('\u{fffd}')
}

struct Blocks<'a>(&'a [Block]);

impl Serialize for Blocks<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // The glue's walk passes over grid blocks.
        let shown = self.0.iter().filter(|b| !matches!(b, Block::Grid(_)));
        let mut seq = s.serialize_seq(Some(shown.clone().count()))?;
        for b in shown {
            match b {
                Block::Text(t) => seq.serialize_element(&TextJson {
                    bbox: rect(t.bbox),
                    flags: t.flags,
                    lines: &t.lines,
                })?,
                Block::Image(i) => {
                    let m = i.transform;
                    seq.serialize_element(&ImageJson {
                        bbox: rect(i.bbox),
                        transform: Numbers([m.a, m.b, m.c, m.d, m.e, m.f]),
                    })?
                }
                Block::Vector(v) => seq.serialize_element(&VectorJson {
                    bbox: rect(v.bbox),
                    flags: v.flags,
                })?,
                Block::Struct(st) => seq.serialize_element(&StructJson(st))?,
                Block::Grid(_) => {}
            }
        }
        seq.end()
    }
}

struct TextJson<'a> {
    bbox: Numbers<4>,
    flags: i32,
    lines: &'a [StextLine],
}

impl Serialize for TextJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(4))?;
        map.serialize_entry("type", "text")?;
        map.serialize_entry("bbox", &self.bbox)?;
        map.serialize_entry("flags", &self.flags)?;
        let lines: Vec<_> = self.lines.iter().map(LineJson).collect();
        map.serialize_entry("lines", &lines)?;
        map.end()
    }
}

struct ImageJson {
    bbox: Numbers<4>,
    transform: Numbers<6>,
}

impl Serialize for ImageJson {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(3))?;
        map.serialize_entry("type", "image")?;
        map.serialize_entry("bbox", &self.bbox)?;
        map.serialize_entry("transform", &self.transform)?;
        map.end()
    }
}

/// A vector block as the glue gives it: its flags as one number (stroked
/// 1, rectangle 2, continues 4); its color is left out, as for characters.
struct VectorJson {
    bbox: Numbers<4>,
    flags: u32,
}

impl Serialize for VectorJson {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(3))?;
        map.serialize_entry("type", "vector")?;
        map.serialize_entry("bbox", &self.bbox)?;
        map.serialize_entry("flags", &self.flags)?;
        map.end()
    }
}

/// A structure block as the glue's `beginStruct` gives it: its standard
/// and raw types and its index, with the blocks within it.
struct StructJson<'a>(&'a StructBlock);

impl Serialize for StructJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let st = self.0;
        let mut map = s.serialize_map(Some(5))?;
        map.serialize_entry("type", "struct")?;
        map.serialize_entry("standard", st.standard)?;
        map.serialize_entry("raw", &st.raw)?;
        map.serialize_entry("index", &st.index)?;
        map.serialize_entry("blocks", &Blocks(&st.blocks))?;
        map.end()
    }
}

struct LineJson<'a>(&'a StextLine);

impl Serialize for LineJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let l = self.0;
        let mut map = s.serialize_map(Some(4))?;
        map.serialize_entry("bbox", &rect(l.bbox))?;
        map.serialize_entry("wmode", &l.wmode)?;
        map.serialize_entry("dir", &point(l.dir))?;
        let chars: Vec<_> = l.chars.iter().map(CharJson).collect();
        map.serialize_entry("chars", &chars)?;
        map.end()
    }
}

/// `[text, origin, font, size, quad, flags, bidi]`.
struct CharJson<'a>(&'a StextChar);

impl Serialize for CharJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let c = self.0;
        let mut t = s.serialize_tuple(7)?;
        t.serialize_element(&char_text(c.c))?;
        t.serialize_element(&point(c.origin))?;
        t.serialize_element(&*crate::trace::script_str(&c.font.name))?;
        t.serialize_element(&Number(c.size))?;
        t.serialize_element(&quad(c.quad))?;
        t.serialize_element(&c.flags)?;
        t.serialize_element(&c.bidi)?;
        t.end()
    }
}
