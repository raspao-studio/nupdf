// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-xref.c (pdf_load_xref and helpers) and
// source/fitz/filter-basic.c (the endstream filter),
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! Cross-reference loading: `startxref`, classic tables, xref streams,
//! hybrid files and `/Prev` chains. Any error here sends the document to
//! repair, exactly as in MuPDF.

use std::collections::HashMap;
use std::sync::Arc;

use memchr::memmem;

use crate::diag::{Diagnostics, report};
use crate::filter;
use crate::lex::{Lexer, Token, is_white};
use crate::limits::Limits;
use crate::object::{Dict, MAX_OBJECT_NUMBER, Object, Stream};
use crate::parse::{parse_dict, parse_indirect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Listed nowhere (MuPDF's type 0).
    None,
    Free,
    /// At byte offset `ofs` (relative to the header).
    InFile,
    /// Inside object stream `ofs`, at index `generation`.
    InStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub ofs: i64,
    pub generation: i32,
    /// Stream data offset found while repairing (selects object streams).
    pub stm_ofs: i64,
}

impl Entry {
    pub const NONE: Entry = Entry {
        kind: Kind::None,
        ofs: 0,
        generation: 0,
        stm_ofs: 0,
    };

    pub fn free(generation: i32) -> Entry {
        Entry {
            kind: Kind::Free,
            generation,
            ..Entry::NONE
        }
    }
}

/// One xref section (one revision), newest first in the list.
#[derive(Debug, Default)]
pub struct Section {
    pub entries: HashMap<i32, Entry>,
    pub num_objects: i32,
    pub trailer: Option<Dict>,
    /// Objects MuPDF caches while loading (the xref streams themselves).
    pub cached: HashMap<i32, Object>,
}

impl Section {
    fn cover(&mut self, start: i32, len: i32) {
        if len > 0 {
            self.num_objects = self.num_objects.max(start + len);
        }
    }

    /// Fill an entry only if this section has not defined it yet.
    fn fill(&mut self, num: i32, entry: Entry) {
        if entry.kind != Kind::None {
            self.entries.entry(num).or_insert(entry);
        }
    }
}

#[derive(Debug)]
pub struct Loaded {
    /// Effective entries for 0..xref_len: for each number, the newest
    /// section that defines it.
    pub entries: Vec<Entry>,
    pub cached: HashMap<i32, Object>,
    pub trailer: Dict,
    pub revisions: usize,
}

pub type XrefError = String;

/// Marks an `XrefError` that is a limit, not a broken file: no repair.
pub const LIMIT_PREFIX: &str = "limit: ";

/// `pdf_load_xref`.
pub fn load(
    data: &[u8],
    bias: usize,
    diag: &Diagnostics,
    limits: &Limits,
) -> Result<Loaded, XrefError> {
    let startxref = read_start_xref(data)?;
    let mut sections: Vec<Section> = Vec::new();
    let mut seen: Vec<i64> = Vec::new();
    let mut ofs = startxref;
    while ofs != 0 {
        if seen.contains(&ofs) {
            diag.warn(
                "xref.recursion",
                format!("ignoring xref section recursion at offset {}", ofs as i32),
            );
            break;
        }
        seen.push(ofs);
        sections.push(Section::default());
        let section = sections.last_mut().ok_or("no section")?;
        ofs = read_xref_section(data, bias, ofs, section, diag, limits)?;
    }
    let newest = sections.first_mut().ok_or("cannot find startxref")?;
    let mut trailer = newest.trailer.clone().unwrap_or_default();

    let size = trailer.get(b"Size").map_or(0, Object::to_int);
    let xref_len = sections.iter().map(|s| s.num_objects).max().unwrap_or(0);
    if xref_len > size {
        if xref_len == size + 1 {
            diag.warn(
                "xref.size_off_by_one",
                "Trailer Size is off-by-one. Ignoring.",
            );
            trailer.put(b"Size", Object::Int(i64::from(size) + 1));
        } else {
            return Err("incorrect number of xref entries in trailer, repairing".into());
        }
    }
    if xref_len == 0 {
        return Err("found xref was empty".into());
    }
    if xref_len as usize > limits.max_xref_entries {
        return Err(format!("{LIMIT_PREFIX}xref has {xref_len} entries"));
    }

    // Broken files where the first object is missing or not free.
    let first = sections.iter().find_map(|s| s.entries.get(&0).copied());
    if first.is_some_and(|e| e.kind != Kind::Free) {
        diag.warn("xref.first_not_free", "first object in xref is not free");
    }

    // "0000000000 * n" means free; other out-of-range offsets fail.
    let file_size = data.len() as i64;
    for section in &mut sections {
        for (&num, entry) in section.entries.iter_mut() {
            if entry.kind == Kind::InFile {
                if entry.ofs == 0 {
                    entry.kind = Kind::Free;
                } else if entry.ofs <= 0 || entry.ofs >= file_size {
                    return Err(format!(
                        "object offset out of range: {} ({num} 0 R)",
                        entry.ofs as i32
                    ));
                }
            }
        }
    }

    let mut entries = vec![Entry::NONE; xref_len as usize];
    let mut cached = HashMap::new();
    for (num, slot) in entries.iter_mut().enumerate() {
        let num = num as i32;
        if let Some((section, entry)) = sections
            .iter()
            .find_map(|s| s.entries.get(&num).map(|e| (s, *e)))
        {
            *slot = entry;
            if let Some(obj) = section.cached.get(&num) {
                cached.insert(num, obj.clone());
            }
        }
    }
    if entries[0].kind == Kind::None {
        entries[0] = Entry::free(65535);
    }
    for section in &sections {
        for (&num, entry) in &section.entries {
            if entry.kind == Kind::InStream {
                let target = usize::try_from(entry.ofs).ok().and_then(|i| entries.get(i));
                if entry.ofs <= 0 || target.is_none_or(|t| t.kind != Kind::InFile) {
                    return Err(format!(
                        "invalid reference to an objstm that does not exist: {} ({num} 0 R)",
                        entry.ofs as i32
                    ));
                }
            }
        }
    }

    Ok(Loaded {
        entries,
        cached,
        trailer,
        revisions: sections.len(),
    })
}

/// Finds the last `startxref` in the final kilobyte.
fn read_start_xref(data: &[u8]) -> Result<i64, XrefError> {
    let tail = &data[data.len().saturating_sub(1024)..];
    if tail.len() < 9 {
        return Err("cannot find startxref".into());
    }
    let Some(at) = memmem::rfind(tail, b"startxref") else {
        return Err("cannot find startxref".into());
    };
    let mut i = at + 9;
    while tail.get(i).is_some_and(|&b| is_white(b)) {
        i += 1;
    }
    let mut value: i64 = 0;
    while let Some(&d) = tail.get(i).filter(|b| b.is_ascii_digit()) {
        if value >= i64::MAX / 10 {
            return Err("startxref too large".into());
        }
        value = value * 10 + i64::from(d - b'0');
        i += 1;
    }
    if value == 0 {
        return Err("cannot find startxref".into());
    }
    Ok(value)
}

fn read_xref_section(
    data: &[u8],
    bias: usize,
    ofs: i64,
    section: &mut Section,
    diag: &Diagnostics,
    limits: &Limits,
) -> Result<i64, XrefError> {
    let trailer = read_xref(data, bias, ofs, section, diag, limits)?;
    section.trailer = Some(trailer.clone());

    let xrefstm = trailer.get(b"XRefStm").map_or(0, Object::to_int64);
    if xrefstm != 0 {
        if xrefstm < 0 {
            return Err("negative xref stream offset".into());
        }
        // Hybrid file: the stream fills gaps; its trailer is discarded and
        // its /Prev is not followed.
        read_xref(data, bias, xrefstm, section, diag, limits)?;
    }
    match trailer.get(b"Prev") {
        Some(Object::Int(prev)) => {
            if *prev <= 0 {
                return Err("invalid offset for previous xref section".into());
            }
            Ok(*prev)
        }
        _ => Ok(0),
    }
}

fn read_xref(
    data: &[u8],
    bias: usize,
    ofs: i64,
    section: &mut Section,
    diag: &Diagnostics,
    limits: &Limits,
) -> Result<Dict, XrefError> {
    let start = usize::try_from(ofs)
        .unwrap_or(usize::MAX)
        .saturating_add(bias);
    let mut lx = Lexer::new(data, start, diag);
    while lx.peek_byte().is_some_and(is_white) {
        lx.read_byte();
    }
    match lx.peek_byte() {
        Some(b'x') => read_old_xref(&mut lx, section, diag),
        Some(b'0'..=b'9') => read_new_xref(&mut lx, bias, section, diag, limits),
        _ => Err("cannot recognize xref format".into()),
    }
}

/// `fz_skip_space`: anything at or below 0x20.
fn skip_space(lx: &mut Lexer) {
    while lx.peek_byte().is_some_and(|b| b <= 32) {
        lx.read_byte();
    }
}

fn skip_xref_keyword(lx: &mut Lexer) -> Result<(), XrefError> {
    for &b in b"xref" {
        if lx.peek_byte() != Some(b) {
            return Err("cannot find xref marker".into());
        }
        lx.read_byte();
    }
    Ok(())
}

/// `fz_read_line`: up to a CR, LF or CRLF, which is consumed.
fn read_line(lx: &mut Lexer) -> Vec<u8> {
    let mut line = Vec::new();
    while let Some(c) = lx.read_byte() {
        if c == b'\r' {
            if lx.peek_byte() == Some(b'\n') {
                lx.read_byte();
            }
            break;
        }
        if c == b'\n' {
            break;
        }
        line.push(c);
    }
    line
}

/// C `strsep(&s, " ")`.
fn strsep<'a>(s: &mut Option<&'a [u8]>) -> Option<&'a [u8]> {
    let current = (*s)?;
    match current.iter().position(|&b| b == b' ') {
        Some(i) => {
            *s = Some(&current[i + 1..]);
            Some(&current[..i])
        }
        None => {
            *s = None;
            Some(current)
        }
    }
}

/// C `atoi` through `strtol`: whitespace, sign, digits; overflow clamps to
/// `long` and then truncates to `int`, as on LP64 systems.
fn atoi(s: Option<&[u8]>) -> i32 {
    let Some(s) = s else { return 0 };
    let mut i = 0;
    while s
        .get(i)
        .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r'))
    {
        i += 1;
    }
    let mut neg = false;
    if let Some(&sign @ (b'+' | b'-')) = s.get(i) {
        neg = sign == b'-';
        i += 1;
    }
    let mut value: i128 = 0;
    while let Some(&d) = s.get(i).filter(|b| b.is_ascii_digit()) {
        value = (value * 10 + i128::from(d - b'0')).min(i128::from(i64::MAX) + 1);
        i += 1;
    }
    let value = if neg { -value } else { value };
    let clamped = value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    clamped as i32
}

/// The rest of the line after `strsep`, up to its first NUL (`strlen`).
fn c_strlen(s: &[u8]) -> usize {
    s.iter().position(|&b| b == 0).unwrap_or(s.len())
}

/// `pdf_xref_size_from_old_trailer`: a first pass to read /Size.
fn size_from_old_trailer(lx: &mut Lexer) -> Result<i32, XrefError> {
    let origin = lx.pos();
    skip_space(lx);
    skip_xref_keyword(lx)?;
    skip_space(lx);
    while lx.peek_byte().is_some_and(|b| b.is_ascii_digit()) {
        let line = read_line(lx);
        let mut s = Some(line.as_slice());
        strsep(&mut s);
        if s.is_none() {
            return Err("xref subsection length missing".into());
        }
        let len = atoi(strsep(&mut s));
        if len < 0 {
            return Err("xref subsection length must be positive".into());
        }
        if let Some(rest) = s
            && c_strlen(rest) > 0
        {
            lx.seek(lx.pos().saturating_sub(2 + c_strlen(rest)));
        }
        let t = lx.pos();
        let mut n: usize = 20;
        if len > 0 {
            let available = lx.data().len().saturating_sub(t).min(20);
            if available < 19 {
                return Err("malformed xref table".into());
            }
            n = available;
            if n == 20 && lx.data()[t + 19] > 32 {
                n = 19;
            }
        }
        lx.seek(t.saturating_add(n.saturating_mul(len as usize)));
    }
    if lx.next_token() != Token::Trailer {
        return Err("expected trailer marker".into());
    }
    if lx.next_token() != Token::OpenDict {
        return Err("expected trailer dictionary".into());
    }
    let trailer = parse_dict(lx, 1).map_err(|e| report::syntax(&e.0))?;
    let size = trailer.get(b"Size");
    if matches!(size, Some(Object::Ref(_))) {
        return Err("trailer Size entry is indirect".into());
    }
    let size = size.map_or(0, Object::to_int);
    if !(0..=MAX_OBJECT_NUMBER + 1).contains(&size) {
        return Err("trailer Size entry out of range".into());
    }
    lx.seek(origin);
    Ok(size)
}

fn validate_range(first: i32, len: i32) -> Result<(), XrefError> {
    if !(0..=MAX_OBJECT_NUMBER).contains(&first) {
        return Err("first object number in xref subsection out of range".into());
    }
    if !(0..=MAX_OBJECT_NUMBER).contains(&len) {
        return Err("number of objects in xref subsection out of range".into());
    }
    if len > 0 && len - 1 > MAX_OBJECT_NUMBER - first {
        return Err("last object number in xref subsection out of range".into());
    }
    Ok(())
}

/// `pdf_read_old_xref`: a classic `xref` table and its trailer.
fn read_old_xref(
    lx: &mut Lexer,
    section: &mut Section,
    diag: &Diagnostics,
) -> Result<Dict, XrefError> {
    let xref_len = size_from_old_trailer(lx)?;
    skip_space(lx);
    skip_xref_keyword(lx)?;
    skip_space(lx);

    while lx.peek_byte().is_some_and(|b| b.is_ascii_digit()) {
        let line = read_line(lx);
        let mut s = Some(line.as_slice());
        let start = atoi(strsep(&mut s));
        let len = atoi(strsep(&mut s));
        if let Some(rest) = s
            && c_strlen(rest) > 0
        {
            diag.warn(
                "xref.broken_subsection",
                "broken xref subsection. proceeding anyway.",
            );
            lx.seek(lx.pos().saturating_sub(2 + c_strlen(rest)));
        }
        validate_range(start, len)?;
        if i64::from(start) + i64::from(len) > i64::from(xref_len) {
            diag.warn(
                "xref.subsection_beyond_size",
                "broken xref subsection, proceeding anyway.",
            );
        }
        section.cover(start, len);

        // Entries should be 20 bytes; 19-byte ones (PCLm drivers) are
        // handled by carrying the extra byte into the next entry.
        let mut buf = [0u8; 21];
        let mut carried = 0usize;
        for i in 0..len {
            let want = 20 - carried;
            let at = lx.pos();
            let got = lx.data().len().saturating_sub(at).min(want);
            if got != want {
                return Err("unexpected EOF in xref table".into());
            }
            buf[carried..20].copy_from_slice(&lx.data()[at..at + want]);
            lx.seek(at + want);
            let num = start + i;
            if section.entries.contains_key(&num) {
                continue;
            }
            let entry = parse_old_entry(&buf[..20], num)?;
            if let Some(entry) = entry {
                section.fill(num, entry);
            }
            carried = usize::from(buf[19] > 32);
            if carried == 1 {
                buf[0] = buf[19];
            }
        }
        if carried == 1 {
            lx.seek(lx.pos().saturating_sub(1));
        }
    }

    if lx.next_token() != Token::Trailer {
        return Err("expected trailer marker".into());
    }
    if lx.next_token() != Token::OpenDict {
        return Err("expected trailer dictionary".into());
    }
    parse_dict(lx, 1).map_err(|e| report::syntax(&e.0))
}

fn parse_old_entry(buf: &[u8], num: i32) -> Result<Option<Entry>, XrefError> {
    let mut i = 0;
    let skip = |i: &mut usize| {
        while buf.get(*i).is_some_and(|&b| is_white(b)) {
            *i += 1;
        }
    };
    skip(&mut i);
    if !buf.get(i).is_some_and(u8::is_ascii_digit) {
        return Err("xref offset missing".into());
    }
    let mut ofs: i64 = 0;
    while let Some(&d) = buf.get(i).filter(|b| b.is_ascii_digit()) {
        ofs = ofs.wrapping_mul(10).wrapping_add(i64::from(d - b'0'));
        i += 1;
    }
    skip(&mut i);
    if !buf.get(i).is_some_and(u8::is_ascii_digit) {
        return Err("xref generation number missing".into());
    }
    let mut generation: i32 = 0;
    while let Some(&d) = buf.get(i).filter(|b| b.is_ascii_digit()) {
        generation = generation
            .wrapping_mul(10)
            .wrapping_add(i32::from(d - b'0'));
        i += 1;
    }
    skip(&mut i);
    let kind = match buf.get(i) {
        Some(b'f') => Kind::Free,
        Some(b'n') => Kind::InFile,
        Some(b'o') => Kind::InStream,
        other => {
            return Err(format!(
                "unexpected xref type: {:#x} ({num} {generation} R)",
                other.copied().unwrap_or(0)
            ));
        }
    };
    Ok(Some(Entry {
        kind,
        ofs,
        generation,
        stm_ofs: 0,
    }))
}

/// `pdf_read_new_xref`: a cross-reference stream.
fn read_new_xref(
    lx: &mut Lexer,
    bias: usize,
    section: &mut Section,
    diag: &Diagnostics,
    limits: &Limits,
) -> Result<Dict, XrefError> {
    let at = lx.pos();
    let ind = parse_indirect(lx).map_err(|e| report::syntax(&e.message))?;
    if ind.num == 0 {
        return Err("Trailer object number cannot be 0\n".into());
    }
    let num = ind.num;
    let mut trailer = ind.obj.as_dict().cloned().unwrap_or_default();

    let Some(size) = trailer.get(b"Size") else {
        return Err(format!("xref stream missing Size entry ({num} 0 R)"));
    };
    let size = size.to_int();
    // Some producers leave this object out of Size.
    if size == num {
        trailer.put(b"Size", Object::Int(i64::from(size) + 1));
    }
    let Some(w) = trailer.get(b"W") else {
        return Err(format!("xref stream missing W entry ({num}  R)"));
    };
    let w_items = w.as_array().unwrap_or_default();
    for (i, what) in ["type field", "field 2", "field 3"].iter().enumerate() {
        if matches!(w_items.get(i), Some(Object::Ref(_))) {
            return Err(format!(
                "xref stream object {what} width an indirect object"
            ));
        }
    }
    let width = |i: usize, what: &str| {
        let v = w_items.get(i).map_or(0, Object::to_int);
        if !(0..=8).contains(&v) {
            diag.warn(
                "xref.corrupt_width",
                format!("xref stream objects have corrupt {what}"),
            );
        }
        v.clamp(0, 8) as usize
    };
    let (w0, w1, w2) = (width(0, "type"), width(1, "offset"), width(2, "generation"));

    let Some(data_start) = ind.stream_start else {
        return Err(format!("xref stream has no data ({num} 0 R)"));
    };
    let length = trailer.get(b"Length").map_or(0, Object::to_int64);
    let raw = raw_stream(lx.data(), data_start, length, lx.data().len(), diag);
    let direct_only = |o: &Object| match o {
        Object::Ref(_) => Object::Null,
        other => other.clone(),
    };
    let decoded =
        filter::decode(&trailer, raw, &direct_only, diag, limits).map_err(|e| e.report())?;

    // A present but non-array /Index reads nothing, as in MuPDF.
    let ranges: Vec<(i32, i32)> = match trailer.get(b"Index") {
        None => vec![(0, size)],
        Some(index) => index
            .as_array()
            .unwrap_or_default()
            .chunks(2)
            .map(|pair| {
                (
                    pair.first().map_or(0, Object::to_int),
                    pair.get(1).map_or(0, Object::to_int),
                )
            })
            .collect(),
    };
    let mut cursor = 0usize;
    for (first, count) in ranges {
        validate_range(first, count)?;
        section.cover(first, count);
        for num in first..first + count {
            if cursor >= decoded.len() {
                return Err("truncated xref stream".into());
            }
            // MuPDF reads past the end as EOF (-1) inside an entry.
            let mut field = |width: usize| -> i64 {
                let mut v: i64 = 0;
                for _ in 0..width {
                    let b = decoded.get(cursor).map_or(-1, |&b| i64::from(b));
                    cursor += 1;
                    v = (v << 8).wrapping_add(b);
                }
                v
            };
            let (a, b, c) = (field(w0), field(w1), field(w2));
            let t = if w0 > 0 { a } else { 1 };
            let kind = match t {
                0 => Kind::Free,
                1 => Kind::InFile,
                2 => Kind::InStream,
                _ => Kind::None,
            };
            section.fill(
                num,
                Entry {
                    kind,
                    ofs: if w1 > 0 { b } else { 0 },
                    generation: if w2 > 0 { c as i32 } else { 0 },
                    stm_ofs: 0,
                },
            );
        }
    }

    // The stream's own entry always points at what we just parsed.
    section.entries.insert(
        num,
        Entry {
            kind: Kind::InFile,
            ofs: (at.saturating_sub(bias)) as i64,
            generation: ind.generation,
            stm_ofs: data_start as i64,
        },
    );
    section.cover(num, 1);
    section.cached.insert(
        num,
        Object::Stream(Arc::new(Stream {
            dict: trailer.clone(),
            data_start,
        })),
    );
    Ok(trailer)
}

/// The raw bytes of a stream, as MuPDF's endstream filter delivers them:
/// `/Length` bytes, then anything up to the next `endstream` (minus the EOL
/// before it), because lengths in the wild are often wrong.
///
/// `max_length` is MuPDF's notion of the file size, which stays 0 when the
/// header was never found; lengths above it are ignored.
pub fn raw_stream<'a>(
    data: &'a [u8],
    start: usize,
    length: i64,
    max_length: usize,
    diag: &Diagnostics,
) -> &'a [u8] {
    let length = if length < 0 || length > max_length as i64 {
        0
    } else {
        length as usize
    };
    let start = start.min(data.len());
    let end = start.saturating_add(length).min(data.len());
    let stop = match memmem::find(&data[end..], b"endstream") {
        Some(p) => {
            let mut stop = end + p;
            if stop > end && data[stop - 1] == b'\n' {
                stop -= 1;
            }
            if stop > end && data[stop - 1] == b'\r' {
                stop -= 1;
            }
            stop
        }
        None => data.len(),
    };
    if stop > end {
        diag.warn("stream.length_incorrect", "PDF stream Length incorrect");
    }
    &data[start..stop]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atoi_matches_c() {
        assert_eq!(atoi(Some(b"12")), 12);
        assert_eq!(atoi(Some(b"  -7x")), -7);
        assert_eq!(atoi(Some(b"")), 0);
        assert_eq!(atoi(None), 0);
        // strtol clamps to LONG_MAX, which truncates to -1 as an int.
        assert_eq!(atoi(Some(b"99999999999999999999999")), -1);
    }

    #[test]
    fn strsep_splits_on_single_spaces() {
        let mut s = Some(b"0  6".as_slice());
        assert_eq!(strsep(&mut s), Some(b"0".as_slice()));
        assert_eq!(strsep(&mut s), Some(b"".as_slice()));
        assert_eq!(s, Some(b"6".as_slice()));
    }

    #[test]
    fn start_xref_is_the_last_one() {
        assert_eq!(
            read_start_xref(b"startxref\n10\n%%EOF startxref 20\n%%EOF"),
            Ok(20)
        );
        assert!(read_start_xref(b"startxref 0 %%EOF").is_err());
        assert!(read_start_xref(b"short").is_err());
    }

    #[test]
    fn raw_stream_trusts_length_then_scans() {
        let diag = Diagnostics::new();
        let data = b"stream\nABCDE\nendstream";
        let all = data.len();
        assert_eq!(raw_stream(data, 7, 5, all, &diag), b"ABCDE");
        assert!(diag.snapshot().is_empty());
        // Too short: the rest up to endstream is included, with a warning.
        assert_eq!(raw_stream(data, 7, 3, all, &diag), b"ABCDE");
        assert!(diag.has("stream.length_incorrect"));
        // Missing or absurd lengths scan from the start.
        assert_eq!(raw_stream(data, 7, -1, all, &Diagnostics::new()), b"ABCDE");
        assert_eq!(
            raw_stream(b"stream\r\nAB\r\nendstream", 8, 0, 100, &Diagnostics::new()),
            b"AB"
        );
        // Lengths above MuPDF's file size (0 without a header) are ignored.
        assert_eq!(raw_stream(data, 7, 6, 0, &Diagnostics::new()), b"ABCDE");
        // No endstream at all: everything to the end of the file.
        assert_eq!(
            raw_stream(b"stream\nABC", 7, 1, 100, &Diagnostics::new()),
            b"ABC"
        );
    }

    #[test]
    fn old_entries_parse_and_reject_junk() {
        assert_eq!(
            parse_old_entry(b"0000000017 00000 n \n", 1).unwrap(),
            Some(Entry {
                kind: Kind::InFile,
                ofs: 17,
                generation: 0,
                stm_ofs: 0
            })
        );
        assert!(parse_old_entry(b"0000000017 00000 x \n", 1).is_err());
        assert!(parse_old_entry(b"          00000 n \n ", 1).is_err());
    }
}
