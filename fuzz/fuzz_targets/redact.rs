// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::Document;
use nupdf::geometry::{Point, Quad};
use nupdf::redact::{RedactOptions, redact_page};

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

// A document's content streams sanitized as `mutool clean -s` does, then
// its pages redacted under a quad over the middle of each, and the result
// written out.
fn one(data: &[u8]) {
    let Ok(doc) = Document::open(data.to_vec()) else {
        return;
    };
    let _ = nupdf::content_clean::clean_content_streams(&doc, true, false, false);
    let pages = doc.page_count().unwrap_or(0).min(8);
    let q = Quad {
        ul: Point { x: 100.0, y: 500.0 },
        ur: Point { x: 400.0, y: 500.0 },
        ll: Point { x: 100.0, y: 300.0 },
        lr: Point { x: 400.0, y: 300.0 },
    };
    let options = RedactOptions {
        black_boxes: true,
        ..RedactOptions::default()
    };
    for i in 0..pages {
        let _ = redact_page(&doc, i, &[q], options);
    }
}
