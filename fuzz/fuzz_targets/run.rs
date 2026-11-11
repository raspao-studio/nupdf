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

// A document whose pages are run as `nupdf trace` runs them: forms,
// patterns, soft masks, images, optional content and structure, with the
// store kept from page to page.
fuzz_target!(|data: &[u8]| {
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
});
