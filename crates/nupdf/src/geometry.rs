// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/geometry.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Points, rectangles and matrices, in single precision as MuPDF computes
//! them, so that coordinates written back into a document match its.

/// `FZ_MIN_INF_RECT` and `FZ_MAX_INF_RECT`: the corners of the infinite
/// rectangle, as floats.
const MIN_INF: f32 = -2_147_483_648.0;
const MAX_INF: f32 = 2_147_483_520.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

/// `fz_matrix`: `[a b c d e f]`, mapping `(x, y)` to
/// `(x a + y c + e, x b + y d + f)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Rect {
    /// `fz_empty_rect`.
    pub const EMPTY: Rect = Rect {
        x0: MAX_INF,
        y0: MAX_INF,
        x1: MIN_INF,
        y1: MIN_INF,
    };
    /// `fz_infinite_rect`.
    pub const INFINITE: Rect = Rect {
        x0: MIN_INF,
        y0: MIN_INF,
        x1: MAX_INF,
        y1: MAX_INF,
    };
    /// `fz_unit_rect`.
    pub const UNIT: Rect = Rect {
        x0: 0.0,
        y0: 0.0,
        x1: 1.0,
        y1: 1.0,
    };

    pub const fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect { x0, y0, x1, y1 }
    }

    /// `fz_is_valid_rect`.
    pub fn is_valid(&self) -> bool {
        self.x0 <= self.x1 && self.y0 <= self.y1
    }

    /// `fz_is_empty_rect`.
    pub fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    /// `fz_is_infinite_rect`.
    pub fn is_infinite(&self) -> bool {
        self.x0 == MIN_INF && self.x1 == MAX_INF && self.y0 == MIN_INF && self.y1 == MAX_INF
    }

    /// `fz_union_rect`.
    pub fn union(self, b: Rect) -> Rect {
        if !b.is_valid() {
            return self;
        }
        if !self.is_valid() {
            return b;
        }
        if self.is_infinite() {
            return self;
        }
        if b.is_infinite() {
            return b;
        }
        let mut a = self;
        if a.x0 > b.x0 {
            a.x0 = b.x0;
        }
        if a.y0 > b.y0 {
            a.y0 = b.y0;
        }
        if a.x1 < b.x1 {
            a.x1 = b.x1;
        }
        if a.y1 < b.y1 {
            a.y1 = b.y1;
        }
        a
    }

    /// `fz_expand_rect`.
    pub fn expand(self, e: f32) -> Rect {
        if self.is_infinite() || !self.is_valid() {
            return self;
        }
        Rect::new(self.x0 - e, self.y0 - e, self.x1 + e, self.y1 + e)
    }

    /// `fz_contains_rect`.
    pub fn contains(&self, b: Rect) -> bool {
        if !self.is_valid() {
            return false;
        }
        if !b.is_valid() {
            return true;
        }
        self.x0 <= b.x0 && self.y0 <= b.y0 && self.x1 >= b.x1 && self.y1 >= b.y1
    }

    /// `fz_intersect_rect`.
    pub fn intersect(self, b: Rect) -> Rect {
        if b.is_infinite() {
            return self;
        }
        if self.is_infinite() {
            return b;
        }
        let mut a = self;
        if a.x0 < b.x0 {
            a.x0 = b.x0;
        }
        if a.y0 < b.y0 {
            a.y0 = b.y0;
        }
        if a.x1 > b.x1 {
            a.x1 = b.x1;
        }
        if a.y1 > b.y1 {
            a.y1 = b.y1;
        }
        a
    }

    /// `fz_transform_rect`: the bounding box of the transformed corners,
    /// an invalid rectangle staying invalid.
    pub fn transform(self, m: Matrix) -> Rect {
        let mut r = self;
        if r.is_infinite() {
            return r;
        }
        if m.b.abs() < f32::EPSILON && m.c.abs() < f32::EPSILON {
            if m.a < 0.0 {
                std::mem::swap(&mut r.x0, &mut r.x1);
            }
            if m.d < 0.0 {
                std::mem::swap(&mut r.y0, &mut r.y1);
            }
            let s = m.transform_xy(r.x0, r.y0);
            let t = m.transform_xy(r.x1, r.y1);
            return Rect::new(s.x, s.y, t.x, t.y);
        }
        if m.a.abs() < f32::EPSILON && m.d.abs() < f32::EPSILON {
            if m.b < 0.0 {
                std::mem::swap(&mut r.x0, &mut r.x1);
            }
            if m.c < 0.0 {
                std::mem::swap(&mut r.y0, &mut r.y1);
            }
            let s = m.transform_xy(r.x0, r.y0);
            let t = m.transform_xy(r.x1, r.y1);
            return Rect::new(s.x, s.y, t.x, t.y);
        }
        let invalid = r.x0 > r.x1 || r.y0 > r.y1;
        let corners = [
            m.transform_xy(r.x0, r.y0),
            m.transform_xy(r.x0, r.y1),
            m.transform_xy(r.x1, r.y1),
            m.transform_xy(r.x1, r.y0),
        ];
        let min = |f: fn(&Point) -> f32| corners.iter().map(f).fold(f32::INFINITY, min4);
        let max = |f: fn(&Point) -> f32| corners.iter().map(f).fold(f32::NEG_INFINITY, max4);
        let mut out = Rect::new(min(|p| p.x), min(|p| p.y), max(|p| p.x), max(|p| p.y));
        if invalid {
            std::mem::swap(&mut out.x0, &mut out.x1);
            std::mem::swap(&mut out.y0, &mut out.y1);
        }
        out
    }
}

/// One step of MIN4, as `fz_min`'s comparisons pick.
fn min4(acc: f32, v: f32) -> f32 {
    if v < acc { v } else { acc }
}

fn max4(acc: f32, v: f32) -> f32 {
    if v > acc { v } else { acc }
}

impl Matrix {
    pub const IDENTITY: Matrix = Matrix {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// `fz_scale`.
    pub fn scale(sx: f32, sy: f32) -> Matrix {
        Matrix {
            a: sx,
            d: sy,
            ..Matrix::ZERO
        }
    }

    /// `fz_translate`.
    pub fn translate(tx: f32, ty: f32) -> Matrix {
        Matrix {
            e: tx,
            f: ty,
            ..Matrix::IDENTITY
        }
    }

    const ZERO: Matrix = Matrix {
        a: 0.0,
        b: 0.0,
        c: 0.0,
        d: 0.0,
        e: 0.0,
        f: 0.0,
    };

    /// `fz_pre_rotate`: exact for multiples of 90 degrees.
    pub fn pre_rotate(self, theta: f32) -> Matrix {
        let mut theta = (f64::from(theta) % 360.0) as f32;
        if theta < 0.0 {
            theta += 360.0;
        }
        let mut m = self;
        if (0.0 - theta).abs() < f32::EPSILON {
        } else if (90.0 - theta).abs() < f32::EPSILON {
            let (a, b) = (m.a, m.b);
            m.a = m.c;
            m.b = m.d;
            m.c = -a;
            m.d = -b;
        } else if (180.0 - theta).abs() < f32::EPSILON {
            m.a = -m.a;
            m.b = -m.b;
            m.c = -m.c;
            m.d = -m.d;
        } else if (270.0 - theta).abs() < f32::EPSILON {
            let (a, b) = (m.a, m.b);
            m.a = -m.c;
            m.b = -m.d;
            m.c = a;
            m.d = b;
        } else {
            let s = (theta * std::f32::consts::PI / 180.0).sin();
            let c = (theta * std::f32::consts::PI / 180.0).cos();
            let (a, b) = (m.a, m.b);
            m.a = c * a + s * m.c;
            m.b = c * b + s * m.d;
            m.c = -s * a + c * m.c;
            m.d = -s * b + c * m.d;
        }
        m
    }

    /// `fz_concat`: `self`, then `two`.
    pub fn concat(self, two: Matrix) -> Matrix {
        let one = self;
        Matrix {
            a: one.a * two.a + one.b * two.c,
            b: one.a * two.b + one.b * two.d,
            c: one.c * two.a + one.d * two.c,
            d: one.c * two.b + one.d * two.d,
            e: one.e * two.a + one.f * two.c + two.e,
            f: one.e * two.b + one.f * two.d + two.f,
        }
    }

    /// `fz_invert_matrix`, in double precision; a matrix that cannot be
    /// inverted gives all zeros.
    pub fn invert(self) -> Matrix {
        let (sa, sb, sc, sd) = (
            f64::from(self.a),
            f64::from(self.b),
            f64::from(self.c),
            f64::from(self.d),
        );
        let det = sa * sd - sb * sc;
        if (-f64::EPSILON..=f64::EPSILON).contains(&det) {
            return Matrix::ZERO;
        }
        let det = 1.0 / det;
        let da = sd * det;
        let db = -sb * det;
        let dc = -sc * det;
        let dd = sa * det;
        let (se, sf) = (f64::from(self.e), f64::from(self.f));
        let de = -se * da - sf * dc;
        let df = -se * db - sf * dd;
        Matrix {
            a: da as f32,
            b: db as f32,
            c: dc as f32,
            d: dd as f32,
            e: de as f32,
            f: df as f32,
        }
    }

    /// `fz_pre_translate`.
    pub fn pre_translate(mut self, tx: f32, ty: f32) -> Matrix {
        self.e += tx * self.a + ty * self.c;
        self.f += tx * self.b + ty * self.d;
        self
    }

    /// `fz_pre_scale`.
    pub fn pre_scale(mut self, sx: f32, sy: f32) -> Matrix {
        self.a *= sx;
        self.b *= sx;
        self.c *= sy;
        self.d *= sy;
        self
    }

    /// `fz_transform_point_xy`.
    pub fn transform_xy(self, x: f32, y: f32) -> Point {
        Point {
            x: x * self.a + y * self.c + self.e,
            y: x * self.b + y * self.d + self.f,
        }
    }
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Point {
        Point { x, y }
    }

    /// `fz_transform_vector`: the point through the matrix, without its
    /// translation.
    pub fn transform_vector(self, m: Matrix) -> Point {
        Point {
            x: self.x * m.a + self.y * m.c,
            y: self.x * m.b + self.y * m.d,
        }
    }

    /// `fz_normalize_vector`.
    pub fn normalize(self) -> Point {
        let len = self.x * self.x + self.y * self.y;
        if len != 0.0 {
            let len = len.sqrt();
            Point {
                x: self.x / len,
                y: self.y / len,
            }
        } else {
            self
        }
    }
}

impl Matrix {
    /// `fz_matrix_expansion`.
    pub fn expansion(self) -> f32 {
        (self.a * self.d - self.b * self.c).abs().sqrt()
    }
}

/// `fz_quad`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quad {
    pub ul: Point,
    pub ur: Point,
    pub ll: Point,
    pub lr: Point,
}

impl Rect {
    /// `fz_invalid_rect`.
    pub const INVALID: Rect = Rect {
        x0: 0.0,
        y0: 0.0,
        x1: -1.0,
        y1: -1.0,
    };

    /// `fz_rect_from_quad`.
    pub fn from_quad(q: Quad) -> Rect {
        let pts = [q.ll, q.ul, q.ur, q.lr];
        if pts.iter().any(|p| p.x.is_nan() || p.y.is_nan()) {
            return Rect::INVALID;
        }
        if pts.iter().all(|p| p.x.is_infinite() && p.y.is_infinite()) {
            let inf = |a: Point, b: Point, c: Point, d: Point| {
                a.x < 0.0
                    && a.y < 0.0
                    && b.x < 0.0
                    && b.y > 0.0
                    && c.x > 0.0
                    && c.y > 0.0
                    && d.x > 0.0
                    && d.y < 0.0
            };
            if inf(q.ll, q.ul, q.ur, q.lr)
                || inf(q.ul, q.ur, q.lr, q.ll)
                || inf(q.ur, q.lr, q.ll, q.ul)
                || inf(q.lr, q.ll, q.ul, q.ur)
                || inf(q.ll, q.lr, q.ur, q.ul)
                || inf(q.lr, q.ur, q.ul, q.ll)
                || inf(q.ur, q.ul, q.ll, q.lr)
                || inf(q.ul, q.ll, q.lr, q.ur)
            {
                return Rect::INFINITE;
            }
        }
        let min4 = |a: f32, b: f32, c: f32, d: f32| a.min(b).min(c).min(d);
        let max4 = |a: f32, b: f32, c: f32, d: f32| a.max(b).max(c).max(d);
        Rect {
            x0: min4(q.ll.x, q.lr.x, q.ul.x, q.ur.x),
            y0: min4(q.ll.y, q.lr.y, q.ul.y, q.ur.y),
            x1: max4(q.ll.x, q.lr.x, q.ul.x, q.ur.x),
            y1: max4(q.ll.y, q.lr.y, q.ul.y, q.ur.y),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rotated_page_maps_back_and_forth() {
        // A 200 by 100 page turned 90 degrees, as pdf_page_transform makes
        // it: flip, rotate, then move the crop box's corner to the origin.
        let m = Matrix::scale(1.0, -1.0).pre_rotate(-90.0);
        let corner = Rect::new(0.0, 0.0, 200.0, 100.0).transform(m);
        let m = m.concat(Matrix::translate(-corner.x0, -corner.y0));
        let r = Rect::new(10.0, 20.0, 30.0, 40.0).transform(m);
        assert_eq!(r, Rect::new(20.0, 10.0, 40.0, 30.0));
        assert_eq!(r.transform(m.invert()), Rect::new(10.0, 20.0, 30.0, 40.0));
        assert_eq!(Matrix::scale(0.0, 1.0).invert(), Matrix::ZERO);
    }

    #[test]
    fn rects_intersect_unless_infinite() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert_eq!(
            a.intersect(Rect::new(5.0, -5.0, 20.0, 5.0)),
            Rect::new(5.0, 0.0, 10.0, 5.0)
        );
        let inf = Rect::new(MIN_INF, MIN_INF, MAX_INF, MAX_INF);
        assert_eq!(a.intersect(inf), a);
        assert!(Rect::EMPTY.is_empty());
    }
}
