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

fn point(p: nupdf::geometry::Point) -> Value {
    json!([number(p.x), number(p.y)])
}

/// Each page's annotations as MuPDF lists them, with what its getters read
/// of each; a property a type does not have is left out. An annotation
/// without an appearance is read as loading the page leaves it, with the
/// appearance MuPDF makes for it. With `synthesize`, each one's appearance
/// is made again and given, as `annot.update()` after
/// `annot.requestResynthesis()` makes it; one MuPDF does not make again (a
/// Stamp with a non-standard name) has none.
pub fn annots(doc: &Document, synthesize: bool) -> Result<Value, String> {
    use nupdf::annot as a;
    each_page(doc, |i, page, ctm| {
        let annots = a::page_annots(doc, page).0;
        let again = if synthesize {
            nupdf::appearance::made_again(doc, &annots, ctm)
        } else {
            vec![false; annots.len()]
        };
        let list: Vec<Value> = annots
            .iter()
            .zip(again)
            .enumerate()
            .map(|(n, (orig, again))| {
                let made = (again || nupdf::appearance::needs_synthesis(doc, orig))
                    .then(|| nupdf::appearance::synthesize(doc, orig, ctm));
                if let Some(Err(e)) = &made {
                    doc.diagnostics().warn(
                        "annot.appearance",
                        format!("cannot create appearance stream: {e}"),
                    );
                }
                let made = made.and_then(Result::ok);
                let obj = made.as_ref().map_or(orig, |m| &m.annot);
                let mut v = json!({
                    "id": format!("p{}.a{}", i + 1, n + 1),
                    "object": match orig { nupdf::Object::Ref(r) => Value::from(r.num), _ => Value::Null },
                    "type": a::type_name(doc, obj),
                    "flags": a::flags(doc, obj),
                    "contents": a::contents(doc, obj),
                    "color": a::color(doc, obj).into_iter().map(number).collect::<Vec<_>>(),
                    "opacity": number(a::opacity(doc, obj)),
                    "created": a::created(doc, obj),
                    "modified": a::modified(doc, obj),
                });
                if let Some(r) = a::rect(doc, obj, ctm) {
                    v["rect"] = json!([number(r.x0), number(r.y0), number(r.x1), number(r.y1)]);
                }
                if let Some(s) = a::author(doc, obj) {
                    v["author"] = s.into();
                }
                if let Some(s) = a::subject(doc, obj) {
                    v["subject"] = s.into();
                }
                if let Some(c) = a::interior_color(doc, obj) {
                    v["interior_color"] = c.into_iter().map(number).collect();
                }
                if let Some(s) = a::icon(doc, obj) {
                    v["icon"] = s.into();
                }
                if let Some(o) = a::is_open(doc, obj) {
                    v["open"] = o.into();
                }
                if let Some((style, width)) = a::border(doc, obj) {
                    v["border"] = json!({ "style": style, "width": number(width) });
                }
                if let Some(l) = a::line(doc, obj, ctm) {
                    v["line"] = l.into_iter().map(point).collect();
                }
                if let Some(p) = a::vertices(doc, obj, ctm) {
                    v["vertices"] = p.into_iter().map(point).collect();
                }
                if let Some(q) = a::quad_points(doc, obj, ctm) {
                    v["quad_points"] = q
                        .iter()
                        .map(|q| q.iter().flat_map(|p| [number(p.x), number(p.y)]).collect::<Value>())
                        .collect();
                }
                if let Some(ink) = a::ink_list(doc, obj, ctm) {
                    v["ink_list"] = ink
                        .into_iter()
                        .map(|s| s.into_iter().map(point).collect::<Value>())
                        .collect();
                }
                if synthesize {
                    v["synthesis"] = match &made {
                        Some(m) if again => synthesis_json(doc, m),
                        _ => Value::Null,
                    };
                }
                v
            })
            .collect();
        json!({ "annots": list })
    })
}

/// A form XObject's BBox, Matrix, Resources and content (each byte as one
/// character).
fn form_json(
    bbox: nupdf::geometry::Rect,
    m: nupdf::geometry::Matrix,
    resources: Option<&nupdf::object::Dict>,
    content: &[u8],
) -> Value {
    json!({
        "bbox": [number(bbox.x0), number(bbox.y0), number(bbox.x1), number(bbox.y1)],
        "matrix": [number(m.a), number(m.b), number(m.c), number(m.d), number(m.e), number(m.f)],
        "resources": resources.map(|r| crate::objects::encode(&nupdf::Object::Dict(std::sync::Arc::new(r.clone())))),
        "content": content.iter().map(|&b| char::from(b)).collect::<String>(),
    })
}

/// A synthesis as the oracle gives it: the /Rect and /RD written back,
/// and the new form XObject.
fn synthesis_json(doc: &Document, m: &nupdf::appearance::Synthesis) -> Value {
    let mut v = form_json(m.bbox, m.matrix, m.resources.as_ref(), &m.content);
    v["rect"] = crate::objects::encode(&doc.dict_get(&m.annot, b"Rect"));
    v["rd"] = crate::objects::encode(&doc.dict_get(&m.annot, b"RD"));
    v
}

/// A widget's appearance made again: one stream, or a button's per state
/// (by /AP key, and state after a slash) with its /AS; null for a signed
/// signature, which MuPDF never makes again.
fn widget_synthesis(doc: &Document, w: &nupdf::Object, ctm: nupdf::geometry::Matrix) -> Value {
    use nupdf::appearance as ap;
    if !ap::widget_made_again(doc, w) {
        return Value::Null;
    }
    let warn = |e: &str| {
        doc.diagnostics().warn(
            "annot.appearance",
            format!("cannot create appearance stream: {e}"),
        );
        Value::Null
    };
    // A button by its /FT as stored, as MuPDF compares it.
    if !nupdf::form::inheritable(doc, w, b"FT").is_name(b"Btn") {
        return match ap::synthesize(doc, w, ctm) {
            Ok(m) => synthesis_json(doc, &m),
            Err(e) => warn(&e),
        };
    }
    match ap::synthesize_button(doc, w, ctm) {
        Ok(s) => {
            let mut states = serde_json::Map::new();
            for (key, state, f) in &s.forms {
                let name = match state {
                    Some(st) => format!("{key}/{}", String::from_utf8_lossy(st)),
                    None => (*key).to_string(),
                };
                states.insert(
                    name,
                    form_json(f.bbox, f.matrix, f.resources.as_ref(), &f.content),
                );
            }
            json!({
                "rect": crate::objects::encode(&doc.dict_get(&s.annot, b"Rect")),
                "rd": crate::objects::encode(&doc.dict_get(&s.annot, b"RD")),
                "as": crate::objects::encode(&doc.dict_get(&s.annot, b"AS")),
                "states": states,
            })
        }
        Err(e) => warn(&e),
    }
}

/// Each page's form widgets (`Page.getWidgets`), with what MuPDF's widget
/// getters read of each field; and whether the form needs appearances made
/// or is an XFA form. With `synthesize`, each widget's appearance is made
/// again and given, as `requestResynthesis()` then `update()` make it.
pub fn forms(doc: &Document, synthesize: bool) -> Result<Value, String> {
    use nupdf::form as f;
    let mut report = each_page(doc, |i, page, ctm| {
        let list: Vec<Value> = nupdf::annot::page_annots(doc, page)
            .1
            .iter()
            .enumerate()
            .map(|(n, w)| {
                let ty = f::type_string(doc, w);
                let mut v = json!({
                    "id": format!("p{}.w{}", i + 1, n + 1),
                    "object": match w { nupdf::Object::Ref(r) => Value::from(r.num), _ => Value::Null },
                    "type": ty,
                    "flags": f::flags(doc, w),
                    "value": f::value(doc, w),
                    "label": f::label(doc, w),
                    "read_only": f::is_read_only(doc, w),
                });
                match f::name(doc, w) {
                    Ok(name) => v["name"] = name.into(),
                    Err(e) => v["name_error"] = e.into(),
                }
                if let Some(r) = nupdf::annot::rect(doc, w, ctm) {
                    v["rect"] = json!([number(r.x0), number(r.y0), number(r.x1), number(r.y1)]);
                }
                match ty {
                    "text" => v["max_len"] = f::max_len(doc, w).into(),
                    "combobox" | "listbox" => {
                        v["options"] = f::options(doc, w)
                            .into_iter()
                            .map(|(export, shown)| json!({ "export": export, "text": shown }))
                            .collect();
                    }
                    "signature" => v["signed"] = f::is_signed(doc, w).into(),
                    _ => {}
                }
                if synthesize {
                    v["synthesis"] = widget_synthesis(doc, w, ctm);
                }
                v
            })
            .collect();
        json!({ "widgets": list })
    })?;
    report["need_appearances"] = f::need_appearances(doc).into();
    report["xfa"] = f::has_xfa(doc).into();
    Ok(report)
}

/// Each page's signature fields: whether signed, by whom, what the
/// signature dictionary says, the bytes it covers, whether the file has
/// changed since, and whether the signed digest is the digest of those
/// bytes (DECISIONS D47).
pub fn signatures(doc: &Document) -> Result<Value, String> {
    use nupdf::signature as sig;
    each_page(doc, |i, page, _| {
        let list: Vec<Value> = nupdf::annot::page_annots(doc, page)
            .1
            .iter()
            .enumerate()
            .filter(|(_, w)| nupdf::form::type_string(doc, w) == "signature")
            .map(|(n, w)| {
                let signed = nupdf::form::is_signed(doc, w);
                let mut v = json!({
                    "id": format!("p{}.w{}", i + 1, n + 1),
                    "signed": signed,
                });
                match nupdf::form::name(doc, w) {
                    Ok(name) => v["name"] = name.into(),
                    Err(e) => v["name_error"] = e.into(),
                }
                if !signed {
                    return v;
                }
                let value = nupdf::form::inheritable(doc, w, b"V");
                let get = |k: &[u8]| match doc.resolve(&doc.dict_get(&value, k)) {
                    nupdf::Object::Name(n) => Value::from(String::from_utf8_lossy(&n).into_owned()),
                    nupdf::Object::String(s) => Value::from(nupdf::text_string::text_string(&s)),
                    _ => Value::Null,
                };
                v["filter"] = get(b"Filter");
                v["sub_filter"] = get(b"SubFilter");
                v["reason"] = get(b"Reason");
                v["location"] = get(b"Location");
                v["signer_name"] = get(b"Name");
                v["signing_time"] = match doc.resolve(&doc.dict_get(&value, b"M")) {
                    nupdf::Object::String(s) => {
                        nupdf::date::parse_date(&s, doc.diagnostics()).into()
                    }
                    _ => Value::Null,
                };
                v["contents_length"] = sig::contents(doc, w).len().into();
                v["signatory"] = sig::signatory(doc, w)
                    .unwrap_or_else(|| "Signature information missing.".into())
                    .into();
                v["byte_range"] = sig::raw_byte_range(doc, w)
                    .iter()
                    .map(|&(o, l)| json!([o, l]))
                    .collect();
                match sig::changed_since_signing(doc, w) {
                    Ok(c) => v["changed_since_signing"] = c.into(),
                    Err(e) => v["changed_error"] = e.into(),
                }
                match sig::check_digest(doc, w) {
                    Ok(d) => v["digest"] = d.into(),
                    Err(e) => v["digest_error"] = e.into(),
                }
                v
            })
            .collect();
        json!({ "signatures": list })
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
