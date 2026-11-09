// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::filter;

fuzz_target!(|data: &[u8]| {
    let diag = nupdf::Diagnostics::new();
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    match selector % 5 {
        0 => {
            let _ = filter::inflate(rest, &diag, 1 << 26);
        }
        1 => {
            let Some((params, body)) = rest.split_at_checked(4.min(rest.len())) else {
                return;
            };
            let p = |i: usize| i32::from(params.get(i).copied().unwrap_or(0));
            let predictor = [1, 2, 10, 11, 12, 13, 14, 15, 7][(p(0) % 9) as usize];
            let _ = filter::predict(
                body,
                predictor,
                p(1) * 4 - 3,
                p(2) % 40 - 2,
                [1, 2, 4, 8, 16, 3][(p(3) % 6) as usize],
                &diag,
            );
        }
        2 => {
            let _ = filter::ascii_hex(rest);
        }
        3 => {
            let _ = filter::ascii85(rest, &diag);
        }
        _ => {
            let _ = filter::run_length(rest, 1 << 26);
        }
    }
});
