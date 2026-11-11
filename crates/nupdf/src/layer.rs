// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-layer.c (pdf_read_ocg,
// pdf_select_layer_config, pdf_is_ocg_hidden), Copyright (C) 2004-2026
// Artifex Software, Inc.

//! Optional content: which groups the default configuration turns off,
//! and whether a group or membership dictionary hides what it marks.

use crate::diag::report;
use crate::document::Document;
use crate::object::Object;

/// Membership dictionaries nested deeper than this are treated as cycles.
const MAX_DEPTH: usize = 100;

/// `pdf_ocg_descriptor`: the document's groups with their states under the
/// default configuration.
#[derive(Debug, Clone, Default)]
pub struct Descriptor {
    /// Each group as listed (usually a reference), with its state; sorted
    /// by object number, highest first, as MuPDF sorts them.
    ocgs: Vec<(Object, bool)>,
    intent: Option<Object>,
}

impl Descriptor {
    /// `pdf_read_ocg`: a broken configuration is reported and reads as
    /// having no groups.
    pub fn read(doc: &Document) -> Descriptor {
        match read(doc) {
            Ok(d) => d,
            Err(line) => {
                let diag = doc.diagnostics();
                diag.warn("error.reported", line);
                diag.warn(
                    "layer.broken_config",
                    "Ignoring broken Optional Content configuration",
                );
                Descriptor::default()
            }
        }
    }

    /// `pdf_is_ocg_hidden` for `usage`, with names looked up by `lookup`
    /// (the Properties resources); None usage shows everything.
    pub fn is_hidden(
        &self,
        doc: &Document,
        usage: Option<&str>,
        ocg: &Object,
        lookup: &dyn Fn(&[u8]) -> Option<Object>,
    ) -> bool {
        let Some(usage) = usage else {
            return false;
        };
        self.hidden(doc, usage, ocg, lookup, &mut Vec::new())
    }

    fn hidden(
        &self,
        doc: &Document,
        usage: &str,
        ocg: &Object,
        lookup: &dyn Fn(&[u8]) -> Option<Object>,
        cycle: &mut Vec<i32>,
    ) -> bool {
        if self.ocgs.is_empty() {
            return false;
        }
        let ocg = match doc.resolve(ocg) {
            Object::Name(n) => match lookup(&n) {
                Some(o) => o,
                None => return false,
            },
            Object::Null => return false,
            _ => ocg.clone(),
        };
        let num = ocg.as_ref().map_or(0, |r| r.num);
        if (num != 0 && cycle.contains(&num)) || cycle.len() >= MAX_DEPTH {
            return false;
        }
        cycle.push(num);
        let out = self.hidden_imp(doc, usage, &ocg, lookup, cycle);
        cycle.pop();
        out
    }

    fn hidden_imp(
        &self,
        doc: &Document,
        usage: &str,
        ocg: &Object,
        lookup: &dyn Fn(&[u8]) -> Option<Object>,
        cycle: &mut Vec<i32>,
    ) -> bool {
        let get = |key: &[u8]| doc.dict_get(ocg, key);
        let kind = get(b"Type");
        if kind.is_name(b"OCG") {
            // Deliberately unresolved, as MuPDF compares them.
            let default = self
                .ocgs
                .iter()
                .find(|(o, _)| o == ocg)
                .is_some_and(|(_, on)| !on);
            let check_intent = match &self.intent {
                None => false,
                Some(Object::Array(a)) => !a.is_empty(),
                Some(_) => true,
            };
            if check_intent {
                match get(b"Intent") {
                    Object::Name(n) => {
                        if !self.intents_include(doc, &n) {
                            return true;
                        }
                    }
                    Object::Array(a) => {
                        let any = a.iter().any(|i| {
                            self.intents_include(doc, doc.resolve(i).as_name().unwrap_or(b""))
                        });
                        if !any {
                            return true;
                        }
                    }
                    _ => {
                        if !self.intents_include(doc, b"View") {
                            return true;
                        }
                    }
                }
            }
            let usage_dict = get(b"Usage");
            if usage_dict.as_dict().is_none() {
                return default;
            }
            let by_usage = doc.dict_get(&usage_dict, usage.as_bytes());
            let event = format!("{usage}State");
            if doc.dict_get(&by_usage, event.as_bytes()).is_name(b"OFF") {
                return true;
            }
            default
        } else if kind.is_name(b"OCMD") {
            if matches!(get(b"VE"), Object::Array(_)) {
                return false;
            }
            // Bit 0: AND; bit 1: true means off.
            let combine = match get(b"P").as_name() {
                Some(b"AllOn") => 1,
                Some(b"AnyOff") => 2,
                Some(b"AllOff") => 3,
                _ => 0,
            };
            let ocgs = get(b"OCGs");
            let mut on = combine & 1 != 0;
            if let Object::Array(list) = &ocgs {
                for item in list.iter() {
                    let mut hidden = self.hidden(doc, usage, item, lookup, cycle);
                    if combine & 1 == 0 {
                        hidden = !hidden;
                    }
                    if combine & 2 != 0 {
                        on &= hidden;
                    } else {
                        on |= hidden;
                    }
                }
            } else {
                // The entry as stored: a reference is not resolved first.
                let entry = doc
                    .resolve(ocg)
                    .as_dict()
                    .and_then(|d| d.get(b"OCGs").cloned())
                    .unwrap_or(Object::Null);
                on = self.hidden(doc, usage, &entry, lookup, cycle);
                if combine & 1 == 0 {
                    on = !on;
                }
            }
            !on
        } else {
            false
        }
    }

    /// `ocg_intents_include`.
    fn intents_include(&self, doc: &Document, name: &[u8]) -> bool {
        if name == b"All" {
            return true;
        }
        match self.intent.as_ref().map(|i| doc.resolve(i)) {
            None => name == b"View",
            Some(Object::Name(i)) => i.as_ref() == b"All" || i.as_ref() == name,
            Some(Object::Array(a)) => a.iter().any(|i| {
                let i = doc.resolve(i);
                let i = i.as_name().unwrap_or(b"");
                i == b"All" || i == name
            }),
            Some(_) => false,
        }
    }
}

fn read(doc: &Document) -> Result<Descriptor, String> {
    let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
    let prop = doc.dict_get(&root, b"OCProperties");
    let ocgs = match doc.dict_get(&prop, b"OCGs") {
        Object::Array(a) => a.to_vec(),
        _ => Vec::new(),
    };
    let num = |o: &Object| o.as_ref().map_or(0, |r| r.num);
    let mut list: Vec<(Object, bool)> = ocgs.into_iter().map(|o| (o, true)).collect();
    list.sort_by_key(|(o, _)| std::cmp::Reverse(num(o)));
    let mut desc = Descriptor {
        ocgs: list,
        intent: None,
    };
    // pdf_select_layer_config(-1).
    if matches!(prop, Object::Null) {
        return Ok(desc);
    }
    let config = doc.dict_get(&prop, b"D");
    if matches!(config, Object::Null) {
        return Err(report::format("No default Layer config"));
    }
    desc.intent = match doc
        .resolve(&config)
        .as_dict()
        .and_then(|d| d.get(b"Intent"))
    {
        Some(Object::Null) | None => None,
        Some(i) => Some(i.clone()),
    };
    let base = doc.dict_get(&config, b"BaseState");
    if !base.is_name(b"Unchanged") {
        let on = !base.is_name(b"OFF");
        for entry in &mut desc.ocgs {
            entry.1 = on;
        }
    }
    for (key, state) in [(&b"ON"[..], true), (&b"OFF"[..], false)] {
        if let Object::Array(items) = doc.dict_get(&config, key) {
            for item in &items[..] {
                if let Some(entry) = desc.ocgs.iter_mut().find(|(o, _)| o == item) {
                    entry.1 = state;
                }
            }
        }
    }
    Ok(desc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(body: &str) -> Document {
        let pdf = format!(
            "%PDF-1.5\n1 0 obj<</Type/Catalog{body}>>endobj\n\
             2 0 obj<</Type/OCG/Name(A)>>endobj\n\
             3 0 obj<</Type/OCG/Name(B)>>endobj\n\
             4 0 obj<</Type/OCMD/OCGs[2 0 R 3 0 R]/P/AllOn>>endobj\n\
             5 0 obj<</Type/OCG/Usage<</View<</ViewState/OFF>>>>>>endobj\n\
             trailer<</Root 1 0 R/Size 6>>\n"
        );
        Document::open(pdf.into_bytes()).unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn r(n: i32) -> Object {
        Object::Ref(crate::ObjRef::new(n, 0))
    }

    #[test]
    fn groups_off_in_the_default_configuration_are_hidden() {
        let d = doc("/OCProperties<</OCGs[2 0 R 3 0 R 5 0 R]/D<</OFF[3 0 R]>>>>");
        let desc = Descriptor::read(&d);
        let none = |_: &[u8]| None;
        let hidden = |o: &Object| desc.is_hidden(&d, Some("View"), o, &none);
        assert!(!hidden(&r(2)));
        assert!(hidden(&r(3)));
        // AllOn starts on and only ORs its members in, so MuPDF shows it
        // whatever they are.
        assert!(!hidden(&r(4)));
        // The usage dictionary turns it off for viewing.
        assert!(hidden(&r(5)));
        // Without a usage everything shows.
        assert!(!desc.is_hidden(&d, None, &r(3), &none));
        // Names are looked up in the properties.
        let props = |n: &[u8]| (n == b"oc").then(|| r(3));
        assert!(desc.is_hidden(&d, Some("View"), &Object::name(b"oc"), &props));
    }

    #[test]
    fn a_configuration_without_a_default_is_broken() {
        let d = doc("/OCProperties<</OCGs[2 0 R]>>");
        let desc = Descriptor::read(&d);
        assert!(desc.ocgs.is_empty());
        let messages: Vec<String> = d
            .diagnostics()
            .snapshot()
            .into_iter()
            .map(|w| w.message)
            .collect();
        assert!(messages.contains(&"format error: No default Layer config".to_string()));
    }
}
