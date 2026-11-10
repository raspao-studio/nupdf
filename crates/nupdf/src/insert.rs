// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-page.c (pdf_insert_page and the page
// label adjustment) and source/pdf/pdf-repair.c (pdf_walk_parent),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Putting a page object into the page tree, as `pdf_insert_page` does:
//! into its parent's /Kids, with /Parent set, every /Count above it
//! raised, and the page labels after it shifted.

use std::sync::Arc;

use crate::document::Document;
use crate::edit::{Loc, Step, array_mut, dict_mut};
use crate::object::{Dict, ObjRef, Object};

/// Page label trees deeper than this are refused; MuPDF recurses without
/// a bound.
const MAX_LABEL_DEPTH: usize = 256;

/// `pdf_insert_page`: puts page object `page` into the page tree before
/// page `at` (0-based), or after the last page when `at` is negative or
/// the page count.
pub fn insert_page(doc: &Document, at: i32, page: i32) -> Result<(), String> {
    let count = doc.page_count().map_err(|e| e.0)?;
    let at = if at < 0 || at == i32::MAX { count } else { at };
    if at > count {
        return Err("cannot insert page beyond end of page tree".into());
    }
    let root = Loc::trailer().child(doc, Step::Key(b"Root"));
    let pages = root
        .as_ref()
        .and_then(|r| r.child(doc, Step::Key(b"Pages")));
    let (parent, index) = if count == 0 {
        let parent = pages.ok_or("cannot find page tree")?;
        if parent.child(doc, Step::Key(b"Kids")).is_none() {
            return Err("malformed page tree".into());
        }
        (parent, 0)
    } else {
        let needle = if at == count { count - 1 } else { at };
        let path = doc.page_location(needle).map_err(|e| e.0)?;
        let (&last, up) = path.split_last().ok_or("cannot find page tree")?;
        let mut parent = pages.ok_or("cannot find page tree")?;
        for &i in up {
            parent = parent
                .child(doc, Step::Key(b"Kids"))
                .and_then(|k| k.child(doc, Step::Index(i)))
                .ok_or("cannot find page tree")?;
        }
        (parent, if at == count { last + 1 } else { last })
    };

    let kids = parent
        .child(doc, Step::Key(b"Kids"))
        .ok_or("malformed page tree")?;
    let page_ref = Object::Ref(ObjRef::new(page, 0));
    kids.edit(doc, |k| {
        let k = array_mut(k)?;
        if index > k.len() {
            return Err("index out of bounds".to_string());
        }
        k.insert(index, page_ref.clone());
        Ok(())
    })?;
    let parent_value = parent.value(doc).unwrap_or(Object::Null);
    let mut page_obj = doc.object(page).ok_or("cannot find page object")?;
    dict_mut(&mut page_obj)?.put(b"Parent", parent_value);
    doc.alter_object(page, page_obj);

    adjust_counts(doc, parent, 1)?;
    adjust_page_labels(doc, at, 1)
}

/// `pdf_walk_parent` with `adjust_count`: /Count on `start` and each
/// node above it changes by `adjust`. A cycle in /Parent is found the way
/// MuPDF finds it, a slow pointer moving every other step after eleven,
/// so the counts it changed before noticing stay changed.
fn adjust_counts(doc: &Document, start: Loc, adjust: i32) -> Result<(), String> {
    let mut node = start.clone();
    let mut slow = start;
    let mut halfbeat = 11;
    loop {
        node.edit(doc, |n| {
            let d = dict_mut(n)?;
            // pdf_dict_get_int and pdf_dict_put_int: C ints.
            let count = d.get(b"Count").map_or(0, |c| doc.resolve(c).to_int());
            d.put(b"Count", Object::Int(i64::from(count.wrapping_add(adjust))));
            Ok(())
        })?;
        let Some(next) = node.child(doc, Step::Key(b"Parent")) else {
            return Ok(());
        };
        if next == slow {
            return Err("cycle in parent chain".into());
        }
        halfbeat -= 1;
        if halfbeat == 0 {
            slow = slow
                .child(doc, Step::Key(b"Parent"))
                .ok_or("cycle in parent chain")?;
            halfbeat = 2;
        }
        node = next;
    }
}

/// The number tree entry that decides a page's label: where the range
/// starts, the /Nums array holding it, and its index there.
struct LabelRange {
    offset: i32,
    nums_ix: usize,
    nums: Option<Loc>,
}

/// `pdf_adjust_page_labels`: after `adjust` pages were inserted at (or,
/// when negative, deleted from) `index`, the label ranges after it move.
fn adjust_page_labels(doc: &Document, index: i32, adjust: i32) -> Result<(), String> {
    let root = Loc::trailer().child(doc, Step::Key(b"Root"));
    let labels = root
        .as_ref()
        .and_then(|r| r.child(doc, Step::Key(b"PageLabels")));
    // Without labels nothing moves, unless the insertion is at page 0 of
    // a document of more than one page, which must start a range there.
    let count = doc.page_count().map_err(|e| e.0)?;
    if labels.is_none() && !(adjust > 0 && index == 0 && count > 1) {
        return Ok(());
    }
    let root = root.ok_or("not a dict (<NULL>)")?;
    flatten_page_labels(doc, &root)?;
    let labels = root
        .child(doc, Step::Key(b"PageLabels"))
        .ok_or("cannot find page labels")?;
    let mut range = LabelRange {
        offset: 0,
        nums_ix: 0,
        nums: labels.child(doc, Step::Key(b"Nums")),
    };
    lookup_page_label(doc, &labels, index, &mut range, 0)?;
    let Some(nums) = range.nums else {
        return Ok(());
    };
    let start = if adjust > 0 && range.offset != index {
        range.nums_ix + 2
    } else if adjust > 0 {
        range.nums_ix
    } else {
        range.nums_ix + 2
    };
    nums.edit(doc, |n| {
        let n = array_mut(n)?;
        let mut i = start;
        while i < n.len() {
            let k = doc.resolve(&n[i]).to_int();
            n[i] = Object::Int(i64::from(k.wrapping_add(adjust)));
            i += 2;
        }
        // The number tree always has an entry for page 0.
        if adjust > 0 && index == 0 {
            n.insert(0, Object::Int(i64::from(index)));
            n.insert(1, decimal_label());
        }
        Ok(())
    })
}

/// `pdf_create_page_label` for decimal numbers from 1.
fn decimal_label() -> Object {
    let mut d = Dict::new();
    d.put(b"S", Object::name(b"D"));
    Object::Dict(Arc::new(d))
}

/// `pdf_flatten_page_label_tree`: the labels as one /Nums array of at
/// least one entry, made from the tree's in order.
fn flatten_page_labels(doc: &Document, root: &Loc) -> Result<(), String> {
    let labels = root.child(doc, Step::Key(b"PageLabels"));
    if let Some(Object::Array(nums)) = labels
        .as_ref()
        .and_then(|l| l.child(doc, Step::Key(b"Nums")))
        .and_then(|n| n.resolved(doc))
        && nums.len() >= 2
    {
        return Ok(());
    }
    let labels = match labels {
        Some(l) => l,
        None => {
            root.edit(doc, |r| {
                dict_mut(r)?.put(b"PageLabels", Object::Dict(Arc::new(Dict::new())));
                Ok(())
            })?;
            root.child(doc, Step::Key(b"PageLabels"))
                .ok_or("cannot find page labels")?
        }
    };
    let mut nums = Vec::new();
    gather_labels(
        doc,
        &labels.resolved(doc).unwrap_or(Object::Null),
        &mut nums,
        0,
    )?;
    if nums.is_empty() {
        nums.push(Object::Int(0));
        nums.push(decimal_label());
    }
    labels.edit(doc, |l| {
        let l = dict_mut(l)?;
        l.remove(b"Kids");
        l.remove(b"Limits");
        l.put(b"Nums", Object::Array(Arc::new(nums)));
        Ok(())
    })
}

/// `pdf_flatten_page_label_tree_imp`: every node's /Kids first, then its
/// /Nums pairs.
fn gather_labels(
    doc: &Document,
    node: &Object,
    out: &mut Vec<Object>,
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_LABEL_DEPTH {
        return Err("page label tree too deep".into());
    }
    let get = |key: &[u8]| {
        node.as_dict()
            .and_then(|d| d.get(key))
            .map(|v| doc.resolve(v))
    };
    if let Some(Object::Array(kids)) = get(b"Kids") {
        for kid in kids.iter() {
            gather_labels(doc, &doc.resolve(kid), out, depth + 1)?;
        }
    }
    if let Some(Object::Array(nums)) = get(b"Nums") {
        let mut i = 0;
        while i < nums.len() {
            out.push(nums[i].clone());
            out.push(nums.get(i + 1).cloned().unwrap_or(Object::Null));
            i += 2;
        }
    }
    Ok(())
}

/// `pdf_lookup_page_label_imp`: the last entry at or before `index`,
/// stopping at the first /Nums entry after it.
fn lookup_page_label(
    doc: &Document,
    node: &Loc,
    index: i32,
    range: &mut LabelRange,
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_LABEL_DEPTH {
        return Err("page label tree too deep".into());
    }
    if let Some(kids) = node.child(doc, Step::Key(b"Kids"))
        && let Some(Object::Array(items)) = kids.resolved(doc)
    {
        for i in 0..items.len() {
            if let Some(kid) = kids.child(doc, Step::Index(i)) {
                lookup_page_label(doc, &kid, index, range, depth + 1)?;
            }
        }
    }
    if let Some(nums) = node.child(doc, Step::Key(b"Nums"))
        && let Some(Object::Array(items)) = nums.resolved(doc)
    {
        let mut i = 0;
        while i < items.len() {
            let k = doc.resolve(&items[i]).to_int();
            if k > index {
                return Ok(());
            }
            range.offset = k;
            range.nums_ix = i;
            range.nums = Some(nums.clone());
            i += 2;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::print::{Options, to_string};

    fn open(bytes: &[u8]) -> Document {
        Document::open(bytes.to_vec()).unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn printed(doc: &Document, num: i32) -> String {
        to_string(&doc.object(num).unwrap_or(Object::Null), Options::default())
    }

    fn new_page(doc: &Document) -> i32 {
        let mut d = Dict::new();
        d.put(b"Type", Object::name(b"Page"));
        doc.add_object(Object::Dict(Arc::new(d)))
    }

    // Each expectation is what MuPDF's insertPage leaves, printed.

    #[test]
    fn inserting_first_starts_the_page_labels() {
        let doc = open(include_bytes!(
            "../../../corpus/smoke/multipage-inherit.pdf"
        ));
        let page = new_page(&doc);
        insert_page(&doc, 0, page).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            printed(&doc, 1),
            "<</Type/Catalog/Pages 2 0 R/PageLabels<</Nums[0<</S/D>>1<</S/D>>]>>>>"
        );
        assert_eq!(
            printed(&doc, 2),
            "<</Type/Pages/Kids[8 0 R 3 0 R 4 0 R 5 0 R]/Count 4/MediaBox[0 0 595 842]/Resources<</Font<</F1 7 0 R>>>>>>"
        );
        assert_eq!(printed(&doc, page), "<</Type/Page/Parent 2 0 R>>");
    }

    #[test]
    fn appending_needs_no_labels() {
        let doc = open(include_bytes!(
            "../../../corpus/smoke/multipage-inherit.pdf"
        ));
        let page = new_page(&doc);
        insert_page(&doc, -1, page).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(printed(&doc, 1), "<</Type/Catalog/Pages 2 0 R>>");
        assert_eq!(
            printed(&doc, 2),
            "<</Type/Pages/Kids[3 0 R 4 0 R 5 0 R 8 0 R]/Count 4/MediaBox[0 0 595 842]/Resources<</Font<</F1 7 0 R>>>>>>"
        );
        assert_eq!(
            insert_page(&doc, 9, page),
            Err("cannot insert page beyond end of page tree".to_string())
        );
    }

    #[test]
    fn every_count_above_a_nested_insertion_rises() {
        let doc = open(include_bytes!("../../../corpus/smoke/pages-rearrange.pdf"));
        let page = new_page(&doc);
        insert_page(&doc, 2, page).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            printed(&doc, 2),
            "<</Type/Pages/Kids[3 0 R 4 0 R]/Count 5>>"
        );
        assert_eq!(
            printed(&doc, 4),
            "<</Type/Pages/Parent 2 0 R/Kids[55 0 R 7 0 R 8 0 R]/Count 3/MediaBox[0 0 300 300]/Rotate 90/Resources<</Font<</F1 9 0 R>>>>>>"
        );
        assert_eq!(printed(&doc, page), "<</Type/Page/Parent 4 0 R>>");
        assert_eq!(doc.page(2), Ok(Object::Ref(ObjRef::new(page, 0))));
    }

    #[test]
    fn a_new_document_takes_its_first_page() {
        let doc = Document::create().unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(doc.page_count(), Ok(0));
        let page = new_page(&doc);
        insert_page(&doc, 0, page).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(printed(&doc, 2), "<</Type/Pages/Count 1/Kids[3 0 R]>>");
        assert_eq!(doc.page_count(), Ok(1));
    }
}
