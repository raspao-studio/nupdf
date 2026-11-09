// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::lex::{Lexer, Token};

fuzz_target!(|data: &[u8]| {
    let diag = nupdf::Diagnostics::new();
    for strings in [true, false] {
        let mut lx = Lexer::new(data, 0, &diag);
        // Every call consumes at least one byte or returns Eof.
        for _ in 0..=data.len() + 1 {
            let t = if strings {
                lx.next_token()
            } else {
                lx.next_token_no_string()
            };
            if t == Token::Eof {
                break;
            }
        }
    }
});
