// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf objects`: every object as canonical JSON, in the encoding that
//! oracle/js/objects.js produces, so the two can be diffed object by object.

use nupdf::{Document, Object};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value, json};

use crate::rows::Rows;

/// The `objects` report body, serialized as it is written: each object is
/// loaded, encoded and written in turn, so memory does not grow with the
/// size of the report.
pub struct Dump<'a> {
    pub doc: &'a Document,
    pub count: usize,
}

impl<'a> Dump<'a> {
    /// The count is taken here, before any object is loaded, as the oracle
    /// does; objects are then loaded in ascending order, so a lazy repair
    /// happens at the same point.
    pub fn new(doc: &'a Document) -> Self {
        Dump {
            doc,
            count: doc.xref_len(),
        }
    }
}

impl Serialize for Dump<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.count.saturating_sub(1)))?;
        for num in 1..self.count {
            let object = self.doc.object(num as i32);
            if let Some(o) = &object {
                touch_references(self.doc, o);
            }
            let value = match object {
                Some(Object::Stream(s)) => {
                    // Null when the data cannot be read (a broken AES stream),
                    // as in the oracle.
                    let raw_len = self.doc.raw_stream_data(&s).ok().map(|raw| raw.len());
                    json!({ "stream": encode_dict(&s.dict), "raw_len": raw_len })
                }
                Some(obj) => encode(&obj),
                None => Value::Null,
            };
            map.serialize_entry(&num.to_string(), &value)?;
        }
        map.end()
    }
}

/// Loads every object `obj` refers to, in the order oracle/js/objects.js
/// meets them: in mutool's JavaScript, looking up a method on an indirect
/// object resolves it, so encoding an object loads its references (without
/// descending into them). Loading order matters: a failed object stream is
/// demoted, and a lazy repair changes what later objects look like.
fn touch_references(doc: &Document, obj: &Object) {
    match obj {
        Object::Ref(r) => {
            let _ = doc.object(r.num);
        }
        Object::Array(items) => items.iter().for_each(|o| touch_references(doc, o)),
        Object::Dict(d) => d.iter().for_each(|(_, v)| touch_references(doc, v)),
        Object::Stream(s) => s.dict.iter().for_each(|(_, v)| touch_references(doc, v)),
        _ => {}
    }
}

/// Every stream decoded for non-image use, in oracle/js/streams.js's
/// format: the decoded length and BLAKE3 hash, or the error. Every object is
/// loaded in ascending order, as the oracle's `isStream` lookups do.
///
/// With `images`, only streams with an image filter are decoded and listed,
/// in oracle/js/images.js's format.
pub fn streams(doc: &Document, images: bool) -> Value {
    let count = doc.xref_len();
    let mut out = Map::new();
    for num in 1..count {
        let Some(Object::Stream(s)) = doc.object(num as i32) else {
            continue;
        };
        let image = has_image_filter(doc, &s.dict);
        if image != images {
            if image {
                out.insert(num.to_string(), json!({ "image": true }));
            }
            continue;
        }
        let entry = match doc.stream_data(&s) {
            Ok(data) => json!({
                "len": data.len(),
                "blake3": blake3::hash(&data).to_hex().to_string(),
            }),
            Err(e) => json!({ "error": e.to_string() }),
        };
        out.insert(num.to_string(), entry);
    }
    json!({ "object_count": count, "streams": out })
}

/// Whether the filter chain (/F, else /Filter) names an image filter.
fn has_image_filter(doc: &Document, dict: &nupdf::Dict) -> bool {
    const IMAGE: [&[u8]; 6] = [
        b"CCITTFaxDecode",
        b"CCF",
        b"DCTDecode",
        b"DCT",
        b"JBIG2Decode",
        b"JPXDecode",
    ];
    let is_image = |o: &Object| o.as_name().is_some_and(|n| IMAGE.contains(&n));
    match dict
        .get(b"F")
        .or_else(|| dict.get(b"Filter"))
        .map(|f| doc.resolve(f))
    {
        Some(Object::Array(items)) => items.iter().any(|f| is_image(&doc.resolve(f))),
        Some(f) => is_image(&f),
        None => false,
    }
}

/// Pages and their inherited attributes, in oracle/js/pages.js's format.
pub fn pages(doc: &Document) -> Value {
    let mut out = Map::new();
    let count = doc.page_count();
    out.insert(
        "page_count".into(),
        count.as_ref().map_or(Value::Null, |n| json!(n)),
    );
    if let Err(e) = &count {
        out.insert("page_count_error".into(), json!(e.0));
    }
    let mut pages = Vec::new();
    for index in 0..count.unwrap_or(0) {
        let entry = doc.page(index).and_then(|page| {
            // Same order as the oracle: the first failing lookup wins, and a
            // parent repair triggered by one lookup applies to the next.
            let attr = |key: &[u8]| -> Result<Value, nupdf::PageError> {
                Ok(doc
                    .inherited(&page, key)?
                    .map_or(Value::Null, |v| encode(&v)))
            };
            let media = attr(b"MediaBox")?;
            let crop = attr(b"CropBox")?;
            let rotate = attr(b"Rotate")?;
            let resources = summarize(doc.inherited(&page, b"Resources")?);
            Ok(json!({
                "object": match &page {
                    Object::Ref(r) => format!("{} {} R", r.num, r.generation),
                    _ => "direct".to_string(),
                },
                "MediaBox": media,
                "CropBox": crop,
                "Rotate": rotate,
                "Resources": resources,
            }))
        });
        pages.push(entry.unwrap_or_else(|e| json!({ "error": e.0 })));
    }
    out.insert("pages".into(), Value::Array(pages));
    out.insert(
        "page_count_after".into(),
        doc.page_count().map_or(Value::Null, |n| json!(n)),
    );
    Value::Object(out)
}

/// A reference, the sorted keys of a dictionary, or the encoded value.
fn summarize(value: Option<Object>) -> Value {
    match value {
        None | Some(Object::Null) => Value::Null,
        Some(Object::Dict(d)) => {
            let mut keys: Vec<String> = d.iter().map(|(k, _)| escape_name(k)).collect();
            keys.sort();
            json!({ "keys": keys })
        }
        Some(other) => encode(&other),
    }
}

pub fn encode(obj: &Object) -> Value {
    match obj {
        Object::Null => Value::Null,
        Object::Bool(b) => Value::Bool(*b),
        Object::Int(i) => json!(i),
        Object::Real(f) => json!({ "r": f64::from(*f) }),
        Object::Name(n) => json!({ "n": escape_name(n) }),
        Object::String(s) => json!({ "s": hex(s) }),
        Object::Array(items) => Value::Array(items.iter().map(encode).collect()),
        Object::Dict(d) => encode_dict(d),
        Object::Ref(r) => json!({ "ref": format!("{} {} R", r.num, r.generation) }),
        Object::Stream(s) => encode_dict(&s.dict),
    }
}

fn encode_dict(dict: &nupdf::Dict) -> Value {
    let map: Map<String, Value> = dict
        .iter()
        .map(|(k, v)| (escape_name(k), encode(v)))
        .collect();
    json!({ "d": map })
}

/// A name as MuPDF prints it (`fmt_name`), without the slash: delimiters,
/// whitespace, '#' and non-printable bytes become `#XX`. Printing stops at a
/// NUL byte, as it does for C strings.
pub fn escape_name(name: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(name.len());
    for &b in name.iter().take_while(|&&b| b != 0) {
        let special = matches!(
            b,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%' | b'#'
        ) || nupdf::lex::is_white(b)
            || !(32..127).contains(&b);
        if special {
            let _ = write!(out, "#{b:02X}");
        } else {
            out.push(b as char);
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Each page's operators, as MuPDF's interpreter hands them to a
/// processor, in oracle/js/operators.js's format. With `bare`, each page
/// is processed with empty resources and only its content streams count.
///
/// The report is written while the pages are processed ([`Rows`]): what it
/// holds of a page at any moment is one operator.
pub struct Operators<'a> {
    pub doc: &'a Document,
    pub bare: bool,
}

impl crate::rows::Body for Operators<'_> {
    fn serialize_into<M: SerializeMap>(&self, map: &mut M) -> Result<(), M::Error> {
        let count = self.doc.page_count();
        map.serialize_entry("page_count", &count.as_ref().ok())?;
        let pages = OperatorPages {
            of: self,
            count: count.unwrap_or(0),
        };
        map.serialize_entry("pages", &pages)
    }
}

struct OperatorPages<'a> {
    of: &'a Operators<'a>,
    count: i32,
}

impl Serialize for OperatorPages<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(None)?;
        for index in 0..self.count {
            seq.serialize_element(&OperatorPage { of: self.of, index })?;
        }
        seq.end()
    }
}

struct OperatorPage<'a> {
    of: &'a Operators<'a>,
    index: i32,
}

impl OperatorPage<'_> {
    /// The page's resources and contents, as loading the page finds them.
    fn load(&self) -> Result<(Option<Object>, Option<Object>), String> {
        let doc = self.of.doc;
        let page = doc.page(self.index).map_err(|e| e.0)?;
        let Some(dict) = doc.resolve(&page).as_dict().cloned() else {
            return Err("page object is not a dictionary".to_string());
        };
        let resources = if self.of.bare {
            Some(Object::Dict(std::sync::Arc::new(nupdf::Dict::new())))
        } else {
            doc.inherited(&page, b"Resources").map_err(|e| e.0)?
        };
        // Loading the page looks up /Group/S for transparency, which
        // loads what it refers to.
        let _ = doc.dict_get(&doc.dict_get(&page, b"Group"), b"S");
        let contents = dict
            .get(b"Contents")
            .filter(|c| !matches!(c, Object::Null))
            .cloned();
        Ok((resources, contents))
    }
}

impl Serialize for OperatorPage<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let doc = self.of.doc;
        let mut map = s.serialize_map(None)?;
        match self.load() {
            Err(e) => map.serialize_entry("error", &e)?,
            Ok((resources, contents)) => {
                let error = std::cell::Cell::new(None);
                let ops = Rows::new(|sink: &mut dyn FnMut(Value)| {
                    let mut rec = Recorder {
                        doc,
                        sink,
                        in_extgstate: false,
                        state: Default::default(),
                    };
                    let result = nupdf::content::process_contents(
                        doc,
                        resources.as_ref(),
                        contents.as_ref(),
                        &mut rec,
                    );
                    if let Err(line) = result {
                        error.set(Some(nupdf::diag::report::strip(&line).to_string()));
                        // The processor was never closed (PDFPage.process).
                        doc.diagnostics().warn(
                            "content.processor_unclosed",
                            "dropping unclosed PDF processor",
                        );
                    }
                });
                map.serialize_entry("ops", &ops)?;
                if let Some(e) = error.take() {
                    map.serialize_entry("error", &e)?;
                }
            }
        }
        map.end()
    }
}

/// Records operators the way MuPDF's script processor reports them: one
/// array per operator, its name then its operands. Its glue has quirks,
/// which this follows:
///
/// - Inside a `gs`, the ExtGState's line width, joins, caps, miter limit,
///   intent, flatness and font are not reported (its dash is), and neither
///   are the entries the script processor does not take.
/// - `"` is not reported: the glue passes it the wrong arguments.
/// - `BI` is not reported: for an image without a /CS name the glue hands
///   the script engine a null string, which crashes it, so the oracle
///   cannot take the call. What follows the image shows where it ended.
/// - ASCII text of `Tj` and `'` is read as a C string, so a short string
///   runs on into what longer ones left in MuPDF's buffer, and the script
///   engine reads that as UTF-8 (`script_text`).
struct Recorder<'a> {
    doc: &'a Document,
    /// Takes each operator as it is recorded.
    sink: &'a mut dyn FnMut(Value),
    in_extgstate: bool,
    state: nupdf::content::ProcState,
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

fn name(n: &[u8]) -> Value {
    json!(escape_name(n))
}

/// Shown text as oracle/js/operators.js prints it. The script glue's
/// `push_byte_string` pushes ASCII text (no NUL, nothing above 127) as a C
/// string, which mujs reads as UTF-8, and the script prints the UTF-16
/// units `charCodeAt` gives, each in hex; other text is pushed byte by
/// byte.
fn script_text(text: &[u8], c_string: &[u8]) -> String {
    if !text.iter().all(|&b| b != 0 && b <= 127) {
        return hex(text);
    }
    let mut out = String::new();
    let mut s = c_string;
    while let Some(&c) = s.first() {
        let (rune, len) = if c < 0x80 {
            (u32::from(c), 1)
        } else {
            chartorune(s)
        };
        s = &s[len.min(s.len())..];
        let units = if rune >= 0x10000 {
            vec![
                0xd800 + ((rune - 0x10000) >> 10),
                0xdc00 + ((rune - 0x10000) & 0x3ff),
            ]
        } else {
            vec![rune]
        };
        for u in units {
            out.push_str(&format!("{u:02x}"));
        }
    }
    out
}

/// mujs's `chartorune` (Plan 9's): one character of UTF-8, U+FFFD and one
/// byte for anything that does not decode. Past the end reads as the NUL
/// that ends the C string.
pub(crate) fn chartorune(s: &[u8]) -> (u32, usize) {
    const BAD: (u32, usize) = (0xfffd, 1);
    let at = |i: usize| u32::from(s.get(i).copied().unwrap_or(0));
    let c = at(0);
    if c == 0xc0 && at(1) == 0x80 {
        return (0, 2);
    }
    let c1 = at(1) ^ 0x80;
    if c1 & 0xc0 != 0 {
        return BAD;
    }
    if c < 0xe0 {
        let l = ((c << 6) | c1) & 0x7ff;
        return if c < 0xc0 || l <= 0x7f { BAD } else { (l, 2) };
    }
    let c2 = at(2) ^ 0x80;
    if c2 & 0xc0 != 0 {
        return BAD;
    }
    if c < 0xf0 {
        let l = ((((c << 6) | c1) << 6) | c2) & 0xffff;
        return if l <= 0x7ff { BAD } else { (l, 3) };
    }
    let c3 = at(3) ^ 0x80;
    if c3 & 0xc0 != 0 {
        return BAD;
    }
    if c < 0xf8 {
        let l = ((((((c << 6) | c1) << 6) | c2) << 6) | c3) & 0x1f_ffff;
        return if l <= 0xffff || l > 0x10_ffff {
            BAD
        } else {
            (l, 4)
        };
    }
    BAD
}

impl Recorder<'_> {
    /// Items of a `TJ` or `d` array: numbers as reals, the rest through
    /// `string` (None for `d`, which reads everything as a number).
    fn items(&self, obj: Option<&Object>, text: bool) -> Value {
        let Some(Object::Array(items)) = obj.map(|o| self.doc.resolve(o)) else {
            return json!([]);
        };
        let doc = self.doc;
        Value::Array(
            items
                .iter()
                .map(|item| match doc.resolve(item) {
                    v if v.is_number() || !text => number(v.to_real()),
                    Object::String(s) => json!(hex(&s)),
                    _ => json!(""),
                })
                .collect(),
        )
    }
}

impl nupdf::content::Processor for Recorder<'_> {
    fn wants_soft_mask(&self) -> bool {
        false
    }

    fn state(&mut self) -> &mut nupdf::content::ProcState {
        &mut self.state
    }

    fn op(&mut self, op: nupdf::content::Op<'_>) -> Result<(), String> {
        use nupdf::content::Op;
        let hidden = self.in_extgstate;
        let entry: Vec<Value> = match op {
            Op::LineWidth(_)
            | Op::LineJoin(_)
            | Op::LineCap(_)
            | Op::MiterLimit(_)
            | Op::Intent(_)
            | Op::Flatness(_)
            | Op::Font { .. }
                if hidden =>
            {
                return Ok(());
            }
            Op::LineWidth(v) => vec![json!("w"), number(v)],
            Op::LineJoin(v) => vec![json!("j"), json!(v)],
            Op::LineCap(v) => vec![json!("J"), json!(v)],
            Op::MiterLimit(v) => vec![json!("M"), number(v)],
            Op::Dash { array, phase } => vec![json!("d"), self.items(array, false), number(phase)],
            Op::Intent(n) => vec![json!("ri"), name(n)],
            Op::Flatness(v) => vec![json!("i"), number(v)],
            Op::GsBegin { name: n, .. } => {
                self.in_extgstate = true;
                vec![json!("gs"), name(n)]
            }
            Op::GsEnd => {
                self.in_extgstate = false;
                return Ok(());
            }
            Op::GsBlendMode(_)
            | Op::GsStrokeAlpha(_)
            | Op::GsFillAlpha(_)
            | Op::GsSoftMask(_)
            | Op::GsOverprintStroke(_)
            | Op::GsOverprintFill(_)
            | Op::GsOverprintMode(_)
            | Op::GsBlackPointCompensation(_)
            | Op::NextLineShowSpaced { .. }
            | Op::InlineImage { .. }
            | Op::EndOfData
            | Op::End => return Ok(()),
            Op::Save => vec![json!("q")],
            Op::Restore => vec![json!("Q")],
            Op::Transform(m) => [vec![json!("cm")], m.map(number).to_vec()].concat(),
            Op::MoveTo(x, y) => vec![json!("m"), number(x), number(y)],
            Op::LineTo(x, y) => vec![json!("l"), number(x), number(y)],
            Op::CurveTo(c) => [vec![json!("c")], c.map(number).to_vec()].concat(),
            Op::CurveToV(c) => [vec![json!("v")], c.map(number).to_vec()].concat(),
            Op::CurveToY(c) => [vec![json!("y")], c.map(number).to_vec()].concat(),
            Op::ClosePath => vec![json!("h")],
            Op::Rect(r) => [vec![json!("re")], r.map(number).to_vec()].concat(),
            Op::Stroke => vec![json!("S")],
            Op::CloseStroke => vec![json!("s")],
            Op::FillCompat => vec![json!("F")],
            Op::Fill => vec![json!("f")],
            Op::FillEvenOdd => vec![json!("fstar")],
            Op::FillStroke => vec![json!("B")],
            Op::FillStrokeEvenOdd => vec![json!("Bstar")],
            Op::CloseFillStroke => vec![json!("b")],
            Op::CloseFillStrokeEvenOdd => vec![json!("bstar")],
            Op::EndPath => vec![json!("n")],
            Op::Clip => vec![json!("W")],
            Op::ClipEvenOdd => vec![json!("Wstar")],
            Op::BeginText => vec![json!("BT")],
            Op::EndText => vec![json!("ET")],
            Op::CharSpacing(v) => vec![json!("Tc"), number(v)],
            Op::WordSpacing(v) => vec![json!("Tw"), number(v)],
            Op::HorizontalScaling(v) => vec![json!("Tz"), number(v)],
            Op::Leading(v) => vec![json!("TL"), number(v)],
            Op::Font { name: n, size, .. } => vec![json!("Tf"), name(n), number(size)],
            Op::RenderMode(v) => vec![json!("Tr"), json!(v)],
            Op::Rise(v) => vec![json!("Ts"), number(v)],
            Op::TextMove(x, y) => vec![json!("Td"), number(x), number(y)],
            Op::TextMoveSetLeading(x, y) => vec![json!("TD"), number(x), number(y)],
            Op::TextMatrix(m) => [vec![json!("Tm")], m.map(number).to_vec()].concat(),
            Op::NextLine => vec![json!("Tstar")],
            Op::ShowArray(a) => vec![json!("TJ"), self.items(a, true)],
            Op::Show { text, c_string } => vec![json!("Tj"), json!(script_text(text, c_string))],
            Op::NextLineShow { text, c_string } => {
                vec![json!("squote"), json!(script_text(text, c_string))]
            }
            Op::Type3Width(x, y) => vec![json!("d0"), number(x), number(y)],
            Op::Type3Bbox(b) => [vec![json!("d1")], b.map(number).to_vec()].concat(),
            Op::ColorSpace {
                stroke, name: n, ..
            } => {
                vec![json!(if stroke { "CS" } else { "cs" }), name(n)]
            }
            Op::ColorPattern {
                stroke,
                name: n,
                color,
                ..
            } => vec![
                json!(if stroke { "SC_pattern" } else { "sc_pattern" }),
                name(n),
                numbers(color),
            ],
            Op::ColorShade {
                stroke, name: n, ..
            } => {
                vec![json!(if stroke { "SC_shade" } else { "sc_shade" }), name(n)]
            }
            Op::Color { stroke, color } => {
                vec![
                    json!(if stroke { "SC_color" } else { "sc_color" }),
                    numbers(color),
                ]
            }
            Op::Gray { stroke, gray } => vec![json!(if stroke { "G" } else { "g" }), number(gray)],
            Op::Rgb { stroke, rgb } => [
                vec![json!(if stroke { "RG" } else { "rg" })],
                rgb.map(number).to_vec(),
            ]
            .concat(),
            Op::Cmyk { stroke, cmyk } => [
                vec![json!(if stroke { "K" } else { "k" })],
                cmyk.map(number).to_vec(),
            ]
            .concat(),
            Op::Shade { name: n, .. } => vec![json!("sh"), name(n)],
            Op::DrawImage { name: n, .. } => vec![json!("Do_image"), name(n)],
            Op::DrawForm { name: n, .. } => vec![json!("Do_form"), name(n)],
            Op::MarkedPoint(t) => vec![json!("MP"), name(t)],
            Op::MarkedPointProperties { tag, raw, .. } => {
                vec![json!("DP"), name(tag), raw.map_or(Value::Null, encode)]
            }
            Op::BeginMarked(t) => vec![json!("BMC"), name(t)],
            Op::BeginMarkedProperties { tag, raw, .. } => {
                vec![json!("BDC"), name(tag), raw.map_or(Value::Null, encode)]
            }
            Op::EndMarked => vec![json!("EMC")],
            Op::BeginCompatibility => vec![json!("BX")],
            Op::EndCompatibility => vec![json!("EX")],
        };
        (self.sink)(Value::Array(entry));
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn shown_text_reads_as_the_script_engine_reads_it() {
        // ASCII text is read on into the buffer, as UTF-8 by mujs.
        assert_eq!(super::script_text(b"AB", b"AB\xc2\x03"), "4142fffd03");
        assert_eq!(super::script_text(b"a", b"a\xc3\xa9"), "61e9");
        assert_eq!(super::script_text(b"a", b"a\xf0\x9f\x98\x80"), "61d83dde00");
        assert_eq!(super::script_text(b"a", b"a\xc0\x80b"), "610062");
        // Anything else is taken byte by byte.
        assert_eq!(super::script_text(b"\xc3\xa9", b"\xc3\xa9"), "c3a9");
    }

    use super::*;

    #[test]
    fn names_escape_like_mupdf() {
        assert_eq!(escape_name(b"Type"), "Type");
        assert_eq!(escape_name(b"A B#C"), "A#20B#23C");
        assert_eq!(escape_name(b"\xba/"), "#BA#2F");
        assert_eq!(escape_name(b"ab\0cd"), "ab");
    }
}
