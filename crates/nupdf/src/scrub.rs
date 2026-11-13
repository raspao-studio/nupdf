// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! nupdf's own document scrub, after the pages are redacted: copies of
//! the targets outside the page contents go too. Strings that match are
//! cut out of outline titles, annotation and field text, page labels and
//! layer names; metadata entries, XMP packets, structure /Alt, /ActualText
//! and /E, JavaScript, named destinations and embedded files that match
//! are removed, and redacted pages lose their thumbnails. MuPDF's
//! redaction does none of this (AGENT-INTERFACE, Redaction contract).

use std::sync::Arc;

use crate::document::Document;
use crate::object::{Dict, Object};
use crate::regexp::{self, Regex};

/// What counts as a target outside the page text: each text target as a
/// pattern (white space matching any run of white space), and each
/// regular expression.
#[derive(Debug)]
pub struct Matcher {
    res: Vec<Regex>,
}

/// A regular expression for literal `text`, its white space loosened.
fn literal_pattern(text: &str) -> String {
    let mut out = String::new();
    let mut in_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !in_space {
                out.push_str("[ \t\r\n]+");
            }
            in_space = true;
            continue;
        }
        in_space = false;
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl Matcher {
    pub fn new(texts: &[String], regexes: &[String], ignore_case: bool) -> Result<Matcher, String> {
        let flags = if ignore_case { regexp::ICASE } else { 0 };
        let mut res = Vec::new();
        for t in texts {
            if !t.trim().is_empty() {
                res.push(Regex::compile(literal_pattern(t).as_bytes(), flags)?);
            }
        }
        for r in regexes {
            res.push(Regex::compile(r.as_bytes(), flags)?);
        }
        Ok(Matcher { res })
    }

    pub fn is_empty(&self) -> bool {
        self.res.is_empty()
    }

    /// The byte ranges of `s` that match, in order, not overlapping.
    pub fn find(&self, s: &[u8]) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::new();
        for re in &self.res {
            let mut at = 0;
            // Each step moves on by at least a byte.
            while at <= s.len() {
                let flags = if at > 0 { regexp::NOTBOL } else { 0 } | regexp::RUNAWAY;
                match re.exec(s, at, flags) {
                    Ok(Some((a, b))) if b > a => {
                        out.push((a, b));
                        at = b;
                    }
                    Ok(Some((a, _))) => at = a + 1,
                    _ => break,
                }
            }
        }
        out.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (a, b) in out {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        merged
    }

    pub fn matches(&self, s: &[u8]) -> bool {
        !self.find(s).is_empty()
    }

    /// `s` with every match cut out.
    pub fn strip(&self, s: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(s.len());
        let mut at = 0;
        for (a, b) in self.find(s) {
            out.extend_from_slice(&s[at..a]);
            at = b;
        }
        out.extend_from_slice(&s[at..]);
        out
    }

    /// A PDF text string's text as UTF-8, matched.
    pub fn matches_text(&self, s: &[u8]) -> bool {
        self.matches(&crate::text_string::utf8_from_pdf_string(s))
    }

    /// A PDF text string with the matches cut out of its text; None when
    /// nothing matched.
    pub fn strip_text(&self, s: &[u8]) -> Option<Object> {
        let text = crate::text_string::utf8_from_pdf_string(s);
        if !self.matches(&text) {
            return None;
        }
        let kept = self.strip(&text);
        Some(crate::edit::text_string(&String::from_utf8_lossy(&kept)))
    }
}

/// What the scrub took out, by kind.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub metadata: usize,
    pub xmp: usize,
    pub outline: usize,
    pub names: usize,
    pub embedded_files: usize,
    pub annotations: usize,
    pub fields: usize,
    pub javascript: usize,
    pub structure: usize,
    pub layers: usize,
    pub page_labels: usize,
    pub thumbnails: usize,
}

/// Keys whose text strings are cut, not removed.
const CUT_KEYS: &[&[u8]] = &[
    b"Title",
    b"Contents",
    b"T",
    b"TU",
    b"TM",
    b"Subj",
    b"RC",
    b"V",
    b"DV",
    b"CA",
    b"P",
    b"URI",
];

/// Keys removed when they match.
const DROP_KEYS: &[&[u8]] = &[b"Alt", b"ActualText", b"E"];

fn stream_text(doc: &Document, obj: &Object) -> Option<Vec<u8>> {
    match doc.resolve(obj) {
        Object::Stream(s) => doc.stream_data(&s).ok(),
        Object::String(s) => Some(s.to_vec()),
        _ => None,
    }
}

/// The scrub of one dictionary: what changed, as (kind, count) bumps.
fn scrub_dict(doc: &Document, m: &Matcher, d: &mut Dict, report: &mut Report) -> bool {
    let mut changed = false;
    let is_annot = d.get(b"Subtype").is_some_and(|s| {
        doc.resolve(s)
            .as_name()
            .is_some_and(|n| n != b"Form" && n != b"Image" && n != b"Type1" && n != b"TrueType")
    }) && d.get(b"Rect").is_some();
    let is_field = d.get(b"FT").is_some() || d.get(b"Kids").is_some() && d.get(b"T").is_some();
    // Structure and marked content.
    for key in DROP_KEYS {
        if let Some(Object::String(s)) = d.get(key).map(|v| doc.resolve(v))
            && m.matches_text(&s)
        {
            d.remove(key);
            report.structure += 1;
            changed = true;
        }
    }
    // JavaScript.
    if let Some(js) = d.get(b"JS").cloned()
        && stream_text(doc, &js).is_some_and(|t| m.matches(&t))
    {
        d.put(b"JS", Object::String(Arc::from(&b""[..])));
        report.javascript += 1;
        changed = true;
    }
    // Text strings, cut.
    for key in CUT_KEYS {
        if let Some(Object::String(s)) = d.get(key).cloned()
            && let Some(cut) = m.strip_text(&s)
        {
            d.put(key, cut);
            changed = true;
            if is_field {
                report.fields += 1;
            } else if is_annot {
                report.annotations += 1;
            } else if *key == b"Title" {
                report.outline += 1;
            } else if *key == b"P" {
                report.page_labels += 1;
            } else {
                report.annotations += 1;
            }
        }
    }
    // An appearance that shows a target goes: viewers draw the annotation
    // without it, or regenerate a field's.
    if (is_annot || is_field)
        && let Some(ap) = d.get(b"AP").cloned()
        && appearance_matches(doc, m, &ap)
    {
        d.remove(b"AP");
        report.annotations += 1;
        changed = true;
    }
    // Optional content group names.
    if d.get(b"Type").is_some_and(|t| t.is_name(b"OCG"))
        && let Some(Object::String(s)) = d.get(b"Name").cloned()
        && let Some(cut) = m.strip_text(&s)
    {
        d.put(b"Name", cut);
        report.layers += 1;
        changed = true;
    }
    // Embedded files: names, descriptions and contents.
    if d.get(b"EF").is_some() || d.get(b"Type").is_some_and(|t| t.is_name(b"Filespec")) {
        let mut hit = false;
        for key in [&b"F"[..], b"UF", b"Desc"] {
            if let Some(Object::String(s)) = d.get(key).map(|v| doc.resolve(v))
                && m.matches_text(&s)
            {
                hit = true;
            }
        }
        if let Some(Object::Dict(ef)) = d.get(b"EF").map(|v| doc.resolve(v)) {
            for (_, f) in ef.iter() {
                if stream_text(doc, f).is_some_and(|t| m.matches(&t)) {
                    hit = true;
                }
            }
        }
        if hit {
            d.remove(b"EF");
            d.remove(b"RF");
            for key in [&b"F"[..], b"UF", b"Desc"] {
                if let Some(Object::String(s)) = d.get(key).cloned()
                    && let Some(cut) = m.strip_text(&s)
                {
                    d.put(key, cut);
                }
            }
            report.embedded_files += 1;
            changed = true;
        }
    }
    // XMP packets.
    if let Some(md) = d.get(b"Metadata").cloned()
        && stream_text(doc, &md).is_some_and(|t| m.matches(&t))
    {
        d.remove(b"Metadata");
        report.xmp += 1;
        changed = true;
    }
    changed
}

/// Whether any appearance stream of `ap` shows target text, as literal
/// bytes in its content.
fn appearance_matches(doc: &Document, m: &Matcher, ap: &Object) -> bool {
    let Object::Dict(ap) = doc.resolve(ap) else {
        return false;
    };
    ap.iter().any(|(_, v)| match doc.resolve(v) {
        Object::Stream(_) => stream_text(doc, v).is_some_and(|t| m.matches(&t)),
        Object::Dict(states) => states
            .iter()
            .any(|(_, s)| stream_text(doc, s).is_some_and(|t| m.matches(&t))),
        _ => false,
    })
}

/// A name tree's pairs whose keys match, cut from its /Names arrays.
fn scrub_name_tree(doc: &Document, m: &Matcher, node: &Object, depth: usize, report: &mut Report) {
    if depth > 64 {
        return;
    }
    let Some(r) = node.as_ref() else {
        return;
    };
    let Object::Dict(d) = doc.resolve(node) else {
        return;
    };
    if let Some(Object::Array(names)) = d.get(b"Names").map(|n| doc.resolve(n)) {
        let mut kept = Vec::new();
        let mut cut = 0;
        for pair in names.chunks(2) {
            let hit = matches!(doc.resolve(&pair[0]), Object::String(ref s) if m.matches_text(s));
            if hit {
                cut += 1;
            } else {
                kept.extend_from_slice(pair);
            }
        }
        if cut > 0 {
            report.names += cut;
            let arr = Object::Array(Arc::new(kept));
            match d.get(b"Names") {
                Some(Object::Ref(a)) => doc.alter_object(a.num, arr),
                _ => {
                    let mut d2 = (*d).clone();
                    d2.put(b"Names", arr);
                    doc.alter_object(r.num, Object::Dict(Arc::new(d2)));
                }
            }
        }
    }
    if let Some(Object::Array(kids)) = d.get(b"Kids").map(|k| doc.resolve(k)) {
        for k in kids.iter() {
            scrub_name_tree(doc, m, k, depth + 1, report);
        }
    }
}

/// Scrubs the document of copies of the targets. `pages` are the pages
/// redacted (0-based), whose thumbnails go.
pub fn scrub(doc: &Document, m: &Matcher, pages: &[i32]) -> Report {
    let mut report = Report::default();
    // Thumbnails show the page as it was.
    for &i in pages {
        let has_thumb = doc
            .page(i)
            .ok()
            .is_some_and(|p| !matches!(doc.dict_get(&p, b"Thumb"), Object::Null));
        if has_thumb && crate::edit::edit_page(doc, i, &mut |d| d.remove(b"Thumb")).is_ok() {
            report.thumbnails += 1;
        }
    }
    if m.is_empty() {
        return report;
    }
    // The information dictionary.
    let mut trailer = doc.trailer();
    if let Some(info) = trailer.get(b"Info").cloned()
        && let Object::Dict(d) = doc.resolve(&info)
    {
        let mut d2 = (*d).clone();
        let mut cut = 0;
        for (k, v) in d.iter() {
            if let Object::String(s) = doc.resolve(v)
                && m.matches_text(&s)
            {
                d2.remove(k);
                cut += 1;
            }
        }
        if cut > 0 {
            report.metadata += cut;
            match info {
                Object::Ref(r) => doc.alter_object(r.num, Object::Dict(Arc::new(d2))),
                _ => {
                    trailer.put(b"Info", Object::Dict(Arc::new(d2)));
                    doc.set_trailer(trailer.clone());
                }
            }
        }
    }
    // Name trees: destinations, embedded files, JavaScript.
    let root = trailer.get(b"Root").cloned().unwrap_or(Object::Null);
    if let Object::Dict(names) = doc.dict_get(&root, b"Names") {
        for (_, tree) in names.iter() {
            scrub_name_tree(doc, m, tree, 0, &mut report);
        }
    }
    // Old-style named destinations.
    if let Some(dests) = doc
        .resolve(&root)
        .as_dict()
        .and_then(|c| c.get(b"Dests").cloned())
        && let (Some(r), Object::Dict(d)) = (dests.as_ref(), doc.resolve(&dests))
    {
        let mut d2 = (*d).clone();
        let mut cut = 0;
        for (k, _) in d.iter() {
            if m.matches(k) {
                d2.remove(k);
                cut += 1;
            }
        }
        if cut > 0 {
            report.names += cut;
            doc.alter_object(r.num, Object::Dict(Arc::new(d2)));
        }
    }
    // Every object, dictionaries and the dictionaries directly in them.
    for num in 1..doc.xref_len() as i32 {
        let Some(obj) = doc.object(num) else { continue };
        match obj {
            Object::Dict(d) => {
                let mut d2 = (*d).clone();
                let mut changed = scrub_dict(doc, m, &mut d2, &mut report);
                changed |= scrub_children(doc, m, &mut d2, &mut report);
                if changed {
                    doc.alter_object(num, Object::Dict(Arc::new(d2)));
                }
            }
            Object::Stream(s) => {
                let mut d2 = s.dict.clone();
                let a = scrub_dict(doc, m, &mut d2, &mut report);
                if a | scrub_children(doc, m, &mut d2, &mut report)
                    && let Ok(data) = doc.raw_stream_data(&s)
                {
                    doc.replace_stream(num, d2, data.into_owned());
                }
            }
            Object::Array(_) => {
                if let Some(new) = scrub_value(doc, m, &obj, &mut report, 0) {
                    doc.alter_object(num, new);
                }
            }
            _ => {}
        }
    }
    report
}

/// The direct dictionaries and arrays held in `d` (an action, a
/// marked-content property list, a page-label tree held in the catalog...),
/// scrubbed in place, down to a bounded depth.
fn scrub_children(doc: &Document, m: &Matcher, d: &mut Dict, report: &mut Report) -> bool {
    scrub_children_at(doc, m, d, report, 0)
}

fn scrub_children_at(
    doc: &Document,
    m: &Matcher,
    d: &mut Dict,
    report: &mut Report,
    depth: usize,
) -> bool {
    if depth > 16 {
        return false;
    }
    let mut changed = false;
    let keys: Vec<Vec<u8>> = d
        .iter()
        .filter(|(_, v)| matches!(v, Object::Dict(_) | Object::Array(_)))
        .map(|(k, _)| k.to_vec())
        .collect();
    for k in keys {
        if let Some(v) = d.get(&k).cloned()
            && let Some(new) = scrub_value(doc, m, &v, report, depth + 1)
        {
            d.put(&k, new);
            changed = true;
        }
    }
    changed
}

/// A direct value scrubbed: its replacement, or None when unchanged.
fn scrub_value(
    doc: &Document,
    m: &Matcher,
    v: &Object,
    report: &mut Report,
    depth: usize,
) -> Option<Object> {
    match v {
        Object::Dict(c) => {
            let mut c2 = (**c).clone();
            let a = scrub_dict(doc, m, &mut c2, report);
            let b = scrub_children_at(doc, m, &mut c2, report, depth);
            (a || b).then(|| Object::Dict(Arc::new(c2)))
        }
        Object::Array(items) if depth <= 16 => {
            let mut changed = false;
            let mut items2 = (**items).clone();
            for item in items2.iter_mut() {
                if let Some(new) = scrub_value(doc, m, item, report, depth + 1) {
                    *item = new;
                    changed = true;
                }
            }
            changed.then(|| Object::Array(Arc::new(items2)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_clean::tests::page_doc;
    use crate::object::ObjRef;
    use crate::testutil::stream;

    fn s(text: &str) -> Object {
        Object::String(Arc::from(text.as_bytes()))
    }

    #[test]
    fn literals_match_across_any_white_space() {
        let m = Matcher::new(&["Jane  Doe".into()], &[], false).unwrap_or_else(|e| panic!("{e}"));
        assert!(m.matches(b"to Jane\nDoe, re"));
        assert_eq!(m.strip(b"to Jane Doe."), b"to .");
        let i = Matcher::new(&["jane".into()], &[], true).unwrap_or_else(|e| panic!("{e}"));
        assert!(i.matches(b"JANE"));
        let r = Matcher::new(&[], &["[0-9]{3}-[0-9]{2}-[0-9]{4}".into()], false)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(r.find(b"a 123-45-6789 b 987-65-4321"), [(2, 13), (16, 27)]);
    }

    #[test]
    fn copies_outside_the_page_are_cut_or_removed() {
        let doc = page_doc(b"BT /F1 12 Tf 72 700 Td (nothing) Tj ET");
        let mut info = Dict::new();
        info.put(b"Title", s("SECRET report"));
        info.put(b"Author", s("Ops"));
        let num = doc.add_object(Object::Dict(Arc::new(info)));
        let mut item = Dict::new();
        item.put(b"Title", s("SECRET chapter"));
        let outline = doc.add_object(Object::Dict(Arc::new(item)));
        let mut js = Dict::new();
        js.put(b"S", Object::name(b"JavaScript"));
        js.put(b"JS", s("app.alert('SECRET')"));
        let action = doc.add_object(Object::Dict(Arc::new(js)));
        let mut trailer = doc.trailer();
        trailer.put(b"Info", Object::Ref(ObjRef::new(num, 0)));
        doc.set_trailer(trailer);
        let m = Matcher::new(&["SECRET".into()], &[], false).unwrap_or_else(|e| panic!("{e}"));
        let report = scrub(&doc, &m, &[0]);
        assert_eq!(report.metadata, 1);
        assert_eq!(report.outline, 1);
        assert_eq!(report.javascript, 1);
        let get = |n: i32, k: &[u8]| doc.dict_get(&Object::Ref(ObjRef::new(n, 0)), k);
        assert_eq!(get(num, b"Title"), Object::Null);
        assert_eq!(get(num, b"Author"), s("Ops"));
        assert_eq!(get(outline, b"Title"), s(" chapter"));
        assert_eq!(get(action, b"JS"), s(""));
    }

    #[test]
    fn patterns_are_literal_and_blank_targets_match_nothing() {
        let m = Matcher::new(&["a.b".into(), "  ".into()], &["x*".into()], false)
            .unwrap_or_else(|e| panic!("{e}"));
        // The dot is a dot; the empty matches of x* between the x es
        // count for nothing.
        assert_eq!(m.find(b"axb a.b xx"), [(1, 2), (4, 7), (8, 10)]);
        assert_eq!(m.strip_text(b"nothing here"), None);
        assert!(Matcher::new(&[], &[], false).is_ok_and(|m| m.is_empty()));
        assert!(Matcher::new(&[], &["(".into()], false).is_err());
    }

    /// Object `num`'s entry `key`.
    fn entry(doc: &Document, num: i32, key: &[u8]) -> Object {
        doc.dict_get(&Object::Ref(ObjRef::new(num, 0)), key)
    }

    fn printed(obj: &Object) -> String {
        crate::print::to_string(obj, crate::print::Options::default())
    }

    #[test]
    fn every_kind_of_copy_is_found() {
        let deep = format!(
            "{}<</Alt(SECRET deep)>>{}",
            "<</A".repeat(18),
            ">>".repeat(18)
        );
        let deep_array = format!("{}(SECRET){}", "[".repeat(20), "]".repeat(20));
        let objects: Vec<Vec<u8>> = vec![
            // 1
            b"<</Type/Catalog/Pages 2 0 R/Names<</Dests 10 0 R/EmbeddedFiles 11 0 R>>/Dests 12 0 R\
              /OCProperties<</OCGs[13 0 R]>>/Metadata 14 0 R/PageLabels<</Nums[0<</P(SECRET-)>>]>>>>"
                .to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Annots[5 0 R 6 0 R 7 0 R]/Thumb 15 0 R/Contents 4 0 R>>".to_vec(),
            stream("", b""),
            // 5: an annotation whose appearance shows a target.
            b"<</Type/Annot/Subtype/Text/Rect[0 0 10 10]/Contents(the SECRET note)/AP<</N 16 0 R>>>>".to_vec(),
            // 6: a field whose appearance states show none.
            b"<</Type/Annot/Subtype/Widget/FT/Tx/T(name)/V(SECRET value)/Rect[0 0 10 10]/AP<</N<</On 17 0 R>>>>>>".to_vec(),
            // 7: a link to a URI with a target.
            b"<</Type/Annot/Subtype/Link/Rect[0 0 10 10]/A<</S/URI/URI(http://x/SECRET)>>>>".to_vec(),
            // 8: a structure element, its /ActualText held indirectly.
            b"<</Type/StructElem/S/P/Alt(SECRET alt)/ActualText 18 0 R/E(fine)>>".to_vec(),
            // 9: a script in a stream.
            b"<</S/JavaScript/JS 19 0 R>>".to_vec(),
            // 10, 11: name trees, one with kids.
            b"<</Kids[20 0 R]>>".to_vec(),
            b"<</Names[(a.txt)21 0 R(b.txt)23 0 R]>>".to_vec(),
            // 12: old-style destinations.
            b"<</SECRET[3 0 R/Fit]/keep[3 0 R/Fit]>>".to_vec(),
            // 13: a layer.
            b"<</Type/OCG/Name(SECRET layer)>>".to_vec(),
            // 14: XMP.
            stream("/Type/Metadata/Subtype/XML", b"<x:xmpmeta>SECRET</x:xmpmeta>"),
            // 15: a thumbnail.
            stream("", b"\0\0\0"),
            // 16, 17: appearance streams.
            stream("/Subtype/Form", b"BT (SECRET) Tj ET"),
            stream("/Subtype/Form", b"BT (fine) Tj ET"),
            // 18, 19
            b"(SECRET actual)".to_vec(),
            stream("", b"app.alert('SECRET')"),
            // 20: a leaf of the destinations tree, its pairs held apart.
            b"<</Names 24 0 R>>".to_vec(),
            // 21: a file specification whose file holds a target.
            b"<</Type/Filespec/F(a.txt)/UF(a.txt)/Desc(a file)/EF<</F 22 0 R>>>>".to_vec(),
            stream("/Type/EmbeddedFile", b"SECRET data"),
            // 23: one whose name holds it.
            b"<</Type/Filespec/F(SECRET.txt)/EF<</F 25 0 R>>/RF<<>>>>".to_vec(),
            b"[(SECRET dest)[3 0 R/Fit](other)[3 0 R/Fit]]".to_vec(),
            stream("/Type/EmbeddedFile", b"plain"),
            // 26: an array object holding a structure dictionary.
            b"[<</Alt(SECRET in array)>>(x)]".to_vec(),
            // 27: an image whose dictionary has an /Alt.
            stream("/Type/XObject/Subtype/Image/Alt(SECRET image)", b"raw"),
            // 28, 29: nested deeper than the scrub looks.
            deep.into_bytes(),
            deep_array.into_bytes(),
            // 30: text outside annotations and fields.
            b"<</Contents(SECRET elsewhere)/Title(no target)>>".to_vec(),
            // 31: a field node with kids.
            b"<</Kids[6 0 R]/T(SECRET parent)>>".to_vec(),
        ];
        let doc = crate::testutil::doc_bytes(&objects);
        let mut trailer = doc.trailer();
        let mut info = Dict::new();
        info.put(b"Title", s("SECRET"));
        info.put(b"Author", s("Ops"));
        trailer.put(b"Info", Object::Dict(Arc::new(info)));
        doc.set_trailer(trailer);
        let m = Matcher::new(&["SECRET".into()], &[], false).unwrap_or_else(|e| panic!("{e}"));
        let report = scrub(&doc, &m, &[0]);
        assert_eq!(
            report,
            Report {
                metadata: 1,
                xmp: 1,
                outline: 0,
                names: 2,
                embedded_files: 2,
                annotations: 4,
                fields: 2,
                javascript: 1,
                structure: 4,
                layers: 1,
                page_labels: 1,
                thumbnails: 1,
            }
        );
        // The information dictionary, held directly in the trailer.
        assert_eq!(
            printed(doc.trailer().get(b"Info").unwrap_or(&Object::Null)),
            "<</Author(Ops)>>"
        );
        assert_eq!(printed(&entry(&doc, 1, b"Metadata")), "null");
        assert_eq!(
            printed(&entry(&doc, 1, b"PageLabels")),
            "<</Nums[0<</P(-)>>]>>"
        );
        assert_eq!(printed(&entry(&doc, 3, b"Thumb")), "null");
        assert_eq!(printed(&entry(&doc, 5, b"Contents")), "(the  note)");
        assert_eq!(printed(&entry(&doc, 5, b"AP")), "null");
        assert_eq!(printed(&entry(&doc, 6, b"V")), "( value)");
        assert_eq!(printed(&entry(&doc, 6, b"AP")), "<</N<</On 17 0 R>>>>");
        assert_eq!(printed(&entry(&doc, 7, b"A")), "<</S/URI/URI(http://x/)>>");
        assert_eq!(printed(&entry(&doc, 8, b"Alt")), "null");
        assert_eq!(printed(&entry(&doc, 8, b"ActualText")), "null");
        assert_eq!(printed(&entry(&doc, 8, b"E")), "(fine)");
        assert_eq!(printed(&entry(&doc, 9, b"JS")), "()");
        assert_eq!(
            printed(&doc.resolve(&Object::Ref(ObjRef::new(24, 0)))),
            "[(other)[3 0 R/Fit]]"
        );
        assert_eq!(printed(&entry(&doc, 12, b"SECRET")), "null");
        assert_eq!(printed(&entry(&doc, 12, b"keep")), "[3 0 R/Fit]");
        assert_eq!(printed(&entry(&doc, 13, b"Name")), "( layer)");
        // Both files lose their data; names are cut.
        assert_eq!(printed(&entry(&doc, 21, b"EF")), "null");
        assert_eq!(printed(&entry(&doc, 23, b"EF")), "null");
        assert_eq!(printed(&entry(&doc, 23, b"RF")), "null");
        assert_eq!(printed(&entry(&doc, 23, b"F")), "(.txt)");
        assert_eq!(
            printed(&doc.resolve(&Object::Ref(ObjRef::new(26, 0)))),
            "[<<>>(x)]"
        );
        assert_eq!(printed(&entry(&doc, 27, b"Alt")), "null");
        let Some(Object::Stream(image)) = doc.object(27) else {
            panic!("no image")
        };
        assert_eq!(doc.stream_data(&image).ok().as_deref(), Some(&b"raw"[..]));
        // Too deep to look into.
        assert!(printed(&doc.resolve(&Object::Ref(ObjRef::new(28, 0)))).contains("SECRET"));
        assert!(printed(&doc.resolve(&Object::Ref(ObjRef::new(29, 0)))).contains("SECRET"));
        assert_eq!(printed(&entry(&doc, 30, b"Contents")), "( elsewhere)");
        assert_eq!(printed(&entry(&doc, 31, b"T")), "( parent)");
    }

    #[test]
    fn with_no_targets_only_thumbnails_go() {
        let doc = page_doc(b"");
        let m = Matcher::new(&[], &[], false).unwrap_or_else(|e| panic!("{e}"));
        // The page has no thumbnail, and page 5 does not exist.
        assert_eq!(scrub(&doc, &m, &[0, 5]), Report::default());
    }
}
