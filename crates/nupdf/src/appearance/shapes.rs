// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-appearance.c (the ink, polygon, line,
// square and circle writers, their line endings and cloudy borders) and
// source/pdf/pdf-annot.c (pdf_annot_border_effect,
// pdf_annot_border_effect_intensity),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Ink, polygons and polylines, lines, squares and circles.
//!
//! As in the rest of the module, arithmetic follows C's order on `f32`,
//! and `a * b + c` within one C expression is a `mul_add`: clang emits
//! `llvm.fmuladd` for it, taking the left product when both sides are
//! products, so `a * b - c * d` is `a.madd(b, -(c * d))`.

use crate::fma::Madd;
use std::f32::consts::PI;

use super::{Writer, len, pt, real_at};
use crate::geometry::{Point, Rect};
use crate::object::Object;

/// `CIRCLE_MAGIC`: the Bézier control distance for a quarter circle.
const CIRCLE_MAGIC: f32 = 0.551915;

/// `fz_min`: `a < b ? a : b`, which keeps a NaN `b`.
fn fmin(a: f32, b: f32) -> f32 {
    if a < b { a } else { b }
}

/// `fz_max`: `a > b ? a : b`.
fn fmax(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}

/// The content stream is bounded like a decoded stream, so that a cloudy
/// border whose spacing is too small to advance ends with an error where
/// MuPDF would loop until memory runs out.
fn check_size(w: &Writer) -> Result<(), String> {
    if w.buf.len() > w.doc.limits().max_decoded_bytes {
        return Err("appearance stream is too large".into());
    }
    Ok(())
}

/// `draw_circle`.
fn draw_circle(w: &mut Writer, rx: f32, ry: f32, cx: f32, cy: f32) {
    let mx = rx * CIRCLE_MAGIC;
    let my = ry * CIRCLE_MAGIC;
    w.op(&[cx, cy + ry], "m");
    w.op(&[cx + mx, cy + ry, cx + rx, cy + my, cx + rx, cy], "c");
    w.op(&[cx + rx, cy - my, cx + mx, cy - ry, cx, cy - ry], "c");
    w.op(&[cx - mx, cy - ry, cx - rx, cy - my, cx - rx, cy], "c");
    w.op(&[cx - rx, cy + my, cx - mx, cy + ry, cx, cy + ry], "c");
}

/// `draw_arc_seg`: one Bézier for the arc from `th0` to `th1`.
#[allow(clippy::too_many_arguments)]
fn draw_arc_seg(w: &mut Writer, r: f32, xc: f32, yc: f32, th0: f32, th1: f32, mv: bool) {
    let x1 = r.madd(th0.cos(), xc);
    let y1 = r.madd(th0.sin(), yc);
    let x4 = r.madd(th1.cos(), xc);
    let y4 = r.madd(th1.sin(), yc);

    let ax = x1 - xc;
    let ay = y1 - yc;
    let bx = x4 - xc;
    let by = y4 - yc;
    let q1 = ax.madd(ax, ay * ay);
    let q2 = ay.madd(by, ax.madd(bx, q1));
    let k2 = (4.0f32 / 3.0) * ((2.0 * q1 * q2).sqrt() - q2) / ax.madd(by, -(ay * bx));

    let x2 = (-k2).madd(ay, xc + ax);
    let y2 = k2.madd(ax, yc + ay);
    let x3 = k2.madd(by, xc + bx);
    let y3 = (-k2).madd(bx, yc + by);

    if mv {
        w.op(&[x1, y1], "m");
    }
    w.op(&[x2, y2, x3, y3, x4, y4], "c");
}

/// `draw_arc`: halved until each piece is at most a quarter of π.
#[allow(clippy::too_many_arguments)]
fn draw_arc(w: &mut Writer, r: f32, xc: f32, yc: f32, th0: f32, th1: f32, mv: bool) {
    let d = th0 - th1;
    if d > PI / 4.0 {
        draw_arc(w, r, xc, yc, th0, th0 - d / 2.0, mv);
        draw_arc(w, r, xc, yc, th0 - d / 2.0, th1, false);
    } else {
        draw_arc_seg(w, r, xc, yc, th0, th1, mv);
    }
}

/// `draw_arc_tail`.
#[allow(clippy::too_many_arguments)]
fn draw_arc_tail(w: &mut Writer, r: f32, xc: f32, yc: f32, th0: f32, th1: f32, fill: bool) {
    draw_arc_seg(w, r, xc, yc, th0, th1, false);
    if fill {
        draw_arc_seg(w, r, xc, yc, th1, th0, false);
    }
}

/// `rotate_vector`.
fn rotate_vector(angle: f32, x: f32, y: f32) -> Point {
    let ca = angle.cos();
    let sa = angle.sin();
    pt(x.madd(ca, -(y * sa)), x.madd(sa, y * ca))
}

/// `pdf_write_arrow_appearance`.
#[allow(clippy::too_many_arguments)]
fn arrow(w: &mut Writer, rect: &mut Rect, x: f32, y: f32, dx: f32, dy: f32, lw: f32, close: bool) {
    let r = fmax(1.0, lw);
    let angle = dy.atan2(dx);
    let v = rotate_vector(angle, 8.8 * r, 4.5 * r);
    let a = pt(x + v.x, y + v.y);
    let v = rotate_vector(angle, 8.8 * r, -4.5 * r);
    let b = pt(x + v.x, y + v.y);

    *rect = rect.include_point(a).include_point(b).expand(lw);

    w.op(&[a.x, a.y], "m");
    w.op(&[x, y], "l");
    w.op(&[b.x, b.y], "l");
    if close {
        w.s("h\n");
    }
}

/// `include_cap`.
fn include_cap(rect: &mut Rect, x: f32, y: f32, r: f32) {
    rect.x0 = fmin(rect.x0, x - r);
    rect.y0 = fmin(rect.y0, y - r);
    rect.x1 = fmax(rect.x1, x + r);
    rect.y1 = fmax(rect.y1, y + r);
}

/// `pdf_write_line_cap_appearance`: the line ending `cap` at `(x, y)`, the
/// line running off along `(dx, dy)`.
#[allow(clippy::too_many_arguments)]
pub(super) fn line_cap(
    w: &mut Writer,
    rect: &mut Rect,
    x: f32,
    y: f32,
    dx: f32,
    dy: f32,
    lw: f32,
    sc: bool,
    ic: bool,
    cap: &Object,
) {
    let l = dx.madd(dx, dy * dy).sqrt();
    let dx = dx / l;
    let dy = dy / l;

    match cap.as_name().unwrap_or(b"") {
        b"Square" => {
            let r = fmax(3.0, lw * 3.0);
            w.op(&[x - r, y - r, r * 2.0, r * 2.0], "re");
            w.stroke_and_fill(sc, ic);
            include_cap(rect, x, y, r + lw / 2.0);
        }
        b"Circle" => {
            let r = fmax(3.0, lw * 3.0);
            draw_circle(w, r, r, x, y);
            w.stroke_and_fill(sc, ic);
            include_cap(rect, x, y, r + lw / 2.0);
        }
        b"Diamond" => {
            let r = fmax(3.0, lw * 3.0);
            w.op(&[x, y + r], "m");
            w.op(&[x + r, y], "l");
            w.op(&[x, y - r], "l");
            w.op(&[x - r, y], "l");
            w.s("h\n");
            w.stroke_and_fill(sc, ic);
            include_cap(rect, x, y, r + lw / 2.0f32.sqrt());
        }
        b"OpenArrow" => {
            arrow(w, rect, x, y, dx, dy, lw, false);
            w.stroke(sc);
        }
        b"ClosedArrow" => {
            arrow(w, rect, x, y, dx, dy, lw, true);
            w.stroke_and_fill(sc, ic);
        }
        b"Butt" => {
            let r = fmax(3.0, lw * 3.0);
            let a = pt((-dy).madd(r, x), dx.madd(r, y));
            let b = pt(dy.madd(r, x), (-dx).madd(r, y));
            w.op(&[a.x, a.y], "m");
            w.op(&[b.x, b.y], "l");
            w.stroke(sc);
            *rect = rect.include_point(a).include_point(b).expand(lw);
        }
        b"ROpenArrow" => {
            arrow(w, rect, x, y, -dx, -dy, lw, false);
            w.stroke(sc);
        }
        b"RClosedArrow" => {
            arrow(w, rect, x, y, -dx, -dy, lw, true);
            w.stroke_and_fill(sc, ic);
        }
        b"Slash" => {
            let r = fmax(5.0, lw * 5.0);
            let angle = dy.atan2(dx) - (30.0 * PI / 180.0);
            let v = rotate_vector(angle, 0.0, r);
            let a = pt(x + v.x, y + v.y);
            let v = rotate_vector(angle, 0.0, -r);
            let b = pt(x + v.x, y + v.y);
            w.op(&[a.x, a.y], "m");
            w.op(&[b.x, b.y], "l");
            w.stroke(sc);
            *rect = rect.include_point(a).include_point(b).expand(lw);
        }
        _ => {}
    }
}

/// /LE's two line endings, unresolved as `pdf_array_get` gives them (an
/// indirect name is not the name MuPDF compares with), when there are two.
fn line_endings(w: &Writer) -> Option<[Object; 2]> {
    let le = w.get(b"LE");
    if len(w.doc, &le) != 2 {
        return None;
    }
    let a = le.as_array()?;
    Some([a[0].clone(), a[1].clone()])
}

/// The opening every shape writer shares: opacity, dashes, line width,
/// stroke colour and, when `interior`, the interior fill colour.
fn begin(w: &mut Writer, interior: bool) -> Result<(f32, bool, bool), String> {
    w.opacity();
    w.dash_pattern()?;
    let lw = w.border()?;
    let sc = w.stroke_color();
    let ic = if interior {
        w.interior_fill_color()?
    } else {
        false
    };
    Ok((lw, sc, ic))
}

/// `pdf_write_ink_appearance`.
pub(super) fn ink(w: &mut Writer, rect: &mut Rect) -> Result<(), String> {
    let (lw, sc, _) = begin(w, false)?;

    *rect = Rect::EMPTY;

    w.s("1 J\n1 j\n");

    let ink_list = w.get(b"InkList");
    let strokes = ink_list.as_array().unwrap_or(&[]).to_vec();
    let mut p = pt(0.0, 0.0);
    for (i, stroke) in strokes.iter().enumerate() {
        let m = len(w.doc, stroke) / 2;
        for k in 0..m {
            p = pt(
                real_at(w.doc, stroke, k * 2),
                real_at(w.doc, stroke, k * 2 + 1),
            );
            if i == 0 && k == 0 {
                *rect = Rect::new(p.x, p.y, p.x, p.y);
            } else {
                *rect = rect.include_point(p);
            }
            w.op(&[p.x, p.y], if k == 0 { "m" } else { "l" });
        }
        if m == 1 {
            w.op(&[p.x, p.y], "l");
        }
    }
    w.stroke(sc);

    // The line width and six points more, so that a dot or a straight
    // line can still be picked.
    *rect = rect.expand(lw + 6.0);
    let e = lw + 6.0;
    w.put_rect(b"RD", Rect::new(e, e, e, e));
    Ok(())
}

/// `cloud_intensity`: /BE /I when /BE /S is /C.
fn cloud_intensity(w: &Writer) -> Result<f32, String> {
    if !matches!(w.subtype(), "Circle" | "FreeText" | "Polygon" | "Square") {
        return Err(w.no_property("BE"));
    }
    let be = w.get(b"BE");
    let cloudy = w
        .doc
        .resolve(&be)
        .as_dict()
        .and_then(|d| d.get(b"S"))
        .is_some_and(|s| s.is_name(b"C"));
    Ok(if cloudy {
        w.doc.dict_get(&be, b"I").to_real()
    } else {
        0.0
    })
}

/// `struct cloud_list`: the first two and the latest three points.
struct Cloud {
    data: [Point; 5],
    len: usize,
    first: bool,
    fill: bool,
    spacing: f32,
    radius: f32,
    phase: f32,
}

/// `intersect_cloud`: the angle at which circles of radius `r` about `p0`
/// and `p1` cross, seen from `p1` (`sel` 0) or `p0`.
fn intersect_cloud(p0: Point, p1: Point, r: f32, sel: i32) -> f32 {
    let dx = p1.x - p0.x;
    let dy = p1.y - p0.y;
    let d = dx.madd(dx, dy * dy).sqrt();

    if d >= r + r {
        return 0.0;
    }
    if d <= 0.0 {
        return 0.0;
    }

    let a = d / 2.0;
    let h = r.madd(r, -(a * a)).sqrt();

    let x2 = (p0.x + p1.x) / 2.0;
    let y2 = (p0.y + p1.y) / 2.0;

    let x3 = x2 - h * (p1.y - p0.y) / d;
    let y3 = y2 + h * (p1.x - p0.x) / d;

    // `atan2`, in double.
    let (y, x) = if sel == 0 {
        (y3 - p1.y, x3 - p1.x)
    } else {
        (y3 - p0.y, x3 - p0.x)
    };
    f64::from(y).atan2(f64::from(x)) as f32
}

impl Cloud {
    /// `start_cloud`.
    fn start(w: &mut Writer, border: f32, intensity: f32, fill: bool) -> Cloud {
        // Constants measured from Acrobat Reader.
        let c = Cloud {
            data: [pt(0.0, 0.0); 5],
            len: 0,
            first: true,
            fill,
            spacing: intensity.madd(6.666667, border * 0.8333333),
            radius: intensity.madd(4.0, border * 0.5),
            phase: 0.0,
        };
        w.s("2 j\n"); // bevel join
        c
    }

    /// `emit_cloud`.
    fn emit(&mut self, w: &mut Writer, a: Point, b: Point, c: Point) {
        let th0 = intersect_cloud(a, b, self.radius, 0);
        let mut th1 = intersect_cloud(b, c, self.radius, 1);
        while th1 > th0 {
            th1 -= PI * 2.0;
        }
        draw_arc(w, self.radius, b.x, b.y, th0, th1, self.first || !self.fill);
        draw_arc_tail(w, self.radius, b.x, b.y, th1, th1 - PI / 8.0, self.fill);
        self.first = false;
    }

    /// `add_cloud`.
    fn add(&mut self, w: &mut Writer, x: f32, y: f32) {
        if self.len < 5 {
            self.data[self.len] = pt(x, y);
            self.len += 1;
        } else {
            self.data[2] = self.data[3];
            self.data[3] = self.data[4];
            self.data[4] = pt(x, y);
        }
        if self.len >= 3 {
            let (a, b, c) = (
                self.data[self.len - 3],
                self.data[self.len - 2],
                self.data[self.len - 1],
            );
            self.emit(w, a, b, c);
        }
    }

    /// `add_cloud_line`.
    fn line(&mut self, w: &mut Writer, x0: f32, y0: f32, x1: f32, y1: f32) -> Result<(), String> {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let total = dx.hypot(dy);
        let mut used = 0.0f32;

        if self.phase == 0.0 {
            self.add(w, x0, y0);
        }

        while total - used > self.spacing - self.phase {
            used += self.spacing - self.phase;
            let t = used / total;
            self.add(w, dx.madd(t, x0), dy.madd(t, y0));
            self.phase = 0.0;
            check_size(w)?;
        }

        self.phase += total - used;
        Ok(())
    }

    /// `add_cloud_circle`: the ellipse in the box, as 32 lines.
    fn circle(&mut self, w: &mut Writer, x0: f32, y0: f32, x1: f32, y1: f32) -> Result<(), String> {
        let cx = (x0 + x1) / 2.0;
        let cy = (y0 + y1) / 2.0;
        let rx = (x1 - x0) / 2.0;
        let ry = (y1 - y0) / 2.0;
        let da = -PI * 2.0 / 32.0;
        for i in 1..=32 {
            let ax = (((i - 1) as f32) * da).cos().madd(rx, cx);
            let ay = (((i - 1) as f32) * da).sin().madd(ry, cy);
            let bx = ((i as f32) * da).cos().madd(rx, cx);
            let by = ((i as f32) * da).sin().madd(ry, cy);
            self.line(w, ax, ay, bx, by)?;
        }
        Ok(())
    }

    /// `end_cloud`: join the last circles up with the first.
    fn end(&mut self, w: &mut Writer) {
        let d = self.data;
        match self.len {
            1 => draw_circle(w, self.radius, self.radius, d[0].x, d[0].y),
            2 => {
                self.emit(w, d[0], d[1], d[0]);
                self.emit(w, d[1], d[0], d[1]);
            }
            3 => {
                self.emit(w, d[1], d[2], d[0]);
                self.emit(w, d[2], d[0], d[1]);
            }
            4 => {
                self.emit(w, d[2], d[3], d[0]);
                self.emit(w, d[3], d[0], d[1]);
            }
            5 => {
                self.emit(w, d[3], d[4], d[0]);
                self.emit(w, d[4], d[0], d[1]);
            }
            _ => {}
        }
    }
}

/// `pdf_annot_rect_diff`: /RD, or zeros.
fn rect_diff(w: &Writer) -> Rect {
    let rd = w.get(b"RD");
    if !matches!(rd, Object::Array(_)) {
        return Rect::new(0.0, 0.0, 0.0, 0.0);
    }
    Rect::new(
        real_at(w.doc, &rd, 0),
        real_at(w.doc, &rd, 1),
        real_at(w.doc, &rd, 2),
        real_at(w.doc, &rd, 3),
    )
}

/// Raise each side of `rd` to at least `exp`.
fn at_least(rd: &mut Rect, exp: f32) {
    if rd.x0 < exp {
        rd.x0 = exp;
    }
    if rd.x1 < exp {
        rd.x1 = exp;
    }
    if rd.y0 < exp {
        rd.y0 = exp;
    }
    if rd.y1 < exp {
        rd.y1 = exp;
    }
}

/// What the square and circle writers share before drawing: the line
/// width, colours, the box the border's centre runs along, and /RD raised
/// to half the line width.
struct Boxed {
    lw: f32,
    sc: bool,
    ic: bool,
    rd: Rect,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

fn boxed(w: &mut Writer) -> Result<Boxed, String> {
    let (lw, sc, ic) = begin(w, true)?;
    let orect = w.doc.to_rect(Some(&w.get(b"Rect")));
    let mut rd = rect_diff(w);

    // Nothing drawn should reach outside /Rect, and the border's centre
    // should lie on /Rect less /RD; a large /RD or line width cannot have
    // both, and this does what it can.
    at_least(&mut rd, lw / 2.0);

    let x = orect.x0 + rd.x0;
    let y = orect.y0 + rd.y0;
    let mut bw = orect.x1 - orect.x0 - rd.x0 - rd.x1;
    let mut bh = orect.y1 - orect.y0 - rd.y0 - rd.y1;

    if bw < 1.0 {
        bw = 1.0;
    }
    if bh < 1.0 {
        bh = 1.0;
    }
    Ok(Boxed {
        lw,
        sc,
        ic,
        rd,
        x,
        y,
        w: bw,
        h: bh,
    })
}

/// `pdf_write_square_appearance`.
pub(super) fn square(w: &mut Writer, rect: &mut Rect) -> Result<(), String> {
    let Boxed {
        lw,
        sc,
        ic,
        mut rd,
        x,
        y,
        w: bw,
        h: bh,
    } = boxed(w)?;
    let mut exp = lw / 2.0;

    let cloud = cloud_intensity(w)?;
    if cloud > 0.0 {
        let mut c = Cloud::start(w, lw, cloud, ic);
        c.line(w, x, y, x, y + bh)?;
        c.phase = 0.0;
        c.line(w, x, y + bh, x + bw, y + bh)?;
        c.phase = 0.0;
        c.line(w, x + bw, y + bh, x + bw, y)?;
        c.phase = 0.0;
        c.line(w, x + bw, y, x, y)?;
        c.end(w);
        exp += c.radius;
        at_least(&mut rd, exp);
    } else {
        w.op(&[x, y, bw, bh], "re");
    }
    w.stroke_and_fill(sc, ic);

    w.put_rect(b"RD", rd);
    rect.x0 = x - rd.x0;
    rect.y0 = y - rd.y0;
    rect.x1 = x + bw + rd.x1;
    rect.y1 = y + bh + rd.y1;
    Ok(())
}

/// `pdf_write_circle_appearance`.
pub(super) fn circle(w: &mut Writer, rect: &mut Rect) -> Result<(), String> {
    let Boxed {
        lw,
        sc,
        ic,
        mut rd,
        x,
        y,
        w: bw,
        h: bh,
    } = boxed(w)?;
    let mut exp = lw / 2.0;

    let cloud = cloud_intensity(w)?;
    if cloud > 0.0 {
        let mut c = Cloud::start(w, lw, cloud, ic);
        c.circle(w, x, y, x + bw, y + bh)?;
        c.end(w);
        exp += c.radius;
        at_least(&mut rd, exp);
    } else {
        draw_circle(w, bw / 2.0, bh / 2.0, x + bw / 2.0, y + bh / 2.0);
    }
    w.stroke_and_fill(sc, ic);

    w.put_rect(b"RD", rd);
    rect.x0 = x - rd.x0;
    // MuPDF takes the bottom from the right-hand difference.
    rect.y0 = y - rd.x1;
    rect.x1 = x + bw + rd.x1;
    rect.y1 = y + bh + rd.y1;
    Ok(())
}

/// `polygon_winding`: whether the vertices run so that drawing them in
/// order leaves most of the area right. Matches Acrobat for simple
/// polygons, as MuPDF says, and makes a fair guess for others.
fn polygon_winding(w: &Writer, v: &Object, n: usize) -> bool {
    let mut mid_y = 0.0f32;
    let mut min_x = 0.0f32;
    let mut area = 0.0f32;

    // The centre in y, and the least x.
    for i in 0..n {
        let x = real_at(w.doc, v, 2 * i);
        if x < min_x || i == 0 {
            min_x = x;
        }
        mid_y += real_at(w.doc, v, 2 * i + 1);
    }
    mid_y /= n as f32;

    // The area weighted by winding across that middle line.
    let mut q = pt(real_at(w.doc, v, 0), real_at(w.doc, v, 1));
    for i in 1..n {
        let r = pt(real_at(w.doc, v, 2 * i), real_at(w.doc, v, 2 * i + 1));
        if (r.y > mid_y && mid_y > q.y) || (r.y < mid_y && mid_y < q.y) {
            let wn: f32 = if r.y > mid_y { 1.0 } else { -1.0 };
            let x = r.x + (q.x - r.x) * (r.y - mid_y) / (r.y - q.y);
            area = (x - min_x).madd(wn, area);
        }
        q = r;
    }

    area < 0.0
}

/// `pdf_write_polygon_appearance`, closed for Polygon, open for PolyLine.
pub(super) fn polygon(w: &mut Writer, rect: &mut Rect, close: bool) -> Result<(), String> {
    let (lw, sc, ic) = begin(w, true)?;
    let mut exp = 0.0f32;

    *rect = Rect::EMPTY;

    let cloud = if close { cloud_intensity(w)? } else { 0.0 };

    let verts = w.get(b"Vertices");
    let n = len(w.doc, &verts) / 2;
    let vertex = |w: &Writer, i: usize| {
        pt(
            real_at(w.doc, &verts, i * 2),
            real_at(w.doc, &verts, i * 2 + 1),
        )
    };
    if n > 0 {
        let forward = polygon_winding(w, &verts, n);

        let mut c = (cloud > 0.0).then(|| Cloud::start(w, lw, cloud, ic));
        let (mut first, mut last) = (pt(0.0, 0.0), pt(0.0, 0.0));

        for j in 0..n {
            let p = vertex(w, if forward { j } else { n - 1 - j });
            if j == 0 {
                *rect = Rect::new(p.x, p.y, p.x, p.y);
            } else {
                *rect = rect.include_point(p);
            }
            if let Some(c) = &mut c {
                if j == 0 {
                    first = p;
                } else {
                    c.line(w, last.x, last.y, p.x, p.y)?;
                }
                last = p;
            } else {
                w.op(&[p.x, p.y], if j == 0 { "m" } else { "l" });
            }
        }

        if let Some(c) = &mut c {
            c.line(w, last.x, last.y, first.x, first.y)?;
            c.end(w);
        } else if close {
            w.s("h\n");
        }

        if close {
            w.stroke_and_fill(sc, ic);
        } else {
            w.stroke(sc);
        }

        exp = lw;
        if let Some(c) = &c {
            exp += c.radius;
        }

        *rect = rect.expand(exp);
    }

    if !close
        && n >= 2
        && let Some([le0, le1]) = line_endings(w)
    {
        let a = vertex(w, 0);
        let b = vertex(w, 1);
        line_cap(w, rect, a.x, a.y, b.x - a.x, b.y - a.y, lw, sc, ic, &le0);

        let a = vertex(w, n - 1);
        let b = vertex(w, n - 2);
        line_cap(w, rect, a.x, a.y, b.x - a.x, b.y - a.y, lw, sc, ic, &le1);
    }

    if exp == 0.0 {
        w.del(b"RD");
    } else {
        w.put_rect(b"RD", Rect::new(exp, exp, exp, exp));
    }
    Ok(())
}

/// `pdf_write_line_appearance`: the line, its leader lines, caption and
/// endings. `a` and `b` are the ends of the line itself, `aa`..`bb` those
/// of the leader lines.
pub(super) fn line(w: &mut Writer, rect: &mut Rect) -> Result<(), String> {
    let (lw, sc, ic) = begin(w, true)?;

    let l = w.get(b"L");
    let mut a = pt(real_at(w.doc, &l, 0), real_at(w.doc, &l, 1));
    let mut b = pt(real_at(w.doc, &l, 2), real_at(w.doc, &l, 3));

    // The line's direction.
    let mut dx = b.x - a.x;
    let mut dy = b.y - a.y;
    let line_length = dx.hypot(dy);
    dx /= line_length;
    dy /= line_length;

    rect.x0 = fmin(a.x, b.x);
    rect.y0 = fmin(a.y, b.y);
    rect.x1 = fmax(a.x, b.x);
    rect.y1 = fmax(a.y, b.y);

    let ll = w.get(b"LL").to_real();

    if ll != 0.0 {
        let mut lle = w.get(b"LLE").to_real();
        let mut llo = w.get(b"LLO").to_real();
        if ll < 0.0 {
            lle = -lle;
            llo = -llo;
        }

        let aa = pt((-dy).madd(llo, a.x), dx.madd(llo, a.y));
        let ba = pt((-dy).madd(llo, b.x), dx.madd(llo, b.y));
        let far = llo + ll + lle;
        let ab = pt((-dy).madd(far, a.x), dx.madd(far, a.y));
        let bb = pt((-dy).madd(far, b.x), dx.madd(far, b.y));
        let off = llo + ll;
        a = pt((-dy).madd(off, a.x), dx.madd(off, a.y));
        b = pt((-dy).madd(off, b.x), dx.madd(off, b.y));

        w.op(&[aa.x, aa.y], "m");
        w.op(&[ab.x, ab.y], "l");
        w.op(&[ba.x, ba.y], "m");
        w.op(&[bb.x, bb.y], "l");

        *rect = rect
            .include_point(a)
            .include_point(b)
            .include_point(ab)
            .include_point(bb);
    }

    let gap = if matches!(w.get(b"Cap"), Object::Bool(true)) {
        super::text::line_caption(w, rect, a, b)?
    } else {
        0.0
    };
    if gap > 0.0 {
        let m = pt((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
        let ca = pt((-dx).madd(gap, m.x), (-dy).madd(gap, m.y));
        let cb = pt(dx.madd(gap, m.x), dy.madd(gap, m.y));
        w.op(&[a.x, a.y], "m");
        w.op(&[ca.x, ca.y], "l");
        w.op(&[cb.x, cb.y], "m");
        w.op(&[b.x, b.y], "l");
    } else {
        w.op(&[a.x, a.y], "m");
        w.op(&[b.x, b.y], "l");
    }

    w.stroke(sc);

    if let Some([le0, le1]) = line_endings(w) {
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        line_cap(w, rect, a.x, a.y, dx, dy, lw, sc, ic, &le0);
        line_cap(w, rect, b.x, b.y, -dx, -dy, lw, sc, ic, &le1);
    }
    *rect = rect.expand(fmax(1.0, lw));
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Each case's content, /Rect and /RD are MuPDF 1.28.6's for the same
    //! annotation dictionary.

    use crate::geometry::Matrix;
    use crate::object::{ObjRef, Object};
    use crate::{Document, Limits};

    use super::super::{Synthesis, synthesize};

    fn pdf(dict: &str) -> Vec<u8> {
        format!(
            "%PDF-1.7\n1 0 obj\n<< /Type /Annot {dict} >>\nendobj\n\
             2 0 obj\n<< /Type /Catalog >>\nendobj\ntrailer\n<< /Size 3 /Root 2 0 R >>\n"
        )
        .into_bytes()
    }

    fn synth(doc: &Document) -> Result<Synthesis, String> {
        synthesize(doc, &Object::Ref(ObjRef::new(1, 0)), Matrix::IDENTITY)
    }

    fn reals(doc: &Document, s: &Synthesis, key: &[u8]) -> Option<[f32; 4]> {
        let a = doc.dict_get(&s.annot, key);
        let a = a.as_array()?;
        Some([0, 1, 2, 3].map(|i| a[i].to_real()))
    }

    fn check(dict: &str, content: &str, rect: [f32; 4], rd: Option<[f32; 4]>) {
        let doc = Document::open(pdf(dict)).unwrap();
        let s = synth(&doc).unwrap();
        assert_eq!(String::from_utf8_lossy(&s.content), content);
        assert_eq!(reals(&doc, &s, b"Rect"), Some(rect));
        assert_eq!(reals(&doc, &s, b"RD"), rd);
        let bbox = s.bbox;
        assert_eq!([bbox.x0, bbox.y0, bbox.x1, bbox.y1], rect);
        assert_eq!(s.matrix, Matrix::IDENTITY);
    }

    #[test]
    fn opacity_goes_into_an_extgstate() {
        let doc = Document::open(pdf("/Subtype /Square /Rect [0 0 10 10] /CA 0.5")).unwrap();
        let s = synth(&doc).unwrap();
        let res = Object::Dict(std::sync::Arc::new(s.resources.unwrap()));
        let h = doc.dict_get(&doc.dict_get(&res, b"ExtGState"), b"H");
        assert!(doc.dict_get(&h, b"Type").is_name(b"ExtGState"));
        assert_eq!(doc.dict_get(&h, b"CA").to_real(), 0.5);
        assert_eq!(doc.dict_get(&h, b"ca").to_real(), 0.5);
        assert!(matches!(doc.dict_get(&h, b"BM"), Object::Null));
    }

    #[test]
    fn a_cloud_that_cannot_advance_stops_at_the_size_limit() {
        // The spacing is so small next to the side that adding it no longer
        // moves along: MuPDF loops here without end.
        let dict = "/Subtype /Square /Rect [0 0 1000000 1000000] \
                    /BE << /S /C /I 0.000001 >> /BS << /W 0 >>";
        let limits = Limits {
            max_decoded_bytes: 100_000,
            ..Limits::default()
        };
        let doc = Document::open_with(pdf(dict), limits).unwrap();
        assert_eq!(synth(&doc).unwrap_err(), "appearance stream is too large");
    }

    #[test]
    fn square_cloudy_filled() {
        check(
            r#"/Subtype /Square /Rect [100 100 112 106] /C [1 0 0] /IC [0 0 1] /BE << /S /C /I 1 >>"#,
            r#"1 w
1 0 0 RG
0 0 1 rg
2 j
96.75834 103 m
96.42968 103.49189 96.20124 104.043758 96.08608 104.62402 c
95.970928 105.20429 95.97131 105.80156 96.08722 106.381679 c
96.203128 106.96179 96.43228 107.51337 96.76157 108.00483 c
97.090869 108.49629 97.51383 108.918 98.00626 109.245838 c
98.4987 109.57367 99.05095 109.801189 99.63141 109.915378 c
100.21186 110.029567 100.80914 110.028179 101.38906 109.9113 c
101.96899 109.79442 102.520168 109.56435 103.01108 109.23423 c
103.50199 108.90411 103.92299 108.48045 104.25 107.987468 c
104.57665 107.49502 104.80313 106.94302 104.916469 106.36305 c
104.80312 106.94304 104.57667 107.494998 104.25 107.987468 c
104.829799 108.86154 105.69672 109.50563 106.70096 109.80842 c
107.70519 110.111209 108.78367 110.05369 109.75 109.64578 c
110.29443 109.41597 110.78826 109.08118 111.20331 108.66051 c
110.78827 109.08117 110.294429 109.41597 109.75 109.64578 c
110.64179 110.022228 111.6312 110.10107 112.57136 109.870609 c
113.51151 109.640148 114.352298 109.11267 114.9689 108.366489 c
115.58551 107.62029 115.94507 106.69517 115.9942 105.728427 c
116.043338 104.76168 115.779438 103.80486 115.24166 103 c
114.91336 102.50864 114.491489 102.086757 114.00014 101.75843 c
114.491489 102.086757 114.91335 102.50864 115.24166 103 c
115.57032 102.50811 115.79876 101.956249 115.91392 101.375988 c
116.029079 100.795719 116.02869 100.19845 115.91278 99.618328 c
115.796878 99.03821 115.56772 98.48663 115.23843 97.99517 c
114.909137 97.50371 114.48617 97.082 113.99374 96.754169 c
113.5013 96.42633 112.94906 96.198818 112.3686 96.084629 c
111.78814 95.970439 111.19086 95.971828 110.61094 96.0887 c
110.03101 96.20558 109.479839 96.43565 108.98892 96.76577 c
108.49801 97.09589 108.07701 97.51955 107.75 98.012538 c
107.42334 98.505 107.19688 99.05697 107.08354 99.63695 c
107.19688 99.05698 107.42335 98.50499 107.75 98.012538 c
107.170207 97.13846 106.30328 96.49437 105.29904 96.19158 c
104.29481 95.888797 103.21633 95.94631 102.25 96.35422 c
101.70557 96.58403 101.21174 96.91882 100.79669 97.33949 c
101.21173 96.91883 101.705577 96.58403 102.25 96.35422 c
101.35821 95.977779 100.3688 95.89893 99.42864 96.129398 c
98.48849 96.359859 97.647708 96.88733 97.0311 97.633518 c
96.41449 98.37971 96.05493 99.30483 96.0058 100.271579 c
95.956668 101.23832 96.220569 102.19514 96.75834 103 c
97.08665 103.49137 97.508518 103.913249 97.99987 104.24157 c
97.50852 103.913249 97.08664 103.491359 96.75834 103 c
b
"#,
            [95.5, 95.5, 116.5, 110.5],
            Some([5.0, 5.0, 5.0, 5.0]),
        );
    }

    // MuPDF's bytes on aarch64: clouds go through cosf, sinf and fused
    // multiply-adds, which round otherwise on x86-64 and its libm.
    #[test]
    #[cfg_attr(not(target_arch = "aarch64"), ignore = "expected bytes are MuPDF's on aarch64")]
    fn circle_cloudy() {
        check(
            r#"/Subtype /Circle /Rect [100 100 106 104] /C [0 1 0] /BS << /W 0.5 >> /BE << /S /C /I 0.5 >>"#,
            r#".5 w
0 1 0 RG
2 j
105.03765 99.86574 m
104.94102 99.32437 104.649478 98.83716 104.21809 98.4961 c
103.786708 98.15505 103.245349 97.98378 102.696308 98.01466 c
102.147258 98.04555 101.628528 98.276439 101.238109 98.66371 c
100.84769 99.050998 100.612628 99.56783 100.57731 100.11662 c
100.558338 100.41149 100.597629 100.70721 100.69293 100.986888 c
100.57731 100.11662 m
100.22638 100.07417 99.87039 100.115 99.538188 100.2358 c
99.20598 100.356609 98.90691 100.55397 98.66522 100.81192 c
98.42353 101.06986 98.24601 101.381137 98.14706 101.72048 c
98.0481 102.05984 98.030498 102.417728 98.095668 102.76515 c
98.160839 103.11259 98.306949 103.43976 98.522159 103.720188 c
98.73736 104.0006 99.01562 104.22639 99.33436 104.3792 c
99.65311 104.53203 100.003368 104.6076 100.356769 104.59979 c
100.71017 104.59199 101.05675 104.50103 101.36844 104.33429 c
101.62897 104.194919 101.85951 104.005588 102.046878 103.777118 c
101.36844 104.33429 m
101.50473 104.81173 101.79513 105.23069 102.194377 105.52587 c
102.59363 105.82105 103.07929 105.975879 103.57572 105.96623 c
104.07214 105.95659 104.55143 105.783008 104.93891 105.472537 c
105.326389 105.16206 105.60029 104.73215 105.71793 104.24977 c
105.78794 103.962718 105.80073 103.66467 105.755558 103.37267 c
105.71794 104.24977 m
106.04526 104.25443 106.36966 104.18762 106.66849 104.05399 c
106.96732 103.920368 107.2334 103.72314 107.44815 103.476078 c
107.66291 103.229007 107.82115 102.93807 107.91187 102.623538 c
108.002597 102.309009 108.02358 101.97846 107.97339 101.65498 c
107.923198 101.331508 107.80302 101.02288 107.62125 100.75063 c
107.43948 100.47839 107.200489 100.24907 106.92097 100.07871 c
106.64143 99.90835 106.32811 99.80103 106.00283 99.76425 c
105.67756 99.72747 105.34816 99.7621 105.03765 99.86574 c
104.75737 99.95929 104.498279 100.10712 104.27513 100.300807 c
S
"#,
            [97.75, 97.75, 108.25, 106.25],
            Some([2.5, 2.5, 2.5, 2.5]),
        );
    }

    #[test]
    fn square_dashed_with_opacity_and_rd() {
        check(
            r#"/Subtype /Square /Rect [10 20 110 70] /C [0 0 0 1] /BS << /W 3 /D [3 2] >> /CA 0.5 /RD [1 2 3 4]"#,
            r#"/H gs
[3 2]0 d
3 w
0 0 0 1 K
11.5 22 95.5 44 re
S
"#,
            [10.0, 20.0, 110.0, 70.0],
            Some([1.5, 2.0, 3.0, 4.0]),
        );
    }

    #[test]
    fn circle_with_rd_takes_bottom_from_right_difference() {
        check(
            r#"/Subtype /Circle /Rect [10 20 110 70] /C [0.5] /IC [1 1 0] /RD [1 2 3 4]"#,
            r#"1 w
.5 G
1 1 0 rg
59 66 m
85.49192 66 107 56.142129 107 44 c
107 31.85787 85.49192 22 59 22 c
32.50808 22 11 31.85787 11 44 c
11 56.142129 32.50808 66 59 66 c
b
"#,
            [10.0, 19.0, 110.0, 70.0],
            Some([1.0, 2.0, 3.0, 4.0]),
        );
    }

    #[test]
    fn polygon_counter_clockwise_is_drawn_backwards() {
        check(
            r#"/Subtype /Polygon /Rect [0 0 1 1] /Vertices [100 100 200 100 200 200 100 200] /C [1 0 0] /IC [0 1 0]"#,
            "1 w\n1 0 0 RG\n0 1 0 rg\n100 200 m\n200 200 l\n200 100 l\n100 100 l\nh\nb\n",
            [99.0, 99.0, 201.0, 201.0],
            Some([1.0, 1.0, 1.0, 1.0]),
        );
    }

    #[test]
    fn polygon_clockwise_keeps_its_order() {
        check(
            r#"/Subtype /Polygon /Rect [0 0 1 1] /Vertices [100 100 100 200 200 200 200 100] /C [1 0 0] /IC [0 1 0]"#,
            r#"1 w
1 0 0 RG
0 1 0 rg
100 100 m
100 200 l
200 200 l
200 100 l
h
b
"#,
            [99.0, 99.0, 201.0, 201.0],
            Some([1.0, 1.0, 1.0, 1.0]),
        );
    }

    // MuPDF's bytes on aarch64: clouds go through cosf, sinf and fused
    // multiply-adds, which round otherwise on x86-64 and its libm.
    #[test]
    #[cfg_attr(not(target_arch = "aarch64"), ignore = "expected bytes are MuPDF's on aarch64")]
    fn polygon_cloudy() {
        check(
            r#"/Subtype /Polygon /Rect [0 0 1 1] /Vertices [100 100 110 100 105 108] /C [0 0 1] /BE << /S /C /I 1 >>"#,
            r#"1 w
0 0 1 RG
2 j
109.09686 106.13837 m
109.72406 106.121379 110.340778 105.97341 110.9074 105.70398 c
111.47404 105.43455 111.97806 105.0496 112.387119 104.57385 c
112.79618 104.09811 113.10123 103.542079 113.282688 102.94146 c
113.46414 102.34084 113.518 101.70892 113.44079 101.086269 c
113.36358 100.46361 113.15701 99.863979 112.83436 99.32587 c
112.511699 98.78776 112.080089 98.32309 111.567218 97.961658 c
111.05435 97.60023 110.47156 97.35004 109.85629 97.22716 c
109.24102 97.10428 108.60684 97.11142 107.99449 97.24813 c
107.41774 97.37689 106.872 97.61799 106.38843 97.957668 c
107.99449 97.24813 m
107.51039 96.62178 106.86964 96.134227 106.13689 95.834659 c
105.404148 95.53509 104.60532 95.43411 103.82109 95.54193 c
103.03685 95.64975 102.29492 95.96254 101.6702 96.44873 c
101.04547 96.93492 100.56007 97.57731 100.262958 98.31106 c
100.04116 98.85881 99.92943 99.44487 99.93413 100.035808 c
100.26295 98.31106 m
99.32251 98.60971 98.506298 99.2092 97.939968 100.01722 c
97.37363 100.82525 97.08859 101.797008 97.12871 102.78292 c
97.16882 103.76882 97.53187 104.71423 98.161968 105.47357 c
98.79206 106.23292 99.65428 106.764118 100.615879 106.98538 c
101.19178 107.117908 101.78813 107.13569 102.37091 107.03773 c
100.615879 106.985378 m
100.45466 107.681987 100.46176 108.40697 100.63659 109.10029 c
100.81142 109.7936 101.14894 110.43529 101.62119 110.97215 c
102.09345 111.509029 102.68683 111.92562 103.352199 112.18745 c
104.01755 112.44927 104.73572 112.548778 105.4472 112.47772 c
106.15867 112.40667 106.84298 112.16709 107.44341 111.778858 c
108.043849 111.39061 108.54312 110.86488 108.89987 110.24522 c
109.25662 109.62556 109.46058 108.929798 109.494838 108.2156 c
109.52909 107.501399 109.39267 106.78934 109.09686 106.13837 c
108.852397 105.600368 108.50434 105.11577 108.07259 104.71227 c
S
"#,
            [94.5, 94.5, 115.5, 113.5],
            Some([5.5, 5.5, 5.5, 5.5]),
        );
    }

    #[test]
    fn polyline_with_endings() {
        check(
            r#"/Subtype /PolyLine /Rect [0 0 1 1] /Vertices [100 100 200 150 300 120] /C [1 0 0] /IC [0 0 1] /LE [/ClosedArrow /Slash] /BS << /W 2 >>"#,
            r#"2 w
1 0 0 RG
0 0 1 rg
100 100 m
200 150 l
300 120 l
S
111.716999 115.92081 m
100 100 l
119.766849 99.82111 l
h
b
292.72236 113.14172 m
307.27766 126.85828 l
S
"#,
            [94.0, 94.0, 309.27765, 156.0],
            Some([2.0, 2.0, 2.0, 2.0]),
        );
    }

    #[test]
    fn line_with_leader_lines_and_endings() {
        check(
            r#"/Subtype /Line /Rect [0 0 1 1] /L [100 100 300 200] /C [0 0 1] /IC [1 1 0] /LE [/Butt /RClosedArrow] /LL 20 /LLE 5 /LLO 3"#,
            r#"1 w
0 0 1 RG
1 1 0 rg
98.658359 102.68328 m
87.47802 125.04396 l
298.65837 202.68329 m
287.47804 225.04396 l
89.71409 120.57182 m
289.71409 220.57183 l
S
88.372447 123.255107 m
91.05573 117.88854 l
S
295.57258 228.53223 m
289.71409 220.57183 l
299.5975 220.48238 l
h
b
"#,
            [84.47802, 97.0, 303.0, 230.53223],
            None,
        );
    }

    #[test]
    fn line_with_negative_leader_and_legacy_dashes() {
        check(
            r#"/Subtype /Line /Rect [0 0 1 1] /L [100.5 200.25 400.75 350.125] /C [0 0 1] /LE [/Diamond /Circle] /LL -15 /Border [0 0 3 [4 1]]"#,
            r#"[4 1]0 d
3 w
0 0 1 RG
100.5 200.25 m
107.199268 186.82913 l
400.75 350.125 m
407.44926 336.70414 l
107.199268 186.82913 m
407.44926 336.70414 l
S
107.199268 195.82913 m
116.199268 186.82913 l
107.199268 177.82913 l
98.199268 186.82913 l
h
S
407.44926 345.70414 m
412.41648 345.70414 416.44926 341.67137 416.44926 336.70414 c
416.44926 331.7369 412.41648 327.70414 407.44926 327.70414 c
402.48204 327.70414 398.44926 331.7369 398.44926 336.70414 c
398.44926 341.67137 402.48204 345.70414 407.44926 345.70414 c
S
"#,
            [93.07794, 172.70781, 420.94925, 353.125],
            None,
        );
    }

    #[test]
    fn line_without_colour() {
        check(
            r#"/Subtype /Line /Rect [0 0 1 1] /L [300 100 120 410] /LE [/Square /ROpenArrow]"#,
            r#"1 w
300 100 m
120 410 l
n
297 97 6 6 re
n
111.68966 415.35054 m
120 410 l
119.472759 419.86976 l
n
"#,
            [109.68966, 94.5, 305.5, 421.86975],
            None,
        );
    }

    #[test]
    fn ink_with_a_single_point_stroke() {
        check(
            r#"/Subtype /Ink /Rect [0 0 1 1] /InkList [[100 100 200 200 300 150] [50 60]] /C [1 0 0] /BS << /W 3 >>"#,
            r#"3 w
1 0 0 RG
1 J
1 j
100 100 m
200 200 l
300 150 l
50 60 m
50 60 l
S
"#,
            [41.0, 51.0, 309.0, 209.0],
            Some([9.0, 9.0, 9.0, 9.0]),
        );
    }

    #[test]
    fn ink_without_strokes() {
        check(
            r#"/Subtype /Ink /Rect [0 0 1 1] /InkList []"#,
            r#"1 w
1 J
1 j
n
"#,
            [2147483520.0, 2147483520.0, -2147483648.0, -2147483648.0],
            Some([7.0, 7.0, 7.0, 7.0]),
        );
    }
}
