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

/// `FZ_RI_IN_SOFTMASK`, or'd into a rendering intent: the color is
/// drawn into a soft mask, where MuPDF converts device colors (and ICC
/// colors of their families) by its formulas, not its profiles.
pub const RI_IN_SOFTMASK: u8 = 0x80;

/// Ours: convert into the soft-mask profiles without swapping the
/// source's, as MuPDF does a palette's or separation's base color.
const RI_SOFT_DEST: u8 = 0x40;

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
            // In a soft mask MuPDF swaps the device families' spaces, ICC
            // ones too, for its PostScript-like profiles (`ps_gray` and
            // the like), and converts into those.
            let soft = ri & RI_IN_SOFTMASK != 0;
            let soft_dest = soft || ri & RI_SOFT_DEST != 0;
            let ri = ri & !(RI_IN_SOFTMASK | RI_SOFT_DEST);
            let (family, profile): (Option<Family>, Option<&[u8]>) = match cs {
                ColorSpace::Gray => (Some(Family::Gray), Some(cms::GRAY_ICC)),
                ColorSpace::Rgb => (Some(Family::Rgb), None),
                ColorSpace::Cmyk => (Some(Family::Cmyk), Some(cms::CMYK_ICC)),
                ColorSpace::Icc(f, data) => (Some(*f), data.0.as_deref()),
                _ => (None, None),
            };
            // The ICC space's own profile, and its hash taken once.
            let own = match cs {
                ColorSpace::Icc(_, data) => data.0.as_deref().map(<[u8]>::as_ptr).zip(data.1),
                _ => None,
            };
            let profile = match family {
                Some(f @ (Family::Gray | Family::Rgb | Family::Cmyk)) if soft => {
                    Some(cms::ps_profile(f))
                }
                _ => profile,
            };
            match (family, profile) {
                // DeviceRGB is the output space: MuPDF copies the color,
                // as it does a soft mask's RGB into RGB and gray into gray.
                (Some(Family::Rgb), None) if !gray => return cs.device_to_rgb(color),
                (Some(Family::Rgb), _) if soft && !gray => return cs.device_to_rgb(color),
                (Some(Family::Gray), _) if soft && gray => {
                    return [color.first().copied().unwrap_or(0.0), 0.0, 0.0];
                }
                (Some(f @ (Family::Gray | Family::Rgb | Family::Cmyk)), p) => {
                    let p = p.unwrap_or(cms::device_profile(f));
                    // The hash is good for the space's own profile only; a
                    // swapped or default one is a built-in.
                    let hash = own.filter(|(at, _)| *at == p.as_ptr()).map(|(_, h)| h);
                    if let Some(t) = cms::transform(p, f, ri, gray, soft_dest, hash) {
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

    /// `fz_convert_color` to DeviceCMYK without color management, for
    /// transparency groups that blend in CMYK: down to the device, ICC or
    /// Lab color an indexed or separation space comes to, then by the
    /// device formulas (`gray_to_cmyk`, `rgb_to_cmyk`, `lab_to_cmyk`).
    pub fn cmyk(&self, doc: Option<&Document>, cs: &ColorSpace, color: &[f32]) -> [f32; 4] {
        let cs = self.default_of(cs);
        let out = std::cell::Cell::new(None);
        let rgb = cs.to_rgb_with(doc, color, &|leaf, c| {
            let v = |i: usize| c.get(i).copied().unwrap_or(0.0);
            let rgb = leaf.device_to_rgb(c);
            out.set(Some(match leaf.family() {
                Some(Family::Gray) => [0.0, 0.0, 0.0, 1.0 - v(0)],
                Some(Family::Cmyk) => [v(0), v(1), v(2), v(3)],
                _ => rgb_to_cmyk(rgb),
            }));
            rgb
        });
        // Without a leaf (nothing to evaluate a tint with), the RGB given.
        out.get().unwrap_or_else(|| rgb_to_cmyk(rgb))
    }

    /// `fz_convert_color` to DeviceGray, for luminosity soft masks.
    pub fn gray(&self, doc: Option<&Document>, cs: &ColorSpace, color: &[f32], ri: u8) -> f32 {
        let cs = self.default_of(cs);
        match cs {
            // A soft mask with ICC: MuPDF converts the base color with the
            // base's own profile into ps_gray (only the destination is
            // swapped, as the source is not a device space).
            ColorSpace::Indexed { .. } | ColorSpace::Separation { .. }
                if self.icc && ri & RI_IN_SOFTMASK != 0 =>
            {
                let ri = (ri & !RI_IN_SOFTMASK) | RI_SOFT_DEST;
                cs.to_rgb_with(doc, color, &|leaf, c| self.leaf(leaf, c, ri, true))[0]
            }
            ColorSpace::Indexed { .. } | ColorSpace::Separation { .. } => {
                let rgb = self.rgb(doc, cs, color, ri);
                rgb[0] * 0.3 + rgb[1] * 0.59 + rgb[2] * 0.11
            }
            _ => self.leaf(cs, color, ri, true)[0],
        }
    }
}

/// `rgb_to_cmyk`: black is the least of the inks.
pub fn rgb_to_cmyk(rgb: [f32; 3]) -> [f32; 4] {
    let (c, m, y) = (1.0 - rgb[0], 1.0 - rgb[1], 1.0 - rgb[2]);
    let k = c.min(m.min(y));
    [c - k, m - k, y - k, k]
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

#[cfg(test)]
mod tests {
    use crate::draw::testkit::{check, hex_stream, page, stream};

    /// A page whose luminosity soft mask fills in `colors`, each a
    /// colorspace resource name (or a device operator) and its color.
    fn masked(name: &str, icc: bool, want: &str) {
        let profile = hex_stream("/N 3", crate::cms::RGB_ICC);
        let mut fills = String::new();
        let colors = [
            ("/DeviceGray cs 0.7 sc", 0),
            ("/DeviceRGB cs 0.9 0.2 0.1 sc", 1),
            ("/DeviceCMYK cs 0.1 0.5 0.2 0.1 sc", 2),
            ("/Ix cs 2 sc", 3),
            ("/Icc cs 0.2 0.8 0.4 sc", 4),
            ("/Cal cs 0.4 0.6 0.9 sc", 5),
        ];
        // CMYK through lcms comes out a level off moxcms, so it is drawn
        // only without ICC.
        for (op, i) in colors.into_iter().filter(|c| !icc || c.1 != 2) {
            fills.push_str(&format!("{op} {} 0 16 64 re f\n", i * 16));
        }
        let group = stream(
            "/Type/XObject/Subtype/Form/BBox[0 0 128 64]/Group<</S/Transparency/CS/DeviceGray>>/Resources<</ColorSpace<</Ix[/Indexed/DeviceRGB 3<ff000000ff000000ffffff00>]/Icc[/ICCBased 7 0 R]/Lab[/Lab<</WhitePoint[0.9505 1 1.089]>>]/Cal[/CalRGB<</WhitePoint[0.9505 1 1.089]/Gamma[2.2 2.2 2.2]>>]>>>>",
            &fills,
        );
        let pdf = page(
            128,
            64,
            "/ExtGState<</M 5 0 R>>",
            "q /M gs 0.1 0.3 0.8 rg 0 0 128 64 re f Q",
            &["<</SMask<</S/Luminosity/G 6 0 R>>>>", &group, &profile],
        );
        check(name, &pdf, icc, want);
    }

    #[test]
    fn soft_masks_take_the_luminance_of_every_colorspace() {
        // Device, indexed, ICC-based and calibrated colors into a
        // luminosity mask: with ICC through MuPDF's soft-mask profiles,
        // without by its formulas. (Lab is left out: MuPDF converts it
        // through its Lab profile, nupdf by formula, some levels apart.)
        masked(
            "color-mask-spaces",
            true,
            "a4db5a05afe5b022c545f2135bde4c3c",
        );
        masked(
            "color-mask-spaces-noicc",
            false,
            "ad4a7fab2291395f973fb3f45abbc86c",
        );
    }
}
