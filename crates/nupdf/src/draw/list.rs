// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/list-device.c,
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! `fz_display_list`: a page's device calls kept, to be played again, as
//! `mutool draw` keeps a page to render it band by band on several threads.

use crate::colorspace::ColorSpace;
use crate::device::{ColorParams, DefaultColorSpaces, Device, Metatext};
use crate::function::Function;
use crate::geometry::{Matrix, Rect};
use crate::image::Image;
use crate::path::{Path, StrokeState};
use crate::shade::Shade;
use crate::text::Text;

/// A fill or stroke's color.
#[derive(Debug, Clone)]
pub struct Paint {
    pub cs: ColorSpace,
    pub color: Vec<f32>,
    pub alpha: f32,
    pub params: ColorParams,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Cmd {
    FillPath(Path, bool, Matrix, Paint),
    StrokePath(Path, StrokeState, Matrix, Paint),
    ClipPath(Path, bool, Matrix, Rect),
    ClipStrokePath(Path, StrokeState, Matrix, Rect),
    FillShade(Shade, Matrix, f32, ColorParams),
    FillImage(Image, Matrix, f32, ColorParams),
    FillImageMask(Image, Matrix, Paint),
    ClipImageMask(Image, Matrix, Rect),
    FillText(Text, Matrix, Paint),
    StrokeText(Text, StrokeState, Matrix, Paint),
    ClipText(Text, Matrix, Rect),
    ClipStrokeText(Text, StrokeState, Matrix, Rect),
    IgnoreText(Text, Matrix),
    PopClip,
    BeginMask(Rect, bool, Option<ColorSpace>, Vec<f32>, ColorParams),
    EndMask(Option<Function>),
    BeginGroup(Rect, Option<ColorSpace>, bool, bool, u8, f32),
    EndGroup,
    BeginTile(Rect, Rect, f32, f32, Matrix, i32),
    EndTile,
    DefaultColorSpaces(DefaultColorSpaces),
    BeginLayer(Vec<u8>),
    EndLayer,
    BeginStructure(&'static str, Vec<u8>, i32),
    EndStructure,
    BeginMetatext(Metatext, Vec<u8>),
    EndMetatext,
}

/// The list device: every call kept in order.
#[derive(Debug, Clone, Default)]
pub struct DisplayList {
    pub cmds: Vec<Cmd>,
}

fn paint(cs: &ColorSpace, color: &[f32], alpha: f32, params: ColorParams) -> Paint {
    Paint {
        cs: cs.clone(),
        color: color.to_vec(),
        alpha,
        params,
    }
}

impl DisplayList {
    /// `fz_run_display_list`: the calls again, onto `dev`. A cached tile
    /// skips the calls up to its end, as the list device's player does.
    pub fn replay(&self, dev: &mut dyn Device) {
        let mut skip_tile = 0usize;
        for cmd in &self.cmds {
            if skip_tile > 0 {
                match cmd {
                    Cmd::BeginTile(..) => skip_tile += 1,
                    Cmd::EndTile => {
                        skip_tile -= 1;
                        if skip_tile == 0 {
                            dev.end_tile();
                        }
                    }
                    _ => {}
                }
                continue;
            }
            match cmd {
                Cmd::FillPath(p, eo, m, c) => {
                    dev.fill_path(p, *eo, *m, &c.cs, &c.color, c.alpha, c.params)
                }
                Cmd::StrokePath(p, s, m, c) => {
                    dev.stroke_path(p, s, *m, &c.cs, &c.color, c.alpha, c.params)
                }
                Cmd::ClipPath(p, eo, m, r) => dev.clip_path(p, *eo, *m, *r),
                Cmd::ClipStrokePath(p, s, m, r) => dev.clip_stroke_path(p, s, *m, *r),
                Cmd::FillShade(s, m, a, cp) => dev.fill_shade(s, *m, *a, *cp),
                Cmd::FillImage(i, m, a, cp) => dev.fill_image(i, *m, *a, *cp),
                Cmd::FillImageMask(i, m, c) => {
                    dev.fill_image_mask(i, *m, &c.cs, &c.color, c.alpha, c.params)
                }
                Cmd::ClipImageMask(i, m, r) => dev.clip_image_mask(i, *m, *r),
                Cmd::FillText(t, m, c) => dev.fill_text(t, *m, &c.cs, &c.color, c.alpha, c.params),
                Cmd::StrokeText(t, s, m, c) => {
                    dev.stroke_text(t, s, *m, &c.cs, &c.color, c.alpha, c.params)
                }
                Cmd::ClipText(t, m, r) => dev.clip_text(t, *m, *r),
                Cmd::ClipStrokeText(t, s, m, r) => dev.clip_stroke_text(t, s, *m, *r),
                Cmd::IgnoreText(t, m) => dev.ignore_text(t, *m),
                Cmd::PopClip => dev.pop_clip(),
                Cmd::BeginMask(r, l, cs, bc, cp) => dev.begin_mask(*r, *l, cs.as_ref(), bc, *cp),
                Cmd::EndMask(f) => dev.end_mask(f.as_ref()),
                Cmd::BeginGroup(r, cs, i, k, b, a) => {
                    dev.begin_group(*r, cs.as_ref(), *i, *k, *b, *a)
                }
                Cmd::EndGroup => dev.end_group(),
                Cmd::BeginTile(a, v, xs, ys, m, id) => {
                    if dev.begin_tile(*a, *v, *xs, *ys, *m, *id) {
                        skip_tile = 1;
                    }
                }
                Cmd::EndTile => dev.end_tile(),
                Cmd::DefaultColorSpaces(d) => dev.set_default_colorspaces(d),
                Cmd::BeginLayer(n) => dev.begin_layer(n),
                Cmd::EndLayer => dev.end_layer(),
                Cmd::BeginStructure(s, raw, i) => dev.begin_structure(s, raw, *i),
                Cmd::EndStructure => dev.end_structure(),
                Cmd::BeginMetatext(m, t) => dev.begin_metatext(*m, t),
                Cmd::EndMetatext => dev.end_metatext(),
            }
        }
    }
}

impl Device for DisplayList {
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
        self.cmds.push(Cmd::FillPath(
            path.clone(),
            even_odd,
            ctm,
            paint(cs, color, alpha, params),
        ));
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
        self.cmds.push(Cmd::StrokePath(
            path.clone(),
            stroke.clone(),
            ctm,
            paint(cs, color, alpha, params),
        ));
    }

    fn clip_path(&mut self, path: &Path, even_odd: bool, ctm: Matrix, scissor: Rect) {
        self.cmds
            .push(Cmd::ClipPath(path.clone(), even_odd, ctm, scissor));
    }

    fn clip_stroke_path(&mut self, path: &Path, stroke: &StrokeState, ctm: Matrix, scissor: Rect) {
        self.cmds.push(Cmd::ClipStrokePath(
            path.clone(),
            stroke.clone(),
            ctm,
            scissor,
        ));
    }

    fn fill_shade(&mut self, shade: &Shade, ctm: Matrix, alpha: f32, params: ColorParams) {
        self.cmds
            .push(Cmd::FillShade(shade.clone(), ctm, alpha, params));
    }

    fn fill_image(&mut self, image: &Image, ctm: Matrix, alpha: f32, params: ColorParams) {
        self.cmds
            .push(Cmd::FillImage(image.clone(), ctm, alpha, params));
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
        self.cmds.push(Cmd::FillImageMask(
            image.clone(),
            ctm,
            paint(cs, color, alpha, params),
        ));
    }

    fn clip_image_mask(&mut self, image: &Image, ctm: Matrix, scissor: Rect) {
        self.cmds
            .push(Cmd::ClipImageMask(image.clone(), ctm, scissor));
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
        self.cmds.push(Cmd::FillText(
            text.clone(),
            ctm,
            paint(cs, color, alpha, params),
        ));
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
        self.cmds.push(Cmd::StrokeText(
            text.clone(),
            stroke.clone(),
            ctm,
            paint(cs, color, alpha, params),
        ));
    }

    fn clip_text(&mut self, text: &Text, ctm: Matrix, scissor: Rect) {
        self.cmds.push(Cmd::ClipText(text.clone(), ctm, scissor));
    }

    fn clip_stroke_text(&mut self, text: &Text, stroke: &StrokeState, ctm: Matrix, scissor: Rect) {
        self.cmds.push(Cmd::ClipStrokeText(
            text.clone(),
            stroke.clone(),
            ctm,
            scissor,
        ));
    }

    fn ignore_text(&mut self, text: &Text, ctm: Matrix) {
        self.cmds.push(Cmd::IgnoreText(text.clone(), ctm));
    }

    fn pop_clip(&mut self) {
        self.cmds.push(Cmd::PopClip);
    }

    fn begin_mask(
        &mut self,
        area: Rect,
        luminosity: bool,
        cs: Option<&ColorSpace>,
        backdrop: &[f32],
        params: ColorParams,
    ) {
        self.cmds.push(Cmd::BeginMask(
            area,
            luminosity,
            cs.cloned(),
            backdrop.to_vec(),
            params,
        ));
    }

    fn end_mask(&mut self, transfer: Option<&Function>) {
        self.cmds.push(Cmd::EndMask(transfer.cloned()));
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
        self.cmds.push(Cmd::BeginGroup(
            area,
            cs.cloned(),
            isolated,
            knockout,
            blendmode,
            alpha,
        ));
    }

    fn end_group(&mut self) {
        self.cmds.push(Cmd::EndGroup);
    }

    /// The list never has a tile cached: the cell is recorded every time.
    fn begin_tile(
        &mut self,
        area: Rect,
        view: Rect,
        xstep: f32,
        ystep: f32,
        ctm: Matrix,
        id: i32,
    ) -> bool {
        self.cmds
            .push(Cmd::BeginTile(area, view, xstep, ystep, ctm, id));
        false
    }

    fn end_tile(&mut self) {
        self.cmds.push(Cmd::EndTile);
    }

    fn set_default_colorspaces(&mut self, defaults: &DefaultColorSpaces) {
        self.cmds.push(Cmd::DefaultColorSpaces(defaults.clone()));
    }

    fn begin_layer(&mut self, name: &[u8]) {
        self.cmds.push(Cmd::BeginLayer(name.to_vec()));
    }

    fn end_layer(&mut self) {
        self.cmds.push(Cmd::EndLayer);
    }

    fn begin_structure(&mut self, standard: &'static str, raw: &[u8], index: i32) {
        self.cmds
            .push(Cmd::BeginStructure(standard, raw.to_vec(), index));
    }

    fn end_structure(&mut self) {
        self.cmds.push(Cmd::EndStructure);
    }

    fn begin_metatext(&mut self, meta: Metatext, text: &[u8]) {
        self.cmds.push(Cmd::BeginMetatext(meta, text.to_vec()));
    }

    fn end_metatext(&mut self) {
        self.cmds.push(Cmd::EndMetatext);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::Store;
    use crate::document::Document;
    use crate::draw::testkit::{render, sink_with};
    use crate::draw::{DrawDevice, Pix};
    use crate::geometry::IRect;

    fn listed(pdf: &[u8]) -> (Document, DisplayList) {
        let doc = Document::open(pdf.to_vec()).unwrap_or_else(|e| panic!("{e:?}"));
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e:?}"));
        let mut list = DisplayList::default();
        let run = crate::run::run_page_contents_with(
            &doc,
            &page,
            &mut list,
            Store::default(),
            Some("View"),
            Matrix::IDENTITY,
        );
        assert_eq!(run.error, None);
        (doc, list)
    }

    #[test]
    fn a_replayed_list_draws_what_the_page_does() {
        // Helvetica stroked, and clipped to its strokes, as well.
        let pdf = sink_with(
            "BT /F1 14 Tf 1 Tr 4 112 Td (Ab) Tj ET q /Pattern CS /Sh SCN BT /F1 16 Tf 1 Tr 100 30 Td (W) Tj ET Q",
        );
        let (doc, list) = listed(&pdf);
        // Every kind of call was kept.
        let kinds: std::collections::HashSet<_> = list
            .cmds
            .iter()
            .map(|c| {
                format!("{c:?}")
                    .split(['(', ' '])
                    .next()
                    .map(str::to_string)
            })
            .collect();
        for k in [
            "FillPath",
            "StrokePath",
            "ClipPath",
            "ClipStrokePath",
            "FillShade",
            "FillImage",
            "FillImageMask",
            "ClipImageMask",
            "FillText",
            "StrokeText",
            "ClipText",
            "ClipStrokeText",
            "IgnoreText",
            "PopClip",
            "BeginMask",
            "EndMask",
            "BeginGroup",
            "EndGroup",
            "BeginTile",
            "EndTile",
            "BeginLayer",
            "EndLayer",
            "BeginMetatext",
            "EndMetatext",
        ] {
            assert!(kinds.contains(&Some(k.to_string())), "no {k}: {kinds:?}");
        }
        // Played into a pixmap, the list paints what the page run does.
        let mut pix =
            Pix::new(IRect::new(0, 0, 128, 128), 3, false).unwrap_or_else(|e| panic!("{e}"));
        pix.clear_with(255);
        let mut dev = DrawDevice::new(Some(&doc), pix);
        dev.cc.icc = true;
        list.replay(&mut dev);
        let direct = render("list-sink", &pdf, 1.0, true);
        assert!(dev.into_pix() == direct, "the replay differs");
        // Played into another list, it is kept call for call.
        let mut again = DisplayList::default();
        list.replay(&mut again);
        assert_eq!(format!("{:?}", again.cmds), format!("{:?}", list.cmds));
    }
}
