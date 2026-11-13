// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::Document;

fuzz_target!(|data: &[u8]| {
    // DER and BER on their own, as signatures hold them, and the RSA
    // public-key operation on what they give.
    let _ = nupdf::signature::signatory_of(data);
    // Default appearance strings and language tags, as fields carry them.
    let _ = nupdf::appearance::parse_default_appearance(data, 100);
    let _ = nupdf::appearance::language_from_string(data);
    if data.len() > 4 {
        let (key, sig) = data.split_at(data.len() / 2);
        let _ = nupdf::rsa::recover(key, &[1, 0, 1], sig);
    }
    let Ok(doc) = Document::open(data.to_vec()) else {
        return;
    };
    // Annotations, form fields and signatures, as `annots`, `forms` and
    // `signatures` read them, with the appearances MuPDF would make.
    for i in 0..doc.page_count().unwrap_or(0).min(16) {
        let Ok(page) = doc.page(i) else { continue };
        let ctm = doc
            .page_transform(&page)
            .map_or(nupdf::geometry::Matrix::IDENTITY, |t| t.1);
        let (annots, widgets) = nupdf::annot::page_annots(&doc, &page);
        for a in annots.iter().take(64) {
            let _ = nupdf::annot::rect(&doc, a, ctm);
            let _ = nupdf::annot::ink_list(&doc, a, ctm);
            let _ = nupdf::appearance::synthesize(&doc, a, ctm);
        }
        for w in widgets.iter().take(64) {
            let _ = nupdf::form::name(&doc, w);
            let _ = nupdf::form::value(&doc, w);
            let _ = nupdf::form::options(&doc, w);
            let _ = nupdf::signature::check_digest(&doc, w);
            let _ = nupdf::signature::signatory(&doc, w);
            let _ = nupdf::appearance::synthesize(&doc, w, ctm);
            let _ = nupdf::appearance::synthesize_button(&doc, w, ctm);
        }
    }
    let _ = nupdf::signature::summary(&doc);
});
