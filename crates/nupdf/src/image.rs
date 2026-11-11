// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-image.c and source/fitz/load-jpx.c
// (the header MuPDF reads), Copyright (C) 2004-2026 Artifex Software, Inc.

//! Images as `pdf_load_image` loads them: their shape, colorspace, decode
//! array, mask and intent, and the failures MuPDF throws while loading
//! one. The samples are not decoded here.
//!
//! JPEG 2000 images are judged by their header: the JP2 image header and
//! colour specification boxes, or a bare codestream's SIZ segment, give
//! the size, components and depth.

use crate::colorspace::{self, ColorSpace, MAX_COLORS};
use crate::content::InlineImage;
use crate::diag::report;
use crate::document::Document;
use crate::object::Object;

/// `fz_image`, without its samples.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub width: i32,
    pub height: i32,
    pub bpc: i32,
    /// Components per pixel: the colorspace's, or 1.
    pub n: usize,
    pub colorspace: Option<ColorSpace>,
    pub image_mask: bool,
    pub interpolate: bool,
    pub decode: Option<Vec<f32>>,
    pub colorkey: Option<Vec<i32>>,
    pub mask: Option<Box<Image>>,
    /// The /Intent, as MuPDF's rendering intent number.
    pub intent: Option<i32>,
    /// From the inline image's dictionary or the XObject's stream.
    pub is_jpx: bool,
}

/// `fz_lookup_rendering_intent` of an image's /Intent.
fn intent(doc: &Document, v: Option<&Object>) -> Option<i32> {
    match v.map(|v| doc.resolve(v)) {
        Some(Object::Name(n)) => match n.as_ref() {
            b"Perceptual" => Some(0),
            b"RelativeColorimetric" => Some(1),
            b"Saturation" => Some(2),
            b"AbsoluteColorimetric" => Some(3),
            _ => None,
        },
        _ => None,
    }
}

/// `pdf_is_jpx_image`: /Filter (not /F) names JPXDecode.
fn is_jpx(doc: &Document, obj: &Object) -> bool {
    match doc.dict_get(obj, b"Filter") {
        Object::Name(n) => n.as_ref() == b"JPXDecode",
        Object::Array(a) => a.iter().any(|f| doc.resolve(f).is_name(b"JPXDecode")),
        _ => false,
    }
}

/// `pdf_load_image`: an image XObject.
pub fn load(doc: &Document, obj: &Object) -> Result<Image, String> {
    load_imp(doc, obj, false)
}

/// `pdf_load_image_imp`; `forcemask` loads a soft mask, whose own mask is
/// ignored.
fn load_imp(doc: &Document, obj: &Object, forcemask: bool) -> Result<Image, String> {
    if is_jpx(doc, obj) {
        return load_jpx(doc, obj, forcemask);
    }
    let get = |key: &[u8]| match doc.resolve(obj).as_dict().and_then(|d| d.get(key)) {
        Some(Object::Null) | None => None,
        Some(v) => Some(v.clone()),
    };
    let geta = |key: &[u8], abbrev: &[u8]| {
        get(abbrev)
            .or_else(|| get(key))
            .map_or(Object::Null, |v| doc.resolve(&v))
    };
    let width = geta(b"Width", b"W").to_int();
    let height = geta(b"Height", b"H").to_int();
    let mut bpc = geta(b"BitsPerComponent", b"BPC").to_int();
    if bpc == 0 {
        bpc = 8;
    }
    let image_mask = matches!(geta(b"ImageMask", b"IM"), Object::Bool(true));
    let interpolate = matches!(geta(b"Interpolate", b"I"), Object::Bool(true));
    if image_mask {
        bpc = 1;
    }
    check_size(width, height, bpc)?;
    let mut colorspace = None;
    let mut n = 1;
    if let Some(cs) = get(b"CS").or_else(|| get(b"ColorSpace"))
        && !image_mask
        && !forcemask
    {
        let cs = colorspace::load(doc, &cs)?;
        n = cs.n();
        colorspace = Some(cs);
    }
    let area = (bpc as u128).div_ceil(8) * width as u128 * height as u128 * n as u128;
    if area > u64::MAX as u128 {
        return Err(report::syntax("image is too large"));
    }
    let decode = decode_array(doc, &geta(b"Decode", b"D"), colorspace.as_ref(), bpc, n);
    let mut mask_obj = get(b"SMask");
    if mask_obj
        .as_ref()
        .is_none_or(|m| doc.resolve(m).as_dict().is_none())
    {
        mask_obj = get(b"Mask");
    }
    let mut mask = None;
    let mut colorkey = None;
    match mask_obj.as_ref().map(|m| (m, doc.resolve(m))) {
        Some((m, r)) if r.as_dict().is_some() => {
            if forcemask {
                doc.diagnostics()
                    .warn("image.recursive_mask", "Ignoring recursive image soft mask");
            } else {
                mask = Some(Box::new(load_imp(doc, m, true)?));
                // A matte color keys the image.
                if let Object::Array(matte) = doc.dict_get(m, b"Matte") {
                    let mut key = vec![0; n * 2];
                    for i in 0..n {
                        let v = matte.get(i).map_or(0.0, |v| doc.resolve(v).to_real());
                        let v = v.clamp(0.0, 1.0);
                        key[i * 2] = (v * 255.0) as i32;
                        key[i * 2 + 1] = key[i * 2];
                    }
                    colorkey = Some(key);
                }
            }
        }
        Some((_, Object::Array(key))) => {
            colorkey = color_key(doc, &key, n, true);
        }
        _ => {}
    }
    // pdf_load_compressed_stream: the image must be a stream.
    match obj.as_ref().and_then(|r| doc.object(r.num)) {
        Some(Object::Stream(_)) => {}
        _ => return Err(report::format("object is not a stream")),
    }
    Ok(Image {
        width,
        height,
        bpc,
        n,
        colorspace,
        image_mask,
        interpolate,
        decode,
        colorkey,
        mask,
        intent: intent(doc, get(b"Intent").as_ref()),
        is_jpx: false,
    })
}

fn check_size(width: i32, height: i32, bpc: i32) -> Result<(), String> {
    if width <= 0 {
        return Err(report::syntax("image width is zero (or less)"));
    }
    if height <= 0 {
        return Err(report::syntax("image height is zero (or less)"));
    }
    if bpc <= 0 {
        return Err(report::syntax("image depth is zero (or less)"));
    }
    if bpc > 16 {
        return Err(report::syntax(&format!("image depth is too large: {bpc}")));
    }
    Ok(())
}

/// `pdf_load_jpx_as_compressed_image` and its mask variant.
fn load_jpx(doc: &Document, obj: &Object, forcemask: bool) -> Result<Image, String> {
    let data = match doc.resolve(obj) {
        Object::Stream(s) => doc.stream_data(&s).map_err(|e| e.report())?,
        _ => return Err(report::format("object is not a stream")),
    };
    load_jpx_data(doc, obj, &data, forcemask).map_err(to_syntax)
}

/// `pdf_load_jpx_as_compressed_image` (and its mask variant) once the
/// stream is read; errors are as MuPDF throws them before it morphs them.
fn load_jpx_data(
    doc: &Document,
    obj: &Object,
    data: &[u8],
    forcemask: bool,
) -> Result<Image, String> {
    let get = |key: &[u8]| match doc.resolve(obj).as_dict().and_then(|d| d.get(key)) {
        Some(Object::Null) | None => None,
        Some(v) => Some(v.clone()),
    };
    let defcs = if forcemask {
        Some(ColorSpace::Gray)
    } else {
        match get(b"CS").or_else(|| get(b"ColorSpace")) {
            Some(c) => Some(colorspace::load(doc, &c)?),
            None => None,
        }
    };
    let colorspace = jpx_colorspace(doc, data, defcs.as_ref())?;
    let (width, height, cs) = colorspace;
    let n = cs.n();
    let mut img = Image {
        width,
        height,
        bpc: 8,
        n,
        colorspace: Some(cs),
        image_mask: false,
        interpolate: false,
        decode: None,
        colorkey: None,
        mask: None,
        intent: None,
        is_jpx: true,
    };
    img.decode = decode_with_default(doc, &Object::Null, img.colorspace.as_ref(), 8, n, false);
    if forcemask {
        return Ok(img);
    }
    if let Some(d) = get(b"D").or_else(|| get(b"Decode")) {
        let d = doc.resolve(&d);
        let a = d.as_array().unwrap_or_default();
        img.decode = Some(
            (0..n * 2)
                .map(|i| a.get(i).map_or(0.0, |v| doc.resolve(v).to_real()))
                .collect(),
        );
    }
    match get(b"Mask")
        .or_else(|| get(b"SMask"))
        .map(|m| (doc.resolve(&m), m))
    {
        Some((r, m)) if r.as_dict().is_some() => {
            img.mask = Some(Box::new(load_imp(doc, &m, true)?));
        }
        Some((Object::Array(key), _)) => {
            img.colorkey = color_key(doc, &key, n, true);
        }
        _ => {}
    }
    Ok(img)
}

/// `fz_load_jpx_info`: the size and colorspace MuPDF gives a JPEG 2000
/// image from its header, the dictionary's colorspace `defcs` and an
/// embedded ICC profile.
fn jpx_colorspace(
    doc: &Document,
    data: &[u8],
    defcs: Option<&ColorSpace>,
) -> Result<(i32, i32, ColorSpace), String> {
    let diag = doc.diagnostics();
    if data.len() < 2 {
        return Err(report::format("not enough data to determine image format"));
    }
    let info =
        jpx_info(data).ok_or_else(|| "library error: Failed to read JPX header".to_string())?;
    let numcomps = info.numcomps;
    let mut n = match info.color_space {
        JpxSpace::Srgb => 3,
        JpxSpace::Gray => 1,
        JpxSpace::Cmyk => 4,
        JpxSpace::Unknown => match defcs {
            Some(cs) if cs.n() <= numcomps => cs.n(),
            _ if numcomps >= 3 => {
                diag.warn(
                    "image.jpx_colorspace",
                    "unknown JPX colorspace; assuming RGB",
                );
                3
            }
            _ => {
                diag.warn(
                    "image.jpx_colorspace",
                    "unknown JPX colorspace; assuming Gray",
                );
                1
            }
        },
    };
    if n > numcomps {
        diag.warn(
            "image.jpx_colorspace",
            format!("JPX numcomps ({numcomps}) doesn't match color_space ({n})"),
        );
        n = numcomps;
    }
    let mut cs = None;
    if let Some(d) = defcs {
        if d.n() == n {
            cs = Some(d.clone());
        } else {
            diag.warn(
                "image.jpx_colorspace",
                "jpx file and dict colorspace do not match",
            );
        }
    }
    if cs.is_none()
        && let Some(icc) = info.icc.filter(|p| !p.is_empty())
    {
        match colorspace::icc_colorspace(doc, icc, None) {
            Ok(family) => {
                if family.n() == n {
                    cs = Some(ColorSpace::Icc(family));
                } else {
                    diag.warn(
                        "image.jpx_icc",
                        "invalid number of components in ICC profile, ignoring ICC profile in JPX",
                    );
                }
            }
            Err(line) => {
                diag.warn("error.reported", line);
                diag.warn("image.jpx_icc", "ignoring embedded ICC profile in JPX");
            }
        }
    }
    let cs = match cs {
        Some(cs) => cs,
        None => match n {
            1 => ColorSpace::Gray,
            3 => ColorSpace::Rgb,
            4 => ColorSpace::Cmyk,
            _ => {
                return Err(report::format(&format!(
                    "unsupported number of components: {n}"
                )));
            }
        },
    };
    Ok((info.width, info.height, cs))
}

/// JPX loading failures are morphed into syntax errors.
fn to_syntax(line: String) -> String {
    match line
        .strip_prefix("format error: ")
        .or_else(|| line.strip_prefix("library error: "))
    {
        Some(rest) => report::syntax(rest),
        None => line,
    }
}

/// The colour spaces openjpeg names from a JP2 colour specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JpxSpace {
    Unknown,
    /// sRGB, sYCC or e-sYCC.
    Srgb,
    Gray,
    Cmyk,
}

struct JpxInfo<'a> {
    width: i32,
    height: i32,
    numcomps: usize,
    color_space: JpxSpace,
    icc: Option<&'a [u8]>,
}

/// What `opj_read_header` gives: the codestream's size and components
/// (its SIZ segment), and for a JP2 file the colour specification box.
fn jpx_info(data: &[u8]) -> Option<JpxInfo<'_>> {
    let be32 = |d: &[u8], at: usize| -> Option<u32> {
        d.get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let be16 = |d: &[u8], at: usize| -> Option<u16> {
        d.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    };
    let siz = |cs: &[u8]| -> Option<(i32, i32, usize)> {
        if !cs.starts_with(&[0xff, 0x4f, 0xff, 0x51]) {
            return None;
        }
        let x1 = be32(cs, 8)?;
        let y1 = be32(cs, 12)?;
        let x0 = be32(cs, 16)?;
        let y0 = be32(cs, 20)?;
        let numcomps = usize::from(be16(cs, 40)?);
        if x0 >= x1 || y0 >= y1 || numcomps == 0 || numcomps > 16384 {
            return None;
        }
        let w = i32::try_from(x1 - x0).ok()?;
        let h = i32::try_from(y1 - y0).ok()?;
        Some((w, h, numcomps))
    };
    if data.starts_with(&[0xff, 0x4f]) {
        let (width, height, numcomps) = siz(data)?;
        return Some(JpxInfo {
            width,
            height,
            numcomps,
            color_space: JpxSpace::Unknown,
            icc: None,
        });
    }
    // JP2 boxes up to the codestream, the header superbox entered.
    let mut at = 0usize;
    let mut end = data.len();
    let mut steps = 0;
    let mut has_ihdr = false;
    let mut colr: Option<(JpxSpace, Option<&[u8]>)> = None;
    while at + 8 <= end && steps < 10_000 {
        steps += 1;
        let len = be32(data, at)? as usize;
        let kind = data.get(at + 4..at + 8)?;
        let (body, next) = match len {
            0 => (at + 8, end),
            1 => {
                if be32(data, at + 8)? != 0 {
                    return None;
                }
                (at + 16, at.checked_add(be32(data, at + 12)? as usize)?)
            }
            l if l < 8 => return None,
            l => (at + 8, at.checked_add(l)?),
        };
        let next = next.min(data.len());
        match kind {
            b"jp2h" => {
                at = body;
                end = next;
                continue;
            }
            b"ihdr" => has_ihdr = true,
            b"colr" if colr.is_none() => {
                let meth = *data.get(body)?;
                colr = Some(match meth {
                    1 => (
                        match be32(data, body + 3)? {
                            16 | 18 | 24 => JpxSpace::Srgb,
                            17 => JpxSpace::Gray,
                            12 => JpxSpace::Cmyk,
                            _ => JpxSpace::Unknown,
                        },
                        None,
                    ),
                    2 => (JpxSpace::Unknown, data.get(body + 3..next)),
                    _ => (JpxSpace::Unknown, None),
                });
            }
            b"jp2c" => {
                if !has_ihdr {
                    return None;
                }
                let (width, height, numcomps) = siz(data.get(body..next)?)?;
                let (color_space, icc) = colr.unwrap_or((JpxSpace::Unknown, None));
                return Some(JpxInfo {
                    width,
                    height,
                    numcomps,
                    color_space,
                    icc,
                });
            }
            _ => {}
        }
        if next <= at {
            return None;
        }
        at = next;
        if at >= end && end < data.len() {
            // Past the header superbox: back to the top level.
            end = data.len();
        }
    }
    None
}

/// The image `BI` makes, as `pdf_load_inline_image` made it (its color key
/// was checked, and warned about, when it was read).
pub fn from_inline(doc: &Document, image: &InlineImage) -> Image {
    let get = |key: &[u8]| match image.dict.get(key) {
        Some(Object::Null) | None => None,
        Some(v) => Some(doc.resolve(v)),
    };
    let geta = |key: &[u8], abbrev: &[u8]| get(abbrev).or_else(|| get(key)).unwrap_or(Object::Null);
    let n = image.n;
    let mut key_obj = get(b"SMask");
    if key_obj.as_ref().is_none_or(|m| m.as_dict().is_none()) {
        key_obj = get(b"Mask");
    }
    let colorkey = match key_obj {
        Some(Object::Array(key)) => color_key(doc, &key, n, false),
        _ => None,
    };
    Image {
        width: image.width,
        height: image.height,
        bpc: image.bpc,
        n,
        colorspace: image.colorspace.clone(),
        image_mask: image.image_mask,
        interpolate: matches!(geta(b"Interpolate", b"I"), Object::Bool(true)),
        decode: decode_array(
            doc,
            &geta(b"Decode", b"D"),
            image.colorspace.as_ref(),
            image.bpc,
            n,
        ),
        colorkey,
        mask: None,
        intent: intent(doc, get(b"Intent").as_ref()),
        is_jpx: false,
    }
}

/// The decode array `fz_new_image_of_size` keeps, when it is not the
/// identity for each component (MuPDF's `use_decode`): /Decode, or the
/// default for the colorspace, with a Lab one taken back to 0..1.
fn decode_array(
    doc: &Document,
    obj: &Object,
    cs: Option<&ColorSpace>,
    bpc: i32,
    n: usize,
) -> Option<Vec<f32>> {
    decode_with_default(doc, obj, cs, bpc, n, true)
}

/// `pdf_default` is pdf_load_image's default for Lab (its full ranges);
/// without it the default is `fz_new_image_of_size`'s (0 to 1).
fn decode_with_default(
    doc: &Document,
    obj: &Object,
    cs: Option<&ColorSpace>,
    bpc: i32,
    n: usize,
    pdf_default: bool,
) -> Option<Vec<f32>> {
    let n = n.min(MAX_COLORS);
    let lab = cs.is_some_and(|c| {
        matches!(
            c,
            ColorSpace::Lab | ColorSpace::Icc(colorspace::Family::Lab)
        )
    });
    let mut d: Vec<f32> = match obj {
        Object::Null => {
            if lab && pdf_default {
                vec![0.0, 100.0, -128.0, 127.0, -128.0, 127.0]
            } else {
                let indexed = cs.is_some_and(ColorSpace::is_indexed);
                let maxval = if indexed {
                    ((1i64 << bpc.clamp(0, 31)) - 1) as f32
                } else {
                    1.0
                };
                (0..n * 2)
                    .map(|i| if i & 1 == 1 { maxval } else { 0.0 })
                    .collect()
            }
        }
        _ => {
            let a = doc.resolve(obj);
            let a = a.as_array().unwrap_or_default();
            (0..n * 2)
                .map(|i| a.get(i).map_or(0.0, |v| doc.resolve(v).to_real()))
                .collect()
        }
    };
    d.resize(n * 2, 0.0);
    if lab && d.len() >= 6 {
        d[0] /= 100.0;
        d[1] /= 100.0;
        for v in &mut d[2..6] {
            *v = (*v + 128.0) / 255.0;
        }
    }
    let identity = (0..n).all(|i| d[i * 2] == 0.0 && d[i * 2 + 1] == 1.0);
    (!identity).then_some(d)
}

/// A /Mask color key: None when a value is not an integer (warned about
/// when `warn`).
fn color_key(doc: &Document, key: &[Object], n: usize, warn: bool) -> Option<Vec<i32>> {
    let mut ok = true;
    let mut values = Vec::new();
    for i in 0..n.min(MAX_COLORS) * 2 {
        let v = key.get(i).map(|v| doc.resolve(v));
        if !matches!(v, Some(Object::Int(_))) {
            if warn {
                doc.diagnostics()
                    .warn("image.color_key", "invalid value in color key mask");
            }
            ok = false;
        }
        values.push(v.map_or(0, |v| v.to_int()));
    }
    ok.then_some(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_arrays_are_kept_when_they_are_not_the_identity() {
        let d = Document::create().unwrap_or_else(|e| panic!("{e:?}"));
        let none = Object::Null;
        assert_eq!(decode_array(&d, &none, Some(&ColorSpace::Rgb), 8, 3), None);
        let indexed = ColorSpace::Indexed {
            base: Box::new(ColorSpace::Rgb),
            high: 1,
            lookup: vec![0; 6],
        };
        assert_eq!(
            decode_array(&d, &none, Some(&indexed), 4, 1),
            Some(vec![0.0, 15.0])
        );
        // Lab's default is taken back to 0..1, so it is the identity.
        assert_eq!(decode_array(&d, &none, Some(&ColorSpace::Lab), 8, 3), None);
        let inverted = Object::Array(std::sync::Arc::new(vec![Object::Int(1), Object::Int(0)]));
        assert_eq!(
            decode_array(&d, &inverted, None, 1, 1),
            Some(vec![1.0, 0.0])
        );
    }

    /// A bare codestream: SOC, then a SIZ segment for a `w` by `h` image
    /// of `n` components.
    fn codestream(w: u32, h: u32, n: u16) -> Vec<u8> {
        let mut c = vec![0xff, 0x4f, 0xff, 0x51, 0, 41, 0, 0];
        for v in [w, h, 0, 0, w, h, 0, 0] {
            c.extend_from_slice(&v.to_be_bytes());
        }
        c.extend_from_slice(&n.to_be_bytes());
        c
    }

    fn fbox(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    }

    #[test]
    fn jpx_headers_give_size_components_and_colour_space() {
        let j2k = codestream(3, 2, 4);
        let info = jpx_info(&j2k).unwrap_or_else(|| panic!("no header"));
        assert_eq!((info.width, info.height, info.numcomps), (3, 2, 4));
        assert_eq!(info.color_space, JpxSpace::Unknown);
        // A JP2 file: the colour box names sRGB.
        let mut colr = vec![1, 0, 0];
        colr.extend_from_slice(&16u32.to_be_bytes());
        let header = [fbox(b"ihdr", &[0; 14]), fbox(b"colr", &colr)].concat();
        let jp2 = [
            fbox(b"jP  ", &[0x0d, 0x0a, 0x87, 0x0a]),
            fbox(b"jp2h", &header),
            fbox(b"jp2c", &codestream(5, 7, 4)),
        ]
        .concat();
        let info = jpx_info(&jp2).unwrap_or_else(|| panic!("no header"));
        assert_eq!((info.width, info.height, info.numcomps), (5, 7, 4));
        assert_eq!(info.color_space, JpxSpace::Srgb);
        // Four components in sRGB: three colors and an alpha.
        let d = Document::create().unwrap_or_else(|e| panic!("{e:?}"));
        let (w, h, cs) = jpx_colorspace(&d, &jp2, None).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((w, h, cs), (5, 7, ColorSpace::Rgb));
        // No colour box and two components: gray, with a warning.
        let (_, _, cs) =
            jpx_colorspace(&d, &codestream(1, 1, 2), None).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(cs, ColorSpace::Gray);
    }
}
