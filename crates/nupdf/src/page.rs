// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-page.c (pdf_lookup_page_obj and the page
// tree map), source/pdf/pdf-repair.c (pdf_walk_parent) and
// source/pdf/pdf-object.c (pdf_cycle, pdf_dict_get_inheritable),
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! Page tree lookup and attribute inheritance.
//!
//! MuPDF first tries to build a map of every page by walking the tree. If
//! the tree has fewer pages than /Count claims, it rewrites /Count; if the
//! walk fails, it falls back to a slower lookup that guesses node types from
//! their keys. Both paths, and the /Count rewrite, are reproduced here,
//! because later page counts depend on them.

use std::collections::HashMap;
use std::sync::Arc;

use crate::document::Document;
use crate::geometry::{Matrix, Rect};
use crate::object::Object;

/// A page box as `pdf_page_obj_transform_box` settles it: Letter when
/// empty, corners ordered, the unit square when under a point wide.
fn normalized(r: Rect) -> Rect {
    let mut r = if r.is_empty() {
        Rect::new(0.0, 0.0, 612.0, 792.0)
    } else {
        r
    };
    r.x0 = r.x0.min(r.x1);
    r.y0 = r.y0.min(r.y1);
    r.x1 = r.x0.max(r.x1);
    r.y1 = r.y0.max(r.y1);
    if r.x1 - r.x0 < 1.0 || r.y1 - r.y0 < 1.0 {
        Rect::UNIT
    } else {
        r
    }
}

/// Ancestor chains longer than this count as cycles (MAX_CYCLE_STACK_DEPTH).
const MAX_TREE_DEPTH: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageError(pub String);

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The page map as MuPDF builds it: page objects in order, as they appear
/// in their parents' /Kids (so usually references).
pub(crate) type PageMap = Arc<Vec<Object>>;

impl Document {
    /// The page object at `index` (0-based), as `pdf_lookup_page_obj` returns
    /// it: the entry from the parent's /Kids, usually a reference.
    pub fn page(&self, index: i32) -> Result<Object, PageError> {
        let not_found = || {
            PageError(format!(
                "cannot find page {} in page tree",
                i64::from(index) + 1
            ))
        };
        // An empty map is a NULL map in MuPDF (calloc of zero), so lookups
        // fall through to the slow path.
        if let Some(map) = self.page_map().filter(|m| !m.is_empty()) {
            return usize::try_from(index)
                .ok()
                .and_then(|i| map.get(i).cloned())
                .ok_or_else(not_found);
        }
        self.lookup_page_slow(index).map(|(page, _)| page)
    }

    /// Where `page(index)` finds the page: the index in /Kids at each level
    /// down from the root /Pages node. Editing a page that is a direct
    /// dictionary changes it there, as MuPDF does.
    pub(crate) fn page_path(&self, index: i32) -> Option<Vec<usize>> {
        if self.page_map().is_some_and(|m| !m.is_empty()) {
            let root = self.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
            let pages = self.lookup(&root, b"Pages")?;
            let mut seen = 0;
            let target = usize::try_from(index).ok()?;
            return self.find_path(&pages, target, &mut seen, &mut Vec::new());
        }
        self.lookup_page_slow(index).ok().map(|(_, path)| path)
    }

    /// Whether lookups go through a page map (`doc->rev_page_map`).
    pub(crate) fn has_page_map(&self) -> bool {
        self.page_map().is_some_and(|m| !m.is_empty())
    }

    /// `pdf_lookup_page_number_fast`: the index of the page whose object
    /// is `num`, from the page map; the first one when a page repeats.
    pub(crate) fn page_index_by_number(&self, num: i32) -> Option<i32> {
        let map = self.page_map().filter(|m| !m.is_empty())?;
        map.iter()
            .position(|p| p.as_ref().map_or(0, |r| r.num) == num)
            .and_then(|i| i32::try_from(i).ok())
    }

    /// `pdf_to_rect`: four numbers as a rectangle, corners ordered; empty
    /// for anything but an array.
    pub fn to_rect(&self, value: Option<&Object>) -> Rect {
        let Some(Object::Array(a)) = value.map(|v| self.resolve(v)) else {
            return Rect::EMPTY;
        };
        let n = |i: usize| a.get(i).map_or(0.0, |v| self.resolve(v).to_real());
        let (a, b, c, d) = (n(0), n(1), n(2), n(3));
        Rect::new(a.min(c), b.min(d), a.max(c), b.max(d))
    }

    /// `pdf_page_obj_transform`: the page's crop box, and the matrix from
    /// PDF user space to MuPDF's page space (origin at the top left of the
    /// crop box, y down, rotation applied).
    pub fn page_transform(&self, page: &Object) -> Result<(Rect, Matrix), PageError> {
        let userunit = self
            .lookup(page, b"UserUnit")
            .map(|v| self.resolve(&v))
            .filter(Object::is_number)
            .map_or(1.0, |v| v.to_real());
        let mediabox = self.to_rect(self.inherited(page, b"MediaBox")?.as_ref());
        let cropbox_value = self.inherited(page, b"CropBox")?;
        let usedbox = match &cropbox_value {
            Some(c) => mediabox.intersect(self.to_rect(Some(c))),
            None => mediabox,
        };
        let usedbox = normalized(usedbox);

        // Snap the rotation to 0, 90, 180 or 270.
        let mut rotate = self
            .inherited(page, b"Rotate")?
            .map_or(0, |r| self.resolve(&r).to_int());
        if rotate < 0 {
            rotate = 360 - (rotate.wrapping_neg() % 360);
        }
        if rotate >= 360 {
            rotate %= 360;
        }
        rotate = 90 * ((rotate + 45) / 90);
        if rotate >= 360 {
            rotate = 0;
        }

        let ctm = Matrix::scale(userunit, -userunit).pre_rotate(-(rotate as f32));
        // The crop box sets the origin, the media box when it is not an
        // array.
        let origin = match &cropbox_value {
            Some(c) if matches!(self.resolve(c), Object::Array(_)) => self.to_rect(Some(c)),
            _ => mediabox,
        };
        let origin = normalized(origin.intersect(mediabox)).transform(ctm);
        Ok((
            usedbox,
            ctm.concat(Matrix::translate(-origin.x0, -origin.y0)),
        ))
    }

    /// `pdf_lookup_page_loc`: the path the slow descent takes to page
    /// `index`, whatever the page map says.
    pub(crate) fn page_location(&self, index: i32) -> Result<Vec<usize>, PageError> {
        self.lookup_page_slow(index).map(|(_, path)| path)
    }

    /// `walk_tree`'s order, stopping at page `target`.
    fn find_path(
        &self,
        node: &Object,
        target: usize,
        seen: &mut usize,
        path: &mut Vec<usize>,
    ) -> Option<Vec<usize>> {
        let kind = self.lookup(node, b"Type");
        if self.is_type(&kind, b"Pages") {
            if path.len() > MAX_TREE_DEPTH {
                return None;
            }
            let kids = self.lookup(node, b"Kids").map(|k| self.resolve(&k));
            for (i, kid) in kids
                .as_ref()
                .and_then(Object::as_array)
                .unwrap_or_default()
                .iter()
                .enumerate()
            {
                path.push(i);
                let found = self.find_path(kid, target, seen, path);
                path.pop();
                if found.is_some() {
                    return found;
                }
            }
            None
        } else if self.is_type(&kind, b"Page") {
            *seen += 1;
            (*seen == target + 1).then(|| path.clone())
        } else {
            None
        }
    }

    /// `pdf_dict_get_inheritable`: the value of `key` on the page or the
    /// nearest ancestor that has it, as stored (possibly a reference).
    ///
    /// A cycle in the /Parent chain makes MuPDF re-derive every Parent from
    /// the /Kids structure and retry once. A cycle that survives that (one
    /// through the root, which the repair never touches) is an error.
    pub fn inherited(&self, page: &Object, key: &[u8]) -> Result<Option<Object>, PageError> {
        let cycle = || PageError("cycle in parent chain".into());
        if let Some(found) = self.walk_parents(page, key) {
            return Ok(found);
        }
        let state = self.current();
        if state.repaired_parents.get().is_some() {
            return Err(cycle());
        }
        self.diagnostics()
            .warn("page.parent_cycle", "cycle in parent chain");
        let _ = state.repaired_parents.set(self.tree_parents());
        self.walk_parents(page, key).ok_or_else(cycle)
    }

    /// Walks the /Parent chain. `None` means a cycle was found.
    fn walk_parents(&self, page: &Object, key: &[u8]) -> Option<Option<Object>> {
        let state = self.current();
        let repaired = state.repaired_parents.get();
        let mut node = page.clone();
        let mut seen: Vec<i32> = Vec::new();
        loop {
            if let Some(value) = self.lookup(&node, key) {
                return Some(Some(value));
            }
            let fixed = match (&node, repaired) {
                (Object::Ref(r), Some(map)) => map.get(&r.num).cloned(),
                _ => None,
            };
            node = match fixed.or_else(|| self.lookup(&node, b"Parent")) {
                Some(parent) => parent,
                None => return Some(None),
            };
            if let Object::Ref(r) = &node {
                if seen.contains(&r.num) {
                    return None;
                }
                seen.push(r.num);
            }
        }
    }

    /// `pdf_repair_page_tree_parents`: every field's parent is the field
    /// (or /AcroForm) whose /Kids (or /Fields) lists it; every page tree
    /// node's parent is the node whose /Kids lists it; and, as MuPDF has it,
    /// every annotation's parent is the page whose /Annots lists it, the
    /// fields tree walked first. The roots keep their own /Parent.
    fn tree_parents(&self) -> HashMap<i32, Object> {
        let mut parents = HashMap::new();
        let root = self.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
        if let Some(acroform) = self.lookup(&root, b"AcroForm") {
            self.walk_fields(&acroform, &mut Vec::new(), None, b"Fields", &mut parents);
        }
        if let Some(pages) = self.lookup(&root, b"Pages") {
            self.walk_pages(&pages, &mut Vec::new(), None, &mut parents);
        }
        parents
    }

    /// Records `parent` as the parent of `node`, saying so when its own
    /// /Parent is another.
    fn fix_parent(
        &self,
        node: &Object,
        parent: &Object,
        code: &'static str,
        message: &'static str,
        parents: &mut HashMap<i32, Object>,
    ) {
        if self.lookup(node, b"Parent").as_ref() != Some(parent) {
            self.diagnostics().warn(code, message);
        }
        if let Object::Ref(r) = node {
            parents.insert(r.num, parent.clone());
        }
    }

    /// `walk_page_tree`.
    fn walk_pages(
        &self,
        node: &Object,
        ancestors: &mut Vec<i32>,
        parent: Option<&Object>,
        parents: &mut HashMap<i32, Object>,
    ) {
        let num = node.as_ref().map_or(0, |r| r.num);
        if (num > 0 && ancestors.contains(&num)) || ancestors.len() > MAX_TREE_DEPTH {
            return;
        }
        if let Some(parent) = parent {
            self.fix_parent(
                node,
                parent,
                "page.fixed_parent",
                "fixed bad Parent in Pages tree",
                parents,
            );
        }
        self.walk_fields(node, &mut Vec::new(), None, b"Annots", parents);
        ancestors.push(num);
        let kids = self.lookup(node, b"Kids").map(|k| self.resolve(&k));
        for kid in kids.as_ref().and_then(Object::as_array).unwrap_or_default() {
            self.walk_pages(kid, ancestors, Some(node), parents);
        }
        ancestors.pop();
    }

    /// `walk_field_tree`.
    fn walk_fields(
        &self,
        field: &Object,
        ancestors: &mut Vec<i32>,
        parent: Option<&Object>,
        kids_key: &[u8],
        parents: &mut HashMap<i32, Object>,
    ) {
        let num = field.as_ref().map_or(0, |r| r.num);
        if (num > 0 && ancestors.contains(&num)) || ancestors.len() > MAX_TREE_DEPTH {
            return;
        }
        if let Some(parent) = parent {
            self.fix_parent(
                field,
                parent,
                "form.fixed_parent",
                "fixed bad Parent in AcroForm tree",
                parents,
            );
        }
        ancestors.push(num);
        let kids = self.lookup(field, kids_key).map(|k| self.resolve(&k));
        for kid in kids.as_ref().and_then(Object::as_array).unwrap_or_default() {
            self.walk_fields(kid, ancestors, Some(field), b"Kids", parents);
        }
        ancestors.pop();
    }

    /// A node's /Parent, as MuPDF has it after repairing the parents (see
    /// `inherited`).
    pub fn parent_of(&self, node: &Object) -> Option<Object> {
        let fixed = match (node, self.current().repaired_parents.get()) {
            (Object::Ref(r), Some(map)) => map.get(&r.num).cloned(),
            _ => None,
        };
        fixed.or_else(|| self.lookup(node, b"Parent"))
    }

    /// `pdf_dict_get`: resolve the container, return the value as stored.
    /// `pdf_dict_get`: a null value is no value, MuPDF's null object being
    /// the null pointer.
    fn lookup(&self, container: &Object, key: &[u8]) -> Option<Object> {
        self.resolve(container)
            .as_dict()
            .and_then(|d| d.get(key))
            .filter(|v| !matches!(v, Object::Null))
            .cloned()
    }

    fn is_type(&self, value: &Option<Object>, name: &[u8]) -> bool {
        value
            .as_ref()
            .is_some_and(|v| self.resolve(v).is_name(name))
    }

    /// Built on first use, like MuPDF's `fwd_page_map`; `None` when building
    /// it failed and lookups use the slow path.
    fn page_map(&self) -> Option<PageMap> {
        let state = self.current();
        if self.is_edited() {
            // Built again after each edit, as MuPDF drops its map.
            if let Some(map) = self.with_edits(|e| e.page_map.clone()).flatten() {
                return map;
            }
            let map = self.build_page_map(&state);
            self.cache_page_map(map.clone());
            return map;
        }
        state
            .page_map
            .get_or_init(|| self.build_page_map(&state))
            .clone()
    }

    fn build_page_map(&self, state: &crate::document::State) -> Option<PageMap> {
        // MuPDF reports the caught error (a format error) before warning.
        let fail = |e: String| {
            self.diagnostics()
                .warn("page.tree", crate::diag::report::line(&e));
            self.diagnostics().warn(
                "page.tree_fallback",
                "page tree load failed; falling back to slow lookup",
            );
            None
        };
        let count = match self.page_count() {
            Ok(n) => n,
            Err(e) => return fail(e.0),
        };
        let root = self.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
        let pages = self.lookup(&root, b"Pages").unwrap_or(Object::Null);
        let mut out = Vec::new();
        if let Err(e) = self.walk_tree(&pages, count as usize, &mut Vec::new(), &mut out) {
            return fail(e);
        }
        if out.len() < count as usize {
            self.diagnostics().warn(
                "page.count_too_high",
                format!(
                    "Document claims to have {count} pages, but only has {}.",
                    out.len()
                ),
            );
            if self.is_edited() {
                self.rewrite_page_count(out.len() as i32);
            } else {
                let _ = state.page_count_override.set(out.len() as i32);
            }
        }
        Some(Arc::new(out))
    }

    /// `pdf_load_page_tree_imp`.
    fn walk_tree(
        &self,
        node: &Object,
        count: usize,
        ancestors: &mut Vec<i32>,
        out: &mut Vec<Object>,
    ) -> Result<(), String> {
        let kind = self.lookup(node, b"Type");
        if self.is_type(&kind, b"Pages") {
            let num = node.as_ref().map_or(0, |r| r.num);
            if (num > 0 && ancestors.contains(&num)) || ancestors.len() > MAX_TREE_DEPTH {
                return Err("cycle in page tree".into());
            }
            ancestors.push(num);
            let kids = self.lookup(node, b"Kids").map(|k| self.resolve(&k));
            for kid in kids.as_ref().and_then(Object::as_array).unwrap_or_default() {
                self.walk_tree(kid, count, ancestors, out)?;
            }
            ancestors.pop();
            Ok(())
        } else if self.is_type(&kind, b"Page") {
            if out.len() >= count {
                return Err("too many kids in page tree".into());
            }
            out.push(node.clone());
            Ok(())
        } else {
            Err("non-page object in page tree".into())
        }
    }

    /// `pdf_lookup_page_loc`: descend using /Count, guessing node types from
    /// /Kids and /MediaBox when /Type is missing.
    fn lookup_page_slow(&self, index: i32) -> Result<(Object, Vec<usize>), PageError> {
        let root = self.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
        let Some(mut node) = self.lookup(&root, b"Pages") else {
            return Err(PageError("cannot find page tree".into()));
        };
        let mut skip = index;
        let mut marks: Vec<i32> = Vec::new();
        let mut path = Vec::new();
        loop {
            let kids = self.lookup(&node, b"Kids").map(|k| self.resolve(&k));
            let kids = kids.as_ref().and_then(Object::as_array).unwrap_or_default();
            if kids.is_empty() {
                return Err(PageError("malformed page tree".into()));
            }
            if let Object::Ref(r) = &node {
                if marks.contains(&r.num) {
                    return Err(PageError("cycle in page tree".into()));
                }
                marks.push(r.num);
            }
            let mut next = None;
            for (i, kid) in kids.iter().enumerate() {
                let kind = self.lookup(kid, b"Type");
                let has = |key: &[u8]| self.lookup(kid, key).is_some();
                let is_pages = match &kind {
                    Some(_) => self.is_type(&kind, b"Pages"),
                    None => has(b"Kids") && !has(b"MediaBox"),
                };
                if is_pages {
                    let count = self
                        .lookup(kid, b"Count")
                        .map_or(0, |c| self.resolve(&c).to_int());
                    if skip < count {
                        next = Some(kid.clone());
                        path.push(i);
                        break;
                    }
                    skip -= count;
                } else {
                    let not_page = match &kind {
                        Some(_) => !self.is_type(&kind, b"Page"),
                        None => !has(b"MediaBox"),
                    };
                    if not_page {
                        self.diagnostics()
                            .warn("page.non_page", "non-page object in page tree");
                    }
                    if skip == 0 {
                        path.push(i);
                        return Ok((kid.clone(), path));
                    }
                    skip -= 1;
                }
            }
            match next {
                Some(n) => node = n,
                None => {
                    return Err(PageError(format!(
                        "cannot find page {} in page tree",
                        i64::from(index) + 1
                    )));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Document;
    use crate::object::{ObjRef, Object};

    fn open(bytes: &[u8]) -> Document {
        Document::open(bytes.to_vec()).unwrap()
    }

    fn r(num: i32) -> Object {
        Object::Ref(ObjRef::new(num, 0))
    }

    const TREE: &[u8] = b"%PDF-1.4\n\
        1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
        2 0 obj<</Type/Pages/Kids[3 0 R 4 0 R]/Count 3/MediaBox[0 0 10 10]/Rotate 90>>endobj\n\
        3 0 obj<</Type/Page/Parent 2 0 R>>endobj\n\
        4 0 obj<</Type/Pages/Parent 2 0 R/Kids[5 0 R 6 0 R]/Count 2/MediaBox[0 0 20 20]>>endobj\n\
        5 0 obj<</Type/Page/Parent 4 0 R/Rotate 0>>endobj\n\
        6 0 obj<</Type/Page/Parent 4 0 R/MediaBox[0 0 30 30]>>endobj\n\
        trailer<</Root 1 0 R/Size 7>>\n";

    #[test]
    fn finds_pages_in_order() {
        let d = open(TREE);
        assert_eq!(d.page(0), Ok(r(3)));
        assert_eq!(d.page(1), Ok(r(5)));
        assert_eq!(d.page(2), Ok(r(6)));
        assert!(d.page(3).is_err());
    }

    #[test]
    fn attributes_inherit_from_the_nearest_ancestor() {
        let d = open(TREE);
        let media = |i| {
            d.inherited(&d.page(i).unwrap(), b"MediaBox")
                .unwrap()
                .unwrap()
        };
        assert_eq!(media(0).as_array().unwrap()[2], Object::Int(10));
        assert_eq!(media(1).as_array().unwrap()[2], Object::Int(20));
        assert_eq!(media(2).as_array().unwrap()[2], Object::Int(30));
        let attr = |i, key: &[u8]| d.inherited(&d.page(i).unwrap(), key).unwrap();
        assert_eq!(attr(1, b"Rotate"), Some(Object::Int(0)));
        assert_eq!(attr(2, b"Rotate"), Some(Object::Int(90)));
        assert_eq!(attr(0, b"CropBox"), None);
    }

    #[test]
    fn a_null_value_is_inherited_past() {
        // MuPDF's null object is the null pointer: pdf_dict_get finds no
        // value, so the parent's counts.
        let d = open(
            b"%PDF-1.4\n\
            1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1/Rotate 90>>endobj\n\
            3 0 obj<</Type/Page/Parent 2 0 R/Rotate null>>endobj\n\
            trailer<</Root 1 0 R/Size 4>>\n",
        );
        let page = d.page(0).unwrap();
        assert_eq!(d.inherited(&page, b"Rotate"), Ok(Some(Object::Int(90))));
    }

    #[test]
    fn overstated_count_is_corrected_like_mupdf() {
        let d = open(
            b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[3 0 R]/Count 4>>endobj\n3 0 obj<</Type/Page>>endobj\n\
            4 0 obj<<>>endobj\n5 0 obj<<>>endobj\n6 0 obj<<>>endobj\ntrailer<</Root 1 0 R>>\n",
        );
        assert_eq!(d.page_count(), Ok(4));
        assert_eq!(d.page(0), Ok(r(3)));
        assert_eq!(d.page_count(), Ok(1));
        assert_eq!(d.page(1).unwrap_err().0, "cannot find page 2 in page tree");
        assert!(d.diagnostics().has("page.count_too_high"));
    }

    #[test]
    fn broken_trees_fall_back_to_the_slow_lookup() {
        // No /Type on the intermediate node: the map fails, the slow lookup
        // recognizes it by /Kids without /MediaBox.
        let d = open(b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
            3 0 obj<</Kids[4 0 R]/Count 1>>endobj\n4 0 obj<</Type/Page>>endobj\ntrailer<</Root 1 0 R>>\n");
        assert_eq!(d.page(0), Ok(r(4)));
        assert!(d.diagnostics().has("page.tree_fallback"));
    }

    #[test]
    fn cycles_are_errors_not_hangs() {
        let d = open(
            b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[2 0 R]/Count 1>>endobj\ntrailer<</Root 1 0 R>>\n",
        );
        assert!(d.page(0).is_err());
        // A cycle through the root survives MuPDF's parent repair.
        let d = open(
            b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1/Parent 2 0 R>>endobj\n\
            3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 1 1]>>endobj\ntrailer<</Root 1 0 R>>\n",
        );
        let page = d.page(0).unwrap();
        assert!(d.inherited(&page, b"MediaBox").unwrap().is_some());
        assert_eq!(
            d.inherited(&page, b"CropBox").unwrap_err().0,
            "cycle in parent chain"
        );
    }

    #[test]
    fn bad_parents_below_the_root_are_repaired() {
        // Page 4's /Parent points at itself; /Kids says it belongs to 3.
        let d = open(
            b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1/Rotate 180>>endobj\n\
            3 0 obj<</Type/Pages/Kids[4 0 R]/Count 1/Parent 2 0 R>>endobj\n\
            4 0 obj<</Type/Page/Parent 4 0 R>>endobj\ntrailer<</Root 1 0 R>>\n",
        );
        let page = d.page(0).unwrap();
        assert_eq!(d.inherited(&page, b"Rotate"), Ok(Some(Object::Int(180))));
        assert!(d.diagnostics().has("page.fixed_parent"));
    }

    #[test]
    fn empty_trees_use_the_slow_lookup() {
        let d = open(
            b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Count 1>>endobj\n3 0 obj<<>>endobj\ntrailer<</Root 1 0 R>>\n",
        );
        assert_eq!(d.page(0).unwrap_err().0, "malformed page tree");
        assert_eq!(d.page_count(), Ok(0));
    }
}
