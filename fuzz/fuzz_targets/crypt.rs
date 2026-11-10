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
    // Streams, through the default filter and a named one, read whole and
    // byte by byte.
    let mut filters = vec![crypt.stream_filter()];
    filters.extend(crypt.parse_filter(b"StdCF", &diag));
    for cf in filters {
        let cipher = crypt.stream_cipher(&cf, 7, 0);
        let raw = nupdf::stream::Stream::memory(rest, &diag);
        if let Ok(mut s) = nupdf::filter::open_cipher(raw, cipher.clone()) {
            let _ = s.read_best(rest.len(), 0, 1 << 26);
        }
        let raw = nupdf::stream::Stream::memory(rest, &diag);
        if let Ok(mut s) = nupdf::filter::open_cipher(raw, cipher) {
            let _ = s.read_bytewise(1 << 26);
        }
    }
    let string = Object::String(rest.into());
    let nested = Object::Array(std::sync::Arc::new(vec![string.clone(), string]));
    let _ = crypt.decrypt_object(nested, 7, 0, &diag);
});
