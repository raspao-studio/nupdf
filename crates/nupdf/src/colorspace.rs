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
//! reading one. An ICC profile is judged by the component count its header
//! declares; the profile itself is not parsed.

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
    /// An ICCBased space, with the components it has.
    Icc(usize),
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
            ColorSpace::Icc(n) => *n,
            ColorSpace::Indexed { .. } => 1,
            ColorSpace::Separation { n, .. } => *n,
        }
    }

    pub fn is_indexed(&self) -> bool {
        matches!(self, ColorSpace::Indexed { .. })
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

/// `load_icc_based` with the profile judged by its header.
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
    let profile_n = match doc.resolve(dict) {
        Object::Stream(s) => doc.stream_data(&s).ok().and_then(|d| icc_components(&d)),
        _ => None,
    };
    let mut cs = None;
    if let Some(pn) = profile_n {
        let pn_i = i32::try_from(pn).unwrap_or(i32::MAX);
        if pn_i > n {
            diag.warn(
                "colorspace.icc_n_mismatch",
                format!("ICC colorspace N={n} does not match profile N={pn} (ignoring profile)"),
            );
        } else {
            if pn_i < n {
                diag.warn(
                    "colorspace.icc_n_mismatch",
                    format!("ICC colorspace N={n} does not match profile N={pn} (using profile)"),
                );
            }
            cs = Some(ColorSpace::Icc(pn));
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

/// The component count an ICC profile header declares (its data colour
/// space signature), or None for data that is not a profile.
fn icc_components(data: &[u8]) -> Option<usize> {
    if data.len() < 128 || &data[36..40] != b"acsp" {
        return None;
    }
    Some(match &data[16..20] {
        b"GRAY" => 1,
        b"RGB " | b"Lab " | b"XYZ " | b"Luv " | b"YCbr" | b"Yxy " | b"HSV " | b"HLS " | b"CMY " => {
            3
        }
        b"CMYK" => 4,
        [n @ b'2'..=b'9', b'C', b'L', b'R'] => usize::from(n - b'0'),
        [n @ b'A'..=b'F', b'C', b'L', b'R'] => usize::from(n - b'A') + 10,
        _ => return None,
    })
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
