// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Checks a JSON value against one of the reports' schemas: the part of
//! JSON Schema they use (`type`, `properties`, `required`, `items`,
//! `additionalProperties`, `anyOf`, `$ref` into `$defs`). Shared by the
//! CLI's tests and `cargo xtask schemas --validate`.

#![allow(dead_code)]

use serde_json::Value;

/// Where `value` breaks `schema`, as JSON pointers with what was expected.
pub fn violations(schema: &Value, value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    check(schema, schema, value, "", &mut out);
    out
}

fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "number",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_ok(want: &Value, v: &Value) -> bool {
    let t = type_of(v);
    let one = |w: &str| w == t || (w == "number" && t == "integer");
    match want {
        Value::String(w) => one(w),
        Value::Array(ws) => ws.iter().filter_map(Value::as_str).any(one),
        _ => true,
    }
}

fn check(root: &Value, schema: &Value, v: &Value, at: &str, out: &mut Vec<String>) {
    if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
        let target = r
            .strip_prefix("#/$defs/")
            .and_then(|name| root.get("$defs")?.get(name));
        match target {
            Some(t) => check(root, t, v, at, out),
            None => out.push(format!("{at}: unknown $ref {r}")),
        }
        return;
    }
    if let Some(Value::Array(any)) = schema.get("anyOf") {
        let fits = any.iter().any(|s| {
            let mut found = Vec::new();
            check(root, s, v, at, &mut found);
            found.is_empty()
        });
        if !fits {
            out.push(format!("{at}: none of anyOf fits {}", type_of(v)));
        }
        return;
    }
    if let Some(want) = schema.get("type")
        && !type_ok(want, v)
    {
        out.push(format!("{at}: {} where {want} was expected", type_of(v)));
        return;
    }
    match v {
        Value::Object(m) => {
            if let Some(Value::Array(required)) = schema.get("required") {
                for k in required.iter().filter_map(Value::as_str) {
                    if !m.contains_key(k) {
                        out.push(format!("{at}: missing {k}"));
                    }
                }
            }
            let props = schema.get("properties").and_then(Value::as_object);
            for (k, value) in m {
                let at = format!("{at}/{k}");
                match (
                    props.and_then(|p| p.get(k)),
                    schema.get("additionalProperties"),
                ) {
                    (Some(s), _) => check(root, s, value, &at, out),
                    (None, Some(Value::Bool(false))) => out.push(format!("{at}: not allowed")),
                    (None, Some(s @ Value::Object(_))) => check(root, s, value, &at, out),
                    // Fields a schema does not know are allowed: adding one
                    // is not a breaking change.
                    (None, _) => {}
                }
            }
        }
        Value::Array(items) => {
            if let Some(s) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    check(root, s, item, &format!("{at}/{i}"), out);
                }
            }
        }
        _ => {}
    }
}
