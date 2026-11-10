// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from libjpeg 10 jdmarker.c, as bundled with MuPDF 1.28.6,
// Copyright (C) 1991-1998, Thomas G. Lane. Modified 2009-2026 by Guido
// Vollbeding. This software is based in part on the work of the
// Independent JPEG Group. The data source follows MuPDF
// source/fitz/filter-dct.c, Copyright (C) 2004-2026 Artifex Software, Inc.

//! Input and marker parsing. The data source is MuPDF's: it hands libjpeg
//! whatever the underlying stream has buffered, and at the end of the data
//! warns and supplies a fake EOI marker (as often as asked), so libjpeg
//! never suspends.

use super::{Error, HuffTable, Info, R};
use crate::stream::Stream;

pub(crate) const M_SOS: i32 = 0xda;
pub(crate) const M_EOI: i32 = 0xd9;
const M_SOI: i32 = 0xd8;
const M_RST0: i32 = 0xd0;
const M_SOF0: i32 = 0xc0;

/// What `read_markers` stopped at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    Sos,
    Eoi,
}

/// The data source plus the marker reader's own state.
pub(crate) struct Reader<'s> {
    chain: Stream<'s>,
    buf: Vec<u8>,
    pos: usize,
    /// `cinfo->unread_marker`: a marker code read but not processed (0 when
    /// there is none).
    pub unread_marker: i32,
    pub saw_soi: bool,
    pub saw_sof: bool,
    pub next_restart_num: i32,
}

impl<'s> Reader<'s> {
    /// Starts at whatever `chain` already has buffered, as `next_dctd` does.
    pub fn new(mut chain: Stream<'s>) -> Reader<'s> {
        let buf = chain.data().to_vec();
        chain.consume(buf.len());
        Reader {
            chain,
            buf,
            pos: 0,
            unread_marker: 0,
            saw_soi: false,
            saw_sof: false,
            next_restart_num: 0,
        }
    }

    /// `fill_input_buffer_dct`.
    fn fill(&mut self) {
        self.buf.clear();
        self.pos = 0;
        let n = self.chain.available(1);
        if n == 0 {
            self.chain
                .diag()
                .warn("filter.dct_premature_end", "premature end of file in jpeg");
            self.buf.extend_from_slice(&[0xff, M_EOI as u8]);
        } else {
            self.buf.extend_from_slice(self.chain.data());
            self.chain.consume(n);
        }
    }

    /// `INPUT_BYTE`.
    pub fn byte(&mut self) -> i32 {
        if self.pos >= self.buf.len() {
            self.fill();
        }
        let b = self.buf.get(self.pos).copied().unwrap_or(M_EOI as u8);
        self.pos += 1;
        i32::from(b)
    }

    /// `INPUT_2BYTES`.
    pub fn two_bytes(&mut self) -> i32 {
        let hi = self.byte();
        (hi << 8) + self.byte()
    }

    /// `skip_input_data_dct`.
    pub fn skip(&mut self, mut n: i64) {
        if n <= 0 {
            return;
        }
        loop {
            let left = (self.buf.len() - self.pos.min(self.buf.len())) as i64;
            if n <= left {
                break;
            }
            n -= left;
            self.fill();
        }
        self.pos += n as usize;
    }

    /// `next_marker`: skips to the next marker. Garbage before it would be
    /// a (swallowed) libjpeg warning.
    pub fn next_marker(&mut self) {
        let c = loop {
            let mut c = self.byte();
            while c != 0xff {
                c = self.byte();
            }
            loop {
                c = self.byte();
                if c != 0xff {
                    break;
                }
            }
            if c != 0 {
                break c;
            }
        };
        self.unread_marker = c;
    }

    /// `first_marker`.
    fn first_marker(&mut self) -> R<()> {
        let c = self.byte();
        let c2 = self.byte();
        if c != 0xff || c2 != M_SOI {
            return Err(Error::jpeg(format!(
                "Not a JPEG file: starts with 0x{c:02x} 0x{c2:02x}"
            )));
        }
        self.unread_marker = c2;
        Ok(())
    }

    /// `read_restart_marker`.
    pub fn read_restart_marker(&mut self) {
        if self.unread_marker == 0 {
            self.next_marker();
        }
        if self.unread_marker == M_RST0 + self.next_restart_num {
            self.unread_marker = 0;
        } else {
            self.resync_to_restart(self.next_restart_num);
        }
        self.next_restart_num = (self.next_restart_num + 1) & 7;
    }

    /// `jpeg_resync_to_restart`.
    fn resync_to_restart(&mut self, desired: i32) {
        let rst = |d: i32| M_RST0 + (d & 7);
        let mut marker = self.unread_marker;
        loop {
            let action = if marker < M_SOF0 {
                2
            } else if !(M_RST0..=M_RST0 + 7).contains(&marker)
                || marker == rst(desired + 1)
                || marker == rst(desired + 2)
            {
                3
            } else if marker == rst(desired - 1) || marker == rst(desired - 2) {
                2
            } else {
                1
            };
            match action {
                1 => {
                    self.unread_marker = 0;
                    return;
                }
                2 => {
                    self.next_marker();
                    marker = self.unread_marker;
                }
                _ => return,
            }
        }
    }
}

// The natural-order tables for quantization tables smaller than 8x8, padded
// with 63s as libjpeg's are.
const NATURAL_ORDER7: [usize; 49] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 14, 21, 28, 35, 42, 49, 50, 43, 36, 29, 22, 30, 37, 44, 51, 52, 45, 38, 46, 53, 54,
];
const NATURAL_ORDER6: [usize; 36] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 41, 34, 27, 20, 13,
    21, 28, 35, 42, 43, 36, 29, 37, 44, 45,
];
const NATURAL_ORDER5: [usize; 25] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 12, 19, 26, 33, 34, 27, 20, 28, 35, 36,
];
const NATURAL_ORDER4: [usize; 16] = [0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 25, 18, 11, 19, 26, 27];
const NATURAL_ORDER3: [usize; 9] = [0, 1, 8, 16, 9, 2, 10, 17, 18];
const NATURAL_ORDER2: [usize; 4] = [0, 1, 8, 9];

impl Info<'_> {
    /// `read_markers`: processes markers up to the next SOS or EOI.
    pub(crate) fn read_markers(&mut self) -> R<Stop> {
        loop {
            if self.rd.unread_marker == 0 {
                if !self.rd.saw_soi {
                    self.rd.first_marker()?;
                } else {
                    self.rd.next_marker();
                }
            }
            match self.rd.unread_marker {
                M_SOI => self.get_soi()?,
                0xc0 => self.get_sof(true, false, false)?,
                0xc1 => self.get_sof(false, false, false)?,
                0xc2 => self.get_sof(false, true, false)?,
                0xc9 => self.get_sof(false, false, true)?,
                0xca => self.get_sof(false, true, true)?,
                m @ (0xc3 | 0xc5 | 0xc6 | 0xc7 | 0xc8 | 0xcb | 0xcd | 0xce | 0xcf) => {
                    return Err(Error::jpeg(format!(
                        "Unsupported JPEG process: SOF type 0x{m:02x}"
                    )));
                }
                M_SOS => {
                    self.get_sos()?;
                    self.rd.unread_marker = 0;
                    return Ok(Stop::Sos);
                }
                M_EOI => {
                    self.rd.unread_marker = 0;
                    return Ok(Stop::Eoi);
                }
                0xcc => self.get_dac()?,
                0xc4 => self.get_dht()?,
                0xdb => self.get_dqt()?,
                0xdd => self.get_dri()?,
                0xf8 => self.get_lse()?,
                0xe0 | 0xee => self.get_interesting_appn(),
                0xe1..=0xef | 0xfe | 0xdc => self.skip_variable(),
                0xd0..=0xd7 | 0x01 => {}
                m => {
                    return Err(Error::jpeg(format!("Unsupported marker type 0x{m:02x}")));
                }
            }
            self.rd.unread_marker = 0;
        }
    }

    fn get_soi(&mut self) -> R<()> {
        if self.rd.saw_soi {
            return Err(Error::jpeg("Invalid JPEG file structure: two SOI markers"));
        }
        self.arith_dc_l = [0; 16];
        self.arith_dc_u = [1; 16];
        self.arith_ac_k = [5; 16];
        self.restart_interval = 0;
        self.subtract_green = false;
        self.saw_jfif = false;
        self.saw_adobe = false;
        self.adobe_transform = 0;
        self.rd.saw_soi = true;
        Ok(())
    }

    fn get_sof(&mut self, baseline: bool, progressive: bool, arith: bool) -> R<()> {
        self.is_baseline = baseline;
        self.progressive = progressive;
        self.arith_code = arith;
        let length = self.rd.two_bytes() - 8;
        self.precision = self.rd.byte();
        self.image_height = self.rd.two_bytes() as usize;
        self.image_width = self.rd.two_bytes() as usize;
        let n = self.rd.byte() as usize;
        if self.rd.saw_sof {
            return Err(Error::jpeg("Invalid JPEG file structure: two SOF markers"));
        }
        if self.image_height == 0 || self.image_width == 0 || n == 0 {
            return Err(Error::jpeg("Empty JPEG image (DNL not supported)"));
        }
        if length != n as i32 * 3 {
            return Err(Error::jpeg("Bogus marker length"));
        }
        self.comps = Vec::with_capacity(n);
        for ci in 0..n {
            let mut c = self.rd.byte();
            // A repeated ID becomes one more than the largest so far.
            if self.comps.iter().any(|p| p.id == c) {
                c = self.comps.iter().map(|p| p.id).max().unwrap_or(c) + 1;
            }
            let s = self.rd.byte();
            let quant = self.rd.byte();
            self.comps.push(super::Component {
                id: c,
                index: ci,
                h: (s >> 4 & 15) as usize,
                v: (s & 15) as usize,
                quant_tbl_no: quant,
                ..super::Component::default()
            });
        }
        self.rd.saw_sof = true;
        Ok(())
    }

    fn get_sos(&mut self) -> R<()> {
        if !self.rd.saw_sof {
            return Err(Error::jpeg("Invalid JPEG file structure: SOS before SOF"));
        }
        let length = self.rd.two_bytes();
        let n = self.rd.byte();
        if length != n * 2 + 6 || n > 4 || (n == 0 && !self.progressive) {
            return Err(Error::jpeg("Bogus marker length"));
        }
        let n = n as usize;
        self.comps_in_scan = n;
        for i in 0..n {
            let mut c = self.rd.byte();
            let scan = &self.cur_comp[..i];
            if scan.iter().any(|&ci| self.comps[ci].id == c) {
                c = scan.iter().map(|&ci| self.comps[ci].id).max().unwrap_or(c) + 1;
            }
            let Some(ci) = self.comps.iter().position(|p| p.id == c) else {
                return Err(Error::jpeg(format!("Invalid component ID {c} in SOS")));
            };
            self.cur_comp[i] = ci;
            let t = self.rd.byte();
            self.comps[ci].dc_tbl_no = (t >> 4 & 15) as usize;
            self.comps[ci].ac_tbl_no = (t & 15) as usize;
        }
        self.ss = self.rd.byte();
        self.se = self.rd.byte();
        let a = self.rd.byte();
        self.ah = a >> 4 & 15;
        self.al = a & 15;
        self.rd.next_restart_num = 0;
        if n > 0 {
            self.input_scan_number += 1;
        }
        Ok(())
    }

    fn get_dac(&mut self) -> R<()> {
        let mut length = self.rd.two_bytes() - 2;
        while length > 0 {
            let index = self.rd.byte() as usize;
            let val = self.rd.byte();
            length -= 2;
            if index >= 32 {
                return Err(Error::jpeg(format!("Bogus DAC index {index}")));
            }
            if index >= 16 {
                self.arith_ac_k[index - 16] = val as u8;
            } else {
                self.arith_dc_l[index] = (val & 0x0f) as u8;
                self.arith_dc_u[index] = (val >> 4) as u8;
                if self.arith_dc_l[index] > self.arith_dc_u[index] {
                    return Err(Error::jpeg(format!("Bogus DAC value 0x{val:x}")));
                }
            }
        }
        if length != 0 {
            return Err(Error::jpeg("Bogus marker length"));
        }
        Ok(())
    }

    fn get_dht(&mut self) -> R<()> {
        let mut length = self.rd.two_bytes() - 2;
        while length > 16 {
            let mut index = self.rd.byte();
            let mut bits = [0u8; 17];
            let mut count = 0;
            for b in bits.iter_mut().skip(1) {
                *b = self.rd.byte() as u8;
                count += i32::from(*b);
            }
            length -= 1 + 16;
            if count > 256 || count > length {
                return Err(Error::jpeg("Bogus Huffman table definition"));
            }
            let mut huffval = [0u8; 256];
            for v in huffval.iter_mut().take(count as usize) {
                *v = self.rd.byte() as u8;
            }
            length -= count;
            let ac = index & 0x10 != 0;
            if ac {
                index -= 0x10;
            }
            if !(0..4).contains(&index) {
                return Err(Error::jpeg(format!("Bogus DHT index {index}")));
            }
            let slot = if ac {
                &mut self.ac_huff[index as usize]
            } else {
                &mut self.dc_huff[index as usize]
            };
            // A redefinition keeps values past the new count from the old
            // table (libjpeg copies only `count` of them).
            let table = slot.get_or_insert_with(HuffTable::default);
            table.bits = bits;
            table.huffval[..count as usize].copy_from_slice(&huffval[..count as usize]);
        }
        if length != 0 {
            return Err(Error::jpeg("Bogus marker length"));
        }
        Ok(())
    }

    fn get_dqt(&mut self) -> R<()> {
        let mut length = self.rd.two_bytes() - 2;
        while length > 0 {
            length -= 1;
            let n = self.rd.byte();
            let prec = n >> 4 != 0;
            let n = (n & 0x0f) as usize;
            if n >= 4 {
                return Err(Error::jpeg(format!("Bogus DQT index {n}")));
            }
            let table = self.quant_tbls[n].get_or_insert([0; 64]);
            let count = if prec {
                if length < 128 {
                    *table = [1; 64];
                    length >> 1
                } else {
                    64
                }
            } else if length < 64 {
                *table = [1; 64];
                length
            } else {
                64
            };
            let order: &[usize] = match count {
                4 => &NATURAL_ORDER2,
                9 => &NATURAL_ORDER3,
                16 => &NATURAL_ORDER4,
                25 => &NATURAL_ORDER5,
                36 => &NATURAL_ORDER6,
                49 => &NATURAL_ORDER7,
                _ => &super::NATURAL_ORDER,
            };
            for &pos in order.iter().take(count as usize) {
                let v = if prec {
                    self.rd.two_bytes()
                } else {
                    self.rd.byte()
                };
                table[pos] = v as u16;
            }
            length -= count;
            if prec {
                length -= count;
            }
        }
        if length != 0 {
            return Err(Error::jpeg("Bogus marker length"));
        }
        Ok(())
    }

    fn get_dri(&mut self) -> R<()> {
        if self.rd.two_bytes() != 4 {
            return Err(Error::jpeg("Bogus marker length"));
        }
        self.restart_interval = self.rd.two_bytes() as u32;
        Ok(())
    }

    /// `get_lse`: the JPEG-LS color transform marker, which libjpeg accepts
    /// only in the form its encoder writes (subtract green).
    fn get_lse(&mut self) -> R<()> {
        if !self.rd.saw_sof {
            return Err(Error::jpeg("Invalid JPEG file structure: LSE before SOF"));
        }
        let bad = || Err(Error::jpeg("Unsupported color conversion request"));
        if self.comps.len() < 3 {
            return bad();
        }
        if self.rd.two_bytes() != 24 {
            return Err(Error::jpeg("Bogus marker length"));
        }
        if self.rd.byte() != 0x0d {
            return Err(Error::jpeg(format!(
                "Unsupported marker type 0x{:02x}",
                self.rd.unread_marker
            )));
        }
        self.lse_maxtrans = self.rd.two_bytes();
        if self.rd.byte() != 3 {
            return bad();
        }
        for want in [1, 0, 2] {
            if self.rd.byte() != self.comps[want].id {
                return bad();
            }
        }
        // The rest of the marker must describe G, R - G, B - G.
        let expect: [(bool, i32); 10] = [
            (false, 0x80),
            (true, 0),
            (true, 0),
            (false, 0),
            (true, 1),
            (true, 0),
            (false, 0),
            (true, 1),
            (true, 0),
            (false, -1),
        ];
        for (two, v) in expect.into_iter().take(9) {
            let got = if two {
                self.rd.two_bytes()
            } else {
                self.rd.byte()
            };
            if got != v {
                return bad();
            }
        }
        self.subtract_green = true;
        Ok(())
    }

    /// `get_interesting_appn`: APP0 (JFIF) and APP14 (Adobe).
    fn get_interesting_appn(&mut self) {
        let mut length = i64::from(self.rd.two_bytes()) - 2;
        let n = length.clamp(0, 14) as usize;
        let mut b = [0u8; 14];
        for v in b.iter_mut().take(n) {
            *v = self.rd.byte() as u8;
        }
        length -= n as i64;
        let data = &b[..n];
        if self.rd.unread_marker == 0xe0 {
            if data.len() >= 14 && data.starts_with(b"JFIF\0") {
                self.saw_jfif = true;
            }
        } else if data.len() >= 12 && data.starts_with(b"Adobe") {
            self.saw_adobe = true;
            self.adobe_transform = data[11];
        }
        self.rd.skip(length);
    }

    fn skip_variable(&mut self) {
        let length = i64::from(self.rd.two_bytes()) - 2;
        self.rd.skip(length);
    }
}
