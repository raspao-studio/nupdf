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
/// specification, in the order a script's record of them has: MuJS keeps
/// an object's properties sorted by their bytes (strcmp), numbers or not;
/// a name given twice keeps its last value.
pub fn embedded_files(doc: &Document) -> Vec<(String, Object)> {
    let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
    let names = doc.dict_get(&root, b"Names");
    let tree = doc.dict_get(&names, b"EmbeddedFiles");
    let mut found: Vec<(String, Object)> = Vec::new();
    walk(doc, &tree, &mut found, &mut HashSet::new(), 0);
    found.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    found
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::stream;

    /// Six file specifications in an /EmbeddedFiles tree of two leaves,
    /// a name given in both.
    fn attachments() -> Document {
        let s = |t: &str| t.as_bytes().to_vec();
        crate::testutil::doc_bytes(&[
            s("<</Type/Catalog/Pages 2 0 R/Names<</EmbeddedFiles 3 0 R>>>>"),
            s("<</Type/Pages/Kids[]/Count 0>>"),
            s("<</Kids[4 0 R 5 0 R]>>"),
            s("<</Names[(b)6 0 R(10)7 0 R(2)8 0 R(01)9 0 R]>>"),
            s("<</Names[(b)12 0 R(z)13 0 R(x)14 0 R]>>"),
            // 6: a name and an embedded file under /F, with every
            // parameter.
            s("<</Type/Filespec/F(a.txt)/EF<</F 10 0 R>>>>"),
            // 7: named under /UF, embedded under /F.
            s("<</Type/FileSpec/UF(b.txt)/EF<</F 11 0 R>>>>"),
            // 8: a name and nothing embedded.
            s("<</Type/Filespec/DOS(c.txt)>>"),
            // 9: something embedded and no name.
            s("<</Type/Filespec/EF<</F 11 0 R>>>>"),
            stream(
                "/Type/EmbeddedFile/Subtype/text#2Fplain/Params<</Size 5\
                 /CreationDate(D:20240102030405Z)/ModDate(D:20250102030405Z)\
                 /CheckSum<5d41402abc4b2a76b9719d911017c592>>>",
                b"hello",
            ),
            stream("/Subtype(odd)/Params<</CheckSum(wrong)>>", b"hello"),
            // 12: what (b) is in the second leaf.
            s("<</Type/Filespec/Unix(e.txt)/EF<</Unix 15 0 R>>>>"),
            s("<</Type/Other/F(f.txt)/EF<</F 10 0 R>>>>"),
            s("<</Type/Filespec/Mac(g.txt)/EF<</Mac 16 0 R>>>>"),
            stream("/Params<</CheckSum<00>>>", b"no check"),
            stream(
                "/Filter/FlateDecode/Params<</CheckSum(0123456789abcdef)>>",
                b"bad",
            ),
        ])
    }

    #[test]
    fn attachments_read_as_mupdf_reads_them() {
        // getEmbeddedFiles and the filespec functions in `mutool run` on
        // the same file: names sorted by their bytes.
        let doc = attachments();
        let files = embedded_files(&doc);
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["01", "10", "2", "b", "x", "z"]);
        let report: Vec<(bool, FilespecParams)> = files
            .iter()
            .map(|(_, fs)| (is_embedded_file(&doc, fs), filespec_params(&doc, fs)))
            .collect();
        let none = FilespecParams {
            filename: None,
            mimetype: None,
            size: -1,
            created: -1,
            modified: -1,
        };
        let p = |name: &str, mime: &str, size, created, modified| FilespecParams {
            filename: Some(name.into()),
            mimetype: Some(mime.into()),
            size,
            created,
            modified,
        };
        assert_eq!(
            report,
            [
                (false, none.clone()),
                (true, p("b.txt", "", 0, -1, -1)),
                (false, none.clone()),
                (true, p("e.txt", "application/octet-stream", 0, -1, -1)),
                (true, p("g.txt", "application/octet-stream", 0, -1, -1)),
                (false, p("f.txt", "text/plain", 5, 1704164645, 1735787045)),
            ]
        );
        let fs = |name: &str| {
            files
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, f)| f.clone())
                .unwrap_or(Object::Null)
        };
        assert_eq!(checksum_ok(&doc, &fs("10")), Ok(false));
        assert_eq!(checksum_ok(&doc, &fs("b")), Ok(true));
        assert_eq!(checksum_ok(&doc, &fs("2")), Ok(true));
        // Damaged data reads as far as it goes: nothing here.
        assert_eq!(checksum_ok(&doc, &fs("x")), Ok(false));
        assert_eq!(contents(&doc, &fs("x")), Some(Ok(Vec::new())));
        assert_eq!(contents(&doc, &fs("10")), Some(Ok(b"hello".to_vec())));
        assert_eq!(contents(&doc, &fs("b")), Some(Ok(b"no check".to_vec())));
        assert_eq!(contents(&doc, &fs("2")), None);
        // The checksum that matches.
        let a = Object::Ref(crate::object::ObjRef::new(6, 0));
        assert_eq!(checksum_ok(&doc, &a), Ok(true));
        assert_eq!(file_stream_and_name(&doc, &fs("01")), (None, None));
    }

    #[test]
    fn a_tree_that_loops_is_read_once() {
        // MuPDF's script glue overflows its stack here; nupdf reads each
        // node once.
        let doc = crate::testutil::doc_of(&[
            "<</Type/Catalog/Pages 2 0 R/Names<</EmbeddedFiles 3 0 R>>>>",
            "<</Type/Pages/Kids[]/Count 0>>",
            "<</Kids[4 0 R 3 0 R]>>",
            "<</Kids[3 0 R]/Names[(a)5 0 R]>>",
            "<</Type/Filespec/F(a)>>",
        ]);
        let names: Vec<String> = embedded_files(&doc).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["a"]);
    }
}
