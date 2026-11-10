// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use nupdf::filter;
use nupdf::{Dict, Limits, Object};

const FILTERS: [&[u8]; 9] = [
    b"FlateDecode",
    b"LZWDecode",
    b"BrotliDecode",
    b"RunLengthDecode",
    b"ASCIIHexDecode",
    b"ASCII85Decode",
    b"JPXDecode",
    b"Crypt",
    b"Unknown",
];

// Byte 0 picks up to two filters, bytes 1-4 the predictor parameters of the
// last one; the rest is the encoded data, decoded whole and byte by byte.
fuzz_target!(|data: &[u8]| {
    let Some((head, body)) = data.split_at_checked(5) else {
        return;
    };
    let pick = |b: u8| Object::name(FILTERS[usize::from(b) % FILTERS.len()]);
    let mut dict = Dict::new();
    if head[0] & 0x80 != 0 {
        dict.put(
            b"Filter",
            Object::Array(Arc::new(vec![pick(head[0] & 0x0f), pick(head[0] >> 4 & 0x07)])),
        );
    } else {
        dict.put(b"Filter", pick(head[0]));
    }
    let mut params = Dict::new();
    let p = |i: usize| i64::from(head[i]);
    params.put(b"Predictor", Object::Int([1, 2, 10, 11, 12, 13, 14, 15, 7][(p(1) % 9) as usize]));
    params.put(b"Columns", Object::Int(p(2) * 4 - 3));
    params.put(b"Colors", Object::Int(p(3) % 40 - 2));
    params.put(b"BitsPerComponent", Object::Int([1, 2, 4, 8, 16, 3][(p(4) % 6) as usize]));
    params.put(b"EarlyChange", Object::Int(p(4) & 1));
    dict.put(b"DecodeParms", Object::Dict(Arc::new(params)));

    let diag = nupdf::Diagnostics::new();
    let limits = Limits {
        max_decoded_bytes: 1 << 26,
        ..Limits::default()
    };
    let _ = filter::decode(&dict, body, &|o| o.clone(), &diag, &limits);
    let raw = nupdf::stream::Stream::memory(body, &diag);
    if let Ok(mut s) = filter::open_filters(raw, &dict, &|o| o.clone(), None, None, true) {
        let _ = s.read_bytewise(1 << 26);
    }
});
