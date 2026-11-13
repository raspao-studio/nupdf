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
    /// The XObject whose stream holds the samples; None for an inline
    /// image.
    pub stream: Option<Object>,
    /// An inline image's dictionary and its still encoded data.
    pub inline: Option<(crate::object::Dict, std::sync::Arc<[u8]>)>,
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
        stream: Some(obj.clone()),
        inline: None,
    })
}

/// The JPEG data of an image whose last filter is DCTDecode, as MuPDF's
/// compressed buffer holds it: the stream with its other filters undone.
/// None for an image with another last filter.
pub fn jpeg_data(doc: &Document, image: &Image) -> Result<Option<Vec<u8>>, String> {
    Ok(jpeg_parts(doc, image)?.map(|(data, _)| data))
}

/// `jpeg_data`, with the DCTDecode filter's parameters.
pub(crate) fn jpeg_parts(
    doc: &Document,
    image: &Image,
) -> Result<Option<(Vec<u8>, Object)>, String> {
    let Some(obj) = &image.stream else {
        return Ok(None);
    };
    let Object::Stream(s) = doc.resolve(obj) else {
        return Ok(None);
    };
    // pdf_dict_geta: the abbreviation first.
    let geta = |key: &[u8], abbrev: &[u8]| {
        s.dict
            .get(abbrev)
            .filter(|v| !matches!(v, Object::Null))
            .or_else(|| s.dict.get(key))
            .map_or(Object::Null, |v| doc.resolve(v))
    };
    let filters = geta(b"Filter", b"F");
    let is_dct = |o: &Object| o.is_name(b"DCTDecode") || o.is_name(b"DCT");
    let earlier = match &filters {
        f if is_dct(f) => Vec::new(),
        Object::Array(fs) => match fs.split_last() {
            Some((last, rest)) if is_dct(&doc.resolve(last)) => rest.to_vec(),
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    let params = geta(b"DecodeParms", b"DP");
    let last_params = match &params {
        Object::Array(ps) => ps
            .get(earlier.len())
            .map_or(Object::Null, |p| doc.resolve(p)),
        p if earlier.is_empty() => p.clone(),
        _ => Object::Null,
    };
    let raw = doc.raw_stream_data(&s)?;
    if earlier.is_empty() {
        return Ok(Some((raw.into_owned(), last_params)));
    }
    let mut dict = crate::object::Dict::new();
    dict.put(
        b"Filter",
        Object::Array(std::sync::Arc::new(earlier.clone())),
    );
    if let Object::Array(ps) = &params {
        let ps: Vec<Object> = ps.iter().take(earlier.len()).cloned().collect();
        dict.put(b"DecodeParms", Object::Array(std::sync::Arc::new(ps)));
    }
    let resolve = |o: &Object| doc.resolve(o);
    crate::filter::decode(&dict, &raw, &resolve, doc.diagnostics(), &doc.limits())
        .map(|d| Some((d, last_params)))
        .map_err(|e| e.report())
}

/// `compressed_image_get_pixmap`'s patch for JPEG data: a frame header
/// with no height, or more rows than the image has, gets the image's.
pub(crate) fn patch_jpeg_height(data: &mut [u8], height: i32) {
    let mut d = 2usize;
    while d + 9 < data.len() && data[d] == 0xFF {
        let m = data[d + 1];
        let sof = !(m < 0xC0 || (0xC3 < m && m < 0xC9) || 0xCB < m);
        if sof {
            let h = (i32::from(data[d + 5]) << 8) | i32::from(data[d + 6]);
            if h == 0 || h > height {
                data[d + 5] = ((height >> 8) & 0xFF) as u8;
                data[d + 6] = (height & 0xFF) as u8;
            }
        }
        let len = (usize::from(data[d + 2]) << 8) | usize::from(data[d + 3]);
        d += len + 2;
    }
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
        stream: Some(obj.clone()),
        inline: None,
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
                    cs = Some(ColorSpace::Icc(
                        family,
                        colorspace::IccData::new(std::sync::Arc::from(icc)),
                    ));
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
    // Inside the header superbox, where openjpeg skips a jp2c box as an
    // unknown sub-box.
    let mut in_header = false;
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
            // openjpeg reads the codestream from the end of the jp2c box
            // header on, whatever length the box claims.
            _ if kind == b"jp2c" && !in_header => (at + 8, data.len()),
            l if l < 8 => return None,
            l => (at + 8, at.checked_add(l)?),
        };
        let next = next.min(data.len());
        match kind {
            b"jp2h" => {
                at = body;
                in_header = true;
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
            b"jp2c" if !in_header => {
                if !has_ihdr {
                    return None;
                }
                let (width, height, numcomps) = siz(data.get(body..)?)?;
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
            in_header = false;
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
        stream: None,
        inline: Some((image.dict.clone(), std::sync::Arc::from(image.data))),
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
            ColorSpace::Lab | ColorSpace::Icc(colorspace::Family::Lab, _)
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
    fn jpeg_frame_heights_are_patched_to_the_image() {
        // SOI, an APP0 of 4 bytes, then SOF0 claiming 0x1234 rows, and SOS.
        let mut jpeg = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x12,
            0x34, 0x00, 0x10, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xDA, 0x00, 0x02,
        ];
        patch_jpeg_height(&mut jpeg, 100);
        assert_eq!(&jpeg[13..15], &[0x00, 100]);
        // A height within the image is kept.
        patch_jpeg_height(&mut jpeg, 200);
        assert_eq!(&jpeg[13..15], &[0x00, 100]);
        // Huffman tables (C4) are not frames.
        let mut dht = vec![
            0xFF, 0xD8, 0xFF, 0xC4, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let before = dht.clone();
        patch_jpeg_height(&mut dht, 1);
        assert_eq!(dht, before);
    }

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

    /// A PDF with `objects` as objects 2, 3, ..., with an xref table.
    fn pdf_of(objects: &[Vec<u8>]) -> Vec<u8> {
        let mut file = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        let catalog = b"<</Type/Catalog/Pages 2 0 R>>".to_vec();
        for (i, body) in std::iter::once(&catalog).chain(objects).enumerate() {
            offsets.push(file.len());
            file.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            file.extend_from_slice(body);
            file.extend_from_slice(b"\nendobj\n");
        }
        let xref = file.len();
        file.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
        );
        for o in offsets {
            file.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        file.extend_from_slice(
            format!(
                "trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 2
            )
            .as_bytes(),
        );
        file
    }

    fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
        let mut s = format!("<<{dict}/Length {}>>stream\n", data.len()).into_bytes();
        s.extend_from_slice(data);
        s.extend_from_slice(b"\nendstream");
        s
    }

    fn img(dict: &str) -> Vec<u8> {
        stream(dict, &[0; 4])
    }

    /// What tests/data/image/image.js prints for an image.
    fn summary(image: &Image) -> String {
        let num = |v: f32| ((v * 1e4).round() / 1e4).to_string();
        let list =
            |v: Option<Vec<String>>| v.map_or("-".to_string(), |v| format!("[{}]", v.join(" ")));
        let mut s = format!(
            "{}x{} bpc {} n {} cs {} im {} interp {} decode {} key {}",
            image.width,
            image.height,
            image.bpc,
            image.n,
            image.colorspace.as_ref().map_or("-", ColorSpace::type_name),
            image.image_mask,
            image.interpolate,
            list(
                image
                    .decode
                    .as_ref()
                    .map(|d| d.iter().map(|&v| num(v)).collect())
            ),
            list(
                image
                    .colorkey
                    .as_ref()
                    .map(|k| k.iter().map(i32::to_string).collect())
            ),
        );
        if let Some(m) = &image.mask {
            s += &format!(" mask ({})", summary(m));
        }
        s
    }

    macro_rules! jpx {
        ($name:literal) => {
            include_bytes!(concat!("../tests/data/image/", $name)).as_slice()
        };
    }

    /// `jp2` with its colour box replaced by one with method 2 (an ICC
    /// profile).
    fn with_icc(jp2: &[u8], profile: &[u8]) -> Vec<u8> {
        let at = jp2.windows(4).position(|w| w == b"colr").unwrap_or(0) - 4;
        let len = u32::from_be_bytes([jp2[at], jp2[at + 1], jp2[at + 2], jp2[at + 3]]) as usize;
        let mut body = vec![2, 0, 0];
        body.extend_from_slice(profile);
        let colr = fbox(b"colr", &body);
        let mut out = [&jp2[..at], &colr[..], &jp2[at + len..]].concat();
        // The header superbox grows by as much.
        let h = out.windows(4).position(|w| w == b"jp2h").unwrap_or(0) - 4;
        let hlen = u32::from_be_bytes([out[h], out[h + 1], out[h + 2], out[h + 3]]) as usize;
        let hlen = (hlen + colr.len() - len) as u32;
        out[h..h + 4].copy_from_slice(&hlen.to_be_bytes());
        out
    }

    /// An ICC profile header for `space` (see colorspace's tests).
    fn profile(space: &[u8; 4]) -> Vec<u8> {
        let mut p = vec![0u8; 132];
        p[0..4].copy_from_slice(&132u32.to_be_bytes());
        p[8] = 2;
        p[12..16].copy_from_slice(b"mntr");
        p[16..20].copy_from_slice(space);
        p[36..40].copy_from_slice(b"acsp");
        p
    }

    /// Image dictionaries, as objects 2, 3, ... of a file; object 2 is
    /// loaded.
    fn cases() -> Vec<(&'static str, Vec<Vec<u8>>)> {
        let jpx = |dict: &str, data: &[u8]| stream(&format!("/Filter/JPXDecode{dict}"), data);
        vec![
            (
                "rgb",
                vec![img(
                    "/Width 2/Height 2/ColorSpace/DeviceRGB/BitsPerComponent 8",
                )],
            ),
            (
                "abbreviations_first",
                vec![img(
                    "/W 3/Width 5/H 1/Height 9/BPC 4/BitsPerComponent 8/CS/G/D[1 0]/I true",
                )],
            ),
            (
                "default_depth",
                vec![img("/Width 1/Height 1/ColorSpace/DeviceGray")],
            ),
            (
                "image_mask",
                vec![img(
                    "/Width 4/Height 4/ImageMask true/BitsPerComponent 8/ColorSpace/DeviceRGB/Decode[1 0]",
                )],
            ),
            ("zero_width", vec![img("/Width 0/Height 4")]),
            ("zero_height", vec![img("/Width 4/Height -1")]),
            (
                "negative_depth",
                vec![img("/Width 4/Height 4/BitsPerComponent -2")],
            ),
            ("deep", vec![img("/Width 4/Height 4/BitsPerComponent 17")]),
            (
                "huge",
                vec![img(
                    "/Width 2147483647/Height 2147483647/BitsPerComponent 16/ColorSpace/DeviceCMYK",
                )],
            ),
            (
                "indexed_default_decode",
                vec![img(
                    "/Width 1/Height 1/BitsPerComponent 4/ColorSpace[/Indexed/DeviceRGB 1<000000ffffff>]",
                )],
            ),
            (
                "lab",
                vec![img(
                    "/Width 1/Height 1/ColorSpace[/Lab<<>>]/Decode[0 50 -100 100 0 127]",
                )],
            ),
            (
                "lab_default",
                vec![img("/Width 1/Height 1/ColorSpace[/Lab<<>>]")],
            ),
            (
                "short_decode",
                vec![img("/Width 1/Height 1/ColorSpace/DeviceRGB/Decode[0 1 1]")],
            ),
            (
                "soft_mask",
                vec![
                    img("/Width 2/Height 2/ColorSpace/DeviceRGB/SMask 3 0 R"),
                    img("/Width 2/Height 2/ColorSpace/DeviceGray/BitsPerComponent 8/Decode[1 0]"),
                ],
            ),
            (
                "matte",
                vec![
                    img("/Width 2/Height 2/ColorSpace/DeviceRGB/SMask 3 0 R"),
                    img("/Width 2/Height 2/ColorSpace/DeviceGray/Matte[0.5 2 -1]"),
                ],
            ),
            (
                "mask_of_a_mask",
                vec![
                    img("/Width 2/Height 2/ColorSpace/DeviceRGB/SMask 3 0 R"),
                    img("/Width 2/Height 2/ColorSpace/DeviceGray/SMask 4 0 R"),
                    img("/Width 2/Height 2/ColorSpace/DeviceGray"),
                ],
            ),
            (
                "stencil_mask",
                vec![
                    img("/Width 2/Height 2/ColorSpace/DeviceRGB/SMask/None/Mask 3 0 R"),
                    img("/Width 2/Height 2/ImageMask true"),
                ],
            ),
            (
                "color_key",
                vec![img(
                    "/Width 2/Height 2/ColorSpace/DeviceRGB/Mask[0 10 20 30 40 50]",
                )],
            ),
            (
                "bad_color_key",
                vec![img(
                    "/Width 2/Height 2/ColorSpace/DeviceRGB/Mask[0 10 20.5 30]",
                )],
            ),
            (
                "not_a_stream",
                vec![b"<</Width 2/Height 2/ColorSpace/DeviceRGB>>".to_vec()],
            ),
            (
                "bad_colorspace",
                vec![img("/Width 2/Height 2/ColorSpace/Foo")],
            ),
            // JPEG 2000: size and colour space from the file, the
            // dictionary's where they agree.
            ("jpx_rgb", vec![jpx("", jpx!("rgb.jp2"))]),
            ("jpx_codestream", vec![jpx("", jpx!("rgb.j2k"))]),
            (
                "jpx_codestream_cmyk_dict",
                vec![jpx("/ColorSpace/DeviceCMYK", jpx!("rgb.j2k"))],
            ),
            ("jpx_gray", vec![jpx("/Decode[1 0]", jpx!("gray.jp2"))]),
            ("jpx_rgba", vec![jpx("", jpx!("rgba.jp2"))]),
            (
                "jpx_cmyk",
                vec![jpx("/ColorSpace/DeviceGray", jpx!("cmyk.jp2"))],
            ),
            (
                "jpx_masks",
                vec![
                    jpx("/SMask 3 0 R/Mask[1 2 3 4 5 6]", jpx!("rgb.jp2")),
                    jpx("", jpx!("gray.jp2")),
                ],
            ),
            (
                "jpx_color_key",
                vec![jpx("/Mask[1 2 3 4 5 6]", jpx!("rgb.jp2"))],
            ),
            (
                "jpx_icc",
                vec![jpx("", &with_icc(jpx!("rgb.jp2"), &profile(b"RGB ")))],
            ),
            (
                "jpx_icc_gray",
                vec![jpx("", &with_icc(jpx!("rgb.jp2"), &profile(b"GRAY")))],
            ),
            (
                "jpx_icc_broken",
                vec![jpx("", &with_icc(jpx!("rgb.jp2"), b"junk"))],
            ),
            ("jpx_junk", vec![jpx("", b"\x00\x00\x00\x0cjunkjunkjunk")]),
            ("jpx_empty", vec![jpx("", b"x")]),
            ("jpx_not_a_stream", vec![b"<</Filter/JPXDecode>>".to_vec()]),
            (
                "jpx_bad_colorspace",
                vec![jpx("/ColorSpace/Foo", jpx!("rgb.jp2"))],
            ),
            (
                "jpx_filter_array",
                vec![stream("/Filter[/JPXDecode]", jpx!("gray.jp2"))],
            ),
            (
                "jpx_dict_colorspace",
                vec![jpx("/ColorSpace/CalRGB", jpx!("rgb.jp2"))],
            ),
            (
                "jpx_as_soft_mask",
                vec![
                    img("/Width 2/Height 2/ColorSpace/DeviceRGB/SMask 3 0 R"),
                    jpx("/ColorSpace/DeviceRGB/Decode[1 0 1 0 1 0]", jpx!("rgb.jp2")),
                ],
            ),
            (
                "jpx_soft_mask",
                vec![
                    jpx("/SMask 3 0 R", jpx!("rgb.jp2")),
                    img("/Width 2/Height 2/ColorSpace/DeviceGray"),
                ],
            ),
            // Colour boxes: sRGB over one component, over two; an unknown
            // enumerated space; an unknown method.
            (
                "jpx_srgb_gray",
                vec![jpx("", &colr(jpx!("gray.jp2"), 1, 16))],
            ),
            ("jpx_srgb_two", vec![jpx("", &colr(jpx!("la.jp2"), 1, 16))]),
            ("jpx_two", vec![jpx("", jpx!("la.jp2"))]),
            (
                "jpx_enum_unknown",
                vec![jpx("", &colr(jpx!("rgb.jp2"), 1, 99))],
            ),
            ("jpx_method_3", vec![jpx("", &colr(jpx!("rgb.jp2"), 3, 16))]),
            (
                "jpx_enum_cmyk_on_rgb",
                vec![jpx("", &colr(jpx!("rgb.jp2"), 1, 12))],
            ),
            // Box lengths: 0 (to the end), extended, too small.
            (
                "jpx_box_to_end",
                vec![jpx("", &box_len(jpx!("rgb.jp2"), b"jp2c", 0))],
            ),
            (
                "jpx_extended_box",
                vec![jpx("", &extended(jpx!("rgb.jp2"), b"jp2c"))],
            ),
            (
                "jpx_short_box",
                vec![jpx("", &box_len(jpx!("rgb.jp2"), b"jp2c", 4))],
            ),
            (
                "jpx_long_box",
                vec![jpx("", &box_len(jpx!("rgb.jp2"), b"jp2c", 9999))],
            ),
            (
                "jpx_header_to_end",
                vec![jpx("", &box_len(jpx!("rgb.jp2"), b"jp2h", 0))],
            ),
            (
                "jpx_short_header",
                vec![jpx("", &box_len(jpx!("rgb.jp2"), b"jp2h", 4))],
            ),
            (
                "jpx_short_colr",
                vec![jpx("", &box_len(jpx!("rgb.jp2"), b"colr", 4))],
            ),
            (
                "jpx_no_ihdr",
                vec![jpx("", &rename(jpx!("rgb.jp2"), b"ihdr", b"ihdX"))],
            ),
            (
                "jpx_no_siz",
                vec![jpx("", &[0xff, 0x4f, 0xff, 0x52, 0, 0, 0, 0])],
            ),
            ("jpx_empty_siz", vec![jpx("", &zero_siz(jpx!("rgb.j2k")))]),
        ]
    }

    /// Offset of the first box of type `kind` (its length field).
    fn box_at(data: &[u8], kind: &[u8; 4]) -> usize {
        data.windows(4).position(|w| w == kind).unwrap_or(4) - 4
    }

    /// `jp2` with its colour box's method and enumerated space replaced.
    fn colr(jp2: &[u8], method: u8, enumcs: u32) -> Vec<u8> {
        let mut out = jp2.to_vec();
        let at = box_at(jp2, b"colr") + 8;
        out[at] = method;
        out[at + 3..at + 7].copy_from_slice(&enumcs.to_be_bytes());
        out
    }

    fn box_len(data: &[u8], kind: &[u8; 4], len: u32) -> Vec<u8> {
        let mut out = data.to_vec();
        let at = box_at(data, kind);
        out[at..at + 4].copy_from_slice(&len.to_be_bytes());
        out
    }

    fn rename(data: &[u8], kind: &[u8; 4], to: &[u8; 4]) -> Vec<u8> {
        let mut out = data.to_vec();
        let at = box_at(data, kind) + 4;
        out[at..at + 4].copy_from_slice(to);
        out
    }

    /// `data` with box `kind` given an extended (64-bit) length.
    fn extended(data: &[u8], kind: &[u8; 4]) -> Vec<u8> {
        let at = box_at(data, kind);
        let len = u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
        let mut b = 1u32.to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(&u64::from(len + 8).to_be_bytes());
        [&data[..at], &b[..], &data[at + 8..]].concat()
    }

    /// A codestream whose SIZ has no components.
    fn zero_siz(j2k: &[u8]) -> Vec<u8> {
        let mut out = j2k.to_vec();
        out[40..42].copy_from_slice(&[0, 0]);
        out
    }

    /// MuPDF 1.28.6's summary of each case, from image.js.
    #[rustfmt::skip]
    const MUPDF_IMAGES: &[(&str, &str)] = &[
        ("abbreviations_first", "3x1 bpc 4 n 1 cs Gray im false interp true decode [1 0] key -"),
        ("bad_color_key", "2x2 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("bad_colorspace", "error unknown colorspace: Foo"),
        ("color_key", "2x2 bpc 8 n 3 cs RGB im false interp false decode - key [0 10 20 30 40 50]"),
        ("deep", "error image depth is too large: 17"),
        ("default_depth", "1x1 bpc 8 n 1 cs Gray im false interp false decode - key -"),
        ("huge", "error image is too large"),
        ("image_mask", "4x4 bpc 1 n 1 cs - im true interp false decode [1 0] key -"),
        ("indexed_default_decode", "1x1 bpc 4 n 1 cs Indexed im false interp false decode [0 15] key -"),
        ("jpx_as_soft_mask", "2x2 bpc 8 n 3 cs RGB im false interp false decode - key - mask (5x3 bpc 8 n 3 cs RGB im false interp false decode - key -)"),
        ("jpx_bad_colorspace", "error unknown colorspace: Foo"),
        ("jpx_box_to_end", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_cmyk", "2x2 bpc 8 n 4 cs CMYK im false interp false decode - key -"),
        ("jpx_codestream", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_codestream_cmyk_dict", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_color_key", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key [1 2 3 4 5 6]"),
        ("jpx_dict_colorspace", "error unknown colorspace: CalRGB"),
        ("jpx_empty", "error not enough data to determine image format"),
        ("jpx_empty_siz", "error Failed to read JPX header"),
        ("jpx_enum_cmyk_on_rgb", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_enum_unknown", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_extended_box", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_filter_array", "4x2 bpc 8 n 1 cs Gray im false interp false decode - key -"),
        ("jpx_gray", "4x2 bpc 8 n 1 cs Gray im false interp false decode [1 0] key -"),
        ("jpx_header_to_end", "error Failed to read JPX header"),
        ("jpx_icc", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_icc_broken", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_icc_gray", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_junk", "error Failed to read JPX header"),
        ("jpx_long_box", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_masks", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key [1 2 3 4 5 6]"),
        ("jpx_method_3", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_no_ihdr", "error Failed to read JPX header"),
        ("jpx_no_siz", "error Failed to read JPX header"),
        ("jpx_not_a_stream", "error object is not a stream"),
        ("jpx_rgb", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_rgba", "3x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_short_box", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("jpx_short_colr", "error Failed to read JPX header"),
        ("jpx_short_header", "error Failed to read JPX header"),
        ("jpx_soft_mask", "5x3 bpc 8 n 3 cs RGB im false interp false decode - key - mask (2x2 bpc 8 n 1 cs - im false interp false decode - key -)"),
        ("jpx_srgb_gray", "4x2 bpc 8 n 1 cs Gray im false interp false decode - key -"),
        ("jpx_srgb_two", "error unsupported number of components: 2"),
        ("jpx_two", "2x2 bpc 8 n 1 cs Gray im false interp false decode - key -"),
        ("lab", "1x1 bpc 8 n 3 cs Lab im false interp false decode [0 0.5 0.1098 0.8941 0.502 1] key -"),
        ("lab_default", "1x1 bpc 8 n 3 cs Lab im false interp false decode - key -"),
        ("mask_of_a_mask", "2x2 bpc 8 n 3 cs RGB im false interp false decode - key - mask (2x2 bpc 8 n 1 cs - im false interp false decode - key -)"),
        ("matte", "2x2 bpc 8 n 3 cs RGB im false interp false decode - key [127 127 255 255 0 0] mask (2x2 bpc 8 n 1 cs - im false interp false decode - key -)"),
        ("negative_depth", "error image depth is zero (or less)"),
        ("not_a_stream", "error object is not a stream"),
        ("rgb", "2x2 bpc 8 n 3 cs RGB im false interp false decode - key -"),
        ("short_decode", "1x1 bpc 8 n 3 cs RGB im false interp false decode [0 1 1 0 0 0] key -"),
        ("soft_mask", "2x2 bpc 8 n 3 cs RGB im false interp false decode - key - mask (2x2 bpc 8 n 1 cs - im false interp false decode [1 0] key -)"),
        ("stencil_mask", "2x2 bpc 8 n 3 cs RGB im false interp false decode - key - mask (2x2 bpc 1 n 1 cs - im true interp false decode - key -)"),
        ("zero_height", "error image height is zero (or less)"),
        ("zero_width", "error image width is zero (or less)"),
    ];

    #[test]
    fn images_load_as_in_mupdf() {
        let dump = std::env::var_os("NUPDF_DUMP_IMAGE_CASES");
        let mut wrong = Vec::new();
        for (name, objects) in cases() {
            let file = pdf_of(&objects);
            if let Some(dir) = &dump {
                let path = std::path::Path::new(dir).join(format!("{name}.pdf"));
                std::fs::write(path, &file).unwrap_or_else(|e| panic!("{e}"));
            }
            let doc = Document::open(file).unwrap_or_else(|e| panic!("{e:?}"));
            let got = match load(&doc, &Object::Ref(crate::ObjRef::new(2, 0))) {
                Ok(image) => summary(&image),
                Err(e) => format!("error {}", crate::diag::report::strip(&e)),
            };
            match MUPDF_IMAGES.iter().find(|c| c.0 == name) {
                Some((_, want)) if *want == got => {}
                Some((_, want)) => wrong.push(format!("{name}:\n  got   {got}\n  MuPDF {want}")),
                None => wrong.push(format!("{name}: no MuPDF result (got {got})")),
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn intents_are_mupdfs_numbers() {
        for (name, want) in [
            ("/Perceptual", Some(0)),
            ("/RelativeColorimetric", Some(1)),
            ("/Saturation", Some(2)),
            ("/AbsoluteColorimetric", Some(3)),
            ("/Other", None),
            ("1", None),
        ] {
            let file = pdf_of(&[img(&format!("/Width 1/Height 1/Intent {name}"))]);
            let doc = Document::open(file).unwrap_or_else(|e| panic!("{e:?}"));
            let image = load(&doc, &Object::Ref(crate::ObjRef::new(2, 0)));
            assert_eq!(image.map(|i| i.intent), Ok(want), "{name}");
        }
    }

    #[test]
    fn jpeg_data_is_the_stream_with_earlier_filters_undone() {
        let jpeg = b"\xff\xd8\xff\xd9";
        let hex: String = jpeg.iter().map(|b| format!("{b:02x}")).collect();
        // (dictionary, data, the JPEG data and its parameters).
        type Case<'a> = (&'a str, Vec<u8>, Option<(&'a [u8], &'a str)>);
        let cases: [Case; 6] = [
            (
                "/Filter/DCTDecode/DecodeParms<</ColorTransform 0>>",
                jpeg.to_vec(),
                Some((jpeg, "<</ColorTransform 0>>")),
            ),
            ("/F/DCT", jpeg.to_vec(), Some((jpeg, "null"))),
            (
                "/Filter[/AHx/DCTDecode]/DecodeParms[null<</ColorTransform 1>>]",
                format!("{hex}>").into_bytes(),
                Some((jpeg, "<</ColorTransform 1>>")),
            ),
            (
                "/Filter[/AHx/DCTDecode]",
                format!("{hex}>").into_bytes(),
                Some((jpeg, "null")),
            ),
            ("/Filter[/DCTDecode/AHx]", jpeg.to_vec(), None),
            ("/Filter/FlateDecode", jpeg.to_vec(), None),
        ];
        for (dict, data, want) in cases {
            let file = pdf_of(&[stream(&format!("/Width 1/Height 1{dict}"), &data)]);
            let doc = Document::open(file).unwrap_or_else(|e| panic!("{e:?}"));
            let image = load(&doc, &Object::Ref(crate::ObjRef::new(2, 0)))
                .unwrap_or_else(|e| panic!("{e}"));
            let got = jpeg_parts(&doc, &image).unwrap_or_else(|e| panic!("{e}"));
            let got = got.map(|(d, p)| {
                let p = crate::print::to_string(&p, crate::print::Options::default());
                (d, p)
            });
            let want = want.map(|(d, p)| (d.to_vec(), p.to_string()));
            assert_eq!(got, want, "{dict}");
            assert_eq!(
                jpeg_data(&doc, &image).unwrap_or_default(),
                want.map(|w| w.0),
                "{dict}"
            );
        }
        // Inline images have no stream to read.
        let doc = Document::create().unwrap_or_else(|e| panic!("{e:?}"));
        let inline = from_inline(&doc, &inline_image(&doc, "/W 1/H 1"));
        assert_eq!(jpeg_data(&doc, &inline), Ok(None));
    }

    fn inline_image<'a>(doc: &Document, dict: &str) -> InlineImage<'a> {
        let diag = doc.diagnostics();
        let text = format!("<<{dict}>>");
        let mut lx = crate::lex::Lexer::new(text.as_bytes(), 0, diag);
        let crate::object::Object::Dict(d) =
            crate::parse::parse_stm_obj(&mut lx).unwrap_or_else(|e| panic!("{e:?}"))
        else {
            panic!("not a dictionary");
        };
        InlineImage {
            dict: (*d).clone(),
            width: 2,
            height: 2,
            bpc: 8,
            image_mask: false,
            colorspace: Some(ColorSpace::Rgb),
            n: 3,
            data: b"xyz",
        }
    }

    #[test]
    fn inline_images_keep_their_dictionary_and_data() {
        let doc = Document::create().unwrap_or_else(|e| panic!("{e:?}"));
        let image = from_inline(
            &doc,
            &inline_image(
                &doc,
                "/I true/D[1 0 1 0 1 0]/Intent/Saturation/Mask[1 2 3 4 5 6]",
            ),
        );
        assert_eq!(
            summary(&image),
            "2x2 bpc 8 n 3 cs RGB im false interp true decode [1 0 1 0 1 0] key [1 2 3 4 5 6]"
        );
        assert_eq!(image.intent, Some(2));
        assert!(image.stream.is_none());
        assert_eq!(image.inline.as_ref().map(|i| &i.1[..]), Some(&b"xyz"[..]));
        // A bad color key is dropped without a second warning (the content
        // parser gave one).
        let image = from_inline(&doc, &inline_image(&doc, "/Mask[1 2.5]"));
        assert_eq!(image.colorkey, None);
        assert!(doc.diagnostics().snapshot().is_empty());
        // A soft mask dictionary shadows /Mask; anything else yields to it.
        let image = from_inline(&doc, &inline_image(&doc, "/SMask<<>>/Mask[0 0 0 0 0 0]"));
        assert_eq!(image.colorkey, None);
        let image = from_inline(
            &doc,
            &inline_image(&doc, "/SMask[1 1 1 1 1 1]/Mask[0 0 0 0 0 0]"),
        );
        assert_eq!(image.colorkey, Some(vec![0; 6]));
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
