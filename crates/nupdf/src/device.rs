// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF include/mupdf/fitz/device.h and source/fitz/device.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! What the interpreter draws to: `fz_device`, as a trait whose methods
//! default to doing nothing, the way a device leaves a callback unset.

use crate::colorspace::ColorSpace;
use crate::function::Function;
use crate::geometry::{Matrix, Rect};
use crate::image::Image;
use crate::path::{Path, StrokeState};
use crate::shade::Shade;
use crate::text::Text;

/// `fz_color_params`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorParams {
    /// The rendering intent, as `fz_lookup_rendering_intent` numbers it.
    pub ri: u8,
    pub bp: bool,
    pub op: bool,
    pub opm: bool,
}

impl Default for ColorParams {
    /// `fz_default_color_params`.
    fn default() -> Self {
        ColorParams {
            ri: 1,
            bp: true,
            op: false,
            opm: false,
        }
    }
}

/// `fz_intent_names`.
pub const INTENT_NAMES: [&str; 4] = [
    "Perceptual",
    "RelativeColorimetric",
    "Saturation",
    "AbsoluteColorimetric",
];

/// `fz_lookup_rendering_intent`: unknown names are relative colorimetric.
pub fn lookup_intent(name: &[u8]) -> u8 {
    INTENT_NAMES
        .iter()
        .position(|n| n.as_bytes() == name)
        .map_or(1, |i| i as u8)
}

/// `fz_blendmode_names`.
pub const BLEND_NAMES: [&str; 16] = [
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

/// `fz_lookup_blendmode`: unknown names are Normal (0).
pub fn lookup_blendmode(name: &[u8]) -> u8 {
    BLEND_NAMES
        .iter()
        .position(|n| n.as_bytes() == name)
        .map_or(0, |i| i as u8)
}

/// `fz_metatext`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metatext {
    ActualText,
    Alt,
    Abbreviation,
    Title,
}

impl Metatext {
    /// As MuPDF's script glue names it.
    pub fn name(self) -> &'static str {
        match self {
            Metatext::ActualText => "ActualText",
            Metatext::Alt => "Alt",
            Metatext::Abbreviation => "Abbreviation",
            Metatext::Title => "Title",
        }
    }
}

/// The default colorspaces a page or form sets (`fz_default_colorspaces`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DefaultColorSpaces {
    /// None stands for the device space.
    pub gray: Option<ColorSpace>,
    pub rgb: Option<ColorSpace>,
    pub cmyk: Option<ColorSpace>,
    pub output_intent: Option<ColorSpace>,
}

/// `fz_device`. Every method does nothing unless a device implements it.
#[allow(unused_variables)]
#[allow(clippy::too_many_arguments)]
pub trait Device {
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
    }

    fn clip_path(&mut self, path: &Path, even_odd: bool, ctm: Matrix, scissor: Rect) {}

    fn clip_stroke_path(&mut self, path: &Path, stroke: &StrokeState, ctm: Matrix, scissor: Rect) {}

    fn fill_shade(&mut self, shade: &Shade, ctm: Matrix, alpha: f32, params: ColorParams) {}

    fn fill_image(&mut self, image: &Image, ctm: Matrix, alpha: f32, params: ColorParams) {}

    fn fill_image_mask(
        &mut self,
        image: &Image,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        params: ColorParams,
    ) {
    }

    fn clip_image_mask(&mut self, image: &Image, ctm: Matrix, scissor: Rect) {}

    fn fill_text(
        &mut self,
        text: &Text,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        params: ColorParams,
    ) {
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
    }

    fn clip_text(&mut self, text: &Text, ctm: Matrix, scissor: Rect) {}

    fn clip_stroke_text(&mut self, text: &Text, stroke: &StrokeState, ctm: Matrix, scissor: Rect) {}

    /// Text in render mode 3: there for extraction, not drawn.
    fn ignore_text(&mut self, text: &Text, ctm: Matrix) {}

    fn pop_clip(&mut self) {}

    fn begin_mask(
        &mut self,
        area: Rect,
        luminosity: bool,
        cs: Option<&ColorSpace>,
        backdrop: &[f32],
        params: ColorParams,
    ) {
    }

    fn end_mask(&mut self, transfer: Option<&Function>) {}

    fn begin_group(
        &mut self,
        area: Rect,
        cs: Option<&ColorSpace>,
        isolated: bool,
        knockout: bool,
        blendmode: u8,
        alpha: f32,
    ) {
    }

    fn end_group(&mut self) {}

    /// True when the device has the tile cached, so its content need not
    /// be run.
    fn begin_tile(
        &mut self,
        area: Rect,
        view: Rect,
        xstep: f32,
        ystep: f32,
        ctm: Matrix,
        id: i32,
    ) -> bool {
        false
    }

    fn end_tile(&mut self) {}

    fn set_default_colorspaces(&mut self, defaults: &DefaultColorSpaces) {}

    fn begin_layer(&mut self, name: &[u8]) {}

    fn end_layer(&mut self) {}

    /// `standard` is the structure type's name, `raw` the tag as written.
    fn begin_structure(&mut self, standard: &'static str, raw: &[u8], index: i32) {}

    fn end_structure(&mut self) {}

    fn begin_metatext(&mut self, meta: Metatext, text: &[u8]) {}

    fn end_metatext(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::list::{Cmd, DisplayList};

    #[test]
    fn names_look_up_as_mupdf_numbers_them() {
        assert_eq!(lookup_intent(b"Perceptual"), 0);
        assert_eq!(lookup_intent(b"AbsoluteColorimetric"), 3);
        assert_eq!(lookup_intent(b"Vivid"), 1);
        assert_eq!(lookup_blendmode(b"Luminosity"), 15);
        assert_eq!(lookup_blendmode(b"Compatible"), 0);
        let names: Vec<_> = [
            Metatext::ActualText,
            Metatext::Alt,
            Metatext::Abbreviation,
            Metatext::Title,
        ]
        .map(Metatext::name)
        .into();
        assert_eq!(names, ["ActualText", "Alt", "Abbreviation", "Title"]);
        assert_eq!(ColorParams::default().ri, 1);
    }

    /// Counts its fills; every other call falls to the trait's defaults.
    #[derive(Default)]
    struct Fills(usize);

    impl Device for Fills {
        fn fill_path(
            &mut self,
            _: &Path,
            _: bool,
            _: Matrix,
            _: &ColorSpace,
            _: &[f32],
            _: f32,
            _: ColorParams,
        ) {
            self.0 += 1;
        }
    }

    #[test]
    fn calls_a_device_does_not_implement_do_nothing() {
        // A page of every kind of call, kept and played to a device that
        // only counts fills: the rest are ignored, and as no tile is
        // cached, the fills inside tiles are played too.
        let pdf = crate::draw::testkit::sink_with(
            "BT /F1 14 Tf 1 Tr 4 112 Td (Ab) Tj ET q /Pattern CS /Sh SCN BT /F1 16 Tf 1 Tr 100 30 Td (W) Tj ET Q",
        );
        let doc = crate::document::Document::open(pdf).unwrap_or_else(|e| panic!("{e:?}"));
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e:?}"));
        let mut list = DisplayList::default();
        let run = crate::run::run_page_contents_with(
            &doc,
            &page,
            &mut list,
            crate::content::Store::default(),
            Some("View"),
            Matrix::IDENTITY,
        );
        assert_eq!(run.error, None);
        let mut fills = Fills::default();
        list.replay(&mut fills);
        let want = list
            .cmds
            .iter()
            .filter(|c| matches!(c, Cmd::FillPath(..)))
            .count();
        assert!(want > 0);
        assert_eq!(fills.0, want);
    }
}
