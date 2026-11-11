// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use nupdf::shade;
use nupdf::{Dict, Document, ObjRef, Object};

fn reals(values: &[f32]) -> Object {
    Object::Array(Arc::new(values.iter().map(|&v| Object::Real(v)).collect()))
}

// The first byte picks a calculator or a sampled function and its shape;
// the rest is the function's stream. It is loaded, evaluated, and used as
// an axial shading's function, which samples it 256 times.
fuzz_target!(|data: &[u8]| {
    let Ok(doc) = Document::create() else {
        return;
    };
    let pick = data.first().copied().unwrap_or(0);
    let body = data.get(1..).unwrap_or_default().to_vec();
    let (m, n) = (usize::from(pick >> 6) % 2 + 1, usize::from(pick >> 4) % 3 + 1);
    let mut dict = Dict::new();
    dict.put(b"Domain", reals(&[0.0, 1.0].repeat(m)));
    dict.put(b"Range", reals(&[-1.0, 2.0].repeat(n)));
    if pick & 1 == 0 {
        dict.put(b"FunctionType", Object::Int(4));
    } else {
        let bps = [1, 2, 4, 8, 12, 16, 24, 32][usize::from(pick >> 1) % 8];
        dict.put(b"FunctionType", Object::Int(0));
        dict.put(b"BitsPerSample", Object::Int(bps));
        dict.put(
            b"Size",
            Object::Array(Arc::new(vec![Object::Int(3); m])),
        );
    }
    let num = doc.add_stream(dict, body);
    let func = Object::Ref(ObjRef::new(num, 0));
    if let Ok(f) = nupdf::function::load(&doc, &func, m, n) {
        let mut out = [0.0f32; 4];
        for x in [0.0f32, 0.25, 0.5, 1.0, -3.0, f32::NAN] {
            f.eval(&doc, &[x, 1.0 - x], &mut out);
        }
    }
    let mut sh = Dict::new();
    sh.put(b"ShadingType", Object::Int(2));
    sh.put(b"ColorSpace", Object::name(b"DeviceRGB"));
    sh.put(b"Coords", reals(&[0.0, 0.0, 1.0, 1.0]));
    sh.put(b"Function", func);
    let _ = shade::load(&doc, &Object::Dict(Arc::new(sh)));
});
