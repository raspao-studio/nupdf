// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-appearance.c
// (pdf_write_free_text_appearance, pdf_write_line_caption, the stamp
// writers, escape_text, text_needs_rich_layout) and source/pdf/pdf-annot.c
// (pdf_annot_is_standard_stamp, pdf_annot_stamp_image_obj),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Free text, stamps and line captions, and the text layout they share
//! with text widgets (in `layout`, with the fonts it needs in `fonts`).
//!
//! The font resources these writers add are direct dictionaries in
//! `Synthesis::resources`, where MuPDF adds indirect objects; the image
//! stamp's image stays the reference the file has.

use std::sync::Arc;

use super::{Writer, len, pt, real_at};
use crate::annot;
use crate::document::Document;
use crate::geometry::{Matrix, Point, Rect};
use crate::glyphs;
use crate::object::{Dict, Object};

pub mod fonts;
pub mod layout;

use layout::TextBox;

/// `fz_rotate`: exact at the quarter turns.
pub(crate) fn rotate(theta: f32) -> Matrix {
    let mut theta = theta % 360.0;
    if theta < 0.0 {
        theta += 360.0;
    }
    let (s, c) = if (0.0 - theta).abs() < f32::EPSILON {
        (0.0, 1.0)
    } else if (90.0 - theta).abs() < f32::EPSILON {
        (1.0, 0.0)
    } else if (180.0 - theta).abs() < f32::EPSILON {
        (0.0, -1.0)
    } else if (270.0 - theta).abs() < f32::EPSILON {
        (-1.0, 0.0)
    } else {
        let r = theta * std::f32::consts::PI / 180.0;
        (r.sin(), r.cos())
    };
    Matrix {
        a: c,
        b: s,
        c: -s,
        d: c,
        e: 0.0,
        f: 0.0,
    }
}

impl Writer<'_> {
    /// `%M`: a matrix's six numbers.
    fn matrix(&mut self, m: Matrix) {
        for (i, v) in [m.a, m.b, m.c, m.d, m.e, m.f].into_iter().enumerate() {
            if i > 0 {
                self.buf.push(b' ');
            }
            self.g(v);
        }
    }

    /// The annotation's own value for `key`, unresolved, as C compares it
    /// with a name by pointer.
    fn raw(&self, key: &[u8]) -> Object {
        self.doc
            .resolve(&self.annot)
            .as_dict()
            .and_then(|d| d.get(key).cloned())
            .unwrap_or(Object::Null)
    }

    /// The resources' /Font, a new dictionary each time, as
    /// `pdf_dict_put_dict` makes it.
    fn put_res(&mut self, key: &[u8], d: Dict) {
        self.res
            .get_or_insert_with(Dict::new)
            .put(key, Object::Dict(Arc::new(d)));
    }
}

/// Whether a raw value is the name `n` itself (not a reference to it).
fn is_direct_name(o: &Object, n: &[u8]) -> bool {
    matches!(o, Object::Name(v) if &**v == n)
}

/// `pdf_annot_rect_diff`: /RD, zero when it is not an array.
fn rect_diff(w: &Writer) -> Rect {
    let rd = w.get(b"RD");
    if !matches!(rd, Object::Array(_)) {
        return Rect::new(0.0, 0.0, 0.0, 0.0);
    }
    let doc = w.doc;
    Rect::new(
        real_at(doc, &rd, 0),
        real_at(doc, &rd, 1),
        real_at(doc, &rd, 2),
        real_at(doc, &rd, 3),
    )
}

/// `escape_text`: the text as HTML, for the rich text writer.
pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            c => out.push(c),
        }
    }
    out
}

/// `text_needs_rich_layout`: whether a character has no font among the
/// base-14 encodings and the CJK fonts.
pub fn text_needs_rich_layout(s: &str) -> bool {
    use crate::unicode as ucd;
    s.chars().any(|ch| {
        let c = ch as i32;
        if glyphs::windows_1252_from_unicode(c) > 0
            || glyphs::iso8859_7_from_unicode(c) > 0
            || glyphs::koi8u_from_unicode(c) > 0
        {
            return false;
        }
        let script = ucd::script(c);
        !(matches!(
            script,
            ucd::SCRIPT_HANGUL
                | ucd::SCRIPT_HIRAGANA
                | ucd::SCRIPT_KATAKANA
                | ucd::SCRIPT_BOPOMOFO
                | ucd::SCRIPT_HAN
        ) || fonts::is_han_form(c))
    })
}

/// `pdf_dict_get_text_string_opt`.
fn text_string_opt(w: &Writer, key: &[u8]) -> Option<String> {
    match w.get(key) {
        Object::String(s) => Some(crate::text_string::text_string(&s)),
        _ => None,
    }
}

/// `pdf_write_free_text_appearance`: the bounding box. The text box is
/// /Rect less /RD; a callout line widens /Rect and /RD is written again.
/// Text MuPDF lays out with its HTML engine (rich text, a /DS style, or
/// characters no base-14 or CJK font has) is not supported.
pub(super) fn free_text(
    w: &mut Writer,
    rect: &mut Rect,
    matrix: &mut Matrix,
) -> Result<Rect, String> {
    let doc = w.doc;
    let text = annot::contents(doc, &w.annot);
    let q = match w.get(b"Q").to_int() {
        q @ 0..=2 => q,
        _ => 0,
    };
    let da = fonts::default_appearance(doc, &w.annot)?;
    let lang = fonts::language(doc, &w.annot)?;
    let mut rd = rect_diff(w);
    let mut size = da.size;
    // FreeText is always multi-line, so a zero size defaults to 12pt.
    if size <= 0.0 {
        size = 12.0;
    }
    // /Rotate, undocumented but Adobe's, turns only the text in the box.
    let r = w.get(b"Rotate").to_int();

    let text_box = Rect::new(
        rect.x0 + rd.x0,
        rect.y0 + rd.y0,
        rect.x1 - rd.x1,
        rect.y1 - rd.y1,
    );
    *rect = text_box;
    let mut bw = text_box.x1 - text_box.x0;
    let mut bh = text_box.y1 - text_box.y0;

    w.opacity();
    w.dash_pattern()?;
    let ic = w.fill_color();
    w.color0(da.color(), true);
    let b = w.border()?;

    // The callout line.
    if w.get(b"IT").is_name(b"FreeTextCallout") {
        let cl = w.get(b"CL");
        let n = len(doc, &cl);
        if n == 4 || n == 6 {
            let mut xy = [0.0f32; 6];
            for i in (0..n).step_by(2) {
                let x = real_at(doc, &cl, i);
                let y = real_at(doc, &cl, i + 1);
                xy[i] = x;
                xy[i + 1] = y;
                if x < rect.x0 {
                    rect.x0 = x;
                }
                if y < rect.y0 {
                    rect.y0 = y;
                }
                if x > rect.x1 {
                    rect.x1 = x;
                }
                if y > rect.y1 {
                    rect.y1 = y;
                }
            }
            w.op(&[xy[0], xy[1]], "m");
            for i in (2..n).step_by(2) {
                w.op(&[xy[i], xy[i + 1]], "l");
            }
            w.s("S\n");
            let le = w.raw(b"LE");
            super::shapes::line_cap(
                w,
                rect,
                xy[0],
                xy[1],
                xy[2] - xy[0],
                xy[3] - xy[1],
                b,
                true,
                ic,
                &le,
            );
        }
    }

    // The box's background and border, and the clip to its inside.
    if ic {
        w.op(&[text_box.x0, text_box.y0, bw, bh], "re\nf");
    }
    if b > 0.0 {
        w.op(
            &[text_box.x0 + b / 2.0, text_box.y0 + b / 2.0, bw - b, bh - b],
            "re\nS",
        );
    }
    w.op(
        &[text_box.x0 + b, text_box.y0 + b, bw - b * 2.0, bh - b * 2.0],
        "re\nW\nn",
    );

    // /RD again, around the callout.
    rd.x0 = text_box.x0 - rect.x0;
    rd.y0 = text_box.y0 - rect.y0;
    rd.x1 = rect.x1 - text_box.x1;
    rd.y1 = rect.y1 - text_box.y1;

    // The text's rotation within the box.
    if r == 90 || r == 270 {
        std::mem::swap(&mut bw, &mut bh);
    }
    let mut tfm = rotate(r as f32);
    match r {
        270 => tfm.f += bw,
        90 => tfm.e += bh,
        180 => {
            tfm.e += bw;
            tfm.f += bh;
        }
        _ => {}
    }
    tfm.e += text_box.x0;
    tfm.f += text_box.y0;
    w.s("q\n");
    w.op(&[tfm.a, tfm.b, tfm.c, tfm.d, tfm.e, tfm.f], "cm");

    let ds = text_string_opt(w, b"DS");
    let rc = text_string_opt(w, b"RC");
    if rc.is_some() || ds.is_some() || text_needs_rich_layout(&text) {
        return Err("rich text appearances are not supported".into());
    }
    layout::write_variable_text(
        w,
        lang,
        &text,
        da.mapped_font(),
        da.color(),
        &TextBox {
            size,
            q,
            w: bw,
            h: bh,
            padding: b * 2.0,
            baseline: 0.8,
            lineheight: 1.2,
            multiline: true,
            comb: 0,
            adjust_baseline: false,
        },
    )?;
    w.s("Q\n");
    w.put_rect(b"RD", rd);
    *matrix = Matrix::IDENTITY;
    Ok(*rect)
}

/// `pdf_write_line_caption`: a Line annotation's /Contents along the line
/// from `a` to `b`, in Helvetica 12 and black; inline unless /CP is Top,
/// it does not fit, or /CO moves it. The gap to leave in the line for an
/// inline caption, else 0.
pub(super) fn line_caption(
    w: &mut Writer,
    rect: &mut Rect,
    a: Point,
    b: Point,
) -> Result<f32, String> {
    let doc = w.doc;
    let mut dx = b.x - a.x;
    let mut dy = b.y - a.y;
    let line_length = dx.hypot(dy);
    dx /= line_length;
    dy /= line_length;

    let text = annot::contents(doc, &w.annot);
    let lang = fonts::language(doc, &w.annot)?;
    let co_obj = w.get(b"CO");
    let mut co = pt(real_at(doc, &co_obj, 0), real_at(doc, &co_obj, 1));

    let font = fonts::base14("Helvetica")?;
    let mut res_font = Dict::new();
    fonts::add_required_fonts(&mut res_font, lang, &font, "Helvetica", "Helv", &text);
    w.put_res(b"Font", res_font);
    let size = 12.0f32;
    let tw = layout::measure_string(lang, &font, &text) * size;

    // Inline unless /CP says Top or the caption does not fit.
    let mut top = is_direct_name(&w.raw(b"CP"), b"Top");
    if tw + size > line_length {
        top = true;
    }

    let angle = (f64::from(dy).atan2(f64::from(dx)) * 180.0 / std::f64::consts::PI) as f32;
    let mut tm = rotate(angle);
    tm.e = (-dx).mul_add(tw / 2.0, (a.x + b.x) / 2.0);
    tm.f = (-dy).mul_add(tw / 2.0, (a.y + b.y) / 2.0);

    if co.x != 0.0 || co.y != 0.0 {
        // An offset caption is never inline.
        top = true;
        if co.y < 0.0 {
            co.y -= size;
        }
        tm.e += co.x.mul_add(dx, -(co.y * dy));
        tm.f += co.x.mul_add(dy, co.y * dx);
    } else if top {
        // `e -= dy * size * 0.2f`: the last product fused.
        tm.e = (-(dy * size)).mul_add(0.2, tm.e);
        tm.f = (dx * size).mul_add(0.2, tm.f);
    } else {
        tm.e = (dy * size).mul_add(0.3, tm.e);
        tm.f = (-(dx * size)).mul_add(0.3, tm.f);
    }

    w.s("q\n");
    w.matrix(tm);
    w.s(" cm\n");
    // Acrobat always draws captions in black.
    w.s("0 g\n");
    w.s("BT\n");
    layout::write_string(w, lang, &font, "Helv", size, &text, text.len());
    w.s("ET\n");
    w.s("Q\n");

    // `tm.e - dx * tw/2` divides last, so nothing is fused.
    *rect = rect
        .include_point(pt(tm.e - dx * tw / 2.0, tm.f - dy * tw / 2.0))
        .include_point(pt(tm.e + dx * tw / 2.0, tm.f + dy * tw / 2.0))
        .expand(size);

    Ok(if top { 0.0 } else { (tw + size / 2.0) / 2.0 })
}

/// The standard stamps' names.
const STANDARD_STAMPS: [&[u8]; 14] = [
    b"Approved",
    b"AsIs",
    b"Confidential",
    b"Departmental",
    b"Draft",
    b"Experimental",
    b"Expired",
    b"Final",
    b"ForComment",
    b"ForPublicRelease",
    b"NotApproved",
    b"NotForPublicRelease",
    b"Sold",
    b"TopSecret",
];

/// `pdf_is_image_stream`: an image by its type, its filter or its size.
fn is_image_stream(doc: &Document, obj: &Object) -> bool {
    let d = doc.resolve(obj);
    let Some(dict) = d.as_dict() else {
        return false;
    };
    if doc.dict_get(&d, b"Type").is_name(b"XObject")
        && doc.dict_get(&d, b"Subtype").is_name(b"Image")
    {
        return true;
    }
    let is_image_filter = |o: &Object| {
        matches!(o, Object::Name(n) if matches!(&**n,
            b"CCITTFaxDecode" | b"CCF" | b"DCTDecode" | b"DCT" | b"RunLengthDecode"
            | b"RL" | b"JBIG2Decode" | b"JPXDecode"))
    };
    let filter = dict.get(b"Filter").cloned().unwrap_or(Object::Null);
    let implied = match doc.resolve(&filter) {
        Object::Name(_) => is_image_filter(&filter),
        Object::Array(a) => a.iter().any(is_image_filter),
        _ => false,
    };
    if implied {
        return true;
    }
    let present = |k: &[u8]| !matches!(dict.get(k), None | Some(Object::Null));
    present(b"Width") && present(b"Height")
}

/// `pdf_annot_stamp_image_obj`: the image a stamp's appearance shows, its
/// only XObject resource, as stored.
pub(crate) fn stamp_image(doc: &Document, annot: &Object) -> Option<Object> {
    let ap = doc.dict_get(annot, b"AP");
    let n = doc.dict_get(&ap, b"N");
    let res = doc.dict_get(&n, b"Resources");
    let xobj = doc.dict_get(&res, b"XObject");
    let xobj = xobj.as_dict()?;
    if xobj.len() != 1 {
        return None;
    }
    let (_, img) = xobj.iter().next()?;
    is_image_stream(doc, img).then(|| img.clone())
}

/// `pdf_annot_is_standard_stamp`: whether a Stamp's /Name is one MuPDF
/// draws, or its appearance is an image MuPDF can draw again.
pub(crate) fn is_standard_stamp(doc: &Document, annot: &Object) -> bool {
    let name = doc.dict_get(annot, b"Name");
    if let Some(n) = name.as_name()
        && STANDARD_STAMPS.contains(&n)
    {
        return true;
    }
    stamp_image(doc, annot).is_some()
}

/// `measure_stamp_string`: the width in ems, each character as WinAnsi
/// has it or a middle dot.
fn measure_stamp_string(font: &crate::fontdesc::Font, text: &str) -> f32 {
    let mut w = 0.0f32;
    for ch in text.chars() {
        let mut c = ch as i32;
        if glyphs::windows_1252_from_unicode(c) < 0 {
            c = layout::REPLACEMENT;
        }
        let g = fonts::encode_character(font, c);
        w += fonts::advance(font, g);
    }
    w
}

/// `write_stamp`: one line of a rubber stamp, centred, `h` high at `y`.
fn write_stamp(w: &mut Writer, font: &crate::fontdesc::Font, text: &str, y: f32, h: f32) {
    let tw = measure_stamp_string(font, text) * h;
    w.s("BT\n");
    w.s("/Times ");
    w.g(h);
    w.s(" Tf\n");
    w.op(&[(190.0 - tw) / 2.0, y], "Td");
    w.buf.push(b'(');
    for ch in text.chars() {
        let mut c = glyphs::windows_1252_from_unicode(ch as i32);
        if c < 0 {
            c = layout::REPLACEMENT;
        }
        if c == i32::from(b'(') || c == i32::from(b')') || c == i32::from(b'\\') {
            w.buf.push(b'\\');
        }
        w.buf.push(c as u8);
    }
    w.buf.push(b')');
    w.s(" Tj\n");
    w.s("ET\n");
}

/// `pdf_write_stamp_appearance_rubber`: the stamp's words in Times-Bold
/// in a box 190 by 50, tilted a little; /Rect shrinks to that shape.
fn rubber_stamp(w: &mut Writer, rect: &mut Rect, matrix: &mut Matrix) -> Result<Rect, String> {
    let name = match w.raw(b"Name") {
        Object::Null => Object::name(b"Draft"),
        n => n,
    };
    let h = rect.y1 - rect.y0;
    let wd = rect.x1 - rect.x0;
    let xs = wd / 190.0;
    let ys = h / 50.0;

    let font = fonts::base14("Times-Bold")?;
    let mut res_font = Dict::new();
    res_font.put(b"Times", fonts::latin_font(&font, "Times-Bold"));
    w.put_res(b"Font", res_font);

    w.opacity();
    w.fill_color();
    w.stroke_color();
    w.matrix(rotate(0.6));
    w.s(" cm\n");
    w.s("2 w\n2 2 186 44 re\nS\n");

    // The name compared as stored: a reference to a name is drawn as the
    // name's text.
    let lines: &[(&str, f32, f32)] = match &name {
        Object::Name(n) => match &**n {
            b"Approved" => &[("APPROVED", 13.0, 30.0)],
            b"AsIs" => &[("AS IS", 13.0, 30.0)],
            b"Confidential" => &[("CONFIDENTIAL", 17.0, 20.0)],
            b"Departmental" => &[("DEPARTMENTAL", 17.0, 20.0)],
            b"Experimental" => &[("EXPERIMENTAL", 17.0, 20.0)],
            b"Expired" => &[("EXPIRED", 13.0, 30.0)],
            b"Final" => &[("FINAL", 13.0, 30.0)],
            b"ForComment" => &[("FOR COMMENT", 17.0, 20.0)],
            b"ForPublicRelease" => &[("FOR PUBLIC", 26.0, 18.0), ("RELEASE", 8.5, 18.0)],
            b"NotApproved" => &[("NOT APPROVED", 17.0, 20.0)],
            b"NotForPublicRelease" => &[("NOT FOR", 26.0, 18.0), ("PUBLIC RELEASE", 8.5, 18.0)],
            b"Sold" => &[("SOLD", 13.0, 30.0)],
            b"TopSecret" => &[("TOP SECRET", 14.0, 26.0)],
            b"Draft" => &[("DRAFT", 13.0, 30.0)],
            _ => &[],
        },
        _ => &[],
    };
    if lines.is_empty() {
        // `pdf_to_name`: the name's text, empty for anything else.
        let text = match w.doc.resolve(&name) {
            Object::Name(n) => {
                let end = n.iter().position(|&b| b == 0).unwrap_or(n.len());
                String::from_utf8_lossy(&n[..end]).into_owned()
            }
            _ => String::new(),
        };
        write_stamp(w, &font, &text, 17.0, 20.0);
    }
    for &(text, y, size) in lines {
        write_stamp(w, &font, text, y, size);
    }

    if xs > ys {
        let xc = (rect.x1 + rect.x0) / 2.0;
        rect.x0 = (-95.0f32).mul_add(ys, xc);
        rect.x1 = 95.0f32.mul_add(ys, xc);
    } else {
        let yc = (rect.y1 + rect.y0) / 2.0;
        rect.y0 = (-25.0f32).mul_add(xs, yc);
        rect.y1 = 25.0f32.mul_add(xs, yc);
    }
    *matrix = Matrix::IDENTITY;
    Ok(Rect::new(0.0, 0.0, 190.0, 50.0))
}

/// `pdf_write_stamp_appearance`: the image the stamp shows, drawn again in
/// the unit square, else a rubber stamp.
pub(super) fn stamp(w: &mut Writer, rect: &mut Rect, matrix: &mut Matrix) -> Result<Rect, String> {
    let Some(img) = stamp_image(w.doc, &w.annot) else {
        return rubber_stamp(w, rect, matrix);
    };
    let mut xobj = Dict::new();
    xobj.put(b"I", img);
    w.put_res(b"XObject", xobj);
    w.opacity();
    w.s("/I Do\n");
    *matrix = Matrix::IDENTITY;
    Ok(Rect::UNIT)
}

#[cfg(test)]
mod tests {
    //! The contents, /Rect, /RD and fonts expected are MuPDF 1.28.6's for
    //! the same annotation dictionary.

    use super::super::{Synthesis, synthesize};
    use super::*;
    use crate::object::ObjRef;

    fn pdf(dict: &str) -> Vec<u8> {
        format!(
            "%PDF-1.7\n1 0 obj\n<< /Type /Annot {dict} >>\nendobj\n\
             2 0 obj\n<< /Type /Catalog >>\nendobj\ntrailer\n<< /Size 3 /Root 2 0 R >>\n"
        )
        .into_bytes()
    }

    fn open(dict: &str) -> Document {
        Document::open(pdf(dict)).unwrap()
    }

    fn synth(doc: &Document) -> Synthesis {
        synthesize(doc, &Object::Ref(ObjRef::new(1, 0)), Matrix::IDENTITY).unwrap()
    }

    fn reals(doc: &Document, s: &Synthesis, key: &[u8]) -> Option<Vec<f32>> {
        let a = doc.dict_get(&s.annot, key);
        Some(a.as_array()?.iter().map(Object::to_real).collect())
    }

    /// The resource names under /Font, and each font's /BaseFont.
    fn fonts_of(s: &Synthesis) -> Vec<(String, String)> {
        let Some(Object::Dict(fonts)) = s.resources.as_ref().and_then(|r| r.get(b"Font")) else {
            return Vec::new();
        };
        fonts
            .iter()
            .map(|(k, v)| {
                let base = v
                    .as_dict()
                    .and_then(|d| d.get(b"BaseFont"))
                    .and_then(Object::as_name)
                    .unwrap_or(b"");
                (
                    String::from_utf8_lossy(k).into_owned(),
                    String::from_utf8_lossy(base).into_owned(),
                )
            })
            .collect()
    }

    fn check(dict: &str, content: &str, rect: [f32; 4], rd: Option<[f32; 4]>) -> Synthesis {
        let doc = open(dict);
        let s = synth(&doc);
        // Each byte as one character, as the CLI gives the content.
        let got: String = s.content.iter().map(|&b| char::from(b)).collect();
        assert_eq!(got, content);
        assert_eq!(reals(&doc, &s, b"Rect"), Some(rect.to_vec()));
        assert_eq!(reals(&doc, &s, b"RD"), rd.map(|r| r.to_vec()));
        s
    }

    #[test]
    fn free_text_centred() {
        let s = check(
            "/Subtype /FreeText /Rect [50 700 250 740] /Contents (Quadding test) \
             /DA (/Helv 12 Tf 0 g) /Q 1",
            "0 G\n1 w\n50.5 700.5 199 39 re\nS\n51 701 198 38 re\nW\nn\nq\n\
             1 0 -0 1 50 700 cm\nBT\n0 g\n2 42.8 Td\n60.644 -14.400001 Td\n\
             /Helv 12 Tf\n(Quadding test) Tj\nET\nQ\n",
            [50.0, 700.0, 250.0, 740.0],
            Some([0.0; 4]),
        );
        assert_eq!(fonts_of(&s), [("Helv".into(), "Helvetica".into())]);
        assert_eq!(s.bbox, Rect::new(50.0, 700.0, 250.0, 740.0));
    }

    #[test]
    fn free_text_breaks_lines_at_newlines_and_spaces() {
        let s = check(
            "/Subtype /FreeText /Rect [50 600 250 700] /Contents (Line one\\nLine two\\r\\n\
             and a much longer line that wraps \\(around\\)) /DA (/TiRo 10 Tf 1 0 0 rg) /Q 2",
            "1 0 0 RG\n1 w\n50.5 600.5 199 99 re\nS\n51 601 198 98 re\nW\nn\nq\n\
             1 0 -0 1 50 600 cm\nBT\n1 0 0 rg\n2 102 Td\n160.73 -12 Td\n/TiRo 10 Tf\n\
             (Line one) Tj\n-.55999758 -12 Td\n/TiRo 10 Tf\n(Line two) Tj\n\
             -136.63002 -12 Td\n/TiRo 10 Tf\n\
             (and a much longer line that wraps \\(around\\)) Tj\nET\nQ\n",
            [50.0, 600.0, 250.0, 700.0],
            Some([0.0; 4]),
        );
        assert_eq!(fonts_of(&s), [("TiRo".into(), "Times-Roman".into())]);
    }

    #[test]
    fn free_text_callout_widens_rect_and_rd() {
        check(
            "/Subtype /FreeText /Rect [50 500 400 560] /Contents (Callout) /DA (/Cour 9 Tf) \
             /IT /FreeTextCallout /CL [60 480 120 510 200 510] /LE /ClosedArrow \
             /RD [150 30 0 0] /C [0.2 0.4 1] /BS << /W 2 >>",
            ".2 .4 1 rg\n0 G\n2 w\n60 480 m\n120 510 l\n200 510 l\nS\n\
             71.716999 495.9208 m\n60 480 l\n79.766849 479.8211 l\nh\nb\n\
             200 530 200 30 re\nf\n201 531 198 28 re\nS\n202 532 196 26 re\nW\nn\nq\n\
             1 0 -0 1 200 530 cm\nBT\n0 g\n4 29.599999 Td\n0 -10.8 Td\n/Cour 9 Tf\n\
             (Callout) Tj\nET\nQ\n",
            [58.0, 477.821_1, 402.0, 562.0],
            Some([142.0, 52.178_894, 2.0, 2.0]),
        );
    }

    #[test]
    fn free_text_rotated_turns_only_the_text() {
        check(
            "/Subtype /FreeText /Rect [50 100 250 200] /Contents (Rotated text) \
             /DA (/Helv 0 Tf) /Rotate 90",
            "0 G\n1 w\n50.5 100.5 199 99 re\nS\n51 101 198 98 re\nW\nn\nq\n\
             0 1 -1 0 250 100 cm\nBT\n0 g\n2 202.79999 Td\n0 -14.400001 Td\n\
             /Helv 12 Tf\n(Rotated text) Tj\nET\nQ\n",
            [50.0, 100.0, 250.0, 200.0],
            Some([0.0; 4]),
        );
    }

    #[test]
    fn free_text_writes_each_script_in_its_font() {
        let s = check(
            "/Subtype /FreeText /Rect [0 0 200 100] /Contents <FEFF006103B2043665E5> \
             /DA (/Helv 10 Tf)",
            "0 G\n1 w\n.5 .5 199 99 re\nS\n1 1 198 98 re\nW\nn\nq\n1 0 -0 1 0 0 cm\nBT\n\
             0 g\n2 102 Td\n0 -12 Td\n/Helv 10 Tf\n(a) Tj\n/HelvGRK 10 Tf\n(\u{e2}) Tj\n\
             /HelvCYR 10 Tf\n(\u{d6}) Tj\n/Mincho 10 Tf\n<65e5> Tj\nET\nQ\n",
            [0.0, 0.0, 200.0, 100.0],
            Some([0.0; 4]),
        );
        assert_eq!(
            fonts_of(&s),
            [
                ("Helv".into(), "Helvetica".into()),
                ("HelvGRK".into(), "Helvetica".into()),
                ("HelvCYR".into(), "Helvetica".into()),
                ("Mincho".into(), "Mincho".into()),
            ]
        );
    }

    #[test]
    fn free_text_rich_text_is_refused() {
        let doc = open("/Subtype /FreeText /Rect [0 0 100 100] /Contents (x) /RC (<body>x</body>)");
        let r = synthesize(&doc, &Object::Ref(ObjRef::new(1, 0)), Matrix::IDENTITY);
        assert!(r.is_err());
    }

    #[test]
    fn rubber_stamp_on_two_lines() {
        let s = check(
            "/Subtype /Stamp /Rect [50 700 220 740] /Name /ForPublicRelease /C [1 0 0] /CA 0.6",
            "/H gs\n1 0 0 rg\n1 0 0 RG\n.99994519 .0104717849 -.0104717849 .99994519 0 0 cm\n\
             2 w\n2 2 186 44 re\nS\nBT\n/Times 18 Tf\n39.749 26 Td\n(FOR PUBLIC) Tj\nET\n\
             BT\n/Times 18 Tf\n52.988 8.5 Td\n(RELEASE) Tj\nET\n",
            [59.0, 700.0, 211.0, 740.0],
            None,
        );
        assert_eq!(s.bbox, Rect::new(0.0, 0.0, 190.0, 50.0));
        assert_eq!(fonts_of(&s), [("Times".into(), "Times-Bold".into())]);
    }

    #[test]
    fn rubber_stamp_keeps_its_shape() {
        check(
            "/Subtype /Stamp /Rect [0 0 100 200] /Name /Draft",
            ".99994519 .0104717849 -.0104717849 .99994519 0 0 cm\n2 w\n2 2 186 44 re\nS\n\
             BT\n/Times 30 Tf\n43.34 13 Td\n(DRAFT) Tj\nET\n",
            [0.0, 86.842_1, 100.0, 113.157_9],
            None,
        );
    }

    #[test]
    fn only_standard_or_image_stamps_are_standard() {
        let doc = open("/Subtype /Stamp /Name /TopSecret");
        assert!(is_standard_stamp(&doc, &Object::Ref(ObjRef::new(1, 0))));
        let doc = open("/Subtype /Stamp /Name /Custom");
        assert!(!is_standard_stamp(&doc, &Object::Ref(ObjRef::new(1, 0))));
        let doc = open("/Subtype /Stamp");
        assert!(!is_standard_stamp(&doc, &Object::Ref(ObjRef::new(1, 0))));
        let doc = open(
            "/Subtype /Stamp /Name /Custom /AP << /N << /Resources << /XObject << \
             /I << /Width 1 /Height 1 >> >> >> >> >>",
        );
        assert!(is_standard_stamp(&doc, &Object::Ref(ObjRef::new(1, 0))));
    }

    #[test]
    fn line_caption_inline() {
        let doc = open(
            "/Subtype /Line /Rect [40 490 310 590] /L [50 500 300 580] \
             /Contents (Sloped caption) /Cap true",
        );
        let mut w = Writer {
            doc: &doc,
            annot: doc.resolve(&Object::Ref(ObjRef::new(1, 0))),
            ctm: Matrix::IDENTITY,
            buf: Vec::new(),
            res: None,
        };
        let mut rect = Rect::new(50.0, 500.0, 300.0, 580.0);
        let gap = line_caption(&mut w, &mut rect, pt(50.0, 500.0), pt(300.0, 580.0)).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&w.buf),
            "q\n.95242419 .3047757 -.3047757 .95242419 138.29549 524.47476 cm\n0 g\nBT\n\
             /Helv 12 Tf\n(Sloped caption) Tj\nET\nQ\n"
        );
        // The line stops short of the caption on both sides.
        let half = 300.0f32.hypot(0.0);
        assert!(gap > 0.0 && gap < half);
    }

    #[test]
    fn rotate_is_exact_at_quarter_turns() {
        let m = rotate(90.0);
        assert_eq!((m.a, m.b, m.c, m.d), (0.0, 1.0, -1.0, 0.0));
        let m = rotate(-90.0);
        assert_eq!((m.a, m.b, m.c, m.d), (0.0, -1.0, 1.0, 0.0));
        let m = rotate(0.0);
        assert!(m.c == 0.0 && m.c.is_sign_negative());
        let m = rotate(0.6);
        assert!((m.b - 0.6f32.to_radians().sin()).abs() < 1e-6);
    }

    #[test]
    fn escape_text_escapes_markup() {
        assert_eq!(escape_text("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[test]
    fn rich_layout_is_needed_only_without_a_font() {
        assert!(!text_needs_rich_layout("Hello, World"));
        assert!(!text_needs_rich_layout("Ελληνικά и кириллица"));
        assert!(!text_needs_rich_layout("日本語、한국어"));
        assert!(text_needs_rich_layout("עברית"));
        assert!(text_needs_rich_layout("emoji 😀"));
    }
}
