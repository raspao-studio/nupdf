// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Following FreeType 2 src/cff/cffload.c, src/cff/cffparse.c,
// src/cff/cffobjs.c, src/cff/cffgload.c (the font matrix) and
// src/psaux/psintrp.c (glyph widths), Copyright (C)
// 1996-2025 by David Turner, Robert Wilhelm, and Werner Lemberg, and Adobe
// Systems Incorporated, used under the FreeType License (NOTICE.md).

//! Compact Font Format fonts, as FreeType's CFF driver reads them for
//! MuPDF: the top and private dictionaries, the charset and encoding, glyph
//! names, and the advance a glyph's charstring sets.

use super::names;
use super::outline::{self, CBox, Charstrings, LoadError};

#[derive(Debug, Clone)]
pub struct Cff {
    data: std::sync::Arc<[u8]>,
    strings: Index,
    gsubrs: Index,
    charstrings: Index,
    /// SID (or CID) of each glyph.
    sids: Vec<u16>,
    /// The code to glyph map of the encoding, and whether it has codes.
    pub codes: [u16; 256],
    pub encoding_offset: u32,
    pub encoding_count: u32,
    pub is_cid: bool,
    pub units_per_em: u32,
    pub bbox: [i32; 4],
    /// The style FreeType's CFF driver gives the face.
    pub bold: bool,
    pub italic: bool,
    pub fixed_pitch: bool,
    /// Per font dictionary: default and nominal width, local subrs.
    privates: Vec<Private>,
    fd_select: FdSelect,
}

#[derive(Debug, Clone, Default)]
struct Private {
    default_width: f64,
    nominal_width: f64,
    subrs: Index,
    /// The font dictionary's units per em, as `cff_face_init` works it
    /// out from its FontMatrix and the top one's.
    units_per_em: u32,
    /// The FontMatrix left once its scale is taken out (16.16), with its
    /// offset in font units, when that is not the identity.
    matrix: Option<[i64; 6]>,
}

/// `cff_face_init`'s normalized FontMatrix: divided by its vertical
/// scale (or its yx when that is 0), in 16.16, with the offset in whole
/// font units. None for the identity, which leaves outlines as they are.
fn normalized_matrix(m: &[f64]) -> Option<[i64; 6]> {
    if m.len() < 6 {
        return None;
    }
    let temp = if m[3] != 0.0 { m[3].abs() } else { m[1].abs() };
    if temp == 0.0 || !temp.is_finite() {
        return None;
    }
    let fixed = |v: f64| {
        let f = (v / temp * 65536.0).round();
        if f.is_finite() {
            f.clamp(-2147483648.0, 2147483647.0) as i64
        } else {
            0
        }
    };
    let offset = |v: f64| fixed(v) >> 16;
    let out = [
        fixed(m[0]),
        fixed(m[1]),
        fixed(m[2]),
        fixed(m[3]),
        offset(m[4]),
        offset(m[5]),
    ];
    (out != [0x10000, 0, 0, 0x10000, 0, 0]).then_some(out)
}

/// `FT_Outline_Transform` by a normalized FontMatrix, then
/// `FT_Outline_Translate` by its offset, as `cff_slot_load` does before
/// scaling.
fn apply_matrix(o: &mut outline::Outline, m: &[i64; 6]) {
    let [xx, yx, xy, yy, ox, oy] = *m;
    for p in &mut o.points {
        let (x, y) = *p;
        *p = (
            outline::mul_fix(x, xx) + outline::mul_fix(y, xy) + ox,
            outline::mul_fix(x, yx) + outline::mul_fix(y, yy) + oy,
        );
    }
}

/// The units per em a FontMatrix gives: one over its vertical scale.
fn matrix_upm(m: &[f64]) -> Option<u32> {
    let yy = if m.get(3).copied().unwrap_or(0.0) != 0.0 {
        m[3].abs()
    } else {
        m.get(1)?.abs()
    };
    (yy > 0.0)
        .then(|| ((1.0 / yy) + 0.5) as u32)
        .filter(|&u| u > 0)
}

#[derive(Debug, Clone, Default)]
enum FdSelect {
    #[default]
    None,
    Format0(Vec<u8>),
    /// (first glyph, fd) ranges and the sentinel.
    Format3(Vec<(u16, u8)>, u16),
}

#[derive(Debug, Clone, Default)]
struct Index {
    /// Each item's start and end in the font data.
    items: Vec<(usize, usize)>,
}

impl Index {
    fn len(&self) -> usize {
        self.items.len()
    }
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    d.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn offset_at(d: &[u8], at: usize, size: usize) -> Option<usize> {
    let b = d.get(at..at + size)?;
    Some(b.iter().fold(0usize, |a, &x| (a << 8) | usize::from(x)))
}

/// An INDEX at `at`, and where it ends.
fn read_index(d: &[u8], at: usize) -> Option<(Index, usize)> {
    let count = usize::from(u16_at(d, at)?);
    if count == 0 {
        return Some((Index::default(), at + 2));
    }
    let off_size = usize::from(*d.get(at + 2)?);
    if !(1..=4).contains(&off_size) {
        return None;
    }
    let offsets = at + 3;
    let data = offsets + (count + 1) * off_size - 1;
    let mut items = Vec::with_capacity(count);
    let mut prev = offset_at(d, offsets, off_size)?;
    for i in 1..=count {
        let next = offset_at(d, offsets + i * off_size, off_size)?;
        // FreeType clamps broken offsets to the data it has.
        let (s, e) = (data + prev, data + next.max(prev));
        items.push((s.min(d.len()), e.min(d.len())));
        prev = next.max(prev);
    }
    let end = data + prev;
    Some((Index { items }, end.min(d.len())))
}

/// A DICT's operators with their operands.
fn read_dict(d: &[u8]) -> Vec<(u16, Vec<f64>)> {
    let mut out = Vec::new();
    let mut ops: Vec<f64> = Vec::new();
    let mut i = 0;
    while i < d.len() {
        let b0 = d[i];
        match b0 {
            0..=21 => {
                let op = if b0 == 12 {
                    i += 1;
                    1200 + u16::from(*d.get(i).unwrap_or(&0))
                } else {
                    u16::from(b0)
                };
                out.push((op, std::mem::take(&mut ops)));
                i += 1;
            }
            28 => {
                let v =
                    i16::from_be_bytes([*d.get(i + 1).unwrap_or(&0), *d.get(i + 2).unwrap_or(&0)]);
                ops.push(f64::from(v));
                i += 3;
            }
            29 => {
                let b = d.get(i + 1..i + 5).unwrap_or(&[0, 0, 0, 0]);
                let v = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                ops.push(f64::from(v));
                i += 5;
            }
            30 => {
                let mut s = String::new();
                i += 1;
                'real: while i < d.len() {
                    let b = d[i];
                    i += 1;
                    for nib in [b >> 4, b & 15] {
                        match nib {
                            0..=9 => s.push(char::from(b'0' + nib)),
                            0xa => s.push('.'),
                            0xb => s.push('E'),
                            0xc => s.push_str("E-"),
                            0xe => s.push('-'),
                            0xf => break 'real,
                            _ => {}
                        }
                    }
                }
                ops.push(s.parse().unwrap_or(0.0));
            }
            32..=246 => {
                ops.push(f64::from(i32::from(b0) - 139));
                i += 1;
            }
            247..=250 => {
                let b1 = i32::from(*d.get(i + 1).unwrap_or(&0));
                ops.push(f64::from((i32::from(b0) - 247) * 256 + b1 + 108));
                i += 2;
            }
            251..=254 => {
                let b1 = i32::from(*d.get(i + 1).unwrap_or(&0));
                ops.push(f64::from(-(i32::from(b0) - 251) * 256 - b1 - 108));
                i += 2;
            }
            _ => i += 1,
        }
    }
    out
}

fn get(dict: &[(u16, Vec<f64>)], op: u16) -> Option<&Vec<f64>> {
    dict.iter().find(|(o, _)| *o == op).map(|(_, v)| v)
}

fn first(dict: &[(u16, Vec<f64>)], op: u16, default: f64) -> f64 {
    get(dict, op)
        .and_then(|v| v.first().copied())
        .unwrap_or(default)
}

impl Cff {
    /// Whether the data starts like a CFF font (version 1).
    pub fn recognize(data: &[u8]) -> bool {
        data.len() >= 4 && data[0] == 1 && data[2] >= 4
    }

    /// `cff_font_load` and the parts of `cff_face_init` MuPDF sees.
    pub fn open(data: std::sync::Arc<[u8]>) -> Result<Cff, &'static str> {
        let d: &[u8] = &data;
        if !Self::recognize(d) {
            return Err("Unknown_File_Format");
        }
        let hdr = usize::from(d[2]);
        let (_names, at) = read_index(d, hdr).ok_or("Invalid_File_Format")?;
        let (top, at) = read_index(d, at).ok_or("Invalid_File_Format")?;
        let (strings, at) = read_index(d, at).ok_or("Invalid_File_Format")?;
        let (gsubrs, _) = read_index(d, at).ok_or("Invalid_File_Format")?;
        let &(ts, te) = top.items.first().ok_or("Invalid_File_Format")?;
        let top = read_dict(&d[ts..te]);
        let is_cid = get(&top, 1230).is_some();
        let cs_off = first(&top, 17, 0.0) as usize;
        if cs_off == 0 {
            return Err("Invalid_File_Format");
        }
        let (charstrings, _) = read_index(d, cs_off).ok_or("Invalid_File_Format")?;
        let num_glyphs = charstrings.len();
        let mut font = Cff {
            data: data.clone(),
            strings,
            gsubrs,
            charstrings,
            sids: Vec::new(),
            codes: [0; 256],
            encoding_offset: 0,
            encoding_count: 0,
            is_cid,
            units_per_em: 1000,
            bbox: [0; 4],
            bold: false,
            italic: false,
            fixed_pitch: false,
            privates: Vec::new(),
            fd_select: FdSelect::None,
        };
        // FontMatrix sets the em; the default is 1/1000.
        if let Some(m) = get(&top, 1207).filter(|m| m.len() >= 4) {
            let yy = if m[3] != 0.0 { m[3].abs() } else { m[1].abs() };
            if yy > 0.0 {
                font.units_per_em = ((1.0 / yy) + 0.5) as u32;
                if font.units_per_em == 0 {
                    font.units_per_em = 1000;
                }
            }
        }
        if let Some(b) = get(&top, 5).filter(|b| b.len() >= 4) {
            font.bbox = [
                b[0].floor() as i32,
                b[1].floor() as i32,
                b[2].ceil() as i32,
                b[3].ceil() as i32,
            ];
        }
        font.style(&top, &_names);
        let charset = first(&top, 15, 0.0) as usize;
        font.load_charset(charset, num_glyphs)?;
        if is_cid {
            let fdarray = first(&top, 1236, 0.0) as usize;
            if let Some((fds, _)) = read_index(d, fdarray) {
                let top_matrix = get(&top, 1207).filter(|m| m.len() >= 4).is_some();
                for &(s, e) in &fds.items {
                    let fd = read_dict(&d[s..e]);
                    let mut private = font.load_private(&fd);
                    let top_upm = font.units_per_em;
                    let sub = get(&fd, 1207)
                        .filter(|m| m.len() >= 4)
                        .and_then(|m| matrix_upm(m));
                    // A font dictionary without a matrix of its own takes
                    // the top one's; one with its own is concatenated in
                    // FreeType, which nupdf leaves at its scale.
                    if get(&fd, 1207).is_none() {
                        private.matrix = get(&top, 1207).and_then(|m| normalized_matrix(m));
                    }
                    private.units_per_em = match sub {
                        Some(sub) if top_matrix => {
                            let scaling = if top_upm > 1 && sub > 1 {
                                top_upm.min(sub)
                            } else {
                                1
                            };
                            (u64::from(sub) * u64::from(top_upm) / u64::from(scaling)) as u32
                        }
                        Some(sub) => sub,
                        None => top_upm,
                    };
                    font.privates.push(private);
                }
            }
            let sel = first(&top, 1237, 0.0) as usize;
            font.fd_select = font.load_fd_select(sel, num_glyphs);
        } else {
            let mut p = font.load_private(&top);
            p.units_per_em = font.units_per_em;
            p.matrix = get(&top, 1207).and_then(|m| normalized_matrix(m));
            font.privates.push(p);
            let enc = first(&top, 16, 0.0) as usize;
            font.load_encoding(enc, num_glyphs);
        }
        Ok(font)
    }

    /// `cff_face_init`'s style flags: ItalicAngle (an integer, rounded
    /// down), the Weight string, and a style name taken from FullName
    /// past FamilyName that starts with Bold or Black.
    fn style(&mut self, top: &[(u16, Vec<f64>)], names: &Index) {
        let d: &[u8] = &self.data;
        self.italic = first(top, 1202, 0.0).floor() != 0.0;
        self.fixed_pitch = first(top, 1201, 0.0) != 0.0;
        let sid = |op: u16| get(top, op).and_then(|v| v.first()).map(|&v| v as u16);
        let weight = sid(4).and_then(|s| self.sid_string(s));
        self.bold = matches!(weight, Some(b"Bold" | b"Black"));
        if self.bold {
            return;
        }
        let family: Option<Vec<u8>> = if self.is_cid {
            None
        } else {
            match sid(3).and_then(|s| self.sid_string(s)) {
                Some(f) => Some(f.to_vec()),
                None => names.items.first().map(|&(s, e)| {
                    let n = &d[s..e];
                    // remove_subset_prefix.
                    match n.iter().position(|&c| c == b'+') {
                        Some(6) if n[..6].iter().all(u8::is_ascii_uppercase) => n[7..].to_vec(),
                        _ => n.to_vec(),
                    }
                }),
            }
        };
        let (Some(family), Some(full)) = (family, sid(2).and_then(|s| self.sid_string(s))) else {
            return;
        };
        let (mut f, mut p) = (0usize, 0usize);
        let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
        while at(full, p) != 0 {
            if at(full, p) == at(&family, f) {
                f += 1;
                p += 1;
                continue;
            }
            if matches!(at(full, p), b' ' | b'-') {
                p += 1;
                continue;
            }
            if matches!(at(&family, f), b' ' | b'-') {
                f += 1;
                continue;
            }
            if at(&family, f) == 0 {
                let style = &full[p..];
                self.bold = style.starts_with(b"Bold") || style.starts_with(b"Black");
            }
            break;
        }
    }

    fn load_private(&self, dict: &[(u16, Vec<f64>)]) -> Private {
        let d = &self.data;
        let Some(p) = get(dict, 18).filter(|p| p.len() >= 2) else {
            return Private::default();
        };
        let (size, off) = (p[0] as usize, p[1] as usize);
        let Some(bytes) = d.get(off..off + size) else {
            return Private::default();
        };
        let private = read_dict(bytes);
        let subrs_off = first(&private, 19, 0.0) as usize;
        let subrs = if subrs_off > 0 {
            read_index(d, off + subrs_off)
                .map(|(i, _)| i)
                .unwrap_or_default()
        } else {
            Index::default()
        };
        Private {
            default_width: first(&private, 20, 0.0),
            nominal_width: first(&private, 21, 0.0),
            subrs,
            units_per_em: self.units_per_em,
            matrix: None,
        }
    }

    fn load_fd_select(&self, at: usize, num_glyphs: usize) -> FdSelect {
        let d = &self.data;
        match d.get(at) {
            Some(0) => FdSelect::Format0(
                d.get(at + 1..at + 1 + num_glyphs)
                    .unwrap_or_default()
                    .to_vec(),
            ),
            Some(3) => {
                let n = usize::from(u16_at(d, at + 1).unwrap_or(0));
                let mut ranges = Vec::new();
                for i in 0..n {
                    let r = at + 3 + i * 3;
                    ranges.push((u16_at(d, r).unwrap_or(0), *d.get(r + 2).unwrap_or(&0)));
                }
                FdSelect::Format3(ranges, u16_at(d, at + 3 + n * 3).unwrap_or(0))
            }
            _ => FdSelect::None,
        }
    }

    fn fd_of(&self, gid: usize) -> usize {
        match &self.fd_select {
            FdSelect::None => 0,
            FdSelect::Format0(v) => usize::from(*v.get(gid).unwrap_or(&0)),
            FdSelect::Format3(r, sentinel) => {
                let g = gid as u16;
                if g >= *sentinel {
                    return 0;
                }
                r.iter()
                    .rev()
                    .find(|(first, _)| *first <= g)
                    .map_or(0, |&(_, fd)| usize::from(fd))
            }
        }
    }

    /// `cff_charset_load`.
    fn load_charset(&mut self, offset: usize, num_glyphs: usize) -> Result<(), &'static str> {
        let d = self.data.clone();
        if offset > 2 {
            let format = *d.get(offset).ok_or("Invalid_File_Format")?;
            let mut sids = vec![0u16; num_glyphs];
            let mut p = offset + 1;
            match format {
                0 => {
                    for s in sids.iter_mut().skip(1) {
                        *s = u16_at(&d, p).ok_or("Invalid_File_Format")?;
                        p += 2;
                    }
                }
                1 | 2 => {
                    let mut j = 1;
                    while j < num_glyphs {
                        let mut sid = u32::from(u16_at(&d, p).ok_or("Invalid_File_Format")?);
                        p += 2;
                        let mut nleft = if format == 2 {
                            let n = u32::from(u16_at(&d, p).ok_or("Invalid_File_Format")?);
                            p += 2;
                            n
                        } else {
                            let n = u32::from(*d.get(p).ok_or("Invalid_File_Format")?);
                            p += 1;
                            n
                        };
                        if sid > 0xffff - nleft {
                            nleft = 0xffff - sid;
                        }
                        let mut i = 0;
                        while j < num_glyphs && i <= nleft {
                            sids[j] = sid as u16;
                            i += 1;
                            j += 1;
                            sid += 1;
                        }
                    }
                }
                _ => return Err("Invalid_File_Format"),
            }
            self.sids = sids;
        } else {
            let (table, max): (&[u16], usize) = match offset {
                0 => (&[], 229),
                1 => (&names::EXPERT_CHARSET, 166),
                _ => (&names::EXPERTSUBSET_CHARSET, 87),
            };
            if num_glyphs > max {
                return Err("Invalid_File_Format");
            }
            self.sids = (0..num_glyphs)
                .map(|i| if offset == 0 { i as u16 } else { table[i] })
                .collect();
        }
        Ok(())
    }

    /// `cff_charset_cid_to_gindex` over SIDs: the lowest glyph with it.
    fn gid_of_sid(&self, sid: u16) -> u16 {
        self.sids
            .iter()
            .position(|&s| s == sid)
            .map_or(0, |g| g as u16)
    }

    /// `cff_encoding_load`.
    fn load_encoding(&mut self, offset: usize, num_glyphs: usize) {
        let d = self.data.clone();
        if offset > 1 {
            self.encoding_offset = offset as u32;
            let Some(&format) = d.get(offset) else {
                return;
            };
            let count = u32::from(*d.get(offset + 1).unwrap_or(&0));
            let mut p = offset + 2;
            match format & 0x7f {
                0 => {
                    self.encoding_count = count + 1;
                    for j in 1..=count {
                        let code = usize::from(*d.get(p).unwrap_or(&0));
                        p += 1;
                        if (j as usize) < num_glyphs {
                            self.codes[code] = j as u16;
                        }
                    }
                }
                1 => {
                    self.encoding_count = 0;
                    let mut i = 1u32;
                    for _ in 0..count {
                        let first = u32::from(*d.get(p).unwrap_or(&0));
                        let nleft = u32::from(*d.get(p + 1).unwrap_or(&0)) + 1;
                        p += 2;
                        if nleft > self.encoding_count {
                            self.encoding_count = nleft;
                        }
                        for (code, k) in (first..).zip(i..i + nleft) {
                            if (k as usize) < num_glyphs && code < 256 {
                                self.codes[code as usize] = k as u16;
                            }
                        }
                        i += nleft;
                    }
                    self.encoding_count = self.encoding_count.min(256);
                }
                _ => {
                    self.encoding_count = 0;
                    return;
                }
            }
            if format & 0x80 != 0 {
                let n = usize::from(*d.get(p).unwrap_or(&0));
                p += 1;
                for _ in 0..n {
                    let code = usize::from(*d.get(p).unwrap_or(&0));
                    let sid = u16_at(&d, p + 1).unwrap_or(0);
                    p += 3;
                    if let Some(g) = self.sids.iter().position(|&s| s == sid) {
                        self.codes[code] = g as u16;
                    }
                }
            }
        } else {
            self.encoding_offset = offset as u32;
            let table = if offset == 0 {
                &names::STANDARD_ENCODING
            } else {
                &names::EXPERT_ENCODING
            };
            self.encoding_count = 0;
            for (j, &sid) in table.iter().enumerate() {
                let gid = if sid != 0 { self.gid_of_sid(sid) } else { 0 };
                if gid != 0 {
                    self.codes[j] = gid;
                    self.encoding_count = j as u32 + 1;
                }
            }
        }
    }

    pub fn num_glyphs(&self) -> u32 {
        self.charstrings.len() as u32
    }

    /// The largest CID, for a CID-keyed font.
    pub fn max_cid(&self) -> u32 {
        self.sids.iter().copied().max().map_or(0, u32::from)
    }

    /// `cff_index_get_sid_string`.
    fn sid_string(&self, sid: u16) -> Option<&[u8]> {
        let sid = usize::from(sid);
        if sid < names::SID_NAMES.len() {
            return Some(names::SID_NAMES[sid].as_bytes());
        }
        let &(s, e) = self.strings.items.get(sid - names::SID_NAMES.len())?;
        self.data.get(s..e)
    }

    /// `cff_get_glyph_name`.
    pub fn glyph_name(&self, gid: u32) -> Option<Vec<u8>> {
        if self.is_cid {
            return None;
        }
        let sid = *self.sids.get(gid as usize)?;
        self.sid_string(sid).map(<[u8]>::to_vec)
    }

    /// `cff_get_name_index`.
    pub fn name_index(&self, name: &[u8]) -> u32 {
        if self.is_cid {
            return 0;
        }
        (0..self.sids.len())
            .find(|&g| self.sid_string(self.sids[g]) == Some(name))
            .map_or(0, |g| g as u32)
    }

    /// For a CID-keyed font, the glyph with a CID.
    pub fn gid_of_cid(&self, cid: u32) -> u32 {
        self.sids
            .iter()
            .position(|&s| u32::from(s) == cid)
            .map_or(0, |g| g as u32)
    }

    /// `cff_slot_load` as far as `fz_bound_ft_glyph` goes: the control box
    /// of glyph `index` (a CID in a CID-keyed font), in font units.
    pub fn outline_cbox(&self, index: u32) -> Result<Option<CBox>, LoadError> {
        let gid = if self.is_cid {
            if index == 0 {
                0
            } else {
                let g = self.gid_of_cid(index);
                if g == 0 {
                    return Err(LoadError);
                }
                g
            }
        } else if index >= self.num_glyphs() {
            return Err(LoadError);
        } else {
            index
        };
        let &(s, e) = self.charstrings.items.get(gid as usize).ok_or(LoadError)?;
        let cs = self.data.get(s..e).ok_or(LoadError)?;
        let private = self
            .privates
            .get(self.fd_of(gid as usize))
            .cloned()
            .unwrap_or_default();
        let glyph = Glyph {
            font: self,
            private: &private,
        };
        let cbox = match &private.matrix {
            None => outline::charstring_cbox(&glyph, cs)?,
            // The control box of the transformed outline.
            Some(m) => {
                let mut o = outline::charstring_outline(&glyph, cs)?;
                apply_matrix(&mut o, m);
                o.points.iter().fold(None, |b: Option<CBox>, &(x, y)| {
                    let (x, y) = (x as f64, y as f64);
                    Some(match b {
                        None => CBox {
                            x0: x,
                            y0: y,
                            x1: x,
                            y1: y,
                        },
                        Some(b) => CBox {
                            x0: b.x0.min(x),
                            y0: b.y0.min(y),
                            x1: b.x1.max(x),
                            y1: b.y1.max(y),
                        },
                    })
                })
            }
        };
        // A font dictionary with units of its own is scaled to the top
        // dictionary's (`cff_slot_load`).
        let (top, sub) = (self.units_per_em, private.units_per_em);
        if sub == 0 || sub == top {
            return Ok(cbox);
        }
        let k = f64::from(top) / f64::from(sub);
        Ok(cbox.map(|b| CBox {
            x0: b.x0 * k,
            y0: b.y0 * k,
            x1: b.x1 * k,
            y1: b.y1 * k,
        }))
    }

    /// The unhinted outline of glyph `index` (a CID in a CID-keyed
    /// font) in font units, with the top dictionary's units per em and
    /// the glyph's font dictionary's.
    pub fn outline(&self, index: u32) -> Result<(outline::Outline, u32, u32), LoadError> {
        let gid = if self.is_cid {
            if index == 0 {
                0
            } else {
                let g = self.gid_of_cid(index);
                if g == 0 {
                    return Err(LoadError);
                }
                g
            }
        } else if index >= self.num_glyphs() {
            return Err(LoadError);
        } else {
            index
        };
        let &(s, e) = self.charstrings.items.get(gid as usize).ok_or(LoadError)?;
        let cs = self.data.get(s..e).ok_or(LoadError)?;
        let private = self
            .privates
            .get(self.fd_of(gid as usize))
            .cloned()
            .unwrap_or_default();
        let mut o = outline::charstring_outline(
            &Glyph {
                font: self,
                private: &private,
            },
            cs,
        )?;
        if let Some(m) = &private.matrix {
            apply_matrix(&mut o, m);
        }
        Ok((o, self.units_per_em, private.units_per_em))
    }

    fn item<'a>(&'a self, index: &Index, n: i64) -> Option<&'a [u8]> {
        let i = usize::try_from(n + bias(index.len())).ok()?;
        let &(s, e) = index.items.get(i)?;
        self.data.get(s..e)
    }

    /// The advance the charstring of `gid` sets, as `cff_slot_load` finds
    /// it for `FT_Get_Advance`: the width operand of its first
    /// stack-clearing operator, or the default width. None when the glyph
    /// does not exist or fails before its width.
    pub fn advance(&self, gid: u32) -> Option<f64> {
        let &(s, e) = self.charstrings.items.get(gid as usize)?;
        let private = self
            .privates
            .get(self.fd_of(gid as usize))
            .cloned()
            .unwrap_or_default();
        let cs = self.data.get(s..e)?;
        let glyph = Glyph {
            font: self,
            private: &private,
        };
        match outline::charstring_width(&glyph, cs) {
            Ok(Some(w)) => Some(private.nominal_width + w),
            Ok(None) => Some(private.default_width),
            Err(_) => None,
        }
    }
}

fn bias(n: usize) -> i64 {
    if n < 1240 {
        107
    } else if n < 33900 {
        1131
    } else {
        32768
    }
}

/// A glyph's charstring context: its font and private dictionary.
struct Glyph<'a> {
    font: &'a Cff,
    private: &'a Private,
}

impl Charstrings for Glyph<'_> {
    fn is_t1(&self) -> bool {
        false
    }

    fn local_subr(&self, n: i64) -> Option<&[u8]> {
        self.font.item(&self.private.subrs, n)
    }

    fn global_subr(&self, n: i64) -> Option<&[u8]> {
        self.font.item(&self.font.gsubrs, n)
    }

    /// `cff_lookup_glyph_by_stdcharcode`.
    fn seac_component(&self, code: i64) -> Option<&[u8]> {
        if self.font.is_cid {
            return None;
        }
        let sid = *names::STANDARD_ENCODING.get(usize::try_from(code).ok()?)?;
        let gid = self.font.sids.iter().position(|&s| s == sid)?;
        let &(s, e) = self.font.charstrings.items.get(gid)?;
        self.font.data.get(s..e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// An INDEX with four-byte offsets.
    fn index(items: &[Vec<u8>]) -> Vec<u8> {
        let mut out = (items.len() as u16).to_be_bytes().to_vec();
        if items.is_empty() {
            return out;
        }
        out.push(4);
        let mut off = 1u32;
        out.extend(off.to_be_bytes());
        for i in items {
            off += i.len() as u32;
            out.extend(off.to_be_bytes());
        }
        for i in items {
            out.extend(i);
        }
        out
    }

    /// A DICT operand: integers as five bytes (so sizes do not depend on
    /// values), others as reals.
    fn operand(v: f64) -> Vec<u8> {
        if v.fract() == 0.0 {
            let mut o = vec![29];
            o.extend((v as i32).to_be_bytes());
            return o;
        }
        let mut nibbles: Vec<u8> = format!("{v}")
            .bytes()
            .map(|c| match c {
                b'0'..=b'9' => c - b'0',
                b'.' => 0xa,
                _ => 0xe,
            })
            .collect();
        nibbles.push(0xf);
        if nibbles.len() % 2 == 1 {
            nibbles.push(0xf);
        }
        let mut o = vec![30];
        o.extend(nibbles.chunks(2).map(|p| p[0] << 4 | p[1]));
        o
    }

    type Dict = Vec<(Vec<f64>, u16)>;

    fn dict(entries: &[(Vec<f64>, u16)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (args, op) in entries {
            for &a in args {
                out.extend(operand(a));
            }
            if *op >= 1200 {
                out.extend([12, (*op - 1200) as u8]);
            } else {
                out.push(*op as u8);
            }
        }
        out
    }

    /// A predefined table by its offset, or one written into the font.
    #[derive(Clone)]
    enum Table {
        Predefined(u32),
        Data(Vec<u8>),
    }

    #[derive(Clone, Default)]
    struct Fd {
        top: Dict,
        private: Dict,
        subrs: Vec<Vec<u8>>,
    }

    /// The parts of a test font. With font dictionaries it is CID-keyed
    /// (its `top` should then hold a ROS).
    #[derive(Clone)]
    struct Build {
        name: Vec<u8>,
        top: Dict,
        strings: Vec<Vec<u8>>,
        gsubrs: Vec<Vec<u8>>,
        glyphs: Vec<Vec<u8>>,
        charset: Table,
        encoding: Table,
        private: Fd,
        fds: Vec<Fd>,
        fd_select: Vec<u8>,
    }

    impl Default for Build {
        fn default() -> Build {
            Build {
                name: b"Test".to_vec(),
                top: Vec::new(),
                strings: Vec::new(),
                gsubrs: Vec::new(),
                glyphs: vec![t2(&[(&[], ENDCHAR)])],
                charset: Table::Predefined(0),
                encoding: Table::Predefined(0),
                private: Fd::default(),
                fds: Vec::new(),
                fd_select: Vec::new(),
            }
        }
    }

    #[derive(Clone, Default, PartialEq)]
    struct Layout {
        charstrings: usize,
        charset: usize,
        encoding: usize,
        fd_select: usize,
        fd_array: usize,
        privates: Vec<(usize, usize)>,
    }

    impl Build {
        fn bytes(&self) -> Vec<u8> {
            let mut layout = Layout::default();
            loop {
                let (out, next) = self.assemble(&layout);
                if next == layout {
                    return out;
                }
                layout = next;
            }
        }

        fn assemble(&self, l: &Layout) -> (Vec<u8>, Layout) {
            let cid = !self.fds.is_empty();
            let table = |t: &Table, at: usize| match t {
                Table::Predefined(n) => f64::from(*n),
                Table::Data(_) => at as f64,
            };
            let private_at = |i: usize| {
                let (o, s) = l.privates.get(i).copied().unwrap_or_default();
                vec![s as f64, o as f64]
            };
            let mut top = self.top.clone();
            top.push((vec![l.charstrings as f64], 17));
            top.push((vec![table(&self.charset, l.charset)], 15));
            if cid {
                top.push((vec![l.fd_array as f64], 1236));
                top.push((vec![l.fd_select as f64], 1237));
            } else {
                top.push((vec![table(&self.encoding, l.encoding)], 16));
                top.push((private_at(0), 18));
            }
            let mut out = vec![1, 0, 4, 4];
            out.extend(index(std::slice::from_ref(&self.name)));
            out.extend(index(&[dict(&top)]));
            out.extend(index(&self.strings));
            out.extend(index(&self.gsubrs));
            let mut next = Layout {
                charstrings: out.len(),
                ..Layout::default()
            };
            out.extend(index(&self.glyphs));
            if let Table::Data(d) = &self.charset {
                next.charset = out.len();
                out.extend(d);
            }
            if let Table::Data(d) = &self.encoding {
                next.encoding = out.len();
                out.extend(d);
            }
            let fds = if cid {
                next.fd_select = out.len();
                out.extend(&self.fd_select);
                let dicts: Vec<Vec<u8>> = (self.fds.iter().enumerate())
                    .map(|(i, fd)| {
                        let mut t = fd.top.clone();
                        t.push((private_at(i), 18));
                        dict(&t)
                    })
                    .collect();
                next.fd_array = out.len();
                out.extend(index(&dicts));
                &self.fds[..]
            } else {
                std::slice::from_ref(&self.private)
            };
            for fd in fds {
                let mut entries = fd.private.clone();
                if !fd.subrs.is_empty() {
                    entries.push((vec![0.0], 19));
                    let len = dict(&entries).len();
                    entries.last_mut().unwrap_or_else(|| panic!()).0 = vec![len as f64];
                }
                let d = dict(&entries);
                next.privates.push((out.len(), d.len()));
                out.extend(&d);
                if !fd.subrs.is_empty() {
                    out.extend(index(&fd.subrs));
                }
            }
            (out, next)
        }

        fn open(&self) -> Cff {
            Cff::open(self.bytes().into()).unwrap_or_else(|e| panic!("{e}"))
        }
    }

    /// A Type 2 charstring number.
    fn num(v: f64) -> Vec<u8> {
        if v.fract() != 0.0 || !(-32768.0..=32767.0).contains(&v) {
            let mut o = vec![255];
            o.extend(((v * 65536.0) as i32).to_be_bytes());
            return o;
        }
        let v = v as i32;
        match v {
            -107..=107 => vec![(v + 139) as u8],
            108..=1131 => vec![((v - 108) / 256 + 247) as u8, ((v - 108) % 256) as u8],
            -1131..=-108 => vec![((-v - 108) / 256 + 251) as u8, ((-v - 108) % 256) as u8],
            _ => {
                let b = (v as i16).to_be_bytes();
                vec![28, b[0], b[1]]
            }
        }
    }

    /// Operators with their operands; from 1200 an escape, from 2000 a
    /// raw byte (`op - 2000`), as hint masks are.
    fn t2(ops: &[(&[f64], u16)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (args, op) in ops {
            for &a in *args {
                out.extend(num(a));
            }
            match *op {
                2000.. => out.push((*op - 2000) as u8),
                1200.. => out.extend([12, (*op - 1200) as u8]),
                _ => out.push(*op as u8),
            }
        }
        out
    }

    const HSTEM: u16 = 1;
    const VSTEM: u16 = 3;
    const VMOVETO: u16 = 4;
    const RLINETO: u16 = 5;
    const HLINETO: u16 = 6;
    const VLINETO: u16 = 7;
    const RRCURVETO: u16 = 8;
    const CALLSUBR: u16 = 10;
    const RETURN: u16 = 11;
    const ENDCHAR: u16 = 14;
    const HSTEMHM: u16 = 18;
    const HINTMASK: u16 = 19;
    const CNTRMASK: u16 = 20;
    const RMOVETO: u16 = 21;
    const HMOVETO: u16 = 22;
    const VSTEMHM: u16 = 23;
    const RCURVELINE: u16 = 24;
    const RLINECURVE: u16 = 25;
    const VVCURVETO: u16 = 26;
    const HHCURVETO: u16 = 27;
    const CALLGSUBR: u16 = 29;
    const VHCURVETO: u16 = 30;
    const HVCURVETO: u16 = 31;
    const MASK: u16 = 2000;

    fn sid(name: &str) -> f64 {
        names::SID_NAMES
            .iter()
            .position(|&n| n == name)
            .unwrap_or_else(|| panic!("{name}")) as f64
    }

    fn base14(name: &str) -> Cff {
        let d: Arc<[u8]> = super::super::base14(name).unwrap_or_default().into();
        Cff::open(d).unwrap_or_else(|e| panic!("{e}"))
    }

    const FLEX: u16 = 1235;
    const HFLEX: u16 = 1234;
    const HFLEX1: u16 = 1236;
    const FLEX1: u16 = 1237;

    /// Glyph names of `rich()` after `.notdef`; past the standard
    /// strings, they are its String INDEX.
    const RICH: [&str; 7] = ["A", "acute", "Aacute", "curves", "flex", "subrs", "arith"];

    /// A font whose glyphs use every Type 2 operator.
    fn rich() -> Build {
        let curves = t2(&[
            (&[10.0, 20.0, 30.0, 40.0], HSTEMHM),
            (&[50.0, 60.0], VSTEMHM),
            (&[], HINTMASK),
            (&[], MASK + 0xe0),
            (&[100.0, 100.0], RMOVETO),
            (&[], CNTRMASK),
            (&[], MASK + 0xe0),
            (&[100.0, 0.0], RLINETO),
            (&[50.0, 50.0, 50.0], HLINETO),
            (&[50.0, -50.0], VLINETO),
            (&[0.0, 50.0, 50.0, 50.0, 50.0, 0.0], RRCURVETO),
            (&[10.0, 0.0, 20.0, 10.0, 0.0, 20.0, 30.0, 0.0], RCURVELINE),
            (&[0.0, 10.0, 10.0, 10.0, 10.0, 0.0, 0.0, 10.0], RLINECURVE),
            (
                &[5.0, 10.0, 10.0, 10.0, 10.0, 0.0, 10.0, 10.0, 10.0],
                VVCURVETO,
            ),
            (&[5.0, 10.0, 10.0, 10.0, 10.0], HHCURVETO),
            (
                &[10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 5.0],
                VHCURVETO,
            ),
            (&[10.0, 10.0, 10.0, 10.0], HVCURVETO),
            (&[-450.0], HMOVETO),
            (&[50.0], VMOVETO),
            (&[10.0, 0.0, 0.0, 10.0], RLINETO),
            (&[], ENDCHAR),
        ]);
        let flex = t2(&[
            (&[0.0, 100.0], RMOVETO),
            (
                &[
                    10.0, 20.0, 30.0, 20.0, 40.0, 0.0, 40.0, 0.0, 30.0, -20.0, 10.0, -20.0, 50.0,
                ],
                FLEX,
            ),
            (&[10.0, 20.0, 30.0, 40.0, 10.0, 20.0, 30.0], HFLEX),
            (
                &[10.0, 10.0, 20.0, 20.0, 30.0, 30.0, 20.0, -20.0, 10.0],
                HFLEX1,
            ),
            (
                &[
                    10.0, 10.0, 20.0, 20.0, 30.0, 0.0, 30.0, 0.0, 20.0, -20.0, 10.0,
                ],
                FLEX1,
            ),
            (
                &[5.0, 30.0, 5.0, 40.0, 0.0, 30.0, 0.0, 30.0, -5.0, 40.0, 10.0],
                FLEX1,
            ),
            (&[], ENDCHAR),
        ]);
        let subrs = t2(&[
            (&[400.0, 0.0, 0.0], RMOVETO),
            (&[-107.0], CALLSUBR),
            (&[-107.0], CALLGSUBR),
            (&[-106.0], CALLGSUBR),
            (&[], ENDCHAR),
        ]);
        let arith = t2(&[
            (&[0.0, 0.0], RMOVETO),
            (&[2.0, 3.0], 1210),
            (&[4.0], 1224),
            (&[2.0], 1212),
            (&[], 1214),
            (&[], 1209),
            (&[], 1227),
            (&[], RLINETO),
            (&[1.0, 0.0], 1203),
            (&[1.0], 1204),
            (&[], 1205),
            (&[0.0], 1215),
            (&[], 1218),
            (&[16.0], 1226),
            (&[25.0, 0.0], 1220),
            (&[0.0], 1221),
            (&[], 1228),
            (&[], RLINETO),
            (&[5.0, 6.0, 7.0, 1.0], 1229),
            (&[4.0, 1.0], 1230),
            (&[], RLINETO),
            (&[1.0, 2.0, 3.0, 4.0], 1222),
            (&[0.0], 1228),
            (&[], RLINETO),
            (&[], 1223),
            (&[0.0], 1224),
            (&[5.0], 1210),
            (&[0.0], RLINETO),
            (&[9.0, -1.0], 1229),
            (&[3.0, 9.0], 1229),
            (&[1.0, 1.0], 1230),
            (&[], 1218),
            (&[], 1218),
            (&[], RLINETO),
            (&[10.5, 0.0], RLINETO),
            (&[], ENDCHAR),
        ]);
        let mut strings: Vec<Vec<u8>> = RICH[3..].iter().map(|s| s.as_bytes().to_vec()).collect();
        strings.push(b"Test Bold".to_vec());
        let mut charset = vec![0];
        for (i, name) in RICH.iter().enumerate() {
            let s = if i < 3 {
                sid(name) as u16
            } else {
                391 + i as u16 - 3
            };
            charset.extend(s.to_be_bytes());
        }
        Build {
            top: vec![
                (vec![0.001, 0.0, 0.0, 0.001, 0.0, 0.0], 1207),
                (vec![-50.5, -200.0, 1000.0, 900.25], 5),
            ],
            strings,
            gsubrs: vec![
                t2(&[(&[-100.0, 0.0], RLINETO), (&[], RETURN)]),
                t2(&[(&[0.0, -100.0], RLINETO)]),
            ],
            glyphs: vec![
                t2(&[(&[], ENDCHAR)]),
                t2(&[
                    (&[467.0, 100.0, 0.0], RMOVETO),
                    (&[300.0, 700.0, 300.0, -700.0], RLINETO),
                    (&[], ENDCHAR),
                ]),
                t2(&[
                    (&[200.0, 650.0], RMOVETO),
                    (&[50.0, 100.0, 50.0, -100.0], RLINETO),
                    (&[], ENDCHAR),
                ]),
                t2(&[(&[10.0, 20.0, 30.0, 65.0, 194.0], ENDCHAR)]),
                curves,
                flex,
                subrs,
                arith,
            ],
            charset: Table::Data(charset),
            private: Fd {
                private: vec![(vec![500.0], 20), (vec![100.0], 21)],
                subrs: vec![
                    t2(&[
                        (&[100.0, 0.0], RLINETO),
                        (&[-106.0], CALLSUBR),
                        (&[], RETURN),
                    ]),
                    t2(&[(&[0.0, 100.0], RLINETO), (&[], RETURN)]),
                ],
                ..Fd::default()
            },
            ..Build::default()
        }
    }

    fn cbox(x0: f64, y0: f64, x1: f64, y1: f64) -> Option<CBox> {
        Some(CBox { x0, y0, x1, y1 })
    }

    fn points(c: &Cff, gid: u32) -> Vec<(i64, i64)> {
        c.outline(gid).unwrap_or_else(|e| panic!("{e:?}")).0.points
    }

    // The outlines, boxes and widths below are MuPDF's, from a PDF that
    // shows each glyph of `rich()` (mutool draw -F svg for the outlines,
    // Text.getBounds in mutool run for the unhinted boxes, Font.advanceGlyph
    // for the widths).

    #[test]
    fn type2_lines_curves_and_hints_draw_as_mupdf_draws_them() {
        let c = rich().open();
        assert_eq!(c.num_glyphs(), 8);
        let gid = c.name_index(b"curves");
        assert_eq!(gid, 4);
        #[rustfmt::skip]
        let want = [
            (100, 100), (200, 100), (250, 100), (250, 150), (300, 150), (300, 200),
            (250, 200), (250, 250), (300, 300), (350, 300), (360, 300), (380, 310),
            (380, 330), (410, 330), (410, 340), (420, 350), (430, 350), (430, 360),
            (435, 370), (445, 380), (445, 390), (445, 390), (455, 400), (455, 410),
            (465, 415), (475, 425), (485, 425), (485, 435), (495, 445), (505, 445),
            (515, 445), (525, 455), (530, 465), (540, 465), (550, 475), (550, 485),
            (100, 535), (110, 535), (110, 545),
        ];
        let (o, top, sub) = c.outline(gid).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(o.points, want);
        assert_eq!(o.contours, [35, 38]);
        assert_eq!((top, sub), (1000, 1000));
        assert_eq!(c.outline_cbox(gid), Ok(cbox(100.0, 100.0, 550.0, 545.0)));
        assert_eq!(c.advance(gid), Some(500.0));
    }

    #[test]
    fn flex_subroutines_and_arithmetic_draw_as_mupdf_draws_them() {
        let c = rich().open();
        #[rustfmt::skip]
        let flex = [
            (0, 100), (10, 120), (40, 140), (80, 140), (120, 140), (150, 120),
            (160, 100), (170, 100), (190, 130), (230, 130), (240, 130), (260, 100),
            (290, 100), (300, 110), (320, 130), (350, 130), (380, 130), (400, 110),
            (410, 100), (420, 110), (440, 130), (470, 130), (500, 130), (520, 110),
            (530, 100), (535, 130), (540, 170), (540, 200), (540, 230), (535, 270),
            (530, 280),
        ];
        assert_eq!(points(&c, 5), flex);
        assert_eq!(c.outline_cbox(5), Ok(cbox(0.0, 100.0, 540.0, 280.0)));
        // Local subroutines, one calling another, and global ones, one
        // ending without a return.
        assert_eq!(
            points(&c, 6),
            [(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)]
        );
        assert_eq!(c.advance(6), Some(500.0));
        // add mul div neg abs dup, and or not eq drop sqrt put get exch,
        // index roll, ifelse, random; the last half unit is dropped.
        assert_eq!(
            points(&c, 7),
            [
                (0, 0),
                (10, 10),
                (35, 14),
                (41, 19),
                (47, 26),
                (47, 27),
                (52, 27),
                (61, 36),
                (71, 36),
            ]
        );
        assert_eq!(c.outline_cbox(7), Ok(cbox(0.0, 0.0, 71.0, 36.0)));
    }

    #[test]
    fn seac_by_endchar_draws_accent_then_base() {
        let c = rich().open();
        assert_eq!(points(&c, 1), [(100, 0), (400, 700), (700, 0)]);
        assert_eq!(points(&c, 2), [(200, 650), (250, 750), (300, 650)]);
        assert_eq!(
            points(&c, 3),
            [
                (220, 680),
                (270, 780),
                (320, 680),
                (100, 0),
                (400, 700),
                (700, 0)
            ]
        );
        assert_eq!(c.outline_cbox(3), Ok(cbox(100.0, 0.0, 700.0, 780.0)));
        assert_eq!(c.outline_cbox(0), Ok(None));
        assert_eq!(
            (c.advance(0), c.advance(1), c.advance(2), c.advance(3)),
            (Some(500.0), Some(567.0), Some(500.0), Some(110.0))
        );
        assert_eq!(c.outline(8).err(), Some(LoadError));
        assert_eq!(c.outline_cbox(8), Err(LoadError));
        assert_eq!(c.advance(8), None);
    }

    /// The width a one-glyph font (default 500, nominal 100) gives this
    /// charstring, with these local and global subroutines.
    fn width(glyph: Vec<u8>, subrs: Vec<Vec<u8>>, gsubrs: Vec<Vec<u8>>) -> Option<f64> {
        let b = Build {
            gsubrs,
            glyphs: vec![t2(&[(&[], ENDCHAR)]), glyph],
            private: Fd {
                private: vec![(vec![500.0], 20), (vec![100.0], 21)],
                subrs,
                ..Fd::default()
            },
            ..Build::default()
        };
        b.open().advance(1)
    }

    #[test]
    fn a_width_is_the_odd_operand_of_the_first_clearing_operator() {
        let w = |ops: &[(&[f64], u16)]| width(t2(ops), Vec::new(), Vec::new());
        assert_eq!(w(&[(&[50.0, 10.0, 20.0], HSTEM)]), Some(150.0));
        assert_eq!(w(&[(&[10.0, 20.0], VSTEM)]), Some(500.0));
        assert_eq!(w(&[(&[60.0, 10.0, 20.0], HINTMASK)]), Some(160.0));
        assert_eq!(w(&[(&[10.0, 20.0], CNTRMASK)]), Some(500.0));
        assert_eq!(w(&[(&[70.0, 5.0], HMOVETO)]), Some(170.0));
        assert_eq!(w(&[(&[80.0, 5.0], VMOVETO)]), Some(180.0));
        assert_eq!(w(&[(&[5.0], VMOVETO)]), Some(500.0));
        assert_eq!(w(&[(&[90.0, 0.0, 0.0], RMOVETO)]), Some(190.0));
        assert_eq!(w(&[(&[5.0], ENDCHAR)]), Some(105.0));
        assert_eq!(w(&[(&[1.0, 2.0], ENDCHAR)]), Some(500.0));
        // Each number encoding.
        assert_eq!(w(&[(&[1000.0, 1.0, 2.0], HSTEM)]), Some(1100.0));
        assert_eq!(w(&[(&[2000.0, 1.0, 2.0], HSTEM)]), Some(2100.0));
        assert_eq!(w(&[(&[-200.0, 1.0, 2.0], HSTEM)]), Some(-100.0));
        assert_eq!(w(&[(&[50.5, 1.0, 2.0], HSTEM)]), Some(150.5));
        // Arithmetic leaves its result for the width; a path operator
        // before any width means the default.
        assert_eq!(
            w(&[(&[1.0, 2.0], 1210), (&[30.0, 0.0, 0.0], RMOVETO)]),
            Some(103.0)
        );
        assert_eq!(w(&[(&[10.0, 10.0], RLINETO)]), Some(500.0));
        // A charstring cut short reads zeros, then ends as endchar would.
        for (cut, want) in [
            (vec![247], 208.0),
            (vec![251], -8.0),
            (vec![28, 1], 356.0),
            (vec![255, 0, 0], 100.0),
            (vec![12], 500.0),
        ] {
            assert_eq!(width(cut, Vec::new(), Vec::new()), Some(want));
        }
        assert_eq!(width(vec![139], Vec::new(), Vec::new()), Some(100.0));
        // Through subroutines: a width inside one, none in one that
        // returns; a call to one that does not exist or calls itself fails.
        let sub_width = t2(&[(&[40.0, 1.0, 2.0], HSTEM)]);
        let returns = t2(&[(&[], RETURN)]);
        let call = |op: u16| t2(&[(&[-107.0], op), (&[30.0, 0.0, 0.0], RMOVETO)]);
        assert_eq!(
            width(call(CALLSUBR), vec![sub_width.clone()], Vec::new()),
            Some(140.0)
        );
        assert_eq!(
            width(call(CALLGSUBR), Vec::new(), vec![returns.clone()]),
            Some(130.0)
        );
        assert_eq!(width(call(CALLSUBR), Vec::new(), Vec::new()), None);
        assert_eq!(width(vec![10], Vec::new(), Vec::new()), None);
        let me = t2(&[(&[-107.0], CALLSUBR)]);
        assert_eq!(width(call(CALLSUBR), vec![me], Vec::new()), None);
    }

    #[test]
    fn the_top_dictionary_sets_the_em_box_and_style() {
        let strings = vec![
            b"Test".to_vec(),
            b"Test Bold".to_vec(),
            b"Test-Black".to_vec(),
        ];
        let open = |name: &[u8], top: Dict| {
            Build {
                name: name.to_vec(),
                top,
                strings: strings.clone(),
                ..Build::default()
            }
            .open()
        };
        let c = open(
            b"Test",
            vec![
                (vec![0.0005, 0.0, 0.0, 0.0005, 0.0, 0.0], 1207),
                (vec![-10.5, -200.0, 600.25, 800.0], 5),
                (vec![-12.0], 1202),
                (vec![1.0], 1201),
                (vec![sid("Bold")], 4),
            ],
        );
        assert_eq!(c.units_per_em, 2000);
        assert_eq!(c.bbox, [-11, -200, 601, 800]);
        assert!(c.bold && c.italic && c.fixed_pitch && !c.is_cid);
        // A FontMatrix that does not scale y takes its skew; an angle
        // under a degree leans; Medium is not bold.
        let c = open(
            b"Test",
            vec![
                (vec![0.001, 0.00025, 0.0, 0.0, 0.0, 0.0], 1207),
                (vec![-0.5], 1202),
                (vec![sid("Medium")], 4),
            ],
        );
        assert_eq!(c.units_per_em, 4000);
        assert!(c.italic && !c.bold && !c.fixed_pitch);
        let c = open(b"Test", vec![(vec![0.0; 6], 1207)]);
        assert_eq!(c.units_per_em, 1000);
        // Bold or Black after the family name in the full name.
        let full = |sid: f64| vec![(vec![sid], 2)];
        let family = |mut top: Dict| {
            top.push((vec![391.0], 3));
            top
        };
        assert!(open(b"X", family(full(392.0))).bold);
        assert!(open(b"X", family(full(393.0))).bold);
        assert!(!open(b"X", family(full(391.0))).bold);
        assert!(!open(b"X", family(full(sid("Bold")))).bold);
        // Without a family name, the font's name less a subset prefix.
        assert!(open(b"ABCDEF+Test", full(392.0)).bold);
        assert!(open(b"Test", full(393.0)).bold);
        assert!(!open(b"abcdef+Test", full(392.0)).bold);
        assert!(!open(b"Test", full(500.0)).bold);
        assert!(!open(b"Test", Vec::new()).bold);
    }

    #[test]
    fn charsets_in_each_format() {
        let open = |glyphs: usize, charset: Table| {
            Cff::open(
                Build {
                    glyphs: vec![t2(&[(&[], ENDCHAR)]); glyphs],
                    charset,
                    ..Build::default()
                }
                .bytes()
                .into(),
            )
        };
        let names = |c: &Cff| -> Vec<String> {
            (0..c.num_glyphs())
                .map(|g| String::from_utf8_lossy(&c.glyph_name(g).unwrap_or_default()).into())
                .collect()
        };
        // Format 1 and 2 ranges.
        let a = sid("A") as u16;
        let mut f1 = vec![1];
        f1.extend(a.to_be_bytes());
        f1.push(1);
        f1.extend((a + 10).to_be_bytes());
        f1.push(0);
        let c = open(4, Table::Data(f1)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(names(&c), [".notdef", "A", "B", "K"]);
        assert_eq!(c.name_index(b"K"), 3);
        assert_eq!(c.name_index(b"Z"), 0);
        assert_eq!(c.glyph_name(9), None);
        let mut f2 = vec![2];
        f2.extend(a.to_be_bytes());
        f2.extend(2u16.to_be_bytes());
        let c = open(3, Table::Data(f2)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(names(&c), [".notdef", "A", "B"]);
        // A range that would pass SID 65535 stops there; a SID past the
        // strings has no name.
        let mut f2 = vec![2, 0xff, 0xfd];
        f2.extend(9u16.to_be_bytes());
        let c = open(4, Table::Data(f2)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.sids, [0, 0xfffd, 0xfffe, 0xffff]);
        assert_eq!(c.glyph_name(1), None);
        // The predefined charsets, and fonts with more glyphs than they
        // name.
        let c = open(3, Table::Predefined(1)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(names(&c), [".notdef", "space", "exclamsmall"]);
        let c = open(3, Table::Predefined(2)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(names(&c), [".notdef", "space", "dollaroldstyle"]);
        assert_eq!(
            names(&open(3, Table::Predefined(0)).unwrap_or_else(|e| panic!("{e}")))[2],
            "exclam"
        );
        for (n, charset) in [(230, 0), (167, 1), (88, 2)] {
            assert_eq!(
                open(n, Table::Predefined(charset)).err(),
                Some("Invalid_File_Format")
            );
        }
        // An unknown format, and tables cut short.
        for bad in [
            vec![3, 0, 0],
            vec![0, 0],
            vec![1, 0],
            vec![1, 0, 34],
            vec![2, 0, 34, 0],
        ] {
            assert_eq!(open(3, Table::Data(bad)).err(), Some("Invalid_File_Format"));
        }
        assert_eq!(
            open(3, Table::Predefined(99999)).err(),
            Some("Invalid_File_Format")
        );
    }

    /// Short, broken and odd charstrings, with what MuPDF makes of each:
    /// its advance (`Font.advanceGlyph`; 0 where FreeType fails) and its
    /// outline (mutool draw -F svg of a PDF showing each, unhinted boxes
    /// from Text.getBounds), in glyph order.
    #[test]
    fn broken_and_odd_charstrings_read_as_freetype_reads_them() {
        let me = t2(&[(&[-107.0], CALLSUBR)]);
        let b = Build {
            glyphs: vec![
                t2(&[(&[], ENDCHAR)]),
                t2(&[(&[-106.0], CALLSUBR), (&[30.0, 0.0, 0.0], RMOVETO)]),
                t2(&[(&[-107.0], CALLSUBR), (&[30.0, 0.0, 0.0], RMOVETO)]),
                vec![247],
                t2(&[(&[10.0, 10.0], RLINETO)]),
                t2(&[(&[1.0, 2.0], 1210), (&[30.0, 0.0, 0.0], RMOVETO)]),
                t2(&[(&[1.0, 2.0], ENDCHAR)]),
                t2(&[(&[1.0, 2.0], 1210), (&[30.0, 10.0], HSTEM)]),
                t2(&[(&[5.0, 1.0, 2.0], 1203), (&[30.0, 10.0], HSTEM)]),
                t2(&[(&[], 1223), (&[30.0, 10.0], HSTEM)]),
                vec![12],
                t2(&[(&[4.0, 1.0, 2.0], 1229), (&[30.0, 10.0], HSTEM)]),
                [
                    t2(&[(&[100.0, 100.0], RMOVETO), (&[50.0, 50.0], RLINETO)]),
                    vec![247],
                ]
                .concat(),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[50.0, 50.0], RLINETO),
                    (&[-106.0], CALLSUBR),
                ]),
                [
                    t2(&[(&[100.0, 100.0], RMOVETO), (&[50.0, 50.0], RLINETO)]),
                    vec![12],
                ]
                .concat(),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[50.0, 50.0, 30.0], RLINETO),
                    (&[0.0, 40.0], RLINETO),
                ]),
                t2(&[(&[100.0, 100.0], RMOVETO), (&[1.0; 50], RLINETO)]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 10.0], RRCURVETO),
                    (&[0.0, 40.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[10.0, 10.0, 10.0], VVCURVETO),
                    (&[0.0, 40.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 7.0], RCURVELINE),
                    (&[0.0, 40.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 7.0], RLINECURVE),
                    (&[0.0, 40.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[10.0, 10.0, 10.0], HVCURVETO),
                    (&[0.0, 40.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[10.0, 10.0, 10.0], FLEX),
                    (&[0.0, 40.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[50.0], RLINETO),
                    (&[0.0, 40.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[], 1218),
                    (&[0.0, 40.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[1.0, 0.0], 1212),
                    (&[0.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[10.0, 20.0, 5.0], 1230),
                    (&[0.0, 40.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[10.0, 40.0], 1220),
                    (&[99.0], 1221),
                    (&[0.0, 40.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[2.0, 3.0], 1210),
                    (&[], CALLSUBR),
                    (&[0.0, 40.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[0.0, 40.0], RLINETO),
                    (&[], RETURN),
                    (&[40.0, 0.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[0.0, 40.0], RLINETO),
                    (&[0.0], 1200),
                    (&[40.0, 0.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[0.0, 40.0], RLINETO),
                    (&[4.0, 4.0, 4.0, 4.0], 1201),
                    (&[40.0, 0.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[0.0, 40.0], RLINETO),
                    (&[4.0, 4.0, 4.0, 4.0], 1206),
                    (&[40.0, 0.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[0.0, 40.0], RLINETO),
                    (&[4.0, 4.0], 1207),
                    (&[40.0, 0.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[0.0, 40.0], RLINETO),
                    (&[4.0, 4.0], 1216),
                    (&[40.0, 0.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[0.0, 40.0], RLINETO),
                    (&[4.0, 4.0], 1233),
                    (&[40.0, 0.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[0.0, 40.0], RLINETO),
                    (&[4.0, 4.0], 1217),
                    (&[40.0, 0.0], RLINETO),
                ]),
                t2(&[
                    (&[100.0, 100.0], RMOVETO),
                    (&[0.0, 40.0], RLINETO),
                    (&[4.0, 4.0], 9),
                    (&[40.0, 0.0], RLINETO),
                    (&[4.0, 4.0], 13),
                    (&[0.0, 10.0], RLINETO),
                ]),
            ],
            private: Fd {
                private: vec![(vec![500.0], 20), (vec![100.0], 21)],
                subrs: vec![me],
                ..Fd::default()
            },
            ..Build::default()
        };
        let c = b.open();
        let fail = None;
        let widths = [
            Some(500.0),
            fail,
            fail,
            Some(208.0),
            Some(500.0),
            Some(103.0),
            Some(500.0),
            Some(103.0),
            Some(500.0),
            None,
            Some(500.0),
            Some(104.0),
        ];
        for (g, w) in widths.into_iter().enumerate() {
            // Glyph 9 uses random, which FreeType seeds per process.
            if g != 9 {
                assert_eq!(c.advance(g as u32), w, "glyph {g}");
            }
        }
        for g in 12..c.num_glyphs() {
            assert_eq!(c.advance(g), Some(500.0), "glyph {g}");
        }
        let v = vec![(100, 100), (100, 140)];
        let vh = vec![(100, 100), (100, 140), (140, 140)];
        let kept = vec![(100, 100), (100, 140), (104, 144), (144, 144)];
        let up = vec![(100, 100), (150, 150)];
        let want: Vec<Option<Vec<(i64, i64)>>> = vec![
            Some(vec![]),
            None,
            None,
            Some(vec![]),
            Some(vec![(0, 0), (10, 10)]),
            Some(vec![]),
            None,
            Some(vec![]),
            Some(vec![]),
            Some(vec![]),
            Some(vec![]),
            Some(vec![]),
            Some(up.clone()),
            None,
            Some(up),
            None,
            None,
            Some(vec![
                (100, 100),
                (110, 110),
                (120, 120),
                (130, 130),
                (130, 170),
            ]),
            Some(v.clone()),
            None,
            None,
            Some(v.clone()),
            None,
            None,
            None,
            Some(vec![(100, 100), (32868, 100)]),
            None,
            Some(v),
            None,
            None,
            Some(vh.clone()),
            Some(vh.clone()),
            Some(vh.clone()),
            Some(vh.clone()),
            Some(kept.clone()),
            Some(vh),
            Some(kept),
            Some(vec![(100, 100), (100, 140), (140, 140), (140, 150)]),
        ];
        assert_eq!(want.len(), c.num_glyphs() as usize);
        for (g, w) in want.into_iter().enumerate() {
            let got = c.outline(g as u32).ok().map(|o| o.0.points);
            assert_eq!(got, w, "glyph {g}");
        }
    }

    /// A font of glyphs named `names` (standard strings) with this
    /// encoding.
    fn encoded(names: &[&str], encoding: Table) -> Cff {
        let mut charset = vec![0];
        for n in names {
            charset.extend((sid(n) as u16).to_be_bytes());
        }
        Build {
            glyphs: vec![t2(&[(&[], ENDCHAR)]); names.len() + 1],
            charset: Table::Data(charset),
            encoding,
            ..Build::default()
        }
        .open()
    }

    #[test]
    fn encodings_in_each_format() {
        // Format 0: a code per glyph, past the glyphs ignored.
        let c = encoded(&["A", "B"], Table::Data(vec![0, 3, 65, 66, 67]));
        assert_eq!((c.codes[65], c.codes[66], c.codes[67]), (1, 2, 0));
        assert_eq!(c.encoding_count, 4);
        assert!(c.encoding_offset > 1);
        // Format 1: ranges, and supplements naming glyphs by SID.
        let mut f1 = vec![0x81, 2, 65, 1, 250, 9, 2, 200];
        f1.extend((sid("B") as u16).to_be_bytes());
        f1.push(201);
        f1.extend((sid("Z") as u16).to_be_bytes());
        let c = encoded(&["A", "B", "C"], Table::Data(f1));
        assert_eq!((c.codes[65], c.codes[66], c.codes[250]), (1, 2, 3));
        assert_eq!((c.codes[200], c.codes[201], c.codes[255]), (2, 0, 0));
        assert_eq!(c.encoding_count, 10);
        // An unknown format, and an offset past the data.
        let c = encoded(&["A"], Table::Data(vec![2, 1, 65]));
        assert_eq!((c.encoding_count, c.codes[65]), (0, 0));
        let c = encoded(&["A"], Table::Predefined(99999));
        assert_eq!((c.encoding_offset, c.encoding_count), (99999, 0));
        // The standard and expert encodings, by the glyphs' names.
        let c = encoded(&["A", "space", "Z"], Table::Predefined(0));
        assert_eq!((c.codes[65], c.codes[32], c.codes[90]), (1, 2, 3));
        assert_eq!(c.encoding_count, 91);
        let c = encoded(&["space", "dollaroldstyle"], Table::Predefined(1));
        assert_eq!((c.codes[32], c.codes[36], c.codes[65]), (1, 2, 0));
        assert_eq!((c.encoding_offset, c.encoding_count), (1, 37));
    }

    /// A CID-keyed font: glyphs 1 to 3 are CIDs 100 to 102. Font
    /// dictionary 0 has widths 300 and 30 and the top dictionary's em;
    /// dictionary 1 has widths 700 and 70 and an em of 2000.
    fn cid(fd_select: Vec<u8>, top_matrix: bool) -> Build {
        let square = t2(&[
            (&[100.0, 100.0], RMOVETO),
            (&[200.0, 0.0, 0.0, 200.0], RLINETO),
            (&[-107.0], CALLSUBR),
            (&[], ENDCHAR),
        ]);
        let mut charset = vec![2];
        charset.extend(100u16.to_be_bytes());
        charset.extend(2u16.to_be_bytes());
        let mut top = vec![(vec![391.0, 392.0, 0.0], 1230)];
        if top_matrix {
            top.push((vec![0.001, 0.0, 0.0, 0.001, 0.0, 0.0], 1207));
        }
        Build {
            top,
            strings: vec![b"Adobe".to_vec(), b"Identity".to_vec()],
            glyphs: vec![
                t2(&[(&[], ENDCHAR)]),
                t2(&[(&[50.0], ENDCHAR)]),
                square,
                t2(&[(&[0.0, 0.0, 65.0, 66.0], ENDCHAR)]),
            ],
            charset: Table::Data(charset),
            fds: vec![
                Fd {
                    private: vec![(vec![300.0], 20), (vec![30.0], 21)],
                    subrs: vec![t2(&[(&[-200.0, 0.0], RLINETO), (&[], RETURN)])],
                    ..Fd::default()
                },
                Fd {
                    top: vec![(vec![0.0005, 0.0, 0.0, 0.0005, 0.0, 0.0], 1207)],
                    private: vec![(vec![700.0], 20), (vec![70.0], 21)],
                    subrs: vec![t2(&[(&[0.0, -200.0], RLINETO), (&[], RETURN)])],
                },
            ],
            fd_select,
            ..Build::default()
        }
    }

    #[test]
    fn a_cid_font_finds_glyphs_by_cid_and_privates_by_fd_select() {
        // FDSelect format 0: glyphs 0 and 1 in dictionary 0, 2 and 3 in 1.
        let c = cid(vec![0, 0, 0, 1, 1], true).open();
        assert!(c.is_cid);
        assert_eq!(
            (c.max_cid(), c.gid_of_cid(101), c.gid_of_cid(7)),
            (102, 2, 0)
        );
        assert_eq!((c.glyph_name(1), c.name_index(b"A")), (None, 0));
        assert!(!c.bold);
        let adv: Vec<_> = (0..5).map(|g| c.advance(g)).collect();
        assert_eq!(
            adv,
            [Some(300.0), Some(80.0), Some(700.0), Some(700.0), None]
        );
        // CID 101 draws in dictionary 1's units, with its subroutines;
        // the box comes back in the top dictionary's.
        let (o, top, sub) = c.outline(101).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(o.points, [(100, 100), (300, 100), (300, 300), (300, 100)]);
        assert_eq!((top, sub), (1000, 2000));
        assert_eq!(c.outline_cbox(101), Ok(cbox(50.0, 50.0, 150.0, 150.0)));
        // CID 0 is glyph 0; a CID with no glyph fails; so does seac,
        // which a CID font does not have.
        assert_eq!(c.outline(0).map(|o| o.0.points), Ok(Vec::new()));
        assert_eq!(c.outline_cbox(0), Ok(None));
        assert_eq!(c.outline(5).err(), Some(LoadError));
        assert_eq!(c.outline_cbox(5), Err(LoadError));
        assert_eq!(c.outline(102).err(), Some(LoadError));

        // FDSelect format 3: glyphs from 2 in dictionary 1, below the
        // sentinel 3; past it, dictionary 0.
        let mut f3 = vec![3, 0, 2, 0, 0, 0, 0, 2, 1];
        f3.extend(3u16.to_be_bytes());
        let c = cid(f3, false).open();
        let adv: Vec<_> = (0..4).map(|g| c.advance(g)).collect();
        assert_eq!(adv, [Some(300.0), Some(80.0), Some(700.0), Some(300.0)]);
        // Without a top FontMatrix the dictionary's em stands alone.
        assert_eq!(c.outline(101).map(|o| (o.1, o.2)), Ok((1000, 2000)));
        // An unknown FDSelect format puts every glyph in dictionary 0.
        let c = cid(vec![9], true).open();
        assert_eq!(c.advance(2), Some(300.0));
        assert_eq!(c.outline_cbox(101), Ok(cbox(100.0, 100.0, 300.0, 300.0)));
    }

    #[test]
    fn broken_fonts_are_refused_or_read_as_far_as_they_go() {
        assert!(!Cff::recognize(&[1, 0, 3, 1]));
        assert!(!Cff::recognize(&[2, 0, 4, 1]));
        assert_eq!(
            Cff::open(vec![1, 0].into()).err(),
            Some("Unknown_File_Format")
        );
        let good = rich().bytes();
        // A Name INDEX with an offset size of 5; an empty Top DICT INDEX.
        let mut bad = good.clone();
        bad[6] = 5;
        assert_eq!(Cff::open(bad.into()).err(), Some("Invalid_File_Format"));
        let mut d = vec![1, 0, 4, 4];
        d.extend(index(&[b"T".to_vec()]));
        d.extend([0, 0, 0, 0, 0, 0]);
        assert_eq!(
            Cff::open(d.clone().into()).err(),
            Some("Invalid_File_Format")
        );
        // A Top DICT without CharStrings, or with them at a broken INDEX.
        let mut no_cs = vec![1, 0, 4, 4];
        no_cs.extend(index(&[b"T".to_vec()]));
        no_cs.extend(index(&[dict(&[(vec![1.0], 15)])]));
        no_cs.extend([0, 0, 0, 0]);
        assert_eq!(Cff::open(no_cs.into()).err(), Some("Invalid_File_Format"));
        let mut bad_cs = vec![1, 0, 4, 4];
        bad_cs.extend(index(&[b"T".to_vec()]));
        bad_cs.extend(index(&[dict(&[(vec![40.0], 17)])]));
        bad_cs.extend([0, 0, 0, 0]);
        bad_cs.resize(40, 0);
        bad_cs.extend([0, 1, 9]);
        assert_eq!(Cff::open(bad_cs.into()).err(), Some("Invalid_File_Format"));
        // A Private DICT past the end, Subrs at a broken INDEX, and an
        // FDArray at one: default widths, no subroutines.
        let b = Build {
            top: vec![(vec![0.0, 99999.0], 18)],
            glyphs: vec![t2(&[(&[], ENDCHAR)]), t2(&[(&[5.0], ENDCHAR)])],
            ..Build::default()
        };
        let mut bytes = b.bytes();
        // The Private operator the builder adds comes after the one above;
        // the first one found wins.
        let c = Cff::open(std::mem::take(&mut bytes).into()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.advance(1), Some(5.0));
        let b = Build {
            glyphs: vec![t2(&[(&[], ENDCHAR)]), t2(&[(&[-107.0], CALLSUBR)])],
            private: Fd {
                private: vec![(vec![99999.0], 19)],
                ..Fd::default()
            },
            ..Build::default()
        };
        assert_eq!(b.open().advance(1), None);
        let mut b = cid(vec![0, 0, 0, 1, 1], true);
        b.top.push((vec![99999.0], 1236));
        let c = b.open();
        assert_eq!(c.advance(2), Some(0.0));
        assert_eq!(c.outline(101).map(|o| o.2), Err(LoadError));
    }

    #[test]
    fn dict_operands_in_each_encoding() {
        let d = [
            28, 0x01, 0x00, // 256
            29, 0xff, 0xff, 0xff, 0xfe, // -2
            30, 0xe2, 0xa5, 0xb3, 0xff, // -2.5E3
            30, 0x1c, 0x2f, // 1E-2
            30, 0x1d, 0x5f, // 15, the reserved nibble passed over
            32, 246, 247, 0, 251, 0, 254, 255, // -107 107 108 -108 -1131
            255, 22, 31, // reserved bytes
            12, 7, // FontMatrix
            17,
        ];
        let got = read_dict(&d);
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[0],
            (
                1207,
                vec![
                    256.0, -2.0, -2500.0, 0.01, 15.0, -107.0, 107.0, 108.0, -108.0, -1131.0
                ]
            )
        );
        assert_eq!(got[1], (17, Vec::new()));
        // Operands cut short read zeros; an escape at the end is 1200.
        assert_eq!(read_dict(&[28, 1]), Vec::new());
        assert_eq!(read_dict(&[29, 1, 247]), Vec::new());
        assert_eq!(read_dict(&[247]), Vec::new());
        assert_eq!(read_dict(&[251, 0, 12]), vec![(1200, vec![-108.0])]);
        assert_eq!(read_dict(&[30, 0x1a]), Vec::new());
    }

    #[test]
    fn index_offsets_are_clamped_to_the_data() {
        // Two items whose second offset runs back, the last past the end.
        let d = [0, 3, 1, 1, 3, 2, 9, b'a', b'b', b'c'];
        let (i, end) = read_index(&d, 0).unwrap_or_else(|| panic!());
        assert_eq!(i.items, [(7, 9), (9, 9), (9, 10)]);
        assert_eq!(end, 10);
        assert_eq!(
            read_index(&[0, 0], 0).map(|(i, e)| (i.len(), e)),
            Some((0, 2))
        );
        assert!(read_index(&[0, 1, 0, 1, 1], 0).is_none());
        assert!(read_index(&[0, 1, 1, 1], 0).is_none());
        assert!(read_index(&[0], 0).is_none());
    }

    #[test]
    fn many_subroutines_take_a_bigger_bias() {
        for (n, bias) in [(1240, 1131.0), (33900, 32768.0)] {
            let mut subrs = vec![t2(&[(&[], RETURN)]); n];
            subrs[0] = t2(&[(&[60.0, 1.0, 2.0], VSTEM)]);
            assert_eq!(
                width(t2(&[(&[-bias], CALLSUBR)]), subrs.clone(), Vec::new()),
                Some(160.0)
            );
            assert_eq!(
                width(t2(&[(&[-bias], CALLGSUBR)]), Vec::new(), subrs.clone()),
                Some(160.0)
            );
            subrs[0] = t2(&[(&[70.0, 1.0, 2.0], HSTEM)]);
            assert_eq!(
                width(t2(&[(&[-bias], CALLSUBR)]), subrs, Vec::new()),
                Some(170.0)
            );
        }
    }

    // Boxes and widths of the bundled fonts below are MuPDF's: boxes from
    // Text.getBounds of a PDF showing each glyph with the base font, widths
    // from Font.advanceGlyph.

    #[test]
    fn the_base_fonts_read_as_mupdf_reads_them() {
        let c = base14("Helvetica");
        assert!(!c.is_cid && !c.bold && !c.italic && !c.fixed_pitch);
        assert_eq!(c.units_per_em, 1000);
        assert_eq!(c.encoding_offset, 0);
        let a = c.name_index(b"A");
        assert_eq!((a, c.codes[65]), (34, 34));
        assert_eq!(c.glyph_name(a).as_deref(), Some(&b"A"[..]));
        assert_eq!(c.advance(a), Some(667.0));
        assert_eq!(c.outline_cbox(34), Ok(cbox(17.0, 0.0, 653.0, 729.0)));
        assert_eq!(c.outline_cbox(72), Ok(cbox(35.0, -218.0, 481.0, 539.0)));
        assert_eq!(c.outline_cbox(138), Ok(cbox(11.0, 0.0, 950.0, 729.0)));
        assert_eq!(c.codes[225], 138);
        let c = base14("Times-BoldItalic");
        assert!(c.bold && c.italic);
        assert_eq!(c.outline_cbox(34), Ok(cbox(-67.0, 0.0, 593.0, 683.0)));
        assert!(base14("Courier").fixed_pitch);
        assert!(base14("Helvetica-Bold").bold);
        // Symbol and Dingbats carry encodings of their own.
        for (name, gid, want) in [
            ("Symbol", 66, cbox(41.0, -13.0, 622.0, 513.0)),
            ("ZapfDingbats", 66, cbox(35.0, -14.0, 754.0, 704.0)),
        ] {
            let c = base14(name);
            assert!(c.encoding_offset > 1, "{name}");
            assert_eq!(c.codes[97], gid, "{name}");
            assert_eq!(c.outline_cbox(u32::from(gid)), Ok(want), "{name}");
        }
    }

    #[test]
    fn a_seac_in_a_seac_part_too_many_hints_and_repeated_points() {
        let names = ["A", "acute", "Aacute", "B"];
        let mut charset = vec![0];
        for n in names {
            charset.extend((sid(n) as u16).to_be_bytes());
        }
        let mut many = Vec::new();
        for _ in 0..4 {
            many.extend(t2(&[(&[1.0; 48], HSTEMHM)]));
        }
        many.extend(t2(&[
            (&[1.0, 1.0], VSTEMHM),
            (&[], HINTMASK),
            (&[], ENDCHAR),
        ]));
        let c = Build {
            glyphs: vec![
                t2(&[(&[], ENDCHAR)]),
                t2(&[
                    (&[0.0, 0.0], RMOVETO),
                    (&[0.0, 0.0, 10.0, 3.0], RLINETO),
                    (&[10.0, 3.0], 1211),
                    (&[0.0], RLINETO),
                    (&[], ENDCHAR),
                ]),
                t2(&[(&[0.0, 0.0, 65.0, 65.0], ENDCHAR)]),
                t2(&[(&[0.0, 0.0, 65.0, 194.0], ENDCHAR)]),
                many,
            ],
            charset: Table::Data(charset),
            ..Build::default()
        }
        .open();
        // A line to where the pen is adds no point; sub subtracts.
        assert_eq!(points(&c, 1), [(0, 0), (10, 3), (17, 3)]);
        // The accent is a seac itself: FreeType refuses the nesting.
        assert_eq!(points(&c, 2).len(), 6);
        assert_eq!(c.outline(3).err(), Some(LoadError));
        // 97 stems are one more than a hint mask holds.
        assert_eq!(c.outline(4).err(), Some(LoadError));
    }

    #[test]
    fn a_skewed_font_matrix_slants_the_outlines() {
        // FreeType applies what is left of the FontMatrix once its scale
        // is the em (here a slant of 0.3) to every outline and control
        // box; nupdf took only the em from it, so oblique faces made by
        // their matrix drew upright.
        let glyph = t2(&[
            (&[500.0, 100.0, 0.0], RMOVETO),
            (&[400.0], HLINETO),
            (&[600.0], VLINETO),
            (&[-400.0], HLINETO),
            (&[], ENDCHAR),
        ]);
        let build = Build {
            top: vec![(vec![0.001, 0.0, 0.0003, 0.001, 0.0, 0.0], 1207)],
            glyphs: vec![t2(&[(&[], ENDCHAR)]), glyph],
            ..Build::default()
        };
        let c = build.open();
        assert_eq!(c.units_per_em, 1000);
        let (o, _, _) = c.outline(1).unwrap_or_else(|e| panic!("{e:?}"));
        // x + 0.3 y: the top edge moves 180 units right of the bottom.
        assert_eq!(o.points, [(100, 0), (500, 0), (680, 600), (280, 600)]);
        let b = c.outline_cbox(1).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            b,
            Some(CBox {
                x0: 100.0,
                y0: 0.0,
                x1: 680.0,
                y1: 600.0
            })
        );
        // And drawn: a space (code 32, glyph 1 of the ISOAdobe charset)
        // in an embedded Type1C font, as mutool draw draws it.
        let font = crate::draw::testkit::hex_stream("/Subtype/Type1C", &build.bytes());
        let pdf = crate::draw::testkit::page(
            128,
            128,
            "/Font<</F 5 0 R>>",
            "BT /F 100 Tf 10 10 Td ( ) Tj ET",
            &[
                "<</Type/Font/Subtype/Type1/BaseFont/Test/FirstChar 32/LastChar 32/Widths[1000]/FontDescriptor 6 0 R>>",
                "<</Type/FontDescriptor/FontName/Test/Flags 32/FontBBox[0 0 1000 1000]/ItalicAngle 0/Ascent 1000/Descent 0/CapHeight 700/StemV 80/FontFile3 7 0 R>>",
                &font,
            ],
        );
        crate::draw::testkit::check(
            "font-cff-skewed-matrix",
            &pdf,
            false,
            "c1d3fddf1b1125b3baaaef1fb1250d77",
        );
    }
}
