// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::Document;

fuzz_target!(|data: &[u8]| {
    // PDF dates on their own, as /Params and Info dictionaries hold them.
    let _ = nupdf::date::parse_date(data, &nupdf::diag::Diagnostics::new());
    let Ok(doc) = Document::open(data.to_vec()) else {
        return;
    };
    // The outline, page labels (a number tree) and embedded files (a name
    // tree and file specifications), as `outline`, `labels` and
    // `attachments` read them.
    let _ = nupdf::outline::load_outline(&doc);
    for i in 0..doc.page_count().unwrap_or(0).min(16) {
        let _ = nupdf::page_label::page_label(&doc, i);
    }
    for (_, fs) in nupdf::embedded::embedded_files(&doc) {
        let _ = nupdf::embedded::filespec_params(&doc, &fs);
        let _ = nupdf::embedded::is_embedded_file(&doc, &fs);
        let _ = nupdf::embedded::checksum_ok(&doc, &fs);
    }
});
