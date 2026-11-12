// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-form.c (pdf_bake_document,
// pdf_bake_page, pdf_bake_annot, get_annot_ap, get_annot_transform),
// source/pdf/pdf-op-buffer.c (pdf_count_q_balance),
// source/pdf/pdf-appearance.c (the appearance written back by
// pdf_update_appearance) and source/pdf/pdf-xobject.c (pdf_new_xobject),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Flattening: each annotation's appearance drawn into its page's content,
//! and the annotation removed, as `pdf_bake_document` does it. Links stay.
//! With widgets baked, the form (/AcroForm) goes too.

use std::sync::Arc;

use crate::appearance;
use crate::content::{Op, ProcState, Processor};
use crate::document::Document;
use crate::geometry::{Matrix, Rect};
use crate::object::{Dict, ObjRef, Object};

/// What baking did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Baked {
    /// Annotations and widgets drawn into the pages and removed.
    pub annots: usize,
    pub widgets: usize,
    /// Pages changed.
    pub pages: usize,
    /// The appearances that could not be made: (page from 0, object, why).
    pub unmade: Vec<(i32, i32, String)>,
    /// Where baking stopped, as MuPDF stops: a page that does not load.
    pub stopped: Option<String>,
}

/// `pdf_count_q_balance`'s processor: how far `Q` goes below where the
/// stream starts, how many `q` are open at its end, and whether an
/// operator runs outside every `q`.
struct Balance {
    state: ProcState,
    balance: i32,
    min_q: i32,
    min_op_q: i32,
    ending: bool,
}

impl Processor for Balance {
    fn op(&mut self, op: Op<'_>) -> Result<(), String> {
        match op {
            Op::Save => self.balance += 1,
            Op::Restore => {
                if !self.ending {
                    self.balance -= 1;
                    self.min_q = self.min_q.min(self.balance);
                }
            }
            Op::EndOfData => self.ending = true,
            Op::End | Op::GsEnd | Op::BeginCompatibility | Op::EndCompatibility => {}
            _ => self.min_op_q = self.min_op_q.min(self.balance),
        }
        Ok(())
    }

    fn state(&mut self) -> &mut ProcState {
        &mut self.state
    }
}

/// `pdf_count_q_balance`: how many `q` to put before the contents and `Q`
/// after them to get back to the state they start in.
pub(crate) fn q_balance(doc: &Document, res: &Object, contents: &Object) -> (i32, i32) {
    let mut p = Balance {
        state: ProcState::default(),
        balance: 0,
        min_q: 0,
        min_op_q: 1,
        ending: false,
    };
    let contents = (!matches!(contents, Object::Null)).then_some(contents);
    let _ = crate::content::process_contents(doc, Some(res), contents, &mut p);
    let (mut prepend, mut append) = (-p.min_q, p.balance - p.min_q);
    if p.min_op_q == p.min_q {
        prepend += 1;
        append += 1;
    }
    (prepend, append)
}

fn reference(num: i32) -> Object {
    Object::Ref(ObjRef::new(num, 0))
}

/// A value to change: an object of its own, changed as that object, or a
/// direct value that the caller puts back where it was.
fn edit_dict(doc: &Document, value: &Object, f: impl FnOnce(&mut Dict)) -> Option<Object> {
    let mut d = doc.resolve(value).as_dict()?.clone();
    f(&mut d);
    let new = Object::Dict(Arc::new(d));
    match value {
        Object::Ref(r) => {
            doc.alter_object(r.num, new);
            Some(value.clone())
        }
        _ => Some(new),
    }
}

/// `pdf_dict_get`: the value of `key` as stored, a reference left as one.
fn raw(doc: &Document, container: &Object, key: &[u8]) -> Object {
    doc.resolve(container)
        .as_dict()
        .and_then(|d| d.get(key).cloned())
        .unwrap_or(Object::Null)
}

/// `get_annot_ap`: the normal appearance stream, or the one /AS names.
fn annot_ap(doc: &Document, annot: &Object) -> Option<Object> {
    let ap = raw(doc, annot, b"AP");
    if matches!(ap, Object::Null) {
        return None;
    }
    let n = raw(doc, &ap, b"N");
    if matches!(doc.resolve(&n), Object::Stream(_)) {
        return Some(n);
    }
    let r#as = doc.resolve(&raw(doc, annot, b"AS"));
    let state = raw(doc, &n, r#as.as_name()?);
    matches!(doc.resolve(&state), Object::Stream(_)).then_some(state)
}

/// `pdf_dict_get_matrix`: six numbers, else the identity.
fn matrix_of(doc: &Document, obj: &Object) -> Matrix {
    match doc.resolve(obj) {
        Object::Array(a) if a.len() == 6 => {
            let n = |i: usize| doc.resolve(&a[i]).to_real();
            Matrix {
                a: n(0),
                b: n(1),
                c: n(2),
                d: n(3),
                e: n(4),
                f: n(5),
            }
        }
        _ => Matrix::IDENTITY,
    }
}

/// `get_annot_transform`: the appearance's box, through its matrix, onto
/// the annotation's /Rect.
fn annot_transform(doc: &Document, annot: &Object, ap: &Object) -> Matrix {
    let rect = doc.to_rect(Some(&doc.dict_get(annot, b"Rect")));
    let form = doc.resolve(ap);
    let bbox = doc.to_rect(Some(&doc.dict_get(&form, b"BBox")));
    let bbox: Rect = bbox.transform(matrix_of(doc, &doc.dict_get(&form, b"Matrix")));
    let w = (rect.x1 - rect.x0) / (bbox.x1 - bbox.x0);
    let h = (rect.y1 - rect.y0) / (bbox.y1 - bbox.y0);
    // `rect.x0 - bbox.x0 * w`, contracted.
    let x = (-bbox.x0).mul_add(w, rect.x0);
    let y = (-bbox.y0).mul_add(h, rect.y0);
    Matrix {
        a: w,
        b: 0.0,
        c: 0.0,
        d: h,
        e: x,
        f: y,
    }
}

fn g(out: &mut Vec<u8>, f: f32) {
    crate::print::fmtfloat(f, out);
}

const INVISIBLE: i32 = 1;
const HIDDEN: i32 = 2;

/// The page tree node (an object of its own) whose direct /Resources the
/// page inherits.
fn resources_owner(doc: &Document, page: &Object) -> Option<i32> {
    let mut node = doc.parent_of(page)?;
    for _ in 0..64 {
        if let Object::Dict(_) = raw(doc, &node, b"Resources") {
            return node.as_ref().map(|r| r.num);
        }
        if !matches!(raw(doc, &node, b"Resources"), Object::Null) {
            return None;
        }
        node = doc.parent_of(&node)?;
    }
    None
}

fn put_resources(doc: &Document, node: i32, res: &Object) {
    if let Some(mut d) = doc.resolve(&reference(node)).as_dict().cloned() {
        d.put(b"Resources", res.clone());
        doc.alter_object(node, Object::Dict(Arc::new(d)));
    }
}

/// Page tree nodes whose direct /Resources pages share, and those pages.
type Shared = Vec<(i32, Vec<i32>)>;

/// `pdf_bake_page`, on page `index`.
fn bake_page(
    doc: &Document,
    index: i32,
    annots_too: bool,
    widgets_too: bool,
    baked: &mut Baked,
    shared: &mut Shared,
) -> Result<(), String> {
    let page = doc.page(index).map_err(|e| e.0)?;
    let annots_value = raw(doc, &page, b"Annots");
    let list = doc
        .resolve(&annots_value)
        .as_array()
        .map(<[Object]>::to_vec)
        .unwrap_or_default();
    if list.is_empty() {
        return Ok(());
    }
    // The page's own /Resources, else the inherited ones put on the page,
    // else a new dictionary.
    let mut page_dict = doc
        .resolve(&page)
        .as_dict()
        .cloned()
        .ok_or("page is not a dictionary")?;
    let mut res = page_dict
        .get(b"Resources")
        .cloned()
        .filter(|v| !matches!(v, Object::Null));
    // A direct dictionary is one object in MuPDF, wherever it is put: the
    // page that inherits it shares it with the node it comes from (and the
    // pages that took it before), and a change to it shows in all of them.
    let mut owner = None;
    if res.is_none() {
        let inherited = doc.inherited(&page, b"Resources").map_err(|e| e.0)?;
        if let Some(Object::Dict(_)) = &inherited {
            owner = resources_owner(doc, &page);
        }
        let r = inherited.unwrap_or_else(|| Object::Dict(Arc::new(Dict::new())));
        page_dict.put(b"Resources", r.clone());
        res = Some(r);
    } else if let Some(Object::Dict(_)) = &res {
        owner = shared
            .iter()
            .find(|(_, pages)| pages.contains(&index))
            .map(|(n, _)| *n);
    }
    let mut res = res.unwrap_or(Object::Null);
    let contents = page_dict.get(b"Contents").cloned().unwrap_or(Object::Null);
    let (mut prepend, append) = q_balance(doc, &res, &contents);
    let prologue = (prepend > 0).then(|| {
        let mut buf = Vec::new();
        while prepend > 0 {
            buf.extend_from_slice(b"q\n");
            prepend -= 1;
        }
        let mut d = Dict::new();
        d.put(b"Length", Object::Int(buf.len() as i64));
        doc.add_stream(d, buf)
    });
    let mut buf = Vec::new();
    for _ in 0..append {
        buf.extend_from_slice(b"Q\n");
    }
    // The XObjects the appearances are added to.
    let mut xobjects: Vec<(Vec<u8>, Object)> = Vec::new();
    let mut kept = Vec::new();
    for annot in &list {
        let subtype = doc.resolve(&doc.dict_get(annot, b"Subtype"));
        let bake = match subtype.as_name() {
            Some(b"Link") => false,
            Some(b"Widget") => widgets_too,
            _ => annots_too,
        };
        if !bake {
            kept.push(annot.clone());
            continue;
        }
        if subtype.as_name() == Some(b"Widget") {
            baked.widgets += 1;
        } else {
            baked.annots += 1;
        }
        let flags = doc.resolve(&doc.dict_get(annot, b"F")).to_int();
        if flags & (INVISIBLE | HIDDEN) != 0 {
            continue;
        }
        let Some(ap) = annot_ap(doc, annot) else {
            continue;
        };
        let num = annot.as_ref().map_or(0, |r| r.num);
        let name = format!("Annot{num}");
        xobjects.push((name.clone().into_bytes(), ap.clone()));
        if let Object::Ref(r) = &ap
            && let Object::Stream(s) = doc.resolve(&ap)
        {
            let mut d = s.dict.clone();
            d.put(b"Type", Object::name(b"XObject"));
            d.put(b"Subtype", Object::name(b"Form"));
            if let Ok(data) = doc.raw_stream_data(&s) {
                doc.replace_stream(r.num, d, data.into_owned());
            }
        }
        let m = annot_transform(doc, annot, &ap);
        buf.extend_from_slice(b"q\n");
        for (i, v) in [m.a, m.b, m.c, m.d, m.e, m.f].into_iter().enumerate() {
            if i > 0 {
                buf.push(b' ');
            }
            g(&mut buf, v);
        }
        buf.extend_from_slice(format!(" cm\n/{name} Do\nQ\n").as_bytes());
    }
    // /Resources /XObject, made where missing.
    let put_xobjects = |x: &mut Dict| {
        for (name, ap) in &xobjects {
            x.put(name, ap.clone());
        }
    };
    let res_dict_change = |r: &mut Dict| {
        let x = r
            .get(b"XObject")
            .cloned()
            .filter(|v| !matches!(v, Object::Null));
        match x {
            Some(x) => {
                if let Some(direct) = edit_dict(doc, &x, put_xobjects) {
                    r.put(b"XObject", direct);
                }
            }
            None => {
                let mut x = Dict::new();
                put_xobjects(&mut x);
                r.put(b"XObject", Object::Dict(Arc::new(x)));
            }
        }
    };
    if let Some(direct) = edit_dict(doc, &res, res_dict_change) {
        res = direct;
        page_dict.put(b"Resources", res.clone());
        if let Some(node) = owner {
            let sharers = match shared.iter_mut().find(|(n, _)| *n == node) {
                Some((_, pages)) => {
                    if !pages.contains(&index) {
                        pages.push(index);
                    }
                    pages.clone()
                }
                None => {
                    shared.push((node, vec![index]));
                    vec![index]
                }
            };
            put_resources(doc, node, &res);
            for &other in sharers.iter().filter(|&&p| p != index) {
                let _ =
                    crate::edit::edit_page(doc, other, &mut |d| d.put(b"Resources", res.clone()));
            }
        }
    } else if let Some(node) = owner {
        match shared.iter_mut().find(|(n, _)| *n == node) {
            Some((_, pages)) if !pages.contains(&index) => pages.push(index),
            Some(_) => {}
            None => shared.push((node, vec![index])),
        }
    }
    // The new contents: the prologue, the old contents, the appearances.
    let mut d = Dict::new();
    d.put(b"Length", Object::Int(buf.len() as i64));
    let tail = doc.add_stream(d, buf);
    match doc.resolve(&contents) {
        Object::Array(a) => {
            let mut items = (*a).clone();
            if let Some(p) = prologue {
                items.insert(0, reference(p));
            }
            items.push(reference(tail));
            let new = Object::Array(Arc::new(items));
            match &contents {
                Object::Ref(r) => doc.alter_object(r.num, new),
                _ => page_dict.put(b"Contents", new),
            }
        }
        _ => {
            let mut items = Vec::new();
            if let Some(p) = prologue {
                items.push(reference(p));
            }
            if !matches!(contents, Object::Null) {
                items.push(contents.clone());
            }
            items.push(reference(tail));
            page_dict.put(b"Contents", Object::Array(Arc::new(items)));
        }
    }
    let annots_new = Object::Array(Arc::new(kept));
    match &annots_value {
        Object::Ref(r) => doc.alter_object(r.num, annots_new),
        _ => page_dict.put(b"Annots", annots_new),
    }
    let page_dict = Object::Dict(Arc::new(page_dict));
    crate::edit::edit_page(doc, index, &mut |d| {
        if let Some(new) = page_dict.as_dict() {
            *d = new.clone();
        }
    })?;
    baked.pages += 1;
    Ok(())
}

/// `pdf_bake_document`: on every page, the missing appearances made, then
/// the annotations (with `annots`) and widgets (with `widgets`) drawn into
/// the page and removed. A page that does not load stops it there.
pub fn bake_document(doc: &Document, annots: bool, widgets: bool) -> Baked {
    let mut baked = Baked::default();
    let mut shared = Shared::new();
    let mut commit = appearance::Committer::new();
    let count = match doc.page_count() {
        Ok(n) => n,
        Err(e) => {
            baked.stopped = Some(e.0);
            return baked;
        }
    };
    for i in 0..count {
        let page = match doc.page(i) {
            Ok(p) => p,
            Err(e) => {
                baked.stopped = Some(e.0);
                return baked;
            }
        };
        if i == 0 {
            doc.settle_page_count();
        }
        // pdf_annot_request_synthesis then pdf_update_page: what has no
        // appearance gets one, written into the document.
        let ctm = doc
            .page_transform(&page)
            .map(|t| t.1)
            .unwrap_or(Matrix::IDENTITY);
        let (page_annots, page_widgets) = crate::annot::page_annots(doc, &page);
        let mut todo: Vec<Object> = Vec::new();
        if annots {
            todo.extend(page_annots);
        }
        // Only what has no appearance asks to be made; of those, a Stamp
        // MuPDF made one for on loading the page keeps it, unless making
        // another annotation before it threw that away.
        let todo: Vec<Object> = todo
            .into_iter()
            .filter(|a| appearance::appearance(doc, a).is_none())
            .collect();
        let again = appearance::made_again(doc, &todo, ctm);
        for (annot, again) in todo.into_iter().zip(again) {
            if !again {
                continue;
            }
            match appearance::synthesize(doc, &annot, ctm) {
                Ok(made) => commit.write_back(doc, &annot, &made),
                Err(e) => {
                    doc.diagnostics().warn(
                        "annot.appearance",
                        format!("cannot create appearance stream: {e}"),
                    );
                    baked
                        .unmade
                        .push((i, annot.as_ref().map_or(0, |r| r.num), e));
                }
            }
        }
        if widgets {
            for w in page_widgets {
                if appearance::appearance(doc, &w).is_some()
                    || !appearance::widget_made_again(doc, &w)
                {
                    continue;
                }
                // A button by its /FT as stored, as MuPDF compares it.
                let made = if crate::form::inheritable(doc, &w, b"FT").is_name(b"Btn") {
                    appearance::synthesize_button(doc, &w, ctm)
                        .map(|b| commit.write_back_button(doc, &w, &b))
                } else {
                    appearance::synthesize(doc, &w, ctm).map(|m| commit.write_back(doc, &w, &m))
                };
                if let Err(e) = made {
                    doc.diagnostics().warn(
                        "annot.appearance",
                        format!("cannot create appearance stream: {e}"),
                    );
                    baked.unmade.push((i, w.as_ref().map_or(0, |r| r.num), e));
                }
            }
        }
        if let Err(e) = bake_page(doc, i, annots, widgets, &mut baked, &mut shared) {
            baked.stopped = Some(e);
            return baked;
        }
    }
    if widgets {
        let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
        if let Object::Ref(r) = &root
            && let Some(mut catalog) = doc.resolve(&root).as_dict().cloned()
        {
            catalog.remove(b"AcroForm");
            doc.alter_object(r.num, Object::Dict(Arc::new(catalog)));
        }
    }
    baked
}
