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
/// 0 to 359, put on the page itself. A page that is a direct dictionary is
/// changed where it sits in its parent's /Kids, as MuPDF changes it.
pub fn rotate_pages(doc: &Document, pages: &[i32], degrees: i32) -> Result<(), String> {
    for &index in pages {
        let page = doc.page(index).map_err(|e| e.0)?;
        let current = doc
            .inherited(&page, b"Rotate")
            .map_err(|e| e.0)?
            .map_or(0, |r| doc.resolve(&r).to_int());
        let rotate = (i64::from(current) + i64::from(degrees)).rem_euclid(360);
        let mut put = |d: &mut Dict| d.put(b"Rotate", Object::Int(rotate));
        let not_dict = || format!("not a dict (page {})", index + 1);
        match page {
            Object::Ref(r) => {
                let Object::Dict(d) = doc.resolve(&Object::Ref(r)) else {
                    return Err(not_dict());
                };
                let mut d = (*d).clone();
                put(&mut d);
                doc.alter_object(r.num, Object::Dict(Arc::new(d)));
            }
            _ => {
                let path = doc.page_path(index).ok_or_else(not_dict)?;
                edit_direct_page(doc, &path, &mut put).ok_or_else(not_dict)?;
            }
        }
    }
    Ok(())
}

/// Changes the page `path` leads to from the root /Pages node, in the
/// object that holds it.
fn edit_direct_page(doc: &Document, path: &[usize], f: &mut dyn FnMut(&mut Dict)) -> Option<()> {
    let mut trailer = doc.trailer();
    let root = trailer.get(b"Root").cloned()?;
    let Object::Dict(catalog) = doc.resolve(&root) else {
        return None;
    };
    let pages = catalog.get(b"Pages").cloned()?;
    // The root /Pages node is direct in the catalog: put it back there.
    if let Some(changed) = edit_at(doc, &pages, path, f)? {
        let mut catalog = (*catalog).clone();
        catalog.put(b"Pages", changed);
        let catalog = Object::Dict(Arc::new(catalog));
        match root {
            Object::Ref(r) => doc.alter_object(r.num, catalog),
            _ => {
                trailer.put(b"Root", catalog);
                doc.set_trailer(trailer);
            }
        }
    }
    Some(())
}

/// Applies `f` to what `path` leads to from `node`, through /Kids. An
/// indirect object on the way is changed in the edits, giving
/// `Some(None)`; a direct dictionary's changed copy is returned, for the
/// caller to put back. `None` when the path leads nowhere.
fn edit_at(
    doc: &Document,
    node: &Object,
    path: &[usize],
    f: &mut dyn FnMut(&mut Dict),
) -> Option<Option<Object>> {
    let (dict, num) = match node {
        Object::Ref(r) => match doc.object(r.num)? {
            Object::Dict(d) => (d, Some(r.num)),
            _ => return None,
        },
        Object::Dict(d) => (d.clone(), None),
        _ => return None,
    };
    let changed = match path.split_first() {
        None => {
            let mut d = (*dict).clone();
            f(&mut d);
            Some(d)
        }
        Some((&i, rest)) => match dict.get(b"Kids")? {
            Object::Ref(a) => {
                let Object::Array(kids) = doc.object(a.num)? else {
                    return None;
                };
                if let Some(kid) = edit_at(doc, kids.get(i)?, rest, f)? {
                    let mut kids = (*kids).clone();
                    kids[i] = kid;
                    doc.alter_object(a.num, Object::Array(Arc::new(kids)));
                }
                None
            }
            Object::Array(kids) => edit_at(doc, kids.get(i)?, rest, f)?.map(|kid| {
                let mut kids = (**kids).clone();
                kids[i] = kid;
                let mut d = (*dict).clone();
                d.put(b"Kids", Object::Array(Arc::new(kids)));
                d
            }),
            _ => return None,
        },
    };
    let changed = changed.map(|d| Object::Dict(Arc::new(d)));
    Some(match (num, changed) {
        (Some(num), Some(d)) => {
            doc.alter_object(num, d);
            None
        }
        (_, changed) => changed,
    })
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

    #[test]
    fn direct_pages_turn_where_they_sit() {
        // Two equal direct pages under an indirect /Kids array, in a /Pages
        // node direct in the catalog.
        let doc = open(
            b"%PDF-1.4\n\
            1 0 obj<</Type/Catalog/Pages<</Type/Pages/Kids 2 0 R/Count 2>>>>endobj\n\
            2 0 obj[<</Type/Page/Rotate 90>><</Type/Page/Rotate 90>>]endobj\n\
            trailer<</Root 1 0 R/Size 3>>\n",
        );
        rotate_pages(&doc, &[1], 90).unwrap_or_else(|e| panic!("{e}"));
        let rotate = |i| doc.dict_get(&doc.page(i).unwrap_or(Object::Null), b"Rotate");
        assert_eq!((rotate(0), rotate(1)), (Object::Int(90), Object::Int(180)));
        // The catalog and its direct /Pages are left alone.
        assert!(
            matches!(doc.object(1), Some(Object::Dict(d)) if matches!(d.get(b"Pages"), Some(Object::Dict(_))))
        );
    }
}
