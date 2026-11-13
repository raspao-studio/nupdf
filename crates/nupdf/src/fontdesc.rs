// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-font.c, source/pdf/pdf-type3.c,
// source/pdf/pdf-unicode.c, source/pdf/pdf-metrics.c and source/fitz/font.c
// (fz_new_font_from_buffer), Copyright (C) 2004-2026 Artifex Software, Inc.

//! PDF fonts as `pdf_load_font` loads them: the font program (embedded,
//! builtin or a substitute), the encoding from codes to CIDs and glyphs,
//! the Unicode of each code, and the widths.

use std::sync::{Arc, OnceLock};

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
    /// `fz_font`'s style flags.
    pub is_mono: bool,
    pub is_serif: bool,
    pub is_bold: bool,
    pub is_italic: bool,
    /// `width_table` and `width_default`: the PDF's widths by glyph, for
    /// stretching a substitute font's glyphs.
    pub width_table: Option<Arc<(Vec<i16>, i32)>>,
    /// The glyph boxes measured so far (`bbox_table`).
    glyph_boxes: GlyphBoxes,
}

/// A block of 256 glyph boxes, allocated on first use.
type BoxBlock = OnceLock<Box<[OnceLock<Rect>]>>;

/// Glyph boxes, measured on first use, in blocks of 256 glyphs.
#[derive(Clone, Default)]
pub struct GlyphBoxes(Arc<Vec<BoxBlock>>);

impl std::fmt::Debug for GlyphBoxes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GlyphBoxes({} blocks)", self.0.len())
    }
}

impl GlyphBoxes {
    fn new(glyphs: u32) -> GlyphBoxes {
        GlyphBoxes(Arc::new(
            (0..glyphs.div_ceil(256)).map(|_| OnceLock::new()).collect(),
        ))
    }

    /// The box of glyph `gid`, measured by `f` the first time; None for a
    /// glyph past the font's count.
    fn get(&self, gid: usize, f: impl FnOnce() -> Rect) -> Option<Rect> {
        let block = self
            .0
            .get(gid >> 8)?
            .get_or_init(|| (0..256).map(|_| OnceLock::new()).collect());
        Some(*block.get(gid & 255)?.get_or_init(f))
    }
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
            is_mono: false,
            is_serif: false,
            is_bold: false,
            is_italic: false,
            width_table: None,
            glyph_boxes: GlyphBoxes::default(),
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
#[derive(Debug, Clone)]
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
    /// A copy whose font is counted apart from this one's: every character
    /// holds its font, and counting one font's holders from many threads
    /// at once costs more than copying it.
    pub fn copy_for_thread(&self) -> FontDesc {
        let mut fd = self.clone();
        fd.font = Arc::new(Font::clone(&self.font));
        fd
    }

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
        "Hmtx_Table_Missing" => "horizontal metrics (hmtx) table missing",
        "Locations_Missing" => "locations (loca) table missing",
        "Array_Too_Large" => "array allocation size too large",
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
    // MuPDF keeps a glyph box table for every font it loads from a
    // program, CJK substitutes apart.
    font.glyph_boxes = GlyphBoxes::new(face.num_glyphs);
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
    // The face's style, then what the name says.
    let has = |s: &[u8]| name.windows(s.len()).any(|w| w == s);
    font.is_mono = face.fixed_width;
    font.is_serif = face.family_class.is_none_or(|c| c & 2048 == 0);
    font.is_bold = face.bold || has(b"Semibold") || has(b"Bold");
    font.is_italic = face.italic || has(b"Italic") || has(b"Oblique");
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

/// `fz_bound_glyph`: a Type 3 glyph's prepared box, a font program's
/// glyph as its outline measures, or the font's box.
pub fn bound_glyph(font: &Font, gid: i32, trm: Matrix) -> Rect {
    let tiny = Rect::new(0.0, 0.0, 0.000_000_1, 0.000_000_1);
    let r = match &font.t3 {
        // MuPDF measures an infinite or empty box again, which gives the
        // same box: an infinite one stays, an empty one becomes tiny.
        Some(t3) => match usize::try_from(gid).ok().and_then(|g| t3.bboxes.get(g)) {
            Some(r) if r.is_empty() => tiny,
            Some(r) => *r,
            None => font.bbox,
        },
        None => match (&font.face, usize::try_from(gid)) {
            (Some(face), Ok(g)) => font
                .glyph_boxes
                .get(g, || {
                    let r = measure_glyph(font, face, g as u32);
                    if r.is_empty() { tiny } else { r }
                })
                .unwrap_or(font.bbox),
            _ => font.bbox,
        },
    };
    r.transform(trm)
}

/// `fz_advance_glyph`: the glyph's advance in ems, horizontal or (with
/// `wmode` 1) vertical. A substitute font stretched to the PDF's widths
/// takes them; a Type 3 font its /Widths. FreeType's vertical advance
/// for a font without vertical metrics is its line height.
pub fn advance_glyph(font: &Font, gid: i32, wmode: i32) -> f32 {
    if let Some(face) = &font.face {
        let default = font.width_table.as_ref().map_or(0, |t| t.1);
        if gid < 0 {
            return default as f32 / 1000.0;
        }
        if font.ft_stretch
            && let Some(table) = font.width_table.as_deref()
        {
            return table
                .0
                .get(gid as usize)
                .map_or(table.1 as f32, |&w| f32::from(w))
                / 1000.0;
        }
        let upem = f32::from(face.units_per_em.max(1));
        if wmode != 0 {
            return (f32::from(face.ascender) - f32::from(face.descender)) / upem;
        }
        return face.advance(gid as u32).unwrap_or(0) as f32 / upem;
    }
    match &font.t3 {
        Some(t3) => usize::try_from(gid)
            .ok()
            .filter(|&g| g < 256)
            .and_then(|g| t3.widths.get(g).copied())
            .unwrap_or(0.0),
        None => 0.0,
    }
}

/// `fz_bound_ft_glyph`: the control box of the glyph's outline, in ems,
/// stretched to the PDF's width for a substitute font; empty for a glyph
/// that does not load. A font program whose outlines are not read here
/// gives the font's box.
fn measure_glyph(font: &Font, face: &Face, gid: u32) -> Rect {
    let Some(result) = face.glyph_cbox(gid) else {
        return font.bbox;
    };
    let b = match result {
        Ok(Some(b)) => b,
        Ok(None) | Err(_) => return Rect::new(0.0, 0.0, 0.0, 0.0),
    };
    let (mut x0, mut x1) = (b.x0, b.x1);
    if let Some(xx) = stretch(font, face, gid) {
        x0 = crate::font::outline::mul_fix(x0 as i64, xx) as f64;
        x1 = crate::font::outline::mul_fix(x1 as i64, xx) as f64;
    }
    let recip = 1.0 / f32::from(face.units_per_em);
    Rect::new(
        x0 as f32 * recip,
        b.y0 as f32 * recip,
        x1 as f32 * recip,
        b.y1 as f32 * recip,
    )
}

/// `fz_adjust_ft_glyph_width`: the horizontal scale, in 16.16, that fits
/// a substitute font's glyph to the width the PDF gives it.
fn stretch(font: &Font, face: &Face, gid: u32) -> Option<i64> {
    if !font.ft_stretch {
        return None;
    }
    let (table, default) = font.width_table.as_deref()?;
    let adv = face.advance(gid).unwrap_or(0);
    let realw = adv as f32 * 1000.0 / f32::from(face.units_per_em);
    let subw = table
        .get(gid as usize)
        .map_or(*default as f32, |&w| f32::from(w));
    (realw > 0.0 && subw > 0.0).then(|| ((subw / realw) * 65536.0) as i64)
}

/// `pdf_make_width_table`: the widest PDF width given each glyph, codes
/// taken through the encoding as MuPDF takes them. The walk over the
/// width ranges is bounded, as no real font needs more.
fn make_width_table(fd: &FontDesc) -> (Vec<i16>, i32) {
    const MAX_WALK: u64 = 1 << 24;
    let gids = || {
        fd.hmtx
            .iter()
            .flat_map(|h| (h.lo..=h.hi).map(move |k| (k, h.w)))
            .take(MAX_WALK as usize)
            .map(|(k, w)| (fd.cid_to_gid(fd.encoding.lookup(k as u32)), w))
    };
    let n = gids().map(|(g, _)| g).fold(0, i32::max);
    let count = n as usize + 1;
    let mut table = vec![-1i16; count];
    let default = fd.dhmtx.w;
    for (gid, w) in gids() {
        if let Some(slot) = usize::try_from(gid).ok().and_then(|g| table.get_mut(g)) {
            *slot = (w.max(i32::from(*slot))) as i16;
        }
    }
    for w in &mut table {
        if *w == -1 {
            *w = default as i16;
        }
    }
    (table, default)
}

/// `fz_new_base14_font`.
pub fn base14_font(name: &str) -> Option<Arc<Font>> {
    let data = font::base14(name)?;
    new_font(name.as_bytes(), Arc::from(data))
        .ok()
        .map(Arc::new)
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
        let mut font = new_font(name, Arc::from(data))?;
        font.is_serif = clean.contains("Times");
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
        font.is_mono = mono;
        font.is_serif = serif;
        font.is_bold = bold;
        font.is_italic = italic;
        font.ft_substitute = true;
        font.ft_stretch = true;
        Ok(font)
    }

    /// `pdf_load_substitute_cjk_font`: MuPDF's builtin CJK font, as far
    /// as its metrics (`font::cjk_metrics`). It keeps no glyph boxes, so
    /// its glyphs are bounded by the font's box.
    fn cjk_substitute(&self, name: &[u8]) -> Result<Font, String> {
        let mut font = new_font(name, font::cjk_metrics())?;
        font.glyph_boxes = GlyphBoxes::default();
        font.ft_substitute = true;
        font.ft_stretch = false;
        Ok(font)
    }

    /// `pdf_load_system_font`.
    fn system(&self, fd: &FontDesc, name: &[u8], collection: Option<&str>) -> Result<Font, String> {
        let has = |s: &[u8]| name.windows(s.len()).any(|w| w == s);
        let bold = has(b"Bold") || fd.flags & FD_FORCE_BOLD != 0;
        let italic = has(b"Italic") || has(b"Oblique") || fd.flags & FD_ITALIC != 0;
        let mono = fd.flags & FD_FIXED_PITCH != 0;
        let serif = fd.flags & FD_SERIF != 0;
        let cjk = || self.cjk_substitute(name);
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
                // pdf_dict_get_string: "" for what is no string; the C
                // string stops at a NUL, and fz_snprintf at 255 bytes.
                let s = |k: &[u8]| match doc.dict_get(&info, k) {
                    Object::String(s) => {
                        let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
                        s[..end].to_vec()
                    }
                    _ => Vec::new(),
                };
                let mut name = [s(b"Registry"), s(b"Ordering")].join(&b'-');
                name.truncate(255);
                String::from_utf8_lossy(&name).into_owned()
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
        let mut fd = self.load_kind(dict, resources)?;
        let table = make_width_table(&fd);
        Arc::make_mut(&mut fd.font).width_table = Some(Arc::new(table));
        Ok(fd)
    }

    fn load_kind(&self, dict: &Object, resources: Option<&Object>) -> Result<FontDesc, String> {
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
    //! Fonts loaded from PDFs built here. The expected glyphs, widths and
    //! Unicode are what MuPDF 1.28.6 shows for the same files (mutool run
    //! oracle/js/trace.js), and its warnings.

    use super::*;
    use crate::font::sfnt::tests::{
        base, cmap, fmt0, fmt4, post, post2, set, sfnt_at, ttf, without,
    };
    use crate::object::ObjRef;

    /// A stream object of `dict` entries and `data`.
    fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
        let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        out.extend_from_slice(data);
        out.extend_from_slice(b"\nendstream");
        out
    }

    /// A one-page PDF showing `content` with font 4 as /F1; objects 6 on
    /// are `extra`.
    fn pdf(font: &str, extra: &[Vec<u8>], content: &str) -> Vec<u8> {
        pdf_raw(font.as_bytes().to_vec(), extra, content)
    }

    fn pdf_raw(font: Vec<u8>, extra: &[Vec<u8>], content: &str) -> Vec<u8> {
        let mut objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_vec(),
            font,
            stream("", content.as_bytes()),
        ];
        objects.extend(extra.iter().cloned());
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n", i + 1).bytes());
            out.extend_from_slice(o);
            out.extend(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for o in offsets {
            out.extend(format!("{o:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        out
    }

    /// Text showing each code (hex) at 10 points.
    fn show(hex: &str) -> String {
        format!("BT /F1 10 Tf 10 100 Td <{hex}> Tj ET")
    }

    fn open(data: Vec<u8>) -> Document {
        Document::open(data).unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn load(doc: &Document) -> Result<FontDesc, String> {
        Loader { doc }.load(&Object::Ref(ObjRef::new(4, 0)), None)
    }

    fn loaded(data: Vec<u8>) -> (Document, FontDesc) {
        let doc = open(data);
        let fd = load(&doc).unwrap_or_else(|e| panic!("{e}"));
        (doc, fd)
    }

    /// The glyph, width and Unicode MuPDF's interpreter takes for a code.
    fn glyph(fd: &FontDesc, code: u32) -> (i32, i32, u32) {
        let cid = fd.encoding.lookup(code);
        let gid = fd.cid_to_gid(cid);
        let w = fd.lookup_hmtx(cid).w;
        let mut ucs = fd
            .to_unicode
            .as_ref()
            .map(|u| u.lookup_full(cid as u32))
            .unwrap_or_default();
        if ucs.len() == 1 && (ucs[0] < 32 || (127..160).contains(&ucs[0])) {
            ucs.clear();
        }
        if ucs.is_empty()
            && let Some(&u) = fd.cid_to_ucs.as_ref().and_then(|m| m.get(cid as usize))
        {
            ucs.push(i32::from(u));
        }
        let u = match ucs.first() {
            None | Some(0) => glyphs::REPLACEMENT,
            Some(&u) => u as u32,
        };
        (gid, w, u)
    }

    fn glyphs_of(fd: &FontDesc, codes: &[u32]) -> Vec<(i32, i32, u32)> {
        codes.iter().map(|&c| glyph(fd, c)).collect()
    }

    fn warnings(doc: &Document) -> Vec<String> {
        doc.diagnostics()
            .snapshot()
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

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

    // ---- simple fonts ----

    fn helvetica() -> Vec<u8> {
        pdf(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
            &[],
            &show("41422000"),
        )
    }

    fn times_widths() -> Vec<u8> {
        pdf(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Times-Bold /FirstChar 65 /LastChar 67 \
             /Widths [600 700 800] /Encoding << /BaseEncoding /WinAnsiEncoding /Differences [66 /Euro /bullet 128 /Aring] >> >>",
            &[],
            &show("41424344809f"),
        )
    }

    fn symbol() -> Vec<u8> {
        pdf(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Symbol >>",
            &[],
            &show("61622a"),
        )
    }

    fn substitute(flags: i32, name: &str) -> Vec<u8> {
        pdf(
            &format!(
                "<< /Type /Font /Subtype /Type1 /BaseFont /{name} /FirstChar 65 /LastChar 66 /Widths [500 900] \
                 /FontDescriptor 6 0 R >>"
            ),
            &[format!(
                "<< /Type /FontDescriptor /FontName /{name} /Flags {flags} /Ascent 700 /Descent -250 /MissingWidth 333 >>"
            )
            .into_bytes()],
            &show("414243"),
        )
    }

    /// A FontFile2 stream of a TrueType font.
    fn truetype_file(data: &[u8]) -> Vec<u8> {
        stream(&format!("/Length1 {}", data.len()), data)
    }

    /// A TrueType font of 6 glyphs 500 + 100 g wide, with `cmap_table`
    /// and glyph names .notdef A B space a Euro.
    fn tt_font(cmap_table: Option<Vec<u8>>) -> Vec<u8> {
        let names = post(
            0x2_0000,
            false,
            &post2(&[0, 36, 37, 3, 68, 258], &[b"Euro"]),
        );
        let t = match cmap_table {
            Some(c) => set(set(base(6, Some(c)), b"post", names), b"glyf", vec![0; 4]),
            None => set(without(base(6, None), b"cmap"), b"post", names),
        };
        ttf(&t).to_vec()
    }

    fn ffff() -> (u16, u16, i16, Option<Vec<u16>>) {
        (0xffff, 0xffff, 1, None)
    }

    /// A simple TrueType font dictionary of `flags`, embedding `font`.
    fn truetype(flags: i32, encoding: &str, font: Vec<u8>, content: &str) -> Vec<u8> {
        pdf(
            &format!(
                "<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+Test {encoding} /FontDescriptor 6 0 R >>"
            ),
            &[
                format!(
                    "<< /Type /FontDescriptor /FontName /Test /Flags {flags} /FontFile2 7 0 R >>"
                )
                .into_bytes(),
                truetype_file(&font),
            ],
            content,
        )
    }

    /// (3,1): A B space a Euro at their code points.
    fn unicode_cmap() -> Vec<u8> {
        cmap(&[(
            3,
            1,
            fmt4(&[
                (0x20, 0x20, -0x1d, None),
                (0x41, 0x42, -0x40, None),
                (0x61, 0x61, -0x5d, None),
                (0x20ac, 0x20ac, -0x20a7, None),
                ffff(),
            ]),
        )])
    }

    fn tt_unicode() -> Vec<u8> {
        truetype(
            32,
            "/Encoding /WinAnsiEncoding",
            tt_font(Some(unicode_cmap())),
            &show("414220618043"),
        )
    }

    fn tt_symbolic() -> Vec<u8> {
        let sym = cmap(&[
            (3, 0, fmt4(&[(0xf041, 0xf042, 0x0fc0, None), ffff()])),
            (3, 1, fmt4(&[(0x61, 0x61, -0x5d, None), ffff()])),
        ]);
        truetype(4, "", tt_font(Some(sym)), &show("41426143"))
    }

    fn tt_mac() -> Vec<u8> {
        let mac = cmap(&[(1, 0, fmt0(&[(0x41, 1), (0x42, 2), (0x20, 3), (0xdb, 5)]))]);
        truetype(
            32,
            "/Encoding << /BaseEncoding /WinAnsiEncoding /Differences [97 /Euro /a] >>",
            tt_font(Some(mac)),
            &show("4142206162"),
        )
    }

    fn tt_names_only() -> Vec<u8> {
        truetype(
            4,
            "/Encoding << /Differences [65 /B /A /Euro] >>",
            tt_font(None),
            &show("414243"),
        )
    }

    /// A simple font of `subtype` with a descriptor embedding `file`
    /// under `key`.
    fn embedded(
        subtype: &str,
        flags: i32,
        key: &str,
        file: Vec<u8>,
        rest: &str,
        content: &str,
    ) -> Vec<u8> {
        pdf(
            &format!(
                "<< /Type /Font /Subtype /{subtype} /BaseFont /Embedded {rest} /FontDescriptor 6 0 R >>"
            ),
            &[
                format!(
                    "<< /Type /FontDescriptor /FontName /Embedded /Flags {flags} /{key} 7 0 R >>"
                )
                .into_bytes(),
                file,
            ],
            content,
        )
    }

    fn type1_file() -> Vec<u8> {
        let glyphs = [
            (".notdef", crate::font::tests::t1_box(250, 0, 0, 0, 0)),
            ("A", crate::font::tests::t1_box(600, 10, 0, 590, 700)),
            ("g1", crate::font::tests::t1_box(650, 20, 0, 600, 700)),
        ];
        let array = "256 array 0 1 255 {1 index exch /.notdef put} for dup 65 /A put dup 66 /g1 put readonly def";
        let data = crate::font::tests::type1(Some(array), &glyphs);
        stream(
            &format!("/Length1 {} /Length2 0 /Length3 0", data.len()),
            &data,
        )
    }

    fn t1_embedded_symbolic() -> Vec<u8> {
        embedded("Type1", 4, "FontFile", type1_file(), "", &show("414243"))
    }

    fn t1_embedded_differences() -> Vec<u8> {
        embedded(
            "Type1",
            32,
            "FontFile",
            type1_file(),
            "/Encoding << /Differences [66 /A /g1] >>",
            &show("414243"),
        )
    }

    fn cff_embedded() -> Vec<u8> {
        let cff = crate::font::base14("Helvetica").unwrap_or_default();
        embedded(
            "Type1",
            32,
            "FontFile3",
            stream("/Subtype /Type1C", cff),
            "/Encoding /MacRomanEncoding",
            &show("4142a5"),
        )
    }

    fn otto_embedded() -> Vec<u8> {
        let cff = crate::font::base14("Courier").unwrap_or_default();
        let t = set(base(4, None), b"CFF ", cff.to_vec());
        let otto = sfnt_at(0, b"OTTO", &t);
        embedded(
            "Type1",
            32,
            "FontFile3",
            stream("/Subtype /OpenType", &otto),
            "",
            &show("4142"),
        )
    }

    fn otto_broken() -> Vec<u8> {
        let mut otto = sfnt_at(0, b"OTTO", &set(base(4, None), b"CFF ", vec![1; 8]));
        otto[4..6].copy_from_slice(&99u16.to_be_bytes());
        embedded(
            "TrueType",
            32,
            "FontFile3",
            stream("/Subtype /OpenType", &otto),
            "",
            &show("41"),
        )
    }

    fn broken_embedded() -> Vec<u8> {
        pdf(
            "<< /Type /Font /Subtype /TrueType /BaseFont /Arial,Bold /FontDescriptor 6 0 R >>",
            &[
                b"<< /Type /FontDescriptor /FontName /Arial,Bold /Flags 32 /FontFile2 7 0 R /Ascent 9000 /Descent 100 >>".to_vec(),
                stream("", b"not a font at all"),
            ],
            &show("4142"),
        )
    }

    fn not_a_stream() -> Vec<u8> {
        pdf(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Courier-Oblique /FontDescriptor 6 0 R >>",
            &[
                b"<< /Type /FontDescriptor /Flags 32 /FontFile 7 0 R /Ascent 0 /Descent 0 >>"
                    .to_vec(),
                b"<< /Not /AStream >>".to_vec(),
            ],
            &show("41"),
        )
    }

    fn to_unicode() -> Vec<u8> {
        let cmap = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n\
            /CMapName /Test def 1 begincodespacerange <00> <FF> endcodespacerange\n\
            2 beginbfchar <41> <0058> <42> <00660066> endbfchar\n\
            1 beginbfrange <43> <44> <0061> endbfrange\n\
            endcmap CMapName currentdict /CMap defineresource pop end end";
        pdf(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode 6 0 R /FirstChar 30 /LastChar 20 /Widths [1 2 3] >>",
            &[stream("", cmap)],
            &show("4142434445"),
        )
    }

    fn to_unicode_broken() -> Vec<u8> {
        pdf(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode 6 0 R >>",
            &[stream("/Filter /FlateDecode", b"garbage")],
            &show("41"),
        )
    }

    fn s22pdf() -> Vec<u8> {
        let mut font = b"<< /Type /Font /Subtype /TrueType /BaseFont /".to_vec();
        font.extend(b"\xCB\xCE\xCC\xE5");
        font.extend(b" /Encoding /WinAnsiEncoding /FontDescriptor 6 0 R /FirstChar 32 /LastChar 33 /Widths [500 600] >>");
        let objects = [b"<< /Type /FontDescriptor /Flags 4 >>".to_vec()];
        pdf_raw(font, &objects, &show("b0a1b0a2"))
    }

    fn guess(dict: &str, extra: &[Vec<u8>]) -> Vec<u8> {
        pdf(dict, extra, &show("41"))
    }

    // ---- Type 3 ----

    fn type3(rest: &str) -> Vec<u8> {
        pdf(
            &format!(
                "<< /Type /Font /Subtype /Type3 /FontMatrix [0.001 0 0 0.001 0 0] /FontBBox [0 0 750 750] \
                 /FirstChar 65 /LastChar 67 /Widths [1000 500 6 0 R] {rest} >>"
            ),
            &[
                b"750".to_vec(),
                stream("", b"750 0 0 0 750 750 d1 0 0 750 750 re f"),
                stream("", b"500 0 d0 0 0 m 500 500 l S"),
            ],
            &show("41424344"),
        )
    }

    fn type3_full() -> Vec<u8> {
        type3(
            "/Name /T3 /Encoding << /Differences [65 /square /line /none] >> /CharProcs << /square 7 0 R /line 8 0 R /none 9 0 R >> /Resources << >>",
        )
    }

    // ---- Type 0 ----

    /// A Type 0 font of `encoding` over a CID font (object 6) of
    /// `subtype` and `cid` entries, with a descriptor (7) of `desc`;
    /// objects 8 on are `extra`.
    fn type0(
        encoding: &str,
        subtype: &str,
        cid: &str,
        desc: &str,
        extra: &[Vec<u8>],
        content: &str,
    ) -> Vec<u8> {
        let mut objects = vec![
            format!("<< /Type /Font /Subtype /{subtype} {cid} >>").into_bytes(),
            format!("<< /Type /FontDescriptor {desc} >>").into_bytes(),
        ];
        objects.extend(extra.iter().cloned());
        pdf(
            &format!(
                "<< /Type /Font /Subtype /Type0 /BaseFont /T0 {encoding} /DescendantFonts [6 0 R] >>"
            ),
            &objects,
            content,
        )
    }

    const IDENTITY: &str =
        "/CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >>";
    const JAPAN: &str = "/CIDSystemInfo << /Registry (Adobe) /Ordering (Japan1) /Supplement 6 >>";

    fn t0_embedded(cidtogid: &str) -> Vec<u8> {
        let map: Vec<u8> = [0u16, 2, 1, 3, 4]
            .iter()
            .flat_map(|g| g.to_be_bytes())
            .collect();
        type0(
            "/Encoding /Identity-H",
            "CIDFontType2",
            &format!(
                "/BaseFont /Embedded {IDENTITY} /FontDescriptor 7 0 R /W [1 [400 500] 3 4 600 5 [10 0 R]] /DW 900 {cidtogid}"
            ),
            "/Flags 4 /FontFile2 8 0 R",
            &[
                truetype_file(&tt_font(Some(unicode_cmap()))),
                stream("", &map),
                b"777".to_vec(),
            ],
            &show("000100020003000400050006"),
        )
    }

    fn t0_vertical() -> Vec<u8> {
        type0(
            "/Encoding /Identity-V",
            "CIDFontType0",
            &format!(
                "/BaseFont /MS-Mincho {JAPAN} /FontDescriptor 7 0 R /W [1 2 500] /W2 [1 [-1000 250 880] 3 4 -900 300 800] /DW2 [880 -1100]"
            ),
            "/Flags 4",
            &[],
            &show("0001000200030004"),
        )
    }

    fn t0_vertical_japan() -> Vec<u8> {
        type0(
            "/Encoding /UniJIS-UCS2-V",
            "CIDFontType0",
            &format!("/BaseFont /KozMinPro-Regular {JAPAN} /FontDescriptor 7 0 R"),
            "/Flags 4",
            &[],
            &show("30013002"),
        )
    }

    fn t0_substitute(base_font: &str, info: &str, extra_entries: &str) -> Vec<u8> {
        type0(
            &format!("/Encoding /Identity-H {extra_entries}"),
            "CIDFontType2",
            &format!("/BaseFont /{base_font} {info} /FontDescriptor 7 0 R /W [36 [700]]"),
            "/Flags 32",
            &[],
            &show("00240025"),
        )
    }

    fn t0_embedded_cmap() -> Vec<u8> {
        let cmap = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n\
            /CMapName /Test-H def /WMode 0 def /UseCMap /Identity-H usecmap\n\
            1 begincodespacerange <00> <FF> endcodespacerange\n\
            1 begincidrange <41> <42> 1 endcidrange\n\
            endcmap CMapName currentdict /CMap defineresource pop end end";
        let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n\
            1 begincodespacerange <00> <FF> endcodespacerange\n\
            2 beginbfchar <41> <0058> <42> <00590059> endbfchar\n\
            endcmap CMapName currentdict /CMap defineresource pop end end";
        type0(
            "/Encoding 8 0 R /ToUnicode 9 0 R",
            "CIDFontType2",
            &format!("/BaseFont /Embedded {IDENTITY} /FontDescriptor 7 0 R /CIDToGIDMap /Identity"),
            "/Flags 4 /FontFile2 10 0 R",
            &[
                stream("/Type /CMap /CMapName /Test-H", cmap),
                stream("", to_unicode),
                truetype_file(&tt_font(Some(unicode_cmap()))),
            ],
            &show("4142"),
        )
    }

    const FFFD: u32 = glyphs::REPLACEMENT;

    #[test]
    fn base_14_fonts_load_from_the_bundled_programs() {
        let (_, fd) = loaded(helvetica());
        assert_eq!(fd.font.name, b"Helvetica");
        assert!(!fd.is_embedded && !fd.font.ft_substitute && !fd.font.is_serif);
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42, 0x20]),
            [(34, 667, 65), (35, 667, 66), (1, 278, 32)]
        );
        assert_eq!(glyph(&fd, 0).2, FFFD);
        // Widths and an encoding of differences over WinAnsi.
        let (_, fd) = loaded(times_widths());
        assert!(fd.font.is_serif && fd.font.is_bold);
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42, 0x43, 0x44, 0x80, 0x9f]),
            [
                (34, 600, 65),
                (263, 700, 0x20ac),
                (116, 800, 0x2022),
                (37, 0, 68),
                (175, 0, 0xc5),
                (198, 0, 0x178)
            ]
        );
        // Symbol is symbolic: its own encoding, no standard one.
        let (_, fd) = loaded(symbol());
        assert_ne!(fd.flags & FD_SYMBOLIC, 0);
        assert_eq!(
            glyphs_of(&fd, &[0x61, 0x62, 0x2a]),
            [(66, 631, 0x3b1), (67, 549, 0x3b2), (11, 500, 0x2217)]
        );
        // A dictionary without a subtype is guessed simple.
        let (doc, fd) = loaded(guess("<< /Type /Font /BaseFont /Courier >>", &[]));
        assert_eq!(glyph(&fd, 0x41), (34, 600, 65));
        assert_eq!(
            warnings(&doc),
            ["unknown font format, guessing type1 or truetype."]
        );
    }

    #[test]
    fn fonts_without_programs_take_a_substitute_by_their_flags() {
        let cases = [
            (
                2 | 32 | 64 | (1 << 18),
                "Garamond",
                (false, true, true, true),
            ),
            (1 | 32, "Consolas", (true, false, false, false)),
            (32, "Frutiger-BoldOblique", (false, false, true, true)),
        ];
        for (flags, name, style) in cases {
            let (_, fd) = loaded(substitute(flags, name));
            let f = &fd.font;
            assert_eq!(
                (f.is_mono, f.is_serif, f.is_bold, f.is_italic),
                style,
                "{name}"
            );
            assert!(f.ft_substitute && f.ft_stretch);
            assert_eq!(f.name, name.as_bytes());
            // The PDF's widths, and the missing width past them.
            assert_eq!(
                glyphs_of(&fd, &[0x41, 0x42, 0x43]),
                [(34, 500, 65), (35, 900, 66), (36, 333, 67)]
            );
            assert_eq!(
                (fd.ascent, fd.descent, f.ascender, f.descender),
                (700.0, -250.0, 0.7, -0.25)
            );
            // Glyphs are stretched to those widths.
            assert_eq!(advance_glyph(f, 35, 0), 0.9);
            assert_eq!(advance_glyph(f, 900, 0), 0.333);
            assert_eq!(advance_glyph(f, -1, 0), 0.333);
            let b = bound_glyph(f, 34, Matrix::IDENTITY);
            assert!(b.x1 > b.x0 && b.x1 <= 0.55);
        }
    }

    #[test]
    fn embedded_truetype_fonts_pick_their_cmap_as_mupdf_does() {
        // Non-symbolic with a (3,1) map: glyphs by the names' code points.
        let (_, fd) = loaded(tt_unicode());
        assert!(fd.is_embedded);
        assert_eq!(fd.font.name, b"ABCDEF+Test");
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42, 0x20, 0x61, 0x80, 0x43]),
            [
                (1, 600, 65),
                (2, 700, 66),
                (3, 800, 32),
                (4, 900, 97),
                (5, 1000, 0x20ac),
                (0, 500, 67)
            ]
        );
        // The font's metrics, as the descriptor gives none.
        assert_eq!((fd.ascent, fd.descent), (800.0, -200.0));
        // Symbolic: the (3,0) map, codes in the symbol area.
        let (_, fd) = loaded(tt_symbolic());
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42, 0x61, 0x43]),
            [(1, 600, 65), (2, 700, 66), (0, 500, FFFD), (0, 500, FFFD)]
        );
        // A Macintosh map: names through MacRoman codes, then glyph names.
        let (_, fd) = loaded(tt_mac());
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42, 0x20, 0x61, 0x62]),
            [
                (1, 600, 65),
                (2, 700, 66),
                (3, 800, 32),
                (5, 1000, 0x20ac),
                (4, 900, 97)
            ]
        );
        // No map at all: glyph names only.
        let (doc, fd) = loaded(tt_names_only());
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42, 0x43]),
            [(2, 700, 66), (1, 600, 65), (5, 1000, 0x20ac)]
        );
        assert_eq!(warnings(&doc), ["freetype could not find any cmaps"]);
    }

    #[test]
    fn embedded_type1_and_cff_fonts_encode_by_glyph_name() {
        // Symbolic, with its own encoding and private names.
        let (_, fd) = loaded(t1_embedded_symbolic());
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42, 0x43]),
            [(1, 600, 65), (2, 650, FFFD), (0, 250, FFFD)]
        );
        let (_, fd) = loaded(t1_embedded_differences());
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42, 0x43]),
            [(1, 600, 65), (1, 600, 65), (2, 650, FFFD)]
        );
        assert!(fd.font.is_bold && fd.font.is_italic && fd.font.is_mono);
        // A bare CFF program, and one in an OpenType wrapper.
        let (_, fd) = loaded(cff_embedded());
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42, 0xa5]),
            [(34, 667, 65), (35, 667, 66), (116, 350, 0x2022)]
        );
        let (_, fd) = loaded(otto_embedded());
        assert!(fd.is_embedded && fd.font.is_mono);
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42]),
            [(34, 600, 65), (35, 600, 66)]
        );
    }

    #[test]
    fn broken_programs_fall_back_with_mupdfs_warnings() {
        let (doc, fd) = loaded(otto_broken());
        assert!(!fd.is_embedded && fd.font.ft_substitute);
        assert_eq!(glyph(&fd, 0x41).0, 34);
        assert_eq!(
            warnings(&doc),
            [
                "syntax error: invalid TTF header",
                "ignored error when loading embedded font; attempting to load system font"
            ]
        );
        // A standard name falls back to its builtin font; bogus metrics
        // give the default ascender.
        let (doc, fd) = loaded(broken_embedded());
        assert!(!fd.font.ft_substitute && fd.font.is_bold);
        assert_eq!(glyphs_of(&fd, &[0x41]), [(34, 722, 65)]);
        assert_eq!(
            (fd.ascent, fd.descent, fd.font.ascender, fd.font.descender),
            (9000.0, -100.0, 0.8, -0.2)
        );
        assert_eq!(
            warnings(&doc),
            [
                "library error: FT_New_Memory_Face(Arial,Bold): unknown file format",
                "ignored error when loading embedded font; attempting to load system font",
                "bogus font (Arial,Bold) ascent/descent values (9000 / -100)",
            ]
        );
        // A font file that is no stream; zero metrics are not bogus.
        let (doc, fd) = loaded(not_a_stream());
        assert_eq!((fd.font.ascender, fd.font.descender), (0.8, -0.2));
        assert_eq!(glyph(&fd, 0x41).0, 34);
        assert_eq!(
            warnings(&doc),
            [
                "format error: object is not a stream",
                "ignored error when loading embedded font; attempting to load system font"
            ]
        );
        // An OpenType wrapper whose CFF runs past the data.
        let mut otto = sfnt_at(0, b"OTTO", &set(base(4, None), b"CFF ", vec![1; 8]));
        // The CFF table's record, the ninth.
        let n = 12 + 16 * 8;
        otto[n + 12..n + 16].copy_from_slice(&0xffffu32.to_be_bytes());
        let doc = open(embedded(
            "Type1",
            32,
            "FontFile3",
            stream("/Subtype /OpenType", &otto),
            "",
            &show("41"),
        ));
        assert!(load(&doc).is_ok());
        assert!(
            warnings(&doc)
                .contains(&"syntax error: invalid TTF subtable offset/length".to_string())
        );
        // One without CFF is opened as it is, and fails.
        let otto = sfnt_at(0, b"OTTO", &base(4, None));
        let doc = open(embedded(
            "Type1",
            32,
            "FontFile3",
            stream("/Subtype /OpenType", &otto),
            "",
            &show("41"),
        ));
        assert!(load(&doc).is_ok());
        assert!(warnings(&doc).contains(
            &"library error: FT_New_Memory_Face(Embedded): SFNT font table missing".to_string()
        ));
    }

    #[test]
    fn to_unicode_cmaps_override_the_encodings_names() {
        let (_, fd) = loaded(to_unicode());
        // FirstChar past LastChar: one width, for code 0.
        assert_eq!(fd.lookup_hmtx(0).w, 1);
        let u: Vec<u32> = glyphs_of(&fd, &[0x41, 0x42, 0x43, 0x44, 0x45])
            .iter()
            .map(|g| g.2)
            .collect();
        assert_eq!(u, [0x58, 0x66, 0x61, 0x62, 0x45]);
        assert_eq!(
            fd.to_unicode.as_ref().map(|t| t.lookup_full(0x42)),
            Some(vec![0x66, 0x66])
        );
        // A stream that inflates to nothing is an empty map.
        let (doc, fd) = loaded(to_unicode_broken());
        assert_eq!(glyph(&fd, 0x41), (34, 667, 65));
        assert_eq!(
            warnings(&doc),
            [
                "library error: zlib error: incorrect header check",
                "read error; treating as end of file"
            ]
        );
        // A ToUnicode that is no CMap.
        let doc = open(pdf(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode /NoSuchCMap >>",
            &[],
            &show("41"),
        ));
        let fd = load(&doc).unwrap_or_else(|e| panic!("{e}"));
        // MuPDF gives up on the names too.
        assert_eq!(glyph(&fd, 0x41).2, FFFD);
        assert!(warnings(&doc).contains(&"cannot load ToUnicode CMap".to_string()));
    }

    #[test]
    fn s22pdf_chinese_fonts_are_read_as_gbk() {
        let (doc, fd) = loaded(s22pdf());
        assert_eq!(fd.font.name, b"SimSun,Regular");
        assert_eq!(fd.encoding.name, "GBK-EUC-H");
        // No width for those CIDs: the missing width, 0.
        let w_u = |code| {
            let (_, w, u) = glyph(&fd, code);
            (w, u)
        };
        assert_eq!([w_u(0xb0a1), w_u(0xb0a2)], [(0, 0x554a), (0, 0x963f)]);
        assert_eq!(
            warnings(&doc),
            ["workaround for S22PDF lying about chinese font encodings"]
        );
    }

    #[test]
    fn type3_fonts_keep_their_procedures_widths_and_matrix() {
        let (_, fd) = loaded(type3_full());
        let t3 = fd.font.t3.as_ref().unwrap_or_else(|| panic!("no type 3"));
        assert_eq!(fd.font.name, b"T3");
        assert_eq!(t3.matrix.a, 0.001);
        let b = fd.font.bbox;
        assert!(
            b.x0 == 0.0 && b.y0 == 0.0 && (b.x1 - 0.75).abs() < 1e-6 && (b.y1 - 0.75).abs() < 1e-6
        );
        assert!(t3.procs[65].is_some() && t3.procs[66].is_some() && t3.procs[67].is_none());
        assert!(t3.resources.is_some());
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42, 0x43, 0x44]),
            [
                (65, 1000, 0x25a1),
                (66, 500, 66),
                (67, 750, 67),
                (68, 0, 68)
            ]
        );
        assert_eq!(
            (
                advance_glyph(&fd.font, 66, 0),
                advance_glyph(&fd.font, 300, 0)
            ),
            (0.5, 0.0)
        );
        // Glyph boxes are what the interpreter found, else the font's.
        assert_eq!(
            bound_glyph(&fd.font, 65, Matrix::IDENTITY),
            Rect::new(0.0, 0.0, 0.000_000_1, 0.000_000_1)
        );
        assert_eq!(bound_glyph(&fd.font, 400, Matrix::IDENTITY), fd.font.bbox);
        let mut font = Font::clone(&fd.font);
        if let Some(t3) = font.t3.as_mut() {
            t3.bboxes[65] = Rect::new(0.0, 0.0, 0.5, 0.5);
        }
        assert_eq!(
            bound_glyph(&font, 65, Matrix::scale(2.0, 2.0)),
            Rect::new(0.0, 0.0, 1.0, 1.0)
        );
        // Without its own resources a font borrows the page's, else
        // warns; without a name it is named for its object.
        let doc = open(type3(
            "/Encoding << /Differences [65 /square] >> /CharProcs << /square 7 0 R >>",
        ));
        let page = Object::Dict(Default::default());
        let fd = Loader { doc: &doc }.load(&Object::Ref(ObjRef::new(4, 0)), Some(&page));
        assert!(fd.is_ok_and(|fd| fd.font.t3.as_ref().is_some_and(|t| t.resources.is_some())));
        assert!(warnings(&doc).is_empty());
        let (doc, fd) = loaded(type3(
            "/Encoding << /Differences [65 /square] >> /CharProcs << /square 7 0 R >>",
        ));
        assert_eq!(fd.font.name, b"Type3 (4 0 R)");
        assert_eq!(warnings(&doc), ["no resource dictionary for type 3 font!"]);
        // Guessed from its CharProcs.
        let (doc, fd) = loaded(guess(
            "<< /Type /Font /FontMatrix [0.001 0 0 0.001 0 0] /Encoding << /Differences [65 /a] >> /CharProcs << /a 6 0 R >> /FirstChar 65 /LastChar 65 /Widths [500] >>",
            &[stream("", b"500 0 d0")],
        ));
        assert_eq!(glyph(&fd, 0x41), (65, 500, 97));
        // An empty box is invalid; the identity matrix is the default.
        assert!(fd.font.invalid_bbox && fd.font.bbox.is_empty());
        assert_eq!(
            warnings(&doc).first().map(String::as_str),
            Some("unknown font format, guessing type3.")
        );
    }

    #[test]
    fn type3_fonts_need_an_encoding_widths_and_procedures() {
        let err = |rest: &str| {
            let doc = open(pdf(
                &format!("<< /Type /Font /Subtype /Type3 {rest} >>"),
                &[],
                &show("41"),
            ));
            load(&doc).err()
        };
        assert_eq!(
            err("").as_deref(),
            Some("syntax error: Type3 font missing Encoding")
        );
        assert_eq!(
            err("/Encoding /WinAnsiEncoding").as_deref(),
            Some("syntax error: Type3 font missing Widths")
        );
        assert_eq!(
            err("/Encoding /WinAnsiEncoding /Widths [] /FirstChar 300 /LastChar 2").as_deref(),
            Some("syntax error: Type3 font missing CharProcs")
        );
    }

    #[test]
    fn cid_fonts_map_cids_to_glyphs_and_widths() {
        // A CIDToGIDMap stream; W in both forms, an indirect width and DW.
        let (_, fd) = loaded(t0_embedded("/CIDToGIDMap 9 0 R"));
        assert!(fd.is_embedded);
        assert_eq!(
            glyphs_of(&fd, &[1, 2, 3, 4, 5, 6]),
            [
                (2, 400, FFFD),
                (1, 500, FFFD),
                (3, 600, FFFD),
                (4, 600, FFFD),
                (5, 777, FFFD),
                (6, 900, FFFD)
            ]
        );
        let (_, fd) = loaded(t0_embedded("/CIDToGIDMap /Identity"));
        assert_eq!(glyphs_of(&fd, &[1, 2]), [(1, 400, FFFD), (2, 500, FFFD)]);
        let (doc, fd) = loaded(t0_embedded("/CIDToGIDMap /Foo"));
        assert_eq!(glyph(&fd, 1).0, 1);
        assert_eq!(warnings(&doc), ["ignoring unknown CIDToGIDMap entry"]);
        // An embedded CMap over Identity-H, and a ToUnicode by code.
        let (_, fd) = loaded(t0_embedded_cmap());
        assert_eq!(fd.encoding.name, "Test-H");
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42, 0x0105]),
            [(1, 1000, 0x58), (2, 1000, 0x59), (0x105, 1000, FFFD)]
        );
        assert_eq!(
            fd.to_unicode.as_ref().map(|t| t.lookup_full(2)),
            Some(vec![0x59, 0x59])
        );
    }

    #[test]
    fn substitute_cid_fonts_map_through_unicode() {
        let (doc, fd) = loaded(t0_substitute("Arial-BoldMT", IDENTITY, ""));
        assert!(fd.font.ft_substitute && fd.font.is_bold && fd.font.ft_stretch);
        assert_eq!(
            glyphs_of(&fd, &[0x24, 0x25]),
            [(34, 700, 65), (35, 1000, 66)]
        );
        assert_eq!(advance_glyph(&fd.font, 34, 0), 0.7);
        assert_eq!(
            warnings(&doc),
            ["non-embedded font using identity encoding: Arial-BoldMT (mapping via TrueType-UCS2)"]
        );
        let (doc, fd) = loaded(t0_substitute(
            "Arial-BoldMT",
            IDENTITY,
            "/ToUnicode /Identity-H",
        ));
        assert_eq!(
            glyphs_of(&fd, &[0x24, 0x25]),
            [(5, 700, 0x24), (6, 1000, 0x25)]
        );
        assert_eq!(
            warnings(&doc),
            ["non-embedded font using identity encoding: Arial-BoldMT (mapping via Identity-H)"]
        );
        let (doc, _) = loaded(t0_substitute("Verdana", "", ""));
        assert_eq!(
            warnings(&doc).first().map(String::as_str),
            Some("CIDFont is missing CIDSystemInfo dictionary; assuming Adobe-Identity")
        );
        let (doc, fd) = loaded(t0_substitute(
            "Verdana",
            "/CIDSystemInfo << /Registry (Foo) /Ordering (Bar) >>",
            "",
        ));
        assert_eq!(glyph(&fd, 0x24).0, 34);
        assert!(warnings(&doc).contains(&"unknown cid collection: Foo-Bar".to_string()));
        // A known CJK name takes the CJK font, whose glyphs nupdf lacks.
        let (_, fd) = loaded(t0_substitute("SimSun", IDENTITY, ""));
        assert!(fd.font.ft_substitute && !fd.font.ft_stretch);
        assert_eq!(glyphs_of(&fd, &[0x24]), [(0, 700, 65)]);
        assert_eq!(bound_glyph(&fd.font, 0, Matrix::IDENTITY), fd.font.bbox);
        // Guessed from its descendant.
        let (doc, fd) = loaded(guess(
            "<< /Type /Font /Encoding /Identity-H /DescendantFonts [6 0 R] >>",
            &[
                format!("<< /Type /Font /Subtype /CIDFontType2 /BaseFont /Helvetica {IDENTITY} /FontDescriptor 7 0 R >>").into_bytes(),
                b"<< /Flags 32 >>".to_vec(),
            ],
        ));
        assert!(!fd.font.ft_stretch);
        assert_eq!(
            warnings(&doc).first().map(String::as_str),
            Some("unknown font format, guessing type0.")
        );
    }

    #[test]
    fn vertical_cid_fonts_take_w2_and_dw2() {
        let (_, fd) = loaded(t0_vertical());
        assert_eq!(fd.wmode, 1);
        let v = |cid| {
            let v = fd.lookup_vmtx(cid);
            (v.x, v.y, v.w)
        };
        assert_eq!(
            [v(1), v(2), v(3), v(4)],
            [
                (250, 880, -1000),
                (250, 880, -1100),
                (300, 800, -900),
                (300, 800, -900)
            ]
        );
        let u: Vec<u32> = glyphs_of(&fd, &[1, 2, 3]).iter().map(|g| g.2).collect();
        assert_eq!(u, [0x20, 0x21, 0x22]);
        assert_eq!(fd.font.ascender, 0.8);
        assert_eq!(advance_glyph(&fd.font, 0, 1), (1151.0 + 286.0) / 1000.0);
        // Punctuation takes its vertical form to find a glyph.
        let (_, fd) = loaded(t0_vertical_japan());
        assert_eq!(fd.encoding.name, "UniJIS-UCS2-V");
        let u: Vec<u32> = glyphs_of(&fd, &[0x3001, 0x3002])
            .iter()
            .map(|g| g.2)
            .collect();
        assert_eq!(u, [0x3001, 0x3002]);
    }

    #[test]
    fn type0_fonts_fail_without_their_parts() {
        let err = |font: &str, extra: &[Vec<u8>]| load(&open(pdf(font, extra, &show("41")))).err();
        assert_eq!(
            err(
                "<< /Type /Font /Subtype /Type0 /Encoding /Identity-H >>",
                &[]
            )
            .as_deref(),
            Some("syntax error: cid font is missing descendant fonts")
        );
        let cid = |subtype: &str, rest: &str| {
            format!("<< /Type /Font /Subtype /{subtype} {IDENTITY} {rest} >>").into_bytes()
        };
        let t0 = "<< /Type /Font /Subtype /Type0 /Encoding /Identity-H /DescendantFonts [6 0 R] >>";
        assert_eq!(
            err(t0, &[cid("Type1", "")]).as_deref(),
            Some("syntax error: unknown cid font type")
        );
        assert_eq!(
            err(t0, &[cid("CIDFontType0", "")]).as_deref(),
            Some("syntax error: missing font descriptor")
        );
        assert_eq!(
            err(
                "<< /Type /Font /Subtype /Type0 /DescendantFonts [6 0 R] >>",
                &[cid("CIDFontType0", "")]
            )
            .as_deref(),
            Some("syntax error: font missing encoding")
        );
        // The hail mary font is Times.
        let doc = open(helvetica());
        let fd = Loader { doc: &doc }
            .hail_mary()
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            (fd.font.name.as_slice(), fd.font.is_serif),
            (&b""[..], true)
        );
        assert_eq!(glyph(&fd, 0x41).0, 34);
    }

    #[test]
    fn width_tables_take_the_widest_width_of_each_glyph() {
        let (_, fd) = loaded(t0_substitute("Arial-BoldMT", IDENTITY, ""));
        let (table, default) = make_width_table(&fd);
        assert_eq!((table.get(34), default), (Some(&700), 1000));
        assert!(table.iter().all(|&w| w == 700 || w == 1000));
        // A copy for another thread has a font of its own.
        let copy = fd.copy_for_thread();
        assert!(!Arc::ptr_eq(&copy.font, &fd.font));
        assert_eq!(
            format!("{:?}", GlyphBoxes::new(300)),
            "GlyphBoxes(2 blocks)"
        );
    }

    #[test]
    fn mupdf_names_glyph_codes_and_numbers_as_it_prints_them() {
        assert_eq!(mac_roman_code(b"notequal"), Some(173));
        assert_eq!(mac_roman_code(b"Adieresis"), Some(128));
        assert_eq!(mac_roman_code(b"nonesuch"), None);
        assert_eq!(
            ft_error_string("Locations_Missing"),
            "locations (loca) table missing"
        );
        assert_eq!(ft_error_string("Invalid_File_Format"), "broken file");
        assert_eq!(ucs2_cmap("Adobe-Japan2", true), Some("Adobe-Japan2-UCS2"));
        assert_eq!(ucs2_cmap("Adobe-Japan2", false), None);
        assert_eq!(ucs2_cmap("Adobe-CNS1", false), Some("Adobe-CNS1-UCS2"));
        assert_eq!(ucs2_cmap("Adobe-Korea1", false), Some("Adobe-Korea1-UCS2"));
    }

    /// A non-symbolic TrueType font of private glyph names, mapped by a
    /// (3,1) cmap and by name: `mything`, `uni0061` and an empty name.
    fn tt_private_names() -> Vec<u8> {
        let names = post(
            0x2_0000,
            false,
            &post2(&[0, 258, 259, 260, 68], &[b"mything", b"", b"other"]),
        );
        let map = cmap(&[(
            3,
            1,
            fmt4(&[(0x41, 0x43, -0x40, None), (0x61, 0x61, -0x5d, None), ffff()]),
        )]);
        let mut t = set(base(5, Some(map)), b"post", names);
        // A box with no area.
        t = set(
            t,
            b"head",
            crate::font::sfnt::tests::head(1000)[..36]
                .iter()
                .copied()
                .chain([0; 18])
                .collect(),
        );
        truetype(
            32,
            "/Encoding << /Differences [68 /mything /a.alt /other 1.5 /x / 255 /a /b] >>",
            ttf(&t).to_vec(),
            &show("4142434445464748ff"),
        )
    }

    fn t1_private_symbolic() -> Vec<u8> {
        let glyphs = [
            (".notdef", crate::font::tests::t1_box(250, 0, 0, 0, 0)),
            ("xyz1", crate::font::tests::t1_box(600, 10, 0, 590, 700)),
        ];
        let array =
            "256 array 0 1 255 {1 index exch /.notdef put} for dup 66 /xyz1 put readonly def";
        let data = crate::font::tests::type1(Some(array), &glyphs);
        let file = stream(&format!("/Length1 {}", data.len()), &data);
        embedded(
            "Type1",
            4,
            "FontFile",
            file,
            "/Encoding [1 2 3]",
            &show("42"),
        )
    }

    /// A TrueType font without glyph names (`post` format 3).
    fn tt_unnamed() -> Vec<u8> {
        let t = base(6, Some(unicode_cmap()));
        truetype(32, "", ttf(&t).to_vec(), &show("41422061"))
    }

    #[test]
    fn unnamed_truetype_glyphs_take_the_winansi_names() {
        let (_, fd) = loaded(tt_unnamed());
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42, 0x20, 0x61]),
            [(1, 600, 65), (2, 700, 66), (3, 800, 32), (4, 900, 97)]
        );
    }

    #[test]
    fn truetype_names_fall_back_to_glyph_names_and_code_points() {
        let (_, fd) = loaded(tt_private_names());
        assert_eq!(
            glyphs_of(&fd, &[0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0xff]),
            [
                (1, 600, FFFD),
                (2, 700, FFFD),
                (3, 800, FFFD),
                (1, 600, FFFD),
                (4, 900, 0x61),
                (3, 800, FFFD),
                (0, 500, 0x78),
                // The empty name is glyph 2's.
                (2, 700, FFFD),
                (4, 900, 0x61),
            ]
        );
        // The head's empty box is invalid: the unit square instead.
        assert!(fd.font.invalid_bbox);
        assert_eq!(fd.font.bbox, Rect::UNIT);
        assert_eq!(bound_glyph(&fd.font, -1, Matrix::IDENTITY), Rect::UNIT);
        // A symbolic Type 1 font's private names keep no Unicode: MuPDF
        // would read them as the standard encoding's, but its test of the
        // name (no code point) never passes, as unknown names give U+FFFD.
        let (_, fd) = loaded(t1_private_symbolic());
        assert_eq!(glyph(&fd, 0x42), (1, 600, FFFD));
    }

    #[test]
    fn cid_system_info_without_strings_names_an_empty_collection() {
        // pdf_dict_get_string gives "" for what is no string; the name is
        // cut at a NUL and at 255 bytes, as fz_snprintf cuts it.
        let (doc, _) = loaded(t0_substitute(
            "Verdana",
            "/CIDSystemInfo << /Registry /Adobe /Ordering (Identity) >>",
            "",
        ));
        assert!(warnings(&doc).contains(&"unknown cid collection: -Identity".to_string()));
        let long = "x".repeat(300);
        let (doc, _) = loaded(t0_substitute(
            "Verdana",
            &format!("/CIDSystemInfo << /Registry (Ado\\000be) /Ordering ({long}) >>"),
            "",
        ));
        // (MuPDF's warning is then cut to 255 bytes in all.)
        let want = format!("unknown cid collection: Ado-{}", &long[..251]);
        assert!(warnings(&doc).contains(&want));
    }

    #[test]
    fn untrustworthy_font_metrics_take_the_defaults() {
        let font = |asc: i16, desc: i16| {
            let t = set(
                base(2, None),
                b"hhea",
                crate::font::sfnt::tests::hhea(asc, desc, 2),
            );
            new_font(b"T", ttf(&t)).unwrap_or_else(|e| panic!("{e}"))
        };
        let f = font(0, 0);
        assert_eq!((f.ascender, f.descender), (0.8, 0.0));
        let f = font(9000, 4000);
        assert_eq!((f.ascender, f.descender), (0.8, -0.2));
        let f = font(900, 100);
        assert_eq!((f.ascender, f.descender), (0.9, -0.1));
        // A font of no program and no procedures has no advances.
        assert_eq!(advance_glyph(&Font::new(b"none\0ignored"), 1, 0), 0.0);
        assert_eq!(Font::new(b"none\0ignored").name, b"none");
        assert!(new_font(b"Bad", Arc::from(&b"junk"[..])).is_err());
    }

    #[test]
    fn substitutes_take_mupdfs_vertical_forms() {
        let pairs = [
            (0x0021, 0xFE15),
            (0x0028, 0xFE35),
            (0x0029, 0xFE36),
            (0x002C, 0xFE10),
            (0x003A, 0xFE13),
            (0x003B, 0xFE14),
            (0x003F, 0xFE16),
            (0x005B, 0xFE47),
            (0x005D, 0xFE48),
            (0x005F, 0xFE33),
            (0x007B, 0xFE37),
            (0x007D, 0xFE38),
            (0x2013, 0xFE32),
            (0x2014, 0xFE31),
            (0x2025, 0xFE30),
            (0x2026, 0xFE19),
            (0x3001, 0xFE11),
            (0x3002, 0xFE12),
            (0x3008, 0xFE3F),
            (0x3009, 0xFE40),
            (0x300A, 0xFE3D),
            (0x300B, 0xFE3E),
            (0x300C, 0xFE41),
            (0x300D, 0xFE42),
            (0x300E, 0xFE43),
            (0x300F, 0xFE44),
            (0x3010, 0xFE3B),
            (0x3011, 0xFE3C),
            (0x3014, 0xFE39),
            (0x3015, 0xFE3A),
            (0x3016, 0xFE17),
            (0x3017, 0xFE18),
            (0xFF01, 0xFE15),
            (0xFF08, 0xFE35),
            (0xFF09, 0xFE36),
            (0xFF0C, 0xFE10),
            (0xFF1A, 0xFE13),
            (0xFF1B, 0xFE14),
            (0xFF1F, 0xFE16),
            (0xFF3B, 0xFE47),
            (0xFF3D, 0xFE48),
            (0xFF3F, 0xFE33),
            (0xFF5B, 0xFE37),
            (0xFF5D, 0xFE38),
            (0x30FC, 0xFE31),
            (0xFF0D, 0xFE31),
        ];
        for (c, v) in pairs {
            assert_eq!(vertical_form(c), v, "{c:04X}");
        }
        assert_eq!(vertical_form(0x41), 0x41);
    }

    #[test]
    fn to_unicode_remaps_are_bounded() {
        // An encoding of 2^24 four-byte codes (with one 16-bit range, as
        // a CMap of wide ranges alone counts as the identity): the remap
        // stops there.
        let cmap = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n\
            /CMapName /Big def 1 begincodespacerange <00000000> <FFFFFFFF> endcodespacerange\n\
            2 begincidrange <00000000> <00000001> 5 <00000100> <01000100> 0 endcidrange\n\
            endcmap CMapName currentdict /CMap defineresource pop end end";
        let to_unicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n\
            1 begincodespacerange <00000000> <FFFFFFFF> endcodespacerange\n\
            1 beginbfchar <00000001> <0041> endbfchar\n\
            endcmap CMapName currentdict /CMap defineresource pop end end";
        let doc = open(type0(
            "/Encoding 8 0 R /ToUnicode 9 0 R",
            "CIDFontType2",
            &format!("/BaseFont /Helvetica {IDENTITY} /FontDescriptor 7 0 R"),
            "/Flags 32",
            &[stream("", cmap), stream("", to_unicode)],
            &show("00000001"),
        ));
        assert_eq!(
            load(&doc).err().as_deref(),
            Some("limit error: ToUnicode remap too large")
        );
    }
}
