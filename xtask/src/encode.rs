// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Encoders for the filter fixtures. Kept apart from nupdf's decoders so the
//! fixtures test them against MuPDF rather than against themselves.

use std::collections::HashMap;
use std::io::Write as _;

/// PDF LZW (9 to 12-bit codes, a clear code first and whenever the table
/// fills, EOD last). With `early_change`, the code width grows one code
/// earlier, as `/EarlyChange 1` (the default) says.
pub fn lzw(data: &[u8], early_change: bool) -> Vec<u8> {
    const CLEAR: u16 = 256;
    const EOD: u16 = 257;
    let early = u32::from(early_change);
    let mut out = BitWriter::default();
    let mut table: HashMap<Vec<u8>, u16> = HashMap::new();
    let reset = |table: &mut HashMap<Vec<u8>, u16>| {
        table.clear();
        for i in 0..256u16 {
            table.insert(vec![i as u8], i);
        }
    };
    reset(&mut table);
    let mut next: u32 = 258;
    let mut bits = 9;
    out.write(u32::from(CLEAR), bits);
    let mut w: Vec<u8> = Vec::new();
    for &c in data {
        let mut wc = w.clone();
        wc.push(c);
        if table.contains_key(&wc) {
            w = wc;
            continue;
        }
        out.write(u32::from(table[&w]), bits);
        table.insert(wc, next as u16);
        next += 1;
        // The decoder adds its entries one code later than this encoder.
        if next > (1 << bits) - early && bits < 12 {
            bits += 1;
        }
        if next >= 4093 {
            out.write(u32::from(CLEAR), bits);
            reset(&mut table);
            next = 258;
            bits = 9;
        }
        w = vec![c];
    }
    if !w.is_empty() {
        out.write(u32::from(table[&w]), bits);
        next += 1;
        if next > (1 << bits) - early && bits < 12 {
            bits += 1;
        }
    }
    out.write(u32::from(EOD), bits);
    out.finish()
}

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl BitWriter {
    fn write(&mut self, code: u32, bits: u32) {
        self.acc = (self.acc << bits) | code;
        self.n += bits;
        while self.n >= 8 {
            self.n -= 8;
            self.out.push((self.acc >> self.n) as u8);
        }
        self.acc &= (1 << self.n) - 1;
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push((self.acc << (8 - self.n)) as u8);
        }
        self.out
    }
}

/// PackBits runs, ended by 128.
pub fn run_length(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let mut run = 1;
        while i + run < data.len() && data[i + run] == data[i] && run < 128 {
            run += 1;
        }
        if run >= 3 {
            out.push((257 - run) as u8);
            out.push(data[i]);
            i += run;
            continue;
        }
        let start = i;
        while i < data.len() && i - start < 128 {
            if i + 2 < data.len() && data[i] == data[i + 1] && data[i] == data[i + 2] {
                break;
            }
            i += 1;
        }
        out.push((i - start - 1) as u8);
        out.extend_from_slice(&data[start..i]);
    }
    out.push(128);
    out
}

/// Hex digits in lines of 64, then `>`.
pub fn ascii_hex(data: &[u8]) -> Vec<u8> {
    let mut out = String::new();
    for (i, b) in data.iter().enumerate() {
        out.push_str(&format!("{b:02X}"));
        if i % 32 == 31 {
            out.push('\n');
        }
    }
    out.push('>');
    out.into_bytes()
}

/// Base-85 groups (`z` for zeros), then `~>`.
pub fn ascii85(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in data.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        let mut v = u32::from_be_bytes(word);
        if chunk.len() == 4 && v == 0 {
            out.push(b'z');
            continue;
        }
        let mut digits = [0u8; 5];
        for d in digits.iter_mut().rev() {
            *d = (v % 85) as u8 + b'!';
            v /= 85;
        }
        out.extend_from_slice(&digits[..chunk.len() + 1]);
    }
    out.extend_from_slice(b"~>");
    out
}

pub fn brotli(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut w = brotli::CompressorWriter::new(&mut out, 4096, 9, 22);
        let _ = w.write_all(data);
    }
    out
}

pub fn flate(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

/// PNG prediction with one filter type for every row, as the PDF
/// predictors 10 to 14 expect (the type byte leads each row).
pub fn png_predict(data: &[u8], columns: usize, kind: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = vec![0u8; columns];
    for row in data.chunks(columns) {
        out.push(kind);
        for (i, &b) in row.iter().enumerate() {
            let left = if i > 0 { row[i - 1] } else { 0 };
            let up = prev[i];
            let up_left = if i > 0 { prev[i - 1] } else { 0 };
            let p = match kind {
                1 => left,
                2 => up,
                3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
                4 => {
                    let (a, b2, c) = (i32::from(left), i32::from(up), i32::from(up_left));
                    let p = a + b2 - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b2).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc {
                        left
                    } else if pb <= pc {
                        up
                    } else {
                        up_left
                    }
                }
                _ => 0,
            };
            out.push(b.wrapping_sub(p));
        }
        prev[..row.len()].copy_from_slice(row);
    }
    out
}

/// zlib framing around stored blocks of `block` bytes, followed by a block
/// with the invalid type 3 and more bytes, so inflating fails partway with
/// input left over.
pub fn flate_broken_after(data: &[u8], block: usize) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    for chunk in data.chunks(block) {
        let len = chunk.len() as u16;
        out.push(0x00); // BFINAL 0, BTYPE 00 (stored)
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.push(0x07); // BFINAL 1, BTYPE 11 (invalid)
    out.extend_from_slice(b"trailing bytes after the bad block");
    out
}
