// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! The `trace` command: each page's device calls, as MuPDF's script glue
//! hands them to a script device (oracle/js/trace.js).

use std::cell::{Cell, RefCell};

use nupdf::Document;
use nupdf::colorspace::ColorSpace;
use nupdf::content::Store;
use nupdf::device::{BLEND_NAMES, ColorParams, DefaultColorSpaces, Device, INTENT_NAMES, Metatext};
use nupdf::function::Function;
use nupdf::geometry::{Matrix, Rect};
use nupdf::image::Image;
use nupdf::path::{Path, StrokeState, Walker};
use nupdf::shade::Shade;
use nupdf::text::{Text, language_to_string};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use serde_json::{Value, json};

use crate::rows::{Fill, Rows, Sink};

/// Each page's calls, with its error if the run failed.
///
/// The report is written while the pages run ([`Rows`]): what it holds of
/// a page at any moment is one call.
pub struct Trace<'a> {
    doc: &'a Document,
    /// What the pages before left loaded, for the next.
    store: RefCell<Store>,
}

impl<'a> Trace<'a> {
    pub fn new(doc: &'a Document) -> Self {
        Trace {
            doc,
            store: RefCell::default(),
        }
    }
}

impl crate::rows::Body for Trace<'_> {
    fn serialize_into<M: SerializeMap>(&self, map: &mut M) -> Result<(), M::Error> {
        let count = self.doc.page_count();
        map.serialize_entry("page_count", &count.as_ref().ok())?;
        let pages = TracePages {
            of: self,
            count: count.unwrap_or(0),
        };
        map.serialize_entry("pages", &pages)
    }
}

struct TracePages<'a> {
    of: &'a Trace<'a>,
    count: i32,
}

impl Serialize for TracePages<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(None)?;
        for index in 0..self.count {
            seq.serialize_element(&TracePage { of: self.of, index })?;
        }
        seq.end()
    }
}

struct TracePage<'a> {
    of: &'a Trace<'a>,
    index: i32,
}

impl Serialize for TracePage<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let doc = self.of.doc;
        let mut map = s.serialize_map(None)?;
        let page = match doc.page(self.index) {
            Ok(p) if doc.resolve(&p).as_dict().is_some() => p,
            Ok(_) => {
                map.serialize_entry("error", "page object is not a dictionary")?;
                return map.end();
            }
            Err(e) => {
                map.serialize_entry("error", &e.0)?;
                return map.end();
            }
        };
        let error = Cell::new(None);
        let events = Rows::new(Run {
            of: self.of,
            page: &page,
            error: &error,
        });
        map.serialize_entry("events", &events)?;
        if let Some(line) = error.take() {
            map.serialize_entry("error", nupdf::diag::report::strip(&line))?;
        }
        map.end()
    }
}

/// A page's run, recording into the sink it is given.
struct Run<'a> {
    of: &'a Trace<'a>,
    page: &'a nupdf::Object,
    /// The error that ended the run, as a report line.
    error: &'a Cell<Option<String>>,
}

impl Fill for Run<'_> {
    fn fill<K: Sink>(self, sink: &mut K) {
        let doc = self.of.doc;
        let mut dev = Recorder { doc, sink };
        let store = self.of.store.take();
        let run = nupdf::run::run_page_contents(doc, self.page, &mut dev, store);
        self.of.store.replace(run.store);
        self.error.set(run.error);
    }
}

/// Records calls as oracle/js/trace.js prints them: one array per call,
/// its name then its arguments. Tiles are not recorded: the script
/// glue's tile callback breaks the script engine, so the oracle leaves it
/// out and MuPDF draws each pattern cell's content instead.
struct Recorder<'a, K> {
    doc: &'a Document,
    /// Takes each call as it is recorded.
    sink: &'a mut K,
}

fn number(v: f32) -> Value {
    if v.is_finite() {
        json!(f64::from(v))
    } else {
        Value::Null
    }
}

fn numbers(v: &[f32]) -> Value {
    Value::Array(v.iter().copied().map(number).collect())
}

fn matrix(m: Matrix) -> Value {
    numbers(&[m.a, m.b, m.c, m.d, m.e, m.f])
}

fn rect(r: Rect) -> Value {
    numbers(&[r.x0, r.y0, r.x1, r.y1])
}

/// The colorspace as the script sees it: the glue has no object for
/// MuPDF's device Lab space, so the script gets undefined for it.
fn script_cs(cs: Option<&ColorSpace>) -> Option<&ColorSpace> {
    cs.filter(|c| !matches!(c, ColorSpace::Lab))
}

/// A colorspace as its type and number of components.
fn colorspace(cs: Option<&ColorSpace>) -> Value {
    match script_cs(cs) {
        Some(cs) => json!([cs.type_name(), cs.n()]),
        None => Value::Null,
    }
}

/// The colorspace's components of `color`, or null without one.
fn color(cs: Option<&ColorSpace>, color: &[f32]) -> Value {
    match script_cs(cs) {
        Some(cs) => numbers(&color[..cs.n().min(color.len())]),
        None => Value::Null,
    }
}

fn params(p: ColorParams) -> Value {
    json!([INTENT_NAMES[usize::from(p.ri).min(3)], p.bp, p.op, p.opm])
}

fn stroke(s: &StrokeState) -> Value {
    let cap = ["Butt", "Round", "Square", "Triangle"];
    let join = ["Miter", "Round", "Bevel", "MiterXPS"];
    let pick = |names: &[&'static str], i: i32| {
        usize::try_from(i)
            .ok()
            .and_then(|i| names.get(i))
            .copied()
            .unwrap_or(names[0])
    };
    json!([
        pick(&cap, s.start_cap),
        pick(&join, s.linejoin),
        number(s.linewidth),
        number(s.miterlimit),
        number(s.dash_phase),
        if s.dash.is_empty() {
            Value::Null
        } else {
            numbers(&s.dash)
        },
    ])
}

/// The path as `Path.walk` reports it, written a segment at a time: a
/// path can have as many segments as its page has operators.
struct PathRows<'a>(&'a Path);

fn path(p: &Path) -> PathRows<'_> {
    PathRows(p)
}

impl Serialize for PathRows<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        struct Write<Q: SerializeSeq> {
            seq: Q,
            failed: Option<Q::Error>,
        }
        impl<Q: SerializeSeq> Write<Q> {
            fn push(&mut self, segment: impl Serialize) {
                if self.failed.is_none() {
                    self.failed = self.seq.serialize_element(&segment).err();
                }
            }
        }
        impl<Q: SerializeSeq> Walker for Write<Q> {
            fn move_to(&mut self, x: f32, y: f32) {
                self.push(("m", number(x), number(y)));
            }
            fn line_to(&mut self, x: f32, y: f32) {
                self.push(("l", number(x), number(y)));
            }
            fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) {
                let [x1, y1, x2, y2, x3, y3] = [x1, y1, x2, y2, x3, y3].map(number);
                self.push(("c", x1, y1, x2, y2, x3, y3));
            }
            fn close_path(&mut self) {
                self.push(("h",));
            }
        }
        let mut to = Write {
            seq: s.serialize_seq(None)?,
            failed: None,
        };
        self.0.walk(&mut to);
        match to.failed {
            Some(e) => Err(e),
            None => to.seq.end(),
        }
    }
}

/// An image as trace.js describes it.
fn image(img: &Image) -> Value {
    json!({
        "w": img.width,
        "h": img.height,
        "bpc": img.bpc,
        "n": img.n,
        "imagemask": img.image_mask,
        "cs": colorspace(img.colorspace.as_ref()),
        "interpolate": img.interpolate,
        "decode": img.decode.as_deref().map_or(Value::Null, numbers),
        "colorkey": img.colorkey.as_ref().map_or(Value::Null, |k| json!(k)),
        "mask": img.mask.as_deref().map_or(Value::Null, image),
    })
}

/// A C string as the script engine reads it: mujs decodes the bytes as
/// UTF-8, a byte that does not decode as U+FFFD.
pub(crate) fn script_string(bytes: &[u8]) -> String {
    script_str(bytes).into_owned()
}

/// [`script_string`], borrowed when the bytes before any NUL are ASCII,
/// as font names nearly always are: `stext` writes one per character.
pub(crate) fn script_str(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let head = &bytes[..end];
    if head.is_ascii()
        && let Ok(s) = std::str::from_utf8(head)
    {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::new();
    let mut s = bytes;
    while let Some(&c) = s.first() {
        if c == 0 {
            break;
        }
        let (rune, len) = if c < 0x80 {
            (u32::from(c), 1)
        } else {
            crate::objects::chartorune(s)
        };
        s = &s[len.min(s.len())..];
        out.push(char::from_u32(rune).unwrap_or('\u{fffd}'));
    }
    std::borrow::Cow::Owned(out)
}

/// Text as the script walks it: per span, the font's name, the matrix
/// without its translation, the writing mode, bidi level, markup
/// direction and language, then each glyph's id, Unicode and position.
/// Written a glyph at a time: one text object can hold every glyph of a
/// page.
struct TextRows<'a>(&'a Text);

fn text(t: &Text) -> TextRows<'_> {
    TextRows(t)
}

impl Serialize for TextRows<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        struct Glyphs<'a>(&'a [nupdf::text::TextItem]);
        impl Serialize for Glyphs<'_> {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_seq(
                    self.0
                        .iter()
                        .map(|g| (g.gid, g.ucs, number(g.x), number(g.y))),
                )
            }
        }
        s.collect_seq(self.0.spans.iter().map(|span| {
            (
                script_string(&span.font.name),
                numbers(&[span.trm.a, span.trm.b, span.trm.c, span.trm.d]),
                span.wmode,
                span.bidi_level,
                span.markup_dir,
                language_to_string(span.language),
                Glyphs(&span.items),
            )
        }))
    }
}

impl<K: Sink> Recorder<'_, K> {
    fn push(&mut self, call: impl Serialize) {
        self.sink.push(call);
    }
}

impl<K: Sink> Device for Recorder<'_, K> {
    fn fill_path(
        &mut self,
        p: &Path,
        even_odd: bool,
        ctm: Matrix,
        cs: &ColorSpace,
        c: &[f32],
        alpha: f32,
        pr: ColorParams,
    ) {
        self.push((
            "fill_path",
            path(p),
            even_odd,
            matrix(ctm),
            colorspace(Some(cs)),
            color(Some(cs), c),
            number(alpha),
            params(pr),
        ));
    }

    fn stroke_path(
        &mut self,
        p: &Path,
        s: &StrokeState,
        ctm: Matrix,
        cs: &ColorSpace,
        c: &[f32],
        alpha: f32,
        pr: ColorParams,
    ) {
        self.push((
            "stroke_path",
            path(p),
            stroke(s),
            matrix(ctm),
            colorspace(Some(cs)),
            color(Some(cs), c),
            number(alpha),
            params(pr),
        ));
    }

    fn clip_path(&mut self, p: &Path, even_odd: bool, ctm: Matrix, _scissor: Rect) {
        self.push(("clip_path", path(p), even_odd, matrix(ctm)));
    }

    fn clip_stroke_path(&mut self, p: &Path, s: &StrokeState, ctm: Matrix, _scissor: Rect) {
        self.push(("clip_stroke_path", path(p), stroke(s), matrix(ctm)));
    }

    fn fill_shade(&mut self, shade: &Shade, ctm: Matrix, alpha: f32, pr: ColorParams) {
        self.push(json!([
            "fill_shade",
            rect(shade.bound(ctm)),
            matrix(ctm),
            number(alpha),
            params(pr)
        ]));
    }

    fn fill_image(&mut self, img: &Image, ctm: Matrix, alpha: f32, pr: ColorParams) {
        self.push(json!([
            "fill_image",
            image(img),
            matrix(ctm),
            number(alpha),
            params(pr)
        ]));
    }

    fn fill_image_mask(
        &mut self,
        img: &Image,
        ctm: Matrix,
        cs: &ColorSpace,
        c: &[f32],
        alpha: f32,
        pr: ColorParams,
    ) {
        self.push(json!([
            "fill_image_mask",
            image(img),
            matrix(ctm),
            colorspace(Some(cs)),
            color(Some(cs), c),
            number(alpha),
            params(pr)
        ]));
    }

    fn clip_image_mask(&mut self, img: &Image, ctm: Matrix, _scissor: Rect) {
        self.push(json!(["clip_image_mask", image(img), matrix(ctm)]));
    }

    fn fill_text(
        &mut self,
        t: &Text,
        ctm: Matrix,
        cs: &ColorSpace,
        c: &[f32],
        alpha: f32,
        pr: ColorParams,
    ) {
        self.push((
            "fill_text",
            text(t),
            matrix(ctm),
            colorspace(Some(cs)),
            color(Some(cs), c),
            number(alpha),
            params(pr),
        ));
    }

    fn stroke_text(
        &mut self,
        t: &Text,
        s: &StrokeState,
        ctm: Matrix,
        cs: &ColorSpace,
        c: &[f32],
        alpha: f32,
        pr: ColorParams,
    ) {
        self.push((
            "stroke_text",
            text(t),
            stroke(s),
            matrix(ctm),
            colorspace(Some(cs)),
            color(Some(cs), c),
            number(alpha),
            params(pr),
        ));
    }

    fn clip_text(&mut self, t: &Text, ctm: Matrix, _scissor: Rect) {
        self.push(("clip_text", text(t), matrix(ctm)));
    }

    fn clip_stroke_text(&mut self, t: &Text, s: &StrokeState, ctm: Matrix, _scissor: Rect) {
        self.push(("clip_stroke_text", text(t), stroke(s), matrix(ctm)));
    }

    fn ignore_text(&mut self, t: &Text, ctm: Matrix) {
        self.push(("ignore_text", text(t), matrix(ctm)));
    }

    fn pop_clip(&mut self) {
        self.push(json!(["pop_clip"]));
    }

    fn begin_mask(
        &mut self,
        area: Rect,
        luminosity: bool,
        cs: Option<&ColorSpace>,
        backdrop: &[f32],
        pr: ColorParams,
    ) {
        self.push(json!([
            "begin_mask",
            rect(area),
            luminosity,
            colorspace(cs),
            color(cs, backdrop),
            params(pr)
        ]));
    }

    fn end_mask(&mut self, transfer: Option<&Function>) {
        if transfer.is_some() {
            self.doc
                .diagnostics()
                .warn("trace.transfer_function", "Ignoring Transfer function");
        }
        self.push(json!(["end_mask"]));
    }

    fn begin_group(
        &mut self,
        area: Rect,
        cs: Option<&ColorSpace>,
        isolated: bool,
        knockout: bool,
        blendmode: u8,
        alpha: f32,
    ) {
        self.push(json!([
            "begin_group",
            rect(area),
            colorspace(cs),
            isolated,
            knockout,
            BLEND_NAMES
                .get(usize::from(blendmode))
                .copied()
                .unwrap_or("Normal"),
            number(alpha)
        ]));
    }

    fn end_group(&mut self) {
        self.push(json!(["end_group"]));
    }

    fn set_default_colorspaces(&mut self, d: &DefaultColorSpaces) {
        let or = |cs: &Option<ColorSpace>, device: ColorSpace| {
            colorspace(Some(cs.as_ref().unwrap_or(&device)))
        };
        self.push(json!([
            "set_default_colorspaces",
            or(&d.gray, ColorSpace::Gray),
            or(&d.rgb, ColorSpace::Rgb),
            or(&d.cmyk, ColorSpace::Cmyk),
            colorspace(d.output_intent.as_ref())
        ]));
    }

    fn begin_layer(&mut self, name: &[u8]) {
        self.push(json!(["begin_layer", script_string(name)]));
    }

    fn end_layer(&mut self) {
        self.push(json!(["end_layer"]));
    }

    fn begin_structure(&mut self, standard: &'static str, raw: &[u8], index: i32) {
        self.push(json!([
            "begin_structure",
            standard,
            script_string(raw),
            index
        ]));
    }

    fn end_structure(&mut self) {
        self.push(json!(["end_structure"]));
    }

    fn begin_metatext(&mut self, meta: Metatext, text: &[u8]) {
        self.push(json!(["begin_metatext", meta.name(), script_string(text)]));
    }

    fn end_metatext(&mut self) {
        self.push(json!(["end_metatext"]));
    }
}

#[cfg(test)]
mod tests {
    /// The report's entries as a value.
    fn report(doc: &Document) -> Value {
        struct Whole<'a>(Trace<'a>);
        impl Serialize for Whole<'_> {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut map = s.serialize_map(None)?;
                crate::rows::Body::serialize_into(&self.0, &mut map)?;
                map.end()
            }
        }
        serde_json::to_value(Whole(Trace::new(doc))).unwrap_or_else(|e| panic!("{e}"))
    }

    use super::*;

    #[test]
    fn script_strings_stop_at_nul_and_replace_bad_bytes() {
        assert_eq!(script_string(b"ab\0cd"), "ab");
        assert_eq!(script_string(b"a\xffb"), "a\u{fffd}b");
        assert_eq!(script_string("é".as_bytes()), "é");
        assert!(matches!(
            script_str(b"Times-Roman\0x"),
            std::borrow::Cow::Borrowed("Times-Roman")
        ));
        assert_eq!(script_str(b"a\xffb"), "a\u{fffd}b");
    }

    #[test]
    fn a_page_without_contents_sets_its_defaults_only() {
        let doc = Document::open(
            &b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 10 10]>>endobj\n\
trailer<</Root 1 0 R/Size 4>>\n"[..],
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        let out = report(&doc);
        assert_eq!(
            out["pages"][0]["events"],
            json!([[
                "set_default_colorspaces",
                ["Gray", 1],
                ["RGB", 3],
                ["CMYK", 4],
                null
            ]])
        );
    }
}
