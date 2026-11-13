// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-form.c (pdf_field_type,
// pdf_field_flags, pdf_field_value, pdf_choice_field_option,
// pdf_text_widget_max_len, pdf_field_label, load_field_name,
// pdf_signature_is_signed, pdf_widget_is_readonly),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Form fields as MuPDF's widget getters read them: a widget's field type,
//! flags, value, options, name and label, read through the /Parent chain.

use crate::document::Document;
use crate::object::Object;

/// Field names longer than this are refused, as MuPDF refuses them.
const MAX_NAME: usize = 16 << 10;
/// /Parent chains longer than this are taken for cycles.
const MAX_DEPTH: usize = 256;

pub const READ_ONLY: i32 = 1;
pub const BTN_PUSHBUTTON: i32 = 1 << 16;
pub const BTN_RADIO: i32 = 1 << 15;
pub const CH_COMBO: i32 = 1 << 17;

/// `pdf_dict_get_inheritable` along a field's /Parent chain.
pub fn inheritable(doc: &Document, field: &Object, key: &[u8]) -> Object {
    doc.inherited(field, key)
        .ok()
        .flatten()
        .unwrap_or(Object::Null)
}

/// `pdf_field_flags` (/Ff).
pub fn flags(doc: &Document, field: &Object) -> i32 {
    doc.resolve(&inheritable(doc, field, b"Ff")).to_int()
}

/// `pdf_field_type_string`: button, checkbox, radiobutton, text, combobox,
/// listbox or signature, by /FT and the flags.
pub fn type_string(doc: &Document, field: &Object) -> &'static str {
    let ft = doc.resolve(&inheritable(doc, field, b"FT"));
    let f = flags(doc, field);
    match ft.as_name() {
        Some(b"Btn") if f & BTN_PUSHBUTTON != 0 => "button",
        Some(b"Btn") if f & BTN_RADIO != 0 => "radiobutton",
        Some(b"Btn") => "checkbox",
        Some(b"Tx") => "text",
        Some(b"Ch") if f & CH_COMBO != 0 => "combobox",
        Some(b"Ch") => "listbox",
        Some(b"Sig") => "signature",
        _ => "button",
    }
}

fn text(doc: &Document, obj: &Object) -> String {
    match doc.resolve(obj) {
        Object::String(s) => crate::text_string::text_string(&s),
        _ => String::new(),
    }
}

/// `pdf_field_value`: a name as itself, a stream as its text, else the
/// text string.
pub fn value(doc: &Document, field: &Object) -> String {
    let v = inheritable(doc, field, b"V");
    match doc.resolve(&v) {
        Object::Name(n) => String::from_utf8_lossy(&n).into_owned(),
        Object::Stream(s) => doc
            .stream_data(&s)
            .map(|d| crate::text_string::text_string(&d))
            .unwrap_or_default(),
        _ => text(doc, &v),
    }
}

/// `pdf_choice_field_option` for every option: each one's export value and
/// the text shown for it (the same when /Opt gives one string).
pub fn options(doc: &Document, field: &Object) -> Vec<(String, String)> {
    let opt = doc.resolve(&inheritable(doc, field, b"Opt"));
    opt.as_array()
        .unwrap_or(&[])
        .iter()
        .map(|ent| match doc.resolve(ent) {
            Object::Array(pair) if pair.len() == 2 => (text(doc, &pair[0]), text(doc, &pair[1])),
            other => {
                let s = text(doc, &other);
                (s.clone(), s)
            }
        })
        .collect()
}

/// `pdf_text_widget_max_len` (/MaxLen).
pub fn max_len(doc: &Document, field: &Object) -> i32 {
    doc.resolve(&inheritable(doc, field, b"MaxLen")).to_int()
}

/// `pdf_field_label`: /TU, else /T, else "Unnamed".
pub fn label(doc: &Document, field: &Object) -> String {
    for key in [&b"TU"[..], b"T"] {
        let v = inheritable(doc, field, key);
        if !matches!(v, Object::Null) {
            return text(doc, &v);
        }
    }
    "Unnamed".into()
}

/// `pdf_load_field_name`: the field's /T and its ancestors', outermost
/// first, joined by dots.
pub fn name(doc: &Document, field: &Object) -> Result<String, String> {
    let mut parts = Vec::new();
    let mut seen = Vec::new();
    let mut node = field.clone();
    let mut total = 0usize;
    for _ in 0..MAX_DEPTH {
        if let Object::Ref(r) = &node {
            if seen.contains(&r.num) {
                return Err("Cycle in field parents".into());
            }
            seen.push(r.num);
        }
        let t = text(doc, &doc.dict_get(&node, b"T"));
        // strlen: up to the first NUL.
        let t = t.split('\0').next().unwrap_or("").to_string();
        if t.len() > MAX_NAME || t.len() + total > MAX_NAME {
            return Err("Field name too long".into());
        }
        if !t.is_empty() {
            total += t.len() + 1;
            parts.push(t);
        }
        match doc.parent_of(&node) {
            None => {
                parts.reverse();
                return Ok(parts.join("."));
            }
            Some(parent) => node = parent,
        }
    }
    Err("Cycle in field parents".into())
}

/// `pdf_signature_is_signed`: a signature field whose value is a
/// dictionary, of /Type /Sig if it has a type.
pub fn is_signed(doc: &Document, field: &Object) -> bool {
    if doc.resolve(&inheritable(doc, field, b"FT")).as_name() != Some(b"Sig") {
        return false;
    }
    let v = doc.resolve(&inheritable(doc, field, b"V"));
    let Some(d) = v.as_dict() else { return false };
    match d.get(b"Type").map(|t| doc.resolve(t)) {
        None | Some(Object::Null) => true,
        Some(t) => t.as_name() == Some(b"Sig"),
    }
}

/// `pdf_widget_is_readonly`.
pub fn is_read_only(doc: &Document, field: &Object) -> bool {
    flags(doc, field) & READ_ONLY != 0
}

/// Whether the catalog's /AcroForm has an /XFA entry: the form is (also)
/// an XFA form, which MuPDF does not run.
pub fn has_xfa(doc: &Document) -> bool {
    let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
    let acro = doc.dict_get(&root, b"AcroForm");
    !matches!(doc.resolve(&doc.dict_get(&acro, b"XFA")), Object::Null)
}

/// /AcroForm /NeedAppearances: viewers are to make the fields' appearances.
pub fn need_appearances(doc: &Document) -> bool {
    let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
    let acro = doc.dict_get(&root, b"AcroForm");
    matches!(
        doc.resolve(&doc.dict_get(&acro, b"NeedAppearances")),
        Object::Bool(true)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ObjRef;

    fn r(n: i32) -> Object {
        Object::Ref(ObjRef::new(n, 0))
    }

    fn fields() -> Document {
        let long = "x".repeat(MAX_NAME + 1);
        let objs = [
            "<</Type/Catalog/Pages 2 0 R>>".to_string(),
            "<</Type/Pages/Kids[]/Count 0>>".to_string(),
            "<</FT/Btn/Ff 65536>>".into(),
            "<</FT/Btn/Ff 32768>>".into(),
            "<</FT/Btn>>".into(),
            "<</FT/Ch/Ff 131072>>".into(),
            "<</FT/Ch>>".into(),
            "<</FT/Sig/V<</Type/Sig>>>>".into(),
            "<</FT/Odd/V 16 0 R>>".into(),
            // 10, 11: a cycle of parents; 12: a name too long.
            "<</T(a)/Parent 11 0 R>>".into(),
            "<</T(b)/Parent 10 0 R>>".into(),
            format!("<</T({long})>>"),
            "<</FT/Sig/V<</Type/Other>>>>".into(),
            "<</FT/Sig/V 5>>".into(),
            "<</FT/Sig/V<<>>>>".into(),
            crate::testutil::stream("", b"from a stream")
                .iter()
                .map(|&b| b as char)
                .collect(),
        ];
        let refs: Vec<&str> = objs.iter().map(String::as_str).collect();
        crate::testutil::doc_of(&refs)
    }

    #[test]
    fn types_values_names_and_signatures_read_as_mupdf_reads_them() {
        let doc = fields();
        let types: Vec<&str> = (3..=9).map(|n| type_string(&doc, &r(n))).collect();
        assert_eq!(
            types,
            [
                "button",
                "radiobutton",
                "checkbox",
                "combobox",
                "listbox",
                "signature",
                "button"
            ]
        );
        assert_eq!(value(&doc, &r(9)), "from a stream");
        assert_eq!(label(&doc, &r(3)), "Unnamed");
        assert_eq!(name(&doc, &r(10)), Err("Cycle in field parents".into()));
        assert_eq!(name(&doc, &r(12)), Err("Field name too long".into()));
        let signed: Vec<bool> = [8, 13, 14, 15, 3]
            .iter()
            .map(|&n| is_signed(&doc, &r(n)))
            .collect();
        assert_eq!(signed, [true, false, false, true, false]);
    }
}
