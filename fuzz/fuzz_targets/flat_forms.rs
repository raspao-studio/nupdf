// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
//! Flat-form detection, then a fill and a promotion of what was detected,
//! on arbitrary PDF bytes.
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::Document;
use nupdf::flat_form::{FlatFillOp, detect_flat_forms, fill_flat_forms, promote_flat_forms};
use nupdf::write::WriteOptions;

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
    let Ok(doc) = Document::open(data.to_vec()) else {
        return;
    };
    let Ok(report) = detect_flat_forms(&doc, None) else {
        return;
    };
    let ops: Vec<FlatFillOp> = report
        .fields
        .iter()
        .take(8)
        .map(|f| {
            if f.kind == "checkbox" {
                FlatFillOp::SetCheck {
                    target: f.id.clone(),
                    checked: f.checked != Some(true),
                }
            } else {
                FlatFillOp::SetText {
                    target: f.id.clone(),
                    value: "AB12".into(),
                }
            }
        })
        .collect();
    let _ = fill_flat_forms(&doc, &ops, &WriteOptions::default());
    let _ = promote_flat_forms(&doc, &WriteOptions::default());
}
