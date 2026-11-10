// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/encode-fax.c and source/fitz/buffer.c
// (fz_append_bits), Copyright (C) 2004-2026 Artifex Software, Inc.

//! MuPDF's CCITT Group 4 encoder (`fz_compress_ccitt_fax_g4`), which
//! `clean` uses for uncompressed 1-bit images. Rows are packed 1 bit per
//! pixel with 0 for black (`BlackIs1` false); the output ends with two EOL
//! codes, and its last byte is padded with zeros.

/// A code and its length in bits.
type Code = (u16, u8);

const EOL: Code = (1, 12);
const PASS: Code = (0x1, 4);
const VERTICAL: [Code; 7] = [
    (0x3, 7),
    (0x3, 6),
    (0x3, 3),
    (0x1, 1),
    (0x2, 3),
    (0x2, 6),
    (0x2, 7),
];
const HORIZONTAL: Code = (1, 3);

#[rustfmt::skip]
const WHITE_TERMINATION: [Code; 64] = [
    (0x35, 8), (0x7, 6), (0x7, 4), (0x8, 4), (0xb, 4), (0xc, 4), (0xe, 4), (0xf, 4),
    (0x13, 5), (0x14, 5), (0x7, 5), (0x8, 5), (0x8, 6), (0x3, 6), (0x34, 6), (0x35, 6),
    (0x2a, 6), (0x2b, 6), (0x27, 7), (0xc, 7), (0x8, 7), (0x17, 7), (0x3, 7), (0x4, 7),
    (0x28, 7), (0x2b, 7), (0x13, 7), (0x24, 7), (0x18, 7), (0x2, 8), (0x3, 8), (0x1a, 8),
    (0x1b, 8), (0x12, 8), (0x13, 8), (0x14, 8), (0x15, 8), (0x16, 8), (0x17, 8), (0x28, 8),
    (0x29, 8), (0x2a, 8), (0x2b, 8), (0x2c, 8), (0x2d, 8), (0x4, 8), (0x5, 8), (0xa, 8),
    (0xb, 8), (0x52, 8), (0x53, 8), (0x54, 8), (0x55, 8), (0x24, 8), (0x25, 8), (0x58, 8),
    (0x59, 8), (0x5a, 8), (0x5b, 8), (0x4a, 8), (0x4b, 8), (0x32, 8), (0x33, 8), (0x34, 8),
];
#[rustfmt::skip]
const WHITE_MAKEUP: [Code; 41] = [
    (0, 0), (0x1b, 5), (0x12, 5), (0x17, 6), (0x37, 7), (0x36, 8), (0x37, 8), (0x64, 8),
    (0x65, 8), (0x68, 8), (0x67, 8), (0xcc, 9), (0xcd, 9), (0xd2, 9), (0xd3, 9), (0xd4, 9),
    (0xd5, 9), (0xd6, 9), (0xd7, 9), (0xd8, 9), (0xd9, 9), (0xda, 9), (0xdb, 9), (0x98, 9),
    (0x99, 9), (0x9a, 9), (0x18, 6), (0x9b, 9), (0x8, 11), (0xc, 11), (0xd, 11), (0x12, 12),
    (0x13, 12), (0x14, 12), (0x15, 12), (0x16, 12), (0x17, 12), (0x1c, 12), (0x1d, 12), (0x1e, 12),
    (0x1f, 12),
];
#[rustfmt::skip]
const BLACK_TERMINATION: [Code; 64] = [
    (0x37, 10), (0x2, 3), (0x3, 2), (0x2, 2), (0x3, 3), (0x3, 4), (0x2, 4), (0x3, 5),
    (0x5, 6), (0x4, 6), (0x4, 7), (0x5, 7), (0x7, 7), (0x4, 8), (0x7, 8), (0x18, 9),
    (0x17, 10), (0x18, 10), (0x8, 10), (0x67, 11), (0x68, 11), (0x6c, 11), (0x37, 11), (0x28, 11),
    (0x17, 11), (0x18, 11), (0xca, 12), (0xcb, 12), (0xcc, 12), (0xcd, 12), (0x68, 12), (0x69, 12),
    (0x6a, 12), (0x6b, 12), (0xd2, 12), (0xd3, 12), (0xd4, 12), (0xd5, 12), (0xd6, 12), (0xd7, 12),
    (0x6c, 12), (0x6d, 12), (0xda, 12), (0xdb, 12), (0x54, 12), (0x55, 12), (0x56, 12), (0x57, 12),
    (0x64, 12), (0x65, 12), (0x52, 12), (0x53, 12), (0x24, 12), (0x37, 12), (0x38, 12), (0x27, 12),
    (0x28, 12), (0x58, 12), (0x59, 12), (0x2b, 12), (0x2c, 12), (0x5a, 12), (0x66, 12), (0x67, 12),
];
#[rustfmt::skip]
const BLACK_MAKEUP: [Code; 41] = [
    (0, 0), (0xf, 10), (0xc8, 12), (0xc9, 12), (0x5b, 12), (0x33, 12), (0x34, 12), (0x35, 12),
    (0x6c, 13), (0x6d, 13), (0x4a, 13), (0x4b, 13), (0x4c, 13), (0x4d, 13), (0x72, 13), (0x73, 13),
    (0x74, 13), (0x75, 13), (0x76, 13), (0x77, 13), (0x52, 13), (0x53, 13), (0x54, 13), (0x55, 13),
    (0x5a, 13), (0x5b, 13), (0x64, 13), (0x65, 13), (0x8, 11), (0xc, 11), (0xd, 11), (0x12, 12),
    (0x13, 12), (0x14, 12), (0x15, 12), (0x16, 12), (0x17, 12), (0x1c, 12), (0x1d, 12), (0x1e, 12),
    (0x1f, 12),
];

/// `fz_append_bits`: most significant bit first.
struct Bits {
    out: Vec<u8>,
    /// Unused bits in the last byte.
    unused: u32,
}

impl Bits {
    fn put(&mut self, (code, nbits): Code) {
        let mut left = u32::from(nbits);
        while left > 0 {
            if self.unused == 0 {
                self.out.push(0);
                self.unused = 8;
            }
            let take = left.min(self.unused);
            let chunk = (u32::from(code) >> (left - take)) & ((1 << take) - 1);
            if let Some(last) = self.out.last_mut() {
                *last |= (chunk << (self.unused - take)) as u8;
            }
            self.unused -= take;
            left -= take;
        }
    }

    fn run(&mut self, run: i32, black: bool) {
        let (termination, makeup) = if black {
            (&BLACK_TERMINATION, &BLACK_MAKEUP)
        } else {
            (&WHITE_TERMINATION, &WHITE_MAKEUP)
        };
        if run > 63 {
            let mut m = run >> 6;
            while m > 40 {
                self.put(makeup[40]);
                m -= 40;
            }
            if m > 0 {
                self.put(makeup[m as usize]);
            }
            self.put(termination[(run & 63) as usize]);
        } else {
            self.put(termination[run.max(0) as usize]);
        }
    }
}

/// Pixel `x` of `line`, true for black.
fn get_bit(line: &[u8], x: i32) -> bool {
    line.get((x >> 3) as usize)
        .is_some_and(|b| (b >> (7 - (x & 7))) & 1 == 0)
}

/// The next position after `x` where the colour changes, or `w`; from the
/// start of the row, as a change from white, when `x` is -1.
fn find_changing(line: Option<&[u8]>, x: i32, w: i32) -> i32 {
    let Some(line) = line else { return w };
    if x >= w {
        return w;
    }
    let (a, mut x) = if x == -1 {
        (false, 0)
    } else {
        (get_bit(line, x), x + 1)
    };
    while x < w {
        if get_bit(line, x) != a {
            break;
        }
        x += 1;
    }
    x
}

fn find_changing_color(line: Option<&[u8]>, x: i32, w: i32, black: bool) -> i32 {
    let Some(row) = line else { return w };
    if x >= w {
        return w;
    }
    let mut x = find_changing(line, x, w);
    if x < w && get_bit(row, x) != black {
        x = find_changing(line, x, w);
    }
    x
}

/// `fz_compress_ccitt_fax_g4` over `rows` rows of `stride` bytes, or `None`
/// once the output would pass `limit` bytes.
pub fn compress_g4(
    src: &[u8],
    columns: i32,
    rows: i32,
    stride: usize,
    limit: usize,
) -> Option<Vec<u8>> {
    let mut bits = Bits {
        out: Vec::new(),
        unused: 0,
    };
    let mut reference: Option<&[u8]> = None;
    let mut at = 0usize;
    for _ in 0..rows.max(0) {
        let line = src.get(at..).unwrap_or_default();
        let mut a0 = -1;
        let mut black = false;
        while a0 < columns {
            let a1 = find_changing(Some(line), a0, columns);
            let b1 = find_changing_color(reference, a0, columns, !black);
            let b2 = find_changing(reference, b1, columns);
            let diff = b1 - a1;
            a0 = a0.max(0);
            if b2 < a1 {
                bits.put(PASS);
                a0 = b2;
            } else if (-3..=3).contains(&diff) {
                bits.put(VERTICAL[(diff + 3) as usize]);
                a0 = a1;
                black = !black;
            } else {
                let a2 = find_changing(Some(line), a1, columns);
                bits.put(HORIZONTAL);
                bits.run(a1 - a0, black);
                bits.run(a2 - a1, !black);
                a0 = a2;
            }
        }
        if bits.out.len() > limit {
            return None;
        }
        reference = Some(line);
        at = at.saturating_add(stride);
    }
    bits.put(EOL);
    bits.put(EOL);
    Some(bits.out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::Diagnostics;
    use crate::fax::{FaxParams, open};
    use crate::stream::Stream;

    fn decode(data: &[u8], columns: i32) -> Vec<u8> {
        let p = FaxParams {
            k: -1,
            end_of_line: false,
            encoded_byte_align: false,
            columns,
            rows: 0,
            end_of_block: true,
            black_is_1: false,
        };
        let diag = Diagnostics::new();
        open(Stream::memory(data, &diag), p)
            .and_then(|mut s| s.read_best(0, 0, usize::MAX).map_err(|e| format!("{e:?}")))
            .unwrap_or_default()
    }

    #[test]
    fn white_rows_are_vertical_codes() {
        // Two white rows of 8: V0 each, then two EOLs.
        let out = compress_g4(&[0xff, 0xff], 8, 2, 1, usize::MAX).unwrap_or_default();
        assert_eq!(out, [0xc0, 0x04, 0x00, 0x40]);
    }

    #[test]
    fn rows_decode_back() {
        let w = 77;
        let stride = (w as usize).div_ceil(8);
        let mut x = 5u32;
        let mut image = Vec::new();
        for row in 0..40 {
            for _ in 0..stride {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                // Runs long and short: whole bytes of one colour, or noise.
                image.push(match row % 4 {
                    0 => 0xff,
                    1 => 0x00,
                    _ => (x >> 16) as u8,
                });
            }
            // Pad bits past the last column are white, as MuPDF decodes them.
            if let Some(last) = image.last_mut() {
                *last |= 0xff >> (w % 8);
            }
        }
        let out = compress_g4(&image, w, 40, stride, usize::MAX).unwrap_or_default();
        assert_eq!(decode(&out, w), image);
        // A run past 64 needs make-up codes; past 2560, several.
        let wide = 3000;
        let row = vec![0u8; 375];
        let out = compress_g4(&row, wide, 1, 375, usize::MAX).unwrap_or_default();
        assert_eq!(decode(&out, wide), row);
    }

    #[test]
    fn output_stops_at_the_limit() {
        assert_eq!(compress_g4(&[], 0, 1000, 0, 10), None);
        assert!(compress_g4(&[], 0, 80, 0, 10).is_some());
    }
}
