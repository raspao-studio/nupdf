// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/image.c (fz_decomp_image_from_stream,
// fz_mask_color_key, fz_unblend_masked_tile), source/fitz/draw-unpack.c,
// source/fitz/pixmap.c (fz_convert_indexed_pixmap_to_base, fz_md5_pixmap)
// source/fitz/output-pnm.c (the PAM writer) and source/fitz/output-png.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Pixmaps: an image's samples as `fz_get_pixmap_from_image` gives them at
//! full size, 8 bits per component, in the image's colorspace (an indexed
//! image in its base), with alpha for a stencil mask or a color key.

use crate::colorspace::{ColorSpace, Family};
use crate::diag::report;
use crate::document::Document;
use crate::geometry::IRect;
use crate::image::Image;
use crate::object::Object;

/// `fz_pixmap`: rows of `w * n` bytes, `n` counting alpha.
#[derive(Debug, Clone, PartialEq)]
pub struct Pixmap {
    pub w: usize,
    pub h: usize,
    pub n: usize,
    pub alpha: bool,
    pub colorspace: Option<ColorSpace>,
    pub samples: Vec<u8>,
}

impl Pixmap {
    fn new(
        colorspace: Option<ColorSpace>,
        w: usize,
        h: usize,
        alpha: bool,
    ) -> Result<Pixmap, String> {
        let n = colorspace.as_ref().map_or(0, ColorSpace::n) + usize::from(alpha);
        let len = w
            .checked_mul(h)
            .and_then(|a| a.checked_mul(n))
            .ok_or_else(|| "limit error: image too large".to_string())?;
        Ok(Pixmap {
            w,
            h,
            n,
            alpha,
            colorspace,
            samples: vec![0; len],
        })
    }

    /// `fz_save_pixmap_as_pam`: alpha is taken out of the colors it was
    /// premultiplied into.
    pub fn pam(&self) -> Result<Vec<u8>, String> {
        let colors = self.n - usize::from(self.alpha);
        let family = |f: Family| self.colorspace.as_ref().and_then(ColorSpace::family) == Some(f);
        let tupl = match (colors, self.alpha) {
            (0, true) => "GRAYSCALE",
            (1, false) if family(Family::Gray) => "GRAYSCALE",
            (1, true) if family(Family::Gray) => "GRAYSCALE_ALPHA",
            (3, false) if family(Family::Rgb) => "RGB",
            (3, true) if family(Family::Rgb) => "RGB_ALPHA",
            (4, false) if family(Family::Cmyk) => "CMYK",
            (4, true) if family(Family::Cmyk) => "CMYK_ALPHA",
            _ => {
                return Err("argument error: pixmap must be alpha only, gray, rgb, or cmyk".into());
            }
        };
        let mut out = format!(
            "P7\nWIDTH {}\nHEIGHT {}\nDEPTH {}\nMAXVAL 255\nTUPLTYPE {tupl}\nENDHDR\n",
            self.w, self.h, self.n
        )
        .into_bytes();
        if !self.alpha {
            out.extend_from_slice(&self.samples);
            return Ok(out);
        }
        for px in self.samples.chunks(self.n.max(1)) {
            let (a, c) = (px[self.n - 1], &px[..self.n - 1]);
            for &v in c {
                let a = u32::from(a);
                out.push((u32::from(v) * 255 + (a >> 1)).checked_div(a).unwrap_or(0) as u8);
            }
            out.push(a);
        }
        Ok(out)
    }
}

impl Pixmap {
    /// `fz_save_pixmap_as_png` for a gray, RGB or alpha-only pixmap at
    /// 96 dpi, without the iCCP chunk MuPDF adds for an ICC space.
    pub fn png(&self) -> Result<Vec<u8>, String> {
        let gray_or_rgb = |c: &ColorSpace| matches!(c.family(), Some(Family::Gray | Family::Rgb));
        if self.colorspace.as_ref().is_some_and(|c| !gray_or_rgb(c)) {
            return Err("argument error: pixmap must be grayscale or rgb to write as png".into());
        }
        let n = self.n;
        // Alpha alone is written as gray.
        let alpha = self.alpha && n != 1;
        let color = match (n - usize::from(alpha), alpha) {
            (1, false) => 0u8,
            (1, true) => 4,
            (3, false) => 2,
            (3, true) => 6,
            _ => {
                return Err(
                    "argument error: pixmap must be grayscale or rgb to write as png".into(),
                );
            }
        };
        let mut raw = Vec::with_capacity((self.w * n + 1) * self.h);
        for row in self.samples.chunks(self.w * n).take(self.h) {
            raw.push(0);
            if self.alpha {
                for px in row.chunks(n) {
                    let a = i32::from(px[n - 1]);
                    let inva = if a != 0 { 256 * 255 / a } else { 0 };
                    for &v in &px[..n - 1] {
                        raw.push(((i32::from(v) * inva + 128) >> 8) as u8);
                    }
                    raw.push(a as u8);
                }
            } else {
                raw.extend_from_slice(row);
            }
        }
        let idat = crate::deflate::compress(&raw, -1).ok_or("library error: compression error")?;
        let mut out = vec![137, 80, 78, 71, 13, 10, 26, 10];
        let mut head = Vec::with_capacity(13);
        head.extend_from_slice(&(self.w as u32).to_be_bytes());
        head.extend_from_slice(&(self.h as u32).to_be_bytes());
        head.extend_from_slice(&[8, color, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &head);
        // 96 dpi in dots per metre: 96 * 100 / 2.54 + 0.5.
        let ppm = (96.0f32 * 100.0 / 2.54 + 0.5) as u32;
        let mut phys = Vec::with_capacity(9);
        phys.extend_from_slice(&ppm.to_be_bytes());
        phys.extend_from_slice(&ppm.to_be_bytes());
        phys.push(1);
        chunk(&mut out, b"pHYs", &phys);
        if u32::try_from(idat.len()).is_err() {
            return Err("limit error: PNG chunk too large".into());
        }
        chunk(&mut out, b"IDAT", &idat);
        chunk(&mut out, b"IEND", &[]);
        Ok(out)
    }
}

/// `putchunk`.
fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let mut crc = flate2::Crc::new();
    crc.update(tag);
    crc.update(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

/// An image's pixmap, as `fz_get_pixmap_from_image` decodes it with no
/// scaling (`Image.toPixmap()` in MuPDF's scripts).
pub fn from_image(doc: &Document, image: &Image) -> Result<Pixmap, String> {
    from_image_subsampled(doc, image, 0).map(|(p, _)| p)
}

/// An image's pixmap decoded to be shown `2^l2` times smaller, as
/// `compressed_image_get_pixmap` does it: JPEG images (not /Matte ones)
/// are decoded up to 8 times smaller by libjpeg's scaled IDCTs, then the
/// samples of images that stream (not JPX, indexed or /Matte ones) are
/// averaged in blocks as they are read, before decode arrays and color
/// keys apply. The rest of the factor, for the caller's
/// `fz_subsample_pixmap`, comes back.
pub fn from_image_subsampled(
    doc: &Document,
    image: &Image,
    l2: u32,
) -> Result<(Pixmap, u32), String> {
    from_image_area(doc, image, l2, None).map(|(p, l2, _)| (p, l2))
}

/// [`from_image_subsampled`] for the part `area` of the image only (made
/// with `draw::image::decode_area`), as `fz_decomp_image_from_stream` cuts
/// a subarea out of the samples as they stream in. Images that do not
/// stream (JPX) and /Matte ones are decoded whole. Returns the part
/// decoded, `None` for the whole image.
pub fn from_image_area(
    doc: &Document,
    image: &Image,
    l2: u32,
    area: Option<IRect>,
) -> Result<(Pixmap, u32, Option<IRect>), String> {
    let matte = image.colorkey.is_some() && image.mask.is_some();
    let area = area
        .filter(|a| !matte && !image.is_jpx && *a != IRect::new(0, 0, image.width, image.height));
    // `subarea_stream` around the samples, then the rest of the decoding.
    let finish = |data: &[u8], wh: (usize, usize), native: u32| {
        let (p, rest) = match area {
            Some(a) => {
                let (cut, w, h) = crop(data, image, a, native);
                decomp(doc, image, &cut, (w, h), l2 - native)?
            }
            None => decomp(doc, image, data, wh, l2 - native)?,
        };
        Ok((p, rest, area))
    };
    if image.stream.is_none()
        && let Some((dict, data)) = &image.inline
    {
        let (w, h) = (image.width.max(0) as u64, image.height.max(0) as u64);
        let row = (image.bpc.max(0) as u64 * image.n as u64 * w).div_ceil(8);
        let len = row.saturating_mul(h);
        if len > doc.limits().max_decoded_bytes as u64 {
            return Err("limit error: image too large".into());
        }
        let resolve = |o: &Object| doc.resolve(o);
        let raw = crate::stream::Stream::memory(data, doc.diagnostics());
        let mut st = crate::filter::open_filters(raw, dict, &resolve, None, None, true)
            .map_err(|e| e.report())?;
        let mut out = vec![0; len as usize];
        let n = st.read(&mut out);
        out.truncate(n);
        return finish(&out, (w as usize, h as usize), 0);
    }
    let Some(stream) = &image.stream else {
        return Err(report::format("image has no data"));
    };
    let Object::Stream(s) = doc.resolve(stream) else {
        return Err(report::format("object is not a stream"));
    };
    if image.is_jpx {
        let data = doc.stream_data(&s).map_err(|e| e.report())?;
        return jpx_pixmap(doc, image, &data).map(|p| (p, l2, None));
    }
    let (w, h) = (image.width.max(0) as u64, image.height.max(0) as u64);
    let data = match crate::image::jpeg_parts(doc, image)? {
        // JPEG data is patched before it is decoded, and decoded as small
        // as it can be, up to an eighth: `l2factor` goes to the decoder, and
        // the image is that many times smaller (`fz_decomp_image_from_stream`).
        Some((mut jpeg, params)) => {
            crate::image::patch_jpeg_height(&mut jpeg, image.height);
            let native = if matte { 0 } else { l2.min(3) };
            let f = 1u64 << native;
            let (w, h) = ((w + f - 1) >> native, (h + f - 1) >> native);
            let row = (image.bpc.max(0) as u64 * image.n as u64 * w).div_ceil(8);
            let len = row.saturating_mul(h);
            if len > doc.limits().max_decoded_bytes as u64 {
                return Err("limit error: image too large".into());
            }
            let color_transform = params
                .as_dict()
                .and_then(|d| d.get(b"ColorTransform"))
                .map_or(-1, |v| doc.resolve(v).to_int_or(-1));
            let raw = crate::stream::Stream::memory(&jpeg, doc.diagnostics());
            let mut st = crate::dct::open_scaled(raw, color_transform, native);
            let mut out = vec![0; len as usize];
            let n = st.read(&mut out);
            out.truncate(n);
            return finish(&out, (w as usize, h as usize), native);
        }
        None => {
            // What the read of each row needs, whole rows of packed samples.
            let row = (image.bpc.max(0) as u64 * image.n as u64 * w).div_ceil(8);
            let len = row.saturating_mul(h);
            if len > doc.limits().max_decoded_bytes as u64 {
                return Err("limit error: image too large".into());
            }
            doc.image_samples(&s, len as usize)
                .map_err(|e| e.report())?
        }
    };
    finish(&data, (w as usize, h as usize), 0)
}

/// `subarea_stream`: the rows of `area` cut out of the packed samples of
/// the image decoded `2^l2` times smaller, skipping bytes as MuPDF does
/// (whole bytes, the right margin rounded up). Returns the samples and
/// the part's size.
fn crop(data: &[u8], image: &Image, area: IRect, l2: u32) -> (Vec<u8>, usize, usize) {
    let f = 1i64 << l2;
    let (iw, ih) = (
        i64::from(image.width.max(0)),
        i64::from(image.height.max(0)),
    );
    let bits = image.n as i64 * i64::from(image.bpc.max(0));
    let stream_stride = (((iw + f - 1) >> l2) * bits + 7) / 8;
    let at = |v: i32| i64::from(v).clamp(0, iw.max(ih));
    let l_skip = (at(area.x0) >> l2) * bits / 8;
    let r_skip = ((((iw + f - 1 - at(area.x1)) >> l2).max(0)) * bits + 7) / 8;
    let t_skip = (at(area.y0) >> l2) * stream_stride;
    let w = ((at(area.x1) - at(area.x0)).max(0) + f - 1) >> l2;
    let h = ((at(area.y1) - at(area.y0)).max(0) + f - 1) >> l2;
    let stride = ((w * bits + 7) / 8) as usize;
    let mut out = Vec::with_capacity(stride.saturating_mul(h as usize).min(data.len()));
    let mut pos = (l_skip + t_skip) as usize;
    for _ in 0..h {
        let Some(row) = data.get(pos..) else {
            break;
        };
        let row = &row[..stride.min(row.len())];
        out.extend_from_slice(row);
        if row.len() < stride {
            break;
        }
        pos += stride + (l_skip + r_skip) as usize;
    }
    (out, w as usize, h as usize)
}

/// `fz_decomp_image_from_stream` from the decoded samples, `w` by `h` of
/// them (the image's size, or less as JPEG decodes it), subsampled by
/// `l2` as they stream in where that can be done.
fn decomp(
    doc: &Document,
    image: &Image,
    data: &[u8],
    (w, h): (usize, usize),
    l2: u32,
) -> Result<(Pixmap, u32), String> {
    let diag = doc.diagnostics();
    let n = image.n;
    let bpc = image.bpc;
    let use_colorkey = image.colorkey.is_some();
    let mut matte = use_colorkey && image.mask.is_some();
    if matte
        && let Some(mask) = &image.mask
        && (mask.width != image.width || mask.height != image.height)
    {
        diag.warn(
            "image.matte_size",
            "mask must be of same size as image for /Matte",
        );
        matte = false;
    }
    let indexed = image
        .colorspace
        .as_ref()
        .is_some_and(ColorSpace::is_indexed);
    let alpha = image.colorspace.is_none() || use_colorkey;
    let mut tile = Pixmap::new(image.colorspace.clone(), w, h, alpha)?;
    let limit = doc.limits().max_decoded_bytes;
    if tile.samples.len() > limit {
        return Err("limit error: image too large".into());
    }
    let stride = w * tile.n;
    let read = if bpc != 8 || use_colorkey {
        unpack(data, bpc, w, h, n, indexed, use_colorkey, &mut tile.samples)?
    } else {
        let len = data.len().min(tile.samples.len());
        tile.samples[..len].copy_from_slice(&data[..len]);
        len
    };
    if read < stride * h {
        diag.warn("image.truncated", "padding truncated image");
    }
    // `subsample_stream`: blocks averaged as the samples stream in. A
    // /Matte image keeps its size, and indexed ones are subsampled after
    // they turn into their base colors, by the caller.
    let mut rest = l2;
    if l2 > 0 && !indexed && !(use_colorkey && image.mask.is_some()) {
        let f = 1usize << l2.min(6);
        crate::draw::image::pixblock(&mut tile.samples, w as i32, h as i32, tile.n as i32, l2);
        tile.w = w.div_ceil(f);
        tile.h = h.div_ceil(f);
        tile.samples.truncate(tile.w * tile.h * tile.n);
        rest = 0;
    }
    if image.image_mask {
        // 0 is opaque and 1 transparent.
        for v in &mut tile.samples {
            *v = !*v;
        }
    }
    if let Some(key) = &image.colorkey
        && image.mask.is_none()
    {
        mask_color_key(doc, &mut tile, n, bpc, key, indexed);
    }
    if indexed {
        let maxval = (1i32 << bpc.clamp(1, 16)) - 1;
        if let Some(decode) = &image.decode {
            decode_indexed_tile(&mut tile, decode, maxval);
        }
        tile = indexed_to_base(&tile)?;
    } else if let Some(decode) = &image.decode {
        decode_tile(&mut tile, decode);
    }
    if matte && let (Some(mask), Some(key)) = (&image.mask, &image.colorkey) {
        let mask = from_image(doc, mask)?;
        unblend_masked_tile(&mut tile, &mask, n, key);
    }
    Ok((tile, rest))
}

/// `fz_unpack_stream` read whole: samples of `depth` bits to bytes,
/// scaled to 0..255 unless indexed, with an opaque alpha after each pixel
/// for `pad`. Only whole source rows are unpacked. Returns the bytes
/// written.
#[allow(clippy::too_many_arguments)]
fn unpack(
    src: &[u8],
    depth: i32,
    w: usize,
    h: usize,
    n: usize,
    indexed: bool,
    pad: bool,
    dst: &mut [u8],
) -> Result<usize, String> {
    if !(1..=32).contains(&depth) {
        return Err("argument error: depth out of range in fz_unpack_stream".into());
    }
    let depth = depth as usize;
    let src_stride = (depth * n * w).div_ceil(8);
    let dst_stride = (n + usize::from(pad)) * w;
    let scale: u32 = match (indexed, depth) {
        (false, 1) => 255,
        (false, 2) => 85,
        (false, 4) => 17,
        _ => 1,
    };
    let mut written = 0;
    for y in 0..h {
        let Some(line) = src.get(y * src_stride..(y + 1) * src_stride) else {
            break;
        };
        let Some(out) = dst.get_mut(y * dst_stride..(y + 1) * dst_stride) else {
            break;
        };
        let mut o = 0;
        let mut b = 0usize;
        for _ in 0..w {
            for _ in 0..n {
                let v = match depth {
                    1 => u32::from((line[b >> 3] >> (7 - (b & 7))) & 1) * scale,
                    2 => u32::from((line[b >> 2] >> ((3 - (b & 3)) << 1)) & 3) * scale,
                    4 => u32::from((line[b >> 1] >> ((1 - (b & 1)) << 2)) & 15) * scale,
                    8 => u32::from(line[b]),
                    16 => u32::from(line[b << 1]),
                    24 => u32::from(line[b * 3]),
                    32 => u32::from(line[b << 2]),
                    // MuPDF writes nothing for other depths.
                    _ => 0,
                };
                out[o] = v as u8;
                o += 1;
                b += 1;
            }
            if pad {
                out[o] = 255;
                o += 1;
            }
        }
        written += dst_stride;
    }
    Ok(written)
}

/// `fz_mask_color_key`: pixels whose every color is within the key are
/// made transparent (all components, alpha included, zero).
fn mask_color_key(
    doc: &Document,
    pix: &mut Pixmap,
    n: usize,
    bpc: i32,
    key: &[i32],
    indexed: bool,
) {
    let diag = doc.diagnostics();
    if pix.w == 0 {
        return;
    }
    let (scale, shift) = if indexed {
        (1, 0)
    } else {
        match bpc {
            1 => (255, 0),
            2 => (85, 0),
            4 => (17, 0),
            16 => (1, 8),
            24 => (1, 16),
            32 => (1, 24),
            _ => (1, 0),
        }
    };
    let max: i64 = match bpc {
        1 => 1,
        2 => 3,
        4 => 15,
        16 => 0xffff,
        24 => 0xff_ffff,
        32 => 0xffff_ffff,
        _ => 0xff,
    };
    let mut ck = vec![0i64; 2 * n];
    for (k, slot) in ck.iter_mut().enumerate() {
        let mut v = i64::from(key.get(k).copied().unwrap_or(0));
        if v > max {
            if indexed && bpc == 1 {
                if k == 0 {
                    diag.warn(
                        "image.color_key",
                        "first color key masking value out of range in 1bpc indexed image, ignoring color key masking",
                    );
                    return;
                }
                diag.warn(
                    "image.color_key",
                    "later color key masking value out of range in 1bpc indexed image, assumed to be 1",
                );
                v = 1;
            } else if bpc != 1 {
                diag.warn(
                    "image.color_key",
                    "color key masking value out of range, masking to valid range",
                );
                // `colorkey[k] &= max` on an int.
                v = i64::from((v as i32) & (max as i32));
            }
        }
        if v < 0 || v > max {
            diag.warn(
                "image.color_key",
                "color key masking value out of range, clamping to valid range",
            );
            v = v.clamp(0, max);
        }
        if scale > 1 {
            v *= scale;
        } else if shift > 0 {
            v >>= shift;
        }
        *slot = v;
    }
    let pn = pix.n;
    for px in pix.samples.chunks_mut(pn) {
        let keyed = (0..n).all(|k| {
            let p = i64::from(px[k]);
            p >= ck[k * 2] && p <= ck[k * 2 + 1]
        });
        if keyed {
            px.fill(0);
        }
    }
}

/// `fz_decode_indexed_tile`: a decode array on palette indices.
fn decode_indexed_tile(pix: &mut Pixmap, decode: &[f32], maxval: i32) {
    let n = pix.n - usize::from(pix.alpha);
    let mut add = [0i32; 32];
    let mut mul = [0i32; 32];
    let mut needed = false;
    for k in 0..n.min(32) {
        let min = (decode.get(k * 2).copied().unwrap_or(0.0) * 256.0) as i32;
        let max = (decode.get(k * 2 + 1).copied().unwrap_or(0.0) * 256.0) as i32;
        add[k] = min;
        mul[k] = (max - min) / maxval.max(1);
        needed |= min != 0 || max != maxval * 256;
    }
    if !needed {
        return;
    }
    let pn = pix.n;
    for px in pix.samples.chunks_mut(pn) {
        for k in 0..n.min(32) {
            let v = (add[k] + (((i32::from(px[k]) << 8) * mul[k]) >> 8)) >> 8;
            px[k] = v.clamp(0, 255) as u8;
        }
    }
}

/// `fz_mul255`.
fn mul255(a: i32, b: i32) -> i32 {
    let x = a * b + 128;
    (x + (x >> 8)) >> 8
}

/// `fz_decode_tile`.
fn decode_tile(pix: &mut Pixmap, decode: &[f32]) {
    let n = (pix.n - usize::from(pix.alpha)).max(1);
    let mut add = [0i32; 32];
    let mut mul = [0i32; 32];
    for k in 0..n.min(32) {
        let min = (decode.get(k * 2).copied().unwrap_or(0.0) * 255.0) as i32;
        let max = (decode.get(k * 2 + 1).copied().unwrap_or(0.0) * 255.0) as i32;
        add[k] = min;
        mul[k] = max - min;
    }
    let pn = pix.n.max(1);
    for px in pix.samples.chunks_mut(pn) {
        for k in 0..n.min(32).min(px.len()) {
            let v = add[k] + mul255(i32::from(px[k]), mul[k]);
            px[k] = v.clamp(0, 255) as u8;
        }
    }
}

/// `fz_convert_indexed_pixmap_to_base`.
fn indexed_to_base(src: &Pixmap) -> Result<Pixmap, String> {
    let Some(ColorSpace::Indexed { base, high, lookup }) = &src.colorspace else {
        return Err("argument error: cannot convert non-indexed pixmap".into());
    };
    if src.n != 1 + usize::from(src.alpha) {
        return Err("argument error: cannot convert indexed pixmap mis-matching components".into());
    }
    let mut dst = Pixmap::new(Some((**base).clone()), src.w, src.h, src.alpha)?;
    let n = base.n();
    let high = usize::from(*high);
    let entry = |v: usize, k: usize| lookup.get(v * n + k).copied().unwrap_or(0);
    let mut d = 0;
    for px in src.samples.chunks(src.n) {
        let v = usize::from(px[0]).min(high);
        if src.alpha {
            let a = i32::from(px[1]);
            let aa = a + (a >> 7);
            for k in 0..n {
                dst.samples[d] = ((aa * i32::from(entry(v, k)) + 128) >> 8) as u8;
                d += 1;
            }
            dst.samples[d] = a as u8;
            d += 1;
        } else {
            for k in 0..n {
                dst.samples[d] = entry(v, k);
                d += 1;
            }
        }
    }
    Ok(dst)
}

/// `fz_unblend_masked_tile`: colors premultiplied against a /Matte color
/// taken back out with the soft mask.
fn unblend_masked_tile(tile: &mut Pixmap, mask: &Pixmap, n: usize, key: &[i32]) {
    let tn = tile.n;
    let mn = mask.n.max(1);
    for (px, m) in tile.samples.chunks_mut(tn).zip(mask.samples.chunks(mn)) {
        let s = i32::from(m[0]);
        for k in 0..n.min(px.len()) {
            let c = key.get(k).copied().unwrap_or(0);
            px[k] = if s == 0 {
                c as u8
            } else {
                (c + (i32::from(px[k]) - c) * 255 / s).clamp(0, 255) as u8
            };
        }
    }
}

/// `fz_load_jpx`: a JPEG 2000 image decoded (by hayro-jpeg2000 where MuPDF
/// uses OpenJPEG) into the image's colorspace, alpha premultiplied.
fn jpx_pixmap(doc: &Document, image: &Image, data: &[u8]) -> Result<Pixmap, String> {
    let settings = hayro_jpeg2000::DecodeSettings::default();
    let jpx = hayro_jpeg2000::Image::new(data, &settings)
        .map_err(|e| format!("library error: cannot decode JPX image: {e:?}"))?;
    let (w, h) = (jpx.width() as usize, jpx.height() as usize);
    let limit = doc.limits().max_decoded_bytes;
    if w.saturating_mul(h).saturating_mul(5) > limit {
        return Err("limit error: image too large".into());
    }
    let mut ctx = hayro_jpeg2000::DecoderContext::default();
    let decoded = jpx
        .decode(&mut ctx)
        .map_err(|e| format!("library error: cannot decode JPX image: {e:?}"))?;
    let comps = decoded.components().len();
    if comps == 0 {
        return Err("library error: JPX image has no components".into());
    }
    let samples = decoded.data_u8();
    let cs_n = image.colorspace.as_ref().map_or(0, ColorSpace::n);
    let alpha = image.colorspace.is_none() || comps > cs_n;
    let colors = if image.colorspace.is_none() { 0 } else { cs_n };
    let mut out = Pixmap::new(image.colorspace.clone(), w, h, alpha)?;
    let n = out.n;
    for (i, px) in out.samples.chunks_mut(n).enumerate() {
        let src = &samples[(i * comps).min(samples.len())..];
        let get = |k: usize| src.get(k.min(comps - 1)).copied().unwrap_or(0);
        if colors == 0 {
            px[0] = get(0);
            continue;
        }
        for (k, v) in px.iter_mut().enumerate().take(colors) {
            *v = get(k);
        }
        if alpha {
            let a = get(colors);
            for v in px.iter_mut().take(colors) {
                *v = ((u32::from(*v) * u32::from(a)
                    + 128
                    + ((u32::from(*v) * u32::from(a) + 128) >> 8))
                    >> 8) as u8;
            }
            px[colors] = a;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_unpack_to_bytes_with_padding() {
        let mut out = vec![0; 8];
        // Two rows of four 1-bit gray pixels; the last row is short.
        let n = unpack(&[0b1010_0000], 1, 4, 2, 1, false, false, &mut out);
        assert_eq!(n, Ok(4));
        assert_eq!(&out[..4], &[255, 0, 255, 0]);
        let mut out = vec![0; 4];
        assert_eq!(
            unpack(
                &[0x12, 0x34, 0x56, 0x78],
                16,
                2,
                1,
                1,
                false,
                true,
                &mut out
            ),
            Ok(4)
        );
        assert_eq!(out, vec![0x12, 255, 0x56, 255]);
        let mut out = vec![0; 2];
        assert_eq!(
            unpack(&[0b0111_0000], 4, 2, 1, 1, true, false, &mut out),
            Ok(2)
        );
        assert_eq!(out, vec![7, 0]);
    }

    #[test]
    fn indexed_pixels_take_their_palette_entry() {
        let src = Pixmap {
            w: 2,
            h: 1,
            n: 1,
            alpha: false,
            colorspace: Some(ColorSpace::Indexed {
                base: Box::new(ColorSpace::Rgb),
                high: 1,
                lookup: vec![1, 2, 3, 4, 5, 6],
            }),
            samples: vec![1, 9],
        };
        let dst = indexed_to_base(&src).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(dst.samples, vec![4, 5, 6, 4, 5, 6]);
        assert_eq!(dst.n, 3);
    }

    #[test]
    fn decode_arrays_invert_and_pam_unpremultiplies() {
        let mut pix = Pixmap {
            w: 2,
            h: 1,
            n: 1,
            alpha: false,
            colorspace: Some(ColorSpace::Gray),
            samples: vec![0, 255],
        };
        decode_tile(&mut pix, &[1.0, 0.0]);
        assert_eq!(pix.samples, vec![255, 0]);
        let rgba = Pixmap {
            w: 1,
            h: 1,
            n: 4,
            alpha: true,
            colorspace: Some(ColorSpace::Rgb),
            samples: vec![64, 0, 128, 128],
        };
        let pam = rgba.pam().unwrap_or_else(|e| panic!("{e}"));
        assert!(pam.starts_with(
            b"P7\nWIDTH 1\nHEIGHT 1\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
        ));
        assert_eq!(&pam[pam.len() - 4..], &[128, 0, 255, 128]);
    }

    fn pix(colorspace: Option<ColorSpace>, alpha: bool, samples: &[u8]) -> Pixmap {
        let n = colorspace.as_ref().map_or(0, ColorSpace::n) + usize::from(alpha);
        Pixmap {
            w: samples.len() / n,
            h: 1,
            n,
            alpha,
            colorspace,
            samples: samples.to_vec(),
        }
    }

    #[test]
    fn pam_names_each_kind_of_pixmap() {
        let lab = ColorSpace::Icc(Family::Lab, Default::default());
        for (p, tupl) in [
            (pix(None, true, &[7]), "GRAYSCALE"),
            (pix(Some(ColorSpace::Gray), false, &[7]), "GRAYSCALE"),
            (
                pix(Some(ColorSpace::Gray), true, &[7, 9]),
                "GRAYSCALE_ALPHA",
            ),
            (pix(Some(ColorSpace::Rgb), false, &[1, 2, 3]), "RGB"),
            (pix(Some(ColorSpace::Cmyk), false, &[1, 2, 3, 4]), "CMYK"),
            (
                pix(Some(ColorSpace::Cmyk), true, &[0, 0, 0, 0, 0]),
                "CMYK_ALPHA",
            ),
        ] {
            let pam = p.pam().unwrap_or_else(|e| panic!("{e}"));
            let head = format!("TUPLTYPE {tupl}\nENDHDR\n");
            assert!(
                pam.windows(head.len()).any(|w| w == head.as_bytes()),
                "{tupl}"
            );
        }
        // Clear pixels stay clear rather than dividing by zero.
        let clear = pix(Some(ColorSpace::Cmyk), true, &[0, 0, 0, 0, 0]);
        let pam = clear.pam().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(&pam[pam.len() - 5..], &[0; 5]);
        assert!(pix(Some(lab), false, &[1, 2, 3]).pam().is_err());
    }

    /// The PNG's IHDR color type and its rows inflated.
    fn png_rows(p: &Pixmap) -> (u8, Vec<u8>) {
        use std::io::Read;
        let png = p.png().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        let color = png[8 + 8 + 9];
        let at = png
            .windows(4)
            .position(|w| w == b"IDAT")
            .unwrap_or_else(|| panic!("no IDAT"));
        let len = u32::from_be_bytes([png[at - 4], png[at - 3], png[at - 2], png[at - 1]]) as usize;
        let mut rows = Vec::new();
        flate2::read::ZlibDecoder::new(&png[at + 4..at + 4 + len])
            .read_to_end(&mut rows)
            .unwrap_or_else(|e| panic!("{e}"));
        (color, rows)
    }

    #[test]
    fn png_writes_gray_rgb_and_alpha_unpremultiplied() {
        assert_eq!(
            png_rows(&pix(Some(ColorSpace::Gray), false, &[7, 8])),
            (0, vec![0, 7, 8])
        );
        // Alpha alone is written as gray.
        assert_eq!(png_rows(&pix(None, true, &[200])), (0, vec![0, 200]));
        assert_eq!(
            png_rows(&pix(Some(ColorSpace::Gray), true, &[64, 128])),
            (4, vec![0, 128, 128])
        );
        assert_eq!(
            png_rows(&pix(Some(ColorSpace::Rgb), false, &[1, 2, 3])),
            (2, vec![0, 1, 2, 3])
        );
        assert_eq!(
            png_rows(&pix(
                Some(ColorSpace::Rgb),
                true,
                &[64, 0, 128, 128, 0, 0, 0, 0]
            )),
            (6, vec![0, 128, 0, 255, 128, 0, 0, 0, 0])
        );
        let cmyk = pix(Some(ColorSpace::Cmyk), false, &[1, 2, 3, 4]);
        assert!(cmyk.png().is_err());
    }
}
