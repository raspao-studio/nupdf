// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/stext-search.c (the search API, its text
// transforms and finders) and source/fitz/util.c
// (fz_new_buffer_from_flattened_stext_page), Copyright (C) 2004-2026
// Artifex Software, Inc.

//! Search in structured text, as `fz_match_stext_page_cb` searches: the
//! page's text flattened into one string, both it and the needle
//! transformed (spaces folded, compatibility forms decomposed, hyphens
//! normalized, and for the options case or diacritics dropped), the needle
//! found literally or as a regular expression, and each hit given as the
//! quads of its characters, merged where they touch.

use crate::geometry::{Point, Quad};
use crate::regexp::{self, Regex};
use crate::stext::{Block, LINE_JOINED, StextPage, chartorune};
use crate::unicode;

/// `fz_search_options`.
pub mod options {
    pub const EXACT: u32 = 0;
    pub const IGNORE_CASE: u32 = 1;
    pub const IGNORE_DIACRITICS: u32 = 2;
    pub const REGEXP: u32 = 4;
    pub const KEEP_LINES: u32 = 8;
    pub const KEEP_PARAGRAPHS: u32 = 16;
    pub const KEEP_HYPHENS: u32 = 32;
}

/// `fz_parse_search_options`: MuPDF's option names, comma-separated.
pub fn parse_options(s: &str) -> Result<u32, String> {
    let mut mask = 0;
    for item in s.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let (key, val) = item.split_once('=').unwrap_or((item, "yes"));
        let on = match val {
            "yes" | "true" | "1" => true,
            "no" | "false" | "0" => false,
            _ => return Err(format!("bad value for {key}: {val}")),
        };
        let flag = match key {
            "exact" => options::EXACT,
            "ignore-case" => options::IGNORE_CASE,
            "ignore-diacritics" => options::IGNORE_DIACRITICS,
            "regexp" => options::REGEXP,
            "keep-lines" => options::KEEP_LINES,
            "keep-paragraphs" => options::KEEP_PARAGRAPHS,
            "keep-hyphens" => options::KEEP_HYPHENS,
            _ => return Err(format!("unknown search option: {key}")),
        };
        if on {
            mask |= flag;
        }
    }
    Ok(mask)
}

/// Where a character of the page is: block, line and character indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub block: usize,
    pub line: usize,
    pub ch: usize,
}

/// A hit: its first and last characters, and the quads that cover it.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub begin: Position,
    pub end: Position,
    pub quads: Vec<Quad>,
}

// fz_text_transform.
const T_COMPOSE: u32 = 1;
const T_UPPERCASE: u32 = 2;
const T_FULLWIDTH_ASCII: u32 = 4;
const T_STRIP_MARKING_NONSPACING: u32 = 8;
const T_DECOMPOSE: u32 = 16;
const T_COMPATIBILITY_DECOMPOSE: u32 = 32;
const T_NORMALIZE_HYPHENS: u32 = 64;
const T_KEEP_LINES: u32 = 256;
const T_KEEP_PARAGRAPHS: u32 = 512;
const T_KEEP_HYPHENS: u32 = 1024;
const T_NORMAL: u32 =
    T_COMPATIBILITY_DECOMPOSE | T_NORMALIZE_HYPHENS | T_FULLWIDTH_ASCII | T_COMPOSE;

/// `fz_runetochar`, appended.
fn push_rune(out: &mut Vec<u8>, c: i32) {
    if c == 0 {
        out.extend([0xc0, 0x80]);
        return;
    }
    let c = match u32::try_from(c) {
        Ok(c) if c <= 0x10ffff => c,
        _ => 0xfffd,
    };
    // Surrogates are written as they are, as fz_runetochar does.
    if c < 0x80 {
        out.push(c as u8);
    } else if c < 0x800 {
        out.extend([0xc0 | (c >> 6) as u8, 0x80 | (c & 0x3f) as u8]);
    } else if c < 0x10000 {
        out.extend([
            0xe0 | (c >> 12) as u8,
            0x80 | ((c >> 6) & 0x3f) as u8,
            0x80 | (c & 0x3f) as u8,
        ]);
    } else {
        out.extend([
            0xf0 | (c >> 18) as u8,
            0x80 | ((c >> 12) & 0x3f) as u8,
            0x80 | ((c >> 6) & 0x3f) as u8,
            0x80 | (c & 0x3f) as u8,
        ]);
    }
}

/// `do_flatten` with every flag MuPDF's search passes: the page's text as
/// one string, and for each character of it where it came from (None for
/// spaces and line breaks put in).
fn flatten(page: &StextPage, transform: u32) -> (Vec<u8>, Vec<Option<Position>>) {
    let (mut buf, mut map) = (Vec::new(), Vec::new());
    let mut ws = false;
    let emit = |buf: &mut Vec<u8>, map: &mut Vec<Option<Position>>, pos, c| {
        map.push(pos);
        push_rune(buf, c);
    };
    let inhibit = |c: i32| unicode::is_whitespace(c) || unicode::is_hyphen(c);
    for (bi, block) in page.blocks.iter().enumerate() {
        let Block::Text(t) = block else { continue };
        let mut join_line = false;
        for (li, line) in t.lines.iter().enumerate() {
            join_line = false;
            let n = line.chars.len();
            for (ci, ch) in line.chars.iter().enumerate() {
                if transform & T_KEEP_HYPHENS == 0 {
                    if ci + 1 == n {
                        if ch.c == 0xad || line.flags & LINE_JOINED != 0 {
                            join_line = true;
                            continue;
                        }
                    } else if ch.c == 0xad {
                        continue;
                    }
                }
                emit(
                    &mut buf,
                    &mut map,
                    Some(Position {
                        block: bi,
                        line: li,
                        ch: ci,
                    }),
                    ch.c,
                );
                ws = inhibit(ch.c);
            }
            if join_line {
            } else if transform & T_KEEP_LINES != 0 {
                emit(&mut buf, &mut map, None, '\n' as i32);
                ws = true;
            } else if li + 1 < t.lines.len() {
                if !ws {
                    emit(&mut buf, &mut map, None, ' ' as i32);
                }
                ws = true;
            }
        }
        if join_line {
        } else if transform & T_KEEP_PARAGRAPHS != 0 {
            emit(&mut buf, &mut map, None, '\n' as i32);
            ws = true;
        } else {
            if !ws {
                emit(&mut buf, &mut map, None, ' ' as i32);
            }
            ws = true;
        }
    }
    (buf, map)
}

/// `canon`.
fn canon(c: i32) -> i32 {
    if c == '\r' as i32 || c == '\n' as i32 || c == '\t' as i32 || unicode::is_space_equivalent(c) {
        ' ' as i32
    } else {
        c
    }
}

/// `transform_cache`.
struct Cache {
    chars: Vec<(i32, usize)>,
}

/// The transformed text and, for each byte of it, the byte of the input
/// it came from (one more entry at the end, for the input's end).
struct Transformed {
    out: Vec<u8>,
    index: Vec<usize>,
}

impl Transformed {
    fn push(&mut self, c: i32, pos: usize) {
        let before = self.out.len();
        push_rune(&mut self.out, c);
        for _ in before..self.out.len() {
            self.index.push(pos);
        }
    }
}

/// `flush_transform_cache`.
fn flush(t: &mut Transformed, transform: u32, cache: &mut Cache) {
    let n = cache.chars.len();
    if n == 0 {
        return;
    }
    let chars = &mut cache.chars;
    if transform & (T_COMPATIBILITY_DECOMPOSE | T_COMPOSE) != 0 {
        // The bubble sort, which leaves the positions where they were.
        for i in 0..n.saturating_sub(1) {
            let mut i_ccc = unicode::combining_class(chars[i].0);
            for j in i + 1..n {
                let j_ccc = unicode::combining_class(chars[j].0);
                if i_ccc > j_ccc {
                    let tmp = chars[j].0;
                    chars[j].0 = chars[i].0;
                    chars[i].0 = tmp;
                    i_ccc = j_ccc;
                }
            }
        }
    }
    let mut n = n;
    if transform & T_COMPOSE != 0 {
        let mut i = 0;
        for j in 1..n {
            match unicode::compose(chars[i].0, chars[j].0).filter(|&a| a != 0) {
                Some(a) => chars[i].0 = a,
                None => {
                    i += 1;
                    chars[i] = chars[j];
                }
            }
        }
        n = i + 1;
    }
    for &(c, pos) in &chars[..n] {
        t.push(c, pos);
    }
}

/// `transform_char`.
fn transform_char(t: &mut Transformed, c: i32, pos: usize, transform: u32, cache: &mut Cache) {
    let mut transform = transform;
    if transform & T_DECOMPOSE != 0 {
        if let Some((a, b)) = unicode::decompose(c) {
            transform_char(t, a, pos, transform, cache);
            transform_char(t, b, pos, transform, cache);
            return;
        }
        transform &= !T_DECOMPOSE;
    }
    if transform & T_COMPATIBILITY_DECOMPOSE != 0 {
        if let Some(parts) = unicode::compat_decompose(c).filter(|p| !p.is_empty()) {
            for &p in parts {
                transform_char(t, p as i32, pos, transform, cache);
            }
            return;
        }
        transform &= !T_COMPATIBILITY_DECOMPOSE;
    }
    if transform & T_STRIP_MARKING_NONSPACING != 0
        && unicode::general_category(c) == unicode::CATEGORY_MN
    {
        return;
    }
    let mut c = c;
    if transform & T_FULLWIDTH_ASCII != 0 && (0xFF01..=0xFF5E).contains(&c) {
        c = c - 0xFF01 + 0x21;
    }
    if transform & T_NORMALIZE_HYPHENS != 0 && unicode::is_hyphen(c) {
        c = '-' as i32;
    }
    if transform & T_UPPERCASE != 0 {
        c = unicode::to_upper(c);
    }
    if !(c == '\n' as i32 && transform & (T_KEEP_LINES | T_KEEP_PARAGRAPHS) != 0) {
        c = canon(c);
    }
    if c == ' ' as i32 && cache.chars.last().is_some_and(|&(l, _)| l == ' ' as i32) {
        return;
    }
    let ccc = if transform & (T_DECOMPOSE | T_COMPATIBILITY_DECOMPOSE | T_COMPOSE) != 0 {
        unicode::combining_class(c)
    } else {
        0
    };
    if cache.chars.is_empty() {
        cache.chars.push((c, pos));
        return;
    }
    if ccc > 0 {
        // MuPDF drops a character past a full cache, with a warning.
        if cache.chars.len() < 32 {
            cache.chars.push((c, pos));
        }
        return;
    }
    flush(t, transform, cache);
    cache.chars.clear();
    cache.chars.push((c, pos));
}

/// `do_transform`: to the first NUL of `input`.
fn transform_text(transform: u32, input: &[u8]) -> Transformed {
    let mut t = Transformed {
        out: Vec::new(),
        index: Vec::new(),
    };
    let mut cache = Cache { chars: Vec::new() };
    let mut pos = 0;
    loop {
        let (c, len) = chartorune(input.get(pos..).unwrap_or_default());
        if c == 0 {
            break;
        }
        transform_char(&mut t, c, pos, transform, &mut cache);
        pos += len;
    }
    flush(&mut t, transform, &mut cache);
    t.index.push(pos);
    t
}

/// `init_transform_and_finder`: the transform, and whether a regular
/// expression is to ignore case.
fn transform_for(opts: u32) -> (u32, bool) {
    let mut trans = 0;
    if opts & options::KEEP_LINES != 0 {
        trans |= T_KEEP_LINES;
    }
    if opts & options::KEEP_PARAGRAPHS != 0 {
        trans |= T_KEEP_PARAGRAPHS;
    }
    if opts & options::KEEP_HYPHENS != 0 {
        trans |= T_KEEP_HYPHENS;
    }
    let mut opts = opts;
    let mut icase = false;
    if opts & options::REGEXP != 0 && opts & options::IGNORE_CASE != 0 {
        icase = true;
        opts &= !options::IGNORE_CASE;
    }
    trans |= match opts & (options::IGNORE_CASE | options::IGNORE_DIACRITICS) {
        options::IGNORE_CASE => T_NORMAL | T_UPPERCASE,
        options::IGNORE_DIACRITICS => T_NORMAL | T_STRIP_MARKING_NONSPACING,
        3 => T_NORMAL | T_UPPERCASE | T_STRIP_MARKING_NONSPACING,
        _ => T_NORMAL,
    };
    (trans, icase)
}

/// A byte of a C string: NUL past its end.
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// `advance_one_utf8`.
fn advance(s: &[u8], p: usize) -> Option<usize> {
    if at(s, p) == 0 {
        return None;
    }
    let mut p = p + 1;
    while at(s, p) & 0x80 != 0 && at(s, p) & 0xc0 != 0xc0 && at(s, p) != 0 {
        p += 1;
    }
    Some(p)
}

/// `retreat_one_utf8`.
fn retreat(s: &[u8], p: usize) -> Option<usize> {
    if p == 0 {
        return None;
    }
    let mut p = p - 1;
    while at(s, p) & 0x80 != 0 && at(s, p) & 0xc0 != 0xc0 && p > 0 {
        p -= 1;
    }
    Some(p)
}

/// `is_marking_nonspacing`.
fn marking_nonspacing(s: &[u8], h: usize) -> bool {
    at(s, h) >= 0x80
        && unicode::general_category(chartorune(s.get(h..).unwrap_or_default()).0)
            == unicode::CATEGORY_MN
}

/// `match_exact`: where the needle's match at `h` ends.
fn match_exact(s: &[u8], h: usize, needle: &[u8]) -> Option<usize> {
    let (mut h, mut n) = (h, 0usize);
    loop {
        let (nc, nl) = chartorune(needle.get(n..).unwrap_or_default());
        n += nl;
        if nc == 0 {
            return (!marking_nonspacing(s, h)).then_some(h);
        }
        let (hc, hl) = chartorune(s.get(h..).unwrap_or_default());
        h += hl;
        if hc != nc {
            return None;
        }
    }
}

enum Finder {
    Exact(Vec<u8>),
    Regex(Regex),
}

impl Finder {
    /// `find_exact` and `find_regexp`: the first match from `p`, as its
    /// start and end.
    fn find(&self, s: &[u8], mut p: usize) -> Result<Option<(usize, usize)>, String> {
        match self {
            Finder::Exact(needle) => loop {
                if let Some(end) = match_exact(s, p, needle) {
                    return Ok(Some((p, end)));
                }
                match advance(s, p) {
                    Some(q) => p = q,
                    None => return Ok(None),
                }
            },
            Finder::Regex(prog) => loop {
                let bol = p == 0 || at(s, p - 1) == b'\n';
                let flags = if bol { 0 } else { regexp::NOTBOL } | regexp::RUNAWAY;
                match prog.exec(s, p, flags)? {
                    None => return Ok(None),
                    Some((sp, ep)) if ep > sp && !marking_nonspacing(s, ep) => {
                        return Ok(Some((sp, ep)));
                    }
                    Some(_) => {}
                }
                match advance(s, p) {
                    Some(q) => p = q,
                    None => return Ok(None),
                }
            },
        }
    }
}

/// `hdist` (as MuPDF writes it).
fn hdist(dir: Point, a: Point, b: Point) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    (dx * dir.x - dy * dir.y).abs()
}

/// `vdist`.
fn vdist(dir: Point, a: Point, b: Point) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    (dx * dir.y - dy * dir.x).abs()
}

/// `fz_match_stext_page_cb`: every hit of `needle` on the page.
pub fn search_page(page: &StextPage, needle: &str, opts: u32) -> Result<Vec<Hit>, String> {
    let (transform, icase) = transform_for(opts);
    let needle_bytes = needle.as_bytes();
    if needle_bytes.first().is_none_or(|&b| b == 0) {
        return Ok(Vec::new());
    }
    let spun_needle = transform_text(transform, needle_bytes).out;
    let finder = if opts & options::REGEXP != 0 {
        let mut flags = regexp::NEWLINE;
        if icase {
            flags |= regexp::ICASE;
        }
        Finder::Regex(Regex::compile(&spun_needle, flags)?)
    } else {
        // An empty needle would match everywhere, forever, in MuPDF.
        if spun_needle.is_empty() {
            return Ok(Vec::new());
        }
        Finder::Exact(spun_needle)
    };
    let (haystack, map) = flatten(page, transform);
    if haystack.is_empty() {
        return Ok(Vec::new());
    }
    let spun = transform_text(transform, &haystack);
    // The rune index of each byte of the haystack.
    let mut rune_at = vec![0usize; haystack.len() + 1];
    let (mut i, mut r) = (0, 0);
    while i < haystack.len() {
        let len = chartorune(&haystack[i..]).1.max(1);
        for slot in &mut rune_at[i..(i + len).min(haystack.len())] {
            *slot = r;
        }
        i += len;
        r += 1;
    }
    rune_at[haystack.len()] = r;
    let lookup = |p: usize| -> Option<Position> {
        let unspun = *spun.index.get(p)?;
        map.get(*rune_at.get(unspun)?).copied().flatten()
    };
    let chars = |pos: Position| match &page.blocks[pos.block] {
        Block::Text(t) => Some((&t.lines[pos.line], &t.lines[pos.line].chars[pos.ch])),
        Block::Image(_) | Block::Vector(_) | Block::Struct(_) => None,
    };
    let s = &spun.out;
    let mut hits = Vec::new();
    let mut pos = 0usize;
    while pos < s.len() {
        let Some((begin, end)) = finder.find(s, pos)? else {
            break;
        };
        pos = end;
        let Some(mut last) = retreat(s, end) else {
            continue;
        };
        let mut first = begin;
        while first < last && lookup(first).is_none() {
            first = advance(s, first).unwrap_or(last);
        }
        while last > first && lookup(last).is_none() {
            last = retreat(s, last).unwrap_or(first);
        }
        let (Some(b), Some(mut e)) = (lookup(first), lookup(last)) else {
            continue;
        };
        // lookup_stext_position_end: the marks after the last character.
        if let Some((line, _)) = chars(e) {
            while e.ch + 1 < line.chars.len()
                && unicode::general_category(line.chars[e.ch + 1].c) == unicode::CATEGORY_MN
            {
                e.ch += 1;
            }
        }
        let mut quads: Vec<Quad> = Vec::new();
        let mut p = first;
        while p <= last {
            if let Some(at) = lookup(p)
                && let Some((line, ch)) = chars(at)
            {
                let (h, v) = (ch.size * 0.5, ch.size * 0.1);
                let dir = line.dir;
                match quads.last_mut() {
                    Some(q)
                        if hdist(dir, q.lr, ch.quad.ll) < h
                            && vdist(dir, q.lr, ch.quad.ll) < v
                            && hdist(dir, q.ur, ch.quad.ul) < h
                            && vdist(dir, q.ur, ch.quad.ul) < v =>
                    {
                        q.ur = ch.quad.ur;
                        q.lr = ch.quad.lr;
                    }
                    _ => quads.push(ch.quad),
                }
            }
            match advance(s, p) {
                Some(q) => p = q,
                None => break,
            }
        }
        if !quads.is_empty() {
            hits.push(Hit {
                begin: b,
                end: e,
                quads,
            });
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms_fold_case_marks_and_spaces() {
        let (t, _) = transform_for(options::IGNORE_CASE | options::IGNORE_DIACRITICS);
        let out = transform_text(t, "Caf\u{e9}  au\u{a0}lait".as_bytes()).out;
        assert_eq!(String::from_utf8_lossy(&out), "CAFE AU LAIT");
        let (t, _) = transform_for(options::EXACT);
        // Composed and decomposed forms read the same.
        assert_eq!(
            transform_text(t, "e\u{301}".as_bytes()).out,
            transform_text(t, "\u{e9}".as_bytes()).out
        );
        assert_eq!(transform_text(t, "\u{fb01}".as_bytes()).out, b"fi");
    }

    #[test]
    fn options_parse_like_mupdf() {
        assert_eq!(
            parse_options("ignore-case,regexp"),
            Ok(options::IGNORE_CASE | options::REGEXP)
        );
        assert!(parse_options("fuzzy").is_err());
    }
}
