// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-clean.c (pdf_redact_image_filter_pixels,
// pdf_redact_image_imp), source/pdf/pdf-image.c (pdf_add_image) and
// source/fitz/geometry.c (fz_is_quad_inside_quad),
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! Pixel redaction: an image an area covers goes whole; one an area only
//! meets is decoded, the pixels under each area turned white (or, for a
//! soft mask, opaque), and written back as a new image XObject of 8-bit
//! samples, which the writer then deflates.

use crate::colorspace::ColorSpace;
use crate::content_filter::{ImageAction, ShownImage};
use crate::document::Document;
use crate::geometry::{Matrix, Point, Quad, Rect};
use crate::object::{Dict, ObjRef, Object};
use crate::pixmap::Pixmap;

/// `cross` in `fz_is_point_inside_triangle`.
fn cross(a: Point, b: Point, p: Point) -> f32 {
    (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x)
}

fn inside_triangle(p: Point, a: Point, b: Point, c: Point) -> bool {
    let (ca, cb, cc) = (cross(a, b, p), cross(b, c, p), cross(c, a, p));
    if ca == 0.0 && cb == 0.0 && cc == 0.0 {
        return a.x == p.x && a.y == p.y;
    }
    (ca >= 0.0 && cb >= 0.0 && cc >= 0.0) || (ca <= 0.0 && cb <= 0.0 && cc <= 0.0)
}

fn valid(q: &Quad) -> bool {
    [q.ul, q.ur, q.ll, q.lr]
        .iter()
        .all(|p| !p.x.is_nan() && !p.y.is_nan())
}

/// `fz_is_point_inside_quad`.
fn inside_quad(p: Point, q: &Quad) -> bool {
    valid(q) && (inside_triangle(p, q.ul, q.ur, q.lr) || inside_triangle(p, q.ul, q.lr, q.ll))
}

/// `fz_is_quad_inside_quad`.
pub fn quad_inside(needle: &Quad, haystack: &Quad) -> bool {
    valid(needle)
        && valid(haystack)
        && [needle.ul, needle.ur, needle.ll, needle.lr]
            .iter()
            .all(|&p| inside_quad(p, haystack))
}

/// `fz_is_quad_intersecting_quad`: their boxes overlap.
pub fn quads_meet(a: &Quad, b: &Quad) -> bool {
    !Rect::from_quad(*a)
        .intersect(Rect::from_quad(*b))
        .is_empty()
}

/// The unit square through `ctm`, corner by corner.
fn unit_quad(ctm: Matrix) -> Quad {
    let t = |x: f32, y: f32| ctm.transform_xy(x, y);
    Quad {
        ul: t(0.0, 1.0),
        ur: t(1.0, 1.0),
        ll: t(0.0, 0.0),
        lr: t(1.0, 0.0),
    }
}

/// `fz_round_rect` with the rows flipped and clamped to the pixmap, as
/// `pdf_redact_image_imp` takes them: columns x0..x1, rows y0..y1.
fn pixel_box(q: &Quad, ctm: Matrix, w: usize, h: usize) -> (usize, usize, usize, usize) {
    let inv = ctm.invert().post_scale(w as f32, h as f32);
    let r = Rect::from_quad(*q).transform(inv);
    let clamp = |v: f32, hi: usize| (v.max(0.0) as usize).min(hi);
    let x0 = (r.x0 + 0.001).floor();
    let y0 = (r.y0 + 0.001).floor();
    let x1 = (r.x1 - 0.001).ceil();
    let y1 = (r.y1 - 0.001).ceil();
    let hf = h as f32;
    (
        clamp(x0, w),
        clamp(x1, w),
        clamp(hf - y1, h),
        clamp(hf - y0, h),
    )
}

fn subtractive(cs: Option<&ColorSpace>) -> bool {
    matches!(
        cs,
        Some(ColorSpace::Cmyk)
            | Some(ColorSpace::Icc(crate::colorspace::Family::Cmyk))
            | Some(ColorSpace::Separation { .. })
    )
}

/// Turns the pixels under `q` white, or opaque for alpha.
fn blank(pix: &mut Pixmap, q: &Quad, ctm: Matrix) {
    let (x0, x1, y0, y1) = pixel_box(q, ctm, pix.w, pix.h);
    let n = pix.n - usize::from(pix.alpha);
    let white = if subtractive(pix.colorspace.as_ref()) {
        0
    } else {
        255
    };
    let stride = pix.w * pix.n;
    for y in y0..y1 {
        for x in x0..x1 {
            let at = y * stride + x * pix.n;
            if let Some(px) = pix.samples.get_mut(at..at + pix.n) {
                px[..n].fill(white);
                if pix.alpha {
                    px[n] = 255;
                }
            }
        }
    }
}

/// `pdf_redact_image_filter_pixels`. An inline image, and an image
/// mask, an area meets is removed whole: blanking either is not done yet.
pub fn redact_pixels(
    doc: &Document,
    ctm: Matrix,
    image: ShownImage<'_>,
    quads: &[Quad],
) -> Result<ImageAction, String> {
    let area = unit_quad(ctm);
    if quads.iter().any(|q| quad_inside(&area, q)) {
        return Ok(ImageAction::Remove);
    }
    let meeting: Vec<&Quad> = quads.iter().filter(|q| quads_meet(&area, q)).collect();
    if meeting.is_empty() {
        return Ok(ImageAction::Keep);
    }
    let ShownImage::XObject(obj) = image else {
        return Ok(ImageAction::Remove);
    };
    let img = crate::image::load(doc, obj)?;
    if img.image_mask || img.colorspace.is_none() || img.is_jpx {
        return Ok(ImageAction::Remove);
    }
    let Ok(mut pix) = crate::pixmap::from_image(doc, &img) else {
        return Ok(ImageAction::Remove);
    };
    let mut mask = match &img.mask {
        Some(m) => match crate::pixmap::from_image(doc, m) {
            Ok(p) => Some(p),
            Err(_) => return Ok(ImageAction::Remove),
        },
        None => None,
    };
    for q in &meeting {
        // A 1x1 image under a mask: the mask is what to change.
        if mask.is_none() || pix.w > 1 || pix.h > 1 {
            blank(&mut pix, q, ctm);
        }
        if let Some(m) = mask.as_mut() {
            blank_mask(m, q, ctm);
        }
    }
    let cs = colorspace_object(doc, obj, pix.colorspace.as_ref());
    let smask = match mask {
        Some(m) => Some(add_pixmap(doc, &m, Object::name(b"DeviceGray"), None)?),
        None => None,
    };
    let num = add_pixmap(doc, &pix, cs, smask)?;
    Ok(ImageAction::ReplaceWith(num))
}

/// The mask's samples under `q` made opaque.
fn blank_mask(m: &mut Pixmap, q: &Quad, ctm: Matrix) {
    let (x0, x1, y0, y1) = pixel_box(q, ctm, m.w, m.h);
    let stride = m.w * m.n;
    for y in y0..y1 {
        let at = y * stride + x0 * m.n;
        let end = y * stride + x1 * m.n;
        if let Some(row) = m.samples.get_mut(at..end) {
            row.fill(0xff);
        }
    }
}

/// The colorspace to write the new samples with: a device space by name,
/// else the image's own, the base of an Indexed one.
fn colorspace_object(doc: &Document, obj: &Object, cs: Option<&ColorSpace>) -> Object {
    match cs {
        Some(ColorSpace::Gray) => return Object::name(b"DeviceGray"),
        Some(ColorSpace::Rgb) => return Object::name(b"DeviceRGB"),
        Some(ColorSpace::Cmyk) => return Object::name(b"DeviceCMYK"),
        _ => {}
    }
    let stored = doc.dict_get(obj, b"ColorSpace");
    match doc.resolve(&stored) {
        Object::Array(a)
            if a.first().is_some_and(|n| {
                doc.resolve(n).is_name(b"Indexed") || doc.resolve(n).is_name(b"I")
            }) =>
        {
            a.get(1).cloned().unwrap_or(Object::name(b"DeviceRGB"))
        }
        _ => stored,
    }
}

/// `pdf_add_image` of a pixmap: its color samples at 8 bits (alpha taken
/// out of them), as a new image XObject.
fn add_pixmap(doc: &Document, pix: &Pixmap, cs: Object, smask: Option<i32>) -> Result<i32, String> {
    let n = (pix.n - usize::from(pix.alpha)).max(1);
    let mut data = Vec::with_capacity(pix.w * pix.h * n);
    for px in pix.samples.chunks(pix.n.max(1)) {
        if pix.alpha {
            let a = i32::from(px[pix.n - 1]);
            let inva = if a != 0 { 255 * 256 / a } else { 0 };
            data.extend(px[..n].iter().map(|&s| ((i32::from(s) * inva) >> 8) as u8));
        } else {
            data.extend_from_slice(&px[..n]);
        }
    }
    let mut d = Dict::new();
    d.put(b"Type", Object::name(b"XObject"));
    d.put(b"Subtype", Object::name(b"Image"));
    d.put(b"Width", Object::Int(pix.w as i64));
    d.put(b"Height", Object::Int(pix.h as i64));
    d.put(b"BitsPerComponent", Object::Int(8));
    d.put(b"ColorSpace", cs);
    if let Some(s) = smask {
        d.put(b"SMask", Object::Ref(ObjRef::new(s, 0)));
    }
    d.put(b"Length", Object::Int(data.len() as i64));
    Ok(doc.add_stream(d, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::{ImageMethod, LineArt, RedactOptions, TextMethod, redact_page};

    /// A page drawing a 4x4 grey image over x 60..260, y 690..730.
    fn doc() -> Document {
        let content = b"q 200 0 0 40 60 690 cm /Im1 Do Q";
        let objs: Vec<Vec<u8>> = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</XObject<</Im1 5 0 R>>>>/Contents 4 0 R>>".to_vec(),
            [format!("<</Length {}>>stream\n", content.len()).as_bytes(), content, b"\nendstream"].concat(),
            [&b"<</Type/XObject/Subtype/Image/Width 4/Height 4/BitsPerComponent 8/ColorSpace/DeviceGray/Length 16>>stream\n"[..], &[0x40; 16], b"\nendstream"].concat(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(o);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes(),
        );
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        Document::open(out).unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn quad(x0: f32, y0: f32, x1: f32, y1: f32) -> Quad {
        Quad {
            ul: Point { x: x0, y: y1 },
            ur: Point { x: x1, y: y1 },
            ll: Point { x: x0, y: y0 },
            lr: Point { x: x1, y: y0 },
        }
    }

    fn pixels(doc: &Document) -> Vec<u8> {
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e}"));
        let res = doc.dict_get(&page, b"Resources");
        let xo = doc.dict_get(&res, b"XObject");
        let Object::Dict(d) = xo else {
            panic!("no xobjects")
        };
        let (_, im) = d.iter().next().unwrap_or_else(|| panic!("no image"));
        let img = crate::image::load(doc, im).unwrap_or_else(|e| panic!("{e}"));
        crate::pixmap::from_image(doc, &img)
            .unwrap_or_else(|e| panic!("{e}"))
            .samples
    }

    #[test]
    fn pixels_under_an_area_are_blanked() {
        let doc = doc();
        let opts = RedactOptions {
            black_boxes: false,
            image_method: ImageMethod::Pixels,
            line_art: LineArt::None,
            text: TextMethod::Remove,
        };
        // Columns 0..2, the top two rows (PDF y 710..730 is rows 0..2).
        assert_eq!(
            redact_page(&doc, 0, &[quad(72.0, 712.0, 140.0, 728.0)], opts),
            Ok(true)
        );
        let px = pixels(&doc);
        assert_eq!(&px[0..4], &[255, 255, 0x40, 0x40]);
        assert_eq!(&px[4..8], &[255, 255, 0x40, 0x40]);
        assert_eq!(&px[8..16], &[0x40; 8]);
    }

    #[test]
    fn an_image_inside_an_area_goes_whole() {
        let doc = doc();
        let opts = RedactOptions::default();
        assert_eq!(
            redact_page(&doc, 0, &[quad(0.0, 600.0, 400.0, 760.0)], opts),
            Ok(true)
        );
        let (data, _) = crate::content_clean::tests::page_contents(&doc);
        assert!(!data.contains("Do"), "{data}");
    }
}
