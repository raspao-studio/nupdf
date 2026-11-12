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
        }
    }
}

pub struct DrawDevice<'d> {
    doc: Option<&'d Document>,
    gel: Gel,
    glyphs: GlyphCache,
    tiles: std::collections::HashMap<(i32, [u32; 4]), Pix>,
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
        if self.full() {
            return;
        }
        let mut bbox = IRect::from_rect(text.bound(None, ctm)).intersect(self.scissor());
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
            return;
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
        if bbox.is_empty() {
            return;
        }
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
            super::shade::paint_shade(doc, &self.cc, shade, ctm, target, bbox);
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
                && (!self.cc.icc
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
                image.interpolate,
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
                image.interpolate,
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
            image.interpolate,
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
                let bc = self
                    .cc
                    .gray(self.doc, cs.unwrap_or(&ColorSpace::Gray), backdrop, 1);
                d.clear_with((bc * 255.0) as i32 as u8);
                d
            })
        } else {
            Pix::new(bbox, 1, true)
        };
        let mut state = State::new(dest.ok(), None, bbox);
        state.ga = Ga::Off;
        state.shape = Ga::Off;
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
        if let Some(dest) = self.stack.get_mut(di).and_then(|s| s.dest.as_mut()) {
            match &own {
                // The same group alpha above and below: a plain paint.
                None if mode == 0 && parent_ga.is_none() => dest.paint_pixmap(&src, a),
                None => dest.blend_pixmap(&src, a, mode),
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
        let key = (
            id,
            [
                ctm.a.to_bits(),
                ctm.b.to_bits(),
                ctm.c.to_bits(),
                ctm.d.to_bits(),
            ],
        );
        let cached = if id != 0 {
            self.tiles.get(&key).cloned()
        } else {
            None
        };
        let hit = cached.is_some();
        let n = self.dest_n();
        let dest = cached.or_else(|| Pix::new(bbox, n + 1, true).ok());
        let mut state = self.child(dest, None, bbox);
        state.ga = Ga::Off;
        state.shape = Ga::Off;
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
            let di = self.dest_index();
            if let Some(dest) = self.stack.get_mut(di).and_then(|s| s.dest.as_mut()) {
                let mut copy = cell.clone();
                // A bound on the repeats, which MuPDF does not have.
                let mut budget: u64 = 1 << 24;
                'rows: for y in y0..y1 {
                    for x in x0..x1 {
                        if budget == 0 {
                            break 'rows;
                        }
                        budget -= 1;
                        let ttm = m.pre_translate(x as f32 * xstep, y as f32 * ystep);
                        copy.x = ttm.e as i32;
                        copy.y = ttm.f as i32;
                        if copy.x > 0 && copy.x.wrapping_add(copy.w) < 0 {
                            continue;
                        }
                        if copy.y > 0 && copy.y.wrapping_add(copy.h) < 0 {
                            continue;
                        }
                        dest.paint_pixmap_in(&copy, 255, below);
                    }
                }
            }
        }
        if t.encache && t.id != 0 {
            let key = (
                t.id,
                [
                    ctm.a.to_bits(),
                    ctm.b.to_bits(),
                    ctm.c.to_bits(),
                    ctm.d.to_bits(),
                ],
            );
            self.tiles.insert(key, cell);
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
                    if let Some((_, h)) = shape.as_mut() {
                        h.span_with_color(bbox.x0, y, cov, &[255]);
                    }
                    if let Some((_, g)) = ga.as_mut() {
                        g.span_with_color(bbox.x0, y, cov, &shapebva);
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
        let decoded = match crate::pixmap::from_image(doc, image) {
            Ok(p) => p,
            Err(e) => {
                doc.diagnostics()
                    .warn("render.image", crate::diag::report::strip(&e).to_string());
                return None;
            }
        };
        let (dw, dh, l2) = extent_and_factor(image.width, image.height, ctm);
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
        let fast_cmyk = !self.cc.icc
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
                    let c = self.cc.rgb(self.doc, cs, &f, 1);
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
}
