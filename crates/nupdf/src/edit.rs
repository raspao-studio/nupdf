// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-xref.c (pdf_set_metadata) and
// source/pdf/pdf-parse.c (pdf_new_text_string),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Small edits over the document's overlay: the document information
//! dictionary as `pdf_set_metadata` changes it, removing metadata, and
//! rotating pages.

use std::sync::Arc;

use crate::document::Document;
use crate::object::{Dict, Object};

/// `pdf_new_text_string`: the text as is when it is ASCII, else UTF-16BE
/// after a byte order mark.
pub fn text_string(s: &str) -> Object {
    if s.is_ascii() {
        return Object::String(Arc::from(s.as_bytes()));
    }
    let mut out = vec![0xfe, 0xff];
    for unit in s.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    Object::String(Arc::from(out))
}

/// `pdf_set_metadata(doc, key, value)`: the /Info entry `key`, with an
/// `info:` prefix or without, set to `value` as a text string. An /Info
/// that is not a dictionary is replaced by a new object first. Dates are
/// stored as the text given: MuPDF stores the parsed date, then the text
/// over it.
pub fn set_metadata(doc: &Document, key: &str, value: &str) {
    let mut trailer = doc.trailer();
    let info = trailer.get(b"Info").cloned().unwrap_or(Object::Null);
    let info = match doc.resolve(&info) {
        Object::Dict(_) => info,
        _ => {
            let num = doc.add_object(Object::Dict(Arc::new(Dict::new())));
            let r = Object::Ref(crate::object::ObjRef::new(num, 0));
            trailer.put(b"Info", r.clone());
            doc.set_trailer(trailer.clone());
            r
        }
    };
    let key = key.strip_prefix("info:").unwrap_or(key);
    // pdf_new_name takes a C string.
    let name = key.split('\0').next().unwrap_or_default();
    let value = text_string(value.split('\0').next().unwrap_or_default());
    match info {
        Object::Ref(r) => {
            if let Object::Dict(d) = doc.resolve(&info) {
                let mut d = (*d).clone();
                d.put(name.as_bytes(), value);
                doc.alter_object(r.num, Object::Dict(Arc::new(d)));
            }
        }
        Object::Dict(d) => {
            let mut d = (*d).clone();
            d.put(name.as_bytes(), value);
            trailer.put(b"Info", Object::Dict(Arc::new(d)));
            doc.set_trailer(trailer);
        }
        _ => {}
    }
}

/// Removes the trailer's /Info and the catalog's /Metadata, as
/// `pdf_dict_del` removes them.
pub fn strip_metadata(doc: &Document) {
    let mut trailer = doc.trailer();
    if trailer.get(b"Info").is_some() {
        trailer.remove(b"Info");
        doc.set_trailer(trailer.clone());
    }
    match trailer.get(b"Root").cloned() {
        Some(Object::Ref(r)) => {
            if let Object::Dict(d) = doc.resolve(&Object::Ref(r))
                && d.get(b"Metadata").is_some()
            {
                let mut d = (*d).clone();
                d.remove(b"Metadata");
                doc.alter_object(r.num, Object::Dict(Arc::new(d)));
            }
        }
        Some(Object::Dict(d)) if d.get(b"Metadata").is_some() => {
            let mut d = (*d).clone();
            d.remove(b"Metadata");
            trailer.put(b"Root", Object::Dict(Arc::new(d)));
            doc.set_trailer(trailer);
        }
        _ => {}
    }
}

/// Turns each page in `pages` (0-based, possibly repeated) by `degrees`,
/// clockwise: its /Rotate becomes the inherited value plus `degrees`, in
/// 0 to 359, put on the page itself.
pub fn rotate_pages(doc: &Document, pages: &[i32], degrees: i32) -> Result<(), String> {
    for &index in pages {
        let page = doc.page(index).map_err(|e| e.0)?;
        let current = doc
            .inherited(&page, b"Rotate")
            .map_err(|e| e.0)?
            .map_or(0, |r| doc.resolve(&r).to_int());
        let rotate = (i64::from(current) + i64::from(degrees)).rem_euclid(360);
        let Object::Ref(r) = page else {
            return Err("cannot rotate a page that is not an indirect object".into());
        };
        let Object::Dict(d) = doc.resolve(&Object::Ref(r)) else {
            return Err(format!("not a dict (page {})", index + 1));
        };
        let mut d = (*d).clone();
        d.put(b"Rotate", Object::Int(rotate));
        doc.alter_object(r.num, Object::Dict(Arc::new(d)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(bytes: &[u8]) -> Document {
        Document::open(bytes.to_vec()).unwrap_or_else(|e| panic!("{e:?}"))
    }

    #[test]
    fn text_strings_are_ascii_or_utf16() {
        assert_eq!(
            text_string("Plain"),
            Object::String(Arc::from(&b"Plain"[..]))
        );
        assert_eq!(
            text_string("é😀"),
            Object::String(Arc::from(
                &[0xfe, 0xff, 0x00, 0xe9, 0xd8, 0x3d, 0xde, 0x00][..]
            ))
        );
    }

    #[test]
    fn metadata_goes_into_info_made_if_missing() {
        let doc = open(include_bytes!("../../../corpus/smoke/minimal.pdf"));
        assert!(doc.trailer().get(b"Info").is_none());
        set_metadata(&doc, "info:Title", "Été");
        set_metadata(&doc, "Author", "nupdf");
        assert_eq!(doc.metadata("Title").as_deref(), Some("Été"));
        assert_eq!(doc.metadata("Author").as_deref(), Some("nupdf"));
        strip_metadata(&doc);
        assert!(doc.trailer().get(b"Info").is_none());
    }

    #[test]
    fn rotation_adds_to_what_the_page_inherits() {
        let doc = open(include_bytes!("../../../corpus/smoke/pages-rearrange.pdf"));
        rotate_pages(&doc, &[0, 3, 3], 90).unwrap_or_else(|e| panic!("{e}"));
        let rotate = |i| {
            let p = doc.page(i).unwrap_or(Object::Null);
            doc.inherited(&p, b"Rotate")
                .ok()
                .flatten()
                .map(|r| doc.resolve(&r))
        };
        assert_eq!(rotate(0), Some(Object::Int(90)));
        // Page 4 inherits 90, then turns twice.
        assert_eq!(rotate(3), Some(Object::Int(270)));
        assert!(rotate_pages(&doc, &[9], 90).is_err());
    }
}
