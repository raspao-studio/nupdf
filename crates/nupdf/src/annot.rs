// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-annot.c (pdf_sync_annots and the
// pdf_annot_* getters) and source/pdf/pdf-appearance.c
// (pdf_annot_rect_diff), Copyright (C) 2004-2026 Artifex Software, Inc.

//! A page's annotations as MuPDF loads them, and what its getters read of
//! each: type, flags, rectangle, text, colours, dates, and the points that
//! shape lines, polygons, ink and text markup.

use crate::document::Document;
use crate::geometry::{Matrix, Point, Rect};
use crate::object::Object;

/// `pdf_sync_annots`: the page's annotations and its widgets, in /Annots
/// order: dictionaries only, without links and popups.
pub fn page_annots(doc: &Document, page: &Object) -> (Vec<Object>, Vec<Object>) {
    let (mut annots, mut widgets) = (Vec::new(), Vec::new());
    let list = doc.resolve(&doc.dict_get(page, b"Annots"));
    for obj in list.as_array().unwrap_or(&[]) {
        if doc.resolve(obj).as_dict().is_none() {
            continue;
        }
        match subtype(doc, obj).as_deref() {
            Some(b"Link" | b"Popup") => {}
            Some(b"Widget") => widgets.push(obj.clone()),
            _ => annots.push(obj.clone()),
        }
    }
    (annots, widgets)
}

fn subtype(doc: &Document, annot: &Object) -> Option<Vec<u8>> {
    doc.resolve(&doc.dict_get(annot, b"Subtype"))
        .as_name()
        .map(<[u8]>::to_vec)
}

fn is(doc: &Document, annot: &Object, allowed: &[&[u8]]) -> bool {
    subtype(doc, annot).is_some_and(|s| allowed.contains(&s.as_slice()))
}

pub(crate) const TYPES: [&str; 28] = [
    "Text",
    "Link",
    "FreeText",
    "Line",
    "Square",
    "Circle",
    "Polygon",
    "PolyLine",
    "Highlight",
    "Underline",
    "Squiggly",
    "StrikeOut",
    "Redact",
    "Stamp",
    "Caret",
    "Ink",
    "Popup",
    "FileAttachment",
    "Sound",
    "Movie",
    "RichMedia",
    "Widget",
    "Screen",
    "PrinterMark",
    "TrapNet",
    "Watermark",
    "3D",
    "Projection",
];

/// `pdf_annot_type` as `pdf_string_from_annot_type` names it: the subtype,
/// or "UNKNOWN" for one MuPDF does not know.
pub fn type_name(doc: &Document, annot: &Object) -> &'static str {
    let s = subtype(doc, annot).unwrap_or_default();
    TYPES
        .iter()
        .find(|t| t.as_bytes() == s.as_slice())
        .copied()
        .unwrap_or("UNKNOWN")
}

const NO_ZOOM: i32 = 1 << 3;
const NO_ROTATE: i32 = 1 << 4;

/// `pdf_annot_flags`: /F, with NoZoom and NoRotate always on for Text.
pub fn flags(doc: &Document, annot: &Object) -> i32 {
    let f = doc.resolve(&doc.dict_get(annot, b"F")).to_int();
    if is(doc, annot, &[b"Text"]) {
        f | NO_ROTATE | NO_ZOOM
    } else {
        f
    }
}

fn real(doc: &Document, obj: &Object) -> f32 {
    doc.resolve(obj).to_real()
}

fn array(doc: &Document, obj: &Object) -> Vec<Object> {
    doc.resolve(obj)
        .as_array()
        .map(<[Object]>::to_vec)
        .unwrap_or_default()
}

fn array_real(doc: &Document, a: &[Object], i: usize) -> f32 {
    a.get(i).map_or(0.0, |v| real(doc, v))
}

fn text(doc: &Document, annot: &Object, key: &[u8]) -> String {
    match doc.resolve(&doc.dict_get(annot, key)) {
        Object::String(s) => crate::text_string::text_string(&s),
        _ => String::new(),
    }
}

const RECT: &[&[u8]] = &[
    b"Text",
    b"FreeText",
    b"Square",
    b"Circle",
    b"Redact",
    b"Stamp",
    b"Caret",
    b"Popup",
    b"FileAttachment",
    b"Sound",
    b"Movie",
    b"Widget",
    b"3D",
    b"RichMedia",
];

/// `pdf_annot_rect`, for the types whose /Rect is theirs to set (the others'
/// is worked out from their points): /Rect less /RD, through `ctm`.
pub fn rect(doc: &Document, annot: &Object, ctm: Matrix) -> Option<Rect> {
    if !is(doc, annot, RECT) {
        return None;
    }
    let mut r = doc.to_rect(Some(&doc.dict_get(annot, b"Rect")));
    let rd = array(doc, &doc.dict_get(annot, b"RD"));
    if matches!(doc.resolve(&doc.dict_get(annot, b"RD")), Object::Array(_)) {
        r.x0 += array_real(doc, &rd, 0);
        r.y0 += array_real(doc, &rd, 1);
        r.x1 -= array_real(doc, &rd, 2);
        r.y1 -= array_real(doc, &rd, 3);
    }
    Some(r.transform(ctm))
}

/// `pdf_annot_contents`.
pub fn contents(doc: &Document, annot: &Object) -> String {
    text(doc, annot, b"Contents")
}

const MARKUP: &[&[u8]] = &[
    b"Text",
    b"FreeText",
    b"Line",
    b"Square",
    b"Circle",
    b"Polygon",
    b"PolyLine",
    b"Highlight",
    b"Underline",
    b"Squiggly",
    b"StrikeOut",
    b"Redact",
    b"Stamp",
    b"Caret",
    b"Ink",
    b"FileAttachment",
    b"Sound",
];

/// `pdf_annot_author` (/T), for markup annotations.
pub fn author(doc: &Document, annot: &Object) -> Option<String> {
    is(doc, annot, MARKUP).then(|| text(doc, annot, b"T"))
}

/// `pdf_annot_subject` (/Subj), for markup annotations.
pub fn subject(doc: &Document, annot: &Object) -> Option<String> {
    is(doc, annot, MARKUP).then(|| text(doc, annot, b"Subj"))
}

/// `pdf_annot_color_imp`: none, gray, RGB or CMYK by the array's length.
fn color_of(doc: &Document, obj: &Object) -> Vec<f32> {
    let a = array(doc, obj);
    let n = match a.len() {
        0 => 0,
        1 | 2 => 1,
        3 => 3,
        _ => 4,
    };
    (0..n).map(|i| array_real(doc, &a, i)).collect()
}

/// `pdf_annot_color` (/C).
pub fn color(doc: &Document, annot: &Object) -> Vec<f32> {
    color_of(doc, &doc.dict_get(annot, b"C"))
}

/// `pdf_annot_interior_color` (/IC), for the types that have one.
pub fn interior_color(doc: &Document, annot: &Object) -> Option<Vec<f32>> {
    is(
        doc,
        annot,
        &[b"Circle", b"Line", b"PolyLine", b"Polygon", b"Square"],
    )
    .then(|| color_of(doc, &doc.dict_get(annot, b"IC")))
}

/// `pdf_annot_opacity` (/CA, 1 by default).
pub fn opacity(doc: &Document, annot: &Object) -> f32 {
    match doc.resolve(&doc.dict_get(annot, b"CA")) {
        v if v.is_number() => v.to_real(),
        _ => 1.0,
    }
}

fn date(doc: &Document, annot: &Object, key: &[u8]) -> Option<i64> {
    match doc.resolve(&doc.dict_get(annot, key)) {
        Object::String(s) => crate::date::parse_date(&s, doc.diagnostics()),
        _ => None,
    }
}

/// `pdf_annot_creation_date`, in seconds since 1970.
pub fn created(doc: &Document, annot: &Object) -> Option<i64> {
    date(doc, annot, b"CreationDate")
}

/// `pdf_annot_modification_date` (/M), in seconds since 1970.
pub fn modified(doc: &Document, annot: &Object) -> Option<i64> {
    date(doc, annot, b"M")
}

/// `pdf_annot_icon_name` (/Name), with each type's default.
pub fn icon(doc: &Document, annot: &Object) -> Option<String> {
    let s = subtype(doc, annot)?;
    let default = match s.as_slice() {
        b"Text" => "Note",
        b"Stamp" => "",
        b"FileAttachment" => "PushPin",
        b"Sound" => "Speaker",
        _ => return None,
    };
    // A reference to a missing object is there, and not a name.
    let name = doc.dict_get(annot, b"Name");
    if matches!(name, Object::Null) {
        return Some(default.to_string());
    }
    Some(match doc.resolve(&name) {
        Object::Name(n) => String::from_utf8_lossy(&n).into_owned(),
        _ => String::new(),
    })
}

/// `pdf_annot_is_open`, for Text annotations and those with a popup: the
/// popup's /Open, else a Text annotation's own.
pub fn is_open(doc: &Document, annot: &Object) -> Option<bool> {
    let popup = doc.dict_get(annot, b"Popup");
    let text = is(doc, annot, &[b"Text"]);
    let bool_of =
        |o: &Object, k: &[u8]| matches!(doc.resolve(&doc.dict_get(o, k)), Object::Bool(true));
    if !matches!(popup, Object::Null) {
        Some(bool_of(&popup, b"Open"))
    } else if text {
        Some(bool_of(annot, b"Open"))
    } else {
        None
    }
}

/// `pdf_annot_border_style` and `pdf_annot_border_width`, for the types with
/// a border: /BS /S and /BS /W, else the third number of /Border, else 1.
pub fn border(doc: &Document, annot: &Object) -> Option<(&'static str, f32)> {
    if !is(
        doc,
        annot,
        &[
            b"Circle",
            b"FreeText",
            b"Ink",
            b"Line",
            b"Polygon",
            b"PolyLine",
            b"Square",
            b"Widget",
        ],
    ) {
        return None;
    }
    let bs = doc.dict_get(annot, b"BS");
    let style = match doc.resolve(&doc.dict_get(&bs, b"S")).as_name() {
        Some(b"D") => "Dashed",
        Some(b"B") => "Beveled",
        Some(b"I") => "Inset",
        Some(b"U") => "Underline",
        _ => "Solid",
    };
    let w = doc.resolve(&doc.dict_get(&bs, b"W"));
    let width = if w.is_number() {
        w.to_real()
    } else {
        let border = array(doc, &doc.dict_get(annot, b"Border"));
        match border.get(2).map(|v| doc.resolve(v)) {
            Some(v) if v.is_number() => v.to_real(),
            _ => 1.0,
        }
    };
    Some((style, width))
}

fn points(doc: &Document, a: &[Object], n: usize, ctm: Matrix) -> Vec<Point> {
    (0..n)
        .map(|i| ctm.transform_xy(array_real(doc, a, 2 * i), array_real(doc, a, 2 * i + 1)))
        .collect()
}

/// `pdf_annot_line` (/L), for Line annotations, through `ctm`.
pub fn line(doc: &Document, annot: &Object, ctm: Matrix) -> Option<[Point; 2]> {
    if !is(doc, annot, &[b"Line"]) {
        return None;
    }
    let l = array(doc, &doc.dict_get(annot, b"L"));
    let p = points(doc, &l, 2, ctm);
    Some([p[0], p[1]])
}

/// `pdf_annot_vertex`, each of a polygon's or polyline's /Vertices.
pub fn vertices(doc: &Document, annot: &Object, ctm: Matrix) -> Option<Vec<Point>> {
    if !is(doc, annot, &[b"PolyLine", b"Polygon"]) {
        return None;
    }
    let v = array(doc, &doc.dict_get(annot, b"Vertices"));
    Some(points(doc, &v, v.len() / 2, ctm))
}

/// `pdf_annot_quad_point`, each quad of text markup's /QuadPoints, as
/// eight numbers: upper left, upper right, lower left, lower right.
pub fn quad_points(doc: &Document, annot: &Object, ctm: Matrix) -> Option<Vec<[Point; 4]>> {
    if !is(
        doc,
        annot,
        &[
            b"Highlight",
            b"Link",
            b"Squiggly",
            b"StrikeOut",
            b"Underline",
            b"Redact",
        ],
    ) {
        return None;
    }
    let q = array(doc, &doc.dict_get(annot, b"QuadPoints"));
    Some(
        (0..q.len() / 8)
            .map(|i| {
                let p = points(doc, &q[i * 8..], 4, ctm);
                [p[0], p[1], p[2], p[3]]
            })
            .collect(),
    )
}

/// `pdf_annot_ink_list_stroke_vertex`, each stroke of an Ink annotation's
/// /InkList.
pub fn ink_list(doc: &Document, annot: &Object, ctm: Matrix) -> Option<Vec<Vec<Point>>> {
    if !is(doc, annot, &[b"Ink"]) {
        return None;
    }
    let strokes = array(doc, &doc.dict_get(annot, b"InkList"));
    Some(
        strokes
            .iter()
            .map(|s| {
                let s = array(doc, s);
                points(doc, &s, s.len() / 2, ctm)
            })
            .collect(),
    )
}
