// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::lex::Lexer;
use nupdf::parse::{parse_indirect, parse_stm_obj};

fuzz_target!(|data: &[u8]| {
    let diag = nupdf::Diagnostics::new();
    let mut lx = Lexer::new(data, 0, &diag);
    let _ = parse_indirect(&mut lx);
    let mut lx = Lexer::new(data, 0, &diag);
    for _ in 0..64 {
        if parse_stm_obj(&mut lx).is_err() || lx.at_eof() {
            break;
        }
    }
});
