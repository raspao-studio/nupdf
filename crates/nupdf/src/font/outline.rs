// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Following FreeType 2 src/truetype/ttgload.c and ttpload.c (TrueType
// glyph loading), src/psaux/psintrp.c, psft.c, pshints.c and psstack.c
// (the CFF and Type 1 charstring interpreter), src/cff/cffgload.c and
// src/base/ftoutln.c (FT_Outline_Get_CBox), Copyright (C) 1996-2025 by
// David Turner, Robert Wilhelm, and Werner Lemberg, and Adobe Systems
// Incorporated, used under the FreeType License (NOTICE.md).

//! Glyph outlines, as far as MuPDF measures them: the control box of a
//! glyph's unhinted outline at a size of one font unit per 26.6 unit
//! (`fz_bound_ft_glyph`), in font units. Every point of the outline counts,
//! off-curve ones too.

/// A box over outline points, in font units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CBox {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

/// The points seen so far; None until there is one.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Points(Option<CBox>);

impl Points {
    pub(crate) fn add(&mut self, x: f64, y: f64) {
        self.0 = Some(match self.0 {
            None => CBox {
                x0: x,
                y0: y,
                x1: x,
                y1: y,
            },
            Some(b) => CBox {
                x0: b.x0.min(x),
                y0: b.y0.min(y),
                x1: b.x1.max(x),
                y1: b.y1.max(y),
            },
        });
    }

    pub(crate) fn cbox(self) -> Option<CBox> {
        self.0
    }
}

/// A glyph that FreeType fails to load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadError;

/// `FT_CURVE_TAG_CONIC`, `_ON` and `_CUBIC`.
pub const TAG_CONIC: u8 = 0;
pub const TAG_ON: u8 = 1;
pub const TAG_CUBIC: u8 = 2;

/// `FT_Outline`: points (26.6 once scaled, font units before), a tag a
/// point, the last point of each contour.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outline {
    pub points: Vec<(i64, i64)>,
    pub tags: Vec<u8>,
    pub contours: Vec<usize>,
    pub even_odd: bool,
    /// `FT_OUTLINE_OVERLAP`: rendered oversampled, so overlapping
    /// contours do not darken each other.
    pub overlap: bool,
}

impl Outline {
    /// `FT_Outline_Get_CBox`: the box of every point, off-curve too.
    pub fn cbox(&self) -> (i64, i64, i64, i64) {
        let mut it = self.points.iter();
        let Some(&(x, y)) = it.next() else {
            return (0, 0, 0, 0);
        };
        let mut b = (x, y, x, y);
        for &(x, y) in it {
            b.0 = b.0.min(x);
            b.1 = b.1.min(y);
            b.2 = b.2.max(x);
            b.3 = b.3.max(y);
        }
        b
    }

    /// `FT_Outline_Transform` by a 16.16 matrix.
    pub fn transform(&mut self, xx: i64, xy: i64, yx: i64, yy: i64) {
        for p in &mut self.points {
            let (x, y) = *p;
            *p = (
                mul_fix(x, xx) + mul_fix(y, xy),
                mul_fix(x, yx) + mul_fix(y, yy),
            );
        }
    }

    /// `FT_Outline_Translate`.
    pub fn translate(&mut self, dx: i64, dy: i64) {
        for p in &mut self.points {
            p.0 += dx;
            p.1 += dy;
        }
    }

    /// The scaling `cff_slot_load` and `T1_Load_Glyph` apply to an
    /// unhinted outline: each point by `FT_MulFix`.
    pub fn scale(&mut self, x_scale: i64, y_scale: i64) {
        for p in &mut self.points {
            *p = (mul_fix(p.0, x_scale), mul_fix(p.1, y_scale));
        }
    }

    /// Ends the contour being built, if it has points.
    fn end_contour(&mut self) {
        let start = self.contours.last().map_or(0, |&l| l + 1);
        if self.points.len() > start {
            self.contours.push(self.points.len() - 1);
        }
    }
}

/// `FT_DivFix`: `a * 65536 / b`, rounded.
pub fn div_fix_int(a: i64, b: i64) -> i64 {
    if b == 0 {
        return 0x7FFF_FFFF;
    }
    let s = (a < 0) != (b < 0);
    let (a, b) = (a.unsigned_abs(), b.unsigned_abs());
    let q = ((a << 16) + (b >> 1)) / b;
    if s { -(q as i64) } else { q as i64 }
}

/// `FT_MulFix`: rounds half away from zero.
pub fn mul_fix(a: i64, b: i64) -> i64 {
    let c = a * b;
    (c + 0x8000 - i64::from(c < 0)) >> 16
}

// TrueType.

const ON_REPEAT: u8 = 8;
const X_SHORT: u8 = 2;
const Y_SHORT: u8 = 4;
const X_SAME: u8 = 16;
const Y_SAME: u8 = 32;

const ARGS_ARE_WORDS: u16 = 0x0001;
const ARGS_ARE_XY_VALUES: u16 = 0x0002;
const WE_HAVE_A_SCALE: u16 = 0x0008;
const MORE_COMPONENTS: u16 = 0x0020;
const WE_HAVE_AN_XY_SCALE: u16 = 0x0040;
const WE_HAVE_A_2X2: u16 = 0x0080;
const SCALED_COMPONENT_OFFSET: u16 = 0x0800;
const OVERLAP_COMPOUND: u16 = 0x0400;
const OVERLAP_SIMPLE: u8 = 0x40;

/// What a TrueType glyph loads into: the outline, and the scale its
/// points take as they load (None for font units).
pub(crate) struct Acc {
    pub out: Outline,
    pub scale: Option<(i64, i64)>,
    pub overlap: bool,
}

/// What the TrueType loader reads a glyph from.
pub(crate) struct Glyf<'a> {
    pub data: &'a [u8],
    /// The `loca` table's extent and whether its entries are long.
    pub loca: (usize, usize),
    pub long_loca: bool,
    pub glyf: (usize, usize),
    pub num_glyphs: u32,
}

fn be16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(at)?, *d.get(at + 1)?]))
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *d.get(at)?,
        *d.get(at + 1)?,
        *d.get(at + 2)?,
        *d.get(at + 3)?,
    ]))
}

impl Glyf<'_> {
    /// `tt_face_get_location`: a glyph's offset in `glyf` and its length.
    fn location(&self, gid: u32) -> (usize, usize) {
        let (loca, loca_len) = self.loca;
        let shift = if self.long_loca { 2 } else { 1 };
        let num_locations = (loca_len >> shift) as u64;
        let glyf_len = self.glyf.1;
        let gid = u64::from(gid);
        let (mut pos1, mut pos2) = (0usize, 0usize);
        if gid < num_locations {
            let g = gid as usize;
            if self.long_loca {
                let p = loca + g * 4;
                pos1 = be32(self.data, p).unwrap_or(0) as usize;
                pos2 = if g * 4 + 8 <= loca_len {
                    be32(self.data, p + 4).unwrap_or(0) as usize
                } else {
                    pos1
                };
            } else {
                let p = loca + g * 2;
                pos1 = be16(self.data, p).map_or(0, usize::from) << 1;
                pos2 = if g * 2 + 4 <= loca_len {
                    be16(self.data, p + 2).map_or(0, usize::from) << 1
                } else {
                    pos1
                };
            }
        }
        if pos1 > glyf_len {
            return (0, 0);
        }
        if pos2 > glyf_len {
            if gid + 2 == num_locations {
                pos2 = glyf_len;
            } else {
                return (0, 0);
            }
        }
        let size = if pos2 >= pos1 {
            pos2 - pos1
        } else {
            glyf_len - pos1
        };
        (pos1, size)
    }

    /// `load_truetype_glyph`: the glyph's points appended to `acc`,
    /// scaled as they load when `acc` has a scale. The glyph header's
    /// xMin, for the phantom point, or None for an empty glyph.
    fn load(
        &self,
        gid: u32,
        acc: &mut Acc,
        composites: &mut Vec<u32>,
        depth: usize,
        budget: &mut u32,
    ) -> Result<Option<i64>, LoadError> {
        *budget = budget.checked_sub(1).ok_or(LoadError)?;
        if depth > 100 || gid >= self.num_glyphs {
            return Err(LoadError);
        }
        let (offset, len) = self.location(gid);
        if len == 0 {
            return Ok(None);
        }
        let start = self.glyf.0.checked_add(offset).ok_or(LoadError)?;
        let glyph = self
            .data
            .get(start..start.checked_add(len).ok_or(LoadError)?)
            .ok_or(LoadError)?;
        if glyph.len() < 10 {
            return Err(LoadError);
        }
        let n_contours = i16::from_be_bytes([glyph[0], glyph[1]]);
        let x_min = i64::from(i16::from_be_bytes([glyph[2], glyph[3]]));
        let body = &glyph[10..];
        if n_contours > 0 {
            simple_glyph(body, n_contours as usize, acc)?;
            return Ok(Some(x_min));
        }
        if n_contours == 0 {
            return Ok(Some(x_min));
        }
        // A composite: its index stands at this depth; an ancestor's index
        // again is a loop.
        composites.truncate(depth);
        if composites.contains(&gid) {
            return Err(LoadError);
        }
        composites.push(gid);
        let start_point = acc.out.points.len();
        let subs = self.components(body)?;
        if subs
            .first()
            .is_some_and(|s| s.flags & OVERLAP_COMPOUND != 0)
        {
            acc.overlap = true;
        }
        for sub in subs {
            let base = acc.out.points.len();
            self.load(u32::from(sub.index), acc, composites, depth + 1, budget)?;
            if acc.out.points.len() == base {
                continue;
            }
            component(&mut acc.out.points, start_point, base, &sub, acc.scale)?;
        }
        Ok(Some(x_min))
    }

    /// `TT_Load_Composite_Glyph`.
    fn components(&self, body: &[u8]) -> Result<Vec<SubGlyph>, LoadError> {
        let mut out = Vec::new();
        let mut p = 0usize;
        loop {
            let rd = |at: usize| be16(body, at).ok_or(LoadError);
            let flags = rd(p)?;
            let index = rd(p + 2)?;
            p += 4;
            if u32::from(index) >= self.num_glyphs {
                return Err(LoadError);
            }
            let mut count = 2;
            if flags & ARGS_ARE_WORDS != 0 {
                count += 2;
            }
            if flags & WE_HAVE_A_SCALE != 0 {
                count += 2;
            } else if flags & WE_HAVE_AN_XY_SCALE != 0 {
                count += 4;
            } else if flags & WE_HAVE_A_2X2 != 0 {
                count += 8;
            }
            if p + count > body.len() {
                return Err(LoadError);
            }
            let (arg1, arg2) = if flags & ARGS_ARE_WORDS != 0 {
                let (a, b) = (rd(p)?, rd(p + 2)?);
                p += 4;
                if flags & ARGS_ARE_XY_VALUES != 0 {
                    (i64::from(a as i16), i64::from(b as i16))
                } else {
                    (i64::from(a), i64::from(b))
                }
            } else {
                let (a, b) = (body[p], body[p + 1]);
                p += 2;
                if flags & ARGS_ARE_XY_VALUES != 0 {
                    (i64::from(a as i8), i64::from(b as i8))
                } else {
                    (i64::from(a), i64::from(b))
                }
            };
            let f2 = |at: usize| -> Result<i64, LoadError> { Ok(i64::from(rd(at)? as i16) * 4) };
            let (mut xx, mut xy, mut yx, mut yy) = (0x10000, 0, 0, 0x10000);
            if flags & WE_HAVE_A_SCALE != 0 {
                xx = f2(p)?;
                yy = xx;
                p += 2;
            } else if flags & WE_HAVE_AN_XY_SCALE != 0 {
                xx = f2(p)?;
                yy = f2(p + 2)?;
                p += 4;
            } else if flags & WE_HAVE_A_2X2 != 0 {
                xx = f2(p)?;
                yx = f2(p + 2)?;
                xy = f2(p + 4)?;
                yy = f2(p + 6)?;
                p += 8;
            }
            out.push(SubGlyph {
                flags,
                index,
                arg1,
                arg2,
                matrix: [xx, xy, yx, yy],
            });
            if flags & MORE_COMPONENTS == 0 {
                return Ok(out);
            }
        }
    }
}

struct SubGlyph {
    flags: u16,
    index: u16,
    arg1: i64,
    arg2: i64,
    /// xx, xy, yx, yy in 16.16.
    matrix: [i64; 4],
}

/// `TT_Load_Simple_Glyph`.
fn simple_glyph(body: &[u8], n_contours: usize, acc: &mut Acc) -> Result<(), LoadError> {
    let base = acc.out.points.len();
    if n_contours >= 0xFFF || 2 * n_contours + 2 > body.len() {
        return Err(LoadError);
    }
    let mut last: i64 = -1;
    let mut ends = Vec::with_capacity(n_contours);
    for i in 0..n_contours {
        let e = i64::from(be16(body, 2 * i).ok_or(LoadError)?);
        if e <= last {
            return Err(LoadError);
        }
        last = e;
        ends.push(e as usize);
    }
    let n_points = (last + 1) as usize;
    // FT_GLYPHLOADER_CHECK_POINTS: the outline, with its four phantom
    // points, within FT_OUTLINE_POINTS_MAX.
    if base + n_points + 4 > 0xFFFF {
        return Err(LoadError);
    }
    let mut p = 2 * n_contours;
    let n_ins = usize::from(be16(body, p).ok_or(LoadError)?);
    p += 2;
    if p + n_ins > body.len() {
        return Err(LoadError);
    }
    p += n_ins;
    let mut flags = Vec::with_capacity(n_points);
    while flags.len() < n_points {
        let c = *body.get(p).ok_or(LoadError)?;
        p += 1;
        flags.push(c);
        if c & ON_REPEAT != 0 {
            let count = usize::from(*body.get(p).ok_or(LoadError)?);
            p += 1;
            if flags.len() + count > n_points {
                return Err(LoadError);
            }
            flags.extend(std::iter::repeat_n(c, count));
        }
    }
    let coords = |short: u8, same: u8, p: &mut usize| -> Result<Vec<i64>, LoadError> {
        let mut v = 0i64;
        let mut out = Vec::with_capacity(n_points);
        for &f in &flags {
            let delta = if f & short != 0 {
                let d = i64::from(*body.get(*p).ok_or(LoadError)?);
                *p += 1;
                if f & same != 0 { d } else { -d }
            } else if f & same == 0 {
                let d = i64::from(be16(body, *p).ok_or(LoadError)? as i16);
                *p += 2;
                d
            } else {
                0
            };
            v += delta;
            out.push(v);
        }
        Ok(out)
    };
    let xs = coords(X_SHORT, X_SAME, &mut p)?;
    let ys = coords(Y_SHORT, Y_SAME, &mut p)?;
    if flags.first().is_some_and(|f| f & OVERLAP_SIMPLE != 0) {
        acc.overlap = true;
    }
    let scale = acc.scale;
    acc.out
        .points
        .extend(xs.into_iter().zip(ys).map(|(x, y)| match scale {
            Some((sx, sy)) => (mul_fix(x, sx), mul_fix(y, sy)),
            None => (x, y),
        }));
    acc.out.tags.extend(flags.iter().map(|f| f & 1));
    acc.out.contours.extend(ends.into_iter().map(|e| e + base));
    Ok(())
}

/// `TT_Process_Composite_Component`.
fn component(
    points: &mut [(i64, i64)],
    start_point: usize,
    base: usize,
    sub: &SubGlyph,
    scale: Option<(i64, i64)>,
) -> Result<(), LoadError> {
    let [xx, xy, yx, yy] = sub.matrix;
    let have_scale = sub.flags & (WE_HAVE_A_SCALE | WE_HAVE_AN_XY_SCALE | WE_HAVE_A_2X2) != 0;
    if have_scale {
        for p in &mut points[base..] {
            let (x, y) = *p;
            *p = (
                mul_fix(x, xx) + mul_fix(y, xy),
                mul_fix(x, yx) + mul_fix(y, yy),
            );
        }
    }
    let (x, y) = if sub.flags & ARGS_ARE_XY_VALUES == 0 {
        let k = usize::try_from(sub.arg1).map_err(|_| LoadError)? + start_point;
        let l = usize::try_from(sub.arg2).map_err(|_| LoadError)? + base;
        if k >= base || l >= points.len() {
            return Err(LoadError);
        }
        (points[k].0 - points[l].0, points[k].1 - points[l].1)
    } else {
        let (mut x, mut y) = (sub.arg1, sub.arg2);
        if x == 0 && y == 0 {
            return Ok(());
        }
        if have_scale && sub.flags & SCALED_COMPONENT_OFFSET != 0 {
            let hypot = |a: i64, b: i64| ((a as f64).hypot(b as f64)).round() as i64;
            x = mul_fix(x, hypot(xx, xy));
            y = mul_fix(y, hypot(yy, yx));
        }
        if let Some((sx, sy)) = scale {
            x = mul_fix(x, sx);
            y = mul_fix(y, sy);
        }
        (x, y)
    };
    if x != 0 || y != 0 {
        for p in &mut points[base..] {
            p.0 += x;
            p.1 += y;
        }
    }
    Ok(())
}

/// A TrueType glyph's points, in font units, or an empty list for a
/// glyph with no outline.
pub(crate) fn truetype_points(g: &Glyf<'_>, gid: u32) -> Result<Vec<(i64, i64)>, LoadError> {
    let mut acc = Acc {
        out: Outline::default(),
        scale: None,
        overlap: false,
    };
    g.load(gid, &mut acc, &mut Vec::new(), 0, &mut (1 << 16))?;
    Ok(acc.out.points)
}

/// `TT_Load_Glyph` without hinting at `scale` (16.16, the size's x and y
/// scale): the outline in 26.6, moved so the phantom point `pp1` (the
/// header's xMin less `lsb`) is the origin.
pub(crate) fn truetype_outline(
    g: &Glyf<'_>,
    gid: u32,
    scale: (i64, i64),
    lsb: i64,
) -> Result<Outline, LoadError> {
    let mut acc = Acc {
        out: Outline::default(),
        scale: Some(scale),
        overlap: false,
    };
    let x_min = g.load(gid, &mut acc, &mut Vec::new(), 0, &mut (1 << 16))?;
    let mut out = acc.out;
    out.overlap = acc.overlap;
    if let Some(x_min) = x_min {
        let pp1 = mul_fix(x_min - lsb, scale.0);
        if pp1 != 0 {
            out.translate(-pp1, 0);
        }
    }
    Ok(out)
}

// CFF and Type 1 charstrings, as FreeType's Adobe engine runs them.

/// What a charstring needs of its font.
pub(crate) trait Charstrings {
    fn is_t1(&self) -> bool;
    /// A local subroutine by its biased number.
    fn local_subr(&self, n: i64) -> Option<&[u8]>;
    /// A global subroutine by its biased number (CFF).
    fn global_subr(&self, n: i64) -> Option<&[u8]>;
    /// The charstring of the glyph a standard encoding code names, for
    /// `seac`.
    fn seac_component(&self, code: i64) -> Option<&[u8]>;
}

const STACK_SIZE: usize = 48;
const MAX_SUBR: usize = 16;
const MAX_HINTS: usize = 96;
const STORAGE_SIZE: usize = 32;

#[derive(Debug, Clone, Copy)]
struct Num {
    v: f64,
    int: bool,
}

/// The glyph path and its points: `cf2_glyphpath`, reduced to which points
/// reach the outline.
struct Path<'p> {
    points: &'p mut Points,
    /// The outline itself, when it is wanted: contours, and each point
    /// with its tag.
    outline: Option<&'p mut Outline>,
    start: (f64, f64),
    cur: (f64, f64),
    move_pending: bool,
    open: bool,
}

/// A coordinate as it reaches the outline: 16.16 to 26.6 by a shift.
fn device(v: f64) -> f64 {
    ((v * 1024.0).round() / 1024.0).floor()
}

impl Path<'_> {
    fn point(&mut self, x: f64, y: f64) {
        self.tagged(x, y, TAG_ON);
    }

    fn tagged(&mut self, x: f64, y: f64, tag: u8) {
        let (x, y) = (device(x), device(y));
        self.points.add(x, y);
        if let Some(o) = self.outline.as_deref_mut() {
            o.points.push((x as i64, y as i64));
            o.tags.push(tag);
        }
    }

    fn begin(&mut self) {
        if self.move_pending {
            if let Some(o) = self.outline.as_deref_mut() {
                o.end_contour();
            }
            let (x, y) = self.start;
            self.point(x, y);
            self.move_pending = false;
            self.open = true;
        }
    }

    fn move_to(&mut self, x: f64, y: f64) {
        self.close();
        self.start = (x, y);
        self.cur = (x, y);
        self.move_pending = true;
    }

    fn line_to(&mut self, x: f64, y: f64) {
        if self.cur == (x, y) {
            return;
        }
        self.begin();
        self.point(x, y);
        self.cur = (x, y);
    }

    #[allow(clippy::too_many_arguments)]
    fn curve_to(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, x3: f64, y3: f64) {
        self.begin();
        self.tagged(x1, y1, TAG_CUBIC);
        self.tagged(x2, y2, TAG_CUBIC);
        self.point(x3, y3);
        self.cur = (x3, y3);
    }

    fn close(&mut self) {
        if self.open {
            self.move_pending = true;
            self.open = false;
        }
    }
}

struct Interp<'a, 'p, 'q> {
    font: &'a dyn Charstrings,
    t1: bool,
    stack: Vec<Num>,
    path: &'q mut Path<'p>,
    cur_x: f64,
    cur_y: f64,
    hstems: usize,
    vstems: usize,
    hintmask_valid: bool,
    have_width: bool,
    flex_state: bool,
    num_flex: usize,
    flex_store: [f64; 6],
    results: Vec<f64>,
    known_results: usize,
    storage: [f64; STORAGE_SIZE],
    large_int: bool,
    /// The decoder's left side bearing, which `seac` reads.
    left_bearing: &'q mut (f64, f64),
    instructions: &'q mut u32,
    doing_seac: bool,
    /// The width operand of a Type 2 charstring, once seen.
    width: Option<f64>,
    /// Stop at the width (`width_only`).
    width_only: bool,
}

type Step = Result<(), LoadError>;

/// What an operator leaves the operand stack to.
enum Flow {
    Keep,
    Clear,
    Exit,
}

/// The flex operators' operand patterns.
const HFLEX: [bool; 12] = [
    true, false, true, true, true, false, true, false, true, false, true, false,
];
const FLEX: [bool; 12] = [true; 12];
const HFLEX1: [bool; 12] = [
    true, true, true, true, true, false, true, false, true, true, true, false,
];
const FLEX1: [bool; 12] = [
    true, true, true, true, true, true, true, true, true, true, false, false,
];

impl Interp<'_, '_, '_> {
    fn push(&mut self, v: f64, int: bool) -> Step {
        if self.stack.len() >= STACK_SIZE {
            return Err(LoadError);
        }
        self.stack.push(Num { v, int });
        Ok(())
    }

    fn pop(&mut self) -> Result<f64, LoadError> {
        self.stack.pop().map(|n| n.v).ok_or(LoadError)
    }

    fn pop_int(&mut self) -> Result<i64, LoadError> {
        match self.stack.pop() {
            Some(n) if n.int => Ok(n.v as i64),
            _ => Err(LoadError),
        }
    }

    fn get(&self, i: usize) -> Result<f64, LoadError> {
        self.stack.get(i).map(|n| n.v).ok_or(LoadError)
    }

    /// `cf2_doStems`.
    /// True when only the width was wanted.
    fn stems(&mut self, vertical: bool) -> bool {
        let count = self.stack.len();
        if self.width_seen(count & 1 == 1) {
            return true;
        }
        let n = (count - (count & 1)) / 2;
        if vertical {
            self.vstems += n;
        } else {
            self.hstems += n;
        }
        self.stack.clear();
        false
    }

    /// The width a Type 2 operator gives, the operand under its own when
    /// `has` and there was none before; from here the glyph has a width
    /// (perhaps the default). True when only the width was wanted.
    fn width_seen(&mut self, has: bool) -> bool {
        if !self.t1 && has && !self.have_width {
            self.width = self.stack.first().map(|n| n.v);
        }
        self.have_width = true;
        self.width_only
    }

    /// `cf2_doFlex`.
    fn flex(&mut self, read: [bool; 12], conditional_last: bool) -> Step {
        let (cx, cy) = (self.cur_x, self.cur_y);
        let mut vals = [0.0f64; 14];
        vals[0] = cx;
        vals[1] = cy;
        let mut idx = 0;
        let hflex = !read[9];
        let top = if hflex { 9 } else { 10 };
        for i in 0..top {
            vals[i + 2] = vals[i];
            if read[i] {
                vals[i + 2] += self.get(idx)?;
                idx += 1;
            }
        }
        if hflex {
            vals[11] = cy;
        }
        if conditional_last {
            let last_is_x = (vals[10] - cx).abs() > (vals[11] - cy).abs();
            let last = self.get(idx)?;
            if last_is_x {
                vals[12] = vals[10] + last;
                vals[13] = cy;
            } else {
                vals[12] = cx;
                vals[13] = vals[11] + last;
            }
        } else {
            vals[12] = if read[10] {
                let v = vals[10] + self.get(idx)?;
                idx += 1;
                v
            } else {
                cx
            };
            vals[13] = if read[11] {
                vals[11] + self.get(idx)?
            } else {
                cy
            };
        }
        for j in 0..2 {
            let v = &vals[j * 6 + 2..j * 6 + 8];
            self.path.curve_to(v[0], v[1], v[2], v[3], v[4], v[5]);
        }
        self.stack.clear();
        self.cur_x = vals[12];
        self.cur_y = vals[13];
        Ok(())
    }

    fn move_to(&mut self) {
        if !self.flex_state {
            self.path.move_to(self.cur_x, self.cur_y);
        }
    }

    /// A component of a seac glyph, run from its own origin.
    fn component(&mut self, cs: &[u8], x: f64, y: f64) -> Step {
        let mut sub = Interp {
            font: self.font,
            t1: self.t1,
            stack: Vec::new(),
            path: &mut *self.path,
            cur_x: x,
            cur_y: y,
            hstems: 0,
            vstems: 0,
            hintmask_valid: false,
            have_width: false,
            flex_state: false,
            num_flex: 0,
            flex_store: [0.0; 6],
            results: Vec::new(),
            known_results: 0,
            storage: [0.0; STORAGE_SIZE],
            large_int: false,
            left_bearing: &mut *self.left_bearing,
            instructions: &mut *self.instructions,
            doing_seac: true,
            width: None,
            width_only: false,
        };
        sub.run(cs)
    }

    /// `cf2_interpT2CharString`, for the outline's points.
    fn run(&mut self, top: &[u8]) -> Step {
        let mut frames: Vec<(&[u8], usize)> = vec![(top, 0)];
        loop {
            let Some(&mut (cs, ref mut at)) = frames.last_mut() else {
                return Ok(());
            };
            let op1 = if *at >= cs.len() {
                if frames.len() > 1 { 11 } else { 14 }
            } else {
                let b = cs[*at];
                *at += 1;
                b
            };
            // cf2_buf_readByte: past the end, a zero (its error goes
            // nowhere).
            let read = |frames: &mut Vec<(&[u8], usize)>| -> u8 {
                let Some((cs, at)) = frames.last_mut() else {
                    return 0;
                };
                let b = cs.get(*at).copied().unwrap_or(0);
                *at += 1;
                b
            };
            if self.t1 {
                if !self.results.is_empty() && !matches!(op1, 10 | 11 | 12 | 32..=255) {
                    self.results.clear();
                }
                if self.large_int && !(op1 >= 32 || op1 == 12) {
                    self.large_int = false;
                }
            }
            *self.instructions = self.instructions.checked_sub(1).ok_or(LoadError)?;
            if *self.instructions == 0 {
                return Err(LoadError);
            }
            match op1 {
                0 | 2 | 15 | 16 | 17 => {}
                // hstem, hstemhm.
                1 | 18 => {
                    if (self.t1 || !self.hintmask_valid) && self.stems(false) {
                        return Ok(());
                    }
                }
                // vstem, vstemhm.
                3 | 23 => {
                    if (self.t1 || !self.hintmask_valid) && self.stems(true) {
                        return Ok(());
                    }
                }
                // vmoveto.
                4 => {
                    if self.width_seen(self.stack.len() > 1) {
                        return Ok(());
                    }
                    self.cur_y += self.pop()?;
                    self.move_to();
                }
                // rlineto.
                5 => {
                    let mut i = 0;
                    while i < self.stack.len() {
                        self.cur_x += self.get(i)?;
                        self.cur_y += self.get(i + 1)?;
                        self.path.line_to(self.cur_x, self.cur_y);
                        i += 2;
                    }
                    self.stack.clear();
                    continue;
                }
                // hlineto, vlineto.
                6 | 7 => {
                    let mut is_x = op1 == 6;
                    for i in 0..self.stack.len() {
                        let v = self.get(i)?;
                        if is_x {
                            self.cur_x += v;
                        } else {
                            self.cur_y += v;
                        }
                        is_x = !is_x;
                        self.path.line_to(self.cur_x, self.cur_y);
                    }
                    self.stack.clear();
                    continue;
                }
                // rrcurveto, rcurveline.
                8 | 24 => {
                    let count = self.stack.len();
                    let mut i = 0;
                    while i + 6 <= count {
                        self.curve_from(i, [true; 6])?;
                        i += 6;
                    }
                    if op1 == 24 {
                        self.cur_x += self.get(i)?;
                        self.cur_y += self.get(i + 1)?;
                        self.path.line_to(self.cur_x, self.cur_y);
                    }
                    self.stack.clear();
                    continue;
                }
                // closepath.
                9 => {
                    if self.t1 {
                        self.path.close();
                        self.have_width = true;
                    }
                }
                // callsubr, callgsubr.
                10 | 29 => {
                    let depth = frames.len() - 1;
                    // The subroutine stack holds MAX_SUBR levels under the
                    // charstring (cf2_arrstack_getPointer), for both kinds.
                    if depth >= MAX_SUBR {
                        return Err(LoadError);
                    }
                    let n = self.pop_int()?;
                    let sub = if op1 == 29 {
                        self.font.global_subr(n)
                    } else {
                        self.font.local_subr(n)
                    };
                    frames.push((sub.ok_or(LoadError)?, 0));
                    continue;
                }
                // return.
                11 => {
                    if frames.len() < 2 {
                        return Err(LoadError);
                    }
                    frames.pop();
                    continue;
                }
                12 => {
                    let op2 = read(&mut frames);
                    match self.escape(op2)? {
                        Flow::Keep => continue,
                        Flow::Clear => {}
                        Flow::Exit => return Ok(()),
                    }
                }
                // hsbw.
                13 => {
                    if self.t1 {
                        let _advance = self.pop()?;
                        let lsb = self.pop()?;
                        self.left_bearing.0 += lsb;
                        self.have_width = true;
                        self.cur_x += lsb;
                    }
                }
                // endchar.
                14 => {
                    let n = self.stack.len();
                    if self.width_seen(n == 1 || n == 5) {
                        return Ok(());
                    }
                    self.path.close();
                    if !self.t1 && self.stack.len() > 1 {
                        if self.doing_seac {
                            return Err(LoadError);
                        }
                        let achar = self.pop_int()?;
                        let bchar = self.pop_int()?;
                        let ady = self.pop()?;
                        let adx = self.pop()?;
                        let accent = self.font.seac_component(achar).ok_or(LoadError)?;
                        self.component(accent, adx, ady)?;
                        let base = self.font.seac_component(bchar).ok_or(LoadError)?;
                        self.component(base, 0.0, 0.0)?;
                    }
                    return Ok(());
                }
                // hintmask, cntrmask.
                19 | 20 => {
                    if !(self.stack.len() > 1 && self.hintmask_valid) {
                        if self.stems(true) {
                            return Ok(());
                        }
                        let bits = self.hstems + self.vstems;
                        if bits > MAX_HINTS {
                            return Err(LoadError);
                        }
                        if bits > 0 {
                            for _ in 0..bits.div_ceil(8) {
                                read(&mut frames);
                            }
                            if op1 == 19 {
                                self.hintmask_valid = true;
                            }
                        }
                    }
                }
                // rmoveto.
                21 => {
                    if self.width_seen(self.stack.len() > 2) {
                        return Ok(());
                    }
                    self.cur_y += self.pop()?;
                    self.cur_x += self.pop()?;
                    self.move_to();
                }
                // hmoveto.
                22 => {
                    if self.width_seen(self.stack.len() > 1) {
                        return Ok(());
                    }
                    self.cur_x += self.pop()?;
                    self.move_to();
                }
                // rlinecurve.
                25 => {
                    let count = self.stack.len();
                    let mut i = 0;
                    while i + 6 < count {
                        self.cur_x += self.get(i)?;
                        self.cur_y += self.get(i + 1)?;
                        self.path.line_to(self.cur_x, self.cur_y);
                        i += 2;
                    }
                    while i < count {
                        self.curve_from(i, [true; 6])?;
                        i += 6;
                    }
                    self.stack.clear();
                    continue;
                }
                // vvcurveto, hhcurveto.
                26 | 27 => {
                    let count1 = self.stack.len();
                    let count = count1 & !2;
                    let mut i = count1 - count;
                    while i < count {
                        let lead = (count - i) & 1 == 1;
                        let (x0, y0) = (self.cur_x, self.cur_y);
                        let mut d1 = 0.0;
                        if lead {
                            d1 = self.get(i)?;
                            i += 1;
                        }
                        let (x1, y1, x2, y2, x3, y3);
                        if op1 == 26 {
                            x1 = x0 + d1;
                            y1 = y0 + self.get(i)?;
                            x2 = x1 + self.get(i + 1)?;
                            y2 = y1 + self.get(i + 2)?;
                            x3 = x2;
                            y3 = y2 + self.get(i + 3)?;
                        } else {
                            y1 = y0 + d1;
                            x1 = x0 + self.get(i)?;
                            x2 = x1 + self.get(i + 1)?;
                            y2 = y1 + self.get(i + 2)?;
                            x3 = x2 + self.get(i + 3)?;
                            y3 = y2;
                        }
                        self.path.curve_to(x1, y1, x2, y2, x3, y3);
                        self.cur_x = x3;
                        self.cur_y = y3;
                        i += 4;
                    }
                    self.stack.clear();
                    continue;
                }
                // shortint.
                28 => {
                    let b1 = read(&mut frames);
                    let b2 = read(&mut frames);
                    self.push(f64::from(i16::from_be_bytes([b1, b2])), true)?;
                    continue;
                }
                // vhcurveto, hvcurveto.
                30 | 31 => {
                    let count1 = self.stack.len();
                    let count = count1 & !2;
                    let mut i = count1 - count;
                    let mut alternate = op1 == 31;
                    while i < count {
                        let (x1, y1, x2, y2, x3, y3);
                        if alternate {
                            x1 = self.cur_x + self.get(i)?;
                            y1 = self.cur_y;
                            x2 = x1 + self.get(i + 1)?;
                            y2 = y1 + self.get(i + 2)?;
                            y3 = y2 + self.get(i + 3)?;
                            if count - i == 5 {
                                x3 = x2 + self.get(i + 4)?;
                                i += 1;
                            } else {
                                x3 = x2;
                            }
                        } else {
                            x1 = self.cur_x;
                            y1 = self.cur_y + self.get(i)?;
                            x2 = x1 + self.get(i + 1)?;
                            y2 = y1 + self.get(i + 2)?;
                            x3 = x2 + self.get(i + 3)?;
                            if count - i == 5 {
                                y3 = y2 + self.get(i + 4)?;
                                i += 1;
                            } else {
                                y3 = y2;
                            }
                        }
                        alternate = !alternate;
                        self.path.curve_to(x1, y1, x2, y2, x3, y3);
                        self.cur_x = x3;
                        self.cur_y = y3;
                        i += 4;
                    }
                    self.stack.clear();
                    continue;
                }
                32..=246 => {
                    self.push(f64::from(i32::from(op1) - 139), true)?;
                    continue;
                }
                247..=250 => {
                    let b = i32::from(read(&mut frames));
                    self.push(f64::from((i32::from(op1) - 247) * 256 + b + 108), true)?;
                    continue;
                }
                251..=254 => {
                    let b = i32::from(read(&mut frames));
                    self.push(f64::from(-(i32::from(op1) - 251) * 256 - b - 108), true)?;
                    continue;
                }
                255 => {
                    let mut b = [0u8; 4];
                    for v in &mut b {
                        *v = read(&mut frames);
                    }
                    let v = i32::from_be_bytes(b);
                    if self.t1 {
                        if !(-32000..=32000).contains(&v) {
                            self.large_int = true;
                        }
                        self.push(f64::from(v), true)?;
                    } else {
                        self.push(f64::from(v) / 65536.0, false)?;
                    }
                    continue;
                }
            }
            self.stack.clear();
        }
    }

    /// The two-byte operators.
    fn escape(&mut self, op2: u8) -> Result<Flow, LoadError> {
        match op2 {
            34 => self.flex(HFLEX, false).map(|_| Flow::Keep),
            35 => self.flex(FLEX, false).map(|_| Flow::Clear),
            36 => self.flex(HFLEX1, false).map(|_| Flow::Keep),
            37 => self.flex(FLEX1, true).map(|_| Flow::Keep),
            8 | 13 | 19 | 25 | 31 | 32 | 38.. => Ok(Flow::Clear),
            _ if self.t1 && !self.results.is_empty() && op2 != 17 => {
                self.results.clear();
                Ok(Flow::Clear)
            }
            // dotsection.
            0 => Ok(Flow::Clear),
            // vstem3, hstem3: three stems, in Type 1.
            1 | 2 => {
                if self.t1 {
                    if self.stack.len() < 6 {
                        return Err(LoadError);
                    }
                    self.stack.truncate(6);
                    self.stems(op2 == 1);
                }
                Ok(Flow::Clear)
            }
            // and, or, not.
            3 | 4 => {
                let b = self.pop()?;
                let a = self.pop()?;
                let v = if op2 == 3 {
                    a != 0.0 && b != 0.0
                } else {
                    a != 0.0 || b != 0.0
                };
                self.push(f64::from(u8::from(v)), true)?;
                Ok(Flow::Keep)
            }
            5 => {
                let a = self.pop()?;
                self.push(f64::from(u8::from(a == 0.0)), true)?;
                Ok(Flow::Keep)
            }
            // seac, in Type 1.
            6 => {
                if !self.t1 {
                    return Ok(Flow::Clear);
                }
                let achar = self.pop_int()?;
                let bchar = self.pop_int()?;
                let ady = self.pop()?;
                let adx = self.pop()? + self.left_bearing.0;
                let asb = self.pop()?;
                if self.doing_seac {
                    return Err(LoadError);
                }
                let base = self.font.seac_component(bchar).ok_or(LoadError)?;
                let accent = self.font.seac_component(achar).ok_or(LoadError)?;
                self.component(base, 0.0, 0.0)?;
                *self.left_bearing = (0.0, 0.0);
                self.component(accent, adx - asb, ady)?;
                Ok(Flow::Exit)
            }
            // sbw, in Type 1.
            7 => {
                if self.t1 {
                    let _wy = self.pop()?;
                    let _wx = self.pop()?;
                    let sby = self.pop()?;
                    let sbx = self.pop()?;
                    self.left_bearing.0 += sbx;
                    self.left_bearing.1 += sby;
                    self.have_width = true;
                    self.cur_x += sbx;
                    self.cur_y += sby;
                }
                Ok(Flow::Clear)
            }
            // abs, neg.
            9 | 14 => {
                let a = self.pop()?;
                self.push(if op2 == 9 { a.abs() } else { -a }, false)?;
                Ok(Flow::Keep)
            }
            // add, sub, mul.
            10 | 11 | 24 => {
                let b = self.pop()?;
                let a = self.pop()?;
                let v = match op2 {
                    10 => a + b,
                    11 => a - b,
                    _ => a * b,
                };
                self.push(v, false)?;
                Ok(Flow::Keep)
            }
            // div.
            12 => {
                let b = self.pop()?;
                let a = self.pop()?;
                self.large_int = false;
                self.push(div_fix(a, b), false)?;
                Ok(Flow::Keep)
            }
            // eq.
            15 => {
                let b = self.pop()?;
                let a = self.pop()?;
                self.push(f64::from(u8::from(a == b)), true)?;
                Ok(Flow::Keep)
            }
            // callothersubr, in Type 1; Type 2 leaves the stack.
            16 => {
                if !self.t1 {
                    return Ok(Flow::Keep);
                }
                self.othersubr()?;
                Ok(Flow::Keep)
            }
            // pop, in Type 1; Type 2 leaves the stack.
            17 => {
                if !self.t1 {
                    return Ok(Flow::Keep);
                }
                if self.known_results > 0 {
                    self.known_results -= 1;
                    return Ok(Flow::Keep);
                }
                let v = self.results.pop().ok_or(LoadError)?;
                self.push(v, false)?;
                Ok(Flow::Keep)
            }
            // drop.
            18 => {
                self.pop()?;
                Ok(Flow::Keep)
            }
            // put.
            20 => {
                let idx = self.pop_int()?;
                let v = self.pop()?;
                if let Some(slot) = usize::try_from(idx)
                    .ok()
                    .and_then(|i| self.storage.get_mut(i))
                {
                    *slot = v;
                }
                Ok(Flow::Keep)
            }
            // get.
            21 => {
                let idx = self.pop_int()?;
                if let Some(&v) = usize::try_from(idx).ok().and_then(|i| self.storage.get(i)) {
                    self.push(v, false)?;
                }
                Ok(Flow::Keep)
            }
            // ifelse.
            22 => {
                let c2 = self.pop()?;
                let c1 = self.pop()?;
                let b = self.pop()?;
                let a = self.pop()?;
                self.push(if c1 <= c2 { a } else { b }, false)?;
                Ok(Flow::Keep)
            }
            // random: FreeType's generator, from its seed.
            23 => {
                self.push(1.0 / 65536.0, false)?;
                Ok(Flow::Keep)
            }
            // sqrt.
            26 => {
                let a = self.pop()?;
                self.push(if a > 0.0 { a.sqrt() } else { 0.0 }, false)?;
                Ok(Flow::Keep)
            }
            // dup.
            27 => {
                let a = self.pop()?;
                self.push(a, false)?;
                self.push(a, false)?;
                Ok(Flow::Keep)
            }
            // exch.
            28 => {
                let b = self.stack.pop().ok_or(LoadError)?;
                let a = self.stack.pop().ok_or(LoadError)?;
                self.stack.push(Num { v: b.v, int: false });
                self.stack.push(Num { v: a.v, int: false });
                Ok(Flow::Keep)
            }
            // index.
            29 => {
                let idx = self.pop_int()?;
                let size = self.stack.len();
                if size > 0 {
                    let at = if idx < 0 {
                        size - 1
                    } else if idx as usize >= size {
                        0
                    } else {
                        size - 1 - idx as usize
                    };
                    let v = self.get(at)?;
                    self.push(v, false)?;
                }
                Ok(Flow::Keep)
            }
            // roll.
            30 => {
                let shift = self.pop_int()?;
                let count = self.pop_int()?;
                if count >= 2 {
                    let count = count as usize;
                    if count > self.stack.len() {
                        return Err(LoadError);
                    }
                    let from = self.stack.len() - count;
                    let r = shift.rem_euclid(count as i64) as usize;
                    self.stack[from..].rotate_right(r);
                }
                Ok(Flow::Keep)
            }
            // setcurrentpoint, in Type 1.
            33 => {
                if self.t1 {
                    self.cur_y = self.pop()?;
                    self.cur_x = self.pop()?;
                    self.flex_state = false;
                }
                Ok(Flow::Clear)
            }
        }
    }

    /// `callothersubr` in Type 1: flex, hint replacement and the
    /// arithmetic ones; the operands stay below what it pushes.
    fn othersubr(&mut self) -> Step {
        let subr = self.pop_int()?;
        let argc = self.pop_int()?;
        if argc < 0 || argc as usize > self.stack.len() {
            return Err(LoadError);
        }
        self.known_results = 0;
        self.results.clear();
        let unexpected = Err(LoadError);
        match subr {
            0 => {
                if argc != 3 || !self.flex_state || self.num_flex != 7 {
                    return unexpected;
                }
                self.push(self.cur_x, false)?;
                self.push(self.cur_y, false)?;
                self.known_results = 2;
            }
            1 => {
                if argc != 0 {
                    return unexpected;
                }
                self.flex_state = true;
                self.num_flex = 0;
            }
            2 => {
                if argc != 0 || !self.flex_state {
                    return unexpected;
                }
                let idx = self.num_flex;
                self.num_flex += 1;
                if idx > 0 && idx < 7 {
                    let i = if idx > 3 { idx - 3 } else { idx } * 2;
                    self.flex_store[i - 2] = self.cur_x;
                    self.flex_store[i - 1] = self.cur_y;
                    if idx == 3 || idx == 6 {
                        let f = self.flex_store;
                        self.path.curve_to(f[0], f[1], f[2], f[3], f[4], f[5]);
                    }
                }
            }
            3 => {
                if argc != 1 {
                    return unexpected;
                }
                self.hstems = 0;
                self.vstems = 0;
                self.hintmask_valid = false;
                self.known_results = 1;
            }
            12 | 13 => self.stack.clear(),
            14..=19 | 24 | 25 => return unexpected,
            20..=23 => {
                if argc != 2 {
                    return unexpected;
                }
                let b = self.pop()?;
                let a = self.pop()?;
                let v = match subr {
                    20 => a + b,
                    21 => a - b,
                    22 => a * b,
                    _ => {
                        if b == 0.0 {
                            return unexpected;
                        }
                        div_fix(a, b)
                    }
                };
                self.push(v, false)?;
                self.known_results = 1;
            }
            27 => {
                if argc != 4 {
                    return unexpected;
                }
                let c2 = self.pop()?;
                let c1 = self.pop()?;
                let b = self.pop()?;
                let a = self.pop()?;
                self.push(if c1 <= c2 { a } else { b }, false)?;
                self.known_results = 1;
            }
            28 => {
                if argc != 0 {
                    return unexpected;
                }
                self.push(1.0 / 65536.0, false)?;
                self.known_results = 1;
            }
            _ if subr >= 0 => {
                let n = (argc as usize).min(3);
                let mut vals = Vec::with_capacity(n);
                for _ in 0..n {
                    vals.push(self.pop()?);
                }
                // `pop` gives them back top first.
                self.results = vals.into_iter().rev().collect();
            }
            _ => return unexpected,
        }
        Ok(())
    }

    /// A curve from six operands at `i`, each relative to the point
    /// before it.
    fn curve_from(&mut self, i: usize, _read: [bool; 6]) -> Step {
        let x1 = self.cur_x + self.get(i)?;
        let y1 = self.cur_y + self.get(i + 1)?;
        let x2 = x1 + self.get(i + 2)?;
        let y2 = y1 + self.get(i + 3)?;
        let x3 = x2 + self.get(i + 4)?;
        let y3 = y2 + self.get(i + 5)?;
        self.path.curve_to(x1, y1, x2, y2, x3, y3);
        self.cur_x = x3;
        self.cur_y = y3;
        Ok(())
    }
}

/// `FT_DivFix`, saturating on a zero divisor.
fn div_fix(a: f64, b: f64) -> f64 {
    if b == 0.0 {
        if a < 0.0 { -32768.0 } else { 32_767.999_985 }
    } else {
        a / b
    }
}

/// A charstring's outline control box, in font units: None when it draws
/// nothing.
pub(crate) fn charstring_cbox(
    font: &dyn Charstrings,
    cs: &[u8],
) -> Result<Option<CBox>, LoadError> {
    let mut points = Points::default();
    run_charstring(font, cs, &mut points, None, false)?;
    Ok(points.cbox())
}

/// A charstring's outline in font units, as the Adobe engine hands it to
/// `cff_slot_load` and `T1_Load_Glyph` unhinted (before their scaling).
pub(crate) fn charstring_outline(font: &dyn Charstrings, cs: &[u8]) -> Result<Outline, LoadError> {
    let mut points = Points::default();
    let mut out = Outline::default();
    run_charstring(font, cs, &mut points, Some(&mut out), false)?;
    out.end_contour();
    Ok(out)
}

/// The width operand of a Type 2 charstring as `cff_slot_load` finds it
/// for `FT_LOAD_ADVANCE_ONLY` (`width_only`): None for the default width;
/// an error where FreeType fails before the width.
pub(crate) fn charstring_width(
    font: &dyn Charstrings,
    cs: &[u8],
) -> Result<Option<f64>, LoadError> {
    run_charstring(font, cs, &mut Points::default(), None, true)
}

fn run_charstring(
    font: &dyn Charstrings,
    cs: &[u8],
    points: &mut Points,
    outline: Option<&mut Outline>,
    width_only: bool,
) -> Result<Option<f64>, LoadError> {
    let mut path = Path {
        points,
        outline,
        start: (0.0, 0.0),
        cur: (0.0, 0.0),
        move_pending: true,
        open: false,
    };
    let mut left_bearing = (0.0, 0.0);
    let mut instructions = 20_000_000u32;
    let mut interp = Interp {
        font,
        t1: font.is_t1(),
        stack: Vec::new(),
        path: &mut path,
        cur_x: 0.0,
        cur_y: 0.0,
        hstems: 0,
        vstems: 0,
        hintmask_valid: false,
        have_width: false,
        flex_state: false,
        num_flex: 0,
        flex_store: [0.0; 6],
        results: Vec::new(),
        known_results: 0,
        storage: [0.0; STORAGE_SIZE],
        large_int: false,
        left_bearing: &mut left_bearing,
        instructions: &mut instructions,
        doing_seac: false,
        width: None,
        width_only,
    };
    interp.run(cs)?;
    Ok(interp.width)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Plain;
    impl Charstrings for Plain {
        fn is_t1(&self) -> bool {
            false
        }
        fn local_subr(&self, _: i64) -> Option<&[u8]> {
            None
        }
        fn global_subr(&self, _: i64) -> Option<&[u8]> {
            None
        }
        fn seac_component(&self, _: i64) -> Option<&[u8]> {
            None
        }
    }

    fn num(v: i32) -> Vec<u8> {
        if (-107..=107).contains(&v) {
            vec![(v + 139) as u8]
        } else {
            let b = (v as i16).to_be_bytes();
            vec![28, b[0], b[1]]
        }
    }

    fn cs(ops: &[(&[i32], u8)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (args, op) in ops {
            for &a in *args {
                out.extend(num(a));
            }
            out.push(*op);
        }
        out
    }

    #[test]
    fn a_type2_box_counts_curve_control_points() {
        // width 500, a move to (100, 0), a line up and a curve that bows
        // out to x = 400 by its control points.
        let c = cs(&[
            (&[500, 100, 0], 21),
            (&[0, 700], 5),
            (&[300, 0, 0, -700, -300, 0], 8),
            (&[], 14),
        ]);
        let b = charstring_cbox(&Plain, &c).unwrap_or_else(|_| panic!("load"));
        assert_eq!(
            b,
            Some(CBox {
                x0: 100.0,
                y0: 0.0,
                x1: 400.0,
                y1: 700.0
            })
        );
    }

    #[test]
    fn a_lone_move_draws_nothing_and_a_bad_stack_fails() {
        let c = cs(&[(&[250, 30, 40], 21), (&[], 14)]);
        assert_eq!(charstring_cbox(&Plain, &c), Ok(None));
        assert_eq!(
            charstring_cbox(&Plain, &cs(&[(&[], 5 + 16)])),
            Err(LoadError)
        );
    }

    /// `loca` (long) and `glyf` for these glyph bodies, one after another.
    fn glyf_font(glyphs: &[Vec<u8>]) -> (Vec<u8>, usize) {
        let mut loca: Vec<u8> = Vec::new();
        let mut glyf: Vec<u8> = Vec::new();
        for g in glyphs {
            loca.extend((glyf.len() as u32).to_be_bytes());
            glyf.extend(g);
        }
        loca.extend((glyf.len() as u32).to_be_bytes());
        let at = loca.len();
        loca.extend(glyf);
        (loca, at)
    }

    fn words(v: &[i32]) -> Vec<u8> {
        v.iter().flat_map(|&w| (w as i16).to_be_bytes()).collect()
    }

    #[test]
    fn a_composite_is_its_components_scaled_and_moved() {
        // Glyph 1: a triangle, coordinates as words. Glyph 2: glyph 1 at
        // half size, moved by (50, 20). Glyph 3: a composite of itself.
        let mut simple = words(&[1, 0, 0, 100, 100]);
        simple.extend(words(&[2, 0]));
        simple.extend([1, 1, 1]);
        simple.extend(words(&[0, 100, -100, 0, 0, 100]));
        let mut composite = words(&[-1, 0, 0, 0, 0]);
        composite.extend(words(&[0x000b, 1, 50, 20, 0x2000]));
        let mut looped = words(&[-1, 0, 0, 0, 0]);
        looped.extend(words(&[0x0003, 3, 0, 0]));
        let (data, at) = glyf_font(&[Vec::new(), simple, composite, looped]);
        let g = Glyf {
            data: &data,
            loca: (0, at),
            long_loca: true,
            glyf: (at, data.len() - at),
            num_glyphs: 4,
        };
        let cbox = |gid| {
            truetype_points(&g, gid).map(|pts| {
                let mut b = Points::default();
                pts.iter().for_each(|&(x, y)| b.add(x as f64, y as f64));
                b.cbox()
            })
        };
        assert_eq!(cbox(0), Ok(None));
        assert_eq!(
            cbox(1),
            Ok(Some(CBox {
                x0: 0.0,
                y0: 0.0,
                x1: 100.0,
                y1: 100.0
            }))
        );
        assert_eq!(
            cbox(2),
            Ok(Some(CBox {
                x0: 50.0,
                y0: 20.0,
                x1: 100.0,
                y1: 70.0
            }))
        );
        assert_eq!(cbox(3), Err(LoadError));
        assert_eq!(cbox(4), Err(LoadError));
    }

    struct T1;
    impl Charstrings for T1 {
        fn is_t1(&self) -> bool {
            true
        }
        fn local_subr(&self, _: i64) -> Option<&[u8]> {
            None
        }
        fn global_subr(&self, _: i64) -> Option<&[u8]> {
            None
        }
        fn seac_component(&self, _: i64) -> Option<&[u8]> {
            None
        }
    }

    #[test]
    fn type1_flex_draws_its_two_curves_from_the_side_bearing() {
        let esc = |op: u8| vec![12, op];
        let mut c = cs(&[(&[0, 500], 13), (&[0, 0], 21)]);
        c.extend(num(0));
        c.extend(num(1));
        c.extend(esc(16));
        for (dx, dy) in [
            (10, 0),
            (10, 20),
            (10, 0),
            (10, -20),
            (10, -20),
            (10, 0),
            (10, 20),
        ] {
            c.extend(cs(&[(&[dx, dy], 21)]));
            c.extend(num(0));
            c.extend(num(2));
            c.extend(esc(16));
        }
        for v in [50, 70, 0, 3, 0] {
            c.extend(num(v));
        }
        c.extend(esc(16));
        c.extend(esc(17));
        c.extend(esc(17));
        c.extend(esc(33));
        c.extend(cs(&[(&[], 9), (&[], 14)]));
        let b = charstring_cbox(&T1, &c).unwrap_or_else(|_| panic!("load"));
        assert_eq!(
            b,
            Some(CBox {
                x0: 0.0,
                y0: -20.0,
                x1: 70.0,
                y1: 20.0
            })
        );
        // hsbw moves the start to the side bearing.
        let c = cs(&[
            (&[50, 500], 13),
            (&[0, 0], 21),
            (&[100, 0], 5),
            (&[0, 100], 5),
            (&[], 9),
            (&[], 14),
        ]);
        let b = charstring_cbox(&T1, &c).unwrap_or_else(|_| panic!("load"));
        assert_eq!(
            b,
            Some(CBox {
                x0: 50.0,
                y0: 0.0,
                x1: 150.0,
                y1: 100.0
            })
        );
    }

    #[test]
    fn fixed_multiplication_rounds_like_freetype() {
        assert_eq!(mul_fix(3, 0x8000), 2);
        assert_eq!(mul_fix(-3, 0x8000), -2);
        assert_eq!(mul_fix(1000, 0x10000), 1000);
    }

    /// Subroutines 0 to 39: each calls the next, and the last draws a
    /// line. Local subroutine numbers are unbiased here.
    struct Chain {
        t1: bool,
        subrs: Vec<Vec<u8>>,
    }

    impl Chain {
        fn new(t1: bool) -> Chain {
            let mut subrs: Vec<Vec<u8>> = (1..40).map(|i| cs(&[(&[i], 10), (&[], 11)])).collect();
            subrs.push(cs(&[(&[100, 100], 5), (&[], 11)]));
            Chain { t1, subrs }
        }
    }

    impl Charstrings for Chain {
        fn is_t1(&self) -> bool {
            self.t1
        }
        fn local_subr(&self, n: i64) -> Option<&[u8]> {
            self.subrs.get(usize::try_from(n).ok()?).map(Vec::as_slice)
        }
        fn global_subr(&self, n: i64) -> Option<&[u8]> {
            self.local_subr(n)
        }
        fn seac_component(&self, _: i64) -> Option<&[u8]> {
            None
        }
    }

    #[test]
    fn sixteen_levels_of_subroutines_and_no_more() {
        // MuPDF draws a Type 1 glyph sixteen subroutines deep and fails
        // one seventeen deep (cf2's subroutine stack); Type 2 alike.
        for t1 in [true, false] {
            let font = Chain::new(t1);
            let deep = |levels: i32, op: u8| {
                let start = if t1 {
                    vec![(&[0, 500][..], 13)]
                } else {
                    Vec::new()
                };
                let mut c = cs(&start);
                c.extend(cs(&[(&[0, 0], 21), (&[40 - levels], op), (&[], 14)]));
                charstring_outline(&font, &c).map(|o| o.points)
            };
            for op in [10, 29] {
                assert_eq!(deep(16, op), Ok(vec![(0, 0), (100, 100)]), "{t1} {op}");
                assert_eq!(deep(17, op), Err(LoadError), "{t1} {op}");
            }
        }
    }

    #[test]
    fn outlines_transform_translate_scale_and_bound() {
        let mut o = Outline::default();
        assert_eq!(o.cbox(), (0, 0, 0, 0));
        o.points = vec![(10, 20), (-30, 40), (50, -60)];
        assert_eq!(o.cbox(), (-30, -60, 50, 40));
        // A quarter turn: x' = -y, y' = x.
        o.transform(0, -0x10000, 0x10000, 0);
        assert_eq!(o.points, [(-20, 10), (-40, -30), (60, 50)]);
        o.translate(5, -5);
        assert_eq!(o.points, [(-15, 5), (-35, -35), (65, 45)]);
        o.scale(0x8000, 0x20000);
        assert_eq!(o.points, [(-8, 10), (-18, -70), (33, 90)]);
        assert_eq!(div_fix_int(1, 2), 0x8000);
        assert_eq!(div_fix_int(-3, 2), -0x18000);
        assert_eq!(div_fix_int(3, -4), -0xc000);
        assert_eq!(div_fix_int(5, 0), 0x7FFF_FFFF);
    }

    /// A glyph: its contour count and xMin, then its body.
    fn glyph(n_contours: i32, x_min: i32, body: &[u8]) -> Vec<u8> {
        let mut g = words(&[n_contours, x_min, 0, 0, 0]);
        g.extend(body);
        g
    }

    /// The triangle (10, 0) (10, 50) (-40, 0), the last point off the
    /// curve, its coordinates in every flag form: a positive short x, a
    /// repeated x, a negative short x; a repeated y, a positive short y, a
    /// word y.
    fn triangle(first_flag: u8) -> Vec<u8> {
        let mut b = words(&[2, 0]);
        b.extend([first_flag | 1 | 2 | 16 | 32, 1 | 16 | 4 | 32, 2]);
        b.extend([10, 50]);
        b.extend([50]);
        b.extend(words(&[-50]));
        glyph(1, -40, &b)
    }

    /// A composite of components (flags, glyph, then the bytes after).
    fn composite(parts: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
        let mut b = Vec::new();
        for (i, (flags, gid, rest)) in parts.iter().enumerate() {
            let more = if i + 1 < parts.len() {
                MORE_COMPONENTS
            } else {
                0
            };
            b.extend((flags | more).to_be_bytes());
            b.extend(gid.to_be_bytes());
            b.extend(rest);
        }
        glyph(-1, 0, &b)
    }

    fn points_of(glyphs: &[Vec<u8>], gid: u32) -> Result<Vec<(i64, i64)>, LoadError> {
        let (data, at) = glyf_font(glyphs);
        let g = Glyf {
            data: &data,
            loca: (0, at),
            long_loca: true,
            glyf: (at, data.len() - at),
            num_glyphs: glyphs.len() as u32,
        };
        truetype_points(&g, gid)
    }

    #[test]
    fn simple_glyphs_read_every_flag_form() {
        let tri = vec![(10, 0), (10, 50), (-40, 0)];
        assert_eq!(points_of(&[triangle(0)], 0), Ok(tri.clone()));
        // Repeated flags: three points of words.
        let mut b = words(&[2, 0]);
        b.extend([1 | ON_REPEAT, 2]);
        b.extend(words(&[1, 2, 3, 4, 5, 6]));
        assert_eq!(
            points_of(&[glyph(1, 0, &b)], 0),
            Ok(vec![(1, 4), (3, 9), (6, 15)])
        );
        // The outline keeps tags and contour ends; an overlap flag on the
        // first point marks it.
        let (data, at) = glyf_font(&[triangle(OVERLAP_SIMPLE)]);
        let g = Glyf {
            data: &data,
            loca: (0, at),
            long_loca: true,
            glyf: (at, data.len() - at),
            num_glyphs: 1,
        };
        let o =
            truetype_outline(&g, 0, (0x10000, 0x10000), -40).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            (o.points, o.tags, o.contours, o.overlap),
            (tri, vec![1, 1, 0], vec![2], true)
        );
    }

    #[test]
    fn broken_simple_glyphs_fail() {
        let fails = |g: Vec<u8>| {
            assert_eq!(
                points_of(std::slice::from_ref(&g), 0),
                Err(LoadError),
                "{g:?}"
            )
        };
        // Too many contours, ends that go back, too many points, a body
        // shorter than its ends, instructions past the end.
        fails(glyph(0xFFF, 0, &[0; 8]));
        fails(glyph(2, 0, &words(&[5, 3, 0])));
        fails(glyph(1, 0, &words(&[0xFFFC - 0x10000, 0])));
        fails(glyph(2, 0, &words(&[5])));
        fails(glyph(1, 0, &words(&[0, 9])));
        // Flags and coordinates cut short, a repeat past the points.
        fails(glyph(1, 0, &words(&[1, 0])));
        fails(glyph(1, 0, &[0, 1, 0, 0, 1 | ON_REPEAT]));
        fails(glyph(1, 0, &[0, 1, 0, 0, 1 | ON_REPEAT, 5]));
        fails(glyph(1, 0, &[0, 0, 0, 0, 2]));
        fails(glyph(1, 0, &[0, 0, 0, 0, 0, 0]));
        // A header shorter than ten bytes.
        fails(vec![0, 1, 0, 0]);
        // An empty glyph with a header has no points.
        assert_eq!(points_of(&[glyph(0, 0, &[])], 0), Ok(Vec::new()));
    }

    #[test]
    fn composites_scale_move_and_match_points() {
        let f2 = |v: f64| ((v * 16384.0) as i16).to_be_bytes().to_vec();
        let i8s = |a: i8, b: i8| vec![a as u8, b as u8];
        // Glyph 1 moved by words; glyph 1 at half size, moved by bytes
        // scaled with it.
        let c2 = composite(&[
            (ARGS_ARE_WORDS | ARGS_ARE_XY_VALUES, 1, words(&[-20, 30])),
            (
                ARGS_ARE_XY_VALUES | WE_HAVE_A_SCALE | SCALED_COMPONENT_OFFSET,
                1,
                [i8s(5, -5), f2(0.5)].concat(),
            ),
        ]);
        // Glyph 1 stretched in y, then glyph 1 turned a quarter and placed
        // so its point 0 lies on point 1 of the first.
        let c3 = composite(&[
            (
                ARGS_ARE_XY_VALUES | WE_HAVE_AN_XY_SCALE,
                1,
                [vec![0, 0], f2(1.0), f2(1.5)].concat(),
            ),
            (
                WE_HAVE_A_2X2,
                1,
                [vec![1, 0], f2(0.0), f2(1.0), f2(-1.0), f2(0.0)].concat(),
            ),
        ]);
        // Point numbers as words; one past the points so far; an empty
        // component; a component that is not there.
        let c4 = composite(&[
            (ARGS_ARE_XY_VALUES, 0, vec![9, 9]),
            (ARGS_ARE_XY_VALUES, 1, vec![0, 0]),
            (ARGS_ARE_WORDS, 1, words(&[2, 1])),
        ]);
        let c5 = composite(&[(0, 1, vec![0, 0]), (0, 1, vec![3, 0])]);
        let c6 = composite(&[(ARGS_ARE_XY_VALUES, 9, vec![0, 0])]);
        let c7 = composite(&[(ARGS_ARE_XY_VALUES | WE_HAVE_A_2X2, 1, vec![0, 0])]);
        let glyphs = [Vec::new(), triangle(0), c2, c3, c4, c5, c6, c7];
        assert_eq!(
            points_of(&glyphs, 2),
            Ok(vec![
                (-10, 30),
                (-10, 80),
                (-60, 30),
                (8, -3),
                (8, 22),
                (-17, -3)
            ])
        );
        assert_eq!(
            points_of(&glyphs, 3),
            Ok(vec![
                (10, 0),
                (10, 75),
                (-40, 0),
                (10, 75),
                (-40, 75),
                (10, 25)
            ])
        );
        assert_eq!(
            points_of(&glyphs, 4),
            Ok(vec![
                (10, 0),
                (10, 50),
                (-40, 0),
                (-40, -50),
                (-40, 0),
                (-90, -50)
            ])
        );
        assert_eq!(points_of(&glyphs, 5), Err(LoadError));
        assert_eq!(points_of(&glyphs, 6), Err(LoadError));
        assert_eq!(points_of(&glyphs, 7), Err(LoadError));
    }

    #[test]
    fn composites_scale_their_offsets_with_the_outline() {
        let c = composite(&[
            (OVERLAP_COMPOUND | ARGS_ARE_XY_VALUES, 1, vec![10, 0]),
            (ARGS_ARE_XY_VALUES, 1, vec![0, 0]),
        ]);
        let (data, at) = glyf_font(&[triangle(0), triangle(0), c]);
        let g = Glyf {
            data: &data,
            loca: (0, at),
            long_loca: true,
            glyf: (at, data.len() - at),
            num_glyphs: 3,
        };
        // At twice the size, the offset doubles; pp1 (xMin less lsb) moves
        // the outline to the origin.
        let o = truetype_outline(&g, 2, (0x20000, 0x20000), 0).unwrap_or_else(|e| panic!("{e:?}"));
        assert!(o.overlap);
        assert_eq!(
            o.points,
            [(40, 0), (40, 100), (-60, 0), (20, 0), (20, 100), (-80, 0)]
        );
        let o =
            truetype_outline(&g, 1, (0x20000, 0x20000), -50).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(o.points, [(0, 0), (0, 100), (-100, 0)]);
    }

    #[test]
    fn composites_nest_only_so_deep_and_so_wide() {
        // A chain of 102 composites, each of the next.
        let mut chain: Vec<Vec<u8>> = (1..102)
            .map(|i| composite(&[(ARGS_ARE_XY_VALUES, i, vec![0, 0])]))
            .collect();
        chain.push(triangle(0));
        assert_eq!(points_of(&chain, 1).map(|p| p.len()), Ok(3));
        assert_eq!(points_of(&chain, 0), Err(LoadError));
        // A tree of composites with two of the next, 17 deep: more loads
        // than the budget allows.
        let mut tree: Vec<Vec<u8>> = (1..18)
            .map(|i| {
                composite(&[
                    (ARGS_ARE_XY_VALUES, i, vec![0, 0]),
                    (ARGS_ARE_XY_VALUES, i, vec![0, 0]),
                ])
            })
            .collect();
        tree.push(glyph(0, 0, &[]));
        assert_eq!(points_of(&tree, 3), Ok(Vec::new()));
        assert_eq!(points_of(&tree, 0), Err(LoadError));
    }

    #[test]
    fn loca_entries_short_long_and_past_the_table() {
        // Short loca: offsets in words. Glyph 1 is empty, glyph 2 runs to
        // the end of glyf though its next offset is past it.
        let tri = triangle(0);
        let mut glyf = tri.clone();
        glyf.extend(&tri);
        let n = (tri.len() / 2) as u16;
        let loca: Vec<u8> = [0, n, n, 3 * n]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        let data = [loca.clone(), glyf.clone()].concat();
        let short = Glyf {
            data: &data,
            loca: (0, loca.len()),
            long_loca: false,
            glyf: (loca.len(), glyf.len()),
            num_glyphs: 4,
        };
        assert_eq!(short.location(0), (0, tri.len()));
        assert_eq!(short.location(1), (tri.len(), 0));
        assert_eq!(short.location(2), (tri.len(), tri.len()));
        // The last entry has no next one; past the entries, nothing.
        assert_eq!(short.location(3), (0, 0));
        assert_eq!(short.location(9), (0, 0));
        assert_eq!(truetype_points(&short, 2).map(|p| p.len()), Ok(3));
        assert_eq!(truetype_points(&short, 9), Err(LoadError));
        // Long loca: an offset past glyf, one that goes back, and an end
        // past glyf that is not the last.
        let len = glyf.len() as u32;
        let loca: Vec<u8> = [0, len + 8, 40, 20, len + 4, len]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        let data = [loca.clone(), glyf.clone()].concat();
        let long = Glyf {
            data: &data,
            loca: (0, loca.len()),
            long_loca: true,
            glyf: (loca.len(), glyf.len()),
            num_glyphs: 6,
        };
        assert_eq!(long.location(0), (0, 0));
        assert_eq!(long.location(1), (0, 0));
        assert_eq!(long.location(2), (40, glyf.len() - 40));
        assert_eq!(long.location(3), (0, 0));
        assert_eq!(long.location(4), (0, 0));
        assert_eq!(long.location(5), (glyf.len(), 0));
        // A glyph whose data runs past the font.
        let cut = Glyf {
            data: &data[..data.len() - 4],
            ..long
        };
        let loca: Vec<u8> = [0, len].iter().flat_map(|v| v.to_be_bytes()).collect();
        let data2 = [loca.clone(), glyf].concat();
        let past = Glyf {
            data: &data2[..data2.len() - 1],
            loca: (0, loca.len()),
            long_loca: true,
            glyf: (loca.len(), len as usize),
            num_glyphs: 1,
        };
        assert_eq!(truetype_points(&past, 0), Err(LoadError));
        assert_eq!(cut.location(9), (0, 0));
    }
}
