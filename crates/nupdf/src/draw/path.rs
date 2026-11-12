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
}
