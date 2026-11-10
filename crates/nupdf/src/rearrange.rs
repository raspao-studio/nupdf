// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-clean-file.c (pdf_rearrange_pages),
// source/pdf/pdf-nametree.c (pdf_load_name_tree), source/pdf/pdf-page.c
// (pdf_flatten_inheritable_page_items) and source/pdf/pdf-repair.c
// (pdf_parent_root), Copyright (C) 2004-2026 Artifex Software, Inc.

//! Keeping some pages, in a given order (`pdf_rearrange_pages`, with the
//! structure tree dropped), as `mutool clean` does when it is given page
//! ranges. The catalog is replaced by one that keeps only /Type, /Pages,
//! /Outlines, /OCProperties and the named destinations that lead to a kept
//! page. Each kept page gets its inherited attributes and the root /Pages
//! as parent. Links, form fields and outline items that lead to a page no
//! longer there are removed.
//!
//! MuPDF changes objects in place; here each change goes into the
//! document's edits ([`Document::update_object`]), on the object that holds
//! the value changed.

use std::sync::Arc;

use crate::document::Document;
use crate::object::{Dict, Object};

/// Outline nesting deeper than this is refused rather than followed.
const MAX_OUTLINE_DEPTH: usize = 1024;

/// Why the pages could not be rearranged: MuPDF's error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RearrangeError(pub String);

impl std::fmt::Display for RearrangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `fz_parse_page_range` over a whole list, as `mutool clean` reads its
/// page arguments: 0-based page indices in order. `N` is the last page,
/// negative numbers count from the end, and a range may run backwards.
/// Invalid parts are skipped with MuPDF's warning.
pub fn parse_page_ranges(spec: &str, count: i32, doc: &Document) -> Vec<i32> {
    let mut pages = Vec::new();
    for (a, b) in parse_page_range_pairs(spec, count, Some(doc)) {
        if a <= b {
            pages.extend(a..=b);
        } else {
            pages.extend((b..=a).rev());
        }
    }
    pages.iter_mut().for_each(|p| *p -= 1);
    pages
}

/// Each range `fz_parse_page_range` reads, as its first and last page
/// (1-based, clamped to the document). With `doc`, an invalid part gets
/// MuPDF's warning.
pub fn parse_page_range_pairs(spec: &str, count: i32, doc: Option<&Document>) -> Vec<(i32, i32)> {
    let mut pairs = Vec::new();
    let mut s = spec.as_bytes();
    loop {
        if s.is_empty() {
            break;
        }
        let orig = s.len();
        if s[0] == b',' {
            s = &s[1..];
        }
        let number = |s: &mut &[u8]| -> i64 {
            // strtol: spaces, a sign, digits; nothing read leaves s alone.
            let mut i = 0;
            while s
                .get(i)
                .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
            {
                i += 1;
            }
            let negative = s.get(i) == Some(&b'-');
            if matches!(s.get(i), Some(b'-' | b'+')) {
                i += 1;
            }
            let start = i;
            let mut v: i64 = 0;
            while let Some(d) = s.get(i).filter(|c| c.is_ascii_digit()) {
                v = v.saturating_mul(10).saturating_add(i64::from(d - b'0'));
                i += 1;
            }
            if i == start {
                return 0;
            }
            *s = &s[i..];
            if negative { -v } else { v }
        };
        let mut a = if s.first() == Some(&b'N') {
            s = &s[1..];
            i64::from(count)
        } else {
            number(&mut s)
        };
        let mut b = if s.first() == Some(&b'-') {
            if s.get(1) == Some(&b'N') {
                s = &s[2..];
                i64::from(count)
            } else {
                // strtol from after the '-', which it passes even when it
                // reads no number.
                let mut t = &s[1..];
                let v = number(&mut t);
                s = t;
                v
            }
        } else {
            a
        };
        let n = i64::from(count);
        if a < 0 {
            a += n + 1;
        }
        if b < 0 {
            b += n + 1;
        }
        // fz_clampi(v, 1, n), which gives n when n is below 1.
        let clamp = |v: i64| {
            if v < 1 {
                1
            } else if v > n {
                n
            } else {
                v
            }
        };
        let (a, b) = (clamp(a), clamp(b));
        if s.len() == orig {
            if let Some(doc) = doc {
                doc.diagnostics()
                    .warn("pages.invalid_range", "skipping invalid page range");
            }
            break;
        }
        pairs.push((a as i32, b as i32));
    }
    pairs
}

/// `pdf_rearrange_pages(doc, pages, PDF_CLEAN_STRUCTURE_DROP)`.
pub fn rearrange_pages(doc: &Document, pages: &[i32]) -> Result<(), RearrangeError> {
    let fail = |m: String| RearrangeError(m);
    let ed = Editor { doc };
    let trailer = doc.trailer();
    let oldroot = trailer.get(b"Root").cloned().unwrap_or(Object::Null);
    let pages_node = ed.get(&oldroot, b"Pages");
    let olddests = load_name_tree(doc, b"Dests");
    let outlines = ed.get(&oldroot, b"Outlines");
    let ocproperties = ed.get(&oldroot, b"OCProperties");

    let mut root = Dict::new();
    root.put(b"Type", ed.get(&oldroot, b"Type").unwrap_or(Object::Null));
    root.put(b"Pages", pages_node.clone().unwrap_or(Object::Null));
    if let Some(o) = &outlines {
        root.put(b"Outlines", o.clone());
    }
    if let Some(o) = &ocproperties {
        root.put(b"OCProperties", o.clone());
    }
    let num = oldroot.as_ref().map_or(0, |r| r.num);
    let root_num = if num > 0 && (num as usize) < doc.xref_len() {
        Some(num)
    } else {
        doc.diagnostics().warn(
            "object.out_of_range",
            format!(
                "object out of range ({num} 0 R); xref size {}",
                doc.xref_len()
            ),
        );
        None
    };
    if let Some(num) = root_num {
        doc.update_object(num, Object::Dict(Arc::new(root.clone())));
    }

    // Keep each page, flattened, with the root /Pages as parent.
    let parent = pages_node.clone().unwrap_or(Object::Null);
    let mut kids = Vec::with_capacity(pages.len());
    for &index in pages {
        let pageref = doc.page(index).map_err(|e| fail(e.0))?;
        let mut flat = Vec::new();
        for key in [&b"MediaBox"[..], b"CropBox", b"Rotate", b"Resources"] {
            if let Some(v) = doc.inherited(&pageref, key).map_err(|e| fail(e.0))? {
                flat.push((key, v));
            }
        }
        ed.require_dict(&pageref).map_err(fail)?;
        let changed = ed.with_dict(&pageref, |d| {
            for (key, v) in flat {
                d.put(key, v);
            }
            d.put(b"Parent", parent.clone());
        });
        kids.push(changed.unwrap_or(pageref));
    }
    let count = kids.len();
    let kids = Object::Array(Arc::new(kids));
    let set_tree = |d: &mut Dict| {
        d.put(b"Count", Object::Int(count as i64));
        d.put(b"Kids", kids.clone());
    };
    ed.require_dict(pages_node.as_ref().unwrap_or(&Object::Null))
        .map_err(fail)?;
    // A direct /Pages is shared with the new catalog.
    if let Some(node) = &pages_node
        && let Some(direct) = ed.with_dict(node, set_tree)
    {
        root.put(b"Pages", direct);
    }
    if let Some(num) = root_num {
        doc.update_object(num, Object::Dict(Arc::new(root.clone())));
    }

    let pagecount = doc.page_count().map_err(|e| fail(e.0))?;
    let mut page_nums = Vec::with_capacity(pagecount as usize);
    for i in 0..pagecount {
        let page = doc.page(i).map_err(|e| fail(e.0))?;
        page_nums.push(page.as_ref().map_or(0, |r| r.num));
    }
    let valid_page = |o: Option<Object>| {
        let num = o.as_ref().and_then(Object::as_ref).map_or(0, |r| r.num);
        num != 0 && page_nums.contains(&num)
    };

    // Named destinations that lead to a kept page.
    let mut names_list: Option<Vec<Object>> = None;
    if let Some(olddests) = olddests {
        let mut list = Vec::new();
        for (key, val) in olddests.iter() {
            let dest = ed.get(val, b"D");
            let first = ed.array_get(dest.as_ref().unwrap_or(val), 0);
            if valid_page(first) {
                let name = key.split(|&b| b == 0).next().unwrap_or_default();
                list.push(Object::String(Arc::from(name)));
                list.push(val.clone());
            }
        }
        names_list = Some(list);
    }
    if let Some(list) = &names_list {
        let mut dests = Dict::new();
        dests.put(b"Names", Object::Array(Arc::new(list.clone())));
        let mut names = Dict::new();
        names.put(b"Dests", Object::Dict(Arc::new(dests)));
        root.put(b"Names", Object::Dict(Arc::new(names)));
    }
    let names: &[Object] = names_list.as_deref().unwrap_or_default();
    let dest_ok = |o: &Object| dest_is_valid(&ed, o, &valid_page, names);

    // Links that lead nowhere now.
    for i in 0..pagecount {
        let page = doc.page(i).map_err(|e| fail(e.0))?;
        ed.edit_array_in(&page, b"Annots", |annots| {
            let mut j = 0;
            while j < annots.len() {
                let o = &annots[j];
                let link = ed
                    .get(o, b"Subtype")
                    .is_some_and(|s| ed.resolve(&s).is_name(b"Link"));
                if link && !dest_ok(o) {
                    annots.remove(j);
                } else {
                    j += 1;
                }
            }
        });
    }

    // Form fields: widgets on the kept pages, then the fields above them.
    let mut fields = Vec::new();
    for i in 0..pagecount {
        let page = doc.page(i).map_err(|e| fail(e.0))?;
        let annots = ed.get(&page, b"Annots").map(|a| ed.resolve(&a));
        for f in annots
            .as_ref()
            .and_then(Object::as_array)
            .unwrap_or_default()
        {
            // A pointer comparison: only a direct /Widget counts.
            if ed.get(f, b"Subtype").is_some_and(|s| s.is_name(b"Widget")) {
                fields.push(f.clone());
            }
        }
    }
    for f in &fields {
        let top = parent_root(&ed, f).map_err(fail)?;
        strip_stale_annot_refs(&ed, &top, &page_nums, 0);
    }
    for f in &fields {
        if !dest_ok(f) {
            ed.with_dict(f, |d| d.remove(b"A"));
        }
    }

    // Outlines.
    let mut marks = vec![false; doc.xref_len()];
    let (kept, direct) = match &outlines {
        Some(o) => strip_outlines(&ed, o, &dest_ok, &mut marks, 0).map_err(fail)?,
        None => (0, None),
    };
    // Direct /Outlines are shared with the new catalog, so it sees them
    // changed.
    if let Some(changed) = direct {
        root.put(b"Outlines", changed);
    }
    if kept == 0 {
        root.remove(b"Outlines");
    }
    if let Some(num) = root_num {
        doc.update_object(num, Object::Dict(Arc::new(root)));
    }
    Ok(())
}

/// `dest_is_valid`: an outline item, link or field whose destination is a
/// kept page or a kept name. A /Dest that is a name, not a string or an
/// array, counts as leading nowhere.
fn dest_is_valid(
    ed: &Editor,
    o: &Object,
    valid_page: &dyn Fn(Option<Object>) -> bool,
    names: &[Object],
) -> bool {
    let in_names = |s: &Object| {
        let text = |o: &Object| match o {
            Object::String(b) => b.split(|&c| c == 0).next().unwrap_or_default().to_vec(),
            _ => Vec::new(),
        };
        let wanted = text(s);
        names
            .iter()
            .step_by(2)
            .any(|k| text(&ed.resolve(k)) == wanted)
    };
    if let Some(action) = ed.get(o, b"A")
        && ed
            .get(&action, b"S")
            .is_some_and(|s| ed.resolve(&s).is_name(b"GoTo"))
    {
        let d = ed.get(&action, b"D").map(|d| ed.resolve(&d));
        let leads_nowhere = match &d {
            Some(Object::Array(_)) => !valid_page(d.as_ref().and_then(|d| ed.array_get(d, 0))),
            Some(s @ Object::String(_)) => !in_names(s),
            _ => false,
        };
        if leads_nowhere {
            return false;
        }
    }
    match ed.get(o, b"Dest") {
        None => true,
        Some(p) => match ed.resolve(&p) {
            s @ Object::String(_) => in_names(&s),
            _ => valid_page(ed.array_get(&p, 0)),
        },
    }
}

/// `pdf_parent_root`: the field at the top of `start`'s /Parent chain.
fn parent_root(ed: &Editor, start: &Object) -> Result<Object, String> {
    let mut node = start.clone();
    let mut slow = start.clone();
    let mut halfbeat = 11;
    while let Some(parent) = ed.get(&node, b"Parent") {
        node = parent;
        if same(&node, &slow) {
            return Err("format error: cycle in parent chain".into());
        }
        halfbeat -= 1;
        if halfbeat == 0 {
            slow = ed.get(&slow, b"Parent").unwrap_or(Object::Null);
            halfbeat = 2;
        }
    }
    Ok(node)
}

/// The same object, as MuPDF's pointer comparison of two values sees it.
fn same(a: &Object, b: &Object) -> bool {
    match (a, b) {
        (Object::Ref(x), Object::Ref(y)) => x.num == y.num,
        _ => a == b,
    }
}

/// `strip_stale_annot_refs`: drops the /Kids of `field` that lead to no
/// kept page, returning whether `field` itself should go.
fn strip_stale_annot_refs(ed: &Editor, field: &Object, page_nums: &[i32], depth: usize) -> bool {
    if depth > MAX_OUTLINE_DEPTH {
        return false;
    }
    match ed.get(field, b"Kids") {
        Some(_) => {
            let mut empty = false;
            ed.edit_array_in(field, b"Kids", |kids| {
                let mut j = 0;
                while j < kids.len() {
                    if strip_stale_annot_refs(ed, &kids[j].clone(), page_nums, depth + 1) {
                        kids.remove(j);
                    } else {
                        j += 1;
                    }
                }
                empty = kids.is_empty();
            });
            // /Kids that is not an array has no length.
            let len = ed
                .get(field, b"Kids")
                .map(|k| ed.resolve(&k))
                .as_ref()
                .and_then(Object::as_array)
                .map_or(0, <[Object]>::len);
            empty || len == 0
        }
        None => {
            let page = ed.get(field, b"P");
            let num = page.as_ref().and_then(Object::as_ref).map_or(0, |r| r.num);
            !page_nums.contains(&num)
        }
    }
}

/// `strip_outlines`: the outline items under `outlines` that lead to a kept
/// page, or have children that do; returns how many are left at this level,
/// and `outlines` changed when it is a direct dictionary.
fn strip_outlines(
    ed: &Editor,
    outlines: &Object,
    dest_ok: &dyn Fn(&Object) -> bool,
    marks: &mut Vec<bool>,
    depth: usize,
) -> Result<(usize, Option<Object>), String> {
    if !matches!(ed.resolve(outlines), Object::Dict(_)) {
        return Ok((0, None));
    }
    if depth > MAX_OUTLINE_DEPTH {
        return Err("format error: outlines nested too deeply".into());
    }
    if let Some(r) = outlines.as_ref()
        && let Some(mark) = usize::try_from(r.num).ok().and_then(|n| marks.get_mut(n))
        && r.num > 0
    {
        if *mark {
            return Err("format error: Cycle detected in outlines".into());
        }
        *mark = true;
    }
    let first = ed.get(outlines, b"First");
    let (nc, first, last) = match first {
        Some(f) if matches!(ed.resolve(&f), Object::Dict(_)) => {
            strip_outline(ed, &f, dest_ok, marks, depth)?
        }
        _ => (0, None, None),
    };
    let direct = if nc == 0 {
        ed.with_dict(outlines, |d| {
            d.remove(b"First");
            d.remove(b"Last");
            d.remove(b"Count");
        })
    } else {
        let old_count = ed
            .get(outlines, b"Count")
            .map_or(0, |c| ed.resolve(&c).to_int());
        ed.with_dict(outlines, |d| {
            d.put(b"First", first.unwrap_or(Object::Null));
            d.put(b"Last", last.unwrap_or(Object::Null));
            let n = nc as i64;
            d.put(b"Count", Object::Int(if old_count > 0 { n } else { -n }));
        })
    };
    Ok((nc, direct))
}

/// `strip_outline`: one level of siblings from `start`.
fn strip_outline(
    ed: &Editor,
    start: &Object,
    dest_ok: &dyn Fn(&Object) -> bool,
    marks: &mut Vec<bool>,
    depth: usize,
) -> Result<(usize, Option<Object>, Option<Object>), String> {
    let mut prev: Option<Object> = None;
    let mut first: Option<Object> = None;
    let mut current = Some(start.clone());
    let mut count = 0;
    while let Some(cur) = current {
        let (nc, _) = strip_outlines(ed, &cur, dest_ok, marks, depth + 1)?;
        if !dest_ok(&cur) {
            if nc == 0 {
                let next = ed.get(&cur, b"Next");
                let next_is_dict = next
                    .as_ref()
                    .is_some_and(|n| matches!(ed.resolve(n), Object::Dict(_)));
                if !next_is_dict {
                    if let Some(p) = &prev {
                        ed.with_dict(p, |d| d.remove(b"Next"));
                    }
                } else if let (Some(p), Some(n)) = (&prev, &next) {
                    ed.with_dict(p, |d| d.put(b"Next", n.clone()));
                    ed.with_dict(n, |d| d.put(b"Prev", p.clone()));
                } else if let Some(n) = &next {
                    ed.with_dict(n, |d| d.remove(b"Prev"));
                }
                current = next;
            } else {
                ed.with_dict(&cur, |d| {
                    d.remove(b"Dest");
                    d.remove(b"A");
                });
                current = ed.get(&cur, b"Next");
            }
        } else {
            if first.is_none() {
                first = Some(cur.clone());
            }
            current = ed.get(&cur, b"Next");
            prev = Some(cur);
            count += 1;
        }
    }
    Ok((count, first, prev))
}

/// `pdf_load_name_tree`: the name tree /Root/Names/`which` as one
/// dictionary, keys as names made from the string keys' text.
pub(crate) fn load_name_tree(doc: &Document, which: &[u8]) -> Option<Dict> {
    let ed = Editor { doc };
    let root = doc.trailer().get(b"Root").cloned()?;
    let names = ed.get(&root, b"Names")?;
    let tree = ed.get(&names, which)?;
    if !matches!(ed.resolve(&tree), Object::Dict(_)) {
        return None;
    }
    let mut dict = Dict::new();
    load_name_tree_node(&ed, &mut dict, &tree, &mut Vec::new());
    Some(dict)
}

fn load_name_tree_node(ed: &Editor, dict: &mut Dict, node: &Object, ancestors: &mut Vec<i32>) {
    let kids = ed.get(node, b"Kids");
    let names = ed.get(node, b"Names");
    let num = node.as_ref().map_or(0, |r| r.num);
    // pdf_cycle: a node that is its own ancestor is not entered again.
    let cycle = num > 0 && ancestors.contains(&num) || ancestors.len() > MAX_OUTLINE_DEPTH;
    if let Some(kids) = kids
        && !cycle
    {
        ancestors.push(num);
        let kids = ed.resolve(&kids);
        for kid in kids.as_array().unwrap_or_default() {
            load_name_tree_node(ed, dict, kid, ancestors);
        }
        ancestors.pop();
    }
    if let Some(names) = names {
        let names = ed.resolve(&names);
        let items = names.as_array().unwrap_or_default();
        let mut i = 0;
        while i + 1 < items.len() {
            match ed.resolve(&items[i]) {
                Object::String(s) => {
                    // pdf_new_name takes a C string.
                    let text = crate::text_string::text_string(&s);
                    let name = text
                        .as_bytes()
                        .split(|&b| b == 0)
                        .next()
                        .unwrap_or_default();
                    dict.put(name, items[i + 1].clone());
                }
                // A name as key is taken as it is.
                Object::Name(n) => dict.put(&n, items[i + 1].clone()),
                _ => {}
            }
            i += 2;
        }
    }
}

/// Reading and changing objects as MuPDF's `pdf_dict_get` and in-place
/// puts do, over the document's edits.
struct Editor<'a> {
    doc: &'a Document,
}

impl Editor<'_> {
    fn resolve(&self, o: &Object) -> Object {
        self.doc.resolve(o)
    }

    /// `pdf_dict_get`: the value, unresolved; a null value is none.
    fn get(&self, container: &Object, key: &[u8]) -> Option<Object> {
        let v = match self.resolve(container) {
            Object::Dict(d) => d.get(key).cloned(),
            Object::Stream(s) => s.dict.get(key).cloned(),
            _ => None,
        };
        v.filter(|v| !matches!(v, Object::Null))
    }

    /// The error `pdf_dict_put` throws when `value` is not a dictionary.
    fn require_dict(&self, value: &Object) -> Result<(), String> {
        let kind = match self.resolve(value) {
            Object::Dict(_) | Object::Stream(_) => return Ok(()),
            Object::Null => "null",
            Object::Bool(_) => "boolean",
            Object::Int(_) => "integer",
            Object::Real(_) => "real",
            Object::String(_) => "string",
            Object::Name(_) => "name",
            Object::Array(_) => "array",
            Object::Ref(_) => "reference",
        };
        Err(format!("argument error: not a dict ({kind})"))
    }

    /// `pdf_array_get`, resolving the array but not the item.
    fn array_get(&self, array: &Object, i: usize) -> Option<Object> {
        match self.resolve(array) {
            Object::Array(a) => a.get(i).cloned().filter(|v| !matches!(v, Object::Null)),
            _ => None,
        }
    }

    /// The object number a value leads to, following references.
    fn target(&self, value: &Object) -> Option<i32> {
        let mut num = None;
        let mut v = value.clone();
        for _ in 0..10 {
            match v {
                Object::Ref(r) => {
                    num = Some(r.num);
                    v = self.doc.object(r.num).unwrap_or(Object::Null);
                }
                _ => return num,
            }
        }
        None
    }

    /// Changes the dictionary `value` holds or leads to. A reference's
    /// object is replaced in the edits; a direct dictionary's changed copy
    /// is returned, for the caller to put back where it was.
    fn with_dict<R>(&self, value: &Object, f: impl FnOnce(&mut Dict) -> R) -> Option<Object> {
        match self.target(value) {
            Some(num) => {
                let obj = self.doc.object(num)?;
                let changed = match obj {
                    Object::Dict(d) => {
                        let mut d = (*d).clone();
                        f(&mut d);
                        Object::Dict(Arc::new(d))
                    }
                    Object::Stream(s) => {
                        let mut s = (*s).clone();
                        f(&mut s.dict);
                        Object::Stream(Arc::new(s))
                    }
                    _ => return None,
                };
                self.doc.alter_object(num, changed);
                None
            }
            None => match value {
                Object::Dict(d) => {
                    let mut d = (**d).clone();
                    f(&mut d);
                    Some(Object::Dict(Arc::new(d)))
                }
                _ => None,
            },
        }
    }

    /// Changes the array `container`'s `key` holds or leads to, in place:
    /// in its own object, or in the dictionary that holds it directly.
    fn edit_array_in(&self, container: &Object, key: &[u8], f: impl FnOnce(&mut Vec<Object>)) {
        let Some(value) = self.get(container, key) else {
            return;
        };
        if let Some(num) = self.target(&value) {
            if let Some(Object::Array(a)) = self.doc.object(num) {
                let mut a = (*a).clone();
                f(&mut a);
                self.doc.alter_object(num, Object::Array(Arc::new(a)));
            }
            return;
        }
        if let Object::Array(_) = value {
            let mut f = Some(f);
            self.with_dict(container, |d| {
                if let Some(Object::Array(a)) = d.get_mut(key)
                    && let Some(f) = f.take()
                {
                    f(Arc::make_mut(a));
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(bytes: &[u8]) -> Document {
        Document::open(bytes.to_vec()).unwrap_or_else(|e| panic!("{e:?}"))
    }

    #[test]
    fn page_ranges_parse_as_mupdf_does() {
        let doc = open(include_bytes!(
            "../../../corpus/smoke/multipage-inherit.pdf"
        ));
        let p = |s| parse_page_ranges(s, 10, &doc);
        assert_eq!(p("1-3"), [0, 1, 2]);
        assert_eq!(p("3-1"), [2, 1, 0]);
        assert_eq!(p("N"), [9]);
        assert_eq!(p("2,5-N"), [1, 4, 5, 6, 7, 8, 9]);
        assert_eq!(p("-2"), [8]);
        assert_eq!(p("0-99"), (0..10).collect::<Vec<_>>());
        assert_eq!(p("x"), Vec::<i32>::new());
    }

    const FIXTURE: &[u8] = include_bytes!("../../../corpus/smoke/pages-rearrange.pdf");

    fn kept(pages: &[i32]) -> (Document, Vec<u8>) {
        let doc = open(FIXTURE);
        crate::write::check_document(&doc);
        rearrange_pages(&doc, pages).unwrap_or_else(|e| panic!("{e}"));
        let out =
            crate::write::save(&doc, &crate::write::WriteOptions::default()).unwrap_or_default();
        (doc, out)
    }

    fn text_of(doc: &Document, num: i32) -> String {
        let o = doc.object(num).unwrap_or(Object::Null);
        crate::print::to_string(&o, crate::print::Options::default())
    }

    #[test]
    fn dropped_pages_take_their_links_outlines_and_names_along() {
        // Keep pages 1 and 2: links, outline items and names to 3 and 4 go.
        let (doc, out) = kept(&[0, 1]);
        assert!(
            crate::write::verify(&doc, &out, None, &crate::write::WriteOptions::default())
                .verified()
        );
        let root = text_of(&doc, 1);
        assert!(
            root.starts_with("<</Type/Catalog/Pages 2 0 R/Outlines 20 0 R/Names"),
            "{root}"
        );
        assert!(
            !root.contains("AcroForm") && !root.contains("Lang"),
            "{root}"
        );
        // Only /two leads to a kept page.
        assert!(root.contains("/Names[(two)32 0 R]"), "{root}");
        // The link by GoTo stays. (four) and page 3 go, and so does /two:
        // a /Dest that is a name counts as leading nowhere.
        assert!(
            text_of(&doc, 5).contains("/Annots[50 0 R 41 0 R]"),
            "{}",
            text_of(&doc, 5)
        );
        // Outline items One and Two stay; Three has no child left and goes,
        // as does Four.
        assert_eq!(
            text_of(&doc, 20),
            "<</Type/Outlines/First 21 0 R/Last 22 0 R/Count 2>>"
        );
        assert!(!text_of(&doc, 22).contains("/Next"));
        // The field keeps the widget on page 1, which loses its action.
        assert_eq!(text_of(&doc, 40), "<</FT/Tx/T(field)/Kids[41 0 R]>>");
        assert!(!text_of(&doc, 41).contains("/A<<"));
        // The catalog was replaced, so its entry has generation 0.
        assert!(String::from_utf8_lossy(&out).contains("\n1 0 obj\n"));
    }

    #[test]
    fn pages_can_repeat_and_reverse() {
        let (doc, _) = kept(&[3, 3, 0]);
        assert_eq!(doc.page_count().ok(), Some(3));
        assert_eq!(
            text_of(&doc, 2),
            "<</Type/Pages/Kids[8 0 R 8 0 R 5 0 R]/Count 3>>"
        );
        // Page 4 inherited its box and rotation from node 4.
        let four = text_of(&doc, 8);
        assert!(
            four.contains("/MediaBox[0 0 300 300]") && four.contains("/Rotate 90"),
            "{four}"
        );
        assert!(four.contains("/Parent 2 0 R"));
    }

    #[test]
    fn a_page_that_is_not_a_dictionary_fails() {
        let doc = open(
            b"%PDF-1.4\n1 0 obj\n<</Type/Catalog/Pages 2 0 R>>\nendobj\n\
2 0 obj\n<</Type/Pages/Kids[3 0 R]/Count 1>>\nendobj\n\
3 0 obj\n[1 2]\nendobj\n\
trailer\n<</Root 1 0 R>>\n",
        );
        let e = rearrange_pages(&doc, &[0])
            .err()
            .map(|e| e.0)
            .unwrap_or_default();
        assert!(e.contains("not a dict") || e.contains("page"), "{e}");
    }

    #[test]
    fn kept_pages_are_flattened_under_the_root() {
        let doc = open(include_bytes!(
            "../../../corpus/smoke/multipage-inherit.pdf"
        ));
        rearrange_pages(&doc, &[2, 0]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(doc.page_count().ok(), Some(2));
        let first = doc.page(0).unwrap_or(Object::Null);
        let ed = Editor { doc: &doc };
        assert!(ed.get(&first, b"MediaBox").is_some());
        assert!(ed.get(&first, b"Resources").is_some());
    }
}
