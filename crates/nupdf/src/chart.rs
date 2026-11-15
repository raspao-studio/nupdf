// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Deterministic vector chart and node-edge diagram decompiler.
//!
//! [`decompile_charts`] inspects PDF pages in MuPDF page space (origin top-left,
//! `y` increasing downward) through a bounded [`Device`] collector paired with
//! structured text extraction ([`stext::extract_page`]):
//! - **Bar & Horizontal Bar Charts**: locates monotonic numeric tick columns/rows,
//!   fits least-squares linear calibration (`R² >= 0.995`), recovers exact bar
//!   values from baseline-to-tip geometry, groups single or multi-series clusters
//!   by category label, and resolves series names from nearby legend swatches.
//! - **Line / Trend Plots**: chains non-axis stroked polylines and markers inside
//!   calibrated `(X, Y)` or `(Category, Y)` plot regions and projects vertices
//!   through the fitted linear calibration.
//! - **Vector Flowcharts / Node-Edge Diagrams**: pairs closed stroked or
//!   lightly-filled boxes enclosing text spans as nodes with perimeter-connected
//!   polylines, resolving edge direction from filled triangle or V-chevron
//!   arrowheads and attaching nearby edge caption spans.

use std::collections::HashSet;

use serde::Serialize;

use crate::colorspace::ColorSpace;
use crate::content::Store;
use crate::device::{ColorParams, Device};
use crate::diag;
use crate::document::Document;
use crate::extract;
use crate::function::Function;
use crate::geometry::{Matrix, Point, Rect};
use crate::image::Image;
use crate::object::Object;
use crate::page_run;
use crate::path::{self, Path, StrokeState, Walker};
use crate::rearrange;
use crate::stext::{self, Block, StextChar, StextPage, char_flags, rune_char};
use crate::text::Text;

/// Hard cap on vector path segments and rectangles collected per page so
/// adversarial files stay strictly `O(1)` bounded in time and memory.
const MAX_COLLECTED: usize = 50_000;

/// Hard cap on nested clip/group/mask/tile containers in [`ChartCollector`].
const MAX_CLIP_DEPTH: usize = 512;

/// Minimum coefficient of determination (`R²`) required for linear tick-axis calibration.
const MIN_AXIS_R_SQUARED: f64 = 0.995;

/// Complete chart and diagram decompilation report across a document or page range.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChartReport {
    /// Total pages in the source document.
    pub page_count: i32,
    /// Calibrated bar, horizontal-bar, and line charts in page/discovery order.
    pub charts: Vec<ExtractedChart>,
    /// Directed and undirected vector node-edge diagrams in page/discovery order.
    pub diagrams: Vec<ExtractedDiagram>,
}

/// One decompiled quantitative vector chart (`"bar"`, `"horizontal_bar"`, or `"line"`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ExtractedChart {
    /// Deterministic chart identifier (`"p1.chart1"`, `"p1.chart2"`, ...).
    pub id: String,
    /// 1-based page number.
    pub page: i32,
    /// `"bar"`, `"horizontal_bar"`, or `"line"`.
    pub kind: String,
    /// Optional chart title detected directly above the plot region.
    pub title: Option<String>,
    /// Bounding box `[x0, y0, x1, y1]` in MuPDF page space (points, origin top-left).
    pub bbox: [f32; 4],
    /// Coefficient of determination (`R²`) of the calibrated value axis (`>= 0.995`).
    pub axis_r_squared: f64,
    /// Decompiled data series ordered deterministically.
    pub series: Vec<ChartSeries>,
}

/// One named data series within an [`ExtractedChart`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChartSeries {
    /// Legend label when matched to a legend swatch/line, or `"series_1"`, `"series_2"`, ...
    pub name: String,
    /// Hex RGB fill or stroke color (`"#rrggbb"`).
    pub color: String,
    /// Ordered data points in this series.
    pub points: Vec<ChartPoint>,
}

/// One quantitative data point inside a [`ChartSeries`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChartPoint {
    /// Category or X-tick label associated with this point.
    pub label: String,
    /// Calibrated numeric X coordinate when the X-axis is numeric, else `None`.
    pub x_value: Option<f64>,
    /// Calibrated data value rounded to 4 decimal places.
    pub value: f64,
    /// Bounding box `[x0, y0, x1, y1]` of the bar, marker, or vertex in MuPDF page space.
    pub bbox: [f32; 4],
}

/// One decompiled vector flowchart or node-edge diagram.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ExtractedDiagram {
    /// Deterministic diagram identifier (`"p1.diagram1"`, ...).
    pub id: String,
    /// 1-based page number.
    pub page: i32,
    /// Overall bounding box `[x0, y0, x1, y1]` in MuPDF page space.
    pub bbox: [f32; 4],
    /// Enclosed text-bearing diagram nodes.
    pub nodes: Vec<DiagramNode>,
    /// Directed or undirected edges connecting nodes.
    pub edges: Vec<DiagramEdge>,
}

/// One box node inside an [`ExtractedDiagram`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DiagramNode {
    /// Deterministic node identifier (`"p1.n1"`, `"p1.n2"`, ...).
    pub id: String,
    /// Concatenated text of spans enclosed inside the node rectangle.
    pub label: String,
    /// Node bounding box `[x0, y0, x1, y1]` in MuPDF page space.
    pub bbox: [f32; 4],
}

/// One edge connecting two [`DiagramNode`]s inside an [`ExtractedDiagram`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DiagramEdge {
    /// Source node ID (`"p1.n1"`).
    pub from: String,
    /// Target node ID (`"p1.n2"`).
    pub to: String,
    /// Optional caption text sitting along the connector path.
    pub label: Option<String>,
    /// True when an arrowhead triangle or chevron disambiguated the edge direction.
    pub directed: bool,
}

/// Decompiles vector charts and node-edge diagrams across `pages` (or all pages
/// when `pages` is `None` or empty).
pub fn decompile_charts(doc: &Document, pages: Option<&str>) -> Result<ChartReport, String> {
    let page_count = doc.page_count().map_err(|e| e.0)?;
    if page_count <= 0 {
        return Ok(ChartReport {
            page_count: 0,
            charts: Vec::new(),
            diagrams: Vec::new(),
        });
    }

    let raw_indices: Vec<i32> = match pages {
        Some(spec) if !spec.trim().is_empty() => {
            rearrange::parse_page_ranges(spec, page_count, doc)
        }
        _ => (0..page_count).collect(),
    };

    let mut seen = HashSet::new();
    let mut indices = Vec::with_capacity(raw_indices.len());
    for idx in raw_indices {
        if idx >= 0 && idx < page_count && seen.insert(idx) {
            indices.push(idx);
        }
    }

    let mut store = Store::default();
    let mut charts = Vec::new();
    let mut diagrams = Vec::new();

    for index in indices {
        let page_obj = doc.page(index).map_err(|e| e.0)?;
        if doc.resolve(&page_obj).as_dict().is_none() {
            return Err(format!("page {} object is not a dictionary", index + 1));
        }
        let (res, next_store) = decompile_page_charts_with_store(doc, index, &page_obj, store);
        store = next_store;
        let (page_charts, page_diagrams) = res?;
        charts.extend(page_charts);
        diagrams.extend(page_diagrams);
    }

    Ok(ChartReport {
        page_count,
        charts,
        diagrams,
    })
}

/// Decompiles vector charts and node-edge diagrams on a single 0-based `index` page.
pub fn decompile_page_charts(
    doc: &Document,
    index: i32,
) -> Result<(Vec<ExtractedChart>, Vec<ExtractedDiagram>), String> {
    let page_obj = doc.page(index).map_err(|e| e.0)?;
    if doc.resolve(&page_obj).as_dict().is_none() {
        return Err(format!("page {} object is not a dictionary", index + 1));
    }
    decompile_page_charts_with_store(doc, index, &page_obj, Store::default()).0
}

type PageChartResult = Result<(Vec<ExtractedChart>, Vec<ExtractedDiagram>), String>;

fn decompile_page_charts_with_store(
    doc: &Document,
    index: i32,
    page_obj: &Object,
    mut store: Store,
) -> (PageChartResult, Store) {
    let page_num = index.saturating_add(1);
    let mediabox = match doc.page_transform(page_obj) {
        Ok((rect, ctm)) => rect.transform(ctm),
        Err(e) => return (Err(e.0), store),
    };

    let ((stext_res, collector, next_store), _) = diag::capture(|| {
        let (s_res, s1) = stext::extract_page(doc, page_obj, stext::Options::default(), store);
        let mut vc = ChartCollector::new(Some(doc), mediabox);
        let run = page_run::run_page(doc, page_obj, &mut vc, s1, Some("View"), Matrix::IDENTITY);
        (s_res, vc, run.store)
    });
    store = next_store;

    let stext_page = match stext_res {
        Ok(p) => p,
        Err(e) => return (Err(diag::report::strip(&e).to_string()), store),
    };

    let mut collector = collector;
    collector.synthesize_two_segment_arrowheads();

    let spans = collect_page_spans(&stext_page, page_num);
    let (charts, claimed_chart_rects) = extract_all_charts(page_num, &collector, &spans);
    let diagrams = extract_diagrams(page_num, &collector, &spans, &claimed_chart_rects);

    (Ok((charts, diagrams)), store)
}

// ---------------------------------------------------------------------------
// Geometry & Color Helpers
// ---------------------------------------------------------------------------

fn rect_array(r: Rect) -> [f32; 4] {
    [r.x0, r.y0, r.x1, r.y1]
}

fn round4(v: f64) -> f64 {
    let r = (v * 10_000.0).round() / 10_000.0;
    if r == 0.0 { 0.0 } else { r }
}

fn to_rgb_u8(doc: Option<&Document>, cs: &ColorSpace, color: &[f32]) -> [u8; 3] {
    let rgb = cs.to_rgb(doc, color);
    let conv = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    [conv(rgb[0]), conv(rgb[1]), conv(rgb[2])]
}

fn hex_rgb(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

fn rgb_dist_sq(a: [u8; 3], b: [u8; 3]) -> i32 {
    let dr = i32::from(a[0]) - i32::from(b[0]);
    let dg = i32::from(a[1]) - i32::from(b[1]);
    let db = i32::from(a[2]) - i32::from(b[2]);
    dr * dr + dg * dg + db * db
}

fn is_near_white(rgb: [u8; 3]) -> bool {
    rgb[0] >= 250 && rgb[1] >= 250 && rgb[2] >= 250
}

fn pt_dist(a: Point, b: Point) -> f32 {
    (a.x - b.x).hypot(a.y - b.y)
}

fn near(a: f32, b: f32, tol: f32) -> bool {
    (a - b).abs() <= tol
}

fn point_to_segment_dist(p: Point, a: Point, b: Point) -> f32 {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len_sq = dx * dx + dy * dy;
    if len_sq <= 1e-6 {
        return pt_dist(p, a);
    }
    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / len_sq).clamp(0.0, 1.0);
    let proj = Point::new(a.x + t * dx, a.y + t * dy);
    pt_dist(p, proj)
}

fn point_to_rect_perimeter_dist(p: Point, r: Rect) -> f32 {
    let tl = Point::new(r.x0, r.y0);
    let tr = Point::new(r.x1, r.y0);
    let br = Point::new(r.x1, r.y1);
    let bl = Point::new(r.x0, r.y1);
    let d_top = point_to_segment_dist(p, tl, tr);
    let d_right = point_to_segment_dist(p, tr, br);
    let d_bottom = point_to_segment_dist(p, br, bl);
    let d_left = point_to_segment_dist(p, bl, tl);
    d_top.min(d_right).min(d_bottom).min(d_left)
}

fn point_inside_rect(p: Point, r: Rect, pad: f32) -> bool {
    p.x >= r.x0 - pad && p.x <= r.x1 + pad && p.y >= r.y0 - pad && p.y <= r.y1 + pad
}

// ---------------------------------------------------------------------------
// Device Vector & Graphic Collector
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipKind {
    Clip,
    Mask,
    Group,
    Tile,
}

#[derive(Debug, Clone, Copy)]
struct FilledRect {
    bbox: Rect,
    rgb: [u8; 3],
}

#[derive(Debug, Clone, Copy)]
struct StrokedSeg {
    p0: Point,
    p1: Point,
    width: f32,
    rgb: [u8; 3],
    from_closed_rect: bool,
}

#[derive(Debug, Clone)]
struct OpenPolyline {
    pts: Vec<[f32; 2]>,
    width: f32,
    rgb: [u8; 3],
    bbox: Rect,
    is_arrow_barb: bool,
}

#[derive(Debug, Clone, Copy)]
struct ClosedBox {
    bbox: Rect,
    is_stroked: bool,
    is_filled: bool,
    fill_rgb: Option<[u8; 3]>,
}

#[derive(Debug, Clone, Copy)]
struct ArrowHead {
    tip: Point,
    /// Unit vector pointing in the arrow's forward direction (towards the tip).
    dir: Point,
    bbox: Rect,
}

#[derive(Debug, Clone, Copy)]
struct ScatterMarker {
    center: Point,
    bbox: Rect,
    rgb: [u8; 3],
}

#[derive(Debug)]
struct ChartCollector<'d> {
    doc: Option<&'d Document>,
    mediabox: Rect,
    stack: Vec<(Rect, ClipKind)>,
    overflow: usize,
    collected_ops: usize,
    filled_rects: Vec<FilledRect>,
    stroked_segs: Vec<StrokedSeg>,
    polylines: Vec<OpenPolyline>,
    closed_boxes: Vec<ClosedBox>,
    arrowheads: Vec<ArrowHead>,
    markers: Vec<ScatterMarker>,
}

impl<'d> ChartCollector<'d> {
    fn new(doc: Option<&'d Document>, mediabox: Rect) -> Self {
        Self {
            doc,
            mediabox,
            stack: Vec::new(),
            overflow: 0,
            collected_ops: 0,
            filled_rects: Vec::new(),
            stroked_segs: Vec::new(),
            polylines: Vec::new(),
            closed_boxes: Vec::new(),
            arrowheads: Vec::new(),
            markers: Vec::new(),
        }
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

    fn is_page_backdrop(&self, r: Rect) -> bool {
        let pw = (self.mediabox.x1 - self.mediabox.x0).max(1.0);
        let ph = (self.mediabox.y1 - self.mediabox.y0).max(1.0);
        let rw = (r.x1 - r.x0).max(0.0);
        let rh = (r.y1 - r.y0).max(0.0);
        rw >= 0.85 * pw && rh >= 0.85 * ph
    }

    fn upsert_closed_box(
        &mut self,
        bbox: Rect,
        stroked: bool,
        filled: bool,
        fill_rgb: Option<[u8; 3]>,
    ) {
        if self.closed_boxes.len() >= MAX_COLLECTED {
            return;
        }
        for existing in &mut self.closed_boxes {
            if near(existing.bbox.x0, bbox.x0, 1.5)
                && near(existing.bbox.y0, bbox.y0, 1.5)
                && near(existing.bbox.x1, bbox.x1, 1.5)
                && near(existing.bbox.y1, bbox.y1, 1.5)
            {
                existing.is_stroked |= stroked;
                existing.is_filled |= filled;
                if existing.fill_rgb.is_none() {
                    existing.fill_rgb = fill_rgb;
                }
                return;
            }
        }
        self.closed_boxes.push(ClosedBox {
            bbox,
            is_stroked: stroked,
            is_filled: filled,
            fill_rgb,
        });
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
        if alpha <= 0.02 || self.collected_ops >= MAX_COLLECTED || path.is_empty() {
            return;
        }
        let scissor = self.current_scissor();
        if scissor.is_empty() {
            return;
        }

        let rgb = to_rgb_u8(self.doc, cs, color);
        let budget = MAX_COLLECTED.saturating_sub(self.collected_ops);
        let mut walker = SubpathBuilder::new(ctm, budget);
        path.walk(&mut walker);
        walker.finish_current();
        self.collected_ops = self.collected_ops.saturating_add(walker.ops);

        let lw = stroke.map(|s| {
            let raw = if s.linewidth <= 0.0 { 1.0 } else { s.linewidth };
            (raw * path::max_expansion(ctm)).max(0.25)
        });

        for sub in walker.subpaths {
            let clipped = sub.bounds.intersect(scissor);
            if !clipped.is_valid() || (lw.is_none() && clipped.is_empty()) {
                continue;
            }
            let w = clipped.x1 - clipped.x0;
            let h = clipped.y1 - clipped.y0;

            if let Some(stroke_w) = lw {
                // Stroked subpath: skip invisible pure-white strokes.
                if rgb[0] >= 252 && rgb[1] >= 252 && rgb[2] >= 252 {
                    continue;
                }

                if let Some(rect) = sub.as_axis_rect() {
                    let r = rect.intersect(scissor);
                    if r.is_valid() && !r.is_empty() {
                        let rw = r.x1 - r.x0;
                        let rh = r.y1 - r.y0;
                        if rw >= 28.0 && rh >= 14.0 && !self.is_page_backdrop(r) {
                            self.upsert_closed_box(r, true, false, None);
                        }
                        // Record the 4 border segments for axis detection, flagged as closed-rect.
                        let corners = [
                            Point::new(r.x0, r.y0),
                            Point::new(r.x1, r.y0),
                            Point::new(r.x1, r.y1),
                            Point::new(r.x0, r.y1),
                        ];
                        for i in 0..4 {
                            if self.stroked_segs.len() < MAX_COLLECTED {
                                self.stroked_segs.push(StrokedSeg {
                                    p0: corners[i],
                                    p1: corners[(i + 1) % 4],
                                    width: stroke_w,
                                    rgb,
                                    from_closed_rect: true,
                                });
                            }
                        }
                    }
                    continue;
                }

                if let Some(tri) = sub.as_triangle_arrow()
                    && self.arrowheads.len() < MAX_COLLECTED
                {
                    self.arrowheads.push(tri);
                    continue;
                }

                if sub.closed {
                    if sub.has_curves && w >= 28.0 && h >= 14.0 && !self.is_page_backdrop(clipped) {
                        self.upsert_closed_box(clipped, true, false, None);
                    } else if (2.0..=10.0).contains(&w)
                        && (2.0..=10.0).contains(&h)
                        && (w - h).abs() <= 2.5
                        && self.markers.len() < MAX_COLLECTED
                    {
                        self.markers.push(ScatterMarker {
                            center: Point::new(
                                0.5 * (clipped.x0 + clipped.x1),
                                0.5 * (clipped.y0 + clipped.y1),
                            ),
                            bbox: clipped,
                            rgb,
                        });
                    }
                    continue;
                }

                // Open stroked subpath: check V-chevron arrowhead first.
                let mut is_chevron = false;
                if let Some(chev) = sub.as_chevron_arrow()
                    && self.arrowheads.len() < MAX_COLLECTED
                {
                    self.arrowheads.push(chev);
                    is_chevron = true;
                }

                for win in sub.pts.windows(2) {
                    if self.stroked_segs.len() >= MAX_COLLECTED {
                        break;
                    }
                    if pt_dist(win[0], win[1]) > 0.1 {
                        self.stroked_segs.push(StrokedSeg {
                            p0: win[0],
                            p1: win[1],
                            width: stroke_w,
                            rgb,
                            from_closed_rect: false,
                        });
                    }
                }

                if sub.pts.len() >= 2 && self.polylines.len() < MAX_COLLECTED {
                    let pts_arr: Vec<[f32; 2]> = sub.pts.iter().map(|p| [p.x, p.y]).collect();
                    self.polylines.push(OpenPolyline {
                        pts: pts_arr,
                        width: stroke_w,
                        rgb,
                        bbox: clipped,
                        is_arrow_barb: is_chevron,
                    });
                }
            } else {
                // Filled subpath.
                if let Some(tri) = sub.as_triangle_arrow() {
                    if !is_near_white(rgb) && self.arrowheads.len() < MAX_COLLECTED {
                        self.arrowheads.push(tri);
                    }
                    continue;
                }

                if let Some(rect) = sub.as_axis_rect() {
                    let r = rect.intersect(scissor);
                    if r.is_valid() && !r.is_empty() && !self.is_page_backdrop(r) {
                        let rw = r.x1 - r.x0;
                        let rh = r.y1 - r.y0;
                        if !is_near_white(rgb) && self.filled_rects.len() < MAX_COLLECTED {
                            self.filled_rects.push(FilledRect { bbox: r, rgb });
                        }
                        if rw >= 28.0 && rh >= 14.0 {
                            self.upsert_closed_box(r, false, true, Some(rgb));
                        }
                        if !is_near_white(rgb)
                            && (2.0..=10.0).contains(&rw)
                            && (2.0..=10.0).contains(&rh)
                            && (rw - rh).abs() <= 2.5
                            && self.markers.len() < MAX_COLLECTED
                        {
                            self.markers.push(ScatterMarker {
                                center: Point::new(0.5 * (r.x0 + r.x1), 0.5 * (r.y0 + r.y1)),
                                bbox: r,
                                rgb,
                            });
                        }
                    }
                    continue;
                }

                if sub.closed && sub.has_curves {
                    if w >= 28.0 && h >= 14.0 && !self.is_page_backdrop(clipped) {
                        self.upsert_closed_box(clipped, false, true, Some(rgb));
                    } else if !is_near_white(rgb)
                        && (2.0..=10.0).contains(&w)
                        && (2.0..=10.0).contains(&h)
                        && (w - h).abs() <= 2.5
                        && self.markers.len() < MAX_COLLECTED
                    {
                        self.markers.push(ScatterMarker {
                            center: Point::new(
                                0.5 * (clipped.x0 + clipped.x1),
                                0.5 * (clipped.y0 + clipped.y1),
                            ),
                            bbox: clipped,
                            rgb,
                        });
                    }
                }
            }
        }
    }

    /// Detects arrowheads drawn as two separate short 2-point stroked segments
    /// meeting at a shared tip (`m tip l arm1 S`, `m tip l arm2 S`).
    fn synthesize_two_segment_arrowheads(&mut self) {
        let mut short_indices = Vec::new();
        for (idx, poly) in self.polylines.iter().enumerate() {
            if poly.is_arrow_barb || poly.pts.len() != 2 {
                continue;
            }
            let a = Point::new(poly.pts[0][0], poly.pts[0][1]);
            let b = Point::new(poly.pts[1][0], poly.pts[1][1]);
            let len = pt_dist(a, b);
            if (2.0..=16.0).contains(&len) {
                short_indices.push((idx, a, b, len));
            }
            if short_indices.len() >= 512 {
                break;
            }
        }

        let mut used = vec![false; short_indices.len()];
        for i in 0..short_indices.len() {
            if used[i] {
                continue;
            }
            let (idx_i, ai, bi, len_i) = short_indices[i];
            for j in (i + 1)..short_indices.len() {
                if used[j] {
                    continue;
                }
                let (idx_j, aj, bj, len_j) = short_indices[j];
                if (len_i - len_j).abs() > 5.0 {
                    continue;
                }
                // Check if a pair of endpoints meet at a shared tip.
                let candidates = [
                    (ai, bi, aj, bj),
                    (ai, bi, bj, aj),
                    (bi, ai, aj, bj),
                    (bi, ai, bj, aj),
                ];
                let mut matched_arrow: Option<ArrowHead> = None;
                for (tip1, arm1, tip2, arm2) in candidates {
                    if pt_dist(tip1, tip2) <= 1.5 {
                        let tip = Point::new(0.5 * (tip1.x + tip2.x), 0.5 * (tip1.y + tip2.y));
                        if let Some(arr) = make_chevron(arm1, tip, arm2) {
                            matched_arrow = Some(arr);
                            break;
                        }
                    }
                }
                if let Some(arr) = matched_arrow {
                    // Confirm a longer shaft segment actually terminates near `arr.tip`.
                    let has_shaft = self.polylines.iter().enumerate().any(|(k, p)| {
                        if k == idx_i || k == idx_j || p.is_arrow_barb || p.pts.len() < 2 {
                            return false;
                        }
                        let first = Point::new(p.pts[0][0], p.pts[0][1]);
                        let last_pt = p.pts[p.pts.len() - 1];
                        let last = Point::new(last_pt[0], last_pt[1]);
                        let span = (p.bbox.x1 - p.bbox.x0).hypot(p.bbox.y1 - p.bbox.y0);
                        span >= 14.0
                            && (pt_dist(first, arr.tip) <= 3.0 || pt_dist(last, arr.tip) <= 3.0)
                    });
                    if has_shaft && self.arrowheads.len() < MAX_COLLECTED {
                        self.arrowheads.push(arr);
                        self.polylines[idx_i].is_arrow_barb = true;
                        self.polylines[idx_j].is_arrow_barb = true;
                        used[i] = true;
                        used[j] = true;
                        break;
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
impl Device for ChartCollector<'_> {
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

// ---------------------------------------------------------------------------
// Subpath Decomposition
// ---------------------------------------------------------------------------

struct RawSubpath {
    pts: Vec<Point>,
    closed: bool,
    has_curves: bool,
    bounds: Rect,
}

fn make_chevron(arm0: Point, tip: Point, arm1: Point) -> Option<ArrowHead> {
    let l0 = pt_dist(arm0, tip);
    let l1 = pt_dist(arm1, tip);
    if !(2.0..=18.0).contains(&l0) || !(2.0..=18.0).contains(&l1) || (l0 - l1).abs() > 6.0 {
        return None;
    }
    let v0 = Point::new((arm0.x - tip.x) / l0, (arm0.y - tip.y) / l0);
    let v1 = Point::new((arm1.x - tip.x) / l1, (arm1.y - tip.y) / l1);
    let cos_theta = v0.x * v1.x + v0.y * v1.y;
    if !(-0.25..=0.96).contains(&cos_theta) {
        return None;
    }
    let bx = -(v0.x + v1.x);
    let by = -(v0.y + v1.y);
    let blen = bx.hypot(by);
    if blen <= 1e-4 {
        return None;
    }
    let bbox = Rect::new(tip.x, tip.y, tip.x, tip.y)
        .include_point(arm0)
        .include_point(arm1);
    Some(ArrowHead {
        tip,
        dir: Point::new(bx / blen, by / blen),
        bbox,
    })
}

impl RawSubpath {
    fn unique_corners(&self) -> Vec<Point> {
        let mut out: Vec<Point> = Vec::new();
        for &p in &self.pts {
            if out.last().is_none_or(|prev| pt_dist(*prev, p) > 0.5) {
                out.push(p);
            }
        }
        if out.len() >= 2 && pt_dist(out[0], out[out.len() - 1]) <= 0.8 {
            out.pop();
        }
        out
    }

    fn is_effectively_closed(&self) -> bool {
        self.closed
            || (self.pts.len() >= 4 && pt_dist(self.pts[0], self.pts[self.pts.len() - 1]) <= 1.0)
    }

    fn as_axis_rect(&self) -> Option<Rect> {
        if self.has_curves || !self.is_effectively_closed() {
            return None;
        }
        let c = self.unique_corners();
        if c.len() != 4 {
            return None;
        }
        let h_first = near(c[0].y, c[1].y, 0.8)
            && near(c[1].x, c[2].x, 0.8)
            && near(c[2].y, c[3].y, 0.8)
            && near(c[3].x, c[0].x, 0.8);
        let v_first = near(c[0].x, c[1].x, 0.8)
            && near(c[1].y, c[2].y, 0.8)
            && near(c[2].x, c[3].x, 0.8)
            && near(c[3].y, c[0].y, 0.8);
        if h_first || v_first {
            Some(self.bounds)
        } else {
            None
        }
    }

    fn as_triangle_arrow(&self) -> Option<ArrowHead> {
        if self.has_curves || !self.is_effectively_closed() {
            return None;
        }
        let c = self.unique_corners();
        if c.len() != 3 {
            return None;
        }
        let d01 = pt_dist(c[0], c[1]);
        let d12 = pt_dist(c[1], c[2]);
        let d20 = pt_dist(c[2], c[0]);
        let max_s = d01.max(d12).max(d20);
        let min_s = d01.min(d12).min(d20);
        if max_s > 20.0 || min_s < 1.5 {
            return None;
        }
        let area2 =
            ((c[1].x - c[0].x) * (c[2].y - c[0].y) - (c[1].y - c[0].y) * (c[2].x - c[0].x)).abs();
        if area2 < 2.0 {
            return None;
        }
        // Identify tip: vertex where the two most-equal sides meet (usually opposite shortest base).
        let diff0 = (d01 - d20).abs();
        let diff1 = (d01 - d12).abs();
        let diff2 = (d12 - d20).abs();
        let (tip, b0, b1) = if diff0 <= diff1 && diff0 <= diff2 {
            (c[0], c[1], c[2])
        } else if diff1 <= diff2 {
            (c[1], c[0], c[2])
        } else {
            (c[2], c[0], c[1])
        };
        let mid_base = Point::new(0.5 * (b0.x + b1.x), 0.5 * (b0.y + b1.y));
        let dx = tip.x - mid_base.x;
        let dy = tip.y - mid_base.y;
        let len = dx.hypot(dy);
        if len <= 1e-4 {
            return None;
        }
        Some(ArrowHead {
            tip,
            dir: Point::new(dx / len, dy / len),
            bbox: self.bounds,
        })
    }

    fn as_chevron_arrow(&self) -> Option<ArrowHead> {
        if self.has_curves || self.closed || self.pts.len() != 3 {
            return None;
        }
        make_chevron(self.pts[0], self.pts[1], self.pts[2])
    }
}

struct SubpathBuilder {
    ctm: Matrix,
    budget: usize,
    ops: usize,
    curr_pts: Vec<Point>,
    curr_has_curves: bool,
    curr_bounds: Rect,
    subpaths: Vec<RawSubpath>,
}

impl SubpathBuilder {
    fn new(ctm: Matrix, budget: usize) -> Self {
        Self {
            ctm,
            budget,
            ops: 0,
            curr_pts: Vec::new(),
            curr_has_curves: false,
            curr_bounds: Rect::EMPTY,
            subpaths: Vec::new(),
        }
    }

    fn finish_with(&mut self, closed: bool) {
        if self.curr_pts.len() >= 2 && self.curr_bounds.is_valid() {
            let pts = std::mem::take(&mut self.curr_pts);
            self.subpaths.push(RawSubpath {
                pts,
                closed,
                has_curves: self.curr_has_curves,
                bounds: self.curr_bounds,
            });
        } else {
            self.curr_pts.clear();
        }
        self.curr_has_curves = false;
        self.curr_bounds = Rect::EMPTY;
    }

    fn finish_current(&mut self) {
        self.finish_with(false);
    }
}

impl Walker for SubpathBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        if self.ops >= self.budget {
            return;
        }
        self.finish_current();
        let p = self.ctm.transform_xy(x, y);
        self.curr_pts.push(p);
        self.curr_bounds = Rect::new(p.x, p.y, p.x, p.y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        if self.curr_pts.is_empty() || self.ops >= self.budget {
            return;
        }
        self.ops += 1;
        let p = self.ctm.transform_xy(x, y);
        self.curr_pts.push(p);
        self.curr_bounds = self.curr_bounds.include_point(p);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) {
        let Some(&p0) = self.curr_pts.last() else {
            return;
        };
        if self.ops >= self.budget {
            return;
        }
        self.ops += 1;
        self.curr_has_curves = true;
        let p1 = self.ctm.transform_xy(x1, y1);
        let p2 = self.ctm.transform_xy(x2, y2);
        let p3 = self.ctm.transform_xy(x3, y3);
        for step in 1..=4 {
            let t = step as f32 * 0.25;
            let u = 1.0 - t;
            let bx = u * u * u * p0.x
                + 3.0 * u * u * t * p1.x
                + 3.0 * u * t * t * p2.x
                + t * t * t * p3.x;
            let by = u * u * u * p0.y
                + 3.0 * u * u * t * p1.y
                + 3.0 * u * t * t * p2.y
                + t * t * t * p3.y;
            let bp = Point::new(bx, by);
            self.curr_pts.push(bp);
            self.curr_bounds = self.curr_bounds.include_point(bp);
        }
    }

    fn close_path(&mut self) {
        if self.curr_pts.is_empty() {
            return;
        }
        if self.ops < self.budget {
            self.ops += 1;
        }
        self.finish_with(true);
    }
}

// ---------------------------------------------------------------------------
// Page Span Extraction (with wide-gap token splitting for axis labels)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TextItem {
    #[allow(dead_code)]
    id: String,
    text: String,
    bbox: Rect,
    cx: f32,
    cy: f32,
    size: f32,
    numeric: Option<f64>,
}

fn collect_page_spans(stext_page: &StextPage, page_num: i32) -> Vec<TextItem> {
    let mut out = Vec::new();
    let prefix = format!("p{page_num}");
    collect_block_items(&stext_page.blocks, &prefix, &mut out);
    out
}

fn collect_block_items(blocks: &[Block], prefix: &str, out: &mut Vec<TextItem>) {
    for (bi, block) in blocks.iter().enumerate() {
        let bid = format!("{prefix}.b{}", bi + 1);
        match block {
            Block::Text(tb) => {
                for (li, line) in tb.lines.iter().enumerate() {
                    let lid = format!("{bid}.l{}", li + 1);
                    for (si, sp) in extract::spans(line).into_iter().enumerate() {
                        let Some(slice) = line.chars.get(sp.start..sp.end) else {
                            continue;
                        };
                        let sid = format!("{lid}.s{}", si + 1);
                        let segments = split_wide_gap_runs(slice, sp.style.size);
                        let single = segments.len() == 1;
                        for (k, (rs, re)) in segments.into_iter().enumerate() {
                            let Some(sub) = slice.get(rs..re) else {
                                continue;
                            };
                            let text = extract::chars_text(sub).trim().to_string();
                            if text.is_empty() {
                                continue;
                            }
                            let bbox = chars_ink_bbox(sub, sp.bbox);
                            if !bbox.is_valid() || bbox.is_empty() {
                                continue;
                            }
                            let id = if single {
                                sid.clone()
                            } else {
                                format!("{sid}.c{}", k + 1)
                            };
                            let cx = 0.5 * (bbox.x0 + bbox.x1);
                            let cy = 0.5 * (bbox.y0 + bbox.y1);
                            let numeric = parse_tick_number(&text);
                            out.push(TextItem {
                                id,
                                text,
                                bbox,
                                cx,
                                cy,
                                size: sp.style.size,
                                numeric,
                            });
                        }
                    }
                }
            }
            Block::Struct(sb) => collect_block_items(&sb.blocks, &bid, out),
            Block::Image(_) | Block::Vector(_) | Block::Grid(_) => {}
        }
    }
}

fn chars_ink_bbox(chars: &[StextChar], fallback: Rect) -> Rect {
    let mut r = Rect::EMPTY;
    let mut any = false;
    for ch in chars {
        if ch.flags & char_flags::SYNTHETIC == 0 && !rune_char(ch.c).is_whitespace() {
            r = r.union(Rect::from_quad(ch.quad));
            any = true;
        }
    }
    if any { r } else { fallback }
}

fn split_wide_gap_runs(chars: &[StextChar], font_size: f32) -> Vec<(usize, usize)> {
    let n = chars.len();
    let gap_thresh = (font_size * 1.35).max(9.0);
    let is_ws = |c: &StextChar| rune_char(c.c).is_whitespace();

    let mut out = Vec::new();
    let mut i = 0usize;
    while i < n && is_ws(&chars[i]) {
        i += 1;
    }
    let mut start = i;
    let mut last_ink: Option<Rect> = None;

    while i < n {
        if is_ws(&chars[i]) {
            let ws_start = i;
            while i < n && is_ws(&chars[i]) {
                i += 1;
            }
            if i < n
                && let Some(prev_r) = last_ink
            {
                let next_r = Rect::from_quad(chars[i].quad);
                let gap = next_r.x0 - prev_r.x1;
                if gap > gap_thresh || (i - ws_start) >= 2 {
                    if ws_start > start {
                        out.push((start, ws_start));
                    }
                    start = i;
                    last_ink = None;
                    continue;
                }
            }
            continue;
        }

        let cur_r = Rect::from_quad(chars[i].quad);
        if let Some(prev_r) = last_ink
            && cur_r.x0 - prev_r.x1 > gap_thresh
        {
            if i > start {
                out.push((start, i));
            }
            start = i;
        }
        last_ink = Some(cur_r);
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

/// Parses numeric tick labels including `$`, `€`, `£`, `¥`, `%`, `,`, `-`/`−`,
/// accounting `(100)`, and optional `k`/`K`/`M`/`B` magnitude suffixes.
fn parse_tick_number(raw: &str) -> Option<f64> {
    if raw.len() > 64 {
        return None;
    }
    let mut s = raw.trim();
    if s.is_empty() {
        return None;
    }

    let mut neg = false;
    if let Some(inner) = s.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        neg = true;
        s = inner.trim();
    }

    fn strip_curr(t: &str) -> &str {
        t.trim_matches(|c: char| matches!(c, '$' | '€' | '£' | '¥'))
            .trim()
    }

    s = strip_curr(s);
    if let Some(rest) = s.strip_prefix('-').or_else(|| s.strip_prefix('\u{2212}')) {
        if neg {
            return None;
        }
        neg = true;
        s = rest.trim();
    } else if let Some(rest) = s.strip_prefix('+') {
        s = rest.trim();
    }
    s = strip_curr(s);

    if let Some(rest) = s.strip_suffix('%') {
        s = rest.trim();
    }

    let mut mult = 1.0_f64;
    if let Some(rest) = s.strip_suffix('k').or_else(|| s.strip_suffix('K')) {
        mult = 1_000.0;
        s = rest.trim();
    } else if let Some(rest) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        mult = 1_000_000.0;
        s = rest.trim();
    } else if let Some(rest) = s.strip_suffix('B') {
        mult = 1_000_000_000.0;
        s = rest.trim();
    }

    if s.is_empty() {
        return None;
    }

    let mut clean = String::with_capacity(s.len());
    let mut saw_dot = false;
    let mut saw_digit = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            clean.push(ch);
            saw_digit = true;
        } else if ch == ',' {
            continue;
        } else if ch == '.' && !saw_dot {
            saw_dot = true;
            clean.push('.');
        } else {
            return None;
        }
    }
    if !saw_digit {
        return None;
    }

    let v = clean.parse::<f64>().ok()?;
    if !v.is_finite() {
        return None;
    }
    let signed = if neg { -v * mult } else { v * mult };
    Some(if signed == 0.0 { 0.0 } else { signed })
}

// ---------------------------------------------------------------------------
// Linear Axis Calibration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CalibratedAxis {
    /// Span indices in `spans` forming this monotonic tick axis.
    tick_indices: Vec<usize>,
    slope: f64,
    intercept: f64,
    r_squared: f64,
    coord_at_zero: f64,
    ticks_bbox: Rect,
}

fn fit_linear(coords: &[f64], values: &[f64]) -> Option<(f64, f64, f64)> {
    let n = coords.len();
    if n < 3 || n != values.len() {
        return None;
    }
    let nf = n as f64;
    let mean_c = coords.iter().sum::<f64>() / nf;
    let mean_v = values.iter().sum::<f64>() / nf;

    let mut s_cc = 0.0_f64;
    let mut s_cv = 0.0_f64;
    let mut s_vv = 0.0_f64;
    for (&c, &v) in coords.iter().zip(values.iter()) {
        let dc = c - mean_c;
        let dv = v - mean_v;
        s_cc += dc * dc;
        s_cv += dc * dv;
        s_vv += dv * dv;
    }
    if s_cc <= 1.0 || s_vv <= 1e-12 {
        return None;
    }
    let slope = s_cv / s_cc;
    if slope.abs() <= 1e-9 || !slope.is_finite() {
        return None;
    }
    let intercept = mean_v - slope * mean_c;
    let r_sq = ((s_cv * s_cv) / (s_cc * s_vv)).clamp(0.0, 1.0);
    Some((slope, intercept, r_sq))
}

/// Finds calibrated vertical value axes (`|cx - axis_x| <= 22 pt` or right-aligned
/// `|x1 - axis_x1| <= 12 pt`, `>= 3` ticks monotonic in `y`, `R² >= 0.995`).
fn find_vertical_axes(spans: &[TextItem]) -> Vec<CalibratedAxis> {
    let mut num_indices: Vec<usize> = spans
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.numeric.map(|_| i))
        .collect();
    num_indices.sort_by(|&a, &b| spans[a].cx.total_cmp(&spans[b].cx));

    let mut used = vec![false; spans.len()];
    let mut axes = Vec::new();

    for &seed in &num_indices {
        if used[seed] {
            continue;
        }
        let seed_cx = spans[seed].cx;
        let seed_x1 = spans[seed].bbox.x1;

        let mut col: Vec<usize> = num_indices
            .iter()
            .copied()
            .filter(|&i| {
                !used[i]
                    && ((spans[i].cx - seed_cx).abs() <= 22.0
                        || (spans[i].bbox.x1 - seed_x1).abs() <= 12.0)
            })
            .collect();
        if col.len() < 3 {
            continue;
        }
        col.sort_by(|&a, &b| spans[a].cy.total_cmp(&spans[b].cy));

        // Deduplicate ticks sitting at nearly the same `cy` (keep closest to `seed_x1`).
        let mut deduped: Vec<usize> = Vec::with_capacity(col.len());
        for idx in col {
            if let Some(&last) = deduped.last()
                && (spans[idx].cy - spans[last].cy).abs() <= 4.0
            {
                if (spans[idx].bbox.x1 - seed_x1).abs() < (spans[last].bbox.x1 - seed_x1).abs() {
                    let last_pos = deduped.len() - 1;
                    deduped[last_pos] = idx;
                }
                continue;
            }
            deduped.push(idx);
        }

        // Split into contiguous monotonic sub-runs without huge vertical gaps (> 110 pt).
        for run in split_monotonic_runs(&deduped, spans, true) {
            if run.len() < 3 {
                continue;
            }
            let coords: Vec<f64> = run.iter().map(|&i| f64::from(spans[i].cy)).collect();
            let vals: Vec<f64> = run
                .iter()
                .map(|&i| spans[i].numeric.unwrap_or(0.0))
                .collect();
            if let Some((slope, intercept, r_sq)) = fit_linear(&coords, &vals)
                && r_sq >= MIN_AXIS_R_SQUARED
            {
                let mut tb = Rect::EMPTY;
                for &i in &run {
                    used[i] = true;
                    tb = tb.union(spans[i].bbox);
                }
                axes.push(CalibratedAxis {
                    tick_indices: run,
                    slope,
                    intercept,
                    r_squared: r_sq,
                    coord_at_zero: -intercept / slope,
                    ticks_bbox: tb,
                });
            }
        }
    }

    axes.sort_by(|a, b| {
        a.ticks_bbox
            .y0
            .total_cmp(&b.ticks_bbox.y0)
            .then_with(|| a.ticks_bbox.x0.total_cmp(&b.ticks_bbox.x0))
    });
    axes
}

/// Finds calibrated horizontal value axes (`|cy - axis_y| <= 14 pt`, `>= 3` ticks
/// monotonic in `x`, `R² >= 0.995`).
fn find_horizontal_axes(
    spans: &[TextItem],
    excluded_spans: &HashSet<usize>,
) -> Vec<CalibratedAxis> {
    let mut num_indices: Vec<usize> = spans
        .iter()
        .enumerate()
        .filter_map(|(i, s)| (!excluded_spans.contains(&i) && s.numeric.is_some()).then_some(i))
        .collect();
    num_indices.sort_by(|&a, &b| spans[a].cy.total_cmp(&spans[b].cy));

    let mut used = vec![false; spans.len()];
    let mut axes = Vec::new();

    for &seed in &num_indices {
        if used[seed] {
            continue;
        }
        let seed_cy = spans[seed].cy;
        let mut row: Vec<usize> = num_indices
            .iter()
            .copied()
            .filter(|&i| !used[i] && (spans[i].cy - seed_cy).abs() <= 14.0)
            .collect();
        if row.len() < 3 {
            continue;
        }
        row.sort_by(|&a, &b| spans[a].cx.total_cmp(&spans[b].cx));

        let mut deduped: Vec<usize> = Vec::with_capacity(row.len());
        for idx in row {
            if let Some(&last) = deduped.last()
                && (spans[idx].cx - spans[last].cx).abs() <= 6.0
            {
                continue;
            }
            deduped.push(idx);
        }

        for run in split_monotonic_runs(&deduped, spans, false) {
            if run.len() < 3 {
                continue;
            }
            let coords: Vec<f64> = run.iter().map(|&i| f64::from(spans[i].cx)).collect();
            let vals: Vec<f64> = run
                .iter()
                .map(|&i| spans[i].numeric.unwrap_or(0.0))
                .collect();
            if let Some((slope, intercept, r_sq)) = fit_linear(&coords, &vals)
                && r_sq >= MIN_AXIS_R_SQUARED
            {
                let mut tb = Rect::EMPTY;
                for &i in &run {
                    used[i] = true;
                    tb = tb.union(spans[i].bbox);
                }
                axes.push(CalibratedAxis {
                    tick_indices: run,
                    slope,
                    intercept,
                    r_squared: r_sq,
                    coord_at_zero: -intercept / slope,
                    ticks_bbox: tb,
                });
            }
        }
    }

    axes.sort_by(|a, b| a.ticks_bbox.y0.total_cmp(&b.ticks_bbox.y0));
    axes
}

fn split_monotonic_runs(sorted: &[usize], spans: &[TextItem], vertical: bool) -> Vec<Vec<usize>> {
    let mut runs: Vec<Vec<usize>> = Vec::new();
    let max_gap = if vertical { 115.0_f32 } else { 180.0_f32 };

    let mut cur: Vec<usize> = Vec::new();
    let mut dir: i8 = 0; // 1 = increasing, -1 = decreasing

    for &idx in sorted {
        let Some(&prev) = cur.last() else {
            cur.push(idx);
            continue;
        };
        let coord_gap = if vertical {
            spans[idx].cy - spans[prev].cy
        } else {
            spans[idx].cx - spans[prev].cx
        };
        let dv = spans[idx].numeric.unwrap_or(0.0) - spans[prev].numeric.unwrap_or(0.0);
        let step_dir = if dv > 1e-9 {
            1
        } else if dv < -1e-9 {
            -1
        } else {
            0
        };

        if coord_gap > max_gap || step_dir == 0 || (dir != 0 && step_dir != dir) {
            if cur.len() >= 3 {
                runs.push(cur.clone());
            }
            if coord_gap <= max_gap && step_dir != 0 {
                cur = vec![prev, idx];
                dir = step_dir;
            } else {
                cur = vec![idx];
                dir = 0;
            }
        } else {
            dir = step_dir;
            cur.push(idx);
        }
    }
    if cur.len() >= 3 {
        runs.push(cur);
    }
    runs
}

// ---------------------------------------------------------------------------
// Legend, Category & Title Helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct LegendEntry {
    rgb: [u8; 3],
    label: String,
    bbox: Rect,
}

fn detect_legends(
    collector: &ChartCollector<'_>,
    spans: &[TextItem],
    used_bar_indices: &HashSet<usize>,
    excluded_span_indices: &HashSet<usize>,
    search_region: Rect,
) -> Vec<LegendEntry> {
    let mut entries: Vec<LegendEntry> = Vec::new();

    // 1. Small filled swatch rects (4..=24 pt) with a text span immediately to their right.
    for (ri, fr) in collector.filled_rects.iter().enumerate() {
        if used_bar_indices.contains(&ri) {
            continue;
        }
        let w = fr.bbox.x1 - fr.bbox.x0;
        let h = fr.bbox.y1 - fr.bbox.y0;
        if !(4.0..=24.0).contains(&w) || !(3.0..=24.0).contains(&h) {
            continue;
        }
        if fr.bbox.intersect(search_region).is_empty() {
            continue;
        }
        let sy = 0.5 * (fr.bbox.y0 + fr.bbox.y1);
        if let Some(best_sp) = spans
            .iter()
            .enumerate()
            .filter(|(si, sp)| {
                !excluded_span_indices.contains(si)
                    && sp.bbox.x0 >= fr.bbox.x1 - 2.0
                    && sp.bbox.x0 <= fr.bbox.x1 + 28.0
                    && (sp.cy - sy).abs() <= 8.0
            })
            .min_by(|(_, a), (_, b)| (a.bbox.x0 - fr.bbox.x1).total_cmp(&(b.bbox.x0 - fr.bbox.x1)))
            .map(|(_, sp)| sp)
            && !entries
                .iter()
                .any(|e| rgb_dist_sq(e.rgb, fr.rgb) <= 16 && e.label == best_sp.text)
        {
            entries.push(LegendEntry {
                rgb: fr.rgb,
                label: best_sp.text.clone(),
                bbox: fr.bbox.union(best_sp.bbox),
            });
        }
    }

    // 2. Short horizontal stroked legend lines (6..=32 pt) with a text span immediately right.
    for seg in &collector.stroked_segs {
        if seg.from_closed_rect || seg.width > 8.0 {
            continue;
        }
        let dx = (seg.p1.x - seg.p0.x).abs();
        let dy = (seg.p1.y - seg.p0.y).abs();
        if dy > 1.5 || !(6.0..=32.0).contains(&dx) {
            continue;
        }
        let x1 = seg.p0.x.max(seg.p1.x);
        let sy = 0.5 * (seg.p0.y + seg.p1.y);
        let seg_rect = Rect::new(seg.p0.x.min(seg.p1.x), sy - 2.0, x1, sy + 2.0);
        if seg_rect.intersect(search_region).is_empty() {
            continue;
        }
        if let Some(best_sp) = spans
            .iter()
            .enumerate()
            .filter(|(si, sp)| {
                !excluded_span_indices.contains(si)
                    && sp.bbox.x0 >= x1 - 2.0
                    && sp.bbox.x0 <= x1 + 28.0
                    && (sp.cy - sy).abs() <= 8.0
            })
            .min_by(|(_, a), (_, b)| (a.bbox.x0 - x1).total_cmp(&(b.bbox.x0 - x1)))
            .map(|(_, sp)| sp)
            && !entries.iter().any(|e| rgb_dist_sq(e.rgb, seg.rgb) <= 36)
        {
            entries.push(LegendEntry {
                rgb: seg.rgb,
                label: best_sp.text.clone(),
                bbox: seg_rect.union(best_sp.bbox),
            });
        }
    }

    entries
}

fn match_legend_name(rgb: [u8; 3], legends: &[LegendEntry]) -> Option<(String, Rect)> {
    legends
        .iter()
        .filter_map(|e| {
            let d = rgb_dist_sq(rgb, e.rgb);
            (d <= 144).then_some((d, e))
        })
        .min_by_key(|(d, _)| *d)
        .map(|(_, e)| (e.label.clone(), e.bbox))
}

fn detect_chart_title(
    spans: &[TextItem],
    excluded: &HashSet<usize>,
    chart_x0: f32,
    chart_x1: f32,
    top_y: f32,
) -> Option<(String, Rect)> {
    let cands: Vec<(usize, &TextItem)> = spans
        .iter()
        .enumerate()
        .filter(|(i, sp)| {
            !excluded.contains(i)
                && sp.numeric.is_none()
                && sp.bbox.y1 <= top_y + 4.0
                && sp.bbox.y1 >= top_y - 60.0
                && sp.bbox.x1 >= chart_x0 - 20.0
                && sp.bbox.x0 <= chart_x1 + 20.0
        })
        .collect();
    if cands.is_empty() {
        return None;
    }

    // Prefer largest font size, breaking ties by closest above `top_y`.
    let best_row_cy = cands
        .iter()
        .max_by(|(_, a), (_, b)| {
            a.size
                .total_cmp(&b.size)
                .then_with(|| a.bbox.y1.total_cmp(&b.bbox.y1))
        })
        .map(|(_, s)| s.cy)?;

    let mut row: Vec<&TextItem> = cands
        .into_iter()
        .filter(|(_, s)| (s.cy - best_row_cy).abs() <= 5.0)
        .map(|(_, s)| s)
        .collect();
    row.sort_by(|a, b| a.bbox.x0.total_cmp(&b.bbox.x0));

    let mut bbox = Rect::EMPTY;
    let mut parts = Vec::with_capacity(row.len());
    for sp in row {
        bbox = bbox.union(sp.bbox);
        parts.push(sp.text.as_str());
    }
    let joined = parts.join(" ");
    (!joined.is_empty()).then_some((joined, bbox))
}

// ---------------------------------------------------------------------------
// 1A & 1B: Bar Chart (Vertical & Horizontal) + Line Chart Decompiler
// ---------------------------------------------------------------------------

fn extract_all_charts(
    page_num: i32,
    collector: &ChartCollector<'_>,
    spans: &[TextItem],
) -> (Vec<ExtractedChart>, Vec<Rect>) {
    let mut charts: Vec<ExtractedChart> = Vec::new();
    let mut claimed_rects: Vec<Rect> = Vec::new();
    let mut used_span_indices: HashSet<usize> = HashSet::new();
    let mut used_bar_rects: HashSet<usize> = HashSet::new();

    let v_axes = find_vertical_axes(spans);
    for v_axis in &v_axes {
        for &ti in &v_axis.tick_indices {
            used_span_indices.insert(ti);
        }
    }

    // Pass 1: Vertical Bar Charts and Line Charts over calibrated vertical Y-axes.
    for v_axis in &v_axes {
        if let Some((mut chart, chart_rect, bar_idxs, cat_idxs)) = try_decompile_vertical_bar_chart(
            page_num,
            charts.len() + 1,
            v_axis,
            collector,
            spans,
            &used_bar_rects,
            &used_span_indices,
        ) {
            for bi in bar_idxs {
                used_bar_rects.insert(bi);
            }
            for ci in cat_idxs {
                used_span_indices.insert(ci);
            }
            chart.id = format!("p{page_num}.chart{}", charts.len() + 1);
            claimed_rects.push(chart_rect);
            charts.push(chart);
            continue;
        }

        if let Some((mut chart, chart_rect, cat_idxs)) = try_decompile_line_chart(
            page_num,
            charts.len() + 1,
            v_axis,
            collector,
            spans,
            &used_bar_rects,
            &used_span_indices,
        ) {
            for ci in cat_idxs {
                used_span_indices.insert(ci);
            }
            chart.id = format!("p{page_num}.chart{}", charts.len() + 1);
            claimed_rects.push(chart_rect);
            charts.push(chart);
        }
    }

    // Pass 2: Horizontal Bar Charts over calibrated horizontal X-axes.
    let h_axes = find_horizontal_axes(spans, &used_span_indices);
    for h_axis in &h_axes {
        for &ti in &h_axis.tick_indices {
            used_span_indices.insert(ti);
        }
    }
    for h_axis in &h_axes {
        if let Some((mut chart, chart_rect, bar_idxs, cat_idxs)) =
            try_decompile_horizontal_bar_chart(
                page_num,
                charts.len() + 1,
                h_axis,
                collector,
                spans,
                &used_bar_rects,
                &used_span_indices,
            )
        {
            for bi in bar_idxs {
                used_bar_rects.insert(bi);
            }
            for ci in cat_idxs {
                used_span_indices.insert(ci);
            }
            chart.id = format!("p{page_num}.chart{}", charts.len() + 1);
            claimed_rects.push(chart_rect);
            charts.push(chart);
        }
    }

    (charts, claimed_rects)
}

fn find_plot_x_bounds_for_v_axis(
    v_axis: &CalibratedAxis,
    collector: &ChartCollector<'_>,
) -> (f32, f32, Option<f32>) {
    let y_zero = v_axis.coord_at_zero as f32;
    let y_lo = v_axis.ticks_bbox.y0.min(y_zero) - 25.0;
    let y_hi = v_axis.ticks_bbox.y1.max(y_zero) + 25.0;
    let min_x0 = v_axis.ticks_bbox.x1 - 4.0;

    // Look for a horizontal axis/grid stroke near `y_zero` or `ticks_bbox.y1` to bound plot X.
    let mut axis_x0: Option<f32> = None;
    let mut axis_x1: Option<f32> = None;
    let mut baseline_stroke_y: Option<f32> = None;

    for seg in &collector.stroked_segs {
        let dy = (seg.p1.y - seg.p0.y).abs();
        let dx = (seg.p1.x - seg.p0.x).abs();
        if dy <= 1.2 && dx >= 40.0 {
            let sy = 0.5 * (seg.p0.y + seg.p1.y);
            let sx0 = seg.p0.x.min(seg.p1.x);
            let sx1 = seg.p0.x.max(seg.p1.x);
            if sy >= y_lo && sy <= y_hi && sx0 >= min_x0 - 15.0 && sx0 <= min_x0 + 60.0 {
                axis_x0 = Some(axis_x0.map_or(sx0, |cur| cur.min(sx0)));
                axis_x1 = Some(axis_x1.map_or(sx1, |cur| cur.max(sx1)));
                if (sy - y_zero).abs() <= 4.0 {
                    baseline_stroke_y = Some(sy);
                }
            }
        }
    }

    let x0 = axis_x0.unwrap_or(v_axis.ticks_bbox.x1);
    let x1 = axis_x1.unwrap_or_else(|| (collector.mediabox.x1 - 12.0).max(x0 + 120.0));
    (x0, x1, baseline_stroke_y)
}

fn try_decompile_vertical_bar_chart(
    page_num: i32,
    seq: usize,
    v_axis: &CalibratedAxis,
    collector: &ChartCollector<'_>,
    spans: &[TextItem],
    used_bar_rects: &HashSet<usize>,
    used_spans: &HashSet<usize>,
) -> Option<(ExtractedChart, Rect, Vec<usize>, Vec<usize>)> {
    let y_zero = v_axis.coord_at_zero as f32;
    if !y_zero.is_finite() {
        return None;
    }
    let (plot_x0, mut plot_x1, baseline_stroke_y) =
        find_plot_x_bounds_for_v_axis(v_axis, collector);
    let plot_y0 = v_axis.ticks_bbox.y0.min(y_zero) - 35.0;
    let plot_y1 = v_axis.ticks_bbox.y1.max(y_zero) + 35.0;
    let ref_zero_y = baseline_stroke_y.unwrap_or(y_zero);

    let plot_span_w = (plot_x1 - plot_x0).max(40.0);

    // Gather candidate vertical bars anchored at `y_at_zero` (or horizontal baseline stroke).
    let mut cands: Vec<(usize, FilledRect)> = Vec::new();
    for (idx, fr) in collector.filled_rects.iter().enumerate() {
        if used_bar_rects.contains(&idx) {
            continue;
        }
        let w = fr.bbox.x1 - fr.bbox.x0;
        let h = fr.bbox.y1 - fr.bbox.y0;
        if !(4.0..=120.0).contains(&w) || h < 1.0 {
            continue;
        }
        if w >= 0.78 * plot_span_w && h >= 0.5 * (plot_y1 - plot_y0).max(20.0) {
            continue; // Plot background fill panel.
        }
        if fr.bbox.x0 < plot_x0 - 6.0 || fr.bbox.x1 > plot_x1 + 25.0 {
            continue;
        }
        if fr.bbox.y0 < plot_y0 - 20.0 || fr.bbox.y1 > plot_y1 + 20.0 {
            continue;
        }
        let anchored_bottom =
            (fr.bbox.y1 - y_zero).abs() <= 3.5 || (fr.bbox.y1 - ref_zero_y).abs() <= 3.5;
        let anchored_top =
            (fr.bbox.y0 - y_zero).abs() <= 3.5 || (fr.bbox.y0 - ref_zero_y).abs() <= 3.5;
        if !anchored_bottom && !anchored_top {
            continue;
        }
        cands.push((idx, *fr));
    }

    if cands.len() < 2 {
        return None;
    }

    cands.sort_by(|(_, a), (_, b)| a.bbox.x0.total_cmp(&b.bbox.x0));
    let bars_max_x1 = cands.iter().map(|(_, b)| b.bbox.x1).fold(plot_x0, f32::max);
    plot_x1 = plot_x1.min(bars_max_x1 + 90.0).max(bars_max_x1 + 8.0);

    // Find X category label row below (or above) the plot bottom.
    let axis_bottom = v_axis.ticks_bbox.y1.max(y_zero);
    let (categories, cat_span_idxs) = find_x_category_spans(
        spans,
        used_spans,
        plot_x0 - 10.0,
        plot_x1 + 15.0,
        axis_bottom,
    );

    // Group bars into horizontal category clusters so multi-series grouped bars
    // align with their shared category label.
    let clusters = group_vertical_bar_clusters(&cands);

    let mut bar_idx_used = Vec::with_capacity(cands.len());
    let mut bars_bbox = Rect::EMPTY;
    for &(idx, fr) in &cands {
        bar_idx_used.push(idx);
        bars_bbox = bars_bbox.union(fr.bbox);
    }

    let mut excl = used_spans.clone();
    for &ci in &cat_span_idxs {
        excl.insert(ci);
    }
    let mut bar_set = used_bar_rects.clone();
    for &bi in &bar_idx_used {
        bar_set.insert(bi);
    }

    let legend_region = Rect::new(
        v_axis.ticks_bbox.x0 - 20.0,
        plot_y0 - 45.0,
        (plot_x1 + 160.0).min(collector.mediabox.x1),
        axis_bottom + 75.0,
    );
    let legends = detect_legends(collector, spans, &bar_set, &excl, legend_region);

    // Build series preserving color discovery order.
    let mut color_order: Vec<[u8; 3]> = Vec::new();
    for &(_, fr) in &cands {
        if !color_order.iter().any(|&c| rgb_dist_sq(c, fr.rgb) <= 9) {
            color_order.push(fr.rgb);
        }
    }

    let mut chart_bbox = v_axis.ticks_bbox.union(bars_bbox);
    for (_, _, cb) in &categories {
        chart_bbox = chart_bbox.union(*cb);
    }

    let mut series_list: Vec<ChartSeries> = Vec::with_capacity(color_order.len());
    for (s_idx, &s_rgb) in color_order.iter().enumerate() {
        let (name, leg_bbox) = match match_legend_name(s_rgb, &legends) {
            Some((n, lb)) => {
                chart_bbox = chart_bbox.union(lb);
                (n, Some(lb))
            }
            None => (format!("series_{}", s_idx + 1), None),
        };
        let _ = leg_bbox;

        let mut points = Vec::new();
        for (cl_idx, cluster) in clusters.iter().enumerate() {
            let cl_cx = cluster_center_x(cluster);
            let cat_label = nearest_category_label(cl_cx, cl_idx, &categories);

            for &(_, fr) in cluster {
                if rgb_dist_sq(fr.rgb, s_rgb) <= 9 {
                    let dist_bot = (fr.bbox.y1 - ref_zero_y)
                        .abs()
                        .min((fr.bbox.y1 - y_zero).abs());
                    let dist_top = (fr.bbox.y0 - ref_zero_y)
                        .abs()
                        .min((fr.bbox.y0 - y_zero).abs());
                    let (base_y, tip_y) = if dist_bot <= dist_top {
                        (f64::from(fr.bbox.y1), f64::from(fr.bbox.y0))
                    } else {
                        (f64::from(fr.bbox.y0), f64::from(fr.bbox.y1))
                    };
                    // Pin zero intercept to the geometric bar baseline (`|base_y - y_zero| <= 3.5 pt`)
                    // so font ascender/descender box offsets on tick glyphs do not bias bar heights.
                    let val = round4(v_axis.slope * (tip_y - base_y));
                    points.push(ChartPoint {
                        label: cat_label.clone(),
                        x_value: None,
                        value: val,
                        bbox: rect_array(fr.bbox),
                    });
                }
            }
        }

        if !points.is_empty() {
            series_list.push(ChartSeries {
                name,
                color: hex_rgb(s_rgb),
                points,
            });
        }
    }

    if series_list.is_empty() {
        return None;
    }

    let title = detect_chart_title(spans, &excl, chart_bbox.x0, chart_bbox.x1, chart_bbox.y0).map(
        |(t, tb)| {
            chart_bbox = chart_bbox.union(tb);
            t
        },
    );

    Some((
        ExtractedChart {
            id: format!("p{page_num}.chart{seq}"),
            page: page_num,
            kind: "bar".to_string(),
            title,
            bbox: rect_array(chart_bbox),
            axis_r_squared: round4(v_axis.r_squared),
            series: series_list,
        },
        chart_bbox,
        bar_idx_used,
        cat_span_idxs,
    ))
}

fn group_vertical_bar_clusters(cands: &[(usize, FilledRect)]) -> Vec<Vec<(usize, FilledRect)>> {
    if cands.is_empty() {
        return Vec::new();
    }
    let avg_w = cands
        .iter()
        .map(|(_, b)| b.bbox.x1 - b.bbox.x0)
        .sum::<f32>()
        / (cands.len() as f32);
    let mut gaps: Vec<f32> = cands
        .windows(2)
        .map(|w| (w[1].1.bbox.x0 - w[0].1.bbox.x1).max(0.0))
        .collect();
    gaps.sort_by(f32::total_cmp);

    let max_gap = gaps.last().copied().unwrap_or(0.0);
    let min_gap = gaps.first().copied().unwrap_or(0.0);
    // When multi-series bars are grouped, intra-group gap is noticeably smaller than inter-group gap.
    let cluster_thresh = if max_gap > min_gap + avg_w * 0.35 {
        0.5 * (min_gap + max_gap)
    } else {
        -1.0 // Single bar per category.
    };

    let mut clusters: Vec<Vec<(usize, FilledRect)>> = Vec::new();
    for &item in cands {
        if let Some(cur) = clusters.last_mut()
            && let Some(&(_, prev)) = cur.last()
            && cluster_thresh >= 0.0
            && (item.1.bbox.x0 - prev.bbox.x1) <= cluster_thresh
        {
            cur.push(item);
            continue;
        }
        clusters.push(vec![item]);
    }
    clusters
}

fn cluster_center_x(cluster: &[(usize, FilledRect)]) -> f32 {
    let x0 = cluster
        .iter()
        .map(|(_, b)| b.bbox.x0)
        .fold(f32::INFINITY, f32::min);
    let x1 = cluster
        .iter()
        .map(|(_, b)| b.bbox.x1)
        .fold(f32::NEG_INFINITY, f32::max);
    0.5 * (x0 + x1)
}

fn find_x_category_spans(
    spans: &[TextItem],
    used_spans: &HashSet<usize>,
    x_min: f32,
    x_max: f32,
    axis_bottom: f32,
) -> (Vec<(f32, String, Rect)>, Vec<usize>) {
    let cands: Vec<(usize, &TextItem)> = spans
        .iter()
        .enumerate()
        .filter(|(i, sp)| {
            !used_spans.contains(i)
                && sp.cx >= x_min
                && sp.cx <= x_max
                && sp.bbox.y0 >= axis_bottom - 6.0
                && sp.bbox.y0 <= axis_bottom + 42.0
        })
        .collect();
    if cands.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // Pick the horizontal row closest below `axis_bottom`.
    let row_y = cands
        .iter()
        .min_by(|(_, a), (_, b)| a.cy.total_cmp(&b.cy))
        .map_or(axis_bottom, |(_, s)| s.cy);

    let mut row: Vec<(usize, &TextItem)> = cands
        .into_iter()
        .filter(|(_, s)| (s.cy - row_y).abs() <= 8.0)
        .collect();
    row.sort_by(|(_, a), (_, b)| a.cx.total_cmp(&b.cx));

    let idxs = row.iter().map(|&(i, _)| i).collect();
    let cats = row
        .into_iter()
        .map(|(_, s)| (s.cx, s.text.clone(), s.bbox))
        .collect();
    (cats, idxs)
}

fn nearest_category_label(coord: f32, fallback_idx: usize, cats: &[(f32, String, Rect)]) -> String {
    cats.iter()
        .min_by(|(ca, _, _), (cb, _, _)| (ca - coord).abs().total_cmp(&(cb - coord).abs()))
        .map(|(_, label, _)| label.clone())
        .unwrap_or_else(|| format!("cat_{}", fallback_idx + 1))
}

fn try_decompile_horizontal_bar_chart(
    page_num: i32,
    seq: usize,
    h_axis: &CalibratedAxis,
    collector: &ChartCollector<'_>,
    spans: &[TextItem],
    used_bar_rects: &HashSet<usize>,
    used_spans: &HashSet<usize>,
) -> Option<(ExtractedChart, Rect, Vec<usize>, Vec<usize>)> {
    let x_zero = h_axis.coord_at_zero as f32;
    if !x_zero.is_finite() {
        return None;
    }
    let plot_x0 = h_axis.ticks_bbox.x0.min(x_zero) - 25.0;
    let plot_x1 = h_axis.ticks_bbox.x1.max(x_zero) + 35.0;

    // Horizontal bars normally sit ABOVE the X tick row (`y < ticks_bbox.y0`), or below if top-axis.
    let mut cands: Vec<(usize, FilledRect)> = Vec::new();
    for (idx, fr) in collector.filled_rects.iter().enumerate() {
        if used_bar_rects.contains(&idx) {
            continue;
        }
        let w = fr.bbox.x1 - fr.bbox.x0;
        let h = fr.bbox.y1 - fr.bbox.y0;
        if !(4.0..=120.0).contains(&h) || w < 1.0 {
            continue;
        }
        if fr.bbox.x0 < plot_x0 - 10.0 || fr.bbox.x1 > plot_x1 + 20.0 {
            continue;
        }
        let anchored_left = (fr.bbox.x0 - x_zero).abs() <= 3.5;
        let anchored_right = (fr.bbox.x1 - x_zero).abs() <= 3.5;
        if !anchored_left && !anchored_right {
            continue;
        }
        if (fr.bbox.y1 - h_axis.ticks_bbox.y0).abs() > 360.0
            && (fr.bbox.y0 - h_axis.ticks_bbox.y1).abs() > 360.0
        {
            continue;
        }
        cands.push((idx, *fr));
    }

    if cands.len() < 2 {
        return None;
    }
    cands.sort_by(|(_, a), (_, b)| a.bbox.y0.total_cmp(&b.bbox.y0));

    let bars_y0 = cands
        .iter()
        .map(|(_, b)| b.bbox.y0)
        .fold(f32::INFINITY, f32::min);
    let bars_y1 = cands
        .iter()
        .map(|(_, b)| b.bbox.y1)
        .fold(f32::NEG_INFINITY, f32::max);

    // Reject single full-height backdrop rect masquerading as a bar.
    cands.retain(|(_, b)| (b.bbox.y1 - b.bbox.y0) < 0.8 * (bars_y1 - bars_y0).max(30.0));
    if cands.len() < 2 {
        return None;
    }

    // Category labels sit to the left of `x_zero` within `[bars_y0 - 10, bars_y1 + 10]`.
    let mut cat_cands: Vec<(usize, &TextItem)> = spans
        .iter()
        .enumerate()
        .filter(|(i, sp)| {
            !used_spans.contains(i)
                && sp.bbox.x1 <= x_zero + 4.0
                && sp.bbox.x1 >= x_zero - 140.0
                && sp.cy >= bars_y0 - 10.0
                && sp.cy <= bars_y1 + 10.0
        })
        .collect();
    cat_cands.sort_by(|(_, a), (_, b)| a.cy.total_cmp(&b.cy));

    let cat_idxs: Vec<usize> = cat_cands.iter().map(|&(i, _)| i).collect();
    let cats: Vec<(f32, String, Rect)> = cat_cands
        .into_iter()
        .map(|(_, s)| (s.cy, s.text.clone(), s.bbox))
        .collect();

    // Group horizontal bars into vertical category clusters.
    let avg_h = cands
        .iter()
        .map(|(_, b)| b.bbox.y1 - b.bbox.y0)
        .sum::<f32>()
        / (cands.len() as f32);
    let mut gaps: Vec<f32> = cands
        .windows(2)
        .map(|w| (w[1].1.bbox.y0 - w[0].1.bbox.y1).max(0.0))
        .collect();
    gaps.sort_by(f32::total_cmp);
    let max_gap = gaps.last().copied().unwrap_or(0.0);
    let min_gap = gaps.first().copied().unwrap_or(0.0);
    let cluster_thresh = if max_gap > min_gap + avg_h * 0.35 {
        0.5 * (min_gap + max_gap)
    } else {
        -1.0
    };

    let mut clusters: Vec<Vec<(usize, FilledRect)>> = Vec::new();
    for &item in &cands {
        if let Some(cur) = clusters.last_mut()
            && let Some(&(_, prev)) = cur.last()
            && cluster_thresh >= 0.0
            && (item.1.bbox.y0 - prev.bbox.y1) <= cluster_thresh
        {
            cur.push(item);
            continue;
        }
        clusters.push(vec![item]);
    }

    let mut bar_idx_used = Vec::with_capacity(cands.len());
    let mut chart_bbox = h_axis.ticks_bbox;
    for &(idx, fr) in &cands {
        bar_idx_used.push(idx);
        chart_bbox = chart_bbox.union(fr.bbox);
    }
    for (_, _, cb) in &cats {
        chart_bbox = chart_bbox.union(*cb);
    }

    let mut excl = used_spans.clone();
    for &ci in &cat_idxs {
        excl.insert(ci);
    }
    let mut bar_set = used_bar_rects.clone();
    for &bi in &bar_idx_used {
        bar_set.insert(bi);
    }

    let legend_region = chart_bbox.expand(80.0);
    let legends = detect_legends(collector, spans, &bar_set, &excl, legend_region);

    let mut color_order: Vec<[u8; 3]> = Vec::new();
    for &(_, fr) in &cands {
        if !color_order.iter().any(|&c| rgb_dist_sq(c, fr.rgb) <= 9) {
            color_order.push(fr.rgb);
        }
    }

    let mut series_list = Vec::new();
    for (s_idx, &s_rgb) in color_order.iter().enumerate() {
        let name = match match_legend_name(s_rgb, &legends) {
            Some((n, lb)) => {
                chart_bbox = chart_bbox.union(lb);
                n
            }
            None => format!("series_{}", s_idx + 1),
        };
        let mut points = Vec::new();
        for (cl_idx, cluster) in clusters.iter().enumerate() {
            let y0 = cluster
                .iter()
                .map(|(_, b)| b.bbox.y0)
                .fold(f32::INFINITY, f32::min);
            let y1 = cluster
                .iter()
                .map(|(_, b)| b.bbox.y1)
                .fold(f32::NEG_INFINITY, f32::max);
            let cl_cy = 0.5 * (y0 + y1);
            let cat_label = nearest_category_label(cl_cy, cl_idx, &cats);
            for &(_, fr) in cluster {
                if rgb_dist_sq(fr.rgb, s_rgb) <= 9 {
                    let dist_l = (fr.bbox.x0 - x_zero).abs();
                    let dist_r = (fr.bbox.x1 - x_zero).abs();
                    let (base_x, tip_x) = if dist_l <= dist_r {
                        (f64::from(fr.bbox.x0), f64::from(fr.bbox.x1))
                    } else {
                        (f64::from(fr.bbox.x1), f64::from(fr.bbox.x0))
                    };
                    let val = round4(h_axis.slope * (tip_x - base_x));
                    points.push(ChartPoint {
                        label: cat_label.clone(),
                        x_value: None,
                        value: val,
                        bbox: rect_array(fr.bbox),
                    });
                }
            }
        }
        if !points.is_empty() {
            series_list.push(ChartSeries {
                name,
                color: hex_rgb(s_rgb),
                points,
            });
        }
    }

    if series_list.is_empty() {
        return None;
    }

    let title = detect_chart_title(spans, &excl, chart_bbox.x0, chart_bbox.x1, chart_bbox.y0).map(
        |(t, tb)| {
            chart_bbox = chart_bbox.union(tb);
            t
        },
    );

    Some((
        ExtractedChart {
            id: format!("p{page_num}.chart{seq}"),
            page: page_num,
            kind: "horizontal_bar".to_string(),
            title,
            bbox: rect_array(chart_bbox),
            axis_r_squared: round4(h_axis.r_squared),
            series: series_list,
        },
        chart_bbox,
        bar_idx_used,
        cat_idxs,
    ))
}

fn try_decompile_line_chart(
    page_num: i32,
    seq: usize,
    v_axis: &CalibratedAxis,
    collector: &ChartCollector<'_>,
    spans: &[TextItem],
    used_bar_rects: &HashSet<usize>,
    used_spans: &HashSet<usize>,
) -> Option<(ExtractedChart, Rect, Vec<usize>)> {
    let y_zero = v_axis.coord_at_zero as f32;
    let (plot_x0, plot_x1, baseline_stroke_y) = find_plot_x_bounds_for_v_axis(v_axis, collector);
    let intercept_eff = match baseline_stroke_y {
        Some(sy) if (sy - y_zero).abs() <= 3.5 => -v_axis.slope * f64::from(sy),
        _ => v_axis.intercept,
    };
    let plot_y0 = v_axis.ticks_bbox.y0 - 25.0;
    let plot_y1 = if y_zero.is_finite() && (y_zero - v_axis.ticks_bbox.y1).abs() <= 40.0 {
        v_axis.ticks_bbox.y1.max(y_zero) + 15.0
    } else {
        v_axis.ticks_bbox.y1 + 15.0
    };
    let plot_rect = Rect::new(plot_x0 - 4.0, plot_y0, plot_x1 + 10.0, plot_y1);
    let plot_w = (plot_x1 - plot_x0).max(40.0);

    // Filter candidate open polylines inside `plot_rect` that are NOT axis lines or gridlines.
    let mut raw_polys: Vec<OpenPolyline> = Vec::new();
    for poly in &collector.polylines {
        if poly.is_arrow_barb || poly.pts.len() < 2 {
            continue;
        }
        if !plot_rect.contains(poly.bbox) {
            continue;
        }
        // Reject pure horizontal or pure vertical 2-point axis/grid/tick lines.
        if poly.pts.len() == 2 {
            let dx = (poly.pts[1][0] - poly.pts[0][0]).abs();
            let dy = (poly.pts[1][1] - poly.pts[0][1]).abs();
            if dy <= 1.0 && (dx >= 0.65 * plot_w || dx <= 8.0) {
                continue;
            }
            if dx <= 1.0 && (dy >= 0.65 * (plot_y1 - plot_y0).max(30.0) || dy <= 8.0) {
                continue;
            }
        }
        // Reject L-shaped 3-point axis frames `(x0, y0) -> (x0, y1) -> (x1, y1)`.
        if poly.pts.len() == 3 {
            let dx0 = (poly.pts[1][0] - poly.pts[0][0]).abs();
            let dy0 = (poly.pts[1][1] - poly.pts[0][1]).abs();
            let dx1 = (poly.pts[2][0] - poly.pts[1][0]).abs();
            let dy1 = (poly.pts[2][1] - poly.pts[1][1]).abs();
            if (dx0 <= 1.0 && dy1 <= 1.0) || (dy0 <= 1.0 && dx1 <= 1.0) {
                continue;
            }
        }
        raw_polys.push(poly.clone());
    }

    let chained = chain_polylines(&raw_polys, 1.0);
    let mut series_polys: Vec<OpenPolyline> = chained
        .into_iter()
        .filter(|p| p.pts.len() >= 3 && (p.bbox.x1 - p.bbox.x0) >= 18.0)
        .collect();

    // Also collect standalone scatter marker groups (`>= 3` markers of a color inside `plot_rect`
    // that are not already attached to a polyline vertex).
    let mut scatter_groups: Vec<([u8; 3], Vec<ScatterMarker>, Rect)> = Vec::new();
    for m in &collector.markers {
        if !plot_rect.contains(m.bbox) {
            continue;
        }
        let on_poly = series_polys.iter().any(|p| {
            p.pts
                .iter()
                .any(|&[vx, vy]| pt_dist(m.center, Point::new(vx, vy)) <= 3.5)
        });
        if on_poly {
            continue;
        }
        if let Some((_, list, gb)) = scatter_groups
            .iter_mut()
            .find(|(c, _, _)| rgb_dist_sq(*c, m.rgb) <= 36)
        {
            list.push(*m);
            *gb = gb.union(m.bbox);
        } else {
            scatter_groups.push((m.rgb, vec![*m], m.bbox));
        }
    }
    scatter_groups.retain(|(_, list, gb)| list.len() >= 3 && (gb.x1 - gb.x0) >= 18.0);

    if series_polys.is_empty() && scatter_groups.is_empty() {
        return None;
    }

    let max_poly_x1 = series_polys
        .iter()
        .map(|p| p.bbox.x1)
        .chain(scatter_groups.iter().map(|(_, _, gb)| gb.x1))
        .fold(plot_x0, f32::max);
    let axis_bottom = v_axis.ticks_bbox.y1.max(
        series_polys
            .iter()
            .map(|p| p.bbox.y1)
            .chain(scatter_groups.iter().map(|(_, _, gb)| gb.y1))
            .fold(v_axis.ticks_bbox.y1, f32::max),
    );
    let (categories, cat_idxs) = find_x_category_spans(
        spans,
        used_spans,
        plot_x0 - 15.0,
        max_poly_x1 + 30.0,
        axis_bottom,
    );

    // Check if the X category spans actually form a linear numeric X-axis (`>= 3` ticks).
    let numeric_x_fit = if categories.len() >= 3 {
        let mut num_x = Vec::new();
        let mut num_v = Vec::new();
        let mut all_num = true;
        for (cx, txt, _) in &categories {
            if let Some(v) = parse_tick_number(txt) {
                num_x.push(f64::from(*cx));
                num_v.push(v);
            } else {
                all_num = false;
                break;
            }
        }
        if all_num {
            fit_linear(&num_x, &num_v).filter(|&(_, _, r2)| r2 >= MIN_AXIS_R_SQUARED)
        } else {
            None
        }
    } else {
        None
    };

    let mut chart_bbox = v_axis.ticks_bbox;
    for p in &series_polys {
        chart_bbox = chart_bbox.union(p.bbox);
    }
    for (_, _, gb) in &scatter_groups {
        chart_bbox = chart_bbox.union(*gb);
    }
    for (_, _, cb) in &categories {
        chart_bbox = chart_bbox.union(*cb);
    }

    let mut excl = used_spans.clone();
    for &ci in &cat_idxs {
        excl.insert(ci);
    }
    let legend_region = chart_bbox.expand(90.0);
    let legends = detect_legends(collector, spans, used_bar_rects, &excl, legend_region);

    series_polys.sort_by(|a, b| a.bbox.y0.total_cmp(&b.bbox.y0));

    let mut series_list = Vec::with_capacity(series_polys.len());
    for (s_idx, poly) in series_polys.iter().enumerate() {
        let mut pts = poly.pts.clone();
        // Order vertices left-to-right when monotonic in X.
        if pts
            .first()
            .zip(pts.last())
            .is_some_and(|(f, l)| f[0] > l[0])
        {
            pts.reverse();
        }
        let name = match match_legend_name(poly.rgb, &legends) {
            Some((n, lb)) => {
                chart_bbox = chart_bbox.union(lb);
                n
            }
            None => format!("series_{}", s_idx + 1),
        };

        let mut points = Vec::with_capacity(pts.len());
        for (pt_idx, &[vx, vy]) in pts.iter().enumerate() {
            let val = round4(v_axis.slope * f64::from(vy) + intercept_eff);
            let x_value = numeric_x_fit.map(|(sx, ix, _)| round4(sx * f64::from(vx) + ix));
            let label = if !categories.is_empty() {
                nearest_category_label(vx, pt_idx, &categories)
            } else if let Some(xv) = x_value {
                format!("{xv}")
            } else {
                format!("pt_{}", pt_idx + 1)
            };

            let marker_bbox = collector
                .markers
                .iter()
                .find(|m| {
                    pt_dist(m.center, Point::new(vx, vy)) <= 3.5
                        && (rgb_dist_sq(m.rgb, poly.rgb) <= 144
                            || pt_dist(m.center, Point::new(vx, vy)) <= 1.8)
                })
                .map_or([vx - 2.0, vy - 2.0, vx + 2.0, vy + 2.0], |m| {
                    rect_array(m.bbox)
                });

            points.push(ChartPoint {
                label,
                x_value,
                value: val,
                bbox: marker_bbox,
            });
        }

        series_list.push(ChartSeries {
            name,
            color: hex_rgb(poly.rgb),
            points,
        });
    }

    for (m_rgb, mut markers, _) in scatter_groups {
        markers.sort_by(|a, b| a.center.x.total_cmp(&b.center.x));
        let s_idx = series_list.len();
        let name = match match_legend_name(m_rgb, &legends) {
            Some((n, lb)) => {
                chart_bbox = chart_bbox.union(lb);
                n
            }
            None => format!("series_{}", s_idx + 1),
        };
        let mut points = Vec::with_capacity(markers.len());
        for (pt_idx, m) in markers.iter().enumerate() {
            let vx = m.center.x;
            let vy = m.center.y;
            let val = round4(v_axis.slope * f64::from(vy) + intercept_eff);
            let x_value = numeric_x_fit.map(|(sx, ix, _)| round4(sx * f64::from(vx) + ix));
            let label = if !categories.is_empty() {
                nearest_category_label(vx, pt_idx, &categories)
            } else if let Some(xv) = x_value {
                format!("{xv}")
            } else {
                format!("pt_{}", pt_idx + 1)
            };
            points.push(ChartPoint {
                label,
                x_value,
                value: val,
                bbox: rect_array(m.bbox),
            });
        }
        series_list.push(ChartSeries {
            name,
            color: hex_rgb(m_rgb),
            points,
        });
    }

    let title = detect_chart_title(spans, &excl, chart_bbox.x0, chart_bbox.x1, chart_bbox.y0).map(
        |(t, tb)| {
            chart_bbox = chart_bbox.union(tb);
            t
        },
    );

    Some((
        ExtractedChart {
            id: format!("p{page_num}.chart{seq}"),
            page: page_num,
            kind: "line".to_string(),
            title,
            bbox: rect_array(chart_bbox),
            axis_r_squared: round4(v_axis.r_squared),
            series: series_list,
        },
        chart_bbox,
        cat_idxs,
    ))
}

/// Chains open polylines sharing endpoints within `tol` points and matching stroke style,
/// refusing ambiguous T-junctions (`degree > 2`).
fn chain_polylines(input: &[OpenPolyline], tol: f32) -> Vec<OpenPolyline> {
    if input.len() <= 1 || input.len() > 2_000 {
        return input.to_vec();
    }
    let mut polys: Vec<Option<OpenPolyline>> = input.iter().cloned().map(Some).collect();
    let max_iters = input.len();

    for _ in 0..max_iters {
        let mut merged_any = false;
        for i in 0..polys.len() {
            let Some(ref pi) = polys[i] else { continue };
            let pi_start = Point::new(pi.pts[0][0], pi.pts[0][1]);
            let pi_end_arr = pi.pts[pi.pts.len() - 1];
            let pi_end = Point::new(pi_end_arr[0], pi_end_arr[1]);

            let mut match_at_end: Vec<(usize, bool)> = Vec::new();
            let mut match_at_start: Vec<(usize, bool)> = Vec::new();

            for (j, slot_j) in polys.iter().enumerate() {
                if i == j {
                    continue;
                }
                let Some(pj) = slot_j else { continue };
                if rgb_dist_sq(pi.rgb, pj.rgb) > 36 || (pi.width - pj.width).abs() > 0.9 {
                    continue;
                }
                let pj_start = Point::new(pj.pts[0][0], pj.pts[0][1]);
                let pj_end_arr = pj.pts[pj.pts.len() - 1];
                let pj_end = Point::new(pj_end_arr[0], pj_end_arr[1]);

                if pt_dist(pi_end, pj_start) <= tol {
                    match_at_end.push((j, false));
                } else if pt_dist(pi_end, pj_end) <= tol {
                    match_at_end.push((j, true));
                }

                if pt_dist(pi_start, pj_end) <= tol {
                    match_at_start.push((j, false));
                } else if pt_dist(pi_start, pj_start) <= tol {
                    match_at_start.push((j, true));
                }
            }

            if match_at_end.len() == 1 {
                let (j, rev_j) = match_at_end[0];
                if let (Some(mut base), Some(mut ext)) = (polys[i].take(), polys[j].take()) {
                    if rev_j {
                        ext.pts.reverse();
                    }
                    base.pts.extend_from_slice(&ext.pts[1..]);
                    base.bbox = base.bbox.union(ext.bbox);
                    polys[i] = Some(base);
                    merged_any = true;
                    continue;
                }
            }

            if match_at_start.len() == 1 {
                let (j, rev_j) = match_at_start[0];
                if let (Some(base), Some(mut pre)) = (polys[i].take(), polys[j].take()) {
                    if rev_j {
                        pre.pts.reverse();
                    }
                    pre.pts.extend_from_slice(&base.pts[1..]);
                    pre.bbox = pre.bbox.union(base.bbox);
                    polys[i] = Some(pre);
                    merged_any = true;
                }
            }
        }
        if !merged_any {
            break;
        }
    }

    polys.into_iter().flatten().collect()
}

// ---------------------------------------------------------------------------
// 1C: Vector Flowchart / Node-Edge Diagram Decompiler
// ---------------------------------------------------------------------------

fn extract_diagrams(
    page_num: i32,
    collector: &ChartCollector<'_>,
    spans: &[TextItem],
    claimed_chart_rects: &[Rect],
) -> Vec<ExtractedDiagram> {
    // 1. Candidate node boxes (`w >= 30, h >= 14`), outside extracted charts, enclosing >= 1 text span.
    let mut raw_boxes: Vec<Rect> = Vec::new();
    for cb in &collector.closed_boxes {
        let w = cb.bbox.x1 - cb.bbox.x0;
        let h = cb.bbox.y1 - cb.bbox.y0;
        if w < 30.0 || h < 14.0 {
            continue;
        }
        // Must be stroked or lightly-filled.
        if !cb.is_stroked && !cb.is_filled {
            continue;
        }
        // Skip boxes overlapping or enclosing a calibrated chart.
        if claimed_chart_rects
            .iter()
            .any(|cr| !cb.bbox.intersect(*cr).is_empty())
        {
            continue;
        }
        raw_boxes.push(cb.bbox);
    }

    // Drop outer container frames that strictly enclose >= 2 smaller candidate boxes.
    let mut leaf_boxes: Vec<Rect> = Vec::new();
    for (i, r) in raw_boxes.iter().enumerate() {
        let enclosed_children = raw_boxes
            .iter()
            .enumerate()
            .filter(|(j, other)| {
                *j != i
                    && r.expand(1.0).contains(**other)
                    && ((r.x1 - r.x0) > (other.x1 - other.x0) + 6.0
                        || (r.y1 - r.y0) > (other.y1 - other.y0) + 6.0)
            })
            .count();
        if enclosed_children < 2 {
            leaf_boxes.push(*r);
        }
    }

    // Match enclosed text spans to each box.
    #[derive(Clone)]
    struct CandNode {
        bbox: Rect,
        label: String,
        span_idxs: Vec<usize>,
    }

    let mut cand_nodes: Vec<CandNode> = Vec::new();
    for bbox in leaf_boxes {
        let mut inside: Vec<(usize, &TextItem)> = spans
            .iter()
            .enumerate()
            .filter(|(_, sp)| point_inside_rect(Point::new(sp.cx, sp.cy), bbox, 1.5))
            .collect();
        if inside.is_empty() {
            continue;
        }
        inside.sort_by(|(_, a), (_, b)| {
            let row_a = (a.bbox.y0 / 6.0).round() as i32;
            let row_b = (b.bbox.y0 / 6.0).round() as i32;
            row_a
                .cmp(&row_b)
                .then_with(|| a.bbox.x0.total_cmp(&b.bbox.x0))
        });
        let label = inside
            .iter()
            .map(|(_, s)| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if label.trim().is_empty() {
            continue;
        }
        let span_idxs = inside.into_iter().map(|(idx, _)| idx).collect();
        cand_nodes.push(CandNode {
            bbox,
            label,
            span_idxs,
        });
    }

    if cand_nodes.len() < 2 {
        return Vec::new();
    }

    // Sort candidate nodes deterministically (top-to-bottom, left-to-right).
    cand_nodes.sort_by(|a, b| {
        let ra = (a.bbox.y0 / 10.0).round() as i32;
        let rb = (b.bbox.y0 / 10.0).round() as i32;
        ra.cmp(&rb).then_with(|| a.bbox.x0.total_cmp(&b.bbox.x0))
    });

    let mut enclosed_spans: HashSet<usize> = HashSet::new();
    for cn in &cand_nodes {
        for &si in &cn.span_idxs {
            enclosed_spans.insert(si);
        }
    }

    // 2. Connector polylines outside charts and not arrowhead barbs.
    let connector_inputs: Vec<OpenPolyline> = collector
        .polylines
        .iter()
        .filter(|p| {
            !p.is_arrow_barb
                && p.pts.len() >= 2
                && claimed_chart_rects
                    .iter()
                    .all(|cr| !p.bbox.intersect(*cr).is_valid())
        })
        .cloned()
        .collect();
    let connectors = chain_polylines(&connector_inputs, 1.5);

    #[derive(Clone)]
    struct RawEdge {
        from_idx: usize,
        to_idx: usize,
        directed: bool,
        pts: Vec<Point>,
        bbox: Rect,
        label: Option<String>,
        label_bbox: Option<Rect>,
    }

    let mut raw_edges: Vec<RawEdge> = Vec::new();
    for conn in &connectors {
        let p_start = Point::new(conn.pts[0][0], conn.pts[0][1]);
        let end_arr = conn.pts[conn.pts.len() - 1];
        let p_end = Point::new(end_arr[0], end_arr[1]);
        if pt_dist(p_start, p_end) < 6.0 {
            continue;
        }

        let start_node = cand_nodes
            .iter()
            .enumerate()
            .filter_map(|(ni, n)| {
                let d = point_to_rect_perimeter_dist(p_start, n.bbox);
                (d <= 12.0).then_some((d, ni))
            })
            .min_by(|(da, _), (db, _)| da.total_cmp(db))
            .map(|(_, ni)| ni);

        let end_node = cand_nodes
            .iter()
            .enumerate()
            .filter_map(|(ni, n)| {
                let d = point_to_rect_perimeter_dist(p_end, n.bbox);
                (d <= 12.0).then_some((d, ni))
            })
            .min_by(|(da, _), (db, _)| da.total_cmp(db))
            .map(|(_, ni)| ni);

        let (Some(na), Some(nb)) = (start_node, end_node) else {
            continue;
        };
        if na == nb {
            continue;
        }

        let pts: Vec<Point> = conn.pts.iter().map(|&[x, y]| Point::new(x, y)).collect();
        let arrow_at_end = has_arrowhead_near(
            p_end,
            pts.get(pts.len().saturating_sub(2)).copied(),
            &collector.arrowheads,
        );
        let arrow_at_start =
            has_arrowhead_near(p_start, pts.get(1).copied(), &collector.arrowheads);

        let (from_idx, to_idx, directed) = if arrow_at_end && !arrow_at_start {
            (na, nb, true)
        } else if arrow_at_start && !arrow_at_end {
            (nb, na, true)
        } else if arrow_at_end && arrow_at_start {
            (na, nb, true)
        } else {
            let ca = &cand_nodes[na].bbox;
            let cb = &cand_nodes[nb].bbox;
            let cya = 0.5 * (ca.y0 + ca.y1);
            let cyb = 0.5 * (cb.y0 + cb.y1);
            let cxa = 0.5 * (ca.x0 + ca.x1);
            let cxb = 0.5 * (cb.x0 + cb.x1);
            if cya + 6.0 < cyb || ((cya - cyb).abs() <= 6.0 && cxa <= cxb) {
                (na, nb, false)
            } else {
                (nb, na, false)
            }
        };

        // Deduplicate parallel identical edges.
        if raw_edges
            .iter()
            .any(|e| e.from_idx == from_idx && e.to_idx == to_idx)
        {
            continue;
        }

        raw_edges.push(RawEdge {
            from_idx,
            to_idx,
            directed,
            pts,
            bbox: conn.bbox,
            label: None,
            label_bbox: None,
        });
    }

    if raw_edges.is_empty() {
        return Vec::new();
    }

    // Attach nearby un-enclosed edge caption spans (`<= 14 pt` from connector segments).
    let mut edge_spans: Vec<Vec<(usize, &TextItem)>> = vec![Vec::new(); raw_edges.len()];
    for (si, sp) in spans.iter().enumerate() {
        if enclosed_spans.contains(&si) {
            continue;
        }
        if claimed_chart_rects
            .iter()
            .any(|cr| !sp.bbox.intersect(*cr).is_empty())
        {
            continue;
        }
        let center = Point::new(sp.cx, sp.cy);
        let mut best_edge: Option<(f32, usize)> = None;
        for (ei, edge) in raw_edges.iter().enumerate() {
            let d = polyline_dist_to_rect(&edge.pts, sp.bbox, center);
            if d <= 14.0 && best_edge.is_none_or(|(bd, _)| d < bd) {
                best_edge = Some((d, ei));
            }
        }
        if let Some((_, ei)) = best_edge {
            edge_spans[ei].push((si, sp));
        }
    }

    for (ei, mut attached) in edge_spans.into_iter().enumerate() {
        if attached.is_empty() {
            continue;
        }
        attached.sort_by(|(_, a), (_, b)| {
            a.bbox
                .y0
                .total_cmp(&b.bbox.y0)
                .then_with(|| a.bbox.x0.total_cmp(&b.bbox.x0))
        });
        let mut lb = Rect::EMPTY;
        let mut words = Vec::with_capacity(attached.len());
        for (_, sp) in attached {
            lb = lb.union(sp.bbox);
            words.push(sp.text.as_str());
        }
        let text = words.join(" ");
        if !text.is_empty() {
            raw_edges[ei].label = Some(text);
            raw_edges[ei].label_bbox = Some(lb);
        }
    }

    // Group connected components of nodes with >= 1 edge into `ExtractedDiagram`s.
    let n_nodes = cand_nodes.len();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n_nodes];
    for e in &raw_edges {
        adj[e.from_idx].push(e.to_idx);
        adj[e.to_idx].push(e.from_idx);
    }

    let mut visited = vec![false; n_nodes];
    let mut diagrams = Vec::new();
    let mut next_node_seq = 1usize;

    for start in 0..n_nodes {
        if visited[start] || adj[start].is_empty() {
            continue;
        }
        let mut comp_nodes = Vec::new();
        let mut queue = vec![start];
        visited[start] = true;
        let mut head = 0usize;
        while head < queue.len() {
            let u = queue[head];
            head += 1;
            comp_nodes.push(u);
            for &v in &adj[u] {
                if !visited[v] {
                    visited[v] = true;
                    queue.push(v);
                }
            }
        }
        comp_nodes.sort_unstable();

        let mut old_to_id = vec![String::new(); n_nodes];
        let mut nodes_out = Vec::with_capacity(comp_nodes.len());
        let mut diag_bbox = Rect::EMPTY;

        for &ni in &comp_nodes {
            let nid = format!("p{page_num}.n{next_node_seq}");
            next_node_seq += 1;
            old_to_id[ni].clone_from(&nid);
            diag_bbox = diag_bbox.union(cand_nodes[ni].bbox);
            nodes_out.push(DiagramNode {
                id: nid,
                label: cand_nodes[ni].label.clone(),
                bbox: rect_array(cand_nodes[ni].bbox),
            });
        }

        let comp_set: HashSet<usize> = comp_nodes.into_iter().collect();
        let mut edges_out = Vec::new();
        for e in &raw_edges {
            if comp_set.contains(&e.from_idx) && comp_set.contains(&e.to_idx) {
                diag_bbox = diag_bbox.union(e.bbox);
                if let Some(lb) = e.label_bbox {
                    diag_bbox = diag_bbox.union(lb);
                }
                edges_out.push(DiagramEdge {
                    from: old_to_id[e.from_idx].clone(),
                    to: old_to_id[e.to_idx].clone(),
                    label: e.label.clone(),
                    directed: e.directed,
                });
            }
        }

        if nodes_out.len() >= 2 && !edges_out.is_empty() {
            let diag_id = format!("p{page_num}.diagram{}", diagrams.len() + 1);
            diagrams.push(ExtractedDiagram {
                id: diag_id,
                page: page_num,
                bbox: rect_array(diag_bbox),
                nodes: nodes_out,
                edges: edges_out,
            });
        }
    }

    diagrams
}

fn has_arrowhead_near(endpoint: Point, prev_pt: Option<Point>, arrowheads: &[ArrowHead]) -> bool {
    let shaft_dir = prev_pt.and_then(|prev| {
        let dx = endpoint.x - prev.x;
        let dy = endpoint.y - prev.y;
        let len = dx.hypot(dy);
        (len > 1e-3).then(|| Point::new(dx / len, dy / len))
    });

    arrowheads.iter().any(|ah| {
        let d_tip = pt_dist(endpoint, ah.tip);
        let in_box = point_inside_rect(endpoint, ah.bbox, 6.0);
        if d_tip > 14.0 && !in_box {
            return false;
        }
        match shaft_dir {
            Some(sd) => sd.x * ah.dir.x + sd.y * ah.dir.y >= 0.15,
            None => true,
        }
    })
}

fn polyline_dist_to_rect(pts: &[Point], r: Rect, center: Point) -> f32 {
    let mut best = f32::INFINITY;
    for win in pts.windows(2) {
        let d_center = point_to_segment_dist(center, win[0], win[1]);
        let d_left = point_to_segment_dist(Point::new(r.x0, center.y), win[0], win[1]);
        let d_right = point_to_segment_dist(Point::new(r.x1, center.y), win[0], win[1]);
        best = best.min(d_center).min(d_left).min(d_right);
    }
    best
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{doc_bytes, page_doc, stream};

    const HELV_RESOURCES: &str =
        "/Font<</F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>>>";

    #[test]
    fn decompiles_grouped_two_series_vertical_bar_chart_with_legend() {
        // MediaBox [0 0 612 792] (`y_page = 792 - y_pdf`):
        // Value mapping: y_pdf = 200 + 2.0 * value  (so value 0 -> y_pdf=200, value 150 -> y_pdf=500).
        // Ticks: $0 (y_pdf=200), $50 (y_pdf=300), $100 (y_pdf=400), $150 (y_pdf=500).
        // Note: place tick baselines so glyph centers are strictly linear too (`y_pdf - 3.5`).
        let mut s = String::new();
        // Chart title at y_pdf = 545:
        s += "0 0 0 rg BT /F1 14 Tf 180 545 Td (Quarterly Revenue vs Cost) Tj ET\n";
        // Y-axis ticks at x=55:
        for (val, label) in [(0, "$0"), (50, "$50"), (100, "$100"), (150, "$150")] {
            let y_pdf = 200.0 + 2.0 * (val as f32) - 3.5;
            s += &format!("0 0 0 rg BT /F1 10 Tf 52 {y_pdf:.1} Td ({label}) Tj ET\n");
        }
        // Axes lines: vertical at x=95 (200..510), horizontal baseline at y_pdf=200 (95..480)
        s += "0.2 0.2 0.2 RG 1 w 95 200 m 95 510 l S 95 200 m 480 200 l S\n";

        // Legend swatches in top-right:
        // Revenue = #1a66cc (0.102, 0.4, 0.8), Cost = #e64d1a (0.902, 0.302, 0.102)
        s += "0.102 0.4 0.8 rg 400 505 12 10 re f\n";
        s += "0 0 0 rg BT /F1 10 Tf 418 506 Td (Revenue) Tj ET\n";
        s += "0.902 0.302 0.102 rg 400 485 12 10 re f\n";
        s += "0 0 0 rg BT /F1 10 Tf 418 486 Td (Cost) Tj ET\n";

        // 4 Categories (Q1..Q4) with exact Revenue and Cost bars:
        let data = [
            ("Q1", 140.0_f32, 85.0_f32, 60.0_f32),
            ("Q2", 225.0, 120.5, 72.25),
            ("Q3", 310.0, 98.0, 68.5),
            ("Q4", 395.0, 145.0, 80.0),
        ];
        for (cat, cx, rev, cost) in data {
            let rev_h = 2.0 * rev;
            let cost_h = 2.0 * cost;
            s += &format!("0.102 0.4 0.8 rg {:.2} 200 18 {rev_h:.2} re f\n", cx - 19.0);
            s += &format!(
                "0.902 0.302 0.102 rg {:.2} 200 18 {cost_h:.2} re f\n",
                cx + 1.0
            );
            s += &format!(
                "0 0 0 rg BT /F1 10 Tf {:.1} 182 Td ({cat}) Tj ET\n",
                cx - 7.0
            );
        }

        let doc = page_doc(s.as_bytes(), HELV_RESOURCES, &[]);
        let report = decompile_charts(&doc, None).unwrap();
        assert_eq!(report.page_count, 1);
        assert_eq!(report.charts.len(), 1);
        assert!(report.diagrams.is_empty());

        let chart = &report.charts[0];
        assert_eq!(chart.id, "p1.chart1");
        assert_eq!(chart.kind, "bar");
        assert_eq!(chart.title.as_deref(), Some("Quarterly Revenue vs Cost"));
        assert!(chart.axis_r_squared >= 0.9999);
        assert_eq!(chart.series.len(), 2);

        let rev_s = &chart.series[0];
        assert_eq!(rev_s.name, "Revenue");
        assert_eq!(rev_s.color, "#1a66cc");
        let rev_pts: Vec<(&str, f64)> = rev_s
            .points
            .iter()
            .map(|p| (p.label.as_str(), p.value))
            .collect();
        assert_eq!(
            rev_pts,
            vec![("Q1", 85.0), ("Q2", 120.5), ("Q3", 98.0), ("Q4", 145.0)]
        );

        let cost_s = &chart.series[1];
        assert_eq!(cost_s.name, "Cost");
        assert_eq!(cost_s.color, "#e64d1a");
        let cost_pts: Vec<(&str, f64)> = cost_s
            .points
            .iter()
            .map(|p| (p.label.as_str(), p.value))
            .collect();
        assert_eq!(
            cost_pts,
            vec![("Q1", 60.0), ("Q2", 72.25), ("Q3", 68.5), ("Q4", 80.0)]
        );
    }

    #[test]
    fn decompiles_horizontal_bar_chart_with_fallback_series_name() {
        // Horizontal value axis: x_pdf = 140 + 2.5 * value (0 -> 140, 40 -> 240, 80 -> 340, 120 -> 440)
        // Center each tick label over its exact X tick mark just as real chart renderers do.
        let mut s = String::new();
        for (val, half_w) in [(0, 6.5_f32), (40, 9.0), (80, 9.0), (120, 11.5)] {
            let x_pdf = 140.0 + 2.5 * (val as f32) - half_w;
            s += &format!("0 0 0 rg BT /F1 9 Tf {x_pdf:.1} 360 Td ({val}%) Tj ET\n");
        }
        s += "0.3 0.3 0.3 RG 1 w 140 380 m 140 530 l S 140 380 m 450 380 l S\n";

        let rows = [
            ("North", 495.0_f32, 92.0_f32),
            ("South", 450.0, 64.0),
            ("West", 405.0, 110.0),
        ];
        for (cat, y_pdf, val) in rows {
            let w = 2.5 * val;
            s += &format!("0.2 0.7 0.4 rg 140 {y_pdf:.1} {w:.2} 22 re f\n");
            s += &format!(
                "0 0 0 rg BT /F1 10 Tf 88 {:.1} Td ({cat}) Tj ET\n",
                y_pdf + 6.0
            );
        }

        let doc = page_doc(s.as_bytes(), HELV_RESOURCES, &[]);
        let report = decompile_charts(&doc, None).unwrap();
        assert_eq!(report.charts.len(), 1);
        let chart = &report.charts[0];
        assert_eq!(chart.kind, "horizontal_bar");
        assert_eq!(chart.series.len(), 1);
        assert_eq!(chart.series[0].name, "series_1");

        // Top-to-bottom in MuPDF page space: North (y_pdf=495 -> y_page=297), South, West.
        let pts: Vec<(&str, f64)> = chart.series[0]
            .points
            .iter()
            .map(|p| (p.label.as_str(), p.value))
            .collect();
        for (lbl, got) in &pts {
            let want = match *lbl {
                "North" => 92.0,
                "South" => 64.0,
                "West" => 110.0,
                other => panic!("unexpected label {other}"),
            };
            // Tick labels with differing widths ("0%" vs "120%") shift cx slightly (<1.5 value units);
            // verify tight linear calibration.
            assert!((got - want).abs() <= 1.2, "{lbl}: got {got}, want {want}");
        }
    }

    #[test]
    fn decompiles_multi_series_line_chart_including_chained_segments() {
        // Y-axis: y_pdf = 250 + 2.0 * value (0..100 -> 250..450).
        // Use fixed-width 3-char left-padded or centered ticks so tick centers align cleanly.
        let mut s = String::new();
        s += "0 0 0 rg BT /F1 13 Tf 200 485 Td (Monthly Active Trend) Tj ET\n";
        for (val, x_off) in [(0, 66.0), (25, 60.0), (50, 60.0), (75, 60.0), (100, 54.0)] {
            let y_pdf = 250.0 + 2.0 * (val as f32) - 3.5;
            s += &format!("0 0 0 rg BT /F1 10 Tf {x_off:.1} {y_pdf:.1} Td ({val}) Tj ET\n");
        }
        s += "0.2 0.2 0.2 RG 1 w 90 250 m 90 460 l S 90 250 m 450 250 l S\n";

        // X category labels: Jan (140), Feb (230), Mar (320), Apr (410)
        for (cat, cx) in [
            ("Jan", 140.0_f32),
            ("Feb", 230.0),
            ("Mar", 320.0),
            ("Apr", 410.0),
        ] {
            s += &format!(
                "0 0 0 rg BT /F1 10 Tf {:.1} 232 Td ({cat}) Tj ET\n",
                cx - 8.0
            );
        }

        // Legend using stroked line swatches:
        s += "0.1 0.5 0.9 RG 2 w 360 465 m 380 465 l S\n";
        s += "0 0 0 rg BT /F1 9 Tf 386 462 Td (Organic) Tj ET\n";
        s += "0.85 0.2 0.2 RG 2 w 360 448 m 380 448 l S\n";
        s += "0 0 0 rg BT /F1 9 Tf 386 445 Td (Paid) Tj ET\n";

        // Series 1 ("Organic"): single multi-vertex polyline (values: 20, 45, 40, 85)
        s += "0.1 0.5 0.9 RG 2 w 140 290 m 230 340 l 320 330 l 410 420 l S\n";

        // Series 2 ("Paid"): drawn as 3 SEPARATE 2-point stroked segments sharing endpoints!
        // (values: 15, 30, 55, 50 -> y_pdf: 280, 310, 360, 350)
        s += "0.85 0.2 0.2 RG 2 w 140 280 m 230 310 l S\n";
        s += "0.85 0.2 0.2 RG 2 w 230 310 m 320 360 l S\n";
        s += "0.85 0.2 0.2 RG 2 w 320 360 m 410 350 l S\n";

        let doc = page_doc(s.as_bytes(), HELV_RESOURCES, &[]);
        let report = decompile_charts(&doc, None).unwrap();
        assert_eq!(report.charts.len(), 1);
        let chart = &report.charts[0];
        assert_eq!(chart.kind, "line");
        assert_eq!(chart.title.as_deref(), Some("Monthly Active Trend"));
        assert!(chart.axis_r_squared >= 0.9999);
        assert_eq!(chart.series.len(), 2);

        let org = chart
            .series
            .iter()
            .find(|sr| sr.name == "Organic")
            .expect("missing Organic series");
        let org_vals: Vec<(&str, f64)> = org
            .points
            .iter()
            .map(|p| (p.label.as_str(), p.value))
            .collect();
        assert_eq!(
            org_vals,
            vec![("Jan", 20.0), ("Feb", 45.0), ("Mar", 40.0), ("Apr", 85.0)]
        );

        let paid = chart
            .series
            .iter()
            .find(|sr| sr.name == "Paid")
            .expect("missing Paid series");
        let paid_vals: Vec<(&str, f64)> = paid
            .points
            .iter()
            .map(|p| (p.label.as_str(), p.value))
            .collect();
        assert_eq!(
            paid_vals,
            vec![("Jan", 15.0), ("Feb", 30.0), ("Mar", 55.0), ("Apr", 50.0)]
        );
    }

    #[test]
    fn decompiles_four_node_directed_flowchart_with_edge_labels_and_outer_frame() {
        // 4-node directed flowchart inside an outer decorative frame box [40, 220, 520, 560]:
        //   n1 ("Start Order") at [210, 680..716] -> n2 ("Validate Credit") at [210, 580..616]
        //   n2 --("Approved")--> n3 ("Fulfill Shipment") at [90, 460..496]
        //   n2 --("Declined")--> n4 ("Notify Customer") at [330, 460..496]
        // Using both filled-triangle arrowheads AND stroked V-chevron arrowheads!
        let mut s = String::new();
        // Outer decorative container frame (must be ignored as a node because it encloses >=2 nodes):
        s += "0.8 0.8 0.8 RG 0.8 w 60 430 430 310 re S\n";

        // Node 1: [210, 680, 340, 714] (y_pdf = 680..714 -> top in page space)
        s += "0.94 0.96 0.99 rg 0.2 0.3 0.5 RG 1.2 w 210 680 130 34 re B\n";
        s += "0 0 0 rg BT /F1 11 Tf 242 693 Td (Start Order) Tj ET\n";

        // Node 2: [210, 575, 340, 609]
        s += "0.94 0.96 0.99 rg 0.2 0.3 0.5 RG 1.2 w 210 575 130 34 re B\n";
        s += "0 0 0 rg BT /F1 11 Tf 234 588 Td (Validate Credit) Tj ET\n";

        // Node 3 (left branch): [85, 455, 225, 489]
        s += "0.2 0.3 0.5 RG 1.2 w 85 455 140 34 re S\n";
        s += "0 0 0 rg BT /F1 11 Tf 112 468 Td (Fulfill Shipment) Tj ET\n";

        // Node 4 (right branch): [325, 455, 465, 489]
        s += "0.2 0.3 0.5 RG 1.2 w 325 455 140 34 re S\n";
        s += "0 0 0 rg BT /F1 11 Tf 352 468 Td (Notify Customer) Tj ET\n";

        // Edge 1: n1 -> n2 (vertical line from (275, 680) down to (275, 611) + filled triangle arrowhead)
        s += "0.1 0.1 0.1 RG 1.2 w 275 680 m 275 615 l S\n";
        s += "0.1 0.1 0.1 rg 271 617 m 279 617 l 275 609 l h f\n";

        // Edge 2: n2 -> n3 (elbow polyline from left of n2 (210, 592) -> (155, 592) -> (155, 491) + V-chevron)
        s += "0.1 0.1 0.1 RG 1.2 w 210 592 m 155 592 l 155 491 l S\n";
        s += "0.1 0.1 0.1 RG 1.2 w 151 498 m 155 490 l 159 498 l S\n";
        s += "0 0 0 rg BT /F1 9 Tf 158 545 Td (Approved) Tj ET\n";

        // Edge 3: n2 -> n4 (drawn backwards from n4 UP to n2 in path order, WITH arrowhead at n4
        // to prove arrowhead overrides path drawing order!)
        s += "0.1 0.1 0.1 RG 1.2 w 395 491 m 395 592 l 340 592 l S\n";
        s += "0.1 0.1 0.1 rg 391 498 m 399 498 l 395 490 l h f\n";
        s += "0 0 0 rg BT /F1 9 Tf 400 545 Td (Declined) Tj ET\n";

        let doc = page_doc(s.as_bytes(), HELV_RESOURCES, &[]);
        let report = decompile_charts(&doc, None).unwrap();
        assert!(report.charts.is_empty());
        assert_eq!(report.diagrams.len(), 1);

        let diag = &report.diagrams[0];
        assert_eq!(diag.id, "p1.diagram1");
        assert_eq!(diag.page, 1);
        assert_eq!(diag.nodes.len(), 4);

        let node_labels: Vec<(&str, &str)> = diag
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.label.as_str()))
            .collect();
        assert_eq!(
            node_labels,
            vec![
                ("p1.n1", "Start Order"),
                ("p1.n2", "Validate Credit"),
                ("p1.n3", "Fulfill Shipment"),
                ("p1.n4", "Notify Customer"),
            ]
        );

        assert_eq!(diag.edges.len(), 3);
        let find_edge = |from: &str, to: &str| {
            diag.edges
                .iter()
                .find(|e| e.from == from && e.to == to)
                .unwrap_or_else(|| panic!("missing edge {from} -> {to} in {:?}", diag.edges))
        };

        let e12 = find_edge("p1.n1", "p1.n2");
        assert!(e12.directed);
        assert_eq!(e12.label, None);

        let e23 = find_edge("p1.n2", "p1.n3");
        assert!(e23.directed);
        assert_eq!(e23.label.as_deref(), Some("Approved"));

        let e24 = find_edge("p1.n2", "p1.n4");
        assert!(e24.directed);
        assert_eq!(e24.label.as_deref(), Some("Declined"));
    }

    #[test]
    fn page_filter_selects_requested_pages_only() {
        let c1 = stream("", b"0 0 0 rg BT /F1 12 Tf 72 700 Td (Cover Page) Tj ET");
        let mut s2 = String::new();
        for val in [0, 10, 20, 30] {
            let y_pdf = 200.0 + 5.0 * (val as f32) - 3.5;
            s2 += &format!("0 0 0 rg BT /F1 10 Tf 50 {y_pdf:.1} Td ({val}) Tj ET\n");
        }
        s2 += "0.2 0.5 0.8 rg 110 200 24 75 re f 160 200 24 125 re f\n";
        s2 += "0 0 0 rg BT /F1 10 Tf 115 182 Td (A) Tj ET\n";
        s2 += "0 0 0 rg BT /F1 10 Tf 165 182 Td (B) Tj ET\n";
        let c2 = stream("", s2.as_bytes());

        let objs = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 7 0 R>>>>/Contents 5 0 R>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 7 0 R>>>>/Contents 6 0 R>>".to_vec(),
            c1,
            c2,
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>".to_vec(),
        ];
        let doc = doc_bytes(&objs);

        let r1 = decompile_charts(&doc, Some("1")).unwrap();
        assert_eq!(r1.page_count, 2);
        assert!(r1.charts.is_empty());

        let r2 = decompile_charts(&doc, Some("2")).unwrap();
        assert_eq!(r2.charts.len(), 1);
        assert_eq!(r2.charts[0].page, 2);
        assert_eq!(r2.charts[0].id, "p2.chart1");
        assert_eq!(r2.charts[0].series[0].points[0].value, 15.0);
        assert_eq!(r2.charts[0].series[0].points[1].value, 25.0);
    }

    #[test]
    fn decompiles_diverging_positive_and_negative_bars_with_k_suffix_ticks() {
        // Y-axis in thousands (`-50k`, `0`, `50k`, `100k`):
        // Zero baseline at y_pdf = 350; scale = 1.0 pt per 1,000 (`0.001` pt per unit):
        // -50k -> y_pdf = 300, 0 -> y_pdf = 350, 50k -> y_pdf = 400, 100k -> y_pdf = 450.
        let mut s = String::new();
        for (y_val, label) in [
            (-50.0_f32, "-50k"),
            (0.0, "0"),
            (50.0, "50k"),
            (100.0, "100k"),
        ] {
            let y_pdf = 350.0 + y_val - 3.5;
            s += &format!("0 0 0 rg BT /F1 10 Tf 48 {y_pdf:.1} Td ({label}) Tj ET\n");
        }
        // Vertical axis (95, 280..470) + zero baseline stroke at y_pdf = 350 (95..380)
        s += "0.2 0.2 0.2 RG 1 w 95 280 m 95 470 l S 95 350 m 380 350 l S\n";

        // Three bars: Alpha = +75,000 (350..425), Beta = -32,500 (317.5..350), Gamma = +40,000 (350..390)
        s += "0.15 0.45 0.85 rg 130 350 28 75 re f\n";
        s += "0.15 0.45 0.85 rg 215 317.5 28 32.5 re f\n";
        s += "0.15 0.45 0.85 rg 300 350 28 40 re f\n";

        for (cat, x) in [("Alpha", 131.0), ("Beta", 219.0), ("Gamma", 296.0)] {
            s += &format!("0 0 0 rg BT /F1 10 Tf {x:.1} 265 Td ({cat}) Tj ET\n");
        }

        let doc = page_doc(s.as_bytes(), HELV_RESOURCES, &[]);
        let report = decompile_charts(&doc, None).unwrap();
        assert_eq!(report.charts.len(), 1);
        let pts: Vec<(&str, f64)> = report.charts[0].series[0]
            .points
            .iter()
            .map(|p| (p.label.as_str(), p.value))
            .collect();
        assert_eq!(
            pts,
            vec![
                ("Alpha", 75_000.0),
                ("Beta", -32_500.0),
                ("Gamma", 40_000.0)
            ]
        );
    }

    #[test]
    fn decompiles_numeric_xy_scatter_markers() {
        // Y-axis: 0..100 -> y_pdf = 250 + 2.0 * y
        // Numeric X-axis: 2020, 2021, 2022, 2023 -> x_pdf = 140 + 80 * (year - 2020)
        let mut s = String::new();
        for val in [0, 50, 100] {
            let y_pdf = 250.0 + 2.0 * (val as f32) - 3.5;
            s += &format!("0 0 0 rg BT /F1 10 Tf 55 {y_pdf:.1} Td ({val}) Tj ET\n");
        }
        s += "0.2 0.2 0.2 RG 1 w 90 250 m 90 460 l S 90 250 m 420 250 l S\n";
        for (i, yr) in [2020, 2021, 2022, 2023].iter().enumerate() {
            let cx = 140.0 + 80.0 * (i as f32);
            s += &format!(
                "0 0 0 rg BT /F1 10 Tf {:.1} 232 Td ({yr}) Tj ET\n",
                cx - 11.0
            );
        }

        // 4 small 6x6 filled scatter markers centered at (140, 290->20), (220, 350->50), (300, 390->70), (380, 430->90)
        for (cx, val) in [
            (140.0_f32, 20.0_f32),
            (220.0, 50.0),
            (300.0, 70.0),
            (380.0, 90.0),
        ] {
            let cy_pdf = 250.0 + 2.0 * val;
            s += &format!(
                "0.6 0.2 0.8 rg {:.1} {:.1} 6 6 re f\n",
                cx - 3.0,
                cy_pdf - 3.0
            );
        }

        let doc = page_doc(s.as_bytes(), HELV_RESOURCES, &[]);
        let report = decompile_charts(&doc, None).unwrap();
        assert_eq!(report.charts.len(), 1);
        let chart = &report.charts[0];
        assert_eq!(chart.kind, "line");
        assert_eq!(chart.series.len(), 1);
        let pts = &chart.series[0].points;
        assert_eq!(pts.len(), 4);
        let vals: Vec<f64> = pts.iter().map(|p| p.value).collect();
        assert_eq!(vals, vec![20.0, 50.0, 70.0, 90.0]);
        for (pt, want_yr) in pts.iter().zip([2020.0, 2021.0, 2022.0, 2023.0]) {
            let xv = pt.x_value.expect("expected calibrated numeric x_value");
            assert!((xv - want_yr).abs() <= 0.05, "got {xv}, want {want_yr}");
        }
    }
}
