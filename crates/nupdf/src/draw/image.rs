// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/draw-affine.c (fz_paint_image,
// fz_gridfit_matrix), source/fitz/draw-scale-simple.c (fz_scale_pixmap),
// source/fitz/pixmap.c (fz_subsample_pixmap) and source/fitz/image.c (the
// subsampling factor of fz_get_pixmap_from_image),
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! Images onto pixmaps as MuPDF paints them: decoded at a power of two
//! below full size, scaled down with its filter when the page shows them
//! smaller still, then mapped pixel by pixel in 14-bit fixed point,
//! nearest or bilinear.

use crate::fma::Madd;
use crate::geometry::{IRect, Matrix, Rect};

use super::paint::{Pix, blend, combine, expand, mul255};

const MY_EPSILON: f32 = 0.001;

/// `fz_gridfit_matrix`, not as tiled: an axis-aligned image's edges moved
/// out onto pixel boundaries, upright or turned a quarter.
pub fn gridfit(mut m: Matrix) -> Matrix {
    if m.b.abs() < f32::EPSILON && m.c.abs() < f32::EPSILON {
        fit_edge(&mut m.a, &mut m.e);
        fit_edge(&mut m.d, &mut m.f);
    } else if m.a.abs() < f32::EPSILON && m.d.abs() < f32::EPSILON {
        // As MuPDF pairs them: b's edge is f, and c's is e.
        fit_edge(&mut m.b, &mut m.f);
        fit_edge(&mut m.c, &mut m.e);
    }
    m
}

/// One axis of `fz_gridfit_matrix`: the edge at `origin` and the one at
/// `origin + size` moved out to whole pixels.
fn fit_edge(size: &mut f32, origin: &mut f32) {
    let trunc = |v: f32| v as i32 as f32;
    if *size > 0.0 {
        let mut f = trunc(*origin);
        if f - *origin > MY_EPSILON {
            f -= 1.0;
        }
        *size += *origin - f;
        *origin = f;
        let mut f = trunc(*size);
        if *size - f > MY_EPSILON {
            f += 1.0;
        }
        *size = f;
    } else if *size < 0.0 {
        let mut f = trunc(*origin);
        if *origin - f > MY_EPSILON {
            f += 1.0;
        }
        *size += *origin - f;
        *origin = f;
        let mut f = trunc(*size);
        if f - *size > MY_EPSILON {
            f -= 1.0;
        }
        *size = f;
    }
}

/// The size an image shows at under `ctm`, as `fz_get_pixmap_from_image`
/// works it out (truncated), and the power of two to decode it below
/// full size by.
pub fn extent_and_factor(img_w: i32, img_h: i32, ctm: Matrix) -> (i32, i32, u32) {
    let dw = (ctm.a * ctm.a + ctm.b * ctm.b).sqrt() as i32;
    let dh = (ctm.c * ctm.c + ctm.d * ctm.d).sqrt() as i32;
    let (w, h) = (dw.min(img_w), dh.min(img_h));
    let mut l2 = 0;
    if w > 0 && h > 0 {
        while (img_w >> (l2 + 1)) >= w + 2 && (img_h >> (l2 + 1)) >= h + 2 && l2 < 6 {
            l2 += 1;
        }
    }
    (dw, dh, l2)
}

/// `fz_subsample_pixmap`: each `2^factor` square averaged into a pixel.
pub fn subsample(p: &mut Pix, factor: u32) {
    if factor == 0 {
        return;
    }
    let f = 1i32 << factor;
    let (w, h, n) = (p.w, p.h, p.n as i32);
    let subx = w % f;
    let suby = h % f;
    if (subx != 0 || suby != 0) && w >= f && h >= f {
        let subx = if subx != 0 { f - subx } else { 0 };
        let suby = if suby != 0 { f - suby } else { 0 };
        pixblock_bresenham(&mut p.samples, w, h, n, factor, subx, suby);
    } else {
        pixblock(&mut p.samples, w, h, n, factor);
    }
    p.w = (w + f - 1) >> factor;
    p.h = (h + f - 1) >> factor;
    p.samples.truncate((p.w * p.h * n) as usize);
}

fn at(s: &[u8], i: i64) -> i32 {
    usize::try_from(i)
        .ok()
        .and_then(|i| s.get(i))
        .map_or(0, |&v| i32::from(v))
}

fn put(s: &mut [u8], i: i64, v: i32) {
    if let Some(d) = usize::try_from(i).ok().and_then(|i| s.get_mut(i)) {
        *d = v as u8;
    }
}

/// `fz_subsample_pixblock_bresenham`, its pointer walk in offsets.
fn pixblock_bresenham(s2buf: &mut [u8], w: i32, h: i32, n: i32, factor: u32, subx: i32, suby: i32) {
    let f = 1i32 << factor;
    let stride = i64::from(w * n);
    let bxd = ((w + subx) / f) - 1;
    let bxf0 = bxd - ((bxd + 1) >> 1);
    let byd = ((h + suby) / f) - 1;
    let mut byf = byd - ((byd + 1) >> 1);
    let fwd = stride;
    let back = i64::from(f) * fwd - i64::from(n);
    let back2 = i64::from(f * n - 1);
    let fwd2 = i64::from((f - 1) * n);
    let shift = factor * 2;
    let mut d: i64 = 0;
    let mut s2: i64 = 0;
    let mut y = h;
    while y > 0 {
        let mut bxf2 = bxf0;
        let mut s = s2;
        let mut x = w;
        while x > 0 {
            for _ in 0..n {
                let mut v = 0i32;
                for _ in 0..f {
                    for _ in 0..f {
                        v += at(s2buf, s);
                        s += fwd;
                    }
                    s -= back;
                }
                put(s2buf, d, v >> shift);
                d += 1;
                s -= back2;
            }
            s += fwd2;
            bxf2 -= subx;
            while bxf2 < 0 && bxd > 0 {
                s -= i64::from(n);
                bxf2 += bxd;
            }
            x -= f;
        }
        s2 += stride * i64::from(f);
        byf -= suby;
        while byf < 0 && byd > 0 {
            s2 -= stride;
            byf += byd;
        }
        y -= f;
    }
}

/// `fz_subsample_pixblock`: whole squares, then the partial ones at the
/// right and bottom edges averaged by their size.
pub(crate) fn pixblock(buf: &mut [u8], w: i32, h: i32, n: i32, factor: u32) {
    let f = 1i32 << factor;
    let stride = i64::from(w * n);
    let fwd = stride;
    let back = i64::from(f) * fwd - i64::from(n);
    let back2 = i64::from(f * n - 1);
    let fwd2 = i64::from((f - 1) * n);
    let fwd3 = i64::from(f - 1) * fwd + stride - i64::from(w * n);
    let shift = factor * 2;
    let mut s: i64 = 0;
    let mut d: i64 = 0;
    let mut y = h - f;
    while y >= 0 {
        let mut x = w - f;
        while x >= 0 {
            for _ in 0..n {
                let mut v = 0;
                for _ in 0..f {
                    for _ in 0..f {
                        v += at(buf, s);
                        s += fwd;
                    }
                    s -= back;
                }
                put(buf, d, v >> shift);
                d += 1;
                s -= back2;
            }
            s += fwd2;
            x -= f;
        }
        x += f;
        if x > 0 {
            let div = x * f;
            let fwd4 = i64::from((x - 1) * n);
            let back4 = i64::from(x * n - 1);
            for _ in 0..n {
                let mut v = 0;
                for _ in 0..x {
                    for _ in 0..f {
                        v += at(buf, s);
                        s += fwd;
                    }
                    s -= back;
                }
                put(buf, d, v / div);
                d += 1;
                s -= back4;
            }
            s += fwd4;
        }
        s += fwd3;
        y -= f;
    }
    y += f;
    if y > 0 {
        let div = y * f;
        let back5 = fwd * i64::from(y) - i64::from(n);
        let mut x = w - f;
        while x >= 0 {
            for _ in 0..n {
                let mut v = 0;
                for _ in 0..f {
                    for _ in 0..y {
                        v += at(buf, s);
                        s += fwd;
                    }
                    s -= back5;
                }
                put(buf, d, v / div);
                d += 1;
                s -= back2;
            }
            s += fwd2;
            x -= f;
        }
        x += f;
        if x > 0 {
            let back4 = i64::from(x * n - 1);
            let div = x * y;
            for _ in 0..n {
                let mut v = 0;
                for _ in 0..x {
                    for _ in 0..y {
                        v += at(buf, s);
                        s += fwd;
                    }
                    s -= back5;
                }
                put(buf, d, v / div);
                d += 1;
                s -= back4;
            }
        }
    }
}

/// `fz_scale_filter_simple`.
fn simple(x: f32) -> f32 {
    if x >= 1.0 {
        return 0.0;
    }
    1.0 + (2.0 * x - 3.0) * x * x
}

/// `fz_weights` for one direction: per output pixel, the first source
/// pixel and the weights from it on (for rows, in the order of a ring of
/// `max_len` rows).
struct Weights {
    flip: bool,
    max_len: usize,
    entries: Vec<(i32, Vec<i32>)>,
}

impl Weights {
    fn insert(&mut self, j: usize, i: i32, weight: i32, new_line: &mut bool) {
        let e = &mut self.entries[j];
        if *new_line {
            *new_line = false;
            e.0 = i;
            e.1.clear();
        }
        while i < e.0 {
            e.1.insert(0, 0);
            e.0 -= 1;
        }
        let k = (i - e.0) as usize;
        if k >= e.1.len() {
            e.1.resize(k + 1, 0);
            e.1[k] = weight;
        } else {
            e.1[k] += weight;
        }
    }
}

/// `make_weights` with the simple filter.
#[allow(clippy::too_many_arguments)]
fn make_weights(
    src_w: i32,
    x: f32,
    dst_w: f32,
    vertical: bool,
    dst_w_int: i32,
    patch_l: i32,
    patch_r: i32,
    flip: bool,
) -> Weights {
    let srcf = src_w as f32;
    let max_len = if srcf > dst_w {
        ((2.0 * srcf) / dst_w).ceil().min(srcf) as usize
    } else {
        2
    };
    let (ff, g) = if dst_w < srcf {
        (dst_w / srcf, 1.0)
    } else {
        (1.0, srcf / dst_w)
    };
    let window = 1.0 / ff;
    let mut wts = Weights {
        flip,
        max_len: max_len.max(1),
        entries: vec![(0, Vec::new()); (patch_r - patch_l).max(0) as usize],
    };
    for j in patch_l..patch_r {
        let jj = (j - patch_l) as usize;
        let centre = (j as f32 - x + 0.5) * srcf / dst_w - 0.5;
        let mut l = (centre - window).ceil() as i32;
        let mut r = (centre + window).floor() as i32;
        if (r - l) as f32 > 2.0 * window {
            while centre - l as f32 > window {
                l += 1;
            }
            while r as f32 - centre > window {
                r -= 1;
            }
        }
        let mut new_line = true;
        wts.entries[jj] = (0, Vec::new());
        while l <= r {
            let mut dist = j as f32 - x + 0.5 - ((l as f32 + 0.5) * dst_w / srcf);
            dist *= g;
            if dist < 0.0 {
                dist = -dist;
            }
            let f = simple(dist) * ff;
            let weight = (256.0 * f + 0.5) as i32;
            if l >= 0 && l < src_w && weight != 0 {
                wts.insert(jj, l, weight, &mut new_line);
            }
            l += 1;
        }
        if new_line {
            let src_x = (centre.floor() as i32).clamp(0, src_w - 1);
            wts.insert(jj, src_x, 1, &mut new_line);
        }
        // check_weights.
        {
            let e = &mut wts.entries[jj];
            let sum: i32 = e.1.iter().sum();
            let mut max = -256;
            let mut maxidx = 0;
            for (k, &v) in e.1.iter().enumerate() {
                if v > max {
                    max = v;
                    maxidx = k;
                }
            }
            let fix = ((j != 0) && (j != dst_w_int - 1)) || sum > 256;
            let first = j == 0 && x < 0.0001 && sum != 256;
            let last = j == dst_w_int - 1 && (dst_w_int as f32 - dst_w) < 0.0001 && sum != 256;
            if (fix || first || last) && !e.1.is_empty() {
                e.1[maxidx] += 256 - sum;
            }
        }
        if vertical {
            // reorder_weights: padded to the ring's size, each weight at
            // its source row's place in the ring.
            let max = wts.max_len;
            let e = &mut wts.entries[jj];
            let mut min = e.0;
            let len = e.1.len();
            let tmp = e.1.clone();
            let mut off = 0;
            let mut len2 = len;
            if len < max {
                len2 = max;
                if min + max as i32 > src_w {
                    off = min + max as i32 - src_w;
                    min = src_w - max as i32;
                    e.0 = min;
                }
            }
            let mut out = vec![0; len2.max(max)];
            for i in 0..len2 {
                let v = tmp.get(i).copied().unwrap_or(0);
                let slot = ((min + i as i32 + off).rem_euclid(max as i32)) as usize;
                if let Some(o) = out.get_mut(slot) {
                    *o = v;
                }
            }
            out.truncate(len2);
            e.1 = out;
        }
    }
    wts
}

/// One row scaled across by `w`, into `dst` (`count * n` bytes).
fn scale_row(dst: &mut [u8], src: &[u8], w: &Weights, n: usize) {
    let count = w.entries.len();
    for (i, (min, ws)) in w.entries.iter().enumerate() {
        let o = if w.flip { count - 1 - i } else { i };
        for k in 0..n {
            let mut v = 128i32;
            for (t, &c) in ws.iter().enumerate() {
                let at = (*min as usize + t) * n + k;
                v += i32::from(src.get(at).copied().unwrap_or(0)) * c;
            }
            dst[o * n + k] = (v >> 8) as u8;
        }
    }
}

/// `fz_scale_pixmap`: `src` filtered into a pixmap of `w` by `h` at
/// (`x`, `y`) (negative sizes flip), cut to `clip`. None when nothing is
/// left.
pub fn scale(src: &Pix, x: f32, y: f32, w: f32, h: f32, clip: Option<IRect>) -> Option<Pix> {
    let lim = (1 << 24) as f32;
    if w > lim || h > lim || w < -lim || h < -lim || x > lim || y > lim || x < -lim || y < -lim {
        return None;
    }
    let fix = |v: f32| {
        if v <= -1.0 {
            v
        } else if v < 0.0 {
            -1.0
        } else if v < 1.0 {
            1.0
        } else {
            v
        }
    };
    let (mut x, mut y, mut w, mut h) = (x, y, fix(w), fix(h));
    let forcealpha = !src.alpha
        && (x != x as i32 as f32
            || y != y as i32 as f32
            || w != w as i32 as f32
            || h != h as i32 as f32);
    let flip_x = w < 0.0;
    let (mut dst_x, dst_w_int);
    if flip_x {
        w = -w;
        dst_x = (x - w).floor() as i32;
        let tmp = x.ceil();
        dst_w_int = tmp as i32 - dst_x;
        x = tmp - x;
    } else {
        dst_x = x.floor() as i32;
        x -= dst_x as f32;
        dst_w_int = (x + w).ceil() as i32;
    }
    let flip_y = h < 0.0;
    let (mut dst_y, dst_h_int);
    if flip_y {
        h = -h;
        dst_y = (y - h).floor() as i32;
        let tmp = y.ceil();
        dst_h_int = tmp as i32 - dst_y;
        y = tmp - y;
    } else {
        dst_y = y.floor() as i32;
        y -= dst_y as f32;
        dst_h_int = (y + h).ceil() as i32;
    }
    let (mut px0, mut py0, mut px1, mut py1) = (0, 0, dst_w_int, dst_h_int);
    if let Some(c) = clip {
        if flip_x {
            if dst_x + dst_w_int > c.x1 {
                px0 = dst_x + dst_w_int - c.x1;
            }
            if c.x0 > dst_x {
                px1 = dst_w_int - (c.x0 - dst_x);
                dst_x = c.x0;
            }
        } else {
            if dst_x + dst_w_int > c.x1 {
                px1 = c.x1 - dst_x;
            }
            if c.x0 > dst_x {
                px0 = c.x0 - dst_x;
                dst_x += px0;
            }
        }
        if flip_y {
            if dst_y + dst_h_int > c.y1 {
                py1 = c.y1 - dst_y;
            }
            if c.y0 > dst_y {
                py0 = c.y0 - dst_y;
                dst_y = c.y0;
            }
        } else {
            if dst_y + dst_h_int > c.y1 {
                py1 = c.y1 - dst_y;
            }
            if c.y0 > dst_y {
                py0 = c.y0 - dst_y;
                dst_y += py0;
            }
        }
    }
    if px0 >= px1 || py0 >= py1 {
        return None;
    }
    let n = src.n;
    let on = n + usize::from(forcealpha);
    let ow = (px1 - px0) as usize;
    let oh = (py1 - py0) as usize;
    if ow
        .checked_mul(oh)
        .and_then(|v| v.checked_mul(on))
        .is_none_or(|v| v > 1 << 31)
    {
        return None;
    }
    let mut out = Pix {
        x: dst_x,
        y: dst_y,
        w: ow as i32,
        h: oh as i32,
        n: on,
        alpha: src.alpha || forcealpha,
        samples: vec![0; ow * oh * on],
    };
    let cols = (src.w != 1).then(|| make_weights(src.w, x, w, false, dst_w_int, px0, px1, flip_x));
    let rows = (src.h != 1).then(|| make_weights(src.h, y, h, true, dst_h_int, py0, py1, flip_y));
    let sstride = src.w as usize * n;
    let ostride = ow * on;
    match (&rows, &cols) {
        (None, None) => {
            // duplicate_single_pixel.
            for px in out.samples.chunks_mut(on) {
                px[..n].copy_from_slice(&src.samples[..n]);
                if forcealpha {
                    px[n] = 255;
                }
            }
        }
        (None, Some(cols)) => {
            // scale_single_row.
            let mut row = vec![0u8; ow * on];
            let count = cols.entries.len();
            for (i, (min, ws)) in cols.entries.iter().enumerate() {
                let o = if cols.flip { count - 1 - i } else { i };
                for k in 0..on {
                    let mut v = 128i32;
                    for (t, &c) in ws.iter().enumerate() {
                        let s = if k < n {
                            i32::from(
                                src.samples
                                    .get((*min as usize + t) * n + k)
                                    .copied()
                                    .unwrap_or(0),
                            )
                        } else {
                            255
                        };
                        v += s * c;
                    }
                    row[o * on + k] = (v >> 8) as u8;
                }
            }
            for r in out.samples.chunks_mut(ostride) {
                r.copy_from_slice(&row);
            }
        }
        (Some(rows), None) => {
            // scale_single_col: the ring-ordered weights read in order.
            let count = rows.entries.len();
            for (i, (min, ws)) in rows.entries.iter().enumerate() {
                let mut px = vec![0u8; on];
                for (k, p) in px.iter_mut().enumerate() {
                    let mut v = 128i32;
                    for (t, &c) in ws.iter().enumerate() {
                        let sr = *min as i64 + t as i64;
                        let sr = if rows.flip {
                            i64::from(src.h) - 1 - sr
                        } else {
                            sr
                        };
                        let s = if k < n {
                            usize::try_from(sr * sstride as i64 + k as i64)
                                .ok()
                                .and_then(|a| src.samples.get(a))
                                .map_or(0, |&b| i32::from(b))
                        } else {
                            255
                        };
                        v += s * c;
                    }
                    *p = (v >> 8) as u8;
                }
                let _ = count;
                let r = &mut out.samples[i * ostride..(i + 1) * ostride];
                for d in r.chunks_mut(on) {
                    d.copy_from_slice(&px);
                }
            }
        }
        (Some(rows), Some(cols)) => {
            let count = cols.entries.len();
            let temp_span = count * n;
            let temp_rows = rows.max_len;
            let mut temp = vec![0u8; temp_span * temp_rows];
            let mut max_row = rows.entries.first().map_or(0, |e| e.0);
            for (row, (row_min, ws)) in rows.entries.iter().enumerate() {
                while max_row < row_min + ws.len() as i32 {
                    let sr = if flip_y { src.h - 1 - max_row } else { max_row };
                    let slot = (max_row as usize) % temp_rows;
                    let start = sr.max(0) as usize * sstride;
                    let end = (start + sstride).min(src.samples.len());
                    let srow = src.samples.get(start..end).unwrap_or(&[]);
                    scale_row(
                        &mut temp[slot * temp_span..(slot + 1) * temp_span],
                        srow,
                        cols,
                        n,
                    );
                    max_row += 1;
                }
                let o = &mut out.samples[row * ostride..(row + 1) * ostride];
                for x in 0..count {
                    for k in 0..n {
                        let mut v = 128i32;
                        for (t, &c) in ws.iter().enumerate() {
                            v += i32::from(temp[t * temp_span + x * n + k]) * c;
                        }
                        o[x * on + k] = (v >> 8) as u8;
                    }
                    if forcealpha {
                        o[x * on + n] = 255;
                    }
                }
            }
            if forcealpha {
                adjust_alpha_edges(&mut out, rows, cols);
            }
        }
    }
    Some(out)
}

/// `get_alpha_edge_values`: the weight sums of the first and last pixel.
fn edge_values(w: &Weights) -> (i32, i32) {
    let t: i32 = w.entries.first().map_or(0, |e| e.1.iter().sum());
    let b: i32 = if w.entries.len() >= 2 {
        w.entries.last().map_or(0, |e| e.1.iter().sum())
    } else {
        0
    };
    if w.flip && w.entries.len() >= 2 {
        (b, t)
    } else {
        (t, b)
    }
}

/// `adjust_alpha_edges`.
fn adjust_alpha_edges(pix: &mut Pix, rows: &Weights, cols: &Weights) {
    let (t, b) = edge_values(rows);
    let (l, r) = edge_values(cols);
    let l = (255 * l + 128) >> 8;
    let r = (255 * r + 128) >> 8;
    let tl = (l * t + 128) >> 8;
    let tr = (r * t + 128) >> 8;
    let bl = (l * b + 128) >> 8;
    let br = (r * b + 128) >> 8;
    let t = (255 * t + 128) >> 8;
    let b = (255 * b + 128) >> 8;
    let (w, h, n) = (pix.w as usize, pix.h as usize, pix.n);
    let a = |x: usize, y: usize| (y * w + x) * n + n - 1;
    let s = &mut pix.samples;
    if w == 0 || h == 0 {
        return;
    }
    for x in 0..w {
        s[a(x, 0)] = if x == 0 {
            tl
        } else if x == w - 1 {
            tr
        } else {
            t
        } as u8;
    }
    for y in 1..h.saturating_sub(1) {
        s[a(0, y)] = l as u8;
        if w >= 2 {
            s[a(w - 1, y)] = r as u8;
        }
    }
    if h >= 2 {
        for x in 0..w {
            s[a(x, h - 1)] = if x == 0 {
                bl
            } else if x == w - 1 {
                br
            } else {
                b
            } as u8;
        }
    }
}

/// `fz_transform_pixmap`: for an axis-aligned image shown smaller than
/// it is, the image scaled to its device size; `ctm` becomes the scaled
/// pixmap's placement. None when it does not apply.
pub fn transform_pixmap(
    img: &Pix,
    ctm: &mut Matrix,
    dx: i32,
    dy: i32,
    fit: bool,
    clip: IRect,
) -> Option<Pix> {
    if clip.is_empty() {
        return None;
    }
    if ctm.a != 0.0 && ctm.b == 0.0 && ctm.c == 0.0 && ctm.d != 0.0 {
        let m = if fit { gridfit(*ctm) } else { *ctm };
        let s = scale(img, m.e, m.f, m.a, m.d, Some(clip))?;
        ctm.a = s.w as f32;
        ctm.d = s.h as f32;
        ctm.e = s.x as f32;
        ctm.f = s.y as f32;
        return Some(s);
    }
    if ctm.a == 0.0 && ctm.b != 0.0 && ctm.c != 0.0 && ctm.d == 0.0 {
        let m = if fit { gridfit(*ctm) } else { *ctm };
        let rclip = IRect::new(clip.y0, clip.x0, clip.y1, clip.x1);
        let s = scale(img, m.f, m.e, m.b, m.c, Some(rclip))?;
        ctm.b = s.w as f32;
        ctm.c = s.h as f32;
        ctm.f = s.x as f32;
        ctm.e = s.y as f32;
        return Some(s);
    }
    if dx > 0 && dy > 0 {
        return scale(img, 0.0, 0.0, dx as f32, dy as f32, None);
    }
    None
}

const PREC: u32 = 14;
const ONE: i64 = 1 << PREC;
const MASK: i64 = ONE - 1;
const HALF: i64 = 1 << (PREC - 1);
const LIMIT: i64 = 1 << (63 - PREC);

fn lerp(a: i32, b: i32, f: i32) -> i32 {
    a + (((b - a) * f) >> PREC)
}

fn bilerp(a: i32, b: i32, c: i32, d: i32, uf: i32, vf: i32) -> i32 {
    lerp(lerp(a, b, uf), lerp(c, d, uf), vf)
}

/// `fz_paint_image_imp`: `img` (premultiplied, its own colors or, with
/// `color`, a coverage mask painted in it) through `ctm` from the unit
/// square, within `scissor`, at `alpha` (0..255). `interpolate` is the
/// image's /Interpolate.
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
pub fn paint_image(
    dst: &mut Pix,
    scissor: IRect,
    img: &Pix,
    ctm: Matrix,
    color: Option<&[u8]>,
    alpha: i32,
    lerp_allowed: bool,
    interpolate: bool,
    mut ga: Option<&mut Pix>,
    mut shape: Option<&mut Pix>,
) {
    if alpha == 0 || img.w <= 0 || img.h <= 0 {
        return;
    }
    let is_rectilinear = (ctm.b.abs() < f32::EPSILON && ctm.c.abs() < f32::EPSILON)
        || (ctm.a.abs() < f32::EPSILON && ctm.d.abs() < f32::EPSILON);
    let mut dolerp = false;
    let sx = (ctm.a * ctm.a + ctm.b * ctm.b).sqrt();
    let sy = (ctm.c * ctm.c + ctm.d * ctm.d).sqrt();
    if !is_rectilinear {
        dolerp = lerp_allowed;
    }
    if sx > img.w as f32 {
        dolerp = lerp_allowed;
    }
    if sy > img.h as f32 {
        dolerp = lerp_allowed;
    }
    if !interpolate {
        if sx > (img.w * 2) as f32 {
            dolerp = false;
        }
        if sy > (img.h * 2) as f32 {
            dolerp = false;
        }
    }
    let bbox = IRect::from_rect(Rect::new(0.0, 0.0, 1.0, 1.0).transform(ctm))
        .intersect(scissor)
        .intersect(dst.bbox());
    if bbox.is_empty() {
        return;
    }
    let (x, y, w, h) = (bbox.x0, bbox.y0, bbox.width(), bbox.height());
    let inv = ctm
        .pre_scale(1.0 / img.w as f32, 1.0 / img.h as f32)
        .invert();
    let one = ONE as f32;
    let (ia, ib, ic, id) = (inv.a * one, inv.b * one, inv.c * one, inv.d * one);
    let (ie, iff) = (inv.e * one, inv.f * one);
    let (fa, fb, fc, fd) = (ia as i64, ib as i64, ic as i64, id as i64);
    let (xf, yf) = (x as f32, y as f32);
    // Clang fuses these multiply-adds (DECISIONS D39).
    let mut u0 = (ia + ic).madd(0.5, ia.madd(xf, ic * yf) + ie) as i32 as i64;
    let mut v0 = (ib + id).madd(0.5, ib.madd(xf, id * yf) + iff) as i32 as i64;
    let mut sw = i64::from(img.w);
    let mut sh = i64::from(img.h);
    if sw >= LIMIT || sh >= LIMIT {
        return;
    }
    let mut sa = usize::from(img.alpha);
    let mut sn = img.n - sa;
    if img.n == sa && color.is_some() {
        sa = 0;
        sn = 1;
    }
    let da = usize::from(dst.alpha);
    let dn = dst.n - da;
    let g2rgb = dn == 3 && img.n == 1 + sa && color.is_none();
    if dolerp {
        u0 -= HALF;
        v0 -= HALF;
        sw = (sw << PREC) + HALF;
        sh = (sh << PREC) + HALF;
    }
    let iw = i64::from(img.w);
    let ih = i64::from(img.h);
    let sn_px = sn + sa;
    let ss = img.w as usize * img.n;
    let sample = |ui: i64, vi: i64| -> usize {
        let ui = ui.clamp(0, iw - 1);
        let vi = vi.clamp(0, ih - 1);
        vi as usize * ss + ui as usize * sn_px
    };
    let s = &img.samples;
    let get = |i: usize| i32::from(s.get(i).copied().unwrap_or(0));
    let dstride = dst.w as usize * dst.n;
    let dn_px = dst.n;
    for row in 0..h {
        let mut u = u0 + fc * i64::from(row);
        let mut v = v0 + fd * i64::from(row);
        let dy = (y + row - dst.y) as usize;
        let mut dpos = dy * dstride + (x - dst.x) as usize * dn_px;
        for col in 0..w {
            let dp = &mut dst.samples[dpos..dpos + dn_px];
            // The group's alpha at this pixel, when there is one to keep.
            let mut gp = ga.as_deref_mut().and_then(|g| {
                let (gx, gy) = (x + col - g.x, y + row - g.y);
                (gx >= 0 && gy >= 0 && gx < g.w && gy < g.h).then(|| (gy * g.w + gx) as usize)
            });
            let mut put_ga = |f: &dyn Fn(i32) -> i32| {
                if let (Some(i), Some(g)) = (gp.take(), ga.as_deref_mut()) {
                    g.samples[i] = f(i32::from(g.samples[i])) as u8;
                }
            };
            // The shape at this pixel, for knockout groups.
            let mut hpos = shape.as_deref_mut().and_then(|g| {
                let (gx, gy) = (x + col - g.x, y + row - g.y);
                (gx >= 0 && gy >= 0 && gx < g.w && gy < g.h).then(|| (gy * g.w + gx) as usize)
            });
            let mut put_shape = |f: &dyn Fn(i32) -> i32| {
                if let (Some(i), Some(g)) = (hpos.take(), shape.as_deref_mut()) {
                    g.samples[i] = f(i32::from(g.samples[i])) as u8;
                }
            };
            if let Some(color) = color {
                // template_affine_color_N_{lerp,near}.
                let csa = i32::from(color.get(dn).copied().unwrap_or(255));
                let ma = if dolerp {
                    if u + HALF >= 0 && u + ONE < sw && v + HALF >= 0 && v + ONE < sh {
                        let (ui, vi) = (u >> PREC, v >> PREC);
                        let (uf, vf) = ((u & MASK) as i32, (v & MASK) as i32);
                        Some(bilerp(
                            get(sample(ui, vi)),
                            get(sample(ui + 1, vi)),
                            get(sample(ui, vi + 1)),
                            get(sample(ui + 1, vi + 1)),
                            uf,
                            vf,
                        ))
                    } else {
                        None
                    }
                } else {
                    let (ui, vi) = (u >> PREC, v >> PREC);
                    (ui >= 0 && ui < sw && vi >= 0 && vi < sh)
                        .then(|| get(vi as usize * ss + ui as usize))
                };
                if let Some(ma) = ma {
                    let masa = combine(expand(ma), csa);
                    if masa != 0 {
                        for k in 0..dn {
                            dp[k] = blend(i32::from(color[k]), i32::from(dp[k]), masa) as u8;
                        }
                        if da == 1 {
                            dp[dn] = blend(255, i32::from(dp[dn]), masa) as u8;
                        }
                        put_shape(&|h| blend(255, h, ma));
                        put_ga(&|g| blend(255, g, masa));
                    }
                }
            } else {
                // Colors and alpha at this pixel, nearest or bilinear.
                let mut px = [0i32; 33];
                let ok = if dolerp {
                    if u + HALF >= 0 && u + ONE < sw && v + HALF >= 0 && v + ONE < sh {
                        let (ui, vi) = (u >> PREC, v >> PREC);
                        let (uf, vf) = ((u & MASK) as i32, (v & MASK) as i32);
                        let (pa, pb, pc, pd) = (
                            sample(ui, vi),
                            sample(ui + 1, vi),
                            sample(ui, vi + 1),
                            sample(ui + 1, vi + 1),
                        );
                        for k in 0..sn_px.min(33) {
                            px[k] =
                                bilerp(get(pa + k), get(pb + k), get(pc + k), get(pd + k), uf, vf);
                        }
                        true
                    } else {
                        false
                    }
                } else {
                    let (ui, vi) = (u >> PREC, v >> PREC);
                    if ui >= 0 && ui < sw && vi >= 0 && vi < sh {
                        let p = vi as usize * ss + ui as usize * sn_px;
                        for k in 0..sn_px.min(33) {
                            px[k] = get(p + k);
                        }
                        true
                    } else {
                        false
                    }
                };
                if ok {
                    let a = if sa == 1 { px[sn] } else { 255 };
                    let comp = |k: usize| {
                        if g2rgb {
                            px[0]
                        } else if k < sn {
                            px[k]
                        } else {
                            0
                        }
                    };
                    let ncol = if g2rgb { 3 } else { sn.min(dn) };
                    if alpha == 255 {
                        if a != 0 {
                            let t = 255 - a;
                            for k in 0..dn {
                                let c = if k < ncol { comp(k) } else { 0 };
                                dp[k] = if k < ncol {
                                    (c + mul255(i32::from(dp[k]), t)) as u8
                                } else {
                                    0
                                };
                            }
                            if da == 1 {
                                dp[dn] = (a + mul255(i32::from(dp[dn]), t)) as u8;
                            }
                            put_shape(&|h| a + mul255(h, t));
                            put_ga(&|g| a + mul255(g, t));
                        }
                    } else {
                        let aa = if sa == 1 { mul255(a, alpha) } else { alpha };
                        if aa != 0 {
                            let t = 255 - aa;
                            for k in 0..dn {
                                dp[k] = if k < ncol {
                                    (mul255(comp(k), alpha) + mul255(i32::from(dp[k]), t)) as u8
                                } else {
                                    0
                                };
                            }
                            if da == 1 {
                                dp[dn] = (aa + mul255(i32::from(dp[dn]), t)) as u8;
                            }
                            put_shape(&|h| a + mul255(h, 255 - a));
                            put_ga(&|g| aa + mul255(g, t));
                        }
                    }
                }
            }
            dpos += dn_px;
            u += fa;
            v += fb;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gridfit_moves_edges_out() {
        let m = gridfit(Matrix {
            a: 10.4,
            b: 0.0,
            c: 0.0,
            d: 5.2,
            e: 1.3,
            f: 2.7,
        });
        assert_eq!((m.e, m.f), (1.0, 2.0));
        assert_eq!((m.a, m.d), (11.0, 6.0));
        // Turned a quarter, and flipped: b pairs with f, c with e, and
        // the negative sizes move their edges the other way. nupdf left
        // turned images off the grid.
        let m = gridfit(Matrix {
            a: 0.0,
            b: -30.0,
            c: -22.0,
            d: 0.0,
            e: 26.3,
            f: 64.5,
        });
        assert_eq!((m.e, m.f), (27.0, 65.0));
        assert_eq!((m.b, m.c), (-31.0, -23.0));
        // Neither upright nor turned a quarter: as it was.
        let skew = Matrix {
            a: 1.0,
            b: 0.5,
            c: 0.0,
            d: 1.0,
            e: 0.3,
            f: 0.3,
        };
        assert_eq!(gridfit(skew), skew);
    }

    #[test]
    fn subsampling_averages_squares() {
        let mut p = Pix {
            x: 0,
            y: 0,
            w: 2,
            h: 2,
            n: 1,
            alpha: false,
            samples: vec![0, 100, 200, 100],
        };
        subsample(&mut p, 1);
        assert_eq!((p.w, p.h), (1, 1));
        assert_eq!(p.samples, vec![100]);
    }

    #[test]
    fn halving_scales_by_weights() {
        let src = Pix {
            x: 0,
            y: 0,
            w: 4,
            h: 1,
            n: 1,
            alpha: false,
            samples: vec![0, 0, 255, 255],
        };
        let out = scale(&src, 0.0, 0.0, 2.0, 1.0, None).unwrap_or_else(|| panic!("no output"));
        assert_eq!(out.w, 2);
        assert!(
            out.samples[0] < 64 && out.samples[1] > 190,
            "{:?}",
            out.samples
        );
    }

    #[test]
    fn nearest_copies_an_upright_image() {
        let mut dst = Pix::new(IRect::new(0, 0, 2, 2), 3, false).unwrap_or_else(|e| panic!("{e}"));
        dst.clear_with(255);
        let img = Pix {
            x: 0,
            y: 0,
            w: 2,
            h: 2,
            n: 3,
            alpha: false,
            samples: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        };
        let ctm = Matrix {
            a: 2.0,
            b: 0.0,
            c: 0.0,
            d: 2.0,
            e: 0.0,
            f: 0.0,
        };
        paint_image(
            &mut dst,
            IRect::INFINITE,
            &img,
            ctm,
            None,
            255,
            true,
            false,
            None,
            None,
        );
        assert_eq!(dst.samples, img.samples);
    }

    use crate::draw::testkit::{check, hex_stream, page};

    /// `w` by `h` RGB bytes: red across, green down, blue checkered.
    fn rgb(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for y in 0..h {
            for x in 0..w {
                out.push((x * 255 / (w - 1).max(1)) as u8);
                out.push((y * 255 / (h - 1).max(1)) as u8);
                out.push(if (x / 2 + y / 2) % 2 == 0 { 200 } else { 30 });
            }
        }
        out
    }

    fn image(dict: &str, w: u32, h: u32, data: &[u8]) -> String {
        hex_stream(
            &format!("/Type/XObject/Subtype/Image/Width {w}/Height {h}{dict}"),
            data,
        )
    }

    /// A 128-point page drawing `content` with images /I0 to /I(n-1),
    /// which are objects 5 onwards, and `more` objects after them.
    fn image_page(
        name: &str,
        content: &str,
        images: &[String],
        more: &[&str],
        icc: bool,
        want: &str,
    ) {
        let mut res = String::from("/XObject<<");
        for i in 0..images.len() {
            res.push_str(&format!("/I{i} {} 0 R", i + 5));
        }
        res.push_str(">>/Pattern<</P<</PatternType 2/Shading<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 128 0]/Function<</FunctionType 2/Domain[0 1]/C0[1 0 0]/C1[0 0 1]/N 1>>>>>>>>/ExtGState<</H<</ca 0.5>>>>");
        let mut objs: Vec<&str> = images.iter().map(String::as_str).collect();
        objs.extend_from_slice(more);
        check(name, &page(128, 128, &res, content, &objs), icc, want);
    }

    #[test]
    fn images_scale_flip_and_rotate_onto_the_grid() {
        let small = image(
            "/ColorSpace/DeviceRGB/BitsPerComponent 8",
            16,
            16,
            &rgb(16, 16),
        );
        let odd = image(
            "/ColorSpace/DeviceRGB/BitsPerComponent 8",
            50,
            37,
            &rgb(50, 37),
        );
        let smooth = image(
            "/ColorSpace/DeviceRGB/BitsPerComponent 8/Interpolate true",
            16,
            16,
            &rgb(16, 16),
        );
        image_page(
            "image-transforms",
            "q 16 0 0 16 4 4 cm /I0 Do Q q -24 0 0 24 52 4.4 cm /I0 Do Q
             q 24 0 0 -24 56.6 52 cm /I0 Do Q q 0 20 -20 0 104 4 cm /I0 Do Q
             q 10 6 -6 10 112 30 cm /I0 Do Q q 0 -20 20 0 2 50 cm /I2 Do Q
             q 9 0 0 7 4 60 cm /I1 Do Q q 30 0 0 22 16 60 cm /I1 Do Q
             q 0 30 -22 0 72 60 cm /I1 Do Q q 20 3 -3 20 80 60 cm /I1 Do Q
             q 3 0 0 3 108 60 cm /I1 Do Q
             q 40 0 0 40 100 100 cm /I0 Do Q q 36 0 0 36 4 90 cm /I2 Do Q
             q /H gs 40 0 0 40 44 86 cm /I1 Do Q",
            &[small, odd, smooth],
            &[],
            true,
            "77a6ce0fbe68855997d1e4dee557af7d",
        );
    }

    #[test]
    fn images_decode_every_kind_of_sample() {
        let checker: Vec<u8> = (0..8)
            .map(|y| if y % 2 == 0 { 0xAA } else { 0x55 })
            .collect();
        let cmyk: Vec<u8> = (0..16u32)
            .flat_map(|i| [(i * 16) as u8, 255 - (i * 16) as u8, (i % 4 * 80) as u8, 20])
            .collect();
        // Indexed, 2 bits: 6 a row, padded to bytes.
        let indexed: Vec<u8> = (0..6u32)
            .flat_map(|y| {
                let v: Vec<u32> = (0..6).map(|x| (x + y) % 4).collect();
                [
                    (v[0] << 6 | v[1] << 4 | v[2] << 2 | v[3]) as u8,
                    (v[4] << 6 | v[5] << 4) as u8,
                ]
            })
            .collect();
        let wide: Vec<u8> = rgb(4, 4).iter().flat_map(|&v| [v, 0x80]).collect();
        let alpha: Vec<u8> = (0..256u32).map(|i| (i % 16 * 17) as u8).collect();
        let images = [
            image(
                "/ColorSpace/DeviceGray/BitsPerComponent 1/Decode[1 0]",
                8,
                8,
                &checker,
            ),
            image("/ColorSpace/DeviceCMYK/BitsPerComponent 8", 4, 4, &cmyk),
            image(
                "/ColorSpace[/Indexed/DeviceRGB 3<ff0000 00ff00 0000ff ffff00>]/BitsPerComponent 2",
                6,
                6,
                &indexed,
            ),
            image("/ColorSpace/DeviceRGB/BitsPerComponent 16", 4, 4, &wide),
            image(
                "/ColorSpace/DeviceRGB/BitsPerComponent 8/SMask 14 0 R",
                16,
                16,
                &rgb(16, 16),
            ),
            image(
                "/ColorSpace/DeviceRGB/BitsPerComponent 8/Decode[1 0 1 0 1 0]",
                16,
                16,
                &rgb(16, 16),
            ),
            image(
                "/ColorSpace/DeviceRGB/BitsPerComponent 8/Mask[0 120 0 120 0 255]",
                16,
                16,
                &rgb(16, 16),
            ),
            image(
                "/ColorSpace/DeviceRGB/BitsPerComponent 8/Mask 15 0 R",
                16,
                16,
                &rgb(16, 16),
            ),
            image(
                "/ColorSpace/DeviceGray/BitsPerComponent 4",
                4,
                2,
                &[0x01, 0x23, 0x45, 0x67],
            ),
        ];
        let smask = image("/ColorSpace/DeviceGray/BitsPerComponent 8", 16, 16, &alpha);
        let stencil = image("/ImageMask true/BitsPerComponent 1", 8, 8, &checker);
        let mut content = String::new();
        for i in 0..images.len() {
            let (x, y) = (4 + (i % 4) * 30, 4 + (i / 4) * 30);
            content.push_str(&format!("q 26 0 0 26 {x} {y} cm /I{i} Do Q\n"));
        }
        // Without ICC: lcms turns the CMYK image into RGB a level or two
        // off moxcms here and there.
        image_page(
            "image-kinds",
            &content,
            &images,
            &[&smask, &stencil],
            false,
            "4f7cd069493752cbed7465bcc3554431",
        );
    }

    #[test]
    fn stencil_masks_paint_in_the_fill_color_or_a_pattern() {
        let checker: Vec<u8> = (0..8)
            .map(|y| if y % 2 == 0 { 0xCC } else { 0x33 })
            .collect();
        let stencils = [
            image("/ImageMask true/BitsPerComponent 1", 8, 8, &checker),
            image(
                "/ImageMask true/BitsPerComponent 1/Decode[1 0]/Interpolate true",
                8,
                8,
                &checker,
            ),
        ];
        image_page(
            "image-stencils",
            "0.8 0.2 0.3 rg q 30 0 0 30 4 4 cm /I0 Do Q q 30 0 0 30 40 4 cm /I1 Do Q
             q 3 0 0 3 80 4 cm /I0 Do Q q 20 8 -8 20 100 4 cm /I0 Do Q
             q /H gs 30 0 0 30 4 40 cm /I0 Do Q
             /Pattern cs /P scn q 30 0 0 30 40 40 cm /I0 Do Q q 20 8 -8 20 100 40 cm /I1 Do Q
             q 30 0 0 30 4 80 cm /I1 Do Q",
            &stencils,
            &[],
            true,
            "32b1ca6c3408ac4ab5c9b2fece62959e",
        );
    }

    #[test]
    fn images_shown_small_decode_subsampled_as_mupdf_does() {
        // Images shown at under half their size decode by 2^n: streamed
        // samples averaged in plain blocks, before decode arrays and color
        // keys; indexed ones after their palette, with Bresenham's blocks
        // spread over an odd size. nupdf used Bresenham's for all.
        let indexed: Vec<u8> = (0..37u32)
            .flat_map(|y| (0..50u32).map(move |x| ((x / 3 + y / 5) % 4) as u8))
            .collect();
        let gray: Vec<u8> = (0..37u32)
            .flat_map(|y| (0..50u32).map(move |x| ((x * 5 + y * 3) % 256) as u8))
            .collect();
        let stencil: Vec<u8> = (0..61u32)
            .flat_map(|y| (0..8u32).map(move |x| if (x + y / 3) % 2 == 0 { 0xF0 } else { 0x3C }))
            .collect();
        let images = [
            image(
                "/ColorSpace/DeviceRGB/BitsPerComponent 8",
                50,
                37,
                &rgb(50, 37),
            ),
            image(
                "/ColorSpace[/Indexed/DeviceRGB 3<ff0000 00ff00 0000ff ffff00>]/BitsPerComponent 8",
                50,
                37,
                &indexed,
            ),
            image(
                "/ColorSpace/DeviceRGB/BitsPerComponent 8/Mask[0 80 0 255 0 255]/Decode[1 0 1 0 1 0]",
                50,
                37,
                &rgb(50, 37),
            ),
            image(
                "/ColorSpace/DeviceGray/BitsPerComponent 8/SMask 9 0 R",
                50,
                37,
                &gray,
            ),
            image("/ImageMask true/BitsPerComponent 1", 61, 61, &stencil),
        ];
        let mask = image("/ColorSpace/DeviceGray/BitsPerComponent 8", 50, 37, &gray);
        image_page(
            "image-subsampled",
            "q 9 0 0 7 4 4 cm /I0 Do Q q 9 0 0 7 20 4 cm /I1 Do Q q 3 0 0 3 36 4 cm /I1 Do Q
             q 9 0 0 7 44 4 cm /I2 Do Q q 11 0 0 8 60 4 cm /I3 Do Q
             0 0 1 rg q 7 0 0 7 76 4 cm /I4 Do Q q 14 0 0 14 90 4 cm /I4 Do Q
             q 0 9 -7 0 20 30 cm /I1 Do Q q 18 0 0 14 30 30 cm /I1 Do Q",
            &images,
            &[&mask],
            true,
            "66816cf0b6d5a0d5f4e5c31e51f08992",
        );
    }

    #[test]
    fn jpeg_images_shown_small_decode_in_the_idct_as_mupdf_does() {
        // MuPDF hands a JPEG's share of the subsampling (up to 2^3) to
        // libjpeg, whose reduced IDCTs (4x4, 2x2, 1x1 and their 2:1
        // shapes for subsampled chroma) make other samples than a full
        // decode averaged in blocks, which nupdf did. Each picture is
        // shown at a half, a quarter and an eighth (l2factor 1 to 3), and
        // turned a quarter at a half.
        let hex = |data: &[u8]| -> String {
            let mut h: String = data.iter().map(|b| format!("{b:02x}")).collect();
            h.push('>');
            h
        };
        macro_rules! jpeg {
            ($name:literal, $cs:literal) => {
                crate::draw::testkit::stream(
                    concat!(
                        "/Type/XObject/Subtype/Image/Width 37/Height 29/ColorSpace/",
                        $cs,
                        "/BitsPerComponent 8/Filter[/ASCIIHexDecode/DCTDecode]"
                    ),
                    &hex(include_bytes!(concat!(
                        "../../tests/data/jpeg/",
                        $name,
                        ".jpg"
                    ))),
                )
            };
        }
        let images = [
            jpeg!("base420", "DeviceRGB"),
            jpeg!("base422", "DeviceRGB"),
            jpeg!("base440", "DeviceRGB"),
            jpeg!("base411", "DeviceRGB"),
            jpeg!("base444", "DeviceRGB"),
            jpeg!("base13", "DeviceRGB"),
            jpeg!("mixed", "DeviceRGB"),
            jpeg!("sep44", "DeviceRGB"),
            jpeg!("prog", "DeviceRGB"),
            jpeg!("gray", "DeviceGray"),
            jpeg!("cmyk", "DeviceCMYK"),
        ];
        let mut content = String::new();
        for i in 0..images.len() {
            let y = 4 + 11 * i;
            content.push_str(&format!(
                "q 12 0 0 10 4 {y} cm /I{i} Do Q q 5 0 0 4 20 {y} cm /I{i} Do Q \
                 q 1 0 0 1 28 {y} cm /I{i} Do Q q 0 10 -12 0 46 {y} cm /I{i} Do Q\n"
            ));
        }
        image_page(
            "image-jpeg-scaled",
            &content,
            &images,
            &[],
            false,
            "4ab6073f5fe0a28339f70ef3d1bad801",
        );
    }

    #[test]
    fn jpx_images_decode_and_draw() {
        // Codestreams MuPDF wrote (tests/data/render/README.md).
        let gray = include_bytes!("../../tests/data/render/gray8.j2k");
        let rgb = include_bytes!("../../tests/data/render/rgb8.j2k");
        let jpx_image = |data: &[u8], dict: &str| {
            let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
            crate::draw::testkit::stream(
                &format!(
                    "/Type/XObject/Subtype/Image/Width 8/Height 8/BitsPerComponent 8{dict}/Filter[/ASCIIHexDecode/JPXDecode]"
                ),
                &format!("{hex}>"),
            )
        };
        let images = [
            jpx_image(gray, "/ColorSpace/DeviceGray"),
            jpx_image(rgb, "/ColorSpace/DeviceRGB"),
            jpx_image(rgb, "/ColorSpace/DeviceRGB/SMask 10 0 R"),
            jpx_image(gray, ""),
            jpx_image(rgb, "/ColorSpace/DeviceGray"),
        ];
        let smask = image(
            "/ColorSpace/DeviceGray/BitsPerComponent 8",
            2,
            1,
            &[255, 60],
        );
        image_page(
            "image-jpx",
            "q 40 0 0 40 4 4 cm /I0 Do Q q 40 0 0 40 50 4 cm /I1 Do Q
             q 40 0 0 40 4 60 cm /I2 Do Q q 40 0 0 40 50 60 cm /I3 Do Q
             q 20 0 0 20 100 4 cm /I4 Do Q q 3 0 0 3 100 100 cm /I1 Do Q",
            &images,
            &[&smask],
            true,
            "f55c0dff9e0370ffa67b3cf56c710937",
        );
    }

    #[test]
    fn matted_images_unblend_from_their_soft_mask_color() {
        // /Matte: the colors were blended with gray by the soft mask's
        // alpha, which decoding takes out again, at full size and shown
        // small (no streamed subsampling: Bresenham's blocks after).
        let alpha: Vec<u8> = (0..50u32 * 37).map(|i| (i % 50 * 5) as u8).collect();
        let images = [image(
            "/ColorSpace/DeviceRGB/BitsPerComponent 8/SMask 6 0 R",
            50,
            37,
            &rgb(50, 37),
        )];
        let mask = image(
            "/ColorSpace/DeviceGray/BitsPerComponent 8/Matte[0.5 0.5 0.5]",
            50,
            37,
            &alpha,
        );
        image_page(
            "image-matte",
            "q 100 0 0 74 4 4 cm /I0 Do Q q 9 0 0 7 4 100 cm /I0 Do Q",
            &images,
            &[&mask],
            true,
            "dc757a1684800776d18482df8c99429f",
        );
    }

    #[test]
    fn inline_keyed_and_truncated_images_draw() {
        // Color keys on 1, 2, 4 and 16-bit samples and on palette
        // indexes; data cut short, padded; inline images, filtered or not.
        let images = [
            image(
                "/ColorSpace/DeviceGray/BitsPerComponent 1/Mask[1 1]",
                8,
                2,
                &[0xF0, 0x0F],
            ),
            image(
                "/ColorSpace/DeviceGray/BitsPerComponent 2/Mask[1 2]",
                4,
                2,
                &[0b0001_1011, 0b1110_0100],
            ),
            image(
                "/ColorSpace/DeviceGray/BitsPerComponent 4/Mask[3 9]",
                4,
                2,
                &[0x01, 0x5A, 0xCF, 0x37],
            ),
            image(
                "/ColorSpace/DeviceGray/BitsPerComponent 16/Mask[0 30000]",
                2,
                2,
                &[0x10, 0, 0x80, 0, 0xFF, 0xFF, 0x00, 0x20],
            ),
            image(
                "/ColorSpace[/Indexed/DeviceRGB 3<ff0000 00ff00 0000ff ffff00>]/BitsPerComponent 8/Mask[1 2]",
                4,
                1,
                &[0, 1, 2, 3],
            ),
            image("/ColorSpace/DeviceRGB/BitsPerComponent 8", 4, 4, &rgb(4, 2)),
        ];
        image_page(
            "image-keyed",
            "0.9 0.8 0.2 rg 0 0 128 128 re f
             q 28 0 0 28 2 2 cm /I0 Do Q q 28 0 0 28 34 2 cm /I1 Do Q
             q 28 0 0 28 66 2 cm /I2 Do Q q 28 0 0 28 98 2 cm /I3 Do Q
             q 28 0 0 28 2 34 cm /I4 Do Q q 28 0 0 28 34 34 cm /I5 Do Q
             q 28 0 0 28 66 34 cm BI /W 2 /H 2 /CS /RGB /BPC 8 ID \u{1}\u{2}\u{3}\u{fe}\u{fd}\u{fc}\u{40}\u{80}\u{c0}\u{11}\u{22}\u{33} EI Q
             q 28 0 0 28 98 34 cm BI /W 4 /H 2 /CS /G /BPC 4 /F /AHx /D [1 0] ID 0123456789abcdef> EI Q
             q 28 0 0 28 2 66 cm BI /W 3 /H 3 /IM true /D [1 0] /F [/AHx] ID e0a040> EI Q",
            &images,
            &[],
            true,
            "284222802964b48d6695951695dfa230",
        );
    }

    #[test]
    fn images_scale_down_flipped_clipped_and_off_the_grid() {
        // Shrunk images mirrored each way and turned, at fractional
        // offsets (which give them an alpha edge), cut by the page's edge
        // and by a clip, translucent, and with a soft mask.
        let alpha: Vec<u8> = (0..50u32 * 37).map(|i| (i % 50 * 5) as u8).collect();
        let images = [
            image(
                "/ColorSpace/DeviceRGB/BitsPerComponent 8",
                50,
                37,
                &rgb(50, 37),
            ),
            image(
                "/ColorSpace/DeviceRGB/BitsPerComponent 8/SMask 6 0 R",
                50,
                37,
                &rgb(50, 37),
            ),
        ];
        let mask = image("/ColorSpace/DeviceGray/BitsPerComponent 8", 50, 37, &alpha);
        image_page(
            "image-scaled-flipped",
            "q -30 0 0 22 34.3 4.6 cm /I0 Do Q q 30 0 0 -22 40.5 26.2 cm /I0 Do Q
             q -30 0 0 -22 104.7 26.4 cm /I0 Do Q
             q 0 -30 -22 0 26.3 64.5 cm /I0 Do Q q 0 30 22 0 40.2 34.1 cm /I1 Do Q
             q -40 0 0 30 20 110 cm /I0 Do Q q 40 0 0 -30 110 30 cm /I0 Do Q
             q 70 70 30 30 re W n 40 0 0 30 64.4 64.4 cm /I1 Do Q
             q /H gs 33 0 0 25 90.5 90.5 cm /I0 Do Q
             q 0 -30 22 0 64 64 cm /I1 Do Q",
            &images,
            &[&mask],
            true,
            "12006e0ff12fd304484149a4c264ff64",
        );
    }

    #[test]
    fn color_keys_out_of_range_and_mismatched_mattes() {
        // Keys past the sample range are masked or clamped into it, and
        // 1-bit palette keys out of range are dropped or taken as 1; a
        // /Matte whose mask is another size is ignored.
        let pal = "[/Indexed/DeviceRGB 1<ff0000 0000ff>]";
        let images = [
            image(
                "/ColorSpace/DeviceGray/BitsPerComponent 8/Mask[300 400]",
                4,
                1,
                &[10, 44, 144, 200],
            ),
            image(
                "/ColorSpace/DeviceGray/BitsPerComponent 4/Mask[-3 2]",
                4,
                1,
                &[0x01, 0x2F],
            ),
            image(
                &format!("/ColorSpace{pal}/BitsPerComponent 1/Mask[5 5]"),
                8,
                1,
                &[0xA5],
            ),
            image(
                &format!("/ColorSpace{pal}/BitsPerComponent 1/Mask[0 5]"),
                8,
                1,
                &[0xA5],
            ),
            image(
                "/ColorSpace/DeviceGray/BitsPerComponent 1/Mask[0 3]",
                8,
                1,
                &[0xA5],
            ),
            image(
                "/ColorSpace/DeviceRGB/BitsPerComponent 8/SMask 10 0 R",
                4,
                4,
                &rgb(4, 4),
            ),
        ];
        let matte = image(
            "/ColorSpace/DeviceGray/BitsPerComponent 8/Matte[1 1 1]",
            2,
            2,
            &[255, 128, 64, 0],
        );
        image_page(
            "image-odd-keys",
            "0.2 0.8 0.3 rg 0 0 128 128 re f
             q 28 0 0 28 2 2 cm /I0 Do Q q 28 0 0 28 34 2 cm /I1 Do Q
             q 28 0 0 28 66 2 cm /I2 Do Q q 28 0 0 28 98 2 cm /I3 Do Q
             q 28 0 0 28 2 34 cm /I4 Do Q q 28 0 0 28 34 34 cm /I5 Do Q",
            &images,
            &[&matte],
            true,
            "18f0a45d302c71680d31c8ce214582a5",
        );
    }
}
