// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Following FreeType 2 src/smooth/ftgrays.c (the anti-aliasing
// rasterizer), src/smooth/ftsmooth.c (ft_smooth_render and its overlap
// oversampling), src/base/ftoutln.c (FT_Outline_Decompose,
// FT_Outline_Get_CBox, FT_Outline_Transform, FT_Outline_Translate) and
// src/base/ftobjs.c (ft_glyphslot_preset_bitmap), Copyright (C) 1996-2025
// by David Turner, Robert Wilhelm, and Werner Lemberg, used under the
// FreeType License (NOTICE.md).

//! FreeType's smooth renderer, which MuPDF draws glyphs with: an outline
//! in 26.6 pixels, its exact coverage of each pixel as 0..255.

pub use crate::font::outline::{Outline, TAG_CONIC, TAG_CUBIC, TAG_ON};

/// `FT_Outline_Funcs`.
pub trait Sink {
    fn move_to(&mut self, x: i64, y: i64);
    fn line_to(&mut self, x: i64, y: i64);
    fn conic_to(&mut self, cx: i64, cy: i64, x: i64, y: i64);
    fn cubic_to(&mut self, c1: (i64, i64), c2: (i64, i64), to: (i64, i64));
}

/// `FT_Outline_Decompose` with no shift or delta. False for an invalid
/// outline, after what came before it.
pub fn decompose(o: &Outline, s: &mut dyn Sink) -> bool {
    let tag = |i: usize| o.tags.get(i).map_or(TAG_ON, |t| t & 3);
    let pt = |i: usize| o.points.get(i).copied().unwrap_or((0, 0));
    let mut last: isize = -1;
    for &end in &o.contours {
        let first = (last + 1) as usize;
        let end_i = end as isize;
        if end_i < first as isize || end >= o.points.len() {
            return false;
        }
        last = end_i;
        let mut limit = end;
        let mut start = pt(first);
        let v_last = pt(end);
        let mut control;
        // `point` is one before the next point to read.
        let mut point = first as isize;
        let t0 = tag(first);
        if t0 == TAG_CUBIC {
            return false;
        }
        if t0 == TAG_CONIC {
            if tag(end) == TAG_ON {
                start = v_last;
                if limit == 0 {
                    return false;
                }
                limit -= 1;
            } else {
                start = ((start.0 + v_last.0) / 2, (start.1 + v_last.1) / 2);
            }
            point -= 1;
        }
        s.move_to(start.0, start.1);
        let limit = limit as isize;
        let mut closed = false;
        while point < limit {
            point += 1;
            let p = point as usize;
            match tag(p) {
                TAG_ON => s.line_to(pt(p).0, pt(p).1),
                TAG_CONIC => {
                    control = pt(p);
                    loop {
                        if point < limit {
                            point += 1;
                            let q = point as usize;
                            let v = pt(q);
                            match tag(q) {
                                TAG_ON => {
                                    s.conic_to(control.0, control.1, v.0, v.1);
                                    break;
                                }
                                TAG_CONIC => {
                                    let mid = ((control.0 + v.0) / 2, (control.1 + v.1) / 2);
                                    s.conic_to(control.0, control.1, mid.0, mid.1);
                                    control = v;
                                }
                                _ => return false,
                            }
                        } else {
                            s.conic_to(control.0, control.1, start.0, start.1);
                            closed = true;
                            break;
                        }
                    }
                    if closed {
                        break;
                    }
                }
                _ => {
                    if point + 1 > limit || tag(p + 1) != TAG_CUBIC {
                        return false;
                    }
                    point += 2;
                    let c1 = pt(p);
                    let c2 = pt(p + 1);
                    if point <= limit {
                        s.cubic_to(c1, c2, pt(point as usize));
                    } else {
                        s.cubic_to(c1, c2, start);
                        closed = true;
                        break;
                    }
                }
            }
        }
        if !closed {
            s.line_to(start.0, start.1);
        }
    }
    true
}

const PIXEL_BITS: u32 = 8;
const ONE_PIXEL: i64 = 1 << PIXEL_BITS;

fn trunc(x: i64) -> i32 {
    (x >> PIXEL_BITS) as i32
}

fn fract(x: i64) -> i32 {
    (x & (ONE_PIXEL - 1)) as i32
}

fn upscale(x: i64) -> i64 {
    x * (ONE_PIXEL >> 6)
}

#[derive(Debug, Clone, Copy)]
struct Cell {
    x: i32,
    cover: i32,
    area: i32,
}

/// `gray_TWorker` over one band: the cells of each row in x order.
struct Worker {
    min_ex: i32,
    max_ex: i32,
    min_ey: i32,
    max_ey: i32,
    rows: Vec<Vec<Cell>>,
    /// The current cell: its row and index, or None for the null cell.
    cell: Option<(usize, usize)>,
    x: i64,
    y: i64,
}

/// `FT_UDIVPREP`: the reciprocal the divisions of a line use.
fn udiv_prep(c: bool, b: i64) -> i64 {
    if c { 0xFFFF_FFFF_i64 / b } else { 0 }
}

/// `FT_UDIV` with a reciprocal from [`udiv_prep`].
fn udiv(a: i64, r: i64) -> i32 {
    ((a as u64).wrapping_mul(r as u64) >> 32) as i32
}

impl Worker {
    fn set_cell(&mut self, ex: i32, ey: i32) {
        let index = ey.wrapping_sub(self.min_ey);
        if index < 0 || index >= self.max_ey - self.min_ey || ex >= self.max_ex {
            self.cell = None;
            return;
        }
        let ex = ex.max(self.min_ex - 1);
        let row = &mut self.rows[index as usize];
        let at = row.partition_point(|c| c.x < ex);
        if row.get(at).is_none_or(|c| c.x != ex) {
            row.insert(
                at,
                Cell {
                    x: ex,
                    cover: 0,
                    area: 0,
                },
            );
        }
        self.cell = Some((index as usize, at));
    }

    /// `FT_INTEGRATE`.
    fn integrate(&mut self, a: i32, b: i32) {
        if let Some((r, i)) = self.cell
            && let Some(c) = self.rows.get_mut(r).and_then(|row| row.get_mut(i))
        {
            c.cover = c.cover.wrapping_add(a);
            c.area = c.area.wrapping_add(a.wrapping_mul(b));
        }
    }

    /// `gray_render_line`, the 64-bit version.
    fn render_line(&mut self, to_x: i64, to_y: i64) {
        let mut ey1 = trunc(self.y);
        let ey2 = trunc(to_y);
        if (ey1 >= self.max_ey && ey2 >= self.max_ey) || (ey1 < self.min_ey && ey2 < self.min_ey) {
            self.x = to_x;
            self.y = to_y;
            return;
        }
        let mut ex1 = trunc(self.x);
        let ex2 = trunc(to_x);
        let mut fx1 = fract(self.x);
        let mut fy1 = fract(self.y);
        let dx = to_x - self.x;
        let dy = to_y - self.y;
        let one = ONE_PIXEL as i32;
        if ex1 == ex2 && ey1 == ey2 {
        } else if dy == 0 {
            self.set_cell(ex2, ey2);
            self.x = to_x;
            self.y = to_y;
            return;
        } else if dx == 0 {
            if dy > 0 {
                loop {
                    self.integrate(one - fy1, fx1 * 2);
                    fy1 = 0;
                    ey1 += 1;
                    self.set_cell(ex1, ey1);
                    if ey1 == ey2 {
                        break;
                    }
                }
            } else {
                loop {
                    self.integrate(-fy1, fx1 * 2);
                    fy1 = one;
                    ey1 -= 1;
                    self.set_cell(ex1, ey1);
                    if ey1 == ey2 {
                        break;
                    }
                }
            }
        } else {
            let mut prod = dx * i64::from(fy1) - dy * i64::from(fx1);
            let dx_r = udiv_prep(ex1 != ex2, dx);
            let dy_r = udiv_prep(ey1 != ey2, dy);
            // A line crosses at most this many cells; a bound for bad input.
            let mut guard = (i64::from(ex1 - ex2).abs() + i64::from(ey1 - ey2).abs()) * 2 + 4;
            loop {
                let fx2;
                let fy2;
                if prod - dx * ONE_PIXEL > 0 && prod <= 0 {
                    fx2 = 0;
                    fy2 = udiv(-prod, -dx_r);
                    prod -= dy * ONE_PIXEL;
                    self.integrate(fy2 - fy1, fx1 + fx2);
                    fx1 = one;
                    fy1 = fy2;
                    ex1 -= 1;
                } else if prod - dx * ONE_PIXEL + dy * ONE_PIXEL > 0 && prod - dx * ONE_PIXEL <= 0 {
                    prod -= dx * ONE_PIXEL;
                    fx2 = udiv(-prod, dy_r);
                    fy2 = one;
                    self.integrate(fy2 - fy1, fx1 + fx2);
                    fx1 = fx2;
                    fy1 = 0;
                    ey1 += 1;
                } else if prod + dy * ONE_PIXEL >= 0 && prod - dx * ONE_PIXEL + dy * ONE_PIXEL <= 0
                {
                    prod += dy * ONE_PIXEL;
                    fx2 = one;
                    fy2 = udiv(prod, dx_r);
                    self.integrate(fy2 - fy1, fx1 + fx2);
                    fx1 = 0;
                    fy1 = fy2;
                    ex1 += 1;
                } else {
                    fx2 = udiv(prod, -dy_r);
                    fy2 = 0;
                    prod += dx * ONE_PIXEL;
                    self.integrate(fy2 - fy1, fx1 + fx2);
                    fx1 = fx2;
                    fy1 = one;
                    ey1 -= 1;
                }
                self.set_cell(ex1, ey1);
                guard -= 1;
                if (ex1 == ex2 && ey1 == ey2) || guard <= 0 {
                    break;
                }
            }
        }
        let fx2 = fract(to_x);
        let fy2 = fract(to_y);
        self.integrate(fy2 - fy1, fx1 + fx2);
        self.x = to_x;
        self.y = to_y;
    }

    /// `gray_render_conic`, the 64-bit version.
    fn render_conic(&mut self, cx: i64, cy: i64, tx: i64, ty: i64) {
        let p0 = (self.x, self.y);
        let p1 = (upscale(cx), upscale(cy));
        let p2 = (upscale(tx), upscale(ty));
        let (a, b, c) = (trunc(p0.1), trunc(p1.1), trunc(p2.1));
        if (a >= self.max_ey && b >= self.max_ey && c >= self.max_ey)
            || (a < self.min_ey && b < self.min_ey && c < self.min_ey)
        {
            self.x = p2.0;
            self.y = p2.1;
            return;
        }
        let bx = p1.0 - p0.0;
        let by = p1.1 - p0.1;
        let ax = p2.0 - p1.0 - bx;
        let ay = p2.1 - p1.1 - by;
        let mut dx = ax.abs().max(ay.abs());
        if dx <= ONE_PIXEL / 4 {
            self.render_line(p2.0, p2.1);
            return;
        }
        let mut shift: u32 = 16;
        loop {
            dx >>= 2;
            shift -= 1;
            if dx <= ONE_PIXEL / 4 || shift == 0 {
                break;
            }
        }
        let mut count = 0x10000u32 >> shift;
        let shl = |v: i64, s: u32| (v as u64).wrapping_shl(s) as i64;
        let mut rx = shl(ax, shift + shift);
        let mut ry = shl(ay, shift + shift);
        let mut qx = shl(bx, shift + 17).wrapping_add(rx);
        let mut qy = shl(by, shift + 17).wrapping_add(ry);
        rx = rx.wrapping_mul(2);
        ry = ry.wrapping_mul(2);
        let mut px = shl(p0.0, 32);
        let mut py = shl(p0.1, 32);
        loop {
            px = px.wrapping_add(qx);
            py = py.wrapping_add(qy);
            qx = qx.wrapping_add(rx);
            qy = qy.wrapping_add(ry);
            self.render_line(px >> 32, py >> 32);
            count -= 1;
            if count == 0 {
                break;
            }
        }
    }

    /// `gray_render_cubic`.
    fn render_cubic(&mut self, c1: (i64, i64), c2: (i64, i64), to: (i64, i64)) {
        let mut stack = [(0i64, 0i64); 16 * 3 + 1];
        stack[0] = (upscale(to.0), upscale(to.1));
        stack[1] = (upscale(c2.0), upscale(c2.1));
        stack[2] = (upscale(c1.0), upscale(c1.1));
        stack[3] = (self.x, self.y);
        let ys = [stack[0].1, stack[1].1, stack[2].1, stack[3].1].map(trunc);
        if ys.iter().all(|&y| y >= self.max_ey) || ys.iter().all(|&y| y < self.min_ey) {
            self.x = stack[0].0;
            self.y = stack[0].1;
            return;
        }
        let mut arc = 0usize;
        loop {
            let a = &stack[arc..arc + 4];
            let split = (2 * a[0].0 - 3 * a[1].0 + a[3].0).abs() > ONE_PIXEL / 2
                || (2 * a[0].1 - 3 * a[1].1 + a[3].1).abs() > ONE_PIXEL / 2
                || (a[0].0 - 3 * a[2].0 + 2 * a[3].0).abs() > ONE_PIXEL / 2
                || (a[0].1 - 3 * a[2].1 + 2 * a[3].1).abs() > ONE_PIXEL / 2;
            if split && arc + 7 <= stack.len() {
                split_cubic(&mut stack[arc..arc + 7]);
                arc += 3;
                continue;
            }
            self.render_line(stack[arc].0, stack[arc].1);
            if arc == 0 {
                return;
            }
            arc -= 3;
        }
    }
}

/// `gray_split_cubic`.
fn split_cubic(base: &mut [(i64, i64)]) {
    let one = |v: &mut [i64; 7]| {
        v[6] = v[3];
        let mut a = v[0] + v[1];
        let b = v[1] + v[2];
        let mut c = v[2] + v[3];
        v[5] = c >> 1;
        c += b;
        v[4] = c >> 2;
        v[1] = a >> 1;
        a += b;
        v[2] = a >> 2;
        v[3] = (a + c) >> 3;
    };
    let mut xs = [0i64; 7];
    let mut ys = [0i64; 7];
    for i in 0..7 {
        xs[i] = base[i].0;
        ys[i] = base[i].1;
    }
    one(&mut xs);
    one(&mut ys);
    for i in 0..7 {
        base[i] = (xs[i], ys[i]);
    }
}

impl Sink for Worker {
    fn move_to(&mut self, x: i64, y: i64) {
        let (x, y) = (upscale(x), upscale(y));
        self.set_cell(trunc(x), trunc(y));
        self.x = x;
        self.y = y;
    }
    fn line_to(&mut self, x: i64, y: i64) {
        self.render_line(upscale(x), upscale(y));
    }
    fn conic_to(&mut self, cx: i64, cy: i64, x: i64, y: i64) {
        self.render_conic(cx, cy, x, y);
    }
    fn cubic_to(&mut self, c1: (i64, i64), c2: (i64, i64), to: (i64, i64)) {
        self.render_cubic(c1, c2, to);
    }
}

/// `FT_FILL_RULE`.
fn fill_rule(area: i32, even_odd: bool) -> u8 {
    let fill: i32 = if even_odd { 0x100 } else { i32::MIN };
    let mut coverage = area >> (PIXEL_BITS * 2 + 1 - 8);
    if coverage & fill != 0 {
        coverage = !coverage;
    }
    if coverage > 255 && fill & i32::MIN != 0 {
        coverage = 255;
    }
    coverage as u8
}

/// `gray_convert_glyph` and a sweep: each row's spans of one coverage,
/// handed to `span(y, x, len, coverage)`, for the box `(x0, y0, x1, y1)`
/// in pixels.
fn convert(
    o: &Outline,
    clip: (i32, i32, i32, i32),
    span: &mut dyn FnMut(i32, i32, i32, u8),
) -> bool {
    let (x0, y0, x1, y1) = clip;
    if x0 >= x1 || y0 >= y1 || o.points.is_empty() || o.contours.is_empty() {
        return true;
    }
    if o.contours.last().is_none_or(|&l| l + 1 != o.points.len()) {
        return false;
    }
    let mut w = Worker {
        min_ex: x0,
        max_ex: x1,
        min_ey: y0,
        max_ey: y1,
        rows: vec![Vec::new(); (y1 - y0) as usize],
        cell: None,
        x: 0,
        y: 0,
    };
    if !decompose(o, &mut w) {
        return false;
    }
    let even_odd = o.even_odd;
    for (i, row) in w.rows.iter().enumerate() {
        let y = y0 + i as i32;
        let mut x = x0;
        let mut cover: i32 = 0;
        for cell in row {
            if cover != 0 && cell.x > x {
                span(y, x, cell.x - x, fill_rule(cover, even_odd));
            }
            cover = cover.wrapping_add(cell.cover.wrapping_mul(ONE_PIXEL as i32 * 2));
            let area = cover.wrapping_sub(cell.area);
            if area != 0 && cell.x >= x0 {
                span(y, cell.x, 1, fill_rule(area, even_odd));
            }
            x = cell.x + 1;
        }
        if cover != 0 {
            span(y, x, x1 - x, fill_rule(cover, even_odd));
        }
    }
    true
}

/// A rendered glyph: `w` by `h` coverage bytes, row 0 at the bottom (the
/// lowest y), placed with its lower left pixel at (`left`, `bottom`).
#[derive(Debug, Clone, PartialEq)]
pub struct Bitmap {
    pub left: i32,
    pub bottom: i32,
    pub w: i32,
    pub h: i32,
    pub data: Vec<u8>,
}

/// `FT_Render_Glyph` with `FT_RENDER_MODE_NORMAL`: the box as
/// `ft_glyphslot_preset_bitmap` sets it, then the coverage. None when the
/// box is unsuitable (`Raster_Overflow`) or the outline invalid. `ppem`
/// is the face's size, for the box's sanity limit.
pub fn render(outline: &Outline, ppem: i64) -> Option<Bitmap> {
    let (cx0, cy0, cx1, cy1) = outline.cbox();
    let px0 = (cx0 >> 6) + ((cx0 & 63) >> 6);
    let py0 = (cy0 >> 6) + ((cy0 & 63) >> 6);
    let px1 = (cx1 >> 6) + (((cx1 & 63) + 63) >> 6);
    let py1 = (cy1 >> 6) + (((cy1 & 63) + 63) >> 6);
    let width = px1 - px0;
    let height = py1 - py0;
    if width >= 0x10000
        || height >= 0x10000
        || px0 < -0x100_0000
        || px1 >= 0x100_0000
        || py0 < -0x100_0000
        || py1 >= 0x100_0000
        || width > 10 * ppem
        || height > 10 * ppem
    {
        return None;
    }
    let (w, h) = (width as i32, height as i32);
    let mut bm = Bitmap {
        left: px0 as i32,
        bottom: py0 as i32,
        w,
        h,
        data: vec![0; (w.max(0) as usize) * (h.max(0) as usize)],
    };
    if w <= 0 || h <= 0 {
        return Some(bm);
    }
    let mut o = outline.clone();
    o.translate(-64 * px0, -64 * py0);
    let stride = w as usize;
    let ok = if o.overlap {
        // ft_smooth_raster_overlap: four times over in each direction,
        // the spans averaged.
        const SCALE: i64 = 4;
        if i64::from(w) * SCALE > 0x7FFF {
            return None;
        }
        for p in &mut o.points {
            p.0 *= SCALE;
            p.1 *= SCALE;
        }
        let data = &mut bm.data;
        let clip = (0, 0, w * SCALE as i32, h * SCALE as i32);
        convert(&o, clip, &mut |y, x, len, coverage| {
            let cover = (u32::from(coverage) + (SCALE * SCALE / 2) as u32) / (SCALE * SCALE) as u32;
            let row = (y as i64 / SCALE) as usize * stride;
            for i in 0..len {
                let at = row + ((x + i) as i64 / SCALE) as usize;
                if let Some(d) = data.get_mut(at) {
                    let sum = u32::from(*d) + cover;
                    *d = (sum - (sum >> 8)) as u8;
                }
            }
        })
    } else {
        let data = &mut bm.data;
        convert(&o, (0, 0, w, h), &mut |y, x, len, coverage| {
            let row = y as usize * stride;
            for i in 0..len {
                if let Some(d) = data.get_mut(row + (x + i) as usize) {
                    *d = coverage;
                }
            }
        })
    };
    ok.then_some(bm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x0: i64, y0: i64, x1: i64, y1: i64) -> Outline {
        Outline {
            points: vec![(x0, y0), (x0, y1), (x1, y1), (x1, y0)],
            tags: vec![TAG_ON; 4],
            contours: vec![3],
            even_odd: false,
            overlap: false,
        }
    }

    #[test]
    fn a_pixel_square_is_covered_exactly() {
        let bm = render(&square(64, 64, 192, 128), 100).unwrap_or_else(|| panic!("render failed"));
        assert_eq!((bm.left, bm.bottom, bm.w, bm.h), (1, 1, 2, 1));
        assert_eq!(bm.data, vec![255, 255]);
    }

    fn outline(pts: &[(u8, i64, i64)], contours: &[usize]) -> Outline {
        Outline {
            points: pts.iter().map(|&(_, x, y)| (x, y)).collect(),
            tags: pts.iter().map(|&(t, _, _)| t).collect(),
            contours: contours.to_vec(),
            even_odd: false,
            overlap: false,
        }
    }

    fn draw(o: &Outline) -> Bitmap {
        render(o, 1000).unwrap_or_else(|| panic!("render failed"))
    }

    const C: u8 = TAG_CONIC;
    const ON: u8 = TAG_ON;
    const CU: u8 = TAG_CUBIC;

    #[test]
    fn conic_contours_close_the_same_however_they_start() {
        // A rounded square, 40 pixels, from its on-curve points; from its
        // corners alone, the on-curve points implied between them; and
        // starting on a corner with the last point on the curve.
        let s = 2560;
        let explicit = outline(
            &[
                (ON, s / 2, 0),
                (C, s, 0),
                (ON, s, s / 2),
                (C, s, s),
                (ON, s / 2, s),
                (C, 0, s),
                (ON, 0, s / 2),
                (C, 0, 0),
            ],
            &[7],
        );
        let implied = outline(&[(C, s, 0), (C, s, s), (C, 0, s), (C, 0, 0)], &[3]);
        let last_on = outline(
            &[
                (C, s, 0),
                (ON, s, s / 2),
                (C, s, s),
                (ON, s / 2, s),
                (C, 0, s),
                (ON, 0, s / 2),
                (C, 0, 0),
                (ON, s / 2, 0),
            ],
            &[7],
        );
        let a = draw(&explicit);
        assert_eq!(a.data, draw(&implied).data);
        assert_eq!(a.data, draw(&last_on).data);
        // Inside full, the corners rounded off.
        assert_eq!(a.data[20 * 40 + 20], 255);
        assert!(a.data[0] < 255);
        let ink: u32 = a.data.iter().map(|&v| u32::from(v)).sum();
        assert!(ink > 255 * 1300 && ink < 255 * 1600, "{ink}");
    }

    #[test]
    fn cubic_contours_close_to_their_start() {
        // A cubic blob, closed by a last on-curve point and by a cubic
        // that runs back to the start.
        let closed = outline(
            &[
                (ON, 0, 0),
                (CU, 1500, -300),
                (CU, 2000, 1500),
                (ON, 1000, 2000),
                (CU, 0, 2500),
                (CU, -300, 800),
            ],
            &[5],
        );
        let explicit = outline(
            &[
                (ON, 0, 0),
                (CU, 1500, -300),
                (CU, 2000, 1500),
                (ON, 1000, 2000),
                (CU, 0, 2500),
                (CU, -300, 800),
                (ON, 0, 0),
            ],
            &[6],
        );
        assert_eq!(draw(&closed).data, draw(&explicit).data);
    }

    #[test]
    fn invalid_outlines_do_not_render() {
        let cases = [
            // A contour starting on a cubic control point.
            outline(&[(CU, 0, 0), (ON, 640, 0), (ON, 640, 640)], &[2]),
            // A cubic control point without its pair.
            outline(&[(ON, 0, 0), (CU, 640, 0), (ON, 640, 640)], &[2]),
            // A contour ending past the points, or before it starts.
            outline(&[(ON, 0, 0), (ON, 640, 0), (ON, 640, 640)], &[5]),
            outline(&[(ON, 0, 0), (ON, 640, 0), (ON, 640, 640)], &[2, 1]),
            // A conic followed by a cubic.
            outline(
                &[(ON, 0, 0), (C, 640, 0), (CU, 640, 640), (CU, 0, 640)],
                &[3],
            ),
        ];
        for (i, o) in cases.iter().enumerate() {
            assert!(render(o, 1000).is_none(), "case {i}");
        }
        // Boxes past ten times the size, as FreeType refuses them.
        assert!(render(&square(0, 0, 64 * 200, 64), 10).is_none());
        // An empty box is an empty bitmap.
        let empty = render(&square(0, 0, 0, 0), 10).unwrap_or_else(|| panic!("render failed"));
        assert!(empty.data.is_empty());
    }

    #[test]
    fn overlapping_contours_by_rule_and_oversampled() {
        // Two squares overlapping in the middle.
        let mut o = square(0, 0, 1280, 1280);
        let other = square(640, 640, 1920, 1920);
        o.points.extend(other.points);
        o.tags.extend(other.tags);
        o.contours.push(7);
        let nonzero = draw(&o);
        o.even_odd = true;
        let even_odd = draw(&o);
        // Pixel (15, 15) is in both squares: in for non-zero, out for
        // even-odd.
        let at = 15 * 30 + 15;
        assert_eq!((nonzero.data[at], even_odd.data[at]), (255, 0));
        o.even_odd = false;
        o.overlap = true;
        let over = draw(&o);
        assert_eq!(over.data[at], 255);
        assert_eq!(over.data[5 * 30 + 5], 255);
        assert_eq!(over.data[25 * 30 + 5], 0);
    }

    #[test]
    fn long_slopes_cover_what_their_area_says() {
        // Edges crossing many cells in x and in y, shallow and steep,
        // up and down: the coverage adds up to the shape's area.
        let pts = [(0, 0), (6400, 70), (6400, 6400), (6330, 6400)];
        let o = outline(&pts.map(|(x, y)| (ON, x, y)), &[3]);
        let bm = draw(&o);
        assert_eq!((bm.w, bm.h), (100, 100));
        let ink: u32 = bm.data.iter().map(|&v| u32::from(v)).sum();
        // The shoelace formula, in pixels.
        let twice: i64 = (0..4)
            .map(|i| {
                let (a, b) = (pts[i], pts[(i + 1) % 4]);
                a.0 * b.1 - b.0 * a.1
            })
            .sum();
        let area = twice as f64 / 2.0 / 4096.0;
        assert!((f64::from(ink) / 255.0 - area).abs() < 2.0, "{ink} {area}");
    }

    #[test]
    fn half_pixels_are_half_covered() {
        let bm = render(&square(32, 0, 96, 64), 100).unwrap_or_else(|| panic!("render failed"));
        assert_eq!((bm.left, bm.w, bm.h), (0, 2, 1));
        assert_eq!(bm.data, vec![128, 128]);
    }
}
