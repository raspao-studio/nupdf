// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]

use libfuzzer_sys::fuzz_target;
use nupdf::stream::Stream;
use nupdf::{Diagnostics, dct, fax, jbig2};

// Byte 0 picks the decoder and its parameters, the rest is the encoded data.
// JBIG2 takes its globals from the front of the data (length in byte 1).
// Each stream is read whole, then again byte by byte.
fn read<'a>(open: impl Fn() -> Option<Stream<'a>>) {
    if let Some(mut s) = open() {
        let _ = s.read_best(0, 0, 1 << 26);
    }
    if let Some(mut s) = open() {
        let _ = s.read_bytewise(1 << 26);
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&head, body)) = data.split_first() else {
        return;
    };
    let diag = Diagnostics::new();
    match head % 3 {
        0 => {
            let ct = i32::from(head >> 2 & 3) - 1;
            read(|| Some(dct::open(Stream::memory(body, &diag), ct)));
        }
        1 => {
            let Some((&cols, body)) = body.split_first() else {
                return;
            };
            let p = fax::FaxParams {
                k: [-1, 0, 1, 4][usize::from(head >> 2 & 3)],
                end_of_line: head & 0x10 != 0,
                encoded_byte_align: head & 0x20 != 0,
                columns: i32::from(cols) * 8 + 1,
                rows: 0,
                end_of_block: head & 0x40 != 0,
                black_is_1: head & 0x80 != 0,
            };
            read(|| fax::open(Stream::memory(body, &diag), p).ok());
        }
        _ => {
            let Some((&n, body)) = body.split_first() else {
                return;
            };
            let (globals, page) = body.split_at(usize::from(n).min(body.len()));
            read(|| {
                let g = jbig2::load_globals(globals, &diag).ok()?;
                Some(jbig2::open(Stream::memory(page, &diag), g))
            });
        }
    }
});
