// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/draw-edge.c and source/fitz/draw-rasterize.c,
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! The global edge list: path segments as integer edges at 17 by 15
//! subsamples a pixel, stepped with Bresenham, swept a scanline at a time
//! into coverage (MuPDF's default 8-bit anti-aliasing, `fz_gel`).

use crate::geometry::IRect;

/// Subsamples across and down a pixel.
pub const HSCALE: i32 = 17;
pub const VSCALE: i32 = 15;

/// `BBOX_MIN` and `BBOX_MAX`: coordinates beyond these are clamped.
pub const BBOX_MIN: i32 = -(1 << 20);
pub const BBOX_MAX: i32 = 1 << 20;

/// `fz_idiv`: division rounding toward minus infinity.
pub fn idiv(a: i32, b: i32) -> i32 {
    if a < 0 { (a - b + 1) / b } else { a / b }
}

/// `fz_idiv_up`: division rounding toward plus infinity.
pub fn idiv_up(a: i32, b: i32) -> i32 {
    if a < 0 { a / b } else { (a + b - 1) / b }
}

#[derive(Debug, Clone, Copy, Default)]
struct Edge {
    x: i32,
    e: i32,
    h: i32,
    y: i32,
    adj_up: i32,
    adj_down: i32,
    xmove: i32,
    xdir: i32,
    ydir: i32,
}

/// `fz_gel`.
#[derive(Debug, Default)]
pub struct Gel {
    edges: Vec<Edge>,
    /// Indices into `edges`.
    active: Vec<usize>,
    alphas: Vec<u8>,
    deltas: Vec<i32>,
    /// In subsamples.
    pub bbox: IRect,
    pub clip: IRect,
}

#[derive(PartialEq, Eq)]
enum Lerp {
    Inside,
    Outside,
    Leave,
    Enter,
}

fn clip_lerp_x(val: i32, m: bool, x0: i32, y0: i32, x1: i32, y1: i32, out: &mut i32) -> Lerp {
    let v0out = if m { x0 > val } else { x0 < val };
    let v1out = if m { x1 > val } else { x1 < val };
    match (v0out, v1out) {
        (false, false) => Lerp::Inside,
        (true, true) => Lerp::Outside,
        (false, true) => {
            *out = y0 + ((y1 - y0) as f32 * (val - x0) as f32 / (x1 - x0) as f32) as i32;
            Lerp::Leave
        }
        (true, false) => {
            *out = y1 + ((y0 - y1) as f32 * (val - x1) as f32 / (x0 - x1) as f32) as i32;
            Lerp::Enter
        }
    }
}

fn clip_lerp_y(val: i32, m: bool, x0: i32, y0: i32, x1: i32, y1: i32, out: &mut i32) -> Lerp {
    clip_lerp_x(val, m, y0, x0, y1, x1, out)
}

impl Gel {
    pub fn new() -> Gel {
        let mut g = Gel::default();
        g.reset(IRect::INFINITE);
        g
    }

    /// `fz_reset_rasterizer`: empty, with a clip in pixels.
    pub fn reset(&mut self, clip: IRect) {
        if clip.is_infinite() {
            self.clip = IRect {
                x0: BBOX_MIN,
                y0: BBOX_MIN,
                x1: BBOX_MAX,
                y1: BBOX_MAX,
            };
        } else {
            self.clip = IRect {
                x0: clip.x0 * HSCALE,
                x1: clip.x1 * HSCALE,
                y0: clip.y0 * VSCALE,
                y1: clip.y1 * VSCALE,
            };
        }
        self.bbox = IRect {
            x0: BBOX_MAX,
            y0: BBOX_MAX,
            x1: BBOX_MIN,
            y1: BBOX_MIN,
        };
        self.edges.clear();
        self.active.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// `fz_bound_rasterizer`, in pixels.
    pub fn bound(&self) -> IRect {
        if self.bbox.x1 < self.bbox.x0 || self.bbox.y1 < self.bbox.y0 {
            return IRect::EMPTY;
        }
        IRect {
            x0: idiv(self.bbox.x0, HSCALE),
            y0: idiv(self.bbox.y0, VSCALE),
            x1: idiv_up(self.bbox.x1, HSCALE),
            y1: idiv_up(self.bbox.y1, VSCALE),
        }
    }

    /// `fz_scissor_rasterizer`: the clip in pixels, not rounded.
    pub fn scissor(&self) -> crate::geometry::Rect {
        crate::geometry::Rect::new(
            self.clip.x0 as f32 / HSCALE as f32,
            self.clip.y0 as f32 / VSCALE as f32,
            self.clip.x1 as f32 / HSCALE as f32,
            self.clip.y1 as f32 / VSCALE as f32,
        )
    }

    /// `fz_clip_rasterizer`, in pixels.
    pub fn clip_box(&self) -> IRect {
        IRect {
            x0: idiv(self.clip.x0, HSCALE),
            y0: idiv(self.clip.y0, VSCALE),
            x1: idiv_up(self.clip.x1, HSCALE),
            y1: idiv_up(self.clip.y1, VSCALE),
        }
    }

    fn insert_raw(&mut self, mut x0: i32, mut y0: i32, mut x1: i32, mut y1: i32) {
        if y0 == y1 {
            return;
        }
        let winding = if y0 > y1 {
            std::mem::swap(&mut x0, &mut x1);
            std::mem::swap(&mut y0, &mut y1);
            -1
        } else {
            1
        };
        let b = &mut self.bbox;
        b.x0 = b.x0.min(x0).min(x1);
        b.x1 = b.x1.max(x0).max(x1);
        b.y0 = b.y0.min(y0);
        b.y1 = b.y1.max(y1);
        let dy = y1 - y0;
        let dx = x1 - x0;
        let width = dx.abs();
        let mut e = Edge {
            xdir: if dx > 0 { 1 } else { -1 },
            ydir: winding,
            x: x0,
            y: y0,
            h: dy,
            adj_down: dy,
            e: if dx >= 0 { 0 } else { -dy + 1 },
            ..Edge::default()
        };
        if dy >= width {
            e.xmove = 0;
            e.adj_up = width;
        } else {
            e.xmove = (width / dy) * e.xdir;
            e.adj_up = width % dy;
        }
        self.edges.push(e);
    }

    /// `fz_insert_gel`: a segment in pixels, clipped.
    pub fn insert(&mut self, fx0: f32, fy0: f32, fx1: f32, fy1: f32) {
        let clampf = |v: f32, lo: i32, hi: i32| v.clamp(lo as f32, hi as f32) as i32;
        let mut x0 = clampf(
            (fx0 * HSCALE as f32).floor(),
            BBOX_MIN * HSCALE,
            BBOX_MAX * HSCALE,
        );
        let mut y0 = clampf(
            (fy0 * VSCALE as f32).floor(),
            BBOX_MIN * VSCALE,
            BBOX_MAX * VSCALE,
        );
        let mut x1 = clampf(
            (fx1 * HSCALE as f32).floor(),
            BBOX_MIN * HSCALE,
            BBOX_MAX * HSCALE,
        );
        let mut y1 = clampf(
            (fy1 * VSCALE as f32).floor(),
            BBOX_MIN * VSCALE,
            BBOX_MAX * VSCALE,
        );
        let c = self.clip;
        let mut v = 0;
        match clip_lerp_y(c.y0, false, x0, y0, x1, y1, &mut v) {
            Lerp::Outside => return,
            Lerp::Leave => {
                y1 = c.y0;
                x1 = v;
            }
            Lerp::Enter => {
                y0 = c.y0;
                x0 = v;
            }
            Lerp::Inside => {}
        }
        match clip_lerp_y(c.y1, true, x0, y0, x1, y1, &mut v) {
            Lerp::Outside => return,
            Lerp::Leave => {
                y1 = c.y1;
                x1 = v;
            }
            Lerp::Enter => {
                y0 = c.y1;
                x0 = v;
            }
            Lerp::Inside => {}
        }
        match clip_lerp_x(c.x0, false, x0, y0, x1, y1, &mut v) {
            Lerp::Outside => {
                x0 = c.x0;
                x1 = c.x0;
            }
            Lerp::Leave => {
                self.insert_raw(c.x0, v, c.x0, y1);
                x1 = c.x0;
                y1 = v;
            }
            Lerp::Enter => {
                self.insert_raw(c.x0, y0, c.x0, v);
                x0 = c.x0;
                y0 = v;
            }
            Lerp::Inside => {}
        }
        match clip_lerp_x(c.x1, true, x0, y0, x1, y1, &mut v) {
            Lerp::Outside => {
                x0 = c.x1;
                x1 = c.x1;
            }
            Lerp::Leave => {
                self.insert_raw(c.x1, v, c.x1, y1);
                x1 = c.x1;
                y1 = v;
            }
            Lerp::Enter => {
                self.insert_raw(c.x1, y0, c.x1, v);
                x0 = c.x1;
                y0 = v;
            }
            Lerp::Inside => {}
        }
        self.insert_raw(x0, y0, x1, y1);
    }

    /// `fz_insert_gel_rect`: an axis-aligned rectangle as two vertical
    /// edges, at least one subsample wide and tall.
    pub fn insert_rect(&mut self, fx0: f32, fy0: f32, fx1: f32, fy1: f32) {
        let mut fx0 = (fx0 * HSCALE as f32).floor();
        let mut fx1 = (fx1 * HSCALE as f32).floor();
        if fx1 == fx0 {
            fx1 += 1.0;
        }
        let mut fy0 = (fy0 * VSCALE as f32).floor();
        let mut fy1 = (fy1 * VSCALE as f32).floor();
        if fy1 == fy0 {
            fy1 += 1.0;
        }
        let c = self.clip;
        // `fz_clamp`: an empty clip (min above max) gives min, as C's
        // comparisons do; `f32::clamp` would panic.
        let fz_clamp = |v: f32, lo: f32, hi: f32| {
            if v < lo {
                lo
            } else if v > hi {
                hi
            } else {
                v
            }
        };
        fx0 = fz_clamp(fx0, c.x0 as f32, c.x1 as f32);
        fx1 = fz_clamp(fx1, c.x0 as f32, c.x1 as f32);
        fy0 = fz_clamp(fy0, c.y0 as f32, c.y1 as f32);
        fy1 = fz_clamp(fy1, c.y0 as f32, c.y1 as f32);
        let clampf = |v: f32, lo: i32, hi: i32| v.clamp(lo as f32, hi as f32) as i32;
        let x0 = clampf(fx0, BBOX_MIN * HSCALE, BBOX_MAX * HSCALE);
        let y0 = clampf(fy0, BBOX_MIN * VSCALE, BBOX_MAX * VSCALE);
        let x1 = clampf(fx1, BBOX_MIN * HSCALE, BBOX_MAX * HSCALE);
        let y1 = clampf(fy1, BBOX_MIN * VSCALE, BBOX_MAX * VSCALE);
        self.insert_raw(x1, y0, x1, y1);
        self.insert_raw(x0, y1, x0, y0);
    }

    /// `fz_is_rect_gel`: two vertical edges of the same height.
    pub fn is_rect(&self) -> bool {
        if let [a, b] = self.edges[..] {
            a.y == b.y
                && a.h == b.h
                && a.xmove == 0
                && a.adj_up == 0
                && b.xmove == 0
                && b.adj_up == 0
        } else {
            false
        }
    }

    /// `sort_gel`: by y, MuPDF's shell sort, so edges that start on the
    /// same row come in its order.
    fn sort(&mut self) {
        let n = self.edges.len();
        if n > 10000 {
            // MuPDF's qsort leaves ties in an order of its own; the sweep
            // sorts what starts on a row by x anyway.
            self.edges.sort_by_key(|e| e.y);
            return;
        }
        let mut h = 1;
        if n >= 14 {
            while h < n {
                h = 3 * h + 1;
            }
            h /= 3;
            h /= 3;
        }
        while h > 0 {
            for i in 0..n {
                let t = self.edges[i];
                let mut k = i as isize - h as isize;
                while k >= 0 && self.edges[k as usize].y > t.y {
                    self.edges[k as usize + h] = self.edges[k as usize];
                    k -= h as isize;
                }
                self.edges[(k + h as isize) as usize] = t;
            }
            h /= 3;
        }
    }

    fn sort_active(&mut self) {
        let n = self.active.len();
        let mut h = 1;
        if n >= 14 {
            while h < n {
                h = 3 * h + 1;
            }
            h /= 3;
            h /= 3;
        }
        while h > 0 {
            for i in 0..n {
                let t = self.active[i];
                let tx = self.edges[t].x;
                let mut k = i as isize - h as isize;
                while k >= 0 && self.edges[self.active[k as usize]].x > tx {
                    self.active[k as usize + h] = self.active[k as usize];
                    k -= h as isize;
                }
                self.active[(k + h as isize) as usize] = t;
            }
            h /= 3;
        }
    }

    fn insert_active(&mut self, y: i32, e: &mut usize) -> i32 {
        let mut h_min = i32::MAX;
        while *e < self.edges.len() && self.edges[*e].y == y {
            self.active.push(*e);
            *e += 1;
        }
        if *e < self.edges.len() {
            h_min = self.edges[*e].y - y;
        }
        for &a in &self.active {
            let ed = &self.edges[a];
            if ed.xmove != 0 || ed.adj_up != 0 {
                h_min = 1;
                break;
            }
            if ed.h < h_min {
                h_min = ed.h;
                if h_min == 1 {
                    break;
                }
            }
        }
        self.sort_active();
        h_min
    }

    fn advance_active(&mut self, inc: i32) {
        let mut i = 0;
        while i < self.active.len() {
            let idx = self.active[i];
            let edge = &mut self.edges[idx];
            edge.h -= inc;
            if edge.h == 0 {
                let last = self.active.len() - 1;
                self.active[i] = self.active[last];
                self.active.pop();
            } else {
                edge.x += edge.xmove;
                edge.e += edge.adj_up;
                if edge.e > 0 {
                    edge.x += edge.xdir;
                    edge.e -= edge.adj_down;
                }
                i += 1;
            }
        }
    }

    fn add_span(deltas: &mut [i32], x0: i32, x1: i32, xofs: i32, h: i32) {
        if x0 == x1 {
            return;
        }
        let x0 = (x0 - xofs) as u32;
        let x1 = (x1 - xofs) as u32;
        let hs = HSCALE as u32;
        let (x0pix, x0sub) = ((x0 / hs) as usize, (x0 % hs) as i32);
        let (x1pix, x1sub) = ((x1 / hs) as usize, (x1 % hs) as i32);
        let mut add = |i: usize, v: i32| {
            if let Some(d) = deltas.get_mut(i) {
                *d += v;
            }
        };
        if x0pix == x1pix {
            add(x0pix, h * (x1sub - x0sub));
            add(x0pix + 1, h * (x0sub - x1sub));
        } else {
            add(x0pix, h * (HSCALE - x0sub));
            add(x0pix + 1, h * x0sub);
            add(x1pix, h * (x1sub - HSCALE));
            add(x1pix + 1, h * -x1sub);
        }
    }

    fn spans(&mut self, eofill: bool, xofs: i32, h: i32) {
        let mut x = 0;
        if eofill {
            let mut even = false;
            for i in 0..self.active.len() {
                let ex = self.edges[self.active[i]].x;
                if !even {
                    x = ex;
                } else {
                    Self::add_span(&mut self.deltas, x, ex, xofs, h);
                }
                even = !even;
            }
        } else {
            let mut winding = 0;
            for i in 0..self.active.len() {
                let ed = self.edges[self.active[i]];
                if winding == 0 && winding + ed.ydir != 0 {
                    x = ed.x;
                }
                if winding != 0 && winding + ed.ydir == 0 {
                    Self::add_span(&mut self.deltas, x, ed.x, xofs, h);
                }
                winding += ed.ydir;
            }
        }
    }

    fn undelta(&mut self, n: usize) {
        let mut d = 0;
        for i in 0..n.min(self.deltas.len()).min(self.alphas.len()) {
            d += self.deltas[i];
            self.alphas[i] = d as u8;
        }
    }

    /// `fz_convert_rasterizer` with anti-aliasing: each scanline's
    /// coverage, within `clip` (pixels), handed to `blit(x, y, alphas)`.
    pub fn convert(&mut self, eofill: bool, clip: IRect, blit: &mut dyn FnMut(i32, i32, &[u8])) {
        let clip = clip.intersect(self.bound()).intersect(self.clip_box());
        if clip.is_empty() || self.edges.is_empty() {
            return;
        }
        self.sort();
        let xmin = idiv(self.bbox.x0, HSCALE);
        let xmax = idiv_up(self.bbox.x1, HSCALE);
        let xofs = xmin * HSCALE;
        let skipx = (clip.x0 - xmin) as usize;
        let clipn = (clip.x1 - clip.x0) as usize;
        let bcap = (xmax - xmin + 2) as usize;
        self.alphas.clear();
        self.alphas.resize(bcap, 0);
        self.deltas.clear();
        self.deltas.resize(bcap, 0);
        self.active.clear();
        let mut e = 0usize;
        let mut y = self.edges[0].y;
        let mut yd = idiv(y, VSCALE);
        let n = skipx + clipn;
        // Skip to the clip.
        while yd < clip.y0 && (!self.active.is_empty() || e < self.edges.len()) {
            let rh = (yd + 1) * VSCALE - y;
            let mut height = self.insert_active(y, &mut e);
            let mut h0 = height;
            if h0 >= rh {
                h0 -= rh;
                yd += 1;
            }
            while yd < clip.y0 && h0 >= VSCALE {
                h0 -= VSCALE;
                yd += 1;
            }
            if yd < clip.y0 {
                h0 = 0;
            }
            height -= h0;
            self.advance_active(height);
            y += height;
        }
        macro_rules! flush_line {
            ($y:expr) => {{
                self.undelta(n);
                blit(xmin + skipx as i32, $y, &self.alphas[skipx..skipx + clipn]);
            }};
        }
        while !self.active.is_empty() || e < self.edges.len() {
            let yc = idiv(y, VSCALE);
            let rh = (yc + 1) * VSCALE - y;
            if yc != yd {
                flush_line!(yd);
                self.deltas[..n].fill(0);
            }
            yd = yc;
            if yd >= clip.y1 {
                return;
            }
            let height = self.insert_active(y, &mut e);
            let mut h0 = height;
            let mut skip_spans = false;
            if h0 > rh {
                if rh < VSCALE {
                    self.spans(eofill, xofs, rh);
                    flush_line!(yd);
                    self.deltas[..n].fill(0);
                    yd += 1;
                    if yd >= clip.y1 {
                        return;
                    }
                    h0 -= rh;
                }
                if h0 > VSCALE {
                    h0 -= VSCALE;
                    self.spans(eofill, xofs, VSCALE);
                    self.undelta(n);
                    loop {
                        blit(xmin + skipx as i32, yd, &self.alphas[skipx..skipx + clipn]);
                        yd += 1;
                        if yd >= clip.y1 {
                            return;
                        }
                        h0 -= VSCALE;
                        if h0 <= 0 {
                            break;
                        }
                    }
                    if h0 == 0 {
                        skip_spans = true;
                    } else {
                        self.deltas[..n].fill(0);
                        h0 += VSCALE;
                    }
                }
            }
            if !skip_spans {
                self.spans(eofill, xofs, h0);
            }
            self.advance_active(height);
            y += height;
        }
        if yd < clip.y1 {
            flush_line!(yd);
        }
    }
}

#[cfg(test)]
mod tests {
    /// Found by the wild-tier crash sweep: a stroke under a clip that is
    /// empty (its x0 past its x1) made `f32::clamp` panic, where MuPDF's
    /// `fz_clamp` gives the lower bound.
    #[test]
    fn an_empty_clip_takes_a_rectangle_without_panicking() {
        let mut gel = Gel::new();
        gel.reset(IRect::new(3162, 0, 0, 10));
        gel.insert_rect(10.0, 2.0, 20.0, 8.0);
        gel.insert(10.0, 2.0, 20.0, 8.0);
    }

    use super::*;

    fn coverage(gel: &mut Gel, eofill: bool, w: i32, h: i32) -> Vec<u8> {
        let mut out = vec![0u8; (w * h) as usize];
        gel.convert(
            eofill,
            IRect {
                x0: 0,
                y0: 0,
                x1: w,
                y1: h,
            },
            &mut |x, y, a| {
                for (i, &v) in a.iter().enumerate() {
                    let at = (y * w + x) as usize + i;
                    if let Some(o) = out.get_mut(at) {
                        *o = v;
                    }
                }
            },
        );
        out
    }

    #[test]
    fn a_pixel_aligned_square_is_fully_covered() {
        let mut gel = Gel::new();
        gel.insert(1.0, 1.0, 3.0, 1.0);
        gel.insert(3.0, 1.0, 3.0, 3.0);
        gel.insert(3.0, 3.0, 1.0, 3.0);
        gel.insert(1.0, 3.0, 1.0, 1.0);
        let c = coverage(&mut gel, false, 4, 4);
        assert_eq!(&c[4..8], &[0, 255, 255, 0]);
        assert_eq!(&c[8..12], &[0, 255, 255, 0]);
        assert_eq!(&c[0..4], &[0; 4]);
    }

    #[test]
    fn half_a_pixel_is_half_covered() {
        let mut gel = Gel::new();
        gel.insert_rect(0.0, 0.0, 1.5, 1.0);
        let c = coverage(&mut gel, false, 2, 1);
        // 25 of 17 subsamples' columns: floor(1.5 * 17) = 25.
        assert_eq!(c[0], 255);
        assert_eq!(c[1], 8 * 15);
    }
}
