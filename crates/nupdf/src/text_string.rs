// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-parse.c (pdf_new_utf8_from_pdf_string),
// source/fitz/string.c (fz_runetochar) and source/fitz/encodings.h
// (fz_unicode_from_pdf_doc_encoding), Copyright (C) 2004-2025 Artifex
// Software, Inc.

//! PDF text strings (Info entries, outline titles, annotation contents) to
//! UTF-8, exactly as MuPDF converts them.
//!
//! The result is MuPDF's byte sequence, which is not always valid UTF-8: NUL
//! becomes the overlong `C0 80`, and lone surrogates are encoded as three
//! bytes. Callers that need a `String` decide how to handle that.

/// UTF-16BE, UTF-16LE (with BOMs), UTF-8 with a BOM, unmarked UTF-8 when
/// it validates, otherwise PDFDocEncoding.
pub fn utf8_from_pdf_string(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    match s {
        [0xfe, 0xff, ..] => utf16(s, &mut out, true),
        [0xff, 0xfe, ..] => utf16(s, &mut out, false),
        [0xef, 0xbb, 0xbf, ..] => {
            let mut i = 3;
            while i < s.len() {
                match skip_language_code_utf8(s, i) {
                    0 => {
                        out.push(s[i]);
                        i += 1;
                    }
                    n => i += n,
                }
            }
        }
        _ if is_valid_utf8(s) => out.extend_from_slice(s),
        _ => {
            for &b in s {
                push_rune(&mut out, u32::from(PDF_DOC_ENCODING[usize::from(b)]));
            }
        }
    }
    out
}

/// `pdf_to_text_string` as a Rust string: the C string stops at the first
/// NUL byte, the overlong `C0 80` reads as U+0000 (as MuPDF's own consumers
/// read it), and other bytes that are not valid UTF-8 become U+FFFD.
pub fn text_string(s: &[u8]) -> String {
    let bytes = utf8_from_pdf_string(s);
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let mut out = String::with_capacity(end);
    for (i, piece) in bytes[..end].split(|&b| b == 0xc0).enumerate() {
        let piece = if i == 0 {
            piece
        } else if let Some(rest) = piece.strip_prefix(&[0x80]) {
            out.push('\0');
            rest
        } else {
            out.push('\u{fffd}');
            piece
        };
        out.push_str(&String::from_utf8_lossy(piece));
    }
    out
}

fn utf16(s: &[u8], out: &mut Vec<u8>, big_endian: bool) {
    let unit = |i: usize| -> i32 {
        let (hi, lo) = if big_endian {
            (s[i], s[i + 1])
        } else {
            (s[i + 1], s[i])
        };
        i32::from(hi) << 8 | i32::from(lo)
    };
    let mut i = 2;
    while i + 2 <= s.len() {
        let skip = skip_language_code_utf16(s, i, big_endian);
        if skip > 0 {
            i += skip;
            continue;
        }
        let a = unit(i);
        // MuPDF pairs any surrogate with the next unit, valid or not.
        if (0xd800..=0xdfff).contains(&a) && i + 4 <= s.len() {
            let b = unit(i + 2);
            push_rune(out, (((a - 0xd800) << 10) + (b - 0xdc00) + 0x10000) as u32);
            i += 4;
        } else {
            push_rune(out, a as u32);
            i += 2;
        }
    }
}

fn skip_language_code_utf16(s: &[u8], i: usize, big_endian: bool) -> usize {
    let n = s.len();
    let esc = |j: usize| {
        if big_endian {
            s[j] == 0 && s[j + 1] == 27
        } else {
            s[j + 1] == 0 && s[j] == 27
        }
    };
    if i + 6 <= n && esc(i) && esc(i + 4) {
        6
    } else if i + 8 <= n && esc(i) && esc(i + 6) {
        8
    } else {
        0
    }
}

fn skip_language_code_utf8(s: &[u8], i: usize) -> usize {
    // MuPDF reads one byte past the end here, which is the NUL terminator.
    let at = |j: usize| s.get(j).copied().unwrap_or(0);
    if i + 3 <= s.len() && at(i) == 27 && at(i + 3) != 0 {
        3
    } else if i + 5 <= s.len() && at(i) == 27 && at(i + 5) == 27 {
        5
    } else {
        0
    }
}

fn is_valid_utf8(s: &[u8]) -> bool {
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        // Reserved control codes that PDFDocEncoding uses for accents.
        if (0x18..=0x1f).contains(&c) {
            return false;
        }
        let follow = match c {
            0x00..=0x7f => 0,
            0x80..=0xbf => return false,
            0xc0..=0xdf => 1,
            0xe0..=0xef => 2,
            0xf0..=0xf4 => 3,
            _ => return false,
        };
        for _ in 0..follow {
            i += 1;
            if s.get(i).is_none_or(|&b| b & 0xc0 != 0x80) {
                return false;
            }
        }
        i += 1;
    }
    true
}

/// `fz_runetochar`: NUL as overlong `C0 80`, out-of-range runes as U+FFFD,
/// surrogates encoded as ordinary three-byte sequences.
fn push_rune(out: &mut Vec<u8>, rune: u32) {
    let c = if rune > 0x10ffff { 0xfffd } else { rune };
    match c {
        0 => out.extend_from_slice(&[0xc0, 0x80]),
        0x01..=0x7f => out.push(c as u8),
        0x80..=0x7ff => out.extend_from_slice(&[0xc0 | (c >> 6) as u8, 0x80 | (c & 0x3f) as u8]),
        0x800..=0xffff => out.extend_from_slice(&[
            0xe0 | (c >> 12) as u8,
            0x80 | ((c >> 6) & 0x3f) as u8,
            0x80 | (c & 0x3f) as u8,
        ]),
        _ => out.extend_from_slice(&[
            0xf0 | (c >> 18) as u8,
            0x80 | ((c >> 12) & 0x3f) as u8,
            0x80 | ((c >> 6) & 0x3f) as u8,
            0x80 | (c & 0x3f) as u8,
        ]),
    }
}

/// `fz_unicode_from_pdf_doc_encoding`.
#[rustfmt::skip]
const PDF_DOC_ENCODING: [u16; 256] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
    0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
    0x02d8, 0x02c7, 0x02c6, 0x02d9, 0x02dd, 0x02db, 0x02da, 0x02dc,
    0x0020, 0x0021, 0x0022, 0x0023, 0x0024, 0x0025, 0x0026, 0x0027,
    0x0028, 0x0029, 0x002a, 0x002b, 0x002c, 0x002d, 0x002e, 0x002f,
    0x0030, 0x0031, 0x0032, 0x0033, 0x0034, 0x0035, 0x0036, 0x0037,
    0x0038, 0x0039, 0x003a, 0x003b, 0x003c, 0x003d, 0x003e, 0x003f,
    0x0040, 0x0041, 0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0047,
    0x0048, 0x0049, 0x004a, 0x004b, 0x004c, 0x004d, 0x004e, 0x004f,
    0x0050, 0x0051, 0x0052, 0x0053, 0x0054, 0x0055, 0x0056, 0x0057,
    0x0058, 0x0059, 0x005a, 0x005b, 0x005c, 0x005d, 0x005e, 0x005f,
    0x0060, 0x0061, 0x0062, 0x0063, 0x0064, 0x0065, 0x0066, 0x0067,
    0x0068, 0x0069, 0x006a, 0x006b, 0x006c, 0x006d, 0x006e, 0x006f,
    0x0070, 0x0071, 0x0072, 0x0073, 0x0074, 0x0075, 0x0076, 0x0077,
    0x0078, 0x0079, 0x007a, 0x007b, 0x007c, 0x007d, 0x007e, 0x0000,
    0x2022, 0x2020, 0x2021, 0x2026, 0x2014, 0x2013, 0x0192, 0x2044,
    0x2039, 0x203a, 0x2212, 0x2030, 0x201e, 0x201c, 0x201d, 0x2018,
    0x2019, 0x201a, 0x2122, 0xfb01, 0xfb02, 0x0141, 0x0152, 0x0160,
    0x0178, 0x017d, 0x0131, 0x0142, 0x0153, 0x0161, 0x017e, 0x0000,
    0x20ac, 0x00a1, 0x00a2, 0x00a3, 0x00a4, 0x00a5, 0x00a6, 0x00a7,
    0x00a8, 0x00a9, 0x00aa, 0x00ab, 0x00ac, 0x0000, 0x00ae, 0x00af,
    0x00b0, 0x00b1, 0x00b2, 0x00b3, 0x00b4, 0x00b5, 0x00b6, 0x00b7,
    0x00b8, 0x00b9, 0x00ba, 0x00bb, 0x00bc, 0x00bd, 0x00be, 0x00bf,
    0x00c0, 0x00c1, 0x00c2, 0x00c3, 0x00c4, 0x00c5, 0x00c6, 0x00c7,
    0x00c8, 0x00c9, 0x00ca, 0x00cb, 0x00cc, 0x00cd, 0x00ce, 0x00cf,
    0x00d0, 0x00d1, 0x00d2, 0x00d3, 0x00d4, 0x00d5, 0x00d6, 0x00d7,
    0x00d8, 0x00d9, 0x00da, 0x00db, 0x00dc, 0x00dd, 0x00de, 0x00df,
    0x00e0, 0x00e1, 0x00e2, 0x00e3, 0x00e4, 0x00e5, 0x00e6, 0x00e7,
    0x00e8, 0x00e9, 0x00ea, 0x00eb, 0x00ec, 0x00ed, 0x00ee, 0x00ef,
    0x00f0, 0x00f1, 0x00f2, 0x00f3, 0x00f4, 0x00f5, 0x00f6, 0x00f7,
    0x00f8, 0x00f9, 0x00fa, 0x00fb, 0x00fc, 0x00fd, 0x00fe, 0x00ff,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_with_boms() {
        assert_eq!(text_string(b"\xfe\xff\x00H\x00i"), "Hi");
        assert_eq!(text_string(b"\xff\xfeH\x00i\x00"), "Hi");
        // Surrogate pair for U+1F600.
        assert_eq!(text_string(b"\xfe\xff\xd8\x3d\xde\x00"), "\u{1F600}");
        // A trailing odd byte is ignored.
        assert_eq!(text_string(b"\xfe\xff\x00A\x00"), "A");
    }

    #[test]
    fn language_escapes_are_skipped() {
        assert_eq!(text_string(b"\xfe\xff\x00\x1benUS\x00\x1b\x00H"), "H");
    }

    #[test]
    fn utf8_with_and_without_bom() {
        assert_eq!(text_string(b"\xef\xbb\xbfcaf\xc3\xa9"), "café");
        assert_eq!(text_string("café".as_bytes()), "café");
    }

    #[test]
    fn pdf_doc_encoding_fallback() {
        // 0x80 is a bullet, 0x92 a ligature, 0xe9 é in PDFDocEncoding.
        assert_eq!(text_string(b"\x80 \x93 caf\xe9"), "\u{2022} \u{fb01} café");
        // 0x18-0x1f are accents, so such strings are not taken as UTF-8.
        assert_eq!(text_string(b"a\x18"), "a\u{02d8}");
    }

    #[test]
    fn nul_and_lone_surrogates_follow_mupdf() {
        assert_eq!(utf8_from_pdf_string(b"\xfe\xff\x00\x00"), vec![0xc0, 0x80]);
        assert_eq!(
            utf8_from_pdf_string(b"\xfe\xff\xdc\x00"),
            vec![0xed, 0xb0, 0x80]
        );
        // The C string stops at a real NUL byte; an encoded NUL survives.
        assert_eq!(text_string(b"ab\0cd"), "ab");
        assert_eq!(text_string(b"\xfe\xff\x00A\x00\x00"), "A\0");
    }
}
