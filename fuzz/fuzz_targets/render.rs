// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::Document;
use nupdf::geometry::Matrix;

const STACK_SIZE: usize = 1 << 30;

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    let worker = std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || one(&data));
    if let Ok(handle) = worker
        && let Err(panic) = handle.join()
    {
        std::panic::resume_unwind(panic);
    }
});

// A document's first pages rendered at a small size, with and without
// color management, whole and in bands: glyph outlines, images, shadings
// and every painter reached from input.
fn one(data: &[u8]) {
    let Ok(doc) = Document::open(data.to_vec()) else {
        return;
    };
    let pages = doc.page_count().unwrap_or(0).min(4);
    let ctm = Matrix::scale(0.5, 0.5);
    for i in 0..pages {
        let Ok(page) = doc.page(i) else {
            continue;
        };
        let icc = i % 2 == 0;
        let store = Default::default();
        let _ = nupdf::draw::render_page_banded(&doc, &page, ctm, 1 << 20, None, store, 2, icc);
    }
}
