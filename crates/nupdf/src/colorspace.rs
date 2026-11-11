// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-colorspace.c and
// source/fitz/colorspace.c (fz_new_indexed_colorspace),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Colorspace definitions, as `pdf_load_colorspace` reads them.
//!
//! This is the shape of a colorspace: its family, its number of
//! components, the base of an indexed or separation space with a
//! separation's tint transform, and the failures MuPDF throws while
//! reading one. An ICC profile is judged by its header, as lcms opens it:
//! the checks it makes there, and the data colour space it declares; the
//! profile's tags are not read.

use crate::diag::report;
use crate::document::Document;
use crate::function::{self, Function};
use crate::object::Object;

/// MuPDF's FZ_MAX_COLORS.
pub const MAX_COLORS: usize = 32;

/// Nesting deeper than this is treated as recursion.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub enum ColorSpace {
    Gray,
    Rgb,
    Cmyk,
    Lab,
    /// An ICCBased space, of the family its profile has.
    Icc(Family),
    Indexed {
        base: Box<ColorSpace>,
        high: u8,
        lookup: Vec<u8>,
    },
    /// Separation (one component) or DeviceN.
    Separation {
        n: usize,
        base: Box<ColorSpace>,
        tint: Box<Function>,
    },
}

impl ColorSpace {
    /// `fz_colorspace_n`.
    pub fn n(&self) -> usize {
        match self {
            ColorSpace::Gray => 1,
            ColorSpace::Rgb | ColorSpace::Lab => 3,
            ColorSpace::Cmyk => 4,
            ColorSpace::Icc(f) => f.n(),
            ColorSpace::Indexed { .. } => 1,
            ColorSpace::Separation { n, .. } => *n,
        }
    }

    pub fn is_indexed(&self) -> bool {
        matches!(self, ColorSpace::Indexed { .. })
    }

    /// `fz_colorspace_type`, as MuPDF's script glue names it.
    pub fn type_name(&self) -> &'static str {
        match self.family() {
            Some(f) => f.name(),
            None if self.is_indexed() => "Indexed",
            None => "Separation",
        }
    }

    /// The family of a device, calibrated or ICC space.
    pub fn family(&self) -> Option<Family> {
        match self {
            ColorSpace::Gray => Some(Family::Gray),
            ColorSpace::Rgb => Some(Family::Rgb),
            ColorSpace::Cmyk => Some(Family::Cmyk),
            ColorSpace::Lab => Some(Family::Lab),
            ColorSpace::Icc(f) => Some(*f),
            _ => None,
        }
    }
}

/// The families an ICC profile can have in MuPDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Gray,
    Rgb,
    Cmyk,
    Lab,
}

impl Family {
    pub fn n(self) -> usize {
        match self {
            Family::Gray => 1,
            Family::Rgb | Family::Lab => 3,
            Family::Cmyk => 4,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Family::Gray => "Gray",
            Family::Rgb => "RGB",
            Family::Cmyk => "CMYK",
            Family::Lab => "Lab",
        }
    }
}

/// `pdf_load_colorspace`: errors are MuPDF's report lines.
pub fn load(doc: &Document, obj: &Object) -> Result<ColorSpace, String> {
    load_imp(doc, obj, &mut Vec::new())
}

type Cycle = Vec<i32>;

fn device(name: &[u8]) -> Option<ColorSpace> {
    Some(match name {
        b"G" | b"DeviceGray" => ColorSpace::Gray,
        b"RGB" | b"DeviceRGB" => ColorSpace::Rgb,
        b"CMYK" | b"DeviceCMYK" => ColorSpace::Cmyk,
        _ => return None,
    })
}

/// `pdf_load_colorspace_imp`. `cycle` holds the objects being loaded, as
/// `pdf_cycle` tracks them: the number of an indirect one, 0 for a direct
/// one, which only counts toward the depth.
fn load_imp(doc: &Document, obj: &Object, cycle: &mut Cycle) -> Result<ColorSpace, String> {
    let num = obj.as_ref().map_or(0, |r| r.num);
    if (num != 0 && cycle.contains(&num)) || cycle.len() >= MAX_DEPTH {
        return Err(report::syntax("recursive colorspace"));
    }
    cycle.push(num);
    let out = load_resolved(doc, obj, cycle);
    cycle.pop();
    out
}

fn load_resolved(doc: &Document, obj: &Object, cycle: &mut Cycle) -> Result<ColorSpace, String> {
    let value = doc.resolve(obj);
    match &value {
        Object::Name(name) => {
            if name.as_ref() == b"Pattern" {
                return Ok(ColorSpace::Gray);
            }
            device(name).ok_or_else(|| {
                report::syntax(&format!(
                    "unknown colorspace: {}",
                    String::from_utf8_lossy(name)
                ))
            })
        }
        Object::Array(items) => {
            let family = items.first().map(|f| doc.resolve(f));
            let Some(Object::Name(family)) = family else {
                return Err(not_parsed(obj));
            };
            if let Some(cs) = device(&family) {
                return Ok(cs);
            }
            let get = |i: usize| items.get(i).cloned().unwrap_or(Object::Null);
            match family.as_ref() {
                b"CalCMYK" => Ok(ColorSpace::Cmyk),
                b"Lab" => Ok(ColorSpace::Lab),
                // A missing or broken calibration falls back to the device
                // space of the same shape.
                b"CalGray" => Ok(ColorSpace::Gray),
                b"CalRGB" => Ok(ColorSpace::Rgb),
                b"ICCBased" => load_icc_based(doc, &get(1), cycle),
                b"Indexed" | b"I" => load_indexed(doc, items, cycle),
                b"Separation" | b"DeviceN" => load_devicen(doc, items, cycle),
                b"Pattern" => match items.get(1) {
                    None => Ok(ColorSpace::Gray),
                    Some(sub) => load_imp(doc, sub, cycle),
                },
                other => Err(report::syntax(&format!(
                    "unknown colorspace {}",
                    String::from_utf8_lossy(other)
                ))),
            }
        }
        // `/DefaultRGB 1 0 R` naming an ICC profile stream directly.
        Object::Dict(_) | Object::Stream(_) => load_icc_based(doc, obj, cycle),
        _ => Err(not_parsed(obj)),
    }
}

fn not_parsed(obj: &Object) -> String {
    let num = obj.as_ref().map_or(0, |r| r.num);
    report::syntax(&format!("could not parse color space ({num} 0 R)"))
}

/// `load_icc_based`, with the profile judged by its header.
fn load_icc_based(doc: &Document, dict: &Object, cycle: &mut Cycle) -> Result<ColorSpace, String> {
    let n = doc.dict_get(dict, b"N").to_int();
    let diag = doc.diagnostics();
    let mut alt = None;
    let alt_obj = match doc
        .resolve(dict)
        .as_dict()
        .and_then(|d| d.get(b"Alternate"))
    {
        Some(o) if !matches!(doc.resolve(o), Object::Null) => Some(o.clone()),
        _ => None,
    };
    if let Some(o) = alt_obj {
        match load_imp(doc, &o, cycle) {
            Ok(cs) => alt = Some(cs),
            Err(e) => {
                diag.warn("error.reported", e);
                diag.warn(
                    "colorspace.broken_alternate",
                    "ignoring broken ICC Alternate colorspace",
                );
            }
        }
    }
    let mut cs = None;
    let profile = match doc.resolve(dict) {
        Object::Stream(s) => doc.stream_data(&s).map_err(|e| e.report()),
        _ => Err(report::format("object is not a stream")),
    }
    .and_then(|data| icc_colorspace(doc, &data, alt.as_ref()));
    match profile {
        Ok(family) => {
            let pn = family.n() as i32;
            if pn > n {
                diag.warn(
                    "colorspace.icc_n_mismatch",
                    format!(
                        "ICC colorspace N={n} does not match profile N={pn} (ignoring profile)"
                    ),
                );
            } else {
                if pn < n {
                    diag.warn(
                        "colorspace.icc_n_mismatch",
                        format!(
                            "ICC colorspace N={n} does not match profile N={pn} (using profile)"
                        ),
                    );
                }
                cs = Some(ColorSpace::Icc(family));
            }
        }
        Err(line) => {
            diag.warn("error.reported", line);
            diag.warn("colorspace.broken_icc", "ignoring broken ICC profile");
        }
    }
    if let Some(cs) = cs.or(alt) {
        return Ok(cs);
    }
    match n {
        1 => Ok(ColorSpace::Gray),
        3 => Ok(ColorSpace::Rgb),
        4 => Ok(ColorSpace::Cmyk),
        _ => Err(report::syntax("invalid ICC colorspace")),
    }
}

/// `fz_new_icc_colorspace` for a profile, with the family `alt` asks for
/// (any, without one).
pub fn icc_colorspace(
    doc: &Document,
    data: &[u8],
    alt: Option<&ColorSpace>,
) -> Result<Family, String> {
    let sig = icc_open(data).map_err(|lcms| {
        for message in lcms {
            doc.diagnostics()
                .warn("colorspace.lcms", format!("lcms: {message}."));
        }
        report::format("cmsOpenProfileFromMem failed")
    })?;
    let n = icc_channels(sig);
    let lab = &sig == b"Lab ";
    let want = match alt {
        None => None,
        Some(cs) => Some(
            cs.family()
                .ok_or_else(|| report::syntax("invalid colorspace type for ICC profile"))?,
        ),
    };
    let not = |what: &str| report::syntax(&format!("ICC profile (N={n}) is not {what}"));
    Ok(match want {
        None => match n {
            1 => Family::Gray,
            3 if lab => Family::Lab,
            3 => Family::Rgb,
            4 => Family::Cmyk,
            _ => {
                return Err(report::syntax(&format!(
                    "ICC profile has unexpected number of channels: {n}"
                )));
            }
        },
        Some(Family::Gray) if n != 1 => return Err(not("Gray")),
        Some(Family::Rgb) if n != 3 || lab => return Err(not("RGB")),
        Some(Family::Lab) if n != 3 || !lab => return Err(not("Lab")),
        Some(Family::Cmyk) if n != 4 => return Err(not("CMYK")),
        Some(f) => f,
    })
}

/// lcms's `_cmsReadHeader`: the profile's data colour space signature, or
/// the errors lcms signals while opening it.
fn icc_open(data: &[u8]) -> Result<[u8; 4], Vec<String>> {
    let read_error = || vec!["Read from memory error".to_string()];
    if data.is_empty() {
        return Err(vec!["Couldn't allocate 0 bytes for profile".to_string()]);
    }
    let u32_at = |at: usize| {
        data.get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    if data.len() < 128 {
        return Err(read_error());
    }
    if &data[36..40] != b"acsp" {
        return Err(vec!["not an ICC profile, invalid signature".to_string()]);
    }
    // _validatedVersion: the major version is clamped to 9 and the minor
    // nibbles to 9, the rest dropped.
    let major = data[8].min(9);
    let minor = (data[9] & 0xf0).min(0x90) | (data[9] & 0x0f).min(0x09);
    let version = u32::from(major) << 24 | u32::from(minor) << 16;
    if version > 0x0500_0000 {
        return Err(vec![format!("Unsupported profile version '0x{version:x}'")]);
    }
    let class = u32_at(12).unwrap_or(0);
    let valid_class = class == 0
        || matches!(
            &data[12..16],
            b"scnr"
                | b"mntr"
                | b"prtr"
                | b"link"
                | b"abst"
                | b"spac"
                | b"nmcl"
                | b"cenc"
                | b"mid "
                | b"mlnk"
                | b"mvis"
        );
    if !valid_class {
        return Err(vec![format!("Unsupported device class '0x{class:x}'")]);
    }
    let header_size = u32_at(0).unwrap_or(0).min(data.len() as u32);
    let count = u32_at(128).ok_or_else(read_error)?;
    if count > 100 {
        return Err(vec![format!("Too many tags ({count})")]);
    }
    let mut names = Vec::new();
    for i in 0..count as usize {
        let at = 132 + i * 12;
        let (Some(sig), Some(offset), Some(size)) = (u32_at(at), u32_at(at + 4), u32_at(at + 8))
        else {
            return Err(read_error());
        };
        if size == 0 || offset == 0 {
            continue;
        }
        match offset.checked_add(size) {
            Some(end) if end <= header_size => names.push(sig),
            _ => continue,
        }
    }
    for (i, a) in names.iter().enumerate() {
        if names[i + 1..].contains(a) {
            return Err(vec!["Duplicate tag found".to_string()]);
        }
    }
    Ok([data[16], data[17], data[18], data[19]])
}

/// lcms's `cmsChannelsOf`: unknown colour spaces count as 3.
fn icc_channels(sig: [u8; 4]) -> usize {
    match &sig {
        b"GRAY" | b"MCH1" | b"1CLR" => 1,
        b"MCH2" | b"2CLR" => 2,
        b"XYZ " | b"Lab " | b"Luv " | b"YCbr" | b"Yxy " | b"RGB " | b"HSV " | b"HLS " | b"CMY "
        | b"MCH3" | b"3CLR" => 3,
        b"LuvK" | b"CMYK" | b"MCH4" | b"4CLR" => 4,
        [b'M', b'C', b'H', d @ b'5'..=b'9'] | [d @ b'5'..=b'9', b'C', b'L', b'R'] => {
            usize::from(d - b'0')
        }
        [b'M', b'C', b'H', d @ b'A'..=b'F'] | [d @ b'A'..=b'F', b'C', b'L', b'R'] => {
            usize::from(d - b'A') + 10
        }
        _ => 3,
    }
}

fn load_indexed(doc: &Document, items: &[Object], cycle: &mut Cycle) -> Result<ColorSpace, String> {
    let get = |i: usize| items.get(i).cloned().unwrap_or(Object::Null);
    let base = load_imp(doc, &get(1), cycle)?;
    let high = doc.resolve(&get(2)).to_int().clamp(0, 255);
    let n = base.n() * (high as usize + 1);
    let lookup_obj = get(3);
    let mut lookup = match doc.resolve(&lookup_obj) {
        Object::String(s) => s.to_vec(),
        // pdf_open_stream, which throws for anything but a stream.
        Object::Stream(s) if matches!(lookup_obj, Object::Ref(_)) => {
            doc.stream_data(&s).map_err(|e| e.report())?
        }
        _ if matches!(lookup_obj, Object::Ref(_)) => {
            return Err(report::format("object is not a stream"));
        }
        _ => return Err(report::syntax("cannot parse colorspace lookup table")),
    };
    // Short tables are padded with zeros, long ones cut.
    lookup.resize(n, 0);
    Ok(ColorSpace::Indexed {
        base: Box::new(base),
        high: high as u8,
        lookup,
    })
}

fn load_devicen(doc: &Document, items: &[Object], cycle: &mut Cycle) -> Result<ColorSpace, String> {
    let get = |i: usize| items.get(i).cloned().unwrap_or(Object::Null);
    let n = match doc.resolve(&get(1)) {
        Object::Array(names) => {
            if names.is_empty() {
                return Err(report::syntax("too few components in DeviceN colorspace"));
            }
            if names.len() > MAX_COLORS {
                return Err(report::syntax("too many components in DeviceN colorspace"));
            }
            names.len()
        }
        _ => 1,
    };
    let base = load_imp(doc, &get(2), cycle)?;
    let tint = function::load(doc, &get(3), n, base.n())?;
    Ok(ColorSpace::Separation {
        n,
        base: Box::new(base),
        tint: Box::new(tint),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Document {
        Document::open(
            &b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n2 0 obj[/Indexed 2 0 R 1 <00>]endobj\n\
trailer<</Root 1 0 R/Size 3>>\n"[..],
        )
        .unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn parse(text: &str) -> Object {
        let diag = crate::Diagnostics::new();
        let mut lx = crate::lex::Lexer::new(text.as_bytes(), 0, &diag);
        crate::parse::parse_stm_obj(&mut lx).unwrap_or_else(|e| panic!("{e:?}"))
    }

    #[test]
    fn names_and_arrays_give_their_component_counts() {
        let d = doc();
        let n = |t: &str| load(&d, &parse(t)).map(|cs| cs.n());
        assert_eq!(n("/RGB"), Ok(3));
        assert_eq!(n("/Pattern"), Ok(1));
        assert_eq!(n("[/CalRGB <<>>]"), Ok(3));
        let tint = "<</FunctionType 2/Domain[0 1]/N 1>>";
        assert_eq!(n(&format!("[/DeviceN [/A /B] /CMYK {tint}]")), Ok(2));
        assert_eq!(n(&format!("[/Separation /Spot /RGB {tint}]")), Ok(1));
        // A missing tint transform reads as a sample function with no
        // sizes.
        assert_eq!(
            n("[/Separation /Spot /RGB null]"),
            Err("syntax error: too few sample function dimension sizes".into())
        );
        assert_eq!(n("[/I /RGB 1 <ff0000>]"), Ok(1));
        assert_eq!(
            n("/Indexed"),
            Err("syntax error: unknown colorspace: Indexed".into())
        );
        assert_eq!(
            n("[/Indexed /RGB 1 5]"),
            Err("syntax error: cannot parse colorspace lookup table".into())
        );
        assert_eq!(
            n("[/Foo]"),
            Err("syntax error: unknown colorspace Foo".into())
        );
    }

    /// A profile header: size, version, class, colour space, then the
    /// tag count.
    fn profile(version: u8, class: &[u8; 4], space: &[u8; 4], tags: u32) -> Vec<u8> {
        let mut p = vec![0u8; 132];
        p[0..4].copy_from_slice(&132u32.to_be_bytes());
        p[8] = version;
        p[12..16].copy_from_slice(class);
        p[16..20].copy_from_slice(space);
        p[36..40].copy_from_slice(b"acsp");
        p[128..132].copy_from_slice(&tags.to_be_bytes());
        p
    }

    #[test]
    fn icc_headers_are_checked_as_lcms_opens_them() {
        let d = doc();
        let fam = |data: &[u8], alt: Option<&ColorSpace>| icc_colorspace(&d, data, alt);
        assert_eq!(fam(&profile(4, b"mntr", b"RGB ", 0), None), Ok(Family::Rgb));
        assert_eq!(fam(&profile(2, b"prtr", b"Lab ", 0), None), Ok(Family::Lab));
        assert_eq!(
            fam(&profile(2, b"scnr", b"GRAY", 0), None),
            Ok(Family::Gray)
        );
        // Unknown spaces count three channels.
        assert_eq!(fam(&profile(2, b"spac", b"Foo ", 0), None), Ok(Family::Rgb));
        assert_eq!(
            fam(&profile(2, b"mntr", b"5CLR", 0), None),
            Err("syntax error: ICC profile has unexpected number of channels: 5".into())
        );
        // The family must be the alternate's.
        assert_eq!(
            fam(&profile(2, b"mntr", b"RGB ", 0), Some(&ColorSpace::Cmyk)),
            Err("syntax error: ICC profile (N=3) is not CMYK".into())
        );
        let open = "format error: cmsOpenProfileFromMem failed";
        assert_eq!(
            fam(&profile(6, b"mntr", b"RGB ", 0), None),
            Err(open.into())
        );
        assert_eq!(
            fam(&profile(2, b"what", b"RGB ", 0), None),
            Err(open.into())
        );
        assert_eq!(
            fam(&profile(2, b"mntr", b"RGB ", 101), None),
            Err(open.into())
        );
        assert_eq!(fam(b"short", None), Err(open.into()));
        let messages: Vec<String> = d
            .diagnostics()
            .snapshot()
            .into_iter()
            .map(|w| w.message)
            .collect();
        for m in [
            "lcms: Unsupported profile version '0x6000000'.",
            "lcms: Unsupported device class '0x77686174'.",
            "lcms: Too many tags (101).",
            "lcms: Read from memory error.",
        ] {
            assert!(messages.contains(&m.to_string()), "{m} in {messages:?}");
        }
    }

    #[test]
    fn an_indexed_lookup_is_padded_and_recursion_fails() {
        let d = doc();
        let cs = load(&d, &parse("[/Indexed /RGB 1 <ff>]"));
        assert_eq!(
            cs.map(|c| match c {
                ColorSpace::Indexed { lookup, .. } => lookup,
                _ => Vec::new(),
            }),
            Ok(vec![0xff, 0, 0, 0, 0, 0])
        );
        assert_eq!(
            load(&d, &Object::Ref(crate::ObjRef::new(2, 0))),
            Err("syntax error: recursive colorspace".into())
        );
    }
}
