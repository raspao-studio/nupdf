// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-outline.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! MuPDF's outline iterator: walking a document's outline items, and
//! inserting new ones, as `pdf_new_outline_iterator` and its methods do.
//! Creating the iterator checks the tree first, repairing /Parent, /Prev
//! and /Last pointers that disagree with the /First and /Next chains.

use std::sync::Arc;

use crate::document::Document;
use crate::link::{new_action_from_link, new_dest_from_link, parse_link_action, parse_link_dest};
use crate::object::{Dict, ObjRef, Object};

/// Outline trees deeper than this are refused; MuPDF recurses without a
/// bound.
const MAX_DEPTH: usize = 256;

/// What an item says: its title, where it leads, and whether it is open.
/// MuPDF's colour and flags are left out; `mutool merge` copies neither.
#[derive(Debug, Clone, PartialEq)]
pub struct OutlineItem {
    pub title: Option<String>,
    pub uri: Option<String>,
    pub is_open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modifier {
    None,
    /// Below `current`, which has no children yet.
    Below,
    /// After `current`, the last of its siblings.
    After,
}

/// `pdf_outline_iterator`.
#[derive(Debug)]
pub struct OutlineIter<'a> {
    doc: &'a Document,
    current: Option<Object>,
    modifier: Modifier,
}

fn get(doc: &Document, obj: &Object, key: &[u8]) -> Option<Object> {
    match doc.resolve(obj) {
        Object::Dict(d) => d.get(key).cloned(),
        _ => None,
    }
    .filter(|v| !matches!(v, Object::Null))
}

/// `pdf_objcmp` == 0, for the references and nulls an outline links with.
fn same(a: Option<&Object>, b: Option<&Object>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Object::Ref(x)), Some(Object::Ref(y))) => {
            x.num == y.num && x.generation == y.generation
        }
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// `pdf_dict_put` on the object a reference leads to.
fn put(doc: &Document, obj: &Object, key: &[u8], value: Option<Object>) -> Result<(), String> {
    let Object::Ref(r) = obj else {
        return Err("outline item is not an indirect object".into());
    };
    let mut d = match doc.object(r.num) {
        Some(Object::Dict(d)) => (*d).clone(),
        Some(other) => return Err(format!("not a dict ({})", crate::edit::kind(&other))),
        None => return Err("not a dict (null)".into()),
    };
    match value {
        Some(v) => d.put(key, v),
        None => d.remove(key),
    }
    doc.alter_object(r.num, Object::Dict(Arc::new(d)));
    Ok(())
}

fn int(doc: &Document, obj: &Object, key: &[u8]) -> i32 {
    get(doc, obj, key).map_or(0, |v| doc.resolve(&v).to_int())
}

impl<'a> OutlineIter<'a> {
    /// `pdf_new_outline_iterator`: at the first top-level item, after
    /// checking the tree and repairing its back pointers.
    pub fn new(doc: &'a Document) -> Result<OutlineIter<'a>, String> {
        let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
        let outlines = get(doc, &root, b"Outlines");
        let first = outlines.as_ref().and_then(|o| get(doc, o, b"First"));
        if let (Some(first), Some(outlines)) = (&first, &outlines) {
            let mut fixed = false;
            test_outline(doc, first, &mut Vec::new(), outlines, Some(&mut fixed), 0)?;
            if fixed {
                doc.diagnostics().warn(
                    "outline.repaired",
                    "repaired broken tree structure in outline",
                );
                test_outline(doc, first, &mut Vec::new(), outlines, None, 0)?;
            }
        }
        Ok(OutlineIter {
            doc,
            current: first,
            modifier: Modifier::None,
        })
    }

    /// `pdf_outline_iterator_next`: 0 when it moved, 1 when it is now past
    /// the last sibling, -1 when it cannot move.
    pub fn next_sibling(&mut self) -> i32 {
        let Some(current) = self
            .current
            .as_ref()
            .filter(|_| self.modifier == Modifier::None)
        else {
            return -1;
        };
        match get(self.doc, current, b"Next") {
            None => {
                self.modifier = Modifier::After;
                1
            }
            Some(next) => {
                self.current = Some(next);
                0
            }
        }
    }

    /// `pdf_outline_iterator_prev`.
    pub fn prev_sibling(&mut self) -> i32 {
        let Some(current) = self.current.as_ref() else {
            return -1;
        };
        match self.modifier {
            Modifier::Below => -1,
            Modifier::After => {
                self.modifier = Modifier::None;
                0
            }
            Modifier::None => match get(self.doc, current, b"Prev") {
                None => -1,
                Some(prev) => {
                    self.current = Some(prev);
                    0
                }
            },
        }
    }

    /// `pdf_outline_iterator_up`: never onto the outlines dictionary.
    pub fn up(&mut self) -> i32 {
        let Some(current) = self.current.as_ref() else {
            return -1;
        };
        if self.modifier == Modifier::Below {
            self.modifier = Modifier::None;
            return 0;
        }
        let Some(up) = get(self.doc, current, b"Parent") else {
            return -1;
        };
        if get(self.doc, &up, b"Parent").is_none() {
            return -1;
        }
        self.modifier = Modifier::None;
        self.current = Some(up);
        0
    }

    /// `pdf_outline_iterator_down`: 0 at the first child, 1 below an item
    /// that has none.
    pub fn down(&mut self) -> i32 {
        let Some(current) = self
            .current
            .as_ref()
            .filter(|_| self.modifier == Modifier::None)
        else {
            return -1;
        };
        match get(self.doc, current, b"First") {
            None => {
                self.modifier = Modifier::Below;
                1
            }
            Some(down) => {
                self.current = Some(down);
                0
            }
        }
    }

    /// `pdf_outline_iterator_item`: the item here, its target as a URI.
    pub fn item(&self) -> Result<Option<OutlineItem>, String> {
        let Some(current) = self
            .current
            .as_ref()
            .filter(|_| self.modifier == Modifier::None)
        else {
            return Ok(None);
        };
        let doc = self.doc;
        let title = get(doc, current, b"Title").map(|t| match doc.resolve(&t) {
            Object::String(s) => crate::text_string::text_string(&s),
            _ => String::new(),
        });
        let uri = match get(doc, current, b"Dest") {
            Some(dest) => parse_link_dest(doc, Some(&dest))?,
            None => match get(doc, current, b"A") {
                Some(a) => parse_link_action(doc, Some(&a), -1)?,
                None => None,
            },
        };
        Ok(Some(OutlineItem {
            title,
            uri,
            is_open: int(doc, current, b"Count") > 0,
        }))
    }

    /// `pdf_outline_iterator_insert`: a new item here, before the current
    /// one, or as the first child or the next sibling where the iterator
    /// is below or after an item. Returns 1 when the iterator is then at
    /// the new item, 0 when it stays where it was.
    pub fn insert(&mut self, item: &OutlineItem) -> Result<i32, String> {
        let doc = self.doc;
        let num = doc.create_object();
        doc.update_object(num, Object::Dict(Arc::new(Dict::new())));
        let obj = Object::Ref(ObjRef::new(num, 0));
        let parent = match (self.modifier, &self.current) {
            (Modifier::Below, Some(current)) => current.clone(),
            (Modifier::None, None) => {
                let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
                let outlines = match get(doc, &root, b"Outlines") {
                    Some(o) => o,
                    None => {
                        let n = doc.create_object();
                        doc.update_object(n, Object::Dict(Arc::new(Dict::new())));
                        let o = Object::Ref(ObjRef::new(n, 0));
                        put(doc, &root, b"Outlines", Some(o.clone()))?;
                        put(doc, &o, b"Type", Some(Object::name(b"Outlines")))?;
                        o
                    }
                };
                self.modifier = Modifier::Below;
                self.current = Some(outlines.clone());
                outlines
            }
            (_, Some(current)) => get(doc, current, b"Parent").unwrap_or(Object::Null),
            (_, None) => return Err("cannot insert an outline item here".into()),
        };
        put(doc, &obj, b"Parent", Some(parent.clone()))?;
        update(doc, &obj, item, true)?;
        let current = self.current.clone().unwrap_or(Object::Null);
        match self.modifier {
            Modifier::Below => {
                put(doc, &current, b"First", Some(obj.clone()))?;
                put(doc, &current, b"Last", Some(obj.clone()))?;
                self.current = Some(obj);
                self.modifier = Modifier::After;
                Ok(1)
            }
            Modifier::After => {
                put(doc, &obj, b"Prev", Some(current.clone()))?;
                put(doc, &current, b"Next", Some(obj.clone()))?;
                put(doc, &parent, b"Last", Some(obj.clone()))?;
                self.current = Some(obj);
                Ok(1)
            }
            Modifier::None => {
                match get(doc, &current, b"Prev") {
                    Some(prev) => {
                        put(doc, &prev, b"Next", Some(obj.clone()))?;
                        put(doc, &obj, b"Prev", Some(prev))?;
                    }
                    None => put(doc, &parent, b"First", Some(obj.clone()))?,
                }
                put(doc, &current, b"Prev", Some(obj.clone()))?;
                put(doc, &obj, b"Next", Some(current))?;
                Ok(0)
            }
        }
    }
}

/// `do_outline_update`: the item's open state, title and target on `obj`,
/// and the /Count of every node above it adjusted, as `pdf_walk_parent`
/// with `upd` adjusts them.
fn update(doc: &Document, obj: &Object, item: &OutlineItem, is_new: bool) -> Result<(), String> {
    let count = int(doc, obj, b"Count");
    let mut open_delta = 0;
    if (count < 0 && item.is_open) || (count > 0 && !item.is_open) {
        put(doc, obj, b"Count", Some(Object::Int(i64::from(-count))))?;
        open_delta = -count;
    } else if is_new {
        open_delta = 1;
    }

    // pdf_walk_parent with upd: a node with no /Count gets one; counting
    // stops above the first closed node.
    let mut node = get(doc, obj, b"Parent");
    let mut slow = node.clone();
    let mut halfbeat = 11;
    while let Some(n) = node {
        let cobj = get(doc, &n, b"Count");
        let c = cobj.as_ref().map_or(0, |v| doc.resolve(v).to_int());
        if open_delta != 0 || cobj.is_none() {
            let v = if c > 0 {
                c.wrapping_add(open_delta)
            } else {
                c.wrapping_sub(open_delta)
            };
            put(doc, &n, b"Count", Some(Object::Int(i64::from(v))))?;
        }
        if c < 0 {
            break;
        }
        node = get(doc, &n, b"Parent");
        if node.is_some() && same(node.as_ref(), slow.as_ref()) {
            return Err("cycle in parent chain".into());
        }
        halfbeat -= 1;
        if halfbeat == 0 {
            slow = slow.and_then(|s| get(doc, &s, b"Parent"));
            halfbeat = 2;
        }
    }

    match &item.title {
        Some(t) => put(doc, obj, b"Title", Some(crate::edit::text_string(t)))?,
        None => put(doc, obj, b"Title", None)?,
    }
    for key in [&b"A"[..], b"C", b"F", b"Dest"] {
        put(doc, obj, key, None)?;
    }
    if let Some(uri) = &item.uri {
        if uri.starts_with('#') {
            put(
                doc,
                obj,
                b"Dest",
                Some(new_dest_from_link(doc, uri, false)?),
            )?;
        } else if uri.starts_with("file:") {
            put(doc, obj, b"Dest", Some(new_dest_from_link(doc, uri, true)?))?;
        } else {
            put(doc, obj, b"A", Some(new_action_from_link(doc, uri)?))?;
        }
    }
    Ok(())
}

/// `pdf_test_outline`: each item must be an indirect dictionary met once,
/// whose /Parent, /Prev and its parent's /Last agree with the chains from
/// /First and /Next. With `fixed`, disagreements are repaired; without,
/// they fail.
fn test_outline(
    doc: &Document,
    first: &Object,
    marks: &mut Vec<i32>,
    expected_parent: &Object,
    mut fixed: Option<&mut bool>,
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("outline tree too deep".into());
    }
    let last = get(doc, expected_parent, b"Last");
    let mut expected_prev: Option<Object> = None;
    let mut dict = Some(first.clone());
    while let Some(d) = dict.filter(|d| matches!(doc.resolve(d), Object::Dict(_))) {
        let Object::Ref(r) = &d else {
            return Err("Non-indirect outline entry discovered".into());
        };
        if marks.contains(&r.num) {
            return Err("Cycle detected in outlines".into());
        }
        marks.push(r.num);
        let parent = get(doc, &d, b"Parent");
        let prev = get(doc, &d, b"Prev");
        let next = get(doc, &d, b"Next");
        let parent_diff = !same(parent.as_ref(), Some(expected_parent));
        let prev_diff = !same(prev.as_ref(), expected_prev.as_ref());
        let last_diff = next.is_none() && !same(last.as_ref(), Some(&d));
        match fixed.as_deref_mut() {
            None => {
                if parent_diff {
                    return Err("Outline parent pointer still bad or missing despite repair".into());
                }
                if prev_diff {
                    return Err("Outline prev pointer still bad or missing despite repair".into());
                }
                if last_diff {
                    return Err("Outline last pointer still bad or missing despite repair".into());
                }
            }
            Some(fixed) if parent_diff || prev_diff || last_diff => {
                *fixed = true;
                if parent_diff {
                    put(doc, &d, b"Parent", Some(expected_parent.clone()))?;
                }
                if prev_diff {
                    put(doc, &d, b"Prev", expected_prev.clone())?;
                }
                if last_diff {
                    put(doc, expected_parent, b"Last", Some(d.clone()))?;
                }
            }
            Some(_) => {}
        }
        if let Some(child) = get(doc, &d, b"First") {
            test_outline(doc, &child, marks, &d, fixed.as_deref_mut(), depth + 1)?;
        }
        expected_prev = Some(d);
        dict = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_read_as_mupdf_reads_them() {
        // What MuPDF's outline iterator gives, depth first.
        let doc = Document::open(&include_bytes!("../../../corpus/smoke/merge-links.pdf")[..])
            .unwrap_or_else(|e| panic!("{e:?}"));
        let mut it = OutlineIter::new(&doc).unwrap_or_else(|e| panic!("{e}"));
        let mut got = Vec::new();
        fn walk(
            it: &mut OutlineIter,
            depth: usize,
            got: &mut Vec<(usize, String, Option<String>, bool)>,
        ) {
            loop {
                if let Ok(Some(item)) = it.item() {
                    got.push((
                        depth,
                        item.title.unwrap_or_default(),
                        item.uri,
                        item.is_open,
                    ));
                }
                if it.down() == 0 {
                    walk(it, depth + 1, got);
                }
                it.up();
                if it.next_sibling() != 0 {
                    return;
                }
            }
        }
        walk(&mut it, 0, &mut got);
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            got,
            [
                (0, "One".into(), s("#page=1&zoom=150,0,0"), true),
                (
                    1,
                    "One, child".into(),
                    s("#page=3&viewrect=2,320,59,76"),
                    false
                ),
                (1, "One, web".into(), s("https://example.com"), false),
                (0, "Two".into(), s("#nameddest=dest2"), false),
                (0, "Three".into(), None, false),
                (1, "Three, last".into(), s("#page=3"), false),
                (0, "Four".into(), s("#nameddest=intro"), false),
                (0, "Five".into(), s("#page=2&view=FitBH,100.5"), false),
            ]
        );
    }

    #[test]
    fn inserting_builds_a_tree_with_mupdf_counts() {
        let doc = Document::create().unwrap_or_else(|e| panic!("{e:?}"));
        let mut it = OutlineIter::new(&doc).unwrap_or_else(|e| panic!("{e}"));
        let item = |t: &str| OutlineItem {
            title: Some(t.into()),
            uri: Some("https://example.com".into()),
            is_open: true,
        };
        assert_eq!(it.insert(&item("a")), Ok(1));
        assert_eq!(it.insert(&item("b")), Ok(1));
        it.prev_sibling();
        it.down();
        assert_eq!(it.insert(&item("b1")), Ok(1));
        let print = |num| {
            crate::print::to_string(&doc.object(num).unwrap_or(Object::Null), Default::default())
        };
        // As MuPDF's insert leaves them: a new node below one with no
        // /Count closes it (0 - 1), and each insertion reaches the root.
        assert_eq!(
            print(4),
            "<</Type/Outlines/Count -3/First 3 0 R/Last 5 0 R>>"
        );
        assert_eq!(
            print(5),
            "<</Parent 4 0 R/Title(b)/A<</S/URI/URI(https://example.com)>>/Prev 3 0 R/Count -1/First 6 0 R/Last 6 0 R>>"
        );
    }
}
