// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/shade.c (fz_process_shade) and
// source/fitz/draw-mesh.c (fz_paint_shade, fz_paint_triangle),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Shadings as MuPDF paints them: every type turned into triangles, each
//! filled with colors interpolated in 16.16 fixed point; shadings with a
//! function interpolate its parameter and look the color up afterwards.

use crate::colorspace::{ColorSpace, MAX_COLORS};
use crate::document::Document;
use crate::fma::Madd;
use crate::geometry::{IRect, Matrix, Point, Rect};
use crate::shade::{Shade, ShadeData};

use super::color::Converter;
use super::paint::{Pix, mul255};

const MAXN: usize = 2 + MAX_COLORS;
const HUGENUM: f32 = 32000.0;
const SUBDIV: u32 = 3;

/// A vertex as `fz_paint_triangle` reads it: x, y, then the values to
/// interpolate (0..255).
type Vert = [f32; MAXN];

/// `paint_scan`.
#[allow(clippy::too_many_arguments)]
fn paint_scan(
    pix: &mut Pix,
    y: i32,
    fx0: i32,
    fx1: i32,
    cx0: i32,
    cx1: i32,
    v0: &[i32],
    v1: &[i32],
    n: usize,
) {
    let (mut fx0, mut fx1, mut v0, mut v1) = (fx0, fx1, v0, v1);
    if fx0 > fx1 {
        std::mem::swap(&mut fx0, &mut fx1);
        std::mem::swap(&mut v0, &mut v1);
    } else if fx0 == fx1 {
        return;
    }
    if fx0 >= cx1 || fx1 <= cx0 {
        return;
    }
    if y < pix.y || y >= pix.y + pix.h {
        return;
    }
    let x0 = fx0.max(cx0);
    let x1 = fx1.min(cx1);
    let w = x1 - x0;
    if w <= 0 {
        return;
    }
    let div = 1.0f32 / (fx1 - fx0) as f32;
    let mul = (x0 - fx0) as f32;
    let mut c = [0i32; MAXN];
    let mut dc = [0i32; MAXN];
    for k in 0..n {
        dc[k] = ((v1[k] - v0[k]) as f32 * div) as i32;
        c[k] = (v0[k] as f32 + dc[k] as f32 * mul) as i32;
    }
    let pa = pix.alpha;
    let stride = pix.w as usize * pix.n;
    let mut p = (y - pix.y) as usize * stride + (x0 - pix.x) as usize * pix.n;
    for _ in 0..w {
        for k in 0..n {
            if let Some(d) = pix.samples.get_mut(p) {
                *d = (c[k] >> 16) as u8;
            }
            p += 1;
            c[k] = c[k].wrapping_add(dc[k]);
        }
        if pa {
            if let Some(d) = pix.samples.get_mut(p) {
                *d = 255;
            }
            p += 1;
        }
    }
}

struct Edge {
    x: f32,
    dx: f32,
    v: [i32; 2 * MAXN],
}

/// `prepare_edge`, its multiply-adds fused as clang fuses them (D39).
fn prepare_edge(vtop: &Vert, vbot: &Vert, y: f32, n: usize) -> Edge {
    let r = 1.0f32 / (vbot[1] - vtop[1]);
    let t = (y - vtop[1]) * r;
    let diff = vbot[0] - vtop[0];
    let mut e = Edge {
        x: diff.madd(t, vtop[0]),
        dx: diff * r,
        v: [0; 2 * MAXN],
    };
    for i in 0..n {
        let diff = vbot[i + 2] - vtop[i + 2];
        e.v[i] = (65536.0f32 * diff.madd(t, vtop[i + 2])) as i32;
        e.v[i + MAXN] = (65536.0f32 * diff * r) as i32;
    }
    e
}

fn step_edge(e: &mut Edge, n: usize) {
    e.x += e.dx;
    for i in 0..n {
        e.v[i] = e.v[i].wrapping_add(e.v[i + MAXN]);
    }
}

/// `fz_paint_triangle`: `n` counts x and y.
fn paint_triangle(pix: &mut Pix, v: [&Vert; 3], n: usize, bbox: IRect) {
    let (mut top, mut bot) = (0usize, 0usize);
    if v[1][1] < v[0][1] {
        top = 1;
    } else {
        bot = 1;
    }
    if v[2][1] < v[top][1] {
        top = 2;
    } else if v[2][1] > v[bot][1] {
        bot = 2;
    }
    if v[top][1] == v[bot][1] {
        return;
    }
    if v[bot][1] < bbox.y0 as f32 || v[top][1] > bbox.y1 as f32 {
        return;
    }
    let mid = 3 ^ top ^ bot;
    if mid > 2 {
        return;
    }
    let minx = bbox.x0.max(pix.x);
    let maxx = bbox.x1.min(pix.x + pix.w);
    let mut y = (bbox.y0 as f32).max(v[top][1]).ceil();
    let mut y1 = (bbox.y1 as f32).min(v[mid][1]).ceil();
    let n = n - 2;
    let mut e0 = prepare_edge(v[top], v[bot], y, n);
    if y < y1 {
        let mut e1 = prepare_edge(v[top], v[mid], y, n);
        loop {
            paint_scan(
                pix,
                y as i32,
                e0.x as i32,
                e1.x as i32,
                minx,
                maxx,
                &e0.v[..],
                &e1.v[..],
                n,
            );
            step_edge(&mut e0, n);
            step_edge(&mut e1, n);
            y += 1.0;
            if y >= y1 {
                break;
            }
        }
    }
    y1 = (bbox.y1 as f32).min(v[bot][1]).ceil();
    if y < y1 {
        let mut e1 = prepare_edge(v[mid], v[bot], y, n);
        loop {
            paint_scan(
                pix,
                y as i32,
                e0.x as i32,
                e1.x as i32,
                minx,
                maxx,
                &e0.v[..],
                &e1.v[..],
                n,
            );
            y += 1.0;
            if y >= y1 {
                break;
            }
            step_edge(&mut e0, n);
            step_edge(&mut e1, n);
        }
    }
}

/// What turns a shading's input colors into vertex values, and paints
/// the triangles: `prepare_mesh_vertex` and `do_paint_tri`.
struct Painter<'a> {
    doc: Option<&'a Document>,
    cc: &'a Converter,
    /// The rendering intent colors convert with.
    ri: u8,
    shade: &'a Shade,
    dest: &'a mut Pix,
    bbox: IRect,
    /// The colorspace the vertex colors convert to: None for a function
    /// parameter, Some(n) for n device colors (1 gray, 3 RGB, 0 none).
    colors: Option<usize>,
    ncomp: usize,
    /// Triangles painted so far, bounded.
    budget: u64,
}

impl Painter<'_> {
    fn prepare(&self, x: f32, y: f32, input: &[f32]) -> Vert {
        let mut v = [0f32; MAXN];
        v[0] = x;
        v[1] = y;
        match self.colors {
            None => {
                let mut f = input.first().copied().unwrap_or(0.0);
                if let ShadeData::Mesh(m) = &self.shade.data {
                    f = (f - m.c0[0]) / (m.c1[0] - m.c0[0]);
                }
                v[2] = f * 255.0;
            }
            Some(n) => {
                let out = convert(self.doc, self.cc, self.ri, &self.shade.colorspace, input, n);
                for k in 0..n {
                    v[2 + k] = out[k] * 255.0;
                }
            }
        }
        v
    }

    fn tri(&mut self, a: &Vert, b: &Vert, c: &Vert) {
        if self.budget == 0 {
            return;
        }
        self.budget -= 1;
        let n = 2 + self.dest.n - usize::from(self.dest.alpha);
        paint_triangle(self.dest, [a, b, c], n, self.bbox);
    }

    /// `paint_quad`.
    fn quad(&mut self, v0: &Vert, v1: &Vert, v2: &Vert, v3: &Vert) {
        self.tri(v0, v1, v3);
        self.tri(v3, v2, v1);
    }
}

/// A color in `cs` as `n` device components (1 gray, 3 RGB).
fn convert(
    doc: Option<&Document>,
    cc: &Converter,
    ri: u8,
    cs: &ColorSpace,
    input: &[f32],
    n: usize,
) -> [f32; 3] {
    match n {
        0 => [0.0; 3],
        1 => [cc.gray(doc, cs, input, ri), 0.0, 0.0],
        _ => cc.rgb(doc, cs, input, ri),
    }
}

fn at(p: Point, m: Matrix) -> Point {
    m.transform_xy(p.x, p.y)
}

fn on_circle(p: Point, r: f32, theta: f32) -> Point {
    Point::new(p.x + theta.cos() * r, p.y + theta.sin() * r)
}

fn with_pos(mut v: Vert, p: Point) -> Vert {
    v[0] = p.x;
    v[1] = p.y;
    v
}

fn type1(pt: &mut Painter<'_>, ctm: Matrix) {
    let ShadeData::Function {
        matrix,
        domain,
        divs,
        values,
    } = &pt.shade.data
    else {
        return;
    };
    let (xdivs, ydivs) = (*divs, *divs);
    let n = pt.shade.colorspace.n();
    let ctm = matrix.concat(ctm);
    let [[x0, y0], [x1, y1]] = *domain;
    let get = |i: usize| -> Vec<f32> {
        (0..n)
            .map(|k| values.get(i * n + k).copied().unwrap_or(0.0))
            .collect()
    };
    let vert = |pt: &Painter<'_>, x: f32, y: f32, i: usize| {
        let p = ctm.transform_xy(x, y);
        pt.prepare(p.x, p.y, &get(i))
    };
    let mut y = y0;
    let mut p = 0usize;
    for yy in 0..ydivs {
        let yn = y0 + (y1 - y0) * (yy + 1) as f32 / ydivs as f32;
        let mut x = x0;
        let mut v0 = vert(pt, x, y, p);
        p += 1;
        let mut v1 = vert(pt, x, yn, p + xdivs);
        for xx in 0..xdivs {
            x = x0 + (x1 - x0) * (xx + 1) as f32 / xdivs as f32;
            let n0 = vert(pt, x, y, p);
            p += 1;
            let n1 = vert(pt, x, yn, p + xdivs);
            pt.quad(&v0, &n0, &n1, &v1);
            v0 = n0;
            v1 = n1;
        }
        y = yn;
    }
}

fn type2(pt: &mut Painter<'_>, ctm: Matrix, scissor: Rect) {
    let ShadeData::Axial { coords, extend } = &pt.shade.data else {
        return;
    };
    let mut p0 = Point::new(coords[0][0], coords[0][1]);
    let mut p1 = Point::new(coords[1][0], coords[1][1]);
    let dir = Point::new(p0.y - p1.y, p1.x - p0.x);
    p0 = at(p0, ctm);
    p1 = at(p1, ctm);
    let dir = dir.transform_vector(ctm);
    let theta = dir.y.atan2(dir.x);
    let mut r = if scissor.is_infinite() {
        HUGENUM
    } else {
        let mut x = p0.x - scissor.x0;
        let mut y = p0.y - scissor.y0;
        if x < scissor.x1 - p0.x {
            x = scissor.x1 - p0.x;
        }
        if x < p0.x - scissor.x1 {
            x = p0.x - scissor.x1;
        }
        if x < scissor.x1 - p1.x {
            x = scissor.x1 - p1.x;
        }
        if y < scissor.y1 - p0.y {
            y = scissor.y1 - p0.y;
        }
        if y < p0.y - scissor.y1 {
            y = p0.y - scissor.y1;
        }
        if y < scissor.y1 - p1.y {
            y = scissor.y1 - p1.y;
        }
        x + y
    };
    let c0 = pt.prepare(0.0, 0.0, &[0.0]);
    let c1 = pt.prepare(0.0, 0.0, &[1.0]);
    let a0 = on_circle(p0, r, theta);
    let a1 = on_circle(p1, r, theta);
    let a2 = Point::new(2.0 * p0.x - a0.x, 2.0 * p0.y - a0.y);
    let a3 = Point::new(2.0 * p1.x - a1.x, 2.0 * p1.y - a1.y);
    let (v0, v1, v2, v3) = (
        with_pos(c0, a0),
        with_pos(c1, a1),
        with_pos(c0, a2),
        with_pos(c1, a3),
    );
    pt.quad(&v0, &v2, &v3, &v1);
    if extend[0] || extend[1] {
        let d = (p1.x - p0.x).abs().max((p1.y - p0.y).abs());
        if d != 0.0 {
            r /= d;
        }
    }
    let (dx, dy) = (p1.x - p0.x, p1.y - p0.y);
    if extend[0] {
        let e0 = with_pos(c0, Point::new(a0.x - dx * r, a0.y - dy * r));
        let e1 = with_pos(c0, Point::new(a2.x - dx * r, a2.y - dy * r));
        pt.quad(&e0, &v0, &v2, &e1);
    }
    if extend[1] {
        let e0 = with_pos(c1, Point::new(a1.x + dx * r, a1.y + dy * r));
        let e1 = with_pos(c1, Point::new(a3.x + dx * r, a3.y + dy * r));
        pt.quad(&e0, &v1, &v3, &e1);
    }
}

#[allow(clippy::too_many_arguments)]
fn annulus(
    pt: &mut Painter<'_>,
    ctm: Matrix,
    p0: Point,
    r0: f32,
    c0: f32,
    p1: Point,
    r1: f32,
    c1: f32,
    count: i32,
) {
    let theta = (p1.y - p0.y).atan2(p1.x - p0.x);
    let step = std::f32::consts::PI / count as f32;
    let k0 = pt.prepare(0.0, 0.0, &[c0]);
    let k1 = pt.prepare(0.0, 0.0, &[c1]);
    let mut a = 0.0f32;
    for i in 1..=count {
        let b = i as f32 * step;
        let t0 = with_pos(k0, at(on_circle(p0, r0, theta + a), ctm));
        let t1 = with_pos(k0, at(on_circle(p0, r0, theta + b), ctm));
        let t2 = with_pos(k1, at(on_circle(p1, r1, theta + a), ctm));
        let t3 = with_pos(k1, at(on_circle(p1, r1, theta + b), ctm));
        let b0 = with_pos(k0, at(on_circle(p0, r0, theta - a), ctm));
        let b1 = with_pos(k0, at(on_circle(p0, r0, theta - b), ctm));
        let b2 = with_pos(k1, at(on_circle(p1, r1, theta - a), ctm));
        let b3 = with_pos(k1, at(on_circle(p1, r1, theta - b), ctm));
        pt.quad(&t0, &t2, &t3, &t1);
        pt.quad(&b0, &b2, &b3, &b1);
        a = b;
    }
}

fn type3(pt: &mut Painter<'_>, ctm: Matrix) {
    let ShadeData::Axial { coords, extend } = &pt.shade.data else {
        return;
    };
    let (extend, coords) = (*extend, *coords);
    let p0 = Point::new(coords[0][0], coords[0][1]);
    let r0 = coords[0][2];
    let p1 = Point::new(coords[1][0], coords[1][1]);
    let r1 = coords[1][2];
    let count = ((4.0 * (ctm.expansion() * r0.max(r1)).sqrt()) as i32).clamp(3, 1024);
    if extend[0] {
        let rs = if r0 < r1 { r0 / (r0 - r1) } else { -HUGENUM };
        let e = Point::new(p0.x + (p1.x - p0.x) * rs, p0.y + (p1.y - p0.y) * rs);
        let er = r0 + (r1 - r0) * rs;
        annulus(pt, ctm, e, er, 0.0, p0, r0, 0.0, count);
    }
    annulus(pt, ctm, p0, r0, 0.0, p1, r1, 1.0, count);
    if extend[1] {
        let rs = if r0 > r1 { r1 / (r1 - r0) } else { -HUGENUM };
        let e = Point::new(p1.x + (p0.x - p1.x) * rs, p1.y + (p0.y - p1.y) * rs);
        let er = r1 + (r0 - r1) * rs;
        annulus(pt, ctm, p1, r1, 1.0, e, er, 1.0, count);
    }
}

/// `fz_read_bits` over a buffer: past the end every byte reads as EOF.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    bits: i32,
    avail: u32,
}

impl Bits<'_> {
    fn byte(&mut self) -> i32 {
        match self.data.get(self.pos) {
            Some(&b) => {
                self.pos += 1;
                i32::from(b)
            }
            None => -1,
        }
    }

    fn read(&mut self, n: u32) -> u32 {
        let mut n = n.min(32);
        if n <= self.avail {
            self.avail -= n;
            return ((self.bits >> self.avail) as u32) & ((1u64 << n) - 1) as u32;
        }
        let mut x = (self.bits as u32) & ((1u32 << self.avail) - 1);
        n -= self.avail;
        self.avail = 0;
        while n > 8 {
            x = (x << 8) | self.byte() as u32;
            n -= 8;
        }
        if n > 0 {
            self.bits = self.byte();
            self.avail = 8 - n;
            x = (x << n) | ((self.bits >> self.avail) as u32 & ((1u32 << n) - 1));
        }
        x
    }

    fn eof(&self) -> bool {
        self.pos >= self.data.len() && (self.avail == 0 || self.bits == -1)
    }

    /// `read_sample`.
    fn sample(&mut self, bits: i32, min: f32, max: f32) -> f32 {
        let bitscale = 1.0 / (2f32.powi(bits) - 1.0);
        min + self.read(bits.max(0) as u32) as f32 * (max - min) * bitscale
    }
}

fn mesh_vertex(pt: &Painter<'_>, b: &mut Bits<'_>, ctm: Matrix) -> Vert {
    let ShadeData::Mesh(m) = &pt.shade.data else {
        return [0.0; MAXN];
    };
    let x = b.sample(m.bits_per_coordinate, m.x[0], m.x[1]);
    let y = b.sample(m.bits_per_coordinate, m.y[0], m.y[1]);
    let mut c = [0f32; MAX_COLORS];
    for (i, v) in c.iter_mut().enumerate().take(pt.ncomp) {
        *v = b.sample(m.bits_per_component, m.c0[i], m.c1[i]);
    }
    let p = ctm.transform_xy(x, y);
    pt.prepare(p.x, p.y, &c[..pt.ncomp])
}

fn type4(pt: &mut Painter<'_>, ctm: Matrix) {
    let ShadeData::Mesh(m) = &pt.shade.data else {
        return;
    };
    let m = m.clone();
    let mut b = Bits {
        data: &m.data,
        pos: 0,
        bits: 0,
        avail: 0,
    };
    let mut v = [[0f32; MAXN]; 4];
    let (mut va, mut vb, mut vc, mut vd) = (0usize, 1usize, 2usize, 3usize);
    let mut first = true;
    let flag_bits = m.bits_per_flag.max(0) as u32;
    while !b.eof() && pt.budget > 0 {
        let mut flag = b.read(flag_bits);
        v[vd] = mesh_vertex(pt, &mut b, ctm);
        if first {
            flag = 0;
            first = false;
        }
        match flag {
            1 => {
                std::mem::swap(&mut va, &mut vb);
                std::mem::swap(&mut vb, &mut vc);
                std::mem::swap(&mut vc, &mut vd);
            }
            2 => {
                std::mem::swap(&mut vb, &mut vc);
                std::mem::swap(&mut vc, &mut vd);
            }
            _ => {
                std::mem::swap(&mut va, &mut vd);
                b.read(flag_bits);
                v[vb] = mesh_vertex(pt, &mut b, ctm);
                b.read(flag_bits);
                v[vc] = mesh_vertex(pt, &mut b, ctm);
            }
        }
        let (a, bb, c) = (v[va], v[vb], v[vc]);
        pt.tri(&a, &bb, &c);
    }
}

fn type5(pt: &mut Painter<'_>, ctm: Matrix) {
    let ShadeData::Mesh(m) = &pt.shade.data else {
        return;
    };
    let m = m.clone();
    let vprow = m.vertices_per_row.max(0) as usize;
    if vprow == 0 {
        return;
    }
    let mut b = Bits {
        data: &m.data,
        pos: 0,
        bits: 0,
        avail: 0,
    };
    let mut reference: Vec<Vert> = vec![[0.0; MAXN]; vprow];
    let mut first = true;
    while !b.eof() && pt.budget > 0 {
        let buf: Vec<Vert> = (0..vprow).map(|_| mesh_vertex(pt, &mut b, ctm)).collect();
        if !first {
            for i in 0..vprow - 1 {
                pt.quad(&reference[i], &reference[i + 1], &buf[i + 1], &buf[i]);
            }
        }
        reference = buf;
        first = false;
    }
}

#[derive(Clone, Copy)]
struct Patch {
    pole: [[Point; 4]; 4],
    color: [[f32; MAX_COLORS]; 4],
}

/// `split_curve`: the poles at `idx(0..4)` halved into `q0` and `q1`.
fn split_curve(p: [Point; 4]) -> ([Point; 4], [Point; 4]) {
    let x12 = (p[1].x + p[2].x) * 0.5;
    let y12 = (p[1].y + p[2].y) * 0.5;
    let mut q0 = [Point::new(0.0, 0.0); 4];
    let mut q1 = [Point::new(0.0, 0.0); 4];
    q0[1] = Point::new((p[0].x + p[1].x) * 0.5, (p[0].y + p[1].y) * 0.5);
    q1[2] = Point::new((p[2].x + p[3].x) * 0.5, (p[2].y + p[3].y) * 0.5);
    q0[2] = Point::new((q0[1].x + x12) * 0.5, (q0[1].y + y12) * 0.5);
    q1[1] = Point::new((x12 + q1[2].x) * 0.5, (y12 + q1[2].y) * 0.5);
    q0[3] = Point::new((q0[2].x + q1[1].x) * 0.5, (q0[2].y + q1[1].y) * 0.5);
    q1[0] = q0[3];
    q0[0] = p[0];
    q1[3] = p[3];
    (q0, q1)
}

fn mid(a: &[f32; MAX_COLORS], b: &[f32; MAX_COLORS], n: usize) -> [f32; MAX_COLORS] {
    let mut c = [0f32; MAX_COLORS];
    for i in 0..n {
        c[i] = (a[i] + b[i]) * 0.5;
    }
    c
}

fn split_stripe(p: &Patch, n: usize) -> (Patch, Patch) {
    let mut s0 = *p;
    let mut s1 = *p;
    for j in 0..4 {
        let col = [p.pole[0][j], p.pole[1][j], p.pole[2][j], p.pole[3][j]];
        let (a, b) = split_curve(col);
        for i in 0..4 {
            s0.pole[i][j] = a[i];
            s1.pole[i][j] = b[i];
        }
    }
    s0.color[0] = p.color[0];
    s0.color[1] = p.color[1];
    s0.color[2] = mid(&p.color[1], &p.color[2], n);
    s0.color[3] = mid(&p.color[0], &p.color[3], n);
    s1.color[0] = s0.color[3];
    s1.color[1] = s0.color[2];
    s1.color[2] = p.color[2];
    s1.color[3] = p.color[3];
    (s0, s1)
}

fn split_patch(p: &Patch, n: usize) -> (Patch, Patch) {
    let mut s0 = *p;
    let mut s1 = *p;
    for i in 0..4 {
        let (a, b) = split_curve(p.pole[i]);
        s0.pole[i] = a;
        s1.pole[i] = b;
    }
    s0.color[0] = p.color[0];
    s0.color[1] = mid(&p.color[0], &p.color[1], n);
    s0.color[2] = mid(&p.color[2], &p.color[3], n);
    s0.color[3] = p.color[3];
    s1.color[0] = s0.color[1];
    s1.color[1] = p.color[1];
    s1.color[2] = p.color[2];
    s1.color[3] = s0.color[2];
    (s0, s1)
}

fn triangulate(pt: &mut Painter<'_>, p: &Patch) {
    let n = pt.ncomp;
    let v = |pt: &Painter<'_>, q: Point, c: &[f32; MAX_COLORS]| pt.prepare(q.x, q.y, &c[..n]);
    let v0 = v(pt, p.pole[0][0], &p.color[0]);
    let v1 = v(pt, p.pole[0][3], &p.color[1]);
    let v2 = v(pt, p.pole[3][3], &p.color[2]);
    let v3 = v(pt, p.pole[3][0], &p.color[3]);
    pt.quad(&v0, &v1, &v2, &v3);
}

fn draw_stripe(pt: &mut Painter<'_>, p: &Patch, depth: u32) {
    let (s0, s1) = split_stripe(p, pt.ncomp);
    if depth <= 1 {
        triangulate(pt, &s1);
        triangulate(pt, &s0);
    } else {
        draw_stripe(pt, &s1, depth - 1);
        draw_stripe(pt, &s0, depth - 1);
    }
}

fn draw_patch(pt: &mut Painter<'_>, p: &Patch, depth: u32, origdepth: u32) {
    let (s0, s1) = split_patch(p, pt.ncomp);
    if depth <= 1 {
        draw_stripe(pt, &s0, origdepth);
        draw_stripe(pt, &s1, origdepth);
    } else {
        draw_patch(pt, &s0, depth - 1, origdepth);
        draw_patch(pt, &s1, depth - 1, origdepth);
    }
}

#[allow(clippy::too_many_arguments)]
fn tensor_interior(
    a: Point,
    b: Point,
    c: Point,
    d: Point,
    e: Point,
    f: Point,
    g: Point,
    h: Point,
) -> Point {
    let one = |a: f32, b: f32, c: f32, d: f32, e: f32, f: f32, g: f32, h: f32| {
        let mut x = -4.0 * a;
        x += 6.0 * (b + c);
        x += -2.0 * (d + e);
        x += 3.0 * (f + g);
        x += -h;
        x / 9.0
    };
    Point::new(
        one(a.x, b.x, c.x, d.x, e.x, f.x, g.x, h.x),
        one(a.y, b.y, c.y, d.y, e.y, f.y, g.y, h.y),
    )
}

fn make_patch(kind: i32, pt: &[Point; 16]) -> [[Point; 4]; 4] {
    let mut p = [[Point::new(0.0, 0.0); 4]; 4];
    p[0] = [pt[0], pt[1], pt[2], pt[3]];
    p[1][3] = pt[4];
    p[2][3] = pt[5];
    p[3][3] = pt[6];
    p[3][2] = pt[7];
    p[3][1] = pt[8];
    p[3][0] = pt[9];
    p[2][0] = pt[10];
    p[1][0] = pt[11];
    if kind == 6 {
        p[1][1] = tensor_interior(
            p[0][0], p[0][1], p[1][0], p[0][3], p[3][0], p[3][1], p[1][3], p[3][3],
        );
        p[1][2] = tensor_interior(
            p[0][3], p[0][2], p[1][3], p[0][0], p[3][3], p[3][2], p[1][0], p[3][0],
        );
        p[2][1] = tensor_interior(
            p[3][0], p[3][1], p[2][0], p[3][3], p[0][0], p[0][1], p[2][3], p[0][3],
        );
        p[2][2] = tensor_interior(
            p[3][3], p[3][2], p[2][3], p[3][0], p[0][3], p[0][2], p[2][0], p[0][0],
        );
    } else {
        p[1][1] = pt[12];
        p[1][2] = pt[13];
        p[2][2] = pt[14];
        p[2][1] = pt[15];
    }
    p
}

fn type67(pt: &mut Painter<'_>, ctm: Matrix, kind: i32) {
    let ShadeData::Mesh(m) = &pt.shade.data else {
        return;
    };
    let m = m.clone();
    let mut b = Bits {
        data: &m.data,
        pos: 0,
        bits: 0,
        avail: 0,
    };
    let npts = if kind == 6 { 12 } else { 16 };
    let ncomp = pt.ncomp;
    let mut prev: Option<([Point; 16], [[f32; MAX_COLORS]; 4])> = None;
    while !b.eof() && pt.budget > 0 {
        let mut v = [Point::new(0.0, 0.0); 16];
        let mut c = [[0f32; MAX_COLORS]; 4];
        let flag = b.read(m.bits_per_flag.max(0) as u32);
        let (startpt, startcolor) = if flag == 0 { (0, 0) } else { (4, 2) };
        for p in v.iter_mut().take(npts).skip(startpt) {
            let x = b.sample(m.bits_per_coordinate, m.x[0], m.x[1]);
            let y = b.sample(m.bits_per_coordinate, m.y[0], m.y[1]);
            *p = ctm.transform_xy(x, y);
        }
        for col in c.iter_mut().skip(startcolor) {
            for (k, x) in col.iter_mut().enumerate().take(ncomp) {
                *x = b.sample(m.bits_per_component, m.c0[k], m.c1[k]);
            }
        }
        match (flag, &prev) {
            (0, _) => {}
            (1, Some((pp, pc))) => {
                v[..4].copy_from_slice(&pp[3..7]);
                c[0] = pc[1];
                c[1] = pc[2];
            }
            (2, Some((pp, pc))) => {
                v[..4].copy_from_slice(&pp[6..10]);
                c[0] = pc[2];
                c[1] = pc[3];
            }
            (3, Some((pp, pc))) => {
                v[0] = pp[9];
                v[1] = pp[10];
                v[2] = pp[11];
                v[3] = pp[0];
                c[0] = pc[3];
                c[1] = pc[0];
            }
            _ => continue,
        }
        let patch = Patch {
            pole: make_patch(kind, &v),
            color: c,
        };
        draw_patch(pt, &patch, SUBDIV, SUBDIV);
        prev = Some((v, c));
    }
}

/// `fz_paint_shade`: the shading through `ctm` into `dest` within
/// `bbox`, which has `dest_colors` color components.
pub fn paint_shade(
    doc: Option<&Document>,
    cc: &Converter,
    ri: u8,
    shade: &Shade,
    ctm: Matrix,
    dest: &mut Pix,
    bbox: IRect,
) {
    let local = shade.matrix.concat(ctm);
    let stride = shade.function_stride;
    let dest_colors = dest.n - usize::from(dest.alpha);
    let scissor = Rect::new(
        bbox.x0 as f32,
        bbox.y0 as f32,
        bbox.x1 as f32,
        bbox.y1 as f32,
    );
    let mut temp = if stride > 0 {
        match Pix::new(bbox, 2, true) {
            Ok(t) => Some(t),
            Err(_) => return,
        }
    } else {
        None
    };
    {
        let target: &mut Pix = match temp.as_mut() {
            Some(t) => t,
            None => &mut *dest,
        };
        let colors = if stride > 0 {
            None
        } else {
            Some(dest_colors.min(3))
        };
        let ncomp = if stride > 0 { 1 } else { shade.colorspace.n() };
        let mut pt = Painter {
            doc,
            cc,
            ri,
            shade,
            dest: target,
            bbox,
            colors,
            ncomp,
            budget: 1 << 22,
        };
        match shade.kind {
            1 => type1(&mut pt, local),
            2 => type2(&mut pt, local, scissor),
            3 => type3(&mut pt, local),
            4 => type4(&mut pt, local),
            5 => type5(&mut pt, local),
            6 | 7 => type67(&mut pt, local, shade.kind),
            _ => {}
        }
    }
    let Some(temp) = temp else {
        return;
    };
    // The sampled function as a lookup table.
    let cn = shade.colorspace.n();
    let mut clut = vec![[0u8; MAX_COLORS + 1]; 256];
    for (i, row) in clut.iter_mut().enumerate() {
        let f = shade
            .function
            .get(i * stride..(i + 1) * stride)
            .unwrap_or(&[]);
        let col = convert(doc, cc, ri, &shade.colorspace, f, dest_colors.min(3));
        for k in 0..dest_colors.min(3) {
            row[k] = (col[k] * 255.0) as u8;
        }
        row[dest_colors] = (f.get(cn).copied().unwrap_or(1.0) * 255.0) as u8;
    }
    let Ok(mut conv) = Pix::new(bbox, dest_colors + 1, true) else {
        return;
    };
    for (d, s) in conv
        .samples
        .chunks_mut(dest_colors + 1)
        .zip(temp.samples.chunks(2))
    {
        let v = usize::from(s[0]);
        let a = mul255(i32::from(s[1]), i32::from(clut[v][dest_colors]));
        for k in 0..dest_colors {
            d[k] = mul255(i32::from(clut[v][k]), a) as u8;
        }
        d[dest_colors] = a as u8;
    }
    dest.paint_pixmap(&conv, 255);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_triangle_interpolates_its_corners() {
        let mut pix =
            Pix::new(IRect::new(0, 0, 10, 10), 1, false).unwrap_or_else(|e| panic!("{e}"));
        let mut a = [0f32; MAXN];
        let mut b = [0f32; MAXN];
        let mut c = [0f32; MAXN];
        a[..3].copy_from_slice(&[0.0, 0.0, 0.0]);
        b[..3].copy_from_slice(&[10.0, 0.0, 255.0]);
        c[..3].copy_from_slice(&[0.0, 10.0, 0.0]);
        paint_triangle(&mut pix, [&a, &b, &c], 3, IRect::new(0, 0, 10, 10));
        // Row 0 runs from 0 to nearly 255 across.
        assert_eq!(pix.samples[0], 0);
        assert!(pix.samples[8] > 180, "{:?}", &pix.samples[..10]);
    }

    use crate::draw::testkit::{bits, check, hex_stream, page, px, stream};

    /// A 64-point page painting shading 5 with `sh`.
    fn sh_page(shading: &str, extra: &[&str]) -> Vec<u8> {
        let mut objs = vec![shading];
        objs.extend_from_slice(extra);
        page(64, 64, "/Shading<</S 5 0 R>>", "/S sh", &objs)
    }

    const RED_TO_BLUE: &str = "<</FunctionType 2/Domain[0 1]/C0[1 0 0]/C1[0 0 1]/N 1>>";

    #[test]
    fn function_shadings_sample_a_grid() {
        // x y -> x, y, (x + y) / 2, over a domain scaled to the page.
        let f = stream(
            "/FunctionType 4/Domain[0 1 0 1]/Range[0 1 0 1 0 1]",
            "{2 copy add 2 div}",
        );
        let pdf = sh_page(
            "<</ShadingType 1/ColorSpace/DeviceRGB/Domain[0 1 0 1]/Matrix[64 0 0 64 0 0]/Function 6 0 R>>",
            &[&f],
        );
        let pix = check(
            "shade-type1",
            &pdf,
            true,
            "afe82cbdb13d29006ff1dfe72379473b",
        );
        // Bottom left of the page is the domain's origin: black.
        assert!(
            px(&pix, 1, 62).iter().all(|&v| v < 16),
            "{:?}",
            px(&pix, 1, 62)
        );
    }

    #[test]
    fn axial_shadings_extend_past_their_ends() {
        for (name, extend, want) in [
            (
                "shade-axial",
                "[false false]",
                "e1ed185de29095ba26f4bd2252a271a2",
            ),
            (
                "shade-axial-extend",
                "[true true]",
                "bd37f597aa9eac5f2aaac6fd701bf10a",
            ),
        ] {
            let pdf = sh_page(
                &format!(
                    "<</ShadingType 2/ColorSpace/DeviceRGB/Coords[16 0 48 0]/Extend{extend}/Function {RED_TO_BLUE}>>"
                ),
                &[],
            );
            let pix = check(name, &pdf, true, want);
            let left = px(&pix, 4, 32);
            if extend.starts_with("[true") {
                assert_eq!(left, [255, 0, 0], "{name}");
            } else {
                assert_eq!(left, [255, 255, 255], "{name}");
            }
        }
    }

    #[test]
    fn a_shading_pattern_fills_within_its_path_with_its_background() {
        // A pattern fill passes the path's box as the scissor, which bounds
        // the axial quad; the background paints where the shading does not.
        // Without ICC: lcms turns MuPDF's gray ramp into RGB a level off
        // neutral here and there, moxcms does not.
        let pdf = page(
            64,
            64,
            "/Pattern<</P 5 0 R>>",
            "/Pattern cs /P scn 8 8 48 48 re f",
            &[
                "<</PatternType 2/Shading<</ShadingType 2/ColorSpace/DeviceGray/Background[0.5]/Coords[24 0 40 0]/Function<</FunctionType 2/Domain[0 1]/C0[0]/C1[1]/N 1>>>>>>",
            ],
        );
        check(
            "shade-pattern-background",
            &pdf,
            false,
            "cfbee938e5f09e1012096ce83c8491fc",
        );
    }

    #[test]
    fn radial_shadings_extend_both_ways() {
        for (name, coords, want) in [
            (
                "shade-radial-grow",
                "[32 32 4 32 32 28]",
                "7b09d1bfd698308be7416c8f491cc0c7",
            ),
            (
                "shade-radial-shrink",
                "[20 20 28 40 40 6]",
                "36fd6139d8cbca113463b42ffbd19460",
            ),
        ] {
            let pdf = sh_page(
                &format!(
                    "<</ShadingType 3/ColorSpace/DeviceRGB/Coords{coords}/Extend[true true]/Function {RED_TO_BLUE}>>"
                ),
                &[],
            );
            check(name, &pdf, true, want);
        }
    }

    #[test]
    fn free_form_meshes_share_edges_by_flag() {
        // Flag, x, y (16 bits), then RGB; flags of 2 bits leave the
        // vertices off byte boundaries.
        let mut f = Vec::new();
        let verts: [(u32, u32, u32, [u32; 3]); 5] = [
            (0, 4, 4, [255, 0, 0]),
            (0, 60, 4, [0, 255, 0]),
            (0, 4, 60, [0, 0, 255]),
            (1, 60, 60, [255, 255, 0]),
            (2, 32, 30, [0, 255, 255]),
        ];
        for (flag, x, y, c) in verts {
            f.extend([(flag, 2), (x * 1000, 16), (y * 1000, 16)]);
            f.extend(c.map(|v| (v, 8)));
        }
        let s = hex_stream(
            "/ShadingType 4/ColorSpace/DeviceRGB/BitsPerCoordinate 16/BitsPerComponent 8/BitsPerFlag 2/Decode[0 65.535 0 65.535 0 1 0 1 0 1]",
            &bits(&f),
        );
        check(
            "shade-type4",
            &sh_page(&s, &[]),
            true,
            "b3da78e81808d6d0d15d74e373ec2ca8",
        );
    }

    #[test]
    fn meshes_with_a_function_look_their_colors_up() {
        // One parameter a vertex, 4 bits, through a red-to-blue function.
        let mut f = Vec::new();
        for (flag, x, y, t) in [(0, 2, 2, 0), (0, 62, 2, 15), (0, 2, 62, 8), (1, 62, 62, 4)] {
            f.extend([(flag, 8), (x, 8), (y, 8), (t, 4)]);
        }
        let s = hex_stream(
            &format!(
                "/ShadingType 4/ColorSpace/DeviceRGB/BitsPerCoordinate 8/BitsPerComponent 4/BitsPerFlag 8/Decode[0 255 0 255 0 1]/Function {RED_TO_BLUE}"
            ),
            &bits(&f),
        );
        check(
            "shade-type4-function",
            &sh_page(&s, &[]),
            true,
            "b6ac80793d7971c4cf928de926605d71",
        );
    }

    #[test]
    fn lattice_meshes_join_rows() {
        let mut f = Vec::new();
        for row in 0..3u32 {
            for col in 0..3u32 {
                f.extend([(col * 30 + 2, 8), (row * 30 + 2, 8)]);
                f.extend([(row * 120, 8), (col * 120, 8), (255 - row * 60, 8)]);
            }
        }
        let s = hex_stream(
            "/ShadingType 5/ColorSpace/DeviceRGB/BitsPerCoordinate 8/BitsPerComponent 8/VerticesPerRow 3/Decode[0 255 0 255 0 1 0 1 0 1]",
            &bits(&f),
        );
        check(
            "shade-type5",
            &sh_page(&s, &[]),
            true,
            "39a9b8ae1bc7a3eb3da7985c8094013f",
        );
    }

    /// A patch mesh: a first patch, then one sharing each of its edges
    /// in turn (flags 1, 2 and 3), coordinates and colors 8 bits each.
    fn patches(tensor: bool) -> Vec<u8> {
        let mut f = vec![(0, 8)];
        let first = [
            (16, 16),
            (16, 24),
            (16, 40),
            (16, 48),
            (24, 50),
            (40, 46),
            (48, 48),
            (50, 40),
            (46, 24),
            (48, 16),
            (40, 14),
            (24, 18),
        ];
        for (x, y) in first {
            f.extend([(x, 8), (y, 8)]);
        }
        if tensor {
            for (x, y) in [(24, 24), (24, 40), (40, 40), (40, 24)] {
                f.extend([(x, 8), (y, 8)]);
            }
        }
        for c in [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]] {
            f.extend(c.map(|v| (v, 8)));
        }
        let extra = if tensor { 12 } else { 8 };
        for (flag, dx, dy) in [(1u32, 0i32, 12i32), (2, 12, 0), (3, 0, -12)] {
            f.push((flag, 8));
            for k in 0..extra {
                let x = (32 + dx + (k % 4) * 3 - 6).clamp(0, 63) as u32;
                let y = (32 + dy + (k / 4) * 3 - 6).clamp(0, 63) as u32;
                f.extend([(x, 8), (y, 8)]);
            }
            for c in [[0, 255, 255], [255, 0, 255]] {
                f.extend(c.map(|v| (v, 8)));
            }
        }
        bits(&f)
    }

    #[test]
    fn coons_and_tensor_patches_continue_from_their_neighbours() {
        for (name, kind, tensor, want) in [
            ("shade-type6", 6, false, "b3bb355bd4913d271a8c47efc82efa38"),
            ("shade-type7", 7, true, "2616e82bea9045baf2856eaf2b41d94a"),
        ] {
            let s = hex_stream(
                &format!(
                    "/ShadingType {kind}/ColorSpace/DeviceRGB/BitsPerCoordinate 8/BitsPerComponent 8/BitsPerFlag 8/Decode[0 64 0 64 0 1 0 1 0 1]"
                ),
                &patches(tensor),
            );
            check(name, &sh_page(&s, &[]), true, want);
        }
    }

    #[test]
    fn a_luminosity_mask_shades_in_gray() {
        // The soft mask's group draws a shading into a gray pixmap.
        let group = stream(
            "/Type/XObject/Subtype/Form/BBox[0 0 64 64]/Group<</S/Transparency/CS/DeviceGray>>/Resources<</Shading<</S 7 0 R>>>>",
            "/S sh",
        );
        let shading = format!(
            "<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 64 0]/Function {RED_TO_BLUE}>>"
        );
        let pdf = page(
            64,
            64,
            "/ExtGState<</G 5 0 R>>",
            "/G gs 1 0 0 rg 0 0 64 64 re f",
            &["<</SMask<</S/Luminosity/G 6 0 R>>>>", &group, &shading],
        );
        check(
            "shade-luminosity-mask",
            &pdf,
            false,
            "27011420bf53041160d43b4ecc0320fe",
        );
    }
}
