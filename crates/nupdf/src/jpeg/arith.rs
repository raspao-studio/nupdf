// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from libjpeg 10 jdarith.c and jaricom.c, as bundled with MuPDF
// 1.28.6, Developed 1997-2026 by Guido Vollbeding. This software is based
// in part on the work of the Independent JPEG Group.

//! Arithmetic entropy decoding (SOF9 and SOF10). A bad code stops the
//! scan: the remaining MCUs are left as they are, as in libjpeg.

use super::{Block, Error, Info, NATURAL_ORDER, R, marker::Reader};

/// `jpeg_aritab`: Qe, Next_Index_LPS, Next_Index_MPS and Switch_MPS, packed
/// as `V(i, a, b, c, d) = a << 16 | c << 8 | d << 7 | b`.
const ARITAB: [i64; 114] = {
    const T: [(i64, i64, i64, i64); 114] = [
        (0x5a1d, 1, 1, 1),
        (0x2586, 14, 2, 0),
        (0x1114, 16, 3, 0),
        (0x080b, 18, 4, 0),
        (0x03d8, 20, 5, 0),
        (0x01da, 23, 6, 0),
        (0x00e5, 25, 7, 0),
        (0x006f, 28, 8, 0),
        (0x0036, 30, 9, 0),
        (0x001a, 33, 10, 0),
        (0x000d, 35, 11, 0),
        (0x0006, 9, 12, 0),
        (0x0003, 10, 13, 0),
        (0x0001, 12, 13, 0),
        (0x5a7f, 15, 15, 1),
        (0x3f25, 36, 16, 0),
        (0x2cf2, 38, 17, 0),
        (0x207c, 39, 18, 0),
        (0x17b9, 40, 19, 0),
        (0x1182, 42, 20, 0),
        (0x0cef, 43, 21, 0),
        (0x09a1, 45, 22, 0),
        (0x072f, 46, 23, 0),
        (0x055c, 48, 24, 0),
        (0x0406, 49, 25, 0),
        (0x0303, 51, 26, 0),
        (0x0240, 52, 27, 0),
        (0x01b1, 54, 28, 0),
        (0x0144, 56, 29, 0),
        (0x00f5, 57, 30, 0),
        (0x00b7, 59, 31, 0),
        (0x008a, 60, 32, 0),
        (0x0068, 62, 33, 0),
        (0x004e, 63, 34, 0),
        (0x003b, 32, 35, 0),
        (0x002c, 33, 9, 0),
        (0x5ae1, 37, 37, 1),
        (0x484c, 64, 38, 0),
        (0x3a0d, 65, 39, 0),
        (0x2ef1, 67, 40, 0),
        (0x261f, 68, 41, 0),
        (0x1f33, 69, 42, 0),
        (0x19a8, 70, 43, 0),
        (0x1518, 72, 44, 0),
        (0x1177, 73, 45, 0),
        (0x0e74, 74, 46, 0),
        (0x0bfb, 75, 47, 0),
        (0x09f8, 77, 48, 0),
        (0x0861, 78, 49, 0),
        (0x0706, 79, 50, 0),
        (0x05cd, 48, 51, 0),
        (0x04de, 50, 52, 0),
        (0x040f, 50, 53, 0),
        (0x0363, 51, 54, 0),
        (0x02d4, 52, 55, 0),
        (0x025c, 53, 56, 0),
        (0x01f8, 54, 57, 0),
        (0x01a4, 55, 58, 0),
        (0x0160, 56, 59, 0),
        (0x0125, 57, 60, 0),
        (0x00f6, 58, 61, 0),
        (0x00cb, 59, 62, 0),
        (0x00ab, 61, 63, 0),
        (0x008f, 61, 32, 0),
        (0x5b12, 65, 65, 1),
        (0x4d04, 80, 66, 0),
        (0x412c, 81, 67, 0),
        (0x37d8, 82, 68, 0),
        (0x2fe8, 83, 69, 0),
        (0x293c, 84, 70, 0),
        (0x2379, 86, 71, 0),
        (0x1edf, 87, 72, 0),
        (0x1aa9, 87, 73, 0),
        (0x174e, 72, 74, 0),
        (0x1424, 72, 75, 0),
        (0x119c, 74, 76, 0),
        (0x0f6b, 74, 77, 0),
        (0x0d51, 75, 78, 0),
        (0x0bb6, 77, 79, 0),
        (0x0a40, 77, 48, 0),
        (0x5832, 80, 81, 1),
        (0x4d1c, 88, 82, 0),
        (0x438e, 89, 83, 0),
        (0x3bdd, 90, 84, 0),
        (0x34ee, 91, 85, 0),
        (0x2eae, 92, 86, 0),
        (0x299a, 93, 87, 0),
        (0x2516, 86, 71, 0),
        (0x5570, 88, 89, 1),
        (0x4ca9, 95, 90, 0),
        (0x44d9, 96, 91, 0),
        (0x3e22, 97, 92, 0),
        (0x3824, 99, 93, 0),
        (0x32b4, 99, 94, 0),
        (0x2e17, 93, 86, 0),
        (0x56a8, 95, 96, 1),
        (0x4f46, 101, 97, 0),
        (0x47e5, 102, 98, 0),
        (0x41cf, 103, 99, 0),
        (0x3c3d, 104, 100, 0),
        (0x375e, 99, 93, 0),
        (0x5231, 105, 102, 0),
        (0x4c0f, 106, 103, 0),
        (0x4639, 107, 104, 0),
        (0x415e, 103, 99, 0),
        (0x5627, 105, 106, 1),
        (0x50e7, 108, 107, 0),
        (0x4b85, 109, 103, 0),
        (0x5597, 110, 109, 0),
        (0x504f, 111, 107, 0),
        (0x5a10, 110, 111, 1),
        (0x5522, 112, 109, 0),
        (0x59eb, 112, 111, 1),
        (0x5a1d, 113, 113, 0),
    ];
    let mut out = [0i64; 114];
    let mut i = 0;
    while i < 114 {
        let (a, b, c, d) = T[i];
        out[i] = a << 16 | c << 8 | d << 7 | b;
        i += 1;
    }
    out
};

const DC_STAT_BINS: usize = 64;
const AC_STAT_BINS: usize = 256;

/// The arithmetic decoder's registers.
#[derive(Default)]
struct Coder {
    c: i64,
    a: i64,
    ct: i32,
}

impl Coder {
    /// `arith_decode`.
    fn decode(&mut self, rd: &mut Reader<'_>, st: &mut u8) -> i32 {
        while self.a < 0x8000 {
            self.ct -= 1;
            if self.ct < 0 {
                let data = if rd.unread_marker != 0 {
                    0
                } else {
                    let mut data = rd.byte();
                    if data == 0xff {
                        loop {
                            data = rd.byte();
                            if data != 0xff {
                                break;
                            }
                        }
                        if data == 0 {
                            data = 0xff;
                        } else {
                            rd.unread_marker = data;
                            data = 0;
                        }
                    }
                    data
                };
                self.c = (self.c << 8) | i64::from(data);
                self.ct += 8;
                if self.ct < 0 {
                    self.ct += 1;
                    if self.ct == 0 {
                        self.a = 0x8000;
                    }
                }
            }
            self.a <<= 1;
        }

        let sv = i32::from(*st);
        let qe = ARITAB[(sv & 0x7f) as usize];
        let nl = (qe & 0xff) as i32;
        let nm = (qe >> 8 & 0xff) as i32;
        let qe = qe >> 16;

        let mut sv = sv;
        let temp = self.a - qe;
        self.a = temp;
        let temp = temp << self.ct.max(0);
        if self.c >= temp {
            self.c -= temp;
            if self.a < qe {
                self.a = qe;
                *st = ((sv & 0x80) ^ nm) as u8;
            } else {
                self.a = qe;
                *st = ((sv & 0x80) ^ nl) as u8;
                sv ^= 0x80;
            }
        } else if self.a < 0x8000 {
            if self.a < qe {
                *st = ((sv & 0x80) ^ nl) as u8;
                sv ^= 0x80;
            } else {
                *st = ((sv & 0x80) ^ nm) as u8;
            }
        }
        sv >> 7
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

/// `arith_entropy_decoder`.
pub(crate) struct Arith {
    coder: Coder,
    last_dc: [i32; 4],
    dc_context: [usize; 4],
    restarts_to_go: u32,
    mode: Mode,
    dc_stats: [Option<Box<[u8; DC_STAT_BINS]>>; 16],
    ac_stats: [Option<Box<[u8; AC_STAT_BINS]>>; 16],
    fixed_bin: u8,
}

impl Arith {
    pub fn new() -> Arith {
        Arith {
            coder: Coder::default(),
            last_dc: [0; 4],
            dc_context: [0; 4],
            restarts_to_go: 0,
            mode: Mode::Sequential,
            dc_stats: Default::default(),
            ac_stats: Default::default(),
            fixed_bin: 113,
        }
    }

    /// Whether the scan's DC (or AC) statistics are in use.
    fn uses_dc(info: &Info<'_>) -> bool {
        !info.progressive || (info.ss == 0 && info.ah == 0)
    }

    fn uses_ac(info: &Info<'_>) -> bool {
        (!info.progressive && info.lim_se != 0) || (info.progressive && info.ss != 0)
    }

    /// `start_pass`.
    pub fn start_pass(&mut self, info: &mut Info<'_>) -> R<()> {
        if info.progressive {
            super::check_progression(info)?;
            self.mode = match (info.ah == 0, info.ss == 0) {
                (true, true) => Mode::DcFirst,
                (true, false) => Mode::AcFirst,
                (false, true) => Mode::DcRefine,
                (false, false) => Mode::AcRefine,
            };
        } else {
            self.mode = Mode::Sequential;
        }
        let undefined =
            |t: usize| Error::jpeg(format!("Arithmetic table 0x{t:02x} was not defined"));
        for i in 0..info.comps_in_scan {
            let comp = &info.comps[info.cur_comp[i]];
            if Self::uses_dc(info) {
                let t = comp.dc_tbl_no;
                let stats = self.dc_stats.get_mut(t).ok_or_else(|| undefined(t))?;
                **stats.get_or_insert_with(|| Box::new([0; DC_STAT_BINS])) = [0; DC_STAT_BINS];
                self.last_dc[i] = 0;
                self.dc_context[i] = 0;
            }
            if Self::uses_ac(info) {
                let t = comp.ac_tbl_no;
                let stats = self.ac_stats.get_mut(t).ok_or_else(|| undefined(t))?;
                **stats.get_or_insert_with(|| Box::new([0; AC_STAT_BINS])) = [0; AC_STAT_BINS];
            }
        }
        self.coder = Coder {
            c: 0,
            a: 0,
            ct: -16,
        };
        self.restarts_to_go = info.restart_interval;
        Ok(())
    }

    /// `process_restart`.
    fn restart(&mut self, info: &mut Info<'_>) {
        info.rd.read_restart_marker();
        for i in 0..info.comps_in_scan {
            let comp = &info.comps[info.cur_comp[i]];
            if Self::uses_dc(info) {
                if let Some(s) = self.dc_stats[comp.dc_tbl_no].as_deref_mut() {
                    *s = [0; DC_STAT_BINS];
                }
                self.last_dc[i] = 0;
                self.dc_context[i] = 0;
            }
            if Self::uses_ac(info)
                && let Some(s) = self.ac_stats[comp.ac_tbl_no].as_deref_mut()
            {
                *s = [0; AC_STAT_BINS];
            }
        }
        self.coder = Coder {
            c: 0,
            a: 0,
            ct: -16,
        };
        self.restarts_to_go = info.restart_interval;
    }

    pub fn decode_mcu(&mut self, info: &mut Info<'_>, blocks: &mut [Block]) {
        if info.restart_interval != 0 {
            if self.restarts_to_go == 0 {
                self.restart(info);
            }
            self.restarts_to_go = self.restarts_to_go.wrapping_sub(1);
        }
        // DC refinement reads on even after a bad code, as in libjpeg.
        if self.coder.ct == -1 && self.mode != Mode::DcRefine {
            return;
        }
        match self.mode {
            Mode::Sequential => self.sequential(info, blocks),
            Mode::DcFirst => self.dc_first(info, blocks),
            Mode::AcFirst => self.ac_first(info, blocks),
            Mode::DcRefine => self.dc_refine(info, blocks),
            Mode::AcRefine => self.ac_refine(info, blocks),
        }
    }

    /// Decodes a DC difference for scan component `ci`; `None` on a bad
    /// code (the scan stops).
    fn dc_diff(&mut self, info: &mut Info<'_>, ci: usize, tbl: usize) -> Option<i32> {
        let (dc_l, dc_u) = (info.arith_dc_l[tbl], info.arith_dc_u[tbl]);
        let rd = &mut info.rd;
        let stats = self.dc_stats[tbl].as_deref_mut()?;
        let coder = &mut self.coder;
        let mut st = self.dc_context[ci];
        if coder.decode(rd, &mut stats[st]) == 0 {
            self.dc_context[ci] = 0;
            return Some(0);
        }
        let sign = coder.decode(rd, &mut stats[st + 1]);
        st += 2 + sign as usize;
        let mut m = coder.decode(rd, &mut stats[st]);
        if m != 0 {
            st = 20;
            while coder.decode(rd, &mut stats[st]) != 0 {
                m <<= 1;
                if m == 0x8000 {
                    coder.ct = -1;
                    return None;
                }
                st += 1;
            }
        }
        self.dc_context[ci] = if m < (1i32 << dc_l) >> 1 {
            0
        } else if m > (1i32 << dc_u) >> 1 {
            12 + sign as usize * 4
        } else {
            4 + sign as usize * 4
        };
        let mut v = m;
        st += 14;
        loop {
            m >>= 1;
            if m == 0 {
                break;
            }
            if coder.decode(rd, &mut stats[st]) != 0 {
                v |= m;
            }
        }
        v += 1;
        Some(if sign != 0 { -v } else { v })
    }

    /// Decodes one AC coefficient's magnitude and sign after its "nonzero"
    /// decision; `None` on a bad code.
    fn ac_value(&mut self, info: &mut Info<'_>, tbl: usize, k: usize, st: usize) -> Option<i32> {
        let kx = usize::from(info.arith_ac_k[tbl]);
        let rd = &mut info.rd;
        let stats = self.ac_stats[tbl].as_deref_mut()?;
        let coder = &mut self.coder;
        let sign = coder.decode(rd, &mut self.fixed_bin);
        let mut st = st + 2;
        let mut m = coder.decode(rd, &mut stats[st]);
        if m != 0 && coder.decode(rd, &mut stats[st]) != 0 {
            m <<= 1;
            st = if k <= kx { 189 } else { 217 };
            while coder.decode(rd, &mut stats[st]) != 0 {
                m <<= 1;
                if m == 0x8000 {
                    coder.ct = -1;
                    return None;
                }
                st += 1;
            }
        }
        let mut v = m;
        st += 14;
        loop {
            m >>= 1;
            if m == 0 {
                break;
            }
            if coder.decode(rd, &mut stats[st]) != 0 {
                v |= m;
            }
        }
        v += 1;
        Some(if sign != 0 { -v } else { v })
    }

    /// The run of AC coefficients from `k` to `se`; returns false on a bad
    /// code.
    fn ac_run(
        &mut self,
        info: &mut Info<'_>,
        block: &mut Block,
        tbl: usize,
        mut k: usize,
        se: usize,
        shift: u32,
    ) -> bool {
        loop {
            let Some(stats) = self.ac_stats[tbl].as_deref_mut() else {
                return false;
            };
            let mut st = 3 * k;
            if self.coder.decode(&mut info.rd, &mut stats[st]) != 0 {
                return true;
            }
            loop {
                k += 1;
                if self.coder.decode(&mut info.rd, &mut stats[st + 1]) != 0 {
                    break;
                }
                st += 3;
                if k >= se {
                    self.coder.ct = -1;
                    return false;
                }
            }
            let Some(v) = self.ac_value(info, tbl, k, st) else {
                return false;
            };
            block[NATURAL_ORDER[k]] = v.wrapping_shl(shift) as i16;
            if k >= se {
                return true;
            }
        }
    }

    /// `decode_mcu`.
    fn sequential(&mut self, info: &mut Info<'_>, blocks: &mut [Block]) {
        for (b, block) in blocks.iter_mut().enumerate().take(info.blocks_in_mcu) {
            let ci = info.mcu_membership[b];
            let comp = &info.comps[info.cur_comp[ci]];
            let (dc, ac) = (comp.dc_tbl_no, comp.ac_tbl_no);
            let Some(d) = self.dc_diff(info, ci, dc) else {
                return;
            };
            self.last_dc[ci] = self.last_dc[ci].wrapping_add(d);
            block[0] = self.last_dc[ci] as i16;
            if info.lim_se == 0 {
                continue;
            }
            if !self.ac_run(info, block, ac, 0, info.lim_se, 0) {
                return;
            }
        }
    }

    /// `decode_mcu_DC_first`.
    fn dc_first(&mut self, info: &mut Info<'_>, blocks: &mut [Block]) {
        for (b, block) in blocks.iter_mut().enumerate().take(info.blocks_in_mcu) {
            let ci = info.mcu_membership[b];
            let tbl = info.comps[info.cur_comp[ci]].dc_tbl_no;
            let Some(d) = self.dc_diff(info, ci, tbl) else {
                return;
            };
            self.last_dc[ci] = self.last_dc[ci].wrapping_add(d);
            block[0] = self.last_dc[ci].wrapping_shl(info.al as u32) as i16;
        }
    }

    /// `decode_mcu_AC_first`.
    fn ac_first(&mut self, info: &mut Info<'_>, blocks: &mut [Block]) {
        let tbl = info.comps[info.cur_comp[0]].ac_tbl_no;
        let (ss, se, al) = (info.ss as usize, info.se as usize, info.al as u32);
        if let Some(block) = blocks.first_mut() {
            self.ac_run(info, block, tbl, ss - 1, se, al);
        }
    }

    /// `decode_mcu_DC_refine`.
    fn dc_refine(&mut self, info: &mut Info<'_>, blocks: &mut [Block]) {
        let p1 = 1i16.wrapping_shl(info.al as u32);
        for block in blocks.iter_mut().take(info.blocks_in_mcu) {
            if self.coder.decode(&mut info.rd, &mut self.fixed_bin) != 0 {
                block[0] |= p1;
            }
        }
    }

    /// `decode_mcu_AC_refine`.
    fn ac_refine(&mut self, info: &mut Info<'_>, blocks: &mut [Block]) {
        let Some(block) = blocks.first_mut() else {
            return;
        };
        let tbl = info.comps[info.cur_comp[0]].ac_tbl_no;
        let (ss, se) = (info.ss as usize, info.se as usize);
        let p1 = 1i16.wrapping_shl(info.al as u32);
        let m1 = p1.wrapping_neg();
        // The end of the band already established by earlier scans.
        let mut kex = se;
        while kex > 0 && block[NATURAL_ORDER[kex]] == 0 {
            kex -= 1;
        }
        let rd = &mut info.rd;
        let Some(stats) = self.ac_stats[tbl].as_deref_mut() else {
            return;
        };
        let coder = &mut self.coder;
        let mut k = ss - 1;
        loop {
            let mut st = 3 * k;
            if k >= kex && coder.decode(rd, &mut stats[st]) != 0 {
                break;
            }
            loop {
                k += 1;
                let coef = &mut block[NATURAL_ORDER[k]];
                if *coef != 0 {
                    if coder.decode(rd, &mut stats[st + 2]) != 0 {
                        *coef = if *coef < 0 {
                            coef.wrapping_add(m1)
                        } else {
                            coef.wrapping_add(p1)
                        };
                    }
                    break;
                }
                if coder.decode(rd, &mut stats[st + 1]) != 0 {
                    *coef = if coder.decode(rd, &mut self.fixed_bin) != 0 {
                        m1
                    } else {
                        p1
                    };
                    break;
                }
                st += 3;
                if k >= se {
                    coder.ct = -1;
                    return;
                }
            }
            if k >= se {
                break;
            }
        }
    }
}
