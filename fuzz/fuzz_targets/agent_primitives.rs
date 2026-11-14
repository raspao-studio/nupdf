// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::Document;
use nupdf::attest::{AttestOptions, attest_page};
use nupdf::audit::{ClaimInput, audit_claims};
use nupdf::patch::{PatchOptions, PatchSpec, apply_patches};
use nupdf::plan::parse_plan;
use nupdf::query::{QuerySpec, parse_number, parse_region_spec, run_query};

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

fn one(data: &[u8]) {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = parse_plan(text);
        let _ = parse_number(text);
        let _ = parse_region_spec(text);
    }
    let Ok(doc) = Document::open(data.to_vec()) else {
        return;
    };
    let opts = AttestOptions {
        dpi: 36.0,
        max_pixels: 4_000_000,
        contrast_threshold: 12,
    };
    if doc.page_count().unwrap_or(0) > 0 {
        let _ = attest_page(&doc, 0, &opts, false);
        let q = QuerySpec {
            pages: Some("1".into()),
            right_of: Some("Total".into()),
            left_of: None,
            below: None,
            above: None,
            inside: None,
            near_widget: None,
            pattern: None,
            bold: None,
            italic: None,
            min_size: None,
            max_size: None,
            color: None,
            nearest_only: true,
            max_items: Some(16),
        };
        let _ = run_query(&doc, &q);
        let claims = [ClaimInput {
            claim_id: None,
            doc_id: None,
            id: Some("p1.b1.l1.s1".into()),
            page: Some(1),
            quote: "test".into(),
        }];
        let _ = audit_claims(&doc, "b3:00", &claims, false, 36.0);
        let forked = doc.fork_clean();
        let _ = apply_patches(
            &forked,
            &[PatchSpec {
                target_id: "p1.b1.l1.s1".into(),
                new_text: "patched".into(),
            }],
            &PatchOptions::default(),
        );
    }
}
