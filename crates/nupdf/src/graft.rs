// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-graft.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Copying objects and pages from one document into another, as
//! `pdf_graft_mapped_object` and `pdf_graft_mapped_page` do. A graft map
//! remembers which source objects it copied, so an object two pages share
//! is copied once.

use std::sync::Arc;

use crate::document::Document;
use crate::object::{Dict, ObjRef, Object};

/// The page entries `pdf_graft_mapped_page` copies, looked up with
/// inheritance. Entries that lead to other pages are left behind.
const COPY_LIST: [&[u8]; 9] = [
    b"Contents",
    b"Resources",
    b"MediaBox",
    b"CropBox",
    b"BleedBox",
    b"TrimBox",
    b"ArtBox",
    b"Rotate",
    b"UserUnit",
];

/// `pdf_graft_map`: the destination document and, once an indirect object
/// has been copied, the source and where each of its objects went.
#[derive(Debug)]
pub struct GraftMap<'a> {
    dst: &'a Document,
    /// The source document, by address, as MuPDF compares documents.
    src: Option<usize>,
    dst_from_src: Vec<i32>,
}

/// A container being copied: what is left of the source, and the copy so
/// far.
enum Frame {
    Dict {
        src: Arc<Dict>,
        next: usize,
        key: Option<Arc<[u8]>>,
        out: Dict,
    },
    Array {
        src: Arc<Vec<Object>>,
        next: usize,
        out: Vec<Object>,
    },
    /// An indirect object whose copy is being made, under its new number.
    Indirect {
        src_num: i32,
        new_num: i32,
        stream: Option<Arc<crate::object::Stream>>,
    },
}

impl<'a> GraftMap<'a> {
    /// `pdf_new_graft_map`.
    pub fn new(dst: &'a Document) -> GraftMap<'a> {
        GraftMap {
            dst,
            src: None,
            dst_from_src: Vec::new(),
        }
    }

    /// `pdf_graft_mapped_object`: `obj`, from `src`, copied into the
    /// destination. An indirect object gets a new number before what it
    /// holds is copied, so numbers follow the order references are first
    /// met. A stream's data is copied raw, with a /Length to match.
    ///
    /// On an error, what was copied so far stays in the destination, as in
    /// MuPDF: objects already filled, and numbers taken but not filled,
    /// which are free.
    pub fn graft_object(&mut self, src: &Document, obj: &Object) -> Result<Object, String> {
        let mut stack: Vec<Frame> = Vec::new();
        let mut value = self.begin(src, obj, &mut stack)?;
        loop {
            if let Some(v) = value.take() {
                let Some(top) = stack.last_mut() else {
                    return Ok(v);
                };
                match top {
                    Frame::Dict { key, out, .. } => {
                        if let Some(k) = key.take() {
                            out.insert(k, v);
                        }
                    }
                    Frame::Array { out, .. } => out.push(v),
                    Frame::Indirect {
                        src_num,
                        new_num,
                        stream,
                    } => {
                        let (src_num, new_num, stream) = (*src_num, *new_num, stream.take());
                        stack.pop();
                        self.dst.update_object(new_num, v);
                        if let Some(stream) = stream {
                            let data = src
                                .raw_stream_data(&stream)
                                .map_err(|e| format!("cannot load stream ({src_num} 0 R): {e}"))?;
                            self.dst.update_stream(new_num, data.into_owned());
                        }
                        value = Some(Object::Ref(ObjRef::new(new_num, 0)));
                        continue;
                    }
                }
            }
            let Some(top) = stack.last_mut() else {
                return Ok(Object::Null);
            };
            match top {
                Frame::Dict {
                    src: d,
                    next,
                    key,
                    out,
                } => match d.entry(*next) {
                    Some((k, v)) => {
                        *next += 1;
                        *key = Some(k.clone());
                        let v = v.clone();
                        value = self.begin(src, &v, &mut stack)?;
                    }
                    None => {
                        let out = std::mem::take(out);
                        stack.pop();
                        value = Some(Object::Dict(Arc::new(out)));
                    }
                },
                Frame::Array { src: a, next, out } => match a.get(*next) {
                    Some(v) => {
                        *next += 1;
                        let v = v.clone();
                        value = self.begin(src, &v, &mut stack)?;
                    }
                    None => {
                        let out = std::mem::take(out);
                        stack.pop();
                        value = Some(Object::Array(Arc::new(out)));
                    }
                },
                // An indirect object's frame always has its content's
                // frame above it, or a value waiting for it.
                Frame::Indirect { .. } => return Err("graft: lost its place".into()),
            }
        }
    }

    /// Starts copying `obj`: its copy when that needs no more work, or
    /// `None` with frames pushed to make it.
    fn begin(
        &mut self,
        src: &Document,
        obj: &Object,
        stack: &mut Vec<Frame>,
    ) -> Result<Option<Object>, String> {
        match obj {
            Object::Ref(r) => {
                let here = std::ptr::from_ref(src) as usize;
                match self.src {
                    None => {
                        self.src = Some(here);
                        self.dst_from_src = vec![0; src.xref_len()];
                    }
                    Some(s) if s != here => {
                        return Err(
                            "grafted objects must all belong to the same source document".into(),
                        );
                    }
                    Some(_) => {}
                }
                let slot = usize::try_from(r.num)
                    .ok()
                    .filter(|&n| n >= 1 && n < self.dst_from_src.len())
                    .ok_or("source object number out of range")?;
                if self.dst_from_src[slot] != 0 {
                    return Ok(Some(Object::Ref(ObjRef::new(self.dst_from_src[slot], 0))));
                }
                let new_num = self.dst.create_object();
                self.dst_from_src[slot] = new_num;
                // An object that cannot be loaded resolves to no object,
                // and a free one to null, which in MuPDF is the same null
                // pointer: pdf_update_object deletes the new object then.
                let resolved = match src.object(r.num) {
                    None | Some(Object::Null) => {
                        self.dst.delete_object(new_num);
                        return Ok(Some(Object::Ref(ObjRef::new(new_num, 0))));
                    }
                    Some(obj) => obj,
                };
                let (inner, stream) = match resolved {
                    Object::Stream(s) => (Object::Dict(Arc::new(s.dict.clone())), Some(s)),
                    other => (other, None),
                };
                stack.push(Frame::Indirect {
                    src_num: r.num,
                    new_num,
                    stream,
                });
                self.begin_direct(src, &inner, stack)
            }
            other => self.begin_direct(src, other, stack),
        }
    }

    fn begin_direct(
        &mut self,
        src: &Document,
        obj: &Object,
        stack: &mut Vec<Frame>,
    ) -> Result<Option<Object>, String> {
        match obj {
            Object::Dict(d) => {
                stack.push(Frame::Dict {
                    src: d.clone(),
                    next: 0,
                    key: None,
                    out: Dict::new(),
                });
                Ok(None)
            }
            Object::Array(a) => {
                stack.push(Frame::Array {
                    src: a.clone(),
                    next: 0,
                    out: Vec::with_capacity(a.len()),
                });
                Ok(None)
            }
            Object::Stream(s) => {
                self.begin_direct(src, &Object::Dict(Arc::new(s.dict.clone())), stack)
            }
            Object::Ref(_) => self.begin(src, obj, stack),
            // Primitives belong to no document and are used as they are.
            other => Ok(Some(other.clone())),
        }
    }

    /// `pdf_graft_mapped_page`: page `page_from` of `src` copied into the
    /// destination as a new page object, inserted before page `page_to`
    /// (after the last one when negative). Only the entries in
    /// `COPY_LIST` are copied, inherited ones included.
    pub fn graft_page(
        &mut self,
        src: &Document,
        page_from: i32,
        page_to: i32,
    ) -> Result<(), String> {
        let page = src.page(page_from).map_err(|e| e.0)?;
        let mut dict = Dict::new();
        dict.put(b"Type", Object::name(b"Page"));
        for key in COPY_LIST {
            if let Some(value) = src.inherited(&page, key).map_err(|e| e.0)? {
                let copy = self.graft_object(src, &value)?;
                dict.put(key, copy);
            }
        }
        let num = self.dst.add_object(Object::Dict(Arc::new(dict)));
        crate::insert::insert_page(self.dst, page_to, num)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::{WriteOptions, save};

    fn open(bytes: &[u8]) -> Document {
        Document::open(bytes.to_vec()).unwrap_or_else(|e| panic!("{e:?}"))
    }

    /// `mutool merge -o out.pdf minimal.pdf multipage-inherit.pdf 2-3`,
    /// from the first object to the xref, with nupdf as the producer.
    const MERGED: &str = "1 0 obj\n\
<</Type/Catalog/Pages 2 0 R/Info<</Producer(nupdf)>>>>\n\
endobj\n\
\n\
2 0 obj\n\
<</Type/Pages/Count 3/Kids[5 0 R 8 0 R 9 0 R]>>\n\
endobj\n\
\n\
3 0 obj\n\
<</Length 43>>\n\
stream\n\
BT /F1 24 Tf 72 720 Td (Hello, nupdf) Tj ET\n\
endstream\n\
endobj\n\
\n\
4 0 obj\n\
<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>\n\
endobj\n\
\n\
5 0 obj\n\
<</Type/Page/Contents 3 0 R/Resources<</Font<</F1 4 0 R>>>>/MediaBox[0 0 612 792]/Parent 2 0 R>>\n\
endobj\n\
\n\
6 0 obj\n\
<</Length 40>>\n\
stream\n\
BT /F1 24 Tf 72 720 Td (Inherited) Tj ET\n\
endstream\n\
endobj\n\
\n\
7 0 obj\n\
<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>\n\
endobj\n\
\n\
8 0 obj\n\
<</Type/Page/Contents 6 0 R/Resources<</Font<</F1 7 0 R>>>>/MediaBox[0 0 595 842]/Rotate 90/Parent 2 0 R>>\n\
endobj\n\
\n\
9 0 obj\n\
<</Type/Page/Contents 6 0 R/Resources<</Font<</F1 7 0 R>>>>/MediaBox[0 0 300 300]/Parent 2 0 R>>\n\
endobj\n\
\n\
";

    #[test]
    fn pages_merge_as_mutool_merge_merges_them() {
        let dst = Document::create().unwrap_or_else(|e| panic!("{e:?}"));
        let a = open(include_bytes!("../../../corpus/smoke/minimal.pdf"));
        let b = open(include_bytes!(
            "../../../corpus/smoke/multipage-inherit.pdf"
        ));
        GraftMap::new(&dst)
            .graft_page(&a, 0, -1)
            .unwrap_or_else(|e| panic!("{e}"));
        // One map for both pages of the second file: their shared content
        // and font are copied once.
        let mut map = GraftMap::new(&dst);
        for page in [1, 2] {
            map.graft_page(&b, page, -1)
                .unwrap_or_else(|e| panic!("{e}"));
        }
        let out = save(&dst, &WriteOptions::default()).unwrap_or_else(|e| panic!("{e:?}"));
        let text = String::from_utf8_lossy(&out);
        let body = text.find("1 0 obj").zip(text.find("\nxref\n"));
        assert_eq!(body.map(|(i, j)| &text[i..=j]), Some(MERGED));
    }

    /// A stream whose /Length is a reference, and a reference a page
    /// holds twice, with an xref so that no repair bakes the length in.
    fn length_fixture() -> Vec<u8> {
        let objects = [
            "<</Type/Catalog/Pages 2 0 R>>",
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
            "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</A 6 0 R/B 6 0 R>>>>",
            "<</Length 5 0 R>>stream\nabc\nendstream",
            "3",
            "[1 2]",
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut xref = String::from("xref\n0 7\n0000000000 65535 f \n");
        for (i, body) in objects.iter().enumerate() {
            xref.push_str(&format!("{:010} 00000 n \n", pdf.len()));
            pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let start = pdf.len();
        pdf.extend_from_slice(xref.as_bytes());
        pdf.extend_from_slice(
            format!("trailer\n<</Root 1 0 R/Size 7>>\nstartxref\n{start}\n%%EOF\n").as_bytes(),
        );
        pdf
    }

    #[test]
    fn an_indirect_length_is_copied_then_replaced() {
        let dst = Document::create().unwrap_or_else(|e| panic!("{e:?}"));
        let src = open(&length_fixture());
        assert!(!src.repaired());
        GraftMap::new(&dst)
            .graft_page(&src, 0, -1)
            .unwrap_or_else(|e| panic!("{e}"));
        // The stream took 3 and its length 4, which stays behind, unused,
        // once the stream's /Length counts the data copied.
        let copied = match src.object(4) {
            Some(Object::Stream(s)) => src.raw_stream_data(&s).map(|d| d.len()),
            other => panic!("{other:?}"),
        };
        match dst.object(3) {
            Some(Object::Stream(s)) => assert_eq!(
                s.dict.get(b"Length"),
                copied.ok().map(|n| Object::Int(n as i64)).as_ref()
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(dst.object(4), Some(Object::Int(3)));
        let page = dst.page(0).unwrap_or(Object::Null);
        let resources = dst.dict_get(&page, b"Resources");
        let r = |k: &[u8]| resources.as_dict().and_then(|d| d.get(k)).cloned();
        assert_eq!(r(b"A"), Some(Object::Ref(ObjRef::new(5, 0))));
        assert_eq!(r(b"A"), r(b"B"));
    }

    #[test]
    fn references_outside_the_source_fail() {
        let dst = Document::create().unwrap_or_else(|e| panic!("{e:?}"));
        let src = open(&length_fixture());
        let mut map = GraftMap::new(&dst);
        let out_of_range = Object::Ref(ObjRef::new(99, 0));
        assert_eq!(
            map.graft_object(&src, &out_of_range),
            Err("source object number out of range".to_string())
        );
        // Once bound to a source, a map takes no other.
        let other = open(&length_fixture());
        let r = Object::Ref(ObjRef::new(6, 0));
        assert!(map.graft_object(&src, &r).is_ok());
        assert!(map.graft_object(&other, &r).is_err());
        // Primitives belong to no document.
        assert_eq!(
            map.graft_object(&other, &Object::Int(7)),
            Ok(Object::Int(7))
        );
    }

    #[test]
    fn long_reference_chains_do_not_exhaust_the_stack() {
        let mut pdf = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
            3 0 obj<</Type/Page/Parent 2 0 R/Resources 4 0 R>>endobj\n"
            .to_vec();
        let n = 50_000;
        for i in 4..4 + n {
            let next = if i + 1 < 4 + n {
                format!("{} 0 R", i + 1)
            } else {
                String::new()
            };
            pdf.extend_from_slice(format!("{i} 0 obj[{next}]endobj\n").as_bytes());
        }
        pdf.extend_from_slice(format!("trailer<</Root 1 0 R/Size {}>>\n", 4 + n).as_bytes());
        let src = open(&pdf);
        let dst = Document::create().unwrap_or_else(|e| panic!("{e:?}"));
        GraftMap::new(&dst)
            .graft_page(&src, 0, -1)
            .unwrap_or_else(|e| panic!("{e}"));
        // The catalog and page tree, the chain, and the page.
        assert_eq!(dst.xref_len(), 3 + n + 1);
    }
}
