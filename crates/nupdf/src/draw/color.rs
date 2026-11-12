// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/colorspace.c (fz_convert_color,
// fz_default_colorspace) and source/fitz/color-fast.c (the conversions
// MuPDF uses with color management off),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Colors into the page's RGB (or a soft mask's gray), with MuPDF's color
//! management on (`mutool draw`'s default: ICC profiles through
//! [`crate::cms`]) or off (`-N`: the device formulas).

use crate::cms;
use crate::colorspace::{ColorSpace, Family};
use crate::device::DefaultColorSpaces;
use crate::document::Document;

#[derive(Debug, Clone, Default)]
pub struct Converter {
    pub icc: bool,
    pub defaults: DefaultColorSpaces,
}

impl Converter {
    /// `fz_default_colorspace`: a device space as the page's defaults
    /// (/DefaultGray and the like, or the output intent) replace it.
    pub fn default_of<'a>(&'a self, cs: &'a ColorSpace) -> &'a ColorSpace {
        let slot = match cs {
            ColorSpace::Gray => &self.defaults.gray,
            ColorSpace::Rgb => &self.defaults.rgb,
            ColorSpace::Cmyk => &self.defaults.cmyk,
            _ => &None,
        };
        slot.as_ref().unwrap_or(cs)
    }

    /// A device, ICC or Lab color into sRGB (or gray with `gray`).
    fn leaf(&self, cs: &ColorSpace, color: &[f32], ri: u8, gray: bool) -> [f32; 3] {
        if self.icc {
            let (family, profile): (Option<Family>, Option<&[u8]>) = match cs {
                ColorSpace::Gray => (Some(Family::Gray), Some(cms::GRAY_ICC)),
                ColorSpace::Rgb => (Some(Family::Rgb), None),
                ColorSpace::Cmyk => (Some(Family::Cmyk), Some(cms::CMYK_ICC)),
                ColorSpace::Icc(f, data) => (Some(*f), data.0.as_deref()),
                _ => (None, None),
            };
            match (family, profile) {
                // DeviceRGB is the output space: MuPDF copies the color.
                (Some(Family::Rgb), None) if !gray => return cs.device_to_rgb(color),
                (Some(f @ (Family::Gray | Family::Rgb | Family::Cmyk)), p) => {
                    let p = p.unwrap_or(cms::device_profile(f));
                    if let Some(t) = cms::transform(p, f, ri, gray) {
                        let out = t.color(color);
                        return if gray { [out[0], 0.0, 0.0] } else { out };
                    }
                }
                _ => {}
            }
        }
        let rgb = cs.device_to_rgb(color);
        if gray {
            return [gray_from(cs, color, rgb), 0.0, 0.0];
        }
        rgb
    }

    /// `fz_convert_color` to the page's RGB.
    pub fn rgb(&self, doc: Option<&Document>, cs: &ColorSpace, color: &[f32], ri: u8) -> [f32; 3] {
        let cs = self.default_of(cs);
        cs.to_rgb_with(doc, color, &|leaf, c| self.leaf(leaf, c, ri, false))
    }

    /// `fz_convert_color` to DeviceGray, for luminosity soft masks.
    pub fn gray(&self, doc: Option<&Document>, cs: &ColorSpace, color: &[f32], ri: u8) -> f32 {
        let cs = self.default_of(cs);
        match cs {
            ColorSpace::Indexed { .. } | ColorSpace::Separation { .. } => {
                let rgb = self.rgb(doc, cs, color, ri);
                rgb[0] * 0.3 + rgb[1] * 0.59 + rgb[2] * 0.11
            }
            _ => self.leaf(cs, color, ri, true)[0],
        }
    }
}

/// MuPDF's non-ICC conversions to gray: gray as it is, RGB and CMYK by
/// their luminance formulas.
fn gray_from(cs: &ColorSpace, color: &[f32], rgb: [f32; 3]) -> f32 {
    let v = |i: usize| color.get(i).copied().unwrap_or(0.0);
    match cs.family() {
        Some(Family::Gray) => v(0),
        Some(Family::Rgb) => v(0) * 0.3 + v(1) * 0.59 + v(2) * 0.11,
        Some(Family::Cmyk) => 1.0 - (v(0) * 0.3 + v(1) * 0.59 + v(2) * 0.11 + v(3)).min(1.0),
        _ => rgb[0] * 0.3 + rgb[1] * 0.59 + rgb[2] * 0.11,
    }
}
