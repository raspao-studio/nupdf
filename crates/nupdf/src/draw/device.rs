// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/draw-device.c,
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! `fz_draw_device`: what a page run paints into a pixmap. Clips push a
//! state; a clip that is not a rectangle paints into a copy of the
//! pixmap, which a mask puts back when the clip is popped.

use crate::colorspace::{ColorSpace, MAX_COLORS};
use crate::device::{ColorParams, Device};
use crate::document::Document;
use crate::geometry::{IRect, Matrix, Rect};
use crate::path::{Path, StrokeState};

use super::edge::Gel;
use super::glyph::{GlyphCache, outline_glyph};
use super::grays::Bitmap;
use super::paint::{Pix, mul255};
use crate::image::Image;
use crate::shade::Shade;
use crate::text::Text;

use super::image::{extent_and_factor, gridfit, paint_image, scale, subsample, transform_pixmap};
use super::path::{flatten_fill, flatten_stroke};

/// Clips nested deeper than this are not drawn (MuPDF grows its stack
/// until memory runs out).
const MAX_STACK: usize = 4096;

/// Decoded images by object number, generation, subsampling and whether
/// as a mask.
type ImageCache = std::collections::HashMap<(i32, i32, u32, bool), (Pix, Option<ColorSpace>)>;

/// `fz_draw_state`: a pixmap of its own when a clip needed one (else the
/// one below is painted), the mask that puts it back, and the scissor.
#[derive(Debug)]
struct State {
    dest: Option<Pix>,
    mask: Option<Pix>,
    scissor: IRect,
    /// A transparency group's alpha and blend mode, painted onto the
    /// state below when it ends.
    group: Option<(f32, u8, bool)>,
    tile: Option<Tile>,
    /// The group alpha: what a non-isolated group has painted, kept to
    /// composite it back (`group_alpha`).
    ga: Ga,
    /// The shape: what a knockout group's object covered.
    shape: Ga,
    /// `FZ_BLEND_KNOCKOUT` and `FZ_BLEND_ISOLATED` of the state's blend
    /// mode, which the states pushed above it inherit.
    ko: bool,
    iso: bool,
    /// Pushed by `fz_knockout_begin` around one object.
    ko_temp: bool,
    /// Drawing a soft mask (`in_smask`): colors convert without ICC.
    in_smask: bool,
}

/// A state's group alpha: the one below's, none, or its own.
#[derive(Debug, Default)]
enum Ga {
    #[default]
    Inherit,
    Off,
    Own(Pix),
}

fn no_pix() -> Pix {
    Pix {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        n: 1,
        alpha: true,
        samples: Vec::new(),
    }
}

/// `tile_key`: the pattern, its transform without the translation, and
/// whether the cell has a shape and a group alpha.
type TileKey = (i32, [u32; 4], bool, bool);

/// A cell kept: its pixels, and its shape and group alpha when the
/// state it was drawn in had them.
type TileCell = (Pix, Option<Pix>, Option<Pix>);

fn tile_key(id: i32, ctm: Matrix, shape: bool, ga: bool) -> TileKey {
    let m = [
        ctm.a.to_bits(),
        ctm.b.to_bits(),
        ctm.c.to_bits(),
        ctm.d.to_bits(),
    ];
    (id, m, shape, ga)
}

/// A pattern cell being drawn: what `fz_draw_end_tile` repeats it by.
#[derive(Debug, Clone, Copy)]
struct Tile {
    area: Rect,
    xstep: f32,
    ystep: f32,
    ctm: Matrix,
    id: i32,
    /// Drawn now, to be kept; false for a cell from the cache.
    encache: bool,
}

impl State {
    fn new(dest: Option<Pix>, mask: Option<Pix>, scissor: IRect) -> State {
        State {
            dest,
            mask,
            scissor,
            group: None,
            tile: None,
            ga: Ga::Inherit,
            shape: Ga::Inherit,
            ko: false,
            iso: false,
            ko_temp: false,
            in_smask: false,
        }
    }
}

pub struct DrawDevice<'d> {
    doc: Option<&'d Document>,
    gel: Gel,
    glyphs: GlyphCache,
    tiles: std::collections::HashMap<TileKey, TileCell>,
    /// Images decoded so far, by object, subsampling and whether as a
    /// mask, as MuPDF's store keeps them, and their total size.
    images: ImageCache,
    image_bytes: usize,
    stack: Vec<State>,
    /// Pushes past `MAX_STACK`, popped without drawing.
    overflow: usize,
    /// Type 3 glyphs being drawn into this device's pixmap, by nesting.
    depth: usize,
    /// How colors convert: with ICC profiles or MuPDF's formulas.
    pub cc: super::color::Converter,
    /// `FZ_DRAWDEV_FLAGS_TYPE3`: drawing a Type 3 glyph, where images are
    /// not fitted to the pixel grid.
    type3: bool,
}

/// `fz_rasterizer_graphics_aa_level` for 8 bits: `2 / (8 + 2)`.
const AA_LEVEL: f32 = 0.2;

/// Whether an image's pixmap has `FZ_PIXMAP_FLAG_INTERPOLATE`: as its
/// /Interpolate says for images MuPDF streams, always for JPX, which
/// `fz_load_jpx` decodes into a pixmap with `fz_new_pixmap`'s flags.
fn interpolates(image: &Image) -> bool {
    image.interpolate || image.is_jpx
}

fn flatness(expansion: f32) -> (f32, f32) {
    let expansion = if expansion < f32::EPSILON {
        1.0
    } else {
        expansion
    };
    ((0.3 / expansion).max(0.001), expansion)
}

impl std::fmt::Debug for DrawDevice<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DrawDevice")
            .field("depth", &self.stack.len())
            .finish_non_exhaustive()
    }
}

impl<'d> DrawDevice<'d> {
    /// `fz_new_draw_device` with an identity transform.
    pub fn new(doc: Option<&'d Document>, dest: Pix) -> DrawDevice<'d> {
        let scissor = dest.bbox();
        DrawDevice {
            doc,
            gel: Gel::new(),
            glyphs: GlyphCache::default(),
            tiles: std::collections::HashMap::new(),
            images: std::collections::HashMap::new(),
            image_bytes: 0,
            stack: vec![State::new(Some(dest), None, scissor)],
            overflow: 0,
            depth: 0,
            type3: false,
            cc: super::color::Converter::default(),
        }
    }

    /// The page pixmap, once the run is over. Clips left open are dropped.
    pub fn into_pix(mut self) -> Pix {
        while self.stack.len() > 1 {
            self.pop_clip();
        }
        self.stack
            .pop()
            .and_then(|s| s.dest)
            .unwrap_or_else(|| Pix {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
                n: 3,
                alpha: false,
                samples: Vec::new(),
            })
    }

    /// The state whose pixmap is painted.
    fn dest_index(&self) -> usize {
        self.stack
            .iter()
            .rposition(|s| s.dest.is_some())
            .unwrap_or(0)
    }

    fn scissor(&self) -> IRect {
        self.stack.last().map_or(IRect::EMPTY, |s| s.scissor)
    }

    /// `ri` with `FZ_RI_IN_SOFTMASK` when a soft mask is being drawn.
    fn ri(&self, ri: u8) -> u8 {
        if self.stack.last().is_some_and(|s| s.in_smask) {
            ri | super::color::RI_IN_SOFTMASK
        } else {
            ri
        }
    }

    /// Whether colors convert through ICC profiles here: not in a soft
    /// mask.
    fn icc(&self) -> bool {
        self.cc.icc && self.ri(0) == 0
    }

    /// `resolve_color` into the pixmap's RGB: the bytes truncated, as
    /// `colorbv[i] = colorfv[i] * 255` does, and alpha after them.
    fn resolve_color(
        &self,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        n: usize,
    ) -> [u8; MAX_COLORS + 1] {
        self.resolve_color_ri(cs, color, alpha, n, 1)
    }

    fn resolve_color_ri(
        &self,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        n: usize,
        ri: u8,
    ) -> [u8; MAX_COLORS + 1] {
        let ri = self.ri(ri);
        let mut out = [0u8; MAX_COLORS + 1];
        let comps = match n {
            0 => [0.0; 3],
            1 => [self.cc.gray(self.doc, cs, color, ri), 0.0, 0.0],
            _ => self.cc.rgb(self.doc, cs, color, ri),
        };
        let n = n.min(3);
        for (o, v) in out.iter_mut().zip(&comps[..n]) {
            *o = (v * 255.0) as u8;
        }
        out[n] = (alpha * 255.0) as u8;
        out
    }

    /// Paints what is in the rasterizer in `color` (components, then alpha).
    fn paint_gel(&mut self, even_odd: bool, color: &[u8]) {
        let mut ga = self.take_ga();
        let mut shape = self.take_plane(true);
        let alpha = [color.last().copied().unwrap_or(255)];
        let di = self.dest_index();
        let DrawDevice { gel, stack, .. } = self;
        if let Some(dest) = stack.get_mut(di).and_then(|s| s.dest.as_mut()) {
            let clip = dest.bbox();
            let mut g = ga.as_mut().map(|(_, p)| p);
            let mut h = shape.as_mut().map(|(_, p)| p);
            gel.convert(even_odd, clip, &mut |x, y, m| {
                dest.span_with_color(x, y, m, color);
                if let Some(h) = h.as_deref_mut() {
                    h.span_with_color(x, y, m, &[255]);
                }
                if let Some(g) = g.as_deref_mut() {
                    g.span_with_color(x, y, m, &alpha);
                }
            });
        }
        self.put_plane(true, shape);
        self.put_ga(ga);
    }

    fn dest_bbox(&self) -> IRect {
        let di = self.dest_index();
        self.stack
            .get(di)
            .and_then(|s| s.dest.as_ref())
            .map_or(IRect::EMPTY, Pix::bbox)
    }

    /// `push_stack` for a clip whose outline is in the rasterizer, its box
    /// `bbox` (None when empty). `rect` says whether a rectangle covers it.
    fn push_clip(&mut self, bbox: Option<IRect>, rect: bool, even_odd: bool, isolate: bool) {
        let Some(bbox) = bbox else {
            let st = self.child(None, None, IRect::EMPTY);
            self.stack.push(st);
            return;
        };
        if rect {
            let st = self.child(None, None, bbox);
            self.stack.push(st);
            return;
        }
        let di = self.dest_index();
        let (n, alpha) = match self.stack.get(di).and_then(|s| s.dest.as_ref()) {
            Some(d) => (d.n, d.alpha),
            None => (3, false),
        };
        let (Ok(mut mask), Ok(mut dest)) = (Pix::new(bbox, 1, true), Pix::new(bbox, n, alpha))
        else {
            let st = self.child(None, None, IRect::EMPTY);
            self.stack.push(st);
            return;
        };
        if let Some(parent) = self.stack.get(di).and_then(|s| s.dest.as_ref()) {
            dest.copy_rect(parent, bbox);
        }
        let clip = mask.bbox();
        self.gel
            .convert(even_odd, clip, &mut |x, y, m| mask.span_mask(x, y, m));
        let mut state = self.child(Some(dest), Some(mask), bbox);
        state.ga = self.clip_ga(bbox, false);
        state.shape = self.clip_plane(true, bbox, false);
        state.iso |= isolate;
        self.stack.push(state);
    }

    /// The box a clip starts from: the pixmap, the scissor, and the
    /// device's own scissor when it has one.
    fn clip_start(&self, scissor: Rect) -> IRect {
        let mut bbox = self.dest_bbox().intersect(self.scissor());
        if !scissor.is_infinite() {
            bbox = bbox.intersect(IRect::from_rect(scissor));
        }
        bbox
    }

    fn full(&mut self) -> bool {
        if self.stack.len() >= MAX_STACK {
            self.overflow += 1;
            return true;
        }
        false
    }

    /// The state whose plane (group alpha or shape) is painted, if any.
    fn plane_index(&self, shape: bool) -> Option<usize> {
        self.plane_index_upto(shape, self.stack.len())
    }

    fn plane_index_upto(&self, shape: bool, end: usize) -> Option<usize> {
        for (i, st) in self.stack[..end.min(self.stack.len())]
            .iter()
            .enumerate()
            .rev()
        {
            match if shape { &st.shape } else { &st.ga } {
                Ga::Own(_) => return Some(i),
                Ga::Off => return None,
                Ga::Inherit => {}
            }
        }
        None
    }

    fn take_plane(&mut self, shape: bool) -> Option<(usize, Pix)> {
        let i = self.plane_index(shape)?;
        let slot = if shape {
            &mut self.stack[i].shape
        } else {
            &mut self.stack[i].ga
        };
        match std::mem::replace(slot, Ga::Own(no_pix())) {
            Ga::Own(p) => Some((i, p)),
            other => {
                *slot = other;
                None
            }
        }
    }

    fn put_plane(&mut self, shape: bool, taken: Option<(usize, Pix)>) {
        if let Some((i, p)) = taken {
            if shape {
                self.stack[i].shape = Ga::Own(p);
            } else {
                self.stack[i].ga = Ga::Own(p);
            }
        }
    }

    fn take_ga(&mut self) -> Option<(usize, Pix)> {
        self.take_plane(false)
    }

    fn put_ga(&mut self, taken: Option<(usize, Pix)>) {
        self.put_plane(false, taken);
    }

    /// A clip's plane: a copy of the current one over `bbox` (cleared
    /// with `clear`), when there is one.
    fn clip_plane(&self, shape: bool, bbox: IRect, clear: bool) -> Ga {
        let Some(i) = self.plane_index(shape) else {
            return Ga::Inherit;
        };
        let Ok(mut g) = Pix::new(bbox, 1, true) else {
            return Ga::Inherit;
        };
        let parent = if shape {
            &self.stack[i].shape
        } else {
            &self.stack[i].ga
        };
        if !clear && let Ga::Own(p) = parent {
            g.copy_rect(p, bbox);
        }
        Ga::Own(g)
    }

    fn clip_ga(&self, bbox: IRect, clear: bool) -> Ga {
        self.clip_plane(false, bbox, clear)
    }

    /// A state pushed above the top: its blend mode bits inherited.
    fn child(&self, dest: Option<Pix>, mask: Option<Pix>, scissor: IRect) -> State {
        let mut st = State::new(dest, mask, scissor);
        if let Some(top) = self.stack.last() {
            st.ko = top.ko;
            st.iso = top.iso;
            st.in_smask = top.in_smask;
        }
        st
    }

    /// `fz_knockout_begin`: inside a knockout group, an object paints onto
    /// a copy of the group's backdrop, with a shape of its own. True when
    /// a state was pushed.
    fn knockout_begin(&mut self) -> bool {
        let Some(top) = self.stack.last() else {
            return false;
        };
        if !top.ko || self.stack.len() >= MAX_STACK {
            return false;
        }
        let isolated = top.iso;
        let scissor = top.scissor;
        let di = self.dest_index();
        let Some(cur) = self.stack.get(di).and_then(|s| s.dest.as_ref()) else {
            return false;
        };
        let bbox = cur.bbox().intersect(scissor);
        let Ok(mut dest) = Pix::new(bbox, cur.n, cur.alpha) else {
            return false;
        };
        let ga_now = self.plane_index(false);
        let mut ga = Ga::Inherit;
        if let Some(gi) = ga_now
            && let Ga::Own(g) = &self.stack[gi].ga
            && let Ok(p) = Pix::new(g.bbox().intersect(scissor), 1, true)
        {
            ga = Ga::Own(p);
        }
        if !isolated {
            // The last but one destination: the backdrop of the group.
            let prev = self.stack[..di].iter().rposition(|s| s.dest.is_some());
            if let Some(pi) = prev {
                if let Some(pd) = self.stack[pi].dest.as_ref() {
                    dest.copy_rect(pd, bbox);
                }
                if let Ga::Own(g) = &mut ga
                    && let Some(pg) = self.plane_index_upto(false, pi + 1)
                    && let Ga::Own(src) = &self.stack[pg].ga
                {
                    let b = g.bbox();
                    g.copy_rect(src, b);
                }
            }
        }
        let Ok(shape) = Pix::new(bbox, 1, true) else {
            return false;
        };
        let mut st = State::new(Some(dest), None, bbox);
        st.ga = ga;
        st.shape = Ga::Own(shape);
        st.ko = true;
        st.iso = false;
        st.ko_temp = true;
        self.stack.push(st);
        true
    }

    /// `fz_knockout_end`: the object put back, knocking out what it
    /// covers.
    fn knockout_end(&mut self) {
        if !self.stack.last().is_some_and(|s| s.ko_temp) {
            return;
        }
        let Some(st) = self.stack.pop() else {
            return;
        };
        if !self.stack.last().is_some_and(|s| s.ko) {
            return;
        }
        let (Some(src), Ga::Own(shape)) = (st.dest, st.shape) else {
            return;
        };
        let di = self.dest_index();
        if let Some(dest) = self.stack.get_mut(di).and_then(|s| s.dest.as_mut()) {
            dest.blend_knockout(&src, &shape);
        }
        if let Ga::Own(g) = st.ga {
            let mut pg = self.take_plane(false);
            if let Some((_, p)) = pg.as_mut() {
                p.blend_knockout(&g, &shape);
            }
            self.put_plane(false, pg);
        }
        let mut ps = self.take_plane(true);
        if let Some((_, p)) = ps.as_mut() {
            p.paint_pixmap(&shape, 255);
        }
        self.put_plane(true, ps);
    }
}

impl Device for DrawDevice<'_> {
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
        let (flat, _) = flatness(ctm.expansion());
        let bbox = self.dest_bbox().intersect(self.scissor());
        if flatten_fill(&mut self.gel, path, ctm, flat, bbox).is_none() {
            return;
        }
        if alpha == 0.0 {
            return;
        }
        let ko = alpha != 1.0 && self.knockout_begin();
        let n = self.dest_n();
        let colorbv = self.resolve_color_ri(cs, color, alpha, n, params.ri & 3);
        self.paint_gel(even_odd, &colorbv[..=n]);
        if ko {
            self.knockout_end();
        }
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
        let (flat, expansion) = flatness(ctm.expansion());
        let mut linewidth = stroke.linewidth;
        if linewidth * expansion < AA_LEVEL {
            linewidth = AA_LEVEL / expansion;
        }
        let bbox = self.dest_bbox().intersect(self.scissor());
        if flatten_stroke(&mut self.gel, path, stroke, ctm, flat, linewidth, bbox).is_none() {
            return;
        }
        if alpha == 0.0 {
            return;
        }
        let ko = alpha != 1.0 && self.knockout_begin();
        let n = self.dest_n();
        let colorbv = self.resolve_color_ri(cs, color, alpha, n, params.ri & 3);
        self.paint_gel(false, &colorbv[..=n]);
        if ko {
            self.knockout_end();
        }
    }

    fn clip_path(&mut self, path: &Path, even_odd: bool, ctm: Matrix, scissor: Rect) {
        if self.full() {
            return;
        }
        let (flat, _) = flatness(ctm.expansion());
        let start = self.clip_start(scissor);
        let bbox = flatten_fill(&mut self.gel, path, ctm, flat, start);
        let rect = self.gel.is_rect();
        self.push_clip(bbox, rect, even_odd, false);
    }

    fn clip_stroke_path(&mut self, path: &Path, stroke: &StrokeState, ctm: Matrix, scissor: Rect) {
        if self.full() {
            return;
        }
        let (flat, expansion) = flatness(ctm.expansion());
        let mut linewidth = stroke.linewidth;
        if linewidth * expansion < AA_LEVEL {
            linewidth = AA_LEVEL / expansion;
        }
        let start = self.clip_start(scissor);
        let bbox = flatten_stroke(&mut self.gel, path, stroke, ctm, flat, linewidth, start);
        self.push_clip(bbox, false, false, true);
    }

    fn fill_text(
        &mut self,
        text: &Text,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        params: ColorParams,
    ) {
        if alpha == 0.0 {
            return;
        }
        let ko = alpha != 1.0 && self.knockout_begin();
        let n = self.dest_n();
        let colorbv = self.resolve_color_ri(cs, color, alpha, n, params.ri & 3);
        let scissor = self.scissor();
        self.fill_text_spans(text, ctm, cs, color, alpha, params, &colorbv[..=n], scissor);
        if ko {
            self.knockout_end();
        }
    }

    fn stroke_text(
        &mut self,
        text: &Text,
        stroke: &StrokeState,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        params: ColorParams,
    ) {
        for span in &text.spans {
            let mut tm = span.trm;
            for item in &span.items {
                if item.gid < 0 {
                    continue;
                }
                tm.e = item.x;
                tm.f = item.y;
                if let Some(path) = outline_glyph(&span.font, item.gid, tm) {
                    self.stroke_path(&path, stroke, ctm, cs, color, alpha, params);
                }
            }
        }
    }

    fn clip_text(&mut self, text: &Text, ctm: Matrix, scissor: Rect) {
        if self.full() || !self.push_text_clip(text.bound(None, ctm), scissor) {
            return;
        }
        let bbox = self.scissor();
        for span in &text.spans {
            let mut tm = span.trm;
            for item in &span.items {
                if item.gid < 0 {
                    continue;
                }
                tm.e = item.x;
                tm.f = item.y;
                let mut trm = tm.concat(ctm);
                let glyph = if span.font.t3.is_some() {
                    self.type3_glyph(&span.font, item.gid, &mut trm, bbox)
                } else {
                    self.glyphs.render(&span.font, item.gid, &mut trm)
                };
                match glyph {
                    Some(g) => {
                        // fz_draw_clip_text truncates where fill_text floors.
                        let (x, y) = (trm.e as i32, trm.f as i32);
                        self.draw_glyph(None, &g, x, y, bbox);
                    }
                    None => {
                        if let Some(path) = outline_glyph(&span.font, item.gid, tm) {
                            self.fill_mask_path(&path, ctm);
                        }
                    }
                }
            }
        }
    }

    /// Text stroked with a pattern or shading clips to its strokes. Like
    /// `stroke_text`, glyphs stroke as outlines where MuPDF has FreeType
    /// stroke them; Type 3 glyphs draw as they are, as in MuPDF.
    fn clip_stroke_text(&mut self, text: &Text, stroke: &StrokeState, ctm: Matrix, scissor: Rect) {
        if self.full() || !self.push_text_clip(text.bound(Some(stroke), ctm), scissor) {
            return;
        }
        let bbox = self.scissor();
        for span in &text.spans {
            let mut tm = span.trm;
            for item in &span.items {
                if item.gid < 0 {
                    continue;
                }
                tm.e = item.x;
                tm.f = item.y;
                if span.font.t3.is_some() {
                    let mut trm = tm.concat(ctm);
                    if let Some(g) = self.type3_glyph(&span.font, item.gid, &mut trm, bbox) {
                        let (x, y) = (trm.e as i32, trm.f as i32);
                        self.draw_glyph(None, &g, x, y, bbox);
                    }
                } else if let Some(path) = outline_glyph(&span.font, item.gid, tm) {
                    self.stroke_mask_path(&path, stroke, ctm);
                }
            }
        }
    }

    fn fill_shade(&mut self, shade: &Shade, ctm: Matrix, alpha: f32, params: ColorParams) {
        let bbox = IRect::from_rect(shade.bound(ctm)).intersect(self.scissor());
        if bbox.is_empty() || alpha == 0.0 {
            return;
        }
        let ko = alpha != 1.0 && self.knockout_begin();
        let n = self.dest_n();
        let colorbv = shade
            .background
            .as_ref()
            .map(|bg| self.resolve_color(&shade.colorspace, bg, alpha, n));
        let doc = self.doc;
        let di = self.dest_index();
        let _ = params;
        let ri = self.ri(1);
        'paint: {
            let Some(dest) = self.stack.get_mut(di).and_then(|s| s.dest.as_mut()) else {
                break 'paint;
            };
            let mut temp = if alpha < 1.0 {
                let Ok(mut t) = Pix::new(bbox, dest.n, dest.alpha) else {
                    break 'paint;
                };
                if !dest.alpha {
                    t.copy_rect(dest, bbox);
                }
                Some(t)
            } else {
                None
            };
            let target: &mut Pix = match temp.as_mut() {
                Some(t) => t,
                None => &mut *dest,
            };
            if let Some(c) = colorbv {
                let b = bbox.intersect(target.bbox());
                let tn = target.n;
                for y in b.y0..b.y1 {
                    for x in b.x0..b.x1 {
                        let at = ((y - target.y) * target.w + (x - target.x)) as usize * tn;
                        target.samples[at..at + tn].copy_from_slice(&c[..tn]);
                    }
                }
            }
            super::shade::paint_shade(doc, &self.cc, ri, shade, ctm, target, bbox);
            if let Some(t) = temp {
                dest.paint_pixmap(&t, (alpha * 255.0) as i32);
            }
        }
        // The shape is the shading's box.
        let sh = self.take_plane(true);
        if let Some((i, mut h)) = sh {
            if let Ok(mut full) = Pix::new(bbox, 1, true) {
                full.samples.fill(255);
                h.copy_rect(&full, bbox);
            }
            self.put_plane(true, Some((i, h)));
        }
        // The group alpha is the shading's box, at the shading's alpha.
        let ga = self.take_ga();
        if let Some((i, mut g)) = ga {
            if let Ok(mut full) = Pix::new(bbox, 1, true) {
                full.clear_with(255);
                full.samples.fill(255);
                if alpha < 1.0 {
                    g.paint_pixmap(&full, (alpha * 255.0) as i32);
                } else {
                    g.copy_rect(&full, bbox);
                }
            }
            self.put_ga(Some((i, g)));
        }
        if ko {
            self.knockout_end();
        }
    }

    fn fill_image(&mut self, image: &Image, ctm: Matrix, alpha: f32, _params: ColorParams) {
        if alpha == 0.0 {
            return;
        }
        let clip = self.dest_bbox().intersect(self.scissor());
        if image.width == 0 || image.height == 0 || clip.is_empty() {
            return;
        }
        let mut ctm = if self.type3 { ctm } else { gridfit(ctm) };
        let Some((mut pix, cs, dw, dh)) = self.decode(image, ctm, false) else {
            return;
        };
        // The colorspace as the page's defaults replace it: DeviceGray
        // paints as it is (MuPDF's gray to RGB painter), DeviceRGB needs
        // nothing; without color management any gray or RGB space is so.
        let src = cs.as_ref().map(|c| self.cc.default_of(c).clone());
        let family = src.as_ref().and_then(|c| c.family());
        let plain = |f| {
            family == Some(f)
                && (!self.icc()
                    || src
                        .as_ref()
                        .is_some_and(|c| !matches!(c, ColorSpace::Icc(..))))
        };
        let is_gray = plain(crate::colorspace::Family::Gray)
            && !src
                .as_ref()
                .is_some_and(|c| matches!(c, ColorSpace::Separation { .. }));
        let is_rgb = plain(crate::colorspace::Family::Rgb);
        // Images with more components than RGB convert before scaling,
        // the rest after; gray paints as it is.
        let before = !is_gray && cs.as_ref().map_or(0, ColorSpace::n) > 3;
        if before {
            pix = self.to_rgb(&pix, cs.as_ref());
        }
        if dw < pix.w && dh < pix.h {
            let fit = alpha == 1.0;
            let scaled = transform_pixmap(&pix, &mut ctm, dw, dh, fit, clip).or_else(|| {
                scale(
                    &pix,
                    pix.x as f32,
                    pix.y as f32,
                    dw.max(1) as f32,
                    dh.max(1) as f32,
                    None,
                )
            });
            if let Some(s) = scaled {
                pix = s;
            }
        }
        if !before && !is_gray && !is_rgb {
            pix = self.to_rgb(&pix, cs.as_ref());
        }
        let ko = alpha != 1.0 && self.knockout_begin();
        let scissor = self.scissor();
        let mut ga = self.take_ga();
        let mut shape = self.take_plane(true);
        let di = self.dest_index();
        if let Some(dest) = self.stack.get_mut(di).and_then(|s| s.dest.as_mut()) {
            paint_image(
                dest,
                scissor,
                &pix,
                ctm,
                None,
                (alpha * 255.0) as i32,
                true,
                interpolates(image),
                ga.as_mut().map(|(_, g)| g),
                shape.as_mut().map(|(_, g)| g),
            );
        }
        self.put_plane(true, shape);
        self.put_ga(ga);
        if ko {
            self.knockout_end();
        }
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
        if alpha == 0.0 || image.width == 0 || image.height == 0 {
            return;
        }
        let clip = self.dest_bbox().intersect(self.scissor());
        let mut ctm = if self.type3 { ctm } else { gridfit(ctm) };
        let Some((mut pix, _, dw, dh)) = self.decode(image, ctm, true) else {
            return;
        };
        if dw < pix.w && dh < pix.h {
            let fit = alpha == 1.0;
            let scaled = transform_pixmap(&pix, &mut ctm, dw, dh, fit, clip).or_else(|| {
                scale(
                    &pix,
                    pix.x as f32,
                    pix.y as f32,
                    dw.max(1) as f32,
                    dh.max(1) as f32,
                    None,
                )
            });
            if let Some(s) = scaled {
                pix = s;
            }
        }
        let n = self.dest_n();
        let colorbv = self.resolve_color_ri(cs, color, alpha, n, params.ri & 3);
        let ko = alpha != 1.0 && self.knockout_begin();
        let scissor = self.scissor();
        let mut ga = self.take_ga();
        let mut shape = self.take_plane(true);
        let di = self.dest_index();
        if let Some(dest) = self.stack.get_mut(di).and_then(|s| s.dest.as_mut()) {
            paint_image(
                dest,
                scissor,
                &pix,
                ctm,
                Some(&colorbv[..=n]),
                255,
                true,
                interpolates(image),
                ga.as_mut().map(|(_, g)| g),
                shape.as_mut().map(|(_, g)| g),
            );
        }
        self.put_plane(true, shape);
        self.put_ga(ga);
        if ko {
            self.knockout_end();
        }
    }

    fn clip_image_mask(&mut self, image: &Image, ctm: Matrix, scissor: Rect) {
        if self.full() {
            return;
        }
        let empty = self.child(None, None, IRect::EMPTY);
        let clip = self.dest_bbox().intersect(self.scissor());
        if image.width == 0 || image.height == 0 {
            self.stack.push(empty);
            return;
        }
        let mut ctm = if self.type3 { ctm } else { gridfit(ctm) };
        let mut bbox = IRect::from_rect(Rect::new(0.0, 0.0, 1.0, 1.0).transform(ctm))
            .intersect(self.scissor());
        if !scissor.is_infinite() {
            bbox = bbox.intersect(IRect::from_rect(scissor));
        }
        if bbox.x0 > bbox.x1 || bbox.y0 > bbox.y1 {
            self.stack.push(empty);
            return;
        }
        let Some((mut pix, _, dw, dh)) = self.decode(image, ctm, true) else {
            self.stack.push(empty);
            return;
        };
        let di = self.dest_index();
        let (n, alpha) = match self.stack.get(di).and_then(|s| s.dest.as_ref()) {
            Some(d) => (d.n, d.alpha),
            None => (3, false),
        };
        let (Ok(mut mask), Ok(mut dest)) = (Pix::new(bbox, 1, true), Pix::new(bbox, n, alpha))
        else {
            self.stack.push(empty);
            return;
        };
        if let Some(parent) = self.stack.get(di).and_then(|s| s.dest.as_ref()) {
            dest.copy_rect(parent, bbox);
        }
        if dw < pix.w && dh < pix.h {
            let scaled = transform_pixmap(&pix, &mut ctm, dw, dh, true, clip).or_else(|| {
                scale(
                    &pix,
                    pix.x as f32,
                    pix.y as f32,
                    dw.max(1) as f32,
                    dh.max(1) as f32,
                    None,
                )
            });
            if let Some(s) = scaled {
                pix = s;
            }
        }
        let mut ga = self.clip_ga(bbox, true);
        let mut shape = self.clip_plane(true, bbox, true);
        paint_image(
            &mut mask,
            bbox,
            &pix,
            ctm,
            None,
            255,
            true,
            interpolates(image),
            match &mut ga {
                Ga::Own(g) => Some(g),
                _ => None,
            },
            match &mut shape {
                Ga::Own(g) => Some(g),
                _ => None,
            },
        );
        let mut state = self.child(Some(dest), Some(mask), bbox);
        state.ga = ga;
        state.shape = shape;
        state.iso = true;
        self.stack.push(state);
    }

    fn begin_mask(
        &mut self,
        area: Rect,
        luminosity: bool,
        cs: Option<&ColorSpace>,
        backdrop: &[f32],
        _params: ColorParams,
    ) {
        if self.full() {
            return;
        }
        let bbox = IRect::from_rect(area).intersect(self.scissor());
        let dest = if luminosity {
            Pix::new(bbox, 1, false).map(|mut d| {
                let ri = 1 | super::color::RI_IN_SOFTMASK;
                let bc = self
                    .cc
                    .gray(self.doc, cs.unwrap_or(&ColorSpace::Gray), backdrop, ri);
                d.clear_with((bc * 255.0) as i32 as u8);
                d
            })
        } else {
            Pix::new(bbox, 1, true)
        };
        let mut state = State::new(dest.ok(), None, bbox);
        state.ga = Ga::Off;
        state.shape = Ga::Off;
        state.in_smask = true;
        self.stack.push(state);
    }

    fn end_mask(&mut self, transfer: Option<&crate::function::Function>) {
        if self.overflow > 0 || self.stack.len() <= 1 {
            return;
        }
        let Some(top) = self.stack.last_mut() else {
            return;
        };
        let Some(gray) = top.dest.take() else {
            return;
        };
        // fz_alpha_from_gray, then the transfer function.
        let mut mask = Pix {
            n: 1,
            alpha: true,
            samples: gray.samples.chunks(gray.n.max(1)).map(|p| p[0]).collect(),
            ..gray
        };
        if let (Some(tr), Some(doc)) = (transfer, self.doc) {
            let mut memo = [0u8; 256];
            for (i, m) in memo.iter_mut().enumerate() {
                let mut d = [0f32];
                tr.eval(doc, &[i as f32 / 255.0], &mut d);
                *m = ((d[0] * 255.0) as i32).clamp(0, 255) as u8;
            }
            if mask.samples.len() > 1024 {
                for v in &mut mask.samples {
                    *v = memo[usize::from(*v)];
                }
            } else {
                for v in &mut mask.samples {
                    let mut d = [0f32];
                    tr.eval(doc, &[f32::from(*v) / 255.0], &mut d);
                    *v = ((d[0] * 255.0) as i32).clamp(0, 255) as u8;
                }
            }
        }
        let bbox = mask.bbox();
        let below = self.stack.len() - 1;
        let di = self.stack[..below]
            .iter()
            .rposition(|s| s.dest.is_some())
            .unwrap_or(0);
        let parent = self.stack.get(di).and_then(|s| s.dest.as_ref());
        let dest = parent.and_then(|p| {
            let mut d = Pix::new(bbox, p.n, p.alpha).ok()?;
            d.copy_rect(p, bbox);
            Some(d)
        });
        let parent_ga = self.stack[..below].iter().rev().find_map(|s| match s.ga {
            Ga::Own(_) => Some(true),
            Ga::Off => Some(false),
            Ga::Inherit => None,
        });
        let ga = if parent_ga == Some(true) {
            Pix::new(bbox, 1, true).map_or(Ga::Inherit, Ga::Own)
        } else {
            Ga::Inherit
        };
        let parent_shape = self.plane_index_upto(true, below).is_some();
        let shape = if parent_shape {
            Pix::new(bbox, 1, true).map_or(Ga::Inherit, Ga::Own)
        } else {
            Ga::Inherit
        };
        if let Some(top) = self.stack.last_mut() {
            top.dest = dest;
            top.mask = Some(mask);
            top.scissor = bbox;
            top.ga = ga;
            top.shape = shape;
            top.iso = true;
            top.in_smask = false;
        }
    }

    fn begin_group(
        &mut self,
        area: Rect,
        _cs: Option<&ColorSpace>,
        isolated: bool,
        knockout: bool,
        blendmode: u8,
        alpha: f32,
    ) {
        if self.full() {
            return;
        }
        // An object of a knockout group: its own copy of the backdrop.
        if self.stack.last().is_some_and(|s| s.ko) {
            self.knockout_begin();
        }
        let bbox = IRect::from_rect(area).intersect(self.scissor());
        let n = self.dest_n();
        let di = self.dest_index();
        let parent_alpha = self
            .stack
            .get(di)
            .and_then(|s| s.dest.as_ref())
            .is_some_and(|d| d.alpha);
        let mut dest = Pix::new(
            bbox,
            n + usize::from(parent_alpha || isolated),
            parent_alpha || isolated,
        )
        .ok();
        let ga = if isolated {
            Ga::Off
        } else {
            if let (Some(d), Some(parent)) = (
                dest.as_mut(),
                self.stack.get(di).and_then(|s| s.dest.as_ref()),
            ) {
                d.copy_rect(parent, bbox);
            }
            Pix::new(bbox, 1, true).map_or(Ga::Off, Ga::Own)
        };
        let mut state = self.child(dest.take(), None, bbox);
        state.group = Some((alpha, blendmode, isolated));
        state.ga = ga;
        state.ko = knockout;
        state.iso = isolated;
        self.stack.push(state);
    }

    fn end_group(&mut self) {
        if self.overflow > 0 {
            self.overflow -= 1;
            return;
        }
        if self.stack.len() <= 1 {
            return;
        }
        let Some(state) = self.stack.pop() else {
            return;
        };
        let (Some((alpha, blendmode, isolated)), Some(src)) = (state.group, state.dest) else {
            return;
        };
        let a = (alpha * 255.0) as i32;
        let own = match state.ga {
            Ga::Own(g) => Some(g),
            _ => None,
        };
        let mut parent_ga = self.take_ga();
        let mode = blendmode & 15;
        let di = self.dest_index();
        let mut src = src;
        if let Some(dest) = self.stack.get_mut(di).and_then(|s| s.dest.as_mut()) {
            match &own {
                // The same group alpha above and below: a plain paint.
                None if mode == 0 && parent_ga.is_none() => dest.paint_pixmap(&src, a),
                None => {
                    // fz_blend_pixmap's "hack": an isolated group's pixels
                    // take its alpha in place, so the group alpha below is
                    // painted from them so taken.
                    if isolated && a < 255 {
                        for v in &mut src.samples {
                            *v = mul255(i32::from(*v), a) as u8;
                        }
                        dest.blend_pixmap(&src, 255, mode);
                    } else {
                        dest.blend_pixmap(&src, a, mode);
                    }
                }
                Some(g) => dest.blend_nonisolated(&src, a, mode, g),
            }
        }
        if let Some((_, pg)) = parent_ga.as_mut() {
            let ga_alpha = if isolated { 255 } else { a };
            match &own {
                Some(g) => pg.paint_pixmap(g, ga_alpha),
                None => pg.paint_pixmap_alpha(&src, ga_alpha),
            }
        }
        self.put_ga(parent_ga);
        if self.stack.last().is_some_and(|s| s.ko_temp && s.ko) {
            self.knockout_end();
        }
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
        if self.full() {
            return false;
        }
        let mut bbox = IRect::from_rect(view.transform(ctm));
        if bbox.x1 == bbox.x0 {
            bbox.x1 = bbox.x0 + 1;
        }
        if bbox.y1 == bbox.y0 {
            bbox.y1 = bbox.y0 + 1;
        }
        // The cell has a shape and a group alpha of its own when the
        // state below has them, painted into theirs at the end.
        let has_shape = self.plane_index(true).is_some();
        let has_ga = self.plane_index(false).is_some();
        let key = tile_key(id, ctm, has_shape, has_ga);
        let cached = if id != 0 {
            self.tiles.get(&key).cloned()
        } else {
            None
        };
        let hit = cached.is_some();
        let n = self.dest_n();
        let plane = |on: bool| {
            if !on {
                return Ga::Off;
            }
            Pix::new(bbox, 1, true).map_or(Ga::Off, Ga::Own)
        };
        let (dest, shape, ga) = match cached {
            Some((d, s, g)) => (
                Some(d),
                s.map_or(Ga::Off, Ga::Own),
                g.map_or(Ga::Off, Ga::Own),
            ),
            None => (
                Pix::new(bbox, n + 1, true).ok(),
                plane(has_shape),
                plane(has_ga),
            ),
        };
        let mut state = self.child(dest, None, bbox);
        state.ga = ga;
        state.shape = shape;
        state.iso = true;
        state.tile = Some(Tile {
            area,
            xstep,
            ystep,
            ctm,
            id,
            encache: !hit,
        });
        self.stack.push(state);
        hit
    }

    fn end_tile(&mut self) {
        if self.overflow > 0 {
            self.overflow -= 1;
            return;
        }
        if self.stack.len() <= 1 {
            return;
        }
        let Some(state) = self.stack.pop() else {
            return;
        };
        let (Some(t), Some(cell)) = (state.tile, state.dest) else {
            return;
        };
        let own = |g: Ga| match g {
            Ga::Own(p) => Some(p),
            _ => None,
        };
        let (cell_shape, cell_ga) = (own(state.shape), own(state.ga));
        let (mut xstep, mut ystep, ctm) = (t.xstep, t.ystep, t.ctm);
        // Steps of less than a subpixel double until they show.
        let threshold = 1.0f32 / (1 << ((8 + 1) >> 1)) as f32;
        let mut abandon = false;
        let mut i = 0;
        while i < 128 {
            let ttm = ctm.pre_translate(xstep, 0.0);
            if (ttm.e - ctm.e).abs() > threshold || (ttm.f - ctm.f).abs() > threshold {
                break;
            }
            xstep *= 2.0;
            i += 1;
        }
        if i == 128 {
            abandon = true;
            xstep = 1.0;
        }
        i = 0;
        while i < 128 {
            let ttm = ctm.pre_translate(0.0, ystep);
            if (ttm.e - ctm.e).abs() > threshold || (ttm.f - ctm.f).abs() > threshold {
                break;
            }
            ystep *= 2.0;
            i += 1;
        }
        if i == 128 {
            abandon = true;
            ystep = 1.0;
        }
        let inv = ctm.invert();
        let below = self.scissor();
        let scissor = Rect::new(
            below.x0 as f32,
            below.y0 as f32,
            below.x1 as f32,
            below.y1 as f32,
        )
        .expand(1.0)
        .transform(inv);
        let area = t.area.intersect(scissor);
        let tb = cell.bbox();
        let tile_tmp = Rect::new(tb.x0 as f32, tb.y0 as f32, tb.x1 as f32, tb.y1 as f32)
            .expand(1.0)
            .transform(inv);
        let extra_x = ((tile_tmp.x1 - tile_tmp.x0 - xstep) as i32).max(0) as f32;
        let extra_y = ((tile_tmp.y1 - tile_tmp.y0 - ystep) as i32).max(0) as f32;
        let x0 = ((area.x0 - 1.0 - tile_tmp.x0 - extra_x) / xstep).floor() as i32;
        let y0 = ((area.y0 - 1.0 - tile_tmp.y0 - extra_y) / ystep).floor() as i32;
        let x1 = ((area.x1 + 1.0 - tile_tmp.x0 + extra_x) / xstep).ceil() as i32;
        let y1 = ((area.y1 + 1.0 - tile_tmp.y0 + extra_y) / ystep).ceil() as i32;
        let mut m = ctm;
        m.e = cell.x as f32;
        m.f = cell.y as f32;
        if !abandon {
            // Where the cell repeats, a bound on the count that MuPDF does
            // not have.
            let mut at = Vec::new();
            let mut budget: u64 = 1 << 24;
            'rows: for y in y0..y1 {
                for x in x0..x1 {
                    if budget == 0 {
                        break 'rows;
                    }
                    budget -= 1;
                    let ttm = m.pre_translate(x as f32 * xstep, y as f32 * ystep);
                    let (cx, cy) = (ttm.e as i32, ttm.f as i32);
                    if cx > 0 && cx.wrapping_add(cell.w) < 0 {
                        continue;
                    }
                    if cy > 0 && cy.wrapping_add(cell.h) < 0 {
                        continue;
                    }
                    at.push((cx, cy));
                }
            }
            let repeat = |into: &mut Pix, cell: &Pix| {
                let mut copy = cell.clone();
                for &(x, y) in &at {
                    copy.x = x;
                    copy.y = y;
                    into.paint_pixmap_in(&copy, 255, below);
                }
            };
            let di = self.dest_index();
            if let Some(dest) = self.stack.get_mut(di).and_then(|s| s.dest.as_mut()) {
                repeat(dest, &cell);
            }
            // The cell's shape and group alpha onto those below.
            for (is_shape, plane) in [(true, &cell_shape), (false, &cell_ga)] {
                if let Some(p) = plane {
                    let below = self.take_plane(is_shape);
                    if let Some((i, mut g)) = below {
                        repeat(&mut g, p);
                        self.put_plane(is_shape, Some((i, g)));
                    }
                }
            }
        }
        if t.encache && t.id != 0 {
            let key = tile_key(t.id, ctm, cell_shape.is_some(), cell_ga.is_some());
            self.tiles.insert(key, (cell, cell_shape, cell_ga));
        }
    }

    fn set_default_colorspaces(&mut self, defaults: &crate::device::DefaultColorSpaces) {
        self.cc.defaults = defaults.clone();
    }

    fn pop_clip(&mut self) {
        if self.overflow > 0 {
            self.overflow -= 1;
            return;
        }
        if self.stack.len() <= 1 {
            return;
        }
        let Some(state) = self.stack.pop() else {
            return;
        };
        if let (Some(src), Some(mask)) = (state.dest, state.mask) {
            let di = self.dest_index();
            if let Some(dest) = self.stack.get_mut(di).and_then(|s| s.dest.as_mut()) {
                dest.paint_with_mask(&src, &mask);
            }
            if let Ga::Own(g) = state.shape {
                let mut parent = self.take_plane(true);
                if let Some((_, pg)) = parent.as_mut() {
                    pg.paint_over_with_mask(&g, &mask);
                }
                self.put_plane(true, parent);
            }
            if let Ga::Own(g) = state.ga {
                let mut parent = self.take_ga();
                if let Some((_, pg)) = parent.as_mut() {
                    pg.paint_over_with_mask(&g, &mask);
                }
                self.put_ga(parent);
            }
        }
    }
}

/// `fz_glyph`'s RLE form is kept for glyphs at least this large; smaller
/// ones are pixmaps, which paint into masks differently.
const RLE_THRESHOLD: i32 = 256;

impl DrawDevice<'_> {
    /// `draw_glyph`: `glyph` with its origin at (`xorig`, `yorig`),
    /// within `scissor`, in `color` into the painted pixmap, or as
    /// coverage into the mask of the top state when `color` is None.
    fn draw_glyph(
        &mut self,
        color: Option<&[u8]>,
        glyph: &Bitmap,
        xorig: i32,
        yorig: i32,
        scissor: IRect,
    ) {
        let gb = IRect::new(
            glyph.left + xorig,
            glyph.bottom + yorig,
            glyph.left + xorig + glyph.w,
            glyph.bottom + yorig + glyph.h,
        );
        let mut ga = if color.is_some() {
            self.take_ga()
        } else {
            None
        };
        let mut shape = if color.is_some() {
            self.take_plane(true)
        } else {
            None
        };
        let shapebva = [color.and_then(|c| c.last().copied()).unwrap_or(255)];
        let target = match color {
            Some(_) => {
                let di = self.dest_index();
                self.stack.get_mut(di).and_then(|s| s.dest.as_mut())
            }
            None => self.stack.last_mut().and_then(|s| s.mask.as_mut()),
        };
        let Some(dst) = target else {
            self.put_plane(true, shape);
            self.put_ga(ga);
            return;
        };
        let bbox = gb.intersect(scissor).intersect(dst.bbox());
        if bbox.is_empty() {
            self.put_plane(true, shape);
            self.put_ga(ga);
            return;
        }
        let w = glyph.w as usize;
        let rle = !(glyph.w <= 6 || glyph.w * glyph.h < RLE_THRESHOLD);
        for y in bbox.y0..bbox.y1 {
            let row = (y - gb.y0) as usize * w;
            let from = row + (bbox.x0 - gb.x0) as usize;
            let to = row + (bbox.x1 - gb.x0) as usize;
            let Some(cov) = glyph.data.get(from..to) else {
                continue;
            };
            match color {
                Some(c) => {
                    dst.span_with_color(bbox.x0, y, cov, c);
                    // The planes have no colorspace: MuPDF paints them
                    // with the mask painters, not the color ones.
                    if let Some((_, h)) = shape.as_mut() {
                        h.glyph_alpha_row(bbox.x0, y, cov, 255, rle);
                    }
                    if let Some((_, g)) = ga.as_mut() {
                        g.glyph_alpha_row(bbox.x0, y, cov, shapebva[0], rle);
                    }
                }
                None if rle => dst.glyph_mask_row(bbox.x0, y, cov),
                None => dst.span_mask(bbox.x0, y, cov),
            }
        }
        self.put_plane(true, shape);
        self.put_ga(ga);
    }

    /// The glyphs of `text` in `colorbv`, those too large for the glyph
    /// cache filled as paths.
    #[allow(clippy::too_many_arguments)]
    fn fill_text_spans(
        &mut self,
        text: &Text,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        params: ColorParams,
        colorbv: &[u8],
        scissor: IRect,
    ) {
        for span in &text.spans {
            let mut tm = span.trm;
            for item in &span.items {
                if item.gid < 0 {
                    continue;
                }
                tm.e = item.x;
                tm.f = item.y;
                let mut trm = tm.concat(ctm);
                let glyph = if span.font.t3.is_some() {
                    self.type3_glyph(&span.font, item.gid, &mut trm, scissor)
                } else {
                    self.glyphs.render(&span.font, item.gid, &mut trm)
                };
                match glyph {
                    Some(g) => {
                        let (x, y) = (trm.e.floor() as i32, trm.f.floor() as i32);
                        self.draw_glyph(Some(colorbv), &g, x, y, scissor);
                    }
                    None => {
                        if let Some(path) = outline_glyph(&span.font, item.gid, tm) {
                            self.fill_path(&path, false, ctm, cs, color, alpha, params);
                        }
                    }
                }
            }
        }
    }

    /// A path filled white into the top state's mask, as
    /// `fz_draw_clip_text` fills a glyph it cannot render.
    /// The state `fz_draw_clip_text` and `fz_draw_clip_stroke_text` push
    /// for text bounded by `area`: a mask for the glyphs and a copy of
    /// the pixmap. False when there is nothing to draw the glyphs into.
    fn push_text_clip(&mut self, area: Rect, scissor: Rect) -> bool {
        let mut bbox = IRect::from_rect(area).intersect(self.scissor());
        if !scissor.is_infinite() {
            bbox = bbox.intersect(IRect::from_rect(scissor));
        }
        let di = self.dest_index();
        let (n, alpha) = match self.stack.get(di).and_then(|s| s.dest.as_ref()) {
            Some(d) => (d.n, d.alpha),
            None => (3, false),
        };
        let pixmaps = (Pix::new(bbox, 1, true), Pix::new(bbox, n, alpha));
        let (Ok(mask), Ok(mut dest)) = pixmaps else {
            let st = self.child(None, None, IRect::EMPTY);
            self.stack.push(st);
            return false;
        };
        if let Some(parent) = self.stack.get(di).and_then(|s| s.dest.as_ref()) {
            if alpha {
                dest.clear_with(0);
            } else {
                dest.copy_rect(parent, bbox);
            }
        }
        let mut state = self.child(Some(dest), Some(mask), bbox);
        state.ga = self.clip_ga(bbox, false);
        state.shape = self.clip_plane(true, bbox, false);
        state.iso = true;
        self.stack.push(state);
        !bbox.is_empty()
    }

    /// The fallback of `fz_draw_clip_stroke_text`: a glyph's outline
    /// stroked into the clip's mask.
    fn stroke_mask_path(&mut self, path: &Path, stroke: &StrokeState, ctm: Matrix) {
        let (flat, expansion) = flatness(ctm.expansion());
        let mut linewidth = stroke.linewidth;
        if linewidth * expansion < AA_LEVEL {
            linewidth = AA_LEVEL / expansion;
        }
        let Some(state) = self.stack.last_mut() else {
            return;
        };
        let Some(mask) = state.mask.as_mut() else {
            return;
        };
        let bbox = mask.bbox().intersect(state.scissor);
        if flatten_stroke(&mut self.gel, path, stroke, ctm, flat, linewidth, bbox).is_none() {
            return;
        }
        let clip = mask.bbox();
        self.gel.convert(false, clip, &mut |x, y, m| {
            mask.span_with_color(x, y, m, &[255])
        });
    }

    fn fill_mask_path(&mut self, path: &Path, ctm: Matrix) {
        let (flat, _) = flatness(ctm.expansion());
        let Some(state) = self.stack.last_mut() else {
            return;
        };
        let Some(mask) = state.mask.as_mut() else {
            return;
        };
        let bbox = mask.bbox().intersect(state.scissor);
        if flatten_fill(&mut self.gel, path, ctm, flat, bbox).is_none() {
            return;
        }
        let clip = mask.bbox();
        self.gel.convert(false, clip, &mut |x, y, m| {
            mask.span_with_color(x, y, m, &[255])
        });
    }

    /// `fz_get_pixmap_from_image` (with `mask`, the alpha-only form of
    /// `fz_get_pixmap_mask_from_image`): the image decoded and subsampled
    /// for `ctm`, its colorspace, and the size it shows at.
    fn decode(
        &mut self,
        image: &Image,
        ctm: Matrix,
        mask: bool,
    ) -> Option<(Pix, Option<ColorSpace>, i32, i32)> {
        let (dw, dh, l2) = extent_and_factor(image.width, image.height, ctm);
        let key = match &image.stream {
            Some(crate::object::Object::Ref(r)) => Some((r.num, r.generation, l2, mask)),
            _ => None,
        };
        if let Some(k) = key
            && let Some((pix, cs)) = self.images.get(&k)
        {
            return Some((pix.clone(), cs.clone(), dw, dh));
        }
        let out = self.decode_imp(image, ctm, mask)?;
        if let Some(k) = key {
            let size = out.0.samples.len();
            // A bound on what is kept, MuPDF's store being bounded too.
            if self.image_bytes + size <= 1 << 29 {
                self.image_bytes += size;
                self.images.insert(k, (out.0.clone(), out.1.clone()));
            }
        }
        Some(out)
    }

    fn decode_imp(
        &self,
        image: &Image,
        ctm: Matrix,
        mask: bool,
    ) -> Option<(Pix, Option<ColorSpace>, i32, i32)> {
        let doc = self.doc?;
        let (dw, dh, l2) = extent_and_factor(image.width, image.height, ctm);
        let (decoded, l2) = match crate::pixmap::from_image_subsampled(doc, image, l2) {
            Ok(p) => p,
            Err(e) => {
                doc.diagnostics()
                    .warn("render.image", crate::diag::report::strip(&e).to_string());
                return None;
            }
        };
        let mut pix = Pix {
            x: 0,
            y: 0,
            w: decoded.w as i32,
            h: decoded.h as i32,
            n: decoded.n,
            alpha: decoded.alpha,
            samples: decoded.samples,
        };
        subsample(&mut pix, l2);
        let mut cs = decoded.colorspace;
        if mask && !(pix.n == 1 && pix.alpha) {
            // The first component as alpha.
            let n = pix.n;
            pix.samples = pix.samples.chunks(n).map(|p| p[0]).collect();
            pix.n = 1;
            pix.alpha = true;
            cs = None;
        }
        Some((pix, cs, dw, dh))
    }

    /// `fz_convert_pixmap` to RGB: through the color converter, MuPDF's
    /// fast formula for CMYK when color management is off.
    fn to_rgb(&self, pix: &Pix, cs: Option<&ColorSpace>) -> Pix {
        let Some(cs) = cs else {
            return pix.clone();
        };
        let cs = self.cc.default_of(cs);
        let sa = usize::from(pix.alpha);
        let sn = pix.n - sa;
        let mut out = Pix {
            x: pix.x,
            y: pix.y,
            w: pix.w,
            h: pix.h,
            n: 3 + sa,
            alpha: pix.alpha,
            samples: Vec::with_capacity(pix.w as usize * pix.h as usize * (3 + sa)),
        };
        let fast_cmyk = !self.icc()
            && cs.family() == Some(crate::colorspace::Family::Cmyk)
            && !matches!(cs, ColorSpace::Separation { .. });
        let div255 = |c: i32, a: i32| if a == 0 { 0 } else { (c * 255 + a / 2) / a };
        let mut cache: std::collections::HashMap<Vec<u8>, [u8; 3]> =
            std::collections::HashMap::new();
        for p in pix.samples.chunks(pix.n) {
            let a = if sa == 1 { i32::from(p[sn]) } else { 255 };
            let un = |v: u8| {
                if sa == 1 {
                    div255(i32::from(v), a)
                } else {
                    i32::from(v)
                }
            };
            let rgb = if fast_cmyk && sn == 4 {
                // fast_cmyk_to_rgb.
                let (c, m, y, k) = (un(p[0]), un(p[1]), un(p[2]), un(p[3]));
                [
                    (255 - (c + k).min(255)) as u8,
                    (255 - (m + k).min(255)) as u8,
                    (255 - (y + k).min(255)) as u8,
                ]
            } else {
                let key: Vec<u8> = p[..sn].iter().map(|&v| un(v) as u8).collect();
                *cache.entry(key).or_insert_with_key(|k| {
                    let f: Vec<f32> = k.iter().map(|&v| f32::from(v) / 255.0).collect();
                    let c = self.cc.rgb(self.doc, cs, &f, self.ri(1));
                    c.map(|v| (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8)
                })
            };
            for v in rgb {
                out.samples.push(if sa == 1 {
                    mul255(i32::from(v), a) as u8
                } else {
                    v
                });
            }
            if sa == 1 {
                out.samples.push(a as u8);
            }
        }
        out
    }

    /// `fz_render_glyph` for a Type 3 font: the glyph's procedure run into
    /// a coverage pixmap of its own (`fz_render_t3_glyph_pixmap`), kept for
    /// the page when it is small.
    fn type3_glyph(
        &mut self,
        font: &std::sync::Arc<crate::fontdesc::Font>,
        gid: i32,
        ctm: &mut Matrix,
        scissor: IRect,
    ) -> Option<std::rc::Rc<Bitmap>> {
        let doc = self.doc?;
        if !(0..256).contains(&gid) || self.depth > 8 {
            return None;
        }
        let (sub, size, qe, qf) = super::glyph::subpixel_adjust(ctm);
        let cacheable = size <= super::glyph::MAX_GLYPH_SIZE;
        let scissor = if cacheable {
            IRect::INFINITE
        } else {
            let (fe, ff) = (ctm.e.floor() as i32, ctm.f.floor() as i32);
            IRect::new(
                scissor.x0 - fe,
                scissor.y0 - ff,
                scissor.x1 - fe,
                scissor.y1 - ff,
            )
        };
        let key = self.glyphs.key(font, gid, sub, qe, qf);
        if cacheable && let Some(g) = self.glyphs.get(&key) {
            return g;
        }
        let bbox = IRect::from_rect(crate::fontdesc::bound_glyph(font, gid, sub).expand(1.0))
            .intersect(scissor);
        // Colored glyphs paint as masks here (their own colors are lost).
        let _ = crate::run::type3_glyph_colored(font, gid);
        let glyph = match Pix::new(bbox, 1, true) {
            Ok(pix) if !bbox.is_empty() => {
                let mut dev = DrawDevice::new(Some(doc), pix);
                dev.depth = self.depth + 1;
                dev.type3 = true;
                dev.cc = self.cc.clone();
                crate::run::run_type3_glyph(doc, font, gid, sub, &mut dev);
                let p = dev.into_pix();
                Some(std::rc::Rc::new(Bitmap {
                    left: p.x,
                    bottom: p.y,
                    w: p.w,
                    h: p.h,
                    data: p.samples,
                }))
            }
            _ => Some(std::rc::Rc::new(Bitmap {
                left: 0,
                bottom: 0,
                w: 0,
                h: 0,
                data: Vec::new(),
            })),
        };
        if cacheable {
            self.glyphs.put(key, glyph.clone());
        }
        glyph
    }

    fn dest_n(&self) -> usize {
        let di = self.dest_index();
        self.stack
            .get(di)
            .and_then(|s| s.dest.as_ref())
            .map_or(3, |d| d.n - usize::from(d.alpha))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn white(w: i32, h: i32) -> Pix {
        let mut p = Pix::new(IRect::new(0, 0, w, h), 3, false).unwrap_or_else(|e| panic!("{e}"));
        p.clear_with(255);
        p
    }

    fn px(p: &Pix, x: i32, y: i32) -> [u8; 3] {
        let i = (y as usize * p.w as usize + x as usize) * 3;
        [p.samples[i], p.samples[i + 1], p.samples[i + 2]]
    }

    #[test]
    fn fills_a_rectangle_in_color() {
        let mut dev = DrawDevice::new(None, white(10, 10));
        let mut path = Path::new();
        path.rect_to(2.0, 2.0, 6.0, 6.0);
        dev.fill_path(
            &path,
            false,
            Matrix::IDENTITY,
            &ColorSpace::Rgb,
            &[1.0, 0.0, 0.0],
            1.0,
            ColorParams::default(),
        );
        let p = dev.into_pix();
        assert_eq!(px(&p, 3, 3), [255, 0, 0]);
        assert_eq!(px(&p, 7, 7), [255, 255, 255]);
    }

    #[test]
    fn clips_left_open_are_closed_when_the_pixmap_is_taken() {
        let mut dev = DrawDevice::new(None, white(10, 10));
        let mut clip = Path::new();
        clip.move_to(0.0, 0.0);
        clip.line_to(10.0, 0.0);
        clip.line_to(0.0, 10.0);
        clip.close_path();
        dev.clip_path(&clip, false, Matrix::IDENTITY, Rect::INFINITE);
        dev.clip_path(&clip, true, Matrix::IDENTITY, Rect::INFINITE);
        assert!(format!("{dev:?}").contains("depth: 3"));
        let mut all = Path::new();
        all.rect_to(0.0, 0.0, 10.0, 10.0);
        let gray = ColorParams::default();
        dev.fill_path(
            &all,
            false,
            Matrix::IDENTITY,
            &ColorSpace::Gray,
            &[0.0],
            1.0,
            gray,
        );
        // No pop: the painting inside the clips still lands.
        let p = dev.into_pix();
        assert_eq!(px(&p, 1, 1), [0, 0, 0]);
        assert_eq!(px(&p, 8, 8), [255, 255, 255]);
    }

    #[test]
    fn text_clips_take_glyphs_too_big_to_cache_as_paths() {
        // A 300-point O clips as its outline (unhinted, where MuPDF's is
        // hinted, so pixels are sampled away from its edges).
        let pdf = page(
            200,
            200,
            "/Font<</H 5 0 R>>",
            "BT /H 300 Tf 7 Tr 60 -40 Td (O) Tj ET 1 0 0 rg 0 0 200 200 re f",
            &["<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>"],
        );
        let pix = kit::render("clip-big-glyph", &pdf, 1.0, true);
        assert_eq!(kit::px(&pix, 80, 100), [255, 0, 0]);
        assert_eq!(kit::px(&pix, 150, 100), [255, 255, 255]);
        assert_eq!(kit::px(&pix, 10, 100), [255, 255, 255]);
    }

    #[test]
    fn a_clip_limits_what_is_painted() {
        let mut dev = DrawDevice::new(None, white(10, 10));
        let mut tri = Path::new();
        tri.move_to(0.0, 0.0);
        tri.line_to(10.0, 0.0);
        tri.line_to(0.0, 10.0);
        tri.close_path();
        dev.clip_path(&tri, false, Matrix::IDENTITY, Rect::INFINITE);
        let mut all = Path::new();
        all.rect_to(0.0, 0.0, 10.0, 10.0);
        dev.fill_path(
            &all,
            false,
            Matrix::IDENTITY,
            &ColorSpace::Gray,
            &[0.0],
            1.0,
            ColorParams::default(),
        );
        dev.pop_clip();
        let p = dev.into_pix();
        assert_eq!(px(&p, 1, 1), [0, 0, 0]);
        assert_eq!(px(&p, 8, 8), [255, 255, 255]);
    }

    use crate::draw::testkit::{self as kit, check, page, stream};

    /// A page filled red through a luminosity soft mask whose group
    /// (gray, over a backdrop of `bc`) draws `mask`.
    fn masked_red(mask: &str, bc: &str, extra: &[&str]) -> Vec<u8> {
        let group = stream(
            "/Type/XObject/Subtype/Form/BBox[0 0 64 64]/Group<</S/Transparency/CS/DeviceGray>>/Resources<</Shading<</S 7 0 R>>/XObject<</I 7 0 R>>>>",
            mask,
        );
        let gs = format!("<</SMask<</S/Luminosity/G 6 0 R/BC[{bc}]>>>>");
        let mut objs = vec![gs.as_str(), group.as_str()];
        objs.extend_from_slice(extra);
        page(
            64,
            64,
            "/ExtGState<</G 5 0 R>>",
            "/G gs 1 0 0 rg 0 0 64 64 re f",
            &objs,
        )
    }

    #[test]
    fn soft_masks_convert_colors_with_mupdfs_postscript_profiles() {
        // With ICC on, MuPDF draws a soft mask's device colors through its
        // ps_gray, ps_rgb and ps_cmyk profiles (linear, luminance as
        // PostScript has it), not through the page's: pure red is 0.31
        // there, 0.51 through sRGB to gray. nupdf used the page's.
        let pdf = masked_red(
            "0.2 0.5 0.1 0.1 k 0 0 32 32 re f 0.4 g 32 0 32 32 re f 0 1 0 rg 0 32 32 32 re f 1 0 0 rg 32 32 32 32 re f",
            "0.2",
            &[],
        );
        let pix = check(
            "smask-luminosity-icc",
            &pdf,
            true,
            "3261f0f028aaee3f9b48b21e0bcc7e50",
        );
        // Red through red's luminance, 0.31: (255, 176, 176).
        assert_eq!(kit::px(&pix, 48, 16), [255, 176, 176]);
        check(
            "smask-luminosity-noicc",
            &pdf,
            false,
            "fc04b2e5a0f6ec9db1bd1a646c552bb5",
        );
    }

    const MODES: [&str; 16] = [
        "Normal",
        "Multiply",
        "Screen",
        "Overlay",
        "Darken",
        "Lighten",
        "ColorDodge",
        "ColorBurn",
        "HardLight",
        "SoftLight",
        "Difference",
        "Exclusion",
        "Hue",
        "Saturation",
        "Color",
        "Luminosity",
    ];

    /// `/B0` to `/B15`: each blend mode at fill alpha `ca`.
    fn blend_states(ca: &str) -> String {
        let mut s = String::from("/ExtGState<<");
        for (i, m) in MODES.iter().enumerate() {
            s.push_str(&format!("/B{i}<</BM/{m}/ca {ca}>>"));
        }
        s.push_str(">>");
        s
    }

    /// A 128-point page of 16 cells, one a blend mode: each a backdrop
    /// of two colors, then `paint` (with `{x}`, `{y}` its corner) under
    /// the cell's blend mode.
    fn blend_page(name: &str, ca: &str, paint: &str, res: &str, extra: &[&str], want: &str) {
        let mut content = String::new();
        for i in 0..16 {
            let (x, y) = ((i % 4) * 32, (i / 4) * 32);
            content.push_str(&format!(
                "1 0.5 0 rg {x} {y} 16 32 re f 0.2 0.3 0.8 rg {} {y} 16 32 re f q /B{i} gs {} Q\n",
                x + 16,
                paint
                    .replace("{x}", &x.to_string())
                    .replace("{y}", &y.to_string())
                    .replace("{x4}", &(x + 4).to_string())
                    .replace("{y4}", &(y + 4).to_string())
                    .replace("{x10}", &(x + 10).to_string())
                    .replace("{y10}", &(y + 10).to_string()),
            ));
        }
        let res = format!("{}{res}", blend_states(ca));
        check(name, &page(128, 128, &res, &content, extra), true, want);
    }

    #[test]
    fn every_blend_mode_over_a_backdrop() {
        blend_page(
            "blend-objects",
            "0.75",
            "0.2 0.8 0.4 rg {x4} {y4} 24 24 re f 0.9 0.1 0.6 rg {x10} {y10} 12 12 re f",
            "",
            &[],
            "81166c0b5a40b88bf3d37a46eea5afd6",
        );
    }

    #[test]
    fn blend_modes_composite_isolated_and_non_isolated_groups() {
        // The same group, isolated then not, blended into each cell.
        for (name, iso, want) in [
            (
                "blend-group-isolated",
                "true",
                "265b03b54961f84da04249cfbc631374",
            ),
            (
                "blend-group-nonisolated",
                "false",
                "3b9f313bde7f1b9a41bab7100a5d437c",
            ),
        ] {
            let form = stream(
                &format!(
                    "/Type/XObject/Subtype/Form/BBox[0 0 32 32]/Group<</S/Transparency/I {iso}>>/Resources<</ExtGState<</H<</ca 0.6/BM/Multiply>>>>>>"
                ),
                "0.2 0.8 0.4 rg 4 4 20 20 re f /H gs 0.9 0.1 0.6 rg 10 10 18 18 re f",
            );
            blend_page(
                name,
                "0.9",
                "1 0 0 1 {x} {y} cm /G Do",
                "/XObject<</G 5 0 R>>",
                &[&form],
                want,
            );
        }
    }

    #[test]
    fn knockout_groups_replace_what_they_painted() {
        // Inside a knockout group each object paints over the group's
        // backdrop, not over the objects before it: translucent shapes,
        // a shading and an image, in isolated and non-isolated groups.
        let form = |iso: &str| {
            stream(
                &format!(
                    "/Type/XObject/Subtype/Form/BBox[0 0 64 64]/Group<</S/Transparency/K true/I {iso}>>/Resources<</ExtGState<</H<</ca 0.5>>/M<</ca 0.7/BM/Multiply>>>>/Shading<</S 7 0 R>>/XObject<</I 8 0 R>>>>"
                ),
                "/H gs 1 0 0 rg 4 4 36 36 re f 0 0 1 rg 20 20 36 36 re f
                 /M gs 0 1 0 rg 12 30 30 30 re f
                 q 4 0 0 30 30 2 cm /I Do Q
                 q 40 40 20 20 re W n /S sh Q",
            )
        };
        let content = "0.9 0.9 0.2 rg 0 0 128 128 re f 0.3 g 0 0 64 128 re f
                       q /A gs /K1 Do Q q 1 0 0 1 0 64 cm /K2 Do Q
                       q 1 0 0 1 64 32 cm /A gs /K2 Do Q";
        let k1 = form("true");
        let k2 = form("false");
        let shading = "<</ShadingType 2/ColorSpace/DeviceRGB/Coords[40 0 60 0]/Function<</FunctionType 2/Domain[0 1]/C0[1 1 0]/C1[0 1 1]/N 1>>>>";
        let image = kit::hex_stream(
            "/Type/XObject/Subtype/Image/Width 2/Height 2/ColorSpace/DeviceRGB/BitsPerComponent 8",
            &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255],
        );
        let pdf = page(
            128,
            128,
            "/ExtGState<</A<</ca 0.8/BM/Screen>>>>/XObject<</K1 5 0 R/K2 6 0 R>>",
            content,
            &[&k1, &k2, shading, &image],
        );
        check(
            "blend-knockout",
            &pdf,
            true,
            "045fb31560b0d7cf82b4dbc5eaac72cd",
        );
    }

    #[test]
    fn alpha_soft_masks_with_transfer_functions_and_backdrops() {
        let mask = stream(
            "/Type/XObject/Subtype/Form/BBox[8 8 120 120]/Group<</S/Transparency>>/Resources<</ExtGState<</H<</ca 0.4>>>>>>",
            "0 g 16 16 60 60 re f /H gs 60 60 50 50 re f",
        );
        let lum = stream(
            "/Type/XObject/Subtype/Form/BBox[0 0 128 128]/Group<</S/Transparency/CS/DeviceRGB>>",
            "1 1 1 rg 0 64 64 64 re f 0 0 1 rg 64 64 64 64 re f",
        );
        let pdf = page(
            128,
            128,
            "/ExtGState<</A 5 0 R/T 6 0 R/L 7 0 R>>",
            "0 0.6 0 rg q /A gs 0 0 128 64 re f Q q /T gs 1 0 0 rg 0 0 64 128 re f Q
             q /L gs 0.5 0 0.5 rg 0 64 128 64 re f Q",
            &[
                "<</SMask<</S/Alpha/G 8 0 R>>>>",
                "<</SMask<</S/Alpha/G 8 0 R/TR<</FunctionType 2/Domain[0 1]/C0[1]/C1[0]/N 1>>>>>>",
                "<</SMask<</S/Luminosity/G 9 0 R/BC[0.5 0.5 0.5]>>/CA 0.5>>",
                &mask,
                &lum,
            ],
        );
        check(
            "smask-alpha-transfer",
            &pdf,
            true,
            "a5b8f5a5c866ac325ef5c0222aa9ee32",
        );
    }

    #[test]
    fn clips_nest_and_fill_by_either_rule() {
        // A rectangle clip, an even-odd clip inside it, a non-zero
        // triangle; a rotated clip; an empty one hides everything.
        let pdf = page(
            128,
            128,
            "",
            "q 8 8 112 112 re W n
               q 20 20 80 80 re 40 40 40 40 re W* n 0 0 1 rg 0 0 128 128 re f
                 q 20 20 m 110 64 l 20 110 l h W n 1 0 0 rg 0 0 128 128 re f Q
               Q
               q 0.9 0.43 -0.43 0.9 64 0 cm 10 60 m 60 60 l 35 100 l h W n 0 1 0 rg 0 0 128 128 re f Q
             Q
             q 0 0 m 0 0 l W n 0 g 0 0 128 128 re f Q
             q 200 200 10 10 re W n 0 g 0 0 128 128 re f Q",
            &[],
        );
        check(
            "clip-nested",
            &pdf,
            true,
            "dc9fe1313e602905381214b1aae275b1",
        );
    }

    #[test]
    fn a_page_of_everything_draws_as_mupdf_draws_it() {
        // Text in every render mode with a Type 3 font among them, pattern
        // strokes, stencils in a pattern, tiles from the cache, layers.
        check(
            "sink",
            &kit::sink(),
            true,
            "52c0e8802b9702cc6bc3fae69dbbcf65",
        );
        check(
            "sink-noicc",
            &kit::sink(),
            false,
            "52c0e8802b9702cc6bc3fae69dbbcf65",
        );
    }

    #[test]
    fn translucent_patterns_paint_through_their_group() {
        // A pattern filled or stroked at an alpha below 1 runs in a
        // non-isolated group, whose group alpha the tiles must paint too:
        // nupdf's did not, so the group composited as nothing.
        let tile = stream(
            "/PatternType 1/PaintType 1/TilingType 1/BBox[0 0 8 8]/XStep 8/YStep 8/Resources<<>>",
            "1 0 0 rg 0 0 4 4 re f 0 0 1 rg 4 4 4 4 re f",
        );
        let pdf = page(
            64,
            64,
            "/Pattern<</T 5 0 R>>/ExtGState<</A<</ca 0.5/CA 0.5>>/M<</BM/Multiply/ca 0.7>>>>",
            "0 1 0 rg 0 0 64 20 re f
             q /A gs /Pattern cs /T scn 4 4 40 24 re f /Pattern CS /T SCN 6 w 8 40 m 56 56 l S Q
             q /M gs /Pattern cs /T scn 30 10 30 40 re f Q",
            &[&tile],
        );
        let pix = check(
            "pattern-alpha",
            &pdf,
            true,
            "65d703989a6c90f48add72529ca13123",
        );
        assert_ne!(kit::px(&pix, 30, 50), [255, 255, 255]);
    }

    #[test]
    fn text_stroked_in_a_shading_clips_to_its_strokes() {
        // The draw device did not clip to stroked text: the shading
        // filled everything and the pop took the clip below with it.
        // MuPDF strokes these glyphs with FreeType, a level or so off
        // nupdf's outlines, so pixels are sampled where both agree.
        let pdf =
            kit::sink_with("q /Pattern CS /Sh SCN 3 w BT /F1 60 Tf 1 Tr 100 30 Td (I) Tj ET Q");
        let pix = kit::render("clip-stroke-text", &pdf, 1.0, true);
        let page = kit::render("sink", &kit::sink(), 1.0, true);
        // On the stroke: the shading, blue more than red.
        let on = kit::px(&pix, 105, 78);
        assert!(on[2] > on[0] + 100, "{on:?}");
        // Inside the stem, beside it and elsewhere, the page as it was.
        for (x, y) in [(108, 78), (120, 78), (10, 110), (60, 20), (64, 64)] {
            assert_eq!(kit::px(&pix, x, y), kit::px(&page, x, y), "at {x}, {y}");
        }
    }

    #[test]
    fn everything_inside_translucent_non_isolated_groups() {
        // The sink's objects drawn again inside groups that keep a group
        // alpha and a shape: a non-isolated group, a knockout one inside
        // it, clips and soft masks within, at alphas below 1.
        let outer = stream(
            "/Type/XObject/Subtype/Form/BBox[0 0 128 128]/Group<</S/Transparency/I false>>/Resources<</ExtGState<</A<</ca 0.6/CA 0.6>>/M 14 0 R/X<</BM/Screen/ca 0.8>>>>/Shading<</S 11 0 R>>/Pattern<</T1 12 0 R/Sh<</PatternType 2/Shading 11 0 R>>>>/Font<</F1 5 0 R/F3 6 0 R>>/XObject<</Im 9 0 R/St 10 0 R/In 19 0 R>>>>",
            "q 10 10 100 100 re W n 0.5 0 1 rg 0 0 128 128 re f
               q 30 30 m 100 40 l 60 110 l h W n /A gs /S sh Q
               q /A gs 20 0 0 20 4 50 cm /Im Do Q
               q /A gs 0.2 0.6 0.2 rg 20 0 0 20 28 50 cm /St Do Q
               q /A gs /Pattern cs /Sh scn 20 0 0 20 52 50 cm /St Do Q
               q /A gs /Pattern cs /T1 scn 76 50 20 20 re f Q
               q /A gs BT /F1 18 Tf 4 90 Td (Grp) Tj /F3 1 Tf 12 0 0 12 60 90 Tm (ab) Tj ET Q
               q /M gs 0 0 1 rg 40 76 40 20 re f Q
               q /X gs /In Do Q
             Q",
        );
        let inner = stream(
            "/Type/XObject/Subtype/Form/BBox[0 0 128 128]/Group<</S/Transparency/K true/I false>>/Resources<</ExtGState<</H<</ca 0.5>>>>/Shading<</S 11 0 R>>>>",
            "/H gs 1 0 0 rg 60 4 40 30 re f 0 1 0 rg 80 14 40 30 re f
             q 70 0 50 50 re W n /S sh Q",
        );
        let mut pdf = String::from_utf8_lossy(&kit::sink_with("q /G gs /Out Do Q")).into_owned();
        pdf = pdf.replace("/Fm 16 0 R>>", "/Fm 16 0 R/Out 18 0 R>>");
        pdf = pdf.replace("/Hs<</CA 0.5>>", "/Hs<</CA 0.5>>/G<</ca 0.7/BM/Multiply>>");
        // The two forms as objects 18 and 19, after the sink's 17.
        let mut objs: Vec<String> = Vec::new();
        let body = &pdf[..pdf.find("xref\n").unwrap_or(pdf.len())];
        for part in body.split(" 0 obj\n").skip(1) {
            if let Some(end) = part.find("\nendobj\n") {
                objs.push(part[..end].to_string());
            }
        }
        objs.push(outer);
        objs.push(inner);
        let refs: Vec<&str> = objs.iter().map(String::as_str).collect();
        let pdf = kit::pdf_of(&refs);
        check(
            "groups-deep",
            &pdf,
            true,
            "1244237e080f712516537fe662f8e703",
        );
    }

    #[test]
    fn knockout_clips_small_masks_cmyk_images_and_fine_tiles() {
        // A clip in a knockout group (its shape; text there comes out a
        // level apart from MuPDF, so it is left out), a soft mask
        // small enough for its transfer function to run per pixel, a
        // CMYK image shrunk (converted before it is scaled), and a tile
        // whose steps are too fine to show, which is abandoned.
        let ko = stream(
            "/Type/XObject/Subtype/Form/BBox[0 0 128 128]/Group<</S/Transparency/K true>>/Resources<</Font<</F 9 0 R>>/ExtGState<</H<</ca 0.5>>/O<</ca 1>>>>>>",
            "/H gs 1 0 0 rg 4 4 60 30 re f
             /O gs 0 0 1 rg 8 10 30 10 re f /H gs
             q 30 0 40 40 re W n 0 1 0 rg 20 4 40 30 re f Q",
        );
        let mask = stream(
            "/Type/XObject/Subtype/Form/BBox[70 70 90 90]/Group<</S/Transparency>>",
            "0 g 70 70 10 20 re f 0.5 g 80 70 10 20 re f",
        );
        let cmyk: Vec<u8> = (0..50u32 * 37)
            .flat_map(|i| {
                [
                    (i % 50 * 5) as u8,
                    (i / 50 * 7) as u8,
                    60,
                    (i % 7 * 20) as u8,
                ]
            })
            .collect();
        let image = kit::hex_stream(
            "/Type/XObject/Subtype/Image/Width 50/Height 37/ColorSpace/DeviceCMYK/BitsPerComponent 8",
            &cmyk,
        );
        let fine = stream(
            "/PatternType 1/PaintType 1/TilingType 1/BBox[0 0 1 1]/XStep 0.00001/YStep 0.00001/Resources<<>>",
            "1 0 0 rg 0 0 1 1 re f",
        );
        let pdf = page(
            128,
            128,
            "/XObject<</K 5 0 R/I 7 0 R>>/ExtGState<</M<</SMask<</S/Alpha/G 6 0 R/TR<</FunctionType 2/Domain[0 1]/C0[0.2]/C1[1]/N 2>>>>>>>>/Pattern<</P 8 0 R>>",
            "0.9 0.9 0.5 rg 0 0 128 128 re f /K Do
             q /M gs 0 0 0.5 rg 60 60 40 40 re f Q
             q 30 0 0 22 4 90 cm /I Do Q
             q /Pattern cs /P scn 100 4 20 20 re f Q",
            &[
                &ko,
                &mask,
                &image,
                &fine,
                "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
            ],
        );
        check(
            "device-edges",
            &pdf,
            false,
            "1a18641729f678f1a2111b0822c5c513",
        );
    }

    #[test]
    fn groups_nest_isolated_in_non_isolated_and_back() {
        // A non-isolated group holding an isolated one and a non-isolated
        // one, each translucent and blended, the inner ones holding
        // translucent shapes and a blended one of their own: the group
        // alphas below take each one's alpha as it ends.
        let form = |n: i32, iso: &str, content: &str| {
            stream(
                &format!(
                    "/Type/XObject/Subtype/Form/BBox[0 0 128 128]/Group<</S/Transparency/I {iso}>>/Resources<</XObject<</A 6 0 R/B 7 0 R/C 8 0 R>>/ExtGState<</H<</ca 0.5/BM/Screen>>/D<</ca 0.7/BM/Darken>>/L<</ca 0.6/BM/Luminosity>>>>>>"
                ),
                &format!("% {n}\n{content}"),
            )
        };
        let outer = form(
            5,
            "false",
            "0 0.5 0 rg 10 10 108 108 re f
             q /H gs /A Do Q q /D gs /B Do Q q /L gs /C Do Q",
        );
        let a = form(
            6,
            "true",
            "/H gs 1 0 0 rg 20 20 50 50 re f 0 0 1 rg 40 40 50 50 re f",
        );
        let b = form(
            7,
            "false",
            "/D gs 1 1 0 rg 60 20 50 40 re f /L gs 0 1 1 rg 70 30 40 50 re f",
        );
        let c = form(8, "false", "0.8 0.3 0.9 rg 30 70 60 40 re f");
        let pdf = page(
            128,
            128,
            "/XObject<</O 5 0 R>>/ExtGState<</M<</BM/Multiply/ca 0.8>>>>",
            "0.9 0.6 0.3 rg 0 0 128 64 re f q /M gs /O Do Q",
            &[&outer, &a, &b, &c],
        );
        check(
            "groups-nested",
            &pdf,
            true,
            "cfd7bd825f4d4d3a87a110000ff4e03d",
        );
    }

    #[test]
    fn blend_modes_over_nothing_over_gray_and_in_masks() {
        // Each blend mode inside an isolated group, half over the group's
        // own paint and half over nothing; over a gray backdrop (where
        // saturation has none to keep); and inside a luminosity mask,
        // where the group is gray.
        let mut iso = String::from("0.3 0.6 0.9 rg 0 0 128 32 re f\n");
        let mut gray = String::new();
        for (i, _) in MODES.iter().enumerate() {
            let x = i * 8;
            iso.push_str(&format!("q /B{i} gs 0.9 0.4 0.1 rg {x} 16 8 32 re f Q\n"));
            gray.push_str(&format!("q /B{i} gs 0.2 0.7 0.4 rg {x} 52 8 12 re f Q\n"));
        }
        let res = format!("/Resources<<{}>>", blend_states("1"));
        let group = stream(
            &format!(
                "/Type/XObject/Subtype/Form/BBox[0 0 128 48]/Group<</S/Transparency/I true>>{res}"
            ),
            &iso,
        );
        let mask = stream(
            &format!(
                "/Type/XObject/Subtype/Form/BBox[0 64 128 128]/Group<</S/Transparency/CS/DeviceGray>>{res}"
            ),
            &(0..16)
                .map(|i| {
                    format!(
                        "{} g {} 64 8 64 re f q /B{i} gs 0.8 g {} 80 8 30 re f Q\n",
                        f64::from(i) / 16.0,
                        i * 8,
                        i * 8
                    )
                })
                .collect::<String>(),
        );
        let states = blend_states("0.9");
        let inner = &states["/ExtGState<<".len()..states.len() - 2];
        let pdf = page(
            128,
            128,
            &format!(
                "/XObject<</G 5 0 R>>/ExtGState<</M<</SMask<</S/Luminosity/G 6 0 R>>>>{inner}>>"
            ),
            &format!("/G Do 0.5 g 0 48 128 20 re f {gray} q /M gs 1 0 0 rg 0 64 128 64 re f Q"),
            &[&group, &mask],
        );
        check(
            "blend-edges",
            &pdf,
            true,
            "e4a7d6ab178c2b97e00847800352e2da",
        );
    }
}
