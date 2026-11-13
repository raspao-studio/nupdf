// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-annot.c (pdf_set_annot_appearance),
// source/pdf/pdf-appearance.c (pdf_update_appearance and
// pdf_update_button_appearance writing back), source/pdf/pdf-xobject.c
// (pdf_new_xobject), source/pdf/pdf-font-add.c (pdf_add_simple_font,
// pdf_add_cjk_font) and source/pdf/pdf-resources.c
// (pdf_find_font_resource), Copyright (C) 2004-2026 Artifex Software, Inc.

//! A synthesized appearance written into the document as MuPDF writes it:
//! each font the writers made becomes an object of its own (one per font
//! and encoding in the document, as MuPDF's font resource cache keeps
//! them), then the form XObject, then the annotation's /AP.

use std::collections::HashMap;
use std::sync::Arc;

use super::{ButtonSynthesis, Form, Synthesis, rect_object};
use crate::document::Document;
use crate::object::{Dict, ObjRef, Object};

fn reference(num: i32) -> Object {
    Object::Ref(ObjRef::new(num, 0))
}

/// `pdf_new_xobject`'s dictionary for `f`, with `res` as its resources.
pub fn xobject_dict(f: &Form, res: Option<Dict>) -> Dict {
    let mut form = Dict::new();
    form.put(b"Type", Object::name(b"XObject"));
    form.put(b"Subtype", Object::name(b"Form"));
    form.put(b"BBox", rect_object(f.bbox));
    let m = f.matrix;
    form.put(
        b"Matrix",
        Object::Array(Arc::new(
            [m.a, m.b, m.c, m.d, m.e, m.f]
                .iter()
                .map(|&v| Object::Real(v))
                .collect(),
        )),
    );
    if let Some(res) = res {
        form.put(b"Resources", Object::Dict(Arc::new(res)));
    }
    form.put(b"Length", Object::Int(f.content.len() as i64));
    form
}

/// Writes appearances into one document, keeping the font objects made so
/// far (`doc->resources.fonts`).
#[derive(Debug, Default)]
pub struct Committer {
    fonts: HashMap<String, i32>,
}

impl Committer {
    pub fn new() -> Committer {
        Committer::default()
    }

    /// A font dictionary the appearance writers made, as an object:
    /// `pdf_add_simple_font`, or `pdf_add_cjk_font`, whose descendant and
    /// descriptor are objects numbered after it. One made before for the
    /// same font and encoding is used again.
    ///
    /// MuPDF keys a CJK font by the base-14 font the text was written in
    /// too; here two CJK fonts of one script are one object.
    fn font(&mut self, doc: &Document, font: &Object) -> Object {
        let Some(d) = font.as_dict() else {
            return font.clone();
        };
        let key = format!("{font:?}");
        if let Some(&n) = self.fonts.get(&key) {
            return reference(n);
        }
        let n = match d.get(b"DescendantFonts").and_then(Object::as_array) {
            Some([sub]) => {
                let top = doc.create_object();
                let sub_num = doc.create_object();
                let desc_num = doc.create_object();
                let mut sub = sub.as_dict().cloned().unwrap_or_default();
                if let Some(desc) = sub.get(b"FontDescriptor").cloned() {
                    doc.update_object(desc_num, desc);
                    sub.put(b"FontDescriptor", reference(desc_num));
                }
                doc.update_object(sub_num, Object::Dict(Arc::new(sub)));
                let mut top_dict = d.clone();
                top_dict.put(
                    b"DescendantFonts",
                    Object::Array(Arc::new(vec![reference(sub_num)])),
                );
                doc.update_object(top, Object::Dict(Arc::new(top_dict)));
                top
            }
            _ => doc.add_object(font.clone()),
        };
        self.fonts.insert(key, n);
        reference(n)
    }

    /// The resources with each new font as an object of its own.
    fn resources(&mut self, doc: &Document, res: &Dict) -> Dict {
        let mut res = res.clone();
        if let Some(Object::Dict(fonts)) = res.get(b"Font").cloned() {
            let mut made = Dict::new();
            for (k, v) in fonts.iter() {
                let r = self.font(doc, v);
                made.put(k, r);
            }
            res.put(b"Font", Object::Dict(Arc::new(made)));
        }
        res
    }

    /// `pdf_new_xobject`: the form as a new stream object, after the
    /// fonts it needs; its number.
    pub fn form(&mut self, doc: &Document, f: &Form) -> i32 {
        let res = f.resources.as_ref().map(|r| self.resources(doc, r));
        doc.add_stream(xobject_dict(f, res), f.content.clone())
    }

    /// `pdf_update_appearance` writing a new appearance back: the
    /// annotation with the /Rect and /RD the writer left, and /AP /N the
    /// new form (put into an /AP that is an object of its own, else into
    /// the annotation's). An annotation that is not an object of its own is
    /// changed in a copy MuPDF throws away with the page's local objects:
    /// the form is made, and the page never shows it.
    pub fn write_back(&mut self, doc: &Document, annot: &Object, made: &Synthesis) {
        let form = self.form(
            doc,
            &Form {
                bbox: made.bbox,
                matrix: made.matrix,
                resources: made.resources.clone(),
                content: made.content.clone(),
            },
        );
        let Some(mut d) = doc.resolve(&made.annot).as_dict().cloned() else {
            return;
        };
        match d.get(b"AP").cloned().filter(|v| !matches!(v, Object::Null)) {
            Some(Object::Ref(r)) => {
                if let Some(mut ap) = doc.resolve(&reference(r.num)).as_dict().cloned() {
                    ap.put(b"N", reference(form));
                    doc.alter_object(r.num, Object::Dict(Arc::new(ap)));
                }
            }
            Some(Object::Dict(ap)) => {
                let mut ap = (*ap).clone();
                ap.put(b"N", reference(form));
                d.put(b"AP", Object::Dict(Arc::new(ap)));
            }
            _ => {
                let mut ap = Dict::new();
                ap.put(b"N", reference(form));
                d.put(b"AP", Object::Dict(Arc::new(ap)));
            }
        }
        if let Object::Ref(r) = annot {
            doc.alter_object(r.num, Object::Dict(Arc::new(d)));
        }
    }

    /// `pdf_update_button_appearance` writing back: each state's form in
    /// the order MuPDF draws them, and a new /AP of them.
    pub fn write_back_button(&mut self, doc: &Document, annot: &Object, made: &ButtonSynthesis) {
        let mut n = Dict::new();
        let mut ap = Dict::new();
        for (key, state, f) in &made.forms {
            let num = self.form(doc, f);
            match state {
                Some(st) => n.put(st, reference(num)),
                None => ap.put(key.as_bytes(), reference(num)),
            }
        }
        if !n.is_empty() {
            ap.put(b"N", Object::Dict(Arc::new(n)));
        }
        let Some(mut d) = doc.resolve(&made.annot).as_dict().cloned() else {
            return;
        };
        d.put(b"AP", Object::Dict(Arc::new(ap)));
        if let Object::Ref(r) = annot {
            doc.alter_object(r.num, Object::Dict(Arc::new(d)));
        }
    }
}
