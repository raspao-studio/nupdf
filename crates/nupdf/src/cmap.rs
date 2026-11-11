// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-cmap.c, source/pdf/pdf-cmap-parse.c and
// source/pdf/pdf-cmap-load.c, Copyright (C) 2004-2026 Artifex Software, Inc.

//! CMaps: code spaces that split a string into codes, and ranges that map
//! codes to CIDs (an encoding) or to Unicode (a ToUnicode map).
//!
//! Ranges are added as MuPDF adds them: a later range replaces what it
//! overlaps, and one that continues a neighbour with the same offset is
//! merged into it. The predefined CMaps are MuPDF's, generated into
//! `data/cmaps.bin` by `cargo xtask tables`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

use crate::diag::report;
use crate::document::Document;
use crate::lex::{Lexer, Token};
use crate::object::Object;

/// `PDF_MRANGE_CAP`: the longest one-to-many mapping.
pub const MRANGE_CAP: usize = 32;
/// `CMAP_TABLE_LIMIT`.
const TABLE_LIMIT: usize = 65536;
/// `nelem(cmap->codespace)`.
const MAX_CODESPACES: usize = 40;
/// Chains of UseCMap deeper than this are treated as cycles.
pub(crate) const MAX_USECMAP: usize = 32;

/// One mapped range: `low..=high` to `out + (code - low)`, or, for a
/// one-to-many mapping (`many`, a single code), to the values at `out`
/// in the CMap's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    low: u32,
    high: u32,
    out: u32,
    many: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codespace {
    pub n: usize,
    pub low: u32,
    pub high: u32,
}

/// `pdf_cmap`.
#[derive(Debug, Clone, Default)]
pub struct Cmap {
    pub name: String,
    pub usecmap_name: String,
    pub usecmap: Option<Arc<Cmap>>,
    pub wmode: i32,
    pub codespaces: Vec<Codespace>,
    /// By `low`, never overlapping.
    entries: BTreeMap<u32, Entry>,
    /// One-to-many values: a length, then that many values.
    table: Vec<i32>,
}

impl Cmap {
    pub fn new() -> Cmap {
        Cmap::default()
    }

    /// `pdf_new_identity_cmap`.
    pub fn identity(wmode: i32, bytes: usize) -> Cmap {
        let mut cmap = Cmap::new();
        let high = ((1u64 << (bytes * 8)) - 1) as u32;
        cmap.name = if wmode != 0 {
            "Identity-V"
        } else {
            "Identity-H"
        }
        .into();
        cmap.add_codespace(0, high, bytes, &mut |_| {});
        let _ = cmap.map_range(0, high, 0, &mut |_| {});
        cmap.wmode = wmode;
        cmap
    }

    /// `pdf_set_usecmap`.
    pub fn set_usecmap(&mut self, usecmap: Arc<Cmap>) {
        if self.codespaces.is_empty() {
            self.codespaces = usecmap.codespaces.clone();
        }
        self.usecmap = Some(usecmap);
    }

    /// `pdf_add_codespace`.
    pub fn add_codespace(&mut self, low: u32, high: u32, n: usize, warn: &mut dyn FnMut(&str)) {
        if self.codespaces.len() + 1 == MAX_CODESPACES {
            warn("assert: too many code space ranges");
            return;
        }
        self.codespaces.push(Codespace { n, low, high });
    }

    /// The entries counted as MuPDF sorts them: 16-bit ranges, wider
    /// ranges, one-to-many mappings.
    fn counts(&self) -> (usize, usize, usize) {
        let mut c = (0, 0, 0);
        for e in self.entries.values() {
            if e.many {
                c.2 += 1;
            } else if e.low <= 0xffff && e.high <= 0xffff && e.out <= 0xffff {
                c.0 += 1;
            } else {
                c.1 += 1;
            }
        }
        c
    }

    /// `add_range`: a later range replaces what it overlaps, and joins a
    /// neighbour it continues.
    fn add_range(
        &mut self,
        low: u32,
        high: u32,
        out: u32,
        many: bool,
        warn: &mut dyn FnMut(&str),
    ) -> Result<(), String> {
        if low > high {
            warn(&format!("range limits out of range in cmap {}", self.name));
            return Ok(());
        }
        if self.codespaces.is_empty() {
            warn("CMap is missing codespace range");
            self.add_codespace(0, 65535, 2, warn);
        }
        // Take away what overlaps.
        let overlapping: Vec<Entry> = self
            .entries
            .range(..=high)
            .rev()
            .take_while(|(_, e)| e.high >= low)
            .map(|(_, e)| *e)
            .collect();
        for e in overlapping {
            self.entries.remove(&e.low);
            if e.many {
                continue;
            }
            if e.low < low {
                self.entries.insert(e.low, Entry { high: low - 1, ..e });
            }
            if e.high > high {
                let start = high + 1;
                self.entries.insert(
                    start,
                    Entry {
                        low: start,
                        high: e.high,
                        out: e.out.wrapping_add(start - e.low),
                        many: false,
                    },
                );
            }
        }
        if !many {
            let delta = out.wrapping_sub(low);
            let before = self
                .entries
                .range(..low)
                .next_back()
                .map(|(_, e)| *e)
                .filter(|e| {
                    !e.many && e.high.wrapping_add(1) == low && e.out.wrapping_sub(e.low) == delta
                });
            let after = self
                .entries
                .range(high.saturating_add(1)..)
                .next()
                .map(|(_, e)| *e)
                .filter(|e| {
                    !e.many
                        && high.checked_add(1) == Some(e.low)
                        && e.out.wrapping_sub(e.low) == delta
                });
            if let Some(b) = before {
                let mut merged = Entry { high, ..b };
                if let Some(a) = after {
                    merged.high = a.high;
                    self.entries.remove(&a.low);
                }
                self.entries.insert(b.low, merged);
                return Ok(());
            }
            if let Some(a) = after {
                self.entries.remove(&a.low);
                self.entries.insert(low, Entry { low, out, ..a });
                return Ok(());
            }
        }
        if self.entries.len() >= TABLE_LIMIT {
            return Err("limit error: too many entries in CMap table".into());
        }
        self.entries.insert(
            low,
            Entry {
                low,
                high,
                out,
                many,
            },
        );
        Ok(())
    }

    /// `pdf_map_range_to_range`.
    pub fn map_range(
        &mut self,
        low: u32,
        high: u32,
        out: i32,
        warn: &mut dyn FnMut(&str),
    ) -> Result<(), String> {
        self.add_range(low, high, out as u32, false, warn)
    }

    /// `pdf_map_one_to_many`: surrogate pairs are joined first.
    pub fn map_one_to_many(
        &mut self,
        low: u32,
        values: &[i32],
        warn: &mut dyn FnMut(&str),
    ) -> Result<(), String> {
        let mut joined = Vec::with_capacity(values.len());
        if values.len() >= 2 {
            let mut i = 0;
            while i < values.len() {
                let mut hi = values[i];
                if (0xd800..0xdc00).contains(&hi) && i + 1 < values.len() {
                    let lo = values[i + 1];
                    if (0xdc00..0xe000).contains(&lo) {
                        hi = ((hi - 0xd800) << 10) + (lo - 0xdc00) + 0x10000;
                        i += 1;
                    }
                }
                joined.push(hi);
                i += 1;
            }
        } else {
            joined.extend_from_slice(values);
        }
        if joined.len() == 1 {
            return self.add_range(low, low, joined[0] as u32, false, warn);
        }
        if joined.len() > MRANGE_CAP {
            warn(&format!(
                "ignoring one to many mapping in cmap {}",
                self.name
            ));
            return Ok(());
        }
        let pos = self.table.len();
        if pos + joined.len() + 1 > TABLE_LIMIT {
            return Err("limit error: too many entries in CMap table".into());
        }
        self.table.push(joined.len() as i32);
        self.table.extend_from_slice(&joined);
        self.add_range(low, low, pos as u32, true, warn)
    }

    fn find(&self, cpt: u32) -> Option<&Entry> {
        self.entries
            .range(..=cpt)
            .next_back()
            .map(|(_, e)| e)
            .filter(|e| cpt <= e.high)
    }

    /// `pdf_lookup_cmap`: -1 when unmapped (one-to-many mappings count
    /// as unmapped).
    pub fn lookup(&self, cpt: u32) -> i32 {
        let mut cmap = self;
        for _ in 0..MAX_USECMAP {
            if let Some(e) = cmap.find(cpt).filter(|e| !e.many) {
                return cpt.wrapping_sub(e.low).wrapping_add(e.out) as i32;
            }
            match &cmap.usecmap {
                Some(u) => cmap = u,
                None => return -1,
            }
        }
        -1
    }

    /// `pdf_lookup_cmap_full`: the values a code maps to, none when
    /// unmapped.
    pub fn lookup_full(&self, cpt: u32) -> Vec<i32> {
        let mut out = Vec::new();
        self.lookup_full_into(cpt, &mut out);
        out
    }

    /// [`Cmap::lookup_full`] into `out`, which is cleared first: text runs
    /// look up every glyph, and a buffer kept across them saves an
    /// allocation each.
    pub fn lookup_full_into(&self, cpt: u32, out: &mut Vec<i32>) {
        out.clear();
        let mut cmap = self;
        for _ in 0..MAX_USECMAP {
            if let Some(e) = cmap.find(cpt) {
                if !e.many {
                    out.push(cpt.wrapping_sub(e.low).wrapping_add(e.out) as i32);
                    return;
                }
                let at = e.out as usize;
                let len = cmap.table.get(at).copied().unwrap_or(0).max(0) as usize;
                if let Some(values) = cmap.table.get(at + 1..at + 1 + len) {
                    out.extend_from_slice(values);
                }
                return;
            }
            match &cmap.usecmap {
                Some(u) => cmap = u,
                None => return,
            }
        }
    }

    /// `pdf_decode_cmap`: the code at the start of `s` and its length in
    /// bytes; a byte that starts no code reads as code 0.
    pub fn decode(&self, s: &[u8]) -> (u32, usize) {
        let mut c: u32 = 0;
        for (n, &b) in s.iter().take(4).enumerate() {
            c = (c << 8) | u32::from(b);
            for cs in &self.codespaces {
                if cs.n == n + 1 && c >= cs.low && c <= cs.high {
                    return (c, n + 1);
                }
            }
        }
        (0, 1)
    }

    /// The ranges, in order, as (low, high, out) for one-to-one ones;
    /// those that fit 16 bits first, then the wider ones, as MuPDF keeps
    /// them after sorting.
    pub fn sorted_ranges(&self) -> Vec<(u32, u32, u32)> {
        let small = |e: &Entry| e.low <= 0xffff && e.high <= 0xffff && e.out <= 0xffff;
        let narrow = self.entries.values().filter(|e| !e.many && small(e));
        let wide = self.entries.values().filter(|e| !e.many && !small(e));
        narrow.chain(wide).map(|e| (e.low, e.high, e.out)).collect()
    }

    /// `is_effectively_identity`: a single 16-bit range of all codes to
    /// themselves, with no one-to-many mappings here or in `b`.
    pub fn is_effectively_identity(&self, b: &Cmap) -> bool {
        let mut a = self;
        for _ in 0..MAX_USECMAP {
            let (r, _, m) = a.counts();
            if m > 0 || b.counts().2 > 0 {
                return false;
            }
            if r == 1 {
                let first = a.sorted_ranges()[0];
                if first != (0, 65535, 0) {
                    return false;
                }
            } else if r > 1 {
                return false;
            }
            match &a.usecmap {
                Some(u) => a = u,
                None => return true,
            }
        }
        false
    }
}

/// Whether a token is `word`, trailing garbage ignored (`is_keyword`).
fn is_keyword(tok: &Token, word: &[u8]) -> bool {
    matches!(tok, Token::Keyword(k) if k.starts_with(word))
}

/// `pdf_code_from_string`.
fn code(s: &[u8]) -> u32 {
    s.iter().fold(0u32, |a, &b| (a << 8) | u32::from(b))
}

struct Parser<'a, 'w> {
    lx: Lexer<'a>,
    warn: &'w mut dyn FnMut(&str),
}

impl Parser<'_, '_> {
    fn skip_to_keyword(&mut self, end: &[u8], message: &str) {
        (self.warn)(message);
        loop {
            let tok = self.lx.next_token();
            if is_keyword(&tok, end) || matches!(tok, Token::Error | Token::Eof) {
                return;
            }
        }
    }

    fn skip_to_close_array(&mut self, message: &str) {
        (self.warn)(message);
        loop {
            match self.lx.next_token() {
                Token::CloseArray | Token::Error | Token::Eof => return,
                _ => {}
            }
        }
    }

    fn codespace_range(&mut self, cmap: &mut Cmap) {
        loop {
            let tok = self.lx.next_token();
            if is_keyword(&tok, b"endcodespacerange") {
                return;
            }
            let Token::String(lo) = tok else {
                self.skip_to_keyword(b"endcodespacerange", "expected string or endcodespacerange");
                return;
            };
            let Token::String(hi) = self.lx.next_token() else {
                self.skip_to_keyword(b"endcodespacerange", "expected string or endcodespacerange");
                return;
            };
            cmap.add_codespace(code(&lo), code(&hi), hi.len(), self.warn);
        }
    }

    fn cid_range(&mut self, cmap: &mut Cmap) -> Result<(), String> {
        loop {
            let tok = self.lx.next_token();
            if is_keyword(&tok, b"endcidrange") {
                return Ok(());
            }
            let Token::String(lo) = tok else {
                self.skip_to_keyword(b"endcidrange", "expected string or endcidrange");
                return Ok(());
            };
            let Token::String(hi) = self.lx.next_token() else {
                self.skip_to_keyword(b"endcidrange", "expected string");
                return Ok(());
            };
            let Token::Int(dst) = self.lx.next_token() else {
                self.skip_to_keyword(b"endcidrange", "expected integer");
                return Ok(());
            };
            cmap.map_range(code(&lo), code(&hi), dst as i32, self.warn)?;
        }
    }

    fn cid_char(&mut self, cmap: &mut Cmap) -> Result<(), String> {
        loop {
            let tok = self.lx.next_token();
            if is_keyword(&tok, b"endcidchar") {
                return Ok(());
            }
            let Token::String(src) = tok else {
                self.skip_to_keyword(b"endcidchar", "expected string or endcidchar");
                return Ok(());
            };
            let Token::Int(dst) = self.lx.next_token() else {
                self.skip_to_keyword(b"endcidchar", "expected integer");
                return Ok(());
            };
            let src = code(&src);
            cmap.map_range(src, src, dst as i32, self.warn)?;
        }
    }

    /// A string of UTF-16 units as values, at most MRANGE_CAP of them.
    fn units(s: &[u8]) -> Vec<i32> {
        s.as_chunks::<2>()
            .0
            .iter()
            .take(MRANGE_CAP)
            .map(|c| code(c) as i32)
            .collect()
    }

    fn bf_range_array(&mut self, cmap: &mut Cmap, mut lo: u32) -> Result<(), String> {
        loop {
            match self.lx.next_token() {
                Token::CloseArray => return Ok(()),
                Token::String(s) => {
                    let dst = Self::units(&s);
                    if !dst.is_empty() {
                        cmap.map_one_to_many(lo, &dst, self.warn)?;
                    }
                    lo = lo.wrapping_add(1);
                }
                _ => {
                    self.skip_to_close_array("expected string or ]");
                    return Ok(());
                }
            }
        }
    }

    fn bf_range(&mut self, cmap: &mut Cmap) -> Result<(), String> {
        loop {
            let tok = self.lx.next_token();
            if is_keyword(&tok, b"endbfrange") {
                return Ok(());
            }
            let Token::String(lo) = tok else {
                self.skip_to_keyword(b"endbfrange", "expected string or endbfrange");
                return Ok(());
            };
            let Token::String(hi) = self.lx.next_token() else {
                self.skip_to_keyword(b"endbfrange", "expected string");
                return Ok(());
            };
            let (mut lo, hi) = (code(&lo) as i32, code(&hi) as i32);
            if !(0..=65535).contains(&lo) || !(0..=65535).contains(&hi) || lo > hi {
                self.skip_to_keyword(b"endbfrange", "bfrange limits out of range");
                return Ok(());
            }
            match self.lx.next_token() {
                Token::String(s) => {
                    if s.len() == 2 {
                        cmap.map_range(lo as u32, hi as u32, code(&s) as i32, self.warn)?;
                    } else {
                        let mut dst = Self::units(&s);
                        if !dst.is_empty() {
                            while lo <= hi {
                                cmap.map_one_to_many(lo as u32, &dst, self.warn)?;
                                if let Some(last) = dst.last_mut() {
                                    *last += 1;
                                }
                                lo += 1;
                            }
                        }
                    }
                }
                Token::OpenArray => self.bf_range_array(cmap, lo as u32)?,
                _ => {
                    self.skip_to_keyword(b"endbfrange", "expected string or array or endbfrange");
                    return Ok(());
                }
            }
        }
    }

    fn bf_char(&mut self, cmap: &mut Cmap) -> Result<(), String> {
        loop {
            let tok = self.lx.next_token();
            if is_keyword(&tok, b"endbfchar") {
                return Ok(());
            }
            let Token::String(src) = tok else {
                self.skip_to_keyword(b"endbfchar", "expected string or endbfchar");
                return Ok(());
            };
            let Token::String(dst) = self.lx.next_token() else {
                self.skip_to_keyword(b"endbfchar", "expected string");
                return Ok(());
            };
            let dst = Self::units(&dst);
            if !dst.is_empty() {
                cmap.map_one_to_many(code(&src), &dst, self.warn)?;
            }
        }
    }
}

/// `pdf_load_cmap`: a CMap program.
pub fn parse(data: &[u8], doc: &Document) -> Result<Cmap, String> {
    let diag = doc.diagnostics();
    let mut warn = |m: &str| diag.warn("cmap.syntax", m.to_string());
    let mut cmap = Cmap::new();
    let mut key = b".notdef".to_vec();
    let mut p = Parser {
        lx: Lexer::small(data, 0, diag),
        warn: &mut warn,
    };
    loop {
        match p.lx.next_token() {
            Token::Eof => break,
            Token::Name(n) => match n.as_slice() {
                b"CMapName" => match p.lx.next_token() {
                    Token::Name(name) => cmap.name = String::from_utf8_lossy(&name).into_owned(),
                    _ => (p.warn)("expected name after CMapName in cmap"),
                },
                b"WMode" => match p.lx.next_token() {
                    Token::Int(w) => cmap.wmode = w as i32,
                    _ => (p.warn)("expected integer after WMode in cmap"),
                },
                _ => key = n[..n.len().min(63)].to_vec(),
            },
            tok @ Token::Keyword(_) => {
                if is_keyword(&tok, b"endcmap") {
                    break;
                } else if is_keyword(&tok, b"usecmap") {
                    cmap.usecmap_name = String::from_utf8_lossy(&key).into_owned();
                } else if is_keyword(&tok, b"begincodespacerange") {
                    p.codespace_range(&mut cmap);
                } else if is_keyword(&tok, b"beginbfchar") {
                    p.bf_char(&mut cmap)?;
                } else if is_keyword(&tok, b"begincidchar") {
                    p.cid_char(&mut cmap)?;
                } else if is_keyword(&tok, b"beginbfrange") {
                    p.bf_range(&mut cmap)?;
                } else if is_keyword(&tok, b"begincidrange") {
                    p.cid_range(&mut cmap)?;
                }
            }
            _ => {}
        }
    }
    Ok(cmap)
}

/// The predefined CMaps, decoded from the generated table once.
fn builtin_table() -> &'static HashMap<String, Arc<Cmap>> {
    static TABLE: OnceLock<HashMap<String, Arc<Cmap>>> = OnceLock::new();
    TABLE.get_or_init(|| decode_builtin(include_bytes!("../data/cmaps.bin")).unwrap_or_default())
}

fn decode_builtin(data: &[u8]) -> Option<HashMap<String, Arc<Cmap>>> {
    struct R<'a>(&'a [u8]);
    impl R<'_> {
        fn byte(&mut self) -> Option<u8> {
            let (&b, rest) = self.0.split_first()?;
            self.0 = rest;
            Some(b)
        }
        fn varint(&mut self) -> Option<u32> {
            let mut v: u32 = 0;
            for shift in (0..35).step_by(7) {
                let b = self.byte()?;
                v |= u32::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    return Some(v);
                }
            }
            None
        }
        fn short(&mut self) -> Option<String> {
            let len = usize::from(self.byte()?);
            let (s, rest) = self.0.split_at_checked(len)?;
            self.0 = rest;
            Some(String::from_utf8_lossy(s).into_owned())
        }
    }
    let mut r = R(data.strip_prefix(b"NCM1")?);
    let count = r.varint()?;
    let mut raw: Vec<Cmap> = Vec::new();
    for _ in 0..count {
        let mut cmap = Cmap::new();
        cmap.name = r.short()?;
        cmap.usecmap_name = r.short()?;
        cmap.wmode = i32::from(r.byte()?);
        let spaces = r.byte()?;
        for _ in 0..spaces {
            let n = usize::from(r.byte()?);
            let low = r.varint()?;
            let high = r.varint()?;
            cmap.codespaces.push(Codespace { n, low, high });
        }
        let ranges = r.varint()?;
        let mut prev = 0u32;
        for _ in 0..ranges {
            let low = prev.wrapping_add(r.varint()?);
            let high = low.wrapping_add(r.varint()?);
            let out = r.varint()?;
            cmap.entries.insert(
                low,
                Entry {
                    low,
                    high,
                    out,
                    many: false,
                },
            );
            prev = high;
        }
        let mranges = r.varint()?;
        for _ in 0..mranges {
            let low = r.varint()?;
            let out = r.varint()?;
            cmap.entries.insert(
                low,
                Entry {
                    low,
                    high: low,
                    out,
                    many: true,
                },
            );
        }
        let table = r.varint()?;
        for _ in 0..table {
            cmap.table.push(r.varint()? as i32);
        }
        raw.push(cmap);
    }
    // Chain each to its UseCMap, as `pdf_load_system_cmap` does, however
    // deep the chain (UniGB-UTF16-V uses UniGB-UTF16-H, which uses
    // UniGB-X).
    let by_name: HashMap<String, Cmap> = raw.into_iter().map(|c| (c.name.clone(), c)).collect();
    let mut out: HashMap<String, Arc<Cmap>> = HashMap::new();
    for name in by_name.keys() {
        link(name, &by_name, &mut out, 0);
    }
    Some(out)
}

fn link(
    name: &str,
    by_name: &HashMap<String, Cmap>,
    out: &mut HashMap<String, Arc<Cmap>>,
    depth: usize,
) -> Option<Arc<Cmap>> {
    if let Some(done) = out.get(name) {
        return Some(done.clone());
    }
    let mut cmap = by_name.get(name)?.clone();
    if !cmap.usecmap_name.is_empty() && depth < MAX_USECMAP {
        let parent = cmap.usecmap_name.clone();
        if let Some(u) = link(&parent, by_name, out, depth + 1) {
            cmap.set_usecmap(u);
        }
    }
    let cmap = Arc::new(cmap);
    out.insert(name.to_string(), cmap.clone());
    Some(cmap)
}

/// `pdf_load_system_cmap`.
pub fn load_system(name: &[u8]) -> Result<Arc<Cmap>, String> {
    let name = String::from_utf8_lossy(name);
    let table = builtin_table();
    let cmap = table
        .get(name.as_ref())
        .ok_or_else(|| report::format(&format!("no builtin cmap file: {name}")))?;
    if !cmap.usecmap_name.is_empty() && cmap.usecmap.is_none() {
        return Err(report::format(&format!(
            "no builtin cmap file: {}",
            cmap.usecmap_name
        )));
    }
    Ok(cmap.clone())
}

/// `pdf_load_builtin_cmap` for a name MuPDF always has.
pub fn builtin(name: &str) -> Option<Arc<Cmap>> {
    builtin_table().get(name).cloned()
}

/// `pdf_load_embedded_cmap`: a CMap stream, its /UseCMap followed.
pub fn load_embedded(doc: &Document, obj: &Object) -> Result<Arc<Cmap>, String> {
    load_embedded_imp(doc, obj, &mut Vec::new())
}

fn load_embedded_imp(
    doc: &Document,
    obj: &Object,
    cycle: &mut Vec<i32>,
) -> Result<Arc<Cmap>, String> {
    let diag = doc.diagnostics();
    let data = match doc.resolve(obj) {
        Object::Stream(s) => doc.stream_data(&s).map_err(|e| e.report())?,
        _ => return Err(report::format("object is not a stream")),
    };
    let mut cmap = parse(&data, doc)?;
    if let Object::Int(w) = doc.dict_get(obj, b"WMode") {
        cmap.wmode = w as i32;
    }
    let use_obj = doc
        .resolve(obj)
        .as_dict()
        .and_then(|d| d.get(b"UseCMap").cloned())
        .unwrap_or(Object::Null);
    let system = |name: &[u8], cmap: &mut Cmap| match load_system(name) {
        Ok(u) => cmap.set_usecmap(u),
        Err(line) => {
            diag.warn("error.reported", line);
            diag.warn(
                "cmap.system_missing",
                format!("cannot load system CMap: {}", String::from_utf8_lossy(name)),
            );
        }
    };
    match doc.resolve(&use_obj) {
        Object::Name(n) => system(&n, &mut cmap),
        _ if matches!(use_obj, Object::Ref(_)) => {
            let num = use_obj.as_ref().map_or(0, |r| r.num);
            if cycle.contains(&num) || cycle.len() >= MAX_USECMAP {
                return Err(report::format("recursive CMap"));
            }
            cycle.push(num);
            let u = load_embedded_imp(doc, &use_obj, cycle);
            cycle.pop();
            cmap.set_usecmap(u?);
        }
        _ if !cmap.usecmap_name.is_empty() => {
            let name = cmap.usecmap_name.clone().into_bytes();
            // MuPDF names the (missing) /UseCMap object in its warning.
            match load_system(&name) {
                Ok(u) => cmap.set_usecmap(u),
                Err(line) => {
                    diag.warn("error.reported", line);
                    diag.warn("cmap.system_missing", "cannot load system CMap: ");
                }
            }
        }
        _ => {}
    }
    Ok(Arc::new(cmap))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet() -> impl FnMut(&str) {
        |_: &str| {}
    }

    #[test]
    fn later_ranges_replace_what_they_overlap_and_neighbours_merge() {
        let mut w = quiet();
        let mut c = Cmap::new();
        c.add_codespace(0, 0xffff, 2, &mut w);
        c.map_range(0, 99, 1000, &mut w).unwrap_or_default();
        c.map_range(10, 19, 5, &mut w).unwrap_or_default();
        assert_eq!(c.lookup(9), 1009);
        assert_eq!(c.lookup(10), 5);
        assert_eq!(c.lookup(20), 1020);
        assert_eq!(c.lookup(100), -1);
        // A range that continues its neighbour joins it.
        let mut id = Cmap::new();
        id.add_codespace(0, 0xffff, 2, &mut w);
        id.map_range(0, 0xff, 0, &mut w).unwrap_or_default();
        id.map_range(0x100, 0xffff, 0x100, &mut w)
            .unwrap_or_default();
        assert_eq!(id.sorted_ranges(), vec![(0, 0xffff, 0)]);
        assert!(id.is_effectively_identity(&Cmap::new()));
    }

    #[test]
    fn one_to_many_mappings_join_surrogates() {
        let mut w = quiet();
        let mut c = Cmap::new();
        c.add_codespace(0, 0xff, 1, &mut w);
        c.map_one_to_many(1, &[0x66, 0x66, 0x69], &mut w)
            .unwrap_or_default();
        c.map_one_to_many(2, &[0xd83d, 0xde00], &mut w)
            .unwrap_or_default();
        assert_eq!(c.lookup_full(1), vec![0x66, 0x66, 0x69]);
        assert_eq!(c.lookup(1), -1);
        assert_eq!(c.lookup_full(2), vec![0x1f600]);
        // Into a buffer, which each lookup clears.
        let mut buf = vec![7, 7, 7, 7];
        c.lookup_full_into(1, &mut buf);
        assert_eq!(buf, [0x66, 0x66, 0x69]);
        c.lookup_full_into(9, &mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn codes_are_split_by_the_code_spaces() {
        let mut w = quiet();
        let mut c = Cmap::new();
        c.add_codespace(0, 0x7f, 1, &mut w);
        c.add_codespace(0x8140, 0xfefe, 2, &mut w);
        assert_eq!(c.decode(b"A\x81\x40"), (0x41, 1));
        assert_eq!(c.decode(b"\x81\x40"), (0x8140, 2));
        assert_eq!(c.decode(b"\xff"), (0, 1));
    }

    #[test]
    fn vertical_cmaps_find_codes_through_their_horizontal_parent() {
        let c = load_system(b"UniGB-UTF16-V").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.wmode, 1);
        assert_eq!(c.decode(&[0x00, 0x48]), (0x48, 2));
        assert_eq!(c.lookup(0x48), 41);
    }

    #[test]
    fn predefined_cmaps_load_with_their_parents() {
        let c = load_system(b"UniJIS-UCS2-H").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.usecmap_name, "UniJIS-X");
        assert!(c.usecmap.is_some());
        let id = load_system(b"Identity-H").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(id.lookup(0x1234), 0x1234);
        let ucs = load_system(b"Adobe-Japan1-UCS2").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(ucs.lookup(1), 0x20);
        assert!(load_system(b"Nope").is_err());
    }
}
