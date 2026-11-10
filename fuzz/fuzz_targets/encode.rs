// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use nupdf::{Diagnostics, Dict, Limits, Object};

/// `data` through `filter` with `parms`, as a reader decodes it.
fn decode(data: &[u8], filter: &[u8], parms: Option<Dict>) -> Vec<u8> {
    let mut dict = Dict::new();
    dict.put(b"Filter", Object::name(filter));
    if let Some(p) = parms {
        dict.put(b"DecodeParms", Object::Dict(Arc::new(p)));
    }
    let diag = Diagnostics::new();
    nupdf::filter::decode(&dict, data, &|o| o.clone(), &diag, &Limits::default())
        .unwrap_or_default()
}

// The encoders `clean -z` uses, on any input: deflate at the level the
// first byte picks, and CCITT G4 over rows of a width the second picks.
// Both must decode back to their input.
fuzz_target!(|input: &[u8]| {
    let Some((&[level, width], data)) = input.split_first_chunk::<2>() else {
        return;
    };
    let level = i32::from(level % 11) - 1;
    let deflated = nupdf::deflate::compress(data, level).unwrap_or_default();
    assert!(deflated.len() <= nupdf::deflate::compress_bound(data.len()));
    assert_eq!(decode(&deflated, b"FlateDecode", None), data, "deflate level {level}");

    let columns = usize::from(width) + 1;
    let stride = columns.div_ceil(8);
    let rows = data.len() / stride;
    let image = &data[..rows * stride];
    let fax = nupdf::fax_encode::compress_g4(image, columns as i32, rows as i32, stride, 1 << 24)
        .unwrap_or_default();
    let mut parms = Dict::new();
    parms.put(b"K", Object::Int(-1));
    parms.put(b"Columns", Object::Int(columns as i64));
    let back = decode(&fax, b"CCITTFaxDecode", Some(parms));
    // The bits that pad a row to a byte come back white.
    let pad = if columns % 8 == 0 { 0 } else { 0xffu8 >> (columns % 8) };
    let padded: Vec<u8> = image
        .chunks(stride)
        .flat_map(|row| {
            let mut row = row.to_vec();
            if let Some(last) = row.last_mut() {
                *last |= pad;
            }
            row
        })
        .collect();
    assert_eq!(back, padded, "g4 columns {columns}");
});
