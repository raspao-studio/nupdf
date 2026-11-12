// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-form.c (pdf_set_text_field_value,
// pdf_set_choice_field_value, pdf_set_field_value, update_field_value,
// find_head_of_field_group, pdf_field_mark_dirty, set_checkbox_value,
// update_checkbox_selector, pdf_toggle_widget, toggle_check_box,
// set_check_grp, set_check, pdf_update_page), source/pdf/pdf-annot.c
// (pdf_set_annot_appearance), source/pdf/pdf-appearance.c
// (pdf_update_appearance, pdf_update_button_appearance writing back),
// source/pdf/pdf-font-add.c and source/pdf/pdf-resources.c (font
// resources as objects, one per font and encoding),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Filling form fields as MuPDF's widget API fills them (`setTextValue`,
//! `setChoiceValue`, `toggle`), with JavaScript off: no keystroke, format,
//! validate or calculate script runs. A value set marks its field and the
//! field's widgets dirty; their appearances are then made again, as
//! `pdf_update_page` makes them on every open page, and written into the
//! document with their fonts as objects of their own.

use std::collections::HashSet;
use std::sync::Arc;

use crate::appearance;
use crate::document::Document;
use crate::form;
use crate::geometry::Matrix;
use crate::object::Object;

/// `PDF_BTN_FIELD_IS_NO_TOGGLE_TO_OFF`.
const NO_TOGGLE_TO_OFF: i32 = 1 << 14;

/// One edit, by the field's full name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// `setTextValue`, or `setChoiceValue` for a choice field: for check
    /// boxes and radio buttons the value is the on state's name, or Off.
    Set { name: String, value: String },
    /// `toggle`.
    Toggle { name: String },
}

impl Edit {
    pub fn name(&self) -> &str {
        match self {
            Edit::Set { name, .. } | Edit::Toggle { name } => name,
        }
    }
}

/// What an edit did to its field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub name: String,
    pub op: &'static str,
    pub kind: &'static str,
    /// The widget the edit went through: its page (from 0) and object.
    pub page: i32,
    pub object: i32,
    pub old: String,
    pub new: String,
    /// Whether the document changed.
    pub changed: bool,
    /// What MuPDF does that a caller may not expect: it sets read-only
    /// fields, and choice values that are none of the options.
    pub notes: Vec<String>,
}

/// An appearance made again: the widget's page (from 0) and object, and
/// why it could not be made, if it could not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Made {
    pub page: i32,
    pub object: i32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filled {
    pub changes: Vec<Change>,
    pub made: Vec<Made>,
}

/// A page's widgets as `pdf_load_page` lists them, and its transform.
struct Page {
    index: i32,
    ctm: Matrix,
    widgets: Vec<Object>,
}

fn num(obj: &Object) -> i32 {
    obj.as_ref().map_or(0, |r| r.num)
}

/// `pdf_dict_get`: the value as stored.
fn raw(doc: &Document, container: &Object, key: &[u8]) -> Object {
    doc.resolve(container)
        .as_dict()
        .and_then(|d| d.get(key).cloned())
        .unwrap_or(Object::Null)
}

/// `pdf_dict_put` on an object: changed in place when it is an object of
/// its own. A direct dictionary (a widget inside its page's /Annots) is
/// left as it is.
fn put(doc: &Document, obj: &Object, key: &[u8], value: Object) {
    if let Object::Ref(r) = obj
        && let Some(mut d) = doc.resolve(obj).as_dict().cloned()
    {
        d.put(key, value);
        doc.alter_object(r.num, Object::Dict(Arc::new(d)));
    }
}

/// The filling session: the pages, what is dirty, and what writes the
/// appearances back.
struct Session<'a> {
    doc: &'a Document,
    pages: Vec<Page>,
    dirty: HashSet<i32>,
    commit: appearance::Committer,
}

impl Session<'_> {
    /// The first widget, in page order, of the field with this full name.
    fn find(&self, name: &str) -> Option<(i32, Object)> {
        self.pages.iter().find_map(|p| {
            p.widgets
                .iter()
                .find(|w| form::name(self.doc, w).is_ok_and(|n| n == name))
                .map(|w| (p.index, w.clone()))
        })
    }

    /// `find_head_of_field_group`: the nearest of the widget and its
    /// ancestors with a /T.
    fn head_of_group(&self, obj: &Object) -> Object {
        let mut node = obj.clone();
        for _ in 0..256 {
            if !matches!(raw(self.doc, &node, b"T"), Object::Null) {
                return node;
            }
            match self.doc.parent_of(&node) {
                Some(p) => node = p,
                None => break,
            }
        }
        obj.clone()
    }

    /// `pdf_field_mark_dirty`: the field and its kids, all the way down.
    fn mark_dirty(&mut self, field: &Object, depth: usize) {
        if depth > 256 {
            return;
        }
        let kids = self.doc.resolve(&raw(self.doc, field, b"Kids"));
        for kid in kids.as_array().unwrap_or(&[]) {
            self.mark_dirty(kid, depth + 1);
        }
        self.dirty.insert(num(field));
    }

    /// `update_field_value`: /V at the head of the field's group, and the
    /// group dirty, unless the value is the one there: whether it was set.
    fn update_field_value(&mut self, obj: &Object, text: &str) -> bool {
        let grp = self.head_of_group(obj);
        let old = match self.doc.resolve(&raw(self.doc, &grp, b"V")) {
            Object::String(s) => crate::text_string::text_string(&s),
            _ => String::new(),
        };
        if old == text {
            return false;
        }
        put(self.doc, &grp, b"V", crate::edit::text_string(text));
        self.mark_dirty(&grp, 0);
        true
    }

    /// `update_checkbox_selector`: /AS of the widget and its kids, the
    /// state if their /AP /N has it, else Off.
    fn update_checkbox_selector(&self, field: &Object, val: &str, depth: usize) {
        let doc = self.doc;
        let kids = raw(doc, field, b"Kids");
        if !matches!(kids, Object::Null) {
            if depth < 256 {
                for kid in doc.resolve(&kids).as_array().unwrap_or(&[]) {
                    self.update_checkbox_selector(kid, val, depth + 1);
                }
            }
            return;
        }
        let n = doc.dict_get(&doc.dict_get(field, b"AP"), b"N");
        let has = n
            .as_dict()
            .and_then(|d| d.get(val.as_bytes()))
            .is_some_and(|v| !matches!(v, Object::Null));
        let state = if has { val.as_bytes() } else { b"Off" };
        put(doc, field, b"AS", Object::name(state));
    }

    /// `pdf_set_text_field_value` (and `pdf_set_choice_field_value`) with
    /// JavaScript off: the keystroke events give the value back as is, and
    /// `pdf_set_field_value` sets it by the field's type. Whether /V was
    /// set.
    fn set(&mut self, widget: &Object, value: &str) -> bool {
        let doc = self.doc;
        // `pdf_field_value` first: a stream value becomes a text string on
        // the widget itself.
        if let Object::Stream(s) = doc.resolve(&form::inheritable(doc, widget, b"V")) {
            let text = doc
                .stream_data(&s)
                .map(|d| crate::text_string::text_string(&d))
                .unwrap_or_default();
            put(doc, widget, b"V", crate::edit::text_string(&text));
        }
        // A C string: up to the first NUL.
        let value = value.split('\0').next().unwrap_or("");
        let set = match form::type_string(doc, widget) {
            "checkbox" | "radiobutton" => {
                self.update_checkbox_selector(widget, value, 0);
                self.update_field_value(widget, value)
            }
            _ => self.update_field_value(widget, value),
        };
        // `pdf_set_annot_field_value` ends with `pdf_dirty_annot`: the
        // widget's appearance is made again, the value changed or not.
        self.dirty.insert(num(widget));
        set
    }

    /// `set_check`: /AS the state when it is the widget's on state, else
    /// Off.
    fn set_check(&self, chk: &Object, name: &[u8]) {
        let doc = self.doc;
        let on = on_state(doc, chk);
        let val: &[u8] = if on == name { name } else { b"Off" };
        let r#as = doc.resolve(&raw(doc, chk, b"AS"));
        if r#as.as_name() != Some(val) {
            put(doc, chk, b"AS", Object::name(val));
        }
    }

    /// `set_check_grp`.
    fn set_check_grp(&self, grp: &Object, val: &[u8], depth: usize) {
        let kids = raw(self.doc, grp, b"Kids");
        if matches!(kids, Object::Null) {
            self.set_check(grp, val);
            return;
        }
        if depth < 256 {
            for kid in self.doc.resolve(&kids).as_array().unwrap_or(&[]) {
                self.set_check_grp(kid, val, depth + 1);
            }
        }
    }

    /// `toggle_check_box`: on to Off, else to the widget's on state, for
    /// the whole group; /V is a name. Nothing is marked dirty, so no
    /// appearance is made again. False when a radio button that may not
    /// be turned off stays on.
    fn toggle(&mut self, widget: &Object) -> bool {
        let doc = self.doc;
        let ff = form::flags(doc, widget);
        let radio = ff & form::BTN_RADIO != 0;
        let grp = self.head_of_group(widget);
        let r#as = doc.resolve(&raw(doc, widget, b"AS"));
        let val = match r#as.as_name() {
            Some(n) if n != b"Off" => {
                if radio && ff & NO_TOGGLE_TO_OFF != 0 {
                    return false;
                }
                b"Off".to_vec()
            }
            _ => on_state(doc, widget),
        };
        put(doc, &grp, b"V", Object::name(&val));
        self.set_check_grp(&grp, &val, 0);
        true
    }

    /// `pdf_update_page` after the edits: on every open page, last opened
    /// first, each dirty widget's appearance made again and written back.
    fn update(&mut self) -> Vec<Made> {
        let mut made = Vec::new();
        if self.dirty.is_empty() {
            return made;
        }
        let order: Vec<(i32, Matrix, Vec<Object>)> = self
            .pages
            .iter()
            .rev()
            .map(|p| (p.index, p.ctm, p.widgets.clone()))
            .collect();
        for (page, ctm, widgets) in order {
            for w in widgets {
                // Made once: a widget on two pages is clean the second time.
                if !matches!(w, Object::Ref(_)) || !self.dirty.remove(&num(&w)) {
                    continue;
                }
                if !appearance::widget_made_again(self.doc, &w) {
                    continue;
                }
                let btn = form::inheritable(self.doc, &w, b"FT").is_name(b"Btn");
                let error = if btn {
                    match appearance::synthesize_button(self.doc, &w, ctm) {
                        Ok(s) => {
                            self.commit.write_back_button(self.doc, &w, &s);
                            None
                        }
                        Err(e) => Some(e),
                    }
                } else {
                    match appearance::synthesize(self.doc, &w, ctm) {
                        Ok(s) => {
                            self.commit.write_back(self.doc, &w, &s);
                            None
                        }
                        Err(e) => Some(e),
                    }
                };
                if let Some(e) = &error {
                    self.doc.diagnostics().warn(
                        "annot.appearance",
                        format!("cannot create appearance stream: {e}"),
                    );
                }
                made.push(Made {
                    page,
                    object: num(&w),
                    error,
                });
            }
        }
        self.dirty.clear();
        made
    }
}

/// `pdf_button_field_on_state`: the first state of /AP /N (or /D) that is
/// not Off, else Yes.
fn on_state(doc: &Document, field: &Object) -> Vec<u8> {
    let ap = doc.dict_get(field, b"AP");
    for key in [&b"N"[..], b"D"] {
        let states = doc.dict_get(&ap, key);
        if let Some(d) = states.as_dict()
            && let Some((k, _)) = d.iter().find(|(k, _)| *k != b"Off")
        {
            return k.to_vec();
        }
    }
    b"Yes".to_vec()
}

/// Why filling failed, before anything changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FillError {
    /// An edit names no field.
    UnknownField(String),
    /// A page does not load, as MuPDF fails loading every page first.
    Page(String),
}

impl std::fmt::Display for FillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FillError::UnknownField(name) => write!(f, "no field named '{name}'"),
            FillError::Page(e) => f.write_str(e),
        }
    }
}

/// Every page, as MuPDF loads them all before filling.
fn pages(doc: &Document) -> Result<Vec<Page>, FillError> {
    let mut out = Vec::new();
    let mut i = 0;
    // `countPages` again each time: loading the first page settles a count
    // the page tree overstates.
    while i < doc.page_count().map_err(|e| FillError::Page(e.0))? {
        let page = doc.page(i).map_err(|e| FillError::Page(e.0))?;
        if i == 0 {
            doc.settle_page_count();
        }
        // `pdf_load_page` fails where it reads the transform (a page with
        // /Annots) or the inherited /Resources and cannot; otherwise a
        // transform that cannot be read is the identity, which only the
        // getters would use.
        let transform = doc.page_transform(&page);
        if !matches!(raw(doc, &page, b"Annots"), Object::Null)
            && let Err(e) = &transform
        {
            return Err(FillError::Page(e.0.clone()));
        }
        doc.inherited(&page, b"Resources")
            .map_err(|e| FillError::Page(e.0))?;
        let ctm = transform.map(|t| t.1).unwrap_or(Matrix::IDENTITY);
        out.push(Page {
            index: i,
            ctm,
            widgets: crate::annot::page_annots(doc, &page).1,
        });
        i += 1;
    }
    Ok(out)
}

/// The edits applied in order, then the dirty widgets' appearances made
/// again and written into the document. An edit naming no field, or a page
/// that does not load, is an error, and nothing is changed.
pub fn fill(doc: &Document, edits: &[Edit]) -> Result<Filled, FillError> {
    let mut s = Session {
        doc,
        pages: pages(doc)?,
        dirty: HashSet::new(),
        commit: appearance::Committer::new(),
    };
    let mut targets = Vec::new();
    for e in edits {
        match s.find(e.name()) {
            Some(t) => targets.push(t),
            None => return Err(FillError::UnknownField(e.name().to_string())),
        }
    }
    let mut changes = Vec::new();
    for (e, (page, widget)) in edits.iter().zip(targets) {
        let kind = form::type_string(doc, &widget);
        let old = form::value(doc, &widget);
        let mut notes = Vec::new();
        if form::is_read_only(doc, &widget) {
            notes.push("the field is read-only; MuPDF changes it anyway".to_string());
        }
        let (op, changed) = match e {
            Edit::Set { value, .. } => {
                if matches!(kind, "combobox" | "listbox")
                    && !form::options(doc, &widget)
                        .iter()
                        .any(|(export, shown)| export == value || shown == value)
                {
                    notes
                        .push("the value is none of the options; MuPDF sets it anyway".to_string());
                }
                if matches!(kind, "checkbox" | "radiobutton")
                    && value != "Off"
                    && on_state(doc, &widget) != value.as_bytes()
                {
                    notes.push(format!(
                        "'{value}' is not the widget's on state ('{}'): its /AS becomes Off",
                        String::from_utf8_lossy(&on_state(doc, &widget))
                    ));
                }
                let set = s.set(&widget, value);
                ("set", set || old != form::value(doc, &widget))
            }
            Edit::Toggle { .. } => {
                if matches!(kind, "checkbox" | "radiobutton") {
                    let done = s.toggle(&widget);
                    if !done {
                        notes
                            .push("a radio button that may not be turned off stays on".to_string());
                    }
                    ("toggle", done)
                } else {
                    notes.push(format!("a {kind} does not toggle; MuPDF does nothing"));
                    ("toggle", false)
                }
            }
        };
        changes.push(Change {
            name: e.name().to_string(),
            op,
            kind,
            page,
            object: num(&widget),
            old,
            new: form::value(doc, &widget),
            changed,
            notes,
        });
    }
    let made = s.update();
    Ok(Filled { changes, made })
}

/// The deterministic fill the harness compares: the first text field set
/// to `value`, and the first check box toggled.
pub fn fill_first(doc: &Document, value: &str) -> Result<Filled, FillError> {
    let mut edits = Vec::new();
    let names = field_names_in_widget_order(doc)?;
    if let Some((n, _)) = names.iter().find(|(_, k)| *k == "text") {
        edits.push(Edit::Set {
            name: n.clone(),
            value: value.to_string(),
        });
    }
    if let Some((n, _)) = names.iter().find(|(_, k)| *k == "checkbox") {
        edits.push(Edit::Toggle { name: n.clone() });
    }
    fill(doc, &edits)
}

/// Each widget's field name and type, in page order, duplicates kept.
fn field_names_in_widget_order(doc: &Document) -> Result<Vec<(String, &'static str)>, FillError> {
    let mut out = Vec::new();
    for p in pages(doc)? {
        for w in &p.widgets {
            if let Ok(n) = form::name(doc, w) {
                out.push((n, form::type_string(doc, w)));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ObjRef;

    /// A page with a text field of two widgets and a check box.
    fn doc() -> Document {
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [4 0 R 7 0 R] /DA (/Helv 0 Tf 0 g) >> >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Annots [5 0 R 6 0 R 7 0 R] >>",
            "<< /T (name) /FT /Tx /V (Ada) /Kids [5 0 R 6 0 R] >>",
            "<< /Type /Annot /Subtype /Widget /Parent 4 0 R /Rect [10 10 150 30] >>",
            "<< /Type /Annot /Subtype /Widget /Parent 4 0 R /Rect [10 40 150 60] >>",
            "<< /Type /Annot /Subtype /Widget /T (ok) /FT /Btn /AS /Off /Rect [10 70 30 90] >>",
        ];
        let mut pdf = String::from("%PDF-1.7\n");
        for (i, o) in objs.iter().enumerate() {
            pdf.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
        }
        pdf.push_str("trailer\n<< /Size 8 /Root 1 0 R >>\n");
        Document::open(pdf.into_bytes()).unwrap()
    }

    fn obj(n: i32) -> Object {
        Object::Ref(ObjRef::new(n, 0))
    }

    fn set(name: &str, value: &str) -> Edit {
        Edit::Set {
            name: name.into(),
            value: value.into(),
        }
    }

    /// The /Helv entry of a widget's new appearance, as stored.
    fn helv(d: &Document, widget: i32) -> Object {
        let n = d.dict_get(&d.dict_get(&obj(widget), b"AP"), b"N");
        let res = d.dict_get(&n, b"Resources");
        raw(d, &d.dict_get(&res, b"Font"), b"Helv")
    }

    #[test]
    fn a_value_goes_on_the_field_and_every_widget_is_made_again() {
        let d = doc();
        let filled = fill(&d, &[set("name", "Grace")]).unwrap();
        assert_eq!(filled.changes[0].old, "Ada");
        assert_eq!(filled.changes[0].new, "Grace");
        let made: Vec<i32> = filled.made.iter().map(|m| m.object).collect();
        assert_eq!(made, [5, 6]);
        assert_eq!(
            d.dict_get(&obj(4), b"V"),
            Object::String(Arc::from(&b"Grace"[..]))
        );
        // One font object for both appearances, made before the first.
        assert!(matches!(helv(&d, 5), Object::Ref(_)));
        assert_eq!(helv(&d, 5), helv(&d, 6));
        assert!(d.dict_get(&helv(&d, 5), b"BaseFont").is_name(b"Helvetica"));
    }

    #[test]
    fn the_same_value_still_makes_the_widget_again() {
        let d = doc();
        let filled = fill(&d, &[set("name", "Ada")]).unwrap();
        assert!(!filled.changes[0].changed);
        // Only the widget the value went through: the field is not dirty.
        let made: Vec<i32> = filled.made.iter().map(|m| m.object).collect();
        assert_eq!(made, [5]);
    }

    #[test]
    fn a_toggle_changes_state_and_no_appearance() {
        let d = doc();
        let filled = fill(&d, &[Edit::Toggle { name: "ok".into() }]).unwrap();
        assert_eq!(filled.changes[0].new, "Yes");
        assert!(filled.made.is_empty());
        assert!(d.dict_get(&obj(7), b"AS").is_name(b"Yes"));
        let filled = fill(&d, &[Edit::Toggle { name: "ok".into() }]).unwrap();
        assert_eq!(filled.changes[0].new, "Off");
    }

    #[test]
    fn an_unknown_field_changes_nothing() {
        let d = doc();
        let e = fill(&d, &[set("name", "x"), set("nobody", "y")]).unwrap_err();
        assert_eq!(e, FillError::UnknownField("nobody".into()));
        assert!(!d.is_edited());
    }
}
