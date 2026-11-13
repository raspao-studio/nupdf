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

/// The numbers of the array the scanner is at: a `{...}` token, or the
/// tokens from `[` to `]`.
fn array_numbers(s: &mut Scanner<'_>) -> Vec<f64> {
    let tok = s.token().unwrap_or_default();
    if tok != b"[" {
        return numbers(tok);
    }
    let mut out = Vec::new();
    // An array of more than a few dozen numbers is not a matrix or box.
    for _ in 0..64 {
        match s.token() {
            None | Some(b"]") => break,
            Some(t) => out.extend(numbers(t)),
        }
    }
    out
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

/// A PFB segment header at `at`: its type and length.
fn pfb_segment(d: &[u8], at: usize) -> Option<(u8, usize)> {
    let h = d.get(at..at + 6)?;
    (h[0] == 0x80).then(|| (h[1], u32::from_le_bytes([h[2], h[3], h[4], h[5]]) as usize))
}

/// Where the `eexec` keyword starts, looking only at token starts as
/// `T1_Get_Private_Dict` does, so one in a comment or a string is passed.
fn find_eexec(d: &[u8]) -> Option<usize> {
    let mut s = Scanner { d, at: 0 };
    loop {
        s.skip_space();
        if d[s.at.min(d.len())..].starts_with(b"eexec") && s.at + 9 < d.len() {
            return Some(s.at);
        }
        s.token()?;
    }
}

/// eexec-encrypted text in hexadecimal: pairs of digits, blanks between
/// them, up to the first other byte.
fn from_hex(enc: &[u8]) -> Vec<u8> {
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
}

type EncodingNames = Vec<Option<Vec<u8>>>;

/// `parse_encoding`: a named encoding, or an array as `<count> array`
/// followed by `<code> /<name>` pairs up to `def` (the `dup` and `put`
/// between them are passed over), or as `[ /<name> ... ]` from code 0.
/// The names of an array, by code.
fn parse_encoding(
    s: &mut Scanner<'_>,
    encoding: &mut Encoding,
) -> Result<Option<EncodingNames>, &'static str> {
    s.skip_space();
    let Some(&first) = s.d.get(s.at) else {
        return Err("Invalid_File_Format");
    };
    if !first.is_ascii_digit() && first != b'[' {
        match s.token() {
            Some(b"StandardEncoding") => *encoding = Encoding::Standard,
            Some(b"ExpertEncoding") => *encoding = Encoding::Expert,
            Some(b"ISOLatin1Encoding") => *encoding = Encoding::IsoLatin1,
            _ => {}
        }
        return Ok(None);
    }
    // T1_ToInt: the digits a token starts with.
    let int = |t: &[u8]| {
        t.iter()
            .take_while(|b| b.is_ascii_digit())
            .fold(0i64, |a, &b| {
                a.saturating_mul(10).saturating_add(i64::from(b - b'0'))
            })
    };
    let immediates = first == b'[';
    let count = if immediates {
        s.at += 1;
        256
    } else {
        int(s.token().unwrap_or_default())
    };
    let size = count.clamp(0, 256) as usize;
    let mut names = vec![None; size];
    let mut n = 0i64;
    loop {
        s.skip_space();
        let rest = s.d.get(s.at..).unwrap_or_default();
        if rest.is_empty() {
            return Ok(None);
        }
        if rest.len() > 3 && rest.starts_with(b"def") && is_delim(rest[3]) {
            break;
        }
        if rest[0] == b']' {
            s.at += 1;
            break;
        }
        if !rest[0].is_ascii_digit() && !immediates {
            s.token();
            continue;
        }
        let code = if immediates {
            n
        } else {
            int(s.token().unwrap_or_default())
        };
        s.skip_space();
        let rest = s.d.get(s.at..).unwrap_or_default();
        if rest.len() > 2 && rest[0] == b'/' && n < count {
            let name = s.token().unwrap_or_default();
            if s.at >= s.d.len() {
                return Ok(None);
            }
            if n < size as i64 {
                // T1_Add_Table refuses a code past the array.
                let slot = usize::try_from(code)
                    .ok()
                    .and_then(|c| names.get_mut(c))
                    .ok_or("Invalid_Argument")?;
                *slot = Some(name[1..].to_vec());
            }
            n += 1;
        } else if immediates {
            return Err("Unknown_File_Format");
        }
    }
    *encoding = Encoding::Array {
        indices: Vec::new(),
        count: 0,
    };
    Ok(Some(names))
}

impl Type1 {
    /// `T1_New_Parser`'s check of the header: after a PFB's first
    /// segment header if it is a text one.
    pub fn recognize(data: &[u8]) -> bool {
        let d = match pfb_segment(data, 0) {
            Some((1, _)) => &data[6..],
            _ => data,
        };
        d.starts_with(b"%!PS-AdobeFont") || d.starts_with(b"%!FontType")
    }

    /// `T1_Open_Face`: the cleartext and the decrypted private part, then
    /// what MuPDF reads of them.
    pub fn open(data: Arc<[u8]>) -> Result<Type1, &'static str> {
        let d: &[u8] = &data;
        if !Self::recognize(d) {
            return Err("Unknown_File_Format");
        }
        let (clear, binary): (&[u8], Vec<u8>) = if let Some((1, len)) = pfb_segment(d, 0) {
            // A PFB: the first segment is the cleartext, the 0x8002
            // segments after it the private part, as they are.
            let clear = d.get(6..6 + len).ok_or("Invalid_File_Format")?;
            let mut at = 6 + len;
            let mut binary = Vec::new();
            while let Some((2, len)) = pfb_segment(d, at) {
                binary.extend(d.get(at + 6..at + 6 + len).ok_or("Invalid_File_Format")?);
                at += 6 + len;
            }
            if binary.is_empty() {
                return Err("Invalid_File_Format");
            }
            let clear = &clear[..find_eexec(clear).unwrap_or(clear.len())];
            (clear, binary)
        } else {
            let eexec = find_eexec(d).ok_or("Invalid_File_Format")?;
            let mut s = Scanner { d, at: eexec };
            s.token();
            let mut cur = s.at;
            // Blanks before the ciphertext; a carriage return only where
            // it ends lines.
            let lf = d[cur..].iter().position(|&b| b == b'\n');
            let cr = d[cur..].iter().position(|&b| b == b'\r');
            let test_cr = match (lf, cr) {
                (Some(lf), Some(cr)) => lf > cr,
                _ => true,
            };
            while d
                .get(cur)
                .is_some_and(|&b| matches!(b, b' ' | b'\t' | b'\n') || (test_cr && b == b'\r'))
            {
                cur += 1;
            }
            let enc = d
                .get(cur..)
                .filter(|e| !e.is_empty())
                .ok_or("Invalid_File_Format")?;
            // Hex when the first four bytes are hex digits.
            let hex = enc.len() >= 4 && enc[..4].iter().all(u8::is_ascii_hexdigit);
            let binary = if hex { from_hex(enc) } else { enc.to_vec() };
            (&d[..eexec], binary)
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
                    let m = array_numbers(&mut s);
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
                    let b = array_numbers(&mut s);
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
                    if let Some(names) = parse_encoding(&mut s, &mut font.encoding)? {
                        encoding_names = names;
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
    /// The unhinted outline of glyph `gid`, in font units.
    pub fn outline(&self, gid: u32) -> Result<outline::Outline, LoadError> {
        let cs = self.charstrings.get(gid as usize).ok_or(LoadError)?;
        outline::charstring_outline(self, cs)
    }

    pub fn outline_cbox(&self, gid: u32) -> Result<Option<CBox>, LoadError> {
        let cs = self.charstrings.get(gid as usize).ok_or(LoadError)?;
        outline::charstring_cbox(self, cs)
    }

    /// The width `hsbw` or `sbw` sets, rounded, as `T1_Get_Advances`
    /// returns it: 0 where the charstring fails before its width.
    pub fn advance(&self, gid: u32) -> Option<i64> {
        let cs = self.charstrings.get(gid as usize)?;
        // FIXED_TO_INT: FT_RoundFix, half away from zero.
        Some(self.width(cs).map_or(0, |w| {
            let r = (w.abs() + 0x8000) >> 16;
            if w < 0 { -r } else { r }
        }))
    }

    /// `t1_decoder_parse_metrics`: the width in 16.16, None where FreeType
    /// fails (an operator other than these first, a bad operand, a
    /// subroutine that ends without `return`).
    fn width(&self, cs: &[u8]) -> Option<i64> {
        const MAX_SUBRS_CALLS: usize = 16;
        const MAX_OPERANDS: usize = 256;
        let mut stack: Vec<i64> = Vec::new();
        let mut zones: Vec<(&[u8], usize)> = vec![(cs, 0)];
        let mut large_int = false;
        loop {
            let z = zones.len() - 1;
            let (code, mut ip) = zones[z];
            let b0 = *code.get(ip)?;
            ip += 1;
            // The operator, or None for a number.
            let mut op = None;
            let mut value = 0i64;
            match b0 {
                1 | 3..=9 | 14 | 15 | 21 | 22 | 30 | 31 => return None,
                10 | 11 | 13 => op = Some(u16::from(b0)),
                12 => {
                    let b1 = *code.get(ip)?;
                    ip += 1;
                    match b1 {
                        7 | 12 => op = Some(1200 + u16::from(b1)),
                        _ => return None,
                    }
                }
                255 => {
                    let b = code.get(ip..ip + 4)?;
                    ip += 4;
                    value = i64::from(i32::from_be_bytes([b[0], b[1], b[2], b[3]]));
                    // Beyond 32000 a number must be divided; it stays an
                    // integer until then.
                    if !(-32000..=32000).contains(&value) {
                        if large_int {
                            return None;
                        }
                        large_int = true;
                    } else if !large_int {
                        value <<= 16;
                    }
                }
                32..=254 => {
                    value = i64::from(b0) - 139;
                    if b0 >= 247 {
                        let b1 = i64::from(*code.get(ip)?);
                        ip += 1;
                        value = if b0 < 251 {
                            (i64::from(b0) - 247) * 256 + b1 + 108
                        } else {
                            -((i64::from(b0) - 251) * 256 + b1 + 108)
                        };
                    }
                    if !large_int {
                        value <<= 16;
                    }
                }
                _ => return None,
            }
            zones[z].1 = ip;
            if large_int && !matches!(op, None | Some(1212)) {
                return None;
            }
            let Some(op) = op else {
                if stack.len() >= MAX_OPERANDS {
                    return None;
                }
                stack.push(value);
                continue;
            };
            let n = match op {
                13 | 1212 => 2,
                1207 => 4,
                10 => 1,
                _ => 0,
            };
            let top = stack.split_off(stack.len().checked_sub(n)?);
            match op {
                // hsbw: sbx wx; sbw: sbx sby wx wy.
                13 => return Some(top[1]),
                1207 => return Some(top[2]),
                1212 => {
                    stack.push(outline::div_fix_int(top[0], top[1]));
                    large_int = false;
                }
                10 => {
                    // Fix2Int.
                    let idx = i64::from((top[0] >> 16) as i16);
                    let sub = self
                        .subrs
                        .get(usize::try_from(idx).ok()?)
                        .filter(|s| !s.is_empty())?;
                    if zones.len() > MAX_SUBRS_CALLS {
                        return None;
                    }
                    zones.push((sub, 0));
                }
                // return.
                _ => {
                    if zones.len() < 2 {
                        return None;
                    }
                    zones.pop();
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_and_braced_arrays_read_whole() {
        let mut s = Scanner {
            d: b" [0.00048828 0 0 0.00048828 0 0] readonly def {0 -10 500 700}",
            at: 0,
        };
        assert_eq!(
            array_numbers(&mut s),
            vec![0.00048828, 0.0, 0.0, 0.00048828, 0.0, 0.0]
        );
        assert_eq!(s.token(), Some(&b"readonly"[..]));
        s.token();
        assert_eq!(array_numbers(&mut s), vec![0.0, -10.0, 500.0, 700.0]);
    }

    /// eexec and charstring encryption, the inverse of `decrypt`.
    fn encrypt(plain: &[u8], key: u16) -> Vec<u8> {
        let mut r = key;
        plain
            .iter()
            .map(|&p| {
                let c = p ^ (r >> 8) as u8;
                r = (u16::from(c).wrapping_add(r))
                    .wrapping_mul(52845)
                    .wrapping_add(22719);
                c
            })
            .collect()
    }

    /// A Type 1 charstring number.
    fn n(v: i32) -> Vec<u8> {
        match v {
            -107..=107 => vec![(v + 139) as u8],
            108..=1131 => {
                let w = v - 108;
                vec![(w / 256 + 247) as u8, (w % 256) as u8]
            }
            -1131..=-108 => {
                let w = -v - 108;
                vec![(w / 256 + 251) as u8, (w % 256) as u8]
            }
            _ => {
                let mut out = vec![255];
                out.extend(v.to_be_bytes());
                out
            }
        }
    }

    /// Operators of a charstring: an operator below 32 is one byte, from
    /// 1200 the escape and `op - 1200`.
    fn cs(ops: &[(&[i32], u16)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (args, op) in ops {
            for &a in *args {
                out.extend(n(a));
            }
            if *op >= 1200 {
                out.extend([12, (*op - 1200) as u8]);
            } else {
                out.push(*op as u8);
            }
        }
        out
    }

    const HSBW: u16 = 13;
    const RMOVETO: u16 = 21;
    const RLINETO: u16 = 5;
    const CLOSEPATH: u16 = 9;
    const ENDCHAR: u16 = 14;
    const CALLSUBR: u16 = 10;
    const RETURN: u16 = 11;
    const SEAC: u16 = 1206;
    const SBW: u16 = 1207;
    const DIV: u16 = 1212;
    const CALLOTHERSUBR: u16 = 1216;
    const POP: u16 = 1217;
    const SETCURRENTPOINT: u16 = 1233;

    /// The parts of a test font.
    struct Font {
        clear: String,
        private: String,
        len_iv: i64,
        /// The four random bytes that start the private part.
        lead: [u8; 4],
        subrs: Vec<(usize, Vec<u8>)>,
        glyphs: Vec<(&'static str, Vec<u8>)>,
    }

    impl Default for Font {
        fn default() -> Font {
            Font {
                clear: "/FontMatrix [0.001 0 0 0.001 0 0] readonly def\n\
                        /Encoding StandardEncoding def\n"
                    .into(),
                private: String::new(),
                len_iv: 4,
                lead: [1, 2, 3, 4],
                subrs: Vec::new(),
                glyphs: vec![
                    (".notdef", cs(&[(&[0, 250], HSBW), (&[], ENDCHAR)])),
                    ("A", triangle(20, 600)),
                ],
            }
        }
    }

    /// A triangle from the side bearing, 600 by 700 units.
    fn triangle(lsb: i32, width: i32) -> Vec<u8> {
        cs(&[
            (&[lsb, width], HSBW),
            (&[0, 0], RMOVETO),
            (&[300, 700], RLINETO),
            (&[300, -700], RLINETO),
            (&[], CLOSEPATH),
            (&[], ENDCHAR),
        ])
    }

    impl Font {
        fn enc(&self, cs: &[u8]) -> Vec<u8> {
            if self.len_iv < 0 {
                return cs.to_vec();
            }
            let mut plain = vec![0u8; self.len_iv as usize];
            plain.extend(cs);
            encrypt(&plain, 4330)
        }

        fn cleartext(&self) -> Vec<u8> {
            format!(
                "%!PS-AdobeFont-1.0: Test 001\n/FontName /Test def\n{}\
                 currentdict end\ncurrentfile eexec\n",
                self.clear
            )
            .into_bytes()
        }

        /// The private part before encryption.
        fn private_plain(&self) -> Vec<u8> {
            let mut p = self.lead.to_vec();
            p.extend(b"dup /Private 8 dict dup begin\n");
            p.extend(format!("/lenIV {} def\n", self.len_iv).bytes());
            p.extend(self.private.bytes());
            if !self.subrs.is_empty() {
                let count = self.subrs.iter().map(|s| s.0 + 1).max().unwrap_or(0);
                p.extend(format!("/Subrs {count} array\n").bytes());
                for (i, s) in &self.subrs {
                    let e = self.enc(s);
                    p.extend(format!("dup {i} {} RD ", e.len()).bytes());
                    p.extend(e);
                    p.extend(b" NP\n");
                }
                p.extend(b"ND\n");
            }
            p.extend(
                format!(
                    "2 index /CharStrings {} dict dup begin\n",
                    self.glyphs.len()
                )
                .bytes(),
            );
            for (name, g) in &self.glyphs {
                let e = self.enc(g);
                p.extend(format!("/{name} {} RD ", e.len()).bytes());
                p.extend(e);
                p.extend(b" ND\n");
            }
            p.extend(
                &b"end\nend\nreadonly put\nnoaccess put\n\
                   dup /FontName get exch definefont pop\nmark currentfile closefile\n"[..],
            );
            p
        }

        fn pfa_binary(&self) -> Vec<u8> {
            let mut out = self.cleartext();
            out.extend(encrypt(&self.private_plain(), 55665));
            out
        }

        fn pfa_hex(&self) -> Vec<u8> {
            let mut out = self.cleartext();
            for (i, b) in encrypt(&self.private_plain(), 55665).iter().enumerate() {
                out.extend(format!("{b:02x}").bytes());
                if i % 32 == 31 {
                    out.push(b'\n');
                }
            }
            out.extend(b"\n0000000000000000\ncleartomark\n");
            out
        }

        fn pfb(&self) -> Vec<u8> {
            let mut out = segment(1, &self.cleartext());
            out.extend(segment(2, &encrypt(&self.private_plain(), 55665)));
            out.extend(segment(1, b"cleartomark\n"));
            out.extend([0x80, 3]);
            out
        }

        fn open(&self) -> Type1 {
            open(self.pfa_binary())
        }
    }

    fn segment(kind: u8, d: &[u8]) -> Vec<u8> {
        let mut s = vec![0x80, kind];
        s.extend((d.len() as u32).to_le_bytes());
        s.extend(d);
        s
    }

    fn open(d: Vec<u8>) -> Type1 {
        Type1::open(d.into()).unwrap_or_else(|e| panic!("{e}"))
    }

    fn points(t: &Type1, name: &[u8]) -> Vec<(i64, i64)> {
        let gid = t.name_index(name);
        assert_eq!(t.glyph_name(gid).as_deref(), Some(name));
        t.outline(gid).unwrap_or_else(|e| panic!("{e:?}")).points
    }

    #[test]
    fn pfa_hex_and_pfb_fonts_read_alike() {
        let f = Font::default();
        for data in [f.pfa_binary(), f.pfa_hex(), f.pfb()] {
            assert!(Type1::recognize(&data));
            let t = open(data);
            assert_eq!(t.names, [&b".notdef"[..], b"A"]);
            assert_eq!(t.encoding, Encoding::Standard);
            assert_eq!(t.units_per_em, 1000);
            assert_eq!(t.advance(1), Some(600));
            assert_eq!(points(&t, b"A"), [(20, 0), (320, 700), (620, 0)]);
        }
    }

    #[test]
    fn the_cleartext_sets_the_em_box_and_style() {
        let f = Font {
            clear: "/FontMatrix [0.0005 0 0 0.0005 0 0] def\n\
                    /FontInfo 3 dict dup begin /ItalicAngle -12.5 def /Weight (Bold) def\n\
                    /isFixedPitch true def end readonly def\n\
                    /FontBBox {-10.5 -200 600.2 800} readonly def\n\
                    /Encoding ExpertEncoding def\n"
                .into(),
            ..Font::default()
        };
        let t = f.open();
        assert_eq!(t.units_per_em, 2000);
        assert_eq!(t.bbox, [-11, -200, 601, 800]);
        assert!(t.bold && t.italic && t.fixed_pitch);
        assert_eq!(t.encoding, Encoding::Expert);

        // An angle under one degree is upright; Medium is not bold.
        let f = Font {
            clear: "/ItalicAngle -0.5 def /Weight (Medium) def /isFixedPitch false def\n\
                    /Encoding ISOLatin1Encoding def\n"
                .into(),
            ..Font::default()
        };
        let t = f.open();
        assert!(!t.bold && !t.italic && !t.fixed_pitch);
        assert_eq!(t.encoding, Encoding::IsoLatin1);
        assert_eq!(t.units_per_em, 1000);

        // A FontMatrix that does not scale, and no Encoding at all.
        let f = Font {
            clear: "/FontMatrix [0 0 0 0 0 0] def\n".into(),
            ..Font::default()
        };
        assert_eq!(
            Type1::open(f.pfa_binary().into()).err(),
            Some("Invalid_File_Format")
        );
        let f = Font {
            clear: String::new(),
            ..Font::default()
        };
        assert_eq!(f.open().encoding, Encoding::None);
    }

    #[test]
    fn an_encoding_array_maps_codes_to_glyphs_by_name() {
        let f = Font {
            clear: "/Encoding 256 array\n0 1 255 {1 index exch /.notdef put} for\n\
                    dup 65 /A put\ndup 66 /missing put\ndup 10 /.notdef put\n\
                    readonly def\n"
                .into(),
            ..Font::default()
        };
        let t = f.open();
        let Encoding::Array { indices, count } = &t.encoding else {
            panic!("{:?}", t.encoding);
        };
        assert_eq!(*count, 66);
        assert_eq!(indices.len(), 256);
        assert_eq!(indices[65], 1);
        assert_eq!(indices[66], 0);
        assert_eq!(indices[10], 0);
    }

    #[test]
    fn notdef_becomes_glyph_zero() {
        // A .notdef that is not first swaps with glyph 0.
        let f = Font {
            glyphs: vec![
                ("A", triangle(20, 600)),
                ("B", triangle(0, 700)),
                (".notdef", cs(&[(&[0, 250], HSBW), (&[], ENDCHAR)])),
            ],
            ..Font::default()
        };
        let t = f.open();
        assert_eq!(t.names, [&b".notdef"[..], b"B", b"A"]);
        assert_eq!(t.advance(0), Some(250));
        assert_eq!(t.advance(2), Some(600));
        assert_eq!(t.name_index(b"nothere"), 0);
        assert_eq!(t.glyph_name(9), None);

        // Without one, one is made at 0 and the first glyph moves last.
        let f = Font {
            glyphs: vec![("A", triangle(20, 600)), ("B", triangle(0, 700))],
            ..Font::default()
        };
        let t = f.open();
        assert_eq!(t.names, [&b".notdef"[..], b"B", b"A"]);
        assert_eq!(t.advance(0), Some(333));
        assert_eq!(t.outline(0).map(|o| o.points), Ok(Vec::new()));
        assert_eq!(t.advance(2), Some(600));
    }

    const HLINETO: u16 = 6;
    const VLINETO: u16 = 7;
    const RRCURVETO: u16 = 8;
    const VHCURVETO: u16 = 30;
    const HVCURVETO: u16 = 31;
    const HMOVETO: u16 = 22;
    const VMOVETO: u16 = 4;
    const HSTEM: u16 = 1;
    const VSTEM: u16 = 3;
    const DOTSECTION: u16 = 1200;
    const VSTEM3: u16 = 1201;
    const HSTEM3: u16 = 1202;

    /// A font whose glyphs use every Type 1 operator: the standard flex
    /// and hint replacement subroutines 0 to 3, and one of its own.
    fn rich() -> Font {
        let flex = {
            let mut c = cs(&[(&[0, 500], HSBW), (&[0, 100], RMOVETO), (&[1], CALLSUBR)]);
            for (dx, dy) in [
                (100, 0),
                (0, 0),
                (50, 30),
                (50, 0),
                (50, 0),
                (50, -30),
                (0, 0),
            ] {
                c.extend(cs(&[(&[dx, dy], RMOVETO), (&[2], CALLSUBR)]));
            }
            c.extend(cs(&[
                (&[50, 300, 100, 0], CALLSUBR),
                (&[0, -100], RLINETO),
                (&[], CLOSEPATH),
                (&[], ENDCHAR),
            ]));
            c
        };
        let square = |x, y| {
            cs(&[
                (&[x, y], RMOVETO),
                (&[100, 0], RLINETO),
                (&[0, 100], RLINETO),
                (&[-100, 0], RLINETO),
                (&[], CLOSEPATH),
            ])
        };
        let mut hints = cs(&[
            (&[0, 500], HSBW),
            (&[0, 50], HSTEM),
            (&[100, 50], VSTEM),
            (&[], DOTSECTION),
            (&[10, 10], RMOVETO),
            (&[100, 0], RLINETO),
            (&[4, 1, 3], CALLOTHERSUBR),
            (&[], POP),
            (&[], CALLSUBR),
            (&[0, 100], RLINETO),
            (&[], CLOSEPATH),
            (&[], DOTSECTION),
            (&[0, 10, 100, 10, 200, 10], HSTEM3),
            (&[0, 10, 100, 10, 200, 10], VSTEM3),
        ]);
        hints.extend(square(200, -100));
        hints.extend(cs(&[(&[], ENDCHAR)]));
        Font {
            subrs: vec![
                (
                    0,
                    cs(&[
                        (&[3, 0], CALLOTHERSUBR),
                        (&[], POP),
                        (&[], POP),
                        (&[], SETCURRENTPOINT),
                        (&[], RETURN),
                    ]),
                ),
                (1, cs(&[(&[0, 1], CALLOTHERSUBR), (&[], RETURN)])),
                (2, cs(&[(&[0, 2], CALLOTHERSUBR), (&[], RETURN)])),
                (3, cs(&[(&[], RETURN)])),
                (4, cs(&[(&[0, 60], HSTEM), (&[], RETURN)])),
                (
                    5,
                    cs(&[
                        (&[10, 10], RMOVETO),
                        (&[100, 0], RLINETO),
                        (&[0, 100], RLINETO),
                        (&[], RETURN),
                    ]),
                ),
                (6, cs(&[(&[0, 640], HSBW), (&[], RETURN)])),
            ],
            glyphs: vec![
                (".notdef", cs(&[(&[0, 250], HSBW), (&[], ENDCHAR)])),
                ("A", triangle(20, 600)),
                (
                    "acute",
                    cs(&[
                        (&[30, 300], HSBW),
                        (&[100, 600], RMOVETO),
                        (&[50, 100], RLINETO),
                        (&[50, -100], RLINETO),
                        (&[], CLOSEPATH),
                        (&[], ENDCHAR),
                    ]),
                ),
                (
                    "Aacute",
                    cs(&[(&[20, 600], HSBW), (&[30, 200, 50, 65, 194], SEAC)]),
                ),
                (
                    "curves",
                    cs(&[
                        (&[0, 500], HSBW),
                        (&[100, 100], RMOVETO),
                        (&[200], HLINETO),
                        (&[200], VLINETO),
                        (&[50, 50, 50, 50], VHCURVETO),
                        (&[50, 50, -50, -50], HVCURVETO),
                        (&[0, -100, -100, -100, -100, 0], RRCURVETO),
                        (&[], CLOSEPATH),
                        (&[300], HMOVETO),
                        (&[100], VMOVETO),
                        (&[50, 0], RLINETO),
                        (&[0, 50], RLINETO),
                        (&[], CLOSEPATH),
                        (&[], ENDCHAR),
                    ]),
                ),
                (
                    "sbw",
                    cs(&[
                        (&[30, 40, 700, 0], SBW),
                        (&[0, 0], RMOVETO),
                        (&[100, 0], RLINETO),
                        (&[0, 100], RLINETO),
                        (&[], CLOSEPATH),
                        (&[], ENDCHAR),
                    ]),
                ),
                (
                    "div",
                    cs(&[
                        (&[0, 1000, 4], DIV),
                        (&[], HSBW),
                        (&[0, 0], RMOVETO),
                        (&[100_000, 1000], DIV),
                        (&[0], RLINETO),
                        (&[1, 2], DIV),
                        (&[100], RLINETO),
                        (&[], CLOSEPATH),
                        (&[], ENDCHAR),
                    ]),
                ),
                (
                    "subr",
                    cs(&[
                        (&[6], CALLSUBR),
                        (&[5], CALLSUBR),
                        (&[], CLOSEPATH),
                        (&[], ENDCHAR),
                    ]),
                ),
                ("flex", flex),
                ("hints", hints),
                (
                    "other",
                    cs(&[
                        (&[0, 500], HSBW),
                        (&[0, 0], RMOVETO),
                        (&[30, 40, 2, 20], CALLOTHERSUBR),
                        (&[0], POP),
                        (&[], RLINETO),
                        (&[7, 80, 2, 30], CALLOTHERSUBR),
                        (&[], POP),
                        (&[], POP),
                        (&[], RLINETO),
                        (&[], CLOSEPATH),
                        (&[], ENDCHAR),
                    ]),
                ),
            ],
            ..Font::default()
        }
    }

    // The outlines below are MuPDF's (mutool draw -F svg of a PDF that
    // shows each glyph of `rich()`), in font units.

    #[test]
    fn lines_curves_and_moves_draw_as_freetype_draws_them() {
        let t = rich().open();
        let gid = t.name_index(b"curves");
        let o = t.outline(gid).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            o.points,
            [
                (100, 100),
                (300, 100),
                (300, 300),
                (300, 350),
                (350, 400),
                (400, 400),
                (450, 400),
                (500, 350),
                (500, 300),
                (500, 200),
                (400, 100),
                (300, 100),
                (600, 200),
                (650, 200),
                (650, 250),
            ]
        );
        assert_eq!(o.tags[..6], [1, 1, 1, 2, 2, 1]);
        assert_eq!(o.contours, [11, 14]);
        assert_eq!(t.advance(gid), Some(500));
        let cbox = t.outline_cbox(gid).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            cbox,
            Some(CBox {
                x0: 100.0,
                y0: 100.0,
                x1: 650.0,
                y1: 400.0
            })
        );
        assert_eq!(t.outline(99), Err(LoadError));
        assert_eq!(t.outline_cbox(99), Err(LoadError));
        assert_eq!(t.advance(99), None);
    }

    #[test]
    fn sbw_div_and_subroutines_set_the_origin_and_width() {
        let t = rich().open();
        assert_eq!(points(&t, b"sbw"), [(30, 40), (130, 40), (130, 140)]);
        assert_eq!(t.advance(t.name_index(b"sbw")), Some(700));
        // 100000 1000 div is 100; 1 2 div moves half a unit, which the
        // unscaled outline drops.
        assert_eq!(points(&t, b"div"), [(0, 0), (100, 0), (100, 100)]);
        assert_eq!(t.advance(t.name_index(b"div")), Some(250));
        // The width comes from hsbw in subroutine 6.
        assert_eq!(points(&t, b"subr"), [(10, 10), (110, 10), (110, 110)]);
        assert_eq!(t.advance(t.name_index(b"subr")), Some(640));
    }

    #[test]
    fn flex_hint_replacement_and_othersubrs() {
        let t = rich().open();
        assert_eq!(
            points(&t, b"flex"),
            [
                (0, 100),
                (100, 100),
                (150, 130),
                (200, 130),
                (250, 130),
                (300, 100),
                (300, 100),
                (300, 0),
            ]
        );
        assert_eq!(
            points(&t, b"hints"),
            [
                (10, 10),
                (110, 10),
                (110, 110),
                (310, 10),
                (410, 10),
                (410, 110),
                (310, 110),
            ]
        );
        // OtherSubr 20 adds; an unknown one hands its arguments back.
        assert_eq!(points(&t, b"other"), [(0, 0), (70, 0), (150, 7)]);
    }

    #[test]
    fn seac_places_the_accent_over_the_base() {
        let t = rich().open();
        assert_eq!(
            points(&t, b"Aacute"),
            [
                (20, 0),
                (320, 700),
                (620, 0),
                (320, 650),
                (370, 750),
                (420, 650),
            ]
        );
        assert_eq!(t.advance(t.name_index(b"Aacute")), Some(600));
    }

    /// The outline of a one-glyph font with this charstring.
    fn one(glyph: Vec<u8>, subrs: Vec<(usize, Vec<u8>)>) -> Result<outline::Outline, LoadError> {
        let f = Font {
            subrs,
            glyphs: vec![
                (".notdef", cs(&[(&[0, 250], HSBW), (&[], ENDCHAR)])),
                ("A", triangle(0, 600)),
                ("g", glyph),
            ],
            ..Font::default()
        };
        f.open().outline(2)
    }

    #[test]
    fn broken_charstrings_fail_to_load() {
        let start = |ops: &[(&[i32], u16)]| {
            let mut c = cs(&[(&[0, 500], HSBW), (&[0, 0], RMOVETO)]);
            c.extend(cs(ops));
            c
        };
        let fails = |ops: &[(&[i32], u16)]| {
            assert_eq!(one(start(ops), Vec::new()), Err(LoadError), "{ops:?}");
        };
        // A missing subroutine, a return with nothing to return to, an
        // operator short of operands, results nobody left.
        fails(&[(&[3], CALLSUBR)]);
        fails(&[(&[], RETURN)]);
        fails(&[(&[], RLINETO), (&[], HSBW)]);
        fails(&[(&[], POP)]);
        fails(&[(&[1, 2, 3], HSTEM3)]);
        // callothersubr with more arguments than the stack has, flex parts
        // outside a flex, a reserved OtherSubr, a negative one.
        fails(&[(&[5, 1], CALLOTHERSUBR)]);
        fails(&[(&[0, 2], CALLOTHERSUBR)]);
        fails(&[(&[1, 2, 3, 0], CALLOTHERSUBR)]);
        // Known OtherSubrs with the wrong argument counts.
        fails(&[(&[1, 2, 3, 3, 0], CALLOTHERSUBR)]);
        for subr in [1, 20, 27, 28] {
            fails(&[(&[5, 1, subr], CALLOTHERSUBR)]);
        }
        fails(&[(&[0, 3], CALLOTHERSUBR)]);
        fails(&[(&[0, 14], CALLOTHERSUBR)]);
        fails(&[(&[0, -1], CALLOTHERSUBR)]);
        fails(&[(&[1, 3], CALLOTHERSUBR)]);
        fails(&[(&[5, 0, 2, 23], CALLOTHERSUBR)]);
        fails(&[(&[1], CALLOTHERSUBR)]);
        // A seac whose parts are not in the font.
        fails(&[(&[0, 0, 0, 66, 194], SEAC)]);
        // Too many operands.
        fails(&[(&[1; 49], RLINETO)]);
        // Subroutines nested too deep.
        let me = cs(&[(&[0], CALLSUBR), (&[], RETURN)]);
        assert_eq!(
            one(start(&[(&[0], CALLSUBR)]), vec![(0, me)]),
            Err(LoadError)
        );
        // A seac inside a seac component.
        let f = Font {
            glyphs: vec![
                (".notdef", cs(&[(&[0, 250], HSBW), (&[], ENDCHAR)])),
                ("A", cs(&[(&[0, 600], HSBW), (&[0, 0, 0, 65, 65], SEAC)])),
                ("B", cs(&[(&[0, 600], HSBW), (&[0, 0, 0, 65, 65], SEAC)])),
            ],
            ..Font::default()
        };
        assert_eq!(f.open().outline(2), Err(LoadError));
    }

    #[test]
    fn othersubrs_with_their_operands() {
        let start = |ops: &[(&[i32], u16)]| {
            let mut c = cs(&[(&[0, 500], HSBW), (&[0, 0], RMOVETO)]);
            c.extend(cs(ops));
            c.extend(cs(&[(&[], CLOSEPATH), (&[], ENDCHAR)]));
            c
        };
        let ends = |ops: &[(&[i32], u16)]| {
            let o = one(start(ops), Vec::new()).unwrap_or_else(|e| panic!("{e:?} {ops:?}"));
            o.points.last().copied()
        };
        // sub, mul, div, ifelse and random leave one result for pop.
        assert_eq!(
            ends(&[
                (&[50, 20, 2, 21], CALLOTHERSUBR),
                (&[], POP),
                (&[4, 5, 2, 22], CALLOTHERSUBR),
                (&[], POP),
                (&[], RLINETO),
            ]),
            Some((30, 20))
        );
        assert_eq!(
            ends(&[
                (&[300, 3, 2, 23], CALLOTHERSUBR),
                (&[], POP),
                (&[10, 20, 1, 2, 4, 27], CALLOTHERSUBR),
                (&[], POP),
                (&[], RLINETO),
            ]),
            Some((100, 10))
        );
        assert_eq!(
            ends(&[
                (&[0, 28], CALLOTHERSUBR),
                (&[], POP),
                (&[], 1218),
                (&[0, 40], RLINETO),
            ]),
            Some((0, 40))
        );
        // 12 and 13 clear the stack.
        assert_eq!(
            ends(&[(&[9, 9, 0, 12], CALLOTHERSUBR), (&[40, 40], RLINETO)]),
            Some((40, 40))
        );
        // Big numbers only count once divided.
        assert_eq!(
            ends(&[(&[64_000, 1000], DIV), (&[0], RLINETO)]),
            Some((64, 0))
        );
        // An escape that clears, then dotsection with results pending.
        assert_eq!(
            ends(&[
                (&[7, 8, 2, 40], CALLOTHERSUBR),
                (&[], DOTSECTION),
                (&[10, 10], RLINETO),
            ]),
            Some((10, 10))
        );
        // Results left when a path operator comes are dropped.
        assert_eq!(
            ends(&[
                (&[7, 8, 2, 40], CALLOTHERSUBR),
                (&[], POP),
                (&[10], RLINETO),
            ]),
            Some((8, 10))
        );
    }

    #[test]
    fn an_unencrypted_font_reads_its_charstrings_as_they_are() {
        let f = Font {
            len_iv: -1,
            ..rich()
        };
        let t = f.open();
        assert_eq!(points(&t, b"subr"), [(10, 10), (110, 10), (110, 110)]);
        assert_eq!(t.advance(t.name_index(b"subr")), Some(640));
    }

    /// Glyphs whose widths or outlines test `t1_decoder_parse_metrics` and
    /// the interpreter: subroutines 3 to 19 call the next, 20 sets a
    /// width, 22 to 40 call the next and 41 calls 21, which draws.
    fn metrics_font() -> Font {
        let none = cs(&[(&[], RETURN)]);
        let deep = |i: usize| (i, cs(&[(&[i as i32 + 1], CALLSUBR), (&[], RETURN)]));
        let mut subrs = vec![(0, none), (1, Vec::new()), (2, cs(&[(&[1, 1], RLINETO)]))];
        subrs.extend((3..20).map(deep));
        subrs.push((20, cs(&[(&[0, 300], HSBW)])));
        subrs.push((
            21,
            cs(&[(&[100, 100], RLINETO), (&[0, 100], RLINETO), (&[], RETURN)]),
        ));
        subrs.extend((22..41).map(deep));
        subrs.push((41, cs(&[(&[21], CALLSUBR), (&[], RETURN)])));
        Font {
            subrs,
            glyphs: vec![
                (".notdef", cs(&[(&[0, 250], HSBW), (&[], ENDCHAR)])),
                ("a", cs(&[(&[5, 0, 500], HSBW)])),
                ("b", cs(&[(&[0, -5, 2], DIV), (&[], HSBW)])),
                ("c", cs(&[(&[1], CALLSUBR), (&[0, 320], HSBW)])),
                ("d", cs(&[(&[2], CALLSUBR), (&[0, 320], HSBW)])),
                ("e", cs(&[(&[], DOTSECTION), (&[0, 500], HSBW)])),
                ("f", cs(&[(&[0, 200_000, 2], DIV), (&[], HSBW)])),
                ("g", cs(&[(&[0, 200_000, 200_000, 2], DIV), (&[], HSBW)])),
                ("h", cs(&[(&[0, 100_000], HSBW)])),
                ("i", vec![139, 12]),
                ("j", vec![139, 0, 139]),
                ("k", cs(&[(&[], RETURN), (&[0, 330], HSBW)])),
                ("l", cs(&[(&[0, 0, 700, 0], SBW)])),
                ("m", cs(&[(&[3], CALLSUBR), (&[0, 340], HSBW)])),
                ("n", cs(&[(&[0, 320], HSBW)])),
                ("o", cs(&[(&[0], CALLSUBR), (&[0, 350], HSBW)])),
                ("p", cs(&[(&[0, 5, 2], DIV), (&[], HSBW)])),
                ("q", cs(&[(&[13], CALLSUBR), (&[0, 340], HSBW)])),
                ("r", cs(&[(&[12], CALLSUBR), (&[0, 340], HSBW)])),
                ("s", cs(&[(&[0, 1, 3], DIV), (&[], HSBW)])),
                (
                    "n9",
                    cs(&[
                        (&[0, 500], HSBW),
                        (&[0, 0], RMOVETO),
                        (&[34], CALLSUBR),
                        (&[], CLOSEPATH),
                        (&[], ENDCHAR),
                    ]),
                ),
                (
                    "n10",
                    cs(&[
                        (&[0, 500], HSBW),
                        (&[0, 0], RMOVETO),
                        (&[33], CALLSUBR),
                        (&[], CLOSEPATH),
                        (&[], ENDCHAR),
                    ]),
                ),
                (
                    "n17",
                    cs(&[
                        (&[0, 500], HSBW),
                        (&[0, 0], RMOVETO),
                        (&[26], CALLSUBR),
                        (&[], CLOSEPATH),
                        (&[], ENDCHAR),
                    ]),
                ),
                (
                    "n18",
                    cs(&[
                        (&[0, 500], HSBW),
                        (&[0, 0], RMOVETO),
                        (&[25], CALLSUBR),
                        (&[], CLOSEPATH),
                        (&[], ENDCHAR),
                    ]),
                ),
                (
                    "n16",
                    cs(&[
                        (&[0, 500], HSBW),
                        (&[0, 0], RMOVETO),
                        (&[27], CALLSUBR),
                        (&[], CLOSEPATH),
                        (&[], ENDCHAR),
                    ]),
                ),
                ("w15", cs(&[(&[6], CALLSUBR), (&[0, 340], HSBW)])),
                ("w16", cs(&[(&[5], CALLSUBR), (&[0, 340], HSBW)])),
                ("w17", cs(&[(&[4], CALLSUBR), (&[0, 340], HSBW)])),
            ],
            ..Font::default()
        }
    }

    #[test]
    fn broken_fonts_are_refused() {
        let f = Font::default();
        assert!(!Type1::recognize(b"%!PS-Adobe-3.0"));
        assert_eq!(
            Type1::open(b"%!PS-Adobe-3.0 Resource".to_vec().into()).err(),
            Some("Unknown_File_Format")
        );
        // No eexec, or nothing after it.
        let mut clear = f.cleartext();
        clear.truncate(clear.len() - 7);
        assert_eq!(Type1::open(clear.into()).err(), Some("Invalid_File_Format"));
        assert_eq!(
            Type1::open(f.cleartext().into()).err(),
            Some("Invalid_File_Format")
        );
        // A private part cut short loses the glyphs it no longer holds.
        let whole = f.pfa_binary();
        let t = open(whole[..whole.len() - 120].to_vec());
        assert_eq!(t.names, [&b".notdef"[..]]);
        assert_eq!(t.outline(1), Err(LoadError));
        // PFB segments whose length runs past the end.
        let mut pfb = segment(1, &f.cleartext());
        let private = encrypt(&f.private_plain(), 55665);
        pfb.extend([0x80, 2]);
        pfb.extend(((private.len() + 50) as u32).to_le_bytes());
        pfb.extend(&private);
        assert!(Type1::open(pfb.into()).is_err());
        // An empty Subrs array and charstrings without a binary part.
        let f = Font {
            private: "/Subrs 0 array\nND\n".into(),
            ..Font::default()
        };
        assert_eq!(f.open().names.len(), 2);
        let mut p = b"\x01\x02\x03\x04/CharStrings 1 dict dup begin\n/A 99 RD ".to_vec();
        p.extend(b"end\n");
        let mut d = f.cleartext();
        d.extend(encrypt(&p, 55665));
        assert_eq!(Type1::open(d.into()).err(), Some("Invalid_File_Format"));
    }

    /// A font whose private part encrypts to bytes starting with `lead`.
    fn leading(lead: &[u8; 4]) -> Font {
        let mut p = [0; 4];
        p.copy_from_slice(&decrypt(lead, 55665));
        Font {
            lead: p,
            ..Font::default()
        }
    }

    #[test]
    fn pfb_private_segments_are_binary_from_their_first_byte() {
        // T1_Get_Private_Dict takes a PFB's 0x8002 segments as they are:
        // no whitespace skipped, no test for hexadecimal.
        for lead in [b" \t\r\n", b"cafe"] {
            let t = open(leading(lead).pfb());
            assert_eq!(t.names, [&b".notdef"[..], b"A"], "{lead:?}");
        }
        // Binary segments in a row join.
        let f = Font::default();
        let private = encrypt(&f.private_plain(), 55665);
        let mut pfb = segment(1, &f.cleartext());
        pfb.extend(segment(2, &private[..100]));
        pfb.extend(segment(2, &private[100..]));
        pfb.extend([0x80, 3]);
        assert_eq!(open(pfb).names.len(), 2);
        // A PFB with no binary segment, or one that starts with another.
        let mut pfb = segment(1, &f.cleartext());
        pfb.extend([0x80, 3]);
        assert_eq!(Type1::open(pfb.into()).err(), Some("Invalid_File_Format"));
        let mut pfb = segment(2, &f.cleartext());
        pfb.extend([0x80, 3]);
        assert!(!Type1::recognize(&pfb));
    }

    #[test]
    fn a_pfa_private_part_starts_after_eexec_and_its_blanks() {
        // Spaces, tabs and line feeds go; a form feed or a NUL is
        // ciphertext; so is a carriage return in a file of line feeds.
        for (lead, ok) in [
            (b"\x0c\x01\x02\x03", true),
            (b"\x00\x01\x02\x03", true),
            (b"\r\x01\x02\x03", true),
            (b" \t\x01\x02", false),
        ] {
            // Ciphertext that starts with a blank loses it, and with it the font.
            let got = Type1::open(leading(lead).pfa_binary().into()).map(|t| t.names.len());
            assert_eq!(
                got,
                if ok {
                    Ok(2)
                } else {
                    Err("Invalid_File_Format")
                },
                "{lead:?}"
            );
        }
        // An `eexec` in a comment or a string is not the real one.
        let f = Font {
            clear: "% this font ends in eexec and binary\n/Notice (eexec ahead) def\n\
                    /Encoding StandardEncoding def\n"
                .into(),
            ..Font::default()
        };
        assert_eq!(f.open().names.len(), 2);
    }

    #[test]
    fn an_encoding_array_given_inline_or_by_code() {
        // `/Encoding [ ... ]` lists names from code 0.
        let mut list = vec!["/.notdef"; 65];
        list.push("/A");
        let f = Font {
            clear: format!("/Encoding [{}] def\n", list.join(" ")),
            ..Font::default()
        };
        let t = f.open();
        assert_eq!(
            t.encoding,
            Encoding::Array {
                indices: [vec![0; 65], vec![1], vec![0; 190]].concat(),
                count: 66
            }
        );
        // An entry in a list that is not a name: not a Type 1 font.
        let f = Font {
            clear: "/Encoding [/A 5 /A] def\n".into(),
            ..Font::default()
        };
        assert_eq!(
            Type1::open(f.pfa_binary().into()).err(),
            Some("Unknown_File_Format")
        );
        // A code and a name need no `dup`; a code past the array fails.
        let f = Font {
            clear: "/Encoding 70 array 65 /A put readonly def\n".into(),
            ..Font::default()
        };
        let Encoding::Array { indices, count } = f.open().encoding else {
            panic!();
        };
        assert_eq!((indices.len(), indices[65], count), (70, 1, 66));
        let f = Font {
            clear: "/Encoding 20 array dup 65 /A put readonly def\n".into(),
            ..Font::default()
        };
        assert_eq!(
            Type1::open(f.pfa_binary().into()).err(),
            Some("Invalid_Argument")
        );
    }

    /// Widths as MuPDF has them (Font.advanceGlyph; 0 where FreeType
    /// fails), and which outlines fail to load (mutool draw -F svg).
    #[test]
    fn widths_and_nesting_as_mupdf_reads_them() {
        let t = metrics_font().open();
        let widths: Vec<i64> = (0..t.names.len() as u32)
            .map(|g| t.advance(g).unwrap_or(-1))
            .collect();
        #[rustfmt::skip]
        assert_eq!(
            widths,
            [
                250, 500, -3, 0, 0, 0, 100_000, 0, 0, 0, 0, 0, 700, 0, 320, 350, 3, 300, 300,
                0, 500, 500, 500, 500, 500, 300, 300, 0,
            ]
        );
        let failed: Vec<u32> = (0..t.names.len() as u32)
            .filter(|&g| t.outline(g).is_err())
            .collect();
        assert_eq!(failed, [11, 13, 22, 23, 27]);
        // Sixteen levels of subroutines draw; seventeen do not.
        assert_eq!(points(&t, b"n16"), [(0, 0), (100, 100), (100, 200)]);
    }
}
