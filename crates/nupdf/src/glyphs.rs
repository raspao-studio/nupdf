// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/encodings.c and source/pdf/pdf-font.c
// (pdf_load_encoding), Copyright (C) 2004-2026 Artifex Software, Inc.

//! Glyph names to Unicode, as MuPDF reads them with the Adobe Glyph List,
//! and the named encodings of simple fonts.

use crate::tables;

/// `FZ_REPLACEMENT_CHARACTER`.
pub const REPLACEMENT: u32 = 0xfffd;

fn agl(name: &[u8]) -> Option<u32> {
    tables::AGL_NAMES
        .binary_search_by(|n| n.as_bytes().cmp(name))
        .ok()
        .map(|i| u32::from(tables::AGL_CODES[i]))
}

/// `fz_unicode_from_glyph_name_strict`: 0 for a name not in the list.
pub fn unicode_strict(name: &[u8]) -> u32 {
    agl(name).unwrap_or(0)
}

/// `strtol` of all of `s` in `base`, 0 unless it all reads.
fn read_num(s: &[u8], base: u32) -> i64 {
    let text = std::str::from_utf8(s).unwrap_or("");
    let t = text.trim_start();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let digits = if base == 16 {
        digits
            .strip_prefix("0x")
            .or_else(|| digits.strip_prefix("0X"))
            .unwrap_or(digits)
    } else {
        digits
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(base)) {
        return 0;
    }
    // strtol saturates at the long range.
    let v = i64::from_str_radix(digits, base).unwrap_or(i64::MAX);
    if neg { -v } else { v }
}

/// `fz_unicode_from_glyph_name`: the list, then uniXXXX, uXXXX, aNN, the
/// Distiller and Ghostscript conventions, and a bare number; U+FFFD for
/// anything else.
pub fn unicode(name: &[u8]) -> u32 {
    let mut buf = name[..name.len().min(63)].to_vec();
    if let Some(p) = buf.iter().position(|&b| b == b'.') {
        buf.truncate(p);
    }
    if let Some(p) = buf.iter().position(|&b| b == b'_') {
        if buf.first() == Some(&b'f') {
            let lig: Option<&[u8]> = match buf.as_slice() {
                b"f_f" => Some(b"ff"),
                b"f_f_i" => Some(b"ffi"),
                b"f_f_l" => Some(b"ffl"),
                b"f_i" => Some(b"fi"),
                b"f_l" => Some(b"fl"),
                _ => None,
            };
            match lig {
                Some(l) => buf = l.to_vec(),
                None => buf.truncate(p),
            }
        } else {
            buf.truncate(p);
        }
    }
    if let Some(c) = agl(&buf) {
        return c;
    }
    let code = if buf.starts_with(b"uni") && buf.len() == 7 {
        read_num(&buf[3..], 16)
    } else if buf.first() == Some(&b'u') {
        read_num(&buf[1..], 16)
    } else if buf.first() == Some(&b'a') && buf.len() > 2 {
        read_num(&buf[1..], 10)
    } else if buf.first() == Some(&b'G') {
        let c = read_num(&buf[1..], 16);
        if c > 0 && c < 256 {
            i64::from(tables::DOS_437[c as usize])
        } else {
            0
        }
    } else if buf.first() == Some(&b'g') {
        let c = read_num(&buf[1..], 16);
        if c > 0 && c < 258 {
            i64::from(tables::TRUETYPE_GLYPH_INDEX[c as usize])
        } else {
            0
        }
    } else {
        read_num(&buf, 10)
    };
    if code > 0 && code <= 0x10ffff {
        code as u32
    } else {
        REPLACEMENT
    }
}

/// `fz_duplicate_glyph_names_from_unicode`: other names for a code point.
pub fn duplicate_names(ucs: u32) -> Vec<&'static str> {
    let offsets = &tables::AGL_DUP_OFFSETS;
    let pairs = offsets.len() / 2;
    let (mut l, mut r) = (0i64, pairs as i64 - 1);
    while l <= r {
        let m = ((l + r) >> 1) as usize;
        let u = u32::from(offsets[m * 2]);
        if ucs < u {
            r = m as i64 - 1;
        } else if ucs > u {
            l = m as i64 + 1;
        } else {
            let at = usize::from(offsets[m * 2 + 1]);
            return tables::AGL_DUP_NAMES[at..]
                .iter()
                .take_while(|n| !n.is_empty())
                .copied()
                .collect();
        }
    }
    Vec::new()
}

/// `pdf_load_encoding`: a base encoding's names over `estrings`.
pub fn load_encoding(estrings: &mut [Option<Vec<u8>>; 256], name: &[u8]) {
    let table = match name {
        b"StandardEncoding" => &tables::STANDARD,
        b"MacRomanEncoding" => &tables::MAC_ROMAN,
        b"MacExpertEncoding" => &tables::MAC_EXPERT,
        b"WinAnsiEncoding" => &tables::WIN_ANSI,
        _ => return,
    };
    for (slot, n) in estrings.iter_mut().zip(table.iter()) {
        if let Some(n) = n {
            *slot = Some(n.as_bytes().to_vec());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_read_as_unicode_by_mupdfs_rules() {
        assert_eq!(unicode(b"A"), 0x41);
        assert_eq!(unicode(b"fi"), 0xfb01);
        assert_eq!(unicode(b"f_i"), 0xfb01);
        assert_eq!(unicode(b"a.sc"), 0x61);
        assert_eq!(unicode(b"uni0041"), 0x41);
        assert_eq!(unicode(b"u1F600"), 0x1f600);
        assert_eq!(unicode(b"a65"), 65);
        assert_eq!(
            unicode(b"g24"),
            u32::from(tables::TRUETYPE_GLYPH_INDEX[0x24])
        );
        assert_eq!(unicode(b"nonsense"), REPLACEMENT);
        assert_eq!(unicode_strict(b"nonsense"), 0);
        assert!(duplicate_names(0x20).contains(&"spacehackarabic"));
    }

    #[test]
    fn named_encodings_fill_their_codes() {
        let mut e: [Option<Vec<u8>>; 256] = std::array::from_fn(|_| None);
        load_encoding(&mut e, b"WinAnsiEncoding");
        assert_eq!(e[0x80].as_deref(), Some(&b"Euro"[..]));
        load_encoding(&mut e, b"Bogus");
        assert_eq!(e[0x41].as_deref(), Some(&b"A"[..]));
    }
}
