// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-xref.c (pdf_load_xref and helpers) and
// source/fitz/filter-basic.c (the endstream filter),
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! Cross-reference loading: `startxref`, classic tables, xref streams,
//! hybrid files and `/Prev` chains. Any error here sends the document to
//! repair, exactly as in MuPDF.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use memchr::memmem;

use crate::diag::{Diagnostics, report};
use crate::filter;
use crate::lex::{Lexer, Token, is_white};
use crate::limits::Limits;
use crate::object::{Dict, MAX_OBJECT_NUMBER, Object, Stream};
use crate::parse::{parse_dict, parse_indirect};
use crate::slots::{DENSE_LIMIT, Slots};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Listed nowhere (MuPDF's type 0).
    None,
    Free,
    /// At byte offset `ofs` (relative to the header).
    InFile,
    /// Inside object stream `ofs`, at index `generation`.
    InStream,
}

/// One xref entry: 16 bytes, so even a full table stays small.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub generation: i32,
    pub ofs: i64,
}

impl Entry {
    pub const NONE: Entry = Entry {
        kind: Kind::None,
        generation: 0,
        ofs: 0,
    };

    pub fn free(generation: i32) -> Entry {
        Entry {
            kind: Kind::Free,
            generation,
            ofs: 0,
        }
    }
}

/// Consecutive entries of one subsection. Long stretches of identical
/// entries (an xref stream with zero-width fields can declare millions)
/// are stored once.
#[derive(Debug)]
enum RunEntries {
    Each(Vec<Entry>),
    Same { count: i32, entry: Entry },
}

#[derive(Debug)]
struct Run {
    start: i32,
    entries: RunEntries,
}

impl Run {
    fn get(&self, num: i32) -> Entry {
        match &self.entries {
            RunEntries::Each(v) => v[(num - self.start) as usize],
            RunEntries::Same { entry, .. } => *entry,
        }
    }
}

/// Runs of identical entries longer than this become one `Same` run.
const SAME_RUN: usize = 64;

/// A set of numbers as disjoint, non-adjacent ranges. Numbers added in
/// ascending order are collected in `open` without touching the map.
#[derive(Debug, Default)]
struct Intervals {
    map: BTreeMap<i32, i32>,
    open: Option<(i32, i32)>,
}

impl Intervals {
    fn contains(&self, n: i32) -> bool {
        self.open.is_some_and(|(s, e)| n >= s && n < e)
            || self
                .map
                .range(..=n)
                .next_back()
                .is_some_and(|(_, &e)| n < e)
    }

    /// Whether any number in `start..end` is in the set.
    fn any_in(&self, start: i32, end: i32) -> bool {
        self.open.is_some_and(|(s, e)| s < end && start < e)
            || self.contains(start)
            || self.map.range(start..end).next().is_some()
    }

    fn add(&mut self, n: i32) {
        match &mut self.open {
            Some((_, e)) if *e == n => *e += 1,
            _ => {
                self.flush();
                self.open = Some((n, n + 1));
            }
        }
    }

    /// Moves `open` into the map, so `gaps` sees every number.
    fn flush(&mut self) {
        if let Some((s, e)) = self.open.take() {
            self.insert(s, e);
        }
    }

    fn insert(&mut self, start: i32, end: i32) {
        if start >= end {
            return;
        }
        let (mut s, mut e) = (start, end);
        if let Some((&ps, &pe)) = self.map.range(..=s).next_back()
            && pe >= s
        {
            s = ps;
            e = e.max(pe);
        }
        let joined: Vec<i32> = self.map.range(s..=e).map(|(&k, _)| k).collect();
        for k in joined {
            if let Some(v) = self.map.remove(&k) {
                e = e.max(v);
            }
        }
        self.map.insert(s, e);
    }

    /// The parts of `start..end` not in the set, in order. `open` must
    /// have been flushed.
    fn gaps(&self, start: i32, end: i32) -> Vec<(i32, i32)> {
        let mut out = Vec::new();
        let mut cursor = start;
        let covering = self
            .map
            .range(..=start)
            .next_back()
            .map(|(&s, &e)| (s, e))
            .into_iter()
            .chain(self.map.range(start + 1..end).map(|(&s, &e)| (s, e)));
        for (s, e) in covering {
            if s > cursor {
                out.push((cursor, s.min(end)));
            }
            cursor = cursor.max(e);
            if cursor >= end {
                break;
            }
        }
        if cursor < end {
            out.push((cursor, end));
        }
        out
    }
}

/// Where a resolved entry comes from: the xref stream's own entry, or a
/// run (read with `Run::get`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Src {
    Own,
    Run(usize),
}

/// Numbers `start..end` of one section, all defined by `src`.
#[derive(Debug, Clone, Copy)]
struct Piece {
    start: i32,
    end: i32,
    src: Src,
}

/// One xref section (one revision), newest first in the list. As in
/// MuPDF, an entry is kept from the first subsection that defines it,
/// except the xref stream's own entry, which always wins.
#[derive(Debug, Default)]
pub struct Section {
    runs: Vec<Run>,
    /// Numbers this section has defined so far.
    defined: Intervals,
    /// MuPDF's list of subsections for this section, after its merging
    /// rules: start to (length, rank). The highest rank comes first in
    /// MuPDF's list, which decides the order entries are checked in.
    layout: BTreeMap<i32, (i32, u32)>,
    rank: u32,
    /// Whether the current subsection overlaps numbers already defined.
    overlaps: bool,
    next: i32,
    fresh: bool,
    own: Option<(i32, Entry)>,
    pub num_objects: i32,
    pub trailer: Option<Dict>,
    /// Objects MuPDF caches while loading (the xref streams themselves).
    pub cached: HashMap<i32, Object>,
}

impl Section {
    /// Starts a subsection (`pdf_xref_find_subsection`).
    fn begin_subsection(&mut self, start: i32, len: i32) {
        self.next = start;
        self.fresh = true;
        self.overlaps = false;
        if len <= 0 {
            return;
        }
        let end = start + len;
        self.overlaps = self.defined.any_in(start, end);
        self.place(start, end);
        self.num_objects = self.num_objects.max(end);
    }

    /// MuPDF's four cases: inside a subsection (nothing to do), extending
    /// the one that holds or ends at `start`, overlapping several (the
    /// section becomes one solid subsection), or new (first in the list).
    fn place(&mut self, start: i32, end: i32) {
        let below = self
            .layout
            .range(..=start)
            .next_back()
            .map(|(&s, &(l, _))| (s, s + l));
        if below.is_some_and(|(s, e)| s <= start && end <= e) {
            return;
        }
        let mut touching: Vec<i32> = Vec::new();
        if let Some((s, e)) = below
            && e >= start
        {
            touching.push(s);
            if s == start
                && let Some((&ps, &(pl, _))) = self.layout.range(..start).next_back()
                && ps + pl == start
            {
                touching.push(ps);
            }
        }
        let covers = self.layout.range(start + 1..end).next().is_some();
        if covers || touching.len() > 1 {
            self.solidify(end.max(self.num_objects));
        } else if let Some(&s) = touching.first() {
            if let Some(sub) = self.layout.get_mut(&s) {
                sub.0 = end - s;
            }
        } else {
            self.rank += 1;
            self.layout.insert(start, (end - start, self.rank));
        }
    }

    /// `ensure_solid_xref`: one subsection covering `0..num`.
    fn solidify(&mut self, num: i32) {
        let num = num.max(self.num_objects);
        let solid = self.layout.len() == 1 && self.layout.get(&0).is_some_and(|&(l, _)| l >= num);
        if !solid {
            self.rank += 1;
            self.layout.clear();
            self.layout.insert(0, (num, self.rank));
        }
        self.num_objects = num;
    }

    fn in_layout(&self, num: i32) -> Option<u32> {
        self.layout
            .range(..=num)
            .next_back()
            .filter(|&(&s, &(l, _))| num < s + l)
            .map(|(_, &(_, rank))| rank)
    }

    /// Whether `num` was defined before the current subsection, in which
    /// case MuPDF neither parses nor stores its entry.
    fn defines(&self, num: i32) -> bool {
        self.overlaps && self.defined.contains(num)
    }

    /// Appends the next entry of the current subsection.
    fn push(&mut self, e: Entry) {
        let num = self.next;
        self.next = self.next.saturating_add(1);
        let e = if self.defines(num) { Entry::NONE } else { e };
        if e.kind != Kind::None {
            self.defined.add(num);
        }
        if !self.fresh
            && let Some(run) = self.runs.last_mut()
        {
            match &mut run.entries {
                RunEntries::Same { count, entry } if *entry == e => {
                    *count += 1;
                    return;
                }
                RunEntries::Each(v) => {
                    v.push(e);
                    let n = v.len();
                    if n >= SAME_RUN && v[n - SAME_RUN..].iter().all(|x| *x == e) {
                        v.truncate(n - SAME_RUN);
                        let same = Run {
                            start: run.start + (n - SAME_RUN) as i32,
                            entries: RunEntries::Same {
                                count: SAME_RUN as i32,
                                entry: e,
                            },
                        };
                        if n == SAME_RUN {
                            *run = same;
                        } else {
                            self.runs.push(same);
                        }
                    }
                    return;
                }
                RunEntries::Same { .. } => {}
            }
        }
        self.fresh = false;
        self.runs.push(Run {
            start: num,
            entries: RunEntries::Each(vec![e]),
        });
    }

    /// Appends `count` copies of `e` to the current subsection, as `push`
    /// would one by one, in time that does not depend on `count`.
    fn push_many(&mut self, e: Entry, count: i32) {
        let start = self.next;
        let end = start.saturating_add(count);
        self.next = end;
        self.defined.flush();
        let gaps = if self.overlaps {
            self.defined.gaps(start, end)
        } else {
            vec![(start, end)]
        };
        let mut cursor = start;
        for (a, b) in gaps {
            if a > cursor {
                self.push_range(cursor, a, Entry::NONE);
            }
            self.push_range(a, b, e);
            if e.kind != Kind::None {
                self.defined.insert(a, b);
            }
            cursor = b;
        }
        if cursor < end {
            self.push_range(cursor, end, Entry::NONE);
        }
    }

    fn push_range(&mut self, start: i32, end: i32, entry: Entry) {
        self.fresh = false;
        self.runs.push(Run {
            start,
            entries: RunEntries::Same {
                count: end - start,
                entry,
            },
        });
    }

    /// The xref stream's own entry (`pdf_get_populating_xref_entry`): it
    /// replaces whatever the stream listed for its number, and a number
    /// outside every subsection makes the section solid.
    fn set_own(&mut self, num: i32, e: Entry) {
        if let Some((n, prev)) = self.own.take() {
            self.runs.push(Run {
                start: n,
                entries: RunEntries::Each(vec![prev]),
            });
        }
        if self.in_layout(num).is_none() {
            self.solidify(num.saturating_add(1));
        }
        self.own = Some((num, e));
        self.defined.add(num);
        self.fresh = true;
    }

    fn entry(&self, piece: &Piece, num: i32) -> Entry {
        match piece.src {
            Src::Own => self.own.map_or(Entry::NONE, |(_, e)| e),
            Src::Run(ri) => self.runs[ri].get(num),
        }
    }

    /// The section's defined entries as disjoint pieces, in number order.
    fn resolve(&self) -> Vec<Piece> {
        let mut pieces = Vec::new();
        for (ri, run) in self.runs.iter().enumerate() {
            match &run.entries {
                RunEntries::Same { count, entry } => {
                    if entry.kind != Kind::None {
                        pieces.push(Piece {
                            start: run.start,
                            end: run.start + count,
                            src: Src::Run(ri),
                        });
                    }
                }
                RunEntries::Each(v) => {
                    let mut k = 0;
                    while k < v.len() {
                        if v[k].kind == Kind::None {
                            k += 1;
                            continue;
                        }
                        let from = k;
                        while k < v.len() && v[k].kind != Kind::None {
                            k += 1;
                        }
                        pieces.push(Piece {
                            start: run.start + from as i32,
                            end: run.start + k as i32,
                            src: Src::Run(ri),
                        });
                    }
                }
            }
        }
        if let Some((n, e)) = self.own {
            let mut split = Vec::new();
            pieces.retain(|p| {
                if n < p.start || n >= p.end {
                    return true;
                }
                for (start, end) in [(p.start, n), (n + 1, p.end)] {
                    if start < end {
                        split.push(Piece { start, end, ..*p });
                    }
                }
                false
            });
            pieces.extend(split);
            if e.kind != Kind::None {
                pieces.push(Piece {
                    start: n,
                    end: n + 1,
                    src: Src::Own,
                });
            }
        }
        pieces.sort_unstable_by_key(|p| p.start);
        pieces
    }
}

/// A section with its resolved pieces, for lookups.
struct Resolved<'a> {
    section: &'a Section,
    pieces: Vec<Piece>,
}

impl Resolved<'_> {
    fn lookup(&self, num: i32) -> Option<Entry> {
        let i = self.pieces.partition_point(|p| p.start <= num);
        let piece = self.pieces.get(i.checked_sub(1)?)?;
        (num < piece.end).then(|| self.section.entry(piece, num))
    }

    /// Pieces inside one subsection of the layout.
    fn within(&self, start: i32, end: i32) -> &[Piece] {
        let from = self.pieces.partition_point(|p| p.start < start);
        let to = self.pieces.partition_point(|p| p.start < end);
        &self.pieces[from..to]
    }
}

#[derive(Debug)]
pub struct Loaded {
    /// Effective entries for 0..xref_len: for each number, the newest
    /// section that defines it.
    pub entries: Slots,
    pub cached: HashMap<i32, Object>,
    pub trailer: Dict,
    pub revisions: usize,
    /// Where each xref section starts, relative to the header, newest first.
    pub section_offsets: Vec<i64>,
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

    // Broken files where the first object is missing or not free. A
    // missing one is looked up in the newest section, which MuPDF makes
    // solid when no subsection holds it.
    let first = {
        let resolved: Vec<Resolved> = sections
            .iter()
            .map(|section| Resolved {
                section,
                pieces: section.resolve(),
            })
            .collect();
        resolved.iter().find_map(|r| r.lookup(0))
    };
    match first {
        Some(e) if e.kind != Kind::Free => {
            diag.warn("xref.first_not_free", "first object in xref is not free");
        }
        Some(_) => {}
        None => {
            if sections.last().is_some_and(|s| s.num_objects == 0) {
                return Err(
                    "argument error: cannot find object in xref (0 0 R), but not allowed to return NULL"
                        .into(),
                );
            }
            if let Some(newest) = sections.first_mut()
                && newest.in_layout(0).is_none()
            {
                newest.solidify(1);
            }
        }
    }

    check_entries(&sections, xref_len, data.len() as i64)?;
    // "0000000000 * n" means free (some producers, Quartz among them).
    for section in &mut sections {
        let free = |e: &mut Entry| {
            if e.kind == Kind::InFile && e.ofs == 0 {
                e.kind = Kind::Free;
            }
        };
        if let Some((_, e)) = &mut section.own {
            free(e);
        }
        for run in &mut section.runs {
            match &mut run.entries {
                RunEntries::Each(v) => v.iter_mut().for_each(free),
                RunEntries::Same { entry, .. } => free(entry),
            }
        }
    }

    let (mut entries, cached) = merge(&sections, xref_len);
    if entries.get(0).kind == Kind::None {
        entries.set(0, Entry::free(65535));
    }

    Ok(Loaded {
        entries,
        cached,
        trailer,
        revisions: sections.len(),
        section_offsets: seen,
    })
}

/// `check_xref_entry_offsets` over every entry, in MuPDF's order
/// (`pdf_xref_entry_map`): sections newest first, each section's
/// subsections in list order, numbers ascending. The order decides which
/// error is reported. In-file entries must be inside the file; entries in
/// object streams must name one stored in the file, which an earlier
/// "0000000000 * n" entry no longer is: MuPDF turns those into free
/// entries as it goes.
fn check_entries(sections: &[Section], xref_len: i32, file_size: i64) -> Result<(), XrefError> {
    let resolved: Vec<Resolved> = sections
        .iter()
        .map(|section| Resolved {
            section,
            pieces: section.resolve(),
        })
        .collect();
    for (si, current) in resolved.iter().enumerate() {
        let section = current.section;
        let mut layout: Vec<(i32, i32, u32)> = section
            .layout
            .iter()
            .map(|(&s, &(l, r))| (s, s + l, r))
            .collect();
        layout.sort_unstable_by_key(|l| std::cmp::Reverse(l.2));
        for &(start, end, rank) in &layout {
            for piece in current.within(start, end) {
                let uniform = match piece.src {
                    Src::Own => true,
                    Src::Run(ri) => matches!(section.runs[ri].entries, RunEntries::Same { .. }),
                };
                let last = if uniform { piece.start + 1 } else { piece.end };
                for num in piece.start..last {
                    let e = section.entry(piece, num);
                    match e.kind {
                        Kind::InFile if e.ofs != 0 && (e.ofs < 0 || e.ofs >= file_size) => {
                            return Err(format!(
                                "object offset out of range: {} ({num} 0 R)",
                                e.ofs as i32
                            ));
                        }
                        Kind::InStream => {
                            let visited = |k: usize, t: i32| {
                                k < si
                                    || (k == si
                                        && section
                                            .in_layout(t)
                                            .is_some_and(|r| r > rank || (r == rank && t < num)))
                            };
                            let target = i32::try_from(e.ofs)
                                .ok()
                                .filter(|&t| t > 0 && t < xref_len)
                                .and_then(|t| {
                                    resolved[si..]
                                        .iter()
                                        .enumerate()
                                        .find_map(|(k, r)| r.lookup(t).map(|te| (si + k, t, te)))
                                });
                            let stored = target.is_some_and(|(k, t, te)| {
                                te.kind == Kind::InFile && !(te.ofs == 0 && visited(k, t))
                            });
                            if !stored {
                                return Err(format!(
                                    "invalid reference to an objstm that does not exist: {} ({num} 0 R)",
                                    e.ofs as i32
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

/// The effective table: for each number, the newest section that defines
/// it. Each section's pieces only fill numbers no newer section defined,
/// so the work is bounded by the table and the file, not by how many
/// sections repeat a range. Huge tables that are mostly uniform ranges are
/// stored sparsely.
fn merge(sections: &[Section], xref_len: i32) -> (Slots, HashMap<i32, Object>) {
    let resolved: Vec<Resolved> = sections
        .iter()
        .map(|section| Resolved {
            section,
            pieces: section.resolve(),
        })
        .collect();
    let len = usize::try_from(xref_len).unwrap_or(0);
    let listed: usize = resolved
        .iter()
        .flat_map(|r| r.pieces.iter().map(move |p| (r, p)))
        .filter(|(r, p)| match p.src {
            Src::Own => true,
            Src::Run(ri) => matches!(r.section.runs[ri].entries, RunEntries::Each(_)),
        })
        .map(|(_, p)| (p.end - p.start) as usize)
        .sum();
    // An array costs 16 bytes a slot, a map entry about 24 per listed one.
    let mut slots = if len <= DENSE_LIMIT || listed.saturating_mul(4) >= len {
        Slots::from_vec(vec![Entry::NONE; len])
    } else {
        Slots::sparse(len)
    };
    let mut cached = HashMap::new();
    let mut taken = Intervals::default();
    for r in &resolved {
        for piece in &r.pieces {
            let end = piece.end.min(xref_len);
            if piece.start >= end {
                continue;
            }
            for (a, b) in taken.gaps(piece.start, end) {
                match piece.src {
                    Src::Own => {
                        slots.set(a, r.section.entry(piece, a));
                        if let Some(obj) = r.section.cached.get(&a) {
                            cached.insert(a, obj.clone());
                        }
                    }
                    Src::Run(ri) => match &r.section.runs[ri].entries {
                        RunEntries::Same { entry, .. } => slots.fill_range(a, b, *entry),
                        RunEntries::Each(_) => {
                            for num in a..b {
                                slots.set(num, r.section.entry(piece, num));
                            }
                        }
                    },
                }
            }
            taken.insert(piece.start, end);
        }
    }
    (slots, cached)
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
        section.begin_subsection(start, len);

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
            // An entry this section already defined is neither parsed nor
            // allowed to change the 19-byte carry, as in MuPDF.
            if section.defines(num) {
                section.push(Entry::NONE);
                continue;
            }
            let entry = parse_old_entry(&buf[..20], num)?;
            section.push(entry.unwrap_or(Entry::NONE));
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
        section.begin_subsection(first, count);
        // With every width zero, no bytes are read and all entries are the
        // same in-file entry at offset 0: store them as one range.
        if w0 == 0 && w1 == 0 && w2 == 0 && count > 0 {
            if cursor >= decoded.len() {
                return Err("truncated xref stream".into());
            }
            section.push_many(
                Entry {
                    kind: Kind::InFile,
                    ofs: 0,
                    generation: 0,
                },
                count,
            );
            continue;
        }
        for _ in first..first + count {
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
            section.push(Entry {
                kind,
                ofs: if w1 > 0 { b } else { 0 },
                generation: if w2 > 0 { c as i32 } else { 0 },
            });
        }
    }

    // The stream's own entry always points at what we just parsed.
    section.set_own(
        num,
        Entry {
            kind: Kind::InFile,
            ofs: (at.saturating_sub(bias)) as i64,
            generation: ind.generation,
        },
    );
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
            })
        );
        assert!(parse_old_entry(b"0000000017 00000 x \n", 1).is_err());
        assert!(parse_old_entry(b"          00000 n \n ", 1).is_err());
    }

    const HEAD: &[u8] = b"%PDF-1.5\n";
    const CATALOG: &[u8] = b"1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n";
    const PAGES: &[u8] = b"2 0 obj<</Type/Pages/Kids[]/Count 0>>endobj\n";
    /// Offsets of objects 1 and 2, and one past the end of every test file.
    const OBJ1: i64 = HEAD.len() as i64;
    const OBJ2: i64 = OBJ1 + CATALOG.len() as i64;
    const BAD: i64 = 999_999;

    /// An xref stream object; `rows` are (type, field 2, field 3).
    fn xref_stream(
        num: i32,
        size: i32,
        w: [usize; 3],
        index: &[i32],
        rows: &[(i64, i64, i64)],
        prev: Option<usize>,
    ) -> Vec<u8> {
        let mut data = Vec::new();
        for &(a, b, c) in rows {
            for (v, width) in [(a, w[0]), (b, w[1]), (c, w[2])] {
                data.extend_from_slice(&v.to_be_bytes()[8 - width..]);
            }
        }
        if w == [0, 0, 0] {
            data.push(0); // never read; keeps the stream from looking truncated
        }
        let index: Vec<String> = index.iter().map(i32::to_string).collect();
        let prev = prev.map_or(String::new(), |p| format!("/Prev {p}"));
        let mut out = format!(
            "{num} 0 obj<</Type/XRef/Size {size}/W[{} {} {}]/Index[{}]/Root 1 0 R{prev}/Length {}>>stream\n",
            w[0],
            w[1],
            w[2],
            index.join(" "),
            data.len()
        )
        .into_bytes();
        out.extend_from_slice(&data);
        out.extend_from_slice(b"\nendstream endobj\n");
        out
    }

    /// A file whose only xref is one stream, object `own`.
    fn with_stream(index: &[i32], rows: &[(i64, i64, i64)], own: i32, size: i32) -> Vec<u8> {
        let mut body = [HEAD, CATALOG, PAGES].concat();
        let at = body.len();
        body.extend(xref_stream(own, size, [1, 4, 2], index, rows, None));
        body.extend(format!("startxref\n{at}\n%%EOF\n").into_bytes());
        body
    }

    /// `count` sections, each declaring `size` zero-width entries.
    fn chain(count: i32, size: i32) -> Vec<u8> {
        let mut body = [HEAD, CATALOG, PAGES].concat();
        let mut prev = None;
        for k in 0..count {
            let at = body.len();
            body.extend(xref_stream(3 + k, size, [0, 0, 0], &[0, size], &[], prev));
            prev = Some(at);
        }
        body.extend(format!("startxref\n{}\n%%EOF\n", prev.unwrap_or(0)).into_bytes());
        body
    }

    fn load_err(data: &[u8]) -> Option<String> {
        load(data, 0, &Diagnostics::new(), &Limits::default()).err()
    }

    // Expected messages come from MuPDF 1.28.6 on the same files.
    #[test]
    fn entries_are_checked_in_mupdf_order() {
        // [5,7) was added last, so MuPDF's list visits it first.
        let list = with_stream(
            &[0, 2, 5, 2],
            &[(0, 0, 65535), (2, 4, 0), (1, BAD, 0), (1, 0, 0)],
            6,
            7,
        );
        assert_eq!(
            load_err(&list).as_deref(),
            Some("object offset out of range: 999999 (5 0 R)")
        );
        // [2,4) extends [0,2) in place, behind [6,8).
        let extend = with_stream(
            &[0, 2, 6, 2, 2, 2],
            &[
                (0, 0, 65535),
                (1, OBJ1, 0),
                (1, BAD, 0),
                (1, 0, 0),
                (2, 4, 0),
                (1, OBJ2, 0),
            ],
            7,
            8,
        );
        assert_eq!(
            load_err(&extend).as_deref(),
            Some("object offset out of range: 999999 (6 0 R)")
        );
        // The stream's own number is outside /Index: the section becomes
        // one ascending subsection.
        let solid = with_stream(&[1, 1, 5, 1], &[(2, 4, 0), (1, BAD, 0)], 9, 10);
        assert_eq!(
            load_err(&solid).as_deref(),
            Some("invalid reference to an objstm that does not exist: 4 (1 0 R)")
        );
        // Object 0 is listed nowhere: the newest section becomes solid.
        let zero = with_stream(&[1, 1, 5, 2], &[(2, 4, 0), (1, BAD, 0), (1, 0, 0)], 6, 7);
        assert_eq!(
            load_err(&zero).as_deref(),
            Some("invalid reference to an objstm that does not exist: 4 (1 0 R)")
        );
    }

    #[test]
    fn the_first_definition_in_a_section_wins() {
        let dup = with_stream(
            &[0, 3, 1, 1],
            &[(0, 0, 65535), (1, OBJ1, 0), (1, OBJ2, 0), (1, BAD, 0)],
            3,
            4,
        );
        let loaded = load(&dup, 0, &Diagnostics::new(), &Limits::default());
        assert_eq!(loaded.map(|l| l.entries.get(1).ofs), Ok(OBJ1));
    }

    #[test]
    fn zero_offsets_become_free_as_they_are_checked() {
        // 1 is "0 n", already free when 2 is checked against it.
        let before = with_stream(
            &[0, 4],
            &[(0, 0, 65535), (1, 0, 0), (2, 1, 0), (1, OBJ1, 0)],
            4,
            5,
        );
        assert_eq!(
            load_err(&before).as_deref(),
            Some("invalid reference to an objstm that does not exist: 1 (2 0 R)")
        );
        // 1 is checked against 2 before 2 is: accepted, then 2 is free.
        let after = with_stream(
            &[0, 4],
            &[(0, 0, 65535), (2, 2, 0), (1, 0, 0), (1, OBJ1, 0)],
            4,
            5,
        );
        let loaded = load(&after, 0, &Diagnostics::new(), &Limits::default());
        assert_eq!(loaded.map(|l| l.entries.get(2).kind), Ok(Kind::Free));
    }

    #[test]
    fn huge_declared_tables_cost_what_the_file_contains() {
        for (count, size) in [(1, 8_388_607), (300, 8_388_607), (1000, 1003)] {
            let file = chain(count, size);
            let diag = Diagnostics::new();
            let loaded = load(&file, 0, &diag, &Limits::default());
            let entries = loaded.map(|l| l.entries);
            assert_eq!(entries.as_ref().map(Slots::len), Ok(size as usize));
            let bytes = entries.as_ref().map_or(usize::MAX, Slots::heap_bytes);
            assert!(
                bytes < (size as usize).min(1 << 16) * 16 + 4096,
                "{count} sections: {bytes} bytes"
            );
            assert!(diag.has("xref.first_not_free"));
        }
    }

    #[test]
    fn many_subsections_load() {
        let n = 20_000;
        let mut index = vec![0, 3];
        for i in 0..n {
            index.extend([3 + 2 * i, 1]);
        }
        let rows = vec![(0, 0, 0); 3 + n as usize];
        let mut body = [HEAD, CATALOG, PAGES].concat();
        let at = body.len();
        body.extend(xref_stream(
            3 + 2 * n,
            4 + 2 * n,
            [1, 0, 0],
            &index,
            &rows,
            None,
        ));
        body.extend(format!("startxref\n{at}\n%%EOF\n").into_bytes());
        let loaded = load(&body, 0, &Diagnostics::new(), &Limits::default());
        assert_eq!(loaded.map(|l| l.entries.len()), Ok(4 + 2 * n as usize));
    }

    #[test]
    fn subsections_merge_like_mupdf() {
        let layout = |ranges: &[(i32, i32)], own: Option<i32>| {
            let mut s = Section::default();
            for &(start, len) in ranges {
                s.begin_subsection(start, len);
            }
            if let Some(n) = own {
                s.set_own(n, Entry::NONE);
            }
            let mut out: Vec<(i32, i32, u32)> =
                s.layout.iter().map(|(&st, &(l, r))| (st, l, r)).collect();
            out.sort_by_key(|l| std::cmp::Reverse(l.2));
            out.into_iter()
                .map(|(st, l, _)| (st, l))
                .collect::<Vec<_>>()
        };
        // New subsections go first in the list; contained ones change nothing.
        assert_eq!(layout(&[(0, 2), (6, 2), (6, 1)], None), [(6, 2), (0, 2)]);
        // One that starts inside or right after another extends it in place.
        assert_eq!(layout(&[(0, 2), (6, 2), (2, 2)], None), [(6, 2), (0, 4)]);
        assert_eq!(layout(&[(0, 2), (6, 2), (1, 3)], None), [(6, 2), (0, 4)]);
        // Touching or covering two makes the section solid.
        assert_eq!(layout(&[(0, 2), (4, 2), (1, 4)], None), [(0, 6)]);
        assert_eq!(layout(&[(2, 2), (0, 2), (2, 3)], None), [(0, 5)]);
        // Right after one subsection extends it, rather than adding one.
        assert_eq!(layout(&[(0, 2), (2, 2)], None), [(0, 4)]);
        assert_eq!(layout(&[(4, 2), (2, 4)], None), [(0, 6)]);
        // An own entry outside every subsection does too.
        assert_eq!(layout(&[(0, 2), (6, 2)], Some(7)), [(6, 2), (0, 2)]);
        assert_eq!(layout(&[(0, 2), (6, 2)], Some(9)), [(0, 10)]);
    }

    #[test]
    fn intervals_merge_and_report_gaps() {
        let mut set = Intervals::default();
        for n in [1, 2, 3, 7] {
            set.add(n);
        }
        set.insert(10, 20);
        set.flush();
        assert!(set.contains(3) && set.contains(7) && set.contains(19));
        assert!(!set.contains(4) && !set.contains(20) && !set.contains(0));
        assert!(set.any_in(4, 8) && !set.any_in(4, 7));
        assert_eq!(set.gaps(0, 25), [(0, 1), (4, 7), (8, 10), (20, 25)]);
        assert_eq!(set.gaps(11, 15), []);
        set.insert(3, 12);
        assert_eq!(set.gaps(0, 25), [(0, 1), (20, 25)]);
    }
}
