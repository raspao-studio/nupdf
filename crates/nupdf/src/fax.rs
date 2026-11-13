// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/filter-fax.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! The CCITT Group 3 and 4 fax decoder (`CCITTFaxDecode`), ported from
//! MuPDF's so that damaged data decodes to the same rows: an invalid code
//! ends the image after the row it was in, and missing data ends it at the
//! last complete row.

use crate::diag::{Diagnostics, report};
use crate::stream::{Next, Source, Stream};

/// Codes in the tables that are not runs.
const ERROR: i32 = -1;
const UNCOMPRESSED: i32 = -3;
/// 2D codes.
const P: i32 = -4;
const H: i32 = -5;
const VR3: i32 = 0;
const VR2: i32 = 1;
const VR1: i32 = 2;
const V0: i32 = 3;
const VL1: i32 = 4;
const VL2: i32 = 5;
const VL3: i32 = 6;

const WHITE_BITS: u32 = 8;
const BLACK_BITS: u32 = 7;
const TWO_D_BITS: u32 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Init,
    Normal,
    Makeup,
    Eol,
    H1,
    H2,
    Done,
}

/// Parameters from /DecodeParms, with MuPDF's defaults.
#[derive(Debug, Clone, Copy)]
pub struct FaxParams {
    pub k: i32,
    pub end_of_line: bool,
    pub encoded_byte_align: bool,
    pub columns: i32,
    pub rows: i32,
    pub end_of_block: bool,
    pub black_is_1: bool,
}

/// `fz_open_faxd`.
pub fn open(chain: Stream<'_>, p: FaxParams) -> Result<Stream<'_>, String> {
    if p.columns < 0 || p.columns >= i32::MAX - 7 {
        return Err(format!(
            "limit error: too many columns integer overflow ({})",
            p.columns
        ));
    }
    let diag = chain.diag();
    let stride = (((p.columns - 1) >> 3) + 1) as usize;
    Ok(Stream::new(
        Fax {
            chain,
            p,
            stride,
            ridx: 0,
            bidx: 32,
            word: 0,
            stage: Stage::Init,
            a: -1,
            c: false,
            dim: if p.k < 0 { 2 } else { 1 },
            eolc: 0,
            reference: vec![0; stride],
            dst: vec![0; stride],
            rp: 0,
            wp: stride,
        },
        diag,
    ))
}

struct Fax<'a> {
    chain: Stream<'a>,
    p: FaxParams,
    stride: usize,
    ridx: i32,
    bidx: i32,
    word: u32,
    stage: Stage,
    /// The current position in the row (-1 before the first code), and
    /// whether the current colour is black.
    a: i32,
    c: bool,
    dim: i32,
    eolc: i32,
    reference: Vec<u8>,
    dst: Vec<u8>,
    /// The part of `dst` not yet handed out.
    rp: usize,
    wp: usize,
}

/// Where `next_faxd` jumps.
enum Goto {
    Loop,
    Eol,
    Error,
    Rtc,
}

impl Fax<'_> {
    fn eat_bits(&mut self, n: u32) {
        self.word = self.word.checked_shl(n).unwrap_or(0);
        self.bidx += n as i32;
    }

    /// Reads bytes until at least 13 bits are buffered; false at the end.
    fn fill_bits(&mut self) -> bool {
        while self.bidx > 32 - 13 {
            let Some(c) = self.chain.read_byte() else {
                return false;
            };
            self.bidx -= 8;
            self.word |= u32::from(c) << self.bidx;
        }
        true
    }

    fn get_code(&mut self, table: &[(i16, i16)], initial: u32) -> i32 {
        let entry = |i: usize| {
            table
                .get(i)
                .map_or((ERROR, 0), |&(v, n)| (i32::from(v), i32::from(n)))
        };
        let word = self.word;
        let (mut val, mut nbits) = entry((word >> (32 - initial)) as usize);
        if nbits > initial as i32 {
            let wordmask = (1u32 << (32 - initial)) - 1;
            let tidx = val as i64 + i64::from((word & wordmask) >> (32 - nbits as u32));
            let (v, n) = entry(usize::try_from(tidx).unwrap_or(usize::MAX));
            val = v;
            nbits = initial as i32 + n;
        }
        self.eat_bits(nbits.max(0) as u32);
        val
    }

    fn run(&mut self, what: &str) -> Result<(), String> {
        if self.a == -1 {
            self.a = 0;
        }
        let code = if self.c {
            self.get_code(&BLACK, BLACK_BITS)
        } else {
            self.get_code(&WHITE, WHITE_BITS)
        };
        if code == UNCOMPRESSED {
            return Err(report::format("uncompressed data in faxd"));
        }
        if code < 0 {
            return Err(report::format(&format!("negative code in {what} faxd")));
        }
        if self.a + code > self.p.columns {
            return Err(report::format(&format!("overflow in {what} faxd")));
        }
        if self.c {
            set_bits(&mut self.dst, self.a, self.a + code);
        }
        self.a += code;
        let one_d = what == "1d";
        if code < 64 {
            self.c = !self.c;
            self.stage = match self.stage {
                _ if one_d => Stage::Normal,
                Stage::H1 => Stage::H2,
                Stage::H2 => Stage::Normal,
                other => other,
            };
        } else if one_d {
            self.stage = Stage::Makeup;
        }
        Ok(())
    }

    /// `dec2d`.
    fn dec2d(&mut self) -> Result<(), String> {
        if self.stage == Stage::H1 || self.stage == Stage::H2 {
            return self.run("2d");
        }
        let code = self.get_code(&TWO_D, TWO_D_BITS);
        let columns = self.p.columns;
        let changing = |f: &Self| find_changing_color(&f.reference, f.a, columns, !f.c);
        let vertical = |f: &mut Self, delta: i32| {
            let b1 = (delta + changing(f)).clamp(0, columns);
            if f.c {
                set_bits(&mut f.dst, f.a, b1);
            }
            f.a = b1;
            f.c = !f.c;
        };
        match code {
            H => self.stage = Stage::H1,
            P => {
                let b1 = changing(self);
                let b2 = if b1 >= columns {
                    columns
                } else {
                    find_changing(&self.reference, b1, columns)
                };
                if self.c {
                    set_bits(&mut self.dst, self.a, b2);
                }
                self.a = b2;
            }
            V0 => vertical(self, 0),
            VR1 => vertical(self, 1),
            VR2 => vertical(self, 2),
            VR3 => vertical(self, 3),
            VL1 => vertical(self, -1),
            VL2 => vertical(self, -2),
            VL3 => vertical(self, -3),
            UNCOMPRESSED => return Err(report::format("uncompressed data in faxd")),
            ERROR => return Err(report::format("invalid code in 2d faxd")),
            other => {
                return Err(report::format(&format!(
                    "invalid code in 2d faxd ({other})"
                )));
            }
        }
        Ok(())
    }

    /// Copies the rest of the current row out, inverted unless /BlackIs1.
    fn copy_row(&mut self, out: &mut Vec<u8>, max: usize) {
        let n = (self.wp - self.rp).min(max - out.len());
        let invert = if self.p.black_is_1 { 0 } else { 0xff };
        out.extend(self.dst[self.rp..self.rp + n].iter().map(|b| b ^ invert));
        self.rp += n;
    }
}

impl Source for Fax<'_> {
    fn next(&mut self, max: usize, out: &mut Vec<u8>, diag: &Diagnostics) -> Result<Next, String> {
        let max = max.clamp(1, 4096);
        let top = |f: &Self| f.word >> (32 - 12);
        if self.stage == Stage::Init && self.p.end_of_line {
            self.fill_bits();
            if top(self) != 1 {
                diag.warn("filter.fax_no_eol", "faxd stream doesn't start with EOL");
                while self.fill_bits() && top(self) != 1 {
                    self.eat_bits(1);
                }
            }
            if top(self) != 1 {
                return Err(report::format("initial EOL not found"));
            }
        }
        if self.stage == Stage::Init {
            self.stage = Stage::Normal;
        }
        if self.stage == Stage::Done {
            return Ok(Next::Eof);
        }
        let mut goto = if self.stage == Stage::Eol {
            Goto::Eol
        } else {
            Goto::Loop
        };
        loop {
            match goto {
                Goto::Loop => {
                    goto = Goto::Loop;
                    if !self.fill_bits() && self.bidx > 31 {
                        goto = if self.a > 0 { Goto::Eol } else { Goto::Rtc };
                        continue;
                    }
                    if top(self) == 0 {
                        self.eat_bits(1);
                        continue;
                    }
                    let decoded = if top(self) == 1 {
                        self.eat_bits(12);
                        self.eolc += 1;
                        if self.p.k > 0 {
                            if self.a == -1 {
                                self.a = 0;
                            }
                            self.dim = if self.word >> 31 == 1 { 1 } else { 2 };
                            self.eat_bits(1);
                        }
                        Ok(())
                    } else if self.p.k > 0 && self.a == -1 {
                        self.a = 0;
                        self.dim = if self.word >> 31 == 1 { 1 } else { 2 };
                        self.eat_bits(1);
                        Ok(())
                    } else if self.dim == 1 {
                        self.eolc = 0;
                        self.run("1d")
                    } else if self.dim == 2 {
                        self.eolc = 0;
                        self.dec2d()
                    } else {
                        Ok(())
                    };
                    if let Err(e) = decoded {
                        for line in e.lines() {
                            diag.warn("error.reported", line);
                        }
                        goto = Goto::Error;
                        continue;
                    }
                    if matches!(self.stage, Stage::Makeup | Stage::H1 | Stage::H2) {
                        continue;
                    }
                    if self.eolc > 0 || self.a >= self.p.columns {
                        if self.a > 0 {
                            goto = Goto::Eol;
                        } else if self.eolc == if self.p.k < 0 { 2 } else { 6 } {
                            goto = Goto::Rtc;
                        }
                    }
                }
                Goto::Eol => {
                    self.stage = Stage::Eol;
                    self.copy_row(out, max);
                    if self.rp < self.wp {
                        break;
                    }
                    std::mem::swap(&mut self.reference, &mut self.dst);
                    self.dst.fill(0);
                    self.rp = 0;
                    self.wp = self.stride;
                    self.stage = Stage::Normal;
                    self.c = false;
                    self.a = -1;
                    self.ridx += 1;
                    if !self.p.end_of_block && self.p.rows > 0 && self.ridx >= self.p.rows {
                        goto = Goto::Rtc;
                        continue;
                    }
                    // No dimension was read from an EOL: guess it.
                    if self.p.k > 0 && self.eolc == 0 && self.a == -1 {
                        self.dim = if self.ridx % self.p.k == 0 { 1 } else { 2 };
                    }
                    if self.p.encoded_byte_align && !self.p.end_of_line {
                        self.eat_bits(((8 - self.bidx) & 7) as u32);
                    }
                    if out.len() == max {
                        break;
                    }
                    goto = Goto::Loop;
                }
                Goto::Error => {
                    // The row up to the error is kept.
                    self.copy_row(out, max);
                    goto = Goto::Rtc;
                }
                Goto::Rtc => {
                    self.stage = Stage::Done;
                    break;
                }
            }
        }
        Ok(if out.is_empty() {
            Next::Eof
        } else {
            Next::Data
        })
    }
}

fn get_bit(line: &[u8], x: i32) -> bool {
    line.get((x >> 3) as usize)
        .is_some_and(|b| (b >> (7 - (x & 7))) & 1 == 1)
}

const MASK: [u8; 8] = [0x7f, 0x3f, 0x1f, 0x0f, 0x07, 0x03, 0x01, 0];

/// `find_changing`: the next position after `x` where the colour changes,
/// or `w`.
fn find_changing(line: &[u8], x: i32, w: i32) -> i32 {
    let at = |i: i32| i32::from(line.get(i as usize).copied().unwrap_or(0));
    let (mut x, m) = if x < 0 {
        (0, 0xff)
    } else {
        (x, i32::from(MASK[(x & 7) as usize]))
    };
    let whole = w >> 3;
    x >>= 3;
    let mut a = at(x);
    let mut b = (a ^ (a >> 1)) & m;
    if x >= whole {
        return ((x << 3) + i32::from(CLZ[b as usize])).min(w);
    }
    while b == 0 {
        x += 1;
        if x >= whole {
            if (x << 3) == w {
                return w;
            }
            b = a & 1;
            a = at(x);
            b = ((b << 7) ^ a ^ (a >> 1)) & 0xff;
            return ((x << 3) + i32::from(CLZ[b as usize])).min(w);
        }
        b = a & 1;
        a = at(x);
        b = ((b << 7) ^ a ^ (a >> 1)) & 0xff;
    }
    (x << 3) + i32::from(CLZ[b as usize])
}

/// `find_changing_color`: the next change to `color` after `x`.
fn find_changing_color(line: &[u8], x: i32, w: i32, color: bool) -> i32 {
    if x >= w {
        return w;
    }
    let mut x = find_changing(line, if x > 0 || !color { x } else { -1 }, w);
    if x < w && get_bit(line, x) != color {
        x = find_changing(line, x, w);
    }
    x
}

const LM: [u8; 8] = [0xff, 0x7f, 0x3f, 0x1f, 0x0f, 0x07, 0x03, 0x01];
const RM: [u8; 8] = [0x00, 0x80, 0xc0, 0xe0, 0xf0, 0xf8, 0xfc, 0xfe];

/// Sets bits `x0..x1` of `line` (black pixels).
fn set_bits(line: &mut [u8], x0: i32, x1: i32) {
    if x1 <= x0 || x0 < 0 {
        return;
    }
    let (a0, a1) = ((x0 >> 3) as usize, (x1 >> 3) as usize);
    let (b0, b1) = ((x0 & 7) as usize, (x1 & 7) as usize);
    let mut or = |i: usize, v: u8| {
        if let Some(byte) = line.get_mut(i) {
            *byte |= v;
        }
    };
    if a0 == a1 {
        if b1 != 0 {
            or(a0, LM[b0] & RM[b1]);
        }
    } else {
        or(a0, LM[b0]);
        for i in a0 + 1..a1 {
            or(i, 0xff);
        }
        if b1 != 0 {
            or(a1, RM[b1]);
        }
    }
}

#[rustfmt::skip]
const WHITE: [(i16, i16); 304] = [
    (256, 12), (272, 12), (29, 8), (30, 8), (45, 8), (46, 8), (22, 7), (22, 7), (23, 7),
    (23, 7), (47, 8), (48, 8), (13, 6), (13, 6), (13, 6), (13, 6), (20, 7), (20, 7), (33, 8),
    (34, 8), (35, 8), (36, 8), (37, 8), (38, 8), (19, 7), (19, 7), (31, 8), (32, 8), (1, 6),
    (1, 6), (1, 6), (1, 6), (12, 6), (12, 6), (12, 6), (12, 6), (53, 8), (54, 8), (26, 7),
    (26, 7), (39, 8), (40, 8), (41, 8), (42, 8), (43, 8), (44, 8), (21, 7), (21, 7), (28, 7),
    (28, 7), (61, 8), (62, 8), (63, 8), (0, 8), (320, 8), (384, 8), (10, 5), (10, 5), (10, 5),
    (10, 5), (10, 5), (10, 5), (10, 5), (10, 5), (11, 5), (11, 5), (11, 5), (11, 5), (11, 5),
    (11, 5), (11, 5), (11, 5), (27, 7), (27, 7), (59, 8), (60, 8), (288, 9), (290, 9), (18, 7),
    (18, 7), (24, 7), (24, 7), (49, 8), (50, 8), (51, 8), (52, 8), (25, 7), (25, 7), (55, 8),
    (56, 8), (57, 8), (58, 8), (192, 6), (192, 6), (192, 6), (192, 6), (1664, 6), (1664, 6),
    (1664, 6), (1664, 6), (448, 8), (512, 8), (292, 9), (640, 8), (576, 8), (294, 9), (296, 9),
    (298, 9), (300, 9), (302, 9), (256, 7), (256, 7), (2, 4), (2, 4), (2, 4), (2, 4), (2, 4),
    (2, 4), (2, 4), (2, 4), (2, 4), (2, 4), (2, 4), (2, 4), (2, 4), (2, 4), (2, 4), (2, 4),
    (3, 4), (3, 4), (3, 4), (3, 4), (3, 4), (3, 4), (3, 4), (3, 4), (3, 4), (3, 4), (3, 4),
    (3, 4), (3, 4), (3, 4), (3, 4), (3, 4), (128, 5), (128, 5), (128, 5), (128, 5), (128, 5),
    (128, 5), (128, 5), (128, 5), (8, 5), (8, 5), (8, 5), (8, 5), (8, 5), (8, 5), (8, 5),
    (8, 5), (9, 5), (9, 5), (9, 5), (9, 5), (9, 5), (9, 5), (9, 5), (9, 5), (16, 6), (16, 6),
    (16, 6), (16, 6), (17, 6), (17, 6), (17, 6), (17, 6), (4, 4), (4, 4), (4, 4), (4, 4),
    (4, 4), (4, 4), (4, 4), (4, 4), (4, 4), (4, 4), (4, 4), (4, 4), (4, 4), (4, 4), (4, 4),
    (4, 4), (5, 4), (5, 4), (5, 4), (5, 4), (5, 4), (5, 4), (5, 4), (5, 4), (5, 4), (5, 4),
    (5, 4), (5, 4), (5, 4), (5, 4), (5, 4), (5, 4), (14, 6), (14, 6), (14, 6), (14, 6),
    (15, 6), (15, 6), (15, 6), (15, 6), (64, 5), (64, 5), (64, 5), (64, 5), (64, 5), (64, 5),
    (64, 5), (64, 5), (6, 4), (6, 4), (6, 4), (6, 4), (6, 4), (6, 4), (6, 4), (6, 4), (6, 4),
    (6, 4), (6, 4), (6, 4), (6, 4), (6, 4), (6, 4), (6, 4), (7, 4), (7, 4), (7, 4), (7, 4),
    (7, 4), (7, 4), (7, 4), (7, 4), (7, 4), (7, 4), (7, 4), (7, 4), (7, 4), (7, 4), (7, 4),
    (7, 4), (-2, 3), (-2, 3), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0),
    (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-3, 4), (1792, 3), (1792, 3),
    (1984, 4), (2048, 4), (2112, 4), (2176, 4), (2240, 4), (2304, 4), (1856, 3), (1856, 3),
    (1920, 3), (1920, 3), (2368, 4), (2432, 4), (2496, 4), (2560, 4), (1472, 1), (1536, 1),
    (1600, 1), (1728, 1), (704, 1), (768, 1), (832, 1), (896, 1), (960, 1), (1024, 1),
    (1088, 1), (1152, 1), (1216, 1), (1280, 1), (1344, 1), (1408, 1),
];

#[rustfmt::skip]
const BLACK: [(i16, i16); 320] = [
    (128, 12), (160, 13), (224, 12), (256, 12), (10, 7), (11, 7), (288, 12), (12, 7), (9, 6),
    (9, 6), (8, 6), (8, 6), (7, 5), (7, 5), (7, 5), (7, 5), (6, 4), (6, 4), (6, 4), (6, 4),
    (6, 4), (6, 4), (6, 4), (6, 4), (5, 4), (5, 4), (5, 4), (5, 4), (5, 4), (5, 4), (5, 4),
    (5, 4), (1, 3), (1, 3), (1, 3), (1, 3), (1, 3), (1, 3), (1, 3), (1, 3), (1, 3), (1, 3),
    (1, 3), (1, 3), (1, 3), (1, 3), (1, 3), (1, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3),
    (4, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3),
    (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2),
    (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2),
    (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (3, 2), (2, 2),
    (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2),
    (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2),
    (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (2, 2), (-2, 4), (-2, 4),
    (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0),
    (-1, 0), (-1, 0), (-1, 0), (-3, 5), (1792, 4), (1792, 4), (1984, 5), (2048, 5), (2112, 5),
    (2176, 5), (2240, 5), (2304, 5), (1856, 4), (1856, 4), (1920, 4), (1920, 4), (2368, 5),
    (2432, 5), (2496, 5), (2560, 5), (18, 3), (18, 3), (18, 3), (18, 3), (18, 3), (18, 3),
    (18, 3), (18, 3), (52, 5), (52, 5), (640, 6), (704, 6), (768, 6), (832, 6), (55, 5),
    (55, 5), (56, 5), (56, 5), (1280, 6), (1344, 6), (1408, 6), (1472, 6), (59, 5), (59, 5),
    (60, 5), (60, 5), (1536, 6), (1600, 6), (24, 4), (24, 4), (24, 4), (24, 4), (25, 4),
    (25, 4), (25, 4), (25, 4), (1664, 6), (1728, 6), (320, 5), (320, 5), (384, 5), (384, 5),
    (448, 5), (448, 5), (512, 6), (576, 6), (53, 5), (53, 5), (54, 5), (54, 5), (896, 6),
    (960, 6), (1024, 6), (1088, 6), (1152, 6), (1216, 6), (64, 3), (64, 3), (64, 3), (64, 3),
    (64, 3), (64, 3), (64, 3), (64, 3), (13, 1), (13, 1), (13, 1), (13, 1), (13, 1), (13, 1),
    (13, 1), (13, 1), (13, 1), (13, 1), (13, 1), (13, 1), (13, 1), (13, 1), (13, 1), (13, 1),
    (23, 4), (23, 4), (50, 5), (51, 5), (44, 5), (45, 5), (46, 5), (47, 5), (57, 5), (58, 5),
    (61, 5), (256, 5), (16, 3), (16, 3), (16, 3), (16, 3), (17, 3), (17, 3), (17, 3), (17, 3),
    (48, 5), (49, 5), (62, 5), (63, 5), (30, 5), (31, 5), (32, 5), (33, 5), (40, 5), (41, 5),
    (22, 4), (22, 4), (14, 1), (14, 1), (14, 1), (14, 1), (14, 1), (14, 1), (14, 1), (14, 1),
    (14, 1), (14, 1), (14, 1), (14, 1), (14, 1), (14, 1), (14, 1), (14, 1), (15, 2), (15, 2),
    (15, 2), (15, 2), (15, 2), (15, 2), (15, 2), (15, 2), (128, 5), (192, 5), (26, 5), (27, 5),
    (28, 5), (29, 5), (19, 4), (19, 4), (20, 4), (20, 4), (34, 5), (35, 5), (36, 5), (37, 5),
    (38, 5), (39, 5), (21, 4), (21, 4), (42, 5), (43, 5), (0, 3), (0, 3), (0, 3), (0, 3),
];

#[rustfmt::skip]
const TWO_D: [(i16, i16); 152] = [
    (128, 11), (144, 10), (6, 7), (0, 7), (5, 6), (5, 6), (1, 6), (1, 6), (-4, 4), (-4, 4),
    (-4, 4), (-4, 4), (-4, 4), (-4, 4), (-4, 4), (-4, 4), (-5, 3), (-5, 3), (-5, 3), (-5, 3),
    (-5, 3), (-5, 3), (-5, 3), (-5, 3), (-5, 3), (-5, 3), (-5, 3), (-5, 3), (-5, 3), (-5, 3),
    (-5, 3), (-5, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3),
    (4, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3), (4, 3), (2, 3), (2, 3), (2, 3), (2, 3),
    (2, 3), (2, 3), (2, 3), (2, 3), (2, 3), (2, 3), (2, 3), (2, 3), (2, 3), (2, 3), (2, 3),
    (2, 3), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1),
    (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1),
    (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1),
    (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1),
    (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1),
    (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (3, 1), (-2, 4),
    (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0),
    (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0), (-1, 0),
    (-1, 0), (-1, 0), (-3, 3),
];

#[rustfmt::skip]
const CLZ: [u8; 256] = [
    8, 7, 6, 6, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4,
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn params(k: i32, columns: i32) -> FaxParams {
        FaxParams {
            k,
            end_of_line: false,
            encoded_byte_align: false,
            columns,
            rows: 0,
            end_of_block: true,
            black_is_1: false,
        }
    }

    fn decode(data: &[u8], p: FaxParams) -> Vec<u8> {
        let diag = Diagnostics::new();
        let mut s = open(Stream::memory(data, &diag), p).unwrap();
        s.read_best(0, 0, usize::MAX).unwrap()
    }

    #[test]
    fn group4_all_white_rows() {
        // Two rows of 8 white pixels in G4: V0, V0 (one bit each), then EOFB.
        // Each V0 code is "1"; white pixels come out as 1 bits.
        let rows = decode(
            &[0b1100_0000, 0x00, 0x10, 0x01, 0x00, 0x10, 0x01],
            params(-1, 8),
        );
        assert_eq!(&rows[..2], &[0xff, 0xff]);
    }

    #[test]
    fn changing_elements() {
        let line = [0b0011_1100u8, 0xff];
        assert_eq!(find_changing(&line, -1, 16), 2);
        assert_eq!(find_changing(&line, 2, 16), 6);
        assert_eq!(find_changing(&line, 6, 16), 8);
        assert_eq!(find_changing_color(&line, 0, 16, true), 2);
        let mut row = [0u8; 2];
        set_bits(&mut row, 3, 13);
        assert_eq!(row, [0b0001_1111, 0b1111_1000]);
    }

    macro_rules! fixture {
        ($name:literal) => {
            include_bytes!(concat!("../tests/data/fax/", $name, ".fax")).as_slice()
        };
    }

    fn fax_cases() -> Vec<crate::filter::StreamCase> {
        let g3 = fixture!("g3_1d");
        let g4 = fixture!("g4");
        let ccf = |p: &str| format!("/Filter /CCITTFaxDecode /DecodeParms << /Columns 45 {p} >>");
        let mut corrupt = g4.to_vec();
        corrupt[20] ^= 0xff;
        let mut corrupt_1d = g3.to_vec();
        corrupt_1d[10] = 0;
        corrupt_1d[11] = 0;
        vec![
            (
                "g3_1d",
                ccf("/K 0 /EndOfLine true /Rows 12"),
                g3.to_vec(),
                None,
            ),
            ("g3_1d_no_eol_flag", ccf("/K 0"), g3.to_vec(), None),
            (
                "g3_1d_black_is_1",
                ccf("/K 0 /BlackIs1 true"),
                g3.to_vec(),
                None,
            ),
            (
                "g3_1d_rows_5",
                ccf("/K 0 /EndOfLine true /Rows 5 /EndOfBlock false"),
                g3.to_vec(),
                None,
            ),
            (
                "g3_1d_corrupt",
                ccf("/K 0 /EndOfLine true"),
                corrupt_1d,
                None,
            ),
            (
                "g3_1d_late_eol",
                ccf("/K 0 /EndOfLine true"),
                [&[0xaa, 0x55][..], g3].concat(),
                None,
            ),
            (
                "g3_1d_no_eol",
                ccf("/K 0 /EndOfLine true"),
                vec![0xff; 40],
                None,
            ),
            (
                "g3_1d_aligned",
                ccf("/K 0 /EndOfLine true /EncodedByteAlign true"),
                fixture!("g3_1d_aligned").to_vec(),
                None,
            ),
            (
                "g3_2d",
                ccf("/K 1 /EndOfLine true"),
                fixture!("g3_2d").to_vec(),
                None,
            ),
            (
                "g3_2d_aligned",
                ccf("/K 1 /EndOfLine true /EncodedByteAlign true"),
                fixture!("g3_2d_aligned").to_vec(),
                None,
            ),
            ("g3_2d_k_4", ccf("/K 4"), fixture!("g3_2d").to_vec(), None),
            ("g4", ccf("/K -1 /Rows 12"), g4.to_vec(), None),
            (
                "g4_no_eob",
                ccf("/K -1 /EndOfBlock false /Rows 12"),
                g4.to_vec(),
                None,
            ),
            (
                "g4_rows_3",
                ccf("/K -1 /Rows 3 /EndOfBlock false"),
                g4.to_vec(),
                None,
            ),
            (
                "g4_aligned",
                ccf("/K -1 /EncodedByteAlign true"),
                g4.to_vec(),
                None,
            ),
            ("g4_corrupt", ccf("/K -1"), corrupt, None),
            ("g4_truncated", ccf("/K -1"), g4[..30].to_vec(), None),
            ("g4_wrong_width", s_ccf(40), g4.to_vec(), None),
            // The extension code for uncompressed mode, in 2-D and 1-D.
            (
                "g4_uncompressed",
                ccf("/K -1"),
                vec![0x03, 0xc0, 0, 0],
                None,
            ),
            (
                "g3_uncompressed",
                ccf("/K 0"),
                vec![0x00, 0x0f, 0x00, 0, 0],
                None,
            ),
            (
                "g4_invalid",
                ccf("/K -1"),
                vec![0x00, 0x01, 0xff, 0xff],
                None,
            ),
            (
                "columns_overflow",
                "/Filter /CCITTFaxDecode /DecodeParms << /Columns 2147483647 >>".to_string(),
                g4.to_vec(),
                None,
            ),
            (
                "default_columns",
                "/Filter /CCF".to_string(),
                g4.to_vec(),
                None,
            ),
        ]
    }

    fn s_ccf(columns: i32) -> String {
        format!("/Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns {columns} >>")
    }

    /// MuPDF 1.28.6's result for each case, from tests/data/oracle.py.
    #[rustfmt::skip]
    const MUPDF_FAX: &[(&str, usize, &str, &[&str])] = &[
        ("columns_overflow", 0, "e3b0c44298fc1c14", &["error too many columns integer overflow (2147483647)"]),
        ("default_columns", 216, "9ce31e87e7f0949e", &["format error: overflow in 1d faxd"]),
        ("g3_1d", 72, "9585bd5380175186", &[]),
        ("g3_1d_aligned", 72, "9585bd5380175186", &["faxd stream doesn't start with EOL"]),
        ("g3_1d_black_is_1", 72, "18c51331fafb5992", &[]),
        ("g3_1d_corrupt", 12, "c5c273927cdfa4a8", &["format error: overflow in 1d faxd"]),
        ("g3_1d_late_eol", 72, "9585bd5380175186", &["faxd stream doesn't start with EOL"]),
        ("g3_1d_no_eol", 0, "e3b0c44298fc1c14", &["faxd stream doesn't start with EOL", "format error: initial EOL not found", "read error; treating as end of file"]),
        ("g3_1d_no_eol_flag", 72, "9585bd5380175186", &[]),
        ("g3_1d_rows_5", 30, "f582aef27f051f82", &[]),
        ("g3_2d", 72, "9585bd5380175186", &[]),
        ("g3_2d_aligned", 72, "9585bd5380175186", &["faxd stream doesn't start with EOL"]),
        ("g3_2d_k_4", 72, "9585bd5380175186", &[]),
        ("g3_uncompressed", 6, "ce8bee525d6736e9", &[]),
        ("g4", 72, "9585bd5380175186", &[]),
        ("g4_aligned", 24, "707fad5bc6295f3b", &["format error: overflow in 2d faxd"]),
        ("g4_corrupt", 18, "05de5a409cb4564e", &["format error: overflow in 2d faxd"]),
        ("g4_invalid", 96, "ce65609cc564ce3d", &[]),
        ("g4_no_eob", 72, "9585bd5380175186", &[]),
        ("g4_rows_3", 18, "bbac9f0e8005b6ad", &[]),
        ("g4_truncated", 24, "a2e69f985aea1000", &[]),
        ("g4_uncompressed", 6, "ce8bee525d6736e9", &["format error: uncompressed data in faxd"]),
        ("g4_wrong_width", 15, "2ca0ebcbda972e1d", &["format error: overflow in 2d faxd"]),
    ];

    #[test]
    fn fax_streams_decode_as_in_mupdf() {
        crate::filter::check_against_mupdf("NUPDF_DUMP_FAX_CASES", &fax_cases(), MUPDF_FAX);
    }
}
