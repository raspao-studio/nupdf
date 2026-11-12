// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-annot.c (pdf_parse_default_appearance,
// pdf_annot_language and their helpers), source/pdf/pdf-appearance.c
// (add_required_fonts), source/pdf/pdf-font-add.c (pdf_add_simple_font,
// pdf_add_cjk_font) and source/fitz/text.c (fz_text_language_from_string),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! What the text writers read before they draw: the default appearance
//! string, the annotation's language, the base-14 fonts and the font
//! resources MuPDF adds for the scripts in the text.
//!
//! MuPDF adds each font resource as a new indirect object (one per font
//! and encoding in a document). Here they are direct dictionaries in the
//! resources; the oracle gives the objects a synthesis makes inline, so the
//! two compare.

use std::sync::Arc;

use crate::document::Document;
use crate::fontdesc::{self, Font};
use crate::glyphs;
use crate::object::{Dict, Object};
use crate::tables;
use crate::unicode::{self as ucd};

// fz_text_language: two or three letters packed in base 27.
const fn tag2(a: u8, b: u8) -> i32 {
    (a - b'a' + 1) as i32 + (b - b'a' + 1) as i32 * 27
}
const fn tag3(a: u8, b: u8, c: u8) -> i32 {
    tag2(a, b) + (c - b'a' + 1) as i32 * 27 * 27
}

pub const LANG_UNSET: i32 = 0;
pub const LANG_KO: i32 = tag2(b'k', b'o');
pub const LANG_JA: i32 = tag2(b'j', b'a');
pub const LANG_ZH: i32 = tag2(b'z', b'h');
pub const LANG_ZH_HANS: i32 = tag3(b'z', b'h', b's');
pub const LANG_ZH_HANT: i32 = tag3(b'z', b'h', b't');

/// `fz_text_language_from_string` on a C string (up to its NUL).
pub fn language_from_string(s: &[u8]) -> i32 {
    let s = c_str(s);
    if s.is_empty() {
        return LANG_UNSET;
    }
    match s {
        b"zh-Hant" | b"zh-HK" | b"zh-MO" | b"zh-SG" | b"zh-TW" => return LANG_ZH_HANT,
        b"zh-Hans" | b"zh-CN" => return LANG_ZH_HANS,
        _ => {}
    }
    let letter = |i: usize| match s.get(i).copied() {
        Some(c @ b'a'..=b'z') => Some(i32::from(c - b'a' + 1)),
        Some(c @ b'A'..=b'Z') => Some(i32::from(c - b'A' + 1)),
        _ => None,
    };
    let (Some(first), Some(second)) = (letter(0), letter(1)) else {
        return LANG_UNSET;
    };
    let mut lang = first + 27 * second;
    if let Some(third) = letter(2) {
        lang += 27 * 27 * third;
    }
    lang
}

/// `pdf_guess_text_language_from_font_name`.
pub fn language_from_font_name(name: &[u8]) -> i32 {
    let has = |n: &[u8]| name.windows(n.len()).any(|w| w == n);
    if [
        &b"SimFang"[..],
        b"SimHei",
        b"SimKai",
        b"SimLi",
        b"SimSun",
        b"Song",
    ]
    .iter()
    .any(|n| has(n))
    {
        return LANG_ZH_HANS;
    }
    if has(b"MingLiU") {
        return LANG_ZH_HANT;
    }
    if has(b"Gothic") || has(b"Mincho") {
        return LANG_JA;
    }
    if has(b"Batang") || has(b"Gulim") || has(b"Dotum") {
        return LANG_KO;
    }
    LANG_UNSET
}

/// The bytes of a C string: up to the first NUL.
fn c_str(s: &[u8]) -> &[u8] {
    s.iter().position(|&b| b == 0).map_or(s, |end| &s[..end])
}

/// `pdf_to_str_buf`: a string's bytes, empty for anything else.
fn str_buf(doc: &Document, obj: &Object) -> Vec<u8> {
    match doc.resolve(obj) {
        Object::String(s) => c_str(&s).to_vec(),
        _ => Vec::new(),
    }
}

/// `pdf_dict_get_inheritable` for an annotation: the value of `key` on it
/// or the nearest /Parent that has it, as stored. A cycle has MuPDF mend
/// the field and page trees' /Parent links and try again.
pub fn inheritable(doc: &Document, start: &Object, key: &[u8]) -> Result<Option<Object>, String> {
    doc.inherited(start, key).map_err(|e| e.0)
}

/// What a default appearance string sets: the font resource name as
/// written, its size, and the fill colour (`n` components).
#[derive(Debug, Clone, PartialEq)]
pub struct DefaultAppearance {
    pub font: Vec<u8>,
    pub size: f32,
    pub n: usize,
    pub color: [f32; 4],
}

impl DefaultAppearance {
    /// The colour's components.
    pub fn color(&self) -> &[f32] {
        &self.color[..self.n]
    }

    /// The font `pdf_parse_default_appearance` maps the name to: one of the
    /// five MuPDF writes with, else Helv.
    pub fn mapped_font(&self) -> &'static str {
        match self.font.as_slice() {
            b"Cour" => "Cour",
            b"TiRo" => "TiRo",
            b"Symb" => "Symb",
            b"ZaDb" => "ZaDb",
            _ => "Helv",
        }
    }
}

/// `pdf_parse_default_appearance_unmapped` with a font name buffer of
/// `font_name_size` bytes: the string is read through a 100-byte buffer,
/// split at spaces, and its operands kept on a four-deep stack.
pub fn parse_default_appearance(da: &[u8], font_name_size: usize) -> DefaultAppearance {
    let strlcpy = |s: &[u8], size: usize| -> Vec<u8> {
        let s = c_str(s);
        s[..s.len().min(size.saturating_sub(1))].to_vec()
    };
    let mut out = DefaultAppearance {
        font: strlcpy(b"Helv", font_name_size),
        size: 12.0,
        n: 0,
        color: [0.0; 4],
    };
    let buf = strlcpy(da, 100);
    let mut stack = [0.0f32; 4];
    let mut top = 0usize;
    for tok in buf.split(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t')) {
        match tok {
            [] => {}
            [b'/', name @ ..] => out.font = strlcpy(name, font_name_size),
            b"Tf" => {
                out.size = stack[0];
                top = 0;
            }
            b"g" => {
                out.n = 1;
                out.color[0] = stack[0];
                top = 0;
            }
            b"rg" => {
                out.n = 3;
                out.color[..3].copy_from_slice(&stack[..3]);
                top = 0;
            }
            b"k" => {
                out.n = 4;
                out.color = stack;
                top = 0;
            }
            _ => {
                let (v, read) = crate::strtof::fz_strtof(tok);
                if let Some(slot) = stack.get_mut(top) {
                    *slot = v;
                }
                if read == tok.len() {
                    top += 1;
                } else {
                    top = 0;
                }
            }
        }
    }
    out
}

/// The annotation's /DA, inherited, else the AcroForm's, as bytes.
fn default_appearance_string(doc: &Document, annot: &Object) -> Result<Vec<u8>, String> {
    let da = match inheritable(doc, annot, b"DA")? {
        Some(da) => da,
        None => {
            let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
            let form = doc.dict_get(&root, b"AcroForm");
            doc.dict_get(&form, b"DA")
        }
    };
    Ok(str_buf(doc, &da))
}

/// `pdf_annot_default_appearance`: the parsed /DA (the caller checks the
/// type has one).
pub fn default_appearance(doc: &Document, annot: &Object) -> Result<DefaultAppearance, String> {
    Ok(parse_default_appearance(
        &default_appearance_string(doc, annot)?,
        100,
    ))
}

/// `pdf_annot_language`: the inherited /Lang, else the document's, else a
/// guess from the /DA font's name.
pub fn language(doc: &Document, annot: &Object) -> Result<i32, String> {
    let lang = match inheritable(doc, annot, b"Lang")? {
        Some(lang) => language_from_string(&str_buf(doc, &lang)),
        None => {
            let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
            match doc.dict_get(&root, b"Lang") {
                Object::String(s) => {
                    language_from_string(crate::text_string::text_string(&s).as_bytes())
                }
                _ => LANG_UNSET,
            }
        }
    };
    if lang != LANG_UNSET {
        return Ok(lang);
    }
    let da = parse_default_appearance(&default_appearance_string(doc, annot)?, 80);
    Ok(language_from_font_name(&da.font))
}

/// `full_font_name`: the base-14 font for one of MuPDF's resource names,
/// and the name as it is used (an unknown one becomes Helv).
pub fn full_font_name(name: &str) -> (&'static str, &'static str) {
    match name {
        "Cour" => ("Cour", "Courier"),
        "TiRo" => ("TiRo", "Times-Roman"),
        "Symb" => ("Symb", "Symbol"),
        "ZaDb" => ("ZaDb", "ZapfDingbats"),
        _ => ("Helv", "Helvetica"),
    }
}

/// `fz_new_base14_font`.
pub fn base14(name: &str) -> Result<Arc<Font>, String> {
    fontdesc::base14_font(name)
        .ok_or_else(|| format!("cannot find builtin font with name '{name}'"))
}

/// `fz_encode_character`: the glyph for a code point through the font's
/// Unicode charmap.
pub fn encode_character(font: &Font, ucs: i32) -> i32 {
    match &font.face {
        Some(face) => u32::try_from(ucs).map_or(0, |u| face.get_char_index(u) as i32),
        None => ucs,
    }
}

/// `fz_encode_character_by_glyph_name`.
fn encode_by_glyph_name(font: &Font, name: &str) -> i32 {
    let Some(face) = &font.face else {
        return 0;
    };
    let g = face.name_index(name.as_bytes());
    if g != 0 {
        return g as i32;
    }
    face.char_index(glyphs::unicode(name.as_bytes())) as i32
}

/// `fz_advance_glyph`, horizontally.
pub fn advance(font: &Font, gid: i32) -> f32 {
    fontdesc::advance_glyph(font, gid, 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SimpleEncoding {
    Latin,
    Greek,
    Cyrillic,
}

/// `pdf_add_simple_font` for a base-14 font: its standard name and the
/// encoding, with the widths and the differences from WinAnsi for Greek
/// and Cyrillic.
fn simple_font(font: &Font, base: &str, encoding: SimpleEncoding) -> Object {
    let mut d = Dict::new();
    d.put(b"Type", Object::name(b"Font"));
    d.put(b"Subtype", Object::name(b"Type1"));
    d.put(b"BaseFont", Object::name(base.as_bytes()));
    let names: &[Option<&str>; 256] = match encoding {
        SimpleEncoding::Latin => {
            d.put(b"Encoding", Object::name(b"WinAnsiEncoding"));
            return Object::Dict(Arc::new(d));
        }
        SimpleEncoding::Greek => &tables::ISO8859_7,
        SimpleEncoding::Cyrillic => &tables::KOI8U,
    };
    let mut diff = Vec::new();
    let mut last = 0;
    for (i, glyph) in names.iter().enumerate().skip(128) {
        if let Some(glyph) = glyph {
            if last + 1 != i {
                diff.push(Object::Int(i as i64));
            }
            last = i;
            diff.push(Object::name(glyph.as_bytes()));
        }
    }
    let mut enc = Dict::new();
    enc.put(b"BaseEncoding", Object::name(b"WinAnsiEncoding"));
    enc.put(b"Differences", Object::Array(Arc::new(diff)));
    d.put(b"Encoding", Object::Dict(Arc::new(enc)));
    let (mut first, mut last) = (0usize, 0usize);
    let mut widths = [0i64; 256];
    for (i, glyph) in names.iter().enumerate() {
        let gid = glyph.map_or(0, |n| encode_by_glyph_name(font, n));
        if gid > 0 {
            if first == 0 {
                first = i;
            }
            last = i;
            widths[i] = (advance(font, gid) * 1000.0) as i64;
        }
    }
    let widths: Vec<Object> = widths[first..=last]
        .iter()
        .map(|&w| Object::Int(w))
        .collect();
    d.put(b"Widths", Object::Array(Arc::new(widths)));
    d.put(b"FirstChar", Object::Int(first as i64));
    d.put(b"LastChar", Object::Int(last as i64));
    Object::Dict(Arc::new(d))
}

#[derive(Debug, Clone, Copy)]
enum Cjk {
    Cns,
    Gb,
    Japan,
    Korea,
}

/// `pdf_add_cjk_font` for the serif fonts, written horizontally.
fn cjk_font(script: Cjk) -> Object {
    let (basefont, encoding, ordering, supplement) = match script {
        Cjk::Cns => ("Ming", "UniCNS-UTF16-H", "CNS1", 7),
        Cjk::Gb => ("Song", "UniGB-UTF16-H", "GB1", 5),
        Cjk::Japan => ("Mincho", "UniJIS-UTF16-H", "Japan1", 6),
        Cjk::Korea => ("Batang", "UniKS-UTF16-H", "Korea1", 2),
    };
    let string = |s: &str| Object::String(Arc::from(s.as_bytes()));
    let mut desc = Dict::new();
    desc.put(b"Type", Object::name(b"FontDescriptor"));
    desc.put(b"FontName", string(basefont));
    desc.put(
        b"FontBBox",
        super::super::rect_object(crate::geometry::Rect::new(-200.0, -200.0, 1200.0, 1200.0)),
    );
    // PDF_FD_SYMBOLIC | PDF_FD_SERIF
    desc.put(b"Flags", Object::Int(4 | 2));
    desc.put(b"ItalicAngle", Object::Int(0));
    desc.put(b"Ascent", Object::Int(1000));
    desc.put(b"Descent", Object::Int(-200));
    desc.put(b"StemV", Object::Int(80));
    let mut csi = Dict::new();
    csi.put(b"Registry", string("Adobe"));
    csi.put(b"Ordering", string(ordering));
    csi.put(b"Supplement", Object::Int(supplement));
    let mut sub = Dict::new();
    sub.put(b"Type", Object::name(b"Font"));
    sub.put(b"Subtype", Object::name(b"CIDFontType0"));
    sub.put(b"BaseFont", Object::name(basefont.as_bytes()));
    sub.put(b"CIDSystemInfo", Object::Dict(Arc::new(csi)));
    sub.put(b"FontDescriptor", Object::Dict(Arc::new(desc)));
    let mut font = Dict::new();
    font.put(b"Type", Object::name(b"Font"));
    font.put(b"Subtype", Object::name(b"Type0"));
    font.put(b"BaseFont", Object::name(basefont.as_bytes()));
    font.put(b"Encoding", Object::name(encoding.as_bytes()));
    font.put(
        b"DescendantFonts",
        Object::Array(Arc::new(vec![Object::Dict(Arc::new(sub))])),
    );
    Object::Dict(Arc::new(font))
}

/// The simple font resource for a base-14 font in WinAnsi, as the rubber
/// stamp adds Times-Bold.
pub fn latin_font(font: &Font, base: &str) -> Object {
    simple_font(font, base, SimpleEncoding::Latin)
}

/// Whether a code point is drawn with the CJK fonts though its script is
/// not Han: the halfwidth and fullwidth forms, the CJK symbols.
pub fn is_han_form(c: i32) -> bool {
    (0xff01..=0xffee).contains(&c) || (0x3000..=0x303f).contains(&c)
}

/// `add_required_fonts`: the font resources the text's scripts need, under
/// `fontname` (its Greek and Cyrillic versions with GRK and CYR after) and
/// the CJK fonts' own names. `base` is the base-14 font's name.
pub fn add_required_fonts(
    res_font: &mut Dict,
    lang: i32,
    font: &Font,
    base: &str,
    fontname: &str,
    text: &str,
) {
    let (mut latin, mut greek, mut cyrillic) = (false, false, false);
    let (mut korean, mut japanese, mut bopomofo) = (false, false, false);
    let (mut han, mut hans, mut hant) = (false, false, false);
    for ch in text.chars() {
        let c = ch as i32;
        match ucd::script(c) {
            ucd::SCRIPT_COMMON | ucd::SCRIPT_INHERITED => {}
            ucd::SCRIPT_LATIN => latin = true,
            ucd::SCRIPT_GREEK => greek = true,
            ucd::SCRIPT_CYRILLIC => cyrillic = true,
            ucd::SCRIPT_HANGUL => korean = true,
            ucd::SCRIPT_HIRAGANA | ucd::SCRIPT_KATAKANA => japanese = true,
            ucd::SCRIPT_BOPOMOFO => bopomofo = true,
            ucd::SCRIPT_HAN => han = true,
            // For the fallback bullet character.
            _ => latin = true,
        }
        if is_han_form(c) {
            han = true;
        }
    }
    if han {
        match lang {
            LANG_KO => korean = true,
            LANG_ZH | LANG_ZH_HANT => hant = true,
            LANG_ZH_HANS => hans = true,
            _ => japanese = true,
        }
    }
    if bopomofo {
        if lang == LANG_ZH_HANS {
            hans = true;
        } else {
            hant = true;
        }
    }
    if !greek && !cyrillic && !korean && !japanese && !hant && !hans {
        latin = true;
    }
    let mut add = |name: &str, make: &dyn Fn() -> Object| {
        if res_font.get(name.as_bytes()).is_none() {
            res_font.put(name.as_bytes(), make());
        }
    };
    if latin {
        add(fontname, &|| simple_font(font, base, SimpleEncoding::Latin));
    }
    if greek {
        add(&format!("{fontname}GRK"), &|| {
            simple_font(font, base, SimpleEncoding::Greek)
        });
    }
    if cyrillic {
        add(&format!("{fontname}CYR"), &|| {
            simple_font(font, base, SimpleEncoding::Cyrillic)
        });
    }
    if korean {
        add("Batang", &|| cjk_font(Cjk::Korea));
    }
    if japanese {
        add("Mincho", &|| cjk_font(Cjk::Japan));
    }
    if hant {
        add("Ming", &|| cjk_font(Cjk::Cns));
    }
    if hans {
        add("Song", &|| cjk_font(Cjk::Gb));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_read_as_mupdf_packs_them() {
        assert_eq!(language_from_string(b"ja"), LANG_JA);
        assert_eq!(language_from_string(b"ko-KR"), LANG_KO);
        assert_eq!(language_from_string(b"zh-TW"), LANG_ZH_HANT);
        assert_eq!(language_from_string(b"zh-CN"), LANG_ZH_HANS);
        assert_eq!(language_from_string(b"ZH"), LANG_ZH);
        assert_eq!(language_from_string(b"x"), LANG_UNSET);
        assert_eq!(language_from_string(b""), LANG_UNSET);
        assert_ne!(language_from_string(b"jpn"), LANG_JA);
        assert_eq!(language_from_font_name(b"MS-Mincho"), LANG_JA);
        assert_eq!(language_from_font_name(b"STSong-Light"), LANG_ZH_HANS);
        assert_eq!(language_from_font_name(b"Helv"), LANG_UNSET);
    }

    #[test]
    fn default_appearance_keeps_mupdf_stack_rules() {
        let da = parse_default_appearance(b"/TiRo 9 Tf 1 0 0 rg", 100);
        assert_eq!(da.font, b"TiRo");
        assert_eq!(da.mapped_font(), "TiRo");
        assert_eq!(da.size, 9.0);
        assert_eq!(da.color(), &[1.0, 0.0, 0.0]);
        // Empty: Helv 12, no colour.
        let da = parse_default_appearance(b"", 100);
        assert_eq!((da.font.as_slice(), da.size, da.n), (&b"Helv"[..], 12.0, 0));
        // A font MuPDF does not write with maps to Helv.
        let da = parse_default_appearance(b"/F1 0 Tf 0.5 g", 100);
        assert_eq!(
            (da.mapped_font(), da.size, da.color()),
            ("Helv", 0.0, &[0.5][..])
        );
        // A word that is not a number still lands on the stack, then clears it.
        let da = parse_default_appearance(b"3 x Tf", 100);
        assert_eq!(da.size, 3.0);
        let da = parse_default_appearance(b"x Tf", 100);
        assert_eq!(da.size, 0.0);
        // Separators repeat; tabs and newlines split too.
        let da = parse_default_appearance(b"/Cour\t\t7\nTf  0 0 0 1 k", 100);
        assert_eq!((da.mapped_font(), da.size, da.n), ("Cour", 7.0, 4));
        // Only 99 bytes are read: the Tf is cut off.
        let mut long = vec![b' '; 97];
        long.extend_from_slice(b"5 Tf");
        assert_eq!(parse_default_appearance(&long, 100).size, 12.0);
        // The font name buffer truncates.
        let da = parse_default_appearance(b"/ABCDEFGHIJ 1 Tf", 5);
        assert_eq!(da.font, b"ABCD");
    }

    #[test]
    fn required_fonts_follow_the_scripts() {
        let Ok(font) = base14("Helvetica") else {
            panic!("no Helvetica");
        };
        let names = |text: &str, lang: i32| {
            let mut d = Dict::new();
            add_required_fonts(&mut d, lang, &font, "Helvetica", "Helv", text);
            d.iter()
                .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("Hello", LANG_UNSET), ["Helv"]);
        assert_eq!(names("123", LANG_UNSET), ["Helv"]);
        assert_eq!(names("Αβγ", LANG_UNSET), ["HelvGRK"]);
        assert_eq!(names("aЖ", LANG_UNSET), ["Helv", "HelvCYR"]);
        assert_eq!(names("漢", LANG_UNSET), ["Mincho"]);
        assert_eq!(names("漢", LANG_ZH_HANS), ["Song"]);
        assert_eq!(names("漢", LANG_ZH), ["Ming"]);
        assert_eq!(names("한", LANG_UNSET), ["Batang"]);
        assert_eq!(names("ㄅ", LANG_UNSET), ["Ming"]);
        assert_eq!(names("ア", LANG_UNSET), ["Mincho"]);
        // Arabic falls back to Latin for its bullet.
        assert_eq!(names("ع", LANG_UNSET), ["Helv"]);
    }

    #[test]
    fn greek_font_has_differences_and_widths() {
        let Ok(font) = base14("Helvetica") else {
            panic!("no Helvetica");
        };
        let f = simple_font(&font, "Helvetica", SimpleEncoding::Greek);
        let Some(d) = f.as_dict() else {
            panic!("not a dict");
        };
        assert_eq!(d.get(b"FirstChar"), Some(&Object::Int(32)));
        let Some(Object::Array(w)) = d.get(b"Widths") else {
            panic!("no widths");
        };
        // The space in Helvetica is 278 units wide.
        assert_eq!(w.first(), Some(&Object::Int(278)));
    }
}
