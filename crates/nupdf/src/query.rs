// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Spatial, structural, and numeric aggregation query engine over PDF pages.
//!
//! Queries select spans, column cells, or form widgets by spatial relationship
//! (`right_of`, `left_of`, `below`, `above`, `inside`, `near_widget`),
//! typographic style (`bold`, `italic`, `min_size`, `max_size`, `color`),
//! and regular-expression `pattern`, then compute deterministic numeric
//! aggregations over currency, accounting, and percentage values.

use serde::Serialize;

use crate::annot;
use crate::content::Store;
use crate::document::Document;
use crate::extract::{self, Span};
use crate::form;
use crate::geometry::{Matrix, Rect};
use crate::object::Object;
use crate::rearrange;
use crate::regexp::{self, Regex};
use crate::stext::{self, Block, Options, StextChar, StextLine, StextPage, char_flags, rune_char};

/// Numeric aggregation operation over matched query items.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AggregateOp {
    Count,
    Sum,
    Min,
    Max,
    Mean,
    Values,
}

/// Declarative spatial, structural, and style query specification.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct QuerySpec {
    pub pages: Option<String>,
    pub right_of: Option<String>,
    pub left_of: Option<String>,
    pub below: Option<String>,
    pub above: Option<String>,
    #[serde(serialize_with = "serialize_inside")]
    pub inside: Option<(Option<i32>, Rect)>,
    pub near_widget: Option<String>,
    pub pattern: Option<String>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub min_size: Option<f32>,
    pub max_size: Option<f32>,
    pub color: Option<String>,
    pub nearest_only: bool,
    pub max_items: Option<usize>,
}

fn serialize_inside<S>(
    inside: &Option<(Option<i32>, Rect)>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let repr = inside.map(|(page, r)| (page, [r.x0, r.y0, r.x1, r.y1]));
    repr.serialize(serializer)
}

/// A single span, cell, or widget matched by [`run_query`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QueryMatch {
    pub id: String,
    pub page: i32,
    pub text: String,
    pub number: Option<f64>,
    pub bbox: [f32; 4],
    pub font: String,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub color: String,
    pub anchor_id: Option<String>,
    pub distance: Option<f32>,
}

/// Summary statistics over all matched items and their parsed numeric values.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Aggregation {
    pub count: usize,
    pub numeric_count: usize,
    pub sum: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub mean: Option<f64>,
    pub values: Vec<f64>,
}

/// Complete output of [`run_query`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QueryResult {
    pub match_count: usize,
    pub truncated: bool,
    pub matches: Vec<QueryMatch>,
    pub aggregation: Aggregation,
}

/// Maximum input byte length examined by [`parse_number`] so untrusted strings
/// are bounded in constant time.
const MAX_NUMBER_LEN: usize = 128;

/// Maximum Euclidean bounding-box distance (in points) for `near_widget`.
const NEAR_WIDGET_MAX_DIST: f32 = 72.0;

/// Parse a region specification of the form `"p3:[10,20,100,200]"`,
/// `"p3:10,20,100,200"`, `"[10,20,100,200]"`, or `"10,20,100,200"`.
///
/// Coordinates are in page space (`[x0, y0, x1, y1]` with origin at top-left).
pub fn parse_region_spec(s: &str) -> Result<(Option<i32>, Rect), String> {
    let bad =
        || format!("invalid region spec {s:?}: expected [pN:]x0,y0,x1,y1 or pN:[x0,y0,x1,y1]");
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.len() > 256 {
        return Err(bad());
    }

    let (page_opt, coords_part) = if let Some((prefix, rest)) = trimmed.split_once(':') {
        let p_str = prefix.trim();
        let num_str = p_str
            .strip_prefix('p')
            .or_else(|| p_str.strip_prefix('P'))
            .unwrap_or(p_str);
        let page: i32 = num_str.parse::<i32>().map_err(|_| bad())?;
        if page < 1 {
            return Err(format!("invalid region page {page}: pages are 1-based"));
        }
        (Some(page), rest.trim())
    } else {
        (None, trimmed)
    };

    let inner = if let Some(stripped) = coords_part.strip_prefix('[') {
        stripped.strip_suffix(']').ok_or_else(bad)?.trim()
    } else {
        if coords_part.ends_with(']') {
            return Err(bad());
        }
        coords_part
    };

    let mut nums = [0.0f32; 4];
    let mut count = 0usize;
    for part in inner.split(',') {
        if count >= 4 {
            return Err(bad());
        }
        let val = part.trim().parse::<f32>().map_err(|_| bad())?;
        if !val.is_finite() {
            return Err(bad());
        }
        nums[count] = val;
        count += 1;
    }
    if count != 4 {
        return Err(bad());
    }

    let [x0, y0, x1, y1] = nums;
    if !(x0 < x1 && y0 < y1) {
        return Err(format!(
            "invalid region bounds [{x0}, {y0}, {x1}, {y1}]: expected x0 < x1 and y0 < y1"
        ));
    }

    Ok((page_opt, Rect::new(x0, y0, x1, y1)))
}

/// Parse a currency, accounting, percentage, or plain decimal number.
///
/// Examples:
/// - `"$1,234.50"` -> `Some(1234.5)`
/// - `"($45.00)"` -> `Some(-45.0)`
/// - `"-12.5%"` -> `Some(-12.5)`
///
/// Never panics on arbitrary or adversarial strings.
pub fn parse_number(s: &str) -> Option<f64> {
    if s.len() > MAX_NUMBER_LEN {
        return None;
    }
    let mut cur = s.trim();
    if cur.is_empty() {
        return None;
    }

    let mut negative = false;
    let mut saw_sign = false;

    // Strip leading currency code/symbol before accounting parentheses, e.g. "$(45.00)".
    cur = strip_currency_affixes(cur);

    // Accounting parentheses: "(1,234.50)" or "($45.00)".
    if let Some(inner) = cur.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        negative = true;
        saw_sign = true;
        cur = inner.trim();
    }

    // Leading sign before or after currency symbol, e.g. "-$12.50" or "$-12.50".
    if let Some(rest) = cur
        .strip_prefix('-')
        .or_else(|| cur.strip_prefix('\u{2212}'))
    {
        if saw_sign {
            return None;
        }
        negative = true;
        saw_sign = true;
        cur = rest.trim();
    } else if let Some(rest) = cur.strip_prefix('+') {
        if saw_sign {
            return None;
        }
        saw_sign = true;
        cur = rest.trim();
    }

    // Trailing '%' (e.g. "-12.5%" or "(4.2)%").
    if let Some(rest) = cur.strip_suffix('%') {
        cur = rest.trim();
    }

    cur = strip_currency_affixes(cur);

    // Check inner accounting parentheses after currency strip, e.g. "USD (45.00)".
    if !saw_sign && let Some(inner) = cur.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        negative = true;
        saw_sign = true;
        cur = strip_currency_affixes(inner.trim());
    }

    // Check sign after currency symbol, e.g. "$-45.00" or "123.45-".
    if !saw_sign {
        if let Some(rest) = cur
            .strip_prefix('-')
            .or_else(|| cur.strip_prefix('\u{2212}'))
        {
            negative = true;
            cur = rest.trim();
        } else if let Some(rest) = cur.strip_prefix('+') {
            cur = rest.trim();
        } else if let Some(rest) = cur
            .strip_suffix('-')
            .or_else(|| cur.strip_suffix('\u{2212}'))
        {
            negative = true;
            cur = rest.trim();
        }
    }

    // Trailing '%' inside parens or after currency, e.g. "(-12.5%)".
    if let Some(rest) = cur.strip_suffix('%') {
        cur = rest.trim();
    }
    cur = strip_currency_affixes(cur);

    if cur.is_empty() {
        return None;
    }

    // Validate and normalize integer + fractional parts.
    let (int_part, frac_part) = match cur.split_once('.') {
        Some((i, f)) => {
            if f.contains('.') || !f.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            (i, Some(f))
        }
        None => (cur, None),
    };

    let mut normalized = String::with_capacity(cur.len() + 2);
    if !normalize_integer_digits(int_part, &mut normalized) {
        return None;
    }

    if let Some(frac) = frac_part {
        if normalized.is_empty() && frac.is_empty() {
            return None;
        }
        if normalized.is_empty() {
            normalized.push('0');
        }
        normalized.push('.');
        normalized.push_str(frac);
    } else if normalized.is_empty() {
        return None;
    }

    let mut val = normalized.parse::<f64>().ok()?;
    if !val.is_finite() {
        return None;
    }
    if negative {
        val = -val;
    }
    if val == 0.0 {
        val = 0.0;
    }
    Some(val)
}

fn strip_currency_affixes(s: &str) -> &str {
    const CODES: &[&str] = &[
        "US$", "CA$", "AU$", "HK$", "NZ$", "SG$", "NT$", "CHF", "USD", "EUR", "GBP", "JPY", "CNY",
        "CAD", "AUD", "INR", "KRW", "BRL", "MXN", "SEK", "NOK", "DKK", "ZAR",
    ];
    let mut cur = s.trim();
    loop {
        let prev = cur;
        cur = cur.trim_matches(|c: char| {
            matches!(
                c,
                '$' | '\u{00a2}'
                    | '\u{00a3}'
                    | '\u{00a5}'
                    | '\u{20a9}'
                    | '\u{20aa}'
                    | '\u{20ab}'
                    | '\u{20ac}'
                    | '\u{20b9}'
                    | '\u{20bd}'
                    | '\u{20bf}'
            )
        });
        cur = cur.trim();
        for &code in CODES {
            // A code in front, then one behind: each checked against what
            // is left, which the first may have made shorter than a code.
            if let (Some(head), Some(rest)) = (cur.get(..code.len()), cur.get(code.len()..))
                && head.eq_ignore_ascii_case(code)
                && rest
                    .chars()
                    .next()
                    .is_some_and(|c| !c.is_ascii_alphabetic())
            {
                cur = rest.trim();
            }
            if let Some(start) = cur.len().checked_sub(code.len())
                && let (Some(rest), Some(tail)) = (cur.get(..start), cur.get(start..))
                && tail.eq_ignore_ascii_case(code)
                && rest
                    .chars()
                    .next_back()
                    .is_some_and(|c| !c.is_ascii_alphabetic())
            {
                cur = rest.trim();
            }
        }
        if cur == prev {
            break;
        }
    }
    cur
}

fn normalize_integer_digits(int_part: &str, out: &mut String) -> bool {
    if int_part.is_empty() {
        return true;
    }
    let has_comma = int_part.contains(',');
    let has_space = int_part
        .chars()
        .any(|c| c == ' ' || c == '\u{00a0}' || c == '\'');
    if has_comma && has_space {
        return false;
    }
    if has_comma {
        let mut groups = int_part.split(',');
        let Some(first) = groups.next() else {
            return false;
        };
        if first.is_empty() || first.len() > 3 || !first.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        out.push_str(first);
        for g in groups {
            if g.len() != 3 || !g.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
            out.push_str(g);
        }
        true
    } else if has_space {
        let mut groups = int_part.split([' ', '\u{00a0}', '\'']);
        let Some(first) = groups.next() else {
            return false;
        };
        if first.is_empty() || first.len() > 3 || !first.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        out.push_str(first);
        for g in groups {
            if g.len() != 3 || !g.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
            out.push_str(g);
        }
        true
    } else {
        if !int_part.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        out.push_str(int_part);
        true
    }
}

/// Internal representation of a queryable span/cell on a page.
#[derive(Debug, Clone)]
struct CellItem {
    id: String,
    base_span_id: String,
    page: i32,
    line_key: (usize, usize),
    char_start: usize,
    char_end: usize,
    text: String,
    bbox: Rect,
    font: String,
    size: f32,
    bold: bool,
    italic: bool,
    color: String,
}

#[derive(Debug, Clone)]
struct WidgetItem {
    id: String,
    page: i32,
    name: String,
    label: String,
    field_type: &'static str,
    value: String,
    bbox: Rect,
}

struct CompiledFilters {
    regex: Option<Regex>,
    bold: Option<bool>,
    italic: Option<bool>,
    min_size: Option<f32>,
    max_size: Option<f32>,
    color_hex: Option<String>,
    inside: Option<(Option<i32>, Rect)>,
}

impl CompiledFilters {
    fn from_spec(spec: &QuerySpec) -> Result<Self, String> {
        let regex = match &spec.pattern {
            Some(pat) => Some(Regex::compile(pat.as_bytes(), regexp::NEWLINE)?),
            None => None,
        };
        let color_hex = spec.color.as_deref().map(normalize_color_spec);
        Ok(Self {
            regex,
            bold: spec.bold,
            italic: spec.italic,
            min_size: spec.min_size,
            max_size: spec.max_size,
            color_hex,
            inside: spec.inside,
        })
    }

    fn matches_cell(&self, cell: &CellItem) -> Result<bool, String> {
        if let Some(b) = self.bold
            && cell.bold != b
        {
            return Ok(false);
        }
        if let Some(it) = self.italic
            && cell.italic != it
        {
            return Ok(false);
        }
        if let Some(min_s) = self.min_size
            && cell.size + 1e-3 < min_s
        {
            return Ok(false);
        }
        if let Some(max_s) = self.max_size
            && cell.size - 1e-3 > max_s
        {
            return Ok(false);
        }
        if let Some(ref want_col) = self.color_hex
            && normalize_color_spec(&cell.color) != *want_col
        {
            return Ok(false);
        }
        if let Some((opt_p, rect)) = self.inside {
            if let Some(p) = opt_p
                && cell.page != p
            {
                return Ok(false);
            }
            if !bbox_inside_region(cell.bbox, rect) {
                return Ok(false);
            }
        }
        if let Some(ref re) = self.regex
            && re.exec(cell.text.as_bytes(), 0, regexp::RUNAWAY)?.is_none()
        {
            return Ok(false);
        }
        Ok(true)
    }

    fn matches_widget_as_match(&self, w: &WidgetItem, display_text: &str) -> Result<bool, String> {
        if self.bold.is_some()
            || self.italic.is_some()
            || self.min_size.is_some()
            || self.max_size.is_some()
            || self.color_hex.is_some()
        {
            return Ok(false);
        }
        if let Some((opt_p, rect)) = self.inside {
            if let Some(p) = opt_p
                && w.page != p
            {
                return Ok(false);
            }
            if !bbox_inside_region(w.bbox, rect) {
                return Ok(false);
            }
        }
        if let Some(ref re) = self.regex
            && re
                .exec(display_text.as_bytes(), 0, regexp::RUNAWAY)?
                .is_none()
        {
            return Ok(false);
        }
        Ok(true)
    }
}

/// Execute a spatial, structural, and numeric aggregation query against `doc`.
pub fn run_query(doc: &Document, spec: &QuerySpec) -> Result<QueryResult, String> {
    let page_count = doc.page_count().map_err(|e| e.0)?;
    if page_count <= 0 {
        return Ok(empty_result());
    }

    let mut page_indices: Vec<i32> = match &spec.pages {
        Some(range_spec) if !range_spec.trim().is_empty() => {
            rearrange::parse_page_ranges(range_spec, page_count, doc)
        }
        _ => (0..page_count).collect(),
    };

    if let Some((Some(inside_page), _)) = spec.inside {
        let target_idx = inside_page - 1;
        page_indices.retain(|&idx| idx == target_idx);
    }

    let filters = CompiledFilters::from_spec(spec)?;
    let has_spatial = spec.right_of.is_some()
        || spec.left_of.is_some()
        || spec.below.is_some()
        || spec.above.is_some();

    let mut matches: Vec<QueryMatch> = Vec::new();
    let mut store = Store::default();

    for page_idx in page_indices {
        if page_idx < 0 || page_idx >= page_count {
            continue;
        }
        let page_num = page_idx + 1;
        let page_obj = doc.page(page_idx).map_err(|e| e.0)?;
        let (stext_res, next_store) =
            stext::extract_page(doc, &page_obj, Options::default(), store);
        store = next_store;
        let stext_page = stext_res?;

        if let Some(ref widget_query) = spec.near_widget {
            let cells = collect_page_cells(&stext_page, page_num, true);
            let widgets = collect_page_widgets(doc, &page_obj, page_num)?;
            let page_matches =
                eval_near_widget(widget_query, &cells, &widgets, &filters, spec, has_spatial)?;
            matches.extend(page_matches);
        } else if has_spatial {
            let page_matches = eval_spatial_page(&stext_page, page_num, &filters, spec)?;
            matches.extend(page_matches);
        } else {
            let spans = collect_page_cells(&stext_page, page_num, false);
            for cell in spans {
                if filters.matches_cell(&cell)? {
                    matches.push(cell_to_match(&cell, None, None));
                }
            }
        }
    }

    let mut truncated = false;
    if let Some(limit) = spec.max_items
        && matches.len() > limit
    {
        matches.truncate(limit);
        truncated = true;
    }

    let aggregation = compute_aggregation(&matches);
    let match_count = matches.len();

    Ok(QueryResult {
        match_count,
        truncated,
        matches,
        aggregation,
    })
}

fn empty_result() -> QueryResult {
    QueryResult {
        match_count: 0,
        truncated: false,
        matches: Vec::new(),
        aggregation: Aggregation {
            count: 0,
            numeric_count: 0,
            sum: None,
            min: None,
            max: None,
            mean: None,
            values: Vec::new(),
        },
    }
}

fn compute_aggregation(matches: &[QueryMatch]) -> Aggregation {
    let count = matches.len();
    let values: Vec<f64> = matches.iter().filter_map(|m| m.number).collect();
    let numeric_count = values.len();
    if values.is_empty() {
        return Aggregation {
            count,
            numeric_count: 0,
            sum: None,
            min: None,
            max: None,
            mean: None,
            values,
        };
    }

    let mut sum = 0.0f64;
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for &v in &values {
        sum += v;
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
    }
    let mean = sum / (numeric_count as f64);

    Aggregation {
        count,
        numeric_count,
        sum: Some(sum),
        min: Some(min),
        max: Some(max),
        mean: Some(mean),
        values,
    }
}

fn cell_to_match(cell: &CellItem, anchor_id: Option<String>, distance: Option<f32>) -> QueryMatch {
    QueryMatch {
        id: cell.id.clone(),
        page: cell.page,
        number: parse_number(&cell.text),
        text: cell.text.clone(),
        bbox: rect_array(cell.bbox),
        font: cell.font.clone(),
        size: cell.size,
        bold: cell.bold,
        italic: cell.italic,
        color: cell.color.clone(),
        anchor_id,
        distance,
    }
}

fn rect_array(r: Rect) -> [f32; 4] {
    [r.x0, r.y0, r.x1, r.y1]
}

fn format_color(argb: u32) -> String {
    let a = argb >> 24;
    if a == 0xff {
        format!("#{:06x}", argb & 0xff_ffff)
    } else {
        format!("#{:06x}{a:02x}", argb & 0xff_ffff)
    }
}

fn normalize_color_spec(s: &str) -> String {
    let trimmed = s.trim();
    let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
    hex.to_ascii_lowercase()
}

fn bbox_inside_region(bbox: Rect, region: Rect) -> bool {
    let cx = 0.5 * (bbox.x0 + bbox.x1);
    let cy = 0.5 * (bbox.y0 + bbox.y1);
    let center_in = cx >= region.x0 && cx <= region.x1 && cy >= region.y0 && cy <= region.y1;
    let contained = bbox.x0 >= region.x0 - 2.0
        && bbox.x1 <= region.x1 + 2.0
        && bbox.y0 >= region.y0 - 2.0
        && bbox.y1 <= region.y1 + 2.0;
    contained || (center_in && bbox.x0 <= region.x1 && bbox.x1 >= region.x0)
}

/// Collect spans on a page. When `split_columns` is true, spans containing
/// multi-space column gaps (`>= 2` spaces or wide horizontal glyph jumps) are
/// decomposed into individual column cells with `pN.bB.lL.sS:a-b` slice IDs.
fn collect_page_cells(page: &StextPage, page_num: i32, split_columns: bool) -> Vec<CellItem> {
    let mut out = Vec::new();
    let mut flat_line_group = 0usize;
    let prefix = format!("p{page_num}");
    collect_blocks_cells(
        &page.blocks,
        &prefix,
        page_num,
        split_columns,
        &mut flat_line_group,
        &mut out,
    );
    out
}

fn collect_blocks_cells(
    blocks: &[Block],
    prefix: &str,
    page_num: i32,
    split_columns: bool,
    line_counter: &mut usize,
    out: &mut Vec<CellItem>,
) {
    for (bi, block) in blocks.iter().enumerate() {
        let block_id = format!("{prefix}.b{}", bi + 1);
        match block {
            Block::Text(tb) => {
                for (li, line) in tb.lines.iter().enumerate() {
                    let line_seq = *line_counter;
                    *line_counter += 1;
                    let line_id = format!("{block_id}.l{}", li + 1);
                    let line_spans = extract::spans(line);
                    for (si, span) in line_spans.iter().enumerate() {
                        let span_id = format!("{line_id}.s{}", si + 1);
                        append_span_cells(
                            line,
                            span,
                            &span_id,
                            page_num,
                            (bi, line_seq),
                            split_columns,
                            out,
                        );
                    }
                }
            }
            Block::Struct(sb) => {
                collect_blocks_cells(
                    &sb.blocks,
                    &block_id,
                    page_num,
                    split_columns,
                    line_counter,
                    out,
                );
            }
            Block::Image(_) | Block::Vector(_) | Block::Grid(_) => {}
        }
    }
}

fn append_span_cells(
    line: &StextLine,
    span: &Span,
    span_id: &str,
    page_num: i32,
    line_key: (usize, usize),
    split_columns: bool,
    out: &mut Vec<CellItem>,
) {
    let Some(chars) = line.chars.get(span.start..span.end) else {
        return;
    };
    if chars.is_empty() {
        return;
    }

    let font = String::from_utf8_lossy(&span.style.font).into_owned();
    let color = format_color(span.style.argb);

    if !split_columns {
        let text = extract::chars_text(chars);
        if text.trim().is_empty() {
            return;
        }
        out.push(CellItem {
            id: span_id.to_owned(),
            base_span_id: span_id.to_owned(),
            page: page_num,
            line_key,
            char_start: span.start,
            char_end: span.end,
            text,
            bbox: span.bbox,
            font,
            size: span.style.size,
            bold: span.style.bold,
            italic: span.style.italic,
            color,
        });
        return;
    }

    let segments = split_column_segments(chars, span.style.size);
    if segments.is_empty() {
        return;
    }
    let single_full = segments.len() == 1 && segments[0] == (0, chars.len());
    for (rel_s, rel_e) in segments {
        let Some(seg_chars) = chars.get(rel_s..rel_e) else {
            continue;
        };
        let text = extract::chars_text(seg_chars);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(bbox) = chars_bbox(seg_chars) else {
            continue;
        };
        let id = if single_full {
            span_id.to_owned()
        } else {
            format!("{span_id}:{rel_s}-{rel_e}")
        };
        out.push(CellItem {
            id,
            base_span_id: span_id.to_owned(),
            page: page_num,
            line_key,
            char_start: span.start + rel_s,
            char_end: span.start + rel_e,
            text: trimmed.to_owned(),
            bbox,
            font: font.clone(),
            size: span.style.size,
            bold: span.style.bold,
            italic: span.style.italic,
            color: color.clone(),
        });
    }
}

/// Split `chars` (within a single `Span`) into `(rel_start, rel_end)` column
/// ranges across either `>= 2` consecutive whitespace characters, tabs, or
/// large horizontal glyph gaps (`> max(8pt, 1.35 * font_size)`).
fn split_column_segments(chars: &[StextChar], font_size: f32) -> Vec<(usize, usize)> {
    let n = chars.len();
    let gap_thresh = (font_size * 1.35).max(8.0);
    let is_ws = |ch: &StextChar| {
        let c = rune_char(ch.c);
        c.is_whitespace()
    };

    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < n && is_ws(&chars[i]) {
        i += 1;
    }
    let mut seg_start = i;

    while i < n {
        if rune_char(chars[i].c) == '\t' {
            if i > seg_start {
                segments.push(trim_ws_range(chars, seg_start, i));
            }
            while i < n && is_ws(&chars[i]) {
                i += 1;
            }
            seg_start = i;
            continue;
        }

        if is_ws(&chars[i]) {
            let ws_start = i;
            let mut ws_end = i + 1;
            while ws_end < n && is_ws(&chars[ws_end]) {
                ws_end += 1;
            }
            let ws_count = ws_end - ws_start;
            let wide_space_jump = if ws_start > seg_start && ws_end < n {
                let prev_r = Rect::from_quad(chars[ws_start - 1].quad);
                let next_r = Rect::from_quad(chars[ws_end].quad);
                (next_r.x0 - prev_r.x1) > gap_thresh
            } else {
                false
            };
            if ws_count >= 2 || wide_space_jump {
                if ws_start > seg_start {
                    segments.push(trim_ws_range(chars, seg_start, ws_start));
                }
                seg_start = ws_end;
            }
            i = ws_end;
            continue;
        }

        if i > seg_start && !is_ws(&chars[i - 1]) {
            let prev_r = Rect::from_quad(chars[i - 1].quad);
            let cur_r = Rect::from_quad(chars[i].quad);
            if (cur_r.x0 - prev_r.x1) > gap_thresh {
                segments.push(trim_ws_range(chars, seg_start, i));
                seg_start = i;
            }
        }
        i += 1;
    }

    if seg_start < n {
        let r = trim_ws_range(chars, seg_start, n);
        if r.0 < r.1 {
            segments.push(r);
        }
    }

    segments
}

fn trim_ws_range(chars: &[StextChar], mut start: usize, mut end: usize) -> (usize, usize) {
    while start < end && rune_char(chars[start].c).is_whitespace() {
        start += 1;
    }
    while end > start && rune_char(chars[end - 1].c).is_whitespace() {
        end -= 1;
    }
    (start, end)
}

fn chars_bbox(chars: &[StextChar]) -> Option<Rect> {
    let mut acc: Option<Rect> = None;
    for ch in chars {
        if rune_char(ch.c).is_whitespace() && (ch.flags & char_flags::SYNTHETIC != 0) {
            continue;
        }
        let r = Rect::from_quad(ch.quad);
        acc = Some(match acc {
            Some(prev) => prev.union(r),
            None => r,
        });
    }
    if acc.is_none() {
        for ch in chars {
            let r = Rect::from_quad(ch.quad);
            acc = Some(match acc {
                Some(prev) => prev.union(r),
                None => r,
            });
        }
    }
    acc
}

#[derive(Debug, Clone, Copy)]
enum SpatialDir {
    RightOf,
    LeftOf,
    Below,
    Above,
}

#[derive(Debug, Clone)]
struct AnchorOccurrence {
    id: String,
    bbox: Rect,
    size: f32,
    line_key: (usize, usize),
    char_start: usize,
    char_end: usize,
    inline_remainder: Option<CellItem>,
}

fn eval_spatial_page(
    stext_page: &StextPage,
    page_num: i32,
    filters: &CompiledFilters,
    spec: &QuerySpec,
) -> Result<Vec<QueryMatch>, String> {
    let cells = collect_page_cells(stext_page, page_num, true);
    if cells.is_empty() {
        return Ok(Vec::new());
    }

    let mut active_dirs: Vec<(SpatialDir, &str)> = Vec::new();
    if let Some(ref a) = spec.right_of {
        active_dirs.push((SpatialDir::RightOf, a.as_str()));
    }
    if let Some(ref a) = spec.left_of {
        active_dirs.push((SpatialDir::LeftOf, a.as_str()));
    }
    if let Some(ref a) = spec.below {
        active_dirs.push((SpatialDir::Below, a.as_str()));
    }
    if let Some(ref a) = spec.above {
        active_dirs.push((SpatialDir::Above, a.as_str()));
    }

    // Evaluate each spatial direction into an ordered list of (CellItem, anchor_id, distance).
    let mut per_dir_results: Vec<Vec<(CellItem, String, f32)>> =
        Vec::with_capacity(active_dirs.len());

    for (dir, anchor_needle) in active_dirs {
        let anchors = find_anchors(stext_page, page_num, &cells, anchor_needle, dir);
        let mut dir_hits: Vec<(CellItem, String, f32)> = Vec::new();

        for anc in anchors {
            let mut cands: Vec<(CellItem, f32)> = Vec::new();

            if matches!(dir, SpatialDir::RightOf)
                && let Some(ref rem) = anc.inline_remainder
                && filters.matches_cell(rem)?
            {
                let dist = (rem.bbox.x0 - anc.bbox.x1).max(0.0);
                cands.push((rem.clone(), dist));
            }

            for cell in &cells {
                if overlaps_anchor_chars(cell, &anc) {
                    continue;
                }
                let Some(dist) = spatial_distance(dir, &anc, cell) else {
                    continue;
                };
                if !filters.matches_cell(cell)? {
                    continue;
                }
                cands.push((cell.clone(), dist));
            }

            cands.sort_by(|(a, da), (b, db)| {
                da.total_cmp(db)
                    .then_with(|| a.bbox.y0.total_cmp(&b.bbox.y0))
                    .then_with(|| a.bbox.x0.total_cmp(&b.bbox.x0))
            });

            if spec.nearest_only {
                cands.truncate(1);
            }

            for (cell, dist) in cands {
                dir_hits.push((cell, anc.id.clone(), dist));
            }
        }

        per_dir_results.push(dir_hits);
    }

    let Some(first) = per_dir_results.first() else {
        return Ok(Vec::new());
    };

    let mut out: Vec<QueryMatch> = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();

    for (cell, anc_id, dist) in first {
        let mut total_dist = *dist;
        let mut matched_all = true;
        for other in per_dir_results.iter().skip(1) {
            if let Some((_, _, d2)) = other.iter().find(|(c2, _, _)| c2.id == cell.id) {
                total_dist += *d2;
            } else {
                matched_all = false;
                break;
            }
        }
        if matched_all && !seen_ids.contains(&cell.id) {
            seen_ids.push(cell.id.clone());
            out.push(cell_to_match(cell, Some(anc_id.clone()), Some(total_dist)));
        }
    }

    Ok(out)
}

fn overlaps_anchor_chars(cell: &CellItem, anc: &AnchorOccurrence) -> bool {
    cell.line_key == anc.line_key
        && cell.char_start < anc.char_end
        && anc.char_start < cell.char_end
}

fn spatial_distance(dir: SpatialDir, anc: &AnchorOccurrence, cand: &CellItem) -> Option<f32> {
    let anc_cy = 0.5 * (anc.bbox.y0 + anc.bbox.y1);
    let cand_cy = 0.5 * (cand.bbox.y0 + cand.bbox.y1);
    let anc_h = (anc.bbox.y1 - anc.bbox.y0).max(anc.size);
    let cand_h = (cand.bbox.y1 - cand.bbox.y0).max(cand.size);
    let row_tol = 0.8 * anc_h.max(cand_h);

    match dir {
        SpatialDir::RightOf => {
            if (cand_cy - anc_cy).abs() <= row_tol && cand.bbox.x0 >= anc.bbox.x1 - 2.0 {
                Some((cand.bbox.x0 - anc.bbox.x1).max(0.0))
            } else {
                None
            }
        }
        SpatialDir::LeftOf => {
            if (cand_cy - anc_cy).abs() <= row_tol && cand.bbox.x1 <= anc.bbox.x0 + 2.0 {
                Some((anc.bbox.x0 - cand.bbox.x1).max(0.0))
            } else {
                None
            }
        }
        SpatialDir::Below => {
            if cand.bbox.y0 >= anc.bbox.y1 - 2.0 && horizontal_column_aligns(anc, cand) {
                Some((cand.bbox.y0 - anc.bbox.y1).max(0.0))
            } else {
                None
            }
        }
        SpatialDir::Above => {
            if cand.bbox.y1 <= anc.bbox.y0 + 2.0 && horizontal_column_aligns(anc, cand) {
                Some((anc.bbox.y0 - cand.bbox.y1).max(0.0))
            } else {
                None
            }
        }
    }
}

fn horizontal_column_aligns(anc: &AnchorOccurrence, cand: &CellItem) -> bool {
    let tol = (anc.size * 1.5).max(12.0);
    let overlaps = cand.bbox.x0 <= anc.bbox.x1 + tol && cand.bbox.x1 >= anc.bbox.x0 - tol;
    let left_aligned = (cand.bbox.x0 - anc.bbox.x0).abs() <= tol;
    let right_aligned = (cand.bbox.x1 - anc.bbox.x1).abs() <= tol;
    overlaps || left_aligned || right_aligned
}

fn find_anchors(
    stext_page: &StextPage,
    page_num: i32,
    cells: &[CellItem],
    needle: &str,
    dir: SpatialDir,
) -> Vec<AnchorOccurrence> {
    let needle_trimmed = needle.trim();
    if needle_trimmed.is_empty() {
        return Vec::new();
    }
    let needle_lower = needle_trimmed.to_lowercase();
    let mut anchors: Vec<AnchorOccurrence> = Vec::new();

    // A content id names its spans: a span, or every span of a line or block.
    if is_content_id(needle_trimmed) {
        let prefix = format!("{needle_trimmed}.");
        for cell in cells {
            if cell.id == needle_trimmed
                || cell.base_span_id == needle_trimmed
                || cell.base_span_id.starts_with(&prefix)
            {
                anchors.push(AnchorOccurrence {
                    id: cell.id.clone(),
                    bbox: cell.bbox,
                    size: cell.size,
                    line_key: cell.line_key,
                    char_start: cell.char_start,
                    char_end: cell.char_end,
                    inline_remainder: None,
                });
            }
        }
        return anchors;
    }

    let needle_chars: Vec<char> = needle_lower.chars().collect();
    for cell in cells {
        let cell_lower: Vec<char> = cell.text.to_lowercase().chars().collect();
        let Some(byte_pos) = find_word(&cell_lower, &needle_chars) else {
            continue;
        };

        // Check whether this single cell also holds an inline value to the right
        // (e.g. "Total Due: $108.50" separated by only one space).
        if matches!(dir, SpatialDir::RightOf)
            && let Some(split_anc) =
                try_split_inline_anchor(stext_page, page_num, cell, byte_pos, needle_trimmed)
        {
            anchors.push(split_anc);
            continue;
        }

        anchors.push(AnchorOccurrence {
            id: cell.id.clone(),
            bbox: cell.bbox,
            size: cell.size,
            line_key: cell.line_key,
            char_start: cell.char_start,
            char_end: cell.char_end,
            inline_remainder: None,
        });
    }

    // Fallback: if needle spans across adjacent styled spans on the same line
    // (e.g. bold "Total " + regular "Due"), locate it on the full line.
    if anchors.is_empty() {
        find_cross_span_line_anchors(stext_page, page_num, &needle_lower, &mut anchors);
    }

    anchors
}

/// Where `needle` first stands in `hay` as whole words: an alphanumeric
/// at its start or end does not run on into the text around it, so
/// "Total" is not found in "Subtotal".
fn find_word(hay: &[char], needle: &[char]) -> Option<usize> {
    let (first, last) = (needle.first()?, needle.last()?);
    if hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&pos| {
        let end = pos + needle.len();
        hay.get(pos..end) == Some(needle)
            && !(first.is_alphanumeric()
                && pos > 0
                && hay.get(pos - 1).is_some_and(|c| c.is_alphanumeric()))
            && !(last.is_alphanumeric() && hay.get(end).is_some_and(|c| c.is_alphanumeric()))
    })
}

/// Whether `s` is a content id: `p3`, `p3.b2`, `p3.b2.l4` or `p3.b2.l4.s1`.
fn is_content_id(s: &str) -> bool {
    let mut parts = s.split('.');
    let numbered = |part: Option<&str>, prefix: char| {
        part.and_then(|p| p.strip_prefix(prefix))
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    };
    if !numbered(parts.next(), 'p') {
        return false;
    }
    let rest: Vec<&str> = parts.collect();
    rest.len() <= 3
        && rest
            .iter()
            .zip(['b', 'l', 's'])
            .all(|(p, c)| numbered(Some(p), c))
}

fn try_split_inline_anchor(
    stext_page: &StextPage,
    page_num: i32,
    cell: &CellItem,
    _byte_pos: usize,
    needle_trimmed: &str,
) -> Option<AnchorOccurrence> {
    let line = lookup_line(stext_page, cell.line_key)?;
    let cell_chars = line.chars.get(cell.char_start..cell.char_end)?;
    let needle_chars: Vec<char> = needle_trimmed
        .chars()
        .flat_map(char::to_lowercase)
        .collect();
    if needle_chars.is_empty() {
        return None;
    }

    let line_lower: Vec<char> = cell_chars
        .iter()
        .map(|c| rune_char(c.c))
        .flat_map(|ch| {
            let mut v: Vec<char> = ch.to_lowercase().collect();
            if v.len() != 1 {
                v.truncate(1);
            }
            v
        })
        .collect();

    if line_lower.len() != cell_chars.len() {
        return None;
    }

    let match_idx = find_word(&line_lower, &needle_chars)?;
    let anc_rel_end = match_idx + needle_chars.len();

    let mut rem_rel_start = anc_rel_end;
    while rem_rel_start < cell_chars.len() {
        let ch = rune_char(cell_chars[rem_rel_start].c);
        if ch.is_whitespace() || ch == ':' {
            rem_rel_start += 1;
        } else {
            break;
        }
    }

    if rem_rel_start >= cell_chars.len() || rem_rel_start == anc_rel_end {
        return None;
    }

    let rem_chars = &cell_chars[rem_rel_start..];
    let rem_text = extract::chars_text(rem_chars).trim().to_owned();
    if rem_text.is_empty() {
        return None;
    }

    let anc_bbox = chars_bbox(&cell_chars[match_idx..anc_rel_end])?;
    let rem_bbox = chars_bbox(rem_chars)?;

    let span_start_in_line = find_span_start_for_id(line, &cell.base_span_id).unwrap_or(0);
    let anc_span_s = (cell.char_start + match_idx).saturating_sub(span_start_in_line);
    let anc_span_e = (cell.char_start + anc_rel_end).saturating_sub(span_start_in_line);
    let rem_span_s = (cell.char_start + rem_rel_start).saturating_sub(span_start_in_line);
    let rem_span_e = cell.char_end.saturating_sub(span_start_in_line);

    let anc_id = format!("{}:{anc_span_s}-{anc_span_e}", cell.base_span_id);
    let rem_id = format!("{}:{rem_span_s}-{rem_span_e}", cell.base_span_id);

    let remainder = CellItem {
        id: rem_id,
        base_span_id: cell.base_span_id.clone(),
        page: page_num,
        line_key: cell.line_key,
        char_start: cell.char_start + rem_rel_start,
        char_end: cell.char_end,
        text: rem_text,
        bbox: rem_bbox,
        font: cell.font.clone(),
        size: cell.size,
        bold: cell.bold,
        italic: cell.italic,
        color: cell.color.clone(),
    };

    Some(AnchorOccurrence {
        id: anc_id,
        bbox: anc_bbox,
        size: cell.size,
        line_key: cell.line_key,
        char_start: cell.char_start + match_idx,
        char_end: cell.char_start + anc_rel_end,
        inline_remainder: Some(remainder),
    })
}

fn lookup_line(page: &StextPage, line_key: (usize, usize)) -> Option<&StextLine> {
    let mut counter = 0usize;
    lookup_line_in_blocks(&page.blocks, line_key.1, &mut counter)
}

fn lookup_line_in_blocks<'a>(
    blocks: &'a [Block],
    want_seq: usize,
    counter: &mut usize,
) -> Option<&'a StextLine> {
    for block in blocks {
        match block {
            Block::Text(tb) => {
                for line in &tb.lines {
                    if *counter == want_seq {
                        return Some(line);
                    }
                    *counter += 1;
                }
            }
            Block::Struct(sb) => {
                if let Some(found) = lookup_line_in_blocks(&sb.blocks, want_seq, counter) {
                    return Some(found);
                }
            }
            Block::Image(_) | Block::Vector(_) | Block::Grid(_) => {}
        }
    }
    None
}

fn find_span_start_for_id(line: &StextLine, base_span_id: &str) -> Option<usize> {
    let (_, s_num_str) = base_span_id.rsplit_once(".s")?;
    let s_idx = s_num_str.parse::<usize>().ok()?.checked_sub(1)?;
    let spans = extract::spans(line);
    spans.get(s_idx).map(|sp| sp.start)
}

fn find_cross_span_line_anchors(
    page: &StextPage,
    page_num: i32,
    needle_lower: &str,
    anchors: &mut Vec<AnchorOccurrence>,
) {
    let needle_chars: Vec<char> = needle_lower.chars().collect();
    if needle_chars.is_empty() {
        return;
    }
    let mut line_seq = 0usize;
    let prefix = format!("p{page_num}");
    walk_lines_for_anchors(&page.blocks, &prefix, &needle_chars, &mut line_seq, anchors);
}

fn walk_lines_for_anchors(
    blocks: &[Block],
    prefix: &str,
    needle_chars: &[char],
    line_seq: &mut usize,
    anchors: &mut Vec<AnchorOccurrence>,
) {
    for (bi, block) in blocks.iter().enumerate() {
        let block_id = format!("{prefix}.b{}", bi + 1);
        match block {
            Block::Text(tb) => {
                for (li, line) in tb.lines.iter().enumerate() {
                    let seq = *line_seq;
                    *line_seq += 1;
                    let line_chars_lower: Vec<char> = line
                        .chars
                        .iter()
                        .map(|c| {
                            rune_char(c.c)
                                .to_lowercase()
                                .next()
                                .unwrap_or_else(|| rune_char(c.c))
                        })
                        .collect();
                    if line_chars_lower.len() < needle_chars.len() {
                        continue;
                    }
                    if let Some(pos) = find_word(&line_chars_lower, needle_chars) {
                        let end = pos + needle_chars.len();
                        if let Some(bbox) = chars_bbox(&line.chars[pos..end]) {
                            let size = line.chars[pos].size;
                            anchors.push(AnchorOccurrence {
                                id: format!("{block_id}.l{}", li + 1),
                                bbox,
                                size,
                                line_key: (bi, seq),
                                char_start: pos,
                                char_end: end,
                                inline_remainder: None,
                            });
                        }
                    }
                }
            }
            Block::Struct(sb) => {
                walk_lines_for_anchors(&sb.blocks, &block_id, needle_chars, line_seq, anchors);
            }
            Block::Image(_) | Block::Vector(_) | Block::Grid(_) => {}
        }
    }
}

fn collect_page_widgets(
    doc: &Document,
    page_obj: &Object,
    page_num: i32,
) -> Result<Vec<WidgetItem>, String> {
    let (_, page_ctm): (_, Matrix) = doc.page_transform(page_obj).map_err(|e| e.0)?;
    let (_, widgets) = annot::page_annots(doc, page_obj);
    let mut out = Vec::with_capacity(widgets.len());
    for (wi, w) in widgets.iter().enumerate() {
        let Some(rect) = annot::rect(doc, w, page_ctm) else {
            continue;
        };
        if !rect.is_valid() {
            continue;
        }
        let name = form::name(doc, w).unwrap_or_default();
        let label = form::label(doc, w);
        let field_type = form::type_string(doc, w);
        let value = form::value(doc, w);
        out.push(WidgetItem {
            id: format!("p{page_num}.w{}", wi + 1),
            page: page_num,
            name,
            label,
            field_type,
            value,
            bbox: rect,
        });
    }
    Ok(out)
}

fn eval_near_widget(
    widget_query: &str,
    cells: &[CellItem],
    widgets: &[WidgetItem],
    filters: &CompiledFilters,
    spec: &QuerySpec,
    _has_spatial: bool,
) -> Result<Vec<QueryMatch>, String> {
    let q_lower = widget_query.trim().to_lowercase();
    if q_lower.is_empty() || widgets.is_empty() {
        return Ok(Vec::new());
    }

    let mut out: Vec<QueryMatch> = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();

    // Case 1: widgets whose name, label, or field type matches `widget_query`
    // -> find text spans near those widgets.
    let matching_widgets: Vec<&WidgetItem> = widgets
        .iter()
        .filter(|w| {
            w.name.to_lowercase().contains(&q_lower)
                || w.label.to_lowercase().contains(&q_lower)
                || w.field_type.to_lowercase().contains(&q_lower)
        })
        .collect();

    for w in &matching_widgets {
        let mut nearby_cells: Vec<(&CellItem, f32)> = Vec::new();
        for cell in cells {
            let dist = rect_distance(w.bbox, cell.bbox);
            if dist <= NEAR_WIDGET_MAX_DIST && filters.matches_cell(cell)? {
                nearby_cells.push((cell, dist));
            }
        }
        nearby_cells.sort_by(|(a, da), (b, db)| {
            da.total_cmp(db)
                .then_with(|| a.bbox.y0.total_cmp(&b.bbox.y0))
                .then_with(|| a.bbox.x0.total_cmp(&b.bbox.x0))
        });
        if spec.nearest_only {
            nearby_cells.truncate(1);
        }
        for (cell, dist) in nearby_cells {
            if !seen_ids.contains(&cell.id) {
                seen_ids.push(cell.id.clone());
                out.push(cell_to_match(cell, Some(w.id.clone()), Some(dist)));
            }
        }
    }

    // Case 2: text spans matching `widget_query` as a label -> find form widgets near them.
    let label_cells: Vec<&CellItem> = cells
        .iter()
        .filter(|c| c.text.to_lowercase().contains(&q_lower))
        .collect();

    for label in &label_cells {
        let mut nearby_widgets: Vec<(&WidgetItem, f32)> = Vec::new();
        for w in widgets {
            let dist = rect_distance(label.bbox, w.bbox);
            let display = widget_display_text(w);
            if dist <= NEAR_WIDGET_MAX_DIST && filters.matches_widget_as_match(w, &display)? {
                nearby_widgets.push((w, dist));
            }
        }
        nearby_widgets.sort_by(|(a, da), (b, db)| {
            da.total_cmp(db)
                .then_with(|| a.bbox.y0.total_cmp(&b.bbox.y0))
                .then_with(|| a.bbox.x0.total_cmp(&b.bbox.x0))
        });
        if spec.nearest_only {
            nearby_widgets.truncate(1);
        }
        for (w, dist) in nearby_widgets {
            if !seen_ids.contains(&w.id) {
                seen_ids.push(w.id.clone());
                let text = widget_display_text(w);
                out.push(QueryMatch {
                    id: w.id.clone(),
                    page: w.page,
                    number: parse_number(&text),
                    text,
                    bbox: rect_array(w.bbox),
                    font: String::new(),
                    size: 0.0,
                    bold: false,
                    italic: false,
                    color: "#000000".to_owned(),
                    anchor_id: Some(label.id.clone()),
                    distance: Some(dist),
                });
            }
        }
    }

    Ok(out)
}

fn widget_display_text(w: &WidgetItem) -> String {
    if !w.value.is_empty() {
        w.value.clone()
    } else if !w.name.is_empty() {
        w.name.clone()
    } else {
        w.field_type.to_owned()
    }
}

fn rect_distance(a: Rect, b: Rect) -> f32 {
    let dx = (b.x0 - a.x1).max(a.x0 - b.x1).max(0.0);
    let dy = (b.y0 - a.y1).max(a.y0 - b.y1).max(0.0);
    dx.hypot(dy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{doc_bytes, page_doc, stream};

    const FONT_RESOURCES: &str = "/Font<<\
        /F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>\
        /F2<</Type/Font/Subtype/Type1/BaseFont/Helvetica-Bold/Encoding/WinAnsiEncoding>>\
        /F3<</Type/Font/Subtype/Type1/BaseFont/Helvetica-Oblique/Encoding/WinAnsiEncoding>>\
    >>";

    #[test]
    fn parses_currency_accounting_and_percentage_numbers() {
        assert_eq!(parse_number("$1,234.50"), Some(1234.5));
        assert_eq!(parse_number("($45.00)"), Some(-45.0));
        assert_eq!(parse_number("$(45.00)"), Some(-45.0));
        assert_eq!(parse_number("-12.5%"), Some(-12.5));
        assert_eq!(parse_number("+99.25"), Some(99.25));
        assert_eq!(parse_number("USD 2,500.00"), Some(2500.0));
        assert_eq!(parse_number("\u{20ac}1 234.50"), Some(1234.5));
        assert_eq!(parse_number("($0.00)"), Some(0.0));
        assert_eq!(parse_number(".75"), Some(0.75));

        // Reject dates, identifiers, malformed comma groups, and huge strings.
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("2026-02-11"), None);
        assert_eq!(parse_number("INV-1042"), None);
        assert_eq!(parse_number("12,34.50"), None);
        assert_eq!(parse_number("1.2.3"), None);
        assert_eq!(parse_number(&"9".repeat(300)), None);
    }

    #[test]
    fn a_currency_code_longer_than_the_amount_it_prefixes_is_stripped() {
        // After the code in front goes, what is left can be shorter than a
        // code: the check for one behind must not reach past its start.
        assert_eq!(parse_number("USD 45"), Some(45.0));
        assert_eq!(parse_number("EUR 5"), Some(5.0));
        assert_eq!(parse_number("US$ 5"), Some(5.0));
        assert_eq!(parse_number("5 EUR"), Some(5.0));
        assert_eq!(parse_number("USD 5 USD"), Some(5.0));
        assert_eq!(parse_number("USD"), None);
    }

    #[test]
    fn parses_region_specs_with_and_without_page_prefix() {
        let (p, r) =
            parse_region_spec("p3:[10,20,100,200]").unwrap_or_else(|e| panic!("unexpected: {e}"));
        assert_eq!(p, Some(3));
        assert_eq!(r, Rect::new(10.0, 20.0, 100.0, 200.0));

        let (p2, r2) =
            parse_region_spec("10, 20, 100, 200").unwrap_or_else(|e| panic!("unexpected: {e}"));
        assert_eq!(p2, None);
        assert_eq!(r2, Rect::new(10.0, 20.0, 100.0, 200.0));

        assert!(parse_region_spec("p0:[10,20,100,200]").is_err());
        assert!(parse_region_spec("100,20,10,200").is_err());
        assert!(parse_region_spec("p2:[10,20,100]").is_err());
    }

    #[test]
    fn invoice_right_of_total_due_extracts_nearest_amount() {
        // Test both multi-space inline invoice lines ("Total Due  $108.50") and
        // multi-column positioned text on the same row.
        let content = b"BT /F1 12 Tf \
            50 700 Td (Subtotal  $100.00) Tj \
            0 -20 Td (Tax  $8.50) Tj \
            0 -20 Td (Total Due  $108.50  USD) Tj \
            ET";
        let doc = page_doc(content, FONT_RESOURCES, &[]);

        let spec = QuerySpec {
            right_of: Some("Total Due".to_owned()),
            nearest_only: true,
            ..QuerySpec::default()
        };
        let res = run_query(&doc, &spec).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(res.match_count, 1);
        assert!(!res.truncated);
        assert_eq!(res.matches[0].text, "$108.50");
        assert_eq!(res.matches[0].number, Some(108.5));
        assert_eq!(res.aggregation.sum, Some(108.5));

        // Without nearest_only, both "$108.50" and "USD" to the right of "Total Due" match.
        let all_right = QuerySpec {
            right_of: Some("Total Due".to_owned()),
            nearest_only: false,
            ..QuerySpec::default()
        };
        let res_all = run_query(&doc, &all_right).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(res_all.match_count, 2);
        assert_eq!(res_all.matches[0].text, "$108.50");
        assert_eq!(res_all.matches[1].text, "USD");
    }

    #[test]
    fn an_anchor_matches_whole_words_or_a_content_id() {
        let content =
            b"BT /F2 12 Tf 50 700 Td (Subtotal) Tj ET BT /F1 12 Tf 200 700 Td ($100.00) Tj ET \
            BT /F2 12 Tf 50 680 Td (Total) Tj ET BT /F1 12 Tf 200 680 Td ($108.50) Tj ET";
        let doc = page_doc(content, FONT_RESOURCES, &[]);
        let right_of = |anchor: &str| {
            let spec = QuerySpec {
                right_of: Some(anchor.to_owned()),
                ..QuerySpec::default()
            };
            run_query(&doc, &spec).unwrap_or_else(|e| panic!("{e}"))
        };
        let res = right_of("Total");
        assert_eq!(res.aggregation.values, vec![108.5], "{:?}", res.matches);
        let res = right_of("Subtotal");
        assert_eq!(res.aggregation.values, vec![100.0], "{:?}", res.matches);

        let total_id = right_of("Total").matches[0]
            .anchor_id
            .clone()
            .unwrap_or_else(|| panic!("an anchor id"));
        let res = right_of(&total_id);
        assert_eq!(
            res.aggregation.values,
            vec![108.5],
            "{total_id}: {:?}",
            res.matches
        );
    }

    #[test]
    fn content_ids_are_told_from_text() {
        for id in ["p1", "p1.b2", "p3.b2.l4", "p3.b2.l4.s1"] {
            assert!(is_content_id(id), "{id}");
        }
        for text in ["Total", "p", "p1.b", "p1.x2", "p1.b2.l4.s1.c1", "pages"] {
            assert!(!is_content_id(text), "{text}");
        }
    }

    #[test]
    fn multi_row_table_below_amount_sums_column_only() {
        let content =
            b"BT /F2 12 Tf 50 720 Td (Description) Tj 250 0 Td (Qty) Tj 120 0 Td (Amount) Tj ET \
            BT /F1 12 Tf 50 695 Td (Cloud Compute) Tj 250 0 Td (2) Tj 120 0 Td ($120.00) Tj ET \
            BT /F1 12 Tf 50 675 Td (Object Storage) Tj 250 0 Td (5) Tj 120 0 Td ($45.50) Tj ET \
            BT /F1 12 Tf 50 655 Td (Promo Credit) Tj 250 0 Td (1) Tj 120 0 Td (($15.00)) Tj ET";
        let doc = page_doc(content, FONT_RESOURCES, &[]);

        let spec = QuerySpec {
            below: Some("Amount".to_owned()),
            ..QuerySpec::default()
        };
        let res = run_query(&doc, &spec).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(res.match_count, 3);
        assert_eq!(res.aggregation.numeric_count, 3);
        assert_eq!(res.aggregation.values, vec![120.0, 45.5, -15.0]);
        assert_eq!(res.aggregation.sum, Some(150.5));
        assert_eq!(res.aggregation.min, Some(-15.0));
        assert_eq!(res.aggregation.max, Some(120.0));
    }

    #[test]
    fn filters_by_style_color_pattern_region_and_truncates() {
        let content = b"BT 0.8 0 0 rg /F2 16 Tf 50 730 Td (CRITICAL: $500.00) Tj ET \
            BT 0 0 0 rg /F3 10 Tf 50 700 Td (Note: $25.00) Tj ET \
            BT 0.8 0 0 rg /F2 16 Tf 50 500 Td (CRITICAL: $300.00) Tj ET";
        let doc = page_doc(content, FONT_RESOURCES, &[]);

        let spec = QuerySpec {
            bold: Some(true),
            min_size: Some(14.0),
            color: Some("#CC0000".to_owned()),
            pattern: Some(r"\$[0-9]+\.[0-9]{2}".to_owned()),
            max_items: Some(1),
            ..QuerySpec::default()
        };
        let res = run_query(&doc, &spec).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(res.match_count, 1);
        assert!(res.truncated);
        assert!(res.matches[0].bold);
        assert_eq!(res.matches[0].color, "#cc0000");

        // Region filter restricts to top band (y in 40..100 in top-down fitz page coords).
        let region = parse_region_spec("p1:[40,40,400,100]").unwrap_or_else(|e| panic!("{e}"));
        let region_spec = QuerySpec {
            inside: Some(region),
            ..QuerySpec::default()
        };
        let reg_res = run_query(&doc, &region_spec).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(reg_res.match_count, 2);
    }

    #[test]
    fn finds_widgets_near_label_and_spans_near_widget_name() {
        let content = stream("", b"BT /F1 12 Tf 50 700 Td (Total Payment:) Tj ET");
        let objs = vec![
            b"<</Type/Catalog/Pages 2 0 R/AcroForm<</Fields[5 0 R]>>>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<<{FONT_RESOURCES}>>/Contents 4 0 R/Annots[5 0 R]>>"
            )
            .into_bytes(),
            content,
            b"<</Type/Annot/Subtype/Widget/FT/Tx/T(payment_amount)/V($250.00)/Rect[150 696 260 714]>>"
                .to_vec(),
        ];
        let doc = doc_bytes(&objs);

        // Label -> nearby widget value.
        let by_label = QuerySpec {
            near_widget: Some("Total Payment".to_owned()),
            nearest_only: true,
            ..QuerySpec::default()
        };
        let res = run_query(&doc, &by_label).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(res.match_count, 1);
        assert_eq!(res.matches[0].id, "p1.w1");
        assert_eq!(res.matches[0].text, "$250.00");
        assert_eq!(res.matches[0].number, Some(250.0));

        // Widget name -> nearby label span.
        let by_widget = QuerySpec {
            near_widget: Some("payment_amount".to_owned()),
            nearest_only: true,
            ..QuerySpec::default()
        };
        let res_w = run_query(&doc, &by_widget).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(res_w.match_count, 1);
        assert_eq!(res_w.matches[0].text, "Total Payment:");
        assert_eq!(res_w.matches[0].anchor_id.as_deref(), Some("p1.w1"));
    }
}
