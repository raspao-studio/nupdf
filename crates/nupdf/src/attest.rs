// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Perceptual-vs-stream attestation: WYSIWYG visibility truth, vector markup
//! recovery, `/ActualText` spoofing detection, and blind-spot auto-crops.
//!
//! Structured text extraction reads what a PDF content stream *claims* to
//! draw; human readers see what the rasterizer *actually* paints after
//! z-order compositing, clipping, optional-content filtering, alpha blending,
//! and background contrast. This module bridges the two by comparing full
//! page renders against text-free background renders (`NoTextDevice`),
//! reconciling clipped, hidden-layer, and `IGNORE_ACTUALTEXT` extraction
//! passes, and recovering vector strikethroughs, underlines, enclosing boxes,
//! and unexplained graphic/unmapped-glyph blind spots.

use serde::Serialize;

use crate::colorspace::ColorSpace;
use crate::content::Store;
use crate::device::{ColorParams, DefaultColorSpaces, Device, Metatext};
use crate::diag;
use crate::document::Document;
use crate::draw;
use crate::extract;
use crate::function::Function;
use crate::geometry::{IRect, Matrix, Point, Rect};
use crate::image::Image;
use crate::object::Object;
use crate::page_run;
use crate::path::{self, Path, StrokeState, Walker};
use crate::shade::Shade;
use crate::stext::{self, Block, StextChar, StextPage, char_flags, rune_char};
use crate::text::Text;

/// Hard cap on vector path segments inspected per page so pathological files
/// remain strictly bounded in time and memory.
const MAX_SEGMENTS: usize = 50_000;

/// Hard cap on distinct graphic primitives retained for blind-spot clustering.
const MAX_PRIMS: usize = 10_000;

/// Hard cap on nested clip/group/mask/tile containers in [`VectorCollector`].
const MAX_CLIP_DEPTH: usize = 512;

/// Configuration for perceptual-vs-stream page attestation.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AttestOptions {
    /// Rasterization resolution used to measure pixel contrast and render crops.
    pub dpi: f32,
    /// Maximum pixels (`width * height`) permitted for a single page render.
    pub max_pixels: u64,
    /// Minimum max-channel RGB delta (`0..=255`) vs the text-free background
    /// render inside a span's bounding box for the span to count as visible.
    pub contrast_threshold: u8,
}

impl Default for AttestOptions {
    fn default() -> Self {
        Self {
            dpi: 72.0,
            max_pixels: 64_000_000,
            contrast_threshold: 12,
        }
    }
}

/// Perceptual visibility classification of a stream text span.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Painted in the stream and produces human-visible pixel contrast.
    Visible,
    /// Neither filled nor stroked (e.g. render mode `3 Tr`) or drawn at zero alpha.
    InvisibleMode,
    /// Marked filled/stroked in the stream (or in a hidden layer), yet leaves
    /// pixel contrast below [`AttestOptions::contrast_threshold`] against the
    /// text-free background (white-on-white, z-order occlusion, micro-text, etc.).
    ZeroContrastOrOccluded,
    /// Clipped away by an active clip path or positioned outside the page box.
    Clipped,
}

/// Attestation record for a single structured-text span on a page.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AttestedSpan {
    /// Stable content ID (`p1.b2.l1.s1` for standard spans, `p1.hidden1` for
    /// spans dropped by default clipping, hidden layers, or empty `ActualText`).
    pub id: String,
    /// 1-based page number.
    pub page: i32,
    /// Bounding box `[x0, y0, x1, y1]` in MuPDF page space (points, origin top-left).
    pub bbox: [f32; 4],
    /// Extracted text (honoring `/ActualText` when present, as standard extraction does).
    pub text: String,
    /// Underlying glyph text when `IGNORE_ACTUALTEXT` differs from `text`,
    /// exposing `/ActualText` replacement or erasure spoofing.
    pub glyph_text: Option<String>,
    /// Perceptual visibility classification.
    pub visibility: Visibility,
    /// Maximum single-channel RGB delta (`0..=255`) between the full page render
    /// and the text-free background render inside `bbox`.
    pub max_contrast: u8,
    /// True when flagged with `STRIKEOUT` or crossed horizontally by a vector
    /// stroke or thin filled bar through `25%..=75%` of height across `>=45%` width.
    pub struck_through: bool,
    /// True when flagged with `UNDERLINE` or underscored horizontally near
    /// `70%..=115%` of `bbox` height across `>=45%` width.
    pub underlined: bool,
    /// True when enclosed by a vector rectangle or 4-edge frame with reasonable padding.
    pub boxed: bool,
    /// Number of characters mapped to `\u{FFFD}` or flagged `UNICODE_IS_CID` / `UNICODE_IS_GID`.
    pub unmapped_glyphs: usize,
    /// Stable dotted hazard codes (`"visibility.occluded"`, `"visibility.invisible_mode"`,
    /// `"visibility.clipped"`, `"spoof.actual_text"`, `"markup.strikethrough"`,
    /// `"font.unmapped_glyphs"`).
    pub hazards: Vec<String>,
}

/// A page region whose visual information is not faithfully represented as
/// mapped visible text in the stream.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BlindSpot {
    /// Stable blind-spot ID (`p1.spot1`, `p1.spot2`, ...).
    pub id: String,
    /// 1-based page number.
    pub page: i32,
    /// `"vector_graphic"` for unexplained vector/image clusters or
    /// `"unmapped_text"` for spans containing unmapped glyphs.
    pub kind: String,
    /// Bounding box `[x0, y0, x1, y1]` in MuPDF page space.
    pub bbox: [f32; 4],
    /// Count of vector path segment operations contributing to this spot.
    pub path_ops: usize,
    /// Optional PNG crop rendered via [`draw::render_page_banded`] when `crop` is requested.
    #[serde(skip_serializing)]
    pub png: Option<Vec<u8>>,
}

/// Full perceptual-vs-stream attestation report for one PDF page.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PageAttestation {
    /// 1-based page number.
    pub page: i32,
    /// Total spans attested on this page.
    pub span_count: usize,
    /// Count of spans classified as [`Visibility::Visible`].
    pub visible_spans: usize,
    /// Count of spans not classified as [`Visibility::Visible`].
    pub hidden_or_occluded_spans: usize,
    /// Count of spans with `struck_through == true`.
    pub struck_through_spans: usize,
    /// Count of spans where `/ActualText` differed from underlying glyphs.
    pub actual_text_mismatches: usize,
    /// Count of spans with `unmapped_glyphs > 0`.
    pub unmapped_spans: usize,
    /// Per-span attestation records in reading/discovery order.
    pub spans: Vec<AttestedSpan>,
    /// Unexplained visual or unmapped-glyph blind spots.
    pub blind_spots: Vec<BlindSpot>,
}

/// Attests a single 0-based page `index` of `doc`.
pub fn attest_page(
    doc: &Document,
    index: i32,
    opts: &AttestOptions,
    crop: bool,
) -> Result<PageAttestation, String> {
    let page = doc.page(index).map_err(|e| e.0)?;
    if doc.resolve(&page).as_dict().is_none() {
        return Err("page object is not a dictionary".to_string());
    }
    attest_page_with_store(doc, index, &page, opts, crop, Store::default()).0
}

/// Attests every page in `doc` sequentially, sharing a resource [`Store`].
pub fn attest_document(
    doc: &Document,
    opts: &AttestOptions,
    crop: bool,
) -> Result<Vec<PageAttestation>, String> {
    let count = doc.page_count().map_err(|e| e.0)?;
    let mut store = Store::default();
    let mut out = Vec::with_capacity(count.max(0) as usize);
    for index in 0..count {
        let page = doc.page(index).map_err(|e| e.0)?;
        if doc.resolve(&page).as_dict().is_none() {
            return Err("page object is not a dictionary".to_string());
        }
        let (res, next_store) = attest_page_with_store(doc, index, &page, opts, crop, store);
        store = next_store;
        out.push(res?);
    }
    Ok(out)
}

/// Attests the 0-based `indices` across up to `threads` worker threads in
/// deterministic page order.
pub fn attest_pages(
    doc: &Document,
    indices: &[i32],
    opts: &AttestOptions,
    crop: bool,
    threads: usize,
) -> Vec<Result<PageAttestation, String>> {
    let job = |pos: usize,
               doc: &Document,
               page: &Result<Object, String>,
               store: Store|
     -> (Result<PageAttestation, String>, Store) {
        let index = indices.get(pos).copied().unwrap_or(0);
        match page {
            Ok(obj) => attest_page_with_store(doc, index, obj, opts, crop, store),
            Err(e) => (Err(e.clone()), store),
        }
    };
    crate::parallel::run_pages(doc, indices, threads, &job)
}

/// Attests a resolved `page` object at 0-based `index`, reusing and returning `store`.
pub fn attest_page_with_store(
    doc: &Document,
    index: i32,
    page: &Object,
    opts: &AttestOptions,
    crop: bool,
    mut store: Store,
) -> (Result<PageAttestation, String>, Store) {
    if !opts.dpi.is_finite() || opts.dpi <= 0.0 {
        return (
            Err("argument error: dpi must be finite and positive".into()),
            store,
        );
    }
    let page_num = index.saturating_add(1);
    let mediabox = match doc.page_transform(page) {
        Ok((rect, ctm)) => rect.transform(ctm),
        Err(e) => return (Err(e.0), store),
    };

    let scale = opts.dpi / 72.0;
    let render_ctm = Matrix::scale(scale, scale);
    let page_px_bbox = match draw::page_bbox(doc, page, render_ctm) {
        Ok(b) => b,
        Err(e) => return (Err(e), store),
    };
    if page_px_bbox.is_empty() {
        return (Err("page pixel bounding box is empty".into()), store);
    }
    let pixels = (page_px_bbox.width() as u64).saturating_mul(page_px_bbox.height() as u64);
    if pixels > opts.max_pixels {
        let err = format!(
            "limit error: {}x{} pixels is more than {}",
            page_px_bbox.width(),
            page_px_bbox.height(),
            opts.max_pixels
        );
        return (Err(err), store);
    }

    // 1. Standard extraction pass (`CLIP` on, `"View"` layers) so IDs match `nupdf extract`.
    let (std_res, next_store) = stext::extract_page(doc, page, stext::Options::default(), store);
    store = next_store;
    let std_page = match std_res {
        Ok(p) => p,
        Err(e) => return (Err(diag::report::strip(&e).to_string()), store),
    };

    // 2. Supplementary stext passes inside `diag::capture` so warnings are not duplicated:
    //    - `raw_std_page`: `CLIP | IGNORE_ACTUALTEXT` to detect `/ActualText` spoofing on standard spans.
    //    - `all_page`: `ALL_LAYERS` without `CLIP` to surface hidden-layer and clipped spans.
    //    - `raw_all_page`: `ALL_LAYERS | IGNORE_ACTUALTEXT` without `CLIP` to catch glyphs erased by
    //      empty `/ActualText ()` or spoofed inside hidden/clipped regions.
    let ((raw_std_res, all_res, raw_all_res, next_store), _) = diag::capture(|| {
        let (r_std, s1) = stext::extract_page(
            doc,
            page,
            stext::Options {
                flags: stext::flags::CLIP | stext::flags::IGNORE_ACTUALTEXT,
                clip: Rect::INFINITE,
            },
            store,
        );
        let (all, s2) = stext::extract_page(
            doc,
            page,
            stext::Options {
                flags: stext::flags::ALL_LAYERS,
                clip: Rect::INFINITE,
            },
            s1,
        );
        let (r_all, s3) = stext::extract_page(
            doc,
            page,
            stext::Options {
                flags: stext::flags::ALL_LAYERS | stext::flags::IGNORE_ACTUALTEXT,
                clip: Rect::INFINITE,
            },
            s2,
        );
        (r_std, all, r_all, s3)
    });
    store = next_store;

    let raw_std_page = match raw_std_res {
        Ok(p) => p,
        Err(e) => return (Err(diag::report::strip(&e).to_string()), store),
    };
    let all_page = match all_res {
        Ok(p) => p,
        Err(e) => return (Err(diag::report::strip(&e).to_string()), store),
    };
    let raw_all_page = match raw_all_res {
        Ok(p) => p,
        Err(e) => return (Err(diag::report::strip(&e).to_string()), store),
    };

    let std_raw_spans = collect_raw_spans(&std_page, page_num);
    let ignore_std_spans = collect_raw_spans(&raw_std_page, page_num);
    let all_raw_spans = collect_raw_spans(&all_page, page_num);
    let ignore_all_spans = collect_raw_spans(&raw_all_page, page_num);

    let std_glyphs = collect_glyph_keys(&std_raw_spans);
    let all_extra_candidates = extract_unseen_spans(&all_raw_spans, &std_glyphs);

    // Only if `all_page` found extra spans do we run an unclipped `"View"` pass (`flags: 0`)
    // to distinguish clip-dropped spans from hidden-OCG spans deterministically.
    let view_unclipped_glyphs = if all_extra_candidates.is_empty() {
        Vec::new()
    } else {
        let ((unclipped_res, next_store), _) = diag::capture(|| {
            stext::extract_page(
                doc,
                page,
                stext::Options {
                    flags: 0,
                    clip: Rect::INFINITE,
                },
                store,
            )
        });
        store = next_store;
        match unclipped_res {
            Ok(p) => {
                let sp = collect_raw_spans(&p, page_num);
                collect_glyph_keys(&sp)
            }
            Err(_) => Vec::new(),
        }
    };

    // Also recover spans completely erased from both `std_page` and `all_page` by empty
    // `/ActualText ()` constructs (`raw_all_page` sees their glyphs while `all_page` drops them).
    let mut seen_glyphs = std_glyphs;
    seen_glyphs.extend(collect_glyph_keys(&all_extra_candidates));
    let erased_by_actualtext = extract_unseen_spans(&ignore_all_spans, &seen_glyphs);

    // 3. Render `full_pix` (with text) and `bg_pix` (via `NoTextDevice`, without text),
    //    plus collect vector markup and graphic primitives via `VectorCollector`.
    let mut full_pix_init = match draw::Pix::new(page_px_bbox, 3, false) {
        Ok(p) => p,
        Err(e) => return (Err(e), store),
    };
    full_pix_init.clear_with(255);

    let mut bg_pix_init = match draw::Pix::new(page_px_bbox, 3, false) {
        Ok(p) => p,
        Err(e) => return (Err(e), store),
    };
    bg_pix_init.clear_with(255);

    let ((dual_res, vectors, next_store), _) = diag::capture(|| {
        let mut full_dev = draw::DrawDevice::new(Some(doc), full_pix_init);
        full_dev.cc.icc = true;
        let run1 = page_run::run_page(doc, page, &mut full_dev, store, Some("View"), render_ctm);
        let full_out = full_dev.into_pix();
        if let Some(err) = run1.error {
            return (
                Err(diag::report::strip(&err).to_string()),
                VectorCollector::new(Some(doc), mediabox),
                run1.store,
            );
        }

        let mut bg_dev = draw::DrawDevice::new(Some(doc), bg_pix_init);
        bg_dev.cc.icc = true;
        let run2 = {
            let mut no_text = NoTextDevice(&mut bg_dev);
            page_run::run_page(
                doc,
                page,
                &mut no_text,
                run1.store,
                Some("View"),
                render_ctm,
            )
        };
        let bg_out = bg_dev.into_pix();
        if let Some(err) = run2.error {
            return (
                Err(diag::report::strip(&err).to_string()),
                VectorCollector::new(Some(doc), mediabox),
                run2.store,
            );
        }

        let mut vc = VectorCollector::new(Some(doc), mediabox);
        let run3 = page_run::run_page(
            doc,
            page,
            &mut vc,
            run2.store,
            Some("View"),
            Matrix::IDENTITY,
        );
        (Ok((full_out, bg_out)), vc, run3.store)
    });
    store = next_store;

    let (full_pix, bg_pix) = match dual_res {
        Ok(pair) => pair,
        Err(e) => return (Err(e), store),
    };

    // 4. Reconcile `/ActualText` mismatches and evaluate per-span perceptual truth & markup.
    let std_mismatches = match_actual_text(&std_raw_spans, &ignore_std_spans);
    let extra_mismatches = match_actual_text(&all_extra_candidates, &ignore_all_spans);

    let mut spans = Vec::with_capacity(
        std_raw_spans.len() + all_extra_candidates.len() + erased_by_actualtext.len(),
    );
    let mut visible_span_rects: Vec<Rect> = Vec::new();

    for (idx, raw) in std_raw_spans.iter().enumerate() {
        let mismatch = std_mismatches.get(idx).cloned().flatten();
        let attested = build_attested_span(
            raw,
            raw.id.clone(),
            page_num,
            OriginKind::Standard,
            mismatch,
            mediabox,
            render_ctm,
            &full_pix,
            &bg_pix,
            &vectors,
            opts,
        );
        if attested.visibility == Visibility::Visible && raw.bbox.is_valid() {
            visible_span_rects.push(raw.bbox);
        }
        spans.push(attested);
    }

    let mut hidden_seq: usize = 0;
    for (idx, raw) in all_extra_candidates.iter().enumerate() {
        hidden_seq += 1;
        let id = format!("p{page_num}.hidden{hidden_seq}");
        let in_view_unclipped = span_matches_glyphs(raw, &view_unclipped_glyphs);
        let origin = if in_view_unclipped {
            OriginKind::ClippedOut
        } else {
            OriginKind::HiddenLayer
        };
        let mismatch = extra_mismatches.get(idx).cloned().flatten();
        let attested = build_attested_span(
            raw, id, page_num, origin, mismatch, mediabox, render_ctm, &full_pix, &bg_pix,
            &vectors, opts,
        );
        if attested.visibility == Visibility::Visible && raw.bbox.is_valid() {
            visible_span_rects.push(raw.bbox);
        }
        spans.push(attested);
    }

    for raw in &erased_by_actualtext {
        hidden_seq += 1;
        let id = format!("p{page_num}.hidden{hidden_seq}");
        let glyph_str = extract::chars_text(&raw.chars);
        let in_view = span_matches_glyphs(raw, &view_unclipped_glyphs)
            || raw.bbox.intersect(mediabox).is_valid();
        let origin = if !raw.bbox.is_valid() || raw.bbox.intersect(mediabox).is_empty() {
            OriginKind::ClippedOut
        } else if in_view {
            OriginKind::ErasedActualText
        } else {
            OriginKind::HiddenLayer
        };
        let mut empty_span = raw.clone();
        empty_span.text = String::new();
        let attested = build_attested_span(
            &empty_span,
            id,
            page_num,
            origin,
            Some((glyph_str, count_unmapped(&raw.chars))),
            mediabox,
            render_ctm,
            &full_pix,
            &bg_pix,
            &vectors,
            opts,
        );
        if attested.visibility == Visibility::Visible && raw.bbox.is_valid() {
            visible_span_rects.push(raw.bbox);
        }
        spans.push(attested);
    }

    // 5. Discover blind spots (`"vector_graphic"` clusters + `"unmapped_text"` spans)
    //    and optionally render PNG crops via `draw::render_page_banded`.
    let raw_spots = build_blind_spots(
        &vectors,
        &spans,
        &visible_span_rects,
        mediabox,
        render_ctm,
        &full_pix,
        opts.contrast_threshold,
    );

    let mut blind_spots = Vec::with_capacity(raw_spots.len());
    for (i, (kind, bbox_rect, path_ops)) in raw_spots.into_iter().enumerate() {
        let spot_id = format!("p{page_num}.spot{}", i + 1);
        let png = if crop {
            let (bytes, next_store) = render_spot_png(
                doc,
                page,
                bbox_rect,
                mediabox,
                render_ctm,
                page_px_bbox,
                opts.max_pixels,
                store,
            );
            store = next_store;
            bytes
        } else {
            None
        };
        blind_spots.push(BlindSpot {
            id: spot_id,
            page: page_num,
            kind: kind.to_string(),
            bbox: rect_array(bbox_rect),
            path_ops,
            png,
        });
    }

    let span_count = spans.len();
    let visible_spans = spans
        .iter()
        .filter(|s| s.visibility == Visibility::Visible)
        .count();
    let hidden_or_occluded_spans = span_count.saturating_sub(visible_spans);
    let struck_through_spans = spans.iter().filter(|s| s.struck_through).count();
    let actual_text_mismatches = spans.iter().filter(|s| s.glyph_text.is_some()).count();
    let unmapped_spans = spans.iter().filter(|s| s.unmapped_glyphs > 0).count();

    (
        Ok(PageAttestation {
            page: page_num,
            span_count,
            visible_spans,
            hidden_or_occluded_spans,
            struck_through_spans,
            actual_text_mismatches,
            unmapped_spans,
            spans,
            blind_spots,
        }),
        store,
    )
}

// ---------------------------------------------------------------------------
// Internal Device Wrappers (`NoTextDevice` and `VectorCollector`)
// ---------------------------------------------------------------------------

/// Forwards every [`Device`] call to `self.0` except `fill_text`, `stroke_text`,
/// and `ignore_text`, while delegating `clip_text` and `clip_stroke_text` so
/// `pop_clip` calls stay balanced.
struct NoTextDevice<'a>(&'a mut dyn Device);

#[allow(clippy::too_many_arguments)]
impl Device for NoTextDevice<'_> {
    fn fill_path(
        &mut self,
        path: &Path,
        even_odd: bool,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        params: ColorParams,
    ) {
        self.0
            .fill_path(path, even_odd, ctm, cs, color, alpha, params);
    }

    fn stroke_path(
        &mut self,
        path: &Path,
        stroke: &StrokeState,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        params: ColorParams,
    ) {
        self.0
            .stroke_path(path, stroke, ctm, cs, color, alpha, params);
    }

    fn clip_path(&mut self, path: &Path, even_odd: bool, ctm: Matrix, scissor: Rect) {
        self.0.clip_path(path, even_odd, ctm, scissor);
    }

    fn clip_stroke_path(&mut self, path: &Path, stroke: &StrokeState, ctm: Matrix, scissor: Rect) {
        self.0.clip_stroke_path(path, stroke, ctm, scissor);
    }

    fn fill_shade(&mut self, shade: &Shade, ctm: Matrix, alpha: f32, params: ColorParams) {
        self.0.fill_shade(shade, ctm, alpha, params);
    }

    fn fill_image(&mut self, image: &Image, ctm: Matrix, alpha: f32, params: ColorParams) {
        self.0.fill_image(image, ctm, alpha, params);
    }

    fn fill_image_mask(
        &mut self,
        image: &Image,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        params: ColorParams,
    ) {
        self.0.fill_image_mask(image, ctm, cs, color, alpha, params);
    }

    fn clip_image_mask(&mut self, image: &Image, ctm: Matrix, scissor: Rect) {
        self.0.clip_image_mask(image, ctm, scissor);
    }

    fn fill_text(
        &mut self,
        _text: &Text,
        _ctm: Matrix,
        _cs: &ColorSpace,
        _color: &[f32],
        _alpha: f32,
        _params: ColorParams,
    ) {
    }

    fn stroke_text(
        &mut self,
        _text: &Text,
        _stroke: &StrokeState,
        _ctm: Matrix,
        _cs: &ColorSpace,
        _color: &[f32],
        _alpha: f32,
        _params: ColorParams,
    ) {
    }

    fn clip_text(&mut self, text: &Text, ctm: Matrix, scissor: Rect) {
        self.0.clip_text(text, ctm, scissor);
    }

    fn clip_stroke_text(&mut self, text: &Text, stroke: &StrokeState, ctm: Matrix, scissor: Rect) {
        self.0.clip_stroke_text(text, stroke, ctm, scissor);
    }

    fn ignore_text(&mut self, _text: &Text, _ctm: Matrix) {}

    fn pop_clip(&mut self) {
        self.0.pop_clip();
    }

    fn begin_mask(
        &mut self,
        area: Rect,
        luminosity: bool,
        cs: Option<&ColorSpace>,
        backdrop: &[f32],
        params: ColorParams,
    ) {
        self.0.begin_mask(area, luminosity, cs, backdrop, params);
    }

    fn end_mask(&mut self, transfer: Option<&Function>) {
        self.0.end_mask(transfer);
    }

    fn begin_group(
        &mut self,
        area: Rect,
        cs: Option<&ColorSpace>,
        isolated: bool,
        knockout: bool,
        blendmode: u8,
        alpha: f32,
    ) {
        self.0
            .begin_group(area, cs, isolated, knockout, blendmode, alpha);
    }

    fn end_group(&mut self) {
        self.0.end_group();
    }

    fn begin_tile(
        &mut self,
        area: Rect,
        view: Rect,
        xstep: f32,
        ystep: f32,
        ctm: Matrix,
        id: i32,
    ) -> bool {
        self.0.begin_tile(area, view, xstep, ystep, ctm, id)
    }

    fn end_tile(&mut self) {
        self.0.end_tile();
    }

    fn set_default_colorspaces(&mut self, defaults: &DefaultColorSpaces) {
        self.0.set_default_colorspaces(defaults);
    }

    fn begin_layer(&mut self, name: &[u8]) {
        self.0.begin_layer(name);
    }

    fn end_layer(&mut self) {
        self.0.end_layer();
    }

    fn begin_structure(&mut self, standard: &'static str, raw: &[u8], index: i32) {
        self.0.begin_structure(standard, raw, index);
    }

    fn end_structure(&mut self) {
        self.0.end_structure();
    }

    fn begin_metatext(&mut self, meta: Metatext, text: &[u8]) {
        self.0.begin_metatext(meta, text);
    }

    fn end_metatext(&mut self) {
        self.0.end_metatext();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipKind {
    Clip,
    Mask,
    Group,
    Tile,
}

#[derive(Debug, Clone, Copy)]
struct HorizBar {
    bbox: Rect,
}

#[derive(Debug, Clone, Copy)]
struct VertSeg {
    bbox: Rect,
}

#[derive(Debug, Clone, Copy)]
struct BoxRect {
    bbox: Rect,
}

#[derive(Debug, Clone, Copy)]
struct GraphicPrim {
    bbox: Rect,
    path_ops: usize,
    curve_or_diag_ops: usize,
    is_image_or_shade: bool,
    is_filled_bar: bool,
}

/// Bounded device collecting horizontal bars, vertical segments, closed boxes,
/// and non-trivial vector/image primitives in MuPDF page space.
#[derive(Debug)]
struct VectorCollector<'d> {
    doc: Option<&'d Document>,
    mediabox: Rect,
    stack: Vec<(Rect, ClipKind)>,
    overflow: usize,
    segments_seen: usize,
    horiz_bars: Vec<HorizBar>,
    vert_segs: Vec<VertSeg>,
    boxes: Vec<BoxRect>,
    prims: Vec<GraphicPrim>,
}

impl<'d> VectorCollector<'d> {
    fn new(doc: Option<&'d Document>, mediabox: Rect) -> Self {
        Self {
            doc,
            mediabox,
            stack: Vec::new(),
            overflow: 0,
            segments_seen: 0,
            horiz_bars: Vec::new(),
            vert_segs: Vec::new(),
            boxes: Vec::new(),
            prims: Vec::new(),
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

    fn is_near_white(&self, cs: &ColorSpace, color: &[f32]) -> bool {
        let rgb = cs.to_rgb(self.doc, color);
        rgb[0] >= 0.98 && rgb[1] >= 0.98 && rgb[2] >= 0.98
    }

    fn is_page_backdrop(&self, r: Rect) -> bool {
        let pw = (self.mediabox.x1 - self.mediabox.x0).max(1.0);
        let ph = (self.mediabox.y1 - self.mediabox.y0).max(1.0);
        let rw = (r.x1 - r.x0).max(0.0);
        let rh = (r.y1 - r.y0).max(0.0);
        rw >= 0.75 * pw && rh >= 0.75 * ph
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
        if alpha <= 0.02 || self.segments_seen >= MAX_SEGMENTS || path.is_empty() {
            return;
        }
        if self.is_near_white(cs, color) {
            return;
        }

        let scissor = self.current_scissor();
        if scissor.is_empty() {
            return;
        }

        let stroke_half = stroke.map(|s| {
            let lw = if s.linewidth <= 0.0 { 1.0 } else { s.linewidth };
            (lw * path::max_expansion(ctm) * 0.5).max(0.4)
        });

        let budget = MAX_SEGMENTS.saturating_sub(self.segments_seen);
        let mut extractor = PathExtractor::new(ctm, scissor, stroke_half, budget);
        path.walk(&mut extractor);
        extractor.finish_subpath();

        self.segments_seen = self.segments_seen.saturating_add(extractor.ops);

        for hb in extractor.horiz_bars {
            if self.horiz_bars.len() < MAX_SEGMENTS {
                self.horiz_bars.push(hb);
            }
        }
        for vs in extractor.vert_segs {
            if self.vert_segs.len() < MAX_SEGMENTS {
                self.vert_segs.push(vs);
            }
        }
        for bx in extractor.boxes {
            if !self.is_page_backdrop(bx.bbox) && self.boxes.len() < MAX_PRIMS {
                self.boxes.push(bx);
            }
        }

        if self.prims.len() < MAX_PRIMS && extractor.ops > 0 {
            let raw_bound = path.bound(stroke, ctm).intersect(scissor);
            if raw_bound.is_valid() && !raw_bound.is_empty() && !self.is_page_backdrop(raw_bound) {
                let w = raw_bound.x1 - raw_bound.x0;
                let h = raw_bound.y1 - raw_bound.y0;
                let is_filled_bar =
                    stroke.is_none() && w >= 3.0 && h >= 3.0 && (w / h.max(0.1) <= 12.0);
                self.prims.push(GraphicPrim {
                    bbox: raw_bound,
                    path_ops: extractor.ops,
                    curve_or_diag_ops: extractor.curve_or_diag_ops,
                    is_image_or_shade: false,
                    is_filled_bar,
                });
            }
        }
    }

    fn record_raster_or_shade(&mut self, bounds: Rect, alpha: f32) {
        if alpha <= 0.02 || self.prims.len() >= MAX_PRIMS {
            return;
        }
        let clipped = bounds.intersect(self.current_scissor());
        if clipped.is_valid() && !clipped.is_empty() && !self.is_page_backdrop(clipped) {
            self.prims.push(GraphicPrim {
                bbox: clipped,
                path_ops: 0,
                curve_or_diag_ops: 0,
                is_image_or_shade: true,
                is_filled_bar: false,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
impl Device for VectorCollector<'_> {
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

    fn fill_shade(&mut self, shade: &Shade, ctm: Matrix, alpha: f32, _params: ColorParams) {
        self.record_raster_or_shade(shade.bound(ctm), alpha);
    }

    fn fill_image(&mut self, _image: &Image, ctm: Matrix, alpha: f32, _params: ColorParams) {
        self.record_raster_or_shade(Rect::UNIT.transform(ctm), alpha);
    }

    fn fill_image_mask(
        &mut self,
        _image: &Image,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        _params: ColorParams,
    ) {
        if !self.is_near_white(cs, color) {
            self.record_raster_or_shade(Rect::UNIT.transform(ctm), alpha);
        }
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

/// Walks a single [`Path`] into page-space horizontal bars, vertical segments,
/// closed rectangles, and segment operation counts.
struct PathExtractor {
    ctm: Matrix,
    scissor: Rect,
    stroke_half: Option<f32>,
    budget: usize,
    ops: usize,
    curve_or_diag_ops: usize,
    start: Point,
    curr: Point,
    has_curr: bool,
    pts: [Point; 5],
    pt_count: i32,
    subpath_bounds: Rect,
    horiz_bars: Vec<HorizBar>,
    vert_segs: Vec<VertSeg>,
    boxes: Vec<BoxRect>,
}

impl PathExtractor {
    fn new(ctm: Matrix, scissor: Rect, stroke_half: Option<f32>, budget: usize) -> Self {
        let zero = Point::new(0.0, 0.0);
        Self {
            ctm,
            scissor,
            stroke_half,
            budget,
            ops: 0,
            curve_or_diag_ops: 0,
            start: zero,
            curr: zero,
            has_curr: false,
            pts: [zero; 5],
            pt_count: 0,
            subpath_bounds: Rect::EMPTY,
            horiz_bars: Vec::new(),
            vert_segs: Vec::new(),
            boxes: Vec::new(),
        }
    }

    fn record_line_seg(&mut self, a: Point, b: Point) {
        let dx = (b.x - a.x).abs();
        let dy = (b.y - a.y).abs();
        if dx <= 0.05 && dy <= 0.05 {
            return;
        }
        if dy > 0.75 && dx > 0.75 {
            self.curve_or_diag_ops += 1;
        }
        if let Some(hw) = self.stroke_half {
            if dy <= 0.75 && dx >= 2.0 && hw * 2.0 <= 5.0 {
                let ym = 0.5 * (a.y + b.y);
                let r =
                    Rect::new(a.x.min(b.x), ym - hw, a.x.max(b.x), ym + hw).intersect(self.scissor);
                if r.is_valid() && !r.is_empty() {
                    self.horiz_bars.push(HorizBar { bbox: r });
                }
            } else if dx <= 0.75 && dy >= 2.0 && hw * 2.0 <= 5.0 {
                let xm = 0.5 * (a.x + b.x);
                let r =
                    Rect::new(xm - hw, a.y.min(b.y), xm + hw, a.y.max(b.y)).intersect(self.scissor);
                if r.is_valid() && !r.is_empty() {
                    self.vert_segs.push(VertSeg { bbox: r });
                }
            }
        }
    }

    fn finish_subpath(&mut self) {
        if self.pt_count < 4 || !self.subpath_bounds.is_valid() {
            self.pt_count = 0;
            self.subpath_bounds = Rect::EMPTY;
            return;
        }

        let is_rect = if self.pt_count == 4 || self.pt_count == 5 {
            let p = &self.pts;
            let closed = self.pt_count == 4 || (near(p[0].x, p[4].x) && near(p[0].y, p[4].y));
            let h_first = near(p[0].y, p[1].y)
                && near(p[1].x, p[2].x)
                && near(p[2].y, p[3].y)
                && near(p[3].x, p[0].x);
            let v_first = near(p[0].x, p[1].x)
                && near(p[1].y, p[2].y)
                && near(p[2].x, p[3].x)
                && near(p[3].y, p[0].y);
            closed && (h_first || v_first)
        } else {
            false
        };

        if is_rect {
            let mut r = self.subpath_bounds;
            if let Some(hw) = self.stroke_half {
                r = r.expand(hw);
            }
            let r = r.intersect(self.scissor);
            if r.is_valid() && !r.is_empty() {
                let w = r.x1 - r.x0;
                let h = r.y1 - r.y0;
                if self.stroke_half.is_none() {
                    if h <= 3.5 && w >= 2.0 && w >= 4.0 * h.max(0.01) {
                        self.horiz_bars.push(HorizBar { bbox: r });
                    } else if w <= 3.5 && h >= 2.0 && h >= 4.0 * w.max(0.01) {
                        self.vert_segs.push(VertSeg { bbox: r });
                    }
                }
                if w >= 6.0 && h >= 6.0 {
                    self.boxes.push(BoxRect { bbox: r });
                }
            }
        }

        self.pt_count = 0;
        self.subpath_bounds = Rect::EMPTY;
    }
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() <= 0.5
}

impl Walker for PathExtractor {
    fn move_to(&mut self, x: f32, y: f32) {
        if self.ops >= self.budget {
            return;
        }
        self.finish_subpath();
        let p = self.ctm.transform_xy(x, y);
        self.start = p;
        self.curr = p;
        self.has_curr = true;
        self.pts[0] = p;
        self.pt_count = 1;
        self.subpath_bounds = Rect::new(p.x, p.y, p.x, p.y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        if !self.has_curr || self.ops >= self.budget {
            return;
        }
        let p = self.ctm.transform_xy(x, y);
        self.ops += 1;
        self.record_line_seg(self.curr, p);
        self.subpath_bounds = self.subpath_bounds.include_point(p);

        if self.pt_count > 0 {
            let n = self.pt_count as usize;
            let prev = self.pts[n - 1];
            if near(prev.x, p.x) && near(prev.y, p.y) {
                // Coincident vertex: ignore without invalidating rectangle check.
            } else if n < 5 {
                self.pts[n] = p;
                self.pt_count += 1;
            } else {
                self.pt_count = -1;
            }
        }
        self.curr = p;
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) {
        if !self.has_curr || self.ops >= self.budget {
            return;
        }
        self.ops += 1;
        self.curve_or_diag_ops += 1;
        let p1 = self.ctm.transform_xy(x1, y1);
        let p2 = self.ctm.transform_xy(x2, y2);
        let p3 = self.ctm.transform_xy(x3, y3);
        self.subpath_bounds = self
            .subpath_bounds
            .include_point(p1)
            .include_point(p2)
            .include_point(p3);
        self.pt_count = -1;
        self.curr = p3;
    }

    fn close_path(&mut self) {
        if !self.has_curr {
            return;
        }
        if (!near(self.curr.x, self.start.x) || !near(self.curr.y, self.start.y))
            && self.ops < self.budget
        {
            self.ops += 1;
            self.record_line_seg(self.curr, self.start);
        }
        self.curr = self.start;
        self.finish_subpath();
    }
}

// ---------------------------------------------------------------------------
// Multi-pass Span Reconciliation & Perceptual Evaluation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RawSpan {
    id: String,
    bbox: Rect,
    text: String,
    chars: Vec<StextChar>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OriginKind {
    Standard,
    ClippedOut,
    HiddenLayer,
    ErasedActualText,
}

#[derive(Debug, Clone, Copy)]
struct GlyphKey {
    x: f32,
    y: f32,
    c: i32,
}

fn collect_raw_spans(page: &StextPage, page_num: i32) -> Vec<RawSpan> {
    let mut out = Vec::new();
    let prefix = format!("p{page_num}");
    collect_blocks_spans(&page.blocks, &prefix, &mut out);
    out
}

fn collect_blocks_spans(blocks: &[Block], prefix: &str, out: &mut Vec<RawSpan>) {
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
                        out.push(RawSpan {
                            id: format!("{lid}.s{}", si + 1),
                            bbox: sp.bbox,
                            text: extract::chars_text(slice),
                            chars: slice.to_vec(),
                        });
                    }
                }
            }
            Block::Struct(sb) => collect_blocks_spans(&sb.blocks, &bid, out),
            Block::Image(_) | Block::Vector(_) | Block::Grid(_) => {}
        }
    }
}

fn collect_glyph_keys(spans: &[RawSpan]) -> Vec<GlyphKey> {
    let mut keys = Vec::new();
    for sp in spans {
        for ch in &sp.chars {
            if ch.flags & char_flags::SYNTHETIC == 0 {
                keys.push(GlyphKey {
                    x: ch.origin.x,
                    y: ch.origin.y,
                    c: ch.c,
                });
            }
        }
    }
    keys
}

fn glyph_seen(ch: &StextChar, keys: &[GlyphKey]) -> bool {
    keys.iter().any(|k| {
        k.c == ch.c && (k.x - ch.origin.x).abs() <= 0.35 && (k.y - ch.origin.y).abs() <= 0.35
    })
}

fn span_matches_glyphs(span: &RawSpan, keys: &[GlyphKey]) -> bool {
    let mut checked = 0usize;
    let mut matched = 0usize;
    for ch in &span.chars {
        if ch.flags & char_flags::SYNTHETIC == 0 {
            checked += 1;
            if glyph_seen(ch, keys) {
                matched += 1;
            }
        }
    }
    checked > 0 && matched * 2 >= checked
}

/// Extracts spans (or contiguous unseen sub-runs within merged spans) from
/// `candidates` whose real glyphs do not appear in `seen`.
fn extract_unseen_spans(candidates: &[RawSpan], seen: &[GlyphKey]) -> Vec<RawSpan> {
    let mut out = Vec::new();
    for sp in candidates {
        let mut run_start: Option<usize> = None;
        for (i, ch) in sp.chars.iter().enumerate() {
            let synthetic = ch.flags & char_flags::SYNTHETIC != 0;
            let is_new = if synthetic {
                run_start.is_some()
            } else {
                !glyph_seen(ch, seen)
            };
            if is_new {
                if run_start.is_none() {
                    run_start = Some(i);
                }
            } else if let Some(st) = run_start.take()
                && let Some(slice) = sp.chars.get(st..i)
            {
                push_subrun(&mut out, slice);
            }
        }
        if let Some(st) = run_start
            && let Some(slice) = sp.chars.get(st..)
        {
            push_subrun(&mut out, slice);
        }
    }
    out
}

fn push_subrun(out: &mut Vec<RawSpan>, slice: &[StextChar]) {
    // Trim leading/trailing synthetic spaces left over from adjacent seen runs.
    let first = slice
        .iter()
        .position(|c| c.flags & char_flags::SYNTHETIC == 0);
    let last = slice
        .iter()
        .rposition(|c| c.flags & char_flags::SYNTHETIC == 0);
    let (Some(a), Some(b)) = (first, last) else {
        return;
    };
    let Some(trimmed) = slice.get(a..=b) else {
        return;
    };
    let mut bbox = Rect::EMPTY;
    for ch in trimmed {
        bbox = bbox.union(Rect::from_quad(ch.quad));
    }
    out.push(RawSpan {
        id: String::new(),
        bbox,
        text: extract::chars_text(trimmed),
        chars: trimmed.to_vec(),
    });
}

/// Compares spans extracted normally against spans extracted with
/// `IGNORE_ACTUALTEXT`, returning `Some((raw_glyph_text, raw_unmapped_count))`
/// whenever `/ActualText` altered a span's text.
fn match_actual_text(
    actual_spans: &[RawSpan],
    raw_spans: &[RawSpan],
) -> Vec<Option<(String, usize)>> {
    // Fast path: when no `/ActualText` altered anything, the two passes match 1-to-1.
    if actual_spans.len() == raw_spans.len()
        && actual_spans
            .iter()
            .zip(raw_spans.iter())
            .all(|(a, r)| a.text == r.text && same_rect(a.bbox, r.bbox))
    {
        return vec![None; actual_spans.len()];
    }

    let mut out = Vec::with_capacity(actual_spans.len());
    for sp in actual_spans {
        if !sp.bbox.is_valid() {
            out.push(None);
            continue;
        }
        let sh = (sp.bbox.y1 - sp.bbox.y0).max(0.5);

        // Collect all `IGNORE_ACTUALTEXT` spans on the same line/band that overlap horizontally.
        let mut overlapping: Vec<&RawSpan> = raw_spans
            .iter()
            .filter(|r| {
                if !r.bbox.is_valid() {
                    return false;
                }
                let rh = (r.bbox.y1 - r.bbox.y0).max(0.5);
                let oy = sp.bbox.y1.min(r.bbox.y1) - sp.bbox.y0.max(r.bbox.y0);
                let ox = sp.bbox.x1.min(r.bbox.x1) - sp.bbox.x0.max(r.bbox.x0);
                let near_origin = (sp.bbox.x0 - r.bbox.x0).abs() <= 1.5;
                oy >= 0.45 * sh.min(rh) && (ox > 0.25 || near_origin)
            })
            .collect();

        if overlapping.is_empty() {
            // Phantom `ActualText` with zero underlying glyphs.
            if !sp.text.is_empty() {
                out.push(Some((String::new(), 0)));
            } else {
                out.push(None);
            }
            continue;
        }

        // Prefer an exact bbox match first (standard case where `ActualText` wraps one span).
        if let Some(exact) = overlapping.iter().find(|r| same_rect(sp.bbox, r.bbox)) {
            if exact.text != sp.text {
                out.push(Some((exact.text.clone(), count_unmapped(&exact.chars))));
            } else {
                out.push(None);
            }
            continue;
        }

        overlapping.sort_by(|a, b| a.bbox.x0.total_cmp(&b.bbox.x0));
        if overlapping.len() == 1 {
            let r = overlapping[0];
            if r.text != sp.text {
                out.push(Some((r.text.clone(), count_unmapped(&r.chars))));
            } else {
                out.push(None);
            }
        } else {
            let mut combined = String::new();
            let mut unmapped = 0usize;
            for r in &overlapping {
                combined.push_str(&r.text);
                unmapped += count_unmapped(&r.chars);
            }
            if combined != sp.text {
                out.push(Some((combined, unmapped)));
            } else {
                out.push(None);
            }
        }
    }
    out
}

fn same_rect(a: Rect, b: Rect) -> bool {
    a.is_valid()
        && b.is_valid()
        && (a.x0 - b.x0).abs() <= 0.5
        && (a.y0 - b.y0).abs() <= 0.5
        && (a.x1 - b.x1).abs() <= 0.5
        && (a.y1 - b.y1).abs() <= 0.5
}

#[allow(clippy::too_many_arguments)]
fn build_attested_span(
    raw: &RawSpan,
    id: String,
    page_num: i32,
    origin: OriginKind,
    actual_mismatch: Option<(String, usize)>,
    mediabox: Rect,
    render_ctm: Matrix,
    full_pix: &draw::Pix,
    bg_pix: &draw::Pix,
    vectors: &VectorCollector<'_>,
    opts: &AttestOptions,
) -> AttestedSpan {
    let max_contrast = compute_max_contrast(raw.bbox, render_ctm, full_pix, bg_pix);

    let (glyph_text, raw_unmapped) = match actual_mismatch {
        Some((gt, u)) => (Some(gt), u),
        None => (None, 0),
    };

    let unmapped_glyphs = count_unmapped(&raw.chars).max(raw_unmapped);

    let visibility = classify_visibility(
        raw,
        origin,
        glyph_text.as_deref(),
        mediabox,
        max_contrast,
        opts.contrast_threshold,
    );

    let (vec_strike, vec_underline, boxed) = detect_span_markup(raw.bbox, mediabox, vectors);
    let flag_strike = raw
        .chars
        .iter()
        .any(|c| c.flags & char_flags::STRIKEOUT != 0);
    let flag_underline = raw
        .chars
        .iter()
        .any(|c| c.flags & char_flags::UNDERLINE != 0);

    let struck_through = flag_strike || vec_strike;
    let underlined = flag_underline || vec_underline;

    let mut hazards = Vec::new();
    match visibility {
        Visibility::Visible => {}
        Visibility::ZeroContrastOrOccluded => hazards.push("visibility.occluded".to_string()),
        Visibility::InvisibleMode => hazards.push("visibility.invisible_mode".to_string()),
        Visibility::Clipped => hazards.push("visibility.clipped".to_string()),
    }
    if glyph_text.is_some() {
        hazards.push("spoof.actual_text".to_string());
    }
    if struck_through {
        hazards.push("markup.strikethrough".to_string());
    }
    if unmapped_glyphs > 0 {
        hazards.push("font.unmapped_glyphs".to_string());
    }

    AttestedSpan {
        id,
        page: page_num,
        bbox: rect_array(raw.bbox),
        text: raw.text.clone(),
        glyph_text,
        visibility,
        max_contrast,
        struck_through,
        underlined,
        boxed,
        unmapped_glyphs,
        hazards,
    }
}

fn count_unmapped(chars: &[StextChar]) -> usize {
    chars
        .iter()
        .filter(|c| {
            c.flags & char_flags::SYNTHETIC == 0
                && (matches!(rune_char(c.c), '\u{fffd}' | '\u{fffe}' | '\u{ffff}')
                    || c.flags & (char_flags::UNICODE_IS_CID | char_flags::UNICODE_IS_GID) != 0)
        })
        .count()
}

fn classify_visibility(
    raw: &RawSpan,
    origin: OriginKind,
    glyph_text: Option<&str>,
    mediabox: Rect,
    max_contrast: u8,
    threshold: u8,
) -> Visibility {
    if origin == OriginKind::ClippedOut
        || !raw.bbox.is_valid()
        || raw.bbox.intersect(mediabox).is_empty()
    {
        return Visibility::Clipped;
    }

    let has_real = raw
        .chars
        .iter()
        .any(|c| c.flags & char_flags::SYNTHETIC == 0);
    let check_chars = |pred: &dyn Fn(&StextChar) -> bool| -> bool {
        if has_real {
            raw.chars
                .iter()
                .filter(|c| c.flags & char_flags::SYNTHETIC == 0)
                .all(pred)
        } else {
            !raw.chars.is_empty() && raw.chars.iter().all(pred)
        }
    };

    if check_chars(&|c| c.flags & char_flags::CLIPPED != 0) && max_contrast < threshold {
        return Visibility::Clipped;
    }

    let neither_filled_nor_stroked =
        check_chars(&|c| c.flags & (char_flags::FILLED | char_flags::STROKED) == 0);
    let zero_alpha = check_chars(&|c| (c.argb >> 24) == 0);
    if neither_filled_nor_stroked || zero_alpha {
        return Visibility::InvisibleMode;
    }

    if origin == OriginKind::HiddenLayer {
        return Visibility::ZeroContrastOrOccluded;
    }

    // Pure-whitespace spans (e.g. a styled space glyph) carry no ink to paint.
    let whitespace_only = raw.text.chars().all(char::is_whitespace)
        && glyph_text.is_none_or(|g| g.chars().all(char::is_whitespace));
    if whitespace_only && !raw.text.is_empty() {
        let non_white_color = raw
            .chars
            .iter()
            .any(|c| (c.argb & 0x00ff_ffff) != 0x00ff_ffff);
        if non_white_color {
            return Visibility::Visible;
        }
    }

    if max_contrast < threshold {
        Visibility::ZeroContrastOrOccluded
    } else {
        Visibility::Visible
    }
}

/// Computes the maximum single-channel RGB difference (`0..=255`) between
/// `full_pix` and `bg_pix` inside `span_bbox` transformed by `render_ctm`.
fn compute_max_contrast(
    span_bbox: Rect,
    render_ctm: Matrix,
    full_pix: &draw::Pix,
    bg_pix: &draw::Pix,
) -> u8 {
    if !span_bbox.is_valid() || span_bbox.is_infinite() || full_pix.n < 3 || bg_pix.n < 3 {
        return 0;
    }
    let px_rect = span_bbox.transform(render_ctm);
    let mut region = IRect::from_rect(px_rect);
    if region.x1 <= region.x0 {
        region.x1 = region.x0.saturating_add(1);
    }
    if region.y1 <= region.y0 {
        region.y1 = region.y0.saturating_add(1);
    }
    let region = region.intersect(full_pix.bbox()).intersect(bg_pix.bbox());
    if region.is_empty() {
        return 0;
    }

    let full_stride = full_pix.stride();
    let bg_stride = bg_pix.stride();
    let fn_comp = full_pix.n;
    let bn_comp = bg_pix.n;
    let width = region.width() as usize;

    let mut max_delta: u8 = 0;
    for y in region.y0..region.y1 {
        let fy = (y - full_pix.y) as usize;
        let by = (y - bg_pix.y) as usize;
        let fx0 = (region.x0 - full_pix.x) as usize;
        let bx0 = (region.x0 - bg_pix.x) as usize;
        let f_start = fy * full_stride + fx0 * fn_comp;
        let b_start = by * bg_stride + bx0 * bn_comp;

        let Some(f_row) = full_pix.samples.get(f_start..f_start + width * fn_comp) else {
            continue;
        };
        let Some(b_row) = bg_pix.samples.get(b_start..b_start + width * bn_comp) else {
            continue;
        };

        for x in 0..width {
            let fi = x * fn_comp;
            let bi = x * bn_comp;
            let dr = f_row[fi].abs_diff(b_row[bi]);
            let dg = f_row[fi + 1].abs_diff(b_row[bi + 1]);
            let db = f_row[fi + 2].abs_diff(b_row[bi + 2]);
            let d = dr.max(dg).max(db);
            if d > max_delta {
                max_delta = d;
                if max_delta == 255 {
                    return 255;
                }
            }
        }
    }
    max_delta
}

/// Evaluates vector markup (`(struck_through, underlined, boxed)`) for a span.
fn detect_span_markup(
    sb: Rect,
    mediabox: Rect,
    vectors: &VectorCollector<'_>,
) -> (bool, bool, bool) {
    if !sb.is_valid() || sb.is_empty() {
        return (false, false, false);
    }
    let sw = sb.x1 - sb.x0;
    let sh = sb.y1 - sb.y0;
    if sw <= 0.5 || sh <= 0.5 {
        return (false, false, false);
    }

    let min_x_overlap = 0.45 * sw;
    let max_bar_h = (0.45 * sh).max(3.5);

    let strike_y0 = sb.y0 + 0.25 * sh;
    let strike_y1 = sb.y0 + 0.72 * sh;
    let under_y0 = sb.y0 + 0.72 * sh;
    let under_y1 = sb.y0 + 1.15 * sh;

    let mut struck = false;
    let mut underlined = false;

    for bar in &vectors.horiz_bars {
        let hb = bar.bbox;
        let bh = hb.y1 - hb.y0;
        if bh > max_bar_h {
            continue;
        }
        let ox = hb.x1.min(sb.x1) - hb.x0.max(sb.x0);
        if ox < min_x_overlap {
            continue;
        }
        let ymid = 0.5 * (hb.y0 + hb.y1);
        if ymid >= strike_y0 && ymid <= strike_y1 {
            struck = true;
        } else if ymid > under_y0 && ymid <= under_y1 {
            underlined = true;
        }
        if struck && underlined {
            break;
        }
    }

    let page_area = ((mediabox.x1 - mediabox.x0).max(1.0)) * ((mediabox.y1 - mediabox.y0).max(1.0));
    let max_box_w = (sw + 180.0).max(sw * 4.0);
    let max_box_h = (sh + 120.0).max(sh * 6.0);

    let mut boxed = vectors.boxes.iter().any(|bx| {
        let br = bx.bbox;
        let bw = br.x1 - br.x0;
        let bh = br.y1 - br.y0;
        br.x0 <= sb.x0 + 2.0
            && br.y0 <= sb.y0 + 2.0
            && br.x1 >= sb.x1 - 2.0
            && br.y1 >= sb.y1 - 2.0
            && bw <= max_box_w
            && bh <= max_box_h
            && (bw * bh) < 0.65 * page_area
    });

    // Also recognize a 4-edge frame built from individual stroked lines.
    if !boxed {
        boxed = has_four_edge_frame(sb, max_box_w, max_box_h, page_area, vectors);
    }

    (struck, underlined, boxed)
}

fn has_four_edge_frame(
    sb: Rect,
    max_w: f32,
    max_h: f32,
    page_area: f32,
    vectors: &VectorCollector<'_>,
) -> bool {
    let sw = sb.x1 - sb.x0;
    let sh = sb.y1 - sb.y0;
    let pad_x = (max_w - sw) * 0.5;
    let pad_y = (max_h - sh) * 0.5;

    let top = vectors
        .horiz_bars
        .iter()
        .find(|b| {
            let r = b.bbox;
            r.y1 <= sb.y0 + 3.0
                && r.y0 >= sb.y0 - pad_y
                && r.x0 <= sb.x0 + 2.0
                && r.x1 >= sb.x1 - 2.0
        })
        .map(|b| b.bbox);

    let bottom = vectors
        .horiz_bars
        .iter()
        .find(|b| {
            let r = b.bbox;
            r.y0 >= sb.y1 - 3.0
                && r.y1 <= sb.y1 + pad_y
                && r.x0 <= sb.x0 + 2.0
                && r.x1 >= sb.x1 - 2.0
        })
        .map(|b| b.bbox);

    let (Some(t), Some(b)) = (top, bottom) else {
        return false;
    };
    let frame_y0 = t.y0;
    let frame_y1 = b.y1;
    let fh = frame_y1 - frame_y0;
    if fh > max_h || fh <= 0.0 {
        return false;
    }

    let has_left = vectors.vert_segs.iter().any(|v| {
        let r = v.bbox;
        r.x1 <= sb.x0 + 3.0
            && r.x0 >= sb.x0 - pad_x
            && r.y0 <= frame_y0 + 3.0
            && r.y1 >= frame_y1 - 3.0
    });
    let has_right = vectors.vert_segs.iter().any(|v| {
        let r = v.bbox;
        r.x0 >= sb.x1 - 3.0
            && r.x1 <= sb.x1 + pad_x
            && r.y0 <= frame_y0 + 3.0
            && r.y1 >= frame_y1 - 3.0
    });

    let fw = t.x1.max(b.x1) - t.x0.min(b.x0);
    has_left && has_right && fw <= max_w && (fw * fh) < 0.65 * page_area
}

// ---------------------------------------------------------------------------
// Blind-Spot Discovery & PNG Cropping
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PrimCluster {
    bbox: Rect,
    path_ops: usize,
    curve_or_diag_ops: usize,
    has_image_or_shade: bool,
    filled_bars: usize,
}

fn build_blind_spots(
    vectors: &VectorCollector<'_>,
    spans: &[AttestedSpan],
    visible_span_rects: &[Rect],
    mediabox: Rect,
    render_ctm: Matrix,
    full_pix: &draw::Pix,
    contrast_threshold: u8,
) -> Vec<(&'static str, Rect, usize)> {
    let mut out: Vec<(&'static str, Rect, usize)> = Vec::new();

    // 1. Spatial clustering of vector/image primitives.
    let mut clusters: Vec<PrimCluster> = vectors
        .prims
        .iter()
        .map(|p| PrimCluster {
            bbox: p.bbox,
            path_ops: p.path_ops,
            curve_or_diag_ops: p.curve_or_diag_ops,
            has_image_or_shade: p.is_image_or_shade,
            filled_bars: usize::from(p.is_filled_bar),
        })
        .collect();

    // Iteratively merge clusters whose bboxes touch within 4 pt.
    let mut merged_any = true;
    while merged_any && clusters.len() > 1 {
        merged_any = false;
        let mut i = 0;
        while i < clusters.len() {
            let mut j = i + 1;
            while j < clusters.len() {
                if !clusters[i]
                    .bbox
                    .expand(4.0)
                    .intersect(clusters[j].bbox)
                    .is_empty()
                {
                    let other = clusters.swap_remove(j);
                    clusters[i].bbox = clusters[i].bbox.union(other.bbox);
                    clusters[i].path_ops += other.path_ops;
                    clusters[i].curve_or_diag_ops += other.curve_or_diag_ops;
                    clusters[i].has_image_or_shade |= other.has_image_or_shade;
                    clusters[i].filled_bars += other.filled_bars;
                    merged_any = true;
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }

    let page_area = ((mediabox.x1 - mediabox.x0).max(1.0)) * ((mediabox.y1 - mediabox.y0).max(1.0));

    for cl in clusters {
        let cb = cl.bbox.intersect(mediabox);
        if !cb.is_valid() || cb.is_empty() {
            continue;
        }
        let cw = cb.x1 - cb.x0;
        let ch = cb.y1 - cb.y0;
        let area = cw * ch;
        if cw < 8.0 || ch < 6.0 || area < 64.0 || area >= 0.85 * page_area {
            continue;
        }

        // Measure how much of this cluster is already explained by visible mapped text.
        let mut text_area = 0.0f32;
        let mut text_count = 0usize;
        for tr in visible_span_rects {
            let inter = cb.intersect(*tr);
            if !inter.is_empty() {
                text_area += (inter.x1 - inter.x0) * (inter.y1 - inter.y0);
                text_count += 1;
            }
        }
        let text_coverage = text_area / area;

        // Distinguish true graphic blind spots (charts, diagrams, outlined text, scans)
        // from simple rectilinear borders/tables around visible text.
        let qualifies = if text_count == 0 {
            cl.has_image_or_shade || cl.path_ops >= 6 || cl.curve_or_diag_ops >= 3
        } else {
            text_coverage < 0.40
                && (cl.has_image_or_shade
                    || cl.curve_or_diag_ops >= 2
                    || (cl.filled_bars >= 3 && cl.path_ops >= 8))
        };

        if !qualifies {
            continue;
        }

        // Confirm the region actually contains visible non-blank pixels on the rendered page.
        if !region_has_ink(cb, render_ctm, full_pix, contrast_threshold) {
            continue;
        }

        out.push(("vector_graphic", cb, cl.path_ops));
    }

    // 2. Unmapped-glyph spans (`"unmapped_text"`).
    for sp in spans {
        if sp.unmapped_glyphs > 0 {
            let [x0, y0, x1, y1] = sp.bbox;
            let r = Rect::new(x0, y0, x1, y1);
            if r.is_valid() && !r.is_empty() {
                out.push(("unmapped_text", r, 0));
            }
        }
    }

    // Deterministic reading/spatial order: top-to-bottom, left-to-right.
    out.sort_by(|a, b| {
        a.1.y0
            .total_cmp(&b.1.y0)
            .then_with(|| a.1.x0.total_cmp(&b.1.x0))
            .then_with(|| a.0.cmp(b.0))
    });

    out
}

/// Checks whether `full_pix` within `rect` differs from a uniform blank backdrop
/// by at least `threshold`.
fn region_has_ink(rect: Rect, render_ctm: Matrix, full_pix: &draw::Pix, threshold: u8) -> bool {
    if full_pix.n < 3 {
        return false;
    }
    let px = IRect::from_rect(rect.transform(render_ctm)).intersect(full_pix.bbox());
    if px.is_empty() {
        return false;
    }
    let stride = full_pix.stride();
    let n = full_pix.n;
    let w = px.width() as usize;

    let mut min_c = [255u8; 3];
    let mut max_c = [0u8; 3];
    for y in px.y0..px.y1 {
        let start = (y - full_pix.y) as usize * stride + (px.x0 - full_pix.x) as usize * n;
        let Some(row) = full_pix.samples.get(start..start + w * n) else {
            continue;
        };
        for x in 0..w {
            let i = x * n;
            for k in 0..3 {
                let v = row[i + k];
                if v < min_c[k] {
                    min_c[k] = v;
                }
                if v > max_c[k] {
                    max_c[k] = v;
                }
            }
            let spread = (max_c[0] - min_c[0])
                .max(max_c[1] - min_c[1])
                .max(max_c[2] - min_c[2]);
            let non_white = 255u8.saturating_sub(min_c[0].min(min_c[1]).min(min_c[2]));
            if spread >= threshold || non_white >= threshold {
                return true;
            }
        }
    }
    false
}

/// Renders a PNG crop for `spot_rect` via [`draw::render_page_banded`].
#[allow(clippy::too_many_arguments)]
fn render_spot_png(
    doc: &Document,
    page: &Object,
    spot_rect: Rect,
    mediabox: Rect,
    render_ctm: Matrix,
    page_px_bbox: IRect,
    max_pixels: u64,
    store: Store,
) -> (Option<Vec<u8>>, Store) {
    let padded = spot_rect.expand(2.0).intersect(mediabox);
    if !padded.is_valid() || padded.is_empty() {
        return (None, store);
    }
    let mut region = IRect::round(padded.transform(render_ctm));
    if region.x1 <= region.x0 {
        region.x1 = region.x0.saturating_add(1);
    }
    if region.y1 <= region.y0 {
        region.y1 = region.y0.saturating_add(1);
    }
    let region = region.intersect(page_px_bbox);
    if region.is_empty() {
        return (None, store);
    }

    let ((rendered, next_store), _) = diag::capture(|| {
        draw::render_page_banded(
            doc,
            page,
            render_ctm,
            max_pixels,
            Some(region),
            store,
            1,
            true,
            draw::Parts::ALL,
        )
    });
    let png = rendered
        .ok()
        .and_then(|r| r.pix.to_pixmap(Some(ColorSpace::Rgb)).png().ok());
    (png, next_store)
}

fn rect_array(r: Rect) -> [f32; 4] {
    if r.is_valid()
        && !r.is_infinite()
        && r.x0.is_finite()
        && r.y0.is_finite()
        && r.x1.is_finite()
        && r.y1.is_finite()
    {
        [r.x0, r.y0, r.x1, r.y1]
    } else {
        [0.0, 0.0, 0.0, 0.0]
    }
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{doc_bytes, page_doc, pdf_of, stream};

    const HELV_RESOURCES: &str = "/Font<<\
        /F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>\
        /F2<</Type/Font/Subtype/Type1/BaseFont/Helvetica-Bold/Encoding/WinAnsiEncoding>>\
    >>";

    #[test]
    fn visible_spans_preserve_standard_extract_content_ids() {
        let doc = page_doc(
            b"0 0 0 rg \
              BT /F1 12 Tf 72 720 Td (Clause One) Tj ET \
              BT /F1 12 Tf 72 680 Td (Total: ) Tj /F2 12 Tf ($500) Tj ET",
            HELV_RESOURCES,
            &[],
        );

        let att = attest_page(&doc, 0, &AttestOptions::default(), false)
            .unwrap_or_else(|e| panic!("{e}"));

        assert_eq!(att.page, 1);
        assert_eq!(att.span_count, 3);
        assert_eq!(att.visible_spans, 3);
        assert_eq!(att.hidden_or_occluded_spans, 0);
        assert_eq!(att.struck_through_spans, 0);
        assert_eq!(att.actual_text_mismatches, 0);
        assert_eq!(att.unmapped_spans, 0);
        assert!(att.blind_spots.is_empty());

        let ids: Vec<&str> = att.spans.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["p1.b1.l1.s1", "p1.b2.l1.s1", "p1.b2.l1.s2"]);

        for sp in &att.spans {
            assert_eq!(sp.visibility, Visibility::Visible);
            assert!(sp.max_contrast > 150, "contrast was {}", sp.max_contrast);
            assert!(
                sp.hazards.is_empty(),
                "unexpected hazards: {:?}",
                sp.hazards
            );
            assert_eq!(sp.glyph_text, None);
        }
    }

    #[test]
    fn detects_occluded_white_on_white_invisible_mode_and_clipped_text() {
        // Page [0 0 612 792]:
        // 1. Normal visible header at y_pdf = 730
        // 2. Black text at y_pdf = 670 occluded by a white rect `1 1 1 rg 65 664 220 24 re f` drawn AFTER BT..ET
        // 3. White-on-white text (`1 1 1 rg`) at y_pdf = 610
        // 4. Render mode `3 Tr` hidden text at y_pdf = 550
        // 5. Text clipped away by `0 0 10 10 re W n` at y_pdf = 490
        let content = b"0 0 0 rg BT /F1 12 Tf 72 730 Td (Visible Header) Tj ET \
            0 0 0 rg BT /F1 12 Tf 72 670 Td (Covered Secret) Tj ET \
            1 1 1 rg 65 664 220 24 re f \
            1 1 1 rg BT /F1 12 Tf 72 610 Td (White On White) Tj ET \
            0 0 0 rg BT /F1 12 Tf 3 Tr 72 550 Td (Render Mode Three) Tj ET \
            q 0 0 10 10 re W n 0 0 0 rg BT /F1 12 Tf 0 Tr 72 490 Td (Clipped Clause) Tj ET Q";

        let doc = page_doc(content, HELV_RESOURCES, &[]);
        let att = attest_page(&doc, 0, &AttestOptions::default(), false)
            .unwrap_or_else(|e| panic!("{e}"));

        let by_text = |needle: &str| -> &AttestedSpan {
            att.spans
                .iter()
                .find(|s| s.text == needle)
                .unwrap_or_else(|| panic!("missing span {needle:?} in {:?}", att.spans))
        };

        let vis = by_text("Visible Header");
        assert_eq!(vis.visibility, Visibility::Visible);
        assert!(vis.max_contrast > 180);
        assert!(vis.hazards.is_empty());

        let occ = by_text("Covered Secret");
        assert_eq!(occ.visibility, Visibility::ZeroContrastOrOccluded);
        assert_eq!(occ.max_contrast, 0);
        assert!(occ.hazards.contains(&"visibility.occluded".to_string()));

        let wow = by_text("White On White");
        assert_eq!(wow.visibility, Visibility::ZeroContrastOrOccluded);
        assert_eq!(wow.max_contrast, 0);
        assert!(wow.hazards.contains(&"visibility.occluded".to_string()));

        let inv = by_text("Render Mode Three");
        assert_eq!(inv.visibility, Visibility::InvisibleMode);
        assert_eq!(inv.max_contrast, 0);
        assert!(
            inv.hazards
                .contains(&"visibility.invisible_mode".to_string())
        );

        let clip = by_text("Clipped Clause");
        assert_eq!(clip.id, "p1.hidden1");
        assert_eq!(clip.visibility, Visibility::Clipped);
        assert!(clip.hazards.contains(&"visibility.clipped".to_string()));

        assert_eq!(att.visible_spans, 1);
        assert_eq!(att.hidden_or_occluded_spans, 4);
    }

    #[test]
    fn recovers_vector_strikethrough_underline_and_enclosing_box() {
        // 1. Stroked horizontal line `m l S` right through x-height middle of `72 700 Td` (`y_pdf = 704`)
        // 2. Thin filled horizontal rect `re f` through middle of `72 650 Td` (`y_pdf = 653.5`)
        // 3. Stroked underline just below baseline of `72 600 Td` (`y_pdf = 598.2`)
        // 4. Stroked enclosing rectangle around `72 530 Td`
        let content = b"0 0 0 rg 0 0 0 RG 1 w \
            BT /F1 12 Tf 72 700 Td (Voided Warranty Clause) Tj ET \
            70 704 m 210 704 l S \
            BT /F1 12 Tf 72 650 Td (Deleted Fee Schedule) Tj ET \
            70 653.5 140 1.2 re f \
            BT /F1 12 Tf 72 600 Td (Signed Agreement) Tj ET \
            70 598.2 m 180 598.2 l S \
            BT /F1 12 Tf 72 530 Td (Boxed Notice) Tj ET \
            64 522 100 24 re S";

        let doc = page_doc(content, HELV_RESOURCES, &[]);
        let att = attest_page(&doc, 0, &AttestOptions::default(), false)
            .unwrap_or_else(|e| panic!("{e}"));

        let find = |t: &str| {
            att.spans
                .iter()
                .find(|s| s.text == t)
                .unwrap_or_else(|| panic!("missing {t}"))
        };

        let s1 = find("Voided Warranty Clause");
        assert!(s1.struck_through);
        assert!(!s1.underlined);
        assert!(s1.hazards.contains(&"markup.strikethrough".to_string()));

        let s2 = find("Deleted Fee Schedule");
        assert!(s2.struck_through);
        assert!(s2.hazards.contains(&"markup.strikethrough".to_string()));

        let u1 = find("Signed Agreement");
        assert!(u1.underlined);
        assert!(!u1.struck_through);
        assert!(u1.hazards.is_empty());

        let bx = find("Boxed Notice");
        assert!(bx.boxed);
        assert!(!bx.struck_through);
        assert!(
            att.blind_spots.is_empty(),
            "markup should not create blind spots"
        );
        assert_eq!(att.struck_through_spans, 2);
    }

    #[test]
    fn detects_actual_text_replacement_and_empty_erasure_spoofing() {
        let content = b"0 0 0 rg \
            /Span <</ActualText (Pay $100,000)>> BDC \
            BT /F1 12 Tf 72 700 Td (Pay $100) Tj ET \
            EMC \
            /Span <</ActualText ()>> BDC \
            BT /F1 12 Tf 72 650 Td (Hidden Indemnity) Tj ET \
            EMC";

        let doc = page_doc(content, HELV_RESOURCES, &[]);
        let att = attest_page(&doc, 0, &AttestOptions::default(), false)
            .unwrap_or_else(|e| panic!("{e}"));

        assert_eq!(att.actual_text_mismatches, 2);

        let spoof = att
            .spans
            .iter()
            .find(|s| s.text == "Pay $100,000")
            .unwrap_or_else(|| panic!("missing spoofed span"));
        assert_eq!(spoof.glyph_text.as_deref(), Some("Pay $100"));
        assert_eq!(spoof.visibility, Visibility::Visible);
        assert!(spoof.hazards.contains(&"spoof.actual_text".to_string()));

        let erased = att
            .spans
            .iter()
            .find(|s| s.glyph_text.as_deref() == Some("Hidden Indemnity"))
            .unwrap_or_else(|| panic!("missing erased span"));
        assert_eq!(erased.text, "");
        assert_eq!(erased.visibility, Visibility::Visible);
        assert!(erased.hazards.contains(&"spoof.actual_text".to_string()));
    }

    #[test]
    fn flags_unmapped_glyphs_and_renders_vector_graphic_blind_spot_png_crops() {
        // Helvetica font `/U1` whose ToUnicode CMap maps `<61><62>` to `<FFFD>` (visible ink,
        // unmapped Unicode), plus a dense vector bar-and-trendline chart box at [100, 300, 320, 440].
        let bad_cmap = stream(
            "",
            b"/CIDInit/ProcSet findresource begin 12 dict begin begincmap \
              /CMapName/Bad def 1 begincodespacerange <00><FF> endcodespacerange \
              1 beginbfrange <61><62><FFFD> endbfrange endcmap \
              CMapName currentdict /CMap defineresource pop end end",
        );
        let resources = "/Font<<\
            /F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>\
            /U1<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding/ToUnicode 5 0 R>>\
        >>";

        let content = b"0 0 0 rg \
            BT /F1 12 Tf 72 720 Td (Quarterly Report) Tj ET \
            BT /U1 14 Tf 72 680 Td (ab) Tj ET \
            0.2 0.2 0.2 RG 1.5 w 100 300 220 140 re S \
            0.1 0.4 0.8 rg \
            120 310 25 60 re f \
            160 310 25 95 re f \
            200 310 25 45 re f \
            240 310 25 115 re f \
            0.85 0.1 0.1 RG 2 w \
            132 375 m 172 410 l 212 360 l 252 430 l 295 400 l S";

        let doc = page_doc(content, resources, &[bad_cmap]);

        let att_crop =
            attest_page(&doc, 0, &AttestOptions::default(), true).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(att_crop.unmapped_spans, 1);

        let unmapped_span = att_crop
            .spans
            .iter()
            .find(|s| s.unmapped_glyphs > 0)
            .unwrap_or_else(|| panic!("expected unmapped span"));
        assert_eq!(unmapped_span.unmapped_glyphs, 2);
        assert!(
            unmapped_span
                .hazards
                .contains(&"font.unmapped_glyphs".to_string())
        );

        let chart_spot = att_crop
            .blind_spots
            .iter()
            .find(|b| b.kind == "vector_graphic")
            .unwrap_or_else(|| {
                panic!(
                    "expected vector_graphic blind spot in {:?}",
                    att_crop.blind_spots
                )
            });
        assert!(chart_spot.id.starts_with("p1.spot"));
        assert!(
            chart_spot.path_ops >= 10,
            "path_ops={}",
            chart_spot.path_ops
        );
        let png = chart_spot
            .png
            .as_ref()
            .unwrap_or_else(|| panic!("expected PNG crop bytes"));
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));

        let unmapped_spot = att_crop
            .blind_spots
            .iter()
            .find(|b| b.kind == "unmapped_text")
            .unwrap_or_else(|| panic!("expected unmapped_text blind spot"));
        assert!(
            unmapped_spot
                .png
                .as_ref()
                .is_some_and(|b| b.starts_with(b"\x89PNG\r\n\x1a\n"))
        );

        // Without `crop`, `png` is None.
        let att_no_crop = attest_page(&doc, 0, &AttestOptions::default(), false)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(att_no_crop.blind_spots.iter().all(|b| b.png.is_none()));
    }

    #[test]
    fn hidden_ocg_layer_and_multi_page_attestation_work() {
        let c1 = stream(
            "",
            b"0 0 0 rg BT /F1 12 Tf 72 700 Td (Public Clause) Tj ET \
              /OC /OffLayer BDC BT /F1 12 Tf 72 660 Td (Hidden Layer Clause) Tj ET EMC",
        );
        let c2 = stream(
            "",
            b"0 0 0 rg BT /F1 12 Tf 72 700 Td (Page Two Visible) Tj ET",
        );
        let objs = vec![
            b"<</Type/Catalog/Pages 2 0 R/OCProperties<</OCGs[7 0 R]/D<</OFF[7 0 R]>>>>>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 8 0 R>>/Properties<</OffLayer 7 0 R>>>>/Contents 5 0 R>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 8 0 R>>>>/Contents 6 0 R>>".to_vec(),
            c1,
            c2,
            b"<</Type/OCG/Name(OffLayer)>>".to_vec(),
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>".to_vec(),
        ];
        let doc = doc_bytes(&objs);

        let pages = attest_document(&doc, &AttestOptions::default(), false)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].page, 1);
        assert_eq!(pages[0].visible_spans, 1);
        assert_eq!(pages[0].hidden_or_occluded_spans, 1);

        let hidden = pages[0]
            .spans
            .iter()
            .find(|s| s.text == "Hidden Layer Clause")
            .unwrap_or_else(|| panic!("missing hidden OCG span"));
        assert_eq!(hidden.id, "p1.hidden1");
        assert_eq!(hidden.visibility, Visibility::ZeroContrastOrOccluded);
        assert!(hidden.hazards.contains(&"visibility.occluded".to_string()));

        assert_eq!(pages[1].page, 2);
        assert_eq!(pages[1].visible_spans, 1);

        let par = attest_pages(&doc, &[0, 1], &AttestOptions::default(), false, 2);
        assert_eq!(
            par.into_iter()
                .map(|r| r.unwrap_or_else(|e| panic!("{e}")))
                .collect::<Vec<_>>(),
            pages
        );

        // Also ensure invalid DPI is rejected cleanly without panicking.
        let bad_pdf = pdf_of(&[
            "<</Type/Catalog/Pages 2 0 R>>",
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]>>",
        ]);
        let simple = Document::open(bad_pdf).unwrap_or_else(|e| panic!("{e:?}"));
        let bad_opts = AttestOptions {
            dpi: 0.0,
            ..AttestOptions::default()
        };
        assert!(attest_page(&simple, 0, &bad_opts, false).is_err());
    }
}
