// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-font.c, source/pdf/pdf-type3.c,
// source/pdf/pdf-unicode.c, source/pdf/pdf-metrics.c and source/fitz/font.c
// (fz_new_font_from_buffer), Copyright (C) 2004-2026 Artifex Software, Inc.

//! PDF fonts as `pdf_load_font` loads them: the font program (embedded,
//! builtin or a substitute), the encoding from codes to CIDs and glyphs,
//! the Unicode of each code, and the widths.

use std::sync::Arc;

use crate::cmap::{self, Cmap};
use crate::diag::report;
use crate::document::Document;
use crate::font::{self, Encoding as FtEncoding, Face, Format};
use crate::geometry::{Matrix, Rect};
use crate::glyphs;
use crate::object::Object;
use crate::tables;

pub const FD_FIXED_PITCH: i32 = 1;
pub const FD_SERIF: i32 = 1 << 1;
pub const FD_SYMBOLIC: i32 = 1 << 2;
pub const FD_NONSYMBOLIC: i32 = 1 << 5;
pub const FD_ITALIC: i32 = 1 << 6;
pub const FD_FORCE_BOLD: i32 = 1 << 18;

/// `FZ_MAX_TRUSTWORTHY_ASCENT` and `_DESCENT`, in ems.
const MAX_TRUSTWORTHY_ASCENT: i32 = 8;
const MAX_TRUSTWORTHY_DESCENT: i32 = -3;

/// Most codes a ToUnicode remap walks. MuPDF has no bound; a CMap range
/// over all four-byte codes would keep it busy for minutes.
const MAX_REMAP: u64 = 1 << 24;

/// `fz_font`: what devices see of a font.
#[derive(Debug, Clone)]
pub struct Font {
    /// As MuPDF keeps it, cut to 31 bytes.
    pub name: Vec<u8>,
    pub face: Option<Face>,
    pub ft_substitute: bool,
    pub ft_stretch: bool,
    pub invalid_bbox: bool,
    pub bbox: Rect,
    pub ascender: f32,
    pub descender: f32,
    pub t3: Option<Type3>,
}

/// A Type 3 font's glyph procedures, widths and matrix, and what the
/// interpreter found preparing each glyph: the device flags it left and
/// its box.
#[derive(Debug, Clone)]
pub struct Type3 {
    pub matrix: Matrix,
    pub procs: Vec<Option<Arc<[u8]>>>,
    pub widths: Vec<f32>,
    pub resources: Option<Object>,
    pub flags: Vec<u32>,
    pub bboxes: Vec<Rect>,
}

impl Font {
    /// `fz_new_font`.
    pub fn new(name: &[u8]) -> Font {
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        Font {
            name: name[..end.min(31)].to_vec(),
            face: None,
            ft_substitute: false,
            ft_stretch: false,
            invalid_bbox: false,
            bbox: Rect::UNIT,
            ascender: 0.0,
            descender: 0.0,
            t3: None,
        }
    }

    /// `fz_set_font_bbox`.
    fn set_bbox(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        // NaN takes the valid branch, as in C.
        if x0 >= x1 || y0 >= y1 {
            self.bbox = if self.t3.is_some() {
                Rect::EMPTY
            } else {
                Rect::UNIT
            };
            self.invalid_bbox = true;
        } else {
            self.bbox = Rect::new(x0, y0, x1, y1);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hmtx {
    pub lo: i32,
    pub hi: i32,
    pub w: i32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vmtx {
    pub lo: i32,
    pub hi: i32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
}

/// `pdf_font_desc`.
#[derive(Debug)]
pub struct FontDesc {
    pub font: Arc<Font>,
    pub flags: i32,
    pub ascent: f32,
    pub descent: f32,
    pub missing_width: f32,
    pub encoding: Arc<Cmap>,
    to_ttf_cmap: Option<Arc<Cmap>>,
    cid_to_gid: Option<Vec<u16>>,
    pub to_unicode: Option<Arc<Cmap>>,
    /// Kept as MuPDF's `unsigned short`.
    pub cid_to_ucs: Option<Vec<u16>>,
    pub wmode: i32,
    hmtx: Vec<Hmtx>,
    pub dhmtx: Hmtx,
    vmtx: Vec<Vmtx>,
    pub dvmtx: Vmtx,
    pub is_embedded: bool,
}

impl FontDesc {
    /// `pdf_new_font_desc`; the font is a placeholder until the loader
    /// sets it.
    fn new() -> FontDesc {
        FontDesc {
            font: Arc::new(Font::new(b"")),
            flags: 0,
            ascent: 800.0,
            descent: -200.0,
            missing_width: 0.0,
            encoding: Arc::new(Cmap::identity(0, 1)),
            to_ttf_cmap: None,
            cid_to_gid: None,
            to_unicode: None,
            cid_to_ucs: None,
            wmode: 0,
            hmtx: Vec::new(),
            dhmtx: Hmtx {
                lo: 0,
                hi: 0xffff,
                w: 1000,
            },
            vmtx: Vec::new(),
            dvmtx: Vmtx {
                lo: 0,
                hi: 0xffff,
                x: 0,
                y: 880,
                w: -1000,
            },
            is_embedded: false,
        }
    }

    /// `pdf_lookup_hmtx`.
    pub fn lookup_hmtx(&self, cid: i32) -> Hmtx {
        let (mut l, mut r) = (0i64, self.hmtx.len() as i64 - 1);
        while l <= r {
            let m = ((l + r) >> 1) as usize;
            let h = self.hmtx[m];
            if cid < h.lo {
                r = m as i64 - 1;
            } else if cid > h.hi {
                l = m as i64 + 1;
            } else {
                return h;
            }
        }
        self.dhmtx
    }

    /// `pdf_lookup_vmtx`.
    pub fn lookup_vmtx(&self, cid: i32) -> Vmtx {
        let (mut l, mut r) = (0i64, self.vmtx.len() as i64 - 1);
        while l <= r {
            let m = ((l + r) >> 1) as usize;
            let v = self.vmtx[m];
            if cid < v.lo {
                r = m as i64 - 1;
            } else if cid > v.hi {
                l = m as i64 + 1;
            } else {
                return v;
            }
        }
        let h = self.lookup_hmtx(cid);
        Vmtx {
            x: h.w / 2,
            ..self.dvmtx
        }
    }

    /// `pdf_font_cid_to_gid`.
    pub fn cid_to_gid(&self, cid: i32) -> i32 {
        cid_to_gid(&self.font, self, cid)
    }

    fn end_hmtx(&mut self) {
        self.hmtx.sort_by_key(|h| h.lo);
    }

    fn end_vmtx(&mut self) {
        self.vmtx.sort_by_key(|v| v.lo);
    }
}

/// `ft_cid_to_gid`, with the font passed apart while it is being built.
fn cid_to_gid(font: &Font, fd: &FontDesc, cid: i32) -> i32 {
    let Some(face) = &font.face else {
        return cid;
    };
    if let Some(to) = &fd.to_ttf_cmap {
        let mut ucs = to.lookup(cid as u32);
        if font.ft_substitute && fd.wmode != 0 {
            ucs = vertical_form(ucs);
        }
        return face.char_index(ucs as u32) as i32;
    }
    if let Some(map) = &fd.cid_to_gid
        && let Some(&g) = usize::try_from(cid).ok().and_then(|c| map.get(c))
    {
        return i32::from(g);
    }
    cid
}

/// The vertical presentation forms MuPDF uses for a substitute font.
fn vertical_form(c: i32) -> i32 {
    match c {
        0x0021 | 0xFF01 => 0xFE15,
        0x0028 | 0xFF08 => 0xFE35,
        0x0029 | 0xFF09 => 0xFE36,
        0x002C | 0xFF0C => 0xFE10,
        0x003A | 0xFF1A => 0xFE13,
        0x003B | 0xFF1B => 0xFE14,
        0x003F | 0xFF1F => 0xFE16,
        0x005B | 0xFF3B => 0xFE47,
        0x005D | 0xFF3D => 0xFE48,
        0x005F | 0xFF3F => 0xFE33,
        0x007B | 0xFF5B => 0xFE37,
        0x007D | 0xFF5D => 0xFE38,
        0x2013 => 0xFE32,
        0x2014 | 0x30FC | 0xFF0D => 0xFE31,
        0x2025 => 0xFE30,
        0x2026 => 0xFE19,
        0x3001 => 0xFE11,
        0x3002 => 0xFE12,
        0x3008 => 0xFE3F,
        0x3009 => 0xFE40,
        0x300A => 0xFE3D,
        0x300B => 0xFE3E,
        0x300C => 0xFE41,
        0x300D => 0xFE42,
        0x300E => 0xFE43,
        0x300F => 0xFE44,
        0x3010 => 0xFE3B,
        0x3011 => 0xFE3C,
        0x3014 => 0xFE39,
        0x3015 => 0xFE3A,
        0x3016 => 0xFE17,
        0x3017 => 0xFE18,
        other => other,
    }
}

/// `base_font_names`: the standard 14 and the names that stand for them.
const BASE_FONT_NAMES: &[&[&str]] = &[
    &["Courier", "CourierNew", "CourierNewPSMT"],
    &[
        "Courier-Bold",
        "CourierNew,Bold",
        "Courier,Bold",
        "CourierNewPS-BoldMT",
        "CourierNew-Bold",
    ],
    &[
        "Courier-Oblique",
        "CourierNew,Italic",
        "Courier,Italic",
        "CourierNewPS-ItalicMT",
        "CourierNew-Italic",
    ],
    &[
        "Courier-BoldOblique",
        "CourierNew,BoldItalic",
        "Courier,BoldItalic",
        "CourierNewPS-BoldItalicMT",
        "CourierNew-BoldItalic",
    ],
    &["Helvetica", "ArialMT", "Arial"],
    &[
        "Helvetica-Bold",
        "Arial-BoldMT",
        "Arial,Bold",
        "Arial-Bold",
        "Helvetica,Bold",
    ],
    &[
        "Helvetica-Oblique",
        "Arial-ItalicMT",
        "Arial,Italic",
        "Arial-Italic",
        "Helvetica,Italic",
        "Helvetica-Italic",
    ],
    &[
        "Helvetica-BoldOblique",
        "Arial-BoldItalicMT",
        "Arial,BoldItalic",
        "Arial-BoldItalic",
        "Helvetica,BoldItalic",
        "Helvetica-BoldItalic",
    ],
    &[
        "Times-Roman",
        "TimesNewRomanPSMT",
        "TimesNewRoman",
        "TimesNewRomanPS",
    ],
    &[
        "Times-Bold",
        "TimesNewRomanPS-BoldMT",
        "TimesNewRoman,Bold",
        "TimesNewRomanPS-Bold",
        "TimesNewRoman-Bold",
    ],
    &[
        "Times-Italic",
        "TimesNewRomanPS-ItalicMT",
        "TimesNewRoman,Italic",
        "TimesNewRomanPS-Italic",
        "TimesNewRoman-Italic",
    ],
    &[
        "Times-BoldItalic",
        "TimesNewRomanPS-BoldItalicMT",
        "TimesNewRoman,BoldItalic",
        "TimesNewRomanPS-BoldItalic",
        "TimesNewRoman-BoldItalic",
    ],
    &[
        "Symbol",
        "Symbol,Italic",
        "Symbol,Bold",
        "Symbol,BoldItalic",
        "SymbolMT",
        "SymbolMT,Italic",
        "SymbolMT,Bold",
        "SymbolMT,BoldItalic",
    ],
    &["ZapfDingbats"],
];

/// `skip_subset_prefix`.
fn skip_subset_prefix(name: &[u8]) -> &[u8] {
    if name.len() > 7 && name[..6].iter().all(u8::is_ascii_uppercase) && name[6] == b'+' {
        &name[7..]
    } else {
        name
    }
}

/// `strcmp_ignore_space` == 0.
fn eq_ignore_space(a: &[u8], b: &[u8]) -> bool {
    a.iter()
        .filter(|&&c| c != b' ')
        .eq(b.iter().filter(|&&c| c != b' '))
}

/// `pdf_clean_font_name`: the standard name, or None when the name is no
/// standard font's.
pub fn clean_font_name(name: &[u8]) -> Option<&'static str> {
    let s = skip_subset_prefix(name);
    BASE_FONT_NAMES
        .iter()
        .find(|names| names.iter().any(|n| eq_ignore_space(n.as_bytes(), s)))
        .map(|names| names[0])
}

/// FreeType's error strings for the errors a face can fail with.
fn ft_error_string(e: &str) -> &'static str {
    match e {
        "Unknown_File_Format" => "unknown file format",
        "Invalid_Table" => "broken table",
        "Table_Missing" => "SFNT font table missing",
        "Horiz_Header_Missing" => "horizontal header (hhea) table missing",
        _ => "broken file",
    }
}

/// `%g` as MuPDF's printf writes it.
fn fmt_g(f: f32) -> String {
    let mut out = Vec::new();
    crate::print::fmtfloat(f, &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// `fz_new_font_from_buffer`.
fn new_font(name: &[u8], data: Arc<[u8]>) -> Result<Font, String> {
    let face = Face::open(data).map_err(|e| {
        format!(
            "library error: FT_New_Memory_Face({}): {}",
            String::from_utf8_lossy(name),
            ft_error_string(e)
        )
    })?;
    let mut font = Font::new(name);
    let upem = f32::from(face.units_per_em);
    let [x0, y0, x1, y1] = face.bbox;
    font.set_bbox(
        x0 as f32 / upem,
        y0 as f32 / upem,
        x1 as f32 / upem,
        y1 as f32 / upem,
    );
    let (asc, desc, u) = (
        i32::from(face.ascender),
        i32::from(face.descender),
        i32::from(face.units_per_em),
    );
    font.ascender = if asc <= 0 || asc > MAX_TRUSTWORTHY_ASCENT * u {
        0.8
    } else {
        asc as f32 / upem
    };
    font.descender = if desc < MAX_TRUSTWORTHY_DESCENT * u || desc > -MAX_TRUSTWORTHY_DESCENT * u {
        -0.2
    } else {
        let d = desc as f32 / upem;
        if d > 0.0 { -d } else { d }
    };
    font.face = Some(face);
    Ok(font)
}

/// The value stored under `key`, unresolved, with null as None
/// (`pdf_dict_get`).
fn stored(doc: &Document, dict: &Object, key: &[u8]) -> Option<Object> {
    doc.resolve(dict)
        .as_dict()
        .and_then(|d| d.get(key).cloned())
        .filter(|o| !matches!(o, Object::Null))
}

/// The glyph names an /Encoding gives: a name, or a dictionary with
/// /BaseEncoding (else `default_base`) and /Differences.
fn read_encoding(
    doc: &Document,
    estrings: &mut [Option<Vec<u8>>; 256],
    encoding: &Object,
    default_base: Option<&[u8]>,
) {
    match doc.resolve(encoding) {
        Object::Name(n) => glyphs::load_encoding(estrings, &n),
        enc @ Object::Dict(_) => {
            match doc.dict_get(&enc, b"BaseEncoding") {
                Object::Name(n) => glyphs::load_encoding(estrings, &n),
                _ => {
                    if let Some(base) = default_base {
                        glyphs::load_encoding(estrings, base);
                    }
                }
            }
            if let Object::Array(diff) = doc.dict_get(&enc, b"Differences") {
                let mut k: i32 = 0;
                for item in diff.iter() {
                    match doc.resolve(item) {
                        i @ Object::Int(_) => k = i.to_int(),
                        Object::Name(n) => {
                            if let Ok(slot) = usize::try_from(k)
                                && slot < 256
                            {
                                estrings[slot] = Some(n.to_vec());
                                k += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }
}

/// Loads fonts for one document.
#[derive(Debug)]
pub struct Loader<'a> {
    pub doc: &'a Document,
}

impl Loader<'_> {
    fn warn(&self, code: &'static str, message: impl Into<String>) {
        self.doc.diagnostics().warn(code, message);
    }

    /// `pdf_load_builtin_font` (no system fonts, as in the oracle).
    fn builtin(&self, fd: &mut FontDesc, name: &[u8]) -> Result<Font, String> {
        let clean = clean_font_name(name).unwrap_or("Times-Roman");
        let data = font::base14(clean).ok_or_else(|| {
            report::syntax(&format!(
                "cannot find builtin font: '{}'",
                String::from_utf8_lossy(name)
            ))
        })?;
        let font = new_font(name, Arc::from(data))?;
        if clean == "Symbol" || clean == "ZapfDingbats" {
            fd.flags |= FD_SYMBOLIC;
        }
        if let Some(face) = &font.face {
            let upem = f32::from(face.units_per_em);
            fd.ascent = 1000.0 * f32::from(face.ascender) / upem;
            fd.descent = 1000.0 * f32::from(face.descender) / upem;
        }
        Ok(font)
    }

    /// `pdf_load_substitute_font`.
    fn substitute(
        &self,
        name: &[u8],
        mono: bool,
        serif: bool,
        bold: bool,
        italic: bool,
    ) -> Result<Font, String> {
        let family = if mono {
            [
                "Courier",
                "Courier-Oblique",
                "Courier-Bold",
                "Courier-BoldOblique",
            ]
        } else if serif {
            [
                "Times-Roman",
                "Times-Italic",
                "Times-Bold",
                "Times-BoldItalic",
            ]
        } else {
            [
                "Helvetica",
                "Helvetica-Oblique",
                "Helvetica-Bold",
                "Helvetica-BoldOblique",
            ]
        };
        let base = family[usize::from(bold) * 2 + usize::from(italic)];
        let data =
            font::base14(base).ok_or_else(|| report::syntax("cannot find substitute font"))?;
        let mut font = new_font(name, Arc::from(data))?;
        font.ft_substitute = true;
        font.ft_stretch = true;
        Ok(font)
    }

    /// `pdf_load_system_font`. The oracle's builtin CJK fonts are not
    /// carried, so a CJK substitute fails as in a build without them.
    fn system(&self, fd: &FontDesc, name: &[u8], collection: Option<&str>) -> Result<Font, String> {
        let has = |s: &[u8]| name.windows(s.len()).any(|w| w == s);
        let bold = has(b"Bold") || fd.flags & FD_FORCE_BOLD != 0;
        let italic = has(b"Italic") || has(b"Oblique") || fd.flags & FD_ITALIC != 0;
        let mono = fd.flags & FD_FIXED_PITCH != 0;
        let serif = fd.flags & FD_SERIF != 0;
        let cjk = || Err(report::syntax("cannot find builtin CJK font"));
        match collection {
            Some("Adobe-CNS1" | "Adobe-GB1" | "Adobe-Japan1" | "Adobe-Korea1") => cjk(),
            Some(c) => {
                if c != "Adobe-Identity" {
                    self.warn(
                        "font.unknown_collection",
                        format!("unknown cid collection: {c}"),
                    );
                }
                const KNOWN: &[&[u8]] = &[
                    b"SimFang", b"SimHei", b"SimKai", b"SimLi", b"SimSun", b"Song", b"MingLiU",
                    b"Gothic", b"Mincho", b"Batang", b"Gulim", b"Dotum",
                ];
                if KNOWN.iter().any(|k| has(k)) {
                    return cjk();
                }
                self.substitute(name, mono, serif, bold, italic)
            }
            None => self.substitute(name, mono, serif, bold, italic),
        }
    }

    /// `pdf_load_embedded_font`.
    fn embedded(&self, fd: &mut FontDesc, name: &[u8], obj: &Object) -> Result<Font, String> {
        let mut data = match self.doc.resolve(obj) {
            Object::Stream(s) => self.doc.stream_data(&s).map_err(|e| e.report())?,
            _ => return Err(report::format("object is not a stream")),
        };
        if data.len() > 12
            && data.starts_with(b"OTTO")
            && let Some(cff) = extract_cff(&data)?
        {
            data = cff;
        }
        let font = new_font(name, Arc::from(data))?;
        fd.is_embedded = true;
        Ok(font)
    }

    /// `pdf_load_font_descriptor`: the font program, and the flags and
    /// metrics the descriptor gives.
    fn descriptor(
        &self,
        fd: &mut FontDesc,
        dict: &Object,
        collection: Option<&str>,
        basefont: &[u8],
        cid: bool,
        fftype: &[u8],
    ) -> Result<Font, String> {
        let doc = self.doc;
        fd.flags = doc.dict_get(dict, b"Flags").to_int();
        fd.missing_width = doc.dict_get(dict, b"MissingWidth").to_real();
        let file = stored(doc, dict, fftype)
            .or_else(|| stored(doc, dict, b"FontFile"))
            .or_else(|| stored(doc, dict, b"FontFile2"))
            .or_else(|| stored(doc, dict, b"FontFile3"));
        let fallback = |fd: &mut FontDesc| {
            if !cid && clean_font_name(basefont).is_some() {
                self.builtin(fd, basefont)
            } else {
                self.system(fd, basefont, collection)
            }
        };
        let mut font = match file {
            Some(f @ Object::Ref(_)) => match self.embedded(fd, basefont, &f) {
                Ok(font) => font,
                Err(line) => {
                    self.warn("error.reported", line);
                    self.warn(
                        "font.embedded_failed",
                        "ignored error when loading embedded font; attempting to load system font",
                    );
                    fallback(fd)?
                }
            },
            _ => fallback(fd)?,
        };

        if let Some(face) = &font.face
            && face.format == Format::TrueType
        {
            let upem = f32::from(face.units_per_em);
            fd.ascent = 1000.0 * f32::from(face.ascender) / upem;
            fd.descent = 1000.0 * f32::from(face.descender) / upem;
        }
        let real_or = |key: &[u8], default: f32| match doc.dict_get(dict, key) {
            v if v.is_number() => v.to_real(),
            _ => default,
        };
        fd.ascent = real_or(b"Ascent", fd.ascent);
        fd.descent = real_or(b"Descent", fd.descent);
        if fd.descent > 0.0 {
            fd.descent = -fd.descent;
        }
        if fd.ascent <= 0.0
            || fd.ascent > (MAX_TRUSTWORTHY_ASCENT * 1000) as f32
            || fd.descent < (MAX_TRUSTWORTHY_DESCENT * 1000) as f32
        {
            if fd.ascent != 0.0 || fd.descent != 0.0 {
                self.warn(
                    "font.bogus_metrics",
                    format!(
                        "bogus font ({}) ascent/descent values ({} / {})",
                        String::from_utf8_lossy(basefont),
                        fmt_g(fd.ascent),
                        fmt_g(fd.descent)
                    ),
                );
            }
            font.ascender = 0.8;
            font.descender = -0.2;
        } else {
            font.ascender = fd.ascent / 1000.0;
            font.descender = fd.descent / 1000.0;
        }
        Ok(font)
    }

    /// `pdf_load_simple_font`; None for the dictionary is the last-resort
    /// font.
    fn simple(&self, dict: Option<&Object>, fftype: &[u8]) -> Result<FontDesc, String> {
        let doc = self.doc;
        let get = |key: &[u8]| dict.map_or(Object::Null, |d| doc.dict_get(d, key));
        let raw = |key: &[u8]| dict.and_then(|d| stored(doc, d, key));
        let basefont = get(b"BaseFont")
            .as_name()
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        let descriptor = raw(b"FontDescriptor");
        let mut fd = FontDesc::new();
        let mut font = match &descriptor {
            Some(desc) => self.descriptor(&mut fd, desc, None, &basefont, false, fftype)?,
            None => self.builtin(&mut fd, &basefont)?,
        };

        // Some Chinese producers call codepage 936 WinAnsiEncoding.
        if let Some(desc) = &descriptor
            && raw(b"Encoding").is_some_and(|e| e.is_name(b"WinAnsiEncoding"))
            && doc.dict_get(desc, b"Flags").to_int() == 4
        {
            const CP936: &[(&[u8], &[u8])] = &[
                (b"\xCB\xCE\xCC\xE5", b"SimSun,Regular"),
                (b"\xBA\xDA\xCC\xE5", b"SimHei,Regular"),
                (b"\xBF\xAC\xCC\xE5_GB2312", b"SimKai,Regular"),
                (b"\xB7\xC2\xCB\xCE_GB2312", b"SimFang,Regular"),
                (b"\xC1\xA5\xCA\xE9", b"SimLi,Regular"),
            ];
            if let Some((_, real)) = CP936.iter().find(|(n, _)| *n == basefont.as_slice()) {
                self.warn(
                    "font.s22pdf",
                    "workaround for S22PDF lying about chinese font encodings",
                );
                let mut fd = FontDesc::new();
                let font =
                    self.descriptor(&mut fd, desc, Some("Adobe-GB1"), real, false, fftype)?;
                fd.encoding = cmap::load_system(b"GBK-EUC-H")?;
                fd.to_unicode = Some(cmap::load_system(b"Adobe-GB1-UCS2")?);
                fd.to_ttf_cmap = Some(cmap::load_system(b"Adobe-GB1-UCS2")?);
                return Ok(self.simple_widths(fd, font, dict));
            }
        }

        let Some(face) = font.face.as_mut() else {
            return Err(report::syntax("font has no face"));
        };
        let mut kind = match face.format {
            Format::TrueType => Format::TrueType,
            Format::Type1 | Format::Cff => Format::Type1,
        };
        let symbolic = fd.flags & FD_SYMBOLIC != 0 && fd.flags & FD_NONSYMBOLIC == 0;
        let cmap = match kind {
            Format::TrueType => select_truetype_cmap(face, symbolic),
            _ => face
                .charmaps
                .iter()
                .position(|c| c.platform_id == 7)
                .or((!face.charmaps.is_empty()).then_some(0)),
        };
        match cmap {
            Some(i) => face.charmap = Some(i),
            None => self.warn("font.no_cmap", "freetype could not find any cmaps"),
        }

        let mut estrings: [Option<Vec<u8>>; 256] = std::array::from_fn(|_| None);
        let standard = (!fd.is_embedded && !symbolic).then_some(&b"StandardEncoding"[..]);
        match raw(b"Encoding") {
            Some(encoding) => read_encoding(doc, &mut estrings, &encoding, standard),
            None => {
                if let Some(s) = standard {
                    glyphs::load_encoding(&mut estrings, s);
                }
            }
        }

        // The builtin encoding, then glyph names where they can be found.
        let face = &*face;
        let mut etable = [0u16; 256];
        for (i, e) in etable.iter_mut().enumerate() {
            *e = face.char_index(i as u32) as u16;
        }
        match get(b"Subtype").as_name() {
            Some(b"Type1" | b"MMType1" | b"CIDFontType0") => kind = Format::Type1,
            Some(b"TrueType" | b"CIDFontType2") => kind = Format::TrueType,
            _ => {}
        }
        let charmap = face.charmap.and_then(|i| face.charmaps.get(i));
        let by_name = |etable: &mut [u16; 256], find: &dyn Fn(&[u8]) -> u32| {
            for (e, s) in etable.iter_mut().zip(estrings.iter()) {
                if let Some(n) = s {
                    let g = find(n);
                    if g > 0 {
                        *e = g as u16;
                    }
                }
            }
        };
        if kind == Format::Type1 {
            by_name(&mut etable, &|n| face.name_index(n));
        } else {
            let platform = charmap.map(|c| c.platform_id);
            if !symbolic && platform == Some(3) {
                by_name(&mut etable, &|n| find_glyph_by_unicode_name(face, n));
            } else if !symbolic && platform == Some(1) {
                by_name(&mut etable, &|n| {
                    let g = mac_roman_code(n).map_or(0, |c| face.char_index(c));
                    if g == 0 { face.name_index(n) } else { g }
                });
            } else if charmap.is_none_or(|c| c.encoding != FtEncoding::MsSymbol) {
                by_name(&mut etable, &|n| face.name_index(n));
            }
        }

        // Names for the codes only the builtin encoding fills.
        for i in 0..256 {
            if etable[i] != 0 && estrings[i].is_none() {
                if face.has_glyph_names {
                    if let Some(mut n) = face.glyph_name(u32::from(etable[i])) {
                        n.truncate(31);
                        if !n.is_empty() {
                            estrings[i] = Some(n);
                        }
                    }
                } else {
                    estrings[i] = tables::WIN_ANSI[i].map(|n| n.as_bytes().to_vec());
                }
            }
        }
        // Symbolic Type 1 fonts with an implicit encoding and private names.
        if kind == Format::Type1 && symbolic {
            for i in 0..256 {
                if etable[i] != 0
                    && estrings[i]
                        .as_ref()
                        .is_some_and(|n| glyphs::unicode(n) == 0)
                {
                    estrings[i] = tables::STANDARD[i].map(|n| n.as_bytes().to_vec());
                }
            }
        }

        fd.encoding = Arc::new(Cmap::identity(0, 1));
        fd.cid_to_gid = Some(etable.to_vec());
        let to_unicode = raw(b"ToUnicode").unwrap_or(Object::Null);
        if let Err(line) = self.load_to_unicode(&mut fd, Some(&estrings), None, &to_unicode) {
            self.warn("error.reported", line);
            self.warn("font.to_unicode", "cannot load ToUnicode CMap");
        }
        Ok(self.simple_widths(fd, font, dict))
    }

    /// The widths of a simple font, from /Widths or else the font's own.
    fn simple_widths(&self, mut fd: FontDesc, font: Font, dict: Option<&Object>) -> FontDesc {
        let doc = self.doc;
        let get = |key: &[u8]| dict.map_or(Object::Null, |d| doc.dict_get(d, key));
        fd.dhmtx.w = fd.missing_width as i32;
        if dict.and_then(|d| stored(doc, d, b"Widths")).is_some() {
            let widths = get(b"Widths");
            let items = widths.as_array().unwrap_or_default();
            let mut first = get(b"FirstChar").to_int();
            let mut last = get(b"LastChar").to_int();
            if first < 0 || last > 255 || first > last {
                first = 0;
                last = 0;
            }
            for i in 0..=(last - first) {
                let w = items.get(i as usize).map_or(0, |v| doc.resolve(v).to_int());
                fd.hmtx.push(Hmtx {
                    lo: i + first,
                    hi: i + first,
                    w,
                });
            }
        } else if let Some(face) = &font.face {
            let upem = i64::from(if face.units_per_em == 0 {
                2048
            } else {
                face.units_per_em
            });
            for i in 0..256 {
                let gid = cid_to_gid(&font, &fd, i);
                let adv = u32::try_from(gid)
                    .ok()
                    .and_then(|g| face.advance(g))
                    .unwrap_or(0);
                fd.hmtx.push(Hmtx {
                    lo: i,
                    hi: i,
                    w: (adv * 1000 / upem) as i32,
                });
            }
        }
        fd.end_hmtx();
        fd.font = Arc::new(font);
        fd
    }

    /// `pdf_load_to_unicode`.
    fn load_to_unicode(
        &self,
        fd: &mut FontDesc,
        strings: Option<&[Option<Vec<u8>>; 256]>,
        collection: Option<&str>,
        cmapstm: &Object,
    ) -> Result<(), String> {
        let doc = self.doc;
        match doc.resolve(cmapstm) {
            Object::Stream(_) => {
                let ucs = cmap::load_embedded(doc, cmapstm)?;
                fd.to_unicode = Some(remap(doc, &fd.encoding, &ucs)?);
            }
            Object::Name(n) => {
                let ucs = cmap::load_system(&n)?;
                fd.to_unicode = Some(remap(doc, &fd.encoding, &ucs)?);
            }
            _ => {
                if let Some(name) = collection.and_then(|c| ucs2_cmap(c, false)) {
                    fd.to_unicode = Some(cmap::load_system(name.as_bytes())?);
                }
            }
        }
        if let Some(strings) = strings {
            fd.cid_to_ucs = Some(
                strings
                    .iter()
                    .map(|s| {
                        s.as_ref()
                            .map_or(glyphs::REPLACEMENT, |n| glyphs::unicode(n))
                            as u16
                    })
                    .collect(),
            );
        }
        Ok(())
    }

    /// `load_cid_font`.
    fn cid(
        &self,
        dict: &Object,
        encoding: Option<&Object>,
        to_unicode: &Object,
        fftype: &[u8],
    ) -> Result<FontDesc, String> {
        let doc = self.doc;
        let get = |key: &[u8]| doc.dict_get(dict, key);
        let basefont = get(b"BaseFont")
            .as_name()
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        let collection = match stored(doc, dict, b"CIDSystemInfo") {
            Some(info) => {
                let s = |k: &[u8]| match doc.dict_get(&info, k) {
                    Object::String(s) => String::from_utf8_lossy(&s).into_owned(),
                    _ => "(null)".to_string(),
                };
                format!("{}-{}", s(b"Registry"), s(b"Ordering"))
            }
            None => {
                self.warn(
                    "font.no_cidsysteminfo",
                    "CIDFont is missing CIDSystemInfo dictionary; assuming Adobe-Identity",
                );
                "Adobe-Identity".to_string()
            }
        };
        let mut fd = FontDesc::new();
        fd.encoding = match encoding.map(|e| (e, doc.resolve(e))) {
            Some((_, Object::Name(n))) => cmap::load_system(&n)?,
            Some((e @ Object::Ref(_), _)) => cmap::load_embedded(doc, e)?,
            _ => return Err(report::syntax("font missing encoding")),
        };
        fd.wmode = fd.encoding.wmode;
        let descriptor = stored(doc, dict, b"FontDescriptor")
            .ok_or_else(|| report::syntax("missing font descriptor"))?;
        let mut font = self.descriptor(
            &mut fd,
            &descriptor,
            Some(&collection),
            &basefont,
            true,
            fftype,
        )?;

        let cidtogid = stored(doc, dict, b"CIDToGIDMap");
        match cidtogid.as_ref().map(|c| doc.resolve(c)) {
            Some(Object::Stream(s)) => {
                let data = doc.stream_data(&s).map_err(|e| e.report())?;
                fd.cid_to_gid = Some(
                    data.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|&c| u16::from_be_bytes(c))
                        .collect(),
                );
            }
            Some(c) if !c.is_name(b"Identity") => {
                self.warn("font.cidtogidmap", "ignoring unknown CIDToGIDMap entry");
            }
            _ if font.ft_substitute => {
                if let Some(face) = font.face.as_mut()
                    && !face.select_unicode()
                {
                    return Err(report::syntax(
                        "no unicode cmap when emulating CID font: invalid character map handle",
                    ));
                }
                if let Some(name) = ucs2_cmap(&collection, true) {
                    fd.to_ttf_cmap = Some(cmap::load_system(name.as_bytes())?);
                }
            }
            _ => {}
        }

        self.load_to_unicode(&mut fd, None, Some(&collection), to_unicode)?;

        // A substitute cannot take glyph ids: map through Unicode instead.
        if fd.encoding.name.contains("Identity-") && font.ft_substitute {
            if fd.to_ttf_cmap.is_none() {
                fd.to_ttf_cmap = fd
                    .to_unicode
                    .clone()
                    .or_else(|| cmap::builtin("TrueType-UCS2"));
            }
            let base = String::from_utf8_lossy(&basefont);
            match fd.to_ttf_cmap.clone() {
                Some(to) => {
                    self.warn(
                        "font.identity_substitute",
                        format!(
                            "non-embedded font using identity encoding: {base} (mapping via {})",
                            to.name
                        ),
                    );
                    if fd.to_unicode.is_none() {
                        fd.to_unicode = Some(to);
                    }
                }
                None => self.warn(
                    "font.identity_substitute",
                    format!("non-embedded font using identity encoding: {base}"),
                ),
            }
        }

        // Horizontal metrics.
        fd.dhmtx.w = match get(b"DW") {
            v if v.is_number() => v.to_int(),
            _ => 1000,
        };
        if stored(doc, dict, b"W").is_some() {
            let w = get(b"W");
            let w = w.as_array().unwrap_or_default();
            let int = |i: usize| w.get(i).map_or(0, |v| doc.resolve(v).to_int());
            let mut i = 0;
            while i < w.len() {
                let c0 = int(i);
                match w.get(i + 1).map(|v| doc.resolve(v)) {
                    Some(Object::Array(ws)) => {
                        for (k, v) in ws.iter().enumerate() {
                            let c = c0.wrapping_add(k as i32);
                            fd.hmtx.push(Hmtx {
                                lo: c,
                                hi: c,
                                w: doc.resolve(v).to_int(),
                            });
                        }
                        i += 2;
                    }
                    other => {
                        fd.hmtx.push(Hmtx {
                            lo: c0,
                            hi: other.map_or(0, |o| o.to_int()),
                            w: int(i + 2),
                        });
                        i += 3;
                    }
                }
            }
        } else {
            // Without widths there is nothing to stretch to.
            font.ft_stretch = false;
        }
        fd.end_hmtx();

        if fd.encoding.wmode == 1 {
            if stored(doc, dict, b"DW2").is_some() {
                let dw2 = get(b"DW2");
                let dw2 = dw2.as_array().unwrap_or_default();
                let int = |i: usize| dw2.get(i).map_or(0, |v| doc.resolve(v).to_int());
                fd.dvmtx.y = int(0);
                fd.dvmtx.w = int(1);
            }
            if stored(doc, dict, b"W2").is_some() {
                let w2 = get(b"W2");
                let w2 = w2.as_array().unwrap_or_default();
                let int = |i: usize| w2.get(i).map_or(0, |v| doc.resolve(v).to_int());
                let mut i = 0;
                while i < w2.len() {
                    let c0 = int(i);
                    match w2.get(i + 1).map(|v| doc.resolve(v)) {
                        Some(Object::Array(vs)) => {
                            let val = |j: usize| vs.get(j).map_or(0, |v| doc.resolve(v).to_int());
                            let mut k = 0;
                            while k * 3 < vs.len() {
                                let c = c0.wrapping_add(k as i32);
                                fd.vmtx.push(Vmtx {
                                    lo: c,
                                    hi: c,
                                    w: val(k * 3),
                                    x: val(k * 3 + 1),
                                    y: val(k * 3 + 2),
                                });
                                k += 1;
                            }
                            i += 2;
                        }
                        other => {
                            fd.vmtx.push(Vmtx {
                                lo: c0,
                                hi: other.map_or(0, |o| o.to_int()),
                                w: int(i + 2),
                                x: int(i + 3),
                                y: int(i + 4),
                            });
                            i += 5;
                        }
                    }
                }
            }
            fd.end_vmtx();
        }
        fd.font = Arc::new(font);
        Ok(fd)
    }

    /// `pdf_load_type0_font`.
    fn type0(&self, dict: &Object) -> Result<FontDesc, String> {
        let doc = self.doc;
        if stored(doc, dict, b"DescendantFonts").is_none() {
            return Err(report::syntax("cid font is missing descendant fonts"));
        }
        let dfonts = doc.dict_get(dict, b"DescendantFonts");
        let dfont = dfonts
            .as_array()
            .and_then(|a| a.first().cloned())
            .unwrap_or(Object::Null);
        let encoding = stored(doc, dict, b"Encoding");
        let to_unicode = stored(doc, dict, b"ToUnicode").unwrap_or(Object::Null);
        match doc.dict_get(&dfont, b"Subtype").as_name() {
            Some(b"CIDFontType0") => self.cid(&dfont, encoding.as_ref(), &to_unicode, b"FontFile3"),
            Some(b"CIDFontType2") => self.cid(&dfont, encoding.as_ref(), &to_unicode, b"FontFile2"),
            _ => Err(report::syntax("unknown cid font type")),
        }
    }

    /// `pdf_load_type3_font`; `resources` is the innermost resource
    /// dictionary, for a font without its own. The interpreter prepares
    /// the glyphs.
    fn type3(&self, dict: &Object, resources: Option<&Object>) -> Result<FontDesc, String> {
        let doc = self.doc;
        let get = |key: &[u8]| doc.dict_get(dict, key);
        let name = match get(b"Name") {
            Object::Name(n) => n[..n.len().min(255)].to_vec(),
            _ => format!("Type3 ({} 0 R)", dict.as_ref().map_or(0, |r| r.num)).into_bytes(),
        };
        let matrix = match get(b"FontMatrix") {
            Object::Array(a) => {
                let n = |i: usize| a.get(i).map_or(0.0, |v| doc.resolve(v).to_real());
                Matrix {
                    a: n(0),
                    b: n(1),
                    c: n(2),
                    d: n(3),
                    e: n(4),
                    f: n(5),
                }
            }
            _ => Matrix::IDENTITY,
        };
        let bbox = doc.to_rect(Some(&get(b"FontBBox"))).transform(matrix);
        let mut font = Font::new(&name);
        let mut t3 = Type3 {
            matrix,
            procs: vec![None; 256],
            widths: vec![0.0; 256],
            resources: None,
            flags: vec![0; 256],
            bboxes: vec![Rect::EMPTY; 256],
        };
        font.t3 = Some(t3.clone());
        font.set_bbox(bbox.x0, bbox.y0, bbox.x1, bbox.y1);
        font.ascender = bbox.y1;
        font.descender = bbox.y0;

        let mut estrings: [Option<Vec<u8>>; 256] = std::array::from_fn(|_| None);
        let encoding = stored(doc, dict, b"Encoding")
            .ok_or_else(|| report::syntax("Type3 font missing Encoding"))?;
        read_encoding(doc, &mut estrings, &encoding, None);

        let mut fd = FontDesc::new();
        fd.encoding = Arc::new(Cmap::identity(0, 1));
        let to_unicode = stored(doc, dict, b"ToUnicode").unwrap_or(Object::Null);
        self.load_to_unicode(&mut fd, Some(&estrings), None, &to_unicode)?;
        // The code as ASCII when nothing better is known.
        if let Some(ucs) = fd.cid_to_ucs.as_mut() {
            for (i, u) in ucs.iter_mut().enumerate().take(127).skip(32) {
                if u32::from(*u) == glyphs::REPLACEMENT {
                    *u = i as u16;
                }
            }
        }

        fd.dhmtx.w = 0;
        let mut first = get(b"FirstChar").to_int();
        let mut last = get(b"LastChar").to_int();
        if first < 0 || last > 255 || first > last {
            first = 0;
            last = 0;
        }
        if stored(doc, dict, b"Widths").is_none() {
            return Err(report::syntax("Type3 font missing Widths"));
        }
        let widths = get(b"Widths");
        let ws = widths.as_array().unwrap_or_default();
        for i in first..=last {
            let w = ws
                .get((i - first) as usize)
                .map_or(0.0, |v| doc.resolve(v).to_real());
            let w = matrix.a * w * 1000.0;
            t3.widths[i as usize] = w * 0.001;
            fd.hmtx.push(Hmtx {
                lo: i,
                hi: i,
                w: w as i32,
            });
        }
        fd.end_hmtx();

        // A font without resources borrows the innermost ones in use.
        t3.resources = stored(doc, dict, b"Resources").or_else(|| resources.cloned());
        if t3.resources.is_none() {
            self.warn(
                "font.type3_resources",
                "no resource dictionary for type 3 font!",
            );
        }
        let charprocs = stored(doc, dict, b"CharProcs")
            .ok_or_else(|| report::syntax("Type3 font missing CharProcs"))?;
        let charprocs = doc.resolve(&charprocs);
        for (proc, name) in t3.procs.iter_mut().zip(estrings.iter()) {
            if let Some(n) = name
                && let Some(p) = charprocs.as_dict().and_then(|d| d.get(n))
                && let Object::Stream(s) = doc.resolve(p)
            {
                let data = doc.stream_data(&s).map_err(|e| e.report())?;
                *proc = Some(Arc::from(data));
            }
        }
        font.t3 = Some(t3);
        fd.font = Arc::new(font);
        Ok(fd)
    }

    /// `pdf_load_font`, without the store: `resources` is the innermost
    /// resource dictionary (a Type 3 font without its own uses it).
    pub fn load(&self, dict: &Object, resources: Option<&Object>) -> Result<FontDesc, String> {
        let doc = self.doc;
        let has = |key: &[u8]| stored(doc, dict, key).is_some();
        match doc.dict_get(dict, b"Subtype").as_name() {
            Some(b"Type0") => self.type0(dict),
            Some(b"Type1" | b"MMType1") => self.simple(Some(dict), b"FontFile"),
            Some(b"TrueType") => self.simple(Some(dict), b"FontFile2"),
            Some(b"Type3") => self.type3(dict, resources),
            _ if has(b"CharProcs") => {
                self.warn("font.guess", "unknown font format, guessing type3.");
                self.type3(dict, resources)
            }
            _ if has(b"DescendantFonts") => {
                self.warn("font.guess", "unknown font format, guessing type0.");
                self.type0(dict)
            }
            _ => {
                self.warn(
                    "font.guess",
                    "unknown font format, guessing type1 or truetype.",
                );
                self.simple(Some(dict), b"FontFile")
            }
        }
    }

    /// `pdf_load_hail_mary_font`: the font used when a font fails.
    pub fn hail_mary(&self) -> Result<FontDesc, String> {
        self.simple(None, b"FontFile")
    }
}

/// The UCS-2 CMap of a CID collection; Adobe-Japan2's is used only to
/// find glyphs.
fn ucs2_cmap(collection: &str, japan2: bool) -> Option<&'static str> {
    match collection {
        "Adobe-CNS1" => Some("Adobe-CNS1-UCS2"),
        "Adobe-GB1" => Some("Adobe-GB1-UCS2"),
        "Adobe-Japan1" => Some("Adobe-Japan1-UCS2"),
        "Adobe-Japan2" if japan2 => Some("Adobe-Japan2-UCS2"),
        "Adobe-Korea1" => Some("Adobe-Korea1-UCS2"),
        _ => None,
    }
}

/// `pdf_extract_cff_subtable`.
fn extract_cff(data: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let u16_at = |at: usize| {
        data.get(at..at + 2)
            .map_or(0, |b| usize::from(u16::from_be_bytes([b[0], b[1]])))
    };
    let u32_at = |at: usize| {
        data.get(at..at + 4).map_or(0, |b| {
            u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        })
    };
    let n = u16_at(4);
    if 12 + n * 16 > data.len() {
        return Err(report::syntax("invalid TTF header"));
    }
    for i in 0..n {
        let rec = 12 + i * 16;
        if &data[rec..rec + 4] == b"CFF " {
            let offset = u32_at(rec + 8);
            let end = offset + u32_at(rec + 12);
            if end > data.len() as u64 {
                return Err(report::syntax("invalid TTF subtable offset/length"));
            }
            return Ok(Some(data[offset as usize..end as usize].to_vec()));
        }
    }
    Ok(None)
}

/// `select_truetype_cmap`.
fn select_truetype_cmap(face: &Face, symbolic: bool) -> Option<usize> {
    let cms = &face.charmaps;
    let find = |p: u16, e: u16, check: bool| {
        (0..cms.len()).find(|&i| {
            cms[i].platform_id == p
                && cms[i].encoding_id == e
                && (!check || face.cmap_format(i) != -1)
        })
    };
    if symbolic && let Some(i) = find(3, 0, false) {
        return Some(i);
    }
    find(3, 1, true)
        .or_else(|| find(1, 0, true))
        .or_else(|| (!cms.is_empty() && face.cmap_format(0) != -1).then_some(0))
}

/// `ft_find_glyph_by_unicode_name`.
fn find_glyph_by_unicode_name(face: &Face, name: &[u8]) -> u32 {
    let u = glyphs::unicode_strict(name);
    if u > 0 {
        let g = face.char_index(u);
        if g > 0 {
            return g;
        }
    }
    let g = face.name_index(name);
    if g > 0 {
        return g;
    }
    let u = glyphs::unicode(name);
    if u > 0 {
        return face.char_index(u);
    }
    0
}

/// `lookup_mre_code`.
fn mac_roman_code(name: &[u8]) -> Option<u32> {
    const DIFF: &[(u32, &str)] = &[
        (173, "notequal"),
        (176, "infinity"),
        (178, "lessequal"),
        (179, "greaterequal"),
        (182, "partialdiff"),
        (183, "summation"),
        (184, "product"),
        (185, "pi"),
        (186, "integral"),
        (189, "Omega"),
        (195, "radical"),
        (197, "approxequal"),
        (198, "Delta"),
        (215, "lozenge"),
        (219, "Euro"),
        (240, "apple"),
    ];
    if let Some(&(c, _)) = DIFF.iter().find(|(_, n)| n.as_bytes() == name) {
        return Some(c);
    }
    let i = tables::MAC_ROMAN
        .iter()
        .position(|n| n.is_some_and(|n| n.as_bytes() == name))?;
    (i > 0).then_some(i as u32)
}

/// `pdf_remap_cmap`: Unicode by glyph from glyph by code and Unicode by
/// code.
fn remap(
    doc: &Document,
    gid_from_cpt: &Arc<Cmap>,
    ucs_from_cpt: &Arc<Cmap>,
) -> Result<Arc<Cmap>, String> {
    let mut budget = MAX_REMAP;
    remap_imp(doc, gid_from_cpt, ucs_from_cpt, &mut budget, 0)
}

fn remap_imp(
    doc: &Document,
    gid_from_cpt: &Arc<Cmap>,
    ucs_from_cpt: &Arc<Cmap>,
    budget: &mut u64,
    depth: usize,
) -> Result<Arc<Cmap>, String> {
    if gid_from_cpt.is_effectively_identity(ucs_from_cpt) {
        return Ok(ucs_from_cpt.clone());
    }
    let diag = doc.diagnostics();
    let mut warn = |m: &str| diag.warn("cmap.syntax", m.to_string());
    let mut out = Cmap::new();
    if let Some(u) = &gid_from_cpt.usecmap {
        if depth >= cmap::MAX_USECMAP {
            return Err(report::syntax("too many nested CMaps"));
        }
        out.usecmap = Some(remap_imp(doc, u, ucs_from_cpt, budget, depth + 1)?);
    }
    out.add_codespace(0, 0x7fff_ffff, 4, &mut warn);
    for (a, b, x) in gid_from_cpt.sorted_ranges() {
        let n = b.wrapping_sub(a);
        let mut k: u32 = 0;
        loop {
            if *budget == 0 {
                return Err("limit error: ToUnicode remap too large".to_string());
            }
            *budget -= 1;
            let ucs = ucs_from_cpt.lookup_full(a.wrapping_add(k));
            let gid = x.wrapping_add(k);
            if ucs.len() == 1 {
                out.map_range(gid, gid, ucs[0], &mut warn)?;
            } else if ucs.len() > 1 {
                out.map_one_to_many(gid, &ucs, &mut warn)?;
            }
            if k == n {
                break;
            }
            k += 1;
        }
    }
    Ok(Arc::new(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_names_find_the_standard_font() {
        assert_eq!(
            clean_font_name(b"ABCDEF+Arial,Bold"),
            Some("Helvetica-Bold")
        );
        assert_eq!(clean_font_name(b"Times New Roman"), Some("Times-Roman"));
        assert_eq!(clean_font_name(b"ABCDEF+"), None);
        assert_eq!(clean_font_name(b"Garamond"), None);
    }

    #[test]
    fn metrics_fall_back_to_the_defaults() {
        let mut fd = FontDesc::new();
        fd.hmtx = vec![
            Hmtx {
                lo: 10,
                hi: 20,
                w: 300,
            },
            Hmtx {
                lo: 0,
                hi: 5,
                w: 100,
            },
        ];
        fd.end_hmtx();
        assert_eq!(fd.lookup_hmtx(3).w, 100);
        assert_eq!(fd.lookup_hmtx(15).w, 300);
        assert_eq!(fd.lookup_hmtx(7).w, 1000);
        let v = fd.lookup_vmtx(15);
        assert_eq!((v.x, v.y, v.w), (150, 880, -1000));
    }

    #[test]
    fn vertical_forms_replace_punctuation() {
        assert_eq!(vertical_form(0x3001), 0xFE11);
        assert_eq!(vertical_form(0xFF0D), 0xFE31);
        assert_eq!(vertical_form(0x41), 0x41);
    }
}
