// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-lex.c,
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! The PDF tokenizer, with MuPDF's leniencies.
//!
//! The lexer works on a byte slice and a cursor. "Unread" is a step back,
//! and end of input plays the role of `EOF` in MuPDF's stream reader.

use crate::diag::Diagnostics;

/// Names longer than this are truncated (MuPDF's PDF_MAX_NAME_LENGTH).
pub const MAX_NAME_LENGTH: usize = 4095;

/// MuPDF's large lex buffer; numbers longer than this minus one are split.
pub const LEXBUF_LARGE: usize = 65536;
/// MuPDF's small lex buffer, used when repairing object streams.
pub const LEXBUF_SMALL: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Error,
    Eof,
    OpenArray,
    CloseArray,
    OpenDict,
    CloseDict,
    OpenBrace,
    CloseBrace,
    Name(Vec<u8>),
    Int(i64),
    Real(f32),
    String(Vec<u8>),
    Keyword(Vec<u8>),
    R,
    True,
    False,
    Null,
    Obj,
    EndObj,
    Stream,
    EndStream,
    Xref,
    Trailer,
    StartXref,
    NewObj,
}

impl Token {
    pub fn is_keyword(&self, word: &[u8]) -> bool {
        matches!(self, Token::Keyword(k) if k == word)
    }
}

/// A token whose bytes, for a name, string or keyword, are in the buffer
/// given to [`Lexer::next_raw`]: content streams lex many tokens, and a
/// buffer kept across them saves an allocation each.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RawToken {
    Error,
    Eof,
    OpenArray,
    CloseArray,
    OpenDict,
    CloseDict,
    OpenBrace,
    CloseBrace,
    Name,
    Int(i64),
    Real(f32),
    String,
    Keyword,
    R,
    True,
    False,
    Null,
    Obj,
    EndObj,
    Stream,
    EndStream,
    Xref,
    Trailer,
    StartXref,
    NewObj,
}

impl RawToken {
    /// The token, with its bytes from `buf`.
    fn into_token(self, buf: Vec<u8>) -> Token {
        match self {
            RawToken::Error => Token::Error,
            RawToken::Eof => Token::Eof,
            RawToken::OpenArray => Token::OpenArray,
            RawToken::CloseArray => Token::CloseArray,
            RawToken::OpenDict => Token::OpenDict,
            RawToken::CloseDict => Token::CloseDict,
            RawToken::OpenBrace => Token::OpenBrace,
            RawToken::CloseBrace => Token::CloseBrace,
            RawToken::Name => Token::Name(buf),
            RawToken::Int(i) => Token::Int(i),
            RawToken::Real(f) => Token::Real(f),
            RawToken::String => Token::String(buf),
            RawToken::Keyword => Token::Keyword(buf),
            RawToken::R => Token::R,
            RawToken::True => Token::True,
            RawToken::False => Token::False,
            RawToken::Null => Token::Null,
            RawToken::Obj => Token::Obj,
            RawToken::EndObj => Token::EndObj,
            RawToken::Stream => Token::Stream,
            RawToken::EndStream => Token::EndStream,
            RawToken::Xref => Token::Xref,
            RawToken::Trailer => Token::Trailer,
            RawToken::StartXref => Token::StartXref,
            RawToken::NewObj => Token::NewObj,
        }
    }
}

/// PDF whitespace, including NUL.
pub fn is_white(b: u8) -> bool {
    matches!(b, 0x00 | 0x09 | 0x0a | 0x0c | 0x0d | 0x20)
}

fn is_delim(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn unhex(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'A'..=b'F' => b - b'A' + 10,
        b'a'..=b'f' => b - b'a' + 10,
        _ => 0,
    }
}

#[derive(Debug)]
pub struct Lexer<'a> {
    data: &'a [u8],
    pos: usize,
    /// Characters a number may hold before MuPDF's buffer is full.
    number_limit: usize,
    diag: &'a Diagnostics,
}

impl<'a> Lexer<'a> {
    pub fn new(data: &'a [u8], pos: usize, diag: &'a Diagnostics) -> Self {
        Lexer {
            data,
            pos: pos.min(data.len()),
            number_limit: LEXBUF_LARGE - 1,
            diag,
        }
    }

    /// Lexer whose number buffer matches MuPDF's small lex buffer.
    pub fn small(data: &'a [u8], pos: usize, diag: &'a Diagnostics) -> Self {
        Lexer {
            number_limit: LEXBUF_SMALL - 1,
            ..Lexer::new(data, pos, diag)
        }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn seek(&mut self, pos: usize) {
        self.pos = pos.min(self.data.len());
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    pub fn diagnostics(&self) -> &'a Diagnostics {
        self.diag
    }

    pub fn at_eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub fn read_byte(&mut self) -> Option<u8> {
        let b = self.data.get(self.pos).copied();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    pub fn peek_byte(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn unread(&mut self) {
        self.pos = self.pos.saturating_sub(1);
    }

    pub fn next_token(&mut self) -> Token {
        let mut buf = Vec::new();
        self.lex(true, &mut buf).into_token(buf)
    }

    /// [`Lexer::next_token`] with a name's, string's or keyword's bytes
    /// written into `buf`, which is cleared first.
    pub fn next_raw(&mut self, buf: &mut Vec<u8>) -> RawToken {
        buf.clear();
        self.lex(true, buf)
    }

    /// Like `next_token`, but strings are errors (MuPDF's `pdf_lex_no_string`,
    /// used by the repair scanner so that stray parentheses cannot swallow
    /// the rest of the file).
    pub fn next_token_no_string(&mut self) -> Token {
        let mut buf = Vec::new();
        self.lex(false, &mut buf).into_token(buf)
    }

    /// Lexes a token into `out`, which starts empty.
    fn lex(&mut self, strings: bool, out: &mut Vec<u8>) -> RawToken {
        loop {
            let Some(c) = self.read_byte() else {
                return RawToken::Eof;
            };
            match c {
                c if is_white(c) => self.skip_white(),
                b'%' => self.skip_comment(),
                b'/' => {
                    self.lex_name(out);
                    return RawToken::Name;
                }
                b'(' if strings => return self.lex_string(out),
                b'(' | b')' => return RawToken::Error,
                b'<' => {
                    match self.read_byte() {
                        Some(b'<') => return RawToken::OpenDict,
                        Some(_) => self.unread(),
                        None => {}
                    }
                    return if strings {
                        self.lex_hex_string(out)
                    } else {
                        RawToken::Error
                    };
                }
                b'>' => {
                    match self.read_byte() {
                        Some(b'>') => return RawToken::CloseDict,
                        Some(_) => self.unread(),
                        None => {}
                    }
                    return RawToken::Error;
                }
                b'[' => return RawToken::OpenArray,
                b']' => return RawToken::CloseArray,
                b'{' => return RawToken::OpenBrace,
                b'}' => return RawToken::CloseBrace,
                b'+' | b'-' | b'.' | b'0'..=b'9' => return self.lex_number(c, out),
                _ => {
                    self.unread();
                    self.lex_name(out);
                    return keyword(out);
                }
            }
        }
    }

    fn skip_white(&mut self) {
        while let Some(c) = self.read_byte() {
            if !is_white(c) {
                self.unread();
                return;
            }
        }
    }

    fn skip_comment(&mut self) {
        while let Some(c) = self.read_byte() {
            if c == b'\n' || c == b'\r' {
                return;
            }
        }
    }

    /// Lexes a number in `buf`, which starts empty.
    fn lex_number(&mut self, first: u8, buf: &mut Vec<u8>) -> RawToken {
        // MuPDF builds the number in a scratch buffer, writing NUL where a
        // stray '-' appears; parsing then stops at that NUL. Mirror the
        // buffer exactly so odd inputs give the same values.
        let mut real_at: Option<usize> = (first == b'.').then_some(0);
        let neg = first == b'-';
        let mut bad = false;
        buf.push(first);

        let mut c = self.read_byte();
        if neg {
            while c == Some(b'-') {
                c = self.read_byte();
            }
        }
        while buf.len() < self.number_limit {
            match c {
                None => break,
                Some(b) if is_white(b) || is_delim(b) => {
                    self.unread();
                    break;
                }
                Some(b'.') => {
                    if real_at.is_some() {
                        bad = true;
                    }
                    real_at = Some(buf.len());
                    buf.push(b'.');
                }
                // Google Docs writes numbers like 0.000000000000-5684342:
                // stop the value at '-' but consume the trailing digits.
                Some(b'-') => buf.push(0),
                Some(b @ b'0'..=b'9') => buf.push(b),
                Some(b) => {
                    bad = true;
                    buf.push(b);
                }
            }
            c = self.read_byte();
        }
        // When the buffer fills, MuPDF drops the byte it just read.

        if bad {
            return RawToken::Keyword;
        }
        let text = match buf.iter().position(|&b| b == 0) {
            Some(nul) => &buf[..nul],
            None => &buf[..],
        };
        match real_at {
            Some(dot) => {
                // MuPDF uses an Acrobat-compatible parser when the integer
                // part is long; otherwise its strtof.
                let value = if dot >= 10 {
                    acrobat_atof(text)
                } else {
                    fz_atof(text)
                };
                RawToken::Real(value)
            }
            None => RawToken::Int(fast_atoi(text)),
        }
    }

    fn lex_name(&mut self, out: &mut Vec<u8>) {
        let mut truncated = false;
        loop {
            if out.len() == MAX_NAME_LENGTH && !truncated {
                self.diag.warn("lex.name_too_long", "name is too long");
                truncated = true;
            }
            let push = |out: &mut Vec<u8>, b: u8| {
                if out.len() < MAX_NAME_LENGTH {
                    out.push(b);
                }
            };
            let Some(c) = self.read_byte() else {
                return;
            };
            if is_white(c) || is_delim(c) {
                self.unread();
                return;
            }
            if c != b'#' {
                push(out, c);
                continue;
            }
            // '#xx' escape. "#00" and malformed escapes keep the '#'.
            let mut hex = [0u8; 2];
            let mut ok = true;
            for i in 0..2 {
                match self.peek_byte() {
                    Some(d @ b'0'..=b'9') => {
                        if i == 1 && d == b'0' && hex[0] == 0 {
                            if i == 1 {
                                self.unread();
                            }
                            ok = false;
                            break;
                        }
                        hex[i] = d - b'0';
                        self.pos += 1;
                    }
                    Some(d @ b'a'..=b'f') => {
                        hex[i] = d - b'a' + 10;
                        self.pos += 1;
                    }
                    Some(d @ b'A'..=b'F') => {
                        hex[i] = d - b'A' + 10;
                        self.pos += 1;
                    }
                    Some(_) => {
                        if i == 1 {
                            self.unread();
                        }
                        ok = false;
                        break;
                    }
                    // At end of input MuPDF does not give back a digit it
                    // already consumed, so "#4" at EOF loses the '4'.
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                push(out, (hex[0] << 4) + hex[1]);
            } else {
                push(out, b'#');
            }
        }
    }

    fn lex_string(&mut self, out: &mut Vec<u8>) -> RawToken {
        let mut balance = 1;
        loop {
            let Some(c) = self.read_byte() else {
                return RawToken::Error;
            };
            match c {
                b'(' => {
                    balance += 1;
                    out.push(c);
                }
                b')' => {
                    balance -= 1;
                    if balance == 0 {
                        return RawToken::String;
                    }
                    out.push(c);
                }
                b'\\' => {
                    let Some(e) = self.read_byte() else {
                        return RawToken::Error;
                    };
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'(' | b')' | b'\\' => out.push(e),
                        b'0'..=b'7' => {
                            let mut oct = u32::from(e - b'0');
                            for _ in 0..2 {
                                match self.read_byte() {
                                    Some(d @ b'0'..=b'7') => oct = oct * 8 + u32::from(d - b'0'),
                                    Some(_) => {
                                        self.unread();
                                        break;
                                    }
                                    None => break,
                                }
                            }
                            out.push(oct as u8);
                        }
                        b'\n' => {}
                        b'\r' => {
                            if let Some(n) = self.read_byte()
                                && n != b'\n'
                            {
                                self.unread();
                            }
                        }
                        other => out.push(other),
                    }
                }
                // Unescaped CR, LF and CRLF all read as a single LF.
                b'\n' => out.push(b'\n'),
                b'\r' => {
                    out.push(b'\n');
                    if let Some(n) = self.read_byte()
                        && n != b'\n'
                    {
                        self.unread();
                    }
                }
                other => out.push(other),
            }
        }
    }

    fn lex_hex_string(&mut self, out: &mut Vec<u8>) -> RawToken {
        let mut high: Option<u8> = None;
        loop {
            let Some(c) = self.read_byte() else {
                return RawToken::Error;
            };
            match c {
                c if is_white(c) => {}
                b'>' => {
                    // Odd digit count: pad with '0'.
                    if let Some(h) = high {
                        out.push(h * 16);
                    }
                    return RawToken::String;
                }
                c => {
                    if !c.is_ascii_hexdigit() {
                        // MuPDF warns, then treats the byte as a zero digit.
                        self.diag
                            .warn("lex.bad_hex_char", "invalid character in hex string");
                    }
                    match high.take() {
                        Some(h) => out.push(h * 16 + unhex(c)),
                        None => high = Some(unhex(c)),
                    }
                }
            }
        }
    }
}

fn keyword(word: &[u8]) -> RawToken {
    match word {
        b"R" => RawToken::R,
        b"true" => RawToken::True,
        b"trailer" => RawToken::Trailer,
        b"false" => RawToken::False,
        b"null" => RawToken::Null,
        b"newobj" => RawToken::NewObj,
        b"obj" => RawToken::Obj,
        b"endobj" => RawToken::EndObj,
        b"endstream" => RawToken::EndStream,
        b"stream" => RawToken::Stream,
        b"startxref" => RawToken::StartXref,
        b"xref" => RawToken::Xref,
        w if w.iter().all(|&b| (b' '..=b'~').contains(&b)) => RawToken::Keyword,
        _ => RawToken::Error,
    }
}

/// MuPDF's `fast_atoi`: overflow wraps, any run of leading '-' negates.
pub fn fast_atoi(text: &[u8]) -> i64 {
    let mut s = text;
    let mut neg = false;
    while let [b'-', rest @ ..] = s {
        neg = true;
        s = rest;
    }
    while let [b'+', rest @ ..] = s {
        s = rest;
    }
    let mut value: u64 = 0;
    for &b in s.iter().take_while(|b| b.is_ascii_digit()) {
        value = value.wrapping_mul(10).wrapping_add(u64::from(b - b'0'));
    }
    let value = value as i64;
    if neg { value.wrapping_neg() } else { value }
}

/// MuPDF's `acrobat_compatible_atof`: integer overflow wraps like Acrobat.
fn acrobat_atof(text: &[u8]) -> f32 {
    let mut s = text;
    let mut neg = false;
    while let [b'-', rest @ ..] = s {
        neg = true;
        s = rest;
    }
    while let [b'+', rest @ ..] = s {
        s = rest;
    }
    let mut int: u32 = 0;
    while let [d @ b'0'..=b'9', rest @ ..] = s {
        int = int.wrapping_mul(10).wrapping_add(u32::from(d - b'0'));
        s = rest;
    }
    if let [b'.', rest @ ..] = s {
        let mut n = 0f32;
        let mut d = 1f32;
        for &b in rest.iter().take_while(|b| b.is_ascii_digit()) {
            n = 10.0 * n + f32::from(b - b'0');
            d *= 10.0;
        }
        let v = int as f32 + n / d;
        if neg { -v } else { v }
    } else if neg {
        // C computes -(unsigned) and converts to float: unsigned negation.
        int.wrapping_neg() as f32
    } else {
        int as f32
    }
}

pub use crate::strtof::fz_atof;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_tokens_write_their_bytes_into_the_buffer() {
        let diag = Diagnostics::new();
        let input = b"/F1 12 Tf (a\\)b) Tj <41 42> 1.5 obj -.5x [";
        let mut lexer = Lexer::new(input, 0, &diag);
        let mut buf = Vec::new();
        let mut got = Vec::new();
        loop {
            let t = lexer.next_raw(&mut buf);
            if t == RawToken::Eof {
                break;
            }
            got.push((t, buf.clone()));
        }
        let want: Vec<(RawToken, &[u8])> = vec![
            (RawToken::Name, b"F1"),
            (RawToken::Int(12), b"12"),
            (RawToken::Keyword, b"Tf"),
            (RawToken::String, b"a)b"),
            (RawToken::Keyword, b"Tj"),
            (RawToken::String, b"AB"),
            (RawToken::Real(1.5), b"1.5"),
            (RawToken::Obj, b"obj"),
            (RawToken::Keyword, b"-.5x"),
            (RawToken::OpenArray, b""),
        ];
        assert_eq!(got.len(), want.len());
        for ((t, b), (wt, wb)) in got.iter().zip(&want) {
            assert_eq!(t, wt);
            // Numbers leave their digits behind; only bytes of names,
            // strings and keywords are the token's.
            if matches!(t, RawToken::Name | RawToken::String | RawToken::Keyword) {
                assert_eq!(b.as_slice(), *wb, "{t:?}");
            }
        }
        // The same input as tokens.
        let tokens = tokens(input);
        assert_eq!(tokens[3], Token::String(b"a)b".to_vec()));
        assert_eq!(tokens[8], Token::Keyword(b"-.5x".to_vec()));
    }

    fn tokens(input: &[u8]) -> Vec<Token> {
        let diag = Diagnostics::new();
        let mut lexer = Lexer::new(input, 0, &diag);
        let mut out = Vec::new();
        loop {
            let t = lexer.next_token();
            if t == Token::Eof {
                return out;
            }
            out.push(t);
        }
    }

    fn one(input: &[u8]) -> Token {
        tokens(input).into_iter().next().unwrap_or(Token::Eof)
    }

    #[test]
    fn structural_tokens_and_comments() {
        assert_eq!(
            tokens(b"<< /A [1 2] >> % comment\r{ }"),
            vec![
                Token::OpenDict,
                Token::Name(b"A".to_vec()),
                Token::OpenArray,
                Token::Int(1),
                Token::Int(2),
                Token::CloseArray,
                Token::CloseDict,
                Token::OpenBrace,
                Token::CloseBrace,
            ]
        );
    }

    #[test]
    fn keywords() {
        assert_eq!(
            tokens(
                b"obj endobj stream endstream xref trailer startxref R true false null newobj ID"
            ),
            vec![
                Token::Obj,
                Token::EndObj,
                Token::Stream,
                Token::EndStream,
                Token::Xref,
                Token::Trailer,
                Token::StartXref,
                Token::R,
                Token::True,
                Token::False,
                Token::Null,
                Token::NewObj,
                Token::Keyword(b"ID".to_vec()),
            ]
        );
        // Non-printable bytes in a keyword make it an error token.
        assert_eq!(one(b"ab\x80c "), Token::Error);
    }

    #[test]
    fn numbers_follow_mupdf_leniency() {
        assert_eq!(one(b"42"), Token::Int(42));
        assert_eq!(one(b"-17"), Token::Int(-17));
        assert_eq!(one(b"+5"), Token::Int(5));
        assert_eq!(one(b"--5"), Token::Int(-5));
        assert_eq!(one(b"3.5"), Token::Real(3.5));
        assert_eq!(one(b".5"), Token::Real(0.5));
        assert_eq!(one(b"-.25"), Token::Real(-0.25));
        assert_eq!(one(b"4."), Token::Real(4.0));
        // Two dots or a letter make a keyword, not a number.
        assert_eq!(one(b"1.2.3"), Token::Keyword(b"1.2.3".to_vec()));
        assert_eq!(one(b"12abc"), Token::Keyword(b"12abc".to_vec()));
        // A '-' inside a number ends the value but not the token.
        assert_eq!(
            tokens(b"0.000000000000-5684342 7"),
            vec![Token::Real(0.0), Token::Int(7)]
        );
        assert_eq!(one(b"5-3"), Token::Int(5));
        // A lone sign is zero.
        assert_eq!(one(b"-"), Token::Int(0));
        assert_eq!(one(b"."), Token::Real(0.0));
    }

    #[test]
    fn number_overflow_wraps_like_mupdf() {
        assert_eq!(one(b"18446744073709551617"), Token::Int(1));
        // Integer part of ten or more digits uses the Acrobat routine, which
        // wraps the integer in 32 bits: 123450000000000000000678 -> 678 style.
        let Token::Real(v) = one(b"4294967296.5") else {
            panic!("expected a real");
        };
        assert_eq!(v, 0.5);
    }

    #[test]
    fn numbers_stop_at_delimiters() {
        assert_eq!(
            tokens(b"1/A"),
            vec![Token::Int(1), Token::Name(b"A".to_vec())]
        );
        assert_eq!(
            tokens(b"2[3]"),
            vec![
                Token::Int(2),
                Token::OpenArray,
                Token::Int(3),
                Token::CloseArray
            ]
        );
    }

    #[test]
    fn names_decode_hex_escapes() {
        assert_eq!(one(b"/A#20B"), Token::Name(b"A B".to_vec()));
        assert_eq!(one(b"/#2f"), Token::Name(b"/".to_vec()));
        // "#00" is not decoded.
        assert_eq!(one(b"/a#00b"), Token::Name(b"a#00b".to_vec()));
        // Malformed escapes keep the '#'.
        assert_eq!(one(b"/a#zz"), Token::Name(b"a#zz".to_vec()));
        assert_eq!(one(b"/a#4"), Token::Name(b"a#".to_vec()));
        assert_eq!(one(b"/a#4g"), Token::Name(b"a#4g".to_vec()));
        assert_eq!(one(b"/"), Token::Name(Vec::new()));
    }

    #[test]
    fn long_names_are_truncated_with_a_warning() {
        let mut input = b"/".to_vec();
        input.extend(std::iter::repeat_n(b'x', MAX_NAME_LENGTH + 10));
        input.extend_from_slice(b" 1");
        let diag = Diagnostics::new();
        let mut lexer = Lexer::new(&input, 0, &diag);
        let Token::Name(name) = lexer.next_token() else {
            panic!("expected a name");
        };
        assert_eq!(name.len(), MAX_NAME_LENGTH);
        assert_eq!(lexer.next_token(), Token::Int(1));
        assert!(diag.has("lex.name_too_long"));
    }

    #[test]
    fn literal_strings() {
        assert_eq!(one(b"(a(b)c)"), Token::String(b"a(b)c".to_vec()));
        assert_eq!(
            one(b"(\\n\\r\\t\\b\\f\\(\\)\\\\)"),
            Token::String(b"\n\r\t\x08\x0c()\\".to_vec())
        );
        assert_eq!(
            one(b"(\\101\\7\\0123)"),
            Token::String(b"A\x07\n3".to_vec())
        );
        assert_eq!(one(b"(\\q)"), Token::String(b"q".to_vec()));
        // Line continuations.
        assert_eq!(
            one(b"(a\\\nb\\\r\nc\\\rd)"),
            Token::String(b"abcd".to_vec())
        );
        // Raw EOLs normalize to LF.
        assert_eq!(
            one(b"(a\rb\r\nc\nd)"),
            Token::String(b"a\nb\nc\nd".to_vec())
        );
        assert_eq!(one(b"(unterminated"), Token::Error);
    }

    #[test]
    fn hex_strings() {
        assert_eq!(one(b"<48 65 6c6C6f>"), Token::String(b"Hello".to_vec()));
        assert_eq!(one(b"<4>"), Token::String(vec![0x40]));
        assert_eq!(one(b"<>"), Token::String(Vec::new()));
        assert_eq!(one(b"<48"), Token::Error);
        let diag = Diagnostics::new();
        let mut lexer = Lexer::new(b"<4z>", 0, &diag);
        assert_eq!(lexer.next_token(), Token::String(vec![0x40]));
        assert!(diag.has("lex.bad_hex_char"));
    }

    #[test]
    fn stray_closers_are_errors() {
        assert_eq!(one(b")"), Token::Error);
        assert_eq!(one(b"> "), Token::Error);
    }

    #[test]
    fn no_string_mode_rejects_strings() {
        let diag = Diagnostics::new();
        let mut lexer = Lexer::new(b"(abc) <41> <<", 0, &diag);
        assert_eq!(lexer.next_token_no_string(), Token::Error);
        assert_eq!(
            lexer.next_token_no_string(),
            Token::Keyword(b"abc".to_vec())
        );
        assert_eq!(lexer.next_token_no_string(), Token::Error);
    }

    #[test]
    fn whitespace_includes_nul() {
        assert_eq!(
            tokens(b"1\x002\x0c3"),
            vec![Token::Int(1), Token::Int(2), Token::Int(3)]
        );
    }
}
