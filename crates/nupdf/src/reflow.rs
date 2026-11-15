// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Block-scoped paragraph reflow and downstream vertical Y-shift engine.
//!
//! Where [`crate::patch`] replaces a single span or line in place on one
//! baseline, [`reflow_and_verify`] replaces an entire multi-line paragraph
//! block (`p1.b2`) — or reflows a modified line/span (`p1.b2.l1`,
//! `p1.b2.l1.s1`) across its enclosing [`crate::stext::TextBlock`] — word-wrapping
//! the new text to the inferred column width with exact glyph metrics and
//! vertically shifting downstream page content (`delta_y`) and annotations so
//! expanding a 2-line clause into 6 lines (or shrinking 5 lines into 1) never
//! collides with paragraphs, tables, or signature blocks below.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::Serialize;

use crate::content::{self, Store};
use crate::content_clean::{Cycle, Options as CleanOptions};
use crate::content_filter::{Cull, Hooks, Sanitize};
use crate::content_write::Writer;
use crate::document::Document;
use crate::extract::{self, Style};
use crate::fontdesc;
use crate::geometry::{Matrix, Point, Quad, Rect};
use crate::object::{Dict, ObjRef, Object};
use crate::patch::{
    AppliedPatch, CjkLang, EncodedRun, FontKey, PatchError, PatchFinding, PatchOptions, PatchSpec,
    PatchVerification, TargetAddr, add_font_object, baseline_text_matrix, encode_replacement_runs,
    font_vertical_metrics_em, guess_cjk_lang, ink_bbox, match_base14, measure_runs_advance_em,
    next_unique_font_res_name, normalize_ws, page_has_matching_base14, parse_and_validate_specs,
    push_float, push_pdf_hex_utf16, push_pdf_literal, rect_array, rect_from_array,
    strip_subset_prefix, trim_synthetic_edges,
};
use crate::stext::{
    self, Block, LINE_JOINED, Options as StextOptions, StextPage, TextBlock, char_flags,
};
use crate::write::WriteOptions;

/// Hard upper bound on wrapped lines synthesized for a single paragraph block.
const MAX_WRAPPED_LINES: usize = 4_096;

/// Hard upper bound on resource disambiguation probes.
const MAX_RES_PROBES: usize = 16_384;

/// One word-wrapped output line synthesized for a reflowed paragraph block.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WrappedLine {
    /// Text content placed on this line.
    pub text: String,
    /// Baseline start `[x, y]` in MuPDF page space (`y` grows downward).
    pub origin: [f32; 2],
    /// Tight line bounding box `[x0, y0, x1, y1]` in MuPDF page space.
    pub bbox: [f32; 4],
}

/// Geometry, typography, and vertical-shift record for one reflowed block.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AppliedReflow {
    /// Original requested target ID (`p1.b2`, `p1.b2.l1`, or `p1.b2.l1.s1`).
    pub id: String,
    /// Enclosing 1-based block ID (`p1.b2`).
    pub block_id: String,
    /// 1-based page number.
    pub page: i32,
    /// Original text of the addressed target before substitution.
    pub old_text: String,
    /// Requested replacement text for the addressed target.
    pub new_text: String,
    /// Full reconstructed paragraph text wrapped into [`Self::wrapped_lines`].
    pub reflowed_text: String,
    /// Pre-reflow enclosing block bounding box `[x0, y0, x1, y1]` in page space.
    pub old_bbox: [f32; 4],
    /// Post-reflow enclosing block bounding box `[x0, y0, x1, y1]` in page space.
    pub new_bbox: Option<[f32; 4]>,
    /// Inferred column wrap width in points.
    pub column_width: f32,
    /// Baseline-to-baseline line leading in points.
    pub leading: f32,
    /// Vertical shift applied to downstream page content in page space (`+` = down, `-` = up).
    pub delta_y: f32,
    /// Line count in the enclosing block before reflow.
    pub lines_before: usize,
    /// Line count in the enclosing block after reflow.
    pub lines_after: usize,
    /// Individual wrapped lines with exact positions and bounding boxes.
    pub wrapped_lines: Vec<WrappedLine>,
    /// Number of downstream annotations shifted vertically on the page.
    pub shifted_annotations: usize,
    /// The page-space x-range `[x0, x1]` within which what lies below the
    /// block moved by [`Self::delta_y`]: its column, when another sits beside
    /// it; `None` when everything below moved. Not part of any report.
    #[serde(skip)]
    pub shifted_band: Option<[f32; 2]>,
    /// PostScript/Base-14/CJK font name used to draw the reflowed paragraph.
    pub font_used: String,
    /// Whether a substitute font was used instead of an already-present unsubsetted Base-14 font.
    pub fallback_font: bool,
}

impl From<AppliedReflow> for AppliedPatch {
    fn from(r: AppliedReflow) -> Self {
        AppliedPatch {
            id: r.id,
            page: r.page,
            old_text: r.old_text,
            new_text: r.new_text,
            old_bbox: r.old_bbox,
            new_bbox: r.new_bbox,
            font_used: r.font_used,
            fallback_font: r.fallback_font,
        }
    }
}

/// Combined outcome of [`reflow_and_verify`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReflowResult {
    /// Rich block-reflow records including `delta_y` and wrapped line geometry.
    pub reflows: Vec<AppliedReflow>,
    /// Compatibility [`AppliedPatch`] projections for unified patch reporting.
    pub patches: Vec<AppliedPatch>,
    /// Post-write layout, text, collision, and overflow verification summary.
    pub verification: Option<PatchVerification>,
}

/// Returns `true` when `target_id` is a 2-segment block identifier (`p<page>.b<block>`).
pub fn is_block_target_id(target_id: &str) -> bool {
    TargetAddr::parse(target_id).is_ok_and(|a| a.line.is_none())
}

#[derive(Debug, Clone)]
struct ResolvedReflow {
    spec_index: usize,
    id: String,
    block_id: String,
    page: i32,
    old_text: String,
    new_text: String,
    full_new_block_text: String,
    old_block_bbox: Rect,
    left_x: f32,
    indent_x: f32,
    column_width: f32,
    col_band: Option<(f32, f32)>,
    base_y0: f32,
    dir: Point,
    leading: f32,
    style: Style,
    lines_before: usize,
    erase_quads_pdf: Vec<Quad>,
    base14_name: &'static str,
    cjk_lang: CjkLang,
    page_had_exact_base14: bool,
}

fn reference(num: i32) -> Object {
    Object::Ref(ObjRef::new(num, 0))
}

fn clean_line_text(line: &stext::StextLine) -> (String, bool) {
    let joined = line.flags & LINE_JOINED != 0;
    let trimmed = trim_synthetic_edges(&line.chars);
    let slice = if joined && !trimmed.is_empty() {
        &trimmed[..trimmed.len() - 1]
    } else {
        trimmed
    };
    (extract::chars_text(slice), joined)
}

fn join_block_lines(lines: &[(String, bool)]) -> String {
    let mut out = String::new();
    let mut glue = false;
    for (text, joined) in lines {
        let piece = text.trim();
        if piece.is_empty() {
            glue = false;
            continue;
        }
        if !out.is_empty() && !glue {
            out.push(' ');
        }
        out.push_str(piece);
        glue = *joined;
    }
    out
}

/// Reconstructs `(old_target_text, old_block_text, new_block_text)` for a block,
/// line, or span target inside `tb`.
fn reconstruct_block_texts(
    tb: &TextBlock,
    addr: TargetAddr,
    replacement: &str,
    not_found: &dyn Fn() -> PatchError,
) -> Result<(String, String, String), PatchError> {
    let orig_lines: Vec<(String, bool)> = tb.lines.iter().map(clean_line_text).collect();
    let full_old_block = join_block_lines(&orig_lines);

    let Some(line_num) = addr.line else {
        if tb.lines.is_empty() {
            return Err(not_found());
        }
        return Ok((
            full_old_block.clone(),
            full_old_block,
            replacement.to_string(),
        ));
    };

    let line_idx = line_num - 1;
    let Some(line) = tb.lines.get(line_idx) else {
        return Err(not_found());
    };
    if line.chars.is_empty() {
        return Err(not_found());
    }

    match addr.span {
        None => {
            let trimmed = trim_synthetic_edges(&line.chars);
            let old_line_text = extract::chars_text(trimmed);
            let mut updated_lines = orig_lines;
            if let Some(slot) = updated_lines.get_mut(line_idx) {
                *slot = (replacement.to_string(), false);
            }
            let full_new_block = join_block_lines(&updated_lines);
            Ok((old_line_text, full_old_block, full_new_block))
        }
        Some(span_num) => {
            let spans = extract::spans(line);
            let Some(span) = spans.get(span_num - 1) else {
                return Err(not_found());
            };
            let raw_slice = line.chars.get(span.start..span.end).ok_or_else(not_found)?;
            if raw_slice.is_empty() {
                return Err(not_found());
            }
            let mut real_start = span.start;
            let mut real_end = span.end;
            while real_start < real_end
                && line
                    .chars
                    .get(real_start)
                    .is_some_and(|c| c.flags & char_flags::SYNTHETIC != 0)
            {
                real_start += 1;
            }
            while real_end > real_start
                && line
                    .chars
                    .get(real_end - 1)
                    .is_some_and(|c| c.flags & char_flags::SYNTHETIC != 0)
            {
                real_end -= 1;
            }
            let (rep_start, rep_end) = if real_start < real_end {
                (real_start, real_end)
            } else {
                (span.start, span.end)
            };
            let old_span_slice = line.chars.get(rep_start..rep_end).ok_or_else(not_found)?;
            let old_span_text = extract::chars_text(old_span_slice);

            let prefix = extract::chars_text(line.chars.get(..rep_start).unwrap_or(&[]));
            let suffix_end = if line.flags & LINE_JOINED != 0 && !line.chars.is_empty() {
                line.chars.len().saturating_sub(1).max(rep_end)
            } else {
                line.chars.len()
            };
            let suffix = extract::chars_text(line.chars.get(rep_end..suffix_end).unwrap_or(&[]));
            let mut rebuilt_line =
                String::with_capacity(prefix.len() + replacement.len() + suffix.len());
            rebuilt_line.push_str(&prefix);
            rebuilt_line.push_str(replacement);
            rebuilt_line.push_str(&suffix);

            let mut updated_lines = orig_lines;
            if let Some(slot) = updated_lines.get_mut(line_idx) {
                *slot = (rebuilt_line, line.flags & LINE_JOINED != 0);
            }
            let full_new_block = join_block_lines(&updated_lines);
            Ok((old_span_text, full_old_block, full_new_block))
        }
    }
}

/// Infers dominant `Style` across non-whitespace characters in `tb`.
fn dominant_block_style(tb: &TextBlock) -> Option<Style> {
    let mut counts: Vec<(Style, usize)> = Vec::new();
    let mut fallback: Option<Style> = None;
    for line in &tb.lines {
        for span in extract::spans(line) {
            if fallback.is_none() {
                fallback = Some(span.style.clone());
            }
            let slice = line.chars.get(span.start..span.end).unwrap_or(&[]);
            let weight = slice
                .iter()
                .filter(|c| c.flags & char_flags::SYNTHETIC == 0 && c.c != ' ' as i32)
                .count();
            if weight == 0 {
                continue;
            }
            match counts.iter_mut().find(|(s, _)| *s == span.style) {
                Some((_, n)) => *n += weight,
                None => counts.push((span.style, weight)),
            }
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(s, _)| s)
        .or(fallback)
}

fn line_baseline_origin(line: &stext::StextLine) -> Option<Point> {
    let trimmed = trim_synthetic_edges(&line.chars);
    trimmed
        .iter()
        .find(|c| c.flags & char_flags::SYNTHETIC == 0)
        .or_else(|| trimmed.first())
        .map(|c| c.origin)
}

fn infer_leading(tb: &TextBlock, font_size: f32) -> f32 {
    let mut dys = Vec::new();
    let origins: Vec<Point> = tb.lines.iter().filter_map(line_baseline_origin).collect();
    for pair in origins.windows(2) {
        let dy = pair[1].y - pair[0].y;
        if dy > 0.25 * font_size && dy < 4.0 * font_size {
            dys.push(dy);
        }
    }
    if dys.is_empty() {
        return font_size * 1.28;
    }
    dys.sort_by(f32::total_cmp);
    let mid = dys.len() / 2;
    if dys.len() % 2 == 1 {
        dys[mid]
    } else {
        0.5 * (dys[mid - 1] + dys[mid])
    }
}

/// Infers `(column_width, optional_side_column_band)` for `tb` on `stext_page`.
fn infer_column_geometry(
    stext_page: &StextPage,
    block_idx: usize,
    tb: &TextBlock,
    font_size: f32,
) -> (f32, Option<(f32, f32)>) {
    let page_bounds = stext_page.mediabox;
    let left_x = tb.bbox.x0;

    let mut side_right_limit: Option<f32> = None;
    // The nearest edges of the columns beside the block, which the band of
    // what a reflow moves must stop short of.
    let mut right_neighbour_x0: Option<f32> = None;
    let mut left_neighbour_x1: Option<f32> = None;
    let mut same_col_right_max: Option<f32> = None;

    for (idx, block) in stext_page.blocks.iter().enumerate() {
        if idx == block_idx {
            continue;
        }
        let Block::Text(other) = block else {
            continue;
        };
        if !other.bbox.is_valid() || other.bbox.is_empty() {
            continue;
        }
        let vert_overlap = other.bbox.y1 > tb.bbox.y0 - 2.0 && other.bbox.y0 < tb.bbox.y1 + 2.0;
        if vert_overlap && other.bbox.x0 > tb.bbox.x1 + 6.0 {
            let candidate = other.bbox.x0 - 8.0;
            side_right_limit = Some(side_right_limit.map_or(candidate, |cur| cur.min(candidate)));
            right_neighbour_x0 =
                Some(right_neighbour_x0.map_or(other.bbox.x0, |cur| cur.min(other.bbox.x0)));
        } else if vert_overlap && other.bbox.x1 < tb.bbox.x0 - 6.0 {
            left_neighbour_x1 =
                Some(left_neighbour_x1.map_or(other.bbox.x1, |cur| cur.max(other.bbox.x1)));
        }
    }

    for (idx, block) in stext_page.blocks.iter().enumerate() {
        if idx == block_idx {
            continue;
        }
        let Block::Text(other) = block else {
            continue;
        };
        if !other.bbox.is_valid() || other.bbox.is_empty() {
            continue;
        }
        if (other.bbox.x0 - left_x).abs() <= 36.0
            && other.bbox.x1 > left_x + 40.0
            && side_right_limit.is_none_or(|lim| other.bbox.x1 <= lim + 8.0)
        {
            same_col_right_max =
                Some(same_col_right_max.map_or(other.bbox.x1, |cur| cur.max(other.bbox.x1)));
        }
    }

    let page_right = if page_bounds.is_valid() && page_bounds.x1 > left_x + 24.0 {
        page_bounds.x1
    } else {
        (tb.bbox.x1 + 72.0).max(left_x + 360.0)
    };
    let page_left = if page_bounds.is_valid() {
        page_bounds.x0
    } else {
        0.0
    };
    let left_margin = (left_x - page_left).clamp(18.0, 108.0);
    let default_col_right = (page_right - left_margin).min(page_right - 12.0);

    let col_right = match side_right_limit {
        Some(limit) => limit.min(page_right - 6.0),
        None => same_col_right_max
            .unwrap_or(0.0)
            .max(default_col_right)
            .min(page_right - 12.0),
    };

    let min_w = (font_size * 3.0).max(24.0);
    let hard_max_w = (side_right_limit.unwrap_or(page_right - 4.0) - left_x).max(min_w);
    let raw_width = if tb.lines.len() >= 2 {
        (tb.bbox.x1 - left_x).max(col_right - left_x)
    } else {
        col_right - left_x
    };
    let width = raw_width.clamp(min_w, hard_max_w);

    let col_band = if right_neighbour_x0.is_some() || left_neighbour_x1.is_some() {
        let x0 = left_neighbour_x1.map_or(left_x - 12.0, |n| (left_x - 12.0).max(n + 1.0));
        let x1 = right_neighbour_x0.map_or(left_x + width + 12.0, |n| {
            (left_x + width + 12.0).min(n - 1.0)
        });
        Some((x0, x1))
    } else {
        None
    };

    (width, col_band)
}

fn measure_str_width(s: &str, base14_name: &'static str, cjk_lang: CjkLang, font_size: f32) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    let (runs, _) = encode_replacement_runs(s, base14_name, cjk_lang);
    measure_runs_advance_em(&runs) * font_size
}

/// Splits a single word that exceeds `avail_width` into character-level pieces
/// guaranteed to fit within `avail_width`.
fn break_overlong_word(
    word: &str,
    avail_width: f32,
    base14_name: &'static str,
    cjk_lang: CjkLang,
    font_size: f32,
) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut cur = String::new();
    for ch in word.chars() {
        let mut probe = cur.clone();
        probe.push(ch);
        let w = measure_str_width(&probe, base14_name, cjk_lang, font_size);
        if !cur.is_empty() && w > avail_width {
            pieces.push(cur);
            cur = String::from(ch);
        } else {
            cur = probe;
        }
    }
    if !cur.is_empty() {
        pieces.push(cur);
    }
    pieces
}

/// Word-wraps `text` into [`WrappedLine`]s using exact font metrics, honoring
/// explicit `\n` breaks and first-line indentation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn wrap_paragraph_lines(
    text: &str,
    left_x: f32,
    indent_x: f32,
    column_width: f32,
    base_y0: f32,
    leading: f32,
    font_size: f32,
    base14_name: &'static str,
    cjk_lang: CjkLang,
) -> Result<Vec<WrappedLine>, PatchError> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if normalized.trim().is_empty() {
        return Ok(Vec::new());
    }

    let (asc_em, desc_em) = font_vertical_metrics_em(base14_name);
    let asc_px = asc_em * font_size;
    let desc_px = desc_em * font_size; // negative in font space
    let space_w = measure_str_width(" ", base14_name, cjk_lang, font_size);

    let mut out: Vec<WrappedLine> = Vec::new();
    let mut visual_line_idx: usize = 0;

    let push_line = |out: &mut Vec<WrappedLine>,
                     line_str: String,
                     line_x0: f32,
                     v_idx: &mut usize|
     -> Result<(), PatchError> {
        if out.len() >= MAX_WRAPPED_LINES {
            return Err(PatchError {
                code: "limit.reflow_lines",
                message: format!("reflow exceeds maximum of {MAX_WRAPPED_LINES} wrapped lines"),
            });
        }
        let baseline_y = base_y0 + (*v_idx as f32) * leading;
        let width_px = measure_str_width(&line_str, base14_name, cjk_lang, font_size);
        let y0 = baseline_y - asc_px;
        let y1 = baseline_y - desc_px;
        out.push(WrappedLine {
            text: line_str,
            origin: [line_x0, baseline_y],
            bbox: [line_x0, y0, line_x0 + width_px, y1],
        });
        *v_idx += 1;
        Ok(())
    };

    for (para_idx, para) in normalized.split('\n').enumerate() {
        let words: Vec<&str> = para.split_whitespace().collect();
        if words.is_empty() {
            // Blank line between explicit paragraphs advances vertical cursor by one leading.
            if para_idx > 0 {
                visual_line_idx = visual_line_idx.saturating_add(1);
            }
            continue;
        }

        let mut is_first_line_of_para = para_idx == 0;
        let mut cur_line = String::new();
        let mut cur_width = 0.0_f32;

        for raw_word in words {
            let line_indent = if is_first_line_of_para { indent_x } else { 0.0 };
            let avail = (column_width - line_indent).max(font_size * 2.0);
            let word_w = measure_str_width(raw_word, base14_name, cjk_lang, font_size);

            if cur_line.is_empty() {
                if word_w <= avail {
                    cur_line.push_str(raw_word);
                    cur_width = word_w;
                } else {
                    let pieces =
                        break_overlong_word(raw_word, avail, base14_name, cjk_lang, font_size);
                    for (pi, piece) in pieces.iter().enumerate() {
                        let cur_indent = if is_first_line_of_para { indent_x } else { 0.0 };
                        if pi + 1 < pieces.len() {
                            push_line(
                                &mut out,
                                piece.clone(),
                                left_x + cur_indent,
                                &mut visual_line_idx,
                            )?;
                            is_first_line_of_para = false;
                        } else {
                            cur_line = piece.clone();
                            cur_width =
                                measure_str_width(&cur_line, base14_name, cjk_lang, font_size);
                        }
                    }
                }
                continue;
            }

            if cur_width + space_w + word_w <= avail + 0.25 {
                cur_line.push(' ');
                cur_line.push_str(raw_word);
                cur_width += space_w + word_w;
            } else {
                push_line(
                    &mut out,
                    cur_line,
                    left_x + line_indent,
                    &mut visual_line_idx,
                )?;
                is_first_line_of_para = false;
                let next_avail = column_width.max(font_size * 2.0);
                if word_w <= next_avail {
                    cur_line = raw_word.to_string();
                    cur_width = word_w;
                } else {
                    let pieces =
                        break_overlong_word(raw_word, next_avail, base14_name, cjk_lang, font_size);
                    cur_line = String::new();
                    cur_width = 0.0;
                    for (pi, piece) in pieces.iter().enumerate() {
                        if pi + 1 < pieces.len() {
                            push_line(&mut out, piece.clone(), left_x, &mut visual_line_idx)?;
                        } else {
                            cur_line = piece.clone();
                            cur_width =
                                measure_str_width(&cur_line, base14_name, cjk_lang, font_size);
                        }
                    }
                }
            }
        }

        if !cur_line.is_empty() {
            let line_indent = if is_first_line_of_para { indent_x } else { 0.0 };
            push_line(
                &mut out,
                cur_line,
                left_x + line_indent,
                &mut visual_line_idx,
            )?;
        }
    }

    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartitionMode {
    /// Keep everything EXCEPT target glyphs AND EXCEPT downstream primitives.
    Upper,
    /// Keep ONLY downstream non-target primitives.
    Lower,
}

#[derive(Debug)]
struct SectionFilter {
    mode: PartitionMode,
    erase_quads_pdf: Vec<Quad>,
    page_ctm: Matrix,
    y_cut: f32,
    col_band: Option<(f32, f32)>,
}

impl SectionFilter {
    fn is_target_glyph(&self, trm: Matrix, ctm: Matrix, bbox: Rect) -> bool {
        let trm = trm.concat(ctm);
        let mut b = bbox.transform(trm);
        let w = b.x1 - b.x0;
        let h = b.y1 - b.y0;
        b.x0 += w / 10.0;
        b.x1 -= w / 10.0;
        b.y0 += h / 10.0;
        b.y1 -= h / 10.0;
        self.erase_quads_pdf
            .iter()
            .any(|q| b.intersect(Rect::from_quad(*q)).is_valid())
    }

    fn is_downstream(&self, r_page: Rect) -> bool {
        if !r_page.is_valid() {
            return false;
        }
        if r_page.y0 <= self.y_cut {
            return false;
        }
        if let Some((bx0, bx1)) = self.col_band {
            r_page.x1 >= bx0 && r_page.x0 <= bx1
        } else {
            true
        }
    }
}

impl Hooks for SectionFilter {
    fn filters_text(&self) -> bool {
        true
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
        if self.is_target_glyph(trm, ctm, bbox) {
            return true;
        }
        let r_page = bbox.transform(trm.concat(ctm)).transform(self.page_ctm);
        let downstream = self.is_downstream(r_page);
        match self.mode {
            PartitionMode::Upper => downstream,
            PartitionMode::Lower => !downstream,
        }
    }

    fn culls(&self) -> bool {
        true
    }

    fn cull(&mut self, bbox: Rect, kind: Cull) -> bool {
        match kind {
            Cull::Glyph | Cull::ClipPathDrop | Cull::PathDrop => false,
            Cull::PathFill
            | Cull::PathStroke
            | Cull::PathFillStroke
            | Cull::ClipPathFill
            | Cull::ClipPathStroke
            | Cull::ClipPathFillStroke
            | Cull::Image
            | Cull::Shading => {
                let r_page = bbox.transform(self.page_ctm);
                let downstream = self.is_downstream(r_page);
                match self.mode {
                    PartitionMode::Upper => downstream,
                    PartitionMode::Lower => !downstream,
                }
            }
        }
    }
}

fn page_has_form_xobjects(doc: &Document, res: Option<&Object>) -> bool {
    let Some(res_obj) = res else {
        return false;
    };
    let xobjs = doc.dict_get(res_obj, b"XObject");
    let Some(d) = xobjs.as_dict() else {
        return false;
    };
    d.iter()
        .any(|(_, v)| doc.dict_get(v, b"Subtype").is_name(b"Form"))
}

fn run_sanitize_partition(
    doc: &Document,
    contents: Option<&Object>,
    old_res: Option<&Object>,
    opts: CleanOptions,
    hooks: &mut dyn Hooks,
) -> Result<(Vec<u8>, Dict), String> {
    let writer = Writer::new(doc.diagnostics(), opts.ascii, opts.newlines);
    let cycle = Cycle::default();
    let mut proc = Sanitize::new(doc, writer, -1, Matrix::IDENTITY, opts, hooks, &cycle);
    let had_res = if let Some(r) = old_res {
        proc.push_resources(r.clone());
        true
    } else {
        proc.push_resources(Object::Dict(Arc::new(Dict::new())));
        true
    };
    if let Some(c) = contents {
        content::process_raw_contents(doc, c, &mut proc)?;
    }
    proc.close();
    let res_obj = if had_res {
        proc.pop_resources()
    } else {
        Object::Null
    };
    let dict = res_obj.as_dict().cloned().unwrap_or_default();
    Ok((proc.into_output(), dict))
}

fn merge_resource_dicts(doc: &Document, upper: &Dict, lower: &Dict) -> (Dict, bool) {
    let mut merged = upper.clone();
    let mut xobject_conflict = false;

    for (cat_key, lower_cat_val) in lower.iter() {
        let lower_cat_resolved = doc.resolve(lower_cat_val);
        let Some(lower_sub) = lower_cat_resolved.as_dict() else {
            if merged.get(cat_key).is_none() {
                merged.put(cat_key, lower_cat_val.clone());
            }
            continue;
        };
        let mut target_sub = merged
            .get(cat_key)
            .map(|v| doc.resolve(v))
            .and_then(|v| v.as_dict().cloned())
            .unwrap_or_default();
        for (entry_k, entry_v) in lower_sub.iter() {
            if let Some(existing_v) = target_sub.get(entry_k) {
                if existing_v != entry_v && cat_key == b"XObject" {
                    xobject_conflict = true;
                }
            } else {
                target_sub.put(entry_k, entry_v.clone());
            }
        }
        merged.put(cat_key, Object::Dict(Arc::new(target_sub)));
    }

    (merged, xobject_conflict)
}

fn next_unique_form_name(xobjs: &Dict) -> Vec<u8> {
    for idx in 0..MAX_RES_PROBES {
        let cand = format!("FmReflow{idx}").into_bytes();
        if xobjs.get(&cand).is_none() {
            return cand;
        }
    }
    b"FmReflowOverflow".to_vec()
}

/// Shifts annotations on `page_index` whose page-space `y0 >= old_bottom - 0.5`
/// (and within `col_band` when present) by `(dx_pdf, dy_pdf)` in PDF user space.
fn shift_downstream_annotations(
    doc: &Document,
    page_index: i32,
    page_ctm: Matrix,
    old_bottom: f32,
    col_band: Option<(f32, f32)>,
    dx_pdf: f32,
    dy_pdf: f32,
) -> Result<usize, String> {
    if dx_pdf.abs() < 0.01 && dy_pdf.abs() < 0.01 {
        return Ok(0);
    }
    let page = doc.page(page_index).map_err(|e| e.0)?;
    let Object::Array(annots) = doc.dict_get(&page, b"Annots") else {
        return Ok(0);
    };

    let mut shifted = 0usize;
    let mut updated_array: Vec<Object> = Vec::with_capacity(annots.len());
    let mut array_changed = false;

    for annot in annots.iter() {
        let resolved = doc.resolve(annot);
        let Some(adict) = resolved.as_dict() else {
            updated_array.push(annot.clone());
            continue;
        };
        let rect_pdf = doc.to_rect(Some(&doc.dict_get(annot, b"Rect")));
        if !rect_pdf.is_valid() || rect_pdf.is_empty() {
            updated_array.push(annot.clone());
            continue;
        }
        let rect_page = rect_pdf.transform(page_ctm);
        let in_band = col_band.is_none_or(|(bx0, bx1)| rect_page.x1 >= bx0 && rect_page.x0 <= bx1);
        if rect_page.y0 < old_bottom - 0.5 || !in_band {
            updated_array.push(annot.clone());
            continue;
        }

        let mut new_dict = adict.clone();
        new_dict.put(
            b"Rect",
            Object::Array(Arc::new(vec![
                Object::Real(rect_pdf.x0 + dx_pdf),
                Object::Real(rect_pdf.y0 + dy_pdf),
                Object::Real(rect_pdf.x1 + dx_pdf),
                Object::Real(rect_pdf.y1 + dy_pdf),
            ])),
        );
        if let Some(qp_raw) = adict.get(b"QuadPoints")
            && let Object::Array(qp) = doc.resolve(qp_raw)
        {
            let shifted_qp: Vec<Object> = qp
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let f = doc.resolve(v).to_real();
                    let delta = if i % 2 == 0 { dx_pdf } else { dy_pdf };
                    Object::Real(f + delta)
                })
                .collect();
            new_dict.put(b"QuadPoints", Object::Array(Arc::new(shifted_qp)));
        }

        shifted += 1;
        match annot {
            Object::Ref(r) => {
                doc.alter_object(r.num, Object::Dict(Arc::new(new_dict)));
                updated_array.push(annot.clone());
            }
            _ => {
                updated_array.push(Object::Dict(Arc::new(new_dict)));
                array_changed = true;
            }
        }
    }

    if array_changed {
        crate::edit::edit_page(doc, page_index, &mut |d| {
            d.put(b"Annots", Object::Array(Arc::new(updated_array.clone())));
        })?;
    }

    Ok(shifted)
}

/// Applies one resolved block reflow onto `doc` on `page_index`.
fn execute_single_block_reflow(
    doc: &Document,
    page_index: i32,
    page_ctm: Matrix,
    target: ResolvedReflow,
    doc_font_cache: &mut HashMap<FontKey, i32>,
    shared_tounicode: &mut Option<i32>,
) -> Result<AppliedReflow, PatchError> {
    let col_band = target.col_band;
    let font_size = if target.style.size > 0.0 {
        target.style.size
    } else {
        12.0
    };

    let wrapped_lines = wrap_paragraph_lines(
        &target.full_new_block_text,
        target.left_x,
        target.indent_x,
        target.column_width,
        target.base_y0,
        target.leading,
        font_size,
        target.base14_name,
        target.cjk_lang,
    )?;

    let old_bottom = target.old_block_bbox.y1;
    let new_bottom = match wrapped_lines.last() {
        Some(last) => last.bbox[3],
        None => target.old_block_bbox.y0,
    };
    let raw_delta_y = new_bottom - old_bottom;
    let delta_y = if raw_delta_y.abs() < 0.25 {
        0.0
    } else {
        raw_delta_y
    };

    let inv_ctm = page_ctm.invert();
    let dx_pdf = inv_ctm.c * delta_y;
    let dy_pdf = inv_ctm.d * delta_y;

    let page_obj = doc.page(page_index).map_err(|e| PatchError {
        code: "input.page_tree",
        message: e.0,
    })?;
    let contents = doc
        .resolve(&page_obj)
        .as_dict()
        .and_then(|d| d.get(b"Contents").cloned())
        .filter(|o| !matches!(o, Object::Null));
    let old_res = doc
        .inherited(&page_obj, b"Resources")
        .map_err(|e| PatchError {
            code: "input.page_tree",
            message: e.0,
        })?;

    let has_forms = page_has_form_xobjects(doc, old_res.as_ref());
    let clean_opts = CleanOptions {
        recurse: false,
        instance_forms: has_forms,
        ascii: true,
        newlines: false,
        sanitize: true,
    };

    let y_cut = if delta_y == 0.0 {
        f32::INFINITY
    } else {
        old_bottom + 1.0
    };

    let mut upper_filter = SectionFilter {
        mode: PartitionMode::Upper,
        erase_quads_pdf: target.erase_quads_pdf.clone(),
        page_ctm,
        y_cut,
        col_band: target.col_band,
    };
    let (upper_bytes, upper_res) = run_sanitize_partition(
        doc,
        contents.as_ref(),
        old_res.as_ref(),
        clean_opts,
        &mut upper_filter,
    )
    .map_err(|message| PatchError {
        code: "input.page_tree",
        message,
    })?;

    let (lower_bytes, lower_res) = if delta_y == 0.0 {
        (Vec::new(), Dict::new())
    } else {
        let mut lower_filter = SectionFilter {
            mode: PartitionMode::Lower,
            erase_quads_pdf: target.erase_quads_pdf.clone(),
            page_ctm,
            y_cut,
            col_band: target.col_band,
        };
        run_sanitize_partition(
            doc,
            contents.as_ref(),
            old_res.as_ref(),
            clean_opts,
            &mut lower_filter,
        )
        .map_err(|message| PatchError {
            code: "input.page_tree",
            message,
        })?
    };

    let (mut merged_res, xobj_conflict) = merge_resource_dicts(doc, &upper_res, &lower_res);

    let mut existing_fonts = merged_res
        .get(b"Font")
        .map(|f| doc.resolve(f))
        .and_then(|f| f.as_dict().cloned())
        .unwrap_or_default();
    let mut res_counter = 0usize;
    let mut page_font_names: HashMap<FontKey, Vec<u8>> = HashMap::new();

    // Synthesize the reflowed wrapped-paragraph BT...ET stream.
    let mut block_layer = Vec::new();
    let mut all_pure_winansi = true;
    let mut primary_font_used: Option<String> = None;

    if !wrapped_lines.is_empty() {
        block_layer.extend_from_slice(b"q\nBT\n0 Tr\n");
        let r = ((target.style.argb >> 16) & 0xff) as f32 / 255.0;
        let g = ((target.style.argb >> 8) & 0xff) as f32 / 255.0;
        let b = (target.style.argb & 0xff) as f32 / 255.0;
        push_float(&mut block_layer, r);
        block_layer.push(b' ');
        push_float(&mut block_layer, g);
        block_layer.push(b' ');
        push_float(&mut block_layer, b);
        block_layer.extend_from_slice(b" rg\n");

        for wl in &wrapped_lines {
            let (runs, pure_winansi) =
                encode_replacement_runs(&wl.text, target.base14_name, target.cjk_lang);
            if !pure_winansi {
                all_pure_winansi = false;
            }
            if primary_font_used.is_none()
                && let Some(first_run) = runs.first()
            {
                let key = match first_run {
                    EncodedRun::Byte { key, .. } | EncodedRun::Utf16 { key, .. } => key,
                };
                primary_font_used = Some(key.display_name().to_string());
            }

            let tm =
                baseline_text_matrix(Point::new(wl.origin[0], wl.origin[1]), target.dir, page_ctm);
            for comp in [tm.a, tm.b, tm.c, tm.d, tm.e, tm.f] {
                push_float(&mut block_layer, comp);
                block_layer.push(b' ');
            }
            block_layer.extend_from_slice(b"Tm\n");

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
                                let n = add_font_object(doc, key, shared_tounicode);
                                doc_font_cache.insert(key.clone(), n);
                                n
                            }
                        };
                        let fresh = next_unique_font_res_name(&existing_fonts, &mut res_counter);
                        existing_fonts.put(&fresh, reference(obj_num));
                        page_font_names.insert(key.clone(), fresh.clone());
                        fresh
                    }
                };
                block_layer.push(b'/');
                block_layer.extend_from_slice(&res_name);
                block_layer.push(b' ');
                push_float(&mut block_layer, font_size);
                block_layer.extend_from_slice(b" Tf\n");

                match run {
                    EncodedRun::Byte { bytes, .. } => push_pdf_literal(&mut block_layer, bytes),
                    EncodedRun::Utf16 { units, .. } => push_pdf_hex_utf16(&mut block_layer, units),
                }
                block_layer.extend_from_slice(b" Tj\n");
            }
        }
        block_layer.extend_from_slice(b"ET\nQ\n");
    }

    if !existing_fonts.is_empty() {
        merged_res.put(b"Font", Object::Dict(Arc::new(existing_fonts)));
    }

    let make_stream = |data: Vec<u8>| -> Object {
        let mut d = Dict::new();
        d.put(b"Length", Object::Int(data.len() as i64));
        reference(doc.add_stream(d, data))
    };

    let mut content_streams: Vec<Object> = Vec::with_capacity(3);
    if !upper_bytes.iter().all(u8::is_ascii_whitespace) {
        content_streams.push(make_stream(upper_bytes));
    }

    if !lower_bytes.iter().all(u8::is_ascii_whitespace) {
        let mut shifted_stream = Vec::with_capacity(lower_bytes.len() + 48);
        shifted_stream.extend_from_slice(b"q\n1 0 0 1 ");
        push_float(&mut shifted_stream, dx_pdf);
        shifted_stream.push(b' ');
        push_float(&mut shifted_stream, dy_pdf);
        shifted_stream.extend_from_slice(b" cm\n");

        if xobj_conflict {
            let mut fdict = Dict::new();
            fdict.put(b"Type", Object::name(b"XObject"));
            fdict.put(b"Subtype", Object::name(b"Form"));
            fdict.put(
                b"BBox",
                Object::Array(Arc::new(vec![
                    Object::Int(-100_000),
                    Object::Int(-100_000),
                    Object::Int(200_000),
                    Object::Int(200_000),
                ])),
            );
            fdict.put(b"Resources", Object::Dict(Arc::new(lower_res)));
            fdict.put(b"Length", Object::Int(lower_bytes.len() as i64));
            let form_num = doc.add_stream(fdict, lower_bytes);

            let mut xobjs = merged_res
                .get(b"XObject")
                .map(|v| doc.resolve(v))
                .and_then(|v| v.as_dict().cloned())
                .unwrap_or_default();
            let fm_name = next_unique_form_name(&xobjs);
            xobjs.put(&fm_name, reference(form_num));
            merged_res.put(b"XObject", Object::Dict(Arc::new(xobjs)));

            shifted_stream.push(b'/');
            shifted_stream.extend_from_slice(&fm_name);
            shifted_stream.extend_from_slice(b" Do\nQ\n");
        } else {
            shifted_stream.extend_from_slice(&lower_bytes);
            shifted_stream.extend_from_slice(b"\nQ\n");
        }
        content_streams.push(make_stream(shifted_stream));
    }

    if !block_layer.is_empty() {
        content_streams.push(make_stream(block_layer));
    }
    if content_streams.is_empty() {
        content_streams.push(make_stream(Vec::new()));
    }

    let final_res = Object::Dict(Arc::new(merged_res));
    let final_contents = if content_streams.len() == 1 {
        content_streams.remove(0)
    } else {
        Object::Array(Arc::new(content_streams))
    };

    crate::edit::edit_page(doc, page_index, &mut |d| {
        d.put(b"Resources", final_res.clone());
        d.put(b"Contents", final_contents.clone());
    })
    .map_err(|message| PatchError {
        code: "input.page_tree",
        message,
    })?;

    let shifted_annotations = shift_downstream_annotations(
        doc,
        page_index,
        page_ctm,
        old_bottom,
        target.col_band,
        dx_pdf,
        dy_pdf,
    )
    .map_err(|message| PatchError {
        code: "input.page_tree",
        message,
    })?;

    let fallback_font =
        !wrapped_lines.is_empty() && !(target.page_had_exact_base14 && all_pure_winansi);
    let font_used = primary_font_used.unwrap_or_else(|| target.base14_name.to_string());

    let synth_bbox = if wrapped_lines.is_empty() {
        None
    } else {
        let mut r = Rect::EMPTY;
        for wl in &wrapped_lines {
            r = r.union(rect_from_array(wl.bbox));
        }
        r.is_valid().then(|| rect_array(r))
    };

    Ok(AppliedReflow {
        id: target.id,
        block_id: target.block_id,
        page: target.page,
        old_text: target.old_text,
        new_text: target.new_text,
        reflowed_text: target.full_new_block_text,
        old_bbox: rect_array(target.old_block_bbox),
        new_bbox: synth_bbox,
        column_width: target.column_width,
        leading: target.leading,
        delta_y,
        lines_before: target.lines_before,
        lines_after: wrapped_lines.len(),
        wrapped_lines,
        shifted_annotations,
        shifted_band: col_band.map(|(x0, x1)| [x0, x1]),
        font_used,
        fallback_font,
    })
}

/// Applies block-scoped paragraph reflow and downstream vertical Y-shifting onto
/// `doc` for every target in `specs` (`p1.b2`, `p1.b2.l1`, or `p1.b2.l1.s1`).
pub fn apply_reflow(
    doc: &Document,
    specs: &[PatchSpec],
    _options: &PatchOptions,
) -> Result<Vec<AppliedReflow>, PatchError> {
    let addrs = parse_and_validate_specs(specs)?;
    if addrs.is_empty() {
        return Ok(Vec::new());
    }

    // Also guard against two separate line/span specs addressing the same enclosing
    // paragraph block in reflow mode.
    for i in 0..addrs.len() {
        for j in (i + 1)..addrs.len() {
            if addrs[i].page == addrs[j].page && addrs[i].block == addrs[j].block {
                return Err(PatchError {
                    code: "usage.duplicate_target",
                    message: format!(
                        "reflow target {:?} shares enclosing block p{}.b{} with {:?}",
                        specs[j].target_id, addrs[i].page, addrs[i].block, specs[i].target_id
                    ),
                });
            }
        }
    }

    let page_count = doc.page_count().map_err(|e| PatchError {
        code: "input.page_tree",
        message: e.0,
    })?;

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
    let mut resolved_by_page: BTreeMap<i32, (Matrix, Vec<ResolvedReflow>)> = BTreeMap::new();

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

        let mut targets = Vec::with_capacity(entries.len());
        for &(spec_idx, addr) in entries {
            let spec = &specs[spec_idx];
            let not_found = || PatchError {
                code: "input.target_not_found",
                message: format!("target {:?} not found on page {page_num}", spec.target_id),
            };
            let block_idx = addr.block - 1;
            let Some(Block::Text(tb)) = stext_page.blocks.get(block_idx) else {
                return Err(not_found());
            };
            if tb.lines.is_empty() {
                return Err(not_found());
            }

            let (old_text, _full_old_block_text, full_new_block_text) =
                reconstruct_block_texts(tb, addr, &spec.new_text, &not_found)?;

            let style = dominant_block_style(tb).ok_or_else(not_found)?;
            let font_size = if style.size > 0.0 { style.size } else { 12.0 };
            let first_origin = line_baseline_origin(&tb.lines[0]).ok_or_else(not_found)?;
            let dir = tb.lines[0].dir;
            let leading = infer_leading(tb, font_size);
            let (column_width, col_band) =
                infer_column_geometry(&stext_page, block_idx, tb, font_size);

            let left_x = tb.bbox.x0;
            let indent_x = if tb.lines.len() >= 2 {
                (tb.lines[0].bbox.x0 - left_x)
                    .max(0.0)
                    .min(column_width * 0.35)
            } else {
                0.0
            };

            let mut old_ink = Rect::EMPTY;
            let mut erase_quads_pdf: Vec<Quad> = Vec::new();
            for line in &tb.lines {
                let trimmed = trim_synthetic_edges(&line.chars);
                old_ink = old_ink.union(ink_bbox(trimmed, line.bbox));
                for ch in trimmed {
                    if ch.flags & char_flags::SYNTHETIC == 0 {
                        erase_quads_pdf.push(crate::redact::quad_to_pdf(ch.quad, page_ctm));
                    }
                }
            }
            let old_block_bbox = if old_ink.is_valid() && !old_ink.is_empty() {
                old_ink
            } else {
                tb.bbox
            };

            let base14_name = match_base14(&style);
            let cjk_lang = guess_cjk_lang(&style.font);
            let (_, subsetted) = strip_subset_prefix(&style.font);
            let exact_name_match =
                !subsetted && fontdesc::clean_font_name(&style.font) == Some(base14_name);
            let page_had_exact_base14 =
                exact_name_match && page_has_matching_base14(doc, &page_obj, base14_name);

            targets.push(ResolvedReflow {
                spec_index: spec_idx,
                id: spec.target_id.clone(),
                block_id: format!("p{page_num}.b{}", addr.block),
                page: page_num,
                old_text,
                new_text: spec.new_text.clone(),
                full_new_block_text,
                old_block_bbox,
                left_x,
                indent_x,
                column_width,
                col_band,
                base_y0: first_origin.y,
                dir,
                leading,
                style,
                lines_before: tb.lines.len(),
                erase_quads_pdf,
                base14_name,
                cjk_lang,
                page_had_exact_base14,
            });
        }

        // Bottom-to-top execution on each page ensures lower blocks shift first while
        // upper blocks retain their base coordinates and shift lower blocks cumulatively.
        targets.sort_by(|a, b| b.old_block_bbox.y0.total_cmp(&a.old_block_bbox.y0));
        resolved_by_page.insert(page_num, (page_ctm, targets));
    }

    let mut doc_font_cache: HashMap<FontKey, i32> = HashMap::new();
    let mut shared_tounicode: Option<i32> = None;
    let mut applied_by_index: Vec<Option<AppliedReflow>> = vec![None; specs.len()];

    for (page_num, (page_ctm, targets)) in resolved_by_page {
        let page_index = page_num - 1;
        // Track cumulative shifts applied by upper blocks to already-applied lower blocks
        // on the same page so `wrapped_lines` and `new_bbox` reflect final coordinates.
        let mut applied_on_page: Vec<usize> = Vec::new();
        for target in targets {
            let spec_idx = target.spec_index;
            let col_band = target.col_band;
            let old_bottom = target.old_block_bbox.y1;
            let applied = execute_single_block_reflow(
                doc,
                page_index,
                page_ctm,
                target,
                &mut doc_font_cache,
                &mut shared_tounicode,
            )?;
            let dy = applied.delta_y;
            if dy != 0.0 {
                for &lower_idx in &applied_on_page {
                    if let Some(prev) = applied_by_index.get_mut(lower_idx).and_then(Option::as_mut)
                        && prev.old_bbox[1] > old_bottom
                        && col_band.is_none_or(|(bx0, bx1)| {
                            prev.old_bbox[2] >= bx0 && prev.old_bbox[0] <= bx1
                        })
                    {
                        for wl in &mut prev.wrapped_lines {
                            wl.origin[1] += dy;
                            wl.bbox[1] += dy;
                            wl.bbox[3] += dy;
                        }
                        if let Some(nb) = &mut prev.new_bbox {
                            nb[1] += dy;
                            nb[3] += dy;
                        }
                    }
                }
            }
            applied_by_index[spec_idx] = Some(applied);
            applied_on_page.push(spec_idx);
        }
    }

    let mut out = Vec::with_capacity(specs.len());
    for slot in applied_by_index {
        let Some(item) = slot else {
            return Err(PatchError {
                code: "input.target_not_found",
                message: "internal reflow target resolution mismatch".into(),
            });
        };
        out.push(item);
    }
    Ok(out)
}

fn words_appear_in_order(haystack_words: &[&str], needle_words: &[&str]) -> bool {
    if needle_words.is_empty() {
        return true;
    }
    if needle_words.len() > haystack_words.len() {
        return false;
    }
    // First look for an exact contiguous token run.
    if haystack_words
        .windows(needle_words.len())
        .any(|w| w == needle_words)
    {
        return true;
    }
    // Fallback subsequence check in case an overlong single token was split across lines.
    let mut hi = 0usize;
    for &nw in needle_words {
        let mut matched = false;
        while hi < haystack_words.len() {
            if haystack_words[hi] == nw || nw.contains(haystack_words[hi]) {
                hi += 1;
                matched = true;
                break;
            }
            hi += 1;
        }
        if !matched {
            return false;
        }
    }
    true
}

fn all_page_lines(stext_page: &StextPage) -> Vec<(usize, usize, Rect, String)> {
    let mut out = Vec::new();
    let refs: Vec<&Block> = stext_page.blocks.iter().collect();
    for (bi, tb) in extract::text_blocks(&refs).into_iter().enumerate() {
        for (li, line) in tb.lines.iter().enumerate() {
            let trimmed = trim_synthetic_edges(&line.chars);
            let bbox = ink_bbox(trimmed, line.bbox);
            let text = extract::chars_text(trimmed);
            if bbox.is_valid() && !bbox.is_empty() {
                out.push((bi + 1, li + 1, bbox, text));
            }
        }
    }
    out
}

fn matches_wrapped_line(line_bbox: Rect, line_text: &str, wl: &WrappedLine) -> bool {
    let wb = rect_from_array(wl.bbox);
    let dy = (line_bbox.y0 - wb.y0)
        .abs()
        .min((line_bbox.y1 - wb.y1).abs());
    let dx = (line_bbox.x0 - wb.x0).abs();
    if dy <= 3.0 && dx <= 4.0 && normalize_ws(line_text) == normalize_ws(&wl.text) {
        return true;
    }
    // Also handle `stext` merging if a line's text contains `wl.text` on the exact baseline band.
    dy <= 2.5
        && line_bbox.x0 <= wb.x0 + 3.0
        && line_bbox.x1 >= wb.x1 - 3.0
        && normalize_ws(line_text).contains(&normalize_ws(&wl.text))
}

/// Reopens `patched_bytes`, re-extracts structured text on modified pages, and
/// verifies font policy, word-order presence, old-text removal, zero `> 1.5 pt`
/// collisions with other blocks, and full-page `MediaBox` containment for both
/// reflowed and shifted downstream blocks.
pub fn verify_reflowed_bytes(
    patched_bytes: &[u8],
    password: Option<&str>,
    applied: &mut [AppliedReflow],
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

    let mut pages_needed: Vec<i32> = applied.iter().map(|r| r.page).collect();
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
        let unc_opts = StextOptions {
            flags: 0,
            clip: Rect::INFINITE,
        };
        let (unc_res, s2) = stext::extract_page(&doc, &page_obj, unc_opts, s1);
        store = s2;
        let unc_page = unc_res.map_err(|message| PatchError {
            code: "input.page_tree",
            message,
        })?;
        extracted_pages.insert(page_num, (vis_page, unc_page));
    }

    let mut findings: Vec<PatchFinding> = Vec::new();

    for item in applied.iter_mut() {
        // 1. Strict font policy check.
        if options.strict_font && item.fallback_font {
            findings.push(PatchFinding {
                code: "verify.font_fallback_disallowed".into(),
                id: item.id.clone(),
                page: item.page,
                message: format!(
                    "reflow {:?} required fallback font {:?}, disallowed by strict_font",
                    item.id, item.font_used
                ),
            });
        }

        let Some((vis_page, unc_page)) = extracted_pages.get(&item.page) else {
            continue;
        };

        let unc_text = unc_page.text();
        let vis_text = vis_page.text();
        let unc_words: Vec<&str> = unc_text.split_whitespace().collect();
        let expected_words: Vec<&str> = item.reflowed_text.split_whitespace().collect();
        let unc_lines = all_page_lines(unc_page);

        // 2. Word-order presence & measured `new_bbox`.
        if !expected_words.is_empty() {
            let present = words_appear_in_order(&unc_words, &expected_words);
            let mut union_rect = Rect::EMPTY;
            for wl in &item.wrapped_lines {
                if let Some((_, _, lb, _)) = unc_lines
                    .iter()
                    .find(|(_, _, lb, lt)| matches_wrapped_line(*lb, lt, wl))
                {
                    union_rect = union_rect.union(*lb);
                }
            }
            if union_rect.is_valid() && !union_rect.is_empty() {
                item.new_bbox = Some(rect_array(union_rect));
            }
            if !present {
                findings.push(PatchFinding {
                    code: "verify.text_missing".into(),
                    id: item.id.clone(),
                    page: item.page,
                    message: format!(
                        "reflowed text for {:?} did not read back in order on page {}",
                        item.id, item.page
                    ),
                });
            }
        } else {
            item.new_bbox = None;
        }

        // 3. Old text removal check when distinct and not a substring of `reflowed_text`.
        let old_norm = normalize_ws(&item.old_text);
        let new_norm = normalize_ws(&item.reflowed_text);
        if !old_norm.is_empty() && old_norm != new_norm && !new_norm.contains(&old_norm) {
            let old_rect = rect_from_array(item.old_bbox);
            let vis_lines = all_page_lines(vis_page);
            let remained = vis_lines.iter().any(|(_, _, lb, lt)| {
                !lb.intersect(old_rect).is_empty() && normalize_ws(lt) == old_norm
            }) && normalize_ws(&vis_text).contains(&old_norm);
            if remained {
                findings.push(PatchFinding {
                    code: "verify.old_text_remained".into(),
                    id: item.id.clone(),
                    page: item.page,
                    message: format!(
                        "old text {:?} remained at {:?} after reflow",
                        item.old_text, item.id
                    ),
                });
            }
        }

        // 4. Collision check (`> 1.5 pt` intersection in both axes) between reflowed lines
        // and any other extracted text line on the page.
        if options.check_collisions && !item.wrapped_lines.is_empty() {
            let mut collided_block: Option<(usize, usize)> = None;
            'outer: for wl in &item.wrapped_lines {
                let wb = rect_from_array(wl.bbox);
                for (bi, li, other_bbox, other_text) in &unc_lines {
                    if item
                        .wrapped_lines
                        .iter()
                        .any(|own| matches_wrapped_line(*other_bbox, other_text, own))
                    {
                        continue;
                    }
                    let inter = wb.intersect(*other_bbox);
                    if inter.is_valid()
                        && (inter.x1 - inter.x0) > 1.5
                        && (inter.y1 - inter.y0) > 1.5
                    {
                        collided_block = Some((*bi, *li));
                        break 'outer;
                    }
                }
            }
            if let Some((bi, li)) = collided_block {
                findings.push(PatchFinding {
                    code: "verify.collision".into(),
                    id: item.id.clone(),
                    page: item.page,
                    message: format!(
                        "reflowed paragraph {:?} collides (> 1.5 pt) with p{}.b{bi}.l{li}",
                        item.id, item.page
                    ),
                });
            }
        }

        // 5. Full-page overflow check: both the reflowed block AND all shifted downstream
        // blocks must stay within `MediaBox` (`y1 <= page_bottom - 2.0`, `x1 <= page_right + 1.0`).
        if options.check_overflow {
            let bounds = unc_page.mediabox;
            if bounds.is_valid() {
                let check_rect = |r: Rect| -> bool {
                    r.x0 < bounds.x0 - 1.0
                        || r.y0 < bounds.y0 - 1.0
                        || r.x1 > bounds.x1 + 1.0
                        || r.y1 > bounds.y1 - 2.0
                };
                let mut overflow_rect: Option<Rect> = None;
                if let Some(nb_arr) = item.new_bbox {
                    let nb = rect_from_array(nb_arr);
                    if check_rect(nb) {
                        overflow_rect = Some(nb);
                    }
                }
                if overflow_rect.is_none() {
                    let refs: Vec<&Block> = unc_page.blocks.iter().collect();
                    for tb in extract::text_blocks(&refs) {
                        let mut blk = Rect::EMPTY;
                        for line in &tb.lines {
                            let trimmed = trim_synthetic_edges(&line.chars);
                            blk = blk.union(ink_bbox(trimmed, line.bbox));
                        }
                        if blk.is_valid()
                            && !blk.is_empty()
                            && blk.y1 >= item.old_bbox[1] - 1.0
                            && check_rect(blk)
                        {
                            overflow_rect = Some(blk);
                            break;
                        }
                    }
                }
                if let Some(bad) = overflow_rect {
                    findings.push(PatchFinding {
                        code: "verify.page_overflow".into(),
                        id: item.id.clone(),
                        page: item.page,
                        message: format!(
                            "post-reflow content bbox [{:.2}, {:.2}, {:.2}, {:.2}] overflows page bounds [{:.2}, {:.2}, {:.2}, {:.2}]",
                            bad.x0,
                            bad.y0,
                            bad.x1,
                            bad.y1,
                            bounds.x0,
                            bounds.y0,
                            bounds.x1,
                            bounds.y1
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

/// Applies block-scoped paragraph reflow on `doc`, saves serialized bytes with
/// `write_opts`, and runs [`verify_reflowed_bytes`] before returning.
pub fn reflow_and_verify(
    doc: &Document,
    specs: &[PatchSpec],
    options: &PatchOptions,
    write_opts: &WriteOptions,
    password: Option<&str>,
) -> Result<(Vec<u8>, ReflowResult), PatchError> {
    let mut reflows = apply_reflow(doc, specs, options)?;
    let mut effective_write = write_opts.clone();
    if !effective_write.incremental {
        effective_write.garbage = effective_write.garbage.max(1);
    }
    let bytes = crate::write::save(doc, &effective_write).map_err(|e| PatchError {
        code: "input.write",
        message: e.message,
    })?;
    let verification = verify_reflowed_bytes(&bytes, password, &mut reflows, options)?;
    let patches = reflows.iter().cloned().map(Into::into).collect();
    Ok((
        bytes,
        ReflowResult {
            reflows,
            patches,
            verification: Some(verification),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::{patch_and_verify, patch_and_verify_with_reflow};
    use crate::testutil::{doc_bytes, page_doc, stream};

    const HELVETICA: &[u8] =
        b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
    const HELVETICA_BOLD: &[u8] =
        b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica-Bold/Encoding/WinAnsiEncoding>>";

    fn extract_blocks(doc: &Document, page_index: i32) -> Vec<(String, Rect, Vec<String>)> {
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
            let lines: Vec<String> = tb
                .lines
                .iter()
                .map(|l| extract::chars_text(trim_synthetic_edges(&l.chars)))
                .collect();
            out.push((format!("p{p1}.b{}", bi + 1), tb.bbox, lines));
        }
        out
    }

    #[test]
    fn expand_two_line_clause_to_six_lines_shifts_downstream_clause_vector_and_signature() {
        // MediaBox [0 0 612 792] (PDF y grows up; page space y = 792 - y_pdf):
        // Header / Clause 1 at y_pdf=700 (page y~92)
        // Clause 2 (2 lines) at y_pdf=650, 635 (page y~142..160)
        // Clause 3 at y_pdf=590 (page y~202)
        // Vector signature line at y_pdf=530 (page y=262) + Signature text at y_pdf=515 (page y~277)
        // Plus a Link annotation over the signature block at Rect [72 510 260 535]!
        let content = b"BT /F2 13 Tf 0 0 0 rg 1 0 0 1 72 700 Tm (1. Definitions. Capitalized terms carry their agreement meanings.) Tj ET \
            BT /F1 12 Tf 0 0 0 rg 1 0 0 1 72 650 Tm (2. Payment. Client shall pay Contractor a fixed fee within thirty days) Tj \
            1 0 0 1 72 635 Tm (of receipt of an undisputed invoice.) Tj ET \
            BT /F1 12 Tf 0 0 0 rg 1 0 0 1 72 590 Tm (3. Governing Law. This Agreement is governed by Delaware law.) Tj ET \
            0.2 0.2 0.2 RG 1 w 72 530 m 280 530 l S \
            BT /F1 12 Tf 0 0 0 rg 1 0 0 1 72 515 Tm (Signed: Authorized Representative) Tj ET";

        let objs = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R/F2 6 0 R>>>>/Contents 4 0 R/Annots[7 0 R]>>".to_vec(),
            stream("", content),
            HELVETICA.to_vec(),
            HELVETICA_BOLD.to_vec(),
            b"<</Type/Annot/Subtype/Link/Rect[72 510 260 535]/Border[0 0 0]>>".to_vec(),
        ];
        let base_doc = doc_bytes(&objs);
        let before = extract_blocks(&base_doc, 0);
        assert_eq!(before.len(), 4);
        assert_eq!(before[1].0, "p1.b2");
        assert_eq!(before[1].2.len(), 2);
        let clause1_y0_before = before[0].1.y0;
        let clause3_y0_before = before[2].1.y0;
        let sig_y0_before = before[3].1.y0;

        let six_line_clause = "2. Payment and Milestone Schedule. Client shall pay Contractor \
            the aggregate compensation in six consecutive monthly installments upon written \
            acceptance of each deliverable milestone, net thirty calendar days from invoice \
            receipt via wire transfer in immediately available United States dollars, provided \
            that any undisputed late balance accrues simple interest at one percent per month \
            until paid in full by Client.";

        let work = base_doc.fork_clean();
        let specs = [PatchSpec {
            target_id: "p1.b2".into(),
            new_text: six_line_clause.into(),
        }];
        let opts = PatchOptions {
            strict_font: true,
            check_collisions: true,
            check_overflow: true,
        };
        let (bytes, res) =
            reflow_and_verify(&work, &specs, &opts, &WriteOptions::default(), None).unwrap();

        let ver = res.verification.unwrap();
        assert!(ver.verified, "unexpected findings: {:?}", ver.findings);
        let r = &res.reflows[0];
        assert_eq!(r.lines_before, 2);
        assert!(
            r.lines_after >= 5,
            "expected multi-line expansion (>=5 lines), got {}",
            r.lines_after
        );
        assert!(
            r.delta_y > 40.0,
            "expected positive delta_y, got {}",
            r.delta_y
        );
        assert_eq!(r.shifted_annotations, 1);
        assert!(!r.fallback_font);

        // Also confirm `patch::patch_and_verify` directly accepts `p1.b2`!
        let work2 = base_doc.fork_clean();
        let (_, patch_res) =
            patch_and_verify(&work2, &specs, &opts, &WriteOptions::default(), None).unwrap();
        assert!(patch_res.verification.unwrap().verified);

        let reopened = Document::open(bytes).unwrap();
        let after = extract_blocks(&reopened, 0);
        let clause1_after = after
            .iter()
            .find(|(_, _, ls)| ls.iter().any(|l| l.starts_with("1. Definitions.")))
            .expect("Clause 1 intact");
        let clause3_after = after
            .iter()
            .find(|(_, _, ls)| ls.iter().any(|l| l.starts_with("3. Governing Law.")))
            .expect("Clause 3 intact");
        let sig_after = after
            .iter()
            .find(|(_, _, ls)| ls.iter().any(|l| l.starts_with("Signed:")))
            .expect("Signature intact");

        // Clause 1 stayed in place; Clause 3 and Signature shifted down by `delta_y`!
        assert!((clause1_after.1.y0 - clause1_y0_before).abs() < 1.0);
        assert!((clause3_after.1.y0 - (clause3_y0_before + r.delta_y)).abs() < 2.0);
        assert!((sig_after.1.y0 - (sig_y0_before + r.delta_y)).abs() < 2.0);

        // Vector signature line also shifted down by `delta_y`!
        let page_obj = reopened.page(0).unwrap();
        let vec_opts = StextOptions {
            flags: stext::flags::COLLECT_VECTORS,
            clip: Rect::INFINITE,
        };
        let (vec_page, _) = stext::extract_page(&reopened, &page_obj, vec_opts, Store::default());
        let vec_blocks: Vec<Rect> = vec_page
            .unwrap()
            .blocks
            .into_iter()
            .filter_map(|b| match b {
                Block::Vector(v) => Some(v.bbox),
                _ => None,
            })
            .collect();
        assert!(
            vec_blocks
                .iter()
                .any(|vb| (vb.y0 - (262.0 + r.delta_y)).abs() < 3.0),
            "expected shifted signature vector near y={}, got {:?}",
            262.0 + r.delta_y,
            vec_blocks
        );

        // Link annotation `/Rect` shifted in PDF user space (`y_pdf` decreased by `delta_y`).
        let annots = reopened.dict_get(&page_obj, b"Annots");
        let first_annot = &annots.as_array().unwrap()[0];
        let rect_pdf = reopened.to_rect(Some(&reopened.dict_get(first_annot, b"Rect")));
        assert!((rect_pdf.y0 - (510.0 - r.delta_y)).abs() < 1.5);
    }

    #[test]
    fn shrink_five_lines_to_one_pulls_downstream_content_up() {
        let content = b"BT /F1 12 Tf 1 0 0 1 72 720 Tm (Header Clause) Tj ET \
            BT /F1 12 Tf \
            1 0 0 1 72 680 Tm (2. Long Clause line one with detailed provisions and terms) Tj \
            1 0 0 1 72 665 Tm (continuing onto line two with additional obligations here) Tj \
            1 0 0 1 72 650 Tm (continuing onto line three with further representations) Tj \
            1 0 0 1 72 635 Tm (continuing onto line four with indemnification language) Tj \
            1 0 0 1 72 620 Tm (and concluding on line five of the original paragraph.) Tj ET \
            BT /F1 12 Tf 1 0 0 1 72 570 Tm (3. Downstream Clause Follows Immediately.) Tj ET \
            BT /F1 12 Tf 1 0 0 1 72 520 Tm (Signature: Jane Roe) Tj ET";

        let base_doc = page_doc(content, "/Font<</F1 5 0 R>>", &[HELVETICA.to_vec()]);
        let before = extract_blocks(&base_doc, 0);
        assert_eq!(before[1].2.len(), 5);
        let clause3_y0_before = before[2].1.y0;
        let sig_y0_before = before[3].1.y0;

        let work = base_doc.fork_clean();
        let specs = [PatchSpec {
            target_id: "p1.b2".into(),
            new_text: "2. Shortened single-line clause.".into(),
        }];
        let (bytes, res) = reflow_and_verify(
            &work,
            &specs,
            &PatchOptions::default(),
            &WriteOptions::default(),
            None,
        )
        .unwrap();

        assert!(res.verification.unwrap().verified);
        let r = &res.reflows[0];
        assert_eq!(r.lines_before, 5);
        assert_eq!(r.lines_after, 1);
        assert!(
            r.delta_y < -50.0,
            "expected negative delta_y pulling content up, got {}",
            r.delta_y
        );

        let reopened = Document::open(bytes).unwrap();
        let after = extract_blocks(&reopened, 0);
        let clause3_after = after
            .iter()
            .find(|(_, _, ls)| ls.iter().any(|l| l.starts_with("3. Downstream")))
            .unwrap();
        let sig_after = after
            .iter()
            .find(|(_, _, ls)| ls.iter().any(|l| l.starts_with("Signature:")))
            .unwrap();
        assert!((clause3_after.1.y0 - (clause3_y0_before + r.delta_y)).abs() < 2.0);
        assert!((sig_after.1.y0 - (sig_y0_before + r.delta_y)).abs() < 2.0);
    }

    #[test]
    fn top_level_cm_and_text_state_do_not_leak_into_the_shifted_or_new_content() {
        // The page scales everything by half with a `cm` outside any q/Q (or
        // with unbalanced q/Q around it) and leaves character spacing set, so
        // coordinates are doubled. The sanitizer balances each partition, so
        // neither the shifted content nor the new paragraph inherits them.
        for prefix in [
            "0.5 0 0 0.5 0 0 cm 3 Tc",
            "q 0.5 0 0 0.5 0 0 cm 3 Tc",
            "Q Q 0.5 0 0 0.5 0 0 cm 3 Tc q",
        ] {
            let content = format!(
                "{prefix} \
                BT /F1 24 Tf 1 0 0 1 144 1440 Tm (Header Clause) Tj ET \
                BT /F1 24 Tf \
                1 0 0 1 144 1360 Tm (2. Long Clause line one with detailed provisions) Tj \
                1 0 0 1 144 1330 Tm (continuing onto line two with more obligations) Tj \
                1 0 0 1 144 1300 Tm (and concluding on line three of the paragraph.) Tj ET \
                BT /F1 24 Tf 1 0 0 1 144 1140 Tm (3. Downstream Clause Follows.) Tj ET"
            );
            let base_doc = page_doc(
                content.as_bytes(),
                "/Font<</F1 5 0 R>>",
                &[HELVETICA.to_vec()],
            );
            let before = extract_blocks(&base_doc, 0);
            assert_eq!(before[1].2.len(), 3, "{prefix}");
            let clause3_before = before[2].1;

            let work = base_doc.fork_clean();
            let specs = [PatchSpec {
                target_id: "p1.b2".into(),
                new_text: "2. Shortened single-line clause.".into(),
            }];
            let (bytes, res) = reflow_and_verify(
                &work,
                &specs,
                &PatchOptions::default(),
                &WriteOptions::default(),
                None,
            )
            .unwrap();
            let r = &res.reflows[0];
            assert!(r.delta_y < -10.0, "{prefix}: delta_y {}", r.delta_y);
            assert!(res.verification.unwrap().verified, "{prefix}");

            let reopened = Document::open(bytes).unwrap();
            let after = extract_blocks(&reopened, 0);
            let clause3_after = after
                .iter()
                .find(|(_, _, ls)| ls.iter().any(|l| l.starts_with("3. Downstream")))
                .unwrap()
                .1;
            assert!(
                (clause3_after.y0 - (clause3_before.y0 + r.delta_y)).abs() < 2.0,
                "{prefix}: clause 3 moved from {clause3_before:?} to {clause3_after:?}"
            );
            assert!(
                (clause3_after.x0 - clause3_before.x0).abs() < 1.0,
                "{prefix}"
            );
            assert!(
                (clause3_after.x1 - clause3_before.x1).abs() < 1.0,
                "{prefix}"
            );
            let new_block = after
                .iter()
                .find(|(_, _, ls)| ls.iter().any(|l| l.contains("Shortened")))
                .unwrap()
                .1;
            assert!((new_block.x0 - before[1].1.x0).abs() < 1.0, "{prefix}");
            assert!((new_block.y0 - before[1].1.y0).abs() < 3.0, "{prefix}");
        }
    }

    #[test]
    fn forty_five_line_expansion_reports_page_overflow() {
        let content = b"BT /F1 12 Tf 1 0 0 1 72 680 Tm (1. Intro.) Tj ET \
            BT /F1 12 Tf 1 0 0 1 72 640 Tm (2. Expandable Clause.) Tj \
            1 0 0 1 72 625 Tm (Second line of expandable clause.) Tj ET \
            BT /F1 12 Tf 1 0 0 1 72 560 Tm (Signed at bottom: Counterparty) Tj ET";

        let base_doc = page_doc(content, "/Font<</F1 5 0 R>>", &[HELVETICA.to_vec()]);
        let huge_clause = (1..=45)
            .map(|i| {
                format!(
                    "Sub-clause {i}: Contractor warrants compliance with all applicable statutory obligations."
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let work = base_doc.fork_clean();
        let specs = [PatchSpec {
            target_id: "p1.b2".into(),
            new_text: huge_clause,
        }];
        let (_, res) = reflow_and_verify(
            &work,
            &specs,
            &PatchOptions::default(),
            &WriteOptions::default(),
            None,
        )
        .unwrap();

        let ver = res.verification.unwrap();
        assert!(!ver.verified, "45-line expansion must fail verification");
        assert!(
            ver.findings
                .iter()
                .any(|f| f.code == "verify.page_overflow"),
            "expected verify.page_overflow in {:?}",
            ver.findings
        );
    }

    #[test]
    fn span_level_reflow_reconstructs_enclosing_paragraph_and_preserves_side_column() {
        // Two-column layout drawn column-by-column:
        // Left column at x=60..270: Fee clause at y_pdf=700 + downstream left footer at y_pdf=640.
        // Right sidebar at x=330..540: y_pdf=705..673 (must NOT shift or collide!).
        let content = b"BT /F1 12 Tf 1 0 0 1 60 700 Tm (Fee: ) Tj /F2 12 Tf ($500) Tj /F1 12 Tf ( due upon signing.) Tj ET \
            BT /F1 12 Tf 1 0 0 1 60 640 Tm (Left Footer: Initials ____) Tj ET \
            BT /F2 12 Tf 1 0 0 1 330 705 Tm (Sidebar Note: Confidential) Tj \
            /F1 12 Tf 1 0 0 1 330 689 Tm (Internal review copy only.) Tj \
            1 0 0 1 330 673 Tm (Do not distribute externally.) Tj ET";

        let base_doc = page_doc(
            content,
            "/Font<</F1 5 0 R/F2 6 0 R>>",
            &[HELVETICA.to_vec(), HELVETICA_BOLD.to_vec()],
        );

        let before = extract_blocks(&base_doc, 0);
        let fee_block_idx = before
            .iter()
            .position(|(_, _, ls)| ls.iter().any(|l| l.contains("Fee:")))
            .unwrap()
            + 1;
        let sidebar_before = before
            .iter()
            .find(|(_, _, ls)| ls.iter().any(|l| l.contains("Sidebar Note")))
            .unwrap()
            .1;
        let left_footer_before = before
            .iter()
            .find(|(_, _, ls)| ls.iter().any(|l| l.contains("Left Footer")))
            .unwrap()
            .1;

        let work = base_doc.fork_clean();
        let specs = [PatchSpec {
            target_id: format!("p1.b{fee_block_idx}.l1.s2"),
            new_text: "$500,000 payable in quarterly tranches subject to milestone sign-off and audit review"
                .into(),
        }];
        let (bytes, res) = patch_and_verify_with_reflow(
            &work,
            &specs,
            &PatchOptions::default(),
            true,
            &WriteOptions::default(),
            None,
        )
        .unwrap();

        let ver = res.verification.unwrap();
        assert!(ver.verified, "unexpected findings: {:?}", ver.findings);

        let reopened = Document::open(bytes).unwrap();
        let after = extract_blocks(&reopened, 0);
        let sidebar_after = after
            .iter()
            .find(|(_, _, ls)| ls.iter().any(|l| l.contains("Sidebar Note")))
            .unwrap()
            .1;
        let left_footer_after = after
            .iter()
            .find(|(_, _, ls)| ls.iter().any(|l| l.contains("Left Footer")))
            .unwrap()
            .1;

        // Right-hand sidebar stayed untouched while left-column footer shifted down!
        assert!((sidebar_after.y0 - sidebar_before.y0).abs() < 1.0);
        assert!(left_footer_after.y0 > left_footer_before.y0 + 15.0);
    }

    #[test]
    fn explicit_newlines_and_first_line_indent_are_preserved_across_reflow() {
        let content = b"BT /F1 12 Tf \
            1 0 0 1 90 720 Tm (Indented opening sentence of the governing covenant.) Tj \
            1 0 0 1 72 704 Tm (Continuation line flush with the left margin.) Tj ET \
            BT /F1 12 Tf 1 0 0 1 72 640 Tm (Witnesseth below: Authorized Signatory) Tj ET";

        let base_doc = page_doc(content, "/Font<</F1 5 0 R>>", &[HELVETICA.to_vec()]);
        let work = base_doc.fork_clean();
        let specs = [PatchSpec {
            target_id: "p1.b1".into(),
            new_text: "First paragraph starts indented and wraps cleanly onto a flush-left second line within the contract column.\nSecond paragraph after explicit newline also honors the paragraph indent and wraps cleanly across the full column width onto its own continuation line."
                .into(),
        }];

        let (_, res) = reflow_and_verify(
            &work,
            &specs,
            &PatchOptions::default(),
            &WriteOptions::default(),
            None,
        )
        .unwrap();

        let ver = res.verification.unwrap();
        assert!(ver.verified, "unexpected findings: {:?}", ver.findings);
        let r = &res.reflows[0];
        assert_eq!(r.lines_after, 4);
        // First line of block honors `indent_x` (~90pt); wrapped and `\n`-broken lines sit flush-left (~72pt).
        assert!((r.wrapped_lines[0].origin[0] - 90.0).abs() < 2.0);
        assert!((r.wrapped_lines[1].origin[0] - 72.0).abs() < 2.0);
        assert!((r.wrapped_lines[2].origin[0] - 72.0).abs() < 2.0);
        assert!((r.wrapped_lines[3].origin[0] - 72.0).abs() < 2.0);
    }

    #[test]
    fn validation_rejects_overlapping_or_malformed_reflow_targets() {
        let content = b"BT /F1 12 Tf 1 0 0 1 72 720 Tm (Line one) Tj \
            1 0 0 1 72 704 Tm (Line two) Tj ET";
        let base_doc = page_doc(content, "/Font<</F1 5 0 R>>", &[HELVETICA.to_vec()]);

        let dup = apply_reflow(
            &base_doc.fork_clean(),
            &[
                PatchSpec {
                    target_id: "p1.b1.l1".into(),
                    new_text: "Updated one".into(),
                },
                PatchSpec {
                    target_id: "p1.b1.l2".into(),
                    new_text: "Updated two".into(),
                },
            ],
            &PatchOptions::default(),
        )
        .unwrap_err();
        assert_eq!(dup.code, "usage.duplicate_target");

        let bad_id = apply_reflow(
            &base_doc.fork_clean(),
            &[PatchSpec {
                target_id: "p1".into(),
                new_text: "Invalid".into(),
            }],
            &PatchOptions::default(),
        )
        .unwrap_err();
        assert_eq!(bad_id.code, "usage.bad_id");

        let missing = apply_reflow(
            &base_doc.fork_clean(),
            &[PatchSpec {
                target_id: "p1.b9".into(),
                new_text: "Missing".into(),
            }],
            &PatchOptions::default(),
        )
        .unwrap_err();
        assert_eq!(missing.code, "input.target_not_found");
    }

    /// Two columns: a short paragraph at x=72 with a note beside it at
    /// x=420, and below them a total in the left column and a fee in the
    /// right one.
    fn two_column_doc() -> Document {
        let content = b"BT /F1 12 Tf 1 0 0 1 72 700 Tm (Payment is due within thirty days.) Tj ET \
            BT /F1 12 Tf 1 0 0 1 420 712 Tm (Notes) Tj ET \
            BT /F1 12 Tf 1 0 0 1 72 600 Tm (Total:) Tj ET \
            BT /F1 12 Tf 1 0 0 1 200 600 Tm ($9.00) Tj ET \
            BT /F1 12 Tf 1 0 0 1 420 612 Tm (Fee 5) Tj ET";
        page_doc(content, "/Font<</F1 5 0 R>>", &[HELVETICA.to_vec()])
    }

    #[test]
    fn a_reflow_moves_only_its_own_column_and_says_which() {
        let doc = two_column_doc();
        let before = extract_blocks(&doc, 0);
        let line_of = |blocks: &[(String, Rect, Vec<String>)], text: &str| {
            blocks
                .iter()
                .find(|(_, _, ls)| ls.iter().any(|l| l == text))
                .map(|(_, r, _)| *r)
        };
        let fee = line_of(&before, "Fee 5").unwrap();
        let total = line_of(&before, "Total: $9.00").or_else(|| line_of(&before, "Total:"));
        let total = total.unwrap();
        let block = before
            .iter()
            .position(|(_, _, ls)| ls.iter().any(|l| l.starts_with("Payment")))
            .unwrap()
            + 1;

        let work = doc.fork_clean();
        let long = "Payment is due within thirty days of the invoice date. ".repeat(4);
        let applied = apply_reflow(
            &work,
            &[PatchSpec {
                target_id: format!("p1.b{block}"),
                new_text: long.trim().into(),
            }],
            &PatchOptions::default(),
        )
        .unwrap();
        let dy = applied[0].delta_y;
        assert!(dy > 20.0, "{dy}");
        // The band moved is the left column's, short of the note beside it.
        let [x0, x1] = applied[0].shifted_band.unwrap();
        assert!(x0 <= 72.0 && x1 > total.x1 && x1 < 420.0, "{x0}..{x1}");

        let bytes = crate::write::save(&work, &WriteOptions::default()).unwrap();
        let after = extract_blocks(&Document::open(bytes).unwrap(), 0);
        // The fee keeps every glyph where it was; the total moved down.
        let fee_after = line_of(&after, "Fee 5").unwrap_or_else(|| panic!("{after:?}"));
        assert!(
            (fee_after.y0 - fee.y0).abs() < 0.5,
            "{fee:?} -> {fee_after:?}"
        );
        let total_after = line_of(&after, "Total: $9.00")
            .or_else(|| line_of(&after, "Total:"))
            .unwrap();
        assert!(
            (total_after.y0 - (total.y0 + dy)).abs() < 0.5,
            "{total:?} -> {total_after:?}, {dy}"
        );
    }
}
