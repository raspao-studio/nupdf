// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Verified surgical text patching: font-aware in-place span and line
//! replacement with post-write geometric and text-extraction self-checks.
//!
//! [`apply_patches`] resolves content IDs (`p1.b2.l3.s1` or `p1.b2.l3`,
//! 1-based) against structured text, removes only the target glyphs on each
//! affected page through [`crate::content_clean::filter_page_contents`]
//! (leaving neighboring text, vector artwork, backgrounds, and images intact),
//! and appends a balanced `q BT ... ET Q` replacement layer preserving the
//! target's baseline origin, writing direction, font family/weight/posture,
//! size, and fill color. [`verify_patched_bytes`] reopens serialized bytes and
//! verifies that replacement text extracts at the expected location, old text
//! is gone, font fallback policy holds, and new glyphs neither overflow the
//! page nor collide with untouched spans.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::Serialize;

use crate::content::Store;
use crate::content_clean::{self, filter_page_contents};
use crate::content_filter::Hooks;
use crate::document::Document;
use crate::extract::{self, Style};
use crate::fontdesc::{self, Font};
use crate::geometry::{Matrix, Point, Quad, Rect};
use crate::glyphs;
use crate::object::{Dict, ObjRef, Object};
use crate::stext::{self, Block, Options as StextOptions, StextChar, StextPage, char_flags};
use crate::tables;
use crate::unicode as ucd;
use crate::write::WriteOptions;

/// One surgical text replacement request targeting a content span or line ID.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PatchSpec {
    /// Content ID (`p{page}.b{block}.l{line}.s{span}` or `p{page}.b{block}.l{line}`, 1-based).
    pub target_id: String,
    /// Replacement string; an empty string deletes the target glyphs in place.
    pub new_text: String,
}

/// Policy flags controlling font substitution and geometric verification.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct PatchOptions {
    /// Reject fallback to a substitute Base-14/CJK font during verification
    /// (`"verify.font_fallback_disallowed"`).
    pub strict_font: bool,
    /// Check whether replacement glyph bounds collide with untouched spans on
    /// the same page (`"verify.collision"`).
    pub check_collisions: bool,
    /// Check whether replacement glyph bounds extend outside page bounds
    /// beyond a 1.0 pt tolerance (`"verify.page_overflow"`).
    pub check_overflow: bool,
}

impl Default for PatchOptions {
    fn default() -> Self {
        PatchOptions {
            strict_font: false,
            check_collisions: true,
            check_overflow: true,
        }
    }
}

/// Record of one applied text patch and its resolved geometry/font metadata.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AppliedPatch {
    pub id: String,
    /// 1-based page number.
    pub page: i32,
    pub old_text: String,
    pub new_text: String,
    pub old_bbox: [f32; 4],
    pub new_bbox: Option<[f32; 4]>,
    pub font_used: String,
    pub fallback_font: bool,
}

/// One verification failure detected on the serialized patched PDF.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PatchFinding {
    pub code: String,
    pub id: String,
    /// 1-based page number.
    pub page: i32,
    pub message: String,
}

/// Aggregated self-check result from [`verify_patched_bytes`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PatchVerification {
    pub verified: bool,
    pub findings: Vec<PatchFinding>,
}

/// Combined outcome of [`patch_and_verify`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PatchResult {
    pub patches: Vec<AppliedPatch>,
    pub verification: Option<PatchVerification>,
}

/// Error returned when patch target resolution, page mutation, or serialization fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PatchError {}

/// Parsed 1-based content identifier for a line or span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TargetAddr {
    page: i32,
    block: usize,
    line: usize,
    span: Option<usize>,
}

impl TargetAddr {
    fn parse(raw: &str) -> Result<TargetAddr, PatchError> {
        let bad = || PatchError {
            code: "usage.bad_id",
            message: format!(
                "invalid target id {raw:?}: expected p<page>.b<block>.l<line>[.s<span>] with 1-based indices"
            ),
        };
        let parts: Vec<&str> = raw.split('.').collect();
        if parts.len() != 3 && parts.len() != 4 {
            return Err(bad());
        }
        let parse_pos = |part: &str, prefix: char| -> Option<usize> {
            let digits = part.strip_prefix(prefix)?;
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            digits.parse::<usize>().ok().filter(|&v| v >= 1)
        };
        let page_usize = parse_pos(parts[0], 'p').ok_or_else(bad)?;
        let page = i32::try_from(page_usize).map_err(|_| bad())?;
        let block = parse_pos(parts[1], 'b').ok_or_else(bad)?;
        let line = parse_pos(parts[2], 'l').ok_or_else(bad)?;
        let span = match parts.get(3) {
            Some(s) => Some(parse_pos(s, 's').ok_or_else(bad)?),
            None => None,
        };
        Ok(TargetAddr {
            page,
            block,
            line,
            span,
        })
    }

    fn overlaps(self, other: TargetAddr) -> bool {
        if self.page != other.page || self.block != other.block || self.line != other.line {
            return false;
        }
        match (self.span, other.span) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }

    fn matches_span(self, page: i32, block: usize, line: usize, span: usize) -> bool {
        self.page == page
            && self.block == block
            && self.line == line
            && self.span.is_none_or(|s| s == span)
    }
}

fn parse_and_validate_specs(specs: &[PatchSpec]) -> Result<Vec<TargetAddr>, PatchError> {
    let mut addrs = Vec::with_capacity(specs.len());
    for spec in specs {
        let addr = TargetAddr::parse(&spec.target_id)?;
        if let Some(prev) = addrs
            .iter()
            .position(|existing: &TargetAddr| existing.overlaps(addr))
        {
            return Err(PatchError {
                code: "usage.duplicate_target",
                message: format!(
                    "target {:?} overlaps earlier target {:?}",
                    spec.target_id, specs[prev].target_id
                ),
            });
        }
        addrs.push(addr);
    }
    Ok(addrs)
}

/// Union of non-synthetic character boxes in `chars`, falling back to `fallback`
/// if every character in the slice is synthetic.
fn ink_bbox(chars: &[StextChar], fallback: Rect) -> Rect {
    let mut r = Rect::EMPTY;
    let mut any = false;
    for ch in chars {
        if ch.flags & char_flags::SYNTHETIC == 0 {
            r = r.union(Rect::from_quad(ch.quad));
            any = true;
        }
    }
    if any { r } else { fallback }
}

/// Trims leading and trailing gap-synthesized whitespace characters (`SYNTHETIC`)
/// from a span slice so `old_text`, `old_bbox`, and erase quads cover only real
/// glyphs drawn in that span.
fn trim_synthetic_edges(chars: &[StextChar]) -> &[StextChar] {
    let mut start = 0usize;
    let mut end = chars.len();
    while start < end && chars[start].flags & char_flags::SYNTHETIC != 0 {
        start += 1;
    }
    while end > start && chars[end - 1].flags & char_flags::SYNTHETIC != 0 {
        end -= 1;
    }
    if start < end {
        &chars[start..end]
    } else {
        chars
    }
}

fn rect_array(r: Rect) -> [f32; 4] {
    [r.x0, r.y0, r.x1, r.y1]
}

fn rect_from_array(a: [f32; 4]) -> Rect {
    Rect::new(a[0], a[1], a[2], a[3])
}

/// Snapshots the bounding boxes of all untouched spans on every page referenced
/// by `specs`, keyed by 1-based page number. Call before [`apply_patches`] to
/// feed [`verify_patched_bytes`].
pub fn snapshot_untouched_spans(
    doc: &Document,
    specs: &[PatchSpec],
) -> Result<HashMap<i32, Vec<(String, Rect)>>, PatchError> {
    let addrs = parse_and_validate_specs(specs)?;
    if addrs.is_empty() {
        return Ok(HashMap::new());
    }
    let page_count = doc.page_count().map_err(|e| PatchError {
        code: "input.page_tree",
        message: e.0,
    })?;
    let mut pages_needed: Vec<i32> = addrs.iter().map(|a| a.page).collect();
    pages_needed.sort_unstable();
    pages_needed.dedup();

    let mut store = Store::default();
    let mut out: HashMap<i32, Vec<(String, Rect)>> = HashMap::new();
    for page_num in pages_needed {
        if page_num < 1 || page_num > page_count {
            return Err(PatchError {
                code: "input.target_not_found",
                message: format!("page {page_num} out of range (1..={page_count})"),
            });
        }
        let page_obj = doc.page(page_num - 1).map_err(|e| PatchError {
            code: "input.page_tree",
            message: e.0,
        })?;
        let (res, next_store) = stext::extract_page(doc, &page_obj, StextOptions::default(), store);
        store = next_store;
        let stext_page = res.map_err(|message| PatchError {
            code: "input.page_tree",
            message,
        })?;
        out.insert(
            page_num,
            collect_untouched_on_page(&stext_page, page_num, &addrs),
        );
    }
    Ok(out)
}

fn collect_untouched_on_page(
    stext_page: &StextPage,
    page_num: i32,
    addrs: &[TargetAddr],
) -> Vec<(String, Rect)> {
    let mut untouched = Vec::new();
    for (bi, block) in stext_page.blocks.iter().enumerate() {
        let Block::Text(tb) = block else {
            continue;
        };
        for (li, line) in tb.lines.iter().enumerate() {
            for (si, span) in extract::spans(line).into_iter().enumerate() {
                let b1 = bi + 1;
                let l1 = li + 1;
                let s1 = si + 1;
                if addrs.iter().any(|a| a.matches_span(page_num, b1, l1, s1)) {
                    continue;
                }
                let slice = line.chars.get(span.start..span.end).unwrap_or(&[]);
                let trimmed = trim_synthetic_edges(slice);
                let bbox = ink_bbox(trimmed, span.bbox);
                if bbox.is_valid() && !bbox.is_empty() {
                    untouched.push((format!("p{page_num}.b{b1}.l{l1}.s{s1}"), bbox));
                }
            }
        }
    }
    untouched
}

#[derive(Debug, Clone)]
struct ResolvedTarget {
    spec_index: usize,
    id: String,
    page: i32,
    old_text: String,
    new_text: String,
    old_bbox: Rect,
    origin: Point,
    dir: Point,
    style: Style,
    erase_quads_pdf: Vec<Quad>,
    base14_name: &'static str,
    cjk_lang: CjkLang,
    page_had_exact_base14: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CjkLang {
    Japan,
    Korea,
    Hans,
    Hant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Sans,
    Serif,
    Mono,
}

fn strip_subset_prefix(name: &[u8]) -> (&[u8], bool) {
    if name.len() > 7 && name[..6].iter().all(u8::is_ascii_uppercase) && name[6] == b'+' {
        (&name[7..], true)
    } else {
        (name, false)
    }
}

fn guess_cjk_lang(font_name: &[u8]) -> CjkLang {
    let lower = String::from_utf8_lossy(font_name).to_ascii_lowercase();
    if [
        "simfang",
        "simhei",
        "simkai",
        "simli",
        "simsun",
        "song",
        "heiti",
        "pingfang sc",
        "notosanssc",
        "notoserifsc",
        "sourcehansanssc",
        "sourcehansanscn",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        return CjkLang::Hans;
    }
    if [
        "mingliu",
        "pmingliu",
        "ming",
        "kaiti",
        "pingfang tc",
        "notosanstc",
        "notoseriftc",
        "sourcehansanstc",
        "sourcehansanstw",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        return CjkLang::Hant;
    }
    if [
        "batang",
        "gulim",
        "dotum",
        "malgun",
        "nanum",
        "notosanskr",
        "notoserifkr",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        return CjkLang::Korea;
    }
    CjkLang::Japan
}

/// Maps the original span's `Style` to the closest standard Base-14 PostScript
/// font name, preserving family (`Times` / `Courier` / `Helvetica`), bold, and
/// italic/oblique posture.
fn match_base14(style: &Style) -> &'static str {
    let (stripped, _) = strip_subset_prefix(&style.font);
    let lower = String::from_utf8_lossy(stripped).to_ascii_lowercase();

    let bold = style.bold
        || lower.contains("bold")
        || lower.contains("black")
        || lower.contains("heavy")
        || lower.contains("semibold")
        || lower.contains("demibold");
    let italic = style.italic
        || lower.contains("italic")
        || lower.contains("oblique")
        || lower.contains("slanted");

    let family = if [
        "courier",
        "consolas",
        "mono",
        "menlo",
        "firacode",
        "fira code",
        "inconsolata",
        "sourcecode",
        "lucidaconsole",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        Family::Mono
    } else if !lower.contains("sans")
        && [
            "times",
            "georgia",
            "garamond",
            "minion",
            "cambria",
            "serif",
            "palatino",
            "baskerville",
            "bookman",
            "charter",
            "century",
        ]
        .iter()
        .any(|k| lower.contains(k))
    {
        Family::Serif
    } else {
        Family::Sans
    };

    match (family, bold, italic) {
        (Family::Mono, false, false) => "Courier",
        (Family::Mono, true, false) => "Courier-Bold",
        (Family::Mono, false, true) => "Courier-Oblique",
        (Family::Mono, true, true) => "Courier-BoldOblique",
        (Family::Serif, false, false) => "Times-Roman",
        (Family::Serif, true, false) => "Times-Bold",
        (Family::Serif, false, true) => "Times-Italic",
        (Family::Serif, true, true) => "Times-BoldItalic",
        (Family::Sans, false, false) => "Helvetica",
        (Family::Sans, true, false) => "Helvetica-Bold",
        (Family::Sans, false, true) => "Helvetica-Oblique",
        (Family::Sans, true, true) => "Helvetica-BoldOblique",
    }
}

/// Checks whether `page_obj`'s `/Resources /Font` already contains a non-embedded
/// simple Base-14 font matching `base14_name` with a standard encoding (not a
/// subsetted or custom-encoded font).
fn page_has_matching_base14(doc: &Document, page_obj: &Object, base14_name: &str) -> bool {
    let Ok(Some(res_raw)) = doc.inherited(page_obj, b"Resources") else {
        return false;
    };
    let res = doc.resolve(&res_raw);
    let Some(res_dict) = res.as_dict() else {
        return false;
    };
    let fonts_obj = doc.resolve(res_dict.get(b"Font").unwrap_or(&Object::Null));
    let Some(fonts) = fonts_obj.as_dict() else {
        return false;
    };
    for (_, val) in fonts.iter() {
        let fobj = doc.resolve(val);
        let Some(fdict) = fobj.as_dict() else {
            continue;
        };
        let subtype = doc
            .resolve(fdict.get(b"Subtype").unwrap_or(&Object::Null))
            .as_name()
            .map(<[u8]>::to_vec);
        if !matches!(subtype.as_deref(), Some(b"Type1" | b"TrueType")) {
            continue;
        }
        let Some(base_bytes) = doc
            .resolve(fdict.get(b"BaseFont").unwrap_or(&Object::Null))
            .as_name()
            .map(<[u8]>::to_vec)
        else {
            continue;
        };
        let (_, subsetted) = strip_subset_prefix(&base_bytes);
        if subsetted || fontdesc::clean_font_name(&base_bytes) != Some(base14_name) {
            continue;
        }
        // Reject embedded font programs (which may contain only a glyph subset).
        let fd_obj = doc.resolve(fdict.get(b"FontDescriptor").unwrap_or(&Object::Null));
        if let Some(fd) = fd_obj.as_dict()
            && (fd.get(b"FontFile").is_some()
                || fd.get(b"FontFile2").is_some()
                || fd.get(b"FontFile3").is_some())
        {
            continue;
        }
        // Require absent or standard named encoding (not a custom /Differences dict).
        let enc_obj = doc.resolve(fdict.get(b"Encoding").unwrap_or(&Object::Null));
        let standard_enc = match &enc_obj {
            Object::Null => true,
            Object::Name(n) => matches!(
                n.as_ref(),
                b"WinAnsiEncoding" | b"StandardEncoding" | b"MacRomanEncoding"
            ),
            _ => false,
        };
        if standard_enc {
            return true;
        }
    }
    false
}

/// Content-stream filter that removes only glyphs intersecting target quads,
/// leaving vector paths, backgrounds, shadings, and images untouched.
#[derive(Debug)]
struct SpanEraser {
    quads: Vec<Quad>,
}

impl Hooks for SpanEraser {
    fn filters_text(&self) -> bool {
        !self.quads.is_empty()
    }

    fn text_filter(
        &mut self,
        _ucs: &[i32],
        trm: Matrix,
        ctm: Matrix,
        bbox: Rect,
        _render: i32,
        _fill_alpha: f32,
        _stroke_alpha: f32,
    ) -> bool {
        let trm = trm.concat(ctm);
        let mut b = bbox.transform(trm);
        let w = b.x1 - b.x0;
        let h = b.y1 - b.y0;
        b.x0 += w / 10.0;
        b.x1 -= w / 10.0;
        b.y0 += h / 10.0;
        b.y1 -= h / 10.0;
        self.quads
            .iter()
            .any(|q| b.intersect(Rect::from_quad(*q)).is_valid())
    }
}

/// Font encoding variant used for a run of replacement characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FontKey {
    WinAnsi(&'static str),
    Greek(&'static str),
    Cyrillic(&'static str),
    CustomDiff {
        base: &'static str,
        glyphs: Vec<&'static str>,
    },
    Cjk(CjkLang),
}

impl FontKey {
    fn display_name(&self) -> &'static str {
        match self {
            FontKey::WinAnsi(b) | FontKey::Greek(b) | FontKey::Cyrillic(b) => b,
            FontKey::CustomDiff { base, .. } => base,
            FontKey::Cjk(CjkLang::Korea) => "Batang",
            FontKey::Cjk(CjkLang::Japan) => "Mincho",
            FontKey::Cjk(CjkLang::Hant) => "Ming",
            FontKey::Cjk(CjkLang::Hans) => "Song",
        }
    }
}

#[derive(Debug, Clone)]
enum EncodedRun {
    Byte { key: FontKey, bytes: Vec<u8> },
    Utf16 { key: FontKey, units: Vec<u16> },
}

fn is_han_form(u: i32) -> bool {
    (0xff01..=0xffee).contains(&u) || (0x3000..=0x303f).contains(&u)
}

fn encode_by_glyph_name(font: &Font, name: &str) -> i32 {
    let Some(face) = &font.face else {
        return 0;
    };
    let g = face.name_index(name.as_bytes());
    if g != 0 {
        return g as i32;
    }
    let u = glyphs::unicode(name.as_bytes());
    if u == glyphs::REPLACEMENT {
        return 0;
    }
    face.char_index(u) as i32
}

/// Searches Adobe Glyph List names for `u` and checks whether `base_font`,
/// `Symbol`, or `ZapfDingbats` carries that glyph.
fn find_agl_glyph(base_name: &'static str, u: i32) -> Option<(&'static str, &'static str)> {
    let code = u16::try_from(u).ok()?;
    let candidates: [&'static str; 3] = [base_name, "Symbol", "ZapfDingbats"];
    for (idx, &agl_code) in tables::AGL_CODES.iter().enumerate() {
        if agl_code != code {
            continue;
        }
        let glyph_name = tables::AGL_NAMES[idx];
        for cand in candidates {
            if let Some(font) = fontdesc::base14_font(cand)
                && encode_by_glyph_name(&font, glyph_name) > 0
            {
                return Some((cand, glyph_name));
            }
        }
    }
    None
}

/// Splits `text` into encoded runs using `base_name` (WinAnsi, ISO-8859-7,
/// KOI8-U, custom AGL `/Differences`, or CJK UTF-16 Type0), returning the runs
/// and whether all characters fit in plain `WinAnsiEncoding` of `base_name`.
fn encode_replacement_runs(
    text: &str,
    base_name: &'static str,
    cjk_lang: CjkLang,
) -> (Vec<EncodedRun>, bool) {
    let mut runs: Vec<EncodedRun> = Vec::new();
    let mut pure_winansi = true;
    let mut custom_slots: HashMap<&'static str, Vec<&'static str>> = HashMap::new();

    let push_byte = |runs: &mut Vec<EncodedRun>, key: FontKey, b: u8| match runs.last_mut() {
        Some(EncodedRun::Byte {
            key: prev_key,
            bytes,
        }) if *prev_key == key => bytes.push(b),
        _ => runs.push(EncodedRun::Byte {
            key,
            bytes: vec![b],
        }),
    };

    let push_utf16 = |runs: &mut Vec<EncodedRun>, key: FontKey, ch: char| {
        let mut buf = [0u16; 2];
        let encoded = ch.encode_utf16(&mut buf);
        match runs.last_mut() {
            Some(EncodedRun::Utf16 {
                key: prev_key,
                units,
            }) if *prev_key == key => units.extend_from_slice(encoded),
            _ => runs.push(EncodedRun::Utf16 {
                key,
                units: encoded.to_vec(),
            }),
        }
    };

    for raw_ch in text.chars() {
        let ch = match raw_ch {
            '\t' | '\n' | '\r' => ' ',
            c if (c as u32) < 0x20 || c == '\x7f' => {
                pure_winansi = false;
                ' '
            }
            c => c,
        };
        let u = ch as i32;
        let script = if is_han_form(u) {
            ucd::SCRIPT_HAN
        } else {
            ucd::script(u)
        };

        // 1. Direct WinAnsi for ASCII/Latin/Common punctuation.
        if !matches!(
            script,
            ucd::SCRIPT_GREEK
                | ucd::SCRIPT_CYRILLIC
                | ucd::SCRIPT_HANGUL
                | ucd::SCRIPT_HIRAGANA
                | ucd::SCRIPT_KATAKANA
                | ucd::SCRIPT_BOPOMOFO
                | ucd::SCRIPT_HAN
        ) {
            let win = glyphs::windows_1252_from_unicode(u);
            if (0x20..=0xff).contains(&win) {
                push_byte(&mut runs, FontKey::WinAnsi(base_name), win as u8);
                continue;
            }
        }

        pure_winansi = false;

        // 2. Greek via ISO-8859-7 Differences.
        if script == ucd::SCRIPT_GREEK {
            let grk = glyphs::iso8859_7_from_unicode(u);
            if (0x20..=0xff).contains(&grk) {
                push_byte(&mut runs, FontKey::Greek(base_name), grk as u8);
                continue;
            }
        }

        // 3. Cyrillic via KOI8-U Differences.
        if script == ucd::SCRIPT_CYRILLIC {
            let cyr = glyphs::koi8u_from_unicode(u);
            if (0x20..=0xff).contains(&cyr) {
                push_byte(&mut runs, FontKey::Cyrillic(base_name), cyr as u8);
                continue;
            }
        }

        // 4. CJK scripts via Type0 CIDFontType0 + UTF-16BE.
        match script {
            ucd::SCRIPT_HANGUL => {
                push_utf16(&mut runs, FontKey::Cjk(CjkLang::Korea), ch);
                continue;
            }
            ucd::SCRIPT_HIRAGANA | ucd::SCRIPT_KATAKANA => {
                push_utf16(&mut runs, FontKey::Cjk(CjkLang::Japan), ch);
                continue;
            }
            ucd::SCRIPT_BOPOMOFO => {
                let lang = if cjk_lang == CjkLang::Hans {
                    CjkLang::Hans
                } else {
                    CjkLang::Hant
                };
                push_utf16(&mut runs, FontKey::Cjk(lang), ch);
                continue;
            }
            ucd::SCRIPT_HAN => {
                push_utf16(&mut runs, FontKey::Cjk(cjk_lang), ch);
                continue;
            }
            _ => {}
        }

        // 5. Non-WinAnsi Win1252 check (e.g. symbols that happen to sit in 128..159).
        let win = glyphs::windows_1252_from_unicode(u);
        if (0x20..=0xff).contains(&win) {
            push_byte(&mut runs, FontKey::WinAnsi(base_name), win as u8);
            continue;
        }

        // 6. Extra AGL glyphs available in Base-14 / Symbol / ZapfDingbats via custom /Differences.
        if let Some((cand_base, glyph_name)) = find_agl_glyph(base_name, u) {
            let table = custom_slots.entry(cand_base).or_default();
            let pos = match table.iter().position(|&g| g == glyph_name) {
                Some(idx) => Some(idx),
                None if table.len() < 128 => {
                    table.push(glyph_name);
                    Some(table.len() - 1)
                }
                None => None,
            };
            if let Some(slot) = pos {
                let code = (128 + slot) as u8;
                // Keep placeholder key; finalized with full `glyphs` table below.
                push_byte(
                    &mut runs,
                    FontKey::CustomDiff {
                        base: cand_base,
                        glyphs: Vec::new(),
                    },
                    code,
                );
                continue;
            }
        }

        // 7. Universal Unicode fallback via Type0 UTF-16BE with identity /ToUnicode.
        push_utf16(&mut runs, FontKey::Cjk(cjk_lang), ch);
    }

    // Finalize custom /Differences tables so all runs for a candidate base share one font dict.
    for run in &mut runs {
        if let EncodedRun::Byte {
            key: FontKey::CustomDiff { base, glyphs },
            ..
        } = run
            && let Some(table) = custom_slots.get(base)
        {
            *glyphs = table.clone();
        }
    }

    (runs, pure_winansi)
}

fn reference(num: i32) -> Object {
    Object::Ref(ObjRef::new(num, 0))
}

fn build_differences_font(base: &'static str, names: &[Option<&str>; 256]) -> Object {
    let mut d = Dict::new();
    d.put(b"Type", Object::name(b"Font"));
    d.put(b"Subtype", Object::name(b"Type1"));
    d.put(b"BaseFont", Object::name(base.as_bytes()));

    let mut diff = Vec::new();
    let mut last_idx = 0usize;
    for (i, glyph) in names.iter().enumerate().skip(128) {
        if let Some(glyph) = glyph {
            if last_idx + 1 != i {
                diff.push(Object::Int(i as i64));
            }
            last_idx = i;
            diff.push(Object::name(glyph.as_bytes()));
        }
    }
    let mut enc = Dict::new();
    enc.put(b"BaseEncoding", Object::name(b"WinAnsiEncoding"));
    enc.put(b"Differences", Object::Array(Arc::new(diff)));
    d.put(b"Encoding", Object::Dict(Arc::new(enc)));

    if let Some(font) = fontdesc::base14_font(base) {
        let mut first: Option<usize> = None;
        let mut last = 0usize;
        let mut widths = [0i64; 256];
        for (i, glyph) in names.iter().enumerate() {
            let gid = glyph.map_or(0, |n| encode_by_glyph_name(&font, n));
            if gid > 0 {
                if first.is_none() {
                    first = Some(i);
                }
                last = i;
                widths[i] = (fontdesc::advance_glyph(&font, gid, 0) * 1000.0).round() as i64;
            }
        }
        if let Some(f) = first {
            let w_arr: Vec<Object> = widths[f..=last].iter().map(|&w| Object::Int(w)).collect();
            d.put(b"FirstChar", Object::Int(f as i64));
            d.put(b"LastChar", Object::Int(last as i64));
            d.put(b"Widths", Object::Array(Arc::new(w_arr)));
        }
    }

    Object::Dict(Arc::new(d))
}

fn identity_utf16_tounicode() -> Vec<u8> {
    let mut s = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    let highs: Vec<u32> = (0..256).collect();
    for block in highs.chunks(100) {
        s += &format!("{} beginbfrange\n", block.len());
        for hi in block {
            s += &format!("<{hi:02X}00> <{hi:02X}FF> <{hi:02X}00>\n");
        }
        s += "endbfrange\n";
    }
    s += "endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n";
    s.into_bytes()
}

fn add_font_object(doc: &Document, key: &FontKey, tounicode_ref: &mut Option<i32>) -> i32 {
    match key {
        FontKey::WinAnsi(base) => {
            let mut d = Dict::new();
            d.put(b"Type", Object::name(b"Font"));
            d.put(b"Subtype", Object::name(b"Type1"));
            d.put(b"BaseFont", Object::name(base.as_bytes()));
            d.put(b"Encoding", Object::name(b"WinAnsiEncoding"));
            doc.add_object(Object::Dict(Arc::new(d)))
        }
        FontKey::Greek(base) => {
            let obj = build_differences_font(base, &tables::ISO8859_7);
            doc.add_object(obj)
        }
        FontKey::Cyrillic(base) => {
            let obj = build_differences_font(base, &tables::KOI8U);
            doc.add_object(obj)
        }
        FontKey::CustomDiff { base, glyphs } => {
            let mut table = tables::WIN_ANSI;
            for (slot, &gname) in glyphs.iter().enumerate() {
                if let Some(entry) = table.get_mut(128 + slot) {
                    *entry = Some(gname);
                }
            }
            let obj = build_differences_font(base, &table);
            doc.add_object(obj)
        }
        FontKey::Cjk(lang) => {
            let (basefont, encoding, ordering, supplement) = match lang {
                CjkLang::Hant => ("Ming", "UniCNS-UTF16-H", "CNS1", 7),
                CjkLang::Hans => ("Song", "UniGB-UTF16-H", "GB1", 5),
                CjkLang::Japan => ("Mincho", "UniJIS-UTF16-H", "Japan1", 6),
                CjkLang::Korea => ("Batang", "UniKS-UTF16-H", "Korea1", 2),
            };
            let cmap_num = *tounicode_ref.get_or_insert_with(|| {
                let data = identity_utf16_tounicode();
                let mut d = Dict::new();
                d.put(b"Length", Object::Int(data.len() as i64));
                doc.add_stream(d, data)
            });

            let string = |s: &str| Object::String(Arc::from(s.as_bytes()));
            let mut desc = Dict::new();
            desc.put(b"Type", Object::name(b"FontDescriptor"));
            desc.put(b"FontName", Object::name(basefont.as_bytes()));
            desc.put(
                b"FontBBox",
                Object::Array(Arc::new(vec![
                    Object::Int(-200),
                    Object::Int(-200),
                    Object::Int(1200),
                    Object::Int(1200),
                ])),
            );
            desc.put(b"Flags", Object::Int(6));
            desc.put(b"ItalicAngle", Object::Int(0));
            desc.put(b"Ascent", Object::Int(1000));
            desc.put(b"Descent", Object::Int(-200));
            desc.put(b"CapHeight", Object::Int(800));
            desc.put(b"StemV", Object::Int(80));
            let desc_num = doc.add_object(Object::Dict(Arc::new(desc)));

            let mut csi = Dict::new();
            csi.put(b"Registry", string("Adobe"));
            csi.put(b"Ordering", string(ordering));
            csi.put(b"Supplement", Object::Int(supplement));

            let mut cid = Dict::new();
            cid.put(b"Type", Object::name(b"Font"));
            cid.put(b"Subtype", Object::name(b"CIDFontType0"));
            cid.put(b"BaseFont", Object::name(basefont.as_bytes()));
            cid.put(b"CIDSystemInfo", Object::Dict(Arc::new(csi)));
            cid.put(b"FontDescriptor", reference(desc_num));
            cid.put(b"DW", Object::Int(1000));
            let cid_num = doc.add_object(Object::Dict(Arc::new(cid)));

            let mut top = Dict::new();
            top.put(b"Type", Object::name(b"Font"));
            top.put(b"Subtype", Object::name(b"Type0"));
            top.put(b"BaseFont", Object::name(basefont.as_bytes()));
            top.put(b"Encoding", Object::name(encoding.as_bytes()));
            top.put(
                b"DescendantFonts",
                Object::Array(Arc::new(vec![reference(cid_num)])),
            );
            top.put(b"ToUnicode", reference(cmap_num));
            doc.add_object(Object::Dict(Arc::new(top)))
        }
    }
}

fn push_float(out: &mut Vec<u8>, v: f32) {
    crate::print::fmtfloat(v, out);
}

fn push_pdf_literal(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'(');
    for &b in bytes {
        match b {
            b'(' | b')' | b'\\' => {
                out.push(b'\\');
                out.push(b);
            }
            0x20..=0x7e => out.push(b),
            _ => {
                let esc = format!("\\{b:03o}");
                out.extend_from_slice(esc.as_bytes());
            }
        }
    }
    out.push(b')');
}

fn push_pdf_hex_utf16(out: &mut Vec<u8>, units: &[u16]) {
    out.push(b'<');
    for &u in units {
        let hex = format!("{u:04X}");
        out.extend_from_slice(hex.as_bytes());
    }
    out.push(b'>');
}

/// Computes the PDF `Tm` matrix mapping text space (`y` up) to PDF user space
/// for a baseline starting at `origin` along direction `dir` in page space
/// (`y` down).
fn baseline_text_matrix(origin: Point, dir: Point, page_ctm: Matrix) -> Matrix {
    let d = if dir.x == 0.0 && dir.y == 0.0 {
        Point::new(1.0, 0.0)
    } else {
        dir.normalize()
    };
    let glyph_to_page = Matrix {
        a: d.x,
        b: d.y,
        c: d.y,
        d: -d.x,
        e: origin.x,
        f: origin.y,
    };
    glyph_to_page.concat(page_ctm.invert())
}

fn next_unique_font_res_name(fonts: &Dict, counter: &mut usize) -> Vec<u8> {
    loop {
        let cand = format!("FPat{}", *counter).into_bytes();
        *counter += 1;
        if fonts.get(&cand).is_none() {
            return cand;
        }
    }
}

/// Appends `layer` to page `index` with balanced `q`/`Q` wrappers around the
/// existing contents (matching [`crate::ocr::add_layer`]), registering `new_fonts`
/// in the page's `/Resources /Font`.
fn append_patch_layer(
    doc: &Document,
    index: i32,
    new_fonts: &[(Vec<u8>, i32)],
    layer: Vec<u8>,
) -> Result<(), String> {
    let page = doc.page(index).map_err(|e| e.0)?;
    let res = doc.inherited(&page, b"Resources").map_err(|e| e.0)?;
    let mut res = res
        .map(|r| doc.resolve(&r))
        .and_then(|r| r.as_dict().cloned())
        .unwrap_or_default();
    let mut fonts = doc
        .resolve(res.get(b"Font").unwrap_or(&Object::Null))
        .as_dict()
        .cloned()
        .unwrap_or_default();
    for (res_name, obj_num) in new_fonts {
        fonts.put(res_name, reference(*obj_num));
    }
    res.put(b"Font", Object::Dict(Arc::new(fonts)));
    let res_obj = Object::Dict(Arc::new(res));

    let contents = doc.dict_get(&page, b"Contents");
    let stored = doc
        .resolve(&page)
        .as_dict()
        .and_then(|d| d.get(b"Contents").cloned())
        .unwrap_or(Object::Null);
    let (prepend, append) = crate::bake::q_balance(doc, &res_obj, &stored);
    let make_stream = |data: Vec<u8>| {
        let mut d = Dict::new();
        d.put(b"Length", Object::Int(data.len() as i64));
        reference(doc.add_stream(d, data))
    };

    let mut items = Vec::new();
    if prepend > 0 {
        items.push(make_stream(b"q\n".repeat(prepend as usize)));
    }
    match doc.resolve(&contents) {
        Object::Array(a) => items.extend(a.iter().cloned()),
        Object::Null => {}
        _ => items.push(stored),
    }
    let mut tail = b"Q\n".repeat(append.max(0) as usize);
    tail.extend(layer);
    items.push(make_stream(tail));

    crate::edit::edit_page(doc, index, &mut |d| {
        d.put(b"Resources", res_obj.clone());
        d.put(b"Contents", Object::Array(Arc::new(items.clone())));
    })
}

/// Applies surgical in-place text patches onto `doc`'s copy-on-write edit
/// overlay without touching unrelated glyphs, vector graphics, or images.
pub fn apply_patches(
    doc: &Document,
    specs: &[PatchSpec],
    _options: &PatchOptions,
) -> Result<Vec<AppliedPatch>, PatchError> {
    let addrs = parse_and_validate_specs(specs)?;
    if addrs.is_empty() {
        return Ok(Vec::new());
    }

    let page_count = doc.page_count().map_err(|e| PatchError {
        code: "input.page_tree",
        message: e.0,
    })?;

    // Group requested targets by 1-based page so each page is extracted once
    // before any mutation occurs.
    let mut specs_by_page: BTreeMap<i32, Vec<(usize, TargetAddr)>> = BTreeMap::new();
    for (idx, &addr) in addrs.iter().enumerate() {
        if addr.page < 1 || addr.page > page_count {
            return Err(PatchError {
                code: "input.target_not_found",
                message: format!(
                    "target {:?}: page {} out of range (1..={page_count})",
                    specs[idx].target_id, addr.page
                ),
            });
        }
        specs_by_page
            .entry(addr.page)
            .or_default()
            .push((idx, addr));
    }

    let mut store = Store::default();
    let mut resolved_by_page: BTreeMap<i32, (Matrix, Vec<ResolvedTarget>)> = BTreeMap::new();

    for (&page_num, entries) in &specs_by_page {
        let page_index = page_num - 1;
        let page_obj = doc.page(page_index).map_err(|e| PatchError {
            code: "input.page_tree",
            message: e.0,
        })?;
        let (_, page_ctm) = doc.page_transform(&page_obj).map_err(|e| PatchError {
            code: "input.page_tree",
            message: e.0,
        })?;
        let (res, next_store) = stext::extract_page(doc, &page_obj, StextOptions::default(), store);
        store = next_store;
        let stext_page = res.map_err(|message| PatchError {
            code: "input.page_tree",
            message,
        })?;

        let mut page_targets = Vec::with_capacity(entries.len());
        for &(spec_idx, addr) in entries {
            let spec = &specs[spec_idx];
            let not_found = || PatchError {
                code: "input.target_not_found",
                message: format!("target {:?} not found on page {page_num}", spec.target_id),
            };
            let Some(Block::Text(tb)) = stext_page.blocks.get(addr.block - 1) else {
                return Err(not_found());
            };
            let Some(line) = tb.lines.get(addr.line - 1) else {
                return Err(not_found());
            };
            let line_spans = extract::spans(line);
            let (raw_slice, fallback_bbox, style) = match addr.span {
                Some(s1) => {
                    let Some(span) = line_spans.get(s1 - 1) else {
                        return Err(not_found());
                    };
                    let slice = line.chars.get(span.start..span.end).ok_or_else(not_found)?;
                    (slice, span.bbox, span.style.clone())
                }
                None => {
                    let Some(first_span) = line_spans.first() else {
                        return Err(not_found());
                    };
                    (line.chars.as_slice(), line.bbox, first_span.style.clone())
                }
            };
            if raw_slice.is_empty() {
                return Err(not_found());
            }

            let trimmed = trim_synthetic_edges(raw_slice);
            let first_char = trimmed
                .iter()
                .find(|c| c.flags & char_flags::SYNTHETIC == 0)
                .or_else(|| trimmed.first())
                .ok_or_else(not_found)?;

            let old_text = extract::chars_text(trimmed);
            let old_bbox = ink_bbox(trimmed, fallback_bbox);
            let origin = first_char.origin;
            let dir = line.dir;

            let mut erase_quads_pdf: Vec<Quad> = trimmed
                .iter()
                .filter(|c| c.flags & char_flags::SYNTHETIC == 0)
                .map(|c| crate::redact::quad_to_pdf(c.quad, page_ctm))
                .collect();
            if erase_quads_pdf.is_empty() {
                erase_quads_pdf.extend(
                    trimmed
                        .iter()
                        .map(|c| crate::redact::quad_to_pdf(c.quad, page_ctm)),
                );
            }

            let base14_name = match_base14(&style);
            let cjk_lang = guess_cjk_lang(&style.font);
            let (_, subsetted) = strip_subset_prefix(&style.font);
            let exact_name_match =
                !subsetted && fontdesc::clean_font_name(&style.font) == Some(base14_name);
            let page_had_exact_base14 =
                exact_name_match && page_has_matching_base14(doc, &page_obj, base14_name);

            page_targets.push(ResolvedTarget {
                spec_index: spec_idx,
                id: spec.target_id.clone(),
                page: page_num,
                old_text,
                new_text: spec.new_text.clone(),
                old_bbox,
                origin,
                dir,
                style,
                erase_quads_pdf,
                base14_name,
                cjk_lang,
                page_had_exact_base14,
            });
        }
        resolved_by_page.insert(page_num, (page_ctm, page_targets));
    }

    // Shared cache across pages so identical replacement fonts share one PDF object.
    let mut doc_font_cache: HashMap<FontKey, i32> = HashMap::new();
    let mut shared_tounicode: Option<i32> = None;
    let mut applied_by_index: Vec<Option<AppliedPatch>> = vec![None; specs.len()];

    for (page_num, (page_ctm, targets)) in resolved_by_page {
        let page_index = page_num - 1;

        // Step 1: Surgically erase only the target glyphs on this page.
        let mut all_erase_quads = Vec::new();
        for t in &targets {
            all_erase_quads.extend_from_slice(&t.erase_quads_pdf);
        }
        let clean_opts = content_clean::Options {
            recurse: false,
            instance_forms: true,
            ascii: true,
            newlines: false,
            sanitize: true,
        };
        let mut eraser = SpanEraser {
            quads: all_erase_quads,
        };
        filter_page_contents(doc, page_index, &clean_opts, &mut eraser).map_err(|message| {
            PatchError {
                code: "input.page_tree",
                message,
            }
        })?;

        // Inspect post-filter `/Resources /Font` so new resource names never collide.
        let post_page = doc.page(page_index).map_err(|e| PatchError {
            code: "input.page_tree",
            message: e.0,
        })?;
        let existing_fonts = doc
            .inherited(&post_page, b"Resources")
            .ok()
            .flatten()
            .map(|r| doc.resolve(&r))
            .and_then(|r| r.as_dict().cloned())
            .map(|rd| doc.resolve(rd.get(b"Font").unwrap_or(&Object::Null)))
            .and_then(|f| f.as_dict().cloned())
            .unwrap_or_default();
        let mut res_counter = 0usize;
        let mut page_font_names: HashMap<FontKey, Vec<u8>> = HashMap::new();
        let mut page_new_fonts: Vec<(Vec<u8>, i32)> = Vec::new();

        // Step 2: Synthesize replacement text layer for non-empty replacements.
        let mut layer = Vec::new();
        let mut has_replacement_ops = false;

        for t in targets {
            if t.new_text.is_empty() {
                applied_by_index[t.spec_index] = Some(AppliedPatch {
                    id: t.id,
                    page: t.page,
                    old_text: t.old_text,
                    new_text: t.new_text,
                    old_bbox: rect_array(t.old_bbox),
                    new_bbox: None,
                    font_used: t.base14_name.to_string(),
                    fallback_font: false,
                });
                continue;
            }

            let (runs, pure_winansi) =
                encode_replacement_runs(&t.new_text, t.base14_name, t.cjk_lang);
            let fallback_font = !(t.page_had_exact_base14 && pure_winansi);
            let font_used = runs
                .first()
                .map(|r| match r {
                    EncodedRun::Byte { key, .. } | EncodedRun::Utf16 { key, .. } => {
                        key.display_name().to_string()
                    }
                })
                .unwrap_or_else(|| t.base14_name.to_string());

            if !runs.is_empty() {
                if !has_replacement_ops {
                    layer.extend_from_slice(b"q\nBT\n0 Tr\n");
                    has_replacement_ops = true;
                }
                let r = ((t.style.argb >> 16) & 0xff) as f32 / 255.0;
                let g = ((t.style.argb >> 8) & 0xff) as f32 / 255.0;
                let b = (t.style.argb & 0xff) as f32 / 255.0;
                push_float(&mut layer, r);
                layer.push(b' ');
                push_float(&mut layer, g);
                layer.push(b' ');
                push_float(&mut layer, b);
                layer.extend_from_slice(b" rg\n");

                let tm = baseline_text_matrix(t.origin, t.dir, page_ctm);
                for comp in [tm.a, tm.b, tm.c, tm.d, tm.e, tm.f] {
                    push_float(&mut layer, comp);
                    layer.push(b' ');
                }
                layer.extend_from_slice(b"Tm\n");

                let font_size = if t.style.size > 0.0 {
                    t.style.size
                } else {
                    12.0
                };

                for run in &runs {
                    let key = match run {
                        EncodedRun::Byte { key, .. } | EncodedRun::Utf16 { key, .. } => key,
                    };
                    let res_name = match page_font_names.get(key) {
                        Some(existing) => existing.clone(),
                        None => {
                            let obj_num = match doc_font_cache.get(key) {
                                Some(&n) => n,
                                None => {
                                    let n = add_font_object(doc, key, &mut shared_tounicode);
                                    doc_font_cache.insert(key.clone(), n);
                                    n
                                }
                            };
                            let fresh =
                                next_unique_font_res_name(&existing_fonts, &mut res_counter);
                            page_new_fonts.push((fresh.clone(), obj_num));
                            page_font_names.insert(key.clone(), fresh.clone());
                            fresh
                        }
                    };
                    layer.push(b'/');
                    layer.extend_from_slice(&res_name);
                    layer.push(b' ');
                    push_float(&mut layer, font_size);
                    layer.extend_from_slice(b" Tf\n");

                    match run {
                        EncodedRun::Byte { bytes, .. } => push_pdf_literal(&mut layer, bytes),
                        EncodedRun::Utf16 { units, .. } => push_pdf_hex_utf16(&mut layer, units),
                    }
                    layer.extend_from_slice(b" Tj\n");
                }
            }

            applied_by_index[t.spec_index] = Some(AppliedPatch {
                id: t.id,
                page: t.page,
                old_text: t.old_text,
                new_text: t.new_text,
                old_bbox: rect_array(t.old_bbox),
                new_bbox: None,
                font_used,
                fallback_font,
            });
        }

        if has_replacement_ops {
            layer.extend_from_slice(b"ET\nQ\n");
            append_patch_layer(doc, page_index, &page_new_fonts, layer).map_err(|message| {
                PatchError {
                    code: "input.page_tree",
                    message,
                }
            })?;
        }
    }

    let mut out = Vec::with_capacity(specs.len());
    for slot in applied_by_index {
        let Some(p) = slot else {
            return Err(PatchError {
                code: "input.target_not_found",
                message: "internal target resolution mismatch".into(),
            });
        };
        out.push(p);
    }
    Ok(out)
}

fn shrink_for_collision(r: Rect, dx: f32, dy: f32) -> Rect {
    if !r.is_valid() || r.is_empty() {
        return r;
    }
    let shrink_axis = |a: f32, b: f32, d: f32| -> (f32, f32) {
        if b - a > 2.0 * d + 0.1 {
            (a + d, b - d)
        } else {
            let mid = 0.5 * (a + b);
            let half = 0.05_f32.min(0.25 * (b - a));
            (mid - half, mid + half)
        }
    };
    let (x0, x1) = shrink_axis(r.x0, r.x1, dx);
    let (y0, y1) = shrink_axis(r.y0, r.y1, dy);
    Rect::new(x0, y0, x1, y1)
}

/// How far the intervals `a0..a1` and `b0..b1` overlap; negative for a gap.
fn interval_overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    a1.min(b1) - a0.max(b0)
}

/// Whether box `a` (once `a_old`) overlaps box `b` (once `b_old`) where the
/// old boxes did not: the boxes meet, and on some axis they overlap more
/// than before while on the other they are not a thin band the old ones
/// shared already (a line's box over the next line's).
fn overlaps_deeper(a: Rect, a_old: Rect, b: Rect, b_old: Rect) -> bool {
    const TOL: f32 = 0.5;
    if a.intersect(b).is_empty() {
        return false;
    }
    let ox = interval_overlap(a.x0, a.x1, b.x0, b.x1);
    let oy = interval_overlap(a.y0, a.y1, b.y0, b.y1);
    let ox_old = interval_overlap(a_old.x0, a_old.x1, b_old.x0, b_old.x1).max(0.0);
    let oy_old = interval_overlap(a_old.y0, a_old.y1, b_old.y0, b_old.y1).max(0.0);
    let deeper_x = ox > ox_old + TOL;
    let deeper_y = oy > oy_old + TOL;
    let thin_x = ox < 0.5 * (a.x1 - a.x0).min(b.x1 - b.x0);
    let thin_y = oy < 0.5 * (a.y1 - a.y0).min(b.y1 - b.y0);
    let kept_band = (thin_y && oy_old > 0.0 && !deeper_y) || (thin_x && ox_old > 0.0 && !deeper_x);
    (deeper_x || deeper_y) && !kept_band
}

/// Normalizes whitespace runs to single `' '` for resilient comparison across
/// `stext` synthetic space inference.
fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.trim().chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

/// Locates `wanted` (already trimmed and non-empty) on `stext_page` near `old_bbox`,
/// returning the tight ink bounding box of the matched replacement characters.
fn find_replacement_bbox(stext_page: &StextPage, wanted: &str, old_bbox: Rect) -> Option<Rect> {
    let wanted_norm = normalize_ws(wanted);
    let old_w = (old_bbox.x1 - old_bbox.x0).max(1.0);
    let old_h = (old_bbox.y1 - old_bbox.y0).max(1.0);
    let max_start_dist = (old_w.max(old_h) * 2.5).max(36.0);

    let score_rect = |cand: Rect| -> Option<f32> {
        if !cand.is_valid() || cand.is_empty() {
            return None;
        }
        // Compare start edge (x0, y0, y1) so widened replacements starting at the
        // same baseline origin have near-zero distance.
        let dx = (cand.x0 - old_bbox.x0).abs();
        let dy0 = (cand.y0 - old_bbox.y0).abs();
        let dy1 = (cand.y1 - old_bbox.y1).abs();
        let dy = dy0.min(dy1);
        let dist = dx.hypot(dy);
        (dist <= max_start_dist).then_some(dist)
    };

    let mut best: Option<(f32, Rect)> = None;
    let mut consider = |cand: Rect| {
        if let Some(score) = score_rect(cand)
            && best.is_none_or(|(best_score, _)| score < best_score)
        {
            best = Some((score, cand));
        }
    };

    for block in &stext_page.blocks {
        let Block::Text(tb) = block else {
            continue;
        };
        for line in &tb.lines {
            // 1. Exact span match.
            for span in extract::spans(line) {
                let slice = line.chars.get(span.start..span.end).unwrap_or(&[]);
                let trimmed = trim_synthetic_edges(slice);
                let text = extract::chars_text(trimmed);
                if text.trim() == wanted || normalize_ws(&text) == wanted_norm {
                    consider(ink_bbox(trimmed, span.bbox));
                }
            }

            // 2. Substring match within the line (handles `stext` merging adjacent
            // same-style spans onto one line while isolating only `wanted`'s quads).
            let mut line_str = String::new();
            let mut byte_to_char: Vec<(usize, usize)> = Vec::with_capacity(line.chars.len());
            for (ci, ch) in line.chars.iter().enumerate() {
                let start_b = line_str.len();
                let c = crate::stext::rune_char(ch.c);
                line_str.push(c);
                byte_to_char.push((start_b, ci));
            }
            let total_bytes = line_str.len();

            let mut search_from = 0usize;
            while let Some(rel) = line_str.get(search_from..).and_then(|s| s.find(wanted)) {
                let match_start = search_from + rel;
                let match_end = match_start + wanted.len();
                let c_start = byte_to_char.partition_point(|&(b, _)| b < match_start);
                let c_end = byte_to_char.partition_point(|&(b, _)| b < match_end);
                if let Some(sub) = line.chars.get(c_start..c_end)
                    && !sub.is_empty()
                {
                    let trimmed = trim_synthetic_edges(sub);
                    consider(ink_bbox(trimmed, line.bbox));
                }
                let step = line_str
                    .get(match_start..total_bytes)
                    .and_then(|s| s.chars().next())
                    .map_or(1, char::len_utf8);
                search_from = match_start + step;
            }
        }
    }

    best.map(|(_, r)| r)
}

/// Checks whether `old_trimmed` is still sitting unreplaced at `old_bbox`.
fn old_text_remained_at_bbox(
    stext_page: &StextPage,
    old_trimmed: &str,
    new_trimmed: &str,
    old_bbox: Rect,
) -> bool {
    if old_trimmed.is_empty() || old_trimmed == new_trimmed {
        return false;
    }
    let old_norm = normalize_ws(old_trimmed);
    let new_norm = normalize_ws(new_trimmed);
    let probe = shrink_for_collision(old_bbox, 0.5, 1.0);

    for block in &stext_page.blocks {
        let Block::Text(tb) = block else {
            continue;
        };
        for line in &tb.lines {
            let line_norm = normalize_ws(&extract::chars_text(&line.chars));
            for span in extract::spans(line) {
                let slice = line.chars.get(span.start..span.end).unwrap_or(&[]);
                let trimmed = trim_synthetic_edges(slice);
                let sb = ink_bbox(trimmed, span.bbox);
                if sb.intersect(probe).is_empty() {
                    continue;
                }
                let span_norm = normalize_ws(&extract::chars_text(trimmed));
                if span_norm == old_norm {
                    // Guard against prefix/substring expansions (e.g. "$50" -> "$50,000")
                    // where `new_trimmed` contains `old_trimmed` on the same line.
                    if !new_norm.is_empty()
                        && new_norm.contains(&old_norm)
                        && line_norm.contains(&new_norm)
                    {
                        continue;
                    }
                    return true;
                }
            }
        }
    }

    // Also check combined characters whose centers still fall inside `probe`, in
    // case a multi-span line patch left the old glyphs intact.
    if new_norm.is_empty() || !new_norm.contains(&old_norm) {
        let mut inside_chars: Vec<StextChar> = Vec::new();
        for block in &stext_page.blocks {
            let Block::Text(tb) = block else {
                continue;
            };
            for line in &tb.lines {
                for ch in &line.chars {
                    if ch.flags & char_flags::SYNTHETIC != 0 {
                        continue;
                    }
                    let cb = Rect::from_quad(ch.quad);
                    let cx = 0.5 * (cb.x0 + cb.x1);
                    let cy = 0.5 * (cb.y0 + cb.y1);
                    if cx >= old_bbox.x0
                        && cx <= old_bbox.x1
                        && cy >= old_bbox.y0
                        && cy <= old_bbox.y1
                    {
                        inside_chars.push(ch.clone());
                    }
                }
            }
        }
        if !inside_chars.is_empty() {
            let combined = normalize_ws(&extract::chars_text(&inside_chars));
            if combined == old_norm {
                return true;
            }
        }
    }

    false
}

/// Reopens `patched_bytes`, runs structured text extraction on every affected
/// page, populates `new_bbox` on each [`AppliedPatch`], and checks font fallback,
/// text presence, old-text removal, page overflow, and neighbor collisions.
pub fn verify_patched_bytes(
    patched_bytes: &[u8],
    password: Option<&str>,
    applied: &mut [AppliedPatch],
    untouched_bboxes_by_page: &HashMap<i32, Vec<(String, Rect)>>,
    options: &PatchOptions,
) -> Result<PatchVerification, PatchError> {
    let doc = Document::open(patched_bytes.to_vec()).map_err(|e| PatchError {
        code: e.code,
        message: e.message,
    })?;
    if doc.needs_password() || password.is_some() {
        let pw = password.unwrap_or("");
        let access = doc.authenticate(pw).map_err(|message| PatchError {
            code: "input.password",
            message,
        })?;
        if access == 0 {
            return Err(PatchError {
                code: "input.password",
                message: "cannot authenticate password".into(),
            });
        }
    }

    if applied.is_empty() {
        return Ok(PatchVerification {
            verified: true,
            findings: Vec::new(),
        });
    }

    let mut pages_needed: Vec<i32> = applied.iter().map(|p| p.page).collect();
    pages_needed.sort_unstable();
    pages_needed.dedup();

    let mut store = Store::default();
    let mut extracted_pages: HashMap<i32, (StextPage, StextPage)> = HashMap::new();
    for page_num in pages_needed {
        let page_obj = doc.page(page_num - 1).map_err(|e| PatchError {
            code: "input.page_tree",
            message: e.0,
        })?;
        let (vis_res, s1) = stext::extract_page(&doc, &page_obj, StextOptions::default(), store);
        let vis_page = vis_res.map_err(|message| PatchError {
            code: "input.page_tree",
            message,
        })?;
        // Extract unclipped as well so glyphs overflowing the page edge keep
        // their full text and true unclipped bounding box for `check_overflow`.
        let unclipped_opts = StextOptions {
            flags: 0,
            clip: Rect::INFINITE,
        };
        let (unc_res, s2) = stext::extract_page(&doc, &page_obj, unclipped_opts, s1);
        store = s2;
        let unc_page = unc_res.map_err(|message| PatchError {
            code: "input.page_tree",
            message,
        })?;
        extracted_pages.insert(page_num, (vis_page, unc_page));
    }

    let mut findings = Vec::new();

    for patch in applied.iter_mut() {
        // 1. Strict font policy check.
        if options.strict_font && patch.fallback_font {
            findings.push(PatchFinding {
                code: "verify.font_fallback_disallowed".into(),
                id: patch.id.clone(),
                page: patch.page,
                message: format!(
                    "patch {:?} required fallback font {:?}, disallowed by strict_font",
                    patch.id, patch.font_used
                ),
            });
        }

        let Some((vis_page, unc_page)) = extracted_pages.get(&patch.page) else {
            continue;
        };
        let old_rect = rect_from_array(patch.old_bbox);
        let new_trimmed = patch.new_text.trim();
        let old_trimmed = patch.old_text.trim();

        // 2. Replacement text presence & `new_bbox` measurement.
        if !new_trimmed.is_empty() {
            match find_replacement_bbox(unc_page, new_trimmed, old_rect)
                .or_else(|| find_replacement_bbox(vis_page, new_trimmed, old_rect))
            {
                Some(nb) => {
                    patch.new_bbox = Some(rect_array(nb));
                }
                None => {
                    patch.new_bbox = None;
                    findings.push(PatchFinding {
                        code: "verify.text_missing".into(),
                        id: patch.id.clone(),
                        page: patch.page,
                        message: format!(
                            "replacement text {:?} for {:?} was not extracted near expected position",
                            patch.new_text, patch.id
                        ),
                    });
                }
            }
        } else {
            patch.new_bbox = None;
        }

        // 3. Old text removal check.
        if old_trimmed != new_trimmed
            && !old_trimmed.is_empty()
            && old_text_remained_at_bbox(vis_page, old_trimmed, new_trimmed, old_rect)
        {
            findings.push(PatchFinding {
                code: "verify.old_text_remained".into(),
                id: patch.id.clone(),
                page: patch.page,
                message: format!(
                    "old text {:?} remained at target {:?} bbox",
                    patch.old_text, patch.id
                ),
            });
        }

        // 4. Page bounds overflow check (1.0 pt tolerance).
        if options.check_overflow
            && let Some(nb_arr) = patch.new_bbox
        {
            let nb = rect_from_array(nb_arr);
            let bounds = unc_page.mediabox;
            let tol = 1.0_f32;
            if bounds.is_valid()
                && (nb.x0 < bounds.x0 - tol
                    || nb.y0 < bounds.y0 - tol
                    || nb.x1 > bounds.x1 + tol
                    || nb.y1 > bounds.y1 + tol)
            {
                findings.push(PatchFinding {
                    code: "verify.page_overflow".into(),
                    id: patch.id.clone(),
                    page: patch.page,
                    message: format!(
                        "replacement bbox [{:.2}, {:.2}, {:.2}, {:.2}] overflows page bounds [{:.2}, {:.2}, {:.2}, {:.2}]",
                        nb.x0, nb.y0, nb.x1, nb.y1, bounds.x0, bounds.y0, bounds.x1, bounds.y1
                    ),
                });
            }
        }
    }

    // 5. Neighboring span collision check: the replacement (shrunk by 0.5 pt
    // horizontally and 1.5 pt vertically) overlapping a span deeper than the
    // original did. Boxes of single-spaced lines overlap by their ascenders
    // and descenders; a replacement that keeps that overlap collides with
    // nothing.
    if options.check_collisions {
        let shrink = |a: [f32; 4]| shrink_for_collision(rect_from_array(a), 0.5, 1.5);
        for i in 0..applied.len() {
            let Some(nb_arr) = applied[i].new_bbox else {
                continue;
            };
            let new_i = shrink(nb_arr);
            let old_i = shrink(applied[i].old_bbox);
            let page_num = applied[i].page;

            if let Some(untouched) = untouched_bboxes_by_page.get(&page_num) {
                for (other_id, other_rect) in untouched {
                    if overlaps_deeper(new_i, old_i, *other_rect, *other_rect) {
                        findings.push(PatchFinding {
                            code: "verify.collision".into(),
                            id: applied[i].id.clone(),
                            page: page_num,
                            message: format!(
                                "replacement for {:?} collides with untouched span {:?}",
                                applied[i].id, other_id
                            ),
                        });
                    }
                }
            }

            // Also detect collisions between two patches applied on the same page.
            for j in (i + 1)..applied.len() {
                if applied[j].page != page_num {
                    continue;
                }
                let Some(other_arr) = applied[j].new_bbox else {
                    continue;
                };
                if overlaps_deeper(new_i, old_i, shrink(other_arr), shrink(applied[j].old_bbox)) {
                    findings.push(PatchFinding {
                        code: "verify.collision".into(),
                        id: applied[i].id.clone(),
                        page: page_num,
                        message: format!(
                            "replacement for {:?} collides with patched span {:?}",
                            applied[i].id, applied[j].id
                        ),
                    });
                }
            }
        }
    }

    Ok(PatchVerification {
        verified: findings.is_empty(),
        findings,
    })
}

/// Snapshots untouched spans on affected pages, applies `specs` onto `doc`,
/// serializes the updated document with `write_opts`, and runs
/// [`verify_patched_bytes`] on the serialized bytes.
pub fn patch_and_verify(
    doc: &Document,
    specs: &[PatchSpec],
    options: &PatchOptions,
    write_opts: &WriteOptions,
    password: Option<&str>,
) -> Result<(Vec<u8>, PatchResult), PatchError> {
    let untouched = snapshot_untouched_spans(doc, specs)?;
    let mut applied = apply_patches(doc, specs, options)?;
    let mut effective_write = write_opts.clone();
    if !effective_write.incremental {
        effective_write.garbage = effective_write.garbage.max(1);
    }
    let bytes = crate::write::save(doc, &effective_write).map_err(|e| PatchError {
        code: "input.write",
        message: e.message,
    })?;
    let verification = verify_patched_bytes(&bytes, password, &mut applied, &untouched, options)?;
    Ok((
        bytes,
        PatchResult {
            patches: applied,
            verification: Some(verification),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{doc_bytes, page_doc, stream};

    const HELVETICA: &[u8] =
        b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
    const HELVETICA_BOLD: &[u8] =
        b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica-Bold/Encoding/WinAnsiEncoding>>";
    const SUBSET_INTER: &[u8] =
        b"<</Type/Font/Subtype/Type1/BaseFont/ABCDEF+Inter-Regular/Encoding/WinAnsiEncoding>>";

    fn extract_all_spans(doc: &Document, page_index: i32) -> Vec<(String, String, Style, Rect)> {
        let page_obj = doc.page(page_index).unwrap();
        let (res, _) =
            stext::extract_page(doc, &page_obj, StextOptions::default(), Store::default());
        let stext_page = res.unwrap();
        let p1 = page_index + 1;
        let mut out = Vec::new();
        for (bi, block) in stext_page.blocks.iter().enumerate() {
            let Block::Text(tb) = block else {
                continue;
            };
            for (li, line) in tb.lines.iter().enumerate() {
                for (si, span) in extract::spans(line).into_iter().enumerate() {
                    let id = format!("p{p1}.b{}.l{}.s{}", bi + 1, li + 1, si + 1);
                    let slice = &line.chars[span.start..span.end];
                    let trimmed = trim_synthetic_edges(slice);
                    let text = extract::chars_text(trimmed);
                    out.push((id, text, span.style, ink_bbox(trimmed, span.bbox)));
                }
            }
        }
        out
    }

    #[test]
    fn surgical_replacement_preserves_color_size_position_and_neighbors() {
        // Draw a background rectangle + two text items:
        // "Invoice Total:" in black bold at x=72, y=700
        // "$14,500" in red (#cc3319 -> ~0.8, 0.2, 0.1) 14pt at x=220, y=700
        // "Thank you" on a lower line at x=72, y=660
        let content = b"0.9 0.9 0.9 rg 60 640 400 90 re f \
            BT /F2 12 Tf 0 0 0 rg 1 0 0 1 72 700 Tm (Invoice Total:) Tj ET \
            BT /F1 14 Tf 0.8 0.2 0.1 rg 1 0 0 1 220 700 Tm ($14,500) Tj ET \
            BT /F1 11 Tf 0 0 0 rg 1 0 0 1 72 660 Tm (Thank you) Tj ET";
        let doc = page_doc(
            content,
            "/Font<</F1 5 0 R/F2 6 0 R>>",
            &[HELVETICA.to_vec(), HELVETICA_BOLD.to_vec()],
        );

        let before = extract_all_spans(&doc, 0);
        let amount = before
            .iter()
            .find(|(_, t, _, _)| t == "$14,500")
            .expect("target span present");
        let target_id = amount.0.clone();
        let orig_style = amount.2.clone();
        let orig_bbox = amount.3;

        let specs = [PatchSpec {
            target_id: target_id.clone(),
            new_text: "$15,400".into(),
        }];
        let opts = PatchOptions {
            strict_font: true,
            ..PatchOptions::default()
        };
        let (bytes, result) =
            patch_and_verify(&doc, &specs, &opts, &WriteOptions::default(), None).unwrap();

        let ver = result.verification.as_ref().unwrap();
        assert!(ver.verified, "unexpected findings: {:?}", ver.findings);
        assert_eq!(result.patches.len(), 1);
        let p = &result.patches[0];
        assert_eq!(p.old_text, "$14,500");
        assert_eq!(p.new_text, "$15,400");
        assert_eq!(p.font_used, "Helvetica");
        assert!(!p.fallback_font);
        let new_bbox = p.new_bbox.expect("new_bbox populated");
        assert!((new_bbox[0] - orig_bbox.x0).abs() < 1.0);
        assert!((new_bbox[1] - orig_bbox.y0).abs() < 1.0);

        // Re-open and confirm color, size, neighboring text, and vector rectangle survived.
        let reopened = Document::open(bytes).unwrap();
        let after = extract_all_spans(&reopened, 0);
        assert!(after.iter().any(|(_, t, _, _)| t == "Invoice Total:"));
        assert!(after.iter().any(|(_, t, _, _)| t == "Thank you"));
        assert!(!after.iter().any(|(_, t, _, _)| t.contains("$14,500")));

        let replaced = after
            .iter()
            .find(|(_, t, _, _)| t == "$15,400")
            .expect("replacement span extracted");
        assert!((replaced.2.size - 14.0).abs() < 0.01);
        assert_eq!(replaced.2.argb, orig_style.argb);

        // Confirm the background rectangle `60 640 400 90 re f` was not erased.
        let page_obj = reopened.page(0).unwrap();
        let vec_opts = StextOptions {
            flags: stext::flags::COLLECT_VECTORS,
            clip: Rect::INFINITE,
        };
        let (vec_page, _) = stext::extract_page(&reopened, &page_obj, vec_opts, Store::default());
        let has_vector = vec_page
            .unwrap()
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Vector(_)));
        assert!(has_vector, "background vector art must remain intact");
    }

    #[test]
    fn collision_detection_catches_wide_replacement_hitting_neighbor() {
        // Tight two-column row: "$50" at x=72 and "RightColumn" at x=125 on the same y=700 baseline.
        let content = b"BT /F1 12 Tf 1 0 0 1 72 700 Tm ($50) Tj ET \
            BT /F1 12 Tf 1 0 0 1 125 700 Tm (RightColumn) Tj ET";
        let doc = page_doc(content, "/Font<</F1 5 0 R>>", &[HELVETICA.to_vec()]);

        let spans = extract_all_spans(&doc, 0);
        let left_id = spans
            .iter()
            .find(|(_, t, _, _)| t == "$50")
            .expect("left column span")
            .0
            .clone();
        let right_id = spans
            .iter()
            .find(|(_, t, _, _)| t == "RightColumn")
            .expect("right column span")
            .0
            .clone();

        let specs = [PatchSpec {
            target_id: left_id,
            new_text: "$50,000,000.00".into(),
        }];
        let (_, result) = patch_and_verify(
            &doc,
            &specs,
            &PatchOptions::default(),
            &WriteOptions::default(),
            None,
        )
        .unwrap();

        let ver = result.verification.unwrap();
        assert!(!ver.verified);
        let collision = ver
            .findings
            .iter()
            .find(|f| f.code == "verify.collision")
            .expect("expected verify.collision finding");
        assert!(
            collision.message.contains(&right_id),
            "collision message should name collided span {right_id}: {}",
            collision.message
        );
        // Must NOT falsely report `verify.old_text_remained` even though "$50,000,000.00" starts with "$50".
        assert!(
            !ver.findings
                .iter()
                .any(|f| f.code == "verify.old_text_remained"),
            "unexpected findings: {:?}",
            ver.findings
        );
    }

    #[test]
    fn single_spaced_lines_whose_boxes_overlap_do_not_collide() {
        // 12pt Helvetica on 11pt leading: each line's box overlaps the next.
        let content = b"BT /F1 12 Tf 11 TL 72 700 Td (First line of the paragraph) Tj \
            T* (Second line of the paragraph goes on longer) Tj \
            T* (Third line of the paragraph) Tj ET";
        let doc = page_doc(content, "/Font<</F1 5 0 R>>", &[HELVETICA.to_vec()]);
        let spans = extract_all_spans(&doc, 0);
        let ids: Vec<String> = spans.iter().map(|s| s.0.clone()).collect();
        assert_eq!(ids.len(), 3, "{spans:?}");
        let (r1, r2) = (spans[0].3, spans[1].3);
        assert!(!r1.intersect(r2).is_empty(), "the lines' boxes overlap");

        for specs in [
            vec![(ids[0].as_str(), "First line of the paragraph")],
            vec![(ids[0].as_str(), "First line, a little longer")],
            vec![(ids[0].as_str(), "One"), (ids[1].as_str(), "Two")],
        ] {
            let specs: Vec<PatchSpec> = specs
                .into_iter()
                .map(|(id, t)| PatchSpec {
                    target_id: id.into(),
                    new_text: t.into(),
                })
                .collect();
            let d = doc.fork_clean();
            let (_, result) = patch_and_verify(
                &d,
                &specs,
                &PatchOptions::default(),
                &WriteOptions::default(),
                None,
            )
            .unwrap();
            let ver = result.verification.unwrap();
            assert!(ver.verified, "{specs:?}: {:?}", ver.findings);
        }
    }

    #[test]
    fn page_overflow_detection_catches_text_running_past_right_edge() {
        // Narrow 120pt-wide page (`MediaBox [0 0 120 200]`).
        let objs = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 120 200]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>".to_vec(),
            stream("", b"BT /F1 12 Tf 1 0 0 1 70 100 Tm (Hi) Tj ET"),
            HELVETICA.to_vec(),
        ];
        let doc = doc_bytes(&objs);

        let spans = extract_all_spans(&doc, 0);
        let target_id = spans[0].0.clone();

        let specs = [PatchSpec {
            target_id,
            new_text: "This replacement runs way off the narrow page".into(),
        }];
        let (_, result) = patch_and_verify(
            &doc,
            &specs,
            &PatchOptions::default(),
            &WriteOptions::default(),
            None,
        )
        .unwrap();

        let ver = result.verification.unwrap();
        assert!(!ver.verified);
        assert!(
            ver.findings
                .iter()
                .any(|f| f.code == "verify.page_overflow"),
            "expected verify.page_overflow in {:?}",
            ver.findings
        );
        assert!(
            !ver.findings.iter().any(|f| f.code == "verify.text_missing"),
            "unclipped extraction must still find overflowing text: {:?}",
            ver.findings
        );
    }

    #[test]
    fn deletion_cleanly_removes_only_target_span() {
        let content = b"BT /F1 12 Tf 1 0 0 1 72 700 Tm (Keep Left) Tj ET \
            BT /F1 12 Tf 1 0 0 1 200 700 Tm (Delete Me) Tj ET \
            BT /F1 12 Tf 1 0 0 1 320 700 Tm (Keep Right) Tj ET";
        let doc = page_doc(content, "/Font<</F1 5 0 R>>", &[HELVETICA.to_vec()]);

        let spans = extract_all_spans(&doc, 0);
        let del_id = spans
            .iter()
            .find(|(_, t, _, _)| t == "Delete Me")
            .expect("target span")
            .0
            .clone();

        let specs = [PatchSpec {
            target_id: del_id,
            new_text: String::new(),
        }];
        let (bytes, result) = patch_and_verify(
            &doc,
            &specs,
            &PatchOptions::default(),
            &WriteOptions::default(),
            None,
        )
        .unwrap();

        let ver = result.verification.unwrap();
        assert!(ver.verified, "unexpected findings: {:?}", ver.findings);
        assert_eq!(result.patches[0].new_bbox, None);

        let reopened = Document::open(bytes).unwrap();
        let after = extract_all_spans(&reopened, 0);
        let texts: Vec<&str> = after.iter().map(|(_, t, _, _)| t.as_str()).collect();
        assert_eq!(texts, vec!["Keep Left", "Keep Right"]);
    }

    #[test]
    fn validation_and_strict_font_checks_report_expected_codes() {
        let content = b"BT /F1 12 Tf 1 0 0 1 72 700 Tm (Subset Text) Tj ET";
        let doc = page_doc(content, "/Font<</F1 5 0 R>>", &[SUBSET_INTER.to_vec()]);

        // Malformed ID -> usage.bad_id
        let bad = apply_patches(
            &doc,
            &[PatchSpec {
                target_id: "p0.b1.l1".into(),
                new_text: "X".into(),
            }],
            &PatchOptions::default(),
        )
        .unwrap_err();
        assert_eq!(bad.code, "usage.bad_id");

        // Overlapping line + span -> usage.duplicate_target
        let dup = apply_patches(
            &doc,
            &[
                PatchSpec {
                    target_id: "p1.b1.l1".into(),
                    new_text: "A".into(),
                },
                PatchSpec {
                    target_id: "p1.b1.l1.s1".into(),
                    new_text: "B".into(),
                },
            ],
            &PatchOptions::default(),
        )
        .unwrap_err();
        assert_eq!(dup.code, "usage.duplicate_target");

        // Missing span -> input.target_not_found
        let missing = apply_patches(
            &doc,
            &[PatchSpec {
                target_id: "p1.b9.l1.s1".into(),
                new_text: "X".into(),
            }],
            &PatchOptions::default(),
        )
        .unwrap_err();
        assert_eq!(missing.code, "input.target_not_found");

        // Subsetted font with strict_font=true -> verify.font_fallback_disallowed
        let strict_opts = PatchOptions {
            strict_font: true,
            ..PatchOptions::default()
        };
        let (_, res) = patch_and_verify(
            &doc,
            &[PatchSpec {
                target_id: "p1.b1.l1.s1".into(),
                new_text: "Replaced".into(),
            }],
            &strict_opts,
            &WriteOptions::default(),
            None,
        )
        .unwrap();
        assert!(res.patches[0].fallback_font);
        let ver = res.verification.unwrap();
        assert!(!ver.verified);
        assert!(
            ver.findings
                .iter()
                .any(|f| f.code == "verify.font_fallback_disallowed")
        );
    }
}
