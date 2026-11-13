// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Documents for unit tests, built from their objects: numbered from 1,
//! object 1 the catalog, with a classic xref table.

use crate::document::Document;

/// A file of `objects`, each written as `N 0 obj ... endobj`.
pub(crate) fn pdf_bytes(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    let n = objects.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<</Size {n}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    out
}

/// `pdf` (made by `pdf_bytes` or `pdf_of`) with `trailer` as its trailer
/// dictionary instead.
pub(crate) fn with_trailer(pdf: &[u8], trailer: &str) -> Vec<u8> {
    let find = |needle: &[u8]| {
        pdf.windows(needle.len())
            .rposition(|w| w == needle)
            .unwrap_or_else(|| panic!("no {needle:?}"))
    };
    let start = find(b"trailer\n") + b"trailer\n".len();
    let end = find(b"\nstartxref");
    [&pdf[..start], trailer.as_bytes(), &pdf[end..]].concat()
}

/// A file of textual `objects`.
pub(crate) fn pdf_of(objects: &[&str]) -> Vec<u8> {
    let objs: Vec<Vec<u8>> = objects.iter().map(|o| o.as_bytes().to_vec()).collect();
    pdf_bytes(&objs)
}

/// The document of textual `objects`.
pub(crate) fn doc_of(objects: &[&str]) -> Document {
    Document::open(pdf_of(objects)).unwrap_or_else(|e| panic!("{e:?}"))
}

/// The document of `objects`.
pub(crate) fn doc_bytes(objects: &[Vec<u8>]) -> Document {
    Document::open(pdf_bytes(objects)).unwrap_or_else(|e| panic!("{e:?}"))
}

/// A stream object: `<<dict/Length n>>stream ... endstream`.
pub(crate) fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    [
        format!("<<{dict}/Length {}>>stream\n", data.len()).as_bytes(),
        data,
        b"\nendstream",
    ]
    .concat()
}

/// A one-page document (catalog 1, pages 2, page 3, contents 4) whose
/// page has `resources` (the inside of the dictionary) and draws
/// `content`; `extra` become objects 5 onwards.
pub(crate) fn page_doc(content: &[u8], resources: &str, extra: &[Vec<u8>]) -> Document {
    let mut objs = vec![
        b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
        b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
        format!(
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<<{resources}>>/Contents 4 0 R>>"
        )
        .into_bytes(),
        stream("", content),
    ];
    objs.extend(extra.iter().cloned());
    doc_bytes(&objs)
}

#[test]
fn a_page_doc_opens_with_its_contents() {
    let doc = page_doc(b"0 0 1 1 re f", "", &[b"<</Type/Font>>".to_vec()]);
    assert_eq!(doc.page_count().ok(), Some(1));
    assert_eq!(doc.xref_len(), 6);
}
