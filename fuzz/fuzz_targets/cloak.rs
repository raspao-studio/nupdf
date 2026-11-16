// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
//! `cloak` then `uncloak` on arbitrary PDF bytes.
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::Document;
use nupdf::cloak::{CloakTarget, cloak_document, uncloak_pdf};
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
    let targets = [
        CloakTarget::Preset("pii".into()),
        CloakTarget::Literal {
            category: "PERSON".into(),
            secret: "Alice".into(),
        },
        CloakTarget::Regex {
            category: "ACCT".into(),
            pattern: "[A-Z]{2}[0-9]{2,6}".into(),
        },
    ];
    let Ok((bytes, vault, _)) = cloak_document(&doc, &targets, &WriteOptions::default()) else {
        return;
    };
    if let Ok(cloaked) = Document::open(bytes) {
        let _ = uncloak_pdf(&cloaked, &vault, &WriteOptions::default());
    }
    // The vault applies to the input too: tokens it does not hold are left be.
    let _ = uncloak_pdf(&doc, &vault, &WriteOptions::default());
}
