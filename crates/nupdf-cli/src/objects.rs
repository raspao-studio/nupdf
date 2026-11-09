// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf objects`: every object as canonical JSON, in the encoding that
//! oracle/js/objects.js produces, so the two can be diffed object by object.

use nupdf::{Document, Object};
use serde_json::{Map, Value, json};

pub fn dump(doc: &Document) -> Value {
    // The count is taken first and objects are loaded in ascending order,
    // exactly like the oracle, so a lazy repair happens at the same point.
    let count = doc.xref_len();
    let mut objects = Map::new();
    for num in 1..count {
        let value = match doc.object(num as i32) {
            Some(Object::Stream(s)) => {
                let raw = doc.raw_stream_data(&s);
                json!({ "stream": encode_dict(&s.dict), "raw_len": raw.len() })
            }
            Some(obj) => encode(&obj),
            None => Value::Null,
        };
        objects.insert(num.to_string(), value);
    }
    json!({ "object_count": count, "objects": objects })
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
