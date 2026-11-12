// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/draw-glyph.c (fz_subpixel_adjust,
// fz_render_glyph) and source/fitz/font.c (do_ft_render_glyph,
// fz_adjust_ft_glyph_width, fz_outline_ft_glyph),
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! Glyphs as MuPDF draws them: placed on a quarter (or finer) pixel grid,
//! rendered by FreeType's smooth rasterizer at that offset, and kept for
//! the rest of the page. Glyphs too large to cache come back as paths.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::font::outline::Outline;
use crate::fontdesc::Font;
use crate::geometry::{Matrix, Point};
use crate::path::Path;

use super::grays::{self, Bitmap};

/// `MAX_GLYPH_SIZE`: larger glyphs are filled as paths.
pub const MAX_GLYPH_SIZE: f32 = 256.0;

/// The ppem `FT_Set_Char_Size(face, 65536, 65536, 72, 72)` sets.
const PPEM: i64 = 1024;

/// `fz_subpixel_adjust`: `ctm`'s translation snapped to the subpixel
/// grid its size allows. Returns the matrix with only the subpixel part
/// of the translation, the size (expansion), and the quantized parts.
pub fn subpixel_adjust(ctm: &mut Matrix) -> (Matrix, f32, u8, u8) {
    let size = ctm.expansion();
    let (q, r) = if size >= 48.0 {
        (0, 0.5f32)
    } else if size >= 24.0 {
        (128, 0.25)
    } else {
        (192, 0.125)
    };
    let (qmin, rmin) = if size >= 8.0 {
        (0, 0.5f32)
    } else if size >= 4.0 {
        (128, 0.25)
    } else {
        (192, 0.125)
    };
    let (mut hq, mut hr, mut vq, mut vr) = (q, r, q, r);
    if ctm.a == 0.0 && ctm.d == 0.0 {
        hq = qmin;
        hr = rmin;
    }
    if ctm.b == 0.0 && ctm.c == 0.0 {
        vq = qmin;
        vr = rmin;
    }
    let mut sub = *ctm;
    sub.e = ctm.e + hr;
    let pix_e = sub.e.floor();
    sub.e -= pix_e;
    sub.f = ctm.f + vr;
    let pix_f = sub.f.floor();
    sub.f -= pix_f;
    let qe = ((sub.e * 256.0) as i32 & hq) as u8;
    sub.e = f32::from(qe) / 256.0;
    let qf = ((sub.f * 256.0) as i32 & vq) as u8;
    sub.f = f32::from(qf) / 256.0;
    ctm.e = sub.e + pix_e;
    ctm.f = sub.f + pix_f;
    (sub, size, qe, qf)
}

/// `fz_adjust_ft_glyph_width`: a substitute font's glyph stretched to the
/// width the PDF gives it.
fn adjust_width(font: &Font, gid: i32, trm: Matrix) -> Matrix {
    let (Some(face), Some(table)) = (font.face.as_ref(), font.width_table.as_deref()) else {
        return trm;
    };
    if !font.ft_stretch {
        return trm;
    }
    let adv = face.advance(gid as u32).unwrap_or(0) as f32;
    let realw = adv * 1000.0 / f32::from(face.units_per_em.max(1));
    let subw = usize::try_from(gid)
        .ok()
        .and_then(|g| table.0.get(g))
        .map_or(table.1 as f32, |&w| f32::from(w));
    if realw > 0.0 && subw > 0.0 {
        trm.pre_scale(subw / realw, 1.0)
    } else {
        trm
    }
}

/// `do_ft_render_glyph` with anti-aliasing, then `FT_Render_Glyph`.
fn render_ft(font: &Font, gid: i32, trm: Matrix) -> Option<Bitmap> {
    let face = font.face.as_ref()?;
    let trm = adjust_width(font, gid, trm);
    if [trm.a, trm.b, trm.c, trm.d]
        .iter()
        .any(|v| !(-512.0..=512.0).contains(v))
    {
        return None;
    }
    let mut outline = face.glyph_outline(u32::try_from(gid).ok()?)?.ok()?;
    // FT_Set_Transform: the matrix as 16.16 of a 1024 pixel em, the
    // translation in 26.6; both truncated as C converts them.
    let fixed = |v: f32| (v * 64.0) as i64;
    outline.transform(fixed(trm.a), fixed(trm.c), fixed(trm.b), fixed(trm.d));
    outline.translate(fixed(trm.e), fixed(trm.f));
    grays::render(&outline, PPEM)
}

/// The glyphs a page has drawn, by font, glyph, matrix and offset.
#[derive(Debug, Default)]
pub struct GlyphCache {
    map: HashMap<GlyphKey, Option<Rc<Bitmap>>>,
}

/// A glyph cache key: font, glyph, the matrix as 16.16 and the subpixel
/// offsets.
pub type GlyphKey = (usize, i32, [i32; 4], u8, u8);

impl GlyphCache {
    pub fn key(&self, font: &Arc<Font>, gid: i32, sub: Matrix, qe: u8, qf: u8) -> GlyphKey {
        let fixed = |v: f32| (v * 65536.0) as i32;
        (
            Arc::as_ptr(font) as usize,
            gid,
            [fixed(sub.a), fixed(sub.b), fixed(sub.c), fixed(sub.d)],
            qe,
            qf,
        )
    }

    pub fn get(&self, key: &GlyphKey) -> Option<Option<Rc<Bitmap>>> {
        self.map.get(key).cloned()
    }

    pub fn put(&mut self, key: GlyphKey, glyph: Option<Rc<Bitmap>>) {
        self.map.insert(key, glyph);
    }

    /// `fz_render_glyph` for a FreeType font: `ctm` (the glyph's matrix
    /// to device space) has its translation snapped. None for a glyph to
    /// fill as a path instead (too large, or one that does not render).
    pub fn render(&mut self, font: &Arc<Font>, gid: i32, ctm: &mut Matrix) -> Option<Rc<Bitmap>> {
        let (sub, size, qe, qf) = subpixel_adjust(ctm);
        if size > MAX_GLYPH_SIZE || font.face.is_none() {
            return None;
        }
        let fixed = |v: f32| (v * 65536.0) as i32;
        let key = (
            Arc::as_ptr(font) as usize,
            gid,
            [fixed(sub.a), fixed(sub.b), fixed(sub.c), fixed(sub.d)],
            qe,
            qf,
        );
        if let Some(v) = self.map.get(&key) {
            return v.clone();
        }
        let glyph = render_ft(font, gid, sub).map(Rc::new);
        // fz_render_glyph caches what is smaller than MAX_GLYPH_SIZE.
        if glyph
            .as_ref()
            .is_none_or(|g| (g.w as f32) < MAX_GLYPH_SIZE && (g.h as f32) < MAX_GLYPH_SIZE)
        {
            self.map.insert(key, glyph.clone());
        }
        glyph
    }
}

/// `fz_outline_ft_glyph`: the glyph as a path in the space `trm` maps
/// from text space (unhinted, where MuPDF lets FreeType hint).
pub fn outline_glyph(font: &Font, gid: i32, trm: Matrix) -> Option<Path> {
    let face = font.face.as_ref()?;
    let trm = adjust_width(font, gid, trm);
    let outline = face.glyph_outline(u32::try_from(gid).ok()?)?.ok()?;
    let m = Matrix::scale(1.0 / 65536.0, 1.0 / 65536.0).concat(trm);
    Some(outline_path(&outline, m))
}

/// `FT_Outline_Decompose` into a path through `m`, as MuPDF's closure
/// callbacks build it: a conic as `fz_quadto`, here as its cubic.
fn outline_path(o: &Outline, m: Matrix) -> Path {
    struct Pen {
        path: Path,
        m: Matrix,
        cur: Point,
        moved: bool,
    }
    impl Pen {
        fn pt(&self, x: i64, y: i64) -> Point {
            self.m.transform_xy(x as f32, y as f32)
        }
    }
    impl grays::Sink for Pen {
        fn move_to(&mut self, x: i64, y: i64) {
            let p = self.pt(x, y);
            self.path.move_to(p.x, p.y);
            self.cur = p;
            self.moved = true;
        }
        fn line_to(&mut self, x: i64, y: i64) {
            let p = self.pt(x, y);
            self.path.line_to(p.x, p.y);
            self.cur = p;
        }
        fn conic_to(&mut self, cx: i64, cy: i64, x: i64, y: i64) {
            let c = self.pt(cx, cy);
            let t = self.pt(x, y);
            let s = self.cur;
            self.path.curve_to(
                s.x + 2.0 * (c.x - s.x) / 3.0,
                s.y + 2.0 * (c.y - s.y) / 3.0,
                t.x + 2.0 * (c.x - t.x) / 3.0,
                t.y + 2.0 * (c.y - t.y) / 3.0,
                t.x,
                t.y,
            );
            self.cur = t;
        }
        fn cubic_to(&mut self, c1: (i64, i64), c2: (i64, i64), to: (i64, i64)) {
            let (a, b, t) = (
                self.pt(c1.0, c1.1),
                self.pt(c2.0, c2.1),
                self.pt(to.0, to.1),
            );
            self.path.curve_to(a.x, a.y, b.x, b.y, t.x, t.y);
            self.cur = t;
        }
    }
    let mut pen = Pen {
        path: Path::new(),
        m,
        cur: Point { x: 0.0, y: 0.0 },
        moved: false,
    };
    grays::decompose(o, &mut pen);
    if pen.moved {
        pen.path.close_path();
    }
    pen.path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_glyphs_snap_to_eighths() {
        let mut m = Matrix {
            a: 10.0,
            b: 0.0,
            c: 0.0,
            d: -10.0,
            e: 3.3,
            f: 7.9,
        };
        let (sub, size, qe, qf) = subpixel_adjust(&mut m);
        assert_eq!(size, 10.0);
        // e: 3.3 + 0.125 = 3.425, the fraction 0.425 * 256 = 108 & 192 = 64.
        assert_eq!(qe, 64);
        assert_eq!(sub.e, 0.25);
        assert_eq!(m.e, 3.25);
        // f: a horizontal matrix at size 10 snaps y to whole pixels.
        assert_eq!(qf, 0);
        assert_eq!(m.f, 8.0);
    }
}
