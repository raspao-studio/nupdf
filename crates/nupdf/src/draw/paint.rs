// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/draw-paint.c, source/fitz/paint-glyph.h,
// source/fitz/draw-blend.c and source/fitz/pixmap.c,
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! Pixmaps with an origin, and painting into them: coverage spans in a
//! color, and pixmaps through masks, with MuPDF's 8-bit arithmetic.

use crate::geometry::IRect;

/// `fz_pixmap`: `n` components a pixel (alpha last when `alpha`), rows of
/// `w * n` bytes, placed at `x`, `y`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pix {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub n: usize,
    pub alpha: bool,
    pub samples: Vec<u8>,
}

/// `FZ_EXPAND`: 0..255 to 0..256.
#[inline]
pub fn expand(a: i32) -> i32 {
    a + (a >> 7)
}

/// `FZ_BLEND`: `src` over `dst` by `amount` (0..256).
#[inline]
pub fn blend(src: i32, dst: i32, amount: i32) -> i32 {
    (((src - dst) * amount) + (dst << 8)) >> 8
}

/// `FZ_COMBINE`.
#[inline]
pub fn combine(a: i32, b: i32) -> i32 {
    (a * b) >> 8
}

/// `fz_mul255`.
#[inline]
pub fn mul255(a: i32, b: i32) -> i32 {
    let x = a * b + 128;
    (x + (x >> 8)) >> 8
}

impl Pix {
    /// `fz_new_pixmap_with_bbox`, cleared to 0.
    pub fn new(bbox: IRect, n: usize, alpha: bool) -> Result<Pix, String> {
        let w = bbox.width();
        let h = bbox.height();
        let len = (w as usize)
            .checked_mul(h as usize)
            .and_then(|v| v.checked_mul(n))
            .ok_or_else(|| "limit error: pixmap too large".to_string())?;
        if len > 1 << 31 {
            return Err("limit error: pixmap too large".into());
        }
        Ok(Pix {
            x: bbox.x0,
            y: bbox.y0,
            w,
            h,
            n,
            alpha,
            samples: vec![0; len],
        })
    }

    pub fn bbox(&self) -> IRect {
        IRect::new(self.x, self.y, self.x + self.w, self.y + self.h)
    }

    pub fn stride(&self) -> usize {
        self.w as usize * self.n
    }

    /// The byte offset of pixel (`x`, `y`), in pixmap coordinates.
    fn at(&self, x: i32, y: i32) -> usize {
        (y - self.y) as usize * self.stride() + (x - self.x) as usize * self.n
    }

    /// `fz_clear_pixmap_with_value`: colors to `v`, alpha to 255.
    pub fn clear_with(&mut self, v: u8) {
        if !self.alpha {
            self.samples.fill(v);
            return;
        }
        let n = self.n;
        for px in self.samples.chunks_mut(n) {
            px[..n - 1].fill(v);
            px[n - 1] = 255;
        }
    }

    /// `fz_copy_pixmap_rect`: `src`'s pixels in `r` (same layout).
    pub fn copy_rect(&mut self, src: &Pix, r: IRect) {
        let r = r.intersect(self.bbox()).intersect(src.bbox());
        if r.is_empty() || self.n != src.n {
            return;
        }
        let len = r.width() as usize * self.n;
        for y in r.y0..r.y1 {
            let d = self.at(r.x0, y);
            let s = src.at(r.x0, y);
            self.samples[d..d + len].copy_from_slice(&src.samples[s..s + len]);
        }
    }

    /// The coverage row `mask` at (`x`, `y`) painted in `color` (its
    /// components, then alpha as 0..255): `fz_paint_span_with_color`.
    pub fn span_with_color(&mut self, x: i32, y: i32, mask: &[u8], color: &[u8]) {
        let b = self.bbox();
        if y < b.y0 || y >= b.y1 {
            return;
        }
        let x0 = x.max(b.x0);
        let x1 = (x + mask.len() as i32).min(b.x1);
        if x0 >= x1 {
            return;
        }
        let n = self.n;
        let da = usize::from(self.alpha);
        let n1 = n - da;
        let sa = color.get(n1).copied().unwrap_or(255) as i32;
        let start = self.at(x0, y);
        let row = &mut self.samples[start..start + (x1 - x0) as usize * n];
        let mask = &mask[(x0 - x) as usize..(x1 - x) as usize];
        if sa == 255 {
            for (px, &m) in row.chunks_mut(n).zip(mask) {
                if m == 0 {
                    continue;
                }
                if m == 255 {
                    px[..n1].copy_from_slice(&color[..n1]);
                    if da == 1 {
                        px[n1] = 255;
                    }
                } else {
                    let ma = expand(i32::from(m));
                    for k in 0..n1 {
                        px[k] = blend(i32::from(color[k]), i32::from(px[k]), ma) as u8;
                    }
                    if da == 1 {
                        px[n1] = blend(255, i32::from(px[n1]), ma) as u8;
                    }
                }
            }
        } else {
            let sa = expand(sa);
            for (px, &m) in row.chunks_mut(n).zip(mask) {
                let ma = combine(expand(i32::from(m)), sa);
                for k in 0..n1 {
                    px[k] = blend(i32::from(color[k]), i32::from(px[k]), ma) as u8;
                }
                if da == 1 {
                    px[n1] = blend(255, i32::from(px[n1]), ma) as u8;
                }
            }
        }
    }

    /// A coverage row into a one-component mask, as the span painter with
    /// no color writes it (`fz_paint_span` of a solid mask).
    pub fn span_mask(&mut self, x: i32, y: i32, mask: &[u8]) {
        let b = self.bbox();
        if y < b.y0 || y >= b.y1 || self.n != 1 {
            return;
        }
        let x0 = x.max(b.x0);
        let x1 = (x + mask.len() as i32).min(b.x1);
        if x0 >= x1 {
            return;
        }
        let start = self.at(x0, y);
        let row = &mut self.samples[start..start + (x1 - x0) as usize];
        let mask = &mask[(x0 - x) as usize..(x1 - x) as usize];
        for (d, &m) in row.iter_mut().zip(mask) {
            // paint_span_0_da_sa: the coverage over what is there.
            let sv = i32::from(m);
            let t = expand(255 - sv);
            *d = (sv + combine(i32::from(*d), t)) as u8;
        }
    }

    /// A glyph's coverage row into a one-component mask, as
    /// `fz_paint_glyph_mask` paints an RLE glyph.
    pub fn glyph_mask_row(&mut self, x: i32, y: i32, cov: &[u8]) {
        let b = self.bbox();
        if y < b.y0 || y >= b.y1 || self.n != 1 {
            return;
        }
        let x0 = x.max(b.x0);
        let x1 = (x + cov.len() as i32).min(b.x1);
        if x0 >= x1 {
            return;
        }
        let start = self.at(x0, y);
        let row = &mut self.samples[start..start + (x1 - x0) as usize];
        for (d, &a) in row.iter_mut().zip(&cov[(x0 - x) as usize..]) {
            match a {
                0 => {}
                255 => *d = 255,
                _ if *d == 0 => *d = a,
                _ => *d = blend(255, i32::from(*d), expand(i32::from(a))) as u8,
            }
        }
    }

    /// A glyph's coverage row at `alpha` into a one-component plane (a
    /// group alpha or shape), as `draw_glyph` paints one with no
    /// colorspace: an RLE glyph by `fz_paint_glyph_mask(_alpha)`, any
    /// other by `paint_span_0_da_sa(_alpha)`.
    pub fn glyph_alpha_row(&mut self, x: i32, y: i32, cov: &[u8], alpha: u8, rle: bool) {
        if alpha == 255 {
            if rle {
                self.glyph_mask_row(x, y, cov);
            } else {
                self.span_mask(x, y, cov);
            }
            return;
        }
        let b = self.bbox();
        if y < b.y0 || y >= b.y1 || self.n != 1 {
            return;
        }
        let x0 = x.max(b.x0);
        let x1 = (x + cov.len() as i32).min(b.x1);
        if x0 >= x1 {
            return;
        }
        let start = self.at(x0, y);
        let row = &mut self.samples[start..start + (x1 - x0) as usize];
        let al = i32::from(alpha);
        for (d, &a) in row.iter_mut().zip(&cov[(x0 - x) as usize..]) {
            let (a, v) = (i32::from(a), i32::from(*d));
            *d = if rle {
                match a {
                    0 => v,
                    255 => al,
                    _ if v == 0 => mul255(a, al),
                    _ => blend(al, v, expand(a)),
                }
            } else {
                let masa = combine(a, expand(al));
                masa + combine(v, expand(255 - masa))
            } as u8;
        }
    }

    /// `fz_paint_pixmap`: `src` (premultiplied) over `self` at `alpha`;
    /// both have the same colors.
    pub fn paint_pixmap(&mut self, src: &Pix, alpha: i32) {
        self.paint_pixmap_in(src, alpha, IRect::INFINITE);
    }

    /// `fz_paint_pixmap_with_bbox`: [`Pix::paint_pixmap`] within `bbox`.
    pub fn paint_pixmap_in(&mut self, src: &Pix, alpha: i32, bbox: IRect) {
        if alpha == 0 {
            return;
        }
        let sa = usize::from(src.alpha);
        let da = usize::from(self.alpha);
        let n1 = src.n - sa;
        if self.n - da != n1 {
            return;
        }
        let r = self.bbox().intersect(src.bbox()).intersect(bbox);
        if r.is_empty() {
            return;
        }
        let w = r.width() as usize;
        for y in r.y0..r.y1 {
            let mut d = self.at(r.x0, y);
            let mut s = src.at(r.x0, y);
            for _ in 0..w {
                let sp = &src.samples[s..s + src.n];
                let dp = &mut self.samples[d..d + self.n];
                if alpha == 255 {
                    // template_span_N_general.
                    let t = if sa == 1 { i32::from(sp[n1]) } else { 255 };
                    if t == 255 {
                        dp[..n1].copy_from_slice(&sp[..n1]);
                        if da == 1 {
                            dp[n1] = 255;
                        }
                    } else if t != 0 {
                        let t = 256 - expand(t);
                        for k in 0..n1 + da {
                            dp[k] = (i32::from(sp[k]) + combine(i32::from(dp[k]), t)) as u8;
                        }
                    }
                } else {
                    // template_span_N_with_alpha_general.
                    let a = if sa == 1 { expand(alpha) } else { alpha };
                    let masa = if sa == 1 {
                        combine(i32::from(sp[n1]), a)
                    } else {
                        a
                    };
                    let t = expand(255 - masa);
                    for k in 0..n1 {
                        dp[k] = (combine(i32::from(sp[k]), a) + combine(i32::from(dp[k]), t)) as u8;
                    }
                    if da == 1 {
                        dp[n1] = (masa + combine(i32::from(dp[n1]), t)) as u8;
                    }
                }
                d += self.n;
                s += src.n;
            }
        }
    }

    /// `fz_blend_pixmap` for an isolated group: `src` composited onto
    /// `self` with blend mode `mode` (MuPDF's numbering) at `alpha`.
    pub fn blend_pixmap(&mut self, src: &Pix, alpha: i32, mode: u8) {
        let mut src = src.clone();
        if alpha < 255 {
            for v in &mut src.samples {
                *v = mul255(i32::from(*v), alpha) as u8;
            }
        }
        let r = self.bbox().intersect(src.bbox());
        if r.is_empty() {
            return;
        }
        let (mut sal, mut bal) = (usize::from(src.alpha), usize::from(self.alpha));
        if src.n == 1 {
            sal = 0;
            bal = 0;
        }
        let n = src.n - usize::from(src.alpha);
        if n != self.n - usize::from(self.alpha) {
            return;
        }
        let w = r.width() as usize;
        for y in r.y0..r.y1 {
            let mut d = self.at(r.x0, y);
            let mut s = src.at(r.x0, y);
            for _ in 0..w {
                let sp = &src.samples[s..s + src.n];
                let bp = &mut self.samples[d..d + self.n];
                blend_px(bp, bal, sp, sal, n, mode);
                d += self.n;
                s += src.n;
            }
        }
    }

    /// `fz_paint_pixmap_alpha`: `src`'s alpha into this alpha-only pixmap,
    /// at `alpha`. MuPDF's partial-alpha span reads each pixel's blend
    /// value from the next pixel, which is kept.
    pub fn paint_pixmap_alpha(&mut self, src: &Pix, alpha: i32) {
        if alpha == 0 || self.n != 1 || !src.alpha {
            return;
        }
        let r = self.bbox().intersect(src.bbox());
        if r.is_empty() {
            return;
        }
        let n = src.n;
        let w = r.width() as usize;
        for y in r.y0..r.y1 {
            let d0 = self.at(r.x0, y);
            let s0 = src.at(r.x0, y) + n - 1;
            for i in 0..w {
                let s = i32::from(src.samples.get(s0 + i * n).copied().unwrap_or(0));
                let d = &mut self.samples[d0 + i];
                if alpha == 255 {
                    *d = (s + combine(i32::from(*d), expand(255 - s))) as u8;
                } else {
                    let masa = combine(s, expand(alpha));
                    let next = i32::from(src.samples.get(s0 + (i + 1) * n).copied().unwrap_or(0));
                    *d = blend(next, i32::from(*d), masa) as u8;
                }
            }
        }
    }

    /// `fz_paint_over_pixmap_with_mask` for alpha-only pixmaps: `src` over
    /// `self` through `msk`.
    pub fn paint_over_with_mask(&mut self, src: &Pix, msk: &Pix) {
        let r = self.bbox().intersect(src.bbox()).intersect(msk.bbox());
        if r.is_empty() || self.n != 1 || src.n != 1 || msk.n != 1 {
            return;
        }
        let w = r.width() as usize;
        for y in r.y0..r.y1 {
            let (d0, s0, m0) = (self.at(r.x0, y), src.at(r.x0, y), msk.at(r.x0, y));
            for i in 0..w {
                let ma = expand(i32::from(msk.samples[m0 + i]));
                let sv = i32::from(src.samples[s0 + i]);
                if ma == 0 || sv == 0 {
                    continue;
                }
                let a = if ma != 256 { mul255(ma, sv) } else { sv };
                let d = &mut self.samples[d0 + i];
                *d = (255 - mul255(255 - a, 255 - i32::from(*d))) as u8;
            }
        }
    }

    /// `fz_blend_pixmap` for a non-isolated group: `src`, drawn over a
    /// copy of the backdrop, put back onto `self` through the group's
    /// alpha `ga` (`fz_blend_separable_nonisolated`).
    pub fn blend_nonisolated(&mut self, src: &Pix, alpha: i32, mode: u8, ga: &Pix) {
        let r = self.bbox().intersect(src.bbox()).intersect(ga.bbox());
        if r.is_empty() {
            return;
        }
        let (mut sal, mut bal) = (usize::from(src.alpha), usize::from(self.alpha));
        if src.n == 1 {
            sal = 0;
            bal = 0;
        }
        let n1 = src.n - usize::from(src.alpha);
        if n1 != self.n - usize::from(self.alpha) {
            return;
        }
        let w = r.width() as usize;
        for y in r.y0..r.y1 {
            let (d0, s0, h0) = (self.at(r.x0, y), src.at(r.x0, y), ga.at(r.x0, y));
            for i in 0..w {
                let sp = &src.samples[s0 + i * src.n..s0 + (i + 1) * src.n];
                let bp = &mut self.samples[d0 + i * self.n..d0 + (i + 1) * self.n];
                let ha = i32::from(ga.samples[h0 + i]);
                if sal == 0 && alpha == 255 && mode == 0 {
                    if mul255(ha, alpha) != 0 {
                        bp[..n1].copy_from_slice(&sp[..n1]);
                        if bal == 1 {
                            bp[n1] = 255;
                        }
                    }
                    continue;
                }
                let haa = mul255(ha, alpha);
                if haa == 0 {
                    continue;
                }
                if mode >= 12 && n1 == 3 {
                    nonseparable_nonisolated(bp, bal, sp, sal, ha, haa, alpha, mode);
                    continue;
                }
                let sa = if sal == 1 { i32::from(sp[n1]) } else { 255 };
                if sa == 0 {
                    continue;
                }
                let invsa = 255 * 256 / sa;
                let ba = if bal == 1 { i32::from(bp[n1]) } else { 255 };
                if ba == 0 {
                    for k in 0..n1 {
                        bp[k] = mul255((i32::from(sp[k]) * invsa) >> 8, haa) as u8;
                    }
                    if bal == 1 {
                        bp[n1] = haa as u8;
                    }
                    continue;
                }
                let invba = 255 * 256 / ba;
                let scale = (512 * ba + ha) / (ha * 2) - expand(ba);
                let bahaa = mul255(ba, haa);
                let ra0 = ba - bahaa;
                let ra = ra0 + haa;
                if bal == 1 {
                    bp[n1] = ra as u8;
                }
                if ra == 0 {
                    continue;
                }
                for k in 0..n1 {
                    let mut sc = (i32::from(sp[k]) * invsa) >> 8;
                    let bc = (i32::from(bp[k]) * invba) >> 8;
                    sc = (sc + (((sc - bc) * scale) >> 8)).clamp(0, 255);
                    let mut rc = if mode >= 12 {
                        if mode == 15 { sc } else { bc }
                    } else {
                        separable(mode, bc, sc)
                    };
                    if bahaa != 255 {
                        rc = mul255(bahaa, rc);
                    }
                    if ba != 255 {
                        let t = mul255(255 - ba, haa);
                        rc += mul255(t, sc);
                    }
                    if ra0 != 0 {
                        rc += mul255(ra0, bc);
                    }
                    bp[k] = rc.clamp(0, ra) as u8;
                }
            }
        }
    }

    /// `fz_blend_pixmap_knockout`: where `shape` covers, `src` replaces
    /// `self` (`fz_blend_knockout`).
    pub fn blend_knockout(&mut self, src: &Pix, shape: &Pix) {
        let r = self.bbox().intersect(src.bbox()).intersect(shape.bbox());
        if r.is_empty() {
            return;
        }
        let (sal, bal) = (usize::from(src.alpha), usize::from(self.alpha));
        let n1 = src.n - sal;
        if n1 != self.n - bal {
            return;
        }
        let w = r.width() as usize;
        for y in r.y0..r.y1 {
            let (d0, s0, h0) = (self.at(r.x0, y), src.at(r.x0, y), shape.at(r.x0, y));
            for i in 0..w {
                let ha = i32::from(shape.samples[h0 + i]);
                if ha == 0 {
                    continue;
                }
                let sp = &src.samples[s0 + i * src.n..s0 + (i + 1) * src.n];
                let bp = &mut self.samples[d0 + i * self.n..d0 + (i + 1) * self.n];
                let sa = if sal == 1 { i32::from(sp[n1]) } else { 255 };
                let ba = if bal == 1 { i32::from(bp[n1]) } else { 255 };
                if ba == 0 && ha == 0xFF {
                    bp[..n1].copy_from_slice(&sp[..n1]);
                    if bal == 1 {
                        bp[n1] = sa as u8;
                    }
                    continue;
                }
                let hasa = mul255(ha, sa);
                let invsa = if sa != 0 { 255 * 256 / sa } else { 0 };
                let invba = if ba != 0 { 255 * 256 / ba } else { 0 };
                let ra = hasa + mul255(255 - ha, ba);
                for k in 0..n1 {
                    let sc = (i32::from(sp[k]) * invsa) >> 8;
                    let bc = (i32::from(bp[k]) * invba) >> 8;
                    let rc = mul255(255 - ha, bc) + mul255(ha, sc);
                    bp[k] = mul255(ra, rc) as u8;
                }
                if bal == 1 {
                    bp[n1] = ra as u8;
                }
            }
        }
    }

    /// `fz_paint_pixmap_with_mask`: `src` over `self` through `msk`.
    pub fn paint_with_mask(&mut self, src: &Pix, msk: &Pix) {
        let r = self.bbox().intersect(src.bbox()).intersect(msk.bbox());
        if r.is_empty() || self.n != src.n || msk.n != 1 {
            return;
        }
        let n = self.n;
        let alpha_at = src.alpha.then(|| n - 1);
        let w = r.width() as usize;
        for y in r.y0..r.y1 {
            let d0 = self.at(r.x0, y);
            let s0 = src.at(r.x0, y);
            let m0 = msk.at(r.x0, y);
            for i in 0..w {
                let ma = i32::from(msk.samples[m0 + i]);
                let d = d0 + i * n;
                let s = s0 + i * n;
                // template_span_with_mask_N_general: every component,
                // alpha included, blended by the mask.
                if ma == 0 || alpha_at.is_some_and(|a| src.samples[s + a] == 0) {
                    continue;
                }
                if ma == 255 {
                    self.samples[d..d + n].copy_from_slice(&src.samples[s..s + n]);
                } else {
                    let ma = expand(ma);
                    for k in 0..n {
                        self.samples[d + k] = blend(
                            i32::from(src.samples[s + k]),
                            i32::from(self.samples[d + k]),
                            ma,
                        ) as u8;
                    }
                }
            }
        }
    }

    /// The pixmap as `pixmap::Pixmap`, for writing out.
    pub fn to_pixmap(
        &self,
        colorspace: Option<crate::colorspace::ColorSpace>,
    ) -> crate::pixmap::Pixmap {
        crate::pixmap::Pixmap {
            w: self.w as usize,
            h: self.h as usize,
            n: self.n,
            alpha: self.alpha,
            colorspace,
            samples: self.samples.clone(),
        }
    }
}

fn screen(b: i32, s: i32) -> i32 {
    b + s - mul255(b, s)
}

fn hard_light(b: i32, s: i32) -> i32 {
    let s2 = s << 1;
    if s <= 127 {
        mul255(b, s2)
    } else {
        screen(b, s2 - 255)
    }
}

/// The separable blend modes of draw-blend.c, on bytes.
fn separable(mode: u8, b: i32, s: i32) -> i32 {
    match mode {
        1 => mul255(b, s),
        2 => screen(b, s),
        3 => hard_light(s, b),
        4 => b.min(s),
        5 => b.max(s),
        6 => {
            let s = 255 - s;
            if b <= 0 {
                0
            } else if b >= s {
                255
            } else {
                (0x1fe * b + s) / (s << 1)
            }
        }
        7 => {
            let b = 255 - b;
            if b <= 0 {
                255
            } else if b >= s {
                0
            } else {
                0xff - (0x1fe * b + s) / (s << 1)
            }
        }
        8 => hard_light(b, s),
        9 => {
            if s < 128 {
                b - mul255(mul255(255 - (s << 1), b), 255 - b)
            } else {
                let dbd = if b < 64 {
                    mul255(mul255((b << 4) - 3060, b) + 1020, b)
                } else {
                    (255.0f32 * b as f32).sqrt() as i32
                };
                b + mul255((s << 1) - 255, dbd - b)
            }
        }
        10 => (b - s).abs(),
        11 => b + s - (mul255(b, s) << 1),
        _ => s,
    }
}

/// `fz_luminosity_rgb`.
fn luminosity_rgb(rb: i32, gb: i32, bb: i32, rs: i32, gs: i32, bs: i32) -> [i32; 3] {
    let delta = ((rs - rb) * 77 + (gs - gb) * 151 + (bs - bb) * 28 + 0x80) >> 8;
    let (mut r, mut g, mut b) = (rb + delta, gb + delta, bb + delta);
    if (r | g | b) & 0x100 != 0 {
        let y = (rs * 77 + gs * 151 + bs * 28 + 0x80) >> 8;
        let scale = if delta > 0 {
            let max = r.max(g.max(b));
            if max == y {
                0
            } else {
                ((255 - y) << 16) / (max - y)
            }
        } else {
            let min = r.min(g.min(b));
            if y == min { 0 } else { (y << 16) / (y - min) }
        };
        r = y + (((r - y) * scale + 0x8000) >> 16);
        g = y + (((g - y) * scale + 0x8000) >> 16);
        b = y + (((b - y) * scale + 0x8000) >> 16);
    }
    [r.clamp(0, 255), g.clamp(0, 255), b.clamp(0, 255)]
}

/// `fz_saturation_rgb`.
fn saturation_rgb(rb: i32, gb: i32, bb: i32, rs: i32, gs: i32, bs: i32) -> [i32; 3] {
    let minb = rb.min(gb.min(bb));
    let maxb = rb.max(gb.max(bb));
    if minb == maxb {
        let g = gb.clamp(0, 255);
        return [g, g, g];
    }
    let mins = rs.min(gs.min(bs));
    let maxs = rs.max(gs.max(bs));
    let mut scale = ((maxs - mins) << 16) / (maxb - minb);
    let y = (rb * 77 + gb * 151 + bb * 28 + 0x80) >> 8;
    let mut r = y + ((((rb - y) * scale) + 0x8000) >> 16);
    let mut g = y + ((((gb - y) * scale) + 0x8000) >> 16);
    let mut b = y + ((((bb - y) * scale) + 0x8000) >> 16);
    if (r | g | b) & 0x100 != 0 {
        let min = r.min(g.min(b));
        let max = r.max(g.max(b));
        let smin = if min < 0 {
            (y << 16) / (y - min)
        } else {
            0x10000
        };
        let smax = if max > 255 {
            ((255 - y) << 16) / (max - y)
        } else {
            0x10000
        };
        scale = smin.min(smax);
        r = y + (((r - y) * scale + 0x8000) >> 16);
        g = y + (((g - y) * scale + 0x8000) >> 16);
        b = y + (((b - y) * scale + 0x8000) >> 16);
    }
    [r.clamp(0, 255), g.clamp(0, 255), b.clamp(0, 255)]
}

/// One pixel of `fz_blend_nonseparable_nonisolated` for RGB. The results
/// are bytes in MuPDF, and wrap as bytes do.
#[allow(clippy::too_many_arguments)]
fn nonseparable_nonisolated(
    bp: &mut [u8],
    bal: usize,
    sp: &[u8],
    sal: usize,
    ha: i32,
    haa: i32,
    alpha: i32,
    mode: u8,
) {
    let n = 3;
    let sa = if sal == 1 { i32::from(sp[n]) } else { 255 };
    let ba = if bal == 1 { i32::from(bp[n]) } else { 255 };
    if ba == 0 && alpha == 255 {
        let len = n + usize::from(sal == 1 && bal == 1);
        bp[..len].copy_from_slice(&sp[..len]);
        if bal == 1 && sal == 0 {
            bp[n] = 255;
        }
        return;
    }
    let bahaa = mul255(ba, haa);
    let ra0 = ba - bahaa;
    let ra = ra0 + haa;
    if bal == 1 {
        bp[n] = ra as u8;
    }
    if ra == 0 {
        return;
    }
    let invha = if ha != 0 { 255 * 256 / ha } else { 0 };
    let invsa = if sa != 0 { 255 * 256 / sa } else { 0 };
    let invba = if ba != 0 { 255 * 256 / ba } else { 0 };
    let s = |k: usize| (i32::from(sp[k]) * invsa) >> 8;
    let b = |k: usize| (i32::from(bp[k]) * invba) >> 8;
    let (br, bg, bb) = (b(0), b(1), b(2));
    let unc = |sc: i32, bc: i32| ((((sc - bc) * invha) >> 8) + bc).clamp(0, 255);
    let (sr, sg, sb) = (unc(s(0), br), unc(s(1), bg), unc(s(2), bb));
    let rc = match mode {
        13 => saturation_rgb(br, bg, bb, sr, sg, sb),
        14 => luminosity_rgb(sr, sg, sb, br, bg, bb),
        15 => luminosity_rgb(br, bg, bb, sr, sg, sb),
        _ => {
            let t = luminosity_rgb(sr, sg, sb, br, bg, bb);
            saturation_rgb(t[0], t[1], t[2], br, bg, bb)
        }
    };
    let mut r = rc.map(|v| v as u8);
    if bahaa != 255 {
        r = r.map(|v| mul255(bahaa, i32::from(v)) as u8);
    }
    let (sc, bc) = ([sr, sg, sb], [br, bg, bb]);
    if ba != 255 {
        let t = mul255(255 - ba, haa);
        for k in 0..3 {
            r[k] = r[k].wrapping_add(mul255(t, sc[k]) as u8);
        }
    }
    if ra0 != 0 {
        for k in 0..3 {
            r[k] = r[k].wrapping_add(mul255(ra0, bc[k]) as u8);
        }
    }
    bp[..3].copy_from_slice(&r);
}

/// One pixel of `fz_blend_separable` or `fz_blend_nonseparable`.
fn blend_px(bp: &mut [u8], bal: usize, sp: &[u8], sal: usize, n: usize, mode: u8) {
    let sa = if sal == 1 { i32::from(sp[n]) } else { 255 };
    if sa == 0 {
        return;
    }
    let ba = if bal == 1 { i32::from(bp[n]) } else { 255 };
    if ba == 0 {
        let len = n + usize::from(sal == 1 && bal == 1);
        bp[..len].copy_from_slice(&sp[..len]);
        return;
    }
    let saba = mul255(sa, ba);
    let invsa = 255 * 256 / sa;
    let invba = 255 * 256 / ba;
    let un_s = |k: usize| (i32::from(sp[k]) * invsa) >> 8;
    let un_b = |bp: &[u8], k: usize| (i32::from(bp[k]) * invba) >> 8;
    if mode >= 12 && n == 3 {
        let (sr, sg, sb) = (un_s(0), un_s(1), un_s(2));
        let (br, bg, bb) = (un_b(bp, 0), un_b(bp, 1), un_b(bp, 2));
        let rc = match mode {
            13 => saturation_rgb(br, bg, bb, sr, sg, sb),
            14 => luminosity_rgb(sr, sg, sb, br, bg, bb),
            15 => luminosity_rgb(br, bg, bb, sr, sg, sb),
            _ => {
                let t = luminosity_rgb(sr, sg, sb, br, bg, bb);
                saturation_rgb(t[0], t[1], t[2], br, bg, bb)
            }
        };
        for k in 0..3 {
            bp[k] = (mul255(255 - sa, i32::from(bp[k]))
                + mul255(255 - ba, i32::from(sp[k]))
                + mul255(saba, rc[k])) as u8;
        }
    } else {
        for k in 0..n {
            let sc = un_s(k);
            let bc = un_b(bp, k);
            let rc = if mode >= 12 {
                if mode == 15 { sc } else { bc }
            } else {
                separable(mode, bc, sc)
            };
            bp[k] = (mul255(255 - sa, i32::from(bp[k]))
                + mul255(255 - ba, i32::from(sp[k]))
                + mul255(saba, rc)) as u8;
        }
    }
    if bal == 1 {
        bp[n] = (ba + sa - saba) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_blend_by_coverage() {
        let mut p = Pix::new(IRect::new(0, 0, 3, 1), 3, false).unwrap_or_else(|e| panic!("{e}"));
        p.clear_with(255);
        p.span_with_color(0, 0, &[255, 128, 0], &[0, 0, 0, 255]);
        assert_eq!(&p.samples[..3], &[0, 0, 0]);
        // 255 - 255 * 129 / 256, as MuPDF rounds it.
        assert_eq!(p.samples[3], 126);
        assert_eq!(&p.samples[6..9], &[255, 255, 255]);
    }

    #[test]
    fn spans_outside_a_pixmap_leave_it_be() {
        let mut rgb =
            Pix::new(IRect::new(10, 10, 14, 12), 3, false).unwrap_or_else(|e| panic!("{e}"));
        let mut mask =
            Pix::new(IRect::new(10, 10, 14, 12), 1, true).unwrap_or_else(|e| panic!("{e}"));
        let before = (rgb.clone(), mask.clone());
        let cov = [255u8; 4];
        // Rows above and below, and runs wholly to either side.
        for (x, y) in [(10, 9), (10, 12), (2, 10), (14, 11)] {
            rgb.span_with_color(x, y, &cov, &[1, 2, 3, 255]);
            mask.span_mask(x, y, &cov);
            mask.glyph_mask_row(x, y, &cov);
            mask.glyph_alpha_row(x, y, &cov, 128, true);
        }
        // A glyph row into a pixmap with colors does nothing.
        rgb.glyph_mask_row(10, 10, &cov);
        assert_eq!((rgb, mask), before);
        // Pixmaps past MuPDF's size limit are refused.
        assert!(Pix::new(IRect::new(0, 0, 1 << 16, 1 << 14), 3, false).is_err());
    }

    #[test]
    fn glyph_rows_paint_planes_as_mupdfs_mask_painters_do() {
        // Into an alpha plane holding 100: a solid run, an edge, and
        // nothing, at full alpha and at 128, as RLE glyphs and as small
        // ones (span painters).
        let row = |alpha: u8, rle: bool| {
            let mut p = Pix::new(IRect::new(0, 0, 4, 1), 1, true).unwrap_or_else(|e| panic!("{e}"));
            p.samples = vec![100, 100, 0, 100];
            p.glyph_alpha_row(0, 0, &[255, 128, 128, 0], alpha, rle);
            p.samples
        };
        // fz_paint_glyph_mask: solid is 255; an edge over 0 is the edge.
        assert_eq!(row(255, true), [255, 178, 128, 100]);
        // paint_span_0_da_sa: the coverage over what is there.
        assert_eq!(row(255, false), [255, 177, 128, 100]);
        // fz_paint_glyph_mask_alpha: solid is the alpha, replacing.
        assert_eq!(row(128, true), [128, 114, 64, 100]);
        // paint_span_0_da_sa_alpha.
        assert_eq!(row(128, false), [177, 139, 64, 100]);
    }
}
