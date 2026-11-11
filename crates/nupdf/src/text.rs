// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/text.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! `fz_text`: glyphs as the interpreter hands them to a device, in spans
//! of one font, matrix, writing mode, bidi level and language.

use std::sync::Arc;

use crate::fontdesc::{self, Font};
use crate::geometry::{Matrix, Rect};
use crate::path::StrokeState;

/// `fz_text_item`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextItem {
    pub x: f32,
    pub y: f32,
    pub adv: f32,
    /// -1 for a filler of a one-to-many Unicode mapping.
    pub gid: i32,
    pub ucs: i32,
    pub cid: i32,
}

/// `fz_text_span`. The fields MuPDF keeps in bit-fields are kept as their
/// widths leave them.
#[derive(Debug, Clone)]
pub struct TextSpan {
    pub font: Arc<Font>,
    /// The glyph matrix without its translation.
    pub trm: Matrix,
    pub wmode: u32,
    pub bidi_level: u32,
    pub markup_dir: u32,
    pub language: u32,
    pub items: Vec<TextItem>,
}

/// `fz_text`.
#[derive(Debug, Clone)]
pub struct Text {
    pub spans: Vec<TextSpan>,
    /// Which text object this is: devices that see the same text twice
    /// in a row (filled, then stroked) compare it by identity.
    pub id: u64,
}

impl Default for Text {
    fn default() -> Text {
        Text::new()
    }
}

impl Text {
    /// `fz_bound_text`: the union of the glyphs' boxes, widened for the
    /// stroke and by a unit for the glyph cache's positioning.
    pub fn bound(&self, stroke: Option<&StrokeState>, ctm: Matrix) -> Rect {
        let mut bbox = Rect::EMPTY;
        for span in &self.spans {
            for item in &span.items {
                if item.gid >= 0 {
                    let tm = Matrix {
                        e: item.x,
                        f: item.y,
                        ..span.trm
                    };
                    bbox = bbox.union(fontdesc::bound_glyph(&span.font, item.gid, tm.concat(ctm)));
                }
            }
        }
        if !bbox.is_empty() {
            if let Some(s) = stroke {
                bbox = crate::path::adjust_for_stroke(bbox, s, ctm);
            }
            bbox = Rect::new(bbox.x0 - 1.0, bbox.y0 - 1.0, bbox.x1 + 1.0, bbox.y1 + 1.0);
        }
        bbox
    }

    pub fn new() -> Text {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Text {
            spans: Vec::new(),
            id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// `fz_show_glyph_aux`.
    #[allow(clippy::too_many_arguments)]
    pub fn show_glyph(
        &mut self,
        font: &Arc<Font>,
        trm: Matrix,
        adv: f32,
        gid: i32,
        ucs: i32,
        cid: i32,
        wmode: i32,
        bidi_level: i32,
        markup_dir: i32,
        language: i32,
    ) {
        // The tail's bit-fields compared with the full values, as in C.
        let same = self.spans.last().is_some_and(|t| {
            Arc::ptr_eq(&t.font, font)
                && i64::from(t.wmode) == i64::from(wmode as u32)
                && i64::from(t.bidi_level) == i64::from(bidi_level as u32)
                && i64::from(t.markup_dir) == i64::from(markup_dir as u32)
                && i64::from(t.language) == i64::from(language as u32)
                && t.trm.a == trm.a
                && t.trm.b == trm.b
                && t.trm.c == trm.c
                && t.trm.d == trm.d
        });
        if !same {
            self.spans.push(TextSpan {
                font: font.clone(),
                trm: Matrix {
                    e: 0.0,
                    f: 0.0,
                    ..trm
                },
                wmode: wmode as u32 & 1,
                bidi_level: bidi_level as u32 & 0x7f,
                markup_dir: markup_dir as u32 & 3,
                language: language as u32 & 0x7fff,
                items: Vec::new(),
            });
        }
        if let Some(span) = self.spans.last_mut() {
            span.items.push(TextItem {
                x: trm.e,
                y: trm.f,
                adv,
                gid,
                ucs,
                cid,
            });
        }
    }
}

/// `FZ_LANG_zh_Hant` and `FZ_LANG_zh_Hans`.
const LANG_ZH_HANT: i32 = 26 + 8 * 27 + 20 * 27 * 27;
const LANG_ZH_HANS: i32 = 26 + 8 * 27 + 19 * 27 * 27;

/// `fz_text_language_from_string`, for up to the seven bytes MuPDF keeps.
pub fn language_from_string(s: &[u8]) -> i32 {
    let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    let s = &s[..end];
    if s.is_empty() {
        return 0;
    }
    match s {
        b"zh-Hant" | b"zh-HK" | b"zh-MO" | b"zh-SG" | b"zh-TW" => return LANG_ZH_HANT,
        b"zh-Hans" | b"zh-CN" => return LANG_ZH_HANS,
        _ => {}
    }
    let letter = |c: Option<&u8>| match c {
        Some(c @ b'a'..=b'z') => Some(i32::from(c - b'a') + 1),
        Some(c @ b'A'..=b'Z') => Some(i32::from(c - b'A') + 1),
        _ => None,
    };
    let Some(first) = letter(s.first()) else {
        return 0;
    };
    let Some(second) = letter(s.get(1)) else {
        return 0;
    };
    let mut lang = first + 27 * second;
    if let Some(third) = letter(s.get(2)) {
        lang += 27 * 27 * third;
    }
    lang
}

/// `fz_string_from_text_language`.
pub fn language_to_string(lang: u32) -> String {
    if lang == 0 {
        return String::new();
    }
    if lang == LANG_ZH_HANT as u32 {
        return "zh-Hant".into();
    }
    if lang == LANG_ZH_HANS as u32 {
        return "zh-Hans".into();
    }
    let mut out = String::new();
    let mut l = lang;
    for _ in 0..3 {
        let c = l % 27;
        l /= 27;
        if c == 0 {
            break;
        }
        out.push(char::from(b'a' + (c - 1) as u8));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_round_trip_as_mupdf_packs_them() {
        assert_eq!(language_from_string(b"en-US"), 5 + 27 * 14);
        assert_eq!(
            language_to_string(language_from_string(b"en-US") as u32),
            "en"
        );
        assert_eq!(
            language_to_string(language_from_string(b"DEU") as u32),
            "deu"
        );
        assert_eq!(
            language_to_string(language_from_string(b"zh-TW") as u32),
            "zh-Hant"
        );
        assert_eq!(language_from_string(b"e"), 0);
        assert_eq!(language_from_string(b""), 0);
    }

    #[test]
    fn spans_split_on_matrix_and_font() {
        let font = Arc::new(crate::fontdesc::Font::new(b"F"));
        let mut text = Text::new();
        let m = Matrix::scale(12.0, 12.0);
        text.show_glyph(&font, m, 0.5, 1, 65, 65, 0, 0, 0, 0);
        text.show_glyph(&font, m.pre_translate(1.0, 0.0), 0.5, 2, 66, 66, 0, 0, 0, 0);
        text.show_glyph(&font, Matrix::scale(10.0, 10.0), 0.5, 3, 67, 67, 0, 0, 0, 0);
        assert_eq!(text.spans.len(), 2);
        assert_eq!(text.spans[0].items.len(), 2);
        assert_eq!(text.spans[0].items[1].x, 12.0);
        // A writing mode the bit-field cannot hold always starts a span.
        text.show_glyph(&font, Matrix::scale(10.0, 10.0), 0.5, 3, 67, 67, 2, 0, 0, 0);
        text.show_glyph(&font, Matrix::scale(10.0, 10.0), 0.5, 3, 67, 67, 2, 0, 0, 0);
        assert_eq!(text.spans.len(), 4);
        assert_eq!(text.spans[3].wmode, 0);
    }
}
