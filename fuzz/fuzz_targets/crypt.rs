// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use libfuzzer_sys::fuzz_target;
use nupdf::crypt::Crypt;
use nupdf::lex::Lexer;
use nupdf::parse::parse_stm_obj;
use nupdf::Object;

// The first line is an /Encrypt dictionary, the second a password, the rest
// data to decrypt as strings and streams.
fuzz_target!(|data: &[u8]| {
    let mut parts = data.splitn(3, |&b| b == b'\n');
    let (Some(dict), Some(password), Some(rest)) = (parts.next(), parts.next(), parts.next())
    else {
        return;
    };
    let diag = nupdf::Diagnostics::new();
    let mut lx = Lexer::new(dict, 0, &diag);
    let Ok(Object::Dict(dict)) = parse_stm_obj(&mut lx) else {
        return;
    };
    let id_bytes: std::sync::Arc<[u8]> = rest.get(..16).unwrap_or(rest).into();
    let id = Object::Array(std::sync::Arc::new(vec![
        Object::String(id_bytes.clone()),
        Object::String(id_bytes),
    ]));
    let Ok(mut crypt) = Crypt::new(&dict, &id, &|o| o.clone(), &diag) else {
        return;
    };
    let _ = crypt.authenticate(&String::from_utf8_lossy(password));
    let _ = crypt.description();
    let _ = crypt.decrypt_stream(rest, 7, 0);
    let _ = crypt.decrypt_with_filter(b"StdCF", rest, 7, 1, &diag);
    let _ = crypt.decrypt_with_filter(b"Identity", rest, 7, 1, &diag);
    let string = Object::String(rest.into());
    let nested = Object::Array(std::sync::Arc::new(vec![string.clone(), string]));
    let _ = crypt.decrypt_object(nested, 7, 0, &diag);
});
