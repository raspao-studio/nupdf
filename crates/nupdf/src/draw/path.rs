// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/draw-path.c,
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! Paths turned into edges: fills flattened (curves split until flat
//! enough), strokes expanded into their outline with joins, caps and
//! dashes (`fz_flatten_fill_path`, `fz_flatten_stroke_path`).

use super::edge::Gel;
use crate::fma::Madd;
use crate::geometry::{IRect, Matrix, Rect};
use crate::path::{FullWalker, Path, StrokeState, Walker};

const MAX_DEPTH: i32 = 8;
const SQRT2: f32 = std::f32::consts::SQRT_2;
const PI: f32 = std::f32::consts::PI;
/// `FLT_TINY`: its square is about `FLT_EPSILON`.
const FLT_TINY: f32 = 3.4e-4;

pub const LINECAP_BUTT: i32 = 0;
pub const LINECAP_ROUND: i32 = 1;
pub const LINECAP_SQUARE: i32 = 2;
pub const LINECAP_TRIANGLE: i32 = 3;
pub const LINEJOIN_MITER: i32 = 0;
pub const LINEJOIN_ROUND: i32 = 1;
pub const LINEJOIN_BEVEL: i32 = 2;
pub const LINEJOIN_MITER_XPS: i32 = 3;

fn tx(m: &Matrix, x: f32, y: f32) -> (f32, f32) {
    let p = m.transform_xy(x, y);
    (p.x, p.y)
}

fn line(gel: &mut Gel, ctm: &Matrix, x0: f32, y0: f32, x1: f32, y1: f32) {
    let (a, b) = tx(ctm, x0, y0);
    let (c, d) = tx(ctm, x1, y1);
    gel.insert(a, b, c, d);
}

#[allow(clippy::too_many_arguments)]
fn bezier(
    gel: &mut Gel,
    ctm: &Matrix,
    flatness: f32,
    xa: f32,
    ya: f32,
    xb: f32,
    yb: f32,
    xc: f32,
    yc: f32,
    xd: f32,
    yd: f32,
    depth: i32,
) {
    let dmax = (xa - xb)
        .abs()
        .max((ya - yb).abs())
        .max((xd - xc).abs())
        .max((yd - yc).abs());
    if dmax < flatness || depth >= MAX_DEPTH {
        line(gel, ctm, xa, ya, xd, yd);
        return;
    }
    let (s, h) = split(xa, ya, xb, yb, xc, yc, xd, yd);
    bezier(
        gel,
        ctm,
        flatness,
        xa,
        ya,
        s.0,
        s.1,
        s.2,
        s.3,
        h.0,
        h.1,
        depth + 1,
    );
    bezier(
        gel,
        ctm,
        flatness,
        h.0,
        h.1,
        h.2,
        h.3,
        h.4,
        h.5,
        xd,
        yd,
        depth + 1,
    );
}

/// De Casteljau at a half: (xab, yab, xabc, yabc) and (xabcd, yabcd, xbcd,
/// ybcd, xcd, ycd), as MuPDF computes them.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
fn split(
    xa: f32,
    ya: f32,
    xb: f32,
    yb: f32,
    xc: f32,
    yc: f32,
    xd: f32,
    yd: f32,
) -> ((f32, f32, f32, f32), (f32, f32, f32, f32, f32, f32)) {
    let (xab, yab) = (xa + xb, ya + yb);
    let (xbc, ybc) = (xb + xc, yb + yc);
    let (xcd, ycd) = (xc + xd, yc + yd);
    let (xabc, yabc) = (xab + xbc, yab + ybc);
    let (xbcd, ybcd) = (xbc + xcd, ybc + ycd);
    let (xabcd, yabcd) = (xabc + xbcd, yabc + ybcd);
    (
        (xab * 0.5, yab * 0.5, xabc * 0.25, yabc * 0.25),
        (
            xabcd * 0.125,
            yabcd * 0.125,
            xbcd * 0.25,
            ybcd * 0.25,
            xcd * 0.5,
            ycd * 0.5,
        ),
    )
}

/// The fill flattener (`flatten_proc`).
struct Flatten<'g> {
    gel: &'g mut Gel,
    ctm: Matrix,
    flatness: f32,
    bx: f32,
    by: f32,
    cx: f32,
    cy: f32,
}

impl FullWalker for Flatten<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        // An implicit close before a move.
        if self.cx != self.bx || self.cy != self.by {
            line(self.gel, &self.ctm, self.cx, self.cy, self.bx, self.by);
        }
        self.cx = x;
        self.bx = x;
        self.cy = y;
        self.by = y;
    }
    fn line_to(&mut self, x: f32, y: f32) {
        line(self.gel, &self.ctm, self.cx, self.cy, x, y);
        self.cx = x;
        self.cy = y;
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) {
        bezier(
            self.gel,
            &self.ctm,
            self.flatness,
            self.cx,
            self.cy,
            x1,
            y1,
            x2,
            y2,
            x3,
            y3,
            0,
        );
        self.cx = x3;
        self.cy = y3;
    }
    fn close_path(&mut self) {
        line(self.gel, &self.ctm, self.cx, self.cy, self.bx, self.by);
        self.cx = self.bx;
        self.cy = self.by;
    }
    fn curve_to_v(&mut self, x2: f32, y2: f32, x3: f32, y3: f32) {
        let (x1, y1) = (self.cx, self.cy);
        self.curve_to(x1, y1, x2, y2, x3, y3);
    }
    fn curve_to_y(&mut self, x1: f32, y1: f32, x3: f32, y3: f32) {
        self.curve_to(x1, y1, x3, y3, x3, y3);
    }
    fn rect_to(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.move_to(x0, y0);
        let m = self.ctm;
        // The gel's anti-dropout rectangles, for axis-aligned ones.
        if m.b == 0.0 && m.c == 0.0 {
            let tx0 = m.a.madd(x0, m.e);
            let ty0 = m.d.madd(y0, m.f);
            let tx1 = m.a.madd(x1, m.e);
            let ty1 = m.d.madd(y1, m.f);
            self.gel.insert_rect(tx0, ty0, tx1, ty1);
            return;
        } else if m.a == 0.0 && m.d == 0.0 {
            let tx0 = m.c.madd(y0, m.e);
            let ty0 = m.b.madd(x0, m.f);
            let tx1 = m.c.madd(y1, m.e);
            let ty1 = m.b.madd(x1, m.f);
            self.gel.insert_rect(tx0, ty1, tx1, ty0);
            return;
        }
        self.line_to(x1, y0);
        self.line_to(x1, y1);
        self.line_to(x0, y1);
        self.close_path();
    }
}

/// `fz_flatten_fill_path`: the path's edges in `gel`, reset to `scissor`.
/// The pixel box they cover, or None when it is empty.
pub fn flatten_fill(
    gel: &mut Gel,
    path: &Path,
    ctm: Matrix,
    flatness: f32,
    scissor: IRect,
) -> Option<IRect> {
    let mut scissor = scissor;
    if scissor.is_empty() {
        scissor.x1 = scissor.x0;
        scissor.y1 = scissor.y0;
    }
    gel.reset(scissor);
    let mut f = Flatten {
        gel,
        ctm,
        flatness,
        bx: 0.0,
        by: 0.0,
        cx: 0.0,
        cy: 0.0,
    };
    path.walk_full(&mut f);
    if f.cx != f.bx || f.cy != f.by {
        line(f.gel, &ctm, f.cx, f.cy, f.bx, f.by);
    }
    let bound = gel.bound();
    if bound.is_empty() {
        return None;
    }
    let bbox = scissor.intersect(bound);
    (!bbox.is_empty()).then_some(bbox)
}

/// `sctx`: the stroker's state.
struct Stroker<'g> {
    gel: &'g mut Gel,
    ctm: Matrix,
    flatness: f32,
    stroke: &'g StrokeState,
    linejoin: i32,
    linewidth: f32,
    miterlimit: f32,
    beg: [(f32, f32); 2],
    seg: [(f32, f32); 2],
    sn: usize,
    not_just_moves: bool,
    from_bezier: bool,
    cur: (f32, f32),
    rect: Rect,
    dash: bool,
    dash_phase: f32,
    dash_total: f32,
    toggle: bool,
    cap: i32,
    offset: usize,
    phase: f32,
    dash_cur: (f32, f32),
    dash_beg: (f32, f32),
    dirn: (f32, f32),
}

/// `find_normal_vectors`: the normal of length `w`, or None when the
/// segment is too short to have one.
fn normal(dx: f32, dy: f32, w: f32) -> Option<(f32, f32)> {
    if dx == 0.0 {
        if dy < FLT_TINY && dy > -FLT_TINY {
            None
        } else if dy > 0.0 {
            Some((w, 0.0))
        } else {
            Some((-w, 0.0))
        }
    } else if dy == 0.0 {
        if dx < FLT_TINY && dx > -FLT_TINY {
            None
        } else if dx > 0.0 {
            Some((0.0, -w))
        } else {
            Some((0.0, w))
        }
    } else {
        let sq = dx.madd(dx, dy * dy);
        if sq < f32::EPSILON {
            return None;
        }
        let scale = w / sq.sqrt();
        Some((dy * scale, -dx * scale))
    }
}

impl Stroker<'_> {
    fn add_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        line(self.gel, &self.ctm, x0, y0, x1, y1);
    }

    fn add_horiz_rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        let m = self.ctm;
        if m.b == 0.0 && m.c == 0.0 {
            let tx0 = m.a.madd(x0, m.e);
            let ty0 = m.d.madd(y0, m.f);
            let tx1 = m.a.madd(x1, m.e);
            let ty1 = m.d.madd(y1, m.f);
            self.gel.insert_rect(tx1, ty1, tx0, ty0);
        } else if m.a == 0.0 && m.d == 0.0 {
            let tx0 = m.c.madd(y0, m.e);
            let ty0 = m.b.madd(x0, m.f);
            let tx1 = m.c.madd(y1, m.e);
            let ty1 = m.b.madd(x1, m.f);
            self.gel.insert_rect(tx1, ty0, tx0, ty1);
        } else {
            self.add_line(x0, y0, x1, y0);
            self.add_line(x1, y1, x0, y1);
        }
    }

    #[allow(clippy::too_many_arguments, clippy::needless_late_init)]
    fn add_arc(&mut self, xc: f32, yc: f32, x0: f32, y0: f32, x1: f32, y1: f32, rev: bool) {
        let r = self.linewidth.abs();
        let theta = 2.0 * SQRT2 * (self.flatness / r).sqrt();
        let mut th0 = y0.atan2(x0);
        let mut th1 = y1.atan2(x1);
        let n;
        if r > 0.0 {
            if th0 < th1 {
                th0 += PI * 2.0;
            }
            n = ((th0 - th1) / theta).ceil() as i32;
        } else {
            if th1 < th0 {
                th1 += PI * 2.0;
            }
            n = ((th1 - th0) / theta).ceil() as i32;
        }
        let n = n.clamp(0, 1 << 16);
        let at = |i: i32| {
            let t = th0 + (th1 - th0) * i as f32 / n as f32;
            (t.cos() * r, t.sin() * r)
        };
        if rev {
            let (mut ox, mut oy) = (x1, y1);
            for i in (1..n).rev() {
                let (nx, ny) = at(i);
                self.add_line(xc + nx, yc + ny, xc + ox, yc + oy);
                ox = nx;
                oy = ny;
            }
            self.add_line(xc + x0, yc + y0, xc + ox, yc + oy);
        } else {
            let (mut ox, mut oy) = (x0, y0);
            for i in 1..n {
                let (nx, ny) = at(i);
                self.add_line(xc + ox, yc + oy, xc + nx, yc + ny);
                ox = nx;
                oy = ny;
            }
            self.add_line(xc + ox, yc + oy, xc + x1, yc + y1);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_line_join(
        &mut self,
        ax: f32,
        ay: f32,
        bx: f32,
        by: f32,
        cx: f32,
        cy: f32,
        join_under: bool,
    ) {
        let miterlimit = self.miterlimit;
        let w = self.linewidth;
        let mut linejoin = self.linejoin;
        let mut dx0 = bx - ax;
        let mut dy0 = by - ay;
        let mut dx1 = cx - bx;
        let mut dy1 = cy - by;
        let mut cross = dx1.madd(dy0, -(dx0 * dy1));
        let mut rev = false;
        if cross < 0.0 {
            let t = dx1;
            dx1 = -dx0;
            dx0 = -t;
            let t = dy1;
            dy1 = -dy0;
            dy0 = -t;
            cross = -cross;
            rev = !rev;
        }
        let (dlx0, dly0) = normal(dx0, dy0, w).unwrap_or_else(|| {
            linejoin = LINEJOIN_BEVEL;
            (0.0, 0.0)
        });
        let (dlx1, dly1) = normal(dx1, dy1, w).unwrap_or_else(|| {
            linejoin = LINEJOIN_BEVEL;
            (0.0, 0.0)
        });
        let mut dmx = (dlx0 + dlx1) * 0.5;
        let mut dmy = (dly0 + dly1) * 0.5;
        let dmr2 = dmx.madd(dmx, dmy * dmy);
        if cross * cross < f32::EPSILON && dx0.madd(dx1, dy0 * dy1) >= 0.0 {
            linejoin = LINEJOIN_BEVEL;
        }
        if linejoin == LINEJOIN_MITER_XPS {
            if cross == 0.0 {
                linejoin = LINEJOIN_BEVEL;
            } else if dmr2 * miterlimit * miterlimit >= w * w {
                linejoin = LINEJOIN_MITER;
            }
        } else if linejoin == LINEJOIN_MITER && dmr2 * miterlimit * miterlimit < w * w {
            linejoin = LINEJOIN_BEVEL;
        }
        if join_under {
            self.add_line(bx + dlx1, by + dly1, bx + dlx0, by + dly0);
        } else if rev {
            self.add_line(bx + dlx1, by + dly1, bx, by);
            self.add_line(bx, by, bx + dlx0, by + dly0);
        } else {
            self.add_line(bx, by, bx + dlx0, by + dly0);
            self.add_line(bx + dlx1, by + dly1, bx, by);
        }
        match linejoin {
            LINEJOIN_MITER_XPS => {
                let scale = w * w / dmr2;
                dmx *= scale;
                dmy *= scale;
                let k = (scale - w * miterlimit / dmr2.sqrt()) / (scale - 1.0);
                let t0x = bx - dmx + k * (dmx - dlx0);
                let t0y = by - dmy + k * (dmy - dly0);
                let t1x = bx - dmx + k * (dmx - dlx1);
                let t1y = by - dmy + k * (dmy - dly1);
                if rev {
                    self.add_line(t1x, t1y, bx - dlx1, by - dly1);
                    self.add_line(t0x, t0y, t1x, t1y);
                    self.add_line(bx - dlx0, by - dly0, t0x, t0y);
                } else {
                    self.add_line(bx - dlx0, by - dly0, t0x, t0y);
                    self.add_line(t0x, t0y, t1x, t1y);
                    self.add_line(t1x, t1y, bx - dlx1, by - dly1);
                }
            }
            LINEJOIN_MITER => {
                let scale = w * w / dmr2;
                dmx *= scale;
                dmy *= scale;
                if rev {
                    self.add_line(bx - dmx, by - dmy, bx - dlx1, by - dly1);
                    self.add_line(bx - dlx0, by - dly0, bx - dmx, by - dmy);
                } else {
                    self.add_line(bx - dlx0, by - dly0, bx - dmx, by - dmy);
                    self.add_line(bx - dmx, by - dmy, bx - dlx1, by - dly1);
                }
            }
            LINEJOIN_ROUND => self.add_arc(bx, by, -dlx0, -dly0, -dlx1, -dly1, rev),
            _ => self.add_line(bx - dlx0, by - dly0, bx - dlx1, by - dly1),
        }
    }

    fn do_linecap(&mut self, bx: f32, by: f32, linecap: i32, dlx: f32, dly: f32) {
        let w = self.linewidth;
        match linecap {
            LINECAP_ROUND => {
                let n = (PI / (2.0 * SQRT2 * (self.flatness / w).sqrt())).ceil() as i32;
                let n = n.clamp(0, 1 << 16);
                let (mut ox, mut oy) = (bx - dlx, by - dly);
                for i in 1..n {
                    let theta = PI * i as f32 / n as f32;
                    let (cth, sth) = (theta.cos(), theta.sin());
                    let nx = (-dly).madd(sth, (-dlx).madd(cth, bx));
                    let ny = dlx.madd(sth, (-dly).madd(cth, by));
                    self.add_line(ox, oy, nx, ny);
                    ox = nx;
                    oy = ny;
                }
                self.add_line(ox, oy, bx + dlx, by + dly);
            }
            LINECAP_SQUARE => {
                self.add_line(bx - dlx, by - dly, bx - dlx - dly, by - dly + dlx);
                self.add_line(
                    bx - dlx - dly,
                    by - dly + dlx,
                    bx + dlx - dly,
                    by + dly + dlx,
                );
                self.add_line(bx + dlx - dly, by + dly + dlx, bx + dlx, by + dly);
            }
            LINECAP_TRIANGLE => {
                let (mx, my) = (-dly, dlx);
                self.add_line(bx - dlx, by - dly, bx + mx, by + my);
                self.add_line(bx + mx, by + my, bx + dlx, by + dly);
            }
            _ => self.add_line(bx - dlx, by - dly, bx + dlx, by + dly),
        }
    }

    fn add_line_cap(&mut self, ax: f32, ay: f32, bx: f32, by: f32, linecap: i32) {
        let dx = bx - ax;
        let dy = by - ay;
        let scale = self.linewidth / dx.madd(dx, dy * dy).sqrt();
        self.do_linecap(bx, by, linecap, dy * scale, -dx * scale);
    }

    fn add_zero_len_cap(&mut self, ax: f32, ay: f32, linecap: i32, rev: bool) {
        let (dx, dy) = if rev {
            (-self.dirn.0, -self.dirn.1)
        } else {
            self.dirn
        };
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let scale = self.linewidth / dx.madd(dx, dy * dy).sqrt();
        self.do_linecap(ax, ay, linecap, dy * scale, -dx * scale);
    }

    fn add_line_dot(&mut self, ax: f32, ay: f32) {
        let w = self.linewidth;
        let n = ((PI / (SQRT2 * (self.flatness / w).sqrt())).ceil() as i32).clamp(3, 1 << 16);
        let (mut ox, mut oy) = (ax - w, ay);
        for i in 1..n {
            let theta = PI * 2.0 * i as f32 / n as f32;
            let (cth, sth) = (theta.cos(), theta.sin());
            let nx = (-cth).madd(w, ax);
            let ny = sth.madd(w, ay);
            self.add_line(ox, oy, nx, ny);
            ox = nx;
            oy = ny;
        }
        self.add_line(ox, oy, ax - w, ay);
    }

    fn flush(&mut self, start_cap: i32, end_cap: i32) {
        if self.sn == 1 {
            let (b0, b1, s0, s1) = (self.beg[0], self.beg[1], self.seg[0], self.seg[1]);
            self.add_line_cap(b1.0, b1.1, b0.0, b0.1, start_cap);
            self.add_line_cap(s0.0, s0.1, s1.0, s1.1, end_cap);
        } else if self.not_just_moves {
            let b0 = self.beg[0];
            if self.cap == LINECAP_ROUND {
                self.add_line_dot(b0.0, b0.1);
            } else {
                let cap = self.cap;
                self.add_zero_len_cap(b0.0, b0.1, cap, true);
                self.add_zero_len_cap(b0.0, b0.1, cap, false);
            }
        }
    }

    fn stroke_moveto(&mut self, x: f32, y: f32) {
        self.seg[0] = (x, y);
        self.beg[0] = (x, y);
        self.sn = 0;
        self.not_just_moves = false;
        self.from_bezier = false;
        self.dirn = (0.0, 0.0);
    }

    fn lineto_aux(&mut self, x: f32, y: f32, from_bezier: bool, dirn: (f32, f32)) {
        let (ox, oy) = self.seg[self.sn];
        let dx = x - ox;
        let dy = y - oy;
        self.not_just_moves = true;
        self.dirn = dirn;
        let Some((dlx, dly)) = normal(dx, dy, self.linewidth) else {
            return;
        };
        if self.sn == 1 {
            let s0 = self.seg[0];
            let under = self.from_bezier && from_bezier;
            self.add_line_join(s0.0, s0.1, ox, oy, x, y, under);
        }
        if dy == 0.0 {
            self.add_horiz_rect(ox, oy - dly, x, y + dly);
        } else {
            self.add_line(ox - dlx, oy - dly, x - dlx, y - dly);
            self.add_line(x + dlx, y + dly, ox + dlx, oy + dly);
        }
        if self.sn == 1 {
            self.seg[0] = self.seg[1];
            self.seg[1] = (x, y);
        } else {
            self.seg[1] = (x, y);
            self.beg[1] = (x, y);
            self.sn = 1;
        }
        self.from_bezier = from_bezier;
    }

    fn stroke_lineto(&mut self, x: f32, y: f32, from_bezier: bool) {
        let (ox, oy) = self.seg[self.sn];
        self.lineto_aux(x, y, from_bezier, (x - ox, y - oy));
    }

    fn stroke_closepath(&mut self) {
        if self.sn == 1 {
            let b0 = self.beg[0];
            self.stroke_lineto(b0.0, b0.1, false);
            let (s0, b1) = (self.seg[0], self.beg[1]);
            self.add_line_join(s0.0, s0.1, b0.0, b0.1, b1.0, b1.1, false);
        } else if self.cap == LINECAP_ROUND {
            let b0 = self.beg[0];
            self.add_line_dot(b0.0, b0.1);
        }
        self.seg[0] = self.beg[0];
        self.sn = 0;
        self.not_just_moves = false;
        self.from_bezier = false;
        self.dirn = (0.0, 0.0);
    }

    /// Curves, split until flat, as lines to `to` (`fz_stroke_bezier`,
    /// `fz_dash_bezier`).
    #[allow(clippy::too_many_arguments)]
    fn bezier(
        &mut self,
        xa: f32,
        ya: f32,
        xb: f32,
        yb: f32,
        xc: f32,
        yc: f32,
        xd: f32,
        yd: f32,
        depth: i32,
    ) {
        let dmax = (xa - xb)
            .abs()
            .max((ya - yb).abs())
            .max((xd - xc).abs())
            .max((yd - yc).abs());
        if dmax < self.flatness || depth >= MAX_DEPTH {
            if self.dash {
                self.dash_lineto(xd, yd, true);
            } else {
                self.stroke_lineto(xd, yd, true);
            }
            return;
        }
        let (s, h) = split(xa, ya, xb, yb, xc, yc, xd, yd);
        self.bezier(xa, ya, s.0, s.1, s.2, s.3, h.0, h.1, depth + 1);
        self.bezier(h.0, h.1, h.2, h.3, h.4, h.5, xd, yd, depth + 1);
    }

    fn dash_list(&self, i: usize) -> f32 {
        self.stroke.dash.get(i).copied().unwrap_or(0.0)
    }

    fn dash_moveto(&mut self, x: f32, y: f32) {
        self.toggle = true;
        self.offset = 0;
        self.phase = self.dash_phase;
        let len = self.stroke.dash.len();
        // Bounded: each step takes off a dash length, and a length of 0
        // is never larger than the phase left.
        let mut guard = 0;
        while self.phase > 0.0 && self.phase >= self.dash_list(self.offset) && guard < 1 << 20 {
            self.toggle = !self.toggle;
            self.phase -= self.dash_list(self.offset);
            self.offset += 1;
            if self.offset == len {
                self.offset = 0;
            }
            guard += 1;
        }
        self.dash_cur = (x, y);
        if self.toggle {
            let (cap, end) = (self.cap, self.stroke.end_cap);
            self.flush(cap, end);
            self.cap = self.stroke.start_cap;
            self.stroke_moveto(x, y);
        }
    }

    fn dash_step_to(&mut self, x: f32, y: f32, from_bezier: bool, dirn: Option<(f32, f32)>) {
        if self.toggle {
            match dirn {
                Some(d) => self.lineto_aux(x, y, from_bezier, d),
                None => self.stroke_lineto(x, y, from_bezier),
            }
        } else {
            let (cap, dash_cap) = (self.cap, self.stroke.dash_cap);
            self.flush(cap, dash_cap);
            self.cap = dash_cap;
            self.stroke_moveto(x, y);
        }
    }

    /// Moves on through the dash array by `used` (and the current
    /// phase), the whole cycles first.
    fn dash_skip(&mut self, mut used: f32, strict: bool) -> f32 {
        let len = self.stroke.dash.len();
        used += self.phase;
        let n = (used / self.dash_total).trunc();
        used -= n * self.dash_total;
        if is_odd(n) && len & 1 == 1 {
            self.toggle = !self.toggle;
        }
        let mut guard = 0;
        loop {
            let d = self.dash_list(self.offset);
            let go = if strict { used > d } else { used >= d };
            if !go || guard >= 1 << 20 {
                break;
            }
            used -= d;
            self.offset += 1;
            if self.offset == len {
                self.offset = 0;
            }
            self.toggle = !self.toggle;
            guard += 1;
        }
        used
    }

    fn dash_lineto(&mut self, mut bx: f32, mut by: f32, from_bezier: bool) {
        let dash_cap = self.stroke.dash_cap;
        let (mut ax, mut ay) = self.dash_cur;
        let mut dx = bx - ax;
        let mut dy = by - ay;
        let mut used = 0.0f32;
        let mut tail = 0.0f32;
        let (mut old_bx, mut old_by) = (0.0f32, 0.0f32);
        let mut total = dx.madd(dx, dy * dy).sqrt();
        let r = self.rect;
        let mut off_screen = false;
        // a brought onto the screen, horizontally then vertically.
        let d = r.x0 - ax;
        if d > 0.0 {
            if bx < r.x0 {
                off_screen = true;
            } else {
                ax = r.x0;
                ay = advance(ay, by, d, dx);
                used = total * d / dx;
                total -= used;
                dx = bx - ax;
                dy = by - ay;
            }
        } else if d < 0.0 {
            let d = r.x1 - ax;
            if d < 0.0 {
                if bx > r.x1 {
                    off_screen = true;
                } else {
                    ax = r.x1;
                    ay = advance(ay, by, d, dx);
                    used = total * d / dx;
                    total -= used;
                    dx = bx - ax;
                    dy = by - ay;
                }
            }
        }
        if !off_screen {
            let d = r.y0 - ay;
            let mut moved = None;
            if d > 0.0 {
                if by < r.y0 {
                    off_screen = true;
                } else {
                    ay = r.y0;
                    moved = Some(d);
                }
            } else if d < 0.0 {
                let d = r.y1 - ay;
                if d < 0.0 {
                    if by > r.y1 {
                        off_screen = true;
                    } else {
                        ay = r.y1;
                        moved = Some(d);
                    }
                }
            }
            if let Some(d) = moved {
                ax = advance(ax, bx, d, dy);
                let d = total * d / dy;
                total -= d;
                used += d;
                dx = bx - ax;
                dy = by - ay;
            }
        }
        if off_screen {
            tail = total;
            old_bx = bx;
            old_by = by;
        } else {
            if used != 0.0 {
                self.dash_step_to(ax, ay, from_bezier, None);
                let left = self.dash_skip(used, false);
                self.dash_step_to(ax, ay, from_bezier, None);
                self.phase = left;
                used = 0.0;
            }
            // b brought back onto the screen.
            if dx != 0.0 {
                let d = bx - r.x0;
                let mut moved = None;
                if d < 0.0 {
                    old_bx = bx;
                    old_by = by;
                    bx = r.x0;
                    moved = Some(d);
                } else if d > 0.0 {
                    let d = bx - r.x1;
                    if d > 0.0 {
                        old_bx = bx;
                        old_by = by;
                        bx = r.x1;
                        moved = Some(d);
                    }
                }
                if let Some(d) = moved {
                    by = advance(by, ay, d, dx);
                    tail = total * d / dx;
                    total -= tail;
                    dx = bx - ax;
                    dy = by - ay;
                }
            }
            if dy != 0.0 {
                let d = by - r.y0;
                let mut moved = None;
                if d < 0.0 {
                    old_bx = bx;
                    old_by = by;
                    by = r.y0;
                    moved = Some(d);
                } else if d > 0.0 {
                    let d = by - r.y1;
                    if d > 0.0 {
                        old_bx = bx;
                        old_by = by;
                        by = r.y1;
                        moved = Some(d);
                    }
                }
                if let Some(d) = moved {
                    bx = advance(bx, ax, d, dy);
                    let t = total * d / dy;
                    tail += t;
                    total -= t;
                    dx = bx - ax;
                    dy = by - ay;
                }
            }
            let len = self.stroke.dash.len();
            let mut guard = 0;
            while total - used > self.dash_list(self.offset) - self.phase && guard < 1 << 20 {
                used += self.dash_list(self.offset) - self.phase;
                let ratio = used / total;
                let mx = ratio.madd(dx, ax);
                let my = ratio.madd(dy, ay);
                if self.toggle {
                    self.lineto_aux(mx, my, from_bezier, (dx, dy));
                } else {
                    let cap = self.cap;
                    self.flush(cap, dash_cap);
                    self.cap = dash_cap;
                    self.stroke_moveto(mx, my);
                }
                self.toggle = !self.toggle;
                self.phase = 0.0;
                self.offset += 1;
                if self.offset == len {
                    self.offset = 0;
                }
                guard += 1;
            }
            self.phase += total - used;
            if tail == 0.0 {
                self.dash_cur = (bx, by);
                if self.toggle {
                    self.lineto_aux(bx, by, from_bezier, (dx, dy));
                }
                return;
            }
        }
        // adjust_for_tail
        self.dash_cur = (old_bx, old_by);
        self.dash_step_to(old_bx, old_by, from_bezier, Some((dx, dy)));
        let left = self.dash_skip(tail, true);
        self.dash_step_to(old_bx, old_by, from_bezier, Some((dx, dy)));
        self.phase = left;
    }
}

/// `advance`: `a + (b - a) * i / n`, not past `b`.
fn advance(a: f32, b: f32, i: f32, n: f32) -> f32 {
    let d = b - a;
    let target = a + d * i / n;
    if (d < 0.0 && target < b) || (d > 0.0 && target > b) {
        b
    } else {
        target
    }
}

fn is_odd(f: f32) -> bool {
    let intpart = f.trunc();
    f.is_finite() && intpart - 2.0 * (intpart / 2.0).trunc() != 0.0
}

/// The walker for strokes and dashes: rectangles and implied control
/// points expanded, as `fz_walk_path` does for walkers without them.
impl Walker for Stroker<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        if self.dash {
            self.dash_moveto(x, y);
            self.dash_beg = (x, y);
        } else {
            let (s, e) = (self.stroke.start_cap, self.stroke.end_cap);
            self.flush(s, e);
            self.stroke_moveto(x, y);
        }
        self.cur = (x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        if self.dash {
            self.dash_lineto(x, y, false);
        } else {
            self.stroke_lineto(x, y, false);
        }
        self.cur = (x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) {
        let (cx, cy) = self.cur;
        self.bezier(cx, cy, x1, y1, x2, y2, x3, y3, 0);
        self.cur = (x3, y3);
    }
    fn close_path(&mut self) {
        if self.dash {
            let b = self.dash_beg;
            self.dash_lineto(b.0, b.1, false);
            self.cur = b;
        } else {
            self.stroke_closepath();
        }
    }
}

/// `fz_flatten_stroke_path`: the stroke's outline in `gel`, reset to
/// `scissor`. `linewidth` is the width to stroke at (hairlines differ from
/// the state's). The pixel box covered, or None when empty.
pub fn flatten_stroke(
    gel: &mut Gel,
    path: &Path,
    stroke: &StrokeState,
    ctm: Matrix,
    flatness: f32,
    linewidth: f32,
    scissor: IRect,
) -> Option<IRect> {
    gel.reset(scissor);
    let rect = gel.scissor();
    let mut s = Stroker {
        gel,
        ctm,
        flatness,
        stroke,
        linejoin: stroke.linejoin,
        linewidth: linewidth * 0.5,
        miterlimit: stroke.miterlimit,
        beg: [(0.0, 0.0); 2],
        seg: [(0.0, 0.0); 2],
        sn: 0,
        not_just_moves: false,
        from_bezier: false,
        cur: (0.0, 0.0),
        rect,
        dash: false,
        dash_phase: 0.0,
        dash_total: 0.0,
        toggle: false,
        cap: stroke.start_cap,
        offset: 0,
        phase: 0.0,
        dash_cur: (0.0, 0.0),
        dash_beg: (0.0, 0.0),
        dirn: (0.0, 0.0),
    };
    if !stroke.dash.is_empty() {
        s.dash_total = stroke.dash.iter().sum();
        if s.dash_total == 0.0 {
            return None;
        }
        let det = ctm.a * ctm.d - ctm.b * ctm.c;
        if det == 0.0 {
            return None;
        }
        let inv = ctm.invert();
        let mut r = rect.transform(inv);
        r.x0 -= linewidth;
        r.x1 += linewidth;
        r.y0 -= linewidth;
        r.y1 += linewidth;
        s.rect = r;
        let max_expand = crate::path::max_expansion(ctm);
        if s.dash_total >= 0.01 && s.dash_total * max_expand >= 0.5 {
            s.dash = true;
            s.dash_phase = stroke.dash_phase % s.dash_total;
        }
    }
    path.walk(&mut s);
    let (cap, end) = (s.cap, stroke.end_cap);
    s.flush(cap, end);
    let bound = gel.bound();
    if bound.is_empty() {
        return None;
    }
    let bbox = scissor.intersect(bound);
    (!bbox.is_empty()).then_some(bbox)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raster(gel: &mut Gel, bbox: IRect, w: i32, h: i32) -> Vec<u8> {
        let mut out = vec![0u8; (w * h) as usize];
        gel.convert(false, bbox, &mut |x, y, a| {
            for (i, &v) in a.iter().enumerate() {
                if let Some(o) = out.get_mut((y * w + x) as usize + i) {
                    *o = v;
                }
            }
        });
        out
    }

    #[test]
    fn a_filled_rectangle_covers_its_pixels() {
        let mut p = Path::new();
        p.rect_to(1.0, 1.0, 3.0, 2.0);
        let mut gel = Gel::new();
        let bbox = flatten_fill(&mut gel, &p, Matrix::IDENTITY, 0.3, IRect::new(0, 0, 4, 3))
            .unwrap_or(IRect::EMPTY);
        assert_eq!(bbox, IRect::new(1, 1, 3, 2));
        let c = raster(&mut gel, bbox, 4, 3);
        assert_eq!(&c[4..8], &[0, 255, 255, 0]);
    }

    #[test]
    fn a_stroked_line_is_as_wide_as_its_line_width() {
        let mut p = Path::new();
        p.move_to(0.0, 2.0);
        p.line_to(8.0, 2.0);
        let stroke = StrokeState {
            linewidth: 2.0,
            ..StrokeState::default()
        };
        let mut gel = Gel::new();
        let bbox = flatten_stroke(
            &mut gel,
            &p,
            &stroke,
            Matrix::IDENTITY,
            0.3,
            2.0,
            IRect::new(0, 0, 8, 4),
        )
        .unwrap_or(IRect::EMPTY);
        assert_eq!(bbox, IRect::new(0, 1, 8, 3));
        let c = raster(&mut gel, bbox, 8, 4);
        assert_eq!(&c[8..16], &[255; 8]);
        assert_eq!(&c[16..24], &[255; 8]);
        assert_eq!(&c[0..8], &[0; 8]);
    }

    /// The ink of a 10-wide stroke of a sharp corner with `join` and
    /// `cap` at miter limit 1.2, which its right angle passes.
    fn ink(join: i32, cap: i32) -> u32 {
        let mut p = Path::new();
        p.move_to(10.0, 50.0);
        p.line_to(50.0, 10.0);
        p.line_to(90.0, 50.0);
        let stroke = StrokeState {
            linewidth: 10.0,
            linejoin: join,
            start_cap: cap,
            end_cap: cap,
            miterlimit: 1.2,
            ..StrokeState::default()
        };
        let mut gel = Gel::new();
        let r = IRect::new(0, 0, 100, 64);
        let bbox = flatten_stroke(&mut gel, &p, &stroke, Matrix::IDENTITY, 0.3, 10.0, r)
            .unwrap_or(IRect::EMPTY);
        raster(&mut gel, bbox, 100, 64)
            .iter()
            .map(|&v| u32::from(v))
            .sum()
    }

    #[test]
    fn xps_miters_and_triangle_caps_fall_between_their_neighbours() {
        // PDF clamps joins and caps to 0..2; XPS's miter (3) cuts a
        // too-long miter off at the limit rather than bevelling it, and
        // its triangle cap reaches out half as far as a square one.
        let (bevel, xps) = (ink(LINEJOIN_BEVEL, 0), ink(LINEJOIN_MITER_XPS, 0));
        let miter = {
            // The full miter, with no limit to fall back at.
            let mut p = Path::new();
            p.move_to(10.0, 50.0);
            p.line_to(50.0, 10.0);
            p.line_to(90.0, 50.0);
            let stroke = StrokeState {
                linewidth: 10.0,
                miterlimit: 10.0,
                ..StrokeState::default()
            };
            let mut gel = Gel::new();
            let r = IRect::new(0, 0, 100, 64);
            let bbox = flatten_stroke(&mut gel, &p, &stroke, Matrix::IDENTITY, 0.3, 10.0, r)
                .unwrap_or(IRect::EMPTY);
            raster(&mut gel, bbox, 100, 64)
                .iter()
                .map(|&v| u32::from(v))
                .sum::<u32>()
        };
        assert!(bevel < xps && xps < miter, "{bevel} {xps} {miter}");
        let (butt, tri, square) = (
            ink(LINEJOIN_BEVEL, LINECAP_BUTT),
            ink(LINEJOIN_BEVEL, LINECAP_TRIANGLE),
            ink(LINEJOIN_BEVEL, LINECAP_SQUARE),
        );
        assert!(butt < tri && tri < square, "{butt} {tri} {square}");
    }

    #[test]
    fn dashes_leave_gaps() {
        let mut p = Path::new();
        p.move_to(0.0, 2.0);
        p.line_to(8.0, 2.0);
        let stroke = StrokeState {
            linewidth: 2.0,
            dash: vec![2.0, 2.0],
            ..StrokeState::default()
        };
        let mut gel = Gel::new();
        let bbox = flatten_stroke(
            &mut gel,
            &p,
            &stroke,
            Matrix::IDENTITY,
            0.3,
            2.0,
            IRect::new(0, 0, 8, 4),
        )
        .unwrap_or(IRect::EMPTY);
        let c = raster(&mut gel, bbox, 8, 4);
        assert_eq!(&c[8..16], &[255, 255, 0, 0, 255, 255, 0, 0]);
    }

    use crate::draw::testkit::{check, page, px};

    fn draw(name: &str, content: &str, want: &str) -> crate::draw::Pix {
        check(name, &page(128, 128, "", content, &[]), true, want)
    }

    #[test]
    fn joins_and_caps_as_mupdf_strokes_them() {
        // Miter, round and bevel joins with butt, round and square caps;
        // a miter past its limit, which bevels; a sharp one within it.
        let pix = draw(
            "path-joins-caps",
            "8 w 0 J 0 j 10 10 m 40 40 l 70 10 l S
             1 J 1 j 10 50 m 40 80 l 70 50 l S
             2 J 2 j 10 90 m 40 120 l 70 90 l S
             0 J 0 j 2 M 80 10 m 100 40 l 120 10 l S
             10 M 80 50 m 90 80 l 100 50 l S
             1 j 120 70 m 100 70 l 100 70 l 100 90 l S",
            "075d1181b2a242714981221baef38f43",
        );
        // The round cap reaches past the line's end; the butt one does not.
        assert_eq!(px(&pix, 7, 128 - 51), [0, 0, 0]);
        assert_eq!(px(&pix, 5, 128 - 8), [255, 255, 255]);
    }

    #[test]
    fn zero_length_subpaths_draw_their_caps() {
        // A round cap is a dot, a square one a square, a butt one nothing;
        // closed and moved-to subpaths with no length do the same.
        draw(
            "path-zero-length",
            "10 w 1 J 20 20 m 20 20 l S 2 J 50 20 m 50 20 l S 0 J 80 20 m 80 20 l S
             1 J 20 60 m h S 2 J 50 60 m 50 60 l h S 1 J 80 60 m S
             1 J 20 100 m 20 100 20 100 20 100 c S 2 J 50 100 m 50 100 l 50 100 l S",
            "a4fafeb77424b450bb892d3f29b755f0",
        );
    }

    #[test]
    fn curves_fill_and_stroke_flattened() {
        draw(
            "path-curves",
            "0 0 1 rg 10 10 m 50 120 80 -20 120 60 c 100 100 v 20 120 y h f
             1 0 0 RG 3 w 1 j 10 60 m 40 0 90 128 120 60 c 60 110 v S
             0 1 0 rg 0 0 0 RG 1 w 30 30 m 98 30 l 64 98 l h 30 80 m 98 80 l 64 12 l h B*",
            "c62c4486101a117f8c12c23ffc510639",
        );
    }

    #[test]
    fn dashes_follow_lines_curves_and_closes() {
        draw(
            "path-dashes",
            "4 w [6 3] 0 d 10 10 m 118 10 l S
             [6 3 1 3] 2 d 10 25 m 118 40 l S
             1 J [0 6] 0 d 10 55 m 118 55 l S
             0 J [10 4] 25 d 10 70 m 60 128 90 40 118 100 c S
             2 J 1 j [8 5] 3 d 20 80 80 30 re S
             0 j [12 0 4] 1 d 30 90 m 100 120 l 30 120 l h S",
            "7f600c68edb9defa16bb732280d5b49b",
        );
    }

    #[test]
    fn strokes_and_rects_under_rotation_and_skew() {
        draw(
            "path-transforms",
            "q 0.7 0.7 -0.7 0.7 64 0 cm 6 w 20 20 40 30 re S 0 0 1 rg 30 60 20 20 re f Q
             q 0 1 -1 0 128 0 cm 4 w 10 10 30 20 re S 1 0 0 rg 50 10 20 30 re f Q
             q 1 0 0.5 1 0 0 cm 0 w 10 100 m 60 120 l S 0.1 w 10 110 m 60 126 l S Q
             q 1 0 0 -1 0 128 cm 3 w 2 J 70 70 m 120 70 l S 80 80 30 2 re S Q",
            "55da1b66578c32e88b5df75b8a7b6c65",
        );
    }

    #[test]
    fn clamped_joins_and_caps_and_reversals() {
        // `3 j` and `3 J` clamp to bevel and square, as MuPDF reads them;
        // lines that turn back on themselves join flat.
        draw(
            "path-clamped-joins",
            "6 w 3 j 4 M 10 10 m 40 40 l 70 10 l S 70 30 m 40 60 l 10 30 l S
             1.2 M 10 70 m 40 100 l 70 70 l S 70 90 m 40 120 l 10 90 l S
             3 J 0 j 80 20 m 120 20 l S 100 45 m 100 45 l S
             1 J 1 j 80 70 m 120 70 l 90 70 l S 0 j 80 90 m 120 90 l 90 90 l S
             2 J 3 j 80 110 m 120 110 l 120 110 l 90 125 l S",
            "f2ff5993e47b7eb0a472af437965eca6",
        );
    }

    #[test]
    fn dashes_clip_to_the_page_and_skip_whole_cycles() {
        // Dashed lines from far off the page each way, entirely off it,
        // and a long one through many cycles of an odd-length array.
        draw(
            "path-dashes-offpage",
            "3 w [5 3] 1 d -2000 10 m 2000 20 l S 2000 30 m -2000 40 l S
             10 -2000 m 20 2000 l S 30 2000 m 40 -2000 l S
             -500 -500 m 600 700 l S -10 200 m -10 300 l S
             [3] 0 d 50 50 m 120 120 l S [7 2 1] 40 d 60 20 m 120 20 l 120 60 l S",
            "231c30468760662bad5bf105e5b9ddc0",
        );
    }

    #[test]
    fn degenerate_dashes_are_solid_or_nothing() {
        // A dash pattern too short to see strokes solid; one of nothing
        // strokes nothing; neither does any dash through a flat matrix.
        draw(
            "path-dashes-degenerate",
            "4 w [0.001 0.001] 0 d 10 10 m 118 10 l S [0 0] 0 d 10 30 m 118 30 l S
             q 1 0 0 0 0 64 cm [4 4] 0 d 10 0 m 118 0 l S Q
             [4 4] 0 d 10 80 m 10 80.00001 l 118 90 l S",
            "ae9842ef2f4737728e90adb66e70b6a6",
        );
    }

    #[test]
    fn open_subpaths_fill_as_if_closed() {
        // Subpaths left open close before the next moveto and at the end;
        // a fill inside an empty clip paints nothing.
        draw(
            "path-open-fills",
            "10 10 m 60 10 l 35 60 l 70 10 m 120 10 l 95 60 l f
             q 0 0 0 0 re W n 0 0 128 128 re f Q
             10 70 m 120 70 l 120 120 l f*",
            "f14947d2d212418a7dda9cca775e772f",
        );
    }
}
