// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `annots --add`, `--delete` and `--highlight`: annotations added and
//! removed as MuPDF's annotation API does it (oracle/js/clean.js with the
//! `annots` option makes the same edits through MuPDF's JavaScript).

use nupdf::Document;
use nupdf::annot_edit::Editor;
use nupdf::geometry::{Point, Rect};
use serde_json::{Value, json};

use super::{Exit, Failure, FileFacts, write_document};
use crate::commands::WriteArgs;

/// The annotation edits `annots` was given.
pub struct AnnotEdits<'a> {
    pub add: &'a [String],
    pub delete: &'a [String],
    pub highlight: &'a [String],
}

fn usage(message: String) -> Failure {
    Failure {
        exit: Exit::Usage,
        code: "usage.bad_value",
        message,
    }
}

/// An annotation to add, as `--add` gives it.
#[derive(Debug, Default, PartialEq)]
struct NewAnnot {
    page: i32,
    kind: String,
    rect: Option<Rect>,
    popup: Option<Rect>,
    quad_points: Option<Vec<[Point; 4]>>,
    ink_list: Option<Vec<Vec<Point>>>,
    line: Option<[Point; 2]>,
    vertices: Option<Vec<Point>>,
    contents: Option<String>,
    author: Option<String>,
    subject: Option<String>,
    color: Option<Vec<f32>>,
    interior_color: Option<Vec<f32>>,
    opacity: Option<f32>,
    icon: Option<String>,
    border_width: Option<f32>,
    flags: Option<i32>,
    open: Option<bool>,
}

/// Fields the listing gives that an addition does not take.
const LISTED_ONLY: &[&str] = &["id", "object", "created", "modified", "synthesis"];

/// A JSON number as JavaScript gives it to MuPDF: a double, then a float.
fn number(v: &Value, what: &str) -> Result<f32, String> {
    v.as_f64()
        .map(|f| f as f32)
        .ok_or_else(|| format!("{what}: expected a number, got {v}"))
}

fn numbers(v: &Value, what: &str) -> Result<Vec<f32>, String> {
    v.as_array()
        .ok_or_else(|| format!("{what}: expected an array of numbers"))?
        .iter()
        .map(|n| number(n, what))
        .collect()
}

fn point(v: &Value, what: &str) -> Result<Point, String> {
    match numbers(v, what)?.as_slice() {
        &[x, y] => Ok(Point::new(x, y)),
        _ => Err(format!("{what}: a point is [x, y]")),
    }
}

fn points(v: &Value, what: &str) -> Result<Vec<Point>, String> {
    v.as_array()
        .ok_or_else(|| format!("{what}: expected an array of points"))?
        .iter()
        .map(|p| point(p, what))
        .collect()
}

fn rect(v: &Value, what: &str) -> Result<Rect, String> {
    match numbers(v, what)?.as_slice() {
        &[a, b, c, d] => Ok(Rect::new(a, b, c, d)),
        _ => Err(format!("{what}: a rectangle is [x0, y0, x1, y1]")),
    }
}

fn string(v: &Value, what: &str) -> Result<String, String> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{what}: expected a string"))
}

fn quads(v: &Value, what: &str) -> Result<Vec<[Point; 4]>, String> {
    let list = v
        .as_array()
        .ok_or_else(|| format!("{what}: expected an array of quads"))?;
    list.iter()
        .map(|q| match numbers(q, what)?.as_slice() {
            &[a, b, c, d, e, f, g, h] => Ok([
                Point::new(a, b),
                Point::new(c, d),
                Point::new(e, f),
                Point::new(g, h),
            ]),
            _ => Err(format!("{what}: a quad is 8 numbers")),
        })
        .collect()
}

/// `--add`'s JSON.
fn parse_add(spec: &str) -> Result<NewAnnot, String> {
    let v: Value = serde_json::from_str(spec).map_err(|e| format!("--add: {e}"))?;
    let obj = v.as_object().ok_or("--add: expected a JSON object")?;
    let mut a = NewAnnot::default();
    let mut listed_border = None;
    for (key, value) in obj {
        let k = key.as_str();
        match k {
            "page" => {
                a.page = value
                    .as_i64()
                    .and_then(|p| i32::try_from(p).ok())
                    .filter(|&p| p >= 1)
                    .ok_or("page: expected a page number from 1")?;
            }
            "type" => a.kind = string(value, k)?,
            "rect" => a.rect = Some(rect(value, k)?),
            "popup" => a.popup = Some(rect(value, k)?),
            "quad_points" => a.quad_points = Some(quads(value, k)?),
            "ink_list" => {
                let strokes = value
                    .as_array()
                    .ok_or("ink_list: expected an array of strokes")?;
                a.ink_list = Some(
                    strokes
                        .iter()
                        .map(|s| points(s, k))
                        .collect::<Result<_, _>>()?,
                );
            }
            "line" => match points(value, k)?.as_slice() {
                &[p, q] => a.line = Some([p, q]),
                _ => return Err("line: expected two points".into()),
            },
            "vertices" => a.vertices = Some(points(value, k)?),
            "contents" => a.contents = Some(string(value, k)?),
            "author" => a.author = Some(string(value, k)?),
            "subject" => a.subject = Some(string(value, k)?),
            "color" => a.color = Some(numbers(value, k)?),
            "interior_color" => a.interior_color = Some(numbers(value, k)?),
            "opacity" => a.opacity = Some(number(value, k)?),
            "icon" => a.icon = Some(string(value, k)?),
            "border_width" => a.border_width = Some(number(value, k)?),
            // The listing's border: its width.
            "border" => {
                let w = value
                    .get("width")
                    .ok_or("border: expected {\"width\": ...}")?;
                listed_border = Some(number(w, "border.width")?);
            }
            "flags" => {
                a.flags = Some(
                    value
                        .as_i64()
                        .and_then(|f| i32::try_from(f).ok())
                        .ok_or("flags: expected an integer")?,
                );
            }
            "open" => a.open = Some(value.as_bool().ok_or("open: expected true or false")?),
            _ if LISTED_ONLY.contains(&k) => {}
            _ => return Err(format!("--add: unknown field \"{k}\"")),
        }
    }
    if a.border_width.is_none() {
        a.border_width = listed_border;
    }
    if a.page == 0 {
        return Err("--add: \"page\" is required".into());
    }
    if a.kind.is_empty() {
        return Err("--add: \"type\" is required".into());
    }
    Ok(a)
}

/// `p1.a2`: page and annotation, from 0.
fn parse_id(id: &str) -> Option<(i32, usize)> {
    let (p, a) = id.strip_prefix('p')?.split_once(".a")?;
    let p: i32 = p.parse().ok()?;
    let a: usize = a.parse().ok()?;
    (p >= 1 && a >= 1).then_some((p - 1, a - 1))
}

/// The setters, in the order both nupdf and the oracle apply them.
fn apply(ed: &mut Editor, index: i32, num: i32, a: &NewAnnot) -> Result<(), String> {
    if let Some(r) = a.rect {
        ed.set_rect(index, num, r)?;
    }
    if let Some(r) = a.popup {
        ed.set_popup(index, num, r)?;
    }
    if let Some(q) = &a.quad_points {
        ed.set_quad_points(index, num, q)?;
    }
    if let Some(ink) = &a.ink_list {
        ed.set_ink_list(index, num, ink)?;
    }
    if let Some([p, q]) = a.line {
        ed.set_line(index, num, p, q)?;
    }
    if let Some(v) = &a.vertices {
        ed.set_vertices(index, num, v)?;
    }
    if let Some(s) = &a.contents {
        ed.set_contents(num, s);
    }
    if let Some(s) = &a.author {
        ed.set_author(num, s)?;
    }
    if let Some(s) = &a.subject {
        ed.set_subject(num, s)?;
    }
    if let Some(c) = &a.color {
        ed.set_color(num, c)?;
    }
    if let Some(c) = &a.interior_color {
        ed.set_interior_color(num, c)?;
    }
    if let Some(o) = a.opacity {
        ed.set_opacity(num, o);
    }
    if let Some(s) = &a.icon {
        ed.set_icon(num, Some(s))?;
    }
    if let Some(w) = a.border_width {
        ed.set_border_width(num, w)?;
    }
    if let Some(f) = a.flags {
        ed.set_flags(num, f);
    }
    if let Some(o) = a.open {
        ed.set_is_open(num, o);
    }
    Ok(())
}

/// A search hit: its page (from 0) and quads.
type Hit = (i32, Vec<[Point; 4]>);

/// Each hit of a search for `needle` before any change, case and all, as
/// `fz_match_page_cb` finds them (dehyphenated text, no clipping).
fn search_hits(doc: &Document, count: i32, needles: &[String]) -> Result<Vec<Hit>, String> {
    let mut hits = Vec::new();
    let all: Vec<i32> = (0..count).collect();
    let opts = nupdf::stext::Options {
        flags: nupdf::stext::flags::DEHYPHENATE,
        clip: Rect::INFINITE,
    };
    for (&i, page) in all.iter().zip(nupdf::parallel::sequential(doc, &all, opts)) {
        // A page that gives no text (not a dictionary, say) has no hits.
        let Ok(page) = page else { continue };
        for needle in needles {
            for hit in nupdf::search::search_page(&page, needle, 0)? {
                let quads = hit.quads.iter().map(|q| [q.ul, q.ur, q.ll, q.lr]).collect();
                hits.push((i, quads));
            }
        }
    }
    Ok(hits)
}

/// `annots --add/--delete/--highlight`: the edits made, then the pages
/// updated (the new annotations' appearances made) and the document
/// written as `clean` writes it.
pub fn edit_annots(
    doc: &Document,
    facts: &FileFacts,
    edits: &AnnotEdits,
    write: &WriteArgs,
    password: Option<&str>,
) -> Result<Value, Failure> {
    let plan = write.plan(nupdf::write::WriteOptions::default(), password)?;
    let adds = edits
        .add
        .iter()
        .map(|s| parse_add(s))
        .collect::<Result<Vec<_>, _>>()
        .map_err(usage)?;
    let deletes = edits
        .delete
        .iter()
        .map(|id| {
            parse_id(id).ok_or_else(|| usage(format!("--delete: not an annotation id: {id}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let input = |e: String| Failure {
        exit: Exit::Input,
        code: "input.annot",
        message: nupdf::diag::report::strip(&e).to_string(),
    };
    nupdf::write::check_document(doc);
    let count = doc.page_count().map_err(|e| input(e.0))?;
    let mut ed = Editor::new(doc);
    // The pages are loaded before any edit, in the order first named; a
    // search loads every page first. MuPDF updates its open pages newest
    // first, so the order is that of the new appearances' objects.
    let mut order: Vec<i32> = Vec::new();
    let highlights = if edits.highlight.is_empty() {
        Vec::new()
    } else {
        order.extend(0..count);
        for p in 0..count {
            ed.load_page(p).map_err(input)?;
        }
        search_hits(doc, count, edits.highlight).map_err(input)?
    };
    let named = deletes
        .iter()
        .map(|d| d.0)
        .chain(adds.iter().map(|a| a.page - 1))
        .chain(highlights.iter().map(|h| h.0));
    for p in named {
        if !order.contains(&p) {
            order.push(p);
        }
    }
    for &p in &order {
        if p >= count {
            return Err(input(format!(
                "no page {} (the document has {count})",
                p + 1
            )));
        }
        ed.load_page(p).map_err(input)?;
    }
    // Deletions, by the listing before any of them.
    let mut targets = Vec::new();
    for (&(p, a), id) in deletes.iter().zip(edits.delete) {
        let page = doc.page(p).map_err(|e| input(e.0))?;
        let annot = nupdf::annot::page_annots(doc, &page)
            .0
            .get(a)
            .cloned()
            .ok_or_else(|| input(format!("no annotation {id}")))?;
        targets.push((p, annot, id.clone()));
    }
    let (mut deleted, mut kept) = (Vec::new(), Vec::new());
    for (p, annot, id) in &targets {
        if ed.delete(*p, annot).map_err(input)? {
            deleted.push(id.clone());
        } else {
            // As MuPDF: see Editor::delete.
            doc.diagnostics().warn(
                "annot.not_deleted",
                format!("{id} is not deleted: MuPDF does not find a direct annotation without an appearance"),
            );
            kept.push(id.clone());
        }
    }
    let mut added = Vec::new();
    for a in &adds {
        let index = a.page - 1;
        let num = ed.create(index, &a.kind).map_err(input)?;
        apply(&mut ed, index, num, a).map_err(input)?;
        added.push((index, num, a.kind.clone()));
    }
    for (index, quads) in &highlights {
        let num = ed.create(*index, "Highlight").map_err(input)?;
        ed.set_quad_points(*index, num, quads).map_err(input)?;
        added.push((*index, num, "Highlight".to_string()));
    }
    let unmade = ed.update();
    let added: Vec<Value> = added
        .iter()
        .map(|(index, num, kind)| {
            let id = doc.page(*index).ok().and_then(|page| {
                nupdf::annot::page_annots(doc, &page)
                    .0
                    .iter()
                    .position(|o| matches!(o, nupdf::Object::Ref(r) if r.num == *num))
                    .map(|n| format!("p{}.a{}", index + 1, n + 1))
            });
            json!({ "id": id, "object": num, "page": index + 1, "type": kind })
        })
        .collect();
    let unmade: Vec<Value> = unmade
        .iter()
        .map(|(page, object, why)| json!({ "page": page + 1, "object": object, "error": why }))
        .collect();
    let body = json!({
        "added": added,
        "deleted": deleted,
        "not_deleted": kept,
        "unmade": unmade,
    });
    write_document(doc, facts, &plan, "nupdf.annots-edit/v1", body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_page_and_annotation_from_one() {
        assert_eq!(parse_id("p1.a2"), Some((0, 1)));
        assert_eq!(parse_id("p12.a1"), Some((11, 0)));
        assert_eq!(parse_id("p0.a1"), None);
        assert_eq!(parse_id("p1.w1"), None);
        assert_eq!(parse_id("1.a1"), None);
    }

    #[test]
    fn a_listed_annotation_can_be_added_as_it_is() {
        let listed = r#"{"page":1,"id":"p1.a1","object":7,"type":"Square","flags":4,
            "contents":"","color":[1,0,0],"opacity":1,"created":null,"modified":null,
            "rect":[10,20,30,40],"author":"me","subject":"","interior_color":[],
            "border":{"style":"Solid","width":2}}"#;
        let a = parse_add(listed).unwrap();
        assert_eq!(a.kind, "Square");
        assert_eq!(a.rect, Some(Rect::new(10.0, 20.0, 30.0, 40.0)));
        assert_eq!(a.border_width, Some(2.0));
        assert_eq!(a.flags, Some(4));
        assert_eq!(a.interior_color, Some(vec![]));
    }

    #[test]
    fn unknown_fields_and_missing_ones_are_refused() {
        assert!(parse_add(r#"{"page":1,"type":"Ink","colour":[1]}"#).is_err());
        assert!(parse_add(r#"{"type":"Ink"}"#).is_err());
        assert!(parse_add(r#"{"page":1}"#).is_err());
        assert!(parse_add(r#"{"page":1,"type":"Highlight","quad_points":[[1,2,3]]}"#).is_err());
    }
}
