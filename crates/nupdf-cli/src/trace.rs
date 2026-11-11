// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! The `trace` command: each page's device calls, as MuPDF's script glue
//! hands them to a script device (oracle/js/trace.js).

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
use serde_json::{Map, Value, json};

/// Each page's calls, with its error if the run failed.
pub fn trace(doc: &Document) -> Value {
    let mut out = Map::new();
    let count = doc.page_count();
    out.insert(
        "page_count".into(),
        count.as_ref().map_or(Value::Null, |n| json!(n)),
    );
    let mut pages = Vec::new();
    let mut store = Store::default();
    for index in 0..count.unwrap_or(0) {
        let page = match doc.page(index) {
            Ok(p) => p,
            Err(e) => {
                pages.push(json!({ "error": e.0 }));
                continue;
            }
        };
        if doc.resolve(&page).as_dict().is_none() {
            pages.push(json!({ "error": "page object is not a dictionary" }));
            continue;
        }
        let mut dev = Recorder {
            doc,
            events: Vec::new(),
        };
        let run = nupdf::run::run_page_contents(doc, &page, &mut dev, store);
        store = run.store;
        let mut entry = json!({ "events": dev.events });
        if let Some(line) = run.error {
            entry["error"] = json!(nupdf::diag::report::strip(&line));
        }
        pages.push(entry);
    }
    out.insert("pages".into(), Value::Array(pages));
    Value::Object(out)
}

/// Records calls as oracle/js/trace.js prints them: one array per call,
/// its name then its arguments. Tiles are not recorded: the script
/// glue's tile callback breaks the script engine, so the oracle leaves it
/// out and MuPDF draws each pattern cell's content instead.
struct Recorder<'a> {
    doc: &'a Document,
    events: Vec<Value>,
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

/// The path as `Path.walk` reports it.
fn path(p: &Path) -> Value {
    struct Collect(Vec<Value>);
    impl Walker for Collect {
        fn move_to(&mut self, x: f32, y: f32) {
            self.0.push(json!(["m", number(x), number(y)]));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.0.push(json!(["l", number(x), number(y)]));
        }
        fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) {
            let mut v = vec![json!("c")];
            v.extend([x1, y1, x2, y2, x3, y3].map(number));
            self.0.push(Value::Array(v));
        }
        fn close_path(&mut self) {
            self.0.push(json!(["h"]));
        }
    }
    let mut c = Collect(Vec::new());
    p.walk(&mut c);
    Value::Array(c.0)
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
fn script_string(bytes: &[u8]) -> String {
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
    out
}

/// Text as the script walks it: per span, the font's name, the matrix
/// without its translation, the writing mode, bidi level, markup
/// direction and language, then each glyph's id, Unicode and position.
fn text(t: &Text) -> Value {
    Value::Array(
        t.spans
            .iter()
            .map(|span| {
                let glyphs: Vec<Value> = span
                    .items
                    .iter()
                    .map(|g| json!([g.gid, g.ucs, number(g.x), number(g.y)]))
                    .collect();
                json!([
                    script_string(&span.font.name),
                    numbers(&[span.trm.a, span.trm.b, span.trm.c, span.trm.d]),
                    span.wmode,
                    span.bidi_level,
                    span.markup_dir,
                    language_to_string(span.language),
                    glyphs
                ])
            })
            .collect(),
    )
}

impl Recorder<'_> {
    fn push(&mut self, v: Value) {
        self.events.push(v);
    }
}

impl Device for Recorder<'_> {
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
        self.push(json!([
            "fill_path",
            path(p),
            even_odd,
            matrix(ctm),
            colorspace(Some(cs)),
            color(Some(cs), c),
            number(alpha),
            params(pr)
        ]));
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
        self.push(json!([
            "stroke_path",
            path(p),
            stroke(s),
            matrix(ctm),
            colorspace(Some(cs)),
            color(Some(cs), c),
            number(alpha),
            params(pr)
        ]));
    }

    fn clip_path(&mut self, p: &Path, even_odd: bool, ctm: Matrix, _scissor: Rect) {
        self.push(json!(["clip_path", path(p), even_odd, matrix(ctm)]));
    }

    fn clip_stroke_path(&mut self, p: &Path, s: &StrokeState, ctm: Matrix, _scissor: Rect) {
        self.push(json!(["clip_stroke_path", path(p), stroke(s), matrix(ctm)]));
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
        self.push(json!([
            "fill_text",
            text(t),
            matrix(ctm),
            colorspace(Some(cs)),
            color(Some(cs), c),
            number(alpha),
            params(pr)
        ]));
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
        self.push(json!([
            "stroke_text",
            text(t),
            stroke(s),
            matrix(ctm),
            colorspace(Some(cs)),
            color(Some(cs), c),
            number(alpha),
            params(pr)
        ]));
    }

    fn clip_text(&mut self, t: &Text, ctm: Matrix, _scissor: Rect) {
        self.push(json!(["clip_text", text(t), matrix(ctm)]));
    }

    fn clip_stroke_text(&mut self, t: &Text, s: &StrokeState, ctm: Matrix, _scissor: Rect) {
        self.push(json!(["clip_stroke_text", text(t), stroke(s), matrix(ctm)]));
    }

    fn ignore_text(&mut self, t: &Text, ctm: Matrix) {
        self.push(json!(["ignore_text", text(t), matrix(ctm)]));
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
    use super::*;

    #[test]
    fn script_strings_stop_at_nul_and_replace_bad_bytes() {
        assert_eq!(script_string(b"ab\0cd"), "ab");
        assert_eq!(script_string(b"a\xffb"), "a\u{fffd}b");
        assert_eq!(script_string("é".as_bytes()), "é");
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
        let out = trace(&doc);
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
