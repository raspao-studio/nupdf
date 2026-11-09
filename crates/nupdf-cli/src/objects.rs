// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf objects`: every object as canonical JSON, in the encoding that
//! oracle/js/objects.js produces, so the two can be diffed object by object.

use nupdf::{Document, Object};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value, json};

/// The `objects` report body, serialized as it is written: each object is
/// loaded, encoded and written in turn, so memory does not grow with the
/// size of the report.
pub struct Dump<'a> {
    pub doc: &'a Document,
    pub count: usize,
}

impl<'a> Dump<'a> {
    /// The count is taken here, before any object is loaded, as the oracle
    /// does; objects are then loaded in ascending order, so a lazy repair
    /// happens at the same point.
    pub fn new(doc: &'a Document) -> Self {
        Dump {
            doc,
            count: doc.xref_len(),
        }
    }
}

impl Serialize for Dump<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.count.saturating_sub(1)))?;
        for num in 1..self.count {
            let value = match self.doc.object(num as i32) {
                Some(Object::Stream(s)) => {
                    let raw = self.doc.raw_stream_data(&s);
                    json!({ "stream": encode_dict(&s.dict), "raw_len": raw.len() })
                }
                Some(obj) => encode(&obj),
                None => Value::Null,
            };
            map.serialize_entry(&num.to_string(), &value)?;
        }
        map.end()
    }
}

/// Pages and their inherited attributes, in oracle/js/pages.js's format.
pub fn pages(doc: &Document) -> Value {
    let mut out = Map::new();
    let count = doc.page_count();
    out.insert(
        "page_count".into(),
        count.as_ref().map_or(Value::Null, |n| json!(n)),
    );
    if let Err(e) = &count {
        out.insert("page_count_error".into(), json!(e.0));
    }
    let mut pages = Vec::new();
    for index in 0..count.unwrap_or(0) {
        let entry = doc.page(index).and_then(|page| {
            // Same order as the oracle: the first failing lookup wins, and a
            // parent repair triggered by one lookup applies to the next.
            let attr = |key: &[u8]| -> Result<Value, nupdf::PageError> {
                Ok(doc
                    .inherited(&page, key)?
                    .map_or(Value::Null, |v| encode(&v)))
            };
            let media = attr(b"MediaBox")?;
            let crop = attr(b"CropBox")?;
            let rotate = attr(b"Rotate")?;
            let resources = summarize(doc.inherited(&page, b"Resources")?);
            Ok(json!({
                "object": match &page {
                    Object::Ref(r) => format!("{} {} R", r.num, r.generation),
                    _ => "direct".to_string(),
                },
                "MediaBox": media,
                "CropBox": crop,
                "Rotate": rotate,
                "Resources": resources,
            }))
        });
        pages.push(entry.unwrap_or_else(|e| json!({ "error": e.0 })));
    }
    out.insert("pages".into(), Value::Array(pages));
    out.insert(
        "page_count_after".into(),
        doc.page_count().map_or(Value::Null, |n| json!(n)),
    );
    Value::Object(out)
}

/// A reference, the sorted keys of a dictionary, or the encoded value.
fn summarize(value: Option<Object>) -> Value {
    match value {
        None | Some(Object::Null) => Value::Null,
        Some(Object::Dict(d)) => {
            let mut keys: Vec<String> = d.iter().map(|(k, _)| escape_name(k)).collect();
            keys.sort();
            json!({ "keys": keys })
        }
        Some(other) => encode(&other),
    }
}

pub fn encode(obj: &Object) -> Value {
    match obj {
        Object::Null => Value::Null,
        Object::Bool(b) => Value::Bool(*b),
        Object::Int(i) => json!(i),
        Object::Real(f) => json!({ "r": f64::from(*f) }),
        Object::Name(n) => json!({ "n": escape_name(n) }),
        Object::String(s) => json!({ "s": hex(s) }),
        Object::Array(items) => Value::Array(items.iter().map(encode).collect()),
        Object::Dict(d) => encode_dict(d),
        Object::Ref(r) => json!({ "ref": format!("{} {} R", r.num, r.generation) }),
        Object::Stream(s) => encode_dict(&s.dict),
    }
}

fn encode_dict(dict: &nupdf::Dict) -> Value {
    let map: Map<String, Value> = dict
        .iter()
        .map(|(k, v)| (escape_name(k), encode(v)))
        .collect();
    json!({ "d": map })
}

/// A name as MuPDF prints it (`fmt_name`), without the slash: delimiters,
/// whitespace, '#' and non-printable bytes become `#XX`. Printing stops at a
/// NUL byte, as it does for C strings.
pub fn escape_name(name: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(name.len());
    for &b in name.iter().take_while(|&&b| b != 0) {
        let special = matches!(
            b,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%' | b'#'
        ) || nupdf::lex::is_white(b)
            || !(32..127).contains(&b);
        if special {
            let _ = write!(out, "#{b:02X}");
        } else {
            out.push(b as char);
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_escape_like_mupdf() {
        assert_eq!(escape_name(b"Type"), "Type");
        assert_eq!(escape_name(b"A B#C"), "A#20B#23C");
        assert_eq!(escape_name(b"\xba/"), "#BA#2F");
        assert_eq!(escape_name(b"ab\0cd"), "ab");
    }
}
