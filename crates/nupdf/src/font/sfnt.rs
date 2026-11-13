// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Following FreeType 2 src/sfnt/ttload.c, src/sfnt/ttcmap.c,
// src/sfnt/ttpost.c, src/sfnt/ttmtx.c, src/sfnt/sfobjs.c and
// src/truetype/ttpload.c, Copyright (C) 1996-2025 by David Turner, Robert
// Wilhelm, and Werner Lemberg, used under the FreeType License (NOTICE.md).

//! TrueType and OpenType fonts, as FreeType's sfnt module reads them for
//! MuPDF: the table directory, the character maps it keeps (and the
//! broken ones it drops), the glyph names of the `post` table, advances
//! from `hmtx`, and the metrics in `head`, `hhea` and `OS/2`.

use super::names;

/// The sfnt version tags FreeType's TrueType driver takes.
pub const TAG_TRUETYPE: u32 = 0x0001_0000;
pub const TAG_TRUETYPE_CJK: u32 = 0x0002_0000;
pub const TAG_TRUE: u32 = u32::from_be_bytes(*b"true");
pub const TAG_OTTO: u32 = u32::from_be_bytes(*b"OTTO");

/// A read past the end of the data, as FreeType's stream reports it.
const STREAM: &str = "Invalid_Stream_Operation";

/// What MuPDF needs of the face.
/// A directory entry: tag, offset and length.
type Table = ([u8; 4], usize, usize);

#[derive(Debug, Clone)]
pub struct Sfnt {
    data: std::sync::Arc<[u8]>,
    tables: Vec<Table>,
    /// The version tag of the (first) font: TrueType, `true` or `OTTO`.
    pub format_tag: u32,
    pub units_per_em: u16,
    pub num_glyphs: u32,
    pub bbox: [i32; 4],
    pub ascender: i16,
    pub descender: i16,
    /// The `cmap` table's extent, and its kept subtables.
    cmap: Option<(usize, usize)>,
    pub subtables: Vec<Subtable>,
    /// The `post` table's format when its header loads; the glyph names
    /// of formats 2 and 2.5.
    post_format: Option<u32>,
    post: Option<Post>,
    num_hmetrics: usize,
    hmtx: Option<(usize, usize)>,
    pub has_cff: bool,
    pub has_glyf: bool,
    /// The style `sfnt_load_face` gives the face, and the OS/2 family
    /// class when the table loads.
    pub bold: bool,
    pub italic: bool,
    pub fixed_pitch: bool,
    pub family_class: Option<i16>,
}

#[derive(Debug, Clone)]
pub struct Subtable {
    pub platform_id: u16,
    pub encoding_id: u16,
    pub format: u16,
    /// Offset of the subtable in the file.
    at: usize,
    unsorted: bool,
    overlapping: bool,
}

/// The names `load_post_names` reads: an index per glyph, into the
/// Macintosh names below 258 and into the table's own strings from 258.
#[derive(Debug, Clone)]
struct Post(Vec<u16>, Vec<Vec<u8>>);

/// What FreeType reads of the `OS/2` table.
struct Os2 {
    family_class: i16,
    fs_selection: u16,
    typo_asc: i16,
    typo_desc: i16,
    win_asc: u16,
    win_desc: u16,
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    d.get(at..at.checked_add(2)?)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn i16_at(d: &[u8], at: usize) -> Option<i16> {
    u16_at(d, at).map(|v| v as i16)
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at.checked_add(4)?)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn u24_at(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at.checked_add(3)?)
        .map(|b| u32::from_be_bytes([0, b[0], b[1], b[2]]))
}

impl Sfnt {
    /// Whether the data starts like an sfnt (or a collection of them).
    pub fn recognize(data: &[u8]) -> bool {
        matches!(
            data.get(0..4),
            Some(&[0, 1, 0, 0])
                | Some(&[0, 2, 0, 0])
                | Some(b"true")
                | Some(b"OTTO")
                | Some(b"ttcf")
                | Some(b"typ1")
        )
    }

    /// `sfnt_open_font` and `tt_face_load_font_dir` (with
    /// `check_table_dir`): the directory of the collection's first font,
    /// and its version tag.
    fn directory(d: &[u8]) -> Result<(u32, Vec<Table>), &'static str> {
        let mut base = 0usize;
        if d.get(0..4) == Some(b"ttcf") {
            let count = u32_at(d, 8).ok_or(STREAM)?;
            if count == 0 {
                return Err("Invalid_Table");
            }
            if count as usize > d.len() / 32 {
                return Err("Array_Too_Large");
            }
            // A count that fits leaves room for the first offset.
            base = u32_at(d, 12).unwrap_or(0) as usize;
        }
        if base.checked_add(12).is_none_or(|end| end > d.len()) {
            return Err(STREAM);
        }
        let tag = u32_at(d, base).ok_or(STREAM)?;
        let mut num_tables = usize::from(u16_at(d, base + 4).ok_or(STREAM)?);
        let entry = |nn: usize| {
            let rec = base + 12 + nn * 16;
            let t = d.get(rec..rec + 16)?;
            let tag = [t[0], t[1], t[2], t[3]];
            let offset = u32_at(t, 8)? as usize;
            let length = u32_at(t, 12)? as usize;
            Some((tag, offset, length))
        };
        let metrics = |t: &[u8; 4]| t == b"hmtx" || t == b"vmtx";
        if tag != TAG_OTTO {
            let (mut valid, mut has_head, mut has_sing, mut has_meta) = (0, false, false, false);
            for nn in 0..num_tables {
                let Some((t, offset, length)) = entry(nn) else {
                    num_tables = nn;
                    break;
                };
                if offset > d.len() || (length > d.len() - offset && !metrics(&t)) {
                    continue;
                }
                valid += 1;
                if &t == b"head" || &t == b"bhed" {
                    has_head |= &t == b"head";
                    if length < 0x36 {
                        return Err("Table_Missing");
                    }
                } else if &t == b"SING" {
                    has_sing = true;
                } else if &t == b"META" {
                    has_meta = true;
                }
            }
            if valid == 0 {
                return Err("Unknown_File_Format");
            }
            if !has_head && !(has_sing && has_meta) {
                return Err("Table_Missing");
            }
        } else if num_tables == 0 {
            return Err("Unknown_File_Format");
        }
        if base + 12 + num_tables * 16 > d.len() {
            return Err(STREAM);
        }
        let mut tables: Vec<Table> = Vec::new();
        // Every record is there now: the count was cut to them, or checked.
        for (t, offset, mut length) in (0..num_tables).filter_map(entry) {
            if offset > d.len() {
                continue;
            }
            if length > d.len() - offset {
                if !metrics(&t) {
                    continue;
                }
                // A metrics table cut to whole entries.
                length = (d.len() - offset) & !3;
            }
            if tables.iter().all(|e| e.0 != t) {
                tables.push((t, offset, length));
            }
        }
        if tables.is_empty() {
            return Err("Unknown_File_Format");
        }
        Ok((tag, tables))
    }

    /// `sfnt_open_font`, `tt_face_load_font_dir` and the table loads of
    /// `sfnt_load_face`.
    pub fn open(data: std::sync::Arc<[u8]>) -> Result<Sfnt, &'static str> {
        let d: &[u8] = &data;
        let (format_tag, tables) = Self::directory(d)?;
        let mut font = Sfnt {
            data: data.clone(),
            tables,
            format_tag,
            units_per_em: 0,
            num_glyphs: 0,
            bbox: [0; 4],
            ascender: 0,
            descender: 0,
            cmap: None,
            subtables: Vec::new(),
            post_format: None,
            post: None,
            num_hmetrics: 0,
            hmtx: None,
            has_cff: false,
            has_glyf: false,
            bold: false,
            italic: false,
            fixed_pitch: false,
            family_class: None,
        };
        font.has_cff = font.table(b"CFF ").is_some();
        font.has_glyf = font.table(b"glyf").is_some();
        let mut has_outline = font.has_glyf || font.has_cff || font.table(b"CFF2").is_some();
        // The header is read whole, whatever its table's length.
        let (h, _) = font.table(b"head").ok_or("Table_Missing")?;
        let head = d.get(h..h + 54).ok_or(STREAM)?;
        font.units_per_em = u16_at(head, 18).unwrap_or(0);
        // sfnt_load_face: OpenType 1.8.2's limits, for every sfnt.
        if !(16..=16384).contains(&font.units_per_em) {
            return Err("Invalid_Table");
        }
        let mac_style = u16_at(head, 44).unwrap_or(0);
        font.bbox = [36, 38, 40, 42].map(|at| i32::from(i16_at(head, at).unwrap_or(0)));
        if font.table(b"CBLC").is_some() || font.table(b"CBDT").is_some() {
            has_outline = false;
        }
        // An unreadable `maxp` leaves the face without glyphs.
        if let Some((m, _)) = font.table(b"maxp")
            && let Some(maxp) = d.get(m..m + 6)
        {
            font.num_glyphs = u32::from(u16_at(maxp, 4).unwrap_or(0));
        }
        font.cmap = font.table(b"cmap");
        font.load_post();
        match font.table(b"hhea") {
            Some((hh, _)) => {
                let hhea = d.get(hh..hh + 36).ok_or(STREAM)?;
                font.ascender = i16_at(hhea, 4).unwrap_or(0);
                font.descender = i16_at(hhea, 6).unwrap_or(0);
                font.num_hmetrics = usize::from(u16_at(hhea, 34).unwrap_or(0));
                font.hmtx = Some(font.table(b"hmtx").ok_or("Hmtx_Table_Missing")?);
            }
            // No `hhea` is needed in a Macintosh font.
            None if format_tag == TAG_TRUE => has_outline = false,
            None => return Err("Horiz_Header_Missing"),
        }
        if let Some((v, _)) = font.table(b"vhea") {
            d.get(v..v + 36).ok_or(STREAM)?;
        }
        let os2 = font.os2();
        font.metrics(os2.as_ref());
        match &os2 {
            Some(os2) if has_outline => {
                let sel = os2.fs_selection;
                font.italic = sel & 512 != 0 || sel & 1 != 0;
                font.bold = sel & 32 != 0;
            }
            _ => {
                font.bold = mac_style & 1 != 0;
                font.italic = mac_style & 2 != 0;
            }
        }
        font.family_class = os2.as_ref().map(|t| t.family_class);
        font.load_cmaps();
        Ok(font)
    }

    /// `tt_face_lookup_table`: a table of no length is missing.
    pub fn table(&self, tag: &[u8; 4]) -> Option<(usize, usize)> {
        self.tables
            .iter()
            .find(|(t, _, l)| t == tag && *l != 0)
            .map(|&(_, o, l)| (o, l))
    }

    /// What the TrueType glyph loader reads: `loca`, `glyf` and the
    /// location format; None without a `loca` table.
    pub(crate) fn glyf(&self) -> Option<super::outline::Glyf<'_>> {
        let loca = self.table(b"loca")?;
        let glyf = self.table(b"glyf").unwrap_or((0, 0));
        let (h, _) = self.table(b"head")?;
        Some(super::outline::Glyf {
            data: &self.data,
            loca,
            long_loca: i16_at(&self.data, h + 50).unwrap_or(0) != 0,
            glyf,
            num_glyphs: self.num_glyphs,
        })
    }

    /// `tt_face_load_loca`'s check as `tt_face_init` makes it: a TrueType
    /// font needs its `loca` table, and one with neither `glyf` nor `loca`
    /// is no TrueType font.
    pub fn check_loca(&self) -> Result<(), &'static str> {
        match (self.table(b"loca"), self.has_glyf) {
            (Some(_), _) => Ok(()),
            (None, true) => Err("Locations_Missing"),
            (None, false) => Err("Unknown_File_Format"),
        }
    }

    pub fn table_data(&self, tag: &[u8; 4]) -> Option<&[u8]> {
        let (o, l) = self.table(tag)?;
        self.data.get(o..o + l)
    }

    /// `tt_face_load_os2`: None when the table is missing or its fields
    /// for its version run past the data.
    fn os2(&self) -> Option<Os2> {
        let (o, _) = self.table(b"OS/2")?;
        let d = &self.data;
        let version = u16_at(d, o)?;
        let size = match version {
            0 => 78,
            1 => 86,
            2..=4 => 96,
            _ => 100,
        };
        d.get(o..o + size)?;
        Some(Os2 {
            family_class: i16_at(d, o + 30)?,
            fs_selection: u16_at(d, o + 62)?,
            typo_asc: i16_at(d, o + 68)?,
            typo_desc: i16_at(d, o + 70)?,
            win_asc: u16_at(d, o + 74)?,
            win_desc: u16_at(d, o + 76)?,
        })
    }

    /// The ascender and descender `sfnt_load_face` settles on.
    fn metrics(&mut self, os2: Option<&Os2>) {
        let Some(os2) = os2 else {
            return;
        };
        if os2.fs_selection & 128 != 0 {
            self.ascender = os2.typo_asc;
            self.descender = os2.typo_desc;
        } else if self.ascender == 0 && self.descender == 0 {
            if os2.typo_asc != 0 || os2.typo_desc != 0 {
                self.ascender = os2.typo_asc;
                self.descender = os2.typo_desc;
            } else {
                self.ascender = os2.win_asc as i16;
                self.descender = -(os2.win_desc as i16);
            }
        }
    }

    /// `tt_face_load_post`, and the names of `load_post_names`.
    fn load_post(&mut self) {
        let Some((p, post_len)) = self.table(b"post") else {
            return;
        };
        let d = self.data.clone();
        let Some(header) = d.get(p..p + 32) else {
            return;
        };
        let format = u32_at(header, 0).unwrap_or(0);
        self.post_format = Some(format);
        self.fixed_pitch = u32_at(header, 12).unwrap_or(0) != 0;
        if post_len < 34 {
            return;
        }
        let n = usize::from(u16_at(&d, p + 32).unwrap_or(0));
        if n as u32 > self.num_glyphs || n == 0 {
            return;
        }
        let rest = post_len - 34;
        let at = p + 34;
        self.post = match format {
            0x0002_0000 if n * 2 <= rest => {
                let idx: Vec<u16> = (0..n)
                    .map(|i| u16_at(&d, at + i * 2).unwrap_or(0))
                    .collect();
                let max = idx.iter().copied().max().unwrap_or(0);
                let num_names = usize::from(max.saturating_sub(257));
                let strings = d.get(at + n * 2..p + post_len).unwrap_or_default();
                // FreeType overwrites each length byte it reads with a
                // NUL, and the end of the table: a name runs to the next
                // of those, or to a NUL of its own.
                let mut starts = Vec::with_capacity(num_names);
                let mut q = 0;
                while q < strings.len() && starts.len() < num_names {
                    starts.push(q);
                    q += 1 + usize::from(strings[q]);
                }
                let mut names: Vec<Vec<u8>> = (0..starts.len())
                    .map(|i| {
                        let end = starts.get(i + 1).copied().unwrap_or(strings.len());
                        let s = strings.get(starts[i] + 1..end).unwrap_or_default();
                        let nul = s.iter().position(|&b| b == 0).unwrap_or(s.len());
                        s[..nul].to_vec()
                    })
                    .collect();
                names.resize(num_names, Vec::new());
                Some(Post(idx, names))
            }
            0x0002_5000 if n <= rest && n <= 258 + 128 => {
                let idx = (0..n)
                    .map(|i| {
                        let off = i64::from(d.get(at + i).map_or(0, |&b| b as i8));
                        let v = i as i64 + off;
                        if (0..=257).contains(&v) { v as u16 } else { 0 }
                    })
                    .collect();
                Some(Post(idx, Vec::new()))
            }
            _ => None,
        };
    }

    /// Whether FreeType gives the face glyph names: a `post` header that
    /// loads, of a known format other than 3.
    pub fn has_glyph_names(&self) -> bool {
        matches!(
            self.post_format,
            Some(0x0001_0000 | 0x0002_0000 | 0x0002_5000)
        )
    }

    /// `tt_face_get_ps_name` for a glyph below the count: `.notdef` where
    /// the table has no name.
    fn ps_name(&self, gid: u32) -> &[u8] {
        let mac = |i: usize| {
            names::MAC_NAMES
                .get(i)
                .map_or(&b".notdef"[..], |n| n.as_bytes())
        };
        match (self.post_format, &self.post) {
            (Some(0x0002_0000 | 0x0002_5000), Some(Post(idx, strings))) => {
                match idx.get(gid as usize) {
                    Some(&i) if i < 258 => mac(usize::from(i)),
                    Some(&i) => strings
                        .get(usize::from(i) - 258)
                        .map_or(&[][..], Vec::as_slice),
                    None => mac(0),
                }
            }
            (Some(0x0001_0000), _) if self.num_glyphs == 258 => mac(gid as usize),
            _ => mac(0),
        }
    }

    /// `FT_Get_Glyph_Name`: None for a face without names or a glyph past
    /// its count.
    pub fn glyph_name(&self, gid: u32) -> Option<Vec<u8>> {
        (gid < self.num_glyphs && self.has_glyph_names()).then(|| self.ps_name(gid).to_vec())
    }

    /// `FT_Get_Name_Index` through the `post` names.
    pub fn name_index(&self, name: &[u8]) -> u32 {
        if !self.has_glyph_names() {
            return 0;
        }
        (0..self.num_glyphs)
            .find(|&g| self.ps_name(g) == name)
            .unwrap_or(0)
    }

    /// `tt_face_get_metrics`: the left side bearing in font units.
    pub fn lsb(&self, gid: u32) -> i64 {
        let Some((o, l)) = self.hmtx else {
            return 0;
        };
        let n = self.num_hmetrics;
        let g = gid as usize;
        let at = if g < n {
            g * 4 + 2
        } else if n > 0 {
            n * 4 + (g - n) * 2
        } else {
            return 0;
        };
        if at + 2 > l {
            return 0;
        }
        u16_at(&self.data, o + at).map_or(0, |v| i64::from(v as i16))
    }

    /// `tt_face_get_metrics`: the advance in font units.
    pub fn advance(&self, gid: u32) -> u16 {
        let Some((o, l)) = self.hmtx else {
            return 0;
        };
        let n = self.num_hmetrics;
        // A long metric is read whole, the last one's advance alone.
        let (at, need) = if (gid as usize) < n {
            (gid as usize * 4, 4)
        } else if n > 0 {
            ((n - 1) * 4, 2)
        } else {
            return 0;
        };
        if at + need > l {
            return 0;
        }
        u16_at(&self.data, o + at).unwrap_or(0)
    }

    /// `tt_face_build_cmaps`: each subtable of a known format that passes
    /// FreeType's default validation.
    fn load_cmaps(&mut self) {
        let Some((t, tl)) = self.cmap else {
            return;
        };
        if tl < 4 {
            return;
        }
        let d = self.data.clone();
        let limit = t + tl;
        let mut n = usize::from(u16_at(&d, t + 2).unwrap_or(0));
        let mut p = t + 4;
        while n > 0 && p + 8 <= limit {
            n -= 1;
            let platform_id = u16_at(&d, p).unwrap_or(0);
            let encoding_id = u16_at(&d, p + 2).unwrap_or(0);
            let offset = u32_at(&d, p + 4).unwrap_or(0) as usize;
            p += 8;
            if offset == 0 || offset > tl - 2 {
                continue;
            }
            let at = t + offset;
            let format = u16_at(&d, at).unwrap_or(0);
            let Some((unsorted, overlapping)) = self.validate(format, at, limit) else {
                continue;
            };
            self.subtables.push(Subtable {
                platform_id,
                encoding_id,
                format,
                at,
                unsorted,
                overlapping,
            });
        }
    }

    /// The `validate` of each format at FT_VALIDATE_DEFAULT: None for a
    /// subtable FreeType drops; for format 4, whether its segments are
    /// unsorted or overlap.
    fn validate(&self, format: u16, at: usize, limit: usize) -> Option<(bool, bool)> {
        // Reads stop at the end of the `cmap` table.
        let d = self.data.get(..limit)?;
        let ok = Some((false, false));
        match format {
            0 => {
                let length = usize::from(u16_at(d, at + 2)?);
                (at + length <= limit && length >= 262).then_some((false, false))
            }
            2 => {
                let length = usize::from(u16_at(d, at + 2)?);
                if at + length > limit || length < 6 + 512 {
                    return None;
                }
                let max_subs = (0..256)
                    .map(|n| u16_at(d, at + 6 + n * 2).map_or(0, |k| usize::from(k >> 3)))
                    .max()
                    .unwrap_or(0);
                let subs = at + 518;
                let glyph_ids = subs + (max_subs + 1) * 8;
                if glyph_ids > limit {
                    return None;
                }
                for n in 0..=max_subs {
                    let s = subs + n * 8;
                    let count = usize::from(u16_at(d, s + 2)?);
                    let offset = usize::from(u16_at(d, s + 6)?);
                    if count == 0 || offset == 0 {
                        continue;
                    }
                    let ids = s + 6 + offset;
                    if ids < glyph_ids || ids + count * 2 > at + length {
                        return None;
                    }
                }
                ok
            }
            4 => {
                if at + 4 > limit {
                    return None;
                }
                let length = limit - at;
                if length < 16 {
                    return None;
                }
                let num_segs = usize::from(u16_at(d, at + 6)?) / 2;
                if length < 16 + num_segs * 8 {
                    return None;
                }
                let ends = at + 14;
                let starts = at + 16 + num_segs * 2;
                let deltas = starts + num_segs * 2;
                let offsets = deltas + num_segs * 2;
                let glyph_ids = offsets + num_segs * 2;
                let (mut last_start, mut last_end) = (0u32, 0u32);
                let (mut unsorted, mut overlapping) = (false, false);
                for n in 0..num_segs {
                    let start = u32::from(u16_at(d, starts + n * 2)?);
                    let end = u32::from(u16_at(d, ends + n * 2)?);
                    let offset = usize::from(u16_at(d, offsets + n * 2)?);
                    if start > end {
                        return None;
                    }
                    if start <= last_end && n > 0 {
                        if last_start > start || last_end > end {
                            unsorted = true;
                        } else {
                            overlapping = true;
                        }
                    }
                    let last_ffff = n == num_segs - 1 && start == 0xffff && end == 0xffff;
                    if offset != 0 && offset != 0xffff {
                        let p = offsets + n * 2 + offset;
                        if !last_ffff
                            && (p < glyph_ids || p + (end - start + 1) as usize * 2 > limit)
                        {
                            return None;
                        }
                    }
                    // A range offset of 0xFFFF may only close the table.
                    if offset == 0xffff && !last_ffff {
                        return None;
                    }
                    last_start = start;
                    last_end = end;
                }
                Some((unsorted, overlapping))
            }
            6 => {
                if at + 10 > limit {
                    return None;
                }
                let length = usize::from(u16_at(d, at + 2)?);
                let count = usize::from(u16_at(d, at + 8)?);
                (at + length <= limit && length >= 10 + count * 2).then_some((false, false))
            }
            10 => {
                if at + 20 > limit {
                    return None;
                }
                let length = u32_at(d, at + 4)? as usize;
                let count = u32_at(d, at + 16)? as usize;
                (length <= limit - at && length >= 20 && (length - 20) / 2 >= count)
                    .then_some((false, false))
            }
            12 | 13 => {
                if at + 16 > limit {
                    return None;
                }
                let length = u32_at(d, at + 4)? as usize;
                let groups = u32_at(d, at + 12)? as usize;
                if length > limit - at || length < 16 || (length - 16) / 12 < groups {
                    return None;
                }
                let mut last = 0u32;
                for g in 0..groups {
                    let s = u32_at(d, at + 16 + g * 12)?;
                    let e = u32_at(d, at + 16 + g * 12 + 4)?;
                    if s > e || (g > 0 && s <= last) {
                        return None;
                    }
                    last = e;
                }
                ok
            }
            14 => {
                if at + 10 > limit {
                    return None;
                }
                let length = u32_at(d, at + 2)? as usize;
                let selectors = u32_at(d, at + 6)? as usize;
                if length > limit - at || length < 10 || (length - 10) / 11 < selectors {
                    return None;
                }
                let mut last_sel = 1;
                for n in 0..selectors {
                    let r = at + 10 + n * 11;
                    let sel = u24_at(d, r)?;
                    let def = u32_at(d, r + 3)? as usize;
                    let nondef = u32_at(d, r + 7)? as usize;
                    if def >= length || nondef >= length || sel < last_sel {
                        return None;
                    }
                    last_sel = sel + 1;
                    if def != 0 {
                        let p = at + def;
                        let ranges = u32_at(d, p)? as usize;
                        if ranges > (limit - (p + 4)) / 4 {
                            return None;
                        }
                        let mut last_base = 0;
                        for i in 0..ranges {
                            let base = u24_at(d, p + 4 + i * 4)?;
                            let cnt = u32::from(*d.get(p + 7 + i * 4)?);
                            if base + cnt >= 0x11_0000 || base < last_base {
                                return None;
                            }
                            last_base = base + cnt + 1;
                        }
                    }
                    if nondef != 0 {
                        let p = at + nondef;
                        let mappings = u32_at(d, p)? as usize;
                        if mappings > (limit - (p + 4)) / 5 {
                            return None;
                        }
                        let mut last_uni = 0;
                        for i in 0..mappings {
                            let uni = u24_at(d, p + 4 + i * 5)?;
                            if uni >= 0x11_0000 || uni < last_uni {
                                return None;
                            }
                            last_uni = uni + 1;
                        }
                    }
                }
                ok
            }
            _ => None,
        }
    }

    /// The `char_index` of a subtable.
    pub fn char_index(&self, sub: &Subtable, code: u32) -> u32 {
        let d = &self.data;
        let at = sub.at;
        let g = match sub.format {
            0 => {
                if code < 256 {
                    d.get(at + 6 + code as usize).map_or(0, |&b| u32::from(b))
                } else {
                    0
                }
            }
            2 => self.cmap2(at, code),
            4 => {
                if code >= 0x10000 {
                    0
                } else {
                    self.cmap4(sub, code)
                }
            }
            6 => {
                let first = u32::from(u16_at(d, at + 6).unwrap_or(0));
                let count = u32::from(u16_at(d, at + 8).unwrap_or(0));
                let idx = code.wrapping_sub(first);
                if idx < count {
                    u32::from(u16_at(d, at + 10 + idx as usize * 2).unwrap_or(0))
                } else {
                    0
                }
            }
            10 => {
                let first = u32_at(d, at + 12).unwrap_or(0);
                let count = u32_at(d, at + 16).unwrap_or(0);
                let idx = code.wrapping_sub(first);
                if idx < count {
                    u32::from(u16_at(d, at + 20 + idx as usize * 2).unwrap_or(0))
                } else {
                    0
                }
            }
            12 | 13 => {
                let groups = u32_at(d, at + 12).unwrap_or(0) as usize;
                let (mut lo, mut hi) = (0usize, groups);
                let mut out = 0;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let g = at + 16 + mid * 12;
                    let s = u32_at(d, g).unwrap_or(0);
                    let e = u32_at(d, g + 4).unwrap_or(0);
                    if code < s {
                        hi = mid;
                    } else if code > e {
                        lo = mid + 1;
                    } else {
                        let start_id = u32_at(d, g + 8).unwrap_or(0);
                        // A glyph id past 32 bits is no glyph.
                        out = if sub.format == 12 {
                            start_id.checked_add(code - s).unwrap_or(0)
                        } else {
                            start_id
                        };
                        break;
                    }
                }
                out
            }
            _ => 0,
        };
        if g >= self.num_glyphs { 0 } else { g }
    }

    /// `tt_cmap2_char_index`.
    fn cmap2(&self, at: usize, code: u32) -> u32 {
        let d = &self.data;
        if code >= 0x10000 {
            return 0;
        }
        let (hi, lo) = (code >> 8, code & 0xff);
        let sub = |i: u32| u16_at(d, at + 6 + i as usize * 2).unwrap_or(0) / 8;
        let k = if hi == 0 {
            let k = sub(lo);
            if k != 0 {
                return 0;
            }
            0
        } else {
            let k = sub(hi);
            if k == 0 {
                return 0;
            }
            k
        };
        let header = at + 6 + 512 + usize::from(k) * 8;
        // The low byte indexes the subheader either way.
        let char_code = lo;
        let first = u32::from(u16_at(d, header).unwrap_or(0));
        let count = u32::from(u16_at(d, header + 2).unwrap_or(0));
        let delta = i32::from(i16_at(d, header + 4).unwrap_or(0));
        let offset = usize::from(u16_at(d, header + 6).unwrap_or(0));
        let idx = char_code.wrapping_sub(first);
        if idx >= count || offset == 0 {
            return 0;
        }
        let p = header + 6 + offset + idx as usize * 2;
        let g = u32::from(u16_at(d, p).unwrap_or(0));
        if g == 0 {
            return 0;
        }
        ((g as i32 + delta) & 0xffff) as u32
    }

    /// `tt_cmap4_char_map_linear` and `_binary`, for one code.
    fn cmap4(&self, sub: &Subtable, code: u32) -> u32 {
        let d = &self.data;
        let at = sub.at;
        let limit = self.cmap.map_or(0, |(t, l)| t + l);
        let num_segs = usize::from(u16_at(d, at + 6).unwrap_or(0) >> 1);
        if num_segs == 0 {
            return 0;
        }
        let seg = |i: usize| {
            let end = u32::from(u16_at(d, at + 14 + i * 2).unwrap_or(0));
            let start = u32::from(u16_at(d, at + 16 + num_segs * 2 + i * 2).unwrap_or(0));
            let delta = i32::from(i16_at(d, at + 16 + num_segs * 4 + i * 2).unwrap_or(0));
            let offset_at = at + 16 + num_segs * 6 + i * 2;
            let offset = usize::from(u16_at(d, offset_at).unwrap_or(0));
            (start, end, delta, offset, offset_at)
        };
        // Some fonts get the closing segment wrong: one pointing past
        // the table maps 0xFFFF to glyph 0.
        let fixed = |i: usize| {
            let (start, end, delta, offset, offset_at) = seg(i);
            if i == num_segs - 1
                && start == 0xffff
                && end == 0xffff
                && offset != 0
                && offset_at + offset + 2 > limit
            {
                (start, end, 1, 0, offset_at)
            } else {
                (start, end, delta, offset, offset_at)
            }
        };
        let chosen = if sub.unsorted {
            // tt_cmap4_char_map_linear: in table order, until a segment
            // starts past the code.
            let mut chosen = None;
            for i in 0..num_segs {
                let s = fixed(i);
                if code < s.0 {
                    break;
                }
                if code <= s.1 && s.3 != 0xffff {
                    chosen = Some(s);
                    break;
                }
            }
            chosen
        } else {
            // tt_cmap4_char_map_binary.
            let (mut min, mut max) = (0usize, num_segs);
            let mut hit = None;
            while min < max {
                let mid = (min + max) / 2;
                let (s, e, ..) = seg(mid);
                if code < s {
                    max = mid;
                } else if code > e {
                    min = mid + 1;
                } else {
                    hit = Some(mid);
                    break;
                }
            }
            let Some(hit) = hit else {
                return 0;
            };
            if sub.overlapping {
                // The first segment that has the code and maps it. (FreeType
                // would also look past the one found, but only the closing
                // segment may be marked 0xFFFF, and none follows it.)
                let mut m = (fixed(hit).3 != 0xffff).then_some(hit);
                let mut i = hit;
                while i > 0 && code <= seg(i - 1).1 {
                    if seg(i - 1).3 != 0xffff {
                        m = Some(i - 1);
                    }
                    i -= 1;
                }
                let Some(m) = m else {
                    return 0;
                };
                // The found segment keeps its correction only when no
                // other was read after it.
                Some(if m == hit && i == hit {
                    fixed(m)
                } else {
                    seg(m)
                })
            } else {
                Some(fixed(hit)).filter(|s| s.3 != 0xffff)
            }
        };
        let Some((start, _, delta, offset, offset_at)) = chosen else {
            return 0;
        };
        if offset != 0 {
            let p = offset_at + offset + (code - start) as usize * 2;
            let g = u32::from(u16_at(d, p).unwrap_or(0));
            if g == 0 {
                return 0;
            }
            let g = ((g as i32 + delta) & 0xffff) as u32;
            if g >= self.num_glyphs { 0 } else { g }
        } else {
            ((code as i32 + delta) & 0xffff) as u32
        }
    }
}

/// `sfnt_find_encoding`: whether a platform and encoding are Unicode or
/// Microsoft Symbol.
pub fn encoding_of(platform_id: u16, encoding_id: u16) -> super::Encoding {
    use super::Encoding;
    match (platform_id, encoding_id) {
        // Apple Unicode and ISO 10646, whatever the encoding.
        (0 | 2, _) => Encoding::Unicode,
        (3, 0) => Encoding::MsSymbol,
        (3, 1) | (3, 10) => Encoding::Unicode,
        (1, 0) => Encoding::AppleRoman,
        _ => Encoding::Other,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! TrueType files built table by table. The expected glyphs, advances
    //! and errors are what MuPDF 1.28.6 (FreeType 2.13) gives for the same
    //! bytes, through `new Font(name, file)` in `mutool run`.

    use super::*;
    use std::sync::Arc;

    pub(crate) type Tables = Vec<(&'static [u8; 4], Vec<u8>)>;

    pub(crate) fn be16(v: &[i64]) -> Vec<u8> {
        v.iter().flat_map(|&x| (x as u16).to_be_bytes()).collect()
    }

    pub(crate) fn be32(v: &[i64]) -> Vec<u8> {
        v.iter().flat_map(|&x| (x as u32).to_be_bytes()).collect()
    }

    /// An sfnt of the tables in the order given, its directory at `base`
    /// of the file (the tables follow it, each padded to four bytes).
    pub(crate) fn sfnt_at(base: usize, tag: &[u8; 4], tables: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend(be16(&[tables.len() as i64, 0, 0, 0]));
        let mut offset = base + 12 + 16 * tables.len();
        let mut body = Vec::new();
        for (tag, data) in tables {
            out.extend_from_slice(*tag);
            out.extend(be32(&[0, offset as i64, data.len() as i64]));
            body.extend_from_slice(data);
            while body.len() % 4 != 0 {
                body.push(0);
            }
            offset = base + 12 + 16 * tables.len() + body.len();
        }
        out.extend(body);
        out
    }

    pub(crate) fn ttf(tables: &[(&[u8; 4], Vec<u8>)]) -> Arc<[u8]> {
        sfnt_at(0, &[0, 1, 0, 0], tables).into()
    }

    /// A collection of one font.
    pub(crate) fn ttc(tag: &[u8; 4], tables: &[(&[u8; 4], Vec<u8>)]) -> Arc<[u8]> {
        let mut out = b"ttcf".to_vec();
        out.extend(be32(&[0x1_0000, 1, 16]));
        out.extend(sfnt_at(16, tag, tables));
        out.into()
    }

    pub(crate) fn head(upem: u16) -> Vec<u8> {
        let mut h = vec![0; 54];
        h[0..4].copy_from_slice(&[0, 1, 0, 0]);
        h[12..16].copy_from_slice(&0x5F0F_3CF5u32.to_be_bytes());
        h[18..20].copy_from_slice(&upem.to_be_bytes());
        h[36..44].copy_from_slice(&be16(&[-50, -250, 1000, 800]));
        h
    }

    pub(crate) fn with16(mut t: Vec<u8>, at: usize, v: i64) -> Vec<u8> {
        t[at..at + 2].copy_from_slice(&(v as u16).to_be_bytes());
        t
    }

    pub(crate) fn maxp(glyphs: u16) -> Vec<u8> {
        let mut m = vec![0, 0, 0x50, 0];
        m.extend_from_slice(&glyphs.to_be_bytes());
        m
    }

    pub(crate) fn hhea(asc: i16, desc: i16, metrics: u16) -> Vec<u8> {
        let mut h = vec![0; 36];
        h[0..4].copy_from_slice(&[0, 1, 0, 0]);
        h = with16(h, 4, i64::from(asc));
        h = with16(h, 6, i64::from(desc));
        with16(h, 34, i64::from(metrics))
    }

    /// Long metrics (advance, lsb), then left side bearings alone.
    pub(crate) fn hmtx(long: &[(u16, i16)], lsbs: &[i16]) -> Vec<u8> {
        let mut out = Vec::new();
        for &(a, l) in long {
            out.extend(be16(&[i64::from(a), i64::from(l)]));
        }
        for &l in lsbs {
            out.extend(be16(&[i64::from(l)]));
        }
        out
    }

    /// An OS/2 table of version 0.
    pub(crate) fn os2(
        family_class: i16,
        fs_selection: u16,
        typo: (i16, i16),
        win: (u16, u16),
    ) -> Vec<u8> {
        let mut t = vec![0; 78];
        t = with16(t, 30, i64::from(family_class));
        t = with16(t, 62, i64::from(fs_selection));
        t = with16(t, 68, i64::from(typo.0));
        t = with16(t, 70, i64::from(typo.1));
        t = with16(t, 74, i64::from(win.0));
        with16(t, 76, i64::from(win.1))
    }

    /// A `post` header of `format`, then `rest`.
    pub(crate) fn post(format: u32, fixed: bool, rest: &[u8]) -> Vec<u8> {
        let mut t = format.to_be_bytes().to_vec();
        t.extend([0; 8]);
        t.extend(be32(&[i64::from(fixed)]));
        t.extend([0; 16]);
        t.extend_from_slice(rest);
        t
    }

    /// The glyph name part of a format 2 `post`: an index per glyph and
    /// the Pascal strings.
    pub(crate) fn post2(idx: &[u16], strings: &[&[u8]]) -> Vec<u8> {
        let mut t = be16(&[idx.len() as i64]);
        t.extend(be16(&idx.iter().map(|&i| i64::from(i)).collect::<Vec<_>>()));
        for s in strings {
            t.push(s.len() as u8);
            t.extend_from_slice(s);
        }
        t
    }

    /// A `cmap` of the subtables (platform, encoding, data), in order.
    pub(crate) fn cmap(subs: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
        let mut t = be16(&[0, subs.len() as i64]);
        let mut offset = 4 + 8 * subs.len();
        let mut body = Vec::new();
        for (p, e, data) in subs {
            t.extend(be16(&[i64::from(*p), i64::from(*e)]));
            t.extend(be32(&[offset as i64]));
            body.extend_from_slice(data);
            offset += data.len();
        }
        t.extend(body);
        t
    }

    pub(crate) fn fmt0(map: &[(u8, u8)]) -> Vec<u8> {
        let mut t = be16(&[0, 262, 0]);
        let mut glyphs = [0u8; 256];
        for &(c, g) in map {
            glyphs[usize::from(c)] = g;
        }
        t.extend(glyphs);
        t
    }

    /// A format 2 subheader: first code, glyphs, delta.
    pub(crate) type Sub2<'a> = (u16, &'a [u16], i16);

    /// Format 2: one-byte codes through subheader 0, and the high bytes
    /// given, each to its own subheader.
    pub(crate) fn fmt2(low: Sub2<'_>, high: &[(u8, Sub2<'_>)]) -> Vec<u8> {
        let mut keys = [0u16; 256];
        for (i, (hi, _)) in high.iter().enumerate() {
            keys[usize::from(*hi)] = (i as u16 + 1) * 8;
        }
        let subs: Vec<Sub2<'_>> = std::iter::once(low)
            .chain(high.iter().map(|h| h.1))
            .collect();
        let mut headers = Vec::new();
        let mut arrays = Vec::new();
        let arrays_at = 518 + subs.len() * 8;
        for (i, (first, glyphs, delta)) in subs.iter().enumerate() {
            let range_at = 518 + i * 8 + 6;
            let offset = arrays_at + arrays.len() * 2 - range_at;
            headers.extend(be16(&[
                i64::from(*first),
                glyphs.len() as i64,
                i64::from(*delta),
                offset as i64,
            ]));
            arrays.extend_from_slice(glyphs);
        }
        let length = arrays_at + arrays.len() * 2;
        let mut t = be16(&[2, length as i64, 0]);
        t.extend(be16(&keys.map(i64::from)));
        t.extend(headers);
        t.extend(be16(
            &arrays.iter().map(|&g| i64::from(g)).collect::<Vec<_>>(),
        ));
        t
    }

    /// Format 4 segments (start, end, delta, glyphs through the range
    /// offset or none).
    pub(crate) fn fmt4(segs: &[(u16, u16, i16, Option<Vec<u16>>)]) -> Vec<u8> {
        let n = segs.len();
        let mut arrays: Vec<u16> = Vec::new();
        let mut offsets = Vec::new();
        for (i, s) in segs.iter().enumerate() {
            match &s.3 {
                Some(g) => {
                    // From this segment's offset entry to its glyphs.
                    offsets.push(((n - i) * 2 + arrays.len() * 2) as i64);
                    arrays.extend(g);
                }
                None => offsets.push(0),
            }
        }
        let length = 16 + n * 8 + arrays.len() * 2;
        let mut t = be16(&[4, length as i64, 0, 2 * n as i64, 0, 0, 0]);
        t.extend(be16(
            &segs.iter().map(|s| i64::from(s.1)).collect::<Vec<_>>(),
        ));
        t.extend(be16(&[0]));
        t.extend(be16(
            &segs.iter().map(|s| i64::from(s.0)).collect::<Vec<_>>(),
        ));
        t.extend(be16(
            &segs.iter().map(|s| i64::from(s.2)).collect::<Vec<_>>(),
        ));
        t.extend(be16(&offsets));
        t.extend(be16(
            &arrays.iter().map(|&g| i64::from(g)).collect::<Vec<_>>(),
        ));
        t
    }

    pub(crate) fn fmt6(first: u16, glyphs: &[u16]) -> Vec<u8> {
        let mut t = be16(&[
            6,
            10 + 2 * glyphs.len() as i64,
            0,
            i64::from(first),
            glyphs.len() as i64,
        ]);
        t.extend(be16(
            &glyphs.iter().map(|&g| i64::from(g)).collect::<Vec<_>>(),
        ));
        t
    }

    pub(crate) fn fmt10(first: u32, glyphs: &[u16]) -> Vec<u8> {
        let mut t = be16(&[10, 0]);
        t.extend(be32(&[
            20 + 2 * glyphs.len() as i64,
            0,
            i64::from(first),
            glyphs.len() as i64,
        ]));
        t.extend(be16(
            &glyphs.iter().map(|&g| i64::from(g)).collect::<Vec<_>>(),
        ));
        t
    }

    /// Format 12 or 13 groups (start, end, glyph).
    pub(crate) fn fmt12(format: u16, groups: &[(u32, u32, u32)]) -> Vec<u8> {
        let mut t = be16(&[i64::from(format), 0]);
        t.extend(be32(&[
            16 + 12 * groups.len() as i64,
            0,
            groups.len() as i64,
        ]));
        for &(s, e, g) in groups {
            t.extend(be32(&[i64::from(s), i64::from(e), i64::from(g)]));
        }
        t
    }

    /// The tables every face needs, for `glyphs` glyphs of 500 units
    /// (glyph g is 100 * g wider), a `post` of format 3 and a (3,1) map
    /// of A to glyph 1 when `cmap` is None.
    pub(crate) fn base(glyphs: u16, cmap_table: Option<Vec<u8>>) -> Tables {
        let long: Vec<(u16, i16)> = (0..glyphs).map(|g| (500 + 100 * g, g as i16)).collect();
        vec![
            (b"head", head(1000)),
            (b"hhea", hhea(800, -200, glyphs)),
            (b"maxp", maxp(glyphs)),
            (b"hmtx", hmtx(&long, &[])),
            (
                b"cmap",
                cmap_table.unwrap_or_else(|| {
                    cmap(&[(
                        3,
                        1,
                        fmt4(&[(0x41, 0x41, -0x40, None), (0xffff, 0xffff, 1, None)]),
                    )])
                }),
            ),
            (b"post", post(0x3_0000, false, &[])),
            // Empty glyphs, for the TrueType driver.
            (b"loca", vec![0; 2 * (usize::from(glyphs) + 1)]),
            (b"glyf", vec![0; 4]),
        ]
    }

    /// `tables` with `tag`'s data replaced, or added.
    pub(crate) fn set(mut tables: Tables, tag: &'static [u8; 4], data: Vec<u8>) -> Tables {
        match tables.iter_mut().find(|t| t.0 == tag) {
            Some(t) => t.1 = data,
            None => tables.push((tag, data)),
        }
        tables
    }

    pub(crate) fn without(tables: Tables, tag: &[u8; 4]) -> Tables {
        tables.into_iter().filter(|t| t.0 != tag).collect()
    }

    pub(crate) fn open(data: Arc<[u8]>) -> Sfnt {
        Sfnt::open(data).unwrap_or_else(|e| panic!("{e}"))
    }

    /// The glyph of each code through the font's only subtable.
    fn glyphs(sub: Vec<u8>, glyphs: u16, codes: &[u32]) -> Vec<u32> {
        let f = open(ttf(&base(glyphs, Some(cmap(&[(3, 1, sub)])))));
        let Some(s) = f.subtables.first() else {
            return Vec::new();
        };
        codes.iter().map(|&c| f.char_index(s, c)).collect()
    }

    #[test]
    fn a_truetype_file_gives_its_metrics_and_glyphs() {
        let f = open(ttf(&base(4, None)));
        assert_eq!((f.units_per_em, f.num_glyphs), (1000, 4));
        assert_eq!((f.ascender, f.descender), (800, -200));
        assert_eq!(f.bbox, [-50, -250, 1000, 800]);
        assert_eq!(f.format_tag, TAG_TRUETYPE);
        assert_eq!((f.advance(2), f.lsb(2)), (700, 2));
        let sub = f
            .subtables
            .first()
            .cloned()
            .unwrap_or_else(|| panic!("no cmap"));
        assert_eq!((sub.platform_id, sub.encoding_id, sub.format), (3, 1, 4));
        assert_eq!(f.char_index(&sub, 0x41), 1);
        assert_eq!(f.char_index(&sub, 0x42), 0);
    }

    #[test]
    fn freetype_refuses_what_sfnt_load_face_refuses() {
        let err = |t: Tables| Sfnt::open(ttf(&t)).err();
        assert_eq!(
            err(set(base(4, None), b"head", head(0))),
            Some("Invalid_Table")
        );
        assert_eq!(
            err(set(base(4, None), b"head", head(16385))),
            Some("Invalid_Table")
        );
        assert_eq!(err(set(base(4, None), b"head", head(16))), None);
        assert_eq!(
            err(without(base(4, None), b"hhea")),
            Some("Horiz_Header_Missing")
        );
        assert_eq!(
            err(without(base(4, None), b"hmtx")),
            Some("Hmtx_Table_Missing")
        );
        // check_table_dir: a header shorter than 54 bytes is no header.
        assert_eq!(
            err(set(base(4, None), b"head", vec![0; 40])),
            Some("Table_Missing")
        );
        assert_eq!(err(without(base(4, None), b"head")), Some("Table_Missing"));
        // A face without `maxp` loads, with no glyphs.
        let f = open(ttf(&without(base(4, None), b"maxp")));
        assert_eq!(f.num_glyphs, 0);
    }

    /// The closing segment of a format 4 subtable.
    const FFFF: (u16, u16, i16, Option<Vec<u16>>) = (0xffff, 0xffff, 1, None);

    #[test]
    fn each_cmap_format_maps_codes_as_freetype_does() {
        let f0 = fmt0(&[(0x41, 2), (0x42, 3), (0x43, 9)]);
        assert_eq!(
            glyphs(f0, 8, &[0x41, 0x42, 0x43, 0x44, 0x141]),
            [2, 3, 0, 0, 0]
        );
        // High bytes 0x81 and 0x82 lead two-byte codes; 0x82 has no
        // subheader of its own. Glyphs are 0 or the array's plus delta.
        let f2 = fmt2((0x20, &[1, 2, 0], 0), &[(0x81, (0x40, &[3, 2, 0, 4], 1))]);
        assert_eq!(
            glyphs(
                f2,
                8,
                &[
                    0x20, 0x21, 0x22, 0x23, 0x1f, 0x8140, 0x8141, 0x8142, 0x8143, 0x8144, 0x81,
                    0x8240, 0x10000
                ]
            ),
            [1, 2, 0, 0, 0, 4, 3, 0, 5, 0, 0, 0, 0]
        );
        let f4 = fmt4(&[
            (0x41, 0x43, 0, Some(vec![1, 0, 3])),
            (0x61, 0x62, -0x60, None),
            (0x70, 0x71, 3, Some(vec![2, 9])),
            FFFF,
        ]);
        assert_eq!(
            glyphs(
                f4,
                8,
                &[
                    0x41, 0x42, 0x43, 0x44, 0x61, 0x62, 0x63, 0x70, 0x71, 0x10041
                ]
            ),
            [1, 0, 3, 0, 1, 2, 0, 5, 0, 0]
        );
        let f6 = fmt6(0x41, &[1, 2, 3]);
        assert_eq!(
            glyphs(f6, 8, &[0x40, 0x41, 0x43, 0x44, 0x10041]),
            [0, 1, 3, 0, 0]
        );
        let f10 = fmt10(0x1_0000, &[1, 2]);
        assert_eq!(
            glyphs(f10, 8, &[0x41, 0xffff, 0x1_0000, 0x1_0001, 0x1_0002]),
            [0, 0, 1, 2, 0]
        );
        // A group whose glyphs would pass 32 bits maps nothing.
        let f12 = fmt12(
            12,
            &[
                (0x41, 0x43, 2),
                (0x1_f600, 0x1_f601, 5),
                (0x1_f700, 0x1_f702, 0xffff_ffff),
            ],
        );
        assert_eq!(
            glyphs(
                f12,
                8,
                &[
                    0x40, 0x41, 0x43, 0x44, 0x1_f600, 0x1_f601, 0x1_f602, 0x1_f700, 0x1_f702
                ]
            ),
            [0, 2, 4, 0, 5, 6, 0, 0, 0]
        );
        let f13 = fmt12(13, &[(0x41, 0x5a, 3)]);
        assert_eq!(glyphs(f13, 8, &[0x41, 0x5a, 0x5b]), [3, 3, 0]);
        // Format 14 maps no code by itself.
        let f14 = [be16(&[14]), be32(&[10, 0])].concat();
        assert_eq!(glyphs(f14, 8, &[0x41]), [0]);
    }

    #[test]
    fn overlapping_format_4_segments_take_the_first_that_maps_the_code() {
        // Sorted, overlapping: the first segment with the code that is not
        // marked 0xFFFF; past it, the last such after the one found.
        let f = fmt4(&[(0x41, 0x45, -0x40, None), (0x43, 0x48, -0x3e, None), FFFF]);
        assert_eq!(
            glyphs(f, 8, &(0x40..0x4a).collect::<Vec<_>>()),
            [0, 1, 2, 3, 4, 5, 0, 0, 0, 0]
        );
        // A range offset of 0xFFFF may only close the table: elsewhere the
        // subtable is dropped. On the closing segment it maps nothing,
        // unless an earlier segment has the code.
        let mut f = fmt4(&[(0x41, 0x45, -0x40, None), (0x43, 0x48, -0x3e, None), FFFF]);
        f.splice(34..36, be16(&[0xffff]));
        assert_eq!(glyphs(f, 8, &[0x41]), [] as [u32; 0]);
        let mut f = fmt4(&[(0xff00, 0xffff, 0x101, None), FFFF]);
        f.splice(30..32, be16(&[0xffff]));
        assert_eq!(glyphs(f, 300, &[0xff00, 0xffff]), [1, 256]);
        let mut f = fmt4(&[(0x41, 0x45, -0x40, None), FFFF]);
        f.splice(30..32, be16(&[0xffff]));
        assert_eq!(glyphs(f, 8, &[0x41, 0xffff]), [1, 0]);
        // Unsorted: in table order, until a segment starts past the code.
        let f = fmt4(&[(0x50, 0x55, -0x4e, None), (0x41, 0x45, -0x40, None), FFFF]);
        assert_eq!(
            glyphs(f, 8, &[0x41, 0x45, 0x46, 0x50, 0x55, 0x56]),
            [0, 0, 0, 2, 7, 0]
        );
        // A closing segment pointing past the table maps 0xFFFF to 0.
        let mut f = fmt4(&[(0x41, 0x41, -0x40, None), FFFF]);
        let last_offset = 16 + 2 * 6 + 2;
        f.splice(last_offset..last_offset + 2, be16(&[0x4000]));
        assert_eq!(glyphs(f, 8, &[0x41, 0xffff]), [1, 0]);
    }

    #[test]
    fn subtables_that_fail_validation_are_dropped() {
        let dropped = |sub: Vec<u8>| {
            let f = open(ttf(&base(8, Some(cmap(&[(3, 1, sub)])))));
            f.subtables.is_empty()
        };
        // Lengths shorter than the data, or past the table.
        assert!(dropped(with16(fmt0(&[]), 2, 200)));
        assert!(dropped(with16(fmt2((0x20, &[1, 2], 0), &[]), 2, 100)));
        assert!(dropped(with16(fmt6(0x41, &[1, 2, 3]), 2, 12)));
        let mut f10 = fmt10(0x41, &[1]);
        f10[4..8].copy_from_slice(&8u32.to_be_bytes());
        assert!(dropped(f10));
        let mut f12 = fmt12(12, &[(0x41, 0x43, 2)]);
        f12[4..8].copy_from_slice(&8u32.to_be_bytes());
        assert!(dropped(f12));
        // A format 2 subheader whose glyphs lie outside the array.
        let mut f2 = fmt2((0x20, &[1, 2], 0), &[]);
        f2.splice(518 + 6..518 + 8, be16(&[0x100]));
        assert!(dropped(f2));
        // Segments and groups out of order.
        assert!(dropped(fmt4(&[(0x45, 0x41, -0x40, None), FFFF])));
        assert!(dropped(fmt12(12, &[(0x45, 0x46, 2), (0x41, 0x43, 2)])));
        assert!(dropped(fmt12(13, &[(0x45, 0x44, 2)])));
        // Format 4 too short for its segments, or a range offset past
        // the table.
        assert!(dropped(be16(&[4, 16, 0, 2, 0, 0, 0])));
        assert!(dropped(with16(
            fmt4(&[(0x41, 0x41, -0x40, None), FFFF]),
            6,
            40
        )));
        let mut f4 = fmt4(&[(0x41, 0x42, 0, Some(vec![1, 2])), FFFF]);
        f4.splice(16 + 2 * 6..16 + 2 * 6 + 2, be16(&[0x400]));
        assert!(dropped(f4));
        // Format 14: its selectors, default and non-default tables.
        let f14 = |records: &[(u32, u32, u32)], tail: &[u8]| {
            let mut t = be16(&[14]);
            let length = 10 + 11 * records.len() + tail.len();
            t.extend(be32(&[length as i64, records.len() as i64]));
            for &(sel, def, nondef) in records {
                t.extend(&sel.to_be_bytes()[1..]);
                t.extend(be32(&[i64::from(def), i64::from(nondef)]));
            }
            t.extend_from_slice(tail);
            t
        };
        let ranges = |r: &[(u32, u8)]| {
            let mut t = be32(&[r.len() as i64]);
            for &(b, n) in r {
                t.extend(&b.to_be_bytes()[1..]);
                t.push(n);
            }
            t
        };
        let maps = |m: &[(u32, u16)]| {
            let mut t = be32(&[m.len() as i64]);
            for &(u, g) in m {
                t.extend(&u.to_be_bytes()[1..]);
                t.extend(g.to_be_bytes());
            }
            t
        };
        let def = ranges(&[(0x41, 2), (0x50, 0)]);
        let nondef = maps(&[(0x41, 1), (0x42, 2)]);
        let both = [def.clone(), nondef.clone()].concat();
        let good = f14(&[(0xfe00, 21, 21 + def.len() as u32)], &both);
        assert!(!dropped(good));
        assert!(dropped(f14(&[(0xfe00, 0, 0), (0xfe00, 0, 0)], &[])));
        assert!(dropped(f14(&[(0xfe00, 400, 0)], &[])));
        assert!(dropped(f14(
            &[(0xfe00, 21, 0)],
            &ranges(&[(0x10_fff0, 0x20)])
        )));
        assert!(dropped(f14(
            &[(0xfe00, 21, 0)],
            &ranges(&[(0x50, 0), (0x41, 0)])
        )));
        assert!(dropped(f14(&[(0xfe00, 21, 0)], &be32(&[1000]))));
        assert!(dropped(f14(&[(0xfe00, 0, 21)], &maps(&[(0x11_0000, 1)]))));
        assert!(dropped(f14(
            &[(0xfe00, 0, 21)],
            &maps(&[(0x42, 1), (0x41, 1)])
        )));
        assert!(dropped(f14(&[(0xfe00, 0, 21)], &be32(&[1000]))));
        assert!(dropped(with16(f14(&[], &[]), 4, 4)));
        // Unknown formats, and records pointing nowhere.
        assert!(dropped(be16(&[8, 0, 0, 0])));
        let f = open(ttf(&base(
            8,
            Some(
                [
                    be16(&[0, 2, 3, 1]),
                    be32(&[0]),
                    be16(&[3, 1]),
                    be32(&[0xfff]),
                ]
                .concat(),
            ),
        )));
        assert!(f.subtables.is_empty());
        // Subtables too short for their headers, at the end of the table.
        for format in [0, 2, 4, 6, 10, 12, 13, 14] {
            let c = [be16(&[0, 1, 3, 1]), be32(&[12]), be16(&[format])].concat();
            let f = open(ttf(&base(8, Some(c))));
            assert!(f.subtables.is_empty(), "format {format}");
        }
        // Format 2 subheaders past the table; one of no codes is fine.
        let mut f2 = fmt2((0x20, &[1, 2], 0), &[]);
        f2.splice(6 + 0x81 * 2..6 + 0x82 * 2, be16(&[200 * 8]));
        assert!(dropped(f2));
        assert!(!dropped(fmt2((0x20, &[], 0), &[])));
        // A format 4 subtable of no segments maps nothing.
        assert_eq!(glyphs(be16(&[4, 16, 0, 0, 0, 0, 0, 0]), 8, &[0x41]), [0]);
        // A cmap too short for its header has no subtables.
        let f = open(ttf(&base(8, Some(vec![0, 0]))));
        assert!(f.subtables.is_empty());
    }

    #[test]
    fn post_tables_name_glyphs_as_freetype_does() {
        let names = |n: u16, post_table: Vec<u8>| {
            let f = open(ttf(&set(base(n, None), b"post", post_table)));
            let all: Vec<String> = (0..u32::from(n))
                .map(|g| {
                    f.glyph_name(g)
                        .map_or("-".into(), |v| String::from_utf8_lossy(&v).into_owned())
                })
                .collect();
            (f.has_glyph_names(), all)
        };
        // Format 2: Macintosh names below 258, the table's own above.
        let (has, all) = names(
            6,
            post(
                0x2_0000,
                false,
                &post2(&[0, 36, 258, 259, 260, 3], &[b"uni0042", b"a.sc", b"a"]),
            ),
        );
        assert!(has);
        assert_eq!(all, [".notdef", "A", "uni0042", "a.sc", "a", "space"]);
        // Missing strings are empty; a name runs to the next length byte
        // FreeType reads, or a NUL.
        let mut tail = post2(&[0, 258, 259, 260, 261, 262], &[b"uni0041", b"B"]);
        tail.extend([1, b'C', 0, 9, b'D', b'x']);
        let (_, all) = names(6, post(0x2_0000, false, &tail));
        assert_eq!(all, [".notdef", "uni0041", "B", "C", "", "Dx"]);
        let (_, all) = names(
            4,
            post(0x2_0000, false, &post2(&[0, 258, 3, 3], &[b"A", b"x"])),
        );
        assert_eq!(all, [".notdef", "A\u{1}x", "space", "space"]);
        // More names than glyphs, or index arrays past the table: none.
        // Glyphs past the table's own count are .notdef.
        let (_, all) = names(4, post(0x2_0000, false, &post2(&[0, 36, 37], &[])));
        assert_eq!(all, [".notdef", "A", "B", ".notdef"]);
        let (has, all) = names(3, post(0x2_0000, false, &post2(&[0, 36, 37, 38], &[])));
        assert!(has);
        assert_eq!(all, [".notdef"; 3]);
        let (_, all) = names(3, post(0x2_0000, false, &be16(&[3, 36])));
        assert_eq!(all, [".notdef"; 3]);
        // Format 2.5: offsets from the glyph's own index.
        let mut tail = be16(&[6]);
        tail.extend([0u8, 35, 34, 0x80, 64, 2]);
        let (_, all) = names(6, post(0x2_5000, false, &tail));
        assert_eq!(all, [".notdef", "A", "A", ".notdef", "a", "dollar"]);
        // Format 1 names only fonts of exactly 258 glyphs.
        let (has, all) = names(6, post(0x1_0000, false, &[]));
        assert!(has);
        assert_eq!(all, [".notdef"; 6]);
        let f = open(ttf(&set(
            base(258, None),
            b"post",
            post(0x1_0000, false, &[]),
        )));
        assert_eq!(f.glyph_name(36).as_deref(), Some(&b"A"[..]));
        assert_eq!(f.name_index(b"A"), 36);
        assert_eq!(f.name_index(b"nonesuch"), 0);
        // Format 3 and unknown formats give no names; a header that
        // loads still says whether the font is fixed pitch.
        assert_eq!(
            names(3, post(0x3_0000, false, &[])),
            (false, vec!["-".to_string(); 3])
        );
        let f = open(ttf(&set(base(3, None), b"post", post(0x4_0000, true, &[]))));
        assert!(!f.has_glyph_names() && f.fixed_pitch);
        assert_eq!(f.name_index(b".notdef"), 0);
        // A header past the end of the data does not load.
        let mut t = without(base(3, None), b"post");
        t.push((b"post", vec![0, 2, 0, 0]));
        let f = open(ttf(&t));
        assert!(!f.has_glyph_names());
        assert_eq!(f.glyph_name(9), None);
    }

    #[test]
    fn horizontal_metrics_read_as_tt_face_get_metrics() {
        let f = open(ttf(&set(
            set(base(6, None), b"hhea", hhea(800, -200, 2)),
            b"hmtx",
            hmtx(&[(300, 1), (400, 2)], &[3, 4]),
        )));
        let adv: Vec<u16> = (0..6).map(|g| f.advance(g)).collect();
        let lsb: Vec<i64> = (0..6).map(|g| f.lsb(g)).collect();
        assert_eq!(adv, [300, 400, 400, 400, 400, 400]);
        assert_eq!(lsb, [1, 2, 3, 4, 0, 0]);
        // A long metric is read whole: six bytes give one.
        let f = open(ttf(&set(base(4, None), b"hmtx", be16(&[500, 7, 600]))));
        assert_eq!(
            (f.advance(0), f.advance(1), f.lsb(0), f.lsb(1)),
            (500, 0, 7, 0)
        );
        // No long metrics, no metrics at all.
        let f = open(ttf(&set(base(4, None), b"hhea", hhea(800, -200, 0))));
        assert_eq!((f.advance(1), f.lsb(1)), (0, 0));
        // A Macintosh font may lack `hhea`, and has no metrics then.
        let f = open(sfnt_at(0, b"true", &without(base(4, None), b"hhea")).into());
        assert_eq!((f.advance(1), f.lsb(1), f.ascender), (0, 0, 0));
    }

    #[test]
    fn os2_gives_the_style_and_the_metrics_hhea_lacks() {
        let with_os2 = |t: Tables, table: Vec<u8>| open(ttf(&set(t, b"OS/2", table)));
        let f = with_os2(base(4, None), os2(8 << 8, 1 | 32, (700, -300), (900, 100)));
        assert_eq!((f.bold, f.italic, f.family_class), (true, true, Some(2048)));
        assert_eq!((f.ascender, f.descender), (800, -200));
        let f = with_os2(base(4, None), os2(0, 512, (0, 0), (0, 0)));
        assert_eq!((f.bold, f.italic), (false, true));
        // USE_TYPO_METRICS takes the typographic values.
        let f = with_os2(base(4, None), os2(0, 128, (700, -300), (900, 100)));
        assert_eq!((f.ascender, f.descender), (700, -300));
        // A zero hhea takes them too, else the Windows ones.
        let zero = set(base(4, None), b"hhea", hhea(0, 0, 4));
        let f = with_os2(zero.clone(), os2(0, 0, (700, -300), (900, 100)));
        assert_eq!((f.ascender, f.descender), (700, -300));
        let f = with_os2(zero, os2(0, 0, (0, 0), (900, 100)));
        assert_eq!((f.ascender, f.descender), (900, -100));
        // Without outlines, the head's macStyle gives the style.
        let f = with_os2(
            without(base(4, None), b"glyf"),
            os2(0, 1 | 32, (0, 0), (0, 0)),
        );
        assert_eq!((f.bold, f.italic), (false, false));
        let f = open(ttf(&set(base(4, None), b"head", with16(head(1000), 44, 3))));
        assert_eq!((f.bold, f.italic, f.family_class), (true, true, None));
        // A version 1 table needs its 86 bytes; the last table's are not
        // there.
        let mut t = base(4, None);
        t.push((b"OS/2", with16(os2(0, 32, (0, 0), (0, 0)), 0, 1)));
        let f = open(ttf(&t));
        assert_eq!((f.bold, f.family_class), (false, None));
        for version in [2, 5] {
            let mut table = with16(os2(0, 32, (0, 0), (0, 0)), 0, version);
            table.resize(100, 0);
            assert!(with_os2(base(4, None), table).bold);
        }
    }

    #[test]
    fn the_directory_keeps_the_tables_freetype_keeps() {
        let file = |t: &Tables| sfnt_at(0, &[0, 1, 0, 0], t);
        // Record i's offset and length fields.
        let patch = |mut d: Vec<u8>, i: usize, field: usize, v: u32| {
            let at = 12 + i * 16 + field;
            d[at..at + 4].copy_from_slice(&v.to_be_bytes());
            open(d.into())
        };
        let cmap_at = 4;
        // A table past the end, or running past it, is dropped...
        assert!(
            patch(file(&base(4, None)), cmap_at, 8, 0xffff)
                .subtables
                .is_empty()
        );
        assert!(
            patch(file(&base(4, None)), cmap_at, 12, 0xffff)
                .subtables
                .is_empty()
        );
        // ...but `hmtx` is cut to whole metrics.
        let d = file(&base(4, None));
        let hmtx_offset = u32_at(&d, 12 + 3 * 16 + 8).unwrap_or(0) as usize;
        let f = patch(d.clone(), 3, 12, 0xffff);
        assert_eq!(
            f.table(b"hmtx"),
            Some((hmtx_offset, (d.len() - hmtx_offset) & !3))
        );
        assert_eq!(f.advance(3), 800);
        // The first of two tables of a tag counts.
        let mut t = base(8, Some(cmap(&[(3, 1, fmt6(0x41, &[5]))])));
        t.push((b"cmap", cmap(&[(3, 1, fmt6(0x41, &[6]))])));
        let f = open(ttf(&t));
        assert_eq!(f.subtables.len(), 1);
        assert_eq!(f.char_index(&f.subtables[0], 0x41), 5);
        // A table of no length is missing.
        let f = open(ttf(&set(base(4, None), b"cmap", Vec::new())));
        assert!(f.subtables.is_empty() && f.table(b"cmap").is_none());
        assert_eq!(f.table_data(b"head").map(<[u8]>::len), Some(54));
        // A directory cut short keeps the records it has, unless it is
        // an OpenType one.
        let mut d = file(&base(4, None));
        d[4..6].copy_from_slice(&40u16.to_be_bytes());
        d.truncate(12 + 8 * 16 + 8);
        assert_eq!(Sfnt::open(d.into()).err(), Some("Unknown_File_Format"));
        let mut d = file(&base(4, None));
        d[4..6].copy_from_slice(&9u16.to_be_bytes());
        assert_eq!(open(d.into()).num_glyphs, 4);
        let mut d = sfnt_at(0, b"OTTO", &base(4, None));
        d[4..6].copy_from_slice(&40u16.to_be_bytes());
        assert_eq!(Sfnt::open(d.into()).err(), Some("Invalid_Stream_Operation"));
        // No tables, or no valid ones.
        assert_eq!(
            Sfnt::open(file(&Vec::new()).into()).err(),
            Some("Unknown_File_Format")
        );
        assert_eq!(
            Sfnt::open(sfnt_at(0, b"OTTO", &[]).into()).err(),
            Some("Unknown_File_Format")
        );
        let d = patch_raw(sfnt_at(0, b"OTTO", &base(1, None)));
        assert_eq!(Sfnt::open(d.into()).err(), Some("Unknown_File_Format"));
        let d = patch_raw(file(&base(1, None)));
        assert_eq!(Sfnt::open(d.into()).err(), Some("Unknown_File_Format"));
        assert_eq!(
            Sfnt::open(Arc::from(&b"\0\x01\0\0\0\0"[..])).err(),
            Some("Invalid_Stream_Operation")
        );
        // A `SING` and `META` font needs no `head`, but FreeType still
        // reads one.
        let t = [
            without(base(4, None), b"head"),
            vec![(b"SING", vec![0; 4]), (b"META", vec![0; 4])],
        ]
        .concat();
        assert_eq!(Sfnt::open(ttf(&t)).err(), Some("Table_Missing"));
        // An OpenType header is read whole, past its table if need be.
        let mut t = without(base(4, None), b"head");
        t.push((b"head", vec![0; 20]));
        assert_eq!(
            Sfnt::open(sfnt_at(0, b"OTTO", &t).into()).err(),
            Some("Invalid_Stream_Operation")
        );
        // So are `hhea` and `vhea`.
        let mut t = without(base(4, None), b"hhea");
        t.push((b"hhea", vec![0; 8]));
        assert_eq!(Sfnt::open(ttf(&t)).err(), Some("Invalid_Stream_Operation"));
        let mut t = base(4, None);
        t.push((b"vhea", vec![0; 8]));
        assert_eq!(Sfnt::open(ttf(&t)).err(), Some("Invalid_Stream_Operation"));
        let t = set(base(4, None), b"vhea", hhea(800, -200, 4));
        assert!(Sfnt::open(ttf(&t)).is_ok());
    }

    /// Every record of a directory pointed past the end of the file.
    fn patch_raw(mut d: Vec<u8>) -> Vec<u8> {
        let n = usize::from(u16_at(&d, 4).unwrap_or(0));
        for i in 0..n {
            let at = 12 + i * 16 + 8;
            d[at..at + 4].copy_from_slice(&0xff_ffffu32.to_be_bytes());
        }
        d
    }

    #[test]
    fn collections_open_their_first_font() {
        let f = open(ttc(&[0, 1, 0, 0], &base(4, None)));
        assert_eq!((f.format_tag, f.num_glyphs), (TAG_TRUETYPE, 4));
        assert_eq!(f.char_index(&f.subtables[0], 0x41), 1);
        assert_eq!(open(ttc(b"OTTO", &base(4, None))).format_tag, TAG_OTTO);
        let collection = |count: i64, offset: i64| {
            let mut t = b"ttcf".to_vec();
            t.extend(be32(&[0x1_0000, count, offset]));
            t.extend(sfnt_at(16, &[0, 1, 0, 0], &base(4, None)));
            Sfnt::open(t.into()).err()
        };
        assert_eq!(collection(0, 16), Some("Invalid_Table"));
        assert_eq!(collection(1000, 16), Some("Array_Too_Large"));
        assert_eq!(collection(1, 0xffff), Some("Invalid_Stream_Operation"));
        assert_eq!(
            Sfnt::open(Arc::from(&b"ttcf\0\x01\0\0"[..])).err(),
            Some("Invalid_Stream_Operation")
        );
    }

    #[test]
    fn recognized_tags_and_the_loca_check() {
        for tag in [
            &[0, 1, 0, 0],
            &[0, 2, 0, 0],
            b"true",
            b"OTTO",
            b"ttcf",
            b"typ1",
        ] {
            assert!(Sfnt::recognize(&[&tag[..], &[0; 8]].concat()));
        }
        assert!(!Sfnt::recognize(b"wOFF\0\0\0\0"));
        assert_eq!(open(ttf(&base(4, None))).check_loca(), Ok(()));
        let f = open(ttf(&without(base(4, None), b"loca")));
        assert_eq!(f.check_loca(), Err("Locations_Missing"));
        assert!(f.glyf().is_none());
        let f = open(ttf(&without(without(base(4, None), b"loca"), b"glyf")));
        assert_eq!(f.check_loca(), Err("Unknown_File_Format"));
        // A long location format, as head says.
        let f = open(ttf(&set(base(4, None), b"head", with16(head(1000), 50, 1))));
        assert!(f.glyf().is_some_and(|g| g.long_loca));
    }

    #[test]
    fn platforms_map_to_freetype_encodings() {
        use super::super::Encoding as E;
        let cases = [
            ((0, 3), E::Unicode),
            ((2, 0), E::Unicode),
            ((2, 2), E::Unicode),
            ((3, 0), E::MsSymbol),
            ((3, 1), E::Unicode),
            ((3, 10), E::Unicode),
            ((1, 0), E::AppleRoman),
            ((1, 1), E::Other),
            ((3, 3), E::Other),
        ];
        for ((p, e), want) in cases {
            assert_eq!(encoding_of(p, e), want, "({p},{e})");
        }
    }
}
