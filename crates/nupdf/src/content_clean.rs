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

/// `pdf_filter_content_stream`: `contents` through the filters, then,
/// with `recurse`, the resources it uses.
#[allow(clippy::too_many_arguments)]
fn filter_content_stream(
    doc: &Document,
    contents: Option<&Object>,
    in_res: Option<&Object>,
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
        filter_resources(doc, old, new, opts, hooks, cycle)?;
    }
    Ok(Rewritten { data, resources })
}

/// `pdf_filter_resources`: the forms, patterns, soft-mask groups and
/// Type 3 fonts in the new resources `res`, each rewritten in place.
fn filter_resources(
    doc: &Document,
    in_res: &Object,
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
                filter_xobject(doc, &g, Some(in_res), opts, hooks, &cycle)?;
            }
        }
    }
    for pat in category(b"Pattern") {
        if doc.dict_get(&pat, b"PatternType").to_int() == 1 {
            filter_xobject(doc, &pat, Some(in_res), opts, hooks, &cycle)?;
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
                    filter_direct_xobject(doc, &xobj, in_res, res, &name, opts, hooks, &cycle)?
                }
                _ => filter_xobject(doc, &xobj, Some(in_res), opts, hooks, &cycle)?,
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
            put_in(doc, res, b"Font", &name, changed);
        }
    }
    Ok(())
}

/// `res[category][name] = value`, for a font dictionary held directly.
fn put_in(doc: &Document, res: &mut Object, category: &[u8], name: &[u8], value: Object) {
    let put = |d: &mut Dict| {
        let sub = d.get(category).cloned().unwrap_or(Object::Null);
        match sub {
            Object::Ref(r) => {
                if let Object::Dict(s) = doc.resolve(&Object::Ref(r)) {
                    let mut s = (*s).clone();
                    s.put(name, value.clone());
                    doc.alter_object(r.num, Object::Dict(Arc::new(s)));
                }
            }
            Object::Dict(s) => {
                let mut s = (*s).clone();
                s.put(name, value.clone());
                d.put(category, Object::Dict(Arc::new(s)));
            }
            _ => {}
        }
    };
    match res {
        Object::Ref(r) => {
            if let Object::Dict(d) = doc.resolve(&Object::Ref(*r)) {
                let mut d = (*d).clone();
                put(&mut d);
                doc.alter_object(r.num, Object::Dict(Arc::new(d)));
            }
        }
        Object::Dict(d) => put(Arc::make_mut(d)),
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
    match doc.dict_get(obj, b"StructParents") {
        Object::Int(i) => i as i32,
        Object::Real(r) => r as i32,
        _ => -1,
    }
}

/// `pdf_update_stream(..., 0)`, then `edit` on the stream's dictionary:
/// the data is stored as is, so /Filter and /DecodeParms go.
fn update_stream(doc: &Document, num: i32, data: Vec<u8>, edit: impl FnOnce(&mut Dict)) {
    let Some(obj) = doc.object(num) else {
        doc.diagnostics().warn(
            "object.out_of_range",
            format!(
                "object out of range ({num} 0 R); xref size {}",
                doc.xref_len()
            ),
        );
        return;
    };
    let mut dict = match obj {
        Object::Dict(d) => (*d).clone(),
        Object::Stream(s) => s.dict.clone(),
        _ => Dict::new(),
    };
    dict.put(b"Length", Object::Int(data.len() as i64));
    dict.remove(b"Filter");
    dict.remove(b"DecodeParms");
    edit(&mut dict);
    doc.replace_stream(num, dict, data);
}

/// `pdf_filter_xobject`: a form (or pattern, or group) rewritten in place.
fn filter_xobject(
    doc: &Document,
    stm: &Object,
    page_res: Option<&Object>,
    opts: &Options,
    hooks: &mut dyn Hooks,
    cycle_up: &Cycle,
) -> Result<(), String> {
    let sp = struct_parents(doc, stm);
    let old_res = stored(doc, stm, b"Resources").or_else(|| page_res.cloned());
    let Some(cycle) = cycle_up.enter(stm) else {
        return Ok(());
    };
    let out = filter_content_stream(
        doc,
        Some(stm),
        old_res.as_ref(),
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
    });
    Ok(())
}

/// `pdf_filter_xobject` for a form written directly in the resources, as
/// MuPDF treats it: it has no contents of its own, gets a /Length and
/// /Resources, and its data goes to the object that holds it, which
/// becomes a stream. A holder that is not an object of its own gets the
/// warning `pdf_update_stream` gives.
#[allow(clippy::too_many_arguments)]
fn filter_direct_xobject(
    doc: &Document,
    stm: &Object,
    page_res: &Object,
    res: &mut Object,
    name: &[u8],
    opts: &Options,
    hooks: &mut dyn Hooks,
    cycle: &Cycle,
) -> Result<(), String> {
    let sp = struct_parents(doc, stm);
    let old_res = stored(doc, stm, b"Resources").or_else(|| Some(page_res.clone()));
    let out = filter_content_stream(
        doc,
        Some(stm),
        old_res.as_ref(),
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
    let parent = res.as_ref().map(|r| r.num);
    match parent {
        Some(_) => {
            d.remove(b"Filter");
            d.remove(b"DecodeParms");
        }
        None => doc.diagnostics().warn(
            "object.out_of_range",
            format!("object out of range (0 0 R); xref size {}", doc.xref_len()),
        ),
    }
    d.put(b"Resources", out.resources.unwrap_or(Object::Null));
    put_in(doc, res, b"XObject", name, Object::Dict(Arc::new(d)));
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
    let out = filter_content_stream(
        doc,
        Some(old),
        old_res.as_ref(),
        transform,
        opts,
        hooks,
        sp,
        &cycle,
    )?;
    update_stream(doc, num, out.data, |d| {
        d.put(b"Resources", out.resources.unwrap_or(Object::Null));
    });
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
                update_stream(doc, r.num, proc.output().to_vec(), |_| {});
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
                update_stream(doc, r.num, proc.writer.out.clone(), |_| {});
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
    crate::edit::edit_page(doc, index, &mut |d| {
        d.put(b"Contents", Object::Ref(ObjRef::new(num, 0)));
    })?;
    update_stream(doc, num, data, |_| {});
    crate::edit::edit_page(doc, index, &mut |d| d.put(b"Resources", resources.clone()))?;
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
            filter_xobject(doc, stm, None, opts, hooks, &Cycle::default())?;
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
}
