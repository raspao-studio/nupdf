// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::crypt::NewMethod;
use nupdf::write::{Encrypt, WriteOptions, save, verify};
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
    // Write it, as `clean` does, at a garbage collection level, with or
    // without object streams, with the stream options and the security
    // handler the input's last three bytes pick, and read the output back.
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
        objstms: (last / 5) % 2 == 1,
        encrypt: match pick(3) % 6 {
            0 => Encrypt::Keep,
            1 => Encrypt::Remove,
            n => Encrypt::New {
                method: [
                    NewMethod::Rc4_40,
                    NewMethod::Rc4_128,
                    NewMethod::Aes128,
                    NewMethod::Aes256,
                ][usize::from(n - 2)],
                owner: "o".into(),
                user: "u".into(),
                permissions: -4,
            },
        },
        ..Default::default()
    };
    if doc.xref_len() <= 4096
        && let Ok(out) = save(&doc, &options)
    {
        let _ = verify(&doc, &out, None, &options);
        let again = save(&doc, &options).ok();
        assert_eq!(again.as_deref(), Some(out.as_slice()), "writing is deterministic");
    }
    // Keep some pages, as `clean --pages` does, on a fresh copy, then make
    // the edits `meta` and `pages --rotate` make, and write that: the
    // fourth byte from the end picks the pages, the fifth the edits.
    if doc.xref_len() <= 4096
        && let Ok(edited) = Document::open(data.to_vec())
        && let Ok(count) = edited.page_count()
    {
        nupdf::write::check_document(&edited);
        let spec = ["N-1", "1", "2-N", "1,1", "N"][usize::from(pick(4) % 5)];
        let pages = nupdf::rearrange::parse_page_ranges(spec, count, &edited);
        if nupdf::rearrange::rearrange_pages(&edited, &pages).is_ok() {
            let edits = pick(5);
            if edits & 1 != 0 {
                nupdf::edit::strip_metadata(&edited);
            }
            if edits & 2 != 0 {
                nupdf::edit::set_metadata(&edited, "info:Title", "\u{e9}t\u{e9}");
            }
            let rotated = edits & 4 == 0 || {
                let pages: Vec<i32> = (0..edited.page_count().unwrap_or(0)).collect();
                nupdf::edit::rotate_pages(&edited, &pages, 90 * i32::from(edits >> 3)).is_ok()
            };
            // The top bit appends the edits instead, which MuPDF refuses
            // with most options; the refusal is a result too.
            let options = WriteOptions {
                incremental: edits & 0x80 != 0,
                ..options
            };
            if rotated && let Ok(out) = save(&edited, &options) {
                let _ = verify(&edited, &out, None, &options);
            }
        }
    }
    // Merge its first pages into a new document, as `merge` does, links
    // and outline included, and write that with the defaults.
    if doc.xref_len() <= 4096
        && let Ok(dst) = Document::create()
    {
        let _ = nupdf::merge::Merger::new(&dst).merge_range(&doc, "1-8");
        let defaults = WriteOptions::default();
        if let Ok(out) = save(&dst, &defaults) {
            let _ = verify(&dst, &out, None, &defaults);
        }
    }
});
