// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-appearance.c and
// source/pdf/annotation-icons.h, Copyright (C) 2004-2026 Artifex Software, Inc.
// The icons are from open-iconic, Copyright (c) 2014 Waybury, and jam,
// Copyright (c) 2017 Michael Amprimo, both MIT licensed.

//! Text markup, redactions, carets and icons.

use super::{Writer, len, pt, real_at};
use crate::geometry::{Point, Rect};
use crate::object::Object;

/// Most line segments a Squiggly annotation's waves may take. MuPDF has no
/// bound and loops forever on a quad of no height.
const MAX_SQUIGGLES: usize = 1 << 20;

// The points of a quad as /QuadPoints orders them, which is not the
// specification's order but the one Acrobat uses.
const UL: usize = 0;
const UR: usize = 1;
const LL: usize = 2;
const LR: usize = 3;

/// `fz_min`.
fn min(a: f32, b: f32) -> f32 {
    if a < b { a } else { b }
}

/// `fz_max`.
fn max(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}

/// `extract_quad`: the quad of /QuadPoints from index `i`, read as is
/// (not through the page transform), and the length of its left side.
fn extract_quad(w: &Writer, qp: &Object, i: usize) -> ([Point; 4], f32) {
    let r = |k| real_at(w.doc, qp, i + k);
    let quad = [
        pt(r(0), r(1)),
        pt(r(2), r(3)),
        pt(r(4), r(5)),
        pt(r(6), r(7)),
    ];
    let dx = quad[UL].x - quad[LL].x;
    let dy = quad[UL].y - quad[LL].y;
    (quad, dx.mul_add(dx, dy * dy).sqrt())
}

/// `union_quad`: `rect` grown to cover the quad's box expanded by `lw`.
fn union_quad(rect: Rect, quad: &[Point; 4], lw: f32) -> Rect {
    let qbox = Rect::new(
        min(min(quad[0].x, quad[1].x), min(quad[2].x, quad[3].x)),
        min(min(quad[0].y, quad[1].y), min(quad[2].y, quad[3].y)),
        max(max(quad[0].x, quad[1].x), max(quad[2].x, quad[3].x)),
        max(max(quad[0].y, quad[1].y), max(quad[2].y, quad[3].y)),
    );
    rect.union(qbox.expand(lw))
}

/// `lerp_point`.
fn lerp_point(a: Point, b: Point, t: f32) -> Point {
    pt(t.mul_add(b.x - a.x, a.x), t.mul_add(b.y - a.y, a.y))
}

/// The annotation's /QuadPoints and how many numbers it has.
fn quad_points(w: &Writer) -> (Object, usize) {
    let qp = w.get(b"QuadPoints");
    let n = len(w.doc, &qp);
    (qp, n)
}

/// `pdf_write_highlight_appearance`.
pub(super) fn highlight(w: &mut Writer, rect: &mut Rect) -> Result<(), String> {
    *rect = Rect::EMPTY;
    w.opacity_blend_mode(true);
    w.fill_color();
    let (qp, n) = quad_points(w);
    for i in (0..n).step_by(8) {
        let (quad, h) = extract_quad(w, &qp, i);
        // A magic number that matches Adobe's appearance.
        let m = h / 4.2425;
        let dx = quad[LR].x - quad[LL].x;
        let dy = quad[LR].y - quad[LL].y;
        let vn = dx.mul_add(dx, dy * dy).sqrt();
        let v = pt(dx * m / vn, dy * m / vn);

        let mut mquad = [pt(0.0, 0.0); 4];
        mquad[LL] = pt(quad[LL].x - v.x - v.y, quad[LL].y - v.y + v.x);
        mquad[UL] = pt(quad[UL].x - v.x + v.y, quad[UL].y - v.y - v.x);
        mquad[LR] = pt(quad[LR].x + v.x - v.y, quad[LR].y + v.y + v.x);
        mquad[UR] = pt(quad[UR].x + v.x + v.y, quad[UR].y + v.y - v.x);

        w.op(&[quad[LL].x, quad[LL].y], "m");
        w.op(
            &[
                mquad[LL].x,
                mquad[LL].y,
                mquad[UL].x,
                mquad[UL].y,
                quad[UL].x,
                quad[UL].y,
            ],
            "c",
        );
        w.op(&[quad[UR].x, quad[UR].y], "l");
        w.op(
            &[
                mquad[UR].x,
                mquad[UR].y,
                mquad[LR].x,
                mquad[LR].y,
                quad[LR].x,
                quad[LR].y,
            ],
            "c",
        );
        w.s("f\n");

        *rect = union_quad(*rect, &quad, h / 16.0);
        *rect = union_quad(*rect, &mquad, 0.0);
    }
    Ok(())
}

/// A line across each quad at `t` of its height from the bottom, 1/16 of
/// its height thick, as Acrobat draws underlines and strike-outs.
fn line_through(w: &mut Writer, rect: &mut Rect, qp: &Object, n: usize, t: f32) {
    for i in (0..n).step_by(8) {
        let (quad, h) = extract_quad(w, qp, i);
        let a = lerp_point(quad[LL], quad[UL], t);
        let b = lerp_point(quad[LR], quad[UR], t);
        w.op(&[h / 16.0], "w");
        w.op(&[a.x, a.y], "m");
        w.op(&[b.x, b.y], "l");
        w.s("S\n");
        *rect = union_quad(*rect, &quad, h / 16.0);
    }
}

/// `pdf_write_underline_appearance`.
pub(super) fn underline(w: &mut Writer, rect: &mut Rect) -> Result<(), String> {
    *rect = Rect::EMPTY;
    w.opacity();
    w.stroke_color();
    let (qp, n) = quad_points(w);
    line_through(w, rect, &qp, n, 1.0 / 7.0);
    Ok(())
}

/// `pdf_write_strike_out_appearance`: without quads the /Rect is kept.
pub(super) fn strike_out(w: &mut Writer, rect: &mut Rect) -> Result<(), String> {
    w.opacity();
    w.stroke_color();
    let (qp, n) = quad_points(w);
    if n > 0 {
        *rect = Rect::EMPTY;
        line_through(w, rect, &qp, n, 3.0 / 7.0);
    }
    Ok(())
}

/// `pdf_write_squiggly_appearance`.
pub(super) fn squiggly(w: &mut Writer, rect: &mut Rect) -> Result<(), String> {
    *rect = Rect::EMPTY;
    w.opacity();
    w.stroke_color();
    let (qp, n) = quad_points(w);
    let mut segments = 0;
    for i in (0..n).step_by(8) {
        let mut up = true;
        let (quad, h) = extract_quad(w, &qp, i);
        let v = pt(quad[LR].x - quad[LL].x, quad[LR].y - quad[LL].y);
        let width = v.x.mul_add(v.x, v.y * v.y).sqrt();
        let mut x = 0.0f32;

        w.op(&[h / 16.0], "w");
        w.op(&[quad[LL].x, quad[LL].y], "m");
        while x < width {
            segments += 1;
            if segments > MAX_SQUIGGLES {
                return Err("too many squiggles".into());
            }
            x += h / 7.0;
            let a = lerp_point(quad[LL], quad[LR], x / width);
            if up {
                let b = lerp_point(quad[UL], quad[UR], x / width);
                let c = lerp_point(a, b, 1.0 / 7.0);
                w.op(&[c.x, c.y], "l");
            } else {
                w.op(&[a.x, a.y], "l");
            }
            up = !up;
        }
        w.s("S\n");
        *rect = union_quad(*rect, &quad, h / 16.0);
    }
    Ok(())
}

/// `pdf_write_redact_appearance`: a red outline around each quad, or inside
/// the /Rect when there are none.
pub(super) fn redact(w: &mut Writer, rect: &mut Rect) -> Result<(), String> {
    w.opacity();
    w.s("1 0 0 RG\n");
    let (qp, n) = quad_points(w);
    if n > 0 {
        *rect = Rect::EMPTY;
        for i in (0..n).step_by(8) {
            let (quad, _) = extract_quad(w, &qp, i);
            w.op(&[quad[LL].x, quad[LL].y], "m");
            w.op(&[quad[LR].x, quad[LR].y], "l");
            w.op(&[quad[UR].x, quad[UR].y], "l");
            w.op(&[quad[UL].x, quad[UL].y], "l");
            w.s("s\n");
            *rect = union_quad(*rect, &quad, 1.0);
        }
    } else {
        w.op(&[rect.x0 + 1.0, rect.y0 + 1.0], "m");
        w.op(&[rect.x1 - 1.0, rect.y0 + 1.0], "l");
        w.op(&[rect.x1 - 1.0, rect.y1 - 1.0], "l");
        w.op(&[rect.x0 + 1.0, rect.y1 - 1.0], "l");
        w.s("s\n");
    }
    Ok(())
}

/// `pdf_write_caret_appearance`: a 20 by 14 caret centred on the /Rect;
/// the bounding box.
pub(super) fn caret(w: &mut Writer, rect: &mut Rect) -> Result<Rect, String> {
    let xc = (rect.x0 + rect.x1) / 2.0;
    let yc = (rect.y0 + rect.y1) / 2.0;

    w.opacity();
    w.fill_color();

    w.s("0 0 m\n");
    w.s("10 0 10 7 10 14 c\n");
    w.s("10 7 10 0 20 0 c\n");
    w.s("f\n");

    *rect = Rect::new(xc - 10.0, yc - 7.0, xc + 10.0, yc + 7.0);
    Ok(Rect::new(0.0, 0.0, 20.0, 14.0))
}

/// `pdf_annot_icon_name`: /Name, each type's default when the key is
/// absent or a direct null, and nothing when it is anything else but a
/// name, a reference to a null included.
fn icon_name(w: &Writer) -> Result<Vec<u8>, String> {
    let default: &[u8] = match w.subtype() {
        "Text" => b"Note",
        "Stamp" => b"",
        "FileAttachment" => b"PushPin",
        "Sound" => b"Speaker",
        _ => return Err(w.no_property("Name")),
    };
    let annot = w.doc.resolve(&w.annot);
    Ok(match annot.as_dict().and_then(|d| d.get(b"Name")) {
        None | Some(Object::Null) => default.to_vec(),
        Some(v) => w.doc.resolve(v).as_name().unwrap_or(b"").to_vec(),
    })
}

/// `pdf_write_icon_appearance`: the icon /Name names in a box at the top
/// left of the /Rect, 10 to 20 wide; the bounding box.
pub(super) fn icon(w: &mut Writer, rect: &mut Rect) -> Result<Rect, String> {
    let size = max(10.0, min(20.0, min(rect.y1 - rect.y0, rect.x1 - rect.x0)));
    let x = rect.x0;
    let y = rect.y1 - size;

    w.opacity();
    if !w.fill_color() {
        w.s("1 g\n");
    }
    w.s("1 w\n0.5 0.5 15 15 re\nb\n");
    w.s("1 0 0 -1 4 12 cm\n");
    w.s(if w.is_dark_fill_color() {
        "1 g\n"
    } else {
        "0 g\n"
    });

    let name = icon_name(w)?;
    w.s(match name.as_slice() {
        // Text names.
        b"Comment" => ICON_COMMENT,
        b"Key" => ICON_KEY,
        b"Note" => ICON_NOTE,
        b"Help" => ICON_HELP,
        b"NewParagraph" => ICON_NEW_PARAGRAPH,
        b"Paragraph" => ICON_PARAGRAPH,
        b"Insert" => ICON_INSERT,
        // FileAttachment names.
        b"Graph" => ICON_GRAPH,
        b"PushPin" => ICON_PUSH_PIN,
        b"Paperclip" => ICON_PAPERCLIP,
        b"Tag" => ICON_TAG,
        // Sound names.
        b"Speaker" => ICON_SPEAKER,
        b"Mic" => ICON_MIC,
        _ => ICON_STAR,
    });

    *rect = Rect::new(x, y, x + size, y + size);
    Ok(Rect::new(0.0, 0.0, 16.0, 16.0))
}

// The 8 by 8 icons of `annotation-icons.h`.

const ICON_COMMENT: &str = concat!(
    ".09 0 m\n",
    ".03 0 0 .04 0 .09 c\n",
    "0 5.9 l\n",
    "0 5.95 .04 6 .09 6 c\n",
    "6 6 l\n",
    "8 8 l\n",
    "8 .08 l\n",
    "8 .02 7.96 -.01 7.91 -.01 c\n",
    ".1 -.01 l\n",
    "h\n",
    "f\n",
);

const ICON_KEY: &str = concat!(
    "5.5 0 m\n",
    "4.12 0 3 1.12 3 2.5 c\n",
    "3 2.66 3 2.82 3.03 2.97 c\n",
    "0 6 l\n",
    "0 8 l\n",
    "3 8 l\n",
    "3 6 l\n",
    "5 6 l\n",
    "5 5 l\n",
    "5.03 4.97 l\n",
    "5.18 5 5.34 5 5.5 5 c\n",
    "6.88 5 8 3.88 8 2.5 c\n",
    "8 1.12 6.88 0 5.5 0 c\n",
    "h\n",
    "6 1 m\n",
    "6.55 1 7 1.45 7 2 c\n",
    "7 2.55 6.55 3 6 3 c\n",
    "5.45 3 5 2.55 5 2 c\n",
    "5 1.45 5.45 1 6 1 c\n",
    "h\n",
    "f\n",
);

const ICON_NOTE: &str = concat!(
    "0 0 8 1 re\n",
    "0 2 8 1 re\n",
    "0 4 8 1 re\n",
    "0 6 8 1 re\n",
    "f\n",
);

const ICON_HELP: &str = concat!(
    "1 0 0 1 1 0 cm\n",
    "2.47 0 m\n",
    "1.62 0 .99 .26 .59 .66 c\n",
    ".19 1.06 .05 1.56 0 1.94 c\n",
    "1 2.07 l\n",
    "1.04 1.82 1.12 1.57 1.31 1.38 c\n",
    "1.50 1.19 1.80 1 2.47 1.00 c\n",
    "3.13 1 3.49 1.16 3.69 1.34 c\n",
    "3.89 1.52 3.97 1.74 3.97 2 c\n",
    "3.97 2.83 3.63 3.06 3.13 3.50 c\n",
    "2.63 3.94 1.97 4.58 1.97 5.75 c\n",
    "1.97 6 l\n",
    "2.97 6 l\n",
    "2.97 5.75 l\n",
    "2.97 4.92 3.28 4.69 3.78 4.25 c\n",
    "4.28 3.81 4.97 3.17 4.97 2 c\n",
    "4.97 1.52 4.80 .98 4.38 .59 c\n",
    "3.95 .20 3.31 0 2.47 .00 c\n",
    "h\n",
    "1.97 7 m\n",
    "1.97 8 l\n",
    "2.97 8 l\n",
    "2.97 7 l\n",
    "1.97 7 l\n",
    "h\n",
    "f\n",
);

const ICON_GRAPH: &str = concat!(
    "7.03 0 m\n",
    "4 3 l\n",
    "3 2 l\n",
    "0 5.03 l\n",
    "1 6.03 l\n",
    "3 4 l\n",
    "4 5 l\n",
    "8 1 l\n",
    "7.03 0 l\n",
    "h\n",
    "0 7 m\n",
    "0 8 l\n",
    "8 8 l\n",
    "8 7 l\n",
    "0 7 l\n",
    "h\n",
    "f\n",
);

const ICON_PUSH_PIN: &str = concat!(
    "1.34 0 m\n",
    ".92 .04 .76 .64 1.1 .89 c\n",
    "1.34 1.08 1.65 .97 1.93 1 c\n",
    "2.08 .98 1.96 1.22 2 1.32 c\n",
    "2 1.88 2 2.44 2 3 c\n",
    "1.6 3.01 1.2 2.98 .8 3.02 c\n",
    ".35 3.11 -.01 3.54 0 4 c\n",
    "1 4 2 4 3 4 c\n",
    "3 5 3 6 3 7 c\n",
    "3.146 7.33 3.29 7.67 3.44 8 c\n",
    "3.62 7.66 3.83 7.32 4 6.98 c\n",
    "4 5.99 4 4.99 4 4 c\n",
    "5 4 6 4 7 4 c\n",
    "7.02 3.42 6.46 2.94 5.89 3 c\n",
    "5.6 3 5.3 3 5 3 c\n",
    "5 2.33 5 1.67 5 1 c\n",
    "5.30 .98 5.67 1.09 5.89 .81 c\n",
    "6.16 .5 5.89 -.038 5.48 0 c\n",
    "4.15 0 2.83 0 1.5 0 c\n",
    "h\n",
    "f\n",
);

const ICON_PAPERCLIP: &str = concat!(
    "5 0 m\n",
    "4.49 0 3.98 .21 3.59 .59 c\n",
    ".81 3.31 l\n",
    "-.26 4.38 -.26 6.11 .81 7.19 c\n",
    "1.88 8.26 3.61 8.26 4.69 7.19 c\n",
    "5.94 5.94 l\n",
    "5.25 5.25 l\n",
    "4.09 6.38 l\n",
    "4 6.51 l\n",
    "3.31 7.2 2.19 7.2 1.5 6.51 c\n",
    ".82 5.83 .84 4.73 1.5 4.04 c\n",
    "4.28 1.29 l\n",
    "4.67 .9 5.32 .9 5.72 1.29 c\n",
    "6.11 1.68 6.09 2.3 5.72 2.7 c\n",
    "3.22 5.17 l\n",
    "3.12 5.27 2.95 5.27 2.84 5.17 c\n",
    "2.74 5.07 2.74 4.9 2.84 4.79 c\n",
    "2.9 4.76 l\n",
    "3.81 3.82 l\n",
    "3.12 3.13 l\n",
    "2.15 4.1 l\n",
    "1.67 4.58 1.67 5.37 2.15 5.85 c\n",
    "2.63 6.33 3.42 6.34 3.9 5.85 c\n",
    "6.4 3.41 l\n",
    "7.18 2.63 7.18 1.37 6.4 .6 c\n",
    "6.01 .21 5.51 .01 4.99 .01 c\n",
    "h\n",
    "f\n",
);

const ICON_TAG: &str = concat!(
    "0 0 m\n",
    "0 3 l\n",
    "5 8 l\n",
    "8 5 l\n",
    "3 0 l\n",
    "0 0 l\n",
    "h\n",
    "2 1 m\n",
    "2.55 1 3 1.45 3 2 c\n",
    "3 2.55 2.55 3 2 3 c\n",
    "1.45 3 1 2.55 1 2 c\n",
    "1 1.45 1.45 1 2 1 c\n",
    "h\n",
    "f\n",
);

const ICON_SPEAKER: &str = concat!(
    "3.34 0 m\n",
    "2 2 l\n",
    "0 2 l\n",
    "0 6 l\n",
    "2 6 l\n",
    "3.34 8 l\n",
    "4 8 l\n",
    "4 0 l\n",
    "3.34 0 l\n",
    "h\n",
    "5 1 m\n",
    "5 2 l\n",
    "5.17 2 5.34 2.02 5.5 2.06 c\n",
    "6.36 2.28 7 3.06 7 4 c\n",
    "7 4.94 6.37 5.72 5.5 5.94 c\n",
    "5.34 5.98 5.17 6 5 6 c\n",
    "5 7 l\n",
    "5.25 7 5.48 6.96 5.72 6.91 c\n",
    "5.75 6.91 l\n",
    "7.05 6.58 8 5.4 8 4 c\n",
    "8 2.6 7.05 1.42 5.75 1.09 c\n",
    "5.52 1.03 5.26 1 5 1 c\n",
    "h\n",
    "5 3 m\n",
    "5 5 l\n",
    "5.09 5 5.18 4.99 5.25 4.97 c\n",
    "5.68 4.86 6 4.46 6 4 c\n",
    "6 3.54 5.69 3.14 5.25 3.03 c\n",
    "5.17 3.01 5.08 3 5 3 c\n",
    "h\n",
    "f\n",
);

const ICON_MIC: &str = concat!(
    "1 0 0 1 1 0 cm\n",
    "2.91 -.03 m\n",
    "2.49 .03 2.11 .34 2.02 .76 c\n",
    "1.97 1.12 2.01 1.48 2 1.84 c\n",
    "2.01 2.29 1.98 2.73 2.02 3.17 c\n",
    "2.1 3.72 2.68 4.11 3.21 3.98 c\n",
    "3.7 3.89 4.05 3.39 4 2.9 c\n",
    "4 2.21 4.01 1.53 3.99 .84 c\n",
    "3.94 .32 3.43 -.09 2.91 -.03 c\n",
    "h\n",
    ".34 2 m\n",
    ".1 2.07 -.04 2.34 - 2.58 c\n",
    "-.02 3.18 .03 3.81 .32 4.34 c\n",
    ".75 5.18 1.58 5.78 2.5 5.94 c\n",
    "2.5 6.29 2.5 6.65 2.5 7 c\n",
    "2.11 7.01 1.68 6.94 1.36 7.23 c\n",
    "1.14 7.41 .96 7.75 1.02 8 c\n",
    "2.35 8 3.67 8 5 8 c\n",
    "5.02 7.43 4.47 6.94 3.9 7 c\n",
    "3.77 7 3.63 7 3.5 7 c\n",
    "3.5 6.65 3.5 6.29 3.5 5.94 c\n",
    "4.82 5.73 5.92 4.55 5.99 3.21 c\n",
    "5.99 2.93 6.02 2.65 5.98 2.37 c\n",
    "5.91 2.05 5.49 1.89 5.23 2.08 c\n",
    "5.03 2.2 4.97 2.45 5 2.66 c\n",
    "5.02 3.15 4.97 3.66 4.69 4.08 c\n",
    "4.18 4.9 3.05 5.24 2.17 4.82 c\n",
    "1.42 4.5 .94 3.67 1 2.86 c\n",
    ".99 2.61 1.05 2.32 .85 2.13 c\n",
    ".71 1.99 .52 1.98 .34 2 c\n",
    "h\n",
    "f\n",
);

const ICON_STAR: &str = concat!(
    "4 0 m\n",
    "3 3 l\n",
    "0 3 l\n",
    "2.5 5 l\n",
    "1.5 8 l\n",
    "4 6 l\n",
    "6.5 8 l\n",
    "5.5 5 l\n",
    "8 3 l\n",
    "5 3 l\n",
    "4 0 l\n",
    "h\n",
    "f\n",
);

const ICON_INSERT: &str = concat!("8 5 m\n", "4 0 l\n", "0 5 l\n", "f\n",);

const ICON_NEW_PARAGRAPH: &str = concat!("8 8 m\n", "4 0 l\n", "0 8 l\n", "f\n",);

const ICON_PARAGRAPH: &str = concat!(
    "7 0 m\n",
    "2 0 l\n",
    "1 0 0 1 0 2 c\n",
    "0 3 1 4 2 4 c\n",
    "3 4 l\n",
    "3 8 l\n",
    "4 8 l\n",
    "4 1 l\n",
    "5 1 l\n",
    "5 8 l\n",
    "6 8 l\n",
    "6 1 l\n",
    "7 1 l\n",
    "f\n",
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use crate::geometry::Matrix;
    use crate::object::Dict;

    fn doc() -> Document {
        Document::open(
            &b"%PDF-1.7\n1 0 obj<</Type/Catalog>>endobj\n4 0 obj null endobj\n\
trailer<</Root 1 0 R/Size 5>>\n"[..],
        )
        .unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn parse(text: &str) -> Object {
        let diag = crate::Diagnostics::new();
        let mut lx = crate::lex::Lexer::new(text.as_bytes(), 0, &diag);
        crate::parse::parse_stm_obj(&mut lx).unwrap_or_else(|e| panic!("{e:?}"))
    }

    /// The content, /Rect, bounding box and resources made for `annot`.
    fn made(annot: &str) -> (String, Rect, Rect, Option<Dict>) {
        let d = doc();
        let s = super::super::synthesize(&d, &parse(annot), Matrix::IDENTITY)
            .unwrap_or_else(|e| panic!("{e}"));
        let rect = d.to_rect(Some(&d.dict_get(&s.annot, b"Rect")));
        (
            String::from_utf8_lossy(&s.content).into_owned(),
            rect,
            s.bbox,
            s.resources,
        )
    }

    /// `key` of the /H graphics state in `res`.
    fn gs(res: Option<Dict>, key: &[u8]) -> Object {
        let res = Object::Dict(std::sync::Arc::new(res.unwrap_or_default()));
        let d = doc();
        d.dict_get(&d.dict_get(&d.dict_get(&res, b"ExtGState"), b"H"), key)
    }

    // The expected content, /Rect and bounding boxes below are MuPDF's.

    #[test]
    fn highlights_curve_out_at_each_end_and_multiply() {
        let (c, r, b, res) = made(
            "<</Subtype/Highlight/Rect[0 0 1 1]/QuadPoints[100 112 300 112 100 100 300 100]\
             /C[1 1 0]/CA 0.5>>",
        );
        assert_eq!(
            c,
            "/H gs\n1 1 0 rg\n100 100 m\n97.17148 102.82852 97.17148 109.17148 100 112 c\n\
             300 112 l\n302.82853 109.17148 302.82853 102.82852 300 100 c\nf\n"
        );
        let want = Rect::new(97.171_48, 99.25, 302.828_52, 112.75);
        assert_eq!((r, b), (want, want));
        assert_eq!(gs(res.clone(), b"BM"), Object::name(b"Multiply"));
        assert_eq!(gs(res, b"CA"), Object::Real(0.5));
    }

    #[test]
    fn highlights_follow_a_rotated_quad() {
        let (c, r, _, res) = made(
            "<</Subtype/Highlight/Rect[0 0 1 1]/QuadPoints[100.37 489.11 210.2 552.84 \
             105.47 480.27 215.3 543.99]/C[0 0 0 0.1]>>",
        );
        assert_eq!(
            c,
            "/H gs\n0 0 0 .1 k\n105.47 480.27 m\n\
             102.18207 481.14356 99.49644 485.82206 100.37 489.11 c\n210.2 552.84 l\n\
             213.48793 551.9665 216.17357 547.27798 215.3 543.99 c\nf\n"
        );
        assert_eq!(r, Rect::new(99.496_44, 479.632_14, 216.173_57, 553.477_9));
        // Multiply without an opacity.
        assert_eq!(gs(res, b"CA"), Object::Null);
    }

    #[test]
    fn underlines_sit_a_seventh_up() {
        let (c, r, _, res) = made(
            "<</Subtype/Underline/Rect[0 0 1 1]/QuadPoints[100 112 300 112 100 100 300 100]\
             /C[0 0 1]>>",
        );
        assert_eq!(c, "0 0 1 RG\n.75 w\n100 101.71429 m\n300 101.71429 l\nS\n");
        assert_eq!(r, Rect::new(99.25, 99.25, 300.75, 112.75));
        assert!(res.is_none());
    }

    #[test]
    fn strike_outs_read_a_short_last_quad_as_zeros() {
        let (c, r, _, _) = made(
            "<</Subtype/StrikeOut/Rect[0 0 1 1]/QuadPoints[100 114 150 114 100 100 150 100 \
             1 2 3 4]/C[0.5]>>",
        );
        assert_eq!(
            c,
            ".5 G\n.875 w\n100 106 m\n150 106 l\nS\n\
             .13975425 w\n.42857144 .85714289 m\n1.2857143 1.7142857 l\nS\n"
        );
        assert_eq!(r, Rect::new(-0.139_754_25, -0.139_754_25, 150.875, 114.875));
    }

    #[test]
    fn strike_outs_without_quads_keep_their_rect() {
        let (c, r, b, _) = made("<</Subtype/StrikeOut/Rect[10 20 30 40]>>");
        assert_eq!(c, "");
        assert_eq!((r, b), (Rect::new(10.0, 20.0, 30.0, 40.0), r));
    }

    #[test]
    fn squiggles_zigzag_a_seventh_high() {
        let (c, r, _, _) = made(
            "<</Subtype/Squiggly/Rect[0 0 1 1]/QuadPoints[10 17 20 17 10 10 20 10]/C[1 0 0]>>",
        );
        assert_eq!(
            c,
            "1 0 0 RG\n.4375 w\n10 10 m\n11 11 l\n12 10 l\n13 11 l\n14 10 l\n15 11 l\n\
             16 10 l\n17 11 l\n18 10 l\n19 11 l\n20 10 l\nS\n"
        );
        assert_eq!(r, Rect::new(9.5625, 9.5625, 20.4375, 17.4375));
    }

    #[test]
    fn squiggles_of_no_height_stop_where_mupdf_would_loop() {
        let d = doc();
        let a = parse("<</Subtype/Squiggly/Rect[0 0 1 1]/QuadPoints[10 10 20 10 10 10 20 10]>>");
        assert_eq!(
            super::super::synthesize(&d, &a, Matrix::IDENTITY).map(|s| s.content),
            Err("too many squiggles".into())
        );
    }

    #[test]
    fn redactions_outline_their_quads_or_inside_their_rect() {
        let (c, r, _, res) = made("<</Subtype/Redact/Rect[10 20 30 40]/CA 0.25>>");
        assert_eq!(
            c,
            "/H gs\n1 0 0 RG\n11 21 m\n29 21 l\n29 39 l\n11 39 l\ns\n"
        );
        assert_eq!(r, Rect::new(10.0, 20.0, 30.0, 40.0));
        assert_eq!(gs(res, b"ca"), Object::Real(0.25));

        let (c, r, _, _) =
            made("<</Subtype/Redact/Rect[0 0 1 1]/QuadPoints[10 17 20 17 10 10 20 10]>>");
        assert_eq!(c, "1 0 0 RG\n10 10 m\n20 10 l\n20 17 l\n10 17 l\ns\n");
        assert_eq!(r, Rect::new(9.0, 9.0, 21.0, 18.0));
    }

    #[test]
    fn carets_are_centred_on_their_rect() {
        let (c, r, b, _) = made("<</Subtype/Caret/Rect[402.45 688.25 412.45 692.25]/C[0 0 1]>>");
        assert_eq!(
            c,
            "0 0 1 rg\n0 0 m\n10 0 10 7 10 14 c\n10 7 10 0 20 0 c\nf\n"
        );
        assert_eq!(r, Rect::new(397.45, 683.25, 417.45, 697.25));
        assert_eq!(b, Rect::new(0.0, 0.0, 20.0, 14.0));
    }

    const FRAME: &str = "1 w\n0.5 0.5 15 15 re\nb\n1 0 0 -1 4 12 cm\n";

    #[test]
    fn notes_are_drawn_white_at_the_top_left() {
        let (c, r, b, _) = made("<</Subtype/Text/Rect[100 100 150 130]>>");
        assert_eq!(c, format!("1 g\n{FRAME}0 g\n{ICON_NOTE}"));
        assert_eq!(r, Rect::new(100.0, 110.0, 120.0, 130.0));
        assert_eq!(b, Rect::new(0.0, 0.0, 16.0, 16.0));
        assert_eq!(
            ICON_NOTE,
            "0 0 8 1 re\n0 2 8 1 re\n0 4 8 1 re\n0 6 8 1 re\nf\n"
        );
    }

    #[test]
    fn icons_are_at_least_ten_wide_and_light_on_dark() {
        let (c, r, _, _) = made("<</Subtype/Text/Rect[100 100 105 103]/Name/Comment/C[0.1]>>");
        assert_eq!(c, format!(".1 g\n{FRAME}1 g\n{ICON_COMMENT}"));
        assert_eq!(r, Rect::new(100.0, 93.0, 110.0, 103.0));
        let (c, _, _, _) = made("<</Subtype/Sound/Rect[100 100 120 120]/Name/Mic/C[0 0 0 1]>>");
        assert_eq!(c, format!("0 0 0 1 k\n{FRAME}1 g\n{ICON_MIC}"));
    }

    #[test]
    fn icons_default_by_type() {
        let (c, r, _, _) =
            made("<</Subtype/FileAttachment/Rect[100 100 115 112]/C[0.145 0.35 0]>>");
        // A gray of just 0.25 once the products are fused as clang fuses
        // them, just under it the other way.
        assert_eq!(c, format!(".145 .35 0 rg\n{FRAME}0 g\n{ICON_PUSH_PIN}"));
        assert_eq!(r, Rect::new(100.0, 100.0, 112.0, 112.0));
        let (c, _, _, _) = made("<</Subtype/Sound/Rect[0 0 20 20]>>");
        assert_eq!(c, format!("1 g\n{FRAME}0 g\n{ICON_SPEAKER}"));
    }

    #[test]
    fn icon_names_that_are_not_known_names_draw_a_star() {
        let star = format!("1 g\n{FRAME}0 g\n{ICON_STAR}");
        let note = format!("1 g\n{FRAME}0 g\n{ICON_NOTE}");
        let icon = |n: &str| made(&format!("<</Subtype/Text/Rect[0 0 20 20]/Name {n}>>")).0;
        assert_eq!(icon("4 0 R"), star);
        assert_eq!(icon("(Note)"), star);
        assert_eq!(icon("/Draft"), star);
        assert_eq!(icon("null"), note);
    }

    #[test]
    fn each_icon_name_has_its_icon() {
        let names = [
            ("Comment", ICON_COMMENT),
            ("Key", ICON_KEY),
            ("Note", ICON_NOTE),
            ("Help", ICON_HELP),
            ("NewParagraph", ICON_NEW_PARAGRAPH),
            ("Paragraph", ICON_PARAGRAPH),
            ("Insert", ICON_INSERT),
            ("Graph", ICON_GRAPH),
            ("PushPin", ICON_PUSH_PIN),
            ("Paperclip", ICON_PAPERCLIP),
            ("Tag", ICON_TAG),
            ("Speaker", ICON_SPEAKER),
            ("Mic", ICON_MIC),
        ];
        for (name, glyph) in names {
            let (c, _, _, _) = made(&format!("<</Subtype/Text/Rect[0 0 20 20]/Name/{name}>>"));
            assert_eq!(c, format!("1 g\n{FRAME}0 g\n{glyph}"), "{name}");
        }
    }
}
