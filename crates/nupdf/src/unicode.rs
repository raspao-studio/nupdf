// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/ucdn.c (UCDN, by Grigori Goronzy) and
// source/fitz/string.c (fz_toupper, fz_tolower, fz_isalpha),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! The Unicode properties MuPDF reads from its UCDN data and its case
//! tables: categories, combining classes, decomposition and composition,
//! and case.

use crate::tables;

/// `UCDN_GENERAL_CATEGORY_MN`: non-spacing marks.
pub const CATEGORY_MN: u8 = 12;

/// The value of the run `c` falls in.
fn run(table: &[(u32, u8)], c: u32) -> u8 {
    let i = table.partition_point(|&(start, _)| start <= c);
    table[i.saturating_sub(1)].1
}

/// `ucdn_get_general_category`.
pub fn general_category(c: i32) -> u8 {
    match u32::try_from(c).ok().filter(|&c| c < 0x110000) {
        Some(c) => run(&tables::GENERAL_CATEGORY, c),
        // ucdn's record 0.
        None => 2,
    }
}

/// `ucdn_get_script`: `UCDN_SCRIPT_*`.
pub fn script(c: i32) -> u8 {
    match u32::try_from(c).ok().filter(|&c| c < 0x110000) {
        Some(c) => run(&tables::SCRIPT, c),
        // ucdn's record 0.
        None => SCRIPT_UNKNOWN,
    }
}

pub const SCRIPT_COMMON: u8 = 0;
pub const SCRIPT_LATIN: u8 = 1;
pub const SCRIPT_GREEK: u8 = 2;
pub const SCRIPT_CYRILLIC: u8 = 3;
pub const SCRIPT_HANGUL: u8 = 24;
pub const SCRIPT_HIRAGANA: u8 = 32;
pub const SCRIPT_KATAKANA: u8 = 33;
pub const SCRIPT_BOPOMOFO: u8 = 34;
pub const SCRIPT_HAN: u8 = 35;
pub const SCRIPT_INHERITED: u8 = 40;
pub const SCRIPT_UNKNOWN: u8 = 102;

/// `ucdn_get_combining_class`.
pub fn combining_class(c: i32) -> u8 {
    match u32::try_from(c).ok().filter(|&c| c < 0x110000) {
        Some(c) => run(&tables::COMBINING_CLASS, c),
        None => 0,
    }
}

fn decomposition(c: i32) -> Option<(bool, &'static [u32])> {
    let c = u32::try_from(c).ok()?;
    let t = &tables::DECOMPOSITION;
    t.binary_search_by_key(&c, |&(k, ..)| k)
        .ok()
        .map(|i| (t[i].1, t[i].2))
}

const SBASE: u32 = 0xAC00;
const LBASE: u32 = 0x1100;
const VBASE: u32 = 0x1161;
const TBASE: u32 = 0x11A7;
const LCOUNT: u32 = 19;
const VCOUNT: u32 = 21;
const TCOUNT: u32 = 28;
const NCOUNT: u32 = VCOUNT * TCOUNT;
const SCOUNT: u32 = LCOUNT * NCOUNT;

/// `ucdn_decompose`: a canonical decomposition into one or two code
/// points (the second 0 for one), Hangul syllables worked out.
pub fn decompose(c: i32) -> Option<(i32, i32)> {
    if let Ok(code) = u32::try_from(c)
        && let Some(si) = code.checked_sub(SBASE).filter(|&si| si < SCOUNT)
    {
        return Some(if si % TCOUNT != 0 {
            (
                (SBASE + (si / TCOUNT) * TCOUNT) as i32,
                (TBASE + si % TCOUNT) as i32,
            )
        } else {
            (
                (LBASE + si / NCOUNT) as i32,
                (VBASE + (si % NCOUNT) / TCOUNT) as i32,
            )
        });
    }
    let (canonical, parts) = decomposition(c)?;
    if !canonical {
        return None;
    }
    let a = *parts.first()? as i32;
    Some((a, parts.get(1).map_or(0, |&b| b as i32)))
}

/// `ucdn_compat_decompose`: any decomposition, canonical or not (no
/// Hangul).
pub fn compat_decompose(c: i32) -> Option<&'static [u32]> {
    decomposition(c).map(|(_, parts)| parts)
}

/// `ucdn_compose`.
pub fn compose(a: i32, b: i32) -> Option<i32> {
    let (ua, ub) = (u32::try_from(a).ok()?, u32::try_from(b).ok()?);
    if (SBASE..SBASE + SCOUNT).contains(&ua) && (TBASE..TBASE + TCOUNT).contains(&ub) {
        return Some((ua + (ub - TBASE)) as i32);
    }
    if (LBASE..LBASE + LCOUNT).contains(&ua) && (VBASE..VBASE + VCOUNT).contains(&ub) {
        return Some((SBASE + (ua - LBASE) * NCOUNT + (ub - VBASE) * TCOUNT) as i32);
    }
    let t = &tables::COMPOSITION;
    t.binary_search_by_key(&(ua, ub), |&(x, y, _)| (x, y))
        .ok()
        .map(|i| t[i].2 as i32)
}

/// `fz_ucd_bsearch`: the record of `stride` values whose first is the last
/// one at or below `c`.
fn ucd_bsearch(c: i32, table: &[i32], stride: usize) -> Option<&[i32]> {
    let n = table.len() / stride;
    let (mut lo, mut hi) = (0usize, n);
    // The records are sorted by their first value; find the last <= c.
    while lo < hi {
        let mid = (lo + hi) / 2;
        if table[mid * stride] <= c {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let i = lo.checked_sub(1)?;
    Some(&table[i * stride..i * stride + stride])
}

/// `fz_toupper`.
pub fn to_upper(c: i32) -> i32 {
    if let Some(p) = ucd_bsearch(c, &tables::UCD_TOUPPER2, 3)
        && c >= p[0]
        && c <= p[1]
    {
        return c + p[2];
    }
    if let Some(p) = ucd_bsearch(c, &tables::UCD_TOUPPER1, 2)
        && c == p[0]
    {
        return c + p[1];
    }
    c
}

/// `fz_tolower`.
pub fn to_lower(c: i32) -> i32 {
    if c < 128 {
        return if (b'A' as i32..=b'Z' as i32).contains(&c) {
            c + 32
        } else {
            c
        };
    }
    if let Some(p) = ucd_bsearch(c, &tables::UCD_TOLOWER2, 3)
        && c >= p[0]
        && c <= p[1]
    {
        return c + p[2];
    }
    if let Some(p) = ucd_bsearch(c, &tables::UCD_TOLOWER1, 2)
        && c == p[0]
    {
        return c + p[1];
    }
    c
}

/// `fz_isalpha`.
pub fn is_alpha(c: i32) -> bool {
    if let Some(p) = ucd_bsearch(c, &tables::UCD_ALPHA2, 2)
        && c >= p[0]
        && c <= p[1]
    {
        return true;
    }
    ucd_bsearch(c, &tables::UCD_ALPHA1, 1).is_some_and(|p| c == p[0])
}

/// `fz_is_unicode_hyphen`.
pub fn is_hyphen(c: i32) -> bool {
    c == '-' as i32 || c == 0xAD || c == 0x2010 || c == 0x2011
}

/// `fz_is_unicode_space_equivalent`.
pub fn is_space_equivalent(c: i32) -> bool {
    matches!(
        c,
        0xa0 | 0x1680 | 0x2000..=0x200A | 0x202F | 0x205F | 0x3000
    )
}

/// `fz_is_unicode_whitespace`.
pub fn is_whitespace(c: i32) -> bool {
    is_space_equivalent(c) || c == 0x2028 || c == 0x2029 || matches!(c, 9..=13 | 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ucdn_decomposes_and_composes() {
        assert_eq!(decompose(0xe9), Some((0x65, 0x301)));
        assert_eq!(compose(0x65, 0x301), Some(0xe9));
        assert_eq!(decompose(0xAC01), Some((0xAC00, 0x11A8)));
        assert_eq!(compose(0xAC00, 0x11A8), Some(0xAC01));
        assert_eq!(compat_decompose(0xFB01), Some(&[0x66u32, 0x69][..]));
        // A compatibility decomposition is not canonical.
        assert_eq!(decompose(0xFB01), None);
        assert_eq!(combining_class(0x301), 230);
        assert_eq!(combining_class(0x41), 0);
    }

    #[test]
    fn case_follows_mupdf_tables() {
        assert_eq!(to_upper('a' as i32), 'A' as i32);
        assert_eq!(to_upper(0xe9), 0xc9);
        assert_eq!(to_lower(0xc9), 0xe9);
        assert_eq!(to_upper('1' as i32), '1' as i32);
        assert!(is_alpha(0xe9));
        assert!(!is_alpha('1' as i32));
    }
}
