// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::write::{WriteOptions, save, verify};
use nupdf::{Document, Object};

fuzz_target!(|data: &[u8]| {
    let Ok(doc) = Document::open(data.to_vec()) else {
        return;
    };
    let _ = doc.version();
    let _ = doc.page_count();
    let n = doc.xref_len().min(4096);
    for num in 1..n {
        if let Some(Object::Stream(s)) = doc.object(num as i32) {
            let _ = doc.raw_stream_data(&s).map(|raw| raw.len());
            let _ = doc.stream_data(&s);
        }
    }
    // Write it, as `clean` does, at a garbage collection level and with
    // the stream options the input's last two bytes pick, and read the
    // output back.
    let pick = |i: usize| data.len().checked_sub(i).and_then(|i| data.get(i)).copied().unwrap_or(0);
    let (last, flags) = (pick(1), pick(2));
    let options = WriteOptions {
        garbage: last % 5,
        decompress: flags & 1 != 0,
        compress: flags & 2 != 0,
        compress_images: flags & 4 != 0,
        compress_fonts: flags & 8 != 0,
        ascii: flags & 16 != 0,
        effort: i32::from(flags >> 5) * 14,
        ..Default::default()
    };
    if doc.xref_len() <= 4096
        && let Ok(out) = save(&doc, &options)
    {
        let _ = verify(&doc, &out, None, &options);
        let again = save(&doc, &options).ok();
        assert_eq!(again.as_deref(), Some(out.as_slice()), "writing is deterministic");
    }
});
