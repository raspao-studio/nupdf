// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-clean.c (pdf_filter_page_contents and
// its helpers) and source/pdf/pdf-write.c (clean_content_streams),
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! Content streams rewritten through the content writer, optionally
//! through the sanitize filter first: a page's contents become one new
//! stream, and with `recurse` the forms, patterns, soft-mask groups and
//! Type 3 glyphs its resources use are rewritten in place. This is what
//! `mutool clean -c` and `-s` do, and what redaction is built on.

use std::sync::Arc;

use crate::content::{self, Op, ProcState, Processor};
use crate::content_filter::{Hooks, Sanitize};
use crate::content_write::{Writer, bi_image};
use crate::document::Document;
use crate::geometry::Matrix;
use crate::object::{Dict, ObjRef, Object};

/// `MAX_CYCLE_STACK_DEPTH`.
const MAX_CYCLE_DEPTH: usize = 100;

/// `pdf_filter_options`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Rewrite the forms, patterns and glyphs the resources use too.
    pub recurse: bool,
    /// Give each use of a form its own copy, under a new name.
    pub instance_forms: bool,
    /// Hex for binary data and strings (`ascii`).
    pub ascii: bool,
    /// An operator per line (`newlines`).
    pub newlines: bool,
    /// Run the sanitize filter in front of the writer.
    pub sanitize: bool,
}

/// The objects being rewritten, by number (`pdf_cycle_list`).
#[derive(Debug, Clone, Default)]
pub struct Cycle(Vec<i32>);

impl Cycle {
    /// `pdf_cycle`: whether `obj` is already being rewritten (or the
    /// chain is too deep); otherwise the chain with it added.
    fn enter(&self, obj: &Object) -> Option<Cycle> {
        let num = obj.as_ref().map_or(0, |r| r.num);
        if num > 0 && (self.0.contains(&num) || self.0.len() > MAX_CYCLE_DEPTH) {
            return None;
        }
        let mut next = self.0.clone();
        next.push(num);
        Some(Cycle(next))
    }
}

/// The writer alone, as a processor (`pdf_new_buffer_processor`).
struct Plain<'a> {
    doc: &'a Document,
    writer: Writer<'a>,
    state: ProcState,
}

impl Processor for Plain<'_> {
    fn op(&mut self, op: Op<'_>) -> Result<(), String> {
        match op {
            Op::InlineImage {
                image,
                colorspace_name,
            } => {
                let doc = self.doc;
                let state = &self.state;
                let lookup = |n: &[u8]| state.lookup(doc, b"ColorSpace", n);
                let img = bi_image(doc, image, &lookup)?;
                self.writer
                    .inline_image(&img, colorspace_name.unwrap_or_default())
            }
            Op::DrawImage { image, .. } => {
                load_image(self.doc, &mut self.state, image)?;
                self.writer.op(&op);
                Ok(())
            }
            op => {
                self.writer.op(&op);
                Ok(())
            }
        }
    }

    fn state(&mut self) -> &mut ProcState {
        &mut self.state
    }

    fn wants_soft_mask(&self) -> bool {
        false
    }
}

/// `pdf_load_image`, which the interpreter calls before handing `Do` to
/// a processor that wants decoded images: an image that does not load
/// loses its `Do`. Loaded once per object, as MuPDF's store keeps them.
pub(crate) fn load_image(
    doc: &Document,
    state: &mut ProcState,
    obj: &Object,
) -> Result<(), String> {
    let key = obj.as_ref().map_or(0, |r| r.num);
    if key > 0
        && let Some(done) = state.store.images.get(&key)
    {
        return done.clone().map(|_| ());
    }
    let loaded = crate::image::load(doc, obj).map(Arc::new);
    if key > 0 {
        state.store.images.insert(key, loaded.clone());
    }
    loaded.map(|_| ())
}

/// What rewriting one content stream gives: its new data, and the
/// resources it uses (the old ones, without the sanitize filter).
struct Rewritten {
    data: Vec<u8>,
    resources: Option<Object>,
}

/// The number of the object a value sits in, as MuPDF's `parent_num`
/// records it: its own for an indirect value, else `container`'s.
fn holder_of(value: &Object, container: i32) -> i32 {
    value.as_ref().map_or(container, |r| r.num)
}

/// The holder of what `dict[key]` holds directly: the entry's own object
/// when it is indirect, else the dictionary's holder.
fn entry_holder(doc: &Document, dict: &Object, holder: i32, key: &[u8]) -> i32 {
    let entry = doc
        .resolve(dict)
        .as_dict()
        .and_then(|d| d.get(key).cloned());
    holder_of(&entry.unwrap_or(Object::Null), holder_of(dict, holder))
}

/// `pdf_filter_content_stream`: `contents` through the filters, then,
/// with `recurse`, the resources it uses. `holder` is the object that
/// `in_res` sits in when it is direct.
#[allow(clippy::too_many_arguments)]
fn filter_content_stream(
    doc: &Document,
    contents: Option<&Object>,
    in_res: Option<&Object>,
    holder: i32,
    transform: Matrix,
    opts: &Options,
    hooks: &mut dyn Hooks,
    struct_parents: i32,
    cycle: &Cycle,
) -> Result<Rewritten, String> {
    let diag = doc.diagnostics();
    let writer = Writer::new(diag, opts.ascii, opts.newlines);
    let (data, mut resources) = if opts.sanitize {
        let mut proc = Sanitize::new(doc, writer, struct_parents, transform, *opts, hooks, cycle);
        if let Some(res) = in_res {
            proc.push_resources(res.clone());
        }
        let run = match contents {
            Some(c) => content::process_raw_contents(doc, c, &mut proc),
            None => Ok(()),
        };
        run?;
        proc.close();
        let out_res = in_res.map(|_| proc.pop_resources());
        (proc.into_output(), out_res)
    } else {
        let mut proc = Plain {
            doc,
            writer,
            state: ProcState::default(),
        };
        if let Some(res) = in_res {
            proc.state.push_resources(res.clone());
        }
        if let Some(c) = contents {
            content::process_raw_contents(doc, c, &mut proc)?;
        }
        proc.writer.close();
        (proc.writer.out, in_res.cloned())
    };
    if let (Some(old), Some(new)) = (in_res, resources.as_mut()) {
        filter_resources(doc, old, holder, new, opts, hooks, cycle)?;
    }
    Ok(Rewritten { data, resources })
}

/// `pdf_filter_resources`: the forms, patterns, soft-mask groups and
/// Type 3 fonts in the new resources `res`, each rewritten in place.
/// Direct entries are MuPDF's objects shared with `in_res`: what changes
/// in one changes in the other, where `in_res` is an object of its own.
fn filter_resources(
    doc: &Document,
    in_res: &Object,
    holder: i32,
    res: &mut Object,
    opts: &Options,
    hooks: &mut dyn Hooks,
    cycle_up: &Cycle,
) -> Result<(), String> {
    if !opts.recurse {
        return Ok(());
    }
    let Some(cycle) = cycle_up.enter(in_res) else {
        return Ok(());
    };
    let category = |key: &[u8]| -> Vec<Object> {
        match doc.dict_get(res, key) {
            Object::Dict(d) => d.iter().map(|(_, v)| v.clone()).collect(),
            _ => Vec::new(),
        }
    };
    for gs in category(b"ExtGState") {
        let smask = doc.dict_get(&gs, b"SMask");
        if smask.as_dict().is_some() {
            let g = stored(doc, &smask, b"G");
            if let Some(g) = g {
                filter_xobject(doc, &g, Some(in_res), holder, opts, hooks, &cycle)?;
            }
        }
    }
    for pat in category(b"Pattern") {
        if doc.dict_get(&pat, b"PatternType").to_int() == 1 {
            filter_xobject(doc, &pat, Some(in_res), holder, opts, hooks, &cycle)?;
        }
    }
    if !opts.instance_forms {
        let xobjects = match doc.dict_get(res, b"XObject") {
            Object::Dict(d) => d.iter().map(|(k, v)| (k.to_vec(), v.clone())).collect(),
            _ => Vec::new(),
        };
        for (name, xobj) in xobjects {
            if !doc.dict_get(&xobj, b"Subtype").is_name(b"Form") {
                continue;
            }
            match xobj {
                Object::Dict(_) => {
                    let at = Placed {
                        old: in_res,
                        holder: entry_holder(doc, in_res, holder, b"XObject"),
                        name: &name,
                    };
                    filter_direct_xobject(doc, &xobj, &at, holder, res, opts, hooks, &cycle)?
                }
                _ => filter_xobject(doc, &xobj, Some(in_res), holder, opts, hooks, &cycle)?,
            }
        }
    }
    // Type 3 fonts take new resources; a direct font dictionary changes
    // where it sits in `res`.
    let fonts = match doc.dict_get(res, b"Font") {
        Object::Dict(d) => d.iter().map(|(k, v)| (k.to_vec(), v.clone())).collect(),
        _ => Vec::new(),
    };
    for (name, font) in fonts {
        if !doc.dict_get(&font, b"Subtype").is_name(b"Type3") {
            continue;
        }
        if let Some(changed) = filter_type3(doc, &font, in_res, opts, hooks, &cycle)? {
            put_in(doc, res, b"Font", &name, changed.clone());
            if opts.sanitize {
                put_in(doc, &mut in_res.clone(), b"Font", &name, changed);
            }
        }
    }
    Ok(())
}

/// `res[category][name] = value`, for a font dictionary held directly.
fn put_in(doc: &Document, res: &mut Object, category: &[u8], name: &[u8], value: Object) {
    let put = |d: &mut Dict| match d.get(category).cloned().unwrap_or(Object::Null) {
        Object::Ref(r) => edit_object(doc, r.num, |s| s.put(name, value.clone())),
        Object::Dict(s) => {
            let mut s = (*s).clone();
            s.put(name, value.clone());
            d.put(category, Object::Dict(Arc::new(s)));
        }
        _ => {}
    };
    match res {
        Object::Ref(r) => edit_object(doc, r.num, put),
        Object::Dict(d) => put(Arc::make_mut(d)),
        _ => {}
    }
}

/// The dictionary of object `num` changed by `f`. An object that a direct
/// form's data made a stream is still MuPDF's dictionary, with that data.
fn edit_object(doc: &Document, num: i32, f: impl FnOnce(&mut Dict)) {
    match doc.object(num) {
        Some(Object::Dict(d)) => {
            let mut d = (*d).clone();
            f(&mut d);
            doc.alter_object(num, Object::Dict(Arc::new(d)));
        }
        Some(Object::Stream(s)) => {
            if let Ok(data) = doc.raw_stream_data(&s) {
                let mut d = s.dict.clone();
                f(&mut d);
                doc.replace_stream(num, d, data.into_owned());
            }
        }
        _ => {}
    }
}

/// `pdf_dict_get`, None for a missing or null entry.
fn stored(doc: &Document, dict: &Object, key: &[u8]) -> Option<Object> {
    match doc.resolve(dict).as_dict()?.get(key)? {
        Object::Null => None,
        v => Some(v.clone()),
    }
}

fn struct_parents(doc: &Document, obj: &Object) -> i32 {
    doc.dict_get(obj, b"StructParents").to_int_or(-1)
}

/// `pdf_update_stream(..., 0)`, then `edit` on the stream's dictionary:
/// the data is stored as is, so /Filter and /DecodeParms go. Its first
/// step puts /Length in the object, which throws for an object that is
/// missing or not a dictionary.
fn update_stream(
    doc: &Document,
    num: i32,
    data: Vec<u8>,
    edit: impl FnOnce(&mut Dict),
) -> Result<(), String> {
    let mut dict = match doc.object(num).unwrap_or(Object::Null) {
        Object::Dict(d) => (*d).clone(),
        Object::Stream(s) => s.dict.clone(),
        other => {
            return Err(format!(
                "argument error: not a dict ({})",
                crate::edit::kind(&other)
            ));
        }
    };
    dict.put(b"Length", Object::Int(data.len() as i64));
    dict.remove(b"Filter");
    dict.remove(b"DecodeParms");
    edit(&mut dict);
    doc.replace_stream(num, dict, data);
    Ok(())
}

/// `pdf_filter_xobject`: a form (or pattern, or group) rewritten in place.
fn filter_xobject(
    doc: &Document,
    stm: &Object,
    page_res: Option<&Object>,
    page_holder: i32,
    opts: &Options,
    hooks: &mut dyn Hooks,
    cycle_up: &Cycle,
) -> Result<(), String> {
    let sp = struct_parents(doc, stm);
    let (old_res, holder) = match stored(doc, stm, b"Resources") {
        Some(r) => (Some(r), holder_of(stm, 0)),
        None => (page_res.cloned(), page_holder),
    };
    let Some(cycle) = cycle_up.enter(stm) else {
        return Ok(());
    };
    let out = filter_content_stream(
        doc,
        Some(stm),
        old_res.as_ref(),
        holder,
        Matrix::IDENTITY,
        opts,
        hooks,
        sp,
        &cycle,
    )?;
    let Some(r) = stm.as_ref() else {
        return Ok(());
    };
    update_stream(doc, r.num, out.data, |d| {
        d.put(b"Resources", out.resources.unwrap_or(Object::Null));
    })
}

/// Where a direct form sits: `old[XObject][name]`, in object `holder`.
struct Placed<'a> {
    old: &'a Object,
    holder: i32,
    name: &'a [u8],
}

/// `pdf_filter_xobject` for a form written directly in the resources, as
/// MuPDF treats it: it has no contents of its own, gets a /Length and
/// /Resources, and its data goes to the object that holds it (its
/// `parent_num`), which becomes a stream. A holder that is not an object
/// of the document gets the warning `pdf_update_stream` gives. The form is
/// one object in MuPDF, shared by the old resources and the new `res`, so
/// both change.
#[allow(clippy::too_many_arguments)]
fn filter_direct_xobject(
    doc: &Document,
    stm: &Object,
    at: &Placed<'_>,
    page_holder: i32,
    res: &mut Object,
    opts: &Options,
    hooks: &mut dyn Hooks,
    cycle: &Cycle,
) -> Result<(), String> {
    let sp = struct_parents(doc, stm);
    let (old_res, holder) = match stored(doc, stm, b"Resources") {
        Some(r) => (r, at.holder),
        None => (at.old.clone(), page_holder),
    };
    let out = filter_content_stream(
        doc,
        Some(stm),
        Some(&old_res),
        holder,
        Matrix::IDENTITY,
        opts,
        hooks,
        sp,
        cycle,
    )?;
    let Object::Dict(d) = stm else {
        return Ok(());
    };
    let mut d = (**d).clone();
    d.put(b"Length", Object::Int(out.data.len() as i64));
    let parent = (at.holder > 0 && (at.holder as usize) < doc.xref_len()).then_some(at.holder);
    match parent {
        Some(_) => {
            d.remove(b"Filter");
            d.remove(b"DecodeParms");
        }
        None => doc.diagnostics().warn(
            "object.out_of_range",
            format!(
                "object out of range ({} 0 R); xref size {}",
                at.holder,
                doc.xref_len()
            ),
        ),
    }
    d.put(b"Resources", out.resources.unwrap_or(Object::Null));
    let d = Object::Dict(Arc::new(d));
    put_in(doc, res, b"XObject", at.name, d.clone());
    if opts.sanitize {
        put_in(doc, &mut at.old.clone(), b"XObject", at.name, d);
    }
    if let Some(num) = parent
        && let Some(obj) = doc.object(num)
    {
        let dict = match obj {
            Object::Dict(d) => (*d).clone(),
            Object::Stream(s) => s.dict.clone(),
            _ => Dict::new(),
        };
        doc.replace_stream(num, dict, out.data);
    }
    Ok(())
}

/// `pdf_filter_xobject_instance`: a new copy of the form, rewritten as it
/// is drawn with `transform`. A form already being rewritten is used as
/// it is.
pub(crate) fn filter_xobject_instance(
    doc: &Document,
    old: &Object,
    page_res: Option<&Object>,
    transform: Matrix,
    opts: &Options,
    hooks: &mut dyn Hooks,
    cycle_up: &Cycle,
) -> Result<Object, String> {
    let sp = struct_parents(doc, old);
    let old_res = stored(doc, old, b"Resources").or_else(|| page_res.cloned());
    let Some(cycle) = cycle_up.enter(old) else {
        return Ok(old.clone());
    };
    let matrix = crate::run::to_matrix(doc, &doc.dict_get(old, b"Matrix"));
    let transform = matrix.concat(transform);
    let dict = match doc.resolve(old) {
        Object::Stream(s) => s.dict.clone(),
        Object::Dict(d) => (*d).clone(),
        _ => Dict::new(),
    };
    let num = doc.add_object(Object::Dict(Arc::new(dict)));
    // A copy draws no direct forms of its resources (instance_forms), so
    // where its resources sit does not matter.
    let out = filter_content_stream(
        doc,
        Some(old),
        old_res.as_ref(),
        0,
        transform,
        opts,
        hooks,
        sp,
        &cycle,
    )?;
    update_stream(doc, num, out.data, |d| {
        d.put(b"Resources", out.resources.unwrap_or(Object::Null));
    })?;
    Ok(Object::Ref(ObjRef::new(num, 0)))
}

/// `pdf_filter_type3`: each glyph procedure rewritten, sharing one new
/// resource dictionary. Returns the font dictionary when it is direct and
/// changed, for the caller to put back.
fn filter_type3(
    doc: &Document,
    font: &Object,
    page_res: &Object,
    opts: &Options,
    hooks: &mut dyn Hooks,
    cycle_up: &Cycle,
) -> Result<Option<Object>, String> {
    let Some(cycle) = cycle_up.enter(font) else {
        return Ok(None);
    };
    let in_res = stored(doc, font, b"Resources").unwrap_or_else(|| page_res.clone());
    let diag = doc.diagnostics();
    let writer = Writer::new(diag, opts.ascii, opts.newlines);
    let procs = match doc.dict_get(font, b"CharProcs") {
        Object::Dict(d) => d.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let new_res = if opts.sanitize {
        let mut proc = Sanitize::new(doc, writer, -1, Matrix::IDENTITY, *opts, hooks, &cycle);
        proc.push_resources(in_res.clone());
        for (i, val) in procs.iter().enumerate() {
            if i > 0 {
                proc.reset();
            }
            content::process_raw_contents(doc, val, &mut proc)?;
            proc.close();
            // Written back at once: a procedure two glyphs share is read
            // the second time as the first rewrote it, as in MuPDF.
            if let Some(r) = val.as_ref() {
                update_stream(doc, r.num, proc.output().to_vec(), |_| {})?;
            }
        }
        proc.pop_resources()
    } else {
        let mut proc = Plain {
            doc,
            writer,
            state: ProcState::default(),
        };
        proc.state.push_resources(in_res.clone());
        for (i, val) in procs.iter().enumerate() {
            if i > 0 {
                proc.writer.reset();
            }
            content::process_raw_contents(doc, val, &mut proc)?;
            proc.writer.close();
            if let Some(r) = val.as_ref() {
                update_stream(doc, r.num, proc.writer.out.clone(), |_| {})?;
            }
        }
        in_res
    };
    let put = |d: &mut Dict| d.put(b"Resources", new_res.clone());
    match font {
        Object::Ref(r) => {
            if let Object::Dict(d) = doc.resolve(font) {
                let mut d = (*d).clone();
                put(&mut d);
                doc.alter_object(r.num, Object::Dict(Arc::new(d)));
            }
            Ok(None)
        }
        Object::Dict(d) => {
            let mut d = (**d).clone();
            put(&mut d);
            Ok(Some(Object::Dict(Arc::new(d))))
        }
        _ => Ok(None),
    }
}

/// [`crate::edit::edit_page`], for a page object that a direct form in its
/// resources made a stream: MuPDF's page is still a dictionary, with data.
fn edit_page(doc: &Document, index: i32, f: &mut dyn FnMut(&mut Dict)) -> Result<(), String> {
    let page = doc.page(index).map_err(|e| e.0)?;
    if let (Some(r), Object::Stream(s)) = (page.as_ref(), doc.resolve(&page)) {
        let mut dict = s.dict.clone();
        f(&mut dict);
        let data = doc.raw_stream_data(&s)?;
        doc.replace_stream(r.num, dict, data.into_owned());
        return Ok(());
    }
    crate::edit::edit_page(doc, index, f)
}

/// The object the page's /Resources sit in: the page, or the nearest
/// parent that has them, or the object a direct node sits in (0 for a
/// direct page, whose holder is not known here).
fn resources_holder(doc: &Document, page: &Object) -> i32 {
    let mut node = page.clone();
    let mut holder = holder_of(page, 0);
    // Deeper trees are cycles; `inherited` has reported them.
    for _ in 0..64 {
        let d = match doc.resolve(&node) {
            Object::Dict(d) => (*d).clone(),
            Object::Stream(s) => s.dict.clone(),
            _ => return 0,
        };
        if d.get(b"Resources").is_some() {
            return holder;
        }
        match d.get(b"Parent") {
            Some(p) => {
                holder = holder_of(p, holder);
                node = p.clone();
            }
            None => return 0,
        }
    }
    0
}

/// `pdf_filter_page_contents`: the page's contents become one new stream,
/// its resources the ones the contents use.
pub fn filter_page_contents(
    doc: &Document,
    index: i32,
    opts: &Options,
    hooks: &mut dyn Hooks,
) -> Result<(), String> {
    let line = |e: crate::page::PageError| crate::diag::report::line(&e.0);
    let page = doc.page(index).map_err(line)?;
    let sp = struct_parents(doc, &page);
    let contents = stored(doc, &page, b"Contents");
    let old_res = doc.inherited(&page, b"Resources").map_err(line)?;
    let out = filter_content_stream(
        doc,
        contents.as_ref(),
        old_res.as_ref(),
        resources_holder(doc, &page),
        Matrix::IDENTITY,
        opts,
        hooks,
        sp,
        &Cycle::default(),
    )?;
    let mut data = out.data;
    hooks.complete(&mut data);
    let num = doc.add_object(Object::Dict(Arc::new(Dict::new())));
    let resources = out.resources.unwrap_or(Object::Null);
    edit_page(doc, index, &mut |d| {
        d.put(b"Contents", Object::Ref(ObjRef::new(num, 0)));
    })?;
    update_stream(doc, num, data, |_| {})?;
    edit_page(doc, index, &mut |d| d.put(b"Resources", resources.clone()))?;
    Ok(())
}

/// `pdf_filter_annot_contents`: each appearance stream of the annotation.
pub fn filter_annot_contents(
    doc: &Document,
    annot: &Object,
    opts: &Options,
    hooks: &mut dyn Hooks,
) -> Result<(), String> {
    let ap = doc.dict_get(annot, b"AP");
    let Object::Dict(ap) = ap else {
        return Ok(());
    };
    for (_, stm) in ap.iter() {
        if matches!(doc.resolve(stm), Object::Stream(_)) {
            filter_xobject(doc, stm, None, 0, opts, hooks, &Cycle::default())?;
        }
    }
    Ok(())
}

/// The annotations `pdf_load_annots` keeps for a page: dictionaries in
/// /Annots that are not links, popups or widgets.
pub fn page_annots(doc: &Document, page: &Object) -> Vec<Object> {
    let Object::Array(annots) = doc.dict_get(page, b"Annots") else {
        return Vec::new();
    };
    annots
        .iter()
        .filter(|a| {
            let st = doc.dict_get(a, b"Subtype");
            doc.resolve(a).as_dict().is_some()
                && !st.is_name(b"Link")
                && !st.is_name(b"Popup")
                && !st.is_name(b"Widget")
        })
        .cloned()
        .collect()
}

/// `clean_content_streams`: every page's contents and annotation
/// appearances rewritten, as `mutool clean -c` (and `-s`, with
/// `sanitize`) does before saving.
pub fn clean_content_streams(
    doc: &Document,
    sanitize: bool,
    ascii: bool,
    newlines: bool,
) -> Result<(), String> {
    let opts = Options {
        recurse: true,
        instance_forms: false,
        ascii,
        newlines,
        sanitize,
    };
    let n = doc
        .page_count()
        .map_err(|e| crate::diag::report::line(&e.0))?;
    let mut hooks = crate::content_filter::NoHooks;
    for i in 0..n {
        filter_page_contents(doc, i, &opts, &mut hooks)?;
        let page = doc.page(i).map_err(|e| crate::diag::report::line(&e.0))?;
        for annot in page_annots(doc, &page) {
            filter_annot_contents(doc, &annot, &opts, &mut hooks)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A one-page document whose contents are `content`, with Helvetica
    /// as /F1 and /F2.
    pub(crate) fn page_doc(content: &[u8]) -> Document {
        let objs: Vec<Vec<u8>> = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R/F2 5 0 R>>>>/Contents 4 0 R>>".to_vec(),
            [
                format!("<</Length {}>>stream\n", content.len()).as_bytes(),
                content,
                b"\nendstream",
            ]
            .concat(),
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_vec(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(o);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes(),
        );
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        Document::open(out).unwrap_or_else(|e| panic!("{e:?}"))
    }

    /// The page's contents and resources, as rewritten.
    pub(crate) fn page_contents(doc: &Document) -> (String, String) {
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e}"));
        let Object::Stream(s) = doc.resolve(&doc.dict_get(&page, b"Contents")) else {
            panic!("no contents stream");
        };
        let data = doc.stream_data(&s).unwrap_or_else(|e| panic!("{e:?}"));
        let res = crate::print::to_string(
            &doc.dict_get(&page, b"Resources"),
            crate::print::Options::default(),
        );
        (String::from_utf8_lossy(&data).into_owned(), res)
    }

    #[test]
    fn sanitize_sends_only_the_state_that_is_used() {
        // Golden output from `mutool clean -s`.
        let doc = page_doc(b"q 1 0 0 1 0 0 cm 0 g 0 g 1 w BT /F1 12 Tf 72 700 Td (Hello) Tj 0 -14 Td (World) Tj ET Q 0 0 1 rg 10 10 50 50 re f");
        clean_content_streams(&doc, true, false, false).unwrap_or_else(|e| panic!("{e}"));
        let (data, res) = page_contents(&doc);
        assert_eq!(
            data,
            "q 0 g BT/F1 12 Tf 72 700 TD (Hello)Tj 0 -14 TD (World)Tj ET Q q 0 0 1 rg 10 10 50 50 re f Q"
        );
        // Only the font the text used.
        assert_eq!(res, "<</Font<</F1 5 0 R>>>>");
    }

    #[test]
    fn clean_rewrites_operators_as_they_are() {
        let doc = page_doc(b"q 1 0 0 1 0 0 cm 0 g 0 g BT /F1 12 Tf (Hi) Tj ET");
        clean_content_streams(&doc, false, false, false).unwrap_or_else(|e| panic!("{e}"));
        let (data, res) = page_contents(&doc);
        // The q the stream leaves open is closed.
        assert_eq!(data, "q 1 0 0 1 0 0 cm 0 g 0 g BT/F1 12 Tf (Hi)Tj ET Q");
        assert_eq!(res, "<</Font<</F1 5 0 R/F2 5 0 R>>>>");
    }

    #[test]
    fn text_before_any_bt_moves_nothing() {
        // MuPDF's filter starts with zeroed text matrices, so a TJ outside
        // a text object loses its adjustments.
        let doc = page_doc(b"/F1 10 Tf\n100 700 Td\n[(A)-50(B)] TJ");
        clean_content_streams(&doc, true, false, false).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(page_contents(&doc).0, "q[(A)(B)]TJ Q");
    }

    // The cases below are pinned to MuPDF 1.28.6: each document as
    // `crate::testutil::page_doc` builds it, through `mutool clean -d -c`
    // or `-d -s` (one operator a line), every page's new contents and
    // resources read back.

    use crate::testutil::{doc_bytes, stream};

    const HELVETICA: &[u8] = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>";

    /// The one-page document of `content` with `resources`, objects from 5.
    fn case(content: &str, resources: &str, extra: &[Vec<u8>]) -> Document {
        crate::testutil::page_doc(content.as_bytes(), resources, extra)
    }

    /// The page's contents and resources after `mutool clean -d -c`
    /// (`sanitize` false) or `-d -s`.
    fn cleaned(doc: &Document, sanitize: bool) -> (String, String) {
        clean_content_streams(doc, sanitize, false, true).unwrap_or_else(|e| panic!("{e}"));
        page_contents(doc)
    }

    /// Object `num` as MuPDF's `toString` prints it, and its data.
    fn object(doc: &Document, num: i32) -> (String, String) {
        let obj = doc.resolve(&Object::Ref(ObjRef::new(num, 0)));
        let opts = crate::print::Options::default();
        match &obj {
            Object::Stream(s) => {
                let data = doc.stream_data(s).unwrap_or_else(|e| panic!("{e:?}"));
                let dict = crate::print::to_string(&Object::Dict(Arc::new(s.dict.clone())), opts);
                (dict, String::from_utf8_lossy(&data).into_owned())
            }
            o => (crate::print::to_string(o, opts), String::new()),
        }
    }

    fn warnings(doc: &Document) -> Vec<String> {
        doc.diagnostics()
            .snapshot()
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    const OPS: &str = r#"2 w 1 j 1 J 4 M [3 1] 0 d /RelativeColorimetric ri 1 i 0 0 m 10 10 l 20 20 30 30 40 40 c 50 50 60 60 v 70 70 80 80 y h S /G1 gs 1 0 0 RG 0 0 10 10 re s 0 1 0 rg 0 0 5 5 re F 0 0 5 5 re f* 0 0 5 5 re B 0 0 5 5 re B* 0 0 5 5 re b 0 0 5 5 re b* q 0 0 100 100 re W n 0 0 0 1 k 1 2 3 4 re f Q q 0 0 100 100 re W* n 0.5 G 0.5 g 1 2 3 4 re B Q BT /F1 12 Tf 2 Tc 3 Tw 90 Tz 14 TL 1 Tr 2 Ts 10 10 Td 5 5 TD 1 0 0 1 50 50 Tm T* (a) Tj (b) ' 1 2 (c) " ET BX /Foo EX /P MP /P <</A 1>> DP /Span BMC EMC /Span <</MCID 0>> BDC EMC 10 0 d0 10 0 0 0 10 10 d1"#;
    const OPS_RES: &str = "/Font<</F1 5 0 R>>/ExtGState<</G1<</LW 3/CA .5/ca .25/LC 1/LJ 2/ML 5/D[[2] 1]/RI/Saturation/FL 2/BM/Multiply/SMask/None/OP true/op false/OPM 1/UseBlackPtComp/ON>>>>";

    #[test]
    fn every_operator_is_written_back_one_per_line() {
        let doc = case(OPS, OPS_RES, &[HELVETICA.to_vec()]);
        let (data, res) = cleaned(&doc, false);
        assert_eq!(
            data,
            "2 w\n1 j\n1 J\n4 M\n[3 1]0 d\n/RelativeColorimetric ri\n1 i\n0 0 m\n10 10 l\n20 20 30 30 40 40 c\n50 50 60 60 v\n70 70 80 80 y\nh\nS\n/G1 gs\n1 0 0 RG\n0 0 10 10 re\ns\n0 1 0 rg\n0 0 5 5 re\nF\n0 0 5 5 re\nf*\n0 0 5 5 re\nB\n0 0 5 5 re\nB*\n0 0 5 5 re\nb\n0 0 5 5 re\nb*\nq\n0 0 100 100 re\nW\nn\n0 0 0 1 k\n1 2 3 4 re\nf\nQ\nq\n0 0 100 100 re\nW*\nn\n.5 G\n.5 g\n1 2 3 4 re\nB\nQ\nBT\n/F1 12 Tf\n2 Tc\n3 Tw\n90 Tz\n14 TL\n1 Tr\n2 Ts\n10 10 Td\n5 5 TD\n1 0 0 1 50 50 Tm\nT*\n(a)Tj\n(b)'\n1 2 (c)\"\nET\nBX\nEX\n/P MP\n/P<</A 1>>DP\n/Span BMC\nEMC\n/Span<</MCID 0>>BDC\nEMC\n10 0 d0\n10 0 0 0 10 10 d1\n"
        );
        assert_eq!(
            res,
            "<</Font<</F1 5 0 R>>/ExtGState<</G1<</LW 3/CA .5/ca .25/LC 1/LJ 2/ML 5/D[[2]1]/RI/Saturation/FL 2/BM/Multiply/SMask/None/OP true/op false/OPM 1/UseBlackPtComp/ON>>>>>>"
        );
    }

    #[test]
    fn sanitize_sends_the_state_each_operator_needs() {
        let doc = case(OPS, OPS_RES, &[HELVETICA.to_vec()]);
        let (data, res) = cleaned(&doc, true);
        // The ExtGState's stroke state is sent again after it, empty
        // marked content goes, and text moves are rewritten as TD and T*.
        assert_eq!(
            data,
            "q\n/RelativeColorimetric ri\n1 i\n1 J\n1 j\n2 w\n4 M\n[3 1]0 d\n0 0 m\n10 10 l\n20 20 30 30 40 40 c\n50 50 60 60 v\n70 70 80 80 y\nh\nS\n/G1 gs\n1 0 0 RG\n2 j\n3 w\n5 M\n[2]1 d\n0 0 10 10 re\ns\n0 1 0 rg\n0 0 5 5 re\nF\n0 0 5 5 re\nf*\n0 0 5 5 re\nB\n0 0 5 5 re\nB*\n0 0 5 5 re\nb\n0 0 5 5 re\nb*\nq\n0 0 100 100 re\nW\nn\n0 0 0 1 k\n1 2 3 4 re\nf\nQ\nq\n0 0 100 100 re\nW*\nn\n.5 g\n.5 G\n1 2 3 4 re\nB\nQ\nBT\n2 Tc\n3 Tw\n90 Tz\n/F1 12 Tf\n1 Tr\n2 Ts\n50 55 TD\n(a)Tj\n0 -14 TD\n(b)Tj\n1 Tw\nT*\n(c)Tj\nET\nBX\nEX\n/P MP\n/P<</A 1>>DP\n10 0 d0\n10 0 0 0 10 10 d1\nQ\n"
        );
        assert_eq!(
            res,
            "<</ExtGState<</G1<</LW 3/CA .5/ca .25/LC 1/LJ 2/ML 5/D[[2]1]/RI/Saturation/FL 2/BM/Multiply/SMask/None/OP true/op false/OPM 1/UseBlackPtComp/ON>>>>/Font<</F1 5 0 R>>>>"
        );
    }

    #[test]
    fn operators_are_separated_by_spaces_without_newlines() {
        let doc = case(OPS, OPS_RES, &[HELVETICA.to_vec()]);
        clean_content_streams(&doc, true, false, false).unwrap_or_else(|e| panic!("{e}"));
        // No separator before an array, a name or Tf, as MuPDF writes them.
        assert_eq!(
            page_contents(&doc).0,
            "q /RelativeColorimetric ri 1 i 1 J 1 j 2 w 4 M[3 1]0 d 0 0 m 10 10 l 20 20 30 30 40 40 c 50 50 60 60 v 70 70 80 80 y h S/G1 gs 1 0 0 RG 2 j 3 w 5 M[2]1 d 0 0 10 10 re s 0 1 0 rg 0 0 5 5 re F 0 0 5 5 re f* 0 0 5 5 re B 0 0 5 5 re B* 0 0 5 5 re b 0 0 5 5 re b* q 0 0 100 100 re W n 0 0 0 1 k 1 2 3 4 re f Q q 0 0 100 100 re W* n .5 g .5 G 1 2 3 4 re B Q BT 2 Tc 3 Tw 90 Tz/F1 12 Tf 1 Tr 2 Ts 50 55 TD (a)Tj 0 -14 TD (b)Tj 1 Tw T* (c)Tj ET BX EX /P MP /P<</A 1>>DP 10 0 d0 10 0 0 0 10 10 d1 Q"
        );
    }

    const CLIPPED: &str = r#"q 0 0 0 0 re W n 2 w 1 j 1 J 4 M [1] 0 d /G1 gs 0 0 m 1 1 l 1 1 1 1 1 1 c 1 1 1 1 v 1 1 1 1 y h 0 0 1 1 re S f B n W W* BT /F1 9 Tf 1 Tc 1 Tw 50 Tz 2 TL 1 Tr 1 Ts 1 1 Td 1 1 TD 1 0 0 1 0 0 Tm T* (a) Tj [(b)] TJ (c) ' 1 1 (d) " ET 1 0 0 RG /CS0 cs 1 sc /Pattern cs /P0 scn /P1 scn 1 g 1 k /Sh0 sh /Im0 Do /Fm0 Do BI /W 1 /H 1 /CS /G ID x EI /Span BMC EMC 2 0 0 2 0 0 cm Q 0 0 1 1 re f"#;
    const CLIPPED_RES: &str = r#"/Font<</F1 5 0 R>>/ExtGState<</G1<</CA .5/ca .5>>>>/ColorSpace<</CS0[/Separation/Spot/DeviceCMYK <</FunctionType 2/Domain[0 1]/C0[0 0 0 0]/C1[0 1 0 0]/N 1>>]>>/Pattern<</P0 6 0 R/P1<</PatternType 2/Shading<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 1 0]/Function <</FunctionType 2/Domain[0 1]/C0[1 0 0]/C1[0 0 1]/N 1>>>>>>>>/Shading<</Sh0<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 1 0]/Function <</FunctionType 2/Domain[0 1]/C0[1 0 0]/C1[0 0 1]/N 1>>>>>>/XObject<</Im0 7 0 R/Fm0 8 0 R>>"#;

    fn clipped_doc() -> Document {
        case(
            CLIPPED,
            CLIPPED_RES,
            &[
                HELVETICA.to_vec(),
                stream(
                    "/PatternType 1/PaintType 1/TilingType 1/BBox[0 0 10 10]/XStep 10/YStep 10/Resources<<>>",
                    b"0 0 5 5 re f",
                ),
                stream(
                    "/Subtype/Image/Width 1/Height 1/ColorSpace/DeviceGray/BitsPerComponent 8",
                    b"\x80",
                ),
                stream("/Subtype/Form/BBox[0 0 10 10]", b"0 0 1 1 re f"),
            ],
        )
    }

    #[test]
    fn nothing_inside_an_empty_clip_is_kept() {
        let doc = clipped_doc();
        assert_eq!(
            cleaned(&doc, true),
            (
                "q\n0 0 0 0 re\nW\nn\nQ\nq\n0 0 1 1 re\nf\nQ\n".into(),
                "<<>>".into()
            )
        );
        let doc = clipped_doc();
        assert_eq!(
            cleaned(&doc, false).0,
            "q\n0 0 0 0 re\nW\nn\n2 w\n1 j\n1 J\n4 M\n[1]0 d\n/G1 gs\n0 0 m\n1 1 l\n1 1 1 1 1 1 c\n1 1 1 1 v\n1 1 1 1 y\nh\n0 0 1 1 re\nS\nf\nB\nn\nW\nW*\nBT\n/F1 9 Tf\n1 Tc\n1 Tw\n50 Tz\n2 TL\n1 Tr\n1 Ts\n1 1 Td\n1 1 TD\n1 0 0 1 0 0 Tm\nT*\n(a)Tj\n[(b)]TJ\n(c)'\n1 1 (d)\"\nET\n1 0 0 RG\n/CS0 cs\n1 scn\n/Pattern cs\n/P0 scn\n/P1 scn\n1 g\n1 0 0 0 k\n/Sh0 sh\n/Im0 Do\n/Fm0 Do\nBI /W 1/H 1/BPC 8/CS/G/D[0 1]ID x EI /Span BMC\nEMC\n2 0 0 2 0 0 cm\nQ\n0 0 1 1 re\nf\n"
        );
    }

    const COLORS: &str = r#"/CS0 cs .5 scn 0 0 10 10 re f /CS0 cs .5 scn 0 0 10 10 re f /CS0 CS .3 SCN 0 0 10 10 re S /CS1 cs 1 sc 0 0 10 10 re f /CS2 cs 1 0 0 sc 0 0 10 10 re f /CS3 cs .1 .2 .3 sc 0 0 10 10 re f /Pattern cs /P0 scn 0 0 10 10 re f /P0 scn 0 0 10 10 re f /Pattern CS /P1 SCN 0 0 10 10 re S /CS4 cs 1 0 0 /P2 scn 0 0 10 10 re f /CS4 cs 0 1 0 /P2 scn 0 0 10 10 re f /CS5 cs .2 .4 scn 0 0 1 1 re f /DeviceGray cs .5 sc 0 0 1 1 re f /DeviceCMYK CS 0 0 0 1 SC 0 0 1 1 re S 0 0 0 1 K 0 0 1 1 re S 0 0 1 0 K 0 0 1 1 re S 1 0 0 rg 1 0 0 rg 0 0 1 1 re f 1 0 1 rg 0 0 1 1 re f .5 G 0 0 1 1 re S .5 G 0 0 1 1 re S .25 G 0 0 1 1 re S /Sh0 sh /CS6 cs .5 .5 .5 sc 0 0 1 1 re f /CS7 cs 50 0 0 sc 0 0 1 1 re f /CS8 cs .5 sc 0 0 1 1 re f /DeviceRGB CS 0 0 0 SC 0 0 1 1 re S"#;
    const COLORS_RES: &str = r#"/ColorSpace<</CS0[/Separation/Spot/DeviceCMYK <</FunctionType 2/Domain[0 1]/C0[0 0 0 0]/C1[0 1 0 0]/N 1>>]/CS1[/Indexed/DeviceRGB 1<ff000000ff00>]/CS2/DeviceRGB/CS3[/CalRGB<</WhitePoint[1 1 1]>>]/CS4[/Pattern/DeviceRGB]/CS5[/DeviceN[/A/B]/DeviceCMYK <</FunctionType 2/Domain[0 1 0 1]/C0[0 0 0 0]/C1[1 1 0 0]/N 1>>]/CS6[/DeviceRGB]/CS7[/Lab<</WhitePoint[1 1 1]>>]/CS8[/CalGray<</WhitePoint[1 1 1]>>]>>/Pattern<</P0 5 0 R/P1<</PatternType 2/Shading<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 1 0]/Function <</FunctionType 2/Domain[0 1]/C0[1 0 0]/C1[0 0 1]/N 1>>>>>>/P2 6 0 R>>/Shading<</Sh0<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 100 0]/Function <</FunctionType 2/Domain[0 1]/C0[1 0 0]/C1[0 0 1]/N 1>>>>>>"#;

    fn colors_doc() -> Document {
        let tiling = |paint: u8| {
            stream(
                &format!(
                    "/PatternType 1/PaintType {paint}/TilingType 1/BBox[0 0 10 10]/XStep 10/YStep 10/Resources<<>>"
                ),
                b"0 0 5 5 re f",
            )
        };
        case(COLORS, COLORS_RES, &[tiling(1), tiling(2)])
    }

    #[test]
    fn colors_are_sent_once_and_device_spaces_by_their_operators() {
        let doc = colors_doc();
        let (data, res) = cleaned(&doc, true);
        // A colorspace resource that is a device space is written with
        // rg; a repeated color is not written again.
        assert_eq!(
            data,
            "q\n/CS0 cs\n.5 scn\n0 0 10 10 re\nf\n0 0 10 10 re\nf\n/CS0 CS\n.3 SCN\n0 0 10 10 re\nS\n/CS1 cs\n1 scn\n0 0 10 10 re\nf\n1 0 0 rg\n0 0 10 10 re\nf\n/CS3 cs\n.1 .2 .3 scn\n0 0 10 10 re\nf\n/Pattern cs\n/P0 scn\n0 0 10 10 re\nf\n0 0 10 10 re\nf\n/Pattern CS\n/P1 SCN\n0 0 10 10 re\nS\n/CS4 cs\n1 0 0 /P2 scn\n0 0 10 10 re\nf\n0 1 0 /P2 scn\n0 0 10 10 re\nf\n/CS5 cs\n.2 .4 scn\n0 0 1 1 re\nf\n.5 g\n0 0 1 1 re\nf\n0 0 0 1 K\n0 0 1 1 re\nS\n0 0 1 1 re\nS\n0 0 1 0 K\n0 0 1 1 re\nS\n1 0 0 rg\n0 0 1 1 re\nf\n1 0 1 scn\n0 0 1 1 re\nf\n.5 G\n0 0 1 1 re\nS\n.5 G\n0 0 1 1 re\nS\n.25 G\n0 0 1 1 re\nS\n.25 G\n/Sh0 sh\n.5 .5 .5 rg\n0 0 1 1 re\nf\n/CS7 cs\n50 0 0 scn\n0 0 1 1 re\nf\n/CS8 cs\n.5 scn\n0 0 1 1 re\nf\n0 0 0 RG\n0 0 1 1 re\nS\nQ\n"
        );
        assert!(
            res.starts_with("<</ColorSpace<</CS0[/Separation/Spot/DeviceCMYK<</FunctionType 2")
        );
        assert!(res.contains("/CS1[/Indexed/DeviceRGB 1<FF000000FF00>]/CS2/DeviceRGB/CS3"));
        assert!(res.ends_with("/Shading<</Sh0<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 100 0]/Function<</FunctionType 2/Domain[0 1]/C0[1 0 0]/C1[0 0 1]/N 1>>>>>>>>"));
        let doc = colors_doc();
        assert_eq!(
            cleaned(&doc, false).0,
            "/CS0 cs\n.5 scn\n0 0 10 10 re\nf\n/CS0 cs\n.5 scn\n0 0 10 10 re\nf\n/CS0 CS\n.3 SCN\n0 0 10 10 re\nS\n/CS1 cs\n1 scn\n0 0 10 10 re\nf\n/CS2 cs\n1 0 0 scn\n0 0 10 10 re\nf\n/CS3 cs\n.1 .2 .3 scn\n0 0 10 10 re\nf\n/Pattern cs\n/P0 scn\n0 0 10 10 re\nf\n/P0 scn\n0 0 10 10 re\nf\n/Pattern CS\n/P1 SCN\n0 0 10 10 re\nS\n/CS4 cs\n1 0 0 /P2 scn\n0 0 10 10 re\nf\n/CS4 cs\n0 1 0 /P2 scn\n0 0 10 10 re\nf\n/CS5 cs\n.2 .4 scn\n0 0 1 1 re\nf\n/DeviceGray cs\n.5 scn\n0 0 1 1 re\nf\n/DeviceCMYK CS\n0 0 0 1 SCN\n0 0 1 1 re\nS\n0 0 0 1 K\n0 0 1 1 re\nS\n0 0 1 0 K\n0 0 1 1 re\nS\n1 0 0 rg\n1 0 0 rg\n0 0 1 1 re\nf\n1 0 1 rg\n0 0 1 1 re\nf\n.5 G\n0 0 1 1 re\nS\n.5 G\n0 0 1 1 re\nS\n.25 G\n0 0 1 1 re\nS\n/Sh0 sh\n/CS6 cs\n.5 .5 .5 scn\n0 0 1 1 re\nf\n/CS7 cs\n50 0 0 scn\n0 0 1 1 re\nf\n/CS8 cs\n.5 scn\n0 0 1 1 re\nf\n/DeviceRGB CS\n0 0 0 SCN\n0 0 1 1 re\nS\n"
        );
    }

    const TEXT: &str = r#"BT /F1 12 Tf 72 700 Td (Hello) Tj 0 -14 Td (World) Tj 0 -14 Td (again) Tj 14 TL T* (next) Tj (quote) ' 2 1 (dq) " 1 Tc 2 Tw 80 Tz 3 Ts 2 Tr [(A) 120 (B) -50.5 (C)] TJ 0 1 -1 0 300 300 Tm (rot) Tj 0 0 0 0 10 10 Tm (flat) Tj 1 0 0 1 100 100 Tm ( ) Tj (a b) Tj /F1 12 Tf /F2 10 Tf (two) Tj 7 Tr (clip) Tj ET BT /F3 10 Tf 0 0 Td <00410042> Tj [<0043> 100 <0044>] TJ ET BT /F1 10 Tf 3 Tr (inv) Tj ET BT 0 Tr /F1 10 Tf (\(esc\)\\) Tj <0102ff> Tj ET"#;
    const VERTICAL: &[u8] = b"<</Type/Font/Subtype/Type0/BaseFont/Foo/Encoding/Identity-V/DescendantFonts[<</Type/Font/Subtype/CIDFontType2/BaseFont/Foo/CIDSystemInfo<</Registry(Adobe)/Ordering(Identity)/Supplement 0>>/FontDescriptor<</Type/FontDescriptor/FontName/Foo/Flags 4/FontBBox[0 -200 1000 800]/ItalicAngle 0/Ascent 800/Descent -200/CapHeight 700/StemV 80>>/DW2[880 -1000]/W2[67[-500 300 800]]>>]>>";

    fn text_doc() -> Document {
        case(
            TEXT,
            "/Font<</F1 5 0 R/F2 5 0 R/F3 6 0 R>>",
            &[HELVETICA.to_vec(), VERTICAL.to_vec()],
        )
    }

    #[test]
    fn text_moves_and_adjustments_follow_the_glyphs() {
        let doc = text_doc();
        let (data, res) = cleaned(&doc, true);
        // TJ adjustments are recomputed from the glyph advances (here with
        // 80% scaling); a Tm that cannot be inverted is written as it is.
        assert_eq!(
            data,
            "q\nBT\n/F1 12 Tf\n72 700 TD\n(Hello)Tj\n0 -14 TD\n(World)Tj\nT*\n(again)Tj\nT*\n(next)Tj\nT*\n(quote)Tj\n1 Tc\n2 Tw\nT*\n(dq)Tj\n80 Tz\n2 Tr\n3 Ts\n[(A)76.800029(B)-32.32015(C)]TJ\n0 1 -1 0 300 300 Tm\n(rot)Tj\n0 0 0 0 10 10 Tm\n(flat)Tj\n1 0 0 1 100 100 Tm\n( )Tj\n(a b)Tj\n/F2 10 Tf\n(two)Tj\n7 Tr\n(clip)Tj\nET\nBT\n/F3 10 Tf\n<00410042>Tj\n[<0043>100<0044>]TJ\nET\nBT\n/F1 10 Tf\n3 Tr\n(inv)Tj\nET\nBT\n/F1 10 Tf\n0 Tr\n(\\(esc\\)\\\\)Tj\n<0102ff>Tj\nET\nQ\n"
        );
        assert_eq!(res, "<</Font<</F1 5 0 R/F2 5 0 R/F3 6 0 R>>>>");
        let doc = text_doc();
        assert_eq!(
            cleaned(&doc, false).0,
            "BT\n/F1 12 Tf\n72 700 Td\n(Hello)Tj\n0 -14 Td\n(World)Tj\n0 -14 Td\n(again)Tj\n14 TL\nT*\n(next)Tj\n(quote)'\n2 1 (dq)\"\n1 Tc\n2 Tw\n80 Tz\n3 Ts\n2 Tr\n[(A)120(B)-50.5(C)]TJ\n0 1 -1 0 300 300 Tm\n(rot)Tj\n0 0 0 0 10 10 Tm\n(flat)Tj\n1 0 0 1 100 100 Tm\n( )Tj\n(a b)Tj\n/F1 12 Tf\n/F2 10 Tf\n(two)Tj\n7 Tr\n(clip)Tj\nET\nBT\n/F3 10 Tf\n0 0 Td\n<00410042>Tj\n[<0043>100<0044>]TJ\nET\nBT\n/F1 10 Tf\n3 Tr\n(inv)Tj\nET\nBT\n0 Tr\n/F1 10 Tf\n(\\(esc\\)\\\\)Tj\n<0102ff>Tj\nET\n"
        );
    }

    const SAVES: &str = "Q Q 1 w q q 0 0 1 1 re f Q 2 w q 3 w 0 0 1 1 re S Q Q Q 0 0 1 1 re S q 1 0 0 1 5 5 cm q 2 0 0 2 0 0 cm 0 0 1 1 re f Q 1 0 0 1 0 0 cm 0 0 1 1 re f";

    #[test]
    fn unbalanced_saves_and_restores() {
        // The writer passes extra Qs on with a warning and closes what is
        // left open; the filter balances them itself.
        let doc = case(SAVES, "", &[]);
        assert_eq!(
            cleaned(&doc, false).0,
            "Q\nQ\n1 w\nq\nq\n0 0 1 1 re\nf\nQ\n2 w\nq\n3 w\n0 0 1 1 re\nS\nQ\nQ\nQ\n0 0 1 1 re\nS\nq\n1 0 0 1 5 5 cm\nq\n2 0 0 2 0 0 cm\n0 0 1 1 re\nf\nQ\n1 0 0 1 0 0 cm\n0 0 1 1 re\nf\n"
        );
        assert!(warnings(&doc).contains(&"gstate underflow (too many Q operators)".to_string()));
        let doc = case(SAVES, "", &[]);
        assert_eq!(
            cleaned(&doc, true),
            (
                "q\nQ\nq\nQ\nq\nq\n0 0 1 1 re\nf\nQ\nq\nq\n3 w\n0 0 1 1 re\nS\nQ\nQ\nQ\nq\n0 0 1 1 re\nS\nq\n2 0 0 2 5 5 cm\n0 0 1 1 re\nf\nQ\nq\n1 0 0 1 5 5 cm\n0 0 1 1 re\nf\nQ\nQ\n".into(),
                "<<>>".into()
            )
        );
        assert!(!doc.diagnostics().has("content.gstate_underflow"));
    }

    const BAD: &str = "1 2 3 4 5 6 7 rg 0 0 1 1 re f (x) w /F9 12 Tf BT (lost) Tj ET 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 m /Nope gs /Nope Do /Nope sh /Nope cs /Nope scn /Pattern cs /Nope scn 0 0 1 1 re f";

    #[test]
    fn operators_with_bad_operands_or_missing_resources() {
        let doc = case(BAD, "", &[]);
        assert_eq!(
            cleaned(&doc, false),
            (
                "1 2 3 rg\n0 0 1 1 re\nf\n0 w\n/F9 12 Tf\nBT\n(lost)Tj\nET\n0 0 m\n/Pattern cs\n0 0 1 1 re\nf\n".into(),
                "<<>>".into()
            )
        );
        let seen = warnings(&doc);
        for w in [
            "syntax error: cannot find ColorSpace resource 'Nope'",
            "syntax error: cannot find ExtGState resource 'Nope'",
            "syntax error: cannot find Pattern resource 'Nope'",
            "syntax error: cannot find Shading resource 'Nope'",
            "syntax error: cannot find XObject resource 'Nope'",
            "syntax error: stack overflow",
        ] {
            assert!(seen.iter().any(|s| s == w), "{w} in {seen:?}");
        }
        let doc = case(BAD, "", &[]);
        assert_eq!(
            cleaned(&doc, true).0,
            "q\n1 2 3 rg\n0 0 1 1 re\nf\nBT\n/F9 12 Tf\n(lost)Tj\nET\n/Pattern cs\n0 0 1 1 re\nf\nQ\n"
        );
    }

    const MARKED: &str = "EMC /P <</MCID 0>> BDC BT /F1 10 Tf 10 10 Td (Hi there) Tj ET EMC /Span <</MCID 1>> BDC /Inner BMC 0 0 1 1 re f EMC EMC /OC /L1 BDC 0 0 1 1 re f EMC /Empty BMC EMC /Artifact BMC /Nested <</MCID 9>> BDC EMC EMC /P <</MCID 2>> BDC BT /F1 10 Tf (No) Tj ET EMC /P BMC /X <</A 1>> BDC 0 0 1 1 re f";

    /// The MARKED page with a structure tree: its marked content 0 to 2
    /// are objects 8 to 10.
    fn marked_doc() -> Document {
        doc_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R/StructTreeRoot 6 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>/Properties<</L1<</Type/OCG/Name(L)>>>>>>/Contents 4 0 R/StructParents 0>>".to_vec(),
            stream("", MARKED.as_bytes()),
            HELVETICA.to_vec(),
            b"<</Type/StructTreeRoot/ParentTree<</Nums[0 7 0 R]>>>>".to_vec(),
            b"[8 0 R 9 0 R 10 0 R]".to_vec(),
            b"<</S/P/Alt(Hi there)/ActualText(Hi  there)/E(x)/T(t)>>".to_vec(),
            b"<</S/Span/Alt(nothing)>>".to_vec(),
            b"<</S/P/Alt(Yes)>>".to_vec(),
        ])
    }

    #[test]
    fn marked_content_with_nothing_drawn_goes() {
        let doc = marked_doc();
        assert_eq!(
            cleaned(&doc, true),
            (
                "/P<</MCID 0>>BDC\nq\nBT\n/F1 10 Tf\n10 10 TD\n(Hi there)Tj\nET\nEMC\n/Span<</MCID 1>>BDC\n/Inner BMC\n0 0 1 1 re\nf\nEMC\nEMC\n/OC/L1 BDC\n0 0 1 1 re\nf\nEMC\n/P<</MCID 2>>BDC\nBT\n/F1 10 Tf\n(No)Tj\nET\nEMC\n/P BMC\n/X<</A 1>>BDC\n0 0 1 1 re\nf\nQ\n".into(),
                "<</Font<</F1 5 0 R>>/Properties<</L1<</Type/OCG/Name(L)>>>>>>".into()
            )
        );
        // Nothing was removed, so the structure is as it was.
        assert_eq!(
            object(&doc, 8).0,
            "<</S/P/Alt(Hi there)/ActualText(Hi  there)/E(x)/T(t)>>"
        );
        let doc = marked_doc();
        assert_eq!(
            cleaned(&doc, false).0,
            "EMC\n/P<</MCID 0>>BDC\nBT\n/F1 10 Tf\n10 10 Td\n(Hi there)Tj\nET\nEMC\n/Span<</MCID 1>>BDC\n/Inner BMC\n0 0 1 1 re\nf\nEMC\nEMC\n/OC/L1 BDC\n0 0 1 1 re\nf\nEMC\n/Empty BMC\nEMC\n/Artifact BMC\n/Nested<</MCID 9>>BDC\nEMC\nEMC\n/P<</MCID 2>>BDC\nBT\n/F1 10 Tf\n(No)Tj\nET\nEMC\n/P BMC\n/X<</A 1>>BDC\n0 0 1 1 re\nf\n"
        );
    }

    const FORMS: &str = "q 2 0 0 2 10 10 cm /Fm0 Do Q /Fm1 Do /Fm2 Do /Fm3 Do /Im0 Do /Im1 Do /Fd Do /Pattern cs /P0 scn 0 0 5 5 re f /G1 gs 0 0 1 1 re f BT /T3 10 Tf (abba) Tj /T4 10 Tf (b) Tj ET";

    /// Forms with and without resources of their own (5 to 8, 8 drawing
    /// itself), an image and one that does not load (9, 10), a tiling
    /// pattern (11), a soft mask group (12), Type 3 fonts (13, and one held
    /// directly in the font dictionary 17) whose glyphs share a procedure,
    /// and a form held directly in the XObject dictionary 18.
    fn forms_doc() -> Document {
        let form =
            |dict: &str, data: &[u8]| stream(&format!("/Subtype/Form/BBox[0 0 10 10]{dict}"), data);
        let image = |cs: &str| {
            stream(
                &format!("/Subtype/Image/Width 1/Height 1/ColorSpace/{cs}/BitsPerComponent 8"),
                b"\x80",
            )
        };
        case(
            FORMS,
            "/XObject 18 0 R/Pattern<</P0 11 0 R>>/ExtGState<</G1<</SMask<</S/Luminosity/G 12 0 R>>>>>>/Font 17 0 R",
            &[
                form(
                    "/Resources<</ExtGState<</A<</CA .5>>>>/Font<</F1 16 0 R>>>>",
                    b"/A gs 1 0 0 rg 0 0 5 5 re f 0 0 5 5 re f BT /F1 5 Tf (f) Tj ET",
                ),
                form("", b"0 g 0 0 1 1 re f /Im0 Do"),
                form("/Resources<</XObject<</In 5 0 R>>>>/Matrix[1 0 0 1 3 3]", b"/In Do"),
                form("/Resources<</XObject<</Me 8 0 R>>>>", b"/Me Do 0 0 1 1 re f"),
                image("DeviceGray"),
                image("Nope"),
                stream(
                    "/PatternType 1/PaintType 1/TilingType 1/BBox[0 0 10 10]/XStep 10/YStep 10/Resources<</XObject<</X 6 0 R>>>>",
                    b"1 0 0 rg 0 0 5 5 re f /X Do",
                ),
                form(
                    "/Group<</S/Transparency/CS/DeviceGray>>",
                    b".5 g 0 0 10 10 re f .5 g",
                ),
                b"<</Type/Font/Subtype/Type3/FontBBox[0 0 10 10]/FontMatrix[.1 0 0 .1 0 0]/FirstChar 97/LastChar 98/Widths[10 10]/Encoding<</Differences[97/a/b]>>/CharProcs<</a 14 0 R/b 14 0 R>>/Resources<</ExtGState<</Q<</LW 2>>>>>>>>".to_vec(),
                stream("", b"10 0 d0 /Q gs 0 0 5 5 re f 0 0 5 5 re f"),
                stream("", b"10 0 0 0 10 10 d1 0 0 5 5 re f"),
                HELVETICA.to_vec(),
                b"<</T3 13 0 R/T4<</Type/Font/Subtype/Type3/FontBBox[0 0 10 10]/FontMatrix[.1 0 0 .1 0 0]/FirstChar 98/LastChar 98/Widths[10]/Encoding<</Differences[98/b]>>/CharProcs<</b 15 0 R>>>>/H 16 0 R>>".to_vec(),
                b"<</Fm0 5 0 R/Fm1 6 0 R/Fm2 7 0 R/Fm3 8 0 R/Im0 9 0 R/Im1 10 0 R/Fd<</Subtype/Form/BBox[0 0 1 1]/Resources<<>>/Filter/AHx>>>>".to_vec(),
            ],
        )
    }

    const T4: &str = "/Type/Font/Subtype/Type3/FontBBox[0 0 10 10]/FontMatrix[.1 0 0 .1 0 0]/FirstChar 98/LastChar 98/Widths[10]/Encoding<</Differences[98/b]>>/CharProcs<</b 15 0 R>>";

    #[test]
    fn forms_patterns_groups_and_glyphs_are_rewritten_in_place() {
        let doc = forms_doc();
        let (data, res) = cleaned(&doc, false);
        assert_eq!(
            data,
            "q\n2 0 0 2 10 10 cm\n/Fm0 Do\nQ\n/Fm1 Do\n/Fm2 Do\n/Fm3 Do\n/Im0 Do\n/Fd Do\n/Pattern cs\n/P0 scn\n0 0 5 5 re\nf\n/G1 gs\n0 0 1 1 re\nf\nBT\n/T3 10 Tf\n(abba)Tj\n/T4 10 Tf\n(b)Tj\nET\n"
        );
        assert_eq!(
            res,
            "<</XObject 18 0 R/Pattern<</P0 11 0 R>>/ExtGState<</G1<</SMask<</S/Luminosity/G 12 0 R>>>>>>/Font 17 0 R>>"
        );
        // The image that does not load loses its Do.
        assert!(warnings(&doc).contains(&"syntax error: unknown colorspace: Nope".to_string()));
        let expect = [
            (
                5,
                "<</Subtype/Form/BBox[0 0 10 10]/Resources<</ExtGState<</A<</CA .5>>>>/Font<</F1 16 0 R>>>>/Length 62>>",
                "/A gs\n1 0 0 rg\n0 0 5 5 re\nf\n0 0 5 5 re\nf\nBT\n/F1 5 Tf\n(f)Tj\nET\n",
            ),
            // Without resources of its own, a form takes those of what
            // draws it first: here the pattern.
            (
                6,
                "<</Subtype/Form/BBox[0 0 10 10]/Length 17/Resources<</XObject<</X 6 0 R>>>>>>",
                "0 g\n0 0 1 1 re\nf\n",
            ),
            (
                7,
                "<</Subtype/Form/BBox[0 0 10 10]/Resources<</XObject<</In 5 0 R>>>>/Matrix[1 0 0 1 3 3]/Length 7>>",
                "/In Do\n",
            ),
            (
                8,
                "<</Subtype/Form/BBox[0 0 10 10]/Resources<</XObject<</Me 8 0 R>>>>/Length 20>>",
                "/Me Do\n0 0 1 1 re\nf\n",
            ),
            (
                11,
                "<</PatternType 1/PaintType 1/TilingType 1/BBox[0 0 10 10]/XStep 10/YStep 10/Resources<</XObject<</X 6 0 R>>>>/Length 28>>",
                "1 0 0 rg\n0 0 5 5 re\nf\n/X Do\n",
            ),
            (
                12,
                "<</Subtype/Form/BBox[0 0 10 10]/Group<</S/Transparency/CS/DeviceGray>>/Length 25/Resources<</XObject 18 0 R/Pattern<</P0 11 0 R>>/ExtGState<</G1<</SMask<</S/Luminosity/G 12 0 R>>>>>>/Font 17 0 R>>>>",
                ".5 g\n0 0 10 10 re\nf\n.5 g\n",
            ),
            (
                14,
                "<</Length 40>>",
                "10 0 d0\n/Q gs\n0 0 5 5 re\nf\n0 0 5 5 re\nf\n",
            ),
            (15, "<</Length 31>>", "10 0 0 0 10 10 d1\n0 0 5 5 re\nf\n"),
        ];
        for (num, dict, data) in expect {
            assert_eq!(
                object(&doc, num),
                (dict.to_string(), data.to_string()),
                "object {num}"
            );
        }
        // The Type 3 font held directly takes the page's resources.
        assert_eq!(
            object(&doc, 17).0,
            format!(
                "<</T3 13 0 R/T4<<{T4}/Resources<</XObject 18 0 R/Pattern<</P0 11 0 R>>/ExtGState<</G1<</SMask<</S/Luminosity/G 12 0 R>>>>>>/Font 17 0 R>>>>/H 16 0 R>>"
            )
        );
        // The direct form loses its filter, and its (empty) data goes to
        // the dictionary that holds it.
        assert_eq!(
            object(&doc, 18),
            (
                "<</Fm0 5 0 R/Fm1 6 0 R/Fm2 7 0 R/Fm3 8 0 R/Im0 9 0 R/Im1 10 0 R/Fd<</Subtype/Form/BBox[0 0 1 1]/Resources<<>>/Length 0>>>>".into(),
                String::new()
            )
        );
    }

    #[test]
    fn sanitized_forms_get_the_resources_they_use() {
        let doc = forms_doc();
        let (data, res) = cleaned(&doc, true);
        assert_eq!(
            data,
            "q\n2 0 0 2 10 10 cm\n/Fm0 Do\nQ\nq\n/Fm1 Do\n/Fm2 Do\n/Fm3 Do\n/Im0 Do\n/Fd Do\n/Pattern cs\n/P0 scn\n0 0 5 5 re\nf\n/G1 gs\n0 0 1 1 re\nf\nBT\n/T3 10 Tf\n(abba)Tj\n/T4 10 Tf\n(b)Tj\nET\nQ\n"
        );
        assert_eq!(
            res,
            format!(
                "<</XObject<</Fm0 5 0 R/Fm1 6 0 R/Fm2 7 0 R/Fm3 8 0 R/Im0 9 0 R/Fd<</Subtype/Form/BBox[0 0 1 1]/Resources<<>>/Length 4>>>>/Pattern<</P0 11 0 R>>/ExtGState<</G1<</SMask<</S/Luminosity/G 12 0 R>>>>>>/Font<</T3 13 0 R/T4<<{T4}/Resources<<>>>>>>>>"
            )
        );
        let expect = [
            (
                5,
                "<</Subtype/Form/BBox[0 0 10 10]/Resources<</ExtGState<</A<</CA .5>>>>/Font<</F1 16 0 R>>>>/Length 70>>",
                "q\n/A gs\n1 0 0 rg\n0 0 5 5 re\nf\n0 0 5 5 re\nf\nBT\n/F1 5 Tf\n(f)Tj\nET\nQ\nq\nQ\n",
            ),
            (
                6,
                "<</Subtype/Form/BBox[0 0 10 10]/Length 25/Resources<<>>>>",
                "q\n0 g\n0 0 1 1 re\nf\nQ\nq\nQ\n",
            ),
            (
                7,
                "<</Subtype/Form/BBox[0 0 10 10]/Resources<</XObject<</In 5 0 R>>>>/Matrix[1 0 0 1 3 3]/Length 11>>",
                "q\n/In Do\nQ\n",
            ),
            (
                8,
                "<</Subtype/Form/BBox[0 0 10 10]/Resources<</XObject<</Me 8 0 R>>>>/Length 24>>",
                "q\n/Me Do\n0 0 1 1 re\nf\nQ\n",
            ),
            (
                11,
                "<</PatternType 1/PaintType 1/TilingType 1/BBox[0 0 10 10]/XStep 10/YStep 10/Resources<</XObject<</X 6 0 R>>>>/Length 32>>",
                "q\n1 0 0 rg\n0 0 5 5 re\nf\n/X Do\nQ\n",
            ),
            (
                12,
                "<</Subtype/Form/BBox[0 0 10 10]/Group<</S/Transparency/CS/DeviceGray>>/Length 24/Resources<<>>>>",
                "q\n.5 g\n0 0 10 10 re\nf\nQ\n",
            ),
            // The procedure two glyphs share is read the second time as the
            // first rewrote it.
            (
                14,
                "<</Length 48>>",
                "q\n10 0 d0\n/Q gs\n0 0 5 5 re\nf\n0 0 5 5 re\nf\nQ\nq\nQ\n",
            ),
            (
                15,
                "<</Length 35>>",
                "q\n10 0 0 0 10 10 d1\n0 0 5 5 re\nf\nQ\n",
            ),
        ];
        for (num, dict, data) in expect {
            assert_eq!(
                object(&doc, num),
                (dict.to_string(), data.to_string()),
                "object {num}"
            );
        }
        // MuPDF's direct font and form are the objects the old resources
        // hold too, so those change with them.
        assert_eq!(
            object(&doc, 17).0,
            format!("<</T3 13 0 R/T4<<{T4}/Resources<<>>>>/H 16 0 R>>")
        );
        assert_eq!(
            object(&doc, 18),
            (
                "<</Fm0 5 0 R/Fm1 6 0 R/Fm2 7 0 R/Fm3 8 0 R/Im0 9 0 R/Im1 10 0 R/Fd<</Subtype/Form/BBox[0 0 1 1]/Resources<<>>/Length 4>>>>".into(),
                "q\nQ\n".into()
            )
        );
    }

    #[test]
    fn a_direct_form_gives_its_data_to_the_page_that_holds_it() {
        // MuPDF's pdf_update_stream on a direct dictionary updates the
        // object it sits in: here the page, which becomes a stream (its
        // /Length comes when it is written).
        let fd = "/XObject<</Fd<</Subtype/Form/BBox[0 0 1 1]/Resources<<>>>>>>";
        let doc = case("/Fd Do", fd, &[]);
        assert_eq!(
            cleaned(&doc, true),
            (
                "q\n/Fd Do\nQ\n".into(),
                "<</XObject<</Fd<</Subtype/Form/BBox[0 0 1 1]/Resources<<>>/Length 4>>>>>>".into()
            )
        );
        assert_eq!(
            object(&doc, 3),
            (
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</XObject<</Fd<</Subtype/Form/BBox[0 0 1 1]/Resources<<>>/Length 4>>>>>>/Contents 5 0 R>>".into(),
                "q\nQ\n".into()
            )
        );
        assert!(warnings(&doc).contains(&"content stream is not a stream (0 0 R)".to_string()));
        let doc = case("/Fd Do", fd, &[]);
        assert_eq!(cleaned(&doc, false).0, "/Fd Do\n");
        assert_eq!(
            object(&doc, 3),
            (
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</XObject<</Fd<</Subtype/Form/BBox[0 0 1 1]/Resources<<>>/Length 0>>>>>>/Contents 5 0 R>>".into(),
                String::new()
            )
        );
        // Cleaned again, the page is read as the stream it now is.
        clean_content_streams(&doc, true, false, true).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(object(&doc, 3).1, "q\nQ\n");
    }

    #[test]
    fn a_direct_type3_font_changes_where_the_old_resources_hold_it() {
        let doc = crate::testutil::doc_of(&[
            "<</Type/Catalog/Pages 2 0 R>>",
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font 6 0 R>>/Contents 4 0 R>>",
            "<</Length 22>>stream\nBT /T4 10 Tf (b) Tj ET\nendstream",
            "<</Length 20>>stream\n10 0 d0 0 0 1 1 re f\nendstream",
            "<</T4 <</Type/Font/Subtype/Type3/FontBBox[0 0 10 10]/FontMatrix[.1 0 0 .1 0 0]/FirstChar 98/LastChar 98/Widths[10]/Encoding<</Differences[98/b]>>/CharProcs<</b 5 0 R>>>>>>",
        ]);
        let font = "/Type/Font/Subtype/Type3/FontBBox[0 0 10 10]/FontMatrix[.1 0 0 .1 0 0]/FirstChar 98/LastChar 98/Widths[10]/Encoding<</Differences[98/b]>>/CharProcs<</b 5 0 R>>/Resources<<>>";
        assert_eq!(
            cleaned(&doc, true),
            (
                "q\nBT\n/T4 10 Tf\n(b)Tj\nET\nQ\n".into(),
                format!("<</Font<</T4<<{font}>>>>>>")
            )
        );
        assert_eq!(object(&doc, 5).1, "q\n10 0 d0\n0 0 1 1 re\nf\nQ\n");
        assert_eq!(object(&doc, 6).0, format!("<</T4<<{font}>>>>"));
    }

    /// The page's contents, as bytes.
    fn page_bytes(doc: &Document) -> Vec<u8> {
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e}"));
        let Object::Stream(s) = doc.resolve(&doc.dict_get(&page, b"Contents")) else {
            panic!("no contents stream");
        };
        doc.stream_data(&s).unwrap_or_else(|e| panic!("{e:?}"))
    }

    const INLINE_RES: &str = "/ColorSpace<</CS0[/Indexed/DeviceRGB 1<ff000000ff00>]/CS1/DeviceRGB/CS2[/CalGray<</WhitePoint[1 1 1]>>]>>";

    #[test]
    fn inline_images_keep_their_last_image_filter() {
        // Each image's dictionary and data; what mutool clean -d -c writes,
        // and what -d -s -a does. The filters before an image filter are
        // decoded, the image filter's parameters written as
        // build_compression_params reads them, and data a filter cannot
        // read is cut where it stopped.
        #[allow(clippy::type_complexity)]
        let cases: &[(&str, &[u8], &[u8], &[u8])] = &[
            (
                "/W 2 /H 2 /CS /G /F /Fl /DP <</Predictor 2 /Columns 2>>",
                b"x\x9ccdbf\x01\x00\x00\x18\x00\x0b",
                b"BI /W 2/H 2/BPC 8/CS/G/D[0 1]/F/Fl/DP<</Predictor 2/Columns 2>>ID x\x9ccdbf\x01\x00\x00\x18\x00\x0b EI",
                b"q\nBI /W 2/H 2/BPC 8/CS/G/D[0 1]/F[/AHx/Fl]/DP[null<</Predictor 2/Columns 2>>]ID 789c6364626601000018000b> EI Q\n",
            ),
            (
                "/W 3 /H 1 /CS /G /F /LZW /DP <</EarlyChange 0 /Predictor 12 /Columns 3 /Colors 1 /BitsPerComponent 8>>",
                b"\x80\x00\x00  \x1c\x04",
                b"BI /W 3/H 1/BPC 8/CS/G/D[0 1]/F/LZW/DP<</Predictor 12/Columns 3/EarlyChange 0>>ID \x80\x00\x00  \x1c EI",
                b"q\nBI /W 3/H 1/BPC 8/CS/G/D[0 1]/F[/AHx/LZW]/DP[null<</Predictor 12/Columns 3/EarlyChange 0>>]ID 80000020201c> EI Q\n",
            ),
            (
                "/W 2 /H 1 /CS /G /F /LZW",
                b"\x80\x01@\xd0\x10",
                b"BI /W 2/H 1/BPC 8/CS/G/D[0 1]/F/LZW ID \x80\x01@\xd0 EI",
                b"q\nBI /W 2/H 1/BPC 8/CS/G/D[0 1]/F[/AHx/LZW]ID 800140d0> EI Q\n",
            ),
            (
                "/W 16 /H 4 /IM true /D [1 0] /F /CCF /DP <</K -1 /EncodedByteAlign true /Columns 16 /Rows 4 /EndOfBlock false /BlackIs1 true>>",
                b"&\xa8\xba.\x8b\xa2\xe9E$\x92I*I$\x92T\x92I$\x94\x00@",
                b"BI /W 16/H 4/BPC 1/IM true/D[1 0]/F/CCF/DP<</K -1/Columns 16/Rows 4/EncodedByteAlign true/EndOfBlock false/BlackIs1 true>>ID &\xa8\xba.\x8b\xa2\xe9E$\x92 EI",
                b"q\nBI /W 16/H 4/BPC 1/IM true/D[1 0]/F[/AHx/CCF]/DP[null<</K -1/Columns 16/Rows 4/EncodedByteAlign true/EndOfBlock false/BlackIs1 true>>]ID 26a8ba2e8ba2e9452492> EI Q\n",
            ),
            (
                "/W 16 /H 4 /IM true /F /CCF /DP <</K -1 /EndOfLine true /Columns 16.2>>",
                b"&\xa8\xba.\x8b\xa2\xe9E$\x92I*I$\x92T\x92I$\x94\x00@",
                b"BI /W 16/H 4/BPC 1/IM true/D[0 1]/F/CCF/DP<</K -1/Columns 16/EndOfLine true>>ID &\xa8\xba.\x8b\xa2\xe9E$\x92I*I$\x92T\x92I$\x94\x00@  EI",
                b"q\nBI /W 16/H 4/BPC 1/IM true/D[0 1]/F[/AHx/CCF]/DP[null<</K -1/Columns 16/EndOfLine true>>]ID 26a8ba2e8ba2e9452492492a4924925492492494004020> EI Q\n",
            ),
            (
                "/W 1 /H 1 /CS /RGB /F /DCT /DP <</ColorTransform 0>>",
                b"\xff\xd8\xff\xd9",
                b"BI /W 1/H 1/BPC 8/CS/RGB/D[0 1 0 1 0 1]/F/DCT/DP<</ColorTransform 0>>ID \xff\xd8\xff\xd9 EI",
                b"q\nBI /W 1/H 1/BPC 8/CS/RGB/D[0 1 0 1 0 1]/F[/AHx/DCT]/DP<</ColorTransform 0>>ID ffd8ffd9> EI Q\n",
            ),
            (
                "/W 1 /H 1 /CS /CMYK /F /DCT",
                b"\xff\xd8\xff\xd9",
                b"BI /W 1/H 1/BPC 8/CS/CMYK/D[0 1 0 1 0 1 0 1]/F/DCT ID \xff\xd8\xff\xd9 EI",
                b"q\nBI /W 1/H 1/BPC 8/CS/CMYK/D[0 1 0 1 0 1 0 1]/F[/AHx/DCT]ID ffd8ffd9> EI Q\n",
            ),
            (
                "/W 2 /H 1 /CS /G /F /RL",
                b"\x01\x07\x08\x80",
                b"BI /W 2/H 1/BPC 8/CS/G/D[0 1]/F/RL ID \x01\x07\x08 EI",
                b"q\nBI /W 2/H 1/BPC 8/CS/G/D[0 1]/F[/AHx/RL]ID 010708> EI Q\n",
            ),
            (
                "/W 2 /H 1 /CS /G /F [/AHx /RL]",
                b"0107088 0>",
                b"BI /W 2/H 1/BPC 8/CS/G/D[0 1]/F/RL ID \x01\x07\x08 EI",
                b"q\nBI /W 2/H 1/BPC 8/CS/G/D[0 1]/F[/AHx/RL]ID 010708> EI Q\n",
            ),
            (
                "/W 2 /H 1 /CS /G /F /Br",
                b"\x10\x00\x10\x07\x08\x03",
                b"BI /W 2/H 1/BPC 8/CS/G/D[0 1]/F/Br\nID \x10\x00\x10\x07\x08\x03 EI",
                b"q\nBI /W 2/H 1/BPC 8/CS/G/D[0 1]/F[/AHx/Br]\nID 100010070803> EI Q\n",
            ),
            (
                "/W 2 /H 1 /CS /RGB /BPC 4 /F /Br /DP <</Predictor 12 /Columns 2 /Colors 3 /BitsPerComponent 4>>",
                b"@\x00\x10\x00\x01\x02\x03\x04\x03",
                b"BI /W 2/H 1/BPC 4/CS/RGB/D[0 1 0 1 0 1]/F/Br\n/DP<<\n/Predictor 12\n/Columns 2\n/Colors 3\n/BitsPerComponent 4\n>>\nID @\x00\x10\x00\x01\x02\x03\x04\x03 EI",
                b"q\nBI /W 2/H 1/BPC 4/CS/RGB/D[0 1 0 1 0 1]/F[/AHx/Br]\n/DP[null<<\n/Predictor 12\n/Columns 2\n/Colors 3\n/BitsPerComponent 4\n>>]\nID 400010000102030403> EI Q\n",
            ),
            (
                "/W 2 /H 1 /CS /RGB /BPC 4 /F /Fl /DP <</Predictor 12 /Columns 2 /Colors 3 /BitsPerComponent 4>>",
                b"x\x9cc`dbf\x01\x00\x00\x19\x00\x0b",
                b"BI /W 2/H 1/BPC 4/CS/RGB/D[0 1 0 1 0 1]/F/Fl/DP<</Predictor 12/Columns 2/Colors 3/BitsPerComponent 4>>ID x\x9cc`dbf\x01\x00\x00\x19\x00\x0b EI",
                b"q\nBI /W 2/H 1/BPC 4/CS/RGB/D[0 1 0 1 0 1]/F[/AHx/Fl]/DP[null<</Predictor 12/Columns 2/Colors 3/BitsPerComponent 4>>]ID 789c636064626601000019000b> EI Q\n",
            ),
            (
                "/W 2 /H 1 /CS /RGB /BPC 4 /F /LZW /DP <</Predictor 12 /Columns 2 /Colors 3 /BitsPerComponent 4>>",
                b"\x80\x00\x00  \x18\x12\x02",
                b"BI /W 2/H 1/BPC 4/CS/RGB/D[0 1 0 1 0 1]/F/LZW/DP<</Predictor 12/Columns 2/Colors 3/BitsPerComponent 4>>ID \x80\x00\x00  \x18 EI",
                b"q\nBI /W 2/H 1/BPC 4/CS/RGB/D[0 1 0 1 0 1]/F[/AHx/LZW]/DP[null<</Predictor 12/Columns 2/Colors 3/BitsPerComponent 4>>]ID 800000202018> EI Q\n",
            ),
            (
                "/W 1 /H 1 /CS /CS0 /BPC 8",
                b"\x00",
                b"BI /W 1/H 1/BPC 8/CS/CS0/D[0 255]ID \x00 EI",
                b"q\nBI /W 1/H 1/BPC 8/CS/CS0/D[0 255]/F/AHx ID 00> EI Q\n",
            ),
            (
                "/W 1 /H 1 /CS /CS1",
                b"abc",
                b"BI /W 1/H 1/BPC 8/CS/RGB/D[0 1 0 1 0 1]ID abc EI",
                b"q\nBI /W 1/H 1/BPC 8/CS/RGB/D[0 1 0 1 0 1]/F/AHx ID 616263> EI Q\n",
            ),
            (
                "/W 1 /H 1 /CS /CS2",
                b"g",
                b"BI /W 1/H 1/BPC 8/CS/CS2/D[0 1]ID g EI",
                b"q\nBI /W 1/H 1/BPC 8/CS/CS2/D[0 1]/F/AHx ID 67> EI Q\n",
            ),
            (
                "/W 1 /H 1 /CS /G /I true /D [1 0]",
                b"x",
                b"BI /W 1/H 1/BPC 8/CS/G/I true/D[1 0]ID x EI",
                b"q\nBI /W 1/H 1/BPC 8/CS/G/I true/D[1 0]/F/AHx ID 78> EI Q\n",
            ),
            (
                "/W 1 /H 1 /IM true",
                b"\x80",
                b"BI /W 1/H 1/BPC 1/IM true/D[0 1]ID \x80 EI",
                b"q\nBI /W 1/H 1/BPC 1/IM true/D[0 1]/F/AHx ID 80> EI Q\n",
            ),
            (
                "/W 1 /H 1 /CS /G /F /JBIG2Decode",
                b"\x00\x00",
                b"",
                b"q\nQ\n",
            ),
            (
                "/W 1 /H 1 /CS /G /F [/Fl] /DP [null]",
                b"x\x9c\xab\x04\x00\x00z\x00z",
                b"BI /W 1/H 1/BPC 8/CS/G/D[0 1]/F/Fl ID x\x9c\xab\x04\x00\x00z\x00z EI",
                b"q\nBI /W 1/H 1/BPC 8/CS/G/D[0 1]/F[/AHx/Fl]ID 789cab0400007a007a> EI Q\n",
            ),
            (
                "/W 1 /H 1 /CS /G /F [/AHx /AHx]",
                b"3739>>",
                b"BI /W 1/H 1/BPC 8/CS/G/D[0 1]ID y EI",
                b"q\nBI /W 1/H 1/BPC 8/CS/G/D[0 1]/F/AHx ID 79> EI Q\n",
            ),
            (
                "/W 1 /H 1 /CS /G /F []",
                b"z",
                b"BI /W 1/H 1/BPC 8/CS/G/D[0 1]ID z EI",
                b"q\nBI /W 1/H 1/BPC 8/CS/G/D[0 1]/F/AHx ID 7a> EI Q\n",
            ),
            (
                "/W 1 /H 1 /CS /G /F /Fl /DP <</Predictor 1.5 /Columns 1.0>>",
                b"x\x9c+\x07\x00\x00x\x00x",
                b"BI /W 1/H 1/BPC 8/CS/G/D[0 1]/F/Fl/DP<</Predictor 2>>ID x\x9c+\x07\x00\x00x\x00x EI",
                b"q\nBI /W 1/H 1/BPC 8/CS/G/D[0 1]/F[/AHx/Fl]/DP[null<</Predictor 2>>]ID 789c2b070000780078> EI Q\n",
            ),
        ];
        for &(dict, data, plain, sanitized) in cases {
            let content = [format!("BI {dict} ID ").as_bytes(), data, b" EI\n"].concat();
            let doc = crate::testutil::page_doc(&content, INLINE_RES, &[]);
            clean_content_streams(&doc, false, false, true).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(page_bytes(&doc), plain, "{dict}");
            let doc = crate::testutil::page_doc(&content, INLINE_RES, &[]);
            clean_content_streams(&doc, true, true, true).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(page_bytes(&doc), sanitized, "{dict}");
        }
    }

    /// Each page's contents, as bytes.
    fn pages_bytes(doc: &Document) -> Vec<Vec<u8>> {
        let n = doc.page_count().unwrap_or_else(|e| panic!("{e:?}"));
        (0..n)
            .map(|i| {
                let page = doc.page(i).unwrap_or_else(|e| panic!("{e}"));
                match doc.resolve(&doc.dict_get(&page, b"Contents")) {
                    Object::Stream(s) => doc.stream_data(&s).unwrap_or_else(|e| panic!("{e:?}")),
                    _ => Vec::new(),
                }
            })
            .collect()
    }

    fn inline_images_fixture() -> Document {
        Document::open(&include_bytes!("../../../corpus/smoke/content-inline-images.pdf")[..])
            .unwrap_or_else(|e| panic!("{e:?}"))
    }

    #[test]
    fn the_inline_image_fixture_cleans_as_mupdf_cleans_it() {
        // One image a page, each kind the smoke fixture has: mutool clean
        // -d -c -a writes the data in hex, 32 bytes a line, and drops the
        // images MuPDF fails to load. The last page's image never ends.
        let doc = inline_images_fixture();
        clean_content_streams(&doc, false, true, true).unwrap_or_else(|e| panic!("{e}"));
        let expect: &[&[u8]] = &[
            b"q\n1 w\nBI /W 4/H 1/BPC 8/CS/G/D[0 1]/F/AHx ID 45492045> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 2/H 1/BPC 8/CS/RGB/D[0 1 0 1 0 1]/F/AHx ID 616263646566> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 1/H 1/BPC 8/CS/CMYK/I true/D[1 0 1 0 1 0 1 0]/F/AHx ID 7778797a> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 8/H 2/BPC 1/IM true/D[0 1]/F/AHx ID ff00> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 8/H 1/BPC 1/CS//D[0 1]/F/AHx ID aa> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 3/H 2/BPC 4/CS/G/D[0 1]/F/AHx ID 12345678> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 2/H 2/BPC 16/CS/G/D[0 1]/F/AHx ID 0001000200030004> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 1/H 1/BPC 8/CS//D[0 1 0 1 0 1]/F/AHx ID 726762> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F/AHx ID 00254a6f94b9de03284d7297bce1062b50759abfe4092e53789dc2e70c31567b\na0c5ea0f34597ea3c8ed12375c81a6cbf0153a5f84a9cef3183d6287acd1f61b\n> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F/AHx ID 00254a6f94b9de03284d7297bce1062b50759abfe4092e53789dc2e70c31567b\na0c5ea0f34597ea3c8ed12375c81a6cbf0153a5f84a9cef3183d6287acd1f61b\n> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F[/AHx/Fl]ID 789c014000bfff00254a6f94b9de03284d7297bce1062b50759abfe4092e5378\n9dc2e70c31567ba0c5ea0f34597ea3c8ed12375c81a6cbf0153a5f84a9cef318\n3d6287acd1f61ba08d1e61> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 9/H 4/BPC 8/CS/G/D[0 1]/F[/AHx/Fl]ID 789c73f5547025840098c40829> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F[/AHx/LZW]ID 800004a4a37a5172de018a09a724baf1c20615940ea9a5fb90122e299e13ac27\n383062563da818aea078d0b27e51b21da121b97102a665bc02a3a2fa1152ce79\n8607a6243ab1a2f60dc0> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F[/AHx/RL]ID 3f00254a6f94b9de03284d7297bce1062b50759abfe4092e53789dc2e70c3156\n7ba0c5ea0f34597ea3c8ed12375c81a6cbf0153a5f84a9cef3183d6287acd1f6\n1b> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F[/AHx/DCT]ID ffd8ffe000104a46494600010100000100010000ffdb00430008060607060508\n0707070909080a0c140d0c0b0b0c1912130f141d1a1f1e1d1a1c1c20242e2720\n222c231c1c2837292c30313434341f27393d38323c2e333432ffc0000b080008\n000801011100ffc4001f00000105010101010101000000000000000001020304\n05060708090a0bffc400b5100002010303020403050504040000017d01020300\n041105122131410613516107227114328191a1082342b1c11552d1f024336272\n82090a161718191a25262728292a3435363738393a434445464748494a535455\n565758595a636465666768696a737475767778797a838485868788898a929394\n95969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9\ncad2d3d4d5d6d7d8d9dae1e2e3e4e5e6e7e8e9eaf1f2f3f4f5f6f7f8f9faffc4\n001f0100030101010101010101010000000000000102030405060708090a0bff\nc400b51100020102040403040705040400010277000102031104052131061241\n510761711322328108144291a1b1c109233352f0156272d10a162434e125f117\n18191a262728292a35363738393a434445464748494a535455565758595a6364\n65666768696a737475767778797a82838485868788898a92939495969798999a\na2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6\nd7d8d9dae2e3e4e5e6e7e8e9eaf2f3f4f5f6f7f8f9faffda0008010100003f00\na5f092dbfd471e95ffd9> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 16/H 4/BPC 1/IM true/D[0 1]/F[/AHx/CCF]/DP[null<</K -1/Columns 16/Rows 4>>]ID 26a8ba2e8ba2e9452492492a49249254924924940040> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F[/AHx/Fl]ID 789c014000bfff00254a6f94b9de03284d7297bce1062b50759abfe4092e5378\n9dc2e70c31567ba0c5ea0f34597ea3c8ed12375c81a6cbf0153a5f84a9cef318\n3d6287acd1f61ba08d1e61> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F[/AHx/Fl]/DP[null<</Predictor 12/Columns 8>>]ID 789c9dc6310d000008033032b490a0053398c10c66d0c23305eb555854cf9e23\n49cb03e1970bdd> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F/AHx ID 0102> EI 2 w\nQ\n",
            b"q\nBI /W 2/H 1/BPC 8/CS/G/D[0 1]/F/AHx ID 0d0a> EI 3 w\nQ\n",
            b"q\n1 w\n2 w\nQ\n",
            b"q\n1 w\n2 w\nQ\n",
            b"q\n1 w\n2 w\nQ\n",
            b"q\n1 w\n2 w\nQ\n",
            b"q\n1 w\n2 w\nQ\n",
            b"q\n1 w\n2 w\nQ\n",
            b"q\n1 w\nBI /W 1/H 1/BPC 8/CS/G/D[0 1]/F/AHx ID 78> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 1/H 1/BPC 8/CS/RGB/D[0 1 0 1 0 1]/F/AHx ID 78797a> EI 2 w\nQ\n",
            b"q\n1 w\nBI /W 1/H 1/BPC 8/CS/G/D[0 1]/F/AHx ID 78> EI 2 w\nQ\n",
            b"q\n1 w\n2 w\nQ\n",
            b"q\n1 w\n2 w\nQ\n",
            b"",
        ];
        assert_eq!(pages_bytes(&doc), expect);
    }

    #[test]
    fn the_inline_image_fixture_sanitizes_as_mupdf_sanitizes_it() {
        let doc = inline_images_fixture();
        clean_content_streams(&doc, true, false, true).unwrap_or_else(|e| panic!("{e}"));
        let expect: &[&[u8]] = &[
            b"q\nBI /W 4/H 1/BPC 8/CS/G/D[0 1]ID EI E EI Q\nq\nQ\n",
            b"q\nBI /W 2/H 1/BPC 8/CS/RGB/D[0 1 0 1 0 1]ID abcdef EI Q\nq\nQ\n",
            b"q\nBI /W 1/H 1/BPC 8/CS/CMYK/I true/D[1 0 1 0 1 0 1 0]ID wxyz EI Q\nq\nQ\n",
            b"q\nBI /W 8/H 2/BPC 1/IM true/D[0 1]ID \xff\x00 EI Q\nq\nQ\n",
            b"q\nBI /W 8/H 1/BPC 1/CS//D[0 1]ID \xaa EI Q\nq\nQ\n",
            b"q\nBI /W 3/H 2/BPC 4/CS/G/D[0 1]ID \x124Vx EI Q\nq\nQ\n",
            b"q\nBI /W 2/H 2/BPC 16/CS/G/D[0 1]ID \x00\x01\x00\x02\x00\x03\x00\x04 EI Q\nq\nQ\n",
            b"q\nBI /W 1/H 1/BPC 8/CS//D[0 1 0 1 0 1]ID rgb EI Q\nq\nQ\n",
            b"q\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]ID \x00%Jo\x94\xb9\xde\x03(Mr\x97\xbc\xe1\x06+Pu\x9a\xbf\xe4\x09.Sx\x9d\xc2\xe7\x0c1V{\xa0\xc5\xea\x0f4Y~\xa3\xc8\xed\x127\\\x81\xa6\xcb\xf0\x15:_\x84\xa9\xce\xf3\x18=b\x87\xac\xd1\xf6\x1b EI Q\nq\nQ\n",
            b"q\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]ID \x00%Jo\x94\xb9\xde\x03(Mr\x97\xbc\xe1\x06+Pu\x9a\xbf\xe4\x09.Sx\x9d\xc2\xe7\x0c1V{\xa0\xc5\xea\x0f4Y~\xa3\xc8\xed\x127\\\x81\xa6\xcb\xf0\x15:_\x84\xa9\xce\xf3\x18=b\x87\xac\xd1\xf6\x1b EI Q\nq\nQ\n",
            b"q\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F/Fl ID x\x9c\x01@\x00\xbf\xff\x00%Jo\x94\xb9\xde\x03(Mr\x97\xbc\xe1\x06+Pu\x9a\xbf\xe4\x09.Sx\x9d\xc2\xe7\x0c1V{\xa0\xc5\xea\x0f4Y~\xa3\xc8\xed\x127\\\x81\xa6\xcb\xf0\x15:_\x84\xa9\xce\xf3\x18=b\x87\xac\xd1\xf6\x1b\xa0\x8d\x1ea EI Q\nq\nQ\n",
            b"q\nBI /W 9/H 4/BPC 8/CS/G/D[0 1]/F/Fl ID x\x9cs\xf5Tp%\x84\x00\x98\xc4\x08) EI Q\nq\nQ\n",
            b"q\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F/LZW ID \x80\x00\x04\xa4\xa3zQr\xde\x01\x8a\x09\xa7$\xba\xf1\xc2\x06\x15\x94\x0e\xa9\xa5\xfb\x90\x12.)\x9e\x13\xac'80bV=\xa8\x18\xae\xa0x\xd0\xb2~Q\xb2\x1d\xa1!\xb9q\x02\xa6e\xbc\x02\xa3\xa2\xfa\x11R\xcey\x86\x07\xa6$:\xb1\xa2\xf6\x0d\xc0 EI Q\nq\nQ\n",
            b"q\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F/RL ID ?\x00%Jo\x94\xb9\xde\x03(Mr\x97\xbc\xe1\x06+Pu\x9a\xbf\xe4\x09.Sx\x9d\xc2\xe7\x0c1V{\xa0\xc5\xea\x0f4Y~\xa3\xc8\xed\x127\\\x81\xa6\xcb\xf0\x15:_\x84\xa9\xce\xf3\x18=b\x87\xac\xd1\xf6\x1b EI Q\nq\nQ\n",
            b"q\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F/DCT ID \xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xdb\x00C\x00\x08\x06\x06\x07\x06\x05\x08\x07\x07\x07\x09\x09\x08\n\x0c\x14\x0d\x0c\x0b\x0b\x0c\x19\x12\x13\x0f\x14\x1d\x1a\x1f\x1e\x1d\x1a\x1c\x1c $.' \",#\x1c\x1c(7),01444\x1f'9=82<.342\xff\xc0\x00\x0b\x08\x00\x08\x00\x08\x01\x01\x11\x00\xff\xc4\x00\x1f\x00\x00\x01\x05\x01\x01\x01\x01\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\n\x0b\xff\xc4\x00\xb5\x10\x00\x02\x01\x03\x03\x02\x04\x03\x05\x05\x04\x04\x00\x00\x01}\x01\x02\x03\x00\x04\x11\x05\x12!1A\x06\x13Qa\x07\"q\x142\x81\x91\xa1\x08#B\xb1\xc1\x15R\xd1\xf0$3br\x82\x09\n\x16\x17\x18\x19\x1a%&'()*456789:CDEFGHIJSTUVWXYZcdefghijstuvwxyz\x83\x84\x85\x86\x87\x88\x89\x8a\x92\x93\x94\x95\x96\x97\x98\x99\x9a\xa2\xa3\xa4\xa5\xa6\xa7\xa8\xa9\xaa\xb2\xb3\xb4\xb5\xb6\xb7\xb8\xb9\xba\xc2\xc3\xc4\xc5\xc6\xc7\xc8\xc9\xca\xd2\xd3\xd4\xd5\xd6\xd7\xd8\xd9\xda\xe1\xe2\xe3\xe4\xe5\xe6\xe7\xe8\xe9\xea\xf1\xf2\xf3\xf4\xf5\xf6\xf7\xf8\xf9\xfa\xff\xc4\x00\x1f\x01\x00\x03\x01\x01\x01\x01\x01\x01\x01\x01\x01\x00\x00\x00\x00\x00\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\n\x0b\xff\xc4\x00\xb5\x11\x00\x02\x01\x02\x04\x04\x03\x04\x07\x05\x04\x04\x00\x01\x02w\x00\x01\x02\x03\x11\x04\x05!1\x06\x12AQ\x07aq\x13\"2\x81\x08\x14B\x91\xa1\xb1\xc1\x09#3R\xf0\x15br\xd1\n\x16$4\xe1%\xf1\x17\x18\x19\x1a&'()*56789:CDEFGHIJSTUVWXYZcdefghijstuvwxyz\x82\x83\x84\x85\x86\x87\x88\x89\x8a\x92\x93\x94\x95\x96\x97\x98\x99\x9a\xa2\xa3\xa4\xa5\xa6\xa7\xa8\xa9\xaa\xb2\xb3\xb4\xb5\xb6\xb7\xb8\xb9\xba\xc2\xc3\xc4\xc5\xc6\xc7\xc8\xc9\xca\xd2\xd3\xd4\xd5\xd6\xd7\xd8\xd9\xda\xe2\xe3\xe4\xe5\xe6\xe7\xe8\xe9\xea\xf2\xf3\xf4\xf5\xf6\xf7\xf8\xf9\xfa\xff\xda\x00\x08\x01\x01\x00\x00?\x00\xa5\xf0\x92\xdb\xfdG\x1e\x95\xff\xd9 EI Q\nq\nQ\n",
            b"q\nBI /W 16/H 4/BPC 1/IM true/D[0 1]/F/CCF/DP<</K -1/Columns 16/Rows 4>>ID &\xa8\xba.\x8b\xa2\xe9E$\x92I*I$\x92T\x92I$\x94\x00@ EI Q\nq\nQ\n",
            b"q\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F/Fl ID x\x9c\x01@\x00\xbf\xff\x00%Jo\x94\xb9\xde\x03(Mr\x97\xbc\xe1\x06+Pu\x9a\xbf\xe4\x09.Sx\x9d\xc2\xe7\x0c1V{\xa0\xc5\xea\x0f4Y~\xa3\xc8\xed\x127\\\x81\xa6\xcb\xf0\x15:_\x84\xa9\xce\xf3\x18=b\x87\xac\xd1\xf6\x1b\xa0\x8d\x1ea EI Q\nq\nQ\n",
            b"q\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]/F/Fl/DP<</Predictor 12/Columns 8>>ID x\x9c\x9d\xc61\x0d\x00\x00\x08\x0302\xb4\x90\xa0\x053\x98\xc1\x0cf\xd0\xc23\x05\xebUXT\xcf\x9e#I\xcb\x03\xe1\x97\x0b\xdd EI Q\nq\nQ\n",
            b"q\nBI /W 8/H 8/BPC 8/CS/G/D[0 1]ID \x01\x02 EI Q\nq\nQ\n",
            b"q\nBI /W 2/H 1/BPC 8/CS/G/D[0 1]ID \x0d\n EI Q\nq\nQ\n",
            b"q\nQ\nq\nQ\n",
            b"q\nQ\nq\nQ\n",
            b"q\nQ\nq\nQ\n",
            b"q\nQ\nq\nQ\n",
            b"q\nQ\nq\nQ\n",
            b"q\nQ\nq\nQ\n",
            b"q\nBI /W 1/H 1/BPC 8/CS/G/D[0 1]ID x EI Q\nq\nQ\n",
            b"q\nBI /W 1/H 1/BPC 8/CS/RGB/D[0 1 0 1 0 1]ID xyz EI Q\nq\nQ\n",
            b"q\nBI /W 1/H 1/BPC 8/CS/G/D[0 1]ID x EI Q\nq\nQ\n",
            b"q\nQ\nq\nQ\n",
            b"q\nQ\nq\nQ\n",
            b"q\nQ\n",
        ];
        assert_eq!(pages_bytes(&doc), expect);
    }

    /// A page with annotations: appearance streams with and without
    /// resources (5, 6, 14), appearance states that are not streams, an
    /// appearance dictionary of its own (13), and the links, widgets and
    /// popups `pdf_load_annots` leaves out.
    fn annots_doc() -> Document {
        let form =
            |dict: &str, data: &[u8]| stream(&format!("/Subtype/Form/BBox[0 0 10 10]{dict}"), data);
        let annot = |rest: &str| format!("<</Type/Annot/Rect[0 0 10 10]{rest}>>").into_bytes();
        doc_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</ExtGState<</A<</CA .5>>>>>>/Contents 4 0 R/Annots[7 0 R 8 0 R 9 0 R 10 0 R 11 0 R 12 0 R 99 0 R 3 0 R]>>".to_vec(),
            stream("", b"0 0 1 1 re f"),
            form(
                "/Resources<</ExtGState<</B<</ca .5>>>>>>",
                b"0 g 0 g /B gs 0 0 5 5 re f q",
            ),
            form("", b"1 0 0 rg 1 0 0 rg 0 0 5 5 re f"),
            annot("/Subtype/Square/AP<</N 5 0 R/D<</On 6 0 R>>/R 6 0 R>>"),
            annot("/Subtype/Link/AP<</N 6 0 R>>"),
            annot("/Subtype/Widget/AP<</N 6 0 R>>"),
            annot("/Subtype/Popup/AP<</N 6 0 R>>"),
            annot("/Subtype/Text"),
            annot("/Subtype/Ink/AP 13 0 R"),
            b"<</N 14 0 R>>".to_vec(),
            form("", b"0 0 1 rg 0 0 1 rg 0 0 5 5 re f"),
        ])
    }

    #[test]
    fn annotation_appearances_are_rewritten_too() {
        let doc = annots_doc();
        assert_eq!(
            cleaned(&doc, false),
            (
                "0 0 1 1 re\nf\n".into(),
                "<</ExtGState<</A<</CA .5>>>>>>".into()
            )
        );
        let form =
            |len: usize| format!("<</Subtype/Form/BBox[0 0 10 10]/Length {len}/Resources null>>");
        assert_eq!(
            object(&doc, 5),
            (
                "<</Subtype/Form/BBox[0 0 10 10]/Resources<</ExtGState<</B<</ca .5>>>>>>/Length 31>>".into(),
                "0 g\n0 g\n/B gs\n0 0 5 5 re\nf\nq\nQ\n".into()
            )
        );
        assert_eq!(
            object(&doc, 6),
            (form(31), "1 0 0 rg\n1 0 0 rg\n0 0 5 5 re\nf\n".into())
        );
        assert_eq!(
            object(&doc, 14),
            (form(31), "0 0 1 rg\n0 0 1 rg\n0 0 5 5 re\nf\n".into())
        );
        let doc = annots_doc();
        assert_eq!(
            cleaned(&doc, true),
            ("q\n0 0 1 1 re\nf\nQ\n".into(), "<<>>".into())
        );
        assert_eq!(object(&doc, 5).1, "q\n0 g\n/B gs\n0 0 5 5 re\nf\nq\nQ\nQ\n");
        assert_eq!(
            object(&doc, 6),
            (form(26), "q\n1 0 0 rg\n0 0 5 5 re\nf\nQ\n".into())
        );
        assert_eq!(
            object(&doc, 14),
            (form(26), "q\n0 0 1 rg\n0 0 5 5 re\nf\nQ\n".into())
        );
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e}"));
        let kept: Vec<Object> = page_annots(&doc, &page)
            .iter()
            .map(|a| doc.dict_get(a, b"Subtype"))
            .collect();
        assert!(
            !kept
                .iter()
                .any(|s| s.is_name(b"Link") || s.is_name(b"Widget") || s.is_name(b"Popup"))
        );
        assert!(kept.iter().any(|s| s.is_name(b"Ink")));
    }

    #[test]
    fn a_type3_font_that_is_its_own_resources_is_left_alone() {
        // The font is the resource dictionary being rewritten, so its
        // glyphs are a cycle MuPDF skips.
        for sanitize in [false, true] {
            let doc = crate::testutil::doc_of(&[
                "<</Type/Catalog/Pages 2 0 R>>",
                "<</Type/Pages/Kids[3 0 R]/Count 1>>",
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources 5 0 R/Contents 4 0 R>>",
                "<</Length 22>>stream\nBT /T 10 Tf (a) Tj ET\nendstream",
                "<</Type/Font/Subtype/Type3/FontBBox[0 0 10 10]/FontMatrix[.1 0 0 .1 0 0]/FirstChar 97/LastChar 97/Widths[10]/Encoding<</Differences[97/a]>>/CharProcs<</a 6 0 R>>/Font<</T 5 0 R>>>>",
                "<</Length 20>>stream\n10 0 d0 0 0 1 1 re f\nendstream",
            ]);
            let (data, res) = cleaned(&doc, sanitize);
            if sanitize {
                assert_eq!(data, "q\nBT\n/T 10 Tf\n(a)Tj\nET\nQ\n");
                assert_eq!(res, "<</Font<</T 5 0 R>>>>");
            } else {
                assert_eq!(data, "BT\n/T 10 Tf\n(a)Tj\nET\n");
            }
            assert_eq!(object(&doc, 6).1, "10 0 d0 0 0 1 1 re f");
        }
    }

    #[test]
    fn text_without_a_font_or_with_a_bad_array() {
        // The writer gives TJ a null for an operand that is not an array;
        // the filter drops text shown with no font, and what it cannot
        // show.
        let content = "BT (x) Tj [(y)] TJ (z) ' ET BT /F1 10 Tf (abc) TJ 5 TJ [(d) /N (e)] TJ 1 0 0 1 0 0 Tm ET q 1 0 0 1 0 0 cm 2 0 0 2 0 0 cm 0 0 1 1 re f Q BT /F1 10 Tf 2 Tr 3 Tr 0 Tr (f) Tj ET";
        let doc = case(content, "/Font<</F1 5 0 R>>", &[HELVETICA.to_vec()]);
        assert_eq!(
            cleaned(&doc, false).0,
            "BT\n(x)Tj\n[(y)]TJ\n(z)'\nET\nBT\n/F1 10 Tf\nnull TJ\nnull TJ\nnull TJ\n1 0 0 1 0 0 Tm\nET\nq\n1 0 0 1 0 0 cm\n2 0 0 2 0 0 cm\n0 0 1 1 re\nf\nQ\nBT\n/F1 10 Tf\n2 Tr\n3 Tr\n0 Tr\n(f)Tj\nET\n"
        );
        assert!(warnings(&doc).contains(&"syntax error: syntax error in array".to_string()));
        let doc = case(content, "/Font<</F1 5 0 R>>", &[HELVETICA.to_vec()]);
        assert_eq!(
            cleaned(&doc, true).0,
            "q\nq\n2 0 0 2 0 0 cm\n0 0 1 1 re\nf\nQ\nBT\n/F1 10 Tf\n(f)Tj\nET\nQ\n"
        );
    }

    const T3: &str = "/Type/Font/Subtype/Type3/FontBBox[0 0 10 10]/FontMatrix[.1 0 0 .1 0 0]/FirstChar 98/LastChar 98/Widths[10]/Encoding<</Differences[98/b]>>";

    fn objects(objs: &[&str]) -> Document {
        let mut all = vec![
            "<</Type/Catalog/Pages 2 0 R>>",
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        ];
        all.extend_from_slice(objs);
        crate::testutil::doc_of(&all)
    }

    #[test]
    fn a_page_without_contents_gets_an_empty_stream() {
        let page = "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</ExtGState<</A<</CA 1>>>>>>>>";
        let doc = objects(&[page]);
        assert_eq!(
            cleaned(&doc, false),
            (String::new(), "<</ExtGState<</A<</CA 1>>>>>>".into())
        );
        let doc = objects(&[page]);
        assert_eq!(cleaned(&doc, true), (String::new(), "<<>>".into()));
    }

    #[test]
    fn a_form_sharing_the_page_resources_is_rewritten_once() {
        let objs = [
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources 5 0 R/Contents 4 0 R>>",
            "<</Length 7>>stream\n/Fm0 Do\nendstream",
            "<</XObject<</Fm0 6 0 R>>>>",
            "<</Subtype/Form/BBox[0 0 1 1]/Resources 5 0 R/Length 19>>stream\n0 0 1 1 re f /Fm0 Do\nendstream",
        ];
        let doc = objects(&objs);
        assert_eq!(cleaned(&doc, false).0, "/Fm0 Do\n");
        assert_eq!(
            object(&doc, 6),
            (
                "<</Subtype/Form/BBox[0 0 1 1]/Resources 5 0 R/Length 21>>".into(),
                "0 0 1 1 re\nf\n/Fm0 Do\n".into()
            )
        );
        let doc = objects(&objs);
        assert_eq!(cleaned(&doc, true).0, "q\n/Fm0 Do\nQ\n");
        assert_eq!(
            object(&doc, 6),
            (
                "<</Subtype/Form/BBox[0 0 1 1]/Resources<</XObject<</Fm0 6 0 R>>>>/Length 25>>"
                    .into(),
                "q\n0 0 1 1 re\nf\n/Fm0 Do\nQ\n".into()
            )
        );
    }

    #[test]
    fn type3_fonts_in_indirect_resources() {
        // Without resources of its own a Type 3 font takes the page's,
        // and a font whose CharProcs is not a dictionary has no glyphs.
        let doc = objects(&[
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources 6 0 R/Contents 4 0 R>>",
            "<</Length 22>>stream\nBT /T4 10 Tf (b) Tj ET\nendstream",
            "<</Length 20>>stream\n10 0 d0 0 0 1 1 re f\nendstream",
            &format!("<</Font<</T4<<{T3}/CharProcs<</b 5 0 R>>>>>>>>"),
        ]);
        assert_eq!(cleaned(&doc, false).0, "BT\n/T4 10 Tf\n(b)Tj\nET\n");
        assert_eq!(object(&doc, 5).1, "10 0 d0\n0 0 1 1 re\nf\n");
        assert_eq!(
            object(&doc, 6).0,
            format!("<</Font<</T4<<{T3}/CharProcs<</b 5 0 R>>/Resources 6 0 R>>>>>>")
        );
        let doc = objects(&[
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font 6 0 R>>/Contents 4 0 R>>",
            "<</Length 38>>stream\nBT /T4 10 Tf (b) Tj /T5 10 Tf (b) Tj ET\nendstream",
            "<</Length 20>>stream\n10 0 d0 0 0 1 1 re f\nendstream",
            &format!("<</T4<<{T3}/CharProcs<</b 5 0 R>>>>/T5<<{T3}/CharProcs 7>>>>"),
        ]);
        assert_eq!(
            cleaned(&doc, true).0,
            "q\nBT\n/T4 10 Tf\n(b)Tj\n/T5 10 Tf\n(b)Tj\nET\nQ\n"
        );
        assert_eq!(
            object(&doc, 6).0,
            format!(
                "<</T4<<{T3}/CharProcs<</b 5 0 R>>/Resources<<>>>>/T5<<{T3}/CharProcs 7/Resources<<>>>>>>"
            )
        );
    }

    #[test]
    fn a_soft_mask_group_that_is_missing_fails_the_rewrite() {
        // pdf_update_stream puts /Length in the object first, and throws
        // for one that is not a dictionary, so mutool clean -c fails.
        for sanitize in [false, true] {
            let doc = objects(&[
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</ExtGState<</A<</SMask<</S/Luminosity/G null>>>>/B<</SMask<</S/Luminosity/G 99 0 R>>>>/C<</SMask/None>>>>>>/Contents 4 0 R>>",
                "<</Length 30>>stream\n/A gs /B gs /C gs 0 0 1 1 re f\nendstream",
            ]);
            assert_eq!(
                clean_content_streams(&doc, sanitize, false, true),
                Err("argument error: not a dict (null)".into())
            );
            assert!(
                warnings(&doc).contains(&"content stream is not a stream (99 0 R)".to_string())
            );
        }
    }

    #[test]
    fn direct_forms_after_the_first_change_the_stream_they_made() {
        // The first direct form makes the resources object a stream; the
        // second still changes it, as a dictionary with data.
        let objs = [
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources 5 0 R/Contents 4 0 R>>",
            "<</Length 6>>stream\n/Fd Do\nendstream",
            "<</XObject<</Fd<</Subtype/Form/BBox[0 0 1 1]>>/Fe<</Subtype/Form/BBox[0 0 1 1]/Filter/Fl/DecodeParms<<>>/Resources<</ExtGState<</A<</CA 1>>>>>>>>>>/ExtGState<</G<</CA .5>>>>>>",
        ];
        let doc = objects(&objs);
        assert_eq!(cleaned(&doc, false).0, "/Fd Do\n");
        assert_eq!(
            object(&doc, 5),
            (
                "<</XObject<</Fd<</Subtype/Form/BBox[0 0 1 1]/Length 0/Resources 5 0 R>>/Fe<</Subtype/Form/BBox[0 0 1 1]/Length 0/Resources<</ExtGState<</A<</CA 1>>>>>>>>>>/ExtGState<</G<</CA .5>>>>>>".into(),
                String::new()
            )
        );
        let doc = objects(&objs);
        assert_eq!(cleaned(&doc, true).0, "q\n/Fd Do\nQ\n");
        assert_eq!(
            object(&doc, 5),
            (
                "<</XObject<</Fd<</Subtype/Form/BBox[0 0 1 1]/Length 4/Resources<<>>>>/Fe<</Subtype/Form/BBox[0 0 1 1]/Filter/Fl/DecodeParms<<>>/Resources<</ExtGState<</A<</CA 1>>>>>>>>>>/ExtGState<</G<</CA .5>>>>>>".into(),
                "q\nQ\n".into()
            )
        );
    }

    #[test]
    fn text_left_open_stroke_patterns_and_codes_a_cmap_lacks() {
        // A Q inside a text object ends it first; a code the font's CMap
        // does not map is kept, with MuPDF's warning.
        let content = "q BT /F1 10 Tf 10 10 Td (a) Tj Q /Pattern CS /P0 SCN 0 0 5 5 re S /Pattern cs /P1 scn 0 0 5 5 re f /P1 scn 0 0 5 5 re f BT /F2 10 Tf <414243> Tj [<4142> -100 <43>] TJ ET";
        let cmap = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap /CMapName /Bar def 1 begincodespacerange <00> <FF> endcodespacerange 1 begincidrange <41> <41> 1 endcidrange endcmap CMapName currentdict /CMap defineresource pop end end";
        let make = || {
            case(
                content,
                "/Font<</F1 5 0 R/F2 6 0 R>>/Pattern<</P0 7 0 R/P1<</PatternType 2/Shading<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 1 0]/Function<</FunctionType 2/Domain[0 1]/C0[1 0 0]/C1[0 0 1]/N 1>>>>>>>>",
                &[
                    HELVETICA.to_vec(),
                    b"<</Type/Font/Subtype/Type0/BaseFont/Bar/Encoding 8 0 R/DescendantFonts[<</Type/Font/Subtype/CIDFontType0/BaseFont/Bar/CIDSystemInfo<</Registry(Adobe)/Ordering(Identity)/Supplement 0>>/FontDescriptor<</Type/FontDescriptor/FontName/Bar/Flags 4/FontBBox[0 0 1000 1000]/ItalicAngle 0/Ascent 800/Descent -200/CapHeight 700/StemV 80>>>>]>>".to_vec(),
                    stream(
                        "/PatternType 1/PaintType 1/TilingType 1/BBox[0 0 10 10]/XStep 10/YStep 10/Resources<<>>",
                        b"0 0 5 5 re f",
                    ),
                    stream(
                        "/Type/CMap/CMapName/Bar/CIDSystemInfo<</Registry(Adobe)/Ordering(Identity)/Supplement 0>>",
                        cmap,
                    ),
                ],
            )
        };
        let doc = make();
        assert_eq!(
            cleaned(&doc, true).0,
            "q\nBT\n/F1 10 Tf\n10 10 TD\n(a)Tj\nET\nQ\nq\n/Pattern CS\n/P0 SCN\n0 0 5 5 re\nS\n/Pattern cs\n/P1 scn\n0 0 5 5 re\nf\n0 0 5 5 re\nf\nBT\n/F2 10 Tf\n(ABC)Tj\n[(AB)-100(C)]TJ\nET\nQ\n"
        );
        assert!(warnings(&doc).contains(&"cannot encode character".to_string()));
        let doc = make();
        assert_eq!(
            cleaned(&doc, false).0,
            "q\nBT\n/F1 10 Tf\n10 10 Td\n(a)Tj\nQ\n/Pattern CS\n/P0 SCN\n0 0 5 5 re\nS\n/Pattern cs\n/P1 scn\n0 0 5 5 re\nf\n/P1 scn\n0 0 5 5 re\nf\nBT\n/F2 10 Tf\n(ABC)Tj\n[(AB)-100(C)]TJ\nET\n"
        );
    }
}
