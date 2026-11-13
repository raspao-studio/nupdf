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

use crate::fma::Madd;
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

/// `get_annot_transform`: the appearance's box, through its matrix, onto
/// the annotation's /Rect.
fn annot_transform(doc: &Document, annot: &Object, ap: &Object) -> Matrix {
    let rect = doc.to_rect(Some(&doc.dict_get(annot, b"Rect")));
    let form = doc.resolve(ap);
    let bbox = doc.to_rect(Some(&doc.dict_get(&form, b"BBox")));
    // `pdf_dict_get_matrix`: an array of any length, missing numbers 0.
    let matrix = crate::run::to_matrix(doc, &doc.dict_get(&form, b"Matrix"));
    let bbox: Rect = bbox.transform(matrix);
    let w = (rect.x1 - rect.x0) / (bbox.x1 - bbox.x0);
    let h = (rect.y1 - rect.y0) / (bbox.y1 - bbox.y0);
    // `rect.x0 - bbox.x0 * w`, contracted.
    let x = (-bbox.x0).madd(w, rect.x0);
    let y = (-bbox.y0).madd(h, rect.y0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{doc_bytes, stream};

    fn print(obj: &Object) -> String {
        crate::print::to_string(obj, crate::print::Options::default())
    }

    /// Each page's contents (its streams' data joined by `|`), the
    /// contents as stored, its resources and annotations; and /AcroForm.
    fn pages(doc: &Document) -> Vec<String> {
        let mut out = Vec::new();
        let n = doc.page_count().unwrap_or(0);
        for i in 0..n {
            let page = doc.page(i).unwrap_or_else(|e| panic!("{e}"));
            let contents = doc.dict_get(&page, b"Contents");
            let parts: Vec<Object> = match &contents {
                Object::Array(a) => a.to_vec(),
                other => vec![other.clone()],
            };
            let data: Vec<String> = parts
                .iter()
                .filter_map(|p| match doc.resolve(p) {
                    Object::Stream(s) => doc
                        .stream_data(&s)
                        .ok()
                        .map(|d| String::from_utf8_lossy(&d).into_owned()),
                    _ => None,
                })
                .collect();
            out.push(format!("PAGE {i} CONTENTS {:?}", data.join("|")));
            out.push(format!("  RAW {}", print(&raw(doc, &page, b"Contents"))));
            out.push(format!(
                "  RES {}",
                print(&doc.dict_get(&page, b"Resources"))
            ));
            out.push(format!(
                "  ANNOTS {}",
                print(&doc.dict_get(&page, b"Annots"))
            ));
        }
        let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
        out.push(format!(
            "ACROFORM {}",
            print(&doc.dict_get(&root, b"AcroForm"))
        ));
        out
    }

    /// Two pages sharing the resources of their parent; the first draws
    /// a `q` it leaves open and has its annotations in an array of its
    /// own: a square, a link, a checkbox showing its /On state and a
    /// circle with no appearance; the second draws a `Q` below where it
    /// starts, and has a hidden square and a note whose appearance is
    /// turned by its matrix.
    fn objects() -> Vec<Vec<u8>> {
        vec![
            b"<</Type/Catalog/Pages 2 0 R/AcroForm<</Fields[12 0 R]>>>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2/Resources<</Font<</F1 9 0 R>>>>>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Contents 5 0 R/Annots 7 0 R>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Contents[6 0 R]/Annots[13 0 R 14 0 R]>>"
                .to_vec(),
            stream("", b"q 0 0 10 10 re f"),
            stream("", b"Q 0 0 5 5 re f"),
            b"[10 0 R 11 0 R 12 0 R 15 0 R]".to_vec(),
            stream("/BBox[0 0 10 10]/Matrix[1 0 0 1 0 0]", b"0 0 1 rg 0 0 10 10 re f"),
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_vec(),
            b"<</Type/Annot/Subtype/Square/Rect[20 20 40 40]/AP<</N 8 0 R>>>>".to_vec(),
            b"<</Type/Annot/Subtype/Link/Rect[0 0 5 5]>>".to_vec(),
            b"<</Type/Annot/Subtype/Widget/FT/Btn/T(cb)/Rect[50 50 60 60]/AS/On/AP<</N<</On 16 0 R/Off 17 0 R>>>>>>".to_vec(),
            b"<</Type/Annot/Subtype/Square/F 2/Rect[0 0 10 10]/AP<</N 8 0 R>>>>".to_vec(),
            b"<</Type/Annot/Subtype/Text/Rect[100 100 120 110]/AP<</N 18 0 R>>>>".to_vec(),
            b"<</Type/Annot/Subtype/Circle/Rect[100 100 150 150]/C[1 0 0]>>".to_vec(),
            stream("/BBox[0 0 10 10]", b"0 g 0 0 10 10 re f"),
            stream("/BBox[0 0 10 10]", b"1 g 0 0 10 10 re f"),
            stream(
                "/BBox[0 0 10 20]/Matrix[0 1 -1 0 0 0]",
                b"0 0 1 RG 0 0 10 20 re S",
            ),
        ]
    }

    #[test]
    fn appearances_are_drawn_into_the_pages_as_mupdf_bakes_them() {
        let doc = doc_bytes(&objects());
        let baked = bake_document(&doc, true, true);
        assert_eq!(
            baked,
            Baked {
                annots: 4,
                widgets: 1,
                pages: 2,
                unmade: Vec::new(),
                stopped: None,
            }
        );
        // Golden output from `mutool bake`.
        let res = "<</Font<</F1 9 0 R>>/XObject<</Annot10 8 0 R/Annot12 16 0 R/Annot15 19 0 R/Annot14 18 0 R>>>>";
        assert_eq!(
            pages(&doc),
            [
                "PAGE 0 CONTENTS \"q 0 0 10 10 re f|Q\\nq\\n2 0 0 2 20 20 cm\\n/Annot10 Do\\nQ\\nq\\n1 0 0 1 50 50 cm\\n/Annot12 Do\\nQ\\nq\\n1 0 0 1 0 0 cm\\n/Annot15 Do\\nQ\\n\"".to_string(),
                "  RAW [5 0 R 20 0 R]".into(),
                format!("  RES {res}"),
                "  ANNOTS [11 0 R]".into(),
                "PAGE 1 CONTENTS \"q\\nq\\n|Q 0 0 5 5 re f|Q\\nq\\n1 0 0 1 120 100 cm\\n/Annot14 Do\\nQ\\n\"".into(),
                "  RAW [21 0 R 6 0 R 22 0 R]".into(),
                format!("  RES {res}"),
                "  ANNOTS []".into(),
                "ACROFORM null".into(),
            ]
        );
    }

    #[test]
    fn missing_appearances_are_made_first() {
        // A page two levels down the tree, its contents an array of their
        // own; a square whose /AS names no state, an annotation of no type
        // MuPDF draws, a text field and a checkbox with no appearance, and
        // a square whose appearance has a short /Matrix.
        let doc = doc_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R/AcroForm<</Fields[10 0 R 11 0 R]/DA(/Helv 0 Tf 0 g)>>>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1/Resources<</Font<</F1 9 0 R>>>>>>".to_vec(),
            b"<</Type/Pages/Parent 2 0 R/Kids[4 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 3 0 R/MediaBox[0 0 200 200]/Contents 5 0 R/Annots[7 0 R 8 0 R 10 0 R 11 0 R 12 0 R]>>".to_vec(),
            b"[6 0 R]".to_vec(),
            stream("", b"0 0 1 1 re f"),
            b"<</Type/Annot/Subtype/Square/Rect[0 0 10 10]/AS/Missing/AP<</N<</On 13 0 R>>>>>>".to_vec(),
            b"<</Type/Annot/Subtype/Foo/Rect[0 0 10 10]>>".to_vec(),
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_vec(),
            b"<</Type/Annot/Subtype/Widget/FT/Tx/T(name)/V(hello)/Rect[10 10 90 30]/DA(/Helv 12 Tf 0 g)>>".to_vec(),
            b"<</Type/Annot/Subtype/Widget/FT/Btn/T(cb)/AS/Off/Rect[100 10 110 20]/MK<</CA(4)>>>>".to_vec(),
            b"<</Type/Annot/Subtype/Square/Rect[20 20 30 30]/AP<</N 13 0 R>>>>".to_vec(),
            stream("/BBox[0 0 10 10]/Matrix[1 2]", b"0 g 0 0 10 10 re f"),
        ]);
        let baked = bake_document(&doc, true, true);
        assert_eq!((baked.annots, baked.widgets, baked.pages), (3, 2, 1));
        assert_eq!(
            baked.unmade,
            [(
                0,
                8,
                "cannot create appearance stream for Foo annotations".to_string()
            )]
        );
        assert!(doc.diagnostics().has("annot.appearance"));
        // Golden output from `mutool bake`.
        assert_eq!(
            pages(&doc),
            [
                "PAGE 0 CONTENTS \"q\\n|0 0 1 1 re f|Q\\nq\\n1 0 0 1 0 0 cm\\n/Annot7 Do\\nQ\\nq\\n1 0 0 1 10 10 cm\\n/Annot10 Do\\nQ\\nq\\n1 0 0 1 100 10 cm\\n/Annot11 Do\\nQ\\nq\\n1 0 0 .5 20 20 cm\\n/Annot12 Do\\nQ\\n\"",
                "  RAW 5 0 R",
                "  RES <</Font<</F1 9 0 R>>/XObject<</Annot7 14 0 R/Annot10 16 0 R/Annot11 17 0 R/Annot12 13 0 R>>>>",
                "  ANNOTS []",
                "ACROFORM null",
            ]
        );
        assert_eq!(print(&doc.resolve(&reference(5))), "[20 0 R 6 0 R 21 0 R]");
    }

    #[test]
    fn resources_held_apart_are_changed_where_they_are() {
        // A page with no annotations; one with resources of its own, held
        // apart with their XObjects, drawing a Q below where it starts;
        // one with no contents inheriting resources held apart.
        let doc = doc_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R 4 0 R 6 0 R]/Count 3/Resources 9 0 R>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Contents 5 0 R>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Resources 8 0 R/Contents 5 0 R/Annots[7 0 R]>>".to_vec(),
            stream("", b"Q 0 0 5 5 re f"),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Annots[12 0 R]>>".to_vec(),
            b"<</Type/Annot/Subtype/Square/Rect[20 20 40 40]/AP<</N 10 0 R>>>>".to_vec(),
            b"<</XObject 11 0 R>>".to_vec(),
            b"<</Font<<>>>>".to_vec(),
            stream("/BBox[0 0 10 10]", b"0 g 0 0 10 10 re f"),
            b"<<>>".to_vec(),
            b"<</Type/Annot/Subtype/Square/Rect[50 50 60 60]/AP<</N 10 0 R>>>>".to_vec(),
        ]);
        let baked = bake_document(&doc, true, false);
        assert_eq!((baked.annots, baked.pages), (2, 2));
        // Golden output from `mutool bake`.
        let contents: Vec<String> = pages(&doc)
            .into_iter()
            .filter(|l| l.contains("CONTENTS") || l.contains("RAW"))
            .collect();
        assert_eq!(
            contents,
            [
                "PAGE 0 CONTENTS \"Q 0 0 5 5 re f\"",
                "  RAW 5 0 R",
                "PAGE 1 CONTENTS \"q\\nq\\n|Q 0 0 5 5 re f|Q\\nq\\n2 0 0 2 20 20 cm\\n/Annot7 Do\\nQ\\n\"",
                "  RAW [13 0 R 5 0 R 14 0 R]",
                "PAGE 2 CONTENTS \"q\\n1 0 0 1 50 50 cm\\n/Annot12 Do\\nQ\\n\"",
                "  RAW [15 0 R]",
            ]
        );
        let page = |i| doc.page(i).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(print(&raw(&doc, &page(1), b"Resources")), "8 0 R");
        assert_eq!(print(&raw(&doc, &page(2), b"Resources")), "9 0 R");
        assert_eq!(print(&doc.resolve(&reference(8))), "<</XObject 11 0 R>>");
        assert_eq!(
            print(&doc.resolve(&reference(9))),
            "<</Font<<>>/XObject<</Annot12 10 0 R>>>>"
        );
        assert_eq!(print(&doc.resolve(&reference(11))), "<</Annot7 10 0 R>>");
    }

    #[test]
    fn baking_stops_at_a_page_that_does_not_load() {
        let doc = doc_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R 2 0 R]/Count 2>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]>>".to_vec(),
        ]);
        let baked = bake_document(&doc, true, true);
        assert_eq!(baked.pages, 0);
        assert!(baked.stopped.is_some(), "{baked:?}");
        // A page count no file of this size can hold.
        let no_tree = doc_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[]/Count -1>>".to_vec(),
        ]);
        assert!(bake_document(&no_tree, true, true).stopped.is_some());
    }
}
