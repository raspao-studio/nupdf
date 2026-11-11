// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Following FreeType 2 src/type1/t1parse.c, src/type1/t1load.c,
// src/type1/t1objs.c and src/psaux/t1decode.c (glyph widths), Copyright
// (C) 1996-2025 by David Turner, Robert Wilhelm, and Werner Lemberg, used
// under the FreeType License (NOTICE.md).

//! Type 1 fonts, as FreeType's Type 1 driver reads them for MuPDF: the
//! font matrix, the encoding, the charstrings in their order (with
//! `.notdef` made glyph 0), and the advance a glyph's `hsbw` sets.

use super::outline::{self, CBox, Charstrings, LoadError};
use std::sync::Arc;

/// How the font names its encoding.
#[derive(Debug, Clone, PartialEq)]
pub enum Encoding {
    None,
    Standard,
    Expert,
    IsoLatin1,
    /// The glyph each code maps to, for codes below `count`.
    Array {
        indices: Vec<u16>,
        count: u32,
    },
}

#[derive(Debug, Clone)]
pub struct Type1 {
    pub names: Vec<Vec<u8>>,
    charstrings: Vec<Vec<u8>>,
    subrs: Vec<Vec<u8>>,
    pub encoding: Encoding,
    pub units_per_em: u16,
    pub bbox: [i32; 4],
    /// FontInfo's style, as FreeType's Type 1 driver reads it.
    pub bold: bool,
    pub italic: bool,
    pub fixed_pitch: bool,
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | 0)
}

fn is_delim(b: u8) -> bool {
    is_space(b)
        || matches!(
            b,
            b'/' | b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'%'
        )
}

/// eexec and charstring decryption.
fn decrypt(data: &[u8], key: u16) -> Vec<u8> {
    let mut r = key;
    data.iter()
        .map(|&c| {
            let p = c ^ (r >> 8) as u8;
            r = (u16::from(c).wrapping_add(r))
                .wrapping_mul(52845)
                .wrapping_add(22719);
            p
        })
        .collect()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A PostScript token scanner over one part of the font.
struct Scanner<'a> {
    d: &'a [u8],
    at: usize,
}

impl<'a> Scanner<'a> {
    fn skip_space(&mut self) {
        while let Some(&b) = self.d.get(self.at) {
            if is_space(b) {
                self.at += 1;
            } else if b == b'%' {
                while let Some(&c) = self.d.get(self.at) {
                    if c == b'\r' || c == b'\n' {
                        break;
                    }
                    self.at += 1;
                }
            } else {
                break;
            }
        }
    }

    /// The next token, as its bytes (strings and procedures whole).
    fn token(&mut self) -> Option<&'a [u8]> {
        self.skip_space();
        let start = self.at;
        let &b = self.d.get(self.at)?;
        match b {
            b'(' => {
                let mut depth = 0;
                while let Some(&c) = self.d.get(self.at) {
                    self.at += 1;
                    match c {
                        b'\\' => self.at += 1,
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            b'{' => {
                let mut depth = 0;
                while let Some(&c) = self.d.get(self.at) {
                    self.at += 1;
                    match c {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            b'[' | b']' => self.at += 1,
            b'<' => {
                while let Some(&c) = self.d.get(self.at) {
                    self.at += 1;
                    if c == b'>' {
                        break;
                    }
                }
            }
            b'/' => {
                self.at += 1;
                while let Some(&c) = self.d.get(self.at) {
                    if is_delim(c) {
                        break;
                    }
                    self.at += 1;
                }
            }
            _ => {
                self.at += 1;
                while let Some(&c) = self.d.get(self.at) {
                    if is_delim(c) {
                        break;
                    }
                    self.at += 1;
                }
            }
        }
        self.d.get(start..self.at.min(self.d.len()))
    }

    fn int(&mut self) -> Option<i64> {
        std::str::from_utf8(self.token()?).ok()?.parse().ok()
    }

    /// `read_binary_data`: a length, an `RD`-like token, one space, then
    /// that many bytes.
    fn binary(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.int()?).ok()?;
        self.token()?;
        self.at += 1;
        let bytes = self.d.get(self.at..self.at + n)?;
        self.at += n;
        Some(bytes)
    }
}

fn numbers(tok: &[u8]) -> Vec<f64> {
    let inner = tok
        .strip_prefix(b"[")
        .or_else(|| tok.strip_prefix(b"{"))
        .unwrap_or(tok);
    String::from_utf8_lossy(inner)
        .split(|c: char| c.is_whitespace() || c == ']' || c == '}' || c == '[' || c == '{')
        .filter_map(|s| s.parse().ok())
        .collect()
}

impl Type1 {
    /// `T1_New_Parser`'s check of the header.
    pub fn recognize(data: &[u8]) -> bool {
        let d = if data.first() == Some(&0x80) {
            data.get(6..).unwrap_or_default()
        } else {
            data
        };
        d.starts_with(b"%!PS-AdobeFont") || d.starts_with(b"%!FontType")
    }

    pub fn open(data: Arc<[u8]>) -> Result<Type1, &'static str> {
        let d: &[u8] = &data;
        if !Self::recognize(d) {
            return Err("Unknown_File_Format");
        }
        // PFB segments joined; otherwise the stream as it is.
        let joined;
        let d: &[u8] = if d.first() == Some(&0x80) {
            let mut out = Vec::new();
            let mut at = 0;
            while d.get(at) == Some(&0x80) {
                let kind = *d.get(at + 1).unwrap_or(&3);
                if kind == 3 {
                    break;
                }
                let len = d
                    .get(at + 2..at + 6)
                    .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
                let seg = d.get(at + 6..at + 6 + len).unwrap_or_default();
                out.extend_from_slice(seg);
                at += 6 + len;
            }
            joined = out;
            &joined
        } else {
            d
        };
        let eexec = find(d, b"eexec").ok_or("Invalid_File_Format")?;
        let clear = &d[..eexec];
        let mut rest = eexec + 5;
        while d.get(rest).is_some_and(|&b| is_space(b)) {
            rest += 1;
        }
        let enc = d.get(rest..).unwrap_or_default();
        // Hex when the first four bytes are hex digits.
        let hex = enc.len() >= 4 && enc[..4].iter().all(u8::is_ascii_hexdigit);
        let binary: Vec<u8> = if hex {
            let mut out = Vec::new();
            let mut hi: Option<u8> = None;
            for &c in enc {
                let v = match c {
                    b'0'..=b'9' => c - b'0',
                    b'a'..=b'f' => c - b'a' + 10,
                    b'A'..=b'F' => c - b'A' + 10,
                    _ if is_space(c) => continue,
                    _ => break,
                };
                match hi.take() {
                    Some(h) => out.push(h << 4 | v),
                    None => hi = Some(v),
                }
            }
            out
        } else {
            enc.to_vec()
        };
        let private = decrypt(&binary, 55665);
        let private = private.get(4..).unwrap_or_default();

        let mut font = Type1 {
            names: Vec::new(),
            charstrings: Vec::new(),
            subrs: Vec::new(),
            encoding: Encoding::None,
            units_per_em: 0,
            bbox: [0; 4],
            bold: false,
            italic: false,
            fixed_pitch: false,
        };
        let mut encoding_names: Vec<Option<Vec<u8>>> = Vec::new();
        let mut s = Scanner { d: clear, at: 0 };
        while let Some(tok) = s.token() {
            match tok {
                b"/FontMatrix" => {
                    let m = numbers(s.token().unwrap_or_default());
                    if m.len() >= 4 {
                        let t = m[3].abs() * 1000.0;
                        if t == 0.0 {
                            return Err("Invalid_File_Format");
                        }
                        if (t - 1.0).abs() > 1e-9 {
                            font.units_per_em = (1000.0 / t) as u16;
                        }
                    }
                }
                // T1_ToInt: the integer part, so -0.5 is upright.
                b"/ItalicAngle" => {
                    let t = s.token().unwrap_or_default();
                    font.italic = numbers(t).first().is_some_and(|v| v.trunc() != 0.0);
                }
                b"/Weight" => {
                    let t = s.token().unwrap_or_default();
                    let w = t.strip_prefix(b"(").and_then(|t| t.strip_suffix(b")"));
                    font.bold = matches!(w, Some(b"Bold" | b"Black"));
                }
                b"/isFixedPitch" => font.fixed_pitch = s.token() == Some(b"true"),
                b"/FontBBox" => {
                    let b = numbers(s.token().unwrap_or_default());
                    if b.len() >= 4 {
                        font.bbox = [
                            b[0].floor() as i32,
                            b[1].floor() as i32,
                            b[2].ceil() as i32,
                            b[3].ceil() as i32,
                        ];
                    }
                }
                b"/Encoding" => {
                    let save = s.at;
                    match s.token() {
                        Some(b"StandardEncoding") => font.encoding = Encoding::Standard,
                        Some(b"ExpertEncoding") => font.encoding = Encoding::Expert,
                        Some(b"ISOLatin1Encoding") => font.encoding = Encoding::IsoLatin1,
                        Some(t) if t.first().is_some_and(u8::is_ascii_digit) => {
                            let count: usize = std::str::from_utf8(t)
                                .ok()
                                .and_then(|t| t.parse().ok())
                                .unwrap_or(0)
                                .min(256);
                            encoding_names = vec![None; count];
                            // `dup <code> /<name> put` until `def`.
                            while let Some(t) = s.token() {
                                match t {
                                    b"def" | b"readonly" => break,
                                    b"dup" => {
                                        let code = s.int();
                                        let name = s.token();
                                        if let (Some(c), Some(n)) = (code, name)
                                            && let Some(slot) = usize::try_from(c)
                                                .ok()
                                                .and_then(|c| encoding_names.get_mut(c))
                                            && let Some(n) = n.strip_prefix(b"/")
                                        {
                                            *slot = Some(n.to_vec());
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            font.encoding = Encoding::Array {
                                indices: Vec::new(),
                                count: 0,
                            };
                        }
                        Some(t) if t.starts_with(b"[") => {
                            s.at = save;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        let mut p = Scanner { d: private, at: 0 };
        let mut len_iv: i64 = 4;
        let mut notdef: Option<usize> = None;
        while let Some(tok) = p.token() {
            match tok {
                b"/lenIV" => len_iv = p.int().unwrap_or(4),
                b"/Subrs" => {
                    let n = p.int().unwrap_or(0).max(0) as usize;
                    font.subrs = vec![Vec::new(); n.min(65536)];
                    // `dup <i> <len> RD <bytes> NP`, n times.
                    while let Some(t) = p.token() {
                        if t == b"dup" {
                            let idx = p.int();
                            let bin = p.binary();
                            if let (Some(i), Some(b)) = (idx, bin)
                                && let Some(slot) =
                                    usize::try_from(i).ok().and_then(|i| font.subrs.get_mut(i))
                            {
                                *slot = charstring(b, len_iv);
                            }
                        } else if t == b"def" || t == b"ND" || t == b"|-" || t.starts_with(b"/") {
                            if t.starts_with(b"/") {
                                p.at -= t.len();
                            }
                            break;
                        }
                    }
                }
                b"/CharStrings" => {
                    let _count = p.int();
                    loop {
                        p.skip_space();
                        let start = p.at;
                        let rest = p.d.get(start..).unwrap_or_default();
                        if rest.len() > 3 && is_delim(rest[3]) {
                            if rest.starts_with(b"def") && !font.names.is_empty() {
                                break;
                            }
                            if rest.starts_with(b"end") {
                                break;
                            }
                        }
                        let Some(t) = p.token() else {
                            break;
                        };
                        if let Some(name) = t.strip_prefix(b"/") {
                            let Some(bin) = p.binary() else {
                                break;
                            };
                            if name == b".notdef" && notdef.is_none() {
                                notdef = Some(font.names.len());
                            }
                            font.names.push(name.to_vec());
                            font.charstrings.push(charstring(bin, len_iv));
                        }
                    }
                }
                _ => {}
            }
        }
        if font.names.is_empty() {
            return Err("Invalid_File_Format");
        }
        // `.notdef` becomes glyph 0; without one, one is added there.
        match notdef {
            Some(0) => {}
            Some(n) => {
                font.names.swap(0, n);
                font.charstrings.swap(0, n);
            }
            None => {
                let first_name = std::mem::replace(&mut font.names[0], b".notdef".to_vec());
                let first_cs =
                    std::mem::replace(&mut font.charstrings[0], vec![0x8b, 0xf7, 0xe1, 0x0d, 0x0e]);
                font.names.push(first_name);
                font.charstrings.push(first_cs);
            }
        }
        if let Encoding::Array { indices, count } = &mut font.encoding {
            let mut max = 0u32;
            for (code, name) in encoding_names.iter().enumerate() {
                let mut g = 0u16;
                if let Some(n) = name
                    && let Some(i) = font.names.iter().position(|x| x == n)
                {
                    g = i as u16;
                    if n.as_slice() != b".notdef" {
                        max = max.max(code as u32 + 1);
                    }
                }
                indices.push(g);
            }
            *count = max;
        }
        if font.units_per_em == 0 {
            font.units_per_em = 1000;
        }
        Ok(font)
    }

    /// `T1_Get_Name_Index` (FreeType's `t1_get_name_index`).
    pub fn name_index(&self, name: &[u8]) -> u32 {
        self.names
            .iter()
            .position(|n| n == name)
            .map_or(0, |i| i as u32)
    }

    pub fn glyph_name(&self, gid: u32) -> Option<Vec<u8>> {
        self.names.get(gid as usize).cloned()
    }

    /// The control box of glyph `gid`, in font units.
    pub fn outline_cbox(&self, gid: u32) -> Result<Option<CBox>, LoadError> {
        let cs = self.charstrings.get(gid as usize).ok_or(LoadError)?;
        outline::charstring_cbox(self, cs)
    }

    /// The width `hsbw` or `sbw` sets, rounded, as `T1_Get_Advances`
    /// returns it.
    pub fn advance(&self, gid: u32) -> Option<i64> {
        let cs = self.charstrings.get(gid as usize)?;
        let mut stack: Vec<f64> = Vec::new();
        Some(
            self.width(cs, &mut stack, 0)
                .map_or(0, |w| w.round() as i64),
        )
    }

    fn width(&self, cs: &[u8], stack: &mut Vec<f64>, depth: usize) -> Option<f64> {
        if depth > 10 {
            return None;
        }
        let mut i = 0;
        while i < cs.len() {
            let b0 = cs[i];
            i += 1;
            match b0 {
                32..=246 => stack.push(f64::from(i32::from(b0) - 139)),
                247..=250 => {
                    let b1 = i32::from(*cs.get(i)?);
                    stack.push(f64::from((i32::from(b0) - 247) * 256 + b1 + 108));
                    i += 1;
                }
                251..=254 => {
                    let b1 = i32::from(*cs.get(i)?);
                    stack.push(f64::from(-(i32::from(b0) - 251) * 256 - b1 - 108));
                    i += 1;
                }
                255 => {
                    let b = cs.get(i..i + 4)?;
                    stack.push(f64::from(i32::from_be_bytes([b[0], b[1], b[2], b[3]])));
                    i += 4;
                }
                // hsbw: sbx wx.
                13 => return stack.get(1).copied(),
                // callsubr.
                10 => {
                    let n = stack.pop()? as i64;
                    let sub = self.subrs.get(usize::try_from(n).ok()?)?;
                    if let Some(w) = self.width(sub, stack, depth + 1) {
                        return Some(w);
                    }
                }
                // return.
                11 => return None,
                12 => {
                    let b1 = *cs.get(i)?;
                    i += 1;
                    match b1 {
                        // sbw: sbx sby wx wy.
                        7 => return stack.get(2).copied(),
                        // div.
                        12 => {
                            let b = stack.pop()?;
                            let a = stack.pop()?;
                            stack.push(if b != 0.0 { a / b } else { 0.0 });
                        }
                        _ => stack.clear(),
                    }
                }
                _ => return None,
            }
        }
        None
    }
}

/// A charstring decrypted and its first `lenIV` bytes dropped (a negative
/// lenIV means not encrypted).
fn charstring(bin: &[u8], len_iv: i64) -> Vec<u8> {
    if len_iv < 0 {
        return bin.to_vec();
    }
    let plain = decrypt(bin, 4330);
    plain.get(len_iv as usize..).unwrap_or_default().to_vec()
}

impl Charstrings for Type1 {
    fn is_t1(&self) -> bool {
        true
    }

    fn local_subr(&self, n: i64) -> Option<&[u8]> {
        self.subrs.get(usize::try_from(n).ok()?).map(Vec::as_slice)
    }

    fn global_subr(&self, _: i64) -> Option<&[u8]> {
        None
    }

    /// `t1_lookup_glyph_by_stdcharcode_ps`.
    fn seac_component(&self, code: i64) -> Option<&[u8]> {
        let sid = *super::names::STANDARD_ENCODING.get(usize::try_from(code).ok()?)?;
        let name = super::names::SID_NAMES.get(usize::from(sid))?.as_bytes();
        let gid = self.names.iter().position(|n| n == name)?;
        self.charstrings.get(gid).map(Vec::as_slice)
    }
}
