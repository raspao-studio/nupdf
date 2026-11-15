// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `tables`: each page's tables as rows of cells, from the `Table`, `TR`
//! and `TD` structures MuPDF's table hunting makes (`stext --options
//! table-hunt`, checked against MuPDF by the `stext-table` checks).

use nupdf::Document;
use nupdf::stext::{Block, Options};
use serde_json::{Value, json};

fn number(v: f32) -> Value {
    if v.is_finite() {
        json!(f64::from(v))
    } else {
        Value::Null
    }
}

fn rect(r: nupdf::geometry::Rect) -> Value {
    json!([number(r.x0), number(r.y0), number(r.x1), number(r.y1)])
}

/// A cell's text: its lines, each as `extract` gives a line's text, joined
/// by newlines.
fn cell_text(blocks: &[Block], out: &mut Vec<String>) {
    for b in blocks {
        match b {
            Block::Text(t) => {
                for l in &t.lines {
                    let s = nupdf::extract::chars_text(&l.chars);
                    // ASCII white space only: a line of U+0085 or U+00A0 keeps it.
                    let s =
                        s.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'));
                    if !s.is_empty() {
                        out.push(s.to_string());
                    }
                }
            }
            Block::Struct(s) => cell_text(&s.blocks, out),
            _ => {}
        }
    }
}

fn structs<'a>(
    blocks: &'a [Block],
    standard: &str,
) -> impl Iterator<Item = &'a nupdf::stext::StructBlock> {
    let standard = standard.to_string();
    blocks.iter().filter_map(move |b| match b {
        Block::Struct(s) if s.standard == standard => Some(s),
        _ => None,
    })
}

/// Every table in `blocks`, at any depth, in order.
fn find_tables(blocks: &[Block], out: &mut Vec<Value>) {
    for b in blocks {
        let Block::Struct(s) = b else { continue };
        if s.standard != "Table" {
            find_tables(&s.blocks, out);
            continue;
        }
        let rows: Vec<Value> = structs(&s.blocks, "TR")
            .map(|row| {
                let cells: Vec<Value> = structs(&row.blocks, "TD")
                    .map(|cell| {
                        let mut lines = Vec::new();
                        cell_text(&cell.blocks, &mut lines);
                        json!({ "bbox": rect(cell.bbox), "text": lines.join("\n") })
                    })
                    .collect();
                json!({ "bbox": rect(row.bbox), "cells": cells })
            })
            .collect();
        let columns = rows
            .iter()
            .map(|r| r["cells"].as_array().map_or(0, Vec::len))
            .max()
            .unwrap_or(0);
        out.push(json!({ "bbox": rect(s.bbox), "columns": columns, "rows": rows }));
    }
}

/// Each page's tables, with `opts`, table hunting and the vectors it
/// finds ruled tables by (MuPDF finds none in a ruled table without them).
pub fn tables(
    doc: &Document,
    page_count: Option<i32>,
    indices: &[i32],
    opts: Options,
    threads: Option<usize>,
) -> Value {
    let opts = Options {
        flags: opts.flags | nupdf::stext::flags::TABLE_HUNT | nupdf::stext::flags::COLLECT_VECTORS,
        ..opts
    };
    let threads = crate::threads_for(threads, indices.len());
    let pages: Vec<Value> =
        nupdf::parallel::map(doc, indices, opts, threads, &|i, page| match page {
            Ok(p) => {
                let mut found = Vec::new();
                find_tables(&p.blocks, &mut found);
                json!({ "page": indices[i] + 1, "tables": found })
            }
            Err(e) => json!({ "page": indices[i] + 1, "error": e }),
        });
    json!({ "page_count": page_count, "pages": pages })
}

/// The tables as CSV: each table's rows, a blank line between tables.
pub fn csv(report: &Value) -> String {
    let field = |s: &str| {
        if s.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    };
    let mut out = String::new();
    let tables = report["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p["tables"].as_array().into_iter().flatten());
    for (n, table) in tables.enumerate() {
        if n > 0 {
            out.push('\n');
        }
        for row in table["rows"].as_array().into_iter().flatten() {
            let cells: Vec<String> = row["cells"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| field(c["text"].as_str().unwrap_or("")))
                .collect();
            out.push_str(&cells.join(","));
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_what_needs_it() {
        let report = json!({"pages": [{"tables": [
            {"rows": [{"cells": [{"text": "a"}, {"text": "b, c"}]}, {"cells": [{"text": "say \"hi\""}, {"text": "two\nlines"}]}]},
            {"rows": [{"cells": [{"text": "x"}]}]}
        ]}]});
        assert_eq!(
            csv(&report),
            "a,\"b, c\"\n\"say \"\"hi\"\"\",\"two\nlines\"\n\nx\n"
        );
    }
}
