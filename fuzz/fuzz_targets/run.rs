// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::Document;
use nupdf::content::Store;
use nupdf::device::Device;
use nupdf::run::run_page_contents;

/// Takes every call and ignores it.
struct Null;

impl Device for Null {}

/// The stack `nupdf` runs on: content nests as deep as MuPDF lets it, and
/// sanitizer frames are large.
const STACK_SIZE: usize = 1 << 30;

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    let worker = std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || one(&data));
    // A panic on the worker is the fuzzer's to see.
    if let Ok(handle) = worker
        && let Err(panic) = handle.join()
    {
        std::panic::resume_unwind(panic);
    }
});

// A document whose pages are run as `nupdf trace` runs them: forms,
// patterns, soft masks, images, optional content and structure, with the
// store kept from page to page.
fn one(data: &[u8]) {
    let Ok(doc) = Document::open(data.to_vec()) else {
        return;
    };
    let pages = doc.page_count().unwrap_or(0).min(16);
    let mut store = Store::default();
    for index in 0..pages {
        let Ok(page) = doc.page(index) else {
            continue;
        };
        store = run_page_contents(&doc, &page, &mut Null, store).store;
    }
}
