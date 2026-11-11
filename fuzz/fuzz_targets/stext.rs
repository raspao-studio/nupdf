// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::Document;
use nupdf::content::Store;
use nupdf::stext::{Options, extract_page};

// A document's pages gathered into structured text as `nupdf stext` does,
// the first byte choosing the options; the rest is the document.
fuzz_target!(|data: &[u8]| {
    let pick = u32::from(data.first().copied().unwrap_or(0));
    let Ok(doc) = Document::open(data.get(1..).unwrap_or_default().to_vec()) else {
        return;
    };
    // The low bits are MuPDF's first eight option flags; the clip flag is
    // the default's.
    let opts = Options {
        flags: (pick & 0xbf) | (pick & 0x40) ^ 0x40,
        ..Options::default()
    };
    let pages = doc.page_count().unwrap_or(0).min(16);
    let mut store = Store::default();
    for index in 0..pages {
        let Ok(page) = doc.page(index) else {
            continue;
        };
        let (result, next) = extract_page(&doc, &page, opts, store);
        store = next;
        if let Ok(p) = result {
            let _ = p.text();
        }
    }
});
