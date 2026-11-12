// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Following FreeType 2 src/cff/cffload.c, src/cff/cffparse.c,
// src/cff/cffobjs.c and src/psaux/psintrp.c (glyph widths), Copyright (C)
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
        let cbox = outline::charstring_cbox(
            &Glyph {
                font: self,
                private: &private,
            },
            cs,
        )?;
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
        let o = outline::charstring_outline(
            &Glyph {
                font: self,
                private: &private,
            },
            cs,
        )?;
        Ok((o, self.units_per_em, private.units_per_em))
    }

    fn item<'a>(&'a self, index: &Index, n: i64) -> Option<&'a [u8]> {
        let i = usize::try_from(n + bias(index.len())).ok()?;
        let &(s, e) = index.items.get(i)?;
        self.data.get(s..e)
    }

    /// The advance the charstring of `gid` sets: the width operand of its
    /// first stack-clearing operator, or the default width. None when the
    /// glyph does not exist.
    pub fn advance(&self, gid: u32) -> Option<f64> {
        let &(s, e) = self.charstrings.items.get(gid as usize)?;
        let private = self
            .privates
            .get(self.fd_of(gid as usize))
            .cloned()
            .unwrap_or_default();
        let cs = self.data.get(s..e)?;
        let mut state = Width {
            font: self,
            private: &private,
            stack: Vec::new(),
            hints: 0,
            depth: 0,
        };
        Some(match state.run(cs) {
            Some(Some(w)) => private.nominal_width + w,
            _ => private.default_width,
        })
    }
}

/// Enough of the Type 2 interpreter to find a glyph's width.
struct Width<'a> {
    font: &'a Cff,
    private: &'a Private,
    stack: Vec<f64>,
    hints: usize,
    depth: usize,
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

impl Width<'_> {
    /// Some(Some(width)), Some(None) for no width, None at the end of the
    /// charstring or an error.
    fn run(&mut self, cs: &[u8]) -> Option<Option<f64>> {
        if self.depth > 10 {
            return None;
        }
        let mut i = 0;
        while i < cs.len() {
            let b0 = cs[i];
            i += 1;
            match b0 {
                28 => {
                    let v = i16::from_be_bytes([*cs.get(i)?, *cs.get(i + 1)?]);
                    self.stack.push(f64::from(v));
                    i += 2;
                }
                32..=246 => self.stack.push(f64::from(i32::from(b0) - 139)),
                247..=250 => {
                    let b1 = i32::from(*cs.get(i)?);
                    self.stack
                        .push(f64::from((i32::from(b0) - 247) * 256 + b1 + 108));
                    i += 1;
                }
                251..=254 => {
                    let b1 = i32::from(*cs.get(i)?);
                    self.stack
                        .push(f64::from(-(i32::from(b0) - 251) * 256 - b1 - 108));
                    i += 1;
                }
                255 => {
                    let b = cs.get(i..i + 4)?;
                    let v = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                    self.stack.push(f64::from(v) / 65536.0);
                    i += 4;
                }
                // hstem, vstem, hstemhm, vstemhm.
                1 | 3 | 18 | 23 => {
                    if self.stack.len() % 2 == 1 {
                        return Some(Some(self.stack[0]));
                    }
                    return Some(None);
                }
                // hintmask, cntrmask: implicit vstem hints first.
                19 | 20 => {
                    if self.stack.len() % 2 == 1 {
                        return Some(Some(self.stack[0]));
                    }
                    return Some(None);
                }
                // rmoveto.
                21 => return Some((self.stack.len() > 2).then(|| self.stack[0])),
                // hmoveto, vmoveto.
                22 | 4 => return Some((self.stack.len() > 1).then(|| self.stack[0])),
                // endchar.
                14 => {
                    let n = self.stack.len();
                    return Some((n == 1 || n == 5).then(|| self.stack[0]));
                }
                // callsubr, callgsubr.
                10 | 29 => {
                    let idx = self.stack.pop()? as i64;
                    let index = if b0 == 10 {
                        &self.private.subrs
                    } else {
                        &self.font.gsubrs
                    };
                    let n = idx + bias(index.len());
                    let &(s, e) = index.items.get(usize::try_from(n).ok()?)?;
                    let sub = self.font.data.get(s..e)?.to_vec();
                    self.depth += 1;
                    let out = self.run(&sub);
                    self.depth -= 1;
                    if out.is_some() {
                        return out;
                    }
                }
                // return.
                11 => return None,
                12 => {
                    i += 1;
                    // Arithmetic and other escapes do not clear for a
                    // width; drop their operands as the stack allows.
                    self.stack.clear();
                }
                // Path operators before any width: no width.
                _ => return Some(None),
            }
            let _ = self.hints;
        }
        None
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
