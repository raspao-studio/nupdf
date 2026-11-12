// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/path.c and source/fitz/geometry.c
// (fz_is_rectilinear, fz_matrix_max_expansion),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Paths as MuPDF builds them (`fz_path`).
//!
//! Construction folds segments the way MuPDF does: a move after a move
//! replaces it, a line to the current point after anything but a move is
//! dropped, horizontal and vertical lines and degenerate curves get their
//! own commands, and a rectangle after a move replaces the move. Walking
//! expands those back as MuPDF's walker does, and the bounds come from the
//! same walk, with the rectilinear fast path MuPDF takes.

use crate::geometry::{Matrix, Point, Rect};

/// `fz_path_item_kind`. A command and its closing variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Move,
    Line,
    Degen,
    Curve,
    CurveV,
    CurveY,
    Horiz,
    Vert,
    Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Item {
    cmd: Cmd,
    close: bool,
}

/// `fz_linecap`, `fz_linejoin` and the rest of `fz_stroke_state`.
#[derive(Debug, Clone, PartialEq)]
pub struct StrokeState {
    pub start_cap: i32,
    pub dash_cap: i32,
    pub end_cap: i32,
    pub linejoin: i32,
    pub linewidth: f32,
    pub miterlimit: f32,
    pub dash_phase: f32,
    pub dash: Vec<f32>,
}

impl Default for StrokeState {
    /// `fz_default_stroke_state`.
    fn default() -> Self {
        StrokeState {
            start_cap: 0,
            dash_cap: 0,
            end_cap: 0,
            linejoin: 0,
            linewidth: 1.0,
            miterlimit: 10.0,
            dash_phase: 0.0,
            dash: Vec::new(),
        }
    }
}

/// `FZ_LINEJOIN_MITER` and `FZ_LINEJOIN_MITER_XPS`.
/// `FZ_LINECAP_ROUND`.
pub const LINECAP_ROUND: i32 = 1;
pub const LINEJOIN_MITER: i32 = 0;
const LINEJOIN_MITER_XPS: i32 = 3;

/// A path being built, then handed to a device.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    items: Vec<Item>,
    coords: Vec<f32>,
    current: Point,
    begin: Point,
    rectilinear_bounds: Rect,
    only_right_angles: bool,
}

impl Default for Path {
    fn default() -> Self {
        Path::new()
    }
}

/// What walking a path calls (`fz_path_walker` with only the four
/// callbacks MuPDF's script glue and its bounding walker supply).
pub trait Walker {
    fn move_to(&mut self, x: f32, y: f32);
    fn line_to(&mut self, x: f32, y: f32);
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32);
    fn close_path(&mut self);
}

/// `fz_path_walker` with every callback: curves with an implied control
/// point and rectangles are seen as they were built.
pub trait FullWalker {
    fn move_to(&mut self, x: f32, y: f32);
    fn line_to(&mut self, x: f32, y: f32);
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32);
    fn close_path(&mut self);
    fn curve_to_v(&mut self, x2: f32, y2: f32, x3: f32, y3: f32);
    fn curve_to_y(&mut self, x1: f32, y1: f32, x3: f32, y3: f32);
    fn rect_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32);
}

impl Path {
    /// `fz_new_path`.
    pub fn new() -> Path {
        Path {
            items: Vec::new(),
            coords: Vec::new(),
            current: Point { x: 0.0, y: 0.0 },
            begin: Point { x: 0.0, y: 0.0 },
            rectilinear_bounds: Rect::EMPTY,
            only_right_angles: true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn last(&self) -> Option<Item> {
        self.items.last().copied()
    }

    fn last_is(&self, cmd: Cmd) -> bool {
        self.last() == Some(Item { cmd, close: false })
    }

    fn push(&mut self, cmd: Cmd) {
        self.items.push(Item { cmd, close: false });
    }

    fn push_coord(&mut self, x: f32, y: f32) {
        self.coords.extend([x, y]);
        self.current = Point { x, y };
    }

    fn push_ord(&mut self, v: f32, is_x: bool) {
        self.coords.push(v);
        let b = &mut self.rectilinear_bounds;
        if is_x {
            self.current.x = v;
            if v < b.x0 {
                b.x0 = v;
            }
            if b.x1 < v {
                b.x1 = v;
            }
        } else {
            self.current.y = v;
            if v < b.y0 {
                b.y0 = v;
            }
            if b.y1 < v {
                b.y1 = v;
            }
        }
    }

    fn update_bounds(&mut self, x: f32, y: f32) {
        let b = &mut self.rectilinear_bounds;
        if x < b.x0 {
            b.x0 = x;
        }
        if b.x1 < x {
            b.x1 = x;
        }
        if y < b.y0 {
            b.y0 = y;
        }
        if b.y1 < y {
            b.y1 = y;
        }
    }

    /// `fz_moveto`.
    pub fn move_to(&mut self, x: f32, y: f32) {
        if self.last_is(Cmd::Move) {
            let n = self.coords.len();
            self.coords[n - 2] = x;
            self.coords[n - 1] = y;
            self.current = Point { x, y };
            self.begin = self.current;
            return;
        }
        self.push(Cmd::Move);
        self.push_coord(x, y);
        self.begin = self.current;
    }

    /// `fz_lineto`. Returns false when there is no current point (MuPDF
    /// warns "lineto with no current point").
    pub fn line_to(&mut self, x: f32, y: f32) -> bool {
        let Point { x: x0, y: y0 } = self.current;
        if self.items.is_empty() {
            return false;
        }
        if self.last_is(Cmd::Move) {
            self.update_bounds(x0, y0);
        } else if x0 == x && y0 == y {
            return true;
        }
        if x0 == x {
            if y0 == y {
                if !self.last_is(Cmd::Move) {
                    return true;
                }
                self.push(Cmd::Degen);
            } else {
                self.push(Cmd::Vert);
                self.push_ord(y, false);
            }
        } else if y0 == y {
            self.push(Cmd::Horiz);
            self.push_ord(x, true);
        } else {
            self.push(Cmd::Line);
            self.push_coord(x, y);
            self.update_bounds(x, y);
            self.only_right_angles = false;
        }
        true
    }

    /// `fz_curveto`. False when there is no current point.
    pub fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) -> bool {
        let Point { x: x0, y: y0 } = self.current;
        if self.items.is_empty() {
            return false;
        }
        if self.last_is(Cmd::Move) {
            self.update_bounds(x0, y0);
        }
        if x0 == x1 && y0 == y1 {
            if x2 == x3 && y2 == y3 {
                if x1 == x2 && y1 == y2 && !self.last_is(Cmd::Move) {
                    return true;
                }
                self.line_to(x3, y3);
            } else if x1 == x2 && y1 == y2 {
                self.line_to(x3, y3);
            } else {
                self.curve_to_v(x2, y2, x3, y3);
            }
            return true;
        } else if x2 == x3 && y2 == y3 {
            if x1 == x2 && y1 == y2 {
                self.line_to(x3, y3);
            } else {
                self.curve_to_y(x1, y1, x3, y3);
            }
            return true;
        }
        self.push(Cmd::Curve);
        for (x, y) in [(x1, y1), (x2, y2), (x3, y3)] {
            self.push_coord(x, y);
            self.update_bounds(x, y);
        }
        self.only_right_angles = false;
        true
    }

    /// `fz_curvetov`. As in MuPDF, a degenerate curve adds a line and then
    /// the curve too.
    pub fn curve_to_v(&mut self, x2: f32, y2: f32, x3: f32, y3: f32) -> bool {
        let Point { x: x0, y: y0 } = self.current;
        if self.items.is_empty() {
            return false;
        }
        if self.last_is(Cmd::Move) {
            self.update_bounds(x0, y0);
        }
        if x2 == x3 && y2 == y3 {
            if x0 == x2 && y0 == y2 && !self.last_is(Cmd::Move) {
                return true;
            }
            self.line_to(x3, y3);
        } else if x0 == x2 && y0 == y2 {
            self.line_to(x3, y3);
        }
        self.push(Cmd::CurveV);
        for (x, y) in [(x2, y2), (x3, y3)] {
            self.push_coord(x, y);
            self.update_bounds(x, y);
        }
        self.only_right_angles = false;
        true
    }

    /// `fz_curvetoy`, with the same quirk as `curve_to_v`.
    pub fn curve_to_y(&mut self, x1: f32, y1: f32, x3: f32, y3: f32) -> bool {
        let Point { x: x0, y: y0 } = self.current;
        if self.items.is_empty() {
            return false;
        }
        if self.last_is(Cmd::Move) {
            self.update_bounds(x0, y0);
        }
        if x1 == x3 && y1 == y3 {
            if x0 == x1 && y0 == y1 && !self.last_is(Cmd::Move) {
                return true;
            }
            self.line_to(x3, y3);
        }
        self.push(Cmd::CurveY);
        for (x, y) in [(x1, y1), (x3, y3)] {
            self.push_coord(x, y);
            self.update_bounds(x, y);
        }
        self.only_right_angles = false;
        true
    }

    /// `fz_closepath`. False when there is no current point.
    pub fn close_path(&mut self) -> bool {
        let Some(last) = self.last() else {
            return false;
        };
        if self.begin.x != self.current.x && self.begin.y != self.current.y {
            self.only_right_angles = false;
        }
        if last
            == (Item {
                cmd: Cmd::Move,
                close: false,
            })
        {
            let Point { x, y } = self.current;
            self.update_bounds(x, y);
        }
        // A rectangle implies a close, and a close after a close does
        // nothing.
        if last.cmd == Cmd::Rect || last.close {
            return true;
        }
        if let Some(item) = self.items.last_mut() {
            item.close = true;
        }
        self.current = self.begin;
        true
    }

    /// `fz_rectto`.
    pub fn rect_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32) {
        if self.last_is(Cmd::Move) {
            self.coords.truncate(self.coords.len() - 2);
            self.items.pop();
        }
        self.push(Cmd::Rect);
        self.push_coord(x1, y1);
        self.update_bounds(x1, y1);
        self.push_coord(x2, y2);
        self.update_bounds(x2, y2);
        self.current = self.begin;
    }

    /// `fz_walk_path` with a walker of moves, lines, curves and closes.
    pub fn walk(&self, w: &mut dyn Walker) {
        let c = &self.coords;
        let (mut x, mut y, mut sx, mut sy) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let mut k = 0;
        for item in &self.items {
            match item.cmd {
                Cmd::Curve => {
                    w.curve_to(c[k], c[k + 1], c[k + 2], c[k + 3], c[k + 4], c[k + 5]);
                    (x, y) = (c[k + 4], c[k + 5]);
                    k += 6;
                }
                Cmd::CurveV => {
                    w.curve_to(x, y, c[k], c[k + 1], c[k + 2], c[k + 3]);
                    (x, y) = (c[k + 2], c[k + 3]);
                    k += 4;
                }
                Cmd::CurveY => {
                    w.curve_to(c[k], c[k + 1], c[k + 2], c[k + 3], c[k + 2], c[k + 3]);
                    (x, y) = (c[k + 2], c[k + 3]);
                    k += 4;
                }
                Cmd::Move => {
                    (x, y) = (c[k], c[k + 1]);
                    w.move_to(x, y);
                    k += 2;
                    (sx, sy) = (x, y);
                }
                Cmd::Line => {
                    (x, y) = (c[k], c[k + 1]);
                    w.line_to(x, y);
                    k += 2;
                }
                Cmd::Horiz => {
                    x = c[k];
                    w.line_to(x, y);
                    k += 1;
                }
                Cmd::Vert => {
                    y = c[k];
                    w.line_to(x, y);
                    k += 1;
                }
                Cmd::Degen => w.line_to(x, y),
                Cmd::Rect => {
                    (x, y) = (c[k], c[k + 1]);
                    w.move_to(x, y);
                    w.line_to(c[k + 2], c[k + 1]);
                    w.line_to(c[k + 2], c[k + 3]);
                    w.line_to(c[k], c[k + 3]);
                    w.close_path();
                    (sx, sy) = (x, y);
                    k += 4;
                }
            }
            if item.close {
                w.close_path();
                (x, y) = (sx, sy);
            }
        }
    }

    /// `fz_walk_path` with a walker that has every callback.
    pub fn walk_full(&self, w: &mut dyn FullWalker) {
        let c = &self.coords;
        let (mut x, mut y) = (0.0f32, 0.0f32);
        let mut k = 0;
        for item in &self.items {
            match item.cmd {
                Cmd::Curve => {
                    w.curve_to(c[k], c[k + 1], c[k + 2], c[k + 3], c[k + 4], c[k + 5]);
                    (x, y) = (c[k + 4], c[k + 5]);
                    k += 6;
                }
                Cmd::CurveV => {
                    w.curve_to_v(c[k], c[k + 1], c[k + 2], c[k + 3]);
                    (x, y) = (c[k + 2], c[k + 3]);
                    k += 4;
                }
                Cmd::CurveY => {
                    w.curve_to_y(c[k], c[k + 1], c[k + 2], c[k + 3]);
                    (x, y) = (c[k + 2], c[k + 3]);
                    k += 4;
                }
                Cmd::Move => {
                    (x, y) = (c[k], c[k + 1]);
                    w.move_to(x, y);
                    k += 2;
                }
                Cmd::Line => {
                    (x, y) = (c[k], c[k + 1]);
                    w.line_to(x, y);
                    k += 2;
                }
                Cmd::Horiz => {
                    x = c[k];
                    w.line_to(x, y);
                    k += 1;
                }
                Cmd::Vert => {
                    y = c[k];
                    w.line_to(x, y);
                    k += 1;
                }
                Cmd::Degen => w.line_to(x, y),
                Cmd::Rect => {
                    w.rect_to(c[k], c[k + 1], c[k + 2], c[k + 3]);
                    (x, y) = (c[k], c[k + 1]);
                    k += 4;
                }
            }
            if item.close {
                w.close_path();
            }
        }
        let _ = (x, y);
    }

    /// `fz_bound_path`: with a stroke state, widened for the stroke.
    pub fn bound(&self, stroke: Option<&StrokeState>, ctm: Matrix) -> Rect {
        if is_rectilinear(ctm) {
            let r = self.rectilinear_bounds.transform(ctm);
            return match stroke {
                Some(s) => adjust_rect_for_stroke(r, s, ctm, self.only_right_angles),
                None => r,
            };
        }
        let mut b = Bounder {
            ctm,
            rect: Rect::EMPTY,
            mv: Point { x: 0.0, y: 0.0 },
            trailing_move: false,
            first: true,
            only_right_angles: true,
            prev: Point { x: 0.0, y: 0.0 },
        };
        self.walk(&mut b);
        match stroke {
            Some(s) if !b.first => adjust_rect_for_stroke(b.rect, s, ctm, b.only_right_angles),
            _ => b.rect,
        }
    }
}

/// `fz_is_rectilinear`.
pub fn is_rectilinear(m: Matrix) -> bool {
    (m.b.abs() < f32::EPSILON && m.c.abs() < f32::EPSILON)
        || (m.a.abs() < f32::EPSILON && m.d.abs() < f32::EPSILON)
}

/// `fz_matrix_max_expansion`.
pub fn max_expansion(m: Matrix) -> f32 {
    let mut max = m.a.abs();
    for v in [m.b, m.c, m.d] {
        if max < v.abs() {
            max = v.abs();
        }
    }
    max
}

/// `adjust_rect_for_stroke`.
fn adjust_rect_for_stroke(mut r: Rect, s: &StrokeState, ctm: Matrix, no_mitre: bool) -> Rect {
    let mut expand = s.linewidth / 2.0;
    if expand == 0.0 {
        expand = 0.5;
    }
    if r.x1 == r.x0 || r.y1 == r.y0 {
        // Mitring cannot apply.
    } else if !no_mitre && s.linejoin == LINEJOIN_MITER && s.miterlimit > 0.5 {
        expand *= s.miterlimit * 2.0;
    } else if !no_mitre && s.linejoin == LINEJOIN_MITER_XPS && s.miterlimit > 1.0 {
        expand *= s.miterlimit;
    }
    expand *= max_expansion(ctm);
    r.x0 -= expand;
    r.y0 -= expand;
    r.x1 += expand;
    r.y1 += expand;
    r
}

/// `fz_adjust_rect_for_stroke`.
pub fn adjust_for_stroke(r: Rect, s: &StrokeState, ctm: Matrix) -> Rect {
    adjust_rect_for_stroke(r, s, ctm, false)
}

/// The bounding walker of `fz_bound_path`.
struct Bounder {
    ctm: Matrix,
    rect: Rect,
    mv: Point,
    trailing_move: bool,
    first: bool,
    only_right_angles: bool,
    prev: Point,
}

fn eq0(x: f32) -> bool {
    f64::from(x) >= -0.001 && f64::from(x) <= 0.001
}

fn expand(r: &mut Rect, p: Point) {
    if p.x < r.x0 {
        r.x0 = p.x;
    }
    if p.y < r.y0 {
        r.y0 = p.y;
    }
    if p.x > r.x1 {
        r.x1 = p.x;
    }
    if p.y > r.y1 {
        r.y1 = p.y;
    }
}

impl Bounder {
    fn start(&mut self, p: Point) {
        if self.first {
            self.rect = Rect::new(p.x, p.y, p.x, p.y);
            self.first = false;
        } else {
            expand(&mut self.rect, p);
        }
    }

    fn end_trailing_move(&mut self) {
        if self.trailing_move {
            self.trailing_move = false;
            expand(&mut self.rect, self.mv);
        }
    }
}

impl Walker for Bounder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.mv = self.ctm.transform_xy(x, y);
        self.prev = self.mv;
        self.trailing_move = true;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.ctm.transform_xy(x, y);
        self.start(p);
        self.end_trailing_move();
        if self.only_right_angles && !eq0(self.prev.x - p.x) && !eq0(self.prev.y - p.y) {
            self.only_right_angles = false;
        }
        self.prev = p;
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) {
        let p = self.ctm.transform_xy(x1, y1);
        self.start(p);
        expand(&mut self.rect, self.ctm.transform_xy(x2, y2));
        expand(&mut self.rect, self.ctm.transform_xy(x3, y3));
        self.end_trailing_move();
        self.only_right_angles = false;
        self.prev = p;
    }

    fn close_path(&mut self) {
        if self.only_right_angles && !eq0(self.prev.x - self.mv.x) && !eq0(self.prev.y - self.mv.y)
        {
            self.only_right_angles = false;
        }
        self.prev = self.mv;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Log(Vec<String>);

    impl Walker for Log {
        fn move_to(&mut self, x: f32, y: f32) {
            self.0.push(format!("m {x} {y}"));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.0.push(format!("l {x} {y}"));
        }
        fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) {
            self.0.push(format!("c {x1} {y1} {x2} {y2} {x3} {y3}"));
        }
        fn close_path(&mut self) {
            self.0.push("h".into());
        }
    }

    fn walk(p: &Path) -> Vec<String> {
        let mut log = Log::default();
        p.walk(&mut log);
        log.0
    }

    #[test]
    fn construction_folds_as_mupdf_does() {
        let mut p = Path::new();
        p.move_to(1.0, 1.0);
        p.move_to(2.0, 2.0);
        p.line_to(5.0, 2.0);
        p.line_to(5.0, 2.0);
        p.line_to(5.0, 6.0);
        p.close_path();
        p.close_path();
        assert_eq!(walk(&p), ["m 2 2", "l 5 2", "l 5 6", "h"]);
        // A rectangle after a move replaces it, and implies a close.
        let mut p = Path::new();
        p.move_to(9.0, 9.0);
        p.rect_to(0.0, 0.0, 3.0, 4.0);
        p.close_path();
        assert_eq!(walk(&p), ["m 0 0", "l 3 0", "l 3 4", "l 0 4", "h"]);
        // A curve whose first control point is the current point becomes a
        // v curve; walking expands it back.
        let mut p = Path::new();
        p.move_to(0.0, 0.0);
        p.curve_to(0.0, 0.0, 1.0, 2.0, 3.0, 3.0);
        assert_eq!(walk(&p), ["m 0 0", "c 0 0 1 2 3 3"]);
        // MuPDF's v curve to the current point adds a line, then the curve.
        let mut p = Path::new();
        p.move_to(0.0, 0.0);
        p.line_to(4.0, 4.0);
        p.curve_to_v(4.0, 4.0, 6.0, 1.0);
        assert_eq!(walk(&p), ["m 0 0", "l 4 4", "l 6 1", "c 6 1 4 4 6 1"]);
        // No current point.
        assert!(!Path::new().line_to(1.0, 1.0));
    }

    #[test]
    fn bounds_follow_the_rectilinear_fast_path_and_the_walk() {
        let mut p = Path::new();
        p.move_to(0.0, 0.0);
        p.line_to(10.0, 0.0);
        p.line_to(10.0, 5.0);
        assert_eq!(
            p.bound(None, Matrix::IDENTITY),
            Rect::new(0.0, 0.0, 10.0, 5.0)
        );
        let stroke = StrokeState {
            linewidth: 2.0,
            ..StrokeState::default()
        };
        // Only right angles: no mitre.
        assert_eq!(
            p.bound(Some(&stroke), Matrix::IDENTITY),
            Rect::new(-1.0, -1.0, 11.0, 6.0)
        );
        // A lone move bounds to nothing.
        let mut p = Path::new();
        p.move_to(3.0, 3.0);
        assert!(p.bound(None, Matrix::IDENTITY).is_empty());
        let rot = Matrix {
            a: 0.6,
            b: 0.8,
            c: -0.8,
            d: 0.6,
            e: 0.0,
            f: 0.0,
        };
        assert!(p.bound(None, rot).is_empty());
    }
}
