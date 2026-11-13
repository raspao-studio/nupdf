// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-parse.c,
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! Object parsing on top of the lexer, with MuPDF's recovery rules.

use std::fmt;
use std::sync::Arc;

use crate::lex::{Lexer, RawToken, Token};
use crate::object::{Dict, MAX_OBJECT_NUMBER, ObjRef, Object};

/// Nesting limit for arrays and dictionaries. MuPDF recurses without one;
/// nupdf needs one because a stack overflow aborts the process.
pub const MAX_DEPTH: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError(pub String);

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, SyntaxError> {
    Err(SyntaxError(msg.into()))
}

pub fn parse_array(lx: &mut Lexer, depth: usize) -> Result<Vec<Object>, SyntaxError> {
    if depth >= MAX_DEPTH {
        return err("nesting too deep");
    }
    let mut out = Vec::new();
    // Names and strings are lexed into one buffer, kept from token to token.
    let mut buf = Vec::new();
    // Up to two pending integers that may turn out to be "a b R".
    let (mut a, mut b, mut n) = (0i64, 0i64, 0u8);
    loop {
        let tok = lx.next_raw(&mut buf);
        if !matches!(tok, RawToken::Int(_) | RawToken::R) {
            if n > 0 {
                out.push(Object::Int(a));
            }
            if n > 1 {
                out.push(Object::Int(b));
            }
            n = 0;
        }
        if matches!(tok, RawToken::Int(_)) && n == 2 {
            out.push(Object::Int(a));
            a = b;
            n -= 1;
        }
        match tok {
            RawToken::Eof => return err("array not closed before end of file"),
            RawToken::CloseArray => return Ok(out),
            RawToken::Int(i) => {
                if n == 0 {
                    a = i;
                }
                if n == 1 {
                    b = i;
                }
                n += 1;
            }
            RawToken::R => {
                if n != 2 {
                    return err("cannot parse indirect reference in array");
                }
                out.push(Object::Ref(ObjRef::from_i64(a, b)));
                n = 0;
            }
            RawToken::OpenArray => out.push(Object::Array(Arc::new(parse_array(lx, depth + 1)?))),
            RawToken::OpenDict => out.push(Object::Dict(Arc::new(parse_dict(lx, depth + 1)?))),
            RawToken::Name => out.push(Object::Name(Arc::from(&buf[..]))),
            RawToken::Real(f) => out.push(Object::Real(f)),
            RawToken::String => out.push(Object::String(Arc::from(&buf[..]))),
            RawToken::True => out.push(Object::Bool(true)),
            RawToken::False => out.push(Object::Bool(false)),
            _ => out.push(Object::Null),
        }
    }
}

pub fn parse_dict(lx: &mut Lexer, depth: usize) -> Result<Dict, SyntaxError> {
    if depth >= MAX_DEPTH {
        return err("nesting too deep");
    }
    let mut dict = Dict::new();
    // Names and strings are lexed into one buffer, kept from token to token.
    let mut buf = Vec::new();
    let is_id = |tok: RawToken, buf: &[u8]| tok == RawToken::Keyword && buf == b"ID";
    let mut tok = lx.next_raw(&mut buf);
    loop {
        if tok == RawToken::CloseDict {
            return Ok(dict);
        }
        // Inline images: "BI <dict> ID" ends the dictionary.
        if is_id(tok, &buf) {
            return Ok(dict);
        }
        if tok != RawToken::Name {
            return err("invalid key in dict");
        }
        let key: Arc<[u8]> = Arc::from(&buf[..]);

        let value = match lx.next_raw(&mut buf) {
            RawToken::OpenArray => Object::Array(Arc::new(parse_array(lx, depth + 1)?)),
            RawToken::OpenDict => Object::Dict(Arc::new(parse_dict(lx, depth + 1)?)),
            RawToken::Name => Object::Name(Arc::from(&buf[..])),
            RawToken::Real(f) => Object::Real(f),
            RawToken::String => Object::String(Arc::from(&buf[..])),
            RawToken::True => Object::Bool(true),
            RawToken::False => Object::Bool(false),
            RawToken::Null => Object::Null,
            RawToken::Int(a) => {
                let next = lx.next_raw(&mut buf);
                if matches!(next, RawToken::CloseDict | RawToken::Name) || is_id(next, &buf) {
                    dict.insert(key, Object::Int(a));
                    tok = next;
                    continue;
                }
                let mut value = None;
                if let RawToken::Int(b) = next
                    && lx.next_raw(&mut buf) == RawToken::R
                {
                    value = Some(Object::Ref(ObjRef::from_i64(a, b)));
                }
                // The token that was neither R nor a dict terminator is lost,
                // exactly as in MuPDF.
                value.unwrap_or_else(|| {
                    lx_warn(
                        lx,
                        "parse.bad_ref_in_dict",
                        "invalid indirect reference in dict",
                    );
                    Object::Null
                })
            }
            _ => Object::Null,
        };
        dict.insert(key, value);
        tok = lx.next_raw(&mut buf);
    }
}

fn lx_warn(lx: &Lexer, code: &'static str, msg: &str) {
    lx.diagnostics().warn(code, msg);
}

/// One object inside an object stream (`pdf_parse_stm_obj`).
pub fn parse_stm_obj(lx: &mut Lexer) -> Result<Object, SyntaxError> {
    Ok(match lx.next_token() {
        Token::OpenArray => Object::Array(Arc::new(parse_array(lx, 1)?)),
        Token::OpenDict => Object::Dict(Arc::new(parse_dict(lx, 1)?)),
        Token::Name(n) => Object::Name(Arc::from(n)),
        Token::Real(f) => Object::Real(f),
        Token::String(s) => Object::String(Arc::from(s)),
        Token::True => Object::Bool(true),
        Token::False => Object::Bool(false),
        Token::Null => Object::Null,
        Token::Int(i) => Object::Int(i),
        _ => return err("unknown token in object stream"),
    })
}

/// An indirect object as read from the file.
#[derive(Debug, Clone, PartialEq)]
pub struct IndirectObject {
    pub num: i32,
    pub generation: i32,
    /// For streams this is the stream dictionary.
    pub obj: Object,
    /// Absolute offset of the stream data, when the object is a stream.
    pub stream_start: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndirectError {
    pub message: String,
    /// The `num generation obj` header itself was broken, so the xref is probably
    /// wrong and a repair is worth trying (MuPDF's `try_repair`).
    pub try_repair: bool,
}

/// `pdf_parse_ind_obj`: `num generation obj <object> [stream|endobj]`.
pub fn parse_indirect(lx: &mut Lexer) -> Result<IndirectObject, IndirectError> {
    let fail = |message: String, try_repair: bool| IndirectError {
        message,
        try_repair,
    };

    let Token::Int(num) = lx.next_token() else {
        return Err(fail("expected object number".into(), true));
    };
    let num = num as i32;
    if !(0..=MAX_OBJECT_NUMBER).contains(&num) {
        return Err(fail("object number out of range".into(), false));
    }
    let Token::Int(generation) = lx.next_token() else {
        return Err(fail(
            format!("expected generation number ({num} ? obj)"),
            true,
        ));
    };
    let generation = generation as i32;
    if !(0..65536).contains(&generation) {
        return Err(fail(
            format!("invalid generation number ({generation})"),
            true,
        ));
    }
    if lx.next_token() != Token::Obj {
        return Err(fail(
            format!("expected 'obj' keyword ({num} {generation} ?)"),
            true,
        ));
    }

    let body_err = |e: SyntaxError| fail(e.0, false);
    let mut pending: Option<Token> = None;
    let obj = match lx.next_token() {
        Token::OpenArray => Object::Array(Arc::new(parse_array(lx, 1).map_err(body_err)?)),
        Token::OpenDict => Object::Dict(Arc::new(parse_dict(lx, 1).map_err(body_err)?)),
        Token::Name(n) => Object::Name(Arc::from(n)),
        Token::Real(f) => Object::Real(f),
        Token::String(s) => Object::String(Arc::from(s)),
        Token::True => Object::Bool(true),
        Token::False => Object::Bool(false),
        Token::Null => Object::Null,
        Token::Int(a) => {
            let next = lx.next_token();
            match next {
                Token::Stream | Token::EndObj => {
                    pending = Some(next);
                    Object::Int(a)
                }
                Token::Int(b) if lx.next_token() == Token::R => Object::Ref(ObjRef::from_i64(a, b)),
                _ => {
                    return Err(fail(
                        format!("expected 'R' keyword ({num} {generation} R)"),
                        false,
                    ));
                }
            }
        }
        Token::EndObj => {
            pending = Some(Token::EndObj);
            Object::Null
        }
        _ => {
            return Err(fail(
                format!("syntax error in object ({num} {generation} R)"),
                false,
            ));
        }
    };

    let tok = match pending {
        Some(t) => t,
        None => lx.next_token(),
    };
    let stream_start = match tok {
        Token::Stream => {
            let mut c = lx.read_byte();
            while c == Some(b' ') {
                c = lx.read_byte();
            }
            if c == Some(b'\r') {
                if lx.peek_byte() == Some(b'\n') {
                    lx.read_byte();
                } else {
                    lx_warn(
                        lx,
                        "parse.stream_missing_lf",
                        &format!(
                            "line feed missing after stream begin marker ({num} {generation} R)"
                        ),
                    );
                }
            }
            Some(lx.pos())
        }
        Token::EndObj => None,
        _ => {
            lx_warn(
                lx,
                "parse.missing_endobj",
                &format!("expected 'endobj' or 'stream' keyword ({num} {generation} R)"),
            );
            None
        }
    };

    Ok(IndirectObject {
        num,
        generation,
        obj,
        stream_start,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::Diagnostics;

    fn indirect(input: &[u8]) -> Result<IndirectObject, IndirectError> {
        let diag = Diagnostics::new();
        let mut lx = Lexer::new(input, 0, &diag);
        parse_indirect(&mut lx)
    }

    fn obj(input: &[u8]) -> Object {
        indirect(input).unwrap().obj
    }

    fn int(i: i64) -> Object {
        Object::Int(i)
    }

    fn r(num: i32, generation: i32) -> Object {
        Object::Ref(ObjRef::new(num, generation))
    }

    #[test]
    fn arrays_group_references() {
        let o = obj(b"1 0 obj [1 2 0 R 3 4 5 R 6] endobj");
        assert_eq!(
            o.as_array().unwrap(),
            &[int(1), r(2, 0), int(3), r(4, 5), int(6)]
        );
    }

    #[test]
    fn arrays_turn_unknown_tokens_into_null() {
        let o = obj(b"1 0 obj [/A foo (s) true] endobj");
        let items = o.as_array().unwrap();
        assert_eq!(items[1], Object::Null);
        assert_eq!(items[3], Object::Bool(true));
    }

    #[test]
    fn stray_r_in_array_is_an_error() {
        let e = indirect(b"1 0 obj [1 R] endobj").unwrap_err();
        assert!(!e.try_repair);
        assert_eq!(e.message, "cannot parse indirect reference in array");
    }

    #[test]
    fn dict_values_and_references() {
        let o = obj(b"1 0 obj << /A 1 /B 2 0 R /C /N /D [1] /E << /F 3 >> >> endobj");
        let d = o.as_dict().unwrap();
        assert_eq!(d.get(b"A"), Some(&int(1)));
        assert_eq!(d.get(b"B"), Some(&r(2, 0)));
        assert_eq!(d.get(b"C"), Some(&Object::name(b"N")));
        assert!(d.get(b"E").unwrap().as_dict().is_some());
    }

    #[test]
    fn dict_int_before_terminator() {
        let o = obj(b"1 0 obj <</Count 3>> endobj");
        assert_eq!(o.as_dict().unwrap().get(b"Count"), Some(&int(3)));
    }

    #[test]
    fn dict_broken_reference_loses_a_token_like_mupdf() {
        // "/A 1 2 /B" is not a reference; MuPDF sets /A to null and drops /B,
        // so "3" is then read as a key and the dict fails.
        let e = indirect(b"1 0 obj << /A 1 2 /B 3 >> endobj").unwrap_err();
        assert_eq!(e.message, "invalid key in dict");
        // When the dropped token is harmless the dict survives.
        let o = obj(b"1 0 obj << /A 1 2 (x) >> endobj");
        assert_eq!(o.as_dict().unwrap().get(b"A"), Some(&Object::Null));
    }

    #[test]
    fn dict_duplicate_keys_keep_last_value() {
        let o = obj(b"1 0 obj << /A 1 /A 2 >> endobj");
        assert_eq!(o.as_dict().unwrap().get(b"A"), Some(&int(2)));
    }

    #[test]
    fn indirect_header_errors_ask_for_repair() {
        assert!(indirect(b"x 0 obj 1 endobj").unwrap_err().try_repair);
        assert!(indirect(b"1 x obj 1 endobj").unwrap_err().try_repair);
        assert!(indirect(b"1 70000 obj 1 endobj").unwrap_err().try_repair);
        assert!(indirect(b"1 0 bj 1 endobj").unwrap_err().try_repair);
        assert!(!indirect(b"9999999 0 obj 1 endobj").unwrap_err().try_repair);
    }

    #[test]
    fn indirect_values() {
        assert_eq!(obj(b"1 0 obj 42 endobj"), int(42));
        assert_eq!(obj(b"1 0 obj 4 0 R endobj"), r(4, 0));
        assert_eq!(obj(b"1 0 obj endobj"), Object::Null);
        assert!(indirect(b"1 0 obj 4 0 endobj").is_err());
    }

    #[test]
    fn streams_record_where_data_starts() {
        for (input, start) in [
            (b"1 0 obj <<>> stream\nDATA".as_slice(), 20),
            (b"1 0 obj <<>> stream\r\nDATA".as_slice(), 21),
            (b"1 0 obj <<>> stream  \nDATA".as_slice(), 22),
        ] {
            let ind = indirect(input).unwrap();
            assert_eq!(ind.stream_start, Some(start));
        }
        let diag = Diagnostics::new();
        let mut lx = Lexer::new(b"1 0 obj <<>> stream\rDATA", 0, &diag);
        assert_eq!(parse_indirect(&mut lx).unwrap().stream_start, Some(20));
        assert!(diag.has("parse.stream_missing_lf"));
    }

    #[test]
    fn missing_endobj_is_a_warning() {
        let diag = Diagnostics::new();
        let mut lx = Lexer::new(b"1 0 obj 5 6 0 obj", 0, &diag);
        // "5 6" then "0": not a reference, so this one fails...
        assert!(parse_indirect(&mut lx).is_err());
        let diag = Diagnostics::new();
        let mut lx = Lexer::new(b"1 0 obj (x) 2 0 obj", 0, &diag);
        assert_eq!(
            parse_indirect(&mut lx).unwrap().obj,
            Object::String(Arc::from(&b"x"[..]))
        );
        assert!(diag.has("parse.missing_endobj"));
    }

    #[test]
    fn deep_nesting_is_rejected_not_a_crash() {
        let mut input = b"1 0 obj ".to_vec();
        input.extend(std::iter::repeat_n(b'[', MAX_DEPTH + 5));
        let e = indirect(&input).unwrap_err();
        assert_eq!(e.message, "nesting too deep");
    }

    #[test]
    fn object_stream_objects() {
        let diag = Diagnostics::new();
        let mut lx = Lexer::new(b"<</A 1>> 7 /N", 0, &diag);
        assert!(parse_stm_obj(&mut lx).unwrap().as_dict().is_some());
        assert_eq!(parse_stm_obj(&mut lx).unwrap(), int(7));
        assert_eq!(parse_stm_obj(&mut lx).unwrap(), Object::name(b"N"));
        assert!(parse_stm_obj(&mut lx).is_err());
    }
}
