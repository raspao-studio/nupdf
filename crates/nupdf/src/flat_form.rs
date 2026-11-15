// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Flat-form & vector checkbox synthesis, verified filling, and `/AcroForm` promotion.
//!
//! Real-world flat PDFs frequently contain zero `/AcroForm` widgets, instead
//! drawing horizontal underline rules (`m x0 y l x1 y S`), runs of `'_'`
//! characters, small stroked vector squares (`5..18 pt`) for checkboxes, and
//! rows of equal-width stroked cells (`|_|_|_|_|`) for comb grids (SSN, EIN,
//! dates).
//!
//! - [`detect_flat_forms`] recovers flat `"checkbox"`, `"comb"`, and
//!   `"underline"` fields in MuPDF page space (origin top-left) alongside exact
//!   PDF user-space coordinates, skipping regions already covered by existing
//!   `/AcroForm` widgets and resolving z-ordered checkbox check states.
//! - [`fill_flat_forms`] forks the document cleanly, registers `/F_nupdf_ff`
//!   (`Helvetica`) in `/Resources /Font`, appends balanced `q ... Q` content
//!   streams with auto-shrunk text, cell-centered comb characters, and crisp
//!   vector checkmarks (or white-out unchecks), and verifies read-back on the
//!   serialized PDF bytes.
//! - [`promote_flat_forms`] upgrades detected flat fields into interactive PDF
//!   `/Widget` annotations registered under `/AcroForm` (`/FT /Tx`, `/Ff 1<<24`
//!   and `/MaxLen` for combs, `/FT /Btn` for checkboxes) and verifies every
//!   promoted widget via [`crate::form`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde::Serialize;

use crate::annot;
use crate::colorspace::ColorSpace;
use crate::content::Store;
use crate::device::{ColorParams, Device};
use crate::diag;
use crate::document::Document;
use crate::edit;
use crate::extract;
use crate::fontdesc;
use crate::form;
use crate::function::Function;
use crate::geometry::{Matrix, Point, Rect};
use crate::glyphs;
use crate::image::Image;
use crate::object::{Dict, ObjRef, Object};
use crate::page_run::{self, Parts};
use crate::path::{self, Path, StrokeState, Walker};
use crate::rearrange;
use crate::stext::{self, Block, StextChar, StextPage, char_flags, rune_char};
use crate::text::Text;
use crate::write::{self, WriteOptions};

/// Hard cap on vector path segments and primitives collected per page.
const MAX_COLLECTED: usize = 50_000;

/// Hard cap on nested clip/group/mask/tile containers.
const MAX_CLIP_DEPTH: usize = 512;

/// PDF text field `Comb` flag (`1 << 24`, ISO 32000-1 Table 228).
const PDF_TX_COMB_FLAG: i64 = 1 << 24;

/// PDF annotation `Print` flag (`1 << 2`).
const PDF_ANNOT_PRINT_FLAG: i64 = 1 << 2;

/// Font resource name injected into `/Resources /Font` when filling flat forms.
const FLAT_FILL_FONT_RES: &[u8] = b"F_nupdf_ff";

/// Error returned when flat-form detection, pre-write overflow checks, PDF
/// serialization, or post-write read-back verification fails.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FlatFormError {
    /// Stable dotted error code (`"verify.overflow"`, `"verify.comb_overflow"`,
    /// `"input.target_not_found"`, `"usage.kind_mismatch"`, `"verify.readback"`, ...).
    pub code: &'static str,
    /// Human-readable diagnostic explanation.
    pub message: String,
}

impl std::fmt::Display for FlatFormError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for FlatFormError {}

/// Report of flat form fields detected (and optionally promoted) across a document.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FlatFormReport {
    /// Total pages in the document.
    pub page_count: i32,
    /// Detected flat fields in deterministic page and reading order.
    pub fields: Vec<FlatField>,
    /// Count of fields promoted to real `/AcroForm` `/Widget` annotations (`0` for detection-only).
    pub promoted_widgets: usize,
}

/// One synthesized flat form field (`"checkbox"`, `"comb"`, or `"underline"`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FlatField {
    /// Deterministic field ID (`"p1.ff1"`, `"p1.ff2"`, ...).
    pub id: String,
    /// Normalized unique `snake_case` field name (`"applicant_name"`, `"ssn"`, ...).
    pub name: String,
    /// 1-based page number.
    pub page: i32,
    /// `"checkbox"`, `"comb"`, or `"underline"`.
    pub kind: String,
    /// Bounding box `[x0, y0, x1, y1]` in MuPDF page space (origin top-left, `y` down).
    pub bbox: [f32; 4],
    /// Bounding box `[x0, y0, x1, y1]` in PDF user space (`y` up on unrotated pages).
    pub pdf_bbox: [f32; 4],
    /// Associated caption/label text located near the field.
    pub label: Option<String>,
    /// Current checked state for `"checkbox"` fields (`None` for `"underline"` / `"comb"`).
    pub checked: Option<bool>,
    /// Number of character cells for `"comb"` fields (`None` otherwise).
    pub cell_count: Option<usize>,
    /// Per-cell `[x0, x1]` horizontal bounds in MuPDF page space for `"comb"` fields.
    pub cell_x_ranges: Vec<[f32; 2]>,
    /// Existing text already sitting on/inside an `"underline"` or `"comb"` field.
    pub current_text: Option<String>,
    /// True when an `"underline"` blank was formed by consecutive `'_'` glyphs rather than a stroke.
    pub underscore_text: bool,
}

/// One fill operation targeting a detected flat field by `id`, `name`, or `label` substring.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FlatFillOp {
    /// Fill text into an `"underline"` or `"comb"` field.
    SetText { target: String, value: String },
    /// Check or uncheck a `"checkbox"` field.
    SetCheck { target: String, checked: bool },
}

/// Summary of one applied flat-form fill operation.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AppliedFlatFill {
    pub id: String,
    pub name: String,
    pub page: i32,
    pub kind: String,
    pub bbox: [f32; 4],
    pub value: Option<String>,
    pub checked: Option<bool>,
    pub font_size: Option<f32>,
}

/// Outcome of [`fill_flat_forms`] after post-write read-back verification.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FlatFillReport {
    pub applied: Vec<AppliedFlatFill>,
    pub verified: bool,
}

// ---------------------------------------------------------------------------
// 1. Flat-Form Detection
// ---------------------------------------------------------------------------

/// Detects flat `"checkbox"`, `"comb"`, and `"underline"` form fields across
/// `pages` (or all pages when `pages` is `None` or empty), skipping regions
/// already covered by `/AcroForm` widgets.
pub fn detect_flat_forms(doc: &Document, pages: Option<&str>) -> Result<FlatFormReport, String> {
    let page_count = doc.page_count().map_err(|e| e.0)?;
    if page_count <= 0 {
        return Ok(FlatFormReport {
            page_count: 0,
            fields: Vec::new(),
            promoted_widgets: 0,
        });
    }

    let raw_indices: Vec<i32> = match pages {
        Some(spec) if !spec.trim().is_empty() => {
            rearrange::parse_page_ranges(spec, page_count, doc)
        }
        _ => (0..page_count).collect(),
    };

    let wanted: HashSet<i32> = raw_indices
        .into_iter()
        .filter(|&idx| idx >= 0 && idx < page_count)
        .collect();

    // Names are de-duplicated over the whole document, in page order, so a
    // field has the same name whichever pages were listed; fill and promote
    // look fields up over all pages.
    let mut store = Store::default();
    let mut all_fields = Vec::new();
    let mut name_counts: HashMap<String, usize> = HashMap::new();

    for index in 0..page_count {
        let listed = wanted.contains(&index);
        let page_obj = match doc.page(index) {
            Ok(p) => p,
            Err(e) if listed => return Err(e.0),
            Err(_) => continue,
        };
        if doc.resolve(&page_obj).as_dict().is_none() {
            if listed {
                return Err(format!("page {} object is not a dictionary", index + 1));
            }
            continue;
        }
        let (res, next_store) =
            detect_page_flat_forms_with_store(doc, index, &page_obj, store, &mut name_counts);
        store = next_store;
        match res {
            Ok(fields) if listed => all_fields.extend(fields),
            Ok(_) => {}
            Err(e) if listed => return Err(e),
            Err(_) => {}
        }
    }

    Ok(FlatFormReport {
        page_count,
        fields: all_fields,
        promoted_widgets: 0,
    })
}

/// Detects flat form fields on a single 0-based `index` page. Names are
/// de-duplicated within the page only; [`detect_flat_forms`] gives the names
/// fill and promote use.
pub fn detect_page_flat_forms(doc: &Document, index: i32) -> Result<Vec<FlatField>, String> {
    let page_obj = doc.page(index).map_err(|e| e.0)?;
    if doc.resolve(&page_obj).as_dict().is_none() {
        return Err(format!("page {} object is not a dictionary", index + 1));
    }
    let mut name_counts = HashMap::new();
    detect_page_flat_forms_with_store(doc, index, &page_obj, Store::default(), &mut name_counts).0
}

fn detect_page_flat_forms_with_store(
    doc: &Document,
    index: i32,
    page_obj: &Object,
    mut store: Store,
    name_counts: &mut HashMap<String, usize>,
) -> (Result<Vec<FlatField>, String>, Store) {
    let page_num = index.saturating_add(1);
    let (raw_box, page_ctm) = match doc.page_transform(page_obj) {
        Ok(pair) => pair,
        Err(e) => return (Err(e.0), store),
    };
    let mediabox = raw_box.transform(page_ctm);
    let inv_ctm = page_ctm.invert();

    // Collect existing `/AcroForm` widget rects in MuPDF page space so we skip them.
    let (_, widgets) = annot::page_annots(doc, page_obj);
    let mut widget_rects: Vec<Rect> = Vec::with_capacity(widgets.len());
    for w in &widgets {
        if let Some(r) = annot::rect(doc, w, page_ctm)
            && r.is_valid()
            && !r.is_empty()
        {
            widget_rects.push(r);
        }
    }

    let ((stext_res, collector, next_store), _) = diag::capture(|| {
        let (s_res, s1) = stext::extract_page(doc, page_obj, stext::Options::default(), store);
        let mut fc = FlatCollector::new(Some(doc), mediabox);
        let run = page_run::run_page_parts(
            doc,
            page_obj,
            &mut fc,
            s1,
            Some("View"),
            Matrix::IDENTITY,
            Parts::CONTENTS,
        );
        (s_res, fc, run.store)
    });
    store = next_store;

    let stext_page = match stext_res {
        Ok(p) => p,
        Err(e) => return (Err(diag::report::strip(&e).to_string()), store),
    };

    let parsed_text = extract_flat_text(&stext_page);
    let raw_fields = synthesize_page_fields(
        page_num,
        mediabox,
        inv_ctm,
        &collector,
        &parsed_text,
        &widget_rects,
    );

    let mut out = Vec::with_capacity(raw_fields.len());
    for (seq, mut f) in raw_fields.into_iter().enumerate() {
        f.id = format!("p{page_num}.ff{}", seq + 1);
        let base_name = derive_base_field_name(f.label.as_deref(), &f.kind);
        let entry = name_counts.entry(base_name.clone()).or_insert(0);
        *entry += 1;
        f.name = if *entry == 1 {
            base_name
        } else {
            format!("{base_name}_{entry}")
        };
        out.push(f);
    }

    (Ok(out), store)
}

// ---------------------------------------------------------------------------
// Content-Stream Vector & Z-Order Collector
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipKind {
    Clip,
    Mask,
    Group,
    Tile,
}

#[derive(Debug, Clone, Copy)]
struct StrokedBox {
    bbox: Rect,
}

#[derive(Debug, Clone, Copy)]
struct HorizLine {
    x0: f32,
    x1: f32,
    y: f32,
}

#[derive(Debug, Clone, Copy)]
struct VertLine {
    x: f32,
    y0: f32,
    y1: f32,
}

/// The point a whiteout must cover to paint `m` over.
fn ink_midpoint(m: &InkMark) -> Point {
    match *m {
        InkMark::Seg { p0, p1 } => Point::new(0.5 * (p0.x + p1.x), 0.5 * (p0.y + p1.y)),
        InkMark::Box { bbox } => Point::new(0.5 * (bbox.x0 + bbox.x1), 0.5 * (bbox.y0 + bbox.y1)),
    }
}

/// The box a mark's ink lies in.
fn ink_box(m: &InkMark) -> Rect {
    match *m {
        InkMark::Seg { p0, p1 } => Rect::new(
            p0.x.min(p1.x),
            p0.y.min(p1.y),
            p0.x.max(p1.x),
            p0.y.max(p1.y),
        ),
        InkMark::Box { bbox } => bbox,
    }
}

/// Z-ordered dark stroke/fill/glyph mark used to determine whether a vector
/// checkbox interior is currently checked or has been painted white over.
#[derive(Debug, Clone, Copy)]
enum InkMark {
    Seg { p0: Point, p1: Point },
    Box { bbox: Rect },
}

#[cfg(test)]
thread_local! {
    /// Set by tests: every index answers with every item, as the scans the
    /// indexes replaced did, to check both find the same fields.
    static BRUTE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn brute() -> bool {
    BRUTE.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn brute() -> bool {
    false
}

/// Most grid cells one item is filed under; a bigger item, or one with a
/// coordinate that is not finite, is returned by every query.
const MAX_GRID_SPAN: i64 = 1024;

/// Item numbers filed under the cells of a uniform grid their boxes touch,
/// so what lies near a box is found without scanning every item. A query
/// returns a superset of the items whose box meets its box, in item order;
/// callers test their own predicate on each, so results are those of a scan.
struct Grid {
    cell: f32,
    cells: HashMap<(i64, i64), Vec<usize>>,
    wide: Vec<usize>,
    len: usize,
    brute: bool,
}

impl Grid {
    /// Cells of 16 pt, or of 1/128 of the page on a page over 2048 pt.
    fn new(mediabox: Rect) -> Self {
        let size = (mediabox.x1 - mediabox.x0)
            .abs()
            .max((mediabox.y1 - mediabox.y0).abs());
        let cell = if size.is_finite() {
            (size / 128.0).max(16.0)
        } else {
            16.0
        };
        Grid {
            cell,
            cells: HashMap::new(),
            wide: Vec::new(),
            len: 0,
            brute: brute(),
        }
    }

    /// The cells `r` touches, unless it has a coordinate that is not
    /// finite or touches more than [`MAX_GRID_SPAN`].
    fn span(&self, r: Rect) -> Option<(i64, i64, i64, i64)> {
        if ![r.x0, r.y0, r.x1, r.y1].iter().all(|v| v.is_finite()) {
            return None;
        }
        let at = |v: f32| (v / self.cell).floor() as i64;
        let (cx0, cx1) = (at(r.x0.min(r.x1)), at(r.x0.max(r.x1)));
        let (cy0, cy1) = (at(r.y0.min(r.y1)), at(r.y0.max(r.y1)));
        let cells = (cx1.saturating_sub(cx0).saturating_add(1))
            .saturating_mul(cy1.saturating_sub(cy0).saturating_add(1));
        (cells <= MAX_GRID_SPAN).then_some((cx0, cy0, cx1, cy1))
    }

    fn insert(&mut self, item: usize, r: Rect) {
        self.len = self.len.max(item.saturating_add(1));
        match self.span(r) {
            Some((cx0, cy0, cx1, cy1)) => {
                for cy in cy0..=cy1 {
                    for cx in cx0..=cx1 {
                        self.cells.entry((cx, cy)).or_default().push(item);
                    }
                }
            }
            None => self.wide.push(item),
        }
    }

    /// The items whose box may meet `r`, in item order.
    fn query(&self, r: Rect) -> Vec<usize> {
        let span = if self.brute { None } else { self.span(r) };
        let Some((cx0, cy0, cx1, cy1)) = span else {
            return (0..self.len).collect();
        };
        let mut out = self.wide.clone();
        let cells = (cx1 - cx0 + 1) * (cy1 - cy0 + 1);
        if cells as usize > self.cells.len() {
            for ((cx, cy), items) in &self.cells {
                if (cx0..=cx1).contains(cx) && (cy0..=cy1).contains(cy) {
                    out.extend(items);
                }
            }
        } else {
            for cy in cy0..=cy1 {
                for cx in cx0..=cx1 {
                    if let Some(items) = self.cells.get(&(cx, cy)) {
                        out.extend(items);
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Drops the items `keep` refuses from the cells `r` touches.
    fn prune(&mut self, r: Rect, keep: impl Fn(usize) -> bool) {
        if let Some((cx0, cy0, cx1, cy1)) = self.span(r)
            && ((cx1 - cx0 + 1) * (cy1 - cy0 + 1)) as usize <= self.cells.len()
        {
            for cy in cy0..=cy1 {
                for cx in cx0..=cx1 {
                    if let Some(items) = self.cells.get_mut(&(cx, cy)) {
                        items.retain(|&i| keep(i));
                        if items.is_empty() {
                            self.cells.remove(&(cx, cy));
                        }
                    }
                }
            }
        } else {
            self.cells.retain(|_, items| {
                items.retain(|&i| keep(i));
                !items.is_empty()
            });
        }
        self.wide.retain(|&i| keep(i));
    }
}

/// The point `p` as a box.
fn point_box(p: Point) -> Rect {
    Rect::new(p.x, p.y, p.x, p.y)
}

/// Rects in the order they were added, with a [`Grid`] over them.
struct RectIndex {
    rects: Vec<Rect>,
    grid: Grid,
}

impl RectIndex {
    fn new(mediabox: Rect) -> Self {
        RectIndex {
            rects: Vec::new(),
            grid: Grid::new(mediabox),
        }
    }

    fn of(mediabox: Rect, rects: &[Rect]) -> Self {
        let mut index = Self::new(mediabox);
        for &r in rects {
            index.push(r);
        }
        index
    }

    fn push(&mut self, r: Rect) {
        self.grid.insert(self.rects.len(), r);
        self.rects.push(r);
    }

    /// Whether a rect meeting `near` passes `test`: `near` must hold every
    /// rect that can.
    fn any(&self, near: Rect, test: impl Fn(&Rect) -> bool) -> bool {
        self.grid
            .query(near)
            .into_iter()
            .any(|i| self.rects.get(i).is_some_and(&test))
    }

    /// Whether `p` is inside one of the rects.
    fn covers(&self, p: Point) -> bool {
        self.any(point_box(p), |r| point_in_rect(p, *r))
    }
}

struct FlatCollector<'d> {
    doc: Option<&'d Document>,
    mediabox: Rect,
    stack: Vec<(Rect, ClipKind)>,
    overflow: usize,
    ops_seen: usize,
    stroked_boxes: Vec<StrokedBox>,
    horiz_lines: Vec<HorizLine>,
    vert_lines: Vec<VertLine>,
    /// Marks in the order they were made, those painted over since among
    /// them: [`FlatCollector::live_ink`] are the others.
    ink_marks: Vec<InkMark>,
    ink_alive: Vec<bool>,
    ink_live: usize,
    /// The marks by their midpoints, which a whiteout is tested against.
    ink_grid: Grid,
    /// The stroked boxes by their corners in 1 pt cells: a box within
    /// 0.8 pt of another on every side is in a neighbouring cell.
    box_cells: HashMap<[i64; 4], Vec<usize>>,
    whiteout_rects: Vec<Rect>,
}

impl<'d> FlatCollector<'d> {
    fn new(doc: Option<&'d Document>, mediabox: Rect) -> Self {
        Self {
            doc,
            mediabox,
            stack: Vec::new(),
            overflow: 0,
            ops_seen: 0,
            stroked_boxes: Vec::new(),
            horiz_lines: Vec::new(),
            vert_lines: Vec::new(),
            ink_marks: Vec::new(),
            ink_alive: Vec::new(),
            ink_live: 0,
            ink_grid: Grid::new(mediabox),
            box_cells: HashMap::new(),
            whiteout_rects: Vec::new(),
        }
    }

    /// The marks not painted over, in the order they were made.
    fn live_ink(&self) -> impl Iterator<Item = &InkMark> {
        self.ink_marks
            .iter()
            .zip(&self.ink_alive)
            .filter_map(|(m, &alive)| alive.then_some(m))
    }

    fn push_ink(&mut self, mark: InkMark) {
        if self.ink_live >= MAX_COLLECTED {
            return;
        }
        // Painted-over marks are dropped once they outnumber the rest.
        if self.ink_marks.len() >= 2 * MAX_COLLECTED {
            let live: Vec<InkMark> = self.live_ink().copied().collect();
            self.ink_marks.clear();
            self.ink_alive.clear();
            self.ink_live = 0;
            self.ink_grid = Grid::new(self.mediabox);
            for m in live {
                self.push_ink(m);
            }
        }
        self.ink_grid
            .insert(self.ink_marks.len(), point_box(ink_midpoint(&mark)));
        self.ink_marks.push(mark);
        self.ink_alive.push(true);
        self.ink_live += 1;
    }

    fn current_scissor(&self) -> Rect {
        self.stack.last().map_or(self.mediabox, |(r, _)| *r)
    }

    fn push_clip(&mut self, area: Rect, kind: ClipKind) {
        if self.stack.len() >= MAX_CLIP_DEPTH {
            self.overflow += 1;
            return;
        }
        let clipped = area.intersect(self.current_scissor());
        self.stack.push((clipped, kind));
    }

    fn pop_kind(&mut self, kind: ClipKind) {
        if self.overflow > 0 {
            self.overflow -= 1;
            return;
        }
        if let Some(pos) = self.stack.iter().rposition(|(_, k)| *k == kind) {
            self.stack.truncate(pos);
        }
    }

    fn rgb_u8(&self, cs: &ColorSpace, color: &[f32]) -> [u8; 3] {
        let rgb = cs.to_rgb(self.doc, color);
        let conv = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        [conv(rgb[0]), conv(rgb[1]), conv(rgb[2])]
    }

    fn is_page_backdrop(&self, r: Rect) -> bool {
        let pw = (self.mediabox.x1 - self.mediabox.x0).max(1.0);
        let ph = (self.mediabox.y1 - self.mediabox.y0).max(1.0);
        (r.x1 - r.x0) >= 0.85 * pw && (r.y1 - r.y0) >= 0.85 * ph
    }

    fn apply_whiteout(&mut self, white_rect: Rect) {
        if self.is_page_backdrop(white_rect) {
            return;
        }
        if self.whiteout_rects.len() < MAX_COLLECTED {
            self.whiteout_rects.push(white_rect);
        }
        for i in self.ink_grid.query(white_rect) {
            if self.ink_alive.get(i) == Some(&true)
                && let Some(m) = self.ink_marks.get(i)
                && point_in_rect(ink_midpoint(m), white_rect)
            {
                self.ink_alive[i] = false;
                self.ink_live -= 1;
            }
        }
        let alive = &self.ink_alive;
        self.ink_grid
            .prune(white_rect, |i| alive.get(i) == Some(&true));
    }

    fn add_stroked_box(&mut self, r: Rect) {
        if self.stroked_boxes.len() >= MAX_COLLECTED || self.is_page_backdrop(r) {
            return;
        }
        let near = |b: &StrokedBox| {
            (b.bbox.x0 - r.x0).abs() <= 0.8
                && (b.bbox.y0 - r.y0).abs() <= 0.8
                && (b.bbox.x1 - r.x1).abs() <= 0.8
                && (b.bbox.y1 - r.y1).abs() <= 0.8
        };
        // A box with a coordinate that is not finite is near no box.
        let key = [r.x0, r.y0, r.x1, r.y1].map(|v| v.floor() as i64);
        let finite = [r.x0, r.y0, r.x1, r.y1].iter().all(|v| v.is_finite());
        if brute() {
            if self.stroked_boxes.iter().any(near) {
                return;
            }
        } else if finite {
            for d in 0..81 {
                let mut k = key;
                let mut rest = d;
                for c in &mut k {
                    *c = c.saturating_add(rest % 3 - 1);
                    rest /= 3;
                }
                let dup = self.box_cells.get(&k).is_some_and(|boxes| {
                    boxes
                        .iter()
                        .any(|&i| self.stroked_boxes.get(i).is_some_and(near))
                });
                if dup {
                    return;
                }
            }
        }
        if finite {
            self.box_cells
                .entry(key)
                .or_default()
                .push(self.stroked_boxes.len());
        }
        self.stroked_boxes.push(StrokedBox { bbox: r });
    }

    fn record_path(
        &mut self,
        path: &Path,
        ctm: Matrix,
        stroke: Option<&StrokeState>,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
    ) {
        if alpha <= 0.02 || self.ops_seen >= MAX_COLLECTED || path.is_empty() {
            return;
        }
        let scissor = self.current_scissor();
        if scissor.is_empty() {
            return;
        }

        let rgb = self.rgb_u8(cs, color);
        let near_white = rgb[0] >= 250 && rgb[1] >= 250 && rgb[2] >= 250;

        let budget = MAX_COLLECTED.saturating_sub(self.ops_seen);
        let mut walker = FlatWalker::new(ctm, budget);
        path.walk(&mut walker);
        walker.finish_open();
        self.ops_seen = self.ops_seen.saturating_add(walker.ops);

        if let Some(st) = stroke {
            if near_white {
                return;
            }
            let lw = (if st.linewidth <= 0.0 {
                1.0
            } else {
                st.linewidth
            }) * path::max_expansion(ctm);
            for sub in walker.subpaths {
                let r = sub.bounds.intersect(scissor);
                if !r.is_valid() {
                    continue;
                }
                if let Some(axis_rect) = sub.axis_rect() {
                    let ar = axis_rect.intersect(scissor);
                    if ar.is_valid() && !ar.is_empty() {
                        self.add_stroked_box(ar);
                    }
                    continue;
                }
                for win in sub.pts.windows(2) {
                    let a = win[0];
                    let b = win[1];
                    let dx = (b.x - a.x).abs();
                    let dy = (b.y - a.y).abs();
                    if dx <= 0.05 && dy <= 0.05 {
                        continue;
                    }
                    if dy <= 1.0 && dx >= 3.0 && lw <= 3.5 && self.horiz_lines.len() < MAX_COLLECTED
                    {
                        self.horiz_lines.push(HorizLine {
                            x0: a.x.min(b.x),
                            x1: a.x.max(b.x),
                            y: 0.5 * (a.y + b.y),
                        });
                    } else if dx <= 1.0
                        && dy >= 3.0
                        && lw <= 3.5
                        && self.vert_lines.len() < MAX_COLLECTED
                    {
                        self.vert_lines.push(VertLine {
                            x: 0.5 * (a.x + b.x),
                            y0: a.y.min(b.y),
                            y1: a.y.max(b.y),
                        });
                    }
                    self.push_ink(InkMark::Seg { p0: a, p1: b });
                }
            }
        } else {
            // Filled path.
            for sub in walker.subpaths {
                let r = sub.bounds.intersect(scissor);
                if !r.is_valid() || r.is_empty() {
                    continue;
                }
                if near_white {
                    if alpha >= 0.90
                        && let Some(ar) = sub.axis_rect()
                    {
                        self.apply_whiteout(ar.intersect(scissor));
                    }
                    continue;
                }
                let w = r.x1 - r.x0;
                let h = r.y1 - r.y0;
                if let Some(ar) = sub.axis_rect() {
                    let ar = ar.intersect(scissor);
                    let aw = ar.x1 - ar.x0;
                    let ah = ar.y1 - ar.y0;
                    // Thin filled horizontal or vertical rules act just like stroked lines.
                    if ah <= 2.2 && aw >= 25.0 && self.horiz_lines.len() < MAX_COLLECTED {
                        self.horiz_lines.push(HorizLine {
                            x0: ar.x0,
                            x1: ar.x1,
                            y: 0.5 * (ar.y0 + ar.y1),
                        });
                        continue;
                    }
                    if aw <= 2.2 && ah >= 4.0 && self.vert_lines.len() < MAX_COLLECTED {
                        self.vert_lines.push(VertLine {
                            x: 0.5 * (ar.x0 + ar.x1),
                            y0: ar.y0,
                            y1: ar.y1,
                        });
                        continue;
                    }
                }
                if w <= 22.0 && h <= 22.0 {
                    self.push_ink(InkMark::Box { bbox: r });
                }
            }
        }
    }

    fn record_text_ink(
        &mut self,
        text: &Text,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
    ) {
        if alpha <= 0.02 || self.ink_live >= MAX_COLLECTED {
            return;
        }
        let rgb = self.rgb_u8(cs, color);
        if rgb[0] >= 250 && rgb[1] >= 250 && rgb[2] >= 250 {
            return;
        }
        let bound = text.bound(None, ctm).intersect(self.current_scissor());
        if bound.is_valid()
            && !bound.is_empty()
            && (bound.x1 - bound.x0) <= 24.0
            && (bound.y1 - bound.y0) <= 24.0
        {
            self.push_ink(InkMark::Box { bbox: bound });
        }
    }
}

#[allow(clippy::too_many_arguments)]
impl Device for FlatCollector<'_> {
    fn fill_path(
        &mut self,
        path: &Path,
        _even_odd: bool,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        _params: ColorParams,
    ) {
        self.record_path(path, ctm, None, cs, color, alpha);
    }

    fn stroke_path(
        &mut self,
        path: &Path,
        stroke: &StrokeState,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        _params: ColorParams,
    ) {
        self.record_path(path, ctm, Some(stroke), cs, color, alpha);
    }

    fn fill_text(
        &mut self,
        text: &Text,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        _params: ColorParams,
    ) {
        self.record_text_ink(text, ctm, cs, color, alpha);
    }

    fn stroke_text(
        &mut self,
        text: &Text,
        _stroke: &StrokeState,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        _params: ColorParams,
    ) {
        self.record_text_ink(text, ctm, cs, color, alpha);
    }

    fn clip_path(&mut self, path: &Path, _even_odd: bool, ctm: Matrix, scissor: Rect) {
        self.push_clip(path.bound(None, ctm).intersect(scissor), ClipKind::Clip);
    }

    fn clip_stroke_path(&mut self, path: &Path, stroke: &StrokeState, ctm: Matrix, scissor: Rect) {
        self.push_clip(
            path.bound(Some(stroke), ctm).intersect(scissor),
            ClipKind::Clip,
        );
    }

    fn clip_image_mask(&mut self, _image: &Image, ctm: Matrix, scissor: Rect) {
        self.push_clip(Rect::UNIT.transform(ctm).intersect(scissor), ClipKind::Clip);
    }

    fn clip_text(&mut self, text: &Text, ctm: Matrix, scissor: Rect) {
        self.push_clip(text.bound(None, ctm).intersect(scissor), ClipKind::Clip);
    }

    fn clip_stroke_text(&mut self, text: &Text, stroke: &StrokeState, ctm: Matrix, scissor: Rect) {
        self.push_clip(
            text.bound(Some(stroke), ctm).intersect(scissor),
            ClipKind::Clip,
        );
    }

    fn pop_clip(&mut self) {
        self.pop_kind(ClipKind::Clip);
    }

    fn begin_mask(
        &mut self,
        area: Rect,
        _luminosity: bool,
        _cs: Option<&ColorSpace>,
        _backdrop: &[f32],
        _params: ColorParams,
    ) {
        self.push_clip(area, ClipKind::Mask);
    }

    fn end_mask(&mut self, _transfer: Option<&Function>) {
        if self.overflow > 0 {
            return;
        }
        if let Some((_, k)) = self.stack.last_mut()
            && *k == ClipKind::Mask
        {
            *k = ClipKind::Clip;
        }
    }

    fn begin_group(
        &mut self,
        area: Rect,
        _cs: Option<&ColorSpace>,
        _isolated: bool,
        _knockout: bool,
        _blendmode: u8,
        _alpha: f32,
    ) {
        self.push_clip(area, ClipKind::Group);
    }

    fn end_group(&mut self) {
        self.pop_kind(ClipKind::Group);
    }

    fn begin_tile(
        &mut self,
        _area: Rect,
        view: Rect,
        _xstep: f32,
        _ystep: f32,
        ctm: Matrix,
        _id: i32,
    ) -> bool {
        self.push_clip(view.transform(ctm), ClipKind::Tile);
        false
    }

    fn end_tile(&mut self) {
        self.pop_kind(ClipKind::Tile);
    }
}

struct FlatSubpath {
    pts: Vec<Point>,
    closed: bool,
    has_curves: bool,
    bounds: Rect,
}

impl FlatSubpath {
    fn axis_rect(&self) -> Option<Rect> {
        if self.has_curves {
            return None;
        }
        let closed = self.closed
            || (self.pts.len() >= 4
                && (self.pts[0].x - self.pts[self.pts.len() - 1].x)
                    .hypot(self.pts[0].y - self.pts[self.pts.len() - 1].y)
                    <= 0.8);
        if !closed {
            return None;
        }
        let mut c: Vec<Point> = Vec::new();
        for &p in &self.pts {
            if c.last()
                .is_none_or(|prev| (prev.x - p.x).hypot(prev.y - p.y) > 0.4)
            {
                c.push(p);
            }
        }
        if c.len() >= 2 && (c[0].x - c[c.len() - 1].x).hypot(c[0].y - c[c.len() - 1].y) <= 0.8 {
            c.pop();
        }
        if c.len() != 4 {
            return None;
        }
        let h_first = (c[0].y - c[1].y).abs() <= 0.8
            && (c[1].x - c[2].x).abs() <= 0.8
            && (c[2].y - c[3].y).abs() <= 0.8
            && (c[3].x - c[0].x).abs() <= 0.8;
        let v_first = (c[0].x - c[1].x).abs() <= 0.8
            && (c[1].y - c[2].y).abs() <= 0.8
            && (c[2].x - c[3].x).abs() <= 0.8
            && (c[3].y - c[0].y).abs() <= 0.8;
        (h_first || v_first).then_some(self.bounds)
    }
}

struct FlatWalker {
    ctm: Matrix,
    budget: usize,
    ops: usize,
    pts: Vec<Point>,
    has_curves: bool,
    bounds: Rect,
    subpaths: Vec<FlatSubpath>,
}

impl FlatWalker {
    fn new(ctm: Matrix, budget: usize) -> Self {
        Self {
            ctm,
            budget,
            ops: 0,
            pts: Vec::new(),
            has_curves: false,
            bounds: Rect::EMPTY,
            subpaths: Vec::new(),
        }
    }

    fn finish(&mut self, closed: bool) {
        if self.pts.len() >= 2 && self.bounds.is_valid() {
            let pts = std::mem::take(&mut self.pts);
            self.subpaths.push(FlatSubpath {
                pts,
                closed,
                has_curves: self.has_curves,
                bounds: self.bounds,
            });
        } else {
            self.pts.clear();
        }
        self.has_curves = false;
        self.bounds = Rect::EMPTY;
    }

    fn finish_open(&mut self) {
        self.finish(false);
    }
}

impl Walker for FlatWalker {
    fn move_to(&mut self, x: f32, y: f32) {
        if self.ops >= self.budget {
            return;
        }
        self.finish_open();
        let p = self.ctm.transform_xy(x, y);
        self.pts.push(p);
        self.bounds = Rect::new(p.x, p.y, p.x, p.y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        if self.pts.is_empty() || self.ops >= self.budget {
            return;
        }
        self.ops += 1;
        let p = self.ctm.transform_xy(x, y);
        self.pts.push(p);
        self.bounds = self.bounds.include_point(p);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) {
        if self.pts.is_empty() || self.ops >= self.budget {
            return;
        }
        self.ops += 1;
        self.has_curves = true;
        let p1 = self.ctm.transform_xy(x1, y1);
        let p2 = self.ctm.transform_xy(x2, y2);
        let p3 = self.ctm.transform_xy(x3, y3);
        self.bounds = self
            .bounds
            .include_point(p1)
            .include_point(p2)
            .include_point(p3);
        self.pts.push(p3);
    }

    fn close_path(&mut self) {
        if self.pts.is_empty() {
            return;
        }
        if self.ops < self.budget {
            self.ops += 1;
        }
        self.finish(true);
    }
}

// ---------------------------------------------------------------------------
// Text Span & Underscore Run Extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct LabelSpan {
    text: String,
    bbox: Rect,
    cy: f32,
}

#[derive(Debug, Clone)]
struct UnderscoreBlank {
    bbox: Rect,
    inline_label: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct GlyphItem {
    ch: char,
    cx: f32,
    cy: f32,
}

struct FlatPageText {
    labels: Vec<LabelSpan>,
    underscores: Vec<UnderscoreBlank>,
    glyphs: Vec<GlyphItem>,
}

fn extract_flat_text(stext_page: &StextPage) -> FlatPageText {
    let mut labels = Vec::new();
    let mut underscores = Vec::new();
    let mut glyphs = Vec::new();
    extract_blocks_text(
        &stext_page.blocks,
        &mut labels,
        &mut underscores,
        &mut glyphs,
    );
    FlatPageText {
        labels,
        underscores,
        glyphs,
    }
}

fn extract_blocks_text(
    blocks: &[Block],
    labels: &mut Vec<LabelSpan>,
    underscores: &mut Vec<UnderscoreBlank>,
    glyphs: &mut Vec<GlyphItem>,
) {
    for block in blocks {
        match block {
            Block::Text(tb) => {
                for line in &tb.lines {
                    for ch in &line.chars {
                        if ch.flags & char_flags::SYNTHETIC != 0 {
                            continue;
                        }
                        let c = rune_char(ch.c);
                        if !c.is_whitespace() {
                            let r = Rect::from_quad(ch.quad);
                            glyphs.push(GlyphItem {
                                ch: c,
                                cx: 0.5 * (r.x0 + r.x1),
                                cy: 0.5 * (r.y0 + r.y1),
                            });
                        }
                    }

                    // Scan `line.chars` for runs of >= 4 `'_'` characters AND non-underscore label segments.
                    let n = line.chars.len();
                    let line_size = extract::spans(line)
                        .first()
                        .map_or(11.0, |s| s.style.size.max(9.0));
                    let mut i = 0usize;
                    let mut last_label_text: Option<String> = None;
                    let mut last_label_x1: f32 = f32::NEG_INFINITY;

                    while i < n {
                        if rune_char(line.chars[i].c) == '_' {
                            let u_start = i;
                            let mut u_count = 0usize;
                            while i < n && rune_char(line.chars[i].c) == '_' {
                                u_count += 1;
                                i += 1;
                            }
                            if u_count >= 4
                                && let Some(u_slice) = line.chars.get(u_start..i)
                                && let Some(raw_r) = non_ws_bbox(u_slice)
                            {
                                let field_h = (line_size * 1.25).clamp(12.0, 18.0);
                                let top_y = raw_r.y0.min(raw_r.y1 - field_h);
                                let bbox = Rect::new(raw_r.x0, top_y, raw_r.x1, raw_r.y1);
                                let inline_label = last_label_text
                                    .as_ref()
                                    .filter(|_| (bbox.x0 - last_label_x1) <= 90.0)
                                    .cloned();
                                underscores.push(UnderscoreBlank { bbox, inline_label });
                                last_label_text = None;
                            }
                        } else {
                            let seg_start = i;
                            while i < n && rune_char(line.chars[i].c) != '_' {
                                i += 1;
                            }
                            if let Some(seg_slice) = line.chars.get(seg_start..i) {
                                for (rs, re) in split_label_columns(seg_slice, line_size) {
                                    if let Some(sub) = seg_slice.get(rs..re) {
                                        let txt = extract::chars_text(sub).trim().to_string();
                                        if !txt.is_empty()
                                            && let Some(r) = non_ws_bbox(sub)
                                        {
                                            last_label_text = Some(txt.clone());
                                            last_label_x1 = r.x1;
                                            labels.push(LabelSpan {
                                                text: txt,
                                                bbox: r,
                                                cy: 0.5 * (r.y0 + r.y1),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Block::Struct(sb) => extract_blocks_text(&sb.blocks, labels, underscores, glyphs),
            Block::Image(_) | Block::Vector(_) | Block::Grid(_) => {}
        }
    }
}

fn non_ws_bbox(chars: &[StextChar]) -> Option<Rect> {
    let mut r = Rect::EMPTY;
    let mut any = false;
    for ch in chars {
        if ch.flags & char_flags::SYNTHETIC == 0 && !rune_char(ch.c).is_whitespace() {
            r = r.union(Rect::from_quad(ch.quad));
            any = true;
        }
    }
    any.then_some(r)
}

fn split_label_columns(chars: &[StextChar], font_size: f32) -> Vec<(usize, usize)> {
    let n = chars.len();
    let gap_thresh = (font_size * 1.4).max(10.0);
    let is_ws = |c: &StextChar| rune_char(c.c).is_whitespace();

    let mut out = Vec::new();
    let mut i = 0usize;
    while i < n && is_ws(&chars[i]) {
        i += 1;
    }
    let mut start = i;
    let mut last_r: Option<Rect> = None;

    while i < n {
        if is_ws(&chars[i]) {
            let ws_start = i;
            while i < n && is_ws(&chars[i]) {
                i += 1;
            }
            if i < n
                && let Some(prev) = last_r
            {
                let next = Rect::from_quad(chars[i].quad);
                if (next.x0 - prev.x1) > gap_thresh || (i - ws_start) >= 2 {
                    if ws_start > start {
                        out.push((start, ws_start));
                    }
                    start = i;
                    last_r = None;
                    continue;
                }
            }
            continue;
        }
        let cur = Rect::from_quad(chars[i].quad);
        if let Some(prev) = last_r
            && (cur.x0 - prev.x1) > gap_thresh
        {
            if i > start {
                out.push((start, i));
            }
            start = i;
        }
        last_r = Some(cur);
        i += 1;
    }

    let mut end = n;
    while end > start && is_ws(&chars[end - 1]) {
        end -= 1;
    }
    if end > start {
        out.push((start, end));
    }
    out
}

// ---------------------------------------------------------------------------
// Geometry & Field Synthesis
// ---------------------------------------------------------------------------

fn rect_array(r: Rect) -> [f32; 4] {
    [r.x0, r.y0, r.x1, r.y1]
}

fn normalize_rect(r: Rect) -> Rect {
    Rect::new(
        r.x0.min(r.x1),
        r.y0.min(r.y1),
        r.x0.max(r.x1),
        r.y0.max(r.y1),
    )
}

fn point_in_rect(p: Point, r: Rect) -> bool {
    p.x >= r.x0 && p.x <= r.x1 && p.y >= r.y0 && p.y <= r.y1
}

fn overlaps_widget(r: Rect, widgets: &RectIndex) -> bool {
    widgets.any(r.expand(3.0), |w| !r.intersect(w.expand(2.0)).is_empty())
}

/// What a page's fields are looked for among, each with a [`Grid`]: the
/// glyphs by their centres, the captions by their boxes and centre lines,
/// the vertical lines by their ends and their extent, and the marks and
/// whiteouts by their boxes.
struct PageIndex<'a> {
    text: &'a FlatPageText,
    glyphs: Grid,
    labels: Grid,
    vl_starts: Grid,
    vl_ends: Grid,
    vl_extents: Grid,
    ink: Vec<InkMark>,
    ink_boxes: Grid,
    whiteouts: RectIndex,
    widgets: RectIndex,
}

impl<'a> PageIndex<'a> {
    fn new(
        mediabox: Rect,
        collector: &FlatCollector<'_>,
        text: &'a FlatPageText,
        widget_rects: &[Rect],
    ) -> Self {
        let mut glyphs = Grid::new(mediabox);
        for (i, g) in text.glyphs.iter().enumerate() {
            glyphs.insert(i, point_box(Point::new(g.cx, g.cy)));
        }
        let mut labels = Grid::new(mediabox);
        for (i, sp) in text.labels.iter().enumerate() {
            labels.insert(i, label_box(sp));
        }
        let (mut vl_starts, mut vl_ends, mut vl_extents) = (
            Grid::new(mediabox),
            Grid::new(mediabox),
            Grid::new(mediabox),
        );
        for (i, vl) in collector.vert_lines.iter().enumerate() {
            vl_starts.insert(i, point_box(Point::new(vl.x, vl.y0)));
            vl_ends.insert(i, point_box(Point::new(vl.x, vl.y1)));
            vl_extents.insert(i, Rect::new(vl.x, vl.y0, vl.x, vl.y1));
        }
        let ink: Vec<InkMark> = collector.live_ink().copied().collect();
        let mut ink_boxes = Grid::new(mediabox);
        for (i, m) in ink.iter().enumerate() {
            ink_boxes.insert(i, ink_box(m));
        }
        PageIndex {
            text,
            glyphs,
            labels,
            vl_starts,
            vl_ends,
            vl_extents,
            ink,
            ink_boxes,
            whiteouts: RectIndex::of(mediabox, &collector.whiteout_rects),
            widgets: RectIndex::of(mediabox, widget_rects),
        }
    }

    /// The glyphs whose centres may be in `r`, in page order.
    fn glyphs_near(&self, r: Rect) -> impl Iterator<Item = &'a GlyphItem> + '_ {
        self.glyphs
            .query(r)
            .into_iter()
            .filter_map(|i| self.text.glyphs.get(i))
    }

    /// The captions that may meet `r` (box or centre line), with their
    /// numbers, in page order.
    fn labels_near(&self, r: Rect) -> Vec<(usize, &'a LabelSpan)> {
        self.labels
            .query(r)
            .into_iter()
            .filter_map(|i| self.text.labels.get(i).map(|sp| (i, sp)))
            .collect()
    }

    fn whited_out(&self, p: Point) -> bool {
        self.whiteouts.covers(p)
    }
}

/// The box a caption is filed under: its box and its centre line, a point
/// wider, so every caption test near a box finds it there.
fn label_box(sp: &LabelSpan) -> Rect {
    let b = sp.bbox;
    Rect::new(
        b.x0.min(b.x1) - 1.0,
        b.y0.min(b.y1).min(sp.cy) - 1.0,
        b.x0.max(b.x1) + 1.0,
        b.y0.max(b.y1).max(sp.cy) + 1.0,
    )
}

/// The box from `(x0, y0)` to `(x1, y1)` whichever way round, a point
/// wider: a query box that holds the region a predicate allows.
fn around(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
    Rect::new(
        x0.min(x1) - 1.0,
        y0.min(y1) - 1.0,
        x0.max(x1) + 1.0,
        y0.max(y1) + 1.0,
    )
}

fn synthesize_page_fields(
    page_num: i32,
    mediabox: Rect,
    inv_ctm: Matrix,
    collector: &FlatCollector<'_>,
    text: &FlatPageText,
    widget_rects: &[Rect],
) -> Vec<FlatField> {
    let mut fields: Vec<FlatField> = Vec::new();
    let mut used_boxes = vec![false; collector.stroked_boxes.len()];
    let mut comb_regions = RectIndex::new(mediabox);
    let index = PageIndex::new(mediabox, collector, text, widget_rects);
    let widget_rects = &index.widgets;

    // -----------------------------------------------------------------------
    // 1. COMB GRIDS: >= 3 adjacent equal-width stroked cells OR baseline+ticks
    // -----------------------------------------------------------------------
    let mut cell_cands: Vec<(usize, Rect)> = collector
        .stroked_boxes
        .iter()
        .enumerate()
        .filter_map(|(idx, sb)| {
            let w = sb.bbox.x1 - sb.bbox.x0;
            let h = sb.bbox.y1 - sb.bbox.y0;
            ((8.0..=28.0).contains(&w)
                && (10.0..=30.0).contains(&h)
                && !overlaps_widget(sb.bbox, widget_rects))
            .then_some((idx, sb.bbox))
        })
        .collect();

    cell_cands.sort_by(|(_, a), (_, b)| {
        let ra = (a.y0 / 4.0).round() as i32;
        let rb = (b.y0 / 4.0).round() as i32;
        ra.cmp(&rb).then_with(|| a.x0.total_cmp(&b.x0))
    });

    // Sorted by band, a cell's row-mates in the old scan of every later
    // cell are the rest of its band and the band below it, and within a
    // band, sorted by x0, only those whose gap from the row's last cell
    // is in range can join: the rest are skipped by a binary search.
    let bands: Vec<i64> = cell_cands
        .iter()
        .map(|(_, r)| (r.y0 / 4.0).round() as i64)
        .collect();
    let mut run_end = vec![cell_cands.len(); cell_cands.len()];
    for k in (0..cell_cands.len().saturating_sub(1)).rev() {
        if bands[k] == bands[k + 1] {
            run_end[k] = run_end[k + 1];
        } else {
            run_end[k] = k + 1;
        }
    }
    let mut visited_cand = vec![false; cell_cands.len()];
    for i in 0..cell_cands.len() {
        if visited_cand[i] {
            continue;
        }
        let (_, r0) = cell_cands[i];
        let w0 = r0.x1 - r0.x0;
        let mut row: Vec<(usize, usize, Rect)> = vec![(i, cell_cands[i].0, r0)];
        let mut runs = vec![(i + 1, run_end[i])];
        if let Some(&next) = run_end.get(i)
            && next < cell_cands.len()
            && bands[next] == bands[i].saturating_add(1)
        {
            runs.push((next, run_end[next]));
        }
        if brute() {
            runs = vec![(i + 1, cell_cands.len())];
        }

        for (lo, hi) in runs {
            let mut j = lo;
            while j < hi {
                let prev_x1 = row[row.len() - 1].2.x1;
                if !brute() {
                    j += cell_cands[j..hi].partition_point(|(_, r)| r.x0 - prev_x1 < -2.5);
                    if j >= hi || cell_cands[j].1.x0 - prev_x1 > 3.5 {
                        break;
                    }
                }
                let (box_idx, rj) = cell_cands[j];
                if !visited_cand[j] && (rj.y0 - r0.y0).abs() <= 2.5 && (rj.y1 - r0.y1).abs() <= 2.5
                {
                    let prev_r = row[row.len() - 1].2;
                    let gap = rj.x0 - prev_r.x1;
                    let wj = rj.x1 - rj.x0;
                    if (-2.5..=3.5).contains(&gap) && (wj - w0).abs() <= 2.0 {
                        row.push((j, box_idx, rj));
                    }
                }
                j += 1;
            }
        }

        if row.len() >= 3 {
            // Skip table header rows where >= 2 cells already enclose multi-character words.
            let multi_char_cells = row
                .iter()
                .filter(|(_, _, cr)| {
                    let count = index
                        .glyphs_near(*cr)
                        .filter(|g| point_in_rect(Point::new(g.cx, g.cy), *cr))
                        .count();
                    count >= 2
                })
                .count();
            if multi_char_cells >= 2 {
                continue;
            }

            let mut overall = Rect::EMPTY;
            let mut x_ranges = Vec::with_capacity(row.len());
            for &(cand_i, box_idx, cr) in &row {
                visited_cand[cand_i] = true;
                used_boxes[box_idx] = true;
                overall = overall.union(cr);
                x_ranges.push([cr.x0, cr.x1]);
            }
            comb_regions.push(overall);

            let current_text = extract_comb_current_text(&x_ranges, overall.y0, overall.y1, &index);
            let label = find_comb_or_underline_label(overall, &index);
            let pdf_r = normalize_rect(overall.transform(inv_ctm));

            fields.push(FlatField {
                id: String::new(),
                name: String::new(),
                page: page_num,
                kind: "comb".to_string(),
                bbox: rect_array(overall),
                pdf_bbox: rect_array(pdf_r),
                label,
                checked: None,
                cell_count: Some(x_ranges.len()),
                cell_x_ranges: x_ranges,
                current_text,
                underscore_text: false,
            });
        }
    }

    // Also detect comb grids drawn as a single outer stroked box subdivided by >= 2 equal-spaced interior vertical strokes.
    for (box_idx, sb) in collector.stroked_boxes.iter().enumerate() {
        if used_boxes[box_idx] {
            continue;
        }
        let bw = sb.bbox.x1 - sb.bbox.x0;
        let bh = sb.bbox.y1 - sb.bbox.y0;
        if bw < 24.0 || !(10.0..=30.0).contains(&bh) || overlaps_widget(sb.bbox, widget_rects) {
            continue;
        }
        if comb_regions.any(sb.bbox.expand(1.0), |cr| !sb.bbox.intersect(*cr).is_empty()) {
            continue;
        }
        let near = Rect::new(sb.bbox.x0, sb.bbox.y0 - 3.5, sb.bbox.x1, sb.bbox.y0 + 3.5);
        let mut xs: Vec<f32> = index
            .vl_starts
            .query(near)
            .into_iter()
            .filter_map(|i| collector.vert_lines.get(i))
            .filter(|vl| {
                vl.x > sb.bbox.x0 + 4.0
                    && vl.x < sb.bbox.x1 - 4.0
                    && (vl.y0 - sb.bbox.y0).abs() <= 2.5
                    && (vl.y1 - sb.bbox.y1).abs() <= 2.5
            })
            .map(|vl| vl.x)
            .collect();
        if xs.len() < 2 {
            continue;
        }
        xs.push(sb.bbox.x0);
        xs.push(sb.bbox.x1);
        xs.sort_by(f32::total_cmp);
        xs.dedup_by(|a, b| (*a - *b).abs() <= 1.0);
        if xs.len() < 4 {
            continue;
        }
        let w0 = xs[1] - xs[0];
        if !(8.0..=28.0).contains(&w0) || !xs.windows(2).all(|w| ((w[1] - w[0]) - w0).abs() <= 2.0)
        {
            continue;
        }
        let x_ranges: Vec<[f32; 2]> = xs.windows(2).map(|w| [w[0], w[1]]).collect();
        let multi_char_cells = x_ranges
            .iter()
            .filter(|&&[cx0, cx1]| {
                let cr = Rect::new(cx0, sb.bbox.y0, cx1, sb.bbox.y1);
                index
                    .glyphs_near(cr)
                    .filter(|g| point_in_rect(Point::new(g.cx, g.cy), cr))
                    .count()
                    >= 2
            })
            .count();
        if multi_char_cells >= 2 {
            continue;
        }

        used_boxes[box_idx] = true;
        comb_regions.push(sb.bbox);
        let current_text = extract_comb_current_text(&x_ranges, sb.bbox.y0, sb.bbox.y1, &index);
        let label = find_comb_or_underline_label(sb.bbox, &index);
        let pdf_r = normalize_rect(sb.bbox.transform(inv_ctm));
        fields.push(FlatField {
            id: String::new(),
            name: String::new(),
            page: page_num,
            kind: "comb".to_string(),
            bbox: rect_array(sb.bbox),
            pdf_bbox: rect_array(pdf_r),
            label,
            checked: None,
            cell_count: Some(x_ranges.len()),
            cell_x_ranges: x_ranges,
            current_text,
            underscore_text: false,
        });
    }

    // Also detect comb grids drawn as a horizontal baseline + >= 4 vertical tick dividers.
    for hl in &collector.horiz_lines {
        let hw = hl.x1 - hl.x0;
        if hw < 24.0 {
            continue;
        }
        let probe = Rect::new(hl.x0, hl.y - 20.0, hl.x1, hl.y + 2.0);
        if overlaps_widget(probe, widget_rects)
            || comb_regions.any(probe.expand(1.0), |cr| !probe.intersect(*cr).is_empty())
        {
            continue;
        }
        let near = Rect::new(hl.x0 - 3.0, hl.y - 3.5, hl.x1 + 3.0, hl.y + 3.5);
        let mut ticks: Vec<&VertLine> = index
            .vl_ends
            .query(near)
            .into_iter()
            .filter_map(|i| collector.vert_lines.get(i))
            .filter(|vl| {
                vl.x >= hl.x0 - 2.0
                    && vl.x <= hl.x1 + 2.0
                    && (vl.y1 - hl.y).abs() <= 2.5
                    && (4.0..=30.0).contains(&(vl.y1 - vl.y0))
            })
            .collect();
        if ticks.len() < 4 {
            continue;
        }
        ticks.sort_by(|a, b| a.x.total_cmp(&b.x));
        ticks.dedup_by(|a, b| (a.x - b.x).abs() <= 1.0);
        if ticks.len() < 4 {
            continue;
        }

        let w0 = ticks[1].x - ticks[0].x;
        if !(8.0..=28.0).contains(&w0) {
            continue;
        }
        let equal_widths = ticks
            .windows(2)
            .all(|w| ((w[1].x - w[0].x) - w0).abs() <= 2.0);
        if !equal_widths {
            continue;
        }
        let max_tick_h = ticks
            .iter()
            .map(|t| t.y1 - t.y0)
            .fold(14.0_f32, f32::max)
            .max(14.0);
        let overall = Rect::new(
            ticks[0].x,
            hl.y - max_tick_h,
            ticks[ticks.len() - 1].x,
            hl.y,
        );
        let x_ranges: Vec<[f32; 2]> = ticks.windows(2).map(|w| [w[0].x, w[1].x]).collect();
        comb_regions.push(overall);

        let current_text = extract_comb_current_text(&x_ranges, overall.y0, overall.y1, &index);
        let label = find_comb_or_underline_label(overall, &index);
        let pdf_r = normalize_rect(overall.transform(inv_ctm));

        fields.push(FlatField {
            id: String::new(),
            name: String::new(),
            page: page_num,
            kind: "comb".to_string(),
            bbox: rect_array(overall),
            pdf_bbox: rect_array(pdf_r),
            label,
            checked: None,
            cell_count: Some(x_ranges.len()),
            cell_x_ranges: x_ranges,
            current_text,
            underscore_text: false,
        });
    }

    // -----------------------------------------------------------------------
    // 2. VECTOR CHECKBOXES: 5 <= w,h <= 18, |w - h| <= 3, not in a comb
    // -----------------------------------------------------------------------
    let mut checkbox_rects = RectIndex::new(mediabox);
    for (idx, sb) in collector.stroked_boxes.iter().enumerate() {
        if used_boxes[idx] {
            continue;
        }
        let w = sb.bbox.x1 - sb.bbox.x0;
        let h = sb.bbox.y1 - sb.bbox.y0;
        if !(5.0..=18.0).contains(&w) || !(5.0..=18.0).contains(&h) || (w - h).abs() > 3.0 {
            continue;
        }
        if overlaps_widget(sb.bbox, widget_rects)
            || comb_regions.any(sb.bbox.expand(2.0), |cr| {
                !sb.bbox.intersect(cr.expand(1.0)).is_empty()
            })
        {
            continue;
        }
        if checkbox_rects.any(sb.bbox.expand(1.0), |existing| {
            !existing.intersect(sb.bbox).is_empty()
        }) {
            continue;
        }
        checkbox_rects.push(sb.bbox);
    }

    for &cb_rect in &checkbox_rects.rects {
        let checked = is_checkbox_checked(cb_rect, &index);
        let label = find_checkbox_label(cb_rect, &checkbox_rects, &index);
        let pdf_r = normalize_rect(cb_rect.transform(inv_ctm));
        fields.push(FlatField {
            id: String::new(),
            name: String::new(),
            page: page_num,
            kind: "checkbox".to_string(),
            bbox: rect_array(cb_rect),
            pdf_bbox: rect_array(pdf_r),
            label,
            checked: Some(checked),
            cell_count: None,
            cell_x_ranges: Vec::new(),
            current_text: None,
            underscore_text: false,
        });
    }

    // -----------------------------------------------------------------------
    // 3. UNDERLINES: horizontal stroked rule (`>= 35 pt`) OR `>= 4` `'_'` run
    // -----------------------------------------------------------------------
    let page_w = (mediabox.x1 - mediabox.x0).max(1.0);
    let mut underline_rects = RectIndex::new(mediabox);

    for hl in &collector.horiz_lines {
        let w = hl.x1 - hl.x0;
        if w < 35.0 || w >= 0.82 * page_w {
            continue;
        }
        let cand_bbox = Rect::new(hl.x0, hl.y - 14.0, hl.x1, hl.y);
        if overlaps_widget(cand_bbox, widget_rects)
            || comb_regions.any(cand_bbox.expand(3.0), |cr| {
                !cand_bbox.intersect(cr.expand(2.0)).is_empty()
            })
        {
            continue;
        }
        // Skip duplicate parallel rules at almost the same coordinates.
        let near = Rect::new(hl.x0 - 6.0, hl.y - 4.0, hl.x0 + 6.0, hl.y + 4.0);
        if underline_rects.any(near, |ur| {
            (ur.y1 - hl.y).abs() <= 3.0
                && (ur.x0 - hl.x0).abs() <= 5.0
                && (ur.x1 - hl.x1).abs() <= 5.0
        }) {
            continue;
        }
        // Skip table borders: vertical strokes > 10 pt meeting both endpoints.
        let posts_near = |x: f32| {
            index
                .vl_extents
                .query(Rect::new(x - 3.0, hl.y - 3.0, x + 3.0, hl.y + 3.0))
                .into_iter()
                .filter_map(|i| collector.vert_lines.get(i))
        };
        let left_post = posts_near(hl.x0).any(|vl| {
            (vl.x - hl.x0).abs() <= 2.0
                && (vl.y1 - vl.y0) > 10.0
                && hl.y >= vl.y0 - 2.0
                && hl.y <= vl.y1 + 2.0
        });
        let right_post = posts_near(hl.x1).any(|vl| {
            (vl.x - hl.x1).abs() <= 2.0
                && (vl.y1 - vl.y0) > 10.0
                && hl.y >= vl.y0 - 2.0
                && hl.y <= vl.y1 + 2.0
        });
        if left_post && right_post {
            continue;
        }

        // Skip text strikethroughs (crossing through middle 25%..75% of a span).
        let is_strikethrough = index
            .labels_near(Rect::new(hl.x0 - 1.0, hl.y - 2.0, hl.x1 + 1.0, hl.y + 2.0))
            .into_iter()
            .any(|(_, sp)| {
                let sh = (sp.bbox.y1 - sp.bbox.y0).max(1.0);
                let overlap = hl.x1.min(sp.bbox.x1) - hl.x0.max(sp.bbox.x0);
                overlap >= 0.4 * (sp.bbox.x1 - sp.bbox.x0).max(1.0)
                    && hl.y >= sp.bbox.y0 + 0.2 * sh
                    && hl.y <= sp.bbox.y0 + 0.78 * sh
            });
        if is_strikethrough {
            continue;
        }

        let (current_text, on_line_span_idxs) =
            find_text_sitting_on_underline(hl.x0, hl.x1, hl.y, &index);
        let label = find_underline_label(cand_bbox, hl.y, &index, &on_line_span_idxs);

        // Reject stylistic underlined headings that tightly match a span's exact width with no field label.
        if label.is_none()
            && on_line_span_idxs.len() == 1
            && let Some(sp) = text.labels.get(on_line_span_idxs[0])
            && (sp.bbox.x0 - hl.x0).abs() <= 3.5
            && (sp.bbox.x1 - hl.x1).abs() <= 3.5
        {
            continue;
        }

        underline_rects.push(cand_bbox);
        let pdf_r = normalize_rect(cand_bbox.transform(inv_ctm));
        fields.push(FlatField {
            id: String::new(),
            name: String::new(),
            page: page_num,
            kind: "underline".to_string(),
            bbox: rect_array(cand_bbox),
            pdf_bbox: rect_array(pdf_r),
            label,
            checked: None,
            cell_count: None,
            cell_x_ranges: Vec::new(),
            current_text,
            underscore_text: false,
        });
    }

    for ub in &text.underscores {
        if overlaps_widget(ub.bbox, widget_rects)
            || underline_rects.any(ub.bbox.expand(1.0), |ur| !ur.intersect(ub.bbox).is_empty())
        {
            continue;
        }
        let (current_text, on_line_idxs) =
            find_text_sitting_on_underline(ub.bbox.x0, ub.bbox.x1, ub.bbox.y1, &index);
        let label = ub
            .inline_label
            .clone()
            .or_else(|| find_underline_label(ub.bbox, ub.bbox.y1, &index, &on_line_idxs));
        underline_rects.push(ub.bbox);
        let pdf_r = normalize_rect(ub.bbox.transform(inv_ctm));
        fields.push(FlatField {
            id: String::new(),
            name: String::new(),
            page: page_num,
            kind: "underline".to_string(),
            bbox: rect_array(ub.bbox),
            pdf_bbox: rect_array(pdf_r),
            label,
            checked: None,
            cell_count: None,
            cell_x_ranges: Vec::new(),
            current_text,
            underscore_text: true,
        });
    }

    // Sort deterministically in reading order (top-to-bottom bands, then left-to-right).
    fields.sort_by(|a, b| {
        let ra = (a.bbox[3] / 8.0).round() as i32;
        let rb = (b.bbox[3] / 8.0).round() as i32;
        ra.cmp(&rb).then_with(|| a.bbox[0].total_cmp(&b.bbox[0]))
    });

    fields
}

fn is_checkbox_checked(cb: Rect, index: &PageIndex<'_>) -> bool {
    let w = cb.x1 - cb.x0;
    let h = cb.y1 - cb.y0;
    let inset = (0.22 * w.min(h)).clamp(1.1, 2.5);
    let inner = Rect::new(cb.x0 + inset, cb.y0 + inset, cb.x1 - inset, cb.y1 - inset);
    if !inner.is_valid() || inner.is_empty() {
        return false;
    }

    // 1. Check surviving z-ordered vector strokes or small dark fills crossing `inner`.
    for i in index.ink_boxes.query(inner) {
        let Some(m) = index.ink.get(i) else {
            continue;
        };
        match *m {
            InkMark::Seg { p0, p1 } => {
                if segment_intersects_rect(p0, p1, inner) {
                    return true;
                }
            }
            InkMark::Box { bbox } => {
                let inter = bbox.intersect(inner);
                if inter.is_valid() && !inter.is_empty() {
                    let iw = inter.x1 - inter.x0;
                    let ih = inter.y1 - inter.y0;
                    if iw >= 1.2 && ih >= 1.2 {
                        return true;
                    }
                }
            }
        }
    }

    // 2. Check visible check/cross glyph centered inside `cb` (unless covered by a whiteout rect).
    index.glyphs_near(cb).any(|g| {
        let pt = Point::new(g.cx, g.cy);
        let is_mark_char = matches!(
            g.ch,
            '✓' | '✔' | '☑' | '✗' | '✘' | '×' | 'X' | 'x' | 'v' | 'V' | '4' | '8'
        );
        is_mark_char && point_in_rect(pt, cb.expand(-0.5)) && !index.whited_out(pt)
    })
}

fn segment_intersects_rect(a: Point, b: Point, r: Rect) -> bool {
    if point_in_rect(a, r) || point_in_rect(b, r) {
        return true;
    }
    // Sample interior points along segment `a..b` for fast, exact convex intersection.
    for step in 1..8 {
        let t = (step as f32) * 0.125;
        let p = Point::new(a.x + t * (b.x - a.x), a.y + t * (b.y - a.y));
        if point_in_rect(p, r) {
            return true;
        }
    }
    false
}

fn find_checkbox_label(
    cb: Rect,
    all_checkboxes: &RectIndex,
    index: &PageIndex<'_>,
) -> Option<String> {
    let cb_cy = 0.5 * (cb.y0 + cb.y1);

    let no_box_between = |x_lo: f32, x_hi: f32| -> bool {
        let near = around(x_lo, cb_cy - 9.0, x_hi, cb_cy + 9.0);
        !all_checkboxes.any(near, |other| {
            let o_cy = 0.5 * (other.y0 + other.y1);
            (o_cy - cb_cy).abs() <= 9.0 && other.x0 > x_lo + 0.5 && other.x1 < x_hi - 0.5
        })
    };
    let near = index.labels_near(Rect::new(
        cb.x0 - 82.0,
        cb_cy - 10.0,
        cb.x1 + 82.0,
        cb_cy + 10.0,
    ));
    let labels = near.iter().map(|(_, sp)| *sp);

    // Candidate immediately to the right (`dx in 1.5..=80 pt`).
    let right_cand = labels
        .clone()
        .filter(|sp| {
            let dx = sp.bbox.x0 - cb.x1;
            (1.5..=80.0).contains(&dx)
                && (sp.cy - cb_cy).abs() <= 9.0
                && sp.text.chars().count() > 1
                && no_box_between(cb.x1, sp.bbox.x0)
        })
        .min_by(|a, b| (a.bbox.x0 - cb.x1).total_cmp(&(b.bbox.x0 - cb.x1)));

    // Candidate immediately to the left (`dx in 1.5..=80 pt`).
    let left_cand = labels
        .filter(|sp| {
            let dx = cb.x0 - sp.bbox.x1;
            (1.5..=80.0).contains(&dx)
                && (sp.cy - cb_cy).abs() <= 9.0
                && sp.text.chars().count() > 1
                && no_box_between(sp.bbox.x1, cb.x0)
        })
        .min_by(|a, b| (cb.x0 - a.bbox.x1).total_cmp(&(cb.x0 - b.bbox.x1)));

    match (right_cand, left_cand) {
        (Some(r), Some(l)) => {
            let dr = r.bbox.x0 - cb.x1;
            let dl = cb.x0 - l.bbox.x1;
            // Standard `[ ] Domestic` layout puts caption to the right unless right text is a prompt (`Foo:`).
            if !r.text.trim_end().ends_with(':') && (dr <= 36.0 || dr <= dl + 8.0) {
                Some(r.text.clone())
            } else {
                Some(l.text.clone())
            }
        }
        (Some(r), None) => Some(r.text.clone()),
        (None, Some(l)) => Some(l.text.clone()),
        (None, None) => None,
    }
}

fn find_comb_or_underline_label(target: Rect, index: &PageIndex<'_>) -> Option<String> {
    let t_cy = 0.5 * (target.y0 + target.y1);

    // 1. Left on same baseline (`gap <= 95 pt`).
    let near = index.labels_near(Rect::new(
        target.x0 - 96.0,
        t_cy - 11.0,
        target.x0 + 3.0,
        t_cy + 11.0,
    ));
    if let Some(left) = near
        .iter()
        .map(|(_, sp)| *sp)
        .filter(|sp| {
            let dx = target.x0 - sp.bbox.x1;
            (-2.0..=95.0).contains(&dx) && (sp.cy - t_cy).abs() <= 10.0
        })
        .min_by(|a, b| (target.x0 - a.bbox.x1).total_cmp(&(target.x0 - b.bbox.x1)))
    {
        return Some(left.text.clone());
    }

    // 2. Directly above (`dy <= 16 pt`).
    let near = index.labels_near(around(
        target.x0 - 8.0,
        target.y0 - 16.0,
        target.x1,
        target.y0 + 2.0,
    ));
    near.iter()
        .map(|(_, sp)| *sp)
        .filter(|sp| {
            let dy = target.y0 - sp.bbox.y1;
            (-2.0..=16.0).contains(&dy) && sp.bbox.x1 >= target.x0 - 8.0 && sp.bbox.x0 <= target.x1
        })
        .min_by(|a, b| (target.y0 - a.bbox.y1).total_cmp(&(target.y0 - b.bbox.y1)))
        .map(|sp| sp.text.clone())
}

fn find_text_sitting_on_underline(
    x0: f32,
    x1: f32,
    line_y: f32,
    index: &PageIndex<'_>,
) -> (Option<String>, Vec<usize>) {
    let mut idxs = Vec::new();
    let mut words: Vec<(f32, &str)> = Vec::new();
    let near = index.labels_near(around(x0 - 3.0, line_y - 6.0, x1 + 4.0, line_y + 2.5));
    for (i, sp) in near {
        let mid = Point::new(0.5 * (sp.bbox.x0 + sp.bbox.x1), sp.cy);
        if index.whited_out(mid) {
            continue;
        }
        let horiz_in = sp.bbox.x0 >= x0 - 3.0 && sp.bbox.x1 <= x1 + 4.0;
        let sits_on = sp.bbox.y1 >= line_y - 6.0 && sp.bbox.y1 <= line_y + 2.5 && sp.cy < line_y;
        if horiz_in && sits_on {
            idxs.push(i);
            words.push((sp.bbox.x0, sp.text.as_str()));
        }
    }
    words.sort_by(|a, b| a.0.total_cmp(&b.0));
    let joined = words
        .into_iter()
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join(" ");
    let cur = (!joined.is_empty()).then_some(joined);
    (cur, idxs)
}

fn find_underline_label(
    cand_bbox: Rect,
    line_y: f32,
    index: &PageIndex<'_>,
    on_line_idxs: &[usize],
) -> Option<String> {
    let t_cy = line_y - 5.0;

    // 1. Span immediately to the left on the same baseline (`gap <= 90 pt`).
    let near = index.labels_near(Rect::new(
        cand_bbox.x0 - 91.0,
        t_cy - 10.0,
        cand_bbox.x0 + 4.0,
        t_cy + 10.0,
    ));
    if let Some(left) = near
        .into_iter()
        .filter(|(i, sp)| {
            !on_line_idxs.contains(i)
                && (-3.0..=90.0).contains(&(cand_bbox.x0 - sp.bbox.x1))
                && (sp.cy - t_cy).abs() <= 9.0
        })
        .min_by(|(_, a), (_, b)| (cand_bbox.x0 - a.bbox.x1).total_cmp(&(cand_bbox.x0 - b.bbox.x1)))
        .map(|(_, sp)| sp)
    {
        return Some(left.text.clone());
    }

    // 2. Caption directly below the line (`dy in 0.5..=16 pt`, e.g., signature caption).
    let near = index.labels_near(around(
        cand_bbox.x0 + 2.0,
        line_y,
        cand_bbox.x1 - 2.0,
        line_y + 16.0,
    ));
    if let Some(below) = near
        .into_iter()
        .filter(|(i, sp)| {
            !on_line_idxs.contains(i)
                && (0.5..=16.0).contains(&(sp.bbox.y0 - line_y))
                && sp.bbox.x1 >= cand_bbox.x0 + 2.0
                && sp.bbox.x0 <= cand_bbox.x1 - 2.0
        })
        .min_by(|(_, a), (_, b)| (a.bbox.y0 - line_y).total_cmp(&(b.bbox.y0 - line_y)))
        .map(|(_, sp)| sp)
    {
        return Some(below.text.clone());
    }

    // 3. Caption directly above the field box (`dy <= 16 pt`).
    let near = index.labels_near(around(
        cand_bbox.x0,
        cand_bbox.y0 - 16.0,
        cand_bbox.x1,
        cand_bbox.y0 + 2.0,
    ));
    near.into_iter()
        .filter(|(i, sp)| {
            !on_line_idxs.contains(i)
                && (-2.0..=16.0).contains(&(cand_bbox.y0 - sp.bbox.y1))
                && sp.bbox.x1 >= cand_bbox.x0
                && sp.bbox.x0 <= cand_bbox.x1
        })
        .min_by(|(_, a), (_, b)| (cand_bbox.y0 - a.bbox.y1).total_cmp(&(cand_bbox.y0 - b.bbox.y1)))
        .map(|(_, sp)| sp.text.clone())
}

fn extract_comb_current_text(
    cell_x_ranges: &[[f32; 2]],
    y0: f32,
    y1: f32,
    index: &PageIndex<'_>,
) -> Option<String> {
    let mut out = String::new();
    for &[cx0, cx1] in cell_x_ranges {
        let cell_r = Rect::new(cx0, y0 - 1.5, cx1, y1 + 1.5);
        let mut cell_chars: Vec<(f32, char)> = index
            .glyphs_near(cell_r)
            .filter(|g| {
                let pt = Point::new(g.cx, g.cy);
                point_in_rect(pt, cell_r) && !index.whited_out(pt)
            })
            .map(|g| (g.cx, g.ch))
            .collect();
        cell_chars.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (_, ch) in cell_chars {
            out.push(ch);
        }
    }
    (!out.is_empty()).then_some(out)
}

fn normalize_snake_case(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_underscore = true;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_underscore = false;
        } else if !prev_underscore && !out.is_empty() {
            out.push('_');
            prev_underscore = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        out.insert_str(0, "f_");
    }
    out
}

fn derive_base_field_name(label: Option<&str>, kind: &str) -> String {
    if let Some(lbl) = label {
        let norm = normalize_snake_case(lbl);
        if !norm.is_empty() {
            return norm;
        }
    }
    match kind {
        "checkbox" => "checkbox".to_string(),
        "comb" => "comb_field".to_string(),
        _ => "text_field".to_string(),
    }
}

/// Most top-level `/AcroForm /Fields` entries read when naming promoted fields.
const MAX_EXISTING_FIELDS: usize = 50_000;

// ---------------------------------------------------------------------------
// 2. Verified Flat-Form Filling
// ---------------------------------------------------------------------------

/// Fills flat form fields on a clean `O(1)` fork of `doc`, serializes with
/// `write_opts`, reads the output back as every writer does
/// ([`write::save_verified`]), and verifies the fills on the reopened bytes.
pub fn fill_flat_forms(
    doc: &Document,
    ops: &[FlatFillOp],
    write_opts: &WriteOptions,
) -> Result<(Vec<u8>, FlatFillReport), FlatFormError> {
    fill_flat_forms_with_password(doc, ops, write_opts, None)
}

/// [`fill_flat_forms`] for a document opened with `password`, which the
/// read-back needs when the output stays encrypted.
pub fn fill_flat_forms_with_password(
    doc: &Document,
    ops: &[FlatFillOp],
    write_opts: &WriteOptions,
    password: Option<&str>,
) -> Result<(Vec<u8>, FlatFillReport), FlatFormError> {
    let staged = stage_flat_fill(doc, ops)?;
    let bytes = write::save_verified(&staged.work, write_opts, password).map_err(save_error)?;
    let report = staged.finish(&bytes, password)?;
    Ok((bytes, report))
}

fn save_error(e: write::CheckedSaveError) -> FlatFormError {
    FlatFormError {
        code: e.code,
        message: e.message,
    }
}

/// Fills made on a fork of the input, for the caller to save (and read
/// back) and hand the bytes to [`StagedFlatFill::finish`], which checks
/// each fill on them.
#[derive(Debug)]
pub struct StagedFlatFill {
    /// The fork the fills were made on: what to save, and whose
    /// diagnostics to report.
    pub work: Document,
    applied: Vec<AppliedFlatFill>,
}

impl StagedFlatFill {
    /// The fill report for `bytes`, a save of [`StagedFlatFill::work`],
    /// once every fill reads back (with `password` when locked).
    pub fn finish(
        &self,
        bytes: &[u8],
        password: Option<&str>,
    ) -> Result<FlatFillReport, FlatFormError> {
        if !self.applied.is_empty() {
            verify_filled_bytes(bytes, &self.applied, password)?;
        }
        Ok(FlatFillReport {
            applied: self.applied.clone(),
            verified: true,
        })
    }
}

/// The fills of [`fill_flat_forms`], with nothing saved yet.
pub fn stage_flat_fill(
    doc: &Document,
    ops: &[FlatFillOp],
) -> Result<StagedFlatFill, FlatFormError> {
    let fork = doc.fork_clean();
    let detected = detect_flat_forms(&fork, None).map_err(|message| FlatFormError {
        code: "input.detect_failed",
        message,
    })?;

    if ops.is_empty() {
        return Ok(StagedFlatFill {
            work: fork,
            applied: Vec::new(),
        });
    }

    // Resolve every `FlatFillOp` to a unique `FlatField` index.
    let mut resolved_ops: Vec<(usize, &FlatFillOp)> = Vec::with_capacity(ops.len());
    for op in ops {
        let target = match op {
            FlatFillOp::SetText { target, .. } | FlatFillOp::SetCheck { target, .. } => target,
        };
        let field_idx = resolve_target_field(&detected.fields, target)?;
        let field = &detected.fields[field_idx];
        match op {
            FlatFillOp::SetText { .. } if field.kind == "checkbox" => {
                return Err(FlatFormError {
                    code: "usage.kind_mismatch",
                    message: format!(
                        "field {:?} ({:?}) is a checkbox; use SetCheck instead of SetText",
                        field.id, field.name
                    ),
                });
            }
            FlatFillOp::SetCheck { .. } if field.kind != "checkbox" => {
                return Err(FlatFormError {
                    code: "usage.kind_mismatch",
                    message: format!(
                        "field {:?} ({:?}) has kind {:?}; use SetText instead of SetCheck",
                        field.id, field.name, field.kind
                    ),
                });
            }
            _ => {}
        }
        resolved_ops.push((field_idx, op));
    }

    // Group operations by 1-based page number so each edited page gets one balanced stream.
    let mut ops_by_page: BTreeMap<i32, Vec<(usize, usize)>> = BTreeMap::new();
    for (op_idx, &(field_idx, _)) in resolved_ops.iter().enumerate() {
        let p = detected.fields[field_idx].page;
        ops_by_page.entry(p).or_default().push((op_idx, field_idx));
    }

    let mut store = Store::default();
    let mut helv_obj_num: Option<i32> = None;
    let mut applied_slots: Vec<Option<AppliedFlatFill>> = vec![None; ops.len()];

    for (page_num, page_ops) in ops_by_page {
        let page_index = page_num - 1;
        let page_obj = fork.page(page_index).map_err(|e| FlatFormError {
            code: "input.page_tree",
            message: e.0,
        })?;
        let (raw_box, page_ctm) = fork.page_transform(&page_obj).map_err(|e| FlatFormError {
            code: "input.page_tree",
            message: e.0,
        })?;
        let mediabox = raw_box.transform(page_ctm);
        let inv_ctm = page_ctm.invert();

        // Extract page spans so pre-write overflow checks know exact right-neighbor span bounds.
        let (s_res, next_store) =
            stext::extract_page(&fork, &page_obj, stext::Options::default(), store);
        store = next_store;
        let stext_page = s_res.map_err(|message| FlatFormError {
            code: "input.page_tree",
            message,
        })?;
        let page_text = extract_flat_text(&stext_page);

        let mut stream_buf = Vec::new();
        stream_buf.extend_from_slice(b"q\n");
        let mut needs_font = false;

        for (op_idx, field_idx) in page_ops {
            let field = &detected.fields[field_idx];
            let op = resolved_ops[op_idx].1;
            let [x0, y0, x1, y1] = field.bbox;

            match op {
                FlatFillOp::SetText { value, .. } if field.kind == "underline" => {
                    let field_w = (x1 - x0).max(1.0);
                    let avail_w = (field_w - 2.0).max(1.0);
                    let base_size = 10.5_f32;
                    let w_at_base = measure_helvetica_width(value, base_size);
                    let font_size = if w_at_base > avail_w && w_at_base > 0.0 {
                        (base_size * (avail_w / w_at_base)).max(7.5)
                    } else {
                        base_size
                    };
                    let final_w = measure_helvetica_width(value, font_size);
                    let text_start_x = x0 + 1.0;
                    let text_end_x = text_start_x + final_w;

                    // Pre-write verification: reject if even at 7.5 pt the text overflows `bbox` width
                    // (including colliding with a right-hand neighbor span or page edge).
                    if final_w > field_w + 0.5 {
                        let hits_neighbor = page_text.labels.iter().any(|sp| {
                            sp.bbox.x0 >= x1 - 2.0
                                && sp.bbox.x0 <= text_end_x + 1.0
                                && (sp.cy - (y1 - 5.0)).abs() <= 9.0
                        });
                        let detail = if hits_neighbor {
                            "and collides with neighbor span to the right"
                        } else {
                            "exceeding underline blank width at minimum 7.5 pt font size"
                        };
                        return Err(FlatFormError {
                            code: "verify.overflow",
                            message: format!(
                                "value {:?} ({final_w:.1} pt at {font_size:.1} pt) overflows field {:?} ({field_w:.1} pt wide) {detail}",
                                value, field.id
                            ),
                        });
                    }
                    if text_end_x > mediabox.x1 - 1.0 {
                        return Err(FlatFormError {
                            code: "verify.overflow",
                            message: format!(
                                "value {:?} in field {:?} overflows right page margin",
                                value, field.id
                            ),
                        });
                    }

                    // Mask out previous `current_text` above the rule when overwriting.
                    if field.current_text.is_some() && !field.underscore_text {
                        let mask_page = Rect::new(x0, y0 + 0.5, x1, (y1 - 1.2).max(y0 + 1.0));
                        let mask_pdf = normalize_rect(mask_page.transform(inv_ctm));
                        append_white_rect_pdf(&mut stream_buf, mask_pdf);
                    }

                    if !value.is_empty() {
                        needs_font = true;
                        let lift = if field.underscore_text { 1.5 } else { 2.0 };
                        let origin_page = Point::new(text_start_x, y1 - lift);
                        let tm = baseline_text_matrix(origin_page, page_ctm);
                        append_winansi_text_op(&mut stream_buf, tm, font_size, value);
                    }

                    applied_slots[op_idx] = Some(AppliedFlatFill {
                        id: field.id.clone(),
                        name: field.name.clone(),
                        page: page_num,
                        kind: field.kind.clone(),
                        bbox: field.bbox,
                        value: Some(value.clone()),
                        checked: None,
                        font_size: Some(font_size),
                    });
                }

                FlatFillOp::SetText { value, .. } if field.kind == "comb" => {
                    let cell_count = field.cell_count.unwrap_or(field.cell_x_ranges.len());
                    let chars: Vec<char> = value.chars().collect();
                    if chars.len() > cell_count {
                        return Err(FlatFormError {
                            code: "verify.comb_overflow",
                            message: format!(
                                "value {:?} has {} characters, exceeding comb field {:?} cell_count={cell_count}",
                                value,
                                chars.len(),
                                field.id
                            ),
                        });
                    }

                    // Ifcomb already had characters, mask cell interiors cleanly without touching cell borders.
                    if field.current_text.is_some() {
                        for &[cx0, cx1] in &field.cell_x_ranges {
                            let inner = Rect::new(cx0 + 1.2, y0 + 1.2, cx1 - 1.2, y1 - 1.2);
                            if inner.is_valid() && !inner.is_empty() {
                                append_white_rect_pdf(
                                    &mut stream_buf,
                                    normalize_rect(inner.transform(inv_ctm)),
                                );
                            }
                        }
                    }

                    let cell_h = (y1 - y0).max(10.0);
                    let font_size = (cell_h * 0.64).clamp(8.0, 12.0);
                    let base_y = y0 + cell_h * 0.74;

                    if !chars.is_empty() {
                        needs_font = true;
                        let mut char_buf = String::with_capacity(4);
                        for (ci, &ch) in chars.iter().enumerate() {
                            if ch.is_whitespace() {
                                continue;
                            }
                            let Some(&[cx0, cx1]) = field.cell_x_ranges.get(ci) else {
                                break;
                            };
                            char_buf.clear();
                            char_buf.push(ch);
                            let ch_w = measure_helvetica_width(&char_buf, font_size);
                            let origin_x = 0.5 * (cx0 + cx1) - 0.5 * ch_w;
                            let tm = baseline_text_matrix(Point::new(origin_x, base_y), page_ctm);
                            append_winansi_text_op(&mut stream_buf, tm, font_size, &char_buf);
                        }
                    }

                    applied_slots[op_idx] = Some(AppliedFlatFill {
                        id: field.id.clone(),
                        name: field.name.clone(),
                        page: page_num,
                        kind: field.kind.clone(),
                        bbox: field.bbox,
                        value: Some(value.clone()),
                        checked: None,
                        font_size: Some(font_size),
                    });
                }

                FlatFillOp::SetCheck { checked, .. } => {
                    let currently_checked = field.checked.unwrap_or(false);
                    let w = (x1 - x0).max(4.0);
                    let h = (y1 - y0).max(4.0);

                    if *checked && !currently_checked {
                        let pa = inv_ctm.transform_xy(x0 + 0.22 * w, y0 + 0.55 * h);
                        let pb = inv_ctm.transform_xy(x0 + 0.43 * w, y0 + 0.76 * h);
                        let pc = inv_ctm.transform_xy(x0 + 0.80 * w, y0 + 0.24 * h);
                        stream_buf.extend_from_slice(b"q\n0 0 0 RG\n1.5 w\n1 J\n1 j\n");
                        push_xy_op(&mut stream_buf, pa.x, pa.y, b"m");
                        push_xy_op(&mut stream_buf, pb.x, pb.y, b"l");
                        push_xy_op(&mut stream_buf, pc.x, pc.y, b"l");
                        stream_buf.extend_from_slice(b"S\nQ\n");
                    } else if !*checked && currently_checked {
                        let inset = 1.2_f32.min(0.22 * w.min(h));
                        let inner = Rect::new(x0 + inset, y0 + inset, x1 - inset, y1 - inset);
                        if inner.is_valid() && !inner.is_empty() {
                            append_white_rect_pdf(
                                &mut stream_buf,
                                normalize_rect(inner.transform(inv_ctm)),
                            );
                        }
                    }

                    applied_slots[op_idx] = Some(AppliedFlatFill {
                        id: field.id.clone(),
                        name: field.name.clone(),
                        page: page_num,
                        kind: field.kind.clone(),
                        bbox: field.bbox,
                        value: None,
                        checked: Some(*checked),
                        font_size: None,
                    });
                }

                _ => {}
            }
        }

        stream_buf.extend_from_slice(b"Q\n");

        // Only text needs the font; a page with only checkmarks gets none.
        let font_obj = needs_font
            .then(|| *helv_obj_num.get_or_insert_with(|| add_helvetica_font_object(&fork)));

        append_content_layer_with_font(&fork, page_index, font_obj, stream_buf).map_err(
            |message| FlatFormError {
                code: "input.page_edit",
                message,
            },
        )?;
    }

    let mut applied = Vec::with_capacity(ops.len());
    for slot in applied_slots {
        let Some(item) = slot else {
            return Err(FlatFormError {
                code: "internal.missing_slot",
                message: "internal flat-fill resolution mismatch".into(),
            });
        };
        applied.push(item);
    }

    Ok(StagedFlatFill {
        work: fork,
        applied,
    })
}

fn resolve_target_field(fields: &[FlatField], target: &str) -> Result<usize, FlatFormError> {
    let trimmed = target.trim();
    if let Some(pos) = fields.iter().position(|f| f.id == trimmed) {
        return Ok(pos);
    }
    if let Some(pos) = fields.iter().position(|f| f.name == trimmed) {
        return Ok(pos);
    }
    let norm = normalize_snake_case(trimmed);
    if !norm.is_empty()
        && let Some(pos) = fields.iter().position(|f| f.name == norm)
    {
        return Ok(pos);
    }
    let lower = trimmed.to_ascii_lowercase();
    if !lower.is_empty()
        && let Some(pos) = fields.iter().position(|f| {
            f.label
                .as_ref()
                .is_some_and(|l| l.to_ascii_lowercase().contains(&lower))
        })
    {
        return Ok(pos);
    }
    Err(FlatFormError {
        code: "input.target_not_found",
        message: format!("no flat form field matched target {target:?}"),
    })
}

fn measure_helvetica_width(text: &str, font_size: f32) -> f32 {
    let Some(font) = fontdesc::base14_font("Helvetica") else {
        return (text.chars().count() as f32) * font_size * 0.52;
    };
    let mut total_em = 0.0_f32;
    for ch in text.chars() {
        let u = ch as u32;
        let gid = font
            .face
            .as_ref()
            .map_or(0, |face| face.char_index(u) as i32);
        let adv = if gid > 0 {
            fontdesc::advance_glyph(&font, gid, 0)
        } else {
            0.52
        };
        total_em += if adv > 0.0 { adv } else { 0.278 };
    }
    total_em * font_size
}

fn baseline_text_matrix(origin_page: Point, page_ctm: Matrix) -> Matrix {
    let glyph_to_page = Matrix {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: -1.0,
        e: origin_page.x,
        f: origin_page.y,
    };
    glyph_to_page.concat(page_ctm.invert())
}

fn push_float(out: &mut Vec<u8>, v: f32) {
    crate::print::fmtfloat(v, out);
}

fn push_xy_op(out: &mut Vec<u8>, x: f32, y: f32, op: &[u8]) {
    push_float(out, x);
    out.push(b' ');
    push_float(out, y);
    out.push(b' ');
    out.extend_from_slice(op);
    out.push(b'\n');
}

fn append_white_rect_pdf(out: &mut Vec<u8>, pdf_r: Rect) {
    out.extend_from_slice(b"q\n1 1 1 rg\n");
    push_float(out, pdf_r.x0);
    out.push(b' ');
    push_float(out, pdf_r.y0);
    out.push(b' ');
    push_float(out, (pdf_r.x1 - pdf_r.x0).max(0.1));
    out.push(b' ');
    push_float(out, (pdf_r.y1 - pdf_r.y0).max(0.1));
    out.extend_from_slice(b" re\nf\nQ\n");
}

fn append_winansi_text_op(out: &mut Vec<u8>, tm: Matrix, font_size: f32, text: &str) {
    out.extend_from_slice(b"BT\n0 Tr\n0 0 0 rg\n/");
    out.extend_from_slice(FLAT_FILL_FONT_RES);
    out.push(b' ');
    push_float(out, font_size);
    out.extend_from_slice(b" Tf\n");
    for c in [tm.a, tm.b, tm.c, tm.d, tm.e, tm.f] {
        push_float(out, c);
        out.push(b' ');
    }
    out.extend_from_slice(b"Tm\n(");
    for ch in text.chars() {
        let code = glyphs::windows_1252_from_unicode(ch as i32);
        let b = if (0x20..=0xff).contains(&code) {
            code as u8
        } else {
            b'?'
        };
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
    out.extend_from_slice(b") Tj\nET\n");
}

fn obj_ref(num: i32) -> Object {
    Object::Ref(ObjRef::new(num, 0))
}

fn add_helvetica_font_object(doc: &Document) -> i32 {
    let mut d = Dict::new();
    d.put(b"Type", Object::name(b"Font"));
    d.put(b"Subtype", Object::name(b"Type1"));
    d.put(b"BaseFont", Object::name(b"Helvetica"));
    d.put(b"Encoding", Object::name(b"WinAnsiEncoding"));
    doc.add_object(Object::Dict(Arc::new(d)))
}

fn add_zapf_font_object(doc: &Document) -> i32 {
    let mut d = Dict::new();
    d.put(b"Type", Object::name(b"Font"));
    d.put(b"Subtype", Object::name(b"Type1"));
    d.put(b"BaseFont", Object::name(b"ZapfDingbats"));
    doc.add_object(Object::Dict(Arc::new(d)))
}

fn append_content_layer_with_font(
    doc: &Document,
    page_index: i32,
    helv_font_obj: Option<i32>,
    layer: Vec<u8>,
) -> Result<(), String> {
    let page = doc.page(page_index).map_err(|e| e.0)?;
    let res = doc.inherited(&page, b"Resources").map_err(|e| e.0)?;
    let mut res_dict = res
        .map(|r| doc.resolve(&r))
        .and_then(|r| r.as_dict().cloned())
        .unwrap_or_default();

    if let Some(font_num) = helv_font_obj {
        let mut fonts_dict = doc
            .resolve(res_dict.get(b"Font").unwrap_or(&Object::Null))
            .as_dict()
            .cloned()
            .unwrap_or_default();
        if fonts_dict.get(FLAT_FILL_FONT_RES).is_none() {
            fonts_dict.put(FLAT_FILL_FONT_RES, obj_ref(font_num));
        }
        res_dict.put(b"Font", Object::Dict(Arc::new(fonts_dict)));
    }
    let res_obj = Object::Dict(Arc::new(res_dict));

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
        obj_ref(doc.add_stream(d, data))
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

    edit::edit_page(doc, page_index, &mut |d| {
        d.put(b"Resources", res_obj.clone());
        d.put(b"Contents", Object::Array(Arc::new(items.clone())));
    })
}

/// Reopens written bytes for read-back, with `password` when they are
/// encrypted. An output that cannot be read fails: nothing in it was checked.
fn reopen_for_readback(bytes: &[u8], password: Option<&str>) -> Result<Document, FlatFormError> {
    let reopened = Document::open(bytes.to_vec()).map_err(|e| FlatFormError {
        code: e.code,
        message: e.message,
    })?;
    if reopened.needs_password()
        && !password.is_some_and(|p| reopened.authenticate(p).is_ok_and(|a| a != 0))
    {
        return Err(FlatFormError {
            code: "verify.unreadable",
            message: "the output needs a password to read back: pass the document's password"
                .into(),
        });
    }
    Ok(reopened)
}

fn verify_filled_bytes(
    bytes: &[u8],
    applied: &[AppliedFlatFill],
    password: Option<&str>,
) -> Result<(), FlatFormError> {
    let reopened = reopen_for_readback(bytes, password)?;

    let mut pages_needed: Vec<i32> = applied.iter().map(|a| a.page).collect();
    pages_needed.sort_unstable();
    pages_needed.dedup();

    let mut store = Store::default();
    for page_num in pages_needed {
        let page_index = page_num - 1;
        let page_obj = reopened.page(page_index).map_err(|e| FlatFormError {
            code: "verify.readback",
            message: e.0,
        })?;
        let (s_res, next_store) =
            stext::extract_page(&reopened, &page_obj, stext::Options::default(), store);
        store = next_store;
        let stext_page = s_res.map_err(|message| FlatFormError {
            code: "verify.readback",
            message,
        })?;
        let page_text = extract_flat_text(&stext_page);
        let redetected =
            detect_page_flat_forms(&reopened, page_index).map_err(|message| FlatFormError {
                code: "verify.readback",
                message,
            })?;

        for item in applied.iter().filter(|a| a.page == page_num) {
            let [x0, y0, x1, y1] = item.bbox;
            let field_r = Rect::new(x0, y0, x1, y1);

            match item.kind.as_str() {
                "underline" => {
                    let wanted = item.value.as_deref().unwrap_or("").trim();
                    if wanted.is_empty() {
                        continue;
                    }
                    let probe = field_r.expand(4.0);
                    let found = page_text
                        .labels
                        .iter()
                        .any(|sp| !sp.bbox.intersect(probe).is_empty() && sp.text.contains(wanted));
                    if !found {
                        return Err(FlatFormError {
                            code: "verify.readback",
                            message: format!(
                                "filled text {:?} for field {:?} did not extract near {:?}",
                                wanted, item.id, item.bbox
                            ),
                        });
                    }
                }
                "comb" => {
                    let wanted_no_ws: String = item
                        .value
                        .as_deref()
                        .unwrap_or("")
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect();
                    if wanted_no_ws.is_empty() {
                        continue;
                    }
                    let probe = field_r.expand(3.0);
                    let mut inside_glyphs: Vec<&GlyphItem> = page_text
                        .glyphs
                        .iter()
                        .filter(|g| point_in_rect(Point::new(g.cx, g.cy), probe))
                        .collect();
                    inside_glyphs.sort_by(|a, b| a.cx.total_cmp(&b.cx));
                    let got: String = inside_glyphs.into_iter().map(|g| g.ch).collect();
                    if !got.contains(&wanted_no_ws) {
                        return Err(FlatFormError {
                            code: "verify.readback",
                            message: format!(
                                "comb field {:?} read-back {:?} did not match expected {:?}",
                                item.id, got, wanted_no_ws
                            ),
                        });
                    }
                }
                "checkbox" => {
                    let want_checked = item.checked.unwrap_or(false);
                    let matched_cb = redetected.iter().find(|f| {
                        f.kind == "checkbox"
                            && (f.bbox[0] - x0).abs() <= 2.0
                            && (f.bbox[1] - y0).abs() <= 2.0
                    });
                    let Some(cb_after) = matched_cb else {
                        return Err(FlatFormError {
                            code: "verify.readback",
                            message: format!(
                                "checkbox {:?} missing on read-back at {:?}",
                                item.id, item.bbox
                            ),
                        });
                    };
                    if cb_after.checked != Some(want_checked) {
                        return Err(FlatFormError {
                            code: "verify.readback",
                            message: format!(
                                "checkbox {:?} read-back checked={:?}, expected Some({want_checked})",
                                item.id, cb_after.checked
                            ),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// 3. Promotion to Real `/AcroForm` Widgets
// ---------------------------------------------------------------------------

/// Promotes all detected flat form fields in `doc` into real interactive PDF
/// `/Widget` annotations registered in `/AcroForm`, serializes with `write_opts`,
/// reads the output back as every writer does ([`write::save_verified`]), and
/// verifies every promoted widget via [`crate::form`].
pub fn promote_flat_forms(
    doc: &Document,
    write_opts: &WriteOptions,
) -> Result<(Vec<u8>, FlatFormReport), FlatFormError> {
    promote_flat_forms_with_password(doc, write_opts, None)
}

/// [`promote_flat_forms`] for a document opened with `password`, which the
/// read-back needs when the output stays encrypted.
pub fn promote_flat_forms_with_password(
    doc: &Document,
    write_opts: &WriteOptions,
    password: Option<&str>,
) -> Result<(Vec<u8>, FlatFormReport), FlatFormError> {
    let staged = stage_flat_promote(doc)?;
    let bytes = write::save_verified(&staged.work, write_opts, password).map_err(save_error)?;
    let report = staged.finish(&bytes, password)?;
    Ok((bytes, report))
}

/// A promotion made on a fork of the input, for the caller to save (and
/// read back) and hand the bytes to [`StagedFlatPromote::finish`], which
/// checks each widget on them.
#[derive(Debug)]
pub struct StagedFlatPromote {
    /// The fork the widgets were added to: what to save, and whose
    /// diagnostics to report.
    pub work: Document,
    report: FlatFormReport,
}

impl StagedFlatPromote {
    /// The promotion's report for `bytes`, a save of
    /// [`StagedFlatPromote::work`], once every widget reads back (with
    /// `password` when locked).
    pub fn finish(
        &self,
        bytes: &[u8],
        password: Option<&str>,
    ) -> Result<FlatFormReport, FlatFormError> {
        if !self.report.fields.is_empty() {
            verify_promoted_acroform(bytes, &self.report.fields, password)?;
        }
        Ok(self.report.clone())
    }
}

/// The promotion of [`promote_flat_forms`], with nothing saved yet.
pub fn stage_flat_promote(doc: &Document) -> Result<StagedFlatPromote, FlatFormError> {
    let fork = doc.fork_clean();
    let mut report = detect_flat_forms(&fork, None).map_err(|message| FlatFormError {
        code: "input.detect_failed",
        message,
    })?;

    if report.fields.is_empty() {
        return Ok(StagedFlatPromote { work: fork, report });
    }

    rename_clear_of_existing_fields(&fork, &mut report.fields);

    let helv_num = add_helvetica_font_object(&fork);
    let zadb_num = add_zapf_font_object(&fork);

    // Group fields by 1-based page number.
    let mut by_page: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
    for (idx, field) in report.fields.iter().enumerate() {
        by_page.entry(field.page).or_default().push(idx);
    }

    let mut new_field_refs: Vec<Object> = Vec::with_capacity(report.fields.len());

    for (page_num, idxs) in by_page {
        let page_index = page_num - 1;
        let page_ref = fork.page(page_index).map_err(|e| FlatFormError {
            code: "input.page_tree",
            message: e.0,
        })?;

        let mut page_widget_refs = Vec::with_capacity(idxs.len());
        for fi in idxs {
            let field = &report.fields[fi];
            let [px0, py0, px1, py1] = field.pdf_bbox;
            let rect_arr = Object::Array(Arc::new(vec![
                Object::Real(px0),
                Object::Real(py0),
                Object::Real(px1),
                Object::Real(py1),
            ]));

            let mut w_dict = Dict::new();
            w_dict.put(b"Type", Object::name(b"Annot"));
            w_dict.put(b"Subtype", Object::name(b"Widget"));
            w_dict.put(b"F", Object::Int(PDF_ANNOT_PRINT_FLAG));
            if matches!(page_ref, Object::Ref(_)) {
                w_dict.put(b"P", page_ref.clone());
            }
            w_dict.put(b"T", edit::text_string(&field.name));
            if let Some(ref lbl) = field.label {
                w_dict.put(b"TU", edit::text_string(lbl));
            }
            w_dict.put(b"Rect", rect_arr);
            w_dict.put(b"DA", edit::text_string("/Helv 10 Tf 0 g"));

            match field.kind.as_str() {
                "checkbox" => {
                    let is_on = field.checked.unwrap_or(false);
                    let state_name = if is_on {
                        Object::name(b"Yes")
                    } else {
                        Object::name(b"Off")
                    };
                    w_dict.put(b"FT", Object::name(b"Btn"));
                    w_dict.put(b"V", state_name.clone());
                    w_dict.put(b"AS", state_name);

                    let ap_dict =
                        build_checkbox_ap_dict(&fork, (px1 - px0).abs(), (py1 - py0).abs());
                    w_dict.put(b"AP", ap_dict);
                }
                "comb" => {
                    let max_len = field.cell_count.unwrap_or(1) as i64;
                    w_dict.put(b"FT", Object::name(b"Tx"));
                    w_dict.put(b"Ff", Object::Int(PDF_TX_COMB_FLAG));
                    w_dict.put(b"MaxLen", Object::Int(max_len));
                    if let Some(ref txt) = field.current_text
                        && !txt.is_empty()
                    {
                        w_dict.put(b"V", edit::text_string(txt));
                    }
                }
                _ => {
                    w_dict.put(b"FT", Object::name(b"Tx"));
                    if let Some(ref txt) = field.current_text
                        && !txt.is_empty()
                    {
                        w_dict.put(b"V", edit::text_string(txt));
                    }
                }
            }

            let w_num = fork.add_object(Object::Dict(Arc::new(w_dict)));
            let w_ref = obj_ref(w_num);
            page_widget_refs.push(w_ref.clone());
            new_field_refs.push(w_ref);
        }

        edit::edit_page(&fork, page_index, &mut |page_dict| {
            let mut annots_list = match page_dict.get(b"Annots").cloned() {
                Some(v @ Object::Ref(_)) => match fork.resolve(&v) {
                    Object::Array(a) => (*a).clone(),
                    _ => Vec::new(),
                },
                Some(Object::Array(a)) => (*a).clone(),
                _ => Vec::new(),
            };
            annots_list.extend(page_widget_refs.iter().cloned());
            page_dict.put(b"Annots", Object::Array(Arc::new(annots_list)));
        })
        .map_err(|message| FlatFormError {
            code: "input.page_edit",
            message,
        })?;
    }

    install_acroform_catalog(&fork, &new_field_refs, helv_num, zadb_num)?;
    report.promoted_widgets = new_field_refs.len();

    Ok(StagedFlatPromote { work: fork, report })
}

fn build_checkbox_ap_dict(doc: &Document, w: f32, h: f32) -> Object {
    let bw = w.max(6.0);
    let bh = h.max(6.0);
    let bbox_arr = Object::Array(Arc::new(vec![
        Object::Real(0.0),
        Object::Real(0.0),
        Object::Real(bw),
        Object::Real(bh),
    ]));

    let make_form_xobj = |content: Vec<u8>| -> Object {
        let mut d = Dict::new();
        d.put(b"Type", Object::name(b"XObject"));
        d.put(b"Subtype", Object::name(b"Form"));
        d.put(b"BBox", bbox_arr.clone());
        d.put(b"Length", Object::Int(content.len() as i64));
        obj_ref(doc.add_stream(d, content))
    };

    let mut yes_stream = Vec::new();
    yes_stream.extend_from_slice(b"q\n0 0 0 RG\n1.4 w\n1 J\n1 j\n");
    push_xy_op(&mut yes_stream, 0.22 * bw, 0.45 * bh, b"m");
    push_xy_op(&mut yes_stream, 0.43 * bw, 0.24 * bh, b"l");
    push_xy_op(&mut yes_stream, 0.80 * bw, 0.76 * bh, b"l");
    yes_stream.extend_from_slice(b"S\nQ\n");

    let yes_ref = make_form_xobj(yes_stream);
    let off_ref = make_form_xobj(b"q\nQ\n".to_vec());

    let mut n_dict = Dict::new();
    n_dict.put(b"Yes", yes_ref);
    n_dict.put(b"Off", off_ref);

    let mut ap_dict = Dict::new();
    ap_dict.put(b"N", Object::Dict(Arc::new(n_dict)));
    Object::Dict(Arc::new(ap_dict))
}

fn install_acroform_catalog(
    doc: &Document,
    new_fields: &[Object],
    helv_num: i32,
    zadb_num: i32,
) -> Result<(), FlatFormError> {
    let mut trailer = doc.trailer();
    let root_obj = trailer.get(b"Root").cloned().unwrap_or(Object::Null);
    let Some(mut catalog) = doc.resolve(&root_obj).as_dict().cloned() else {
        return Err(FlatFormError {
            code: "input.catalog",
            message: "document catalog /Root is not a dictionary".into(),
        });
    };

    let existing_acro = catalog.get(b"AcroForm").cloned().unwrap_or(Object::Null);
    let mut acro_dict = doc
        .resolve(&existing_acro)
        .as_dict()
        .cloned()
        .unwrap_or_default();

    let mut fields_arr = doc
        .resolve(acro_dict.get(b"Fields").unwrap_or(&Object::Null))
        .as_array()
        .map(<[Object]>::to_vec)
        .unwrap_or_default();
    fields_arr.extend_from_slice(new_fields);
    acro_dict.put(b"Fields", Object::Array(Arc::new(fields_arr)));
    acro_dict.put(b"NeedAppearances", Object::Bool(true));

    let mut dr_dict = doc
        .resolve(acro_dict.get(b"DR").unwrap_or(&Object::Null))
        .as_dict()
        .cloned()
        .unwrap_or_default();
    let mut dr_fonts = doc
        .resolve(dr_dict.get(b"Font").unwrap_or(&Object::Null))
        .as_dict()
        .cloned()
        .unwrap_or_default();
    if dr_fonts.get(b"Helv").is_none() {
        dr_fonts.put(b"Helv", obj_ref(helv_num));
    }
    if dr_fonts.get(b"ZaDb").is_none() {
        dr_fonts.put(b"ZaDb", obj_ref(zadb_num));
    }
    dr_dict.put(b"Font", Object::Dict(Arc::new(dr_fonts)));
    acro_dict.put(b"DR", Object::Dict(Arc::new(dr_dict)));

    match existing_acro {
        Object::Ref(r) => {
            doc.alter_object(r.num, Object::Dict(Arc::new(acro_dict)));
        }
        _ => {
            let acro_num = doc.add_object(Object::Dict(Arc::new(acro_dict)));
            catalog.put(b"AcroForm", obj_ref(acro_num));
        }
    }

    let catalog_obj = Object::Dict(Arc::new(catalog));
    match root_obj {
        Object::Ref(r) => {
            doc.alter_object(r.num, catalog_obj);
        }
        _ => {
            trailer.put(b"Root", catalog_obj);
            doc.set_trailer(trailer);
        }
    }

    Ok(())
}

/// Gives each field to promote a name no existing AcroForm field has (nor
/// any top-level part of one), adding `_N` as detection does.
fn rename_clear_of_existing_fields(doc: &Document, fields: &mut [FlatField]) {
    let mut taken: HashSet<String> = HashSet::new();
    let mut add = |name: String| {
        if let Some(top) = name.split('.').next() {
            taken.insert(top.to_string());
        }
        taken.insert(name);
    };
    let page_count = doc.page_count().unwrap_or(0);
    for index in 0..page_count {
        let Ok(page_obj) = doc.page(index) else {
            continue;
        };
        let (_, widgets) = annot::page_annots(doc, &page_obj);
        for w in &widgets {
            if let Ok(name) = form::name(doc, w) {
                add(name);
            }
        }
    }
    let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
    let acro = doc.dict_get(&root, b"AcroForm");
    let top = doc.resolve(&doc.dict_get(&acro, b"Fields"));
    for f in top
        .as_array()
        .unwrap_or(&[])
        .iter()
        .take(MAX_EXISTING_FIELDS)
    {
        if let Ok(name) = form::name(doc, f) {
            add(name);
        }
    }
    for field in fields.iter_mut() {
        if taken.contains(&field.name) {
            let base = field.name.clone();
            let mut n = 2usize;
            while taken.contains(&format!("{base}_{n}")) {
                n += 1;
            }
            field.name = format!("{base}_{n}");
        }
        taken.insert(field.name.clone());
    }
}

fn verify_promoted_acroform(
    bytes: &[u8],
    expected: &[FlatField],
    password: Option<&str>,
) -> Result<(), FlatFormError> {
    let reopened = reopen_for_readback(bytes, password)?;
    if !form::need_appearances(&reopened) {
        return Err(FlatFormError {
            code: "verify.promote_failed",
            message: "/AcroForm /NeedAppearances was not true after promotion".into(),
        });
    }

    let page_count = reopened.page_count().map_err(|e| FlatFormError {
        code: "verify.promote_failed",
        message: e.0,
    })?;

    let mut found_widgets: HashMap<String, (&'static str, i32, i32)> = HashMap::new();
    for p in 0..page_count {
        let page_obj = reopened.page(p).map_err(|e| FlatFormError {
            code: "verify.promote_failed",
            message: e.0,
        })?;
        let (_, widgets) = annot::page_annots(&reopened, &page_obj);
        for w in widgets {
            if let Ok(w_name) = form::name(&reopened, &w)
                && !w_name.is_empty()
            {
                let ftype = form::type_string(&reopened, &w);
                let mlen = form::max_len(&reopened, &w);
                let ff = form::flags(&reopened, &w);
                found_widgets.insert(w_name, (ftype, mlen, ff));
            }
        }
    }

    for exp in expected {
        let Some(&(got_type, got_max_len, got_flags)) = found_widgets.get(&exp.name) else {
            return Err(FlatFormError {
                code: "verify.promote_failed",
                message: format!(
                    "promoted widget {:?} ({}) not found on reopened PDF",
                    exp.name, exp.id
                ),
            });
        };
        let want_type = if exp.kind == "checkbox" {
            "checkbox"
        } else {
            "text"
        };
        if got_type != want_type {
            return Err(FlatFormError {
                code: "verify.promote_failed",
                message: format!(
                    "promoted widget {:?} had type {got_type:?}, expected {want_type:?}",
                    exp.name
                ),
            });
        }
        if exp.kind == "comb" {
            let want_len = exp.cell_count.unwrap_or(1) as i32;
            if got_max_len != want_len || (got_flags & (PDF_TX_COMB_FLAG as i32)) == 0 {
                return Err(FlatFormError {
                    code: "verify.promote_failed",
                    message: format!(
                        "promoted comb widget {:?} had max_len={got_max_len} flags={got_flags}, expected max_len={want_len} with Comb flag",
                        exp.name
                    ),
                });
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fill::{self, Edit};
    use crate::testutil::{doc_bytes, page_doc, stream};

    const HELV_RESOURCES: &str =
        "/Font<</F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>>>";

    /// Builds a realistic flat government/intake PDF with zero `/AcroForm` widgets:
    /// 1. Stroked underline blank `Applicant Name: [line]` + neighbor `Code: [short line]`
    /// 2. Underscore character blank `Email: ________________`
    /// 3. 9-cell stroked SSN comb grid `[ ][ ][ ][ ][ ][ ][ ][ ][ ]`
    /// 4. Two vector checkboxes: `[ ] Domestic` (unchecked) and `[✓] Foreign` (pre-checked!)
    /// 5. Signature rule with caption underneath (`Authorized Signature`)
    fn flat_intake_form_doc() -> Document {
        let mut s = String::new();
        s += "0 0 0 rg 0 0 0 RG 1 w\n";

        // 1. Applicant Name underline (x=155..335, y_pdf=700) + neighbor on right (`Code:` at x=350)
        s += "BT /F1 11 Tf 72 702 Td (Applicant Name:) Tj ET\n";
        s += "155 700 m 335 700 l S\n";
        s += "BT /F1 11 Tf 350 702 Td (Code:) Tj ET\n";
        s += "385 700 m 440 700 l S\n";

        // 2. Underscore text blank (`Email: __________________`)
        s += "BT /F1 11 Tf 72 660 Td (Email: __________________) Tj ET\n";

        // 3. 9-cell SSN comb grid at y_pdf = 610..628, cell width = 16 pt (x = 120..264)
        s += "BT /F1 11 Tf 72 615 Td (SSN:) Tj ET\n";
        for i in 0..9 {
            let x = 120.0 + (i as f32) * 16.0;
            s += &format!("{x:.1} 610 16 18 re S\n");
        }

        // 4. Vector checkboxes at y_pdf = 565..577 (12x12 pt):
        //    Box 1: [72, 565, 84, 577] + "Domestic" (unchecked)
        //    Box 2: [180, 565, 192, 577] + vector checkmark + "Foreign" (checked!)
        s += "72 565 12 12 re S\n";
        s += "BT /F1 11 Tf 90 567 Td (Domestic) Tj ET\n";
        s += "180 565 12 12 re S\n";
        s += "1.5 w 182.5 570.5 m 185.2 567.8 l 189.8 574.2 l S 1 w\n";
        s += "BT /F1 11 Tf 198 567 Td (Foreign) Tj ET\n";

        // 5. Signature line at y_pdf = 500 with caption directly below (y_pdf = 488)
        s += "72 500 m 260 500 l S\n";
        s += "BT /F1 9 Tf 74 488 Td (Authorized Signature) Tj ET\n";

        page_doc(s.as_bytes(), HELV_RESOURCES, &[])
    }

    /// Three pages, each with a `Signature:` underline.
    fn three_signature_pages() -> Document {
        let page = b"0 0 0 RG 1 w BT /F1 11 Tf 72 702 Td (Signature:) Tj ET 140 700 m 330 700 l S";
        let font = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
        let page_obj = |c: u32| {
            format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents {c} 0 R/Resources<</Font<</F1 9 0 R>>>>>>"
            )
            .into_bytes()
        };
        doc_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R 4 0 R 5 0 R]/Count 3>>".to_vec(),
            page_obj(6),
            page_obj(7),
            page_obj(8),
            stream("", page),
            stream("", page),
            stream("", page),
            font.to_vec(),
        ])
    }

    #[test]
    fn fill_and_promote_read_their_output_back_as_every_writer_does() {
        let doc = three_signature_pages();
        let ops = [FlatFillOp::SetText {
            target: "signature".to_string(),
            value: "Grace Hopper".to_string(),
        }];
        let refused = WriteOptions {
            incremental: true,
            garbage: 1,
            ..WriteOptions::default()
        };
        let e = fill_flat_forms(&doc, &ops, &refused).unwrap_err();
        assert_eq!(e.code, "usage.incremental", "{e}");
        let e = promote_flat_forms(&doc, &refused).unwrap_err();
        assert_eq!(e.code, "usage.incremental", "{e}");
        let incremental = WriteOptions {
            incremental: true,
            ..WriteOptions::default()
        };
        let (bytes, rep) = fill_flat_forms(&doc, &ops, &incremental).unwrap();
        assert!(rep.verified);
        assert!(bytes.starts_with(doc.data()), "an incremental fill appends");

        // A locked output read back without its password is not the
        // document saved.
        let locked = Document::open(
            include_bytes!("../../../corpus/smoke/encrypted-user-password.pdf").to_vec(),
        )
        .unwrap();
        assert!(locked.authenticate("user").is_ok_and(|a| a != 0));
        let e = fill_flat_forms(&locked, &[], &WriteOptions::default()).unwrap_err();
        assert_eq!(e.code, "verify.failed", "{e}");
        let e = promote_flat_forms(&locked, &WriteOptions::default()).unwrap_err();
        assert_eq!(e.code, "verify.failed", "{e}");
        promote_flat_forms_with_password(&locked, &WriteOptions::default(), Some("user")).unwrap();
    }

    #[test]
    fn checking_a_box_adds_no_font() {
        let doc = flat_intake_form_doc();
        let (bytes, _) = fill_flat_forms(
            &doc,
            &[FlatFillOp::SetCheck {
                target: "domestic".to_string(),
                checked: true,
            }],
            &WriteOptions::default(),
        )
        .unwrap();
        let out = Document::open(bytes).unwrap();
        let page = out.page(0).unwrap();
        let res = out.inherited(&page, b"Resources").unwrap().unwrap();
        let fonts = out.dict_get(&res, b"Font");
        assert!(out.dict_get(&fonts, FLAT_FILL_FONT_RES) == Object::Null);
    }

    #[test]
    fn field_names_do_not_depend_on_the_pages_listed() {
        let doc = three_signature_pages();
        let all = detect_flat_forms(&doc, None).unwrap();
        let names: Vec<&str> = all.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["signature", "signature_2", "signature_3"]);

        let third = detect_flat_forms(&doc, Some("3")).unwrap();
        assert_eq!(third.fields.len(), 1);
        assert_eq!(third.fields[0].page, 3);
        let name = third.fields[0].name.clone();
        assert_eq!(name, "signature_3");

        // The name read off `--pages 3` fills page 3's field.
        let (_, rep) = fill_flat_forms(
            &doc,
            &[FlatFillOp::SetText {
                target: name,
                value: "Grace Hopper".to_string(),
            }],
            &WriteOptions::default(),
        )
        .unwrap();
        assert_eq!(rep.applied[0].page, 3);
    }

    #[test]
    fn promoted_names_do_not_collide_with_existing_fields() {
        // A real field already named `signature` lives on page 1, away from
        // the flat underlines.
        let page = b"0 0 0 RG 1 w BT /F1 11 Tf 72 702 Td (Signature:) Tj ET 140 700 m 330 700 l S";
        let doc = doc_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R/AcroForm<</Fields[6 0 R]>>>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>/Annots[6 0 R]>>".to_vec(),
            stream("", page),
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>".to_vec(),
            b"<</Type/Annot/Subtype/Widget/FT/Tx/T(signature)/V(kept)/Rect[72 100 200 120]>>".to_vec(),
        ]);
        let (bytes, rep) = promote_flat_forms(&doc, &WriteOptions::default()).unwrap();
        assert_eq!(rep.fields.len(), 1);
        assert_ne!(rep.fields[0].name, "signature");

        let out = Document::open(bytes).unwrap();
        let page0 = out.page(0).unwrap();
        let (_, widgets) = annot::page_annots(&out, &page0);
        let mut names: Vec<String> = widgets
            .iter()
            .map(|w| form::name(&out, w).unwrap())
            .collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate field names: {names:?}");
        assert!(names.contains(&rep.fields[0].name));
    }

    #[test]
    fn detects_underlines_underscore_runs_comb_grids_and_checked_vector_checkboxes() {
        let doc = flat_intake_form_doc();
        let report = detect_flat_forms(&doc, None).unwrap();
        assert_eq!(report.page_count, 1);
        assert_eq!(report.promoted_widgets, 0);

        let by_name = |n: &str| -> &FlatField {
            report
                .fields
                .iter()
                .find(|f| f.name == n)
                .unwrap_or_else(|| panic!("missing field {n:?} in {:?}", report.fields))
        };

        let app = by_name("applicant_name");
        assert_eq!(app.id, "p1.ff1");
        assert_eq!(app.kind, "underline");
        assert!(!app.underscore_text);

        let code = by_name("code");
        assert_eq!(code.kind, "underline");

        let email = by_name("email");
        assert_eq!(email.kind, "underline");
        assert!(email.underscore_text);

        let ssn = by_name("ssn");
        assert_eq!(ssn.kind, "comb");
        assert_eq!(ssn.cell_count, Some(9));
        assert_eq!(ssn.cell_x_ranges.len(), 9);

        let dom = by_name("domestic");
        assert_eq!(dom.kind, "checkbox");
        assert_eq!(dom.checked, Some(false));

        let frn = by_name("foreign");
        assert_eq!(frn.kind, "checkbox");
        assert_eq!(frn.checked, Some(true));

        let sig = by_name("authorized_signature");
        assert_eq!(sig.kind, "underline");
    }

    #[test]
    fn fills_flat_fields_verifies_readback_and_rejects_comb_and_underline_overflows() {
        let doc = flat_intake_form_doc();
        let write_opts = WriteOptions::default();

        // 1. Reject comb overflow (> 9 digits for 9-cell SSN comb).
        let comb_err = fill_flat_forms(
            &doc,
            &[FlatFillOp::SetText {
                target: "ssn".into(),
                value: "1234567890".into(),
            }],
            &write_opts,
        )
        .unwrap_err();
        assert_eq!(comb_err.code, "verify.comb_overflow");

        // 2. Reject underline overflow on narrow `Code:` blank (55 pt wide) colliding / overflowing at 7.5pt.
        let ovf_err = fill_flat_forms(
            &doc,
            &[FlatFillOp::SetText {
                target: "Applicant Name".into(),
                value: "An Extremely Long Applicant Organization Name That Collides With Code Neighbor Span On The Right".into(),
            }],
            &write_opts,
        )
        .unwrap_err();
        assert_eq!(ovf_err.code, "verify.overflow");

        // 3. Valid fill across underline, underscore blank, 9-cell comb, checkbox check, and checkbox uncheck!
        let (filled_bytes, fill_rep) = fill_flat_forms(
            &doc,
            &[
                FlatFillOp::SetText {
                    target: "applicant_name".into(),
                    value: "Ada Lovelace".into(),
                },
                FlatFillOp::SetText {
                    target: "email".into(),
                    value: "ada@analytical.example".into(),
                },
                FlatFillOp::SetText {
                    target: "ssn".into(),
                    value: "123456789".into(),
                },
                FlatFillOp::SetCheck {
                    target: "domestic".into(),
                    checked: true,
                },
                FlatFillOp::SetCheck {
                    target: "foreign".into(),
                    checked: false,
                },
            ],
            &write_opts,
        )
        .unwrap();

        assert!(fill_rep.verified);
        assert_eq!(fill_rep.applied.len(), 5);

        // Reopen independently and check redetection of checkbox states and comb current_text!
        let reopened = Document::open(filled_bytes).unwrap();
        let post_detect = detect_flat_forms(&reopened, None).unwrap();
        let dom = post_detect
            .fields
            .iter()
            .find(|f| f.name == "domestic")
            .unwrap();
        assert_eq!(dom.checked, Some(true));

        let frn = post_detect
            .fields
            .iter()
            .find(|f| f.name == "foreign")
            .unwrap();
        assert_eq!(frn.checked, Some(false));

        let ssn = post_detect.fields.iter().find(|f| f.name == "ssn").unwrap();
        assert_eq!(ssn.current_text.as_deref(), Some("123456789"));
    }

    #[test]
    fn promotes_flat_fields_to_real_acroform_widgets_and_skips_existing_widgets() {
        let doc = flat_intake_form_doc();
        let write_opts = WriteOptions::default();

        let (promoted_bytes, report) = promote_flat_forms(&doc, &write_opts).unwrap();
        assert_eq!(report.promoted_widgets, 7);
        assert_eq!(report.fields.len(), 7);

        let reopened = Document::open(promoted_bytes).unwrap();
        assert!(form::need_appearances(&reopened));

        let page_obj = reopened.page(0).unwrap();
        let (_, widgets) = annot::page_annots(&reopened, &page_obj);
        assert_eq!(widgets.len(), 7);

        // Verify `nupdf::form` sees every promoted widget with exact type, name, and max_len,
        // AND `nupdf::fill` can interactively fill the newly promoted AcroForm!
        let mut by_name: HashMap<String, Object> = HashMap::new();
        for w in widgets {
            let n = form::name(&reopened, &w).unwrap();
            by_name.insert(n, w);
        }

        let ssn_w = by_name.get("ssn").expect("missing ssn widget");
        assert_eq!(form::type_string(&reopened, ssn_w), "text");
        assert_eq!(form::max_len(&reopened, ssn_w), 9);
        assert_ne!(form::flags(&reopened, ssn_w) & (1 << 24), 0);

        let dom_w = by_name.get("domestic").expect("missing domestic widget");
        assert_eq!(form::type_string(&reopened, dom_w), "checkbox");
        assert_eq!(form::value(&reopened, dom_w), "Off");

        let frn_w = by_name.get("foreign").expect("missing foreign widget");
        assert_eq!(form::type_string(&reopened, frn_w), "checkbox");
        assert_eq!(form::value(&reopened, frn_w), "Yes");

        // Re-running `detect_flat_forms` on the promoted PDF must skip all regions now covered by widgets!
        let second_detect = detect_flat_forms(&reopened, None).unwrap();
        assert!(
            second_detect.fields.is_empty(),
            "expected zero unpromoted flat fields, got {:?}",
            second_detect.fields
        );

        // Even better: fill the promoted `/AcroForm` widgets using `nupdf::fill::fill`!
        let filled = fill::fill(
            &reopened,
            &[
                Edit::Set {
                    name: "applicant_name".into(),
                    value: "Grace Hopper".into(),
                },
                Edit::Toggle {
                    name: "domestic".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(filled.changes.len(), 2);
        assert!(filled.changes.iter().all(|c| c.changed));
    }

    #[test]
    fn deduplicates_repeated_field_names_across_pages() {
        let c1 = stream(
            "",
            b"0 0 0 rg 0 0 0 RG 1 w BT /F1 11 Tf 72 702 Td (Name:) Tj ET 115 700 m 260 700 l S \
              BT /F1 11 Tf 72 662 Td (Name:) Tj ET 115 660 m 260 660 l S",
        );
        let objs = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>".to_vec(),
            c1,
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>".to_vec(),
        ];
        let doc = doc_bytes(&objs);
        let rep = detect_flat_forms(&doc, None).unwrap();
        let names: Vec<&str> = rep.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["name", "name_2"]);
    }

    #[test]
    fn detects_tick_comb_prefilled_underline_and_glyph_checked_checkbox() {
        let mut s = String::new();
        s += "0 0 0 rg 0 0 0 RG 1 w\n";
        // 1. Pre-filled underline: `City:` with `"London"` sitting directly on the stroked rule
        s += "BT /F1 11 Tf 72 702 Td (City:) Tj ET\n";
        s += "110 700 m 250 700 l S\n";
        s += "BT /F1 11 Tf 118 702 Td (London) Tj ET\n";

        // 2. Comb drawn as horizontal baseline + 5 vertical tick dividers (4-digit PIN)
        s += "BT /F1 11 Tf 72 644 Td (PIN:) Tj ET\n";
        s += "110 640 m 182 640 l S\n";
        for i in 0..=4 {
            let x = 110.0 + (i as f32) * 18.0;
            s += &format!("{x:.1} 640 m {x:.1} 654 l S\n");
        }

        // 3. Checkbox checked via an `'X'` glyph centered inside `[72, 590, 84, 602]`
        s += "72 590 12 12 re S\n";
        s += "BT /F1 9 Tf 75 593 Td (X) Tj ET\n";
        s += "BT /F1 11 Tf 90 592 Td (Express) Tj ET\n";

        let doc = page_doc(s.as_bytes(), HELV_RESOURCES, &[]);
        let rep = detect_flat_forms(&doc, None).unwrap();
        assert_eq!(rep.fields.len(), 3);

        let city = rep.fields.iter().find(|f| f.name == "city").unwrap();
        assert_eq!(city.kind, "underline");
        assert_eq!(city.current_text.as_deref(), Some("London"));

        let pin = rep.fields.iter().find(|f| f.name == "pin").unwrap();
        assert_eq!(pin.kind, "comb");
        assert_eq!(pin.cell_count, Some(4));

        let exp = rep.fields.iter().find(|f| f.name == "express").unwrap();
        assert_eq!(exp.kind, "checkbox");
        assert_eq!(exp.checked, Some(true));
    }

    /// A deterministic xorshift, for varied pages without a dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> f32 {
            (self.next() % n.max(1)) as f32
        }
    }

    /// A `size`-point square page of `n` flat-form parts: captioned
    /// checkboxes (some ticked, some whited out), comb cells, outer-box and
    /// tick combs, captioned underlines, underscore runs, table posts and
    /// white fills. With `jitter`, parts land anywhere and overlap.
    fn busy_page(n: usize, size: f32, seed: u64, jitter: bool) -> Document {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let cols = ((size - 40.0) / 60.0).max(1.0) as usize;
        let mut s = String::from("0 0 0 rg 0 0 0 RG 1 w\n");
        for i in 0..n {
            let (x, y) = if jitter {
                (
                    20.0 + rng.below(size as u64 - 80),
                    20.0 + rng.below(size as u64 - 60),
                )
            } else {
                (
                    20.0 + 60.0 * (i % cols) as f32,
                    size - 40.0 - 30.0 * (i / cols) as f32,
                )
            };
            let d = if jitter { rng.below(4) * 0.5 } else { 0.0 };
            match i % 9 {
                0 => {
                    s += &format!(
                        "{x} {y} 10 10 re S BT /F1 8 Tf {} {} Td (Opt {i}) Tj ET\n",
                        x + 13.0,
                        y + 1.0
                    );
                }
                1 => {
                    s += &format!(
                        "{x} {y} 10 10 re S 1.5 w {} {} m {} {} l {} {} l S 1 w\n",
                        x + 2.0,
                        y + 5.0,
                        x + 4.5,
                        y + 2.0 + d,
                        x + 8.0,
                        y + 8.0
                    );
                    // Unticked again by painting it over, or ticked with a glyph.
                    if d >= 1.0 {
                        s += &format!("1 g {} {} 7 7 re f 0 g\n", x + 1.5 + d - 1.0, y + 1.5);
                    }
                    if d == 0.5 {
                        s += &format!(
                            "{} {y} 10 10 re S BT /F1 9 Tf {} {} Td (X) Tj ET\n",
                            x + 20.0,
                            x + 22.0,
                            y + 2.0
                        );
                    }
                }
                2 => {
                    for k in 0..3 {
                        s += &format!("{} {y} 12 14 re S\n", x + 12.0 * k as f32 + d);
                    }
                }
                3 => {
                    s += &format!(
                        "BT /F1 8 Tf {x} {} Td (Name {i}:) Tj ET {} {y} m {} {y} l S\n",
                        y + 1.0,
                        x + 30.0 + d,
                        x + 70.0
                    );
                }
                4 => {
                    s += &format!("{x} {y} m {} {y} l S\n", x + 48.0);
                    for k in 0..5 {
                        let tx = x + 12.0 * k as f32;
                        let base = y + if k == 2 { 2.0 * d } else { 0.0 };
                        s += &format!("{tx} {base} m {tx} {} l S\n", y + 8.0 + d);
                    }
                }
                5 => {
                    s += &format!("{x} {y} 48 14 re S\n");
                    for k in 1..4 {
                        let tx = x + 12.0 * k as f32 + d;
                        s += &format!("{tx} {y} m {tx} {} l S\n", y + 14.0);
                    }
                }
                6 => {
                    s += &format!("1 g {x} {y} 12 12 re f 0 g\n");
                }
                7 => {
                    s += &format!("BT /F1 8 Tf {x} {y} Td (Pay: ________ now) Tj ET\n");
                }
                _ => {
                    s += &format!(
                        "{x} {} m {x} {} l S {} {} m {} {} l S {x} {y} m {} {y} l S\n",
                        y - 6.0,
                        y + 12.0,
                        x + 50.0,
                        y - 6.0,
                        x + 50.0,
                        y + 12.0 + d,
                        x + 50.0
                    );
                }
            }
        }
        let font = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
        doc_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            format!("<</Type/Page/Parent 2 0 R/MediaBox[0 0 {size} {size}]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>").into_bytes(),
            stream("", s.as_bytes()),
            font.to_vec(),
        ])
    }

    /// Detection with every index answering with every item, as the scans
    /// it replaced did.
    fn detect_by_scans(doc: &Document) -> Vec<FlatField> {
        BRUTE.with(|b| b.set(true));
        let fields = detect_page_flat_forms(doc, 0);
        BRUTE.with(|b| b.set(false));
        fields.unwrap()
    }

    #[test]
    fn indexed_detection_finds_what_the_scans_it_replaced_found() {
        let mut found = 0;
        for seed in 1..=40u64 {
            // Crowded small pages, where parts overlap and near-misses are
            // common, and a spread one.
            let (n, size, jitter) = match seed % 4 {
                0 => (400, 2400.0, false),
                1 => (120, 300.0, true),
                2 => (300, 612.0, true),
                _ => (60, 200.0, true),
            };
            let doc = busy_page(n, size, seed, jitter);
            let indexed = detect_page_flat_forms(&doc, 0).unwrap();
            assert_eq!(indexed, detect_by_scans(&doc), "seed {seed}");
            found += indexed.len();
        }
        // The pages have fields of every kind to compare.
        assert!(found > 1_000, "{found}");
        for doc in [flat_intake_form_doc(), three_signature_pages()] {
            assert_eq!(
                detect_page_flat_forms(&doc, 0).unwrap(),
                detect_by_scans(&doc)
            );
        }
    }

    #[test]
    fn a_page_of_thousands_of_parts_is_not_scanned_once_per_part() {
        // The scans took 6.7 s for 16,000 parts in a release build; a
        // generous bound for a debug build, not a benchmark.
        let (took, fields) = time_busy_page(16_000);
        assert!(fields > 5_000, "{fields}");
        assert!(took < std::time::Duration::from_secs(5), "{took:?}");
    }

    fn time_busy_page(n: usize) -> (std::time::Duration, usize) {
        let size = 60.0 * (n as f32).sqrt().ceil() + 100.0;
        let doc = busy_page(n, size, 7, false);
        let started = std::time::Instant::now();
        let fields = detect_page_flat_forms(&doc, 0).unwrap();
        (started.elapsed(), fields.len())
    }

    /// `cargo test --release -p nupdf --lib flat_form_busy_page_timings -- --ignored --nocapture`
    #[test]
    #[ignore = "a timing to read, not a check"]
    fn flat_form_busy_page_timings() {
        for n in [1_000, 4_000, 16_000] {
            let (took, fields) = time_busy_page(n);
            eprintln!("{n} parts: {fields} fields in {took:?}");
        }
    }

    /// A one-page document `size` points square with Helvetica as `/F1`.
    fn square_page(size: f32, content: &str) -> Document {
        let font = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
        doc_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            format!("<</Type/Page/Parent 2 0 R/MediaBox[0 0 {size} {size}]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>").into_bytes(),
            stream("", content.as_bytes()),
            font.to_vec(),
        ])
    }

    /// A one-page letter document with Helvetica as `/F1`.
    fn letter_page(content: &str) -> Document {
        page_doc(content.as_bytes(), HELV_RESOURCES, &[])
    }

    /// Thousands of comb-sized cells packed into one row band, every one
    /// starting within a few points of where the others end, none of the
    /// same width: each is a row start that looks at all the others.
    fn packed_row_page() -> Document {
        let mut s = String::from("0 0 0 RG 1 w\n");
        let step = 0.85;
        // Starts in band 100 (device y0 398.5..399.4), the cells they look
        // at in band 101 (402.1..405.5), too far down to join. PDF y is
        // 792 - y0 - h.
        for (lo, x0s, ws, ys) in [(true, 24, 3, 2), (false, 3, 18, 5)] {
            for a in 0..x0s {
                for b in 0..ws {
                    for c in 0..ys {
                        for d in 0..24 {
                            let (x0, w) = if lo {
                                // Starts ending at x 108..111.4.
                                let w = 8.0 + step * a as f32;
                                (108.0 - w + step * b as f32, w)
                            } else {
                                (109.5 + step * a as f32, 13.0 + step * b as f32)
                            };
                            let y0 = if lo { 398.5 } else { 402.1 } + step * c as f32;
                            let h = 10.0 + step * d as f32;
                            s += &format!("{x0} {} {w} {h} re S\n", 792.0 - y0 - h);
                        }
                    }
                }
            }
        }
        letter_page(&s)
    }

    /// Checkboxes in the top left of a 2,000 pt page and long diagonal
    /// strokes right of them, each too big for the grid's cells, or
    /// strokes with an x that is not finite.
    fn oversized_items_page(nonfinite: bool) -> Document {
        let mut s = String::from("0 0 0 RG 1 w\n");
        for i in 0..6_000 {
            let (x, y) = (20.0 + 10.0 * (i % 80) as f32, 1230.0 + 10.0 * (i / 80) as f32);
            s += &format!("{x} {y} 8 8 re S\n");
        }
        for i in 0..25_000 {
            if nonfinite {
                let y = 100 + i % 200;
                s += &format!("q 1e38 0 0 1 0 0 cm -10 {y} m 10 {} l S Q\n", y + 2);
            } else {
                let d = (i % 50) as f32;
                s += &format!("{} 0 m 2000 {} l S\n", 850.0 + d, 1200.0 - d);
            }
        }
        square_page(2000.0, &s)
    }

    /// Ink in the left margin and thousands of large whiteouts over the
    /// middle of the page, each too big for the grid's cells.
    fn margin_whiteouts_page() -> Document {
        let mut s = String::from("0 0 0 rg\n");
        for i in 0..50_000 {
            let x = 4.0 + (i % 10) as f32;
            let y = 20.0 + (i / 10 % 700) as f32;
            s += &format!("BT /F1 6 Tf {x} {y} Td (.) Tj ET\n");
        }
        s += "1 g\n";
        for i in 0..12_000 {
            let d = (i % 40) as f32 * 0.5;
            s += &format!("{} {} {} 600 re f\n", 60.0 + d, 80.0 + d, 480.0 - d);
        }
        letter_page(&s)
    }

    /// `cargo test --release -p nupdf --lib flat_form_adversarial_timings -- --ignored --nocapture`
    #[test]
    #[ignore = "a timing to read, not a check"]
    fn flat_form_adversarial_timings() {
        for (name, doc) in [
            ("packed row", packed_row_page()),
            ("oversized strokes", oversized_items_page(false)),
            ("non-finite strokes", oversized_items_page(true)),
            ("margin whiteouts", margin_whiteouts_page()),
        ] {
            let started = std::time::Instant::now();
            let fields = detect_page_flat_forms(&doc, 0).unwrap();
            eprintln!("{name}: {} fields in {:?}", fields.len(), started.elapsed());
        }
    }
}
