// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::regexp::{ICASE, NEWLINE, NOTBOL, RUNAWAY, Regex};

// A pattern and a text, split at the first NUL; the pattern compiled as
// search compiles it, with and without ICASE, and run over the text from
// each position, with the runaway limit search uses.
fuzz_target!(|data: &[u8]| {
    let split = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let (pattern, text) = (&data[..split], data.get(split + 1..).unwrap_or_default());
    for flags in [NEWLINE, NEWLINE | ICASE] {
        let Ok(re) = Regex::compile(pattern, flags) else {
            continue;
        };
        for at in 0..text.len().min(64) {
            let _ = re.exec(text, at, RUNAWAY | if at > 0 { NOTBOL } else { 0 });
        }
    }
});
