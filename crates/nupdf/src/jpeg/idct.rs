// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from libjpeg 10 jidctint.c, as bundled with MuPDF 1.28.6,
// Copyright (C) 1991-1998, Thomas G. Lane. Modified 2002-2026 by Guido
// Vollbeding. This software is based in part on the work of the
// Independent JPEG Group.

//! The integer inverse DCTs libjpeg uses for 8-bit data: `jpeg_idct_islow`
//! and the 16-point variants (`16x16`, `16x8`, `8x16`) that upsample chroma
//! inside the transform. Arithmetic is 64-bit (`INT32` is `long` on the
//! platforms MuPDF builds for), and the workspace is `int`, so values are
//! truncated to 32 bits between the passes exactly as in C.

use super::Block;

const CONST_BITS: u32 = 13;
const PASS1_BITS: u32 = 2;
const PASS2_BITS: u32 = 5;
/// `RANGE_CENTER`: `CENTERJSAMPLE << RANGE_BITS`.
const RANGE_CENTER: i64 = 512;
const PASS2_OFFSET: i64 = (RANGE_CENTER << PASS2_BITS) + (1 << (PASS2_BITS - 1));

const FIX_0_298631336: i64 = 2446;
const FIX_0_390180644: i64 = 3196;
const FIX_0_541196100: i64 = 4433;
const FIX_0_765366865: i64 = 6270;
const FIX_0_899976223: i64 = 7373;
const FIX_1_175875602: i64 = 9633;
const FIX_1_501321110: i64 = 12299;
const FIX_1_847759065: i64 = 15137;
const FIX_1_961570560: i64 = 16069;
const FIX_2_053119869: i64 = 16819;
const FIX_2_562915447: i64 = 20995;
const FIX_3_072711026: i64 = 25172;
// FIX(x) for the 16-point transforms.
const F_1_306562965: i64 = 10703;
const F_0_275899379: i64 = 2260;
const F_1_387039845: i64 = 11363;
const F_0_601344887: i64 = 4926;
const F_0_509795579: i64 = 4176;
const F_1_353318001: i64 = 11086;
const F_1_247225013: i64 = 10217;
const F_1_093201867: i64 = 8956;
const F_0_897167586: i64 = 7350;
const F_0_666655658: i64 = 5461;
const F_0_410524528: i64 = 3363;
const F_2_286341144: i64 = 18730;
const F_1_835730603: i64 = 15038;
const F_0_138617169: i64 = 1136;
const F_0_071888074: i64 = 589;
const F_1_125726048: i64 = 9222;
const F_1_407403738: i64 = 11529;
const F_0_766367282: i64 = 6278;
const F_1_971951411: i64 = 16154;
const F_1_065388962: i64 = 8728;
const F_3_141271809: i64 = 25733;

/// Which transform a component uses, from its scaled block size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Islow,
    I16x16,
    I16x8,
    I8x16,
}

impl Method {
    /// `jddctmgr.c` `start_pass`, for the sizes 8x8 blocks can scale to.
    pub fn for_size(h: usize, v: usize) -> Option<Method> {
        match (h, v) {
            (8, 8) => Some(Method::Islow),
            (16, 16) => Some(Method::I16x16),
            (16, 8) => Some(Method::I16x8),
            (8, 16) => Some(Method::I8x16),
            _ => None,
        }
    }
}

/// `range_limit[x & RANGE_MASK]` with `IDCT_range_limit`'s offset: the
/// index is centered on 512 and wraps at 1024, so overflow saturates.
fn limit(x: i64) -> u8 {
    let i = x & 1023;
    if i < 384 {
        0
    } else if i < 640 {
        (i - 384) as u8
    } else {
        255
    }
}

fn dequantize(coef: &Block, table: &[i32; 64], i: usize) -> i64 {
    // `int` times `int`: coefficient and quantizer both fit 16 bits.
    i64::from(i32::from(coef[i]) * table[i])
}

/// `CLAMP_DC`, applied to the DC term of the first column only.
fn clamp_dc(v: i64) -> i64 {
    v.clamp(-1024, 1023)
}

/// The 8-point even/odd kernel shared by both passes of `jpeg_idct_islow`.
/// `z2` and `z3` are the scaled terms 0 and 4; `x` holds terms 1-7 as
/// `[_, x1, x2, x3, _, x5, x6, x7]`. Returns outputs 0..8.
fn kernel8(z2: i64, z3: i64, x: [i64; 8]) -> [i64; 8] {
    let tmp0 = z2 + z3;
    let tmp1 = z2 - z3;
    let (z2, z3) = (x[2], x[6]);
    let z1 = (z2 + z3) * FIX_0_541196100;
    let tmp2 = z1 + z2 * FIX_0_765366865;
    let tmp3 = z1 - z3 * FIX_1_847759065;
    let tmp10 = tmp0 + tmp2;
    let tmp13 = tmp0 - tmp2;
    let tmp11 = tmp1 + tmp3;
    let tmp12 = tmp1 - tmp3;

    let (mut tmp0, mut tmp1, mut tmp2, mut tmp3) = (x[7], x[5], x[3], x[1]);
    let z2 = tmp0 + tmp2;
    let z3 = tmp1 + tmp3;
    let z1 = (z2 + z3) * FIX_1_175875602;
    let z2 = z2 * -FIX_1_961570560 + z1;
    let z3 = z3 * -FIX_0_390180644 + z1;
    let z1 = (tmp0 + tmp3) * -FIX_0_899976223;
    tmp0 = tmp0 * FIX_0_298631336 + z1 + z2;
    tmp3 = tmp3 * FIX_1_501321110 + z1 + z3;
    let z1 = (tmp1 + tmp2) * -FIX_2_562915447;
    tmp1 = tmp1 * FIX_2_053119869 + z1 + z3;
    tmp2 = tmp2 * FIX_3_072711026 + z1 + z2;

    [
        tmp10 + tmp3,
        tmp11 + tmp2,
        tmp12 + tmp1,
        tmp13 + tmp0,
        tmp13 - tmp0,
        tmp12 - tmp1,
        tmp11 - tmp2,
        tmp10 - tmp3,
    ]
}

/// The 16-point kernel shared by every 16-point pass. `tmp0` is the scaled
/// DC term; `x` holds the 8 inputs (x[0] unused). Returns outputs 0..16.
fn kernel16(tmp0: i64, x: [i64; 8]) -> [i64; 16] {
    let z1 = x[4];
    let tmp1 = z1 * F_1_306562965;
    let tmp2 = z1 * FIX_0_541196100;
    let tmp10 = tmp0 + tmp1;
    let tmp11 = tmp0 - tmp1;
    let tmp12 = tmp0 + tmp2;
    let tmp13 = tmp0 - tmp2;

    let (z1, z2) = (x[2], x[6]);
    let z3 = z1 - z2;
    let z4 = z3 * F_0_275899379;
    let z3 = z3 * F_1_387039845;
    let tmp0 = z3 + z2 * FIX_2_562915447;
    let tmp1 = z4 + z1 * FIX_0_899976223;
    let tmp2 = z3 - z1 * F_0_601344887;
    let tmp3 = z4 - z2 * F_0_509795579;

    let tmp20 = tmp10 + tmp0;
    let tmp27 = tmp10 - tmp0;
    let tmp21 = tmp12 + tmp1;
    let tmp26 = tmp12 - tmp1;
    let tmp22 = tmp13 + tmp2;
    let tmp25 = tmp13 - tmp2;
    let tmp23 = tmp11 + tmp3;
    let tmp24 = tmp11 - tmp3;

    let (z1, mut z2, z3, z4) = (x[1], x[3], x[5], x[7]);
    let mut tmp11 = z1 + z3;
    let mut tmp1 = (z1 + z2) * F_1_353318001;
    let mut tmp2 = tmp11 * F_1_247225013;
    let mut tmp3 = (z1 + z4) * F_1_093201867;
    let mut tmp10 = (z1 - z4) * F_0_897167586;
    tmp11 *= F_0_666655658;
    let mut tmp12 = (z1 - z2) * F_0_410524528;
    let tmp0 = tmp1 + tmp2 + tmp3 - z1 * F_2_286341144;
    let tmp13 = tmp10 + tmp11 + tmp12 - z1 * F_1_835730603;
    let w = (z2 + z3) * F_0_138617169;
    tmp1 += w + z2 * F_0_071888074;
    tmp2 += w - z3 * F_1_125726048;
    let w = (z3 - z2) * F_1_407403738;
    tmp11 += w - z3 * F_0_766367282;
    tmp12 += w + z2 * F_1_971951411;
    z2 += z4;
    let w = z2 * -F_0_666655658;
    tmp1 += w;
    tmp3 += w + z4 * F_1_065388962;
    z2 *= -F_1_247225013;
    tmp10 += z2 + z4 * F_3_141271809;
    tmp12 += z2;
    let w = (z3 + z4) * -F_1_353318001;
    tmp2 += w;
    tmp3 += w;
    let w = (z4 - z3) * F_0_410524528;
    tmp10 += w;
    tmp11 += w;

    [
        tmp20 + tmp0,
        tmp21 + tmp1,
        tmp22 + tmp2,
        tmp23 + tmp3,
        tmp24 + tmp10,
        tmp25 + tmp11,
        tmp26 + tmp12,
        tmp27 + tmp13,
        tmp27 - tmp13,
        tmp26 - tmp12,
        tmp25 - tmp11,
        tmp24 - tmp10,
        tmp23 - tmp3,
        tmp22 - tmp2,
        tmp21 - tmp1,
        tmp20 - tmp0,
    ]
}

/// Pass 1 of the 8-point transforms: columns into `ws` (8 rows of 8).
fn columns8(coef: &Block, table: &[i32; 64], ws: &mut [i32]) {
    for c in 0..8 {
        let q = |r: usize| dequantize(coef, table, r * 8 + c);
        if (1..8).all(|r| coef[r * 8 + c] == 0) {
            let mut dc = q(0);
            if c == 0 {
                dc = clamp_dc(dc);
            }
            // `int` shift; the full path truncates the same way.
            let v = (dc as i32).wrapping_shl(PASS1_BITS);
            for r in 0..8 {
                ws[r * 8 + c] = v;
            }
            continue;
        }
        let mut z2 = q(0);
        if c == 0 {
            z2 = clamp_dc(z2);
        }
        let z2 = (z2 << CONST_BITS) + (1 << (CONST_BITS - PASS1_BITS - 1));
        let z3 = q(4) << CONST_BITS;
        let out = kernel8(z2, z3, [0, q(1), q(2), q(3), 0, q(5), q(6), q(7)]);
        for (r, v) in out.into_iter().enumerate() {
            ws[r * 8 + c] = (v >> (CONST_BITS - PASS1_BITS)) as i32;
        }
    }
}

/// Pass 1 of the 16-point transforms: 8 columns into `ws` (16 rows of 8).
fn columns16(coef: &Block, table: &[i32; 64], ws: &mut [i32]) {
    for c in 0..8 {
        let q = |r: usize| dequantize(coef, table, r * 8 + c);
        let mut tmp0 = q(0);
        if c == 0 {
            tmp0 = clamp_dc(tmp0);
        }
        let tmp0 = (tmp0 << CONST_BITS) + (1 << (CONST_BITS - PASS1_BITS - 1));
        let out = kernel16(tmp0, [0, q(1), q(2), q(3), q(4), q(5), q(6), q(7)]);
        for (r, v) in out.into_iter().enumerate() {
            ws[r * 8 + c] = (v >> (CONST_BITS - PASS1_BITS)) as i32;
        }
    }
}

/// Pass 2, 8 points per row.
fn rows8(ws: &[i32], rows: usize, out: &mut [u8], stride: usize, at: usize) {
    for r in 0..rows {
        let w = |i: usize| i64::from(ws[r * 8 + i]);
        let Some(dst) = out.get_mut(at + r * stride..at + r * stride + 8) else {
            return;
        };
        let z2 = w(0) + PASS2_OFFSET;
        if (1..8).all(|i| ws[r * 8 + i] == 0) {
            dst.fill(limit(z2 >> PASS2_BITS));
            continue;
        }
        let res = kernel8(
            z2 << CONST_BITS,
            w(4) << CONST_BITS,
            [0, w(1), w(2), w(3), 0, w(5), w(6), w(7)],
        );
        for (d, v) in dst.iter_mut().zip(res) {
            *d = limit(v >> (CONST_BITS + PASS2_BITS));
        }
    }
}

/// Pass 2, 16 points per row.
fn rows16(ws: &[i32], rows: usize, out: &mut [u8], stride: usize, at: usize) {
    for r in 0..rows {
        let w = |i: usize| i64::from(ws[r * 8 + i]);
        let Some(dst) = out.get_mut(at + r * stride..at + r * stride + 16) else {
            return;
        };
        let tmp0 = (w(0) + PASS2_OFFSET) << CONST_BITS;
        let res = kernel16(tmp0, [0, w(1), w(2), w(3), w(4), w(5), w(6), w(7)]);
        for (d, v) in dst.iter_mut().zip(res) {
            *d = limit(v >> (CONST_BITS + PASS2_BITS));
        }
    }
}

/// Transforms one block into `out` at offset `at` (rows `stride` apart).
pub(crate) fn inverse(
    method: Method,
    coef: &Block,
    table: &[i32; 64],
    out: &mut [u8],
    stride: usize,
    at: usize,
) {
    let mut ws = [0i32; 128];
    match method {
        Method::Islow => {
            columns8(coef, table, &mut ws);
            rows8(&ws, 8, out, stride, at);
        }
        Method::I16x16 => {
            columns16(coef, table, &mut ws);
            rows16(&ws, 16, out, stride, at);
        }
        Method::I16x8 => {
            columns8(coef, table, &mut ws);
            rows16(&ws, 8, out, stride, at);
        }
        Method::I8x16 => {
            columns16(coef, table, &mut ws);
            rows8(&ws, 16, out, stride, at);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(method: Method, coef: &Block, q: i32) -> Vec<u8> {
        let (w, h) = match method {
            Method::Islow => (8, 8),
            Method::I16x16 => (16, 16),
            Method::I16x8 => (16, 8),
            Method::I8x16 => (8, 16),
        };
        let mut out = vec![0u8; w * h];
        inverse(method, coef, &[q; 64], &mut out, w, 0);
        out
    }

    #[test]
    fn dc_only_blocks_are_flat() {
        // DC 80 with quantizer 1 is 80/8 = 10 above mid-gray.
        let mut coef = [0i16; 64];
        coef[0] = 80;
        for m in [Method::Islow, Method::I16x16, Method::I16x8, Method::I8x16] {
            assert!(run(m, &coef, 1).iter().all(|&v| v == 138), "{m:?}");
        }
    }

    #[test]
    fn dc_is_clamped_and_samples_saturate() {
        let mut coef = [0i16; 64];
        coef[0] = i16::MAX;
        // Clamped to 1023 * 1, so 128 + 127.875 rounds to 255.
        assert!(run(Method::Islow, &coef, 1).iter().all(|&v| v == 255));
        coef[0] = i16::MIN;
        assert!(run(Method::Islow, &coef, 1).iter().all(|&v| v == 0));
    }

    #[test]
    fn ac_terms_shade_the_block() {
        let mut coef = [0i16; 64];
        coef[1] = 100;
        let out = run(Method::Islow, &coef, 1);
        // A horizontal cosine: bright on the left, dark on the right.
        assert!(out[0] > 128 && out[7] < 128);
        assert_eq!(out[0], out[8]);
        assert_eq!(u16::from(out[0]) + u16::from(out[7]), 256);
    }
}
