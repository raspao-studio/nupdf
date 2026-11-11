// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::{Dict, Document, ObjRef, Object};

// Any input as an embedded CMap stream, loaded with its /UseCMap chain;
// then the input itself is split into codes and each is looked up.
fuzz_target!(|data: &[u8]| {
    let Ok(doc) = Document::create() else {
        return;
    };
    let num = doc.add_stream(Dict::new(), data.to_vec());
    let Ok(cmap) = nupdf::cmap::load_embedded(&doc, &Object::Ref(ObjRef::new(num, 0))) else {
        return;
    };
    let mut at = 0;
    while at < data.len() {
        let (code, len) = cmap.decode(&data[at..]);
        let _ = cmap.lookup(code);
        let _ = cmap.lookup_full(code);
        at += len.max(1);
    }
});
