// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-xref.c (pdf_set_metadata) and
// source/pdf/pdf-parse.c (pdf_new_text_string),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Small edits over the document's overlay: the document information
//! dictionary as `pdf_set_metadata` changes it, removing metadata, and
//! rotating pages.

use std::sync::Arc;

use crate::document::Document;
use crate::object::{Dict, Object};

/// `pdf_new_text_string`: the text as is when it is ASCII, else UTF-16BE
/// after a byte order mark.
pub fn text_string(s: &str) -> Object {
    if s.is_ascii() {
        return Object::String(Arc::from(s.as_bytes()));
    }
    let mut out = vec![0xfe, 0xff];
    for unit in s.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    Object::String(Arc::from(out))
}

/// `pdf_set_metadata(doc, key, value)`: the /Info entry `key`, with an
/// `info:` prefix or without, set to `value` as a text string. An /Info
/// that is not a dictionary is replaced by a new object first. Dates are
/// stored as the text given: MuPDF stores the parsed date, then the text
/// over it.
pub fn set_metadata(doc: &Document, key: &str, value: &str) {
    let mut trailer = doc.trailer();
    let info = trailer.get(b"Info").cloned().unwrap_or(Object::Null);
    let info = match doc.resolve(&info) {
        Object::Dict(_) => info,
        _ => {
            let num = doc.add_object(Object::Dict(Arc::new(Dict::new())));
            let r = Object::Ref(crate::object::ObjRef::new(num, 0));
            trailer.put(b"Info", r.clone());
            doc.set_trailer(trailer.clone());
            r
        }
    };
    let key = key.strip_prefix("info:").unwrap_or(key);
    // pdf_new_name takes a C string.
    let name = key.split('\0').next().unwrap_or_default();
    let value = text_string(value.split('\0').next().unwrap_or_default());
    match info {
        Object::Ref(r) => {
            if let Object::Dict(d) = doc.resolve(&info) {
                let mut d = (*d).clone();
                d.put(name.as_bytes(), value);
                doc.alter_object(r.num, Object::Dict(Arc::new(d)));
            }
        }
        Object::Dict(d) => {
            let mut d = (*d).clone();
            d.put(name.as_bytes(), value);
            trailer.put(b"Info", Object::Dict(Arc::new(d)));
            doc.set_trailer(trailer);
        }
        _ => {}
    }
}

/// Removes the trailer's /Info and the catalog's /Metadata, as
/// `pdf_dict_del` removes them. Each changes its object even with nothing
/// to remove, as MuPDF's does, which shows in an incremental save: the
/// catalog, and an xref stream whose dictionary is the trailer, which
/// MuPDF's first edit to the trailer moves into the new section.
pub fn strip_metadata(doc: &Document) {
    if let Some(owner) = doc.trailer_owner()
        && let Some(obj) = doc.object(owner)
    {
        doc.alter_object(owner, obj);
    }
    let mut trailer = doc.trailer();
    if trailer.get(b"Info").is_some() {
        trailer.remove(b"Info");
        doc.set_trailer(trailer.clone());
    }
    match trailer.get(b"Root").cloned() {
        Some(Object::Ref(r)) => {
            if let Object::Dict(d) = doc.resolve(&Object::Ref(r)) {
                let mut d = (*d).clone();
                d.remove(b"Metadata");
                doc.alter_object(r.num, Object::Dict(Arc::new(d)));
            }
        }
        Some(Object::Dict(d)) if d.get(b"Metadata").is_some() => {
            let mut d = (*d).clone();
            d.remove(b"Metadata");
            trailer.put(b"Root", Object::Dict(Arc::new(d)));
            doc.set_trailer(trailer);
        }
        _ => {}
    }
}

/// Turns each page in `pages` (0-based, possibly repeated) by `degrees`,
/// clockwise: its /Rotate becomes the inherited value plus `degrees`, in
/// 0 to 359, put on the page itself. A page that is a direct dictionary is
/// changed where it sits in its parent's /Kids, as MuPDF changes it.
pub fn rotate_pages(doc: &Document, pages: &[i32], degrees: i32) -> Result<(), String> {
    for &index in pages {
        let page = doc.page(index).map_err(|e| e.0)?;
        let current = doc
            .inherited(&page, b"Rotate")
            .map_err(|e| e.0)?
            .map_or(0, |r| doc.resolve(&r).to_int());
        let rotate = (i64::from(current) + i64::from(degrees)).rem_euclid(360);
        let mut put = |d: &mut Dict| d.put(b"Rotate", Object::Int(rotate));
        let not_dict = || format!("not a dict (page {})", index + 1);
        match page {
            Object::Ref(r) => {
                let Object::Dict(d) = doc.resolve(&Object::Ref(r)) else {
                    return Err(not_dict());
                };
                let mut d = (*d).clone();
                put(&mut d);
                doc.alter_object(r.num, Object::Dict(Arc::new(d)));
            }
            _ => {
                let path = doc.page_path(index).ok_or_else(not_dict)?;
                edit_direct_page(doc, &path, &mut put).ok_or_else(not_dict)?;
            }
        }
    }
    Ok(())
}

/// `f` applied to page `index` (0-based), as `pdf_dict_put` on the page
/// object changes it: in place, or where a direct page sits in its
/// parent's /Kids.
pub fn edit_page(doc: &Document, index: i32, f: &mut dyn FnMut(&mut Dict)) -> Result<(), String> {
    let page = doc.page(index).map_err(|e| e.0)?;
    let not_dict = || format!("not a dict (page {})", index + 1);
    match page {
        Object::Ref(r) => {
            let Object::Dict(d) = doc.resolve(&Object::Ref(r)) else {
                return Err(not_dict());
            };
            let mut d = (*d).clone();
            f(&mut d);
            doc.alter_object(r.num, Object::Dict(Arc::new(d)));
        }
        _ => {
            let path = doc.page_path(index).ok_or_else(not_dict)?;
            edit_direct_page(doc, &path, f).ok_or_else(not_dict)?;
        }
    }
    Ok(())
}

/// Changes the page `path` leads to from the root /Pages node, in the
/// object that holds it.
fn edit_direct_page(doc: &Document, path: &[usize], f: &mut dyn FnMut(&mut Dict)) -> Option<()> {
    let mut trailer = doc.trailer();
    let root = trailer.get(b"Root").cloned()?;
    let Object::Dict(catalog) = doc.resolve(&root) else {
        return None;
    };
    let pages = catalog.get(b"Pages").cloned()?;
    // The root /Pages node is direct in the catalog: put it back there.
    if let Some(changed) = edit_at(doc, &pages, path, f)? {
        let mut catalog = (*catalog).clone();
        catalog.put(b"Pages", changed);
        let catalog = Object::Dict(Arc::new(catalog));
        match root {
            Object::Ref(r) => doc.alter_object(r.num, catalog),
            _ => {
                trailer.put(b"Root", catalog);
                doc.set_trailer(trailer);
            }
        }
    }
    Some(())
}

/// Applies `f` to what `path` leads to from `node`, through /Kids. An
/// indirect object on the way is changed in the edits, giving
/// `Some(None)`; a direct dictionary's changed copy is returned, for the
/// caller to put back. `None` when the path leads nowhere.
fn edit_at(
    doc: &Document,
    node: &Object,
    path: &[usize],
    f: &mut dyn FnMut(&mut Dict),
) -> Option<Option<Object>> {
    let (dict, num) = match node {
        Object::Ref(r) => match doc.object(r.num)? {
            Object::Dict(d) => (d, Some(r.num)),
            _ => return None,
        },
        Object::Dict(d) => (d.clone(), None),
        _ => return None,
    };
    let changed = match path.split_first() {
        None => {
            let mut d = (*dict).clone();
            f(&mut d);
            Some(d)
        }
        Some((&i, rest)) => match dict.get(b"Kids")? {
            Object::Ref(a) => {
                let Object::Array(kids) = doc.object(a.num)? else {
                    return None;
                };
                if let Some(kid) = edit_at(doc, kids.get(i)?, rest, f)? {
                    let mut kids = (*kids).clone();
                    kids[i] = kid;
                    doc.alter_object(a.num, Object::Array(Arc::new(kids)));
                }
                None
            }
            Object::Array(kids) => edit_at(doc, kids.get(i)?, rest, f)?.map(|kid| {
                let mut kids = (**kids).clone();
                kids[i] = kid;
                let mut d = (*dict).clone();
                d.put(b"Kids", Object::Array(Arc::new(kids)));
                d
            }),
            _ => return None,
        },
    };
    let changed = changed.map(|d| Object::Dict(Arc::new(d)));
    Some(match (num, changed) {
        (Some(num), Some(d)) => {
            doc.alter_object(num, d);
            None
        }
        (_, changed) => changed,
    })
}

/// One step down into a container: a dictionary key or an array index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    Key(&'static [u8]),
    Index(usize),
}

/// Where a stored value is: the object holding it (`None` for the
/// trailer) and the steps from there through direct containers. Two
/// locations are equal when they name the same stored value, as MuPDF's
/// pointers are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Loc {
    holder: Option<i32>,
    steps: Vec<Step>,
}

/// References followed before giving up, as `pdf_resolve_indirect_chain`.
const MAX_CHAIN: usize = 10;

/// `pdf_dict_get` and `pdf_array_get`: a null value is no value.
fn step_into(value: &Object, step: &Step) -> Option<Object> {
    let found = match (value, step) {
        (Object::Dict(d), Step::Key(k)) => d.get(k),
        (Object::Stream(s), Step::Key(k)) => s.dict.get(k),
        (Object::Array(a), Step::Index(i)) => a.get(*i),
        _ => None,
    };
    found.filter(|v| !matches!(v, Object::Null)).cloned()
}

fn step_into_mut<'a>(value: &'a mut Object, step: &Step) -> Option<&'a mut Object> {
    match (value, step) {
        (Object::Dict(d), Step::Key(k)) => Arc::make_mut(d).get_mut(k),
        (Object::Stream(s), Step::Key(k)) => Arc::make_mut(s).dict.get_mut(k),
        (Object::Array(a), Step::Index(i)) => Arc::make_mut(a).get_mut(*i),
        _ => None,
    }
}

/// The object number a reference chain ends at, and that object.
fn follow(doc: &Document, value: &Object) -> Option<(i32, Object)> {
    let mut v = value.clone();
    let mut num = None;
    for _ in 0..MAX_CHAIN {
        match v {
            Object::Ref(r) => {
                num = Some(r.num);
                v = doc.object(r.num).unwrap_or(Object::Null);
            }
            other => return num.map(|n| (n, other)),
        }
    }
    None
}

/// MuPDF's name for an object's kind, as its errors give it.
pub(crate) fn kind(value: &Object) -> &'static str {
    match value {
        Object::Null => "null",
        Object::Bool(_) => "boolean",
        Object::Int(_) => "integer",
        Object::Real(_) => "real",
        Object::String(_) => "string",
        Object::Name(_) => "name",
        Object::Array(_) => "array",
        Object::Dict(_) | Object::Stream(_) => "dictionary",
        Object::Ref(_) => "reference",
    }
}

impl Loc {
    pub(crate) fn trailer() -> Loc {
        Loc {
            holder: None,
            steps: Vec::new(),
        }
    }

    /// The stored value, unresolved; `None` when nothing is stored there.
    pub(crate) fn value(&self, doc: &Document) -> Option<Object> {
        let mut v = match self.holder {
            None => Object::Dict(Arc::new(doc.trailer())),
            Some(num) => doc.object(num)?,
        };
        for step in &self.steps {
            v = step_into(&v, step)?;
        }
        Some(v)
    }

    /// The value one step down from what this one designates, through any
    /// reference it holds (`pdf_dict_get` and `pdf_array_get` resolve
    /// their container).
    pub(crate) fn child(&self, doc: &Document, step: Step) -> Option<Loc> {
        let value = self.value(doc)?;
        let loc = match follow(doc, &value) {
            Some((num, obj)) => {
                step_into(&obj, &step)?;
                Loc {
                    holder: Some(num),
                    steps: vec![step],
                }
            }
            None => {
                step_into(&value, &step)?;
                let mut steps = self.steps.clone();
                steps.push(step);
                Loc {
                    holder: self.holder,
                    steps,
                }
            }
        };
        Some(loc)
    }

    /// The object this value designates, resolved.
    pub(crate) fn resolved(&self, doc: &Document) -> Option<Object> {
        let value = self.value(doc)?;
        Some(follow(doc, &value).map_or(value, |(_, obj)| obj))
    }

    /// Changes the object this value designates, where it is: a
    /// referenced object in the edits, a direct one inside its holder.
    pub(crate) fn edit<R>(
        &self,
        doc: &Document,
        f: impl FnOnce(&mut Object) -> Result<R, String>,
    ) -> Result<R, String> {
        let value = self.value(doc).ok_or("cannot find object to change")?;
        if let Some((num, mut obj)) = follow(doc, &value) {
            let out = f(&mut obj)?;
            doc.alter_object(num, obj);
            return Ok(out);
        }
        let mut top = match self.holder {
            None => Object::Dict(Arc::new(doc.trailer())),
            Some(num) => doc.object(num).ok_or("cannot find object to change")?,
        };
        let mut target = &mut top;
        for step in &self.steps {
            target = step_into_mut(target, step).ok_or("cannot find object to change")?;
        }
        let out = f(target)?;
        match (self.holder, top) {
            (None, Object::Dict(t)) => doc.set_trailer((*t).clone()),
            (Some(num), top) => doc.alter_object(num, top),
            _ => {}
        }
        Ok(out)
    }
}

/// `pdf_dict_put` on whatever `obj` is: the error it throws on anything
/// but a dictionary.
pub(crate) fn dict_mut(obj: &mut Object) -> Result<&mut Dict, String> {
    match obj {
        Object::Dict(d) => Ok(Arc::make_mut(d)),
        Object::Stream(s) => Ok(&mut Arc::make_mut(s).dict),
        other => Err(format!("not a dict ({})", kind(other))),
    }
}

/// The same for `pdf_array_put` and its kin.
pub(crate) fn array_mut(obj: &mut Object) -> Result<&mut Vec<Object>, String> {
    match obj {
        Object::Array(a) => Ok(Arc::make_mut(a)),
        other => Err(format!("not an array ({})", kind(other))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(bytes: &[u8]) -> Document {
        Document::open(bytes.to_vec()).unwrap_or_else(|e| panic!("{e:?}"))
    }

    #[test]
    fn text_strings_are_ascii_or_utf16() {
        assert_eq!(
            text_string("Plain"),
            Object::String(Arc::from(&b"Plain"[..]))
        );
        assert_eq!(
            text_string("é😀"),
            Object::String(Arc::from(
                &[0xfe, 0xff, 0x00, 0xe9, 0xd8, 0x3d, 0xde, 0x00][..]
            ))
        );
    }

    #[test]
    fn metadata_goes_into_info_made_if_missing() {
        let doc = open(include_bytes!("../../../corpus/smoke/minimal.pdf"));
        assert!(doc.trailer().get(b"Info").is_none());
        set_metadata(&doc, "info:Title", "Été");
        set_metadata(&doc, "Author", "nupdf");
        assert_eq!(doc.metadata("Title").as_deref(), Some("Été"));
        assert_eq!(doc.metadata("Author").as_deref(), Some("nupdf"));
        strip_metadata(&doc);
        assert!(doc.trailer().get(b"Info").is_none());
    }

    #[test]
    fn rotation_adds_to_what_the_page_inherits() {
        let doc = open(include_bytes!("../../../corpus/smoke/pages-rearrange.pdf"));
        rotate_pages(&doc, &[0, 3, 3], 90).unwrap_or_else(|e| panic!("{e}"));
        let rotate = |i| {
            let p = doc.page(i).unwrap_or(Object::Null);
            doc.inherited(&p, b"Rotate")
                .ok()
                .flatten()
                .map(|r| doc.resolve(&r))
        };
        assert_eq!(rotate(0), Some(Object::Int(90)));
        // Page 4 inherits 90, then turns twice.
        assert_eq!(rotate(3), Some(Object::Int(270)));
        assert!(rotate_pages(&doc, &[9], 90).is_err());
    }

    #[test]
    fn direct_pages_turn_where_they_sit() {
        // Two equal direct pages under an indirect /Kids array, in a /Pages
        // node direct in the catalog.
        let doc = open(
            b"%PDF-1.4\n\
            1 0 obj<</Type/Catalog/Pages<</Type/Pages/Kids 2 0 R/Count 2>>>>endobj\n\
            2 0 obj[<</Type/Page/Rotate 90>><</Type/Page/Rotate 90>>]endobj\n\
            trailer<</Root 1 0 R/Size 3>>\n",
        );
        rotate_pages(&doc, &[1], 90).unwrap_or_else(|e| panic!("{e}"));
        let rotate = |i| doc.dict_get(&doc.page(i).unwrap_or(Object::Null), b"Rotate");
        assert_eq!((rotate(0), rotate(1)), (Object::Int(90), Object::Int(180)));
        // The catalog and its direct /Pages are left alone.
        assert!(
            matches!(doc.object(1), Some(Object::Dict(d)) if matches!(d.get(b"Pages"), Some(Object::Dict(_))))
        );
    }

    #[test]
    fn metadata_in_a_direct_info_stays_in_the_trailer() {
        let doc = open(
            b"%PDF-1.4\n\
            1 0 obj<</Type/Catalog/Pages 2 0 R/Metadata 3 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[]/Count 0>>endobj\n\
            3 0 obj<</Length 0>>stream\n\nendstream endobj\n\
            trailer<</Root 1 0 R/Size 4/Info<</Title(Old)>>>>\n",
        );
        set_metadata(&doc, "Title", "New\0ignored");
        assert!(matches!(doc.trailer().get(b"Info"), Some(Object::Dict(_))));
        assert_eq!(doc.metadata("Title").as_deref(), Some("New"));
        strip_metadata(&doc);
        assert!(doc.trailer().get(b"Info").is_none());
        let catalog = Object::Ref(crate::object::ObjRef::new(1, 0));
        assert_eq!(doc.dict_get(&catalog, b"Metadata"), Object::Null);
    }

    #[test]
    fn info_that_is_not_a_dictionary_is_replaced() {
        let doc = open(
            b"%PDF-1.4\n\
            1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[]/Count 0>>endobj\n\
            trailer<</Root 1 0 R/Size 3/Info 7>>\n",
        );
        set_metadata(&doc, "Subject", "s");
        assert!(matches!(doc.trailer().get(b"Info"), Some(Object::Ref(_))));
        assert_eq!(doc.metadata("Subject").as_deref(), Some("s"));
    }

    #[test]
    fn a_direct_catalog_loses_its_metadata_in_the_trailer() {
        let pdf = crate::testutil::pdf_of(&[
            "null",
            "<</Type/Pages/Kids[]/Count 0>>",
            "<</Length 0>>stream\n\nendstream",
        ]);
        let doc = open(&crate::testutil::with_trailer(
            &pdf,
            "<</Root<</Type/Catalog/Pages 2 0 R/Metadata 3 0 R>>/Size 4>>",
        ));
        strip_metadata(&doc);
        let Some(Object::Dict(root)) = doc.trailer().get(b"Root").cloned() else {
            panic!("root is not direct");
        };
        assert!(root.get(b"Metadata").is_none());
        assert!(root.get(b"Pages").is_some());
    }

    #[test]
    fn an_xref_stream_trailer_is_touched_when_metadata_goes() {
        let doc = open(include_bytes!("../../../corpus/smoke/meta-xref-stream.pdf"));
        assert!(doc.trailer_owner().is_some());
        strip_metadata(&doc);
        assert!(doc.is_edited());
        assert!(doc.trailer().get(b"Info").is_none());
    }

    #[test]
    fn direct_pages_in_direct_kids_are_edited_in_their_parent() {
        // The root /Pages node is an object; its /Kids is direct, and so
        // are the pages and the node in it.
        let doc = open(
            b"%PDF-1.4\n\
            1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[<</Type/Page>><</Type/Pages/Kids[<</Type/Page>>]/Count 1>>]/Count 2>>endobj\n\
            trailer<</Root 1 0 R/Size 3>>\n",
        );
        edit_page(&doc, 1, &mut |d| d.put(b"Rotate", Object::Int(90)))
            .unwrap_or_else(|e| panic!("{e}"));
        let rotate = |i| doc.dict_get(&doc.page(i).unwrap_or(Object::Null), b"Rotate");
        assert_eq!((rotate(0), rotate(1)), (Object::Null, Object::Int(90)));
        assert!(edit_page(&doc, 5, &mut |_| {}).is_err());
    }

    #[test]
    fn a_direct_root_in_the_trailer_takes_its_changed_pages() {
        let pdf = crate::testutil::pdf_of(&["null"]);
        let doc = open(&crate::testutil::with_trailer(
            &pdf,
            "<</Root<</Type/Catalog/Pages<</Type/Pages/Kids[<</Type/Page>>]/Count 1>>>>/Size 2>>",
        ));
        assert_eq!(doc.page_count(), Ok(1));
        edit_page(&doc, 0, &mut |d| d.put(b"Rotate", Object::Int(180)))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            doc.dict_get(&doc.page(0).unwrap_or(Object::Null), b"Rotate"),
            Object::Int(180)
        );
        assert!(matches!(doc.trailer().get(b"Root"), Some(Object::Dict(_))));
    }

    #[test]
    fn locations_follow_references_and_edit_where_values_are() {
        let doc = open(
            b"%PDF-1.4\n\
            1 0 obj<</Type/Catalog/Pages 2 0 R/Extra<</A[1 2 3]/B 4 0 R>>>>endobj\n\
            2 0 obj<</Type/Pages/Kids[]/Count 0>>endobj\n\
            4 0 obj(text)endobj\n\
            trailer<</Root 1 0 R/Size 5>>\n",
        );
        let root = Loc::trailer()
            .child(&doc, Step::Key(b"Root"))
            .unwrap_or_else(|| panic!("no root"));
        let extra = root
            .child(&doc, Step::Key(b"Extra"))
            .unwrap_or_else(|| panic!("no extra"));
        let item = extra
            .child(&doc, Step::Key(b"A"))
            .and_then(|a| a.child(&doc, Step::Index(1)))
            .unwrap_or_else(|| panic!("no item"));
        assert_eq!(item.value(&doc), Some(Object::Int(2)));
        assert!(extra.child(&doc, Step::Key(b"Missing")).is_none());
        assert!(extra.child(&doc, Step::Index(0)).is_none());
        // A value behind a reference resolves to its object.
        let b = extra
            .child(&doc, Step::Key(b"B"))
            .unwrap_or_else(|| panic!("no b"));
        assert_eq!(
            b.resolved(&doc),
            Some(Object::String(Arc::from(&b"text"[..])))
        );
        // Edited in place inside the catalog...
        item.edit(&doc, |v| {
            *v = Object::Int(20);
            Ok(())
        })
        .unwrap_or_else(|e: String| panic!("{e}"));
        assert_eq!(item.value(&doc), Some(Object::Int(20)));
        // ...or in the referenced object.
        b.edit(&doc, |v| {
            *v = Object::Int(5);
            Ok(())
        })
        .unwrap_or_else(|e: String| panic!("{e}"));
        assert_eq!(doc.object(4), Some(Object::Int(5)));
        // The trailer itself.
        Loc::trailer()
            .edit(&doc, |t| {
                dict_mut(t)?.put(b"Extra", Object::Bool(true));
                Ok(())
            })
            .unwrap_or_else(|e: String| panic!("{e}"));
        assert_eq!(doc.trailer().get(b"Extra"), Some(&Object::Bool(true)));
        let mut not = Object::Int(1);
        assert_eq!(
            dict_mut(&mut not).err().as_deref(),
            Some("not a dict (integer)")
        );
        assert_eq!(
            array_mut(&mut not).err().as_deref(),
            Some("not an array (integer)")
        );
        let mut arr = Object::Array(Arc::new(vec![]));
        assert!(array_mut(&mut arr).is_ok());
    }

    #[test]
    fn a_reference_chain_too_long_is_not_followed() {
        let mut objs = vec![
            "<</Type/Catalog/Pages 2 0 R/Long 3 0 R>>".to_string(),
            "<</Type/Pages/Kids[]/Count 0>>".to_string(),
        ];
        for n in 3..16 {
            objs.push(format!("{} 0 R", n + 1));
        }
        objs.push("<</X 1>>".into());
        let refs: Vec<&str> = objs.iter().map(String::as_str).collect();
        let doc = crate::testutil::doc_of(&refs);
        let long = Loc::trailer()
            .child(&doc, Step::Key(b"Root"))
            .and_then(|r| r.child(&doc, Step::Key(b"Long")))
            .unwrap_or_else(|| panic!("no long"));
        // Past MuPDF's chain limit the value is taken as it is stored.
        assert!(matches!(long.resolved(&doc), Some(Object::Ref(_))));
        assert!(long.child(&doc, Step::Key(b"X")).is_none());
    }

    #[test]
    fn kinds_are_named_as_mupdf_names_them() {
        let cases = [
            (Object::Null, "null"),
            (Object::Bool(true), "boolean"),
            (Object::Int(1), "integer"),
            (Object::Real(1.5), "real"),
            (Object::String(Arc::from(&b""[..])), "string"),
            (Object::name(b"N"), "name"),
            (Object::Array(Arc::new(vec![])), "array"),
            (Object::Dict(Arc::new(Dict::new())), "dictionary"),
            (Object::Ref(crate::object::ObjRef::new(1, 0)), "reference"),
        ];
        for (obj, name) in cases {
            assert_eq!(kind(&obj), name);
        }
    }
}
