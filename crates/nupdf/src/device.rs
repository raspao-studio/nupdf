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
