// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-repair.c (pdf_repair_obj,
// pdf_repair_xref_base), Copyright (C) 2004-2025 Artifex Software, Inc.

//! The repair scanner: when the xref cannot be trusted, walk the whole file
//! looking for `num generation obj`, stream extents and trailer dictionaries.
//! Building the new xref from the scan lives in `document`, because it
//! needs to load objects.

use memchr::memmem;

use crate::diag::Diagnostics;
use crate::lex::{Lexer, Token};
use crate::object::{Dict, MAX_OBJECT_NUMBER, Object};
use crate::parse::parse_dict;

#[derive(Debug, Clone)]
pub struct ScanEntry {
    pub num: i32,
    pub generation: i32,
    pub ofs: i64,
    pub stm_ofs: i64,
    /// Measured stream length when `/Length` was wrong, else -1.
    pub stm_len: i64,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub list: Vec<ScanEntry>,
    pub maxnum: i32,
    pub roots: Vec<Object>,
    pub encrypt: Option<Object>,
    pub id: Option<Object>,
    pub info: Option<Object>,
}

/// What `pdf_repair_obj` learned about one object.
struct ObjScan {
    next: Token,
    tmpofs: usize,
    stm_ofs: i64,
    stm_len: i64,
    root: Option<Object>,
}

pub fn scan(data: &[u8], diag: &Diagnostics) -> Result<Scan, String> {
    let mut lx = Lexer::new(data, 0, diag);
    let mut out = Scan::default();

    // Skip "%PDF-X.Y" and the comment line after it.
    let window = &data[..data.len().min(1024)];
    if window.len() >= 5
        && let Some(j) = window[..window.len() - 1]
            .windows(5)
            .take(window.len() - 5)
            .position(|w| w == b"%PDF-" || w == b"%FDF-")
    {
        lx.seek(j + 8);
    }
    while let Some(c) = lx.read_byte() {
        if c != b' ' && c != b'%' {
            lx.seek(lx.pos() - 1);
            break;
        }
    }

    let (mut num, mut generation) = (0i32, 0i32);
    let (mut numofs, mut genofs) = (0usize, 0usize);
    'scan: loop {
        let mut tmpofs = lx.pos();
        let mut tok = lx.next_token_no_string();
        loop {
            match tok {
                Token::Int(i) => {
                    if i < 0 {
                        num = 0;
                        generation = 0;
                    } else {
                        numofs = genofs;
                        num = generation;
                        genofs = tmpofs;
                        generation = i as i32;
                    }
                    continue 'scan;
                }
                Token::Obj => {
                    let found = match scan_object(&mut lx, &mut out) {
                        Ok(found) => found,
                        Err(e) => {
                            if out.roots.is_empty() {
                                return Err(e);
                            }
                            diag.warn(
                                "repair.ignoring_rest",
                                format!("cannot parse object ({num} {generation} R) - ignoring rest of file"),
                            );
                            break 'scan;
                        }
                    };
                    if let Some(root) = found.root {
                        out.roots.push(root);
                    }
                    tmpofs = found.tmpofs;
                    tok = found.next;
                    if num <= 0 || num > MAX_OBJECT_NUMBER {
                        diag.warn(
                            "repair.bad_object_number",
                            format!(
                                "ignoring object with invalid object number ({num} {generation} R)"
                            ),
                        );
                        continue;
                    }
                    generation = generation.clamp(0, 65535);
                    out.list.push(ScanEntry {
                        num,
                        generation,
                        ofs: numofs as i64,
                        stm_ofs: found.stm_ofs,
                        stm_len: found.stm_len,
                    });
                    out.maxnum = out.maxnum.max(num);
                    continue;
                }
                // Probably a trailer; possibly a bogus dict in a corrupt file.
                Token::OpenDict => {
                    if let Ok(dict) = parse_dict(&mut lx, 1) {
                        note_trailer(&dict, &mut out);
                    }
                    continue 'scan;
                }
                Token::Eof => break 'scan,
                _ => {
                    num = 0;
                    generation = 0;
                    continue 'scan;
                }
            }
        }
    }
    if out.list.is_empty() {
        return Err("no objects found".into());
    }
    Ok(out)
}

fn note_trailer(dict: &Dict, out: &mut Scan) {
    if let Some(e) = dict.get(b"Encrypt") {
        out.encrypt = Some(e.clone());
    }
    if let Some(id) = dict.get(b"ID")
        && (out.id.is_none() || out.encrypt.is_none() || dict.get(b"Encrypt").is_some())
    {
        out.id = Some(id.clone());
    }
    if let Some(root) = dict.get(b"Root") {
        out.roots.push(root.clone());
    }
    if let Some(info) = dict.get(b"Info") {
        out.info = Some(info.clone());
    }
}

/// `pdf_repair_obj`: called right after `num generation obj`. Returns the token
/// after the object so the scanner can continue from it.
fn scan_object(lx: &mut Lexer, out: &mut Scan) -> Result<ObjScan, String> {
    let mut stm_len_dict: i64 = 0;
    let mut stm_len: i64 = -1;
    let mut stm_ofs: i64 = 0;
    let mut root = None;
    let mut tmpofs = lx.pos();

    let mut tok = lx.next_token();
    // A truncated object at EOF must not overwrite a good one.
    if tok == Token::Eof {
        return Err("truncated object".into());
    }
    if tok == Token::OpenDict {
        let dict = match parse_dict(lx, 1) {
            Ok(d) => d,
            Err(e) => {
                if lx.at_eof() {
                    return Err(e.0);
                }
                Dict::new()
            }
        };
        if dict.get(b"Type").is_some_and(|t| t.is_name(b"XRef")) {
            if let Some(e) = dict.get(b"Encrypt") {
                out.encrypt = Some(e.clone());
            }
            if let Some(id) = dict.get(b"ID") {
                out.id = Some(id.clone());
            }
            root = dict.get(b"Root").cloned();
        }
        if let Some(Object::Int(len)) = dict.get(b"Length") {
            stm_len_dict = *len;
        }
    }

    while !matches!(
        tok,
        Token::Stream | Token::EndObj | Token::Error | Token::Eof | Token::Int(_)
    ) {
        tmpofs = lx.pos();
        tok = lx.next_token();
    }

    if tok == Token::Stream {
        if lx.read_byte() == Some(b'\r') && lx.peek_byte() == Some(b'\n') {
            lx.read_byte();
        }
        let start = lx.pos();
        stm_ofs = start as i64;
        let mut trusted = false;
        if stm_len_dict > 0 {
            lx.seek(start.saturating_add(stm_len_dict as usize));
            if lx.next_token() == Token::EndStream {
                trusted = true;
            } else {
                lx.seek(start);
            }
        }
        if !trusted {
            let data = lx.data();
            let end = match memmem::find(&data[start..], b"endstream") {
                Some(p) => start + p + 9,
                None => data.len(),
            };
            lx.seek(end);
            stm_len = end as i64 - start as i64 - 9;
        }
        tmpofs = lx.pos();
        tok = lx.next_token();
        if tok != Token::EndObj {
            lx.diagnostics()
                .warn("repair.missing_endobj", "object missing 'endobj' token");
        } else {
            tmpofs = lx.pos();
            tok = lx.next_token();
        }
    }

    Ok(ObjScan {
        next: tok,
        tmpofs,
        stm_ofs,
        stm_len,
        root,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_objects_and_trailer() {
        let data = b"%PDF-1.4\n1 0 obj <</Type/Catalog>> endobj\n2 0 obj [1 2] endobj\ntrailer <</Root 1 0 R/Info 2 0 R>>";
        let diag = Diagnostics::new();
        let scan = scan(data, &diag).unwrap();
        let nums: Vec<i32> = scan.list.iter().map(|e| e.num).collect();
        assert_eq!(nums, vec![1, 2]);
        assert_eq!(scan.maxnum, 2);
        assert_eq!(scan.roots.len(), 1);
        assert!(scan.info.is_some());
        // Offsets point at (whitespace before) the object number.
        let at = scan.list[0].ofs as usize;
        assert!(data[at..].trim_ascii_start().starts_with(b"1 0 obj"));
    }

    #[test]
    fn measures_streams_with_wrong_lengths() {
        let data = b"%PDF-1.4\n1 0 obj <</Length 99>> stream\nABCDEF\nendstream endobj\n";
        let scan = scan(data, &Diagnostics::new()).unwrap();
        let e = &scan.list[0];
        assert_eq!(&data[e.stm_ofs as usize..][..6], b"ABCDEF");
        assert_eq!(e.stm_len, 7); // includes the newline before endstream
    }

    #[test]
    fn trusts_correct_lengths() {
        let data = b"%PDF-1.4\n1 0 obj <</Length 6>> stream\nABCDEF\nendstream endobj\n";
        let scan = scan(data, &Diagnostics::new()).unwrap();
        assert_eq!(scan.list[0].stm_len, -1);
    }

    #[test]
    fn truncated_object_without_roots_fails() {
        let data = b"%PDF-1.4\n1 0 obj <</Type/Catalog>> endobj\n2 0 obj <</A";
        assert!(scan(data, &Diagnostics::new()).is_err());
    }

    #[test]
    fn empty_file_has_no_objects() {
        assert_eq!(
            scan(b"%PDF-1.4\n", &Diagnostics::new()).unwrap_err(),
            "no objects found"
        );
    }

    #[test]
    fn strings_do_not_swallow_the_file() {
        // An unbalanced '(' outside an object must not hide later objects.
        let data = b"%PDF-1.4\n( junk\n1 0 obj 5 endobj\n";
        let scan = scan(data, &Diagnostics::new()).unwrap();
        assert_eq!(scan.list.len(), 1);
    }
}
