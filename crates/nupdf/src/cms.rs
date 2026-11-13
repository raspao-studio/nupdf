// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Following MuPDF source/fitz/color-lcms.c and source/fitz/colorspace.c
// (the ICC paths of fz_convert_color and fz_convert_pixmap),
// Copyright (C) 2004-2026 Artifex Software, Inc. The profiles in
// crates/nupdf/icc are MuPDF's (resources/icc), Copyright Artifex Software.

//! Color management, as MuPDF does it with lcms2 when ICC is on, done here
//! with moxcms: the device spaces are MuPDF's own profiles (gray, sRGB,
//! SWOP CMYK, Lab), embedded ICCBased spaces their profiles, and
//! conversions go to sRGB with the color's rendering intent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use moxcms::{ColorProfile, Layout, RenderingIntent, TransformOptions};

use crate::colorspace::Family;

pub const GRAY_ICC: &[u8] = include_bytes!("../icc/gray.icc");
pub const RGB_ICC: &[u8] = include_bytes!("../icc/rgb.icc");
pub const CMYK_ICC: &[u8] = include_bytes!("../icc/cmyk.icc");
pub const LAB_ICC: &[u8] = include_bytes!("../icc/lab.icc");
/// The profiles MuPDF converts with inside soft masks (`ps_gray` and the
/// like in colorspace.c).
pub const PS_GRAY_ICC: &[u8] = include_bytes!("../icc/ps_gray.icc");
pub const PS_RGB_ICC: &[u8] = include_bytes!("../icc/ps_rgb.icc");
pub const PS_CMYK_ICC: &[u8] = include_bytes!("../icc/ps_cmyk.icc");

/// The soft-mask profile of a device family (Lab keeps its own).
pub fn ps_profile(f: Family) -> &'static [u8] {
    match f {
        Family::Gray => PS_GRAY_ICC,
        Family::Rgb => PS_RGB_ICC,
        Family::Cmyk => PS_CMYK_ICC,
        Family::Lab => LAB_ICC,
    }
}

/// The profile a family's device space has.
pub fn device_profile(f: Family) -> &'static [u8] {
    match f {
        Family::Gray => GRAY_ICC,
        Family::Rgb => RGB_ICC,
        Family::Cmyk => CMYK_ICC,
        Family::Lab => LAB_ICC,
    }
}

type Key = (blake3::Hash, u8, bool, bool);

/// Transforms to sRGB (or to gray, `to_gray`), by profile and intent; None
/// for a profile moxcms cannot use.
/// The hash of one of the built-in profiles, by where it is.
fn builtin_hash(profile: &[u8]) -> Option<blake3::Hash> {
    static BUILTIN: OnceLock<Vec<(usize, usize, blake3::Hash)>> = OnceLock::new();
    let all = BUILTIN.get_or_init(|| {
        [
            GRAY_ICC,
            RGB_ICC,
            CMYK_ICC,
            LAB_ICC,
            PS_GRAY_ICC,
            PS_RGB_ICC,
            PS_CMYK_ICC,
        ]
        .iter()
        .map(|p| (p.as_ptr() as usize, p.len(), blake3::hash(p)))
        .collect()
    });
    all.iter()
        .find(|(at, len, _)| *at == profile.as_ptr() as usize && *len == profile.len())
        .map(|(_, _, h)| *h)
}

fn cache() -> &'static Mutex<HashMap<Key, Option<Arc<Transform>>>> {
    static CACHE: OnceLock<Mutex<HashMap<Key, Option<Arc<Transform>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A conversion to sRGB (or gray) as lcms2 makes it with black point
/// compensation: the source into Lab, scaled in XYZ so its black point
/// meets the destination's (which for MuPDF's sRGB and gray is zero), then
/// out of Lab.
pub struct Transform {
    /// Into linear RGB with MuPDF's sRGB primaries, unclamped, whence XYZ.
    to_lin: Arc<moxcms::TransformF32Executor>,
    lin_to_xyz: [[f64; 3]; 3],
    xyz_to_lin: [[f64; 3]; 3],
    from_lin: Arc<moxcms::TransformF32Executor>,
    /// The source black point in XYZ (D50), when compensating.
    black: Option<[f64; 3]>,
    src_n: usize,
    dst_n: usize,
}

impl std::fmt::Debug for Transform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transform")
            .field("src_n", &self.src_n)
            .field("black", &self.black)
            .finish_non_exhaustive()
    }
}

const D50: [f64; 3] = [0.9642, 1.0, 0.8249];

fn lab_to_xyz(l: f64, a: f64, b: f64) -> [f64; 3] {
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let f = |t: f64| {
        let lim = 24.0 / 116.0;
        if t <= lim {
            (108.0 / 841.0) * (t - 16.0 / 116.0)
        } else {
            t * t * t
        }
    };
    [f(fx) * D50[0], f(fy) * D50[1], f(fz) * D50[2]]
}

fn xyz_to_lab(xyz: [f64; 3]) -> [f64; 3] {
    let f = |t: f64| {
        let lim = (24.0f64 / 116.0).powi(3);
        if t <= lim {
            (841.0 / 108.0) * t + 16.0 / 116.0
        } else {
            t.cbrt()
        }
    };
    let (fx, fy, fz) = (f(xyz[0] / D50[0]), f(xyz[1] / D50[1]), f(xyz[2] / D50[2]));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

fn layout(n: usize) -> Option<Layout> {
    match n {
        1 => Some(Layout::Gray),
        3 => Some(Layout::Rgb),
        4 => Some(Layout::Rgba),
        _ => None,
    }
}

fn intent(ri: u8) -> RenderingIntent {
    match ri {
        0 => RenderingIntent::Perceptual,
        2 => RenderingIntent::Saturation,
        3 => RenderingIntent::AbsoluteColorimetric,
        _ => RenderingIntent::RelativeColorimetric,
    }
}

/// The transform from `profile` (of family `family`) to sRGB, or to
/// MuPDF's gray with `to_gray`; into the soft-mask profiles with `soft`.
pub fn transform(
    profile: &[u8],
    family: Family,
    ri: u8,
    to_gray: bool,
    soft: bool,
    hash: Option<blake3::Hash>,
) -> Option<Arc<Transform>> {
    // Hashing a profile of megabytes for every color drawn made shadings
    // take minutes: the caller's hash, a built-in's, or this once.
    let hash = hash
        .or_else(|| builtin_hash(profile))
        .unwrap_or_else(|| blake3::hash(profile));
    let key = (hash, ri, to_gray, soft);
    if let Ok(c) = cache().lock()
        && let Some(t) = c.get(&key)
    {
        return t.clone();
    }
    let made = make(profile, family, ri, to_gray, soft).map(Arc::new);
    if let Ok(mut c) = cache().lock() {
        c.insert(key, made.clone());
    }
    made
}

fn options(ri: u8) -> TransformOptions {
    TransformOptions {
        rendering_intent: intent(ri),
        ..Default::default()
    }
}

/// `cmsDetectBlackPoint` for the relative colorimetric intent: an output
/// CMYK profile's black from a perceptual round trip of Lab black, else
/// the darkest colorant's; either neutral, its lightness at most 50.
fn black_point(src: &ColorProfile, family: Family, n: usize) -> Option<[f64; 3]> {
    let (lin, m) = linear_of(RGB_ICC)?;
    let into = |t: &moxcms::TransformF32Executor, dev: &[f32]| -> Option<[f64; 3]> {
        let mut rgb = [0f32; 3];
        t.transform(dev, &mut rgb).ok()?;
        Some(xyz_to_lab(mul(&m, rgb)))
    };
    let lab_out =
        if family == Family::Cmyk && src.profile_class == moxcms::ProfileClass::OutputDevice {
            let to = lin
                .create_transform_f32(Layout::Rgb, src, layout(n)?, options(0))
                .ok()?;
            let back = src
                .create_transform_f32(layout(n)?, &lin, Layout::Rgb, options(0))
                .ok()?;
            let mut dev = vec![0f32; n];
            to.transform(&[0.0, 0.0, 0.0], &mut dev).ok()?;
            into(back.as_ref(), &dev)?
        } else {
            let t = src
                .create_transform_f32(layout(n)?, &lin, Layout::Rgb, options(1))
                .ok()?;
            let dark = match family {
                Family::Cmyk => vec![1f32; n],
                _ => vec![0f32; n],
            };
            into(t.as_ref(), &dark)?
        };
    let l = lab_out[0].clamp(0.0, 50.0);
    Some(lab_to_xyz(l, 0.0, 0.0))
}

/// MuPDF's sRGB with linear curves (its PostScript RGB, whose curves are
/// linear and gamut wider, for soft masks), and its matrix into XYZ (D50).
fn linear_of(base: &[u8]) -> Option<(ColorProfile, [[f64; 3]; 3])> {
    let mut p = ColorProfile::new_from_slice(base).ok()?;
    let one = moxcms::ToneReprCurve::Parametric(vec![1.0]);
    p.red_trc = Some(one.clone());
    p.green_trc = Some(one.clone());
    p.blue_trc = Some(one);
    let m = p.rgb_to_xyz_matrix();
    let mut out = [[0f64; 3]; 3];
    for (r, row) in out.iter_mut().enumerate() {
        for (c, v) in row.iter_mut().enumerate() {
            *v = m.v[r][c];
        }
    }
    Some((p, out))
}

fn mul(m: &[[f64; 3]; 3], v: [f32; 3]) -> [f64; 3] {
    let v = v.map(f64::from);
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

fn make(profile: &[u8], family: Family, ri: u8, to_gray: bool, soft: bool) -> Option<Transform> {
    let src = ColorProfile::new_from_slice(profile).ok()?;
    let dst = match (to_gray, soft) {
        (true, false) => GRAY_ICC,
        (false, false) => RGB_ICC,
        (true, true) => PS_GRAY_ICC,
        (false, true) => PS_RGB_ICC,
    };
    let dst = ColorProfile::new_from_slice(dst).ok()?;
    let src_n = family.n();
    let dst_n = if to_gray { 1 } else { 3 };
    let (lin, lin_to_xyz) = linear_of(if soft { PS_RGB_ICC } else { RGB_ICC })?;
    let to_lin = src
        .create_transform_f32(layout(src_n)?, &lin, Layout::Rgb, options(ri))
        .ok()?;
    let from_lin = lin
        .create_transform_f32(Layout::Rgb, &dst, layout(dst_n)?, options(ri))
        .ok()?;
    // Black point compensation applies to the colorimetric intents
    // (MuPDF asks for it; lcms skips it for absolute colorimetric).
    let black = if ri == 3 {
        None
    } else {
        black_point(&src, family, src_n).filter(|b| b[1] > 0.0)
    };
    Some(Transform {
        to_lin,
        lin_to_xyz,
        xyz_to_lin: invert(&lin_to_xyz)?,
        from_lin,
        black,
        src_n,
        dst_n,
    })
}

fn invert(m: &[[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let [[a, b, c], [d, e, f], [g, h, i]] = *m;
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if det.abs() < 1e-12 {
        return None;
    }
    let k = 1.0 / det;
    Some([
        [
            (e * i - f * h) * k,
            (c * h - b * i) * k,
            (b * f - c * e) * k,
        ],
        [
            (f * g - d * i) * k,
            (a * i - c * g) * k,
            (c * d - a * f) * k,
        ],
        [
            (d * h - e * g) * k,
            (b * g - a * h) * k,
            (a * e - b * d) * k,
        ],
    ])
}

impl Transform {
    /// One color, components 0..1.
    pub fn color(&self, input: &[f32]) -> [f32; 3] {
        let mut src = vec![0f32; self.src_n];
        for (s, v) in src.iter_mut().zip(input) {
            *s = v.clamp(0.0, 1.0);
        }
        let mut lin = [0f32; 3];
        if self.to_lin.transform(&src, &mut lin).is_err() {
            return [0.0; 3];
        }
        let mut xyz = mul(&self.lin_to_xyz, lin);
        if let Some(bp) = self.black {
            for k in 0..3 {
                // ComputeBlackPointCompensation, the output black zero.
                let t = bp[k] - D50[k];
                xyz[k] = (-D50[k] / t) * xyz[k] + (-D50[k] * (0.0 - bp[k]) / t);
            }
        }
        let lin = mul(&self.xyz_to_lin, xyz.map(|v| v as f32));
        let lin = lin.map(|v| v as f32);
        let mut dst = vec![0f32; self.dst_n];
        if self.from_lin.transform(&lin, &mut dst).is_err() {
            return [0.0; 3];
        }
        let mut out = [0f32; 3];
        for (o, v) in out.iter_mut().zip(&dst) {
            *o = v.clamp(0.0, 1.0);
        }
        out
    }

    pub fn dst_n(&self) -> usize {
        self.dst_n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swop_cmyk_lands_near_mupdfs_lcms() {
        let t = transform(CMYK_ICC, Family::Cmyk, 1, false, false, None)
            .unwrap_or_else(|| panic!("no transform"));
        // MuPDF 1.28.6 with ICC on paints these CMYK fills as these sRGB
        // bytes (lcms2, relative colorimetric, black point compensation).
        let cases: [([f32; 4], [u8; 3]); 6] = [
            ([0.0, 0.0, 0.0, 1.0], [34, 31, 31]),
            ([1.0, 0.0, 0.0, 0.0], [0, 173, 239]),
            ([0.0, 1.0, 0.0, 0.0], [236, 0, 139]),
            ([0.0, 0.0, 1.0, 0.0], [255, 241, 0]),
            ([0.5, 0.5, 0.5, 0.5], [82, 74, 72]),
            ([0.2, 0.4, 0.6, 0.1], [186, 143, 104]),
        ];
        let mut worst = 0;
        for (c, want) in cases {
            let got = t.color(&c).map(|v| (v * 255.0) as i32);
            for k in 0..3 {
                worst = worst.max((got[k] - i32::from(want[k])).abs());
            }
            eprintln!("{c:?}: {got:?} want {want:?}");
        }
        assert!(worst <= 2, "worst {worst}");
    }
}
