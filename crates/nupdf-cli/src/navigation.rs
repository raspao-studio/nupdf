// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `outline`, `links`, `labels` and `attachments`: how a document is found
//! around in, as MuPDF reads it (oracle/js/navigation.js).

use std::path::Path;

use nupdf::Document;
use nupdf::outline::Outline;
use serde_json::{Value, json};

fn page1(n: i32) -> Value {
    if n >= 0 { (n + 1).into() } else { Value::Null }
}

fn outline_json(items: &[Outline]) -> Value {
    items
        .iter()
        .map(|o| {
            let mut v = json!({
                "title": o.item.title,
                "uri": o.item.uri,
                "open": o.item.is_open,
                "page": page1(o.page),
            });
            if !o.down.is_empty() {
                v["down"] = outline_json(&o.down);
            }
            v
        })
        .collect()
}

/// The outline, each entry with the page it leads to (from 1) and the
/// entries below it.
pub fn outline(doc: &Document) -> Result<Value, String> {
    let items = nupdf::outline::load_outline(doc)?;
    Ok(json!({ "items": outline_json(&items) }))
}

fn number(v: f32) -> Value {
    if v.is_finite() {
        json!(f64::from(v))
    } else {
        Value::Null
    }
}

/// Each page, with `f` of it and its transform, or the error loading it
/// as `pdf_load_page` would: the page object; with /Annots, its transform
/// (for the links); then its inherited /Resources.
fn each_page(
    doc: &Document,
    f: impl Fn(i32, &nupdf::Object, nupdf::geometry::Matrix) -> Value,
) -> Result<Value, String> {
    let count = doc.page_count().map_err(|e| e.0)?;
    let pages: Vec<Value> = (0..count)
        .map(|i| {
            let loaded = doc.page(i).map_err(|e| e.0).and_then(|page| {
                if doc.resolve(&page).as_dict().is_none() {
                    return Err("page object is not a dictionary".to_string());
                }
                let ctm = if nupdf::link::has_annots(doc, &page) {
                    doc.page_transform(&page).map_err(|e| e.0)?.1
                } else {
                    nupdf::geometry::Matrix::IDENTITY
                };
                doc.inherited(&page, b"Resources").map_err(|e| e.0)?;
                Ok((page, ctm))
            });
            match loaded {
                Ok((page, ctm)) => {
                    let mut entry = f(i, &page, ctm);
                    entry["page"] = (i + 1).into();
                    entry
                }
                Err(e) => json!({ "page": i + 1, "error": e }),
            }
        })
        .collect();
    Ok(json!({ "page_count": count, "pages": pages }))
}

/// Each page's links: their boxes in page space, their URIs and the pages
/// inside the document they lead to.
pub fn links(doc: &Document) -> Result<Value, String> {
    each_page(doc, |i, page, ctm| {
        let links: Vec<Value> = nupdf::link::load_links(doc, page, i, ctm)
            .iter()
            .map(|l| {
                let target = nupdf::link::resolve_link_page(doc, &l.uri).unwrap_or(-1);
                json!({
                    "bbox": [number(l.rect.x0), number(l.rect.y0), number(l.rect.x1), number(l.rect.y1)],
                    "uri": l.uri,
                    "page": page1(target),
                })
            })
            .collect();
        json!({ "links": links })
    })
}

/// Each page's label, as a viewer shows it.
pub fn labels(doc: &Document) -> Result<Value, String> {
    each_page(
        doc,
        |i, _, _| json!({ "label": nupdf::page_label::page_label(doc, i) }),
    )
}

/// The embedded files, and with `dir` each one's contents written there
/// under its name.
pub fn attachments(doc: &Document, dir: Option<&Path>) -> Value {
    let files: Vec<Value> = nupdf::embedded::embedded_files(doc)
        .into_iter()
        .enumerate()
        .map(|(n, (name, fs))| {
            let p = nupdf::embedded::filespec_params(doc, &fs);
            let time = |t: i64| if t >= 0 { Value::from(t) } else { Value::Null };
            let mut entry = json!({
                "name": name,
                "filename": p.filename,
                "embedded": nupdf::embedded::is_embedded_file(doc, &fs),
                "mimetype": p.mimetype,
                "size": if p.size >= 0 { Value::from(p.size) } else { Value::Null },
                "created": time(p.created),
                "modified": time(p.modified),
            });
            if entry["embedded"] == true {
                match nupdf::embedded::contents(doc, &fs).unwrap_or(Ok(Vec::new())) {
                    Ok(data) => {
                        entry["length"] = data.len().into();
                        match nupdf::embedded::checksum_ok(doc, &fs) {
                            Ok(ok) => entry["checksum_ok"] = ok.into(),
                            Err(e) => entry["error"] = e.into(),
                        }
                        if let Some(dir) = dir {
                            // A file name of the document's choosing goes
                            // nowhere but into `dir`.
                            let file = format!("attachment-{:04}-{}", n + 1, safe_name(&name));
                            match crate::write_file(&dir.join(&file), &data) {
                                Ok(()) => entry["file"] = file.into(),
                                Err(e) => entry["write_error"] = e.to_string().into(),
                            }
                        }
                    }
                    Err(e) => entry["error"] = e.into(),
                }
            }
            entry
        })
        .collect();
    json!({ "attachments": files })
}

/// A name safe to write in a directory: letters, digits, `.`, `-` and `_`,
/// the rest as `_`, at most 100 characters, never `.` or `..`.
fn safe_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    if s.is_empty() || s.chars().all(|c| c == '.') {
        "file".into()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_names_cannot_leave_the_directory() {
        assert_eq!(safe_name("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name("report (final).pdf"), "report__final_.pdf");
        assert_eq!(safe_name(""), "file");
    }
}
