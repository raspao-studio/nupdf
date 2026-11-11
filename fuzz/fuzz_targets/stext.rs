// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::Document;
use nupdf::content::Store;
use nupdf::stext::{Options, extract_page};

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

// A document's pages gathered into structured text as `nupdf stext` does,
// the first byte choosing the options; the rest is the document. Each page
// is also taken through what `nupdf extract` makes of it: spans, text and
// Markdown.
fn one(data: &[u8]) {
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
            let _ = nupdf::extract::page_markdown(&p);
            for block in &p.blocks {
                if let nupdf::stext::Block::Text(t) = block {
                    let _ = nupdf::extract::block_text(t);
                    for line in &t.lines {
                        let _ = nupdf::extract::spans(line);
                    }
                }
            }
        }
    }
}
