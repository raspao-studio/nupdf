// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-clean.c (pdf_redact_image_filter_pixels,
// pdf_redact_image_imp), source/pdf/pdf-image.c (pdf_add_image) and
// source/fitz/geometry.c (fz_is_quad_inside_quad),
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! Pixel redaction: an image an area covers goes whole; one an area only
//! meets is decoded, the pixels under each area turned white (or, for a
//! soft mask or an image mask, opaque or unpainted), and written back: an
//! image XObject as a new one of 8-bit samples (a bit each for black and
//! white), which the writer then deflates, an inline image inline.

use std::sync::Arc;

use crate::colorspace::ColorSpace;
use crate::content_filter::{ImageAction, ShownImage};
use crate::content_write::{BiImage, Compression};
use crate::document::Document;
use crate::geometry::{Matrix, Point, Quad, Rect};
use crate::image::Image;
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
            | Some(ColorSpace::Icc(crate::colorspace::Family::Cmyk, _))
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

/// `pdf_redact_image_filter_pixels`. An image an area meets is decoded,
/// blanked under each area and drawn as a new image: an XObject as a new
/// image object, an inline image inline. An inline image whose samples
/// cannot be written inline again (JBIG2 or JPX data, a color key, a
/// colorspace `BI` names by resource) goes whole.
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
    let img = match image {
        ShownImage::XObject(obj) => crate::image::load(doc, obj)?,
        ShownImage::Inline(bi) => match inline_image(bi) {
            Some(img) => img,
            None => return Ok(ImageAction::Remove),
        },
    };
    if !img.image_mask && img.colorspace.is_none() {
        return Ok(ImageAction::Remove);
    }
    let Ok(mut pix) = crate::pixmap::from_image(doc, &img) else {
        return Ok(ImageAction::Remove);
    };
    if img.image_mask {
        // `fz_invert_pixmap_alpha`: 255 is now what the mask leaves
        // unpainted, which is what blanking makes it.
        for s in &mut pix.samples {
            *s = 255 - *s;
        }
    }
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
    let obj = match image {
        ShownImage::XObject(obj) => obj,
        ShownImage::Inline(_) => {
            return Ok(match inline_pixmap(&pix, img.image_mask) {
                Some(bi) => ImageAction::Replace(bi),
                None => ImageAction::Remove,
            });
        }
    };
    let smask = match mask {
        Some(m) => Some(add_pixmap(
            doc,
            &m,
            Some(Object::name(b"DeviceGray")),
            None,
        )?),
        None => None,
    };
    let cs = (!img.image_mask).then(|| colorspace_object(doc, obj, pix.colorspace.as_ref()));
    let num = add_pixmap(doc, &pix, cs, smask)?;
    Ok(ImageAction::ReplaceWith(num))
}

/// The filter and parameters `bi`'s data is still encoded with, as an
/// inline image's dictionary holds them; None for JBIG2 and JPX data.
fn inline_filter(bi: &BiImage) -> Option<Dict> {
    let mut d = Dict::new();
    let mut p = Dict::new();
    let int = |p: &mut Dict, k: &[u8], v: i32| p.put(k, Object::Int(i64::from(v)));
    let name = match &bi.compression {
        Compression::Raw => return Some(d),
        Compression::Jpeg {
            color_transform, ..
        } => {
            if *color_transform >= 0 {
                int(&mut p, b"ColorTransform", *color_transform);
            }
            &b"DCTDecode"[..]
        }
        Compression::Fax {
            k,
            end_of_line,
            encoded_byte_align,
            columns,
            rows,
            end_of_block,
            black_is_1,
        } => {
            int(&mut p, b"K", *k);
            int(&mut p, b"Columns", *columns);
            int(&mut p, b"Rows", *rows);
            p.put(b"EndOfLine", Object::Bool(*end_of_line));
            p.put(b"EncodedByteAlign", Object::Bool(*encoded_byte_align));
            p.put(b"EndOfBlock", Object::Bool(*end_of_block));
            p.put(b"BlackIs1", Object::Bool(*black_is_1));
            b"CCITTFaxDecode"
        }
        Compression::Rld => b"RunLengthDecode",
        Compression::Flate {
            predictor,
            columns,
            colors,
            bpc,
        }
        | Compression::Brotli {
            predictor,
            columns,
            colors,
            bpc,
        }
        | Compression::Lzw {
            predictor,
            columns,
            colors,
            bpc,
            ..
        } => {
            int(&mut p, b"Predictor", *predictor);
            int(&mut p, b"Columns", *columns);
            int(&mut p, b"Colors", *colors);
            int(&mut p, b"BitsPerComponent", *bpc);
            if let Compression::Lzw { early_change, .. } = &bi.compression {
                int(&mut p, b"EarlyChange", *early_change);
            }
            match &bi.compression {
                Compression::Flate { .. } => &b"FlateDecode"[..],
                Compression::Brotli { .. } => b"BrotliDecode",
                _ => b"LZWDecode",
            }
        }
        Compression::Jbig2 | Compression::Jpx => return None,
    };
    d.put(b"Filter", Object::name(name));
    d.put(b"DecodeParms", Object::Dict(Arc::new(p)));
    Some(d)
}

/// The image an inline image's `BI` shows, for decoding.
fn inline_image(bi: &BiImage) -> Option<Image> {
    let dict = inline_filter(bi)?;
    let n = bi.colorspace.as_ref().map_or(1, ColorSpace::n);
    Some(Image {
        width: bi.width,
        height: bi.height,
        bpc: bi.bpc,
        n,
        colorspace: bi.colorspace.clone(),
        image_mask: bi.image_mask,
        interpolate: bi.interpolate,
        decode: Some(bi.decode.clone()),
        colorkey: None,
        mask: None,
        intent: None,
        is_jpx: false,
        stream: None,
        inline: Some((dict, Arc::from(&bi.data[..]))),
    })
}

/// `pdf_out_BI` of the blanked pixmap: 8-bit samples, raw. An image
/// mask's samples are packed to a bit each, as an image mask's must be
/// (MuPDF writes them a byte each, which reads back wrong). None when
/// `BI` cannot name the colorspace.
fn inline_pixmap(pix: &Pixmap, image_mask: bool) -> Option<BiImage> {
    if image_mask {
        return Some(BiImage {
            width: pix.w as i32,
            height: pix.h as i32,
            bpc: 1,
            image_mask: true,
            colorspace: None,
            device: false,
            interpolate: false,
            decode: vec![0.0, 1.0],
            compression: Compression::Raw,
            data: pack_bits(pix),
        });
    }
    if pix.alpha
        || !matches!(
            pix.colorspace,
            Some(ColorSpace::Gray | ColorSpace::Rgb | ColorSpace::Cmyk)
        )
    {
        return None;
    }
    Some(BiImage {
        width: pix.w as i32,
        height: pix.h as i32,
        bpc: 8,
        image_mask: false,
        colorspace: pix.colorspace.clone(),
        device: true,
        interpolate: false,
        decode: (0..pix.n * 2).map(|i| (i & 1) as f32).collect(),
        compression: Compression::Raw,
        data: pix.samples.clone(),
    })
}

/// A one-channel pixmap a bit per sample, set where it is not 0, rows
/// padded to a byte.
fn pack_bits(pix: &Pixmap) -> Vec<u8> {
    let stride = pix.w.div_ceil(8);
    let mut out = vec![0u8; stride * pix.h];
    for (y, row) in pix.samples.chunks(pix.w.max(1)).take(pix.h).enumerate() {
        for (x, &s) in row.iter().enumerate() {
            if s > 0 {
                out[y * stride + (x >> 3)] |= 1 << (7 - (x & 7));
            }
        }
    }
    out
}

/// `fz_is_pixmap_monochrome`.
fn monochrome(pix: &Pixmap) -> bool {
    pix.n == 1 && pix.samples.iter().all(|&s| s == 0 || s == 255)
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
/// out of them, into a soft mask of its own unless `smask` is given), or
/// a bit each when it is one channel of black and white; an image mask
/// when `cs` is None.
fn add_pixmap(
    doc: &Document,
    pix: &Pixmap,
    cs: Option<Object>,
    smask: Option<i32>,
) -> Result<i32, String> {
    let mut d = Dict::new();
    d.put(b"Type", Object::name(b"XObject"));
    d.put(b"Subtype", Object::name(b"Image"));
    d.put(b"Width", Object::Int(pix.w as i64));
    d.put(b"Height", Object::Int(pix.h as i64));
    let data = if monochrome(pix) {
        d.put(b"BitsPerComponent", Object::Int(1));
        pack_bits(pix)
    } else {
        d.put(b"BitsPerComponent", Object::Int(8));
        // A pixmap that is only alpha (a soft mask) is its own samples.
        let colorants = pix.n - usize::from(pix.alpha);
        let alpha = pix.alpha && colorants > 0;
        let n = colorants.max(1);
        if alpha && smask.is_none() && cs.is_some() {
            let alpha = Pixmap {
                w: pix.w,
                h: pix.h,
                n: 1,
                alpha: false,
                colorspace: Some(ColorSpace::Gray),
                samples: pix
                    .samples
                    .chunks(pix.n.max(1))
                    .map(|px| px[pix.n - 1])
                    .collect(),
            };
            let s = add_pixmap(doc, &alpha, Some(Object::name(b"DeviceGray")), None)?;
            d.put(b"SMask", Object::Ref(ObjRef::new(s, 0)));
        }
        let mut data = Vec::with_capacity(pix.w * pix.h * n);
        for px in pix.samples.chunks(pix.n.max(1)) {
            if alpha {
                let a = i32::from(px[pix.n - 1]);
                let inva = if a != 0 { 255 * 256 / a } else { 0 };
                data.extend(px[..n].iter().map(|&s| ((i32::from(s) * inva) >> 8) as u8));
            } else {
                data.extend_from_slice(&px[..n]);
            }
        }
        data
    };
    match cs {
        Some(cs) => d.put(b"ColorSpace", cs),
        None => d.put(b"ImageMask", Object::Bool(true)),
    }
    if let Some(s) = smask {
        d.put(b"SMask", Object::Ref(ObjRef::new(s, 0)));
    }
    d.put(b"Length", Object::Int(data.len() as i64));
    // `pdf_find_image_resource`: an image added before with the same
    // samples is used again, so an image a page draws twice, blanked the
    // same way, becomes one new object.
    let same = doc
        .with_edits(|e| {
            e.objects
                .iter()
                .filter(|(num, obj)| {
                    matches!(obj, Object::Stream(s) if s.dict == d)
                        && e.data.get(num).is_some_and(|old| *old == data)
                })
                .map(|(&num, _)| num)
                .min()
        })
        .flatten();
    Ok(same.unwrap_or_else(|| doc.add_stream(d, data)))
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

    /// Each 2x2 image of a page, drawn 100 wide and high: CMYK at
    /// (100, 600); grey with a soft mask at (300, 600); a 1x1 RGB image
    /// under a 2x2 mask at (100, 400); indexed RGB at (300, 400); an image
    /// mask at (100, 200); an inline grey image at (300, 200); indexed
    /// Lab at (100, 0); grey with a color key mask at (300, 0).
    fn images() -> Document {
        use crate::testutil::stream;
        let img = "/Type/XObject/Subtype/Image/Width 2/Height 2/BitsPerComponent 8";
        crate::testutil::page_doc(
            b"q 100 0 0 100 100 600 cm /Im1 Do Q q 100 0 0 100 300 600 cm /Im2 Do Q \
              q 100 0 0 100 100 400 cm /Im3 Do Q q 100 0 0 100 300 400 cm /Im4 Do Q \
              q 100 0 0 100 100 200 cm /Im6 Do Q \
              q 100 0 0 100 300 200 cm BI /W 2 /H 2 /BPC 8 /CS /G ID \x10\x20\x30\x40 EI Q \
              q 100 0 0 100 100 0 cm /Im5 Do Q q 100 0 0 100 300 0 cm /Im7 Do Q",
            "/XObject<</Im1 5 0 R/Im2 6 0 R/Im3 8 0 R/Im4 10 0 R/Im5 11 0 R/Im6 12 0 R/Im7 13 0 R>>",
            &[
                stream(
                    &format!("{img}/ColorSpace/DeviceCMYK"),
                    &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
                ),
                stream(
                    &format!("{img}/ColorSpace/DeviceGray/SMask 7 0 R"),
                    &[0x11, 0x22, 0x33, 0x44],
                ),
                stream(
                    &format!("{img}/ColorSpace/DeviceGray"),
                    &[0x80, 0x90, 0xa0, 0xb0],
                ),
                stream(
                    "/Type/XObject/Subtype/Image/Width 1/Height 1/BitsPerComponent 8/ColorSpace/DeviceRGB/SMask 9 0 R",
                    &[1, 2, 3],
                ),
                stream(
                    &format!("{img}/ColorSpace/DeviceGray"),
                    &[0x80, 0x90, 0xa0, 0xb0],
                ),
                stream(
                    &format!("{img}/ColorSpace[/Indexed/DeviceRGB 1<ff000000ff00>]"),
                    &[0, 1, 1, 0],
                ),
                stream(
                    &format!(
                        "{img}/ColorSpace[/Indexed[/Lab<</WhitePoint[0.9505 1 1.089]>>]1<ff000000ff00>]"
                    ),
                    &[0, 1, 1, 0],
                ),
                stream(
                    "/Type/XObject/Subtype/Image/Width 2/Height 2/ImageMask true",
                    &[0x40, 0x80],
                ),
                stream(
                    &format!("{img}/ColorSpace/DeviceGray/Mask[0 16]"),
                    &[0, 0x50, 0x60, 0x70],
                ),
            ],
        )
    }

    /// The image drawn as `name`: its dictionary and samples, and its soft
    /// mask's samples.
    fn image(doc: &Document, name: &[u8]) -> (String, Vec<u8>, Option<Vec<u8>>) {
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e}"));
        let res = doc.dict_get(&page, b"Resources");
        let obj = doc.dict_get(&doc.dict_get(&res, b"XObject"), name);
        let Object::Stream(s) = obj else {
            panic!("no image {}", String::from_utf8_lossy(name))
        };
        let data = doc.stream_data(&s).unwrap_or_else(|e| panic!("{e:?}"));
        let mut d = s.dict.clone();
        d.remove(b"Length");
        let smask = match doc.dict_get(&Object::Stream(s.clone()), b"SMask") {
            Object::Stream(m) => Some(doc.stream_data(&m).unwrap_or_else(|e| panic!("{e:?}"))),
            _ => None,
        };
        let dict = crate::print::to_string(
            &Object::Dict(std::sync::Arc::new(d)),
            crate::print::Options::default(),
        );
        (dict, data, smask)
    }

    #[test]
    fn each_kind_of_image_is_blanked_or_removed() {
        let doc = images();
        // The left column of each image.
        let left = |x: f32, y: f32| quad(x - 10.0, y - 10.0, x + 50.0, y + 110.0);
        let areas = [
            left(100.0, 600.0),
            left(300.0, 600.0),
            left(100.0, 400.0),
            left(300.0, 400.0),
            left(100.0, 200.0),
            left(300.0, 200.0),
            left(100.0, 0.0),
            left(300.0, 0.0),
        ];
        assert_eq!(
            redact_page(&doc, 0, &areas, RedactOptions::default()),
            Ok(true)
        );
        let (data, _) = crate::content_clean::tests::page_contents(&doc);
        // Golden output from MuPDF's pdf_redact_page: each image drawn
        // under a name of its own, the inline one blanked in place.
        assert_eq!(
            data,
            "q 100 0 0 100 100 600 cm /Im1 Do Q q 100 0 0 100 300 600 cm /Im2 Do Q \
             q 100 0 0 100 100 400 cm /Im3 Do Q q 100 0 0 100 300 400 cm /Im4 Do Q \
             q 100 0 0 100 100 200 cm /Im5 Do Q \
             q 100 0 0 100 300 200 cm BI /W 2/H 2/BPC 8/CS/G/D[0 1]/F/AHx ID ff20ff40> EI Q \
             q 100 0 0 100 100 0 cm /Im6 Do Q q 100 0 0 100 300 0 cm /Im7 Do Q q Q"
        );
        // MuPDF's samples, image by image. Its colorspaces differ: it
        // writes ICC profiles for the device spaces.
        let img = "/Type/XObject/Subtype/Image/Width 2/Height 2/BitsPerComponent";
        let mask = Some(vec![0xff, 0x90, 0xff, 0xb0]);
        type Shown = (&'static [u8], String, Vec<u8>, Option<Vec<u8>>);
        let expected: [Shown; 7] = [
            // CMYK blanks to 0.
            (
                b"Im1",
                format!("<<{img} 8/ColorSpace/DeviceCMYK>>"),
                vec![0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0, 13, 14, 15, 16],
                None,
            ),
            // The soft mask is made opaque where the image is blanked.
            (
                b"Im2",
                format!("<<{img} 8/ColorSpace/DeviceGray/SMask 15 0 R>>"),
                vec![0xff, 0x22, 0xff, 0x44],
                mask.clone(),
            ),
            // A 1x1 image keeps its color, and shares the mask blanked the
            // same way.
            (
                b"Im3",
                "<</Type/XObject/Subtype/Image/Width 1/Height 1/BitsPerComponent 8/ColorSpace/DeviceRGB/SMask 15 0 R>>".into(),
                vec![1, 2, 3],
                mask,
            ),
            // Indexed images are written in their base space.
            (
                b"Im4",
                format!("<<{img} 8/ColorSpace/DeviceRGB>>"),
                vec![255, 255, 255, 0, 255, 0, 255, 255, 255, 255, 0, 0],
                None,
            ),
            // An image mask is left unpainted, a bit a sample.
            (
                b"Im5",
                format!("<<{img} 1/ImageMask true>>"),
                vec![0xc0, 0x80],
                None,
            ),
            (
                b"Im6",
                format!("<<{img} 8/ColorSpace[/Lab<</WhitePoint[.9505 1 1.089]>>]>>"),
                vec![255, 255, 255, 0, 255, 0, 255, 255, 255, 255, 0, 0],
                None,
            ),
            // A color key becomes a soft mask, black and white.
            (
                b"Im7",
                format!("<<{img} 8/SMask 21 0 R/ColorSpace/DeviceGray>>"),
                vec![0xff, 0x50, 0xff, 0x70],
                Some(vec![0xc0, 0xc0]),
            ),
        ];
        for (name, dict, data, smask) in expected {
            assert_eq!(image(&doc, name), (dict, data, smask));
        }
    }

    #[test]
    fn inline_images_are_blanked_where_they_are() {
        // Inline, each 2x2 and 100 wide: an image mask at (100, 600),
        // deflated RGB at (300, 600), grey at (100, 400), Lab at (300, 400)
        // and grey drawn whole under an area at (100, 200).
        let rgb = crate::deflate::compress(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 9, 9, 9], 6)
            .unwrap_or_else(|| panic!("deflate"));
        let content = [
            &b"q 100 0 0 100 100 600 cm BI /W 2 /H 2 /IM true ID \x40\x80 EI Q "[..],
            b"q 100 0 0 100 300 600 cm BI /W 2 /H 2 /BPC 8 /CS /RGB /F /Fl ID ",
            &rgb,
            b" EI Q q 100 0 0 100 100 400 cm BI /W 2 /H 2 /BPC 8 /CS /G ID \x00\xff\xff\x00 EI Q ",
            b"q 100 0 0 100 300 400 cm BI /W 2 /H 2 /BPC 8 /CS /L ID 012345678901 EI Q ",
            b"q 100 0 0 100 100 200 cm BI /W 2 /H 2 /BPC 8 /CS /G ID 0123 EI Q",
        ]
        .concat();
        let doc = crate::testutil::page_doc(
            &content,
            "/ColorSpace<</L[/Lab<</WhitePoint[0.9505 1 1.089]>>]>>",
            &[],
        );
        let left = |x: f32, y: f32| quad(x - 10.0, y - 10.0, x + 50.0, y + 110.0);
        let areas = [
            left(100.0, 600.0),
            left(300.0, 600.0),
            left(100.0, 400.0),
            left(300.0, 400.0),
            quad(90.0, 190.0, 210.0, 310.0),
        ];
        assert_eq!(
            redact_page(&doc, 0, &areas, RedactOptions::default()),
            Ok(true)
        );
        // Golden output from MuPDF for the RGB and grey images. MuPDF
        // writes the image mask's samples a byte each, which reads back
        // as a different mask; nupdf packs them, as an image mask's samples
        // must be. Lab, which BI names by resource, goes whole.
        assert_eq!(
            crate::content_clean::tests::page_contents(&doc).0,
            "q 100 0 0 100 100 600 cm BI /W 2/H 2/BPC 1/IM true/D[0 1]/F/AHx ID c080> EI Q \
             q 100 0 0 100 300 600 cm BI /W 2/H 2/BPC 8/CS/RGB/D[0 1 0 1 0 1]/F/AHx ID ffffff00ff00ffffff090909> EI Q \
             q 100 0 0 100 100 400 cm BI /W 2/H 2/BPC 8/CS/G/D[0 1]/F/AHx ID ffffff00> EI Q \
             q 100 0 0 100 300 400 cm Q q 100 0 0 100 100 200 cm Q q Q"
        );
    }

    #[test]
    fn a_degenerate_area_holds_only_its_own_points() {
        let p = |x: f32, y: f32| Point { x, y };
        let line = Quad {
            ul: p(0.0, 0.0),
            ur: p(10.0, 0.0),
            ll: p(0.0, 0.0),
            lr: p(10.0, 0.0),
        };
        let at = |x: f32, y: f32| Quad {
            ul: p(x, y),
            ur: p(x, y),
            ll: p(x, y),
            lr: p(x, y),
        };
        assert!(quad_inside(&at(0.0, 0.0), &line));
        assert!(!quad_inside(&at(5.0, 1.0), &line));
        let nan = at(f32::NAN, 0.0);
        assert!(!quad_inside(&nan, &line));
        assert!(!quad_inside(&at(0.0, 0.0), &nan));
    }

    /// Object 6 of a smoke fixture: its raw data, and its samples as a
    /// grey image `w` wide, decoded (none for an image that fails to).
    fn smoke_stream(name: &str) -> (Vec<u8>, usize, Vec<u8>) {
        let path = format!(
            "{}/../../corpus/smoke/{name}.pdf",
            env!("CARGO_MANIFEST_DIR")
        );
        let doc = Document::open(std::fs::read(path).unwrap_or_else(|e| panic!("{e}")))
            .unwrap_or_else(|e| panic!("{e:?}"));
        let obj = Object::Ref(ObjRef::new(6, 0));
        let Object::Stream(s) = doc.resolve(&obj) else {
            panic!("no stream")
        };
        let raw = doc
            .raw_stream_data(&s)
            .unwrap_or_else(|e| panic!("{e}"))
            .into_owned();
        match doc.dict_get(&obj, b"Width") {
            Object::Int(w) => {
                // No samples for an image that does not decode.
                let samples = crate::image::load(&doc, &obj)
                    .and_then(|img| crate::pixmap::from_image(&doc, &img))
                    .map_or(Vec::new(), |pix| pix.samples);
                (raw, w as usize, samples)
            }
            _ => {
                let data = doc.stream_data(&s).unwrap_or_else(|e| panic!("{e:?}"));
                let w = match doc.dict_get(&doc.dict_get(&obj, b"DecodeParms"), b"Columns") {
                    Object::Int(c) => c as usize,
                    _ => data.len(),
                };
                (raw, w, data)
            }
        }
    }

    #[test]
    fn inline_images_are_decoded_whatever_their_filter() {
        for (fixture, filter) in [
            ("filter-lzw", "/F/LZW"),
            ("filter-lzw-early-change-0", "/F/LZW/DP<</EarlyChange 0>>"),
            ("filter-runlength", "/F/RL"),
            ("filter-brotli", "/F/Br"),
            (
                "filter-png-predictor",
                "/F/Fl/DP<</Predictor 14/Columns 4>>",
            ),
            ("filter-dct-gray", "/F/DCT/DP<</ColorTransform 0>>"),
            ("filter-ccitt-g4", "/F/CCF/DP<</K -1/Columns 97/Rows 60>>"),
        ] {
            let (raw, w, samples) = smoke_stream(fixture);
            let h = samples.len() / w;
            let bpc = if fixture.contains("ccitt") { 1 } else { 8 };
            let content = [
                format!("q {w} 0 0 {h} 100 100 cm BI /W {w} /H {h} /BPC {bpc} /CS /G {filter} ID ")
                    .as_bytes(),
                &raw,
                b" EI Q",
            ]
            .concat();
            let doc = crate::testutil::page_doc(&content, "", &[]);
            // The left half of the columns.
            let k = w / 2;
            let area = quad(90.0, 90.0, 100.0 + k as f32, 110.0 + h as f32);
            assert_eq!(
                redact_page(&doc, 0, &[area], RedactOptions::default()),
                Ok(true),
                "{fixture}"
            );
            let (data, _) = crate::content_clean::tests::page_contents(&doc);
            let hex = data
                .split_once("/F/AHx ID ")
                .and_then(|(_, rest)| rest.split_once('>'))
                .map(|(hex, _)| hex.replace('\n', ""))
                .unwrap_or_else(|| panic!("{fixture}: {data}"));
            let got: Vec<u8> = (0..hex.len() / 2)
                .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0))
                .collect();
            let mut expected = samples.clone();
            for row in expected.chunks_mut(w) {
                row[..k].fill(255);
            }
            assert_eq!(got, expected, "{fixture}");
        }
    }

    #[test]
    fn inline_jpx_goes_whole() {
        let (raw, _, _) = smoke_stream("filter-jpx");
        let content = [
            &b"q 8 0 0 8 100 100 cm BI /W 8 /H 8 /BPC 8 /CS /G /F /JPXDecode ID "[..],
            &raw,
            b" EI Q",
        ]
        .concat();
        let doc = crate::testutil::page_doc(&content, "", &[]);
        let area = quad(90.0, 90.0, 104.0, 120.0);
        assert_eq!(
            redact_page(&doc, 0, &[area], RedactOptions::default()),
            Ok(true)
        );
        assert_eq!(
            crate::content_clean::tests::page_contents(&doc).0,
            "q 8 0 0 8 100 100 cm Q q Q"
        );
    }
}
