// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::{Document, Object};

fuzz_target!(|data: &[u8]| {
    let Ok(doc) = Document::open(data.to_vec()) else {
        return;
    };
    let _ = doc.version();
    let _ = doc.page_count();
    let n = doc.xref_len().min(4096);
    for num in 1..n {
        if let Some(Object::Stream(s)) = doc.object(num as i32) {
            let _ = doc.raw_stream_data(&s).map(|raw| raw.len());
            let _ = doc.stream_data(&s);
        }
    }
});
