// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! The `stext` command: each page's structured text, as MuPDF's script
//! glue walks it (oracle/js/stext.js).

use nupdf::Document;
use nupdf::geometry::{Point, Quad, Rect};
use nupdf::stext::{Block, Options, StextPage};
use serde_json::{Map, Value, json};

/// Each page's blocks, or the error that stopped its run.
pub fn stext(doc: &Document, opts: Options, threads: Option<usize>) -> Value {
    let mut out = Map::new();
    let count = doc.page_count();
    out.insert(
        "page_count".into(),
        count.as_ref().map_or(Value::Null, |n| json!(n)),
    );
    let indices: Vec<i32> = (0..count.unwrap_or(0)).collect();
    let pages = nupdf::parallel::map(doc, &indices, opts, threads, &|_, r| match r {
        Ok(p) => json!({ "blocks": blocks(&p) }),
        Err(e) => json!({ "error": e }),
    });
    out.insert("pages".into(), Value::Array(pages));
    Value::Object(out)
}

fn number(v: f32) -> Value {
    if v.is_finite() {
        json!(f64::from(v))
    } else {
        Value::Null
    }
}

fn rect(r: Rect) -> Value {
    json!([number(r.x0), number(r.y0), number(r.x1), number(r.y1)])
}

fn point(p: Point) -> Value {
    json!([number(p.x), number(p.y)])
}

fn quad(q: Quad) -> Value {
    json!([
        number(q.ul.x),
        number(q.ul.y),
        number(q.ur.x),
        number(q.ur.y),
        number(q.ll.x),
        number(q.ll.y),
        number(q.lr.x),
        number(q.lr.y)
    ])
}

/// A character as the glue's `fz_runetochar` and string push give it:
/// U+FFFD past Unicode (NUL is written overlong, so it stays).
fn char_text(c: i32) -> String {
    u32::try_from(c)
        .ok()
        .and_then(char::from_u32)
        .unwrap_or('\u{fffd}')
        .to_string()
}

fn blocks(page: &StextPage) -> Value {
    page.blocks
        .iter()
        .map(|b| match b {
            Block::Text(t) => json!({
                "type": "text",
                "bbox": rect(t.bbox),
                "flags": t.flags,
                "lines": t.lines.iter().map(|l| json!({
                    "bbox": rect(l.bbox),
                    "wmode": l.wmode,
                    "dir": point(l.dir),
                    "chars": l.chars.iter().map(|c| json!([
                        char_text(c.c),
                        point(c.origin),
                        crate::trace::script_string(&c.font.name),
                        number(c.size),
                        quad(c.quad),
                        c.flags,
                        c.bidi,
                    ])).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            }),
            Block::Image(i) => json!({
                "type": "image",
                "bbox": rect(i.bbox),
                "transform": [
                    number(i.transform.a), number(i.transform.b), number(i.transform.c),
                    number(i.transform.d), number(i.transform.e), number(i.transform.f)
                ],
            }),
        })
        .collect()
}
