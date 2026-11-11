// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Following FreeType 2 src/sfnt/ttload.c, src/sfnt/ttcmap.c,
// src/sfnt/ttpost.c, src/sfnt/ttmtx.c and src/sfnt/sfobjs.c, Copyright (C)
// 1996-2025 by David Turner, Robert Wilhelm, and Werner Lemberg, used under
// the FreeType License (NOTICE.md).

//! TrueType and OpenType fonts, as FreeType's sfnt module reads them for
//! MuPDF: the table directory, the character maps it keeps (and the
//! broken ones it drops), the glyph names of the `post` table, advances
//! from `hmtx`, and the metrics in `head`, `hhea` and `OS/2`.

use super::names;

/// What MuPDF needs of the face.
#[derive(Debug, Clone)]
pub struct Sfnt {
    data: std::sync::Arc<[u8]>,
    tables: Vec<([u8; 4], usize, usize)>,
    pub units_per_em: u16,
    pub num_glyphs: u32,
    pub bbox: [i32; 4],
    pub ascender: i16,
    pub descender: i16,
    /// The `cmap` table's extent, and its kept subtables.
    cmap: Option<(usize, usize)>,
    pub subtables: Vec<Subtable>,
    /// `post` glyph names, when the table has them (formats 1 and 2).
    post: Option<Post>,
    has_post: bool,
    num_hmetrics: usize,
    hmtx: Option<(usize, usize)>,
    pub has_cff: bool,
    pub has_glyf: bool,
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

#[derive(Debug, Clone)]
enum Post {
    /// The standard Macintosh order.
    Mac,
    /// A name per glyph: an index into the Macintosh names, or 258 and up
    /// into the table's own Pascal strings.
    Indexed(Vec<u16>, Vec<Vec<u8>>),
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    d.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn i16_at(d: &[u8], at: usize) -> Option<i16> {
    u16_at(d, at).map(|v| v as i16)
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

impl Sfnt {
    /// Whether the data starts like an sfnt (or a collection of them).
    pub fn recognize(data: &[u8]) -> bool {
        matches!(
            data.get(0..4),
            Some(&[0, 1, 0, 0]) | Some(b"true") | Some(b"OTTO") | Some(b"ttcf") | Some(b"typ1")
        )
    }

    /// `sfnt_open_font`, `tt_face_load_font_dir` and the table loads of
    /// `sfnt_load_face`.
    pub fn open(data: std::sync::Arc<[u8]>) -> Result<Sfnt, &'static str> {
        let d: &[u8] = &data;
        let mut base = 0usize;
        if d.get(0..4) == Some(b"ttcf") {
            base = u32_at(d, 12).ok_or("Unknown_File_Format")? as usize;
        }
        let num_tables = usize::from(u16_at(d, base + 4).ok_or("Unknown_File_Format")?);
        let mut tables = Vec::new();
        for i in 0..num_tables {
            let rec = base + 12 + i * 16;
            let Some(tag) = d.get(rec..rec + 4) else {
                break;
            };
            let offset = u32_at(d, rec + 8).unwrap_or(0) as usize;
            let mut length = u32_at(d, rec + 12).unwrap_or(0) as usize;
            // Tables past the end are dropped, long ones cut.
            if offset > d.len() {
                continue;
            }
            if length > d.len() - offset {
                length = d.len() - offset;
            }
            let tag = [tag[0], tag[1], tag[2], tag[3]];
            tables.push((tag, offset, length));
        }
        if tables.is_empty() {
            return Err("Unknown_File_Format");
        }
        let mut font = Sfnt {
            data: data.clone(),
            tables,
            units_per_em: 0,
            num_glyphs: 0,
            bbox: [0; 4],
            ascender: 0,
            descender: 0,
            cmap: None,
            subtables: Vec::new(),
            post: None,
            has_post: false,
            num_hmetrics: 0,
            hmtx: None,
            has_cff: false,
            has_glyf: false,
        };
        font.has_cff = font.table(b"CFF ").is_some();
        font.has_glyf = font.table(b"glyf").is_some();
        let (h, hl) = font.table(b"head").ok_or("Table_Missing")?;
        if hl < 54 {
            return Err("Invalid_Table");
        }
        font.units_per_em = u16_at(d, h + 18).unwrap_or(0);
        // sfnt_load_face: OpenType 1.8.2's limits, for every sfnt.
        if !(16..=16384).contains(&font.units_per_em) {
            return Err("Invalid_Table");
        }
        font.bbox = [
            i32::from(i16_at(d, h + 36).unwrap_or(0)),
            i32::from(i16_at(d, h + 38).unwrap_or(0)),
            i32::from(i16_at(d, h + 40).unwrap_or(0)),
            i32::from(i16_at(d, h + 42).unwrap_or(0)),
        ];
        let (m, ml) = font.table(b"maxp").ok_or("Table_Missing")?;
        if ml < 6 {
            return Err("Invalid_Table");
        }
        font.num_glyphs = u32::from(u16_at(d, m + 4).unwrap_or(0));
        if let Some((hh, hhl)) = font.table(b"hhea") {
            if hhl >= 36 {
                font.ascender = i16_at(d, hh + 4).unwrap_or(0);
                font.descender = i16_at(d, hh + 6).unwrap_or(0);
                font.num_hmetrics = usize::from(u16_at(d, hh + 34).unwrap_or(0));
            }
        } else {
            return Err("Horiz_Header_Missing");
        }
        font.hmtx = font.table(b"hmtx");
        font.metrics_from_os2();
        font.load_post();
        font.load_cmaps();
        Ok(font)
    }

    pub fn table(&self, tag: &[u8; 4]) -> Option<(usize, usize)> {
        self.tables
            .iter()
            .find(|(t, _, _)| t == tag)
            .map(|&(_, o, l)| (o, l))
    }

    pub fn table_data(&self, tag: &[u8; 4]) -> Option<&[u8]> {
        let (o, l) = self.table(tag)?;
        self.data.get(o..o + l)
    }

    /// The ascender and descender `sfnt_load_face` settles on.
    fn metrics_from_os2(&mut self) {
        let Some(os2) = self
            .table_data(b"OS/2")
            .filter(|t| t.len() >= 78)
            .map(<[u8]>::to_vec)
        else {
            return;
        };
        let fs_selection = u16_at(&os2, 62).unwrap_or(0);
        let typo_asc = i16_at(&os2, 68).unwrap_or(0);
        let typo_desc = i16_at(&os2, 70).unwrap_or(0);
        let win_asc = u16_at(&os2, 74).unwrap_or(0);
        let win_desc = u16_at(&os2, 76).unwrap_or(0);
        if fs_selection & 128 != 0 {
            self.ascender = typo_asc;
            self.descender = typo_desc;
        } else if self.ascender == 0 && self.descender == 0 {
            if typo_asc != 0 || typo_desc != 0 {
                self.ascender = typo_asc;
                self.descender = typo_desc;
            } else {
                self.ascender = win_asc as i16;
                self.descender = -(win_desc as i16);
            }
        }
    }

    /// `tt_face_load_post` and the names of `load_post_names`.
    fn load_post(&mut self) {
        let Some(post) = self.table_data(b"post").map(<[u8]>::to_vec) else {
            return;
        };
        if post.len() < 32 {
            return;
        }
        self.has_post = true;
        let format = u32_at(&post, 0).unwrap_or(0);
        self.post = match format {
            0x0001_0000 => Some(Post::Mac),
            0x0002_0000 => {
                let n = usize::from(u16_at(&post, 32).unwrap_or(0));
                let mut idx = Vec::with_capacity(n);
                for i in 0..n {
                    idx.push(u16_at(&post, 34 + i * 2).unwrap_or(0));
                }
                let mut strings = Vec::new();
                let mut at = 34 + n * 2;
                while let Some(&len) = post.get(at) {
                    let s = post
                        .get(at + 1..at + 1 + usize::from(len))
                        .unwrap_or_default();
                    strings.push(s.to_vec());
                    at += 1 + usize::from(len);
                }
                Some(Post::Indexed(idx, strings))
            }
            _ => None,
        };
    }

    /// Whether FreeType gives the face glyph names: a `post` table not of
    /// format 3.
    pub fn has_glyph_names(&self) -> bool {
        let format = self.table_data(b"post").and_then(|p| u32_at(p, 0));
        self.has_post && format != Some(0x0003_0000)
    }

    /// `tt_face_get_ps_name`.
    pub fn glyph_name(&self, gid: u32) -> Option<Vec<u8>> {
        if gid >= self.num_glyphs {
            return None;
        }
        match self.post.as_ref()? {
            Post::Mac => names::MAC_NAMES
                .get(gid as usize)
                .map(|n| n.as_bytes().to_vec()),
            Post::Indexed(idx, strings) => {
                let i = *idx.get(gid as usize)?;
                if i < 258 {
                    Some(names::MAC_NAMES[usize::from(i)].as_bytes().to_vec())
                } else {
                    strings.get(usize::from(i) - 258).cloned()
                }
            }
        }
    }

    /// `FT_Get_Name_Index` through the `post` names.
    pub fn name_index(&self, name: &[u8]) -> u32 {
        if !self.has_glyph_names() {
            return 0;
        }
        (0..self.num_glyphs)
            .find(|&g| self.glyph_name(g).as_deref() == Some(name))
            .unwrap_or(0)
    }

    /// `tt_face_get_metrics`: the advance in font units.
    pub fn advance(&self, gid: u32) -> u16 {
        let Some((o, l)) = self.hmtx else {
            return 0;
        };
        let d = &self.data;
        let n = self.num_hmetrics;
        let at = if (gid as usize) < n {
            gid as usize * 4
        } else if n > 0 {
            (n - 1) * 4
        } else {
            return 0;
        };
        if at + 2 > l {
            return 0;
        }
        u16_at(d, o + at).unwrap_or(0)
    }

    /// `tt_face_build_cmaps`: each subtable of a known format that passes
    /// FreeType's default validation.
    fn load_cmaps(&mut self) {
        let Some((t, tl)) = self.table(b"cmap") else {
            return;
        };
        if tl < 4 {
            return;
        }
        self.cmap = Some((t, tl));
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
        let d = &self.data;
        match format {
            0 => {
                let length = usize::from(u16_at(d, at + 2)?);
                (at + length <= limit && length >= 262).then_some((false, false))
            }
            2 => (at + 6 + 512 <= limit).then_some((false, false)),
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
                    if offset != 0 && offset != 0xffff {
                        let p = offsets + n * 2 + offset;
                        let last_ffff = n == num_segs - 1 && start == 0xffff && end == 0xffff;
                        if !last_ffff
                            && (p < glyph_ids || p + (end - start + 1) as usize * 2 > limit)
                        {
                            return None;
                        }
                    }
                    last_start = start;
                    last_end = end;
                }
                Some((unsorted, overlapping))
            }
            6 => {
                let count = usize::from(u16_at(d, at + 8)?);
                (at + 10 + count * 2 <= limit).then_some((false, false))
            }
            10 => {
                let count = u32_at(d, at + 16)? as usize;
                (at + 20 + count * 2 <= limit).then_some((false, false))
            }
            12 | 13 => {
                let length = u32_at(d, at + 4)? as usize;
                let groups = u32_at(d, at + 12)? as usize;
                if at + 16 > limit || length > limit - at || groups.checked_mul(12)? > length - 16 {
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
                Some((false, false))
            }
            14 => (at + 10 <= limit).then_some((false, false)),
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
                        out = if sub.format == 12 {
                            start_id.wrapping_add(code - s)
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
        let found = if sub.unsorted {
            (0..num_segs).find(|&i| {
                let (s, e, ..) = seg(i);
                code >= s && code <= e
            })
        } else {
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
            if sub.overlapping
                && let Some(m) = hit
            {
                // The first segment that has the code and maps it.
                let mut best = m;
                for i in (0..m).rev() {
                    let (s, e, _, offset, _) = seg(i);
                    if code > e {
                        break;
                    }
                    if code >= s && offset != 0xffff {
                        best = i;
                    }
                }
                Some(best)
            } else {
                hit
            }
        };
        let Some(i) = found else {
            return 0;
        };
        let (start, end, mut delta, mut offset, offset_at) = seg(i);
        if i == num_segs - 1
            && start == 0xffff
            && end == 0xffff
            && offset != 0
            && offset_at + offset + 2 > limit
        {
            delta = 1;
            offset = 0;
        }
        if offset == 0xffff {
            return 0;
        }
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
        (0, _) => Encoding::Unicode,
        (2, 1) => Encoding::Unicode,
        (3, 0) => Encoding::MsSymbol,
        (3, 1) | (3, 10) => Encoding::Unicode,
        (1, 0) => Encoding::AppleRoman,
        _ => Encoding::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TrueType file of the given tables, in tag order.
    fn sfnt(mut tables: Vec<(&[u8; 4], Vec<u8>)>) -> std::sync::Arc<[u8]> {
        tables.sort_by_key(|t| *t.0);
        let mut out = vec![0, 1, 0, 0];
        out.extend_from_slice(&(tables.len() as u16).to_be_bytes());
        out.extend_from_slice(&[0; 6]);
        let mut offset = 12 + 16 * tables.len();
        let mut body = Vec::new();
        for (tag, data) in &tables {
            out.extend_from_slice(*tag);
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&(offset as u32).to_be_bytes());
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            body.extend_from_slice(data);
            while body.len() % 4 != 0 {
                body.push(0);
            }
            offset = 12 + 16 * tables.len() + body.len();
        }
        out.extend_from_slice(&body);
        out.into()
    }

    fn head(upem: u16) -> Vec<u8> {
        let mut h = vec![0; 54];
        h[0..4].copy_from_slice(&[0, 1, 0, 0]);
        h[12..16].copy_from_slice(&0x5F0F_3CF5u32.to_be_bytes());
        h[18..20].copy_from_slice(&upem.to_be_bytes());
        h[40..42].copy_from_slice(&1000i16.to_be_bytes());
        h[42..44].copy_from_slice(&800i16.to_be_bytes());
        h
    }

    fn maxp(glyphs: u16) -> Vec<u8> {
        let mut m = vec![0, 0, 0x50, 0];
        m.extend_from_slice(&glyphs.to_be_bytes());
        m
    }

    fn hhea(metrics: u16) -> Vec<u8> {
        let mut h = vec![0; 36];
        h[0..4].copy_from_slice(&[0, 1, 0, 0]);
        h[4..6].copy_from_slice(&800i16.to_be_bytes());
        h[6..8].copy_from_slice(&(-200i16).to_be_bytes());
        h[34..36].copy_from_slice(&metrics.to_be_bytes());
        h
    }

    /// A (3,1) format 4 cmap mapping 'A'..'C' to glyphs 1..3.
    fn cmap() -> Vec<u8> {
        let mut sub = Vec::new();
        let seg = [
            (0x41u16, 0x43u16, (1i32 - 0x41) as i16),
            (0xffff, 0xffff, 1),
        ];
        sub.extend_from_slice(&4u16.to_be_bytes());
        sub.extend_from_slice(&(16 + 8 * seg.len() as u16).to_be_bytes());
        sub.extend_from_slice(&0u16.to_be_bytes());
        sub.extend_from_slice(&(2 * seg.len() as u16).to_be_bytes());
        sub.extend_from_slice(&[0, 4, 0, 1, 0, 0]);
        for s in &seg {
            sub.extend_from_slice(&s.1.to_be_bytes());
        }
        sub.extend_from_slice(&[0, 0]);
        for s in &seg {
            sub.extend_from_slice(&s.0.to_be_bytes());
        }
        for s in &seg {
            sub.extend_from_slice(&s.2.to_be_bytes());
        }
        for _ in &seg {
            sub.extend_from_slice(&[0, 0]);
        }
        let mut c = vec![0, 0, 0, 1, 0, 3, 0, 1, 0, 0, 0, 12];
        c.extend_from_slice(&sub);
        c
    }

    fn hmtx() -> Vec<u8> {
        [500u16, 0, 600, 0, 700, 0, 800, 0]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect()
    }

    #[test]
    fn a_truetype_file_gives_its_metrics_and_glyphs() {
        let data = sfnt(vec![
            (b"head", head(1000)),
            (b"maxp", maxp(4)),
            (b"hhea", hhea(4)),
            (b"hmtx", hmtx()),
            (b"cmap", cmap()),
        ]);
        let f = Sfnt::open(data).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((f.units_per_em, f.num_glyphs), (1000, 4));
        assert_eq!((f.ascender, f.descender), (800, -200));
        assert_eq!(f.advance(2), 700);
        let sub = f
            .subtables
            .first()
            .cloned()
            .unwrap_or_else(|| panic!("no cmap"));
        assert_eq!((sub.platform_id, sub.encoding_id, sub.format), (3, 1, 4));
        assert_eq!(f.char_index(&sub, 0x42), 2);
        assert_eq!(f.char_index(&sub, 0x44), 0);
    }

    #[test]
    fn freetype_refuses_what_sfnt_load_face_refuses() {
        let with = |upem: u16, hhea_too: bool| {
            let mut t = vec![(b"head", head(upem)), (b"maxp", maxp(4))];
            if hhea_too {
                t.push((b"hhea", hhea(4)));
            }
            Sfnt::open(sfnt(t)).err()
        };
        assert_eq!(with(0, true), Some("Invalid_Table"));
        assert_eq!(with(16385, true), Some("Invalid_Table"));
        assert_eq!(with(1000, false), Some("Horiz_Header_Missing"));
        assert_eq!(with(16, true), None);
    }
}
