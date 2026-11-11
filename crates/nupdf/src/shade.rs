// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-shade.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Shadings, loaded as `pdf_load_shading` loads them.
//!
//! Loading is where MuPDF does most of a shading's work: the colorspace
//! and functions are loaded, and the functions sampled, 256 times along an
//! axial or radial shading's axis and on a 257 by 257 grid for a
//! function-based one. Mesh shadings keep their decoded stream for later.

use crate::colorspace::{self, ColorSpace, MAX_COLORS};
use crate::diag::report;
use crate::document::Document;
use crate::function::{self, Function};
use crate::geometry::{Matrix, Rect};
use crate::object::Object;

/// `FUNSEGS`: the grid a function-based shading is sampled on.
const FUNSEGS: usize = 256;

/// `fz_shade`.
#[derive(Debug, Clone, PartialEq)]
pub struct Shade {
    /// ShadingType, 1 to 7.
    pub kind: i32,
    pub colorspace: ColorSpace,
    pub background: Option<Vec<f32>>,
    pub bbox: Rect,
    /// A shading pattern's /Matrix.
    pub matrix: Matrix,
    /// The functions sampled at 256 points: `function_stride` floats each,
    /// the last of which is 1. Empty for function-based shadings and
    /// meshes without functions.
    pub function: Vec<f32>,
    pub function_stride: usize,
    pub data: ShadeData,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ShadeData {
    /// Type 1: the functions evaluated on the grid.
    Function {
        matrix: Matrix,
        /// `[[x0, y0], [x1, y1]]`.
        domain: [[f32; 2]; 2],
        divs: usize,
        values: Vec<f32>,
    },
    /// Types 2 and 3: the axis (with radii for radial shadings) and
    /// whether it extends.
    Axial {
        coords: [[f32; 3]; 2],
        extend: [bool; 2],
    },
    /// Types 4 to 7.
    Mesh(Box<Mesh>),
}

/// A mesh shading's parameters and its decoded stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Mesh {
    pub vertices_per_row: i32,
    pub bits_per_flag: i32,
    pub bits_per_coordinate: i32,
    pub bits_per_component: i32,
    pub x: [f32; 2],
    pub y: [f32; 2],
    pub c0: [f32; MAX_COLORS],
    pub c1: [f32; MAX_COLORS],
    pub data: Vec<u8>,
}

impl Shade {
    /// `fz_bound_shade`: axial and radial shadings by their /BBox, the
    /// others by their domain or mesh decode range within it.
    pub fn bound(&self, ctm: Matrix) -> Rect {
        let ctm = self.matrix.concat(ctm);
        let mesh = match &self.data {
            ShadeData::Function { matrix, domain, .. } => {
                Rect::new(domain[0][0], domain[0][1], domain[1][0], domain[1][1]).transform(*matrix)
            }
            ShadeData::Mesh(m) => {
                let min = |a: f32, b: f32| if a < b { a } else { b };
                let max = |a: f32, b: f32| if a > b { a } else { b };
                Rect::new(
                    min(m.x[0], m.x[1]),
                    min(m.y[0], m.y[1]),
                    max(m.x[0], m.x[1]),
                    max(m.y[0], m.y[1]),
                )
            }
            ShadeData::Axial { .. } => return self.bbox.transform(ctm),
        };
        mesh.intersect(self.bbox).transform(ctm)
    }
}

/// `pdf_to_matrix`: identity unless it is an array.
fn to_matrix(doc: &Document, v: &Object) -> Matrix {
    let Object::Array(a) = doc.resolve(v) else {
        return Matrix::IDENTITY;
    };
    let n = |i: usize| a.get(i).map_or(0.0, |v| doc.resolve(v).to_real());
    Matrix {
        a: n(0),
        b: n(1),
        c: n(2),
        d: n(3),
        e: n(4),
        f: n(5),
    }
}

/// A key as `pdf_dict_get` finds it: present unless stored as null.
fn raw(doc: &Document, dict: &Object, key: &[u8]) -> Option<Object> {
    doc.resolve(dict)
        .as_dict()
        .and_then(|d| d.get(key))
        .filter(|v| !matches!(v, Object::Null))
        .cloned()
}

/// `pdf_load_shading`: a shading dictionary, or a shading pattern's.
pub fn load(doc: &Document, obj: &Object) -> Result<Shade, String> {
    if raw(doc, obj, b"PatternType").is_some() {
        let matrix = to_matrix(doc, &doc.dict_get(obj, b"Matrix"));
        if let Some(gs) = raw(doc, obj, b"ExtGState")
            && (raw(doc, &gs, b"CA").is_some() || raw(doc, &gs, b"ca").is_some())
        {
            doc.diagnostics()
                .warn("shade.alpha", "shading with alpha not supported");
        }
        let Some(shading) = raw(doc, obj, b"Shading") else {
            return Err(report::syntax("missing shading dictionary"));
        };
        load_dict(doc, &shading, matrix)
    } else {
        load_dict(doc, obj, Matrix::IDENTITY)
    }
}

/// `pdf_load_shading_dict`.
fn load_dict(doc: &Document, dict: &Object, matrix: Matrix) -> Result<Shade, String> {
    let get = |key: &[u8]| doc.dict_get(dict, key);
    let kind = get(b"ShadingType").to_int();
    let Some(cs) = raw(doc, dict, b"ColorSpace") else {
        return Err(report::syntax("shading colorspace is missing"));
    };
    let colorspace = colorspace::load(doc, &cs)?;
    let n = colorspace.n();
    let background = raw(doc, dict, b"Background").map(|bg| {
        let a = doc.resolve(&bg);
        let a = a.as_array().unwrap_or_default();
        (0..n)
            .map(|i| a.get(i).map_or(0.0, |v| doc.resolve(v).to_real()))
            .collect()
    });
    let bbox = match get(b"BBox") {
        b @ Object::Array(_) => doc.to_rect(Some(&b)),
        _ => Rect::INFINITE,
    };
    let inputs = if kind == 1 { 2 } else { 1 };
    let mut funcs: Vec<Function> = Vec::new();
    let fobj = raw(doc, dict, b"Function");
    match fobj.as_ref().map(|f| doc.resolve(f)) {
        Some(f) if f.as_dict().is_some() => {
            let f = fobj.clone().unwrap_or(f);
            funcs.push(function::load(doc, &f, inputs, n)?);
        }
        Some(Object::Array(items)) => {
            if items.len() != 1 && items.len() != n {
                return Err(report::syntax("incorrect number of shading functions"));
            }
            if items.len() > MAX_COLORS {
                return Err(report::syntax("too many shading functions"));
            }
            for item in items.iter() {
                funcs.push(function::load(doc, item, inputs, 1)?);
            }
        }
        _ if kind < 4 => {
            let num = fobj.as_ref().and_then(Object::as_ref).map_or(0, |r| r.num);
            return Err(report::syntax(&format!(
                "cannot load shading function ({num} 0 R)"
            )));
        }
        _ => {}
    }
    let mut shade = Shade {
        kind,
        colorspace,
        background,
        bbox,
        matrix,
        function: Vec::new(),
        function_stride: 0,
        data: ShadeData::Axial {
            coords: [[0.0; 3]; 2],
            extend: [false; 2],
        },
    };
    let reals = |key: &[u8], count: usize| -> Vec<f32> {
        let v = get(key);
        let a = v.as_array().unwrap_or_default();
        (0..count)
            .map(|i| a.get(i).map_or(0.0, |v| doc.resolve(v).to_real()))
            .collect()
    };
    match kind {
        1 => {
            let (mut x0, mut y0, mut x1, mut y1) = (0.0f32, 0.0f32, 1.0f32, 1.0f32);
            if raw(doc, dict, b"Domain").is_some() {
                let d = reals(b"Domain", 4);
                (x0, x1, y0, y1) = (d[0], d[1], d[2], d[3]);
            }
            if funcs.len() != 1 && funcs.len() != n {
                return Err(report::syntax(
                    "Expected 1 2in, n-out function, or n 2 in, 1-out functions",
                ));
            }
            let divs = FUNSEGS;
            let mut values = Vec::with_capacity((divs + 1) * (divs + 1) * n);
            let mut out = vec![0.0f32; n];
            for yy in 0..=divs {
                let fy = y0 + (y1 - y0) * yy as f32 / divs as f32;
                for xx in 0..=divs {
                    let fx = x0 + (x1 - x0) * xx as f32 / divs as f32;
                    if funcs.len() == 1 {
                        funcs[0].eval(doc, &[fx, fy], &mut out);
                        values.extend_from_slice(&out);
                    } else {
                        for f in &funcs {
                            let mut one = [0.0f32];
                            f.eval(doc, &[fx, fy], &mut one);
                            values.push(one[0]);
                        }
                    }
                }
            }
            shade.data = ShadeData::Function {
                matrix: to_matrix(doc, &get(b"Matrix")),
                domain: [[x0, y0], [x1, y1]],
                divs,
                values,
            };
        }
        2 | 3 => {
            let c = reals(b"Coords", if kind == 2 { 4 } else { 6 });
            let coords = if kind == 2 {
                [[c[0], c[1], 0.0], [c[2], c[3], 0.0]]
            } else {
                [[c[0], c[1], c[2]], [c[3], c[4], c[5]]]
            };
            let (mut d0, mut d1) = (0.0f32, 1.0f32);
            if raw(doc, dict, b"Domain").is_some() {
                let d = reals(b"Domain", 2);
                (d0, d1) = (d[0], d[1]);
            }
            let mut extend = [false; 2];
            if let Some(e) = raw(doc, dict, b"Extend") {
                let e = doc.resolve(&e);
                let a = e.as_array().unwrap_or_default();
                for (i, x) in extend.iter_mut().enumerate() {
                    *x = matches!(a.get(i).map(|v| doc.resolve(v)), Some(Object::Bool(true)));
                }
            }
            sample(doc, &mut shade, &funcs, d0, d1);
            shade.data = ShadeData::Axial { coords, extend };
        }
        4..=7 => {
            let mut mesh = mesh_params(doc, dict, kind);
            if !funcs.is_empty() {
                sample(doc, &mut shade, &funcs, mesh.c0[0], mesh.c1[0]);
            }
            // pdf_load_compressed_stream, by the shading's object number.
            mesh.data = match dict.as_ref().and_then(|r| doc.object(r.num)) {
                Some(Object::Stream(s)) => doc.stream_data(&s).map_err(|e| e.report())?,
                _ => return Err(report::format("object is not a stream")),
            };
            shade.data = ShadeData::Mesh(Box::new(mesh));
        }
        other => return Err(report::syntax(&format!("unknown shading type: {other}"))),
    }
    Ok(shade)
}

/// `make_sampled_shade_function`.
fn sample(doc: &Document, shade: &mut Shade, funcs: &[Function], t0: f32, t1: f32) {
    let n = if funcs.len() == 1 {
        shade.colorspace.n()
    } else {
        funcs.len()
    };
    shade.function_stride = n + 1;
    shade.function = Vec::with_capacity(256 * (n + 1));
    let mut out = vec![0.0f32; n];
    for i in 0..256 {
        let t = t0 + (i as f32 / 255.0) * (t1 - t0);
        if funcs.len() == 1 {
            funcs[0].eval(doc, &[t], &mut out);
        } else {
            for (k, f) in funcs.iter().enumerate() {
                f.eval(doc, &[t], &mut out[k..k + 1]);
            }
        }
        shade.function.extend_from_slice(&out);
        shade.function.push(1.0);
    }
}

/// `pdf_load_mesh_params`.
fn mesh_params(doc: &Document, dict: &Object, kind: i32) -> Mesh {
    let diag = doc.diagnostics();
    let int = |key: &[u8]| doc.dict_get(dict, key).to_int();
    let mut vprow = int(b"VerticesPerRow");
    let mut bpflag = int(b"BitsPerFlag");
    let mut bpcoord = int(b"BitsPerCoordinate");
    let mut bpcomp = int(b"BitsPerComponent");
    let (mut x, mut y) = ([0.0f32, 1.0], [0.0f32, 1.0]);
    let mut c0 = [0.0f32; MAX_COLORS];
    let mut c1 = [1.0f32; MAX_COLORS];
    if let Object::Array(d) = doc.dict_get(dict, b"Decode")
        && d.len() >= 6
    {
        let r = |i: usize| d.get(i).map_or(0.0, |v| doc.resolve(v).to_real());
        let n = MAX_COLORS.min((d.len() - 4) / 2);
        x = [r(0), r(1)];
        y = [r(2), r(3)];
        for i in 0..n {
            c0[i] = r(4 + i * 2);
            c1[i] = r(5 + i * 2);
        }
    }
    if vprow < 2 && kind == 5 {
        diag.warn(
            "shade.vertices_per_row",
            format!("Too few vertices per row ({vprow})"),
        );
        vprow = 2;
    }
    if !matches!(bpflag, 2 | 4 | 8) && kind != 5 {
        diag.warn(
            "shade.bits_per_flag",
            format!("Invalid number of bits per flag ({bpflag})"),
        );
        bpflag = 8;
    }
    if !matches!(bpcoord, 1 | 2 | 4 | 8 | 12 | 16 | 24 | 32) {
        diag.warn(
            "shade.bits_per_coordinate",
            format!("Invalid number of bits per coordinate ({bpcoord})"),
        );
        bpcoord = 8;
    }
    if !matches!(bpcomp, 1 | 2 | 4 | 8 | 12 | 16) {
        diag.warn(
            "shade.bits_per_component",
            format!("Invalid number of bits per component ({bpcomp})"),
        );
        bpcomp = 8;
    }
    Mesh {
        vertices_per_row: vprow,
        bits_per_flag: bpflag,
        bits_per_coordinate: bpcoord,
        bits_per_component: bpcomp,
        x,
        y,
        c0,
        c1,
        data: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with(objects: &[&str]) -> Document {
        let mut file = b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n".to_vec();
        for (i, body) in objects.iter().enumerate() {
            file.extend_from_slice(format!("{} 0 obj{body}endobj\n", i + 2).as_bytes());
        }
        file.extend_from_slice(
            format!("trailer<</Root 1 0 R/Size {}>>\n", objects.len() + 2).as_bytes(),
        );
        Document::open(file).unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn obj(num: i32) -> Object {
        Object::Ref(crate::ObjRef::new(num, 0))
    }

    #[test]
    fn an_axial_shading_samples_its_function() {
        let doc = doc_with(&[
            "<</ShadingType 2/ColorSpace/DeviceGray/Coords[0 0 1 0]/Function<</FunctionType 2/Domain[0 1]/N 1>>/Extend[true false]>>",
        ]);
        let shade = load(&doc, &obj(2)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(shade.function_stride, 2);
        assert_eq!(&shade.function[..2], [0.0, 1.0]);
        assert_eq!(&shade.function[510..], [1.0, 1.0]);
        assert_eq!(
            shade.data,
            ShadeData::Axial {
                coords: [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]],
                extend: [true, false]
            }
        );
    }

    #[test]
    fn shading_failures_are_mupdfs() {
        let err = |objects: &[&str]| load(&doc_with(objects), &obj(2)).err();
        assert_eq!(
            err(&["<</ShadingType 2/Coords[0 0 1 0]>>"]).as_deref(),
            Some("syntax error: shading colorspace is missing")
        );
        assert_eq!(
            err(&["<</ShadingType 2/ColorSpace/RGB>>"]).as_deref(),
            Some("syntax error: cannot load shading function (0 0 R)")
        );
        assert_eq!(
            err(&["<</ShadingType 9/ColorSpace/G/Function<</FunctionType 2/N 1>>>>"]).as_deref(),
            Some("syntax error: unknown shading type: 9")
        );
        assert_eq!(
            err(&["<</ShadingType 4/ColorSpace/G/BitsPerFlag 8/BitsPerCoordinate 8/BitsPerComponent 8>>"])
                .as_deref(),
            Some("format error: object is not a stream")
        );
        assert_eq!(
            err(&["<</PatternType 2>>"]).as_deref(),
            Some("syntax error: missing shading dictionary")
        );
    }
}
