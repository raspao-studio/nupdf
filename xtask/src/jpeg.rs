// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! A small JPEG encoder for the DCT fixtures. Baseline or progressive
//! (spectral selection and DC successive approximation), Huffman coded with
//! the standard tables, any sampling factors, an optional Adobe marker and
//! restart interval. Image quality is not a goal; reaching decoder paths is.
//! The forward DCT is integer arithmetic over a fixed cosine table, so the
//! fixtures come out the same on every platform (libm's `cos` does not).

/// One component: its ID and sampling factors.
#[derive(Clone, Copy)]
pub struct Component {
    pub id: u8,
    pub h: usize,
    pub v: usize,
}

/// One progressive scan: components (indexes), spectral band, and
/// successive approximation bits.
#[derive(Clone)]
pub struct Scan {
    pub comps: Vec<usize>,
    pub ss: usize,
    pub se: usize,
    pub ah: u8,
    pub al: u8,
}

pub struct Spec {
    pub width: usize,
    pub height: usize,
    pub components: Vec<Component>,
    /// Sample of component `c` at full-resolution `(x, y)`.
    pub sample: fn(c: usize, x: usize, y: usize) -> u8,
    pub jfif: bool,
    /// The Adobe APP14 marker with this transform.
    pub adobe: Option<u8>,
    pub restart_interval: u16,
    /// `None` for one interleaved baseline scan.
    pub progressive: Option<Vec<Scan>>,
}

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

// Annex K tables (K.1, K.2, K.3).
const QUANT_LUM: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
const QUANT_CHROM: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];
const BITS_DC_LUM: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const BITS_DC_CHROM: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const VAL_DC: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const BITS_AC_LUM: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const VAL_AC_LUM: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
const BITS_AC_CHROM: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const VAL_AC_CHROM: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// A Huffman code table: (code, length) by symbol.
struct Codes([(u16, u8); 256]);

impl Codes {
    fn new(bits: &[u8; 16], vals: &[u8]) -> Codes {
        let mut codes = [(0, 0); 256];
        let mut code = 0u16;
        let mut k = 0;
        for (i, &n) in bits.iter().enumerate() {
            for _ in 0..n {
                codes[usize::from(vals[k])] = (code, i as u8 + 1);
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        Codes(codes)
    }
}

/// Entropy-coded output with byte stuffing.
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits {
    fn put(&mut self, value: u32, len: u32) {
        for i in (0..len).rev() {
            self.acc = self.acc << 1 | (value >> i & 1);
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.acc as u8);
                if self.acc == 0xff {
                    self.out.push(0);
                }
                self.acc = 0;
                self.n = 0;
            }
        }
    }

    fn code(&mut self, t: &Codes, symbol: u8) {
        let (code, len) = t.0[usize::from(symbol)];
        assert!(len > 0, "symbol {symbol:#x} has no code");
        self.put(u32::from(code), u32::from(len));
    }

    /// Pads the last byte with ones.
    fn flush(&mut self) {
        while self.n != 0 {
            self.put(1, 1);
        }
    }
}

/// Magnitude category and its bits (F.1.2.1).
fn category(v: i32) -> (u8, u32) {
    let size = 32 - v.unsigned_abs().leading_zeros();
    let bits = if v < 0 { v - 1 } else { v } as u32 & ((1 << size) - 1);
    (size as u8, bits)
}

fn segment(out: &mut Vec<u8>, marker: u8, body: &[u8]) {
    out.extend([0xff, marker]);
    out.extend(((body.len() + 2) as u16).to_be_bytes());
    out.extend(body);
}

/// Quantized coefficients of one component, in natural order, block by
/// block over the padded MCU grid.
struct Plane {
    blocks_w: usize,
    blocks: Vec<[i32; 64]>,
    /// Blocks covering the component itself (non-interleaved scans).
    width_in_blocks: usize,
    height_in_blocks: usize,
}

/// 2^13, the scale of the cosine table.
const ONE: i64 = 8192;
/// cos(k * pi / 16) * 2^13, rounded, for k = 0..8.
const COS: [i64; 8] = [8192, 8035, 7568, 6811, 5793, 4551, 3135, 1598];

/// cos(a * pi / 16) * 2^13.
fn cos16(a: usize) -> i64 {
    let a = a % 32;
    let a = if a > 16 { 32 - a } else { a };
    match a {
        0..8 => COS[a],
        8 => 0,
        _ => -COS[16 - a],
    }
}

/// `n / d` rounded half away from zero, as `f64::round` does.
fn div_round(n: i128, d: i128) -> i128 {
    if n >= 0 {
        (n + d / 2) / d
    } else {
        -((-n + d / 2) / d)
    }
}

fn quant_table(c: usize) -> [u16; 64] {
    let base = if c == 0 { QUANT_LUM } else { QUANT_CHROM };
    // Quality 75.
    base.map(|q| ((u32::from(q) * 50 + 50) / 100).clamp(1, 255) as u16)
}

fn plane(spec: &Spec, c: usize, max_h: usize, max_v: usize) -> Plane {
    let comp = spec.components[c];
    let cw = (spec.width * comp.h).div_ceil(max_h);
    let ch = (spec.height * comp.v).div_ceil(max_v);
    let mcus_x = spec.width.div_ceil(8 * max_h);
    let mcus_y = spec.height.div_ceil(8 * max_v);
    let (bw, bh) = (mcus_x * comp.h, mcus_y * comp.v);
    let q = quant_table(c);
    let sample = |x: usize, y: usize| -> i64 {
        let (x, y) = (x.min(cw - 1), y.min(ch - 1));
        let sx = (x * max_h / comp.h).min(spec.width - 1);
        let sy = (y * max_v / comp.v).min(spec.height - 1);
        i64::from((spec.sample)(c, sx, sy)) - 128
    };
    let mut blocks = Vec::with_capacity(bw * bh);
    for by in 0..bh {
        for bx in 0..bw {
            let mut block = [0i32; 64];
            for v in 0..8 {
                for u in 0..8 {
                    let mut sum: i64 = 0;
                    for y in 0..8 {
                        for x in 0..8 {
                            sum += sample(bx * 8 + x, by * 8 + y)
                                * cos16((2 * x + 1) * u)
                                * cos16((2 * y + 1) * v);
                        }
                    }
                    // F(u,v) = C(u) C(v) / 4 * sum, then quantized; C(0) is
                    // 1/sqrt(2). Everything is scaled by 2^13 per factor.
                    let c = |k: usize| if k == 0 { COS[4] } else { ONE };
                    let num = i128::from(sum) * i128::from(c(u)) * i128::from(c(v));
                    let den = 4 * i128::from(ONE).pow(4) * i128::from(q[v * 8 + u]);
                    block[v * 8 + u] = div_round(num, den) as i32;
                }
            }
            blocks.push(block);
        }
    }
    Plane {
        blocks_w: bw,
        blocks,
        width_in_blocks: cw.div_ceil(8),
        height_in_blocks: ch.div_ceil(8),
    }
}

/// The blocks of one scan in coding order, as (component, block index)
/// grouped by MCU.
fn mcus(
    spec: &Spec,
    planes: &[Plane],
    comps: &[usize],
    max_h: usize,
    max_v: usize,
) -> Vec<Vec<(usize, usize)>> {
    let mut out = Vec::new();
    if let [c] = comps {
        let p = &planes[*c];
        for by in 0..p.height_in_blocks {
            for bx in 0..p.width_in_blocks {
                out.push(vec![(*c, by * p.blocks_w + bx)]);
            }
        }
        return out;
    }
    let mcus_x = spec.width.div_ceil(8 * max_h);
    let mcus_y = spec.height.div_ceil(8 * max_v);
    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            let mut mcu = Vec::new();
            for &c in comps {
                let comp = spec.components[c];
                for y in 0..comp.v {
                    for x in 0..comp.h {
                        let (bx, by) = (mx * comp.h + x, my * comp.v + y);
                        mcu.push((c, by * planes[c].blocks_w + bx));
                    }
                }
            }
            out.push(mcu);
        }
    }
    out
}

pub fn encode(spec: &Spec) -> Vec<u8> {
    let n = spec.components.len();
    let max_h = spec.components.iter().map(|c| c.h).max().unwrap_or(1);
    let max_v = spec.components.iter().map(|c| c.v).max().unwrap_or(1);
    let planes: Vec<Plane> = (0..n).map(|c| plane(spec, c, max_h, max_v)).collect();
    let tables = [
        (
            Codes::new(&BITS_DC_LUM, &VAL_DC),
            Codes::new(&BITS_AC_LUM, &VAL_AC_LUM),
        ),
        (
            Codes::new(&BITS_DC_CHROM, &VAL_DC),
            Codes::new(&BITS_AC_CHROM, &VAL_AC_CHROM),
        ),
    ];
    let table = |c: usize| usize::from(c != 0);

    let mut out = vec![0xff, 0xd8];
    if spec.jfif {
        segment(&mut out, 0xe0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    }
    if let Some(t) = spec.adobe {
        segment(
            &mut out,
            0xee,
            &[b'A', b'd', b'o', b'b', b'e', 0, 100, 0, 0, 0, 0, t],
        );
    }
    for t in 0..n.min(2) {
        let mut body = vec![t as u8];
        let q = quant_table(t);
        body.extend(ZIGZAG.iter().map(|&i| q[i] as u8));
        segment(&mut out, 0xdb, &body);
    }
    let mut sof = vec![8];
    sof.extend((spec.height as u16).to_be_bytes());
    sof.extend((spec.width as u16).to_be_bytes());
    sof.push(n as u8);
    for (c, comp) in spec.components.iter().enumerate() {
        sof.extend([comp.id, (comp.h << 4 | comp.v) as u8, table(c) as u8]);
    }
    segment(
        &mut out,
        if spec.progressive.is_some() {
            0xc2
        } else {
            0xc0
        },
        &sof,
    );
    for (t, (bits, vals)) in [
        (0x00, (&BITS_DC_LUM, &VAL_DC[..])),
        (0x10, (&BITS_AC_LUM, &VAL_AC_LUM[..])),
        (0x01, (&BITS_DC_CHROM, &VAL_DC[..])),
        (0x11, (&BITS_AC_CHROM, &VAL_AC_CHROM[..])),
    ] {
        let mut body = vec![t];
        body.extend(bits);
        body.extend(vals);
        segment(&mut out, 0xc4, &body);
    }
    if spec.restart_interval != 0 {
        segment(&mut out, 0xdd, &spec.restart_interval.to_be_bytes());
    }

    let scans = spec.progressive.clone().unwrap_or_else(|| {
        vec![Scan {
            comps: (0..n).collect(),
            ss: 0,
            se: 63,
            ah: 0,
            al: 0,
        }]
    });
    for scan in &scans {
        let mut sos = vec![scan.comps.len() as u8];
        for &c in &scan.comps {
            let t = table(c) as u8;
            sos.extend([spec.components[c].id, t << 4 | t]);
        }
        sos.extend([scan.ss as u8, scan.se as u8, scan.ah << 4 | scan.al]);
        segment(&mut out, 0xda, &sos);

        let mut bits = Bits::default();
        let mut pred = vec![0i32; n];
        for (i, mcu) in mcus(spec, &planes, &scan.comps, max_h, max_v)
            .into_iter()
            .enumerate()
        {
            if spec.restart_interval != 0 && i > 0 && i % usize::from(spec.restart_interval) == 0 {
                bits.flush();
                bits.out.extend([
                    0xff,
                    0xd0 + ((i / usize::from(spec.restart_interval) - 1) % 8) as u8,
                ]);
                pred.fill(0);
            }
            for (c, b) in mcu {
                let block = &planes[c].blocks[b];
                let (dc_codes, ac_codes) = &tables[table(c)];
                if scan.ss == 0 {
                    if scan.ah != 0 {
                        bits.put((block[0] >> scan.al) as u32 & 1, 1);
                    } else {
                        let v = block[0] >> scan.al;
                        let (size, extra) = category(v - pred[c]);
                        pred[c] = v;
                        bits.code(dc_codes, size);
                        bits.put(extra, u32::from(size));
                    }
                }
                let ss = scan.ss.max(1);
                if scan.se >= ss {
                    assert_eq!(scan.ah, 0, "AC refinement scans are not supported");
                    let mut run = 0;
                    for k in ss..=scan.se {
                        let v = block[ZIGZAG[k]];
                        let v = if v < 0 {
                            -((-v) >> scan.al)
                        } else {
                            v >> scan.al
                        };
                        if v == 0 {
                            run += 1;
                            continue;
                        }
                        while run > 15 {
                            bits.code(ac_codes, 0xf0);
                            run -= 16;
                        }
                        let (size, extra) = category(v);
                        bits.code(ac_codes, run << 4 | size);
                        bits.put(extra, u32::from(size));
                        run = 0;
                    }
                    if run > 0 {
                        bits.code(ac_codes, 0x00);
                    }
                }
            }
        }
        bits.flush();
        out.extend(bits.out);
    }
    out.extend([0xff, 0xd9]);
    out
}
