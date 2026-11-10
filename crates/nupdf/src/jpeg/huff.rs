// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from libjpeg 10 jdhuff.c and jcomapi.c (jpeg_std_huff_table), as
// bundled with MuPDF 1.28.6, Copyright (C) 1991-1997, Thomas G. Lane.
// Modified 2006-2026 by Guido Vollbeding. This software is based in part on
// the work of the Independent JPEG Group.

//! Huffman entropy decoding, sequential and progressive. Past the end of
//! the data (a marker where entropy bytes should be) the bit buffer is
//! padded with zeros, and once that has happened the remaining MCUs of the
//! scan are left empty, as libjpeg does.

use super::{Block, Error, HuffTable, Info, NATURAL_ORDER, R, marker::Reader};

const MIN_GET_BITS: i32 = 25;

const BMASK: [i32; 16] = [
    0, 0x0001, 0x0003, 0x0007, 0x000f, 0x001f, 0x003f, 0x007f, 0x00ff, 0x01ff, 0x03ff, 0x07ff,
    0x0fff, 0x1fff, 0x3fff, 0x7fff,
];

/// `HUFF_EXTEND`.
fn extend(x: i32, s: i32) -> i32 {
    if x <= BMASK[(s - 1) as usize & 15] {
        x - BMASK[s as usize & 15]
    } else {
        x
    }
}

/// `d_derived_tbl`.
struct Derived {
    maxcode: [i64; 18],
    valoffset: [i64; 17],
    look_nbits: [i32; 256],
    look_sym: [u8; 256],
    huffval: [u8; 256],
}

/// Standard tables for streams that omit them (Motion-JPEG); only 0 and 1.
fn std_table(dc: bool, tblno: usize) -> Option<HuffTable> {
    const BITS_DC_LUM: [u8; 17] = [0, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
    const BITS_DC_CHROM: [u8; 17] = [0, 0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
    const VAL_DC: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    const BITS_AC_LUM: [u8; 17] = [0, 0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
    const VAL_AC_LUM: [u8; 162] = [
        0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61,
        0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52,
        0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25,
        0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45,
        0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
        0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83,
        0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
        0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6,
        0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3,
        0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8,
        0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ];
    const BITS_AC_CHROM: [u8; 17] = [0, 0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
    const VAL_AC_CHROM: [u8; 162] = [
        0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61,
        0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33,
        0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18,
        0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44,
        0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63,
        0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a,
        0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
        0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4,
        0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
        0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7,
        0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ];
    let (bits, val): (&[u8; 17], &[u8]) = match (dc, tblno) {
        (true, 0) => (&BITS_DC_LUM, &VAL_DC),
        (true, 1) => (&BITS_DC_CHROM, &VAL_DC),
        (false, 0) => (&BITS_AC_LUM, &VAL_AC_LUM),
        (false, 1) => (&BITS_AC_CHROM, &VAL_AC_CHROM),
        _ => return None,
    };
    let mut t = HuffTable {
        bits: *bits,
        ..HuffTable::default()
    };
    t.huffval[..val.len()].copy_from_slice(val);
    Some(t)
}

/// `jpeg_make_d_derived_tbl`.
fn derive(info: &mut Info<'_>, dc: bool, tblno: usize) -> R<Box<Derived>> {
    let undefined = || Error::jpeg(format!("Huffman table 0x{tblno:02x} was not defined"));
    if tblno >= 4 {
        return Err(undefined());
    }
    let slot = if dc {
        &mut info.dc_huff[tblno]
    } else {
        &mut info.ac_huff[tblno]
    };
    if slot.is_none() {
        *slot = Some(std_table(dc, tblno).ok_or_else(undefined)?);
    }
    let Some(htbl) = slot.as_ref() else {
        return Err(undefined());
    };
    let bad = || Error::jpeg("Bogus Huffman table definition");

    let mut huffsize = [0u8; 257];
    let mut p = 0;
    for l in 1..=16u8 {
        let i = usize::from(htbl.bits[usize::from(l)]);
        if p + i > 256 {
            return Err(bad());
        }
        huffsize[p..p + i].fill(l);
        p += i;
    }
    huffsize[p] = 0;
    let numsymbols = p;

    let mut huffcode = [0u32; 257];
    let mut code: u32 = 0;
    let mut si = u32::from(huffsize[0]);
    p = 0;
    while huffsize[p] != 0 {
        while u32::from(huffsize[p]) == si {
            huffcode[p] = code;
            p += 1;
            code += 1;
        }
        if i64::from(code) >= 1i64 << si {
            return Err(bad());
        }
        code <<= 1;
        si += 1;
    }

    let mut d = Box::new(Derived {
        maxcode: [0; 18],
        valoffset: [0; 17],
        look_nbits: [0; 256],
        look_sym: [0; 256],
        huffval: htbl.huffval,
    });
    p = 0;
    for l in 1..=16 {
        let n = usize::from(htbl.bits[l]);
        if n != 0 {
            d.valoffset[l] = p as i64 - i64::from(huffcode[p]);
            p += n;
            d.maxcode[l] = i64::from(huffcode[p - 1]);
        } else {
            d.maxcode[l] = -1;
        }
    }
    d.maxcode[17] = 0xfffff;

    p = 0;
    for l in 1..=8usize {
        for _ in 0..htbl.bits[l] {
            let first = (huffcode[p] as usize) << (8 - l);
            for look in (first..).take(1 << (8 - l)) {
                if let Some(n) = d.look_nbits.get_mut(look) {
                    *n = l as i32;
                    d.look_sym[look] = htbl.huffval[p];
                }
            }
            p += 1;
        }
    }

    if dc && htbl.huffval[..numsymbols].iter().any(|&s| s > 15) {
        return Err(bad());
    }
    Ok(d)
}

/// The bit reader (`bitread_perm_state` plus `insufficient_data`).
#[derive(Default)]
struct Bits {
    buf: u64,
    left: i32,
    insufficient: bool,
}

impl Bits {
    /// `jpeg_fill_bit_buffer`.
    fn fill(&mut self, rd: &mut Reader<'_>, nbits: i32) {
        if rd.unread_marker == 0 {
            while self.left < MIN_GET_BITS {
                let mut c = rd.byte();
                if c == 0xff {
                    loop {
                        c = rd.byte();
                        if c != 0xff {
                            break;
                        }
                    }
                    if c == 0 {
                        c = 0xff;
                    } else {
                        rd.unread_marker = c;
                        break;
                    }
                }
                self.buf = (self.buf << 8) | c as u64;
                self.left += 8;
            }
            if rd.unread_marker == 0 {
                return;
            }
        }
        if nbits > self.left {
            self.insufficient = true;
            self.buf <<= MIN_GET_BITS - self.left;
            self.left = MIN_GET_BITS;
        }
    }

    /// `CHECK_BIT_BUFFER`.
    fn check(&mut self, rd: &mut Reader<'_>, nbits: i32) {
        if self.left < nbits {
            self.fill(rd, nbits);
        }
    }

    /// `GET_BITS`.
    fn get(&mut self, nbits: i32) -> i32 {
        self.left -= nbits;
        (self.buf >> self.left) as i32 & BMASK[nbits as usize & 15]
    }

    /// `CHECK_BIT_BUFFER` then `GET_BITS`.
    fn take(&mut self, rd: &mut Reader<'_>, nbits: i32) -> i32 {
        self.check(rd, nbits);
        self.get(nbits)
    }

    /// `HUFF_DECODE`.
    fn decode(&mut self, rd: &mut Reader<'_>, tbl: &Derived) -> i32 {
        if self.left < 8 {
            self.fill(rd, 0);
            if self.left < 8 {
                return self.slow(rd, tbl, 1);
            }
        }
        let look = ((self.buf >> (self.left - 8)) & 0xff) as usize;
        let nb = tbl.look_nbits[look];
        if nb != 0 {
            self.left -= nb;
            return i32::from(tbl.look_sym[look]);
        }
        self.slow(rd, tbl, 9)
    }

    /// `jpeg_huff_decode`.
    fn slow(&mut self, rd: &mut Reader<'_>, tbl: &Derived, min_bits: usize) -> i32 {
        let mut l = min_bits;
        let mut code = i64::from(self.take(rd, l as i32));
        while l < 17 && code > tbl.maxcode[l] {
            code <<= 1;
            code |= i64::from(self.take(rd, 1));
            l += 1;
        }
        if l > 16 {
            // A bad code: libjpeg warns and decodes it as zero.
            return 0;
        }
        let i = code + tbl.valoffset[l];
        usize::try_from(i)
            .ok()
            .and_then(|i| tbl.huffval.get(i))
            .map_or(0, |&v| i32::from(v))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Sequential,
    DcFirst,
    AcFirst,
    DcRefine,
    AcRefine,
}

/// `huff_entropy_decoder`.
pub(crate) struct Huff {
    bits: Bits,
    eobrun: u32,
    last_dc: [i32; 4],
    restarts_to_go: u32,
    mode: Mode,
    /// Progressive tables, by number.
    derived: [Option<Box<Derived>>; 4],
    ac_tbl: usize,
    /// Sequential tables, by number.
    dc_derived: [Option<Box<Derived>>; 4],
    ac_derived: [Option<Box<Derived>>; 4],
    dc_cur: [usize; 10],
    ac_cur: [usize; 10],
    coef_limit: [usize; 10],
}

/// Zigzag positions, for the highest coefficient a scaled IDCT reads.
const ZIGZAG: [[usize; 8]; 8] = [
    [0, 1, 5, 6, 14, 15, 27, 28],
    [2, 4, 7, 13, 16, 26, 29, 42],
    [3, 8, 12, 17, 25, 30, 41, 43],
    [9, 11, 18, 24, 31, 40, 44, 53],
    [10, 19, 23, 32, 39, 45, 52, 54],
    [20, 22, 33, 38, 46, 51, 55, 60],
    [21, 34, 37, 47, 50, 56, 59, 61],
    [35, 36, 48, 49, 57, 58, 62, 63],
];

impl Huff {
    pub fn new() -> Huff {
        Huff {
            bits: Bits::default(),
            eobrun: 0,
            last_dc: [0; 4],
            restarts_to_go: 0,
            mode: Mode::Sequential,
            derived: Default::default(),
            ac_tbl: 0,
            dc_derived: Default::default(),
            ac_derived: Default::default(),
            dc_cur: [0; 10],
            ac_cur: [0; 10],
            coef_limit: [0; 10],
        }
    }

    /// `start_pass_huff_decoder`.
    pub fn start_pass(&mut self, info: &mut Info<'_>) -> R<()> {
        if info.progressive {
            super::check_progression(info)?;
            self.mode = match (info.ah == 0, info.ss == 0) {
                (true, true) => Mode::DcFirst,
                (true, false) => Mode::AcFirst,
                (false, true) => Mode::DcRefine,
                (false, false) => Mode::AcRefine,
            };
            for i in 0..info.comps_in_scan {
                let c = info.cur_comp[i];
                if info.ss == 0 {
                    if info.ah == 0 {
                        let tbl = info.comps[c].dc_tbl_no;
                        let d = derive(info, true, tbl)?;
                        self.derived[tbl] = Some(d);
                    }
                } else {
                    let tbl = info.comps[c].ac_tbl_no;
                    let d = derive(info, false, tbl)?;
                    self.derived[tbl] = Some(d);
                    self.ac_tbl = tbl;
                }
                self.last_dc[i] = 0;
            }
            self.eobrun = 0;
        } else {
            self.mode = Mode::Sequential;
            for i in 0..info.comps_in_scan {
                let c = info.cur_comp[i];
                let tbl = info.comps[c].dc_tbl_no;
                let d = derive(info, true, tbl)?;
                self.dc_derived[tbl] = Some(d);
                let tbl = info.comps[c].ac_tbl_no;
                let d = derive(info, false, tbl)?;
                self.ac_derived[tbl] = Some(d);
                self.last_dc[i] = 0;
            }
            for b in 0..info.blocks_in_mcu {
                let comp = &info.comps[info.cur_comp[info.mcu_membership[b]]];
                self.dc_cur[b] = comp.dc_tbl_no;
                self.ac_cur[b] = comp.ac_tbl_no;
                self.coef_limit[b] = if comp.needed {
                    let size = |s: usize| if s == 0 || s > 8 { 8 } else { s };
                    1 + ZIGZAG[size(comp.dct_v) - 1][size(comp.dct_h) - 1]
                } else {
                    0
                };
            }
        }
        self.bits = Bits::default();
        self.restarts_to_go = info.restart_interval;
        Ok(())
    }

    /// `process_restart`.
    fn restart(&mut self, info: &mut Info<'_>) {
        self.bits.left = 0;
        info.rd.read_restart_marker();
        self.last_dc = [0; 4];
        self.eobrun = 0;
        self.restarts_to_go = info.restart_interval;
        if info.rd.unread_marker == 0 {
            self.bits.insufficient = false;
        }
    }

    /// Decodes one MCU into `blocks`.
    pub fn decode_mcu(&mut self, info: &mut Info<'_>, blocks: &mut [Block]) {
        if info.restart_interval != 0 && self.restarts_to_go == 0 {
            self.restart(info);
        }
        match self.mode {
            Mode::Sequential => self.sequential(info, blocks),
            Mode::DcFirst => self.dc_first(info, blocks),
            Mode::AcFirst => self.ac_first(info, blocks),
            Mode::DcRefine => self.dc_refine(info, blocks),
            Mode::AcRefine => self.ac_refine(info, blocks),
        }
        if info.restart_interval != 0 {
            self.restarts_to_go = self.restarts_to_go.wrapping_sub(1);
        }
    }

    /// `decode_mcu`.
    fn sequential(&mut self, info: &mut Info<'_>, blocks: &mut [Block]) {
        if self.bits.insufficient {
            return;
        }
        let rd = &mut info.rd;
        for (b, block) in blocks.iter_mut().enumerate().take(info.blocks_in_mcu) {
            let (Some(dc), Some(ac)) = (
                self.dc_derived[self.dc_cur[b]].as_deref(),
                self.ac_derived[self.ac_cur[b]].as_deref(),
            ) else {
                return;
            };
            let mut s = self.bits.decode(rd, dc);
            let mut k = 1;
            let coef_limit = self.coef_limit[b];
            let mut done = false;
            if coef_limit != 0 {
                if s != 0 {
                    let r = self.bits.take(rd, s);
                    s = extend(r, s);
                }
                let ci = info.mcu_membership[b];
                s = s.wrapping_add(self.last_dc[ci]);
                self.last_dc[ci] = s;
                block[0] = s as i16;
                while k < coef_limit {
                    let sym = self.bits.decode(rd, ac);
                    let r = (sym >> 4) as usize;
                    let s = sym & 15;
                    if s != 0 {
                        k += r;
                        let v = self.bits.take(rd, s);
                        block[NATURAL_ORDER[k]] = extend(v, s) as i16;
                    } else {
                        if r != 15 {
                            done = true;
                            break;
                        }
                        k += 15;
                    }
                    k += 1;
                }
            } else if s != 0 {
                self.bits.check(rd, s);
                self.bits.left -= s;
            }
            if done {
                continue;
            }
            while k < 64 {
                let sym = self.bits.decode(rd, ac);
                let r = (sym >> 4) as usize;
                let s = sym & 15;
                if s != 0 {
                    k += r;
                    self.bits.check(rd, s);
                    self.bits.left -= s;
                } else {
                    if r != 15 {
                        break;
                    }
                    k += 15;
                }
                k += 1;
            }
        }
    }

    /// `decode_mcu_DC_first`.
    fn dc_first(&mut self, info: &mut Info<'_>, blocks: &mut [Block]) {
        if self.bits.insufficient {
            return;
        }
        let rd = &mut info.rd;
        for (b, block) in blocks.iter_mut().enumerate().take(info.blocks_in_mcu) {
            let ci = info.mcu_membership[b];
            let tbl = info.comps[info.cur_comp[ci]].dc_tbl_no;
            let Some(t) = self.derived[tbl].as_deref() else {
                return;
            };
            let mut s = self.bits.decode(rd, t);
            if s != 0 {
                let r = self.bits.take(rd, s);
                s = extend(r, s);
            }
            s = s.wrapping_add(self.last_dc[ci]);
            self.last_dc[ci] = s;
            block[0] = s.wrapping_shl(info.al as u32) as i16;
        }
    }

    /// `decode_mcu_AC_first`.
    fn ac_first(&mut self, info: &mut Info<'_>, blocks: &mut [Block]) {
        if self.bits.insufficient {
            return;
        }
        if self.eobrun != 0 {
            self.eobrun -= 1;
            return;
        }
        let Some(t) = self.derived[self.ac_tbl].as_deref() else {
            return;
        };
        let Some(block) = blocks.first_mut() else {
            return;
        };
        let rd = &mut info.rd;
        let (se, al) = (info.se as usize, info.al as u32);
        let mut k = info.ss as usize;
        while k <= se {
            let sym = self.bits.decode(rd, t);
            let r = sym >> 4;
            let s = sym & 15;
            if s != 0 {
                k += r as usize;
                let v = self.bits.take(rd, s);
                block[NATURAL_ORDER[k]] = extend(v, s).wrapping_shl(al) as i16;
            } else {
                if r != 15 {
                    if r != 0 {
                        let mut run = 1u32 << r;
                        run += self.bits.take(rd, r) as u32;
                        self.eobrun = run - 1;
                    }
                    break;
                }
                k += 15;
            }
            k += 1;
        }
    }

    /// `decode_mcu_DC_refine`.
    fn dc_refine(&mut self, info: &mut Info<'_>, blocks: &mut [Block]) {
        // No `insufficient_data` check here, as in libjpeg.
        let rd = &mut info.rd;
        let p1 = 1i16.wrapping_shl(info.al as u32);
        for block in blocks.iter_mut().take(info.blocks_in_mcu) {
            if self.bits.take(rd, 1) != 0 {
                block[0] |= p1;
            }
        }
    }

    /// `decode_mcu_AC_refine`.
    fn ac_refine(&mut self, info: &mut Info<'_>, blocks: &mut [Block]) {
        if self.bits.insufficient {
            return;
        }
        let Some(t) = self.derived[self.ac_tbl].as_deref() else {
            return;
        };
        let Some(block) = blocks.first_mut() else {
            return;
        };
        let rd = &mut info.rd;
        let se = info.se as usize;
        let p1 = 1i16.wrapping_shl(info.al as u32);
        let m1 = p1.wrapping_neg();
        let mut eobrun = self.eobrun;
        let mut k = info.ss as usize;
        let correct = |bits: &mut Bits, rd: &mut Reader<'_>, coef: &mut i16| {
            if bits.take(rd, 1) != 0 && *coef & p1 == 0 {
                *coef = if *coef >= 0 {
                    coef.wrapping_add(p1)
                } else {
                    coef.wrapping_add(m1)
                };
            }
        };
        if eobrun == 0 {
            while k <= se {
                let sym = self.bits.decode(rd, t);
                let mut r = sym >> 4;
                let mut s = sym & 15;
                if s != 0 {
                    // s != 1 would be a (swallowed) warning.
                    s = if self.bits.take(rd, 1) != 0 {
                        i32::from(p1)
                    } else {
                        i32::from(m1)
                    };
                } else if r != 15 {
                    eobrun = 1 << r;
                    if r != 0 {
                        eobrun += self.bits.take(rd, r) as u32;
                    }
                    break;
                }
                // Advance over already-nonzero coefficients, correcting
                // them, and r zero ones.
                loop {
                    let coef = &mut block[NATURAL_ORDER[k]];
                    if *coef != 0 {
                        correct(&mut self.bits, rd, coef);
                    } else {
                        r -= 1;
                        if r < 0 {
                            break;
                        }
                    }
                    k += 1;
                    if k > se {
                        break;
                    }
                }
                if s != 0 {
                    block[NATURAL_ORDER[k]] = s as i16;
                }
                k += 1;
            }
        }
        if eobrun != 0 {
            while k <= se {
                let coef = &mut block[NATURAL_ORDER[k]];
                if *coef != 0 {
                    correct(&mut self.bits, rd, coef);
                }
                k += 1;
            }
            eobrun -= 1;
        }
        self.eobrun = eobrun;
    }
}
