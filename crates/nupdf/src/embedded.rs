// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-link.c (get_file_stream_and_name,
// pdf_is_embedded_file, pdf_get_filespec_params,
// pdf_load_embedded_file_contents, pdf_verify_embedded_file_checksum) and
// source/tools/murun.c (PDFDocument.getEmbeddedFiles),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Embedded files: the catalog's /EmbeddedFiles name tree, each file
//! specification's name, type, size and dates, and its contents.

use std::collections::HashSet;

use crate::document::Document;
use crate::object::Object;

/// Name-tree nodes deeper than this are not looked into.
const MAX_DEPTH: usize = 64;

fn get(doc: &Document, obj: &Object, key: &[u8]) -> Option<Object> {
    Some(doc.dict_get(obj, key)).filter(|v| !matches!(v, Object::Null))
}

/// `pdf_to_text_string`: a string as text, anything else as "".
fn text(doc: &Document, obj: &Object) -> String {
    match doc.resolve(obj) {
        Object::String(s) => crate::text_string::text_string(&s),
        _ => String::new(),
    }
}

/// `getEmbeddedFiles`: each name in the /EmbeddedFiles tree and its file
/// specification, in the order a script's record of them has: names that
/// are array indices first, in number order, then the others as the tree
/// gives them; a name given twice keeps its first place and its last value.
pub fn embedded_files(doc: &Document) -> Vec<(String, Object)> {
    let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
    let names = doc.dict_get(&root, b"Names");
    let tree = doc.dict_get(&names, b"EmbeddedFiles");
    let mut found: Vec<(String, Object)> = Vec::new();
    walk(doc, &tree, &mut found, &mut HashSet::new(), 0);
    let index = |k: &str| -> Option<u32> {
        k.parse::<u32>()
            .ok()
            .filter(|n| *n != u32::MAX && n.to_string() == k)
    };
    let (mut numbered, others): (Vec<_>, Vec<_>) =
        found.into_iter().partition(|(k, _)| index(k).is_some());
    numbered.sort_by_key(|(k, _)| index(k));
    numbered.into_iter().chain(others).collect()
}

fn walk(
    doc: &Document,
    node: &Object,
    out: &mut Vec<(String, Object)>,
    seen: &mut HashSet<i32>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    if let Object::Ref(r) = node
        && !seen.insert(r.num)
    {
        return;
    }
    if !matches!(doc.resolve(node), Object::Dict(_) | Object::Stream(_)) {
        return;
    }
    if let Object::Array(pairs) = doc.resolve(&doc.dict_get(node, b"Names")) {
        for i in (0..pairs.len()).step_by(2) {
            let key = text(doc, &pairs[i]);
            let value = pairs.get(i + 1).cloned().unwrap_or(Object::Null);
            match out.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = value,
                None => out.push((key, value)),
            }
        }
    }
    if let Object::Array(kids) = doc.resolve(&doc.dict_get(node, b"Kids")) {
        for kid in kids.iter() {
            walk(doc, kid, out, seen, depth + 1);
        }
    }
}

/// `get_file_stream_and_name`: the embedded file stream a specification
/// holds and the name it goes by. A file is given only with a name.
pub fn file_stream_and_name(doc: &Document, fs: &Object) -> (Option<Object>, Option<Object>) {
    let ef = get(doc, fs, b"EF").unwrap_or(Object::Null);
    let mut name = get(doc, fs, b"UF");
    let mut file = get(doc, &ef, b"UF");
    let mut any = name.clone();
    for key in [&b"F"[..], b"Unix", b"DOS", b"Mac"] {
        if name.is_none() && file.is_none() {
            name = get(doc, fs, key);
            if any.is_none() {
                any = name.clone();
            }
            file = get(doc, &ef, key);
        }
    }
    // Some files name it under one key and embed it under another.
    if any.is_some() && file.is_none() {
        name = any;
        file = [&b"UF"[..], b"F", b"Unix", b"DOS", b"Mac"]
            .iter()
            .find_map(|k| get(doc, &ef, k));
    }
    match name {
        Some(n) => (file, Some(n)),
        None => (None, None),
    }
}

fn is_stream(doc: &Document, obj: Option<&Object>) -> bool {
    obj.is_some_and(|o| matches!(doc.resolve(o), Object::Stream(_)))
}

/// `pdf_is_embedded_file`: a file specification (/Type /Filespec, or the
/// common /FileSpec) that embeds a stream.
pub fn is_embedded_file(doc: &Document, fs: &Object) -> bool {
    let ty = doc.resolve(&doc.dict_get(fs, b"Type"));
    if !matches!(ty.as_name(), Some(b"Filespec" | b"FileSpec")) {
        return false;
    }
    is_stream(doc, file_stream_and_name(doc, fs).0.as_ref())
}

/// What `pdf_get_filespec_params` reports of a specification.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilespecParams {
    pub filename: Option<String>,
    pub mimetype: Option<String>,
    /// -1 where there is no embedded stream.
    pub size: i64,
    /// Seconds since 1970; -1 where missing or unreadable.
    pub created: i64,
    pub modified: i64,
}

/// `pdf_get_filespec_params`.
pub fn filespec_params(doc: &Document, fs: &Object) -> FilespecParams {
    let mut out = FilespecParams {
        size: -1,
        created: -1,
        modified: -1,
        ..FilespecParams::default()
    };
    let (file, name) = file_stream_and_name(doc, fs);
    let Some(file) = file.filter(|f| is_stream(doc, Some(f))) else {
        return out;
    };
    let params = doc.dict_get(&file, b"Params");
    out.filename = Some(name.map(|n| text(doc, &n)).unwrap_or_default());
    out.mimetype = Some(match doc.resolve(&doc.dict_get(&file, b"Subtype")) {
        Object::Null => "application/octet-stream".to_string(),
        Object::Name(n) => String::from_utf8_lossy(&n).into_owned(),
        _ => String::new(),
    });
    out.size = i64::from(doc.resolve(&doc.dict_get(&params, b"Size")).to_int());
    let date = |key: &[u8]| match doc.resolve(&doc.dict_get(&params, key)) {
        Object::String(s) => crate::date::parse_date(&s, doc.diagnostics()).unwrap_or(-1),
        _ => -1,
    };
    out.created = date(b"CreationDate");
    out.modified = date(b"ModDate");
    out
}

/// `pdf_load_embedded_file_contents`: the decoded data, or None where the
/// specification embeds no stream.
pub fn contents(doc: &Document, fs: &Object) -> Option<Result<Vec<u8>, String>> {
    let file = file_stream_and_name(doc, fs).0?;
    match doc.resolve(&file) {
        Object::Stream(s) => Some(doc.stream_data(&s).map_err(|e| e.to_string())),
        _ => None,
    }
}

/// `pdf_verify_embedded_file_checksum`: whether the data's MD5 is the
/// /CheckSum its parameters give; true where there is nothing to check.
pub fn checksum_ok(doc: &Document, fs: &Object) -> Result<bool, String> {
    let Some(file) = file_stream_and_name(doc, fs)
        .0
        .filter(|f| is_stream(doc, Some(f)))
    else {
        return Ok(true);
    };
    let params = doc.dict_get(&file, b"Params");
    let checksum = match doc.resolve(&doc.dict_get(&params, b"CheckSum")) {
        Object::String(s) => s.to_vec(),
        _ => Vec::new(),
    };
    // strlen: up to the first NUL.
    if checksum
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(checksum.len())
        == 0
    {
        return Ok(true);
    }
    let data = contents(doc, fs).unwrap_or(Ok(Vec::new()))?;
    use md5::Digest;
    let digest = md5::Md5::digest(&data);
    Ok(checksum.len() == 16 && checksum[..] == digest[..])
}
