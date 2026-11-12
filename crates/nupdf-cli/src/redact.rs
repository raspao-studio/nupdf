// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! The `redact` command: targets resolved to quads on each page, the pages
//! redacted, the document rewritten whole, and the output checked for
//! what should be gone before it is kept (AGENT-INTERFACE, Redaction
//! contract).

use nupdf::Document;
use nupdf::extract;
use nupdf::geometry::{Point, Quad, Rect};
use nupdf::redact::{ImageMethod, LineArt, RedactOptions, TextMethod};
use nupdf::scrub::Matcher;
use nupdf::search;
use nupdf::stext::{Block, Options, StextPage};
use nupdf::verify::{Class, Targets, Verification};
use serde_json::{Map, Value, json};

/// The presets, as regular expressions in MuPDF's syntax.
pub const PRESETS: &[(&str, &str)] = &[
    (
        "email",
        r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z][A-Za-z]+",
    ),
    (
        "phone",
        r"(\+[0-9]{1,3}[ .-]?)?(\([0-9]{2,4}\)[ .-]?|[0-9]{2,4}[ .-])[0-9]{3,4}[ .-]?[0-9]{3,4}",
    ),
    ("card", r"[0-9]{4}[ -]?[0-9]{4}[ -]?[0-9]{4}[ -]?[0-9]{1,7}"),
    ("ssn", r"[0-9]{3}[- ][0-9]{2}[- ][0-9]{4}"),
    (
        "iban",
        r"[A-Z][A-Z][0-9][0-9] ?[A-Z0-9]{4} ?[A-Z0-9]{4} ?[A-Z0-9]{4}( ?[A-Z0-9]{1,4}){0,4}",
    ),
];

/// One thing to redact, as given.
#[derive(Debug, Clone)]
pub enum Target {
    Text(String),
    Regex(String),
    Preset(String),
    Region {
        page: i32,
        rect: Rect,
    },
    Id(String),
    /// Quads by page (1-based), from a hits file.
    Hits(Vec<(i32, Quad)>),
}

impl Target {
    fn describe(&self) -> Value {
        match self {
            Target::Text(t) => json!({ "kind": "text", "value": t }),
            Target::Regex(r) => json!({ "kind": "regex", "value": r }),
            Target::Preset(p) => json!({ "kind": "preset", "value": p }),
            Target::Region { page, rect } => json!({
                "kind": "region",
                "value": format!("p{page}:[{},{},{},{}]", rect.x0, rect.y0, rect.x1, rect.y1),
            }),
            Target::Id(id) => json!({ "kind": "id", "value": id }),
            Target::Hits(h) => json!({ "kind": "hits", "value": h.len() }),
        }
    }

    /// The regular expressions or literal needles a target searches for,
    /// with MuPDF's search options.
    fn needles(&self, ignore_case: bool) -> Vec<(String, u32)> {
        let ic = if ignore_case { "ignore-case" } else { "" };
        let opts = |extra: &str| {
            let s = [ic, extra]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(",");
            search::parse_options(&s).unwrap_or(0)
        };
        match self {
            Target::Text(t) => vec![(t.clone(), opts(""))],
            Target::Regex(r) => vec![(r.clone(), opts("regexp"))],
            Target::Preset(p) => PRESETS
                .iter()
                .filter(|(name, _)| p == "pii" || p == name)
                .map(|(_, re)| ((*re).to_string(), opts("regexp")))
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Parses `p3:[x0,y0,x1,y1]`.
pub fn parse_region(s: &str) -> Result<Target, String> {
    let bad = || format!("bad region {s:?}: expected p<page>:[x0,y0,x1,y1]");
    let (page, rest) = s.split_once(':').ok_or_else(bad)?;
    let page: i32 = page
        .strip_prefix('p')
        .ok_or_else(bad)?
        .parse()
        .map_err(|_| bad())?;
    let inner = rest
        .trim()
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .ok_or_else(bad)?;
    let v: Vec<f32> = inner
        .split(',')
        .map(|x| x.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .map_err(|_| bad())?;
    let [x0, y0, x1, y1] = v[..] else {
        return Err(bad());
    };
    if page < 1 || !(x0 <= x1 && y0 <= y1) {
        return Err(bad());
    }
    Ok(Target::Region {
        page,
        rect: Rect::new(x0, y0, x1, y1),
    })
}

/// Reads a `search` report: its hits' quads, by page. The report must be
/// of this document.
pub fn parse_hits(text: &str, doc_id: &str) -> Result<Target, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("bad hits file: {e}"))?;
    match v.get("doc_id").and_then(Value::as_str) {
        Some(id) if id == doc_id => {}
        Some(id) => return Err(format!("hits are for {id}, not {doc_id}")),
        None => return Err("bad hits file: no doc_id".into()),
    }
    let mut out = Vec::new();
    for hit in v
        .get("hits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let page = hit.get("page").and_then(Value::as_i64).unwrap_or(0) as i32;
        for q in hit
            .get("quads")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let n: Vec<f32> = q
                .as_array()
                .into_iter()
                .flatten()
                .map(|x| x.as_f64().unwrap_or(f64::NAN) as f32)
                .collect();
            let [a, b, c, d, e, f, g, h] = n[..] else {
                return Err("bad hits file: a quad is not 8 numbers".into());
            };
            out.push((
                page,
                Quad {
                    ul: Point { x: a, y: b },
                    ur: Point { x: c, y: d },
                    ll: Point { x: e, y: f },
                    lr: Point { x: g, y: h },
                },
            ));
        }
    }
    Ok(Target::Hits(out))
}

fn quad_of(r: Rect) -> Quad {
    Quad {
        ul: Point { x: r.x0, y: r.y0 },
        ur: Point { x: r.x1, y: r.y0 },
        ll: Point { x: r.x0, y: r.y1 },
        lr: Point { x: r.x1, y: r.y1 },
    }
}

fn quad_json(q: &Quad) -> Value {
    let n = |v: f32| {
        if v.is_finite() {
            json!(f64::from(v))
        } else {
            Value::Null
        }
    };
    json!([
        n(q.ul.x),
        n(q.ul.y),
        n(q.ur.x),
        n(q.ur.y),
        n(q.ll.x),
        n(q.ll.y),
        n(q.lr.x),
        n(q.lr.y)
    ])
}

/// The characters a content id names: `pN`, `pN.bB`, `pN.bB.lL`,
/// `pN.bB.lL.sS` or `pN.bB.lL.sS:a-b`.
fn id_quads(page: &StextPage, n: i32, id: &str) -> Result<Vec<Quad>, String> {
    let bad = || format!("bad content id {id:?}");
    let (path, range) = match id.split_once(':') {
        Some((p, r)) => (p, Some(r)),
        None => (id, None),
    };
    let mut parts = path.split('.');
    let p: i32 = parts
        .next()
        .and_then(|s| s.strip_prefix('p'))
        .and_then(|s| s.parse().ok())
        .ok_or_else(bad)?;
    if p != n {
        return Err(bad());
    }
    let mut idx = |prefix: char| -> Result<Option<usize>, String> {
        match parts.next() {
            None => Ok(None),
            Some(s) => s
                .strip_prefix(prefix)
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&i| i >= 1)
                .map(|i| Some(i - 1))
                .ok_or_else(bad),
        }
    };
    let (b, l, s) = (idx('b')?, idx('l')?, idx('s')?);
    let missing = || format!("content id {id:?} names nothing on page {n}");
    let mut out = Vec::new();
    for (bi, block) in page.blocks.iter().enumerate() {
        if b.is_some_and(|b| b != bi) {
            continue;
        }
        let Block::Text(t) = block else { continue };
        for (li, line) in t.lines.iter().enumerate() {
            if l.is_some_and(|l| l != li) {
                continue;
            }
            for (si, span) in extract::spans(line).into_iter().enumerate() {
                if s.is_some_and(|s| s != si) {
                    continue;
                }
                let (from, to) = match range {
                    Some(r) => {
                        let (a, z) = r.split_once('-').ok_or_else(bad)?;
                        let a: usize = a.parse().map_err(|_| bad())?;
                        let z: usize = z.parse().map_err(|_| bad())?;
                        (span.start + a, (span.start + z).min(span.end))
                    }
                    None => (span.start, span.end),
                };
                out.extend(
                    line.chars
                        .get(from..to)
                        .into_iter()
                        .flatten()
                        .map(|c| c.quad),
                );
            }
        }
    }
    if out.is_empty() {
        return Err(missing());
    }
    Ok(out)
}

/// What `redact` was asked for.
pub struct Args<'a> {
    pub targets: &'a [Target],
    pub ignore_case: bool,
    pub stext: Options,
    pub options: RedactOptions,
    pub pages: Option<&'a str>,
}

/// The quads each target covers, by page index; and, per target, how many
/// hits it had.
pub struct Plan {
    pub quads: Vec<Vec<Quad>>,
    pub hits: Vec<usize>,
}

/// The pages' structured text, as MuPDF gathers it.
fn page_text(doc: &Document, index: i32, opts: Options) -> Result<StextPage, String> {
    let r = nupdf::parallel::sequential(doc, &[index], opts).next();
    r.unwrap_or_else(|| Err("no page".into()))
}

/// Hits of every text-like target on one page.
fn text_hits(
    page: &StextPage,
    targets: &[Target],
    ignore_case: bool,
) -> Result<Vec<Vec<Vec<Quad>>>, String> {
    targets
        .iter()
        .map(|t| {
            let mut out = Vec::new();
            for (needle, o) in t.needles(ignore_case) {
                for hit in search::search_page(page, &needle, o)? {
                    out.push(hit.quads);
                }
            }
            Ok(out)
        })
        .collect()
}

/// Resolves the targets on the pages chosen.
pub fn plan(doc: &Document, args: &Args) -> Result<Plan, String> {
    let count = doc.page_count().map_err(|e| e.0)?;
    let indices: Vec<i32> = match args.pages {
        Some(spec) => nupdf::rearrange::parse_page_ranges(spec, count, doc),
        None => (0..count).collect(),
    };
    let mut quads = vec![Vec::new(); count.max(0) as usize];
    let mut hits = vec![0usize; args.targets.len()];
    let needs_ids = args.targets.iter().any(|t| matches!(t, Target::Id(_)));
    for &i in &indices {
        let n = i + 1;
        let slot = &mut quads[i as usize];
        let page = page_text(doc, i, args.stext)?;
        let found = text_hits(&page, args.targets, args.ignore_case)?;
        let ids_page = if needs_ids {
            Some(page_text(doc, i, Options::default())?)
        } else {
            None
        };
        for (k, t) in args.targets.iter().enumerate() {
            match t {
                Target::Region { page, rect } if *page == n => {
                    slot.push(quad_of(*rect));
                    hits[k] += 1;
                }
                Target::Hits(h) => {
                    for (p, q) in h {
                        if *p == n {
                            slot.push(*q);
                            hits[k] += 1;
                        }
                    }
                }
                Target::Id(id) if id.split('.').next() == Some(&format!("p{n}")) => {
                    let page = ids_page.as_ref().ok_or("no page")?;
                    slot.extend(id_quads(page, n, id)?);
                    hits[k] += 1;
                }
                _ => {
                    for hit in &found[k] {
                        slot.extend(hit.iter().copied());
                        hits[k] += 1;
                    }
                }
            }
        }
    }
    for (k, t) in args.targets.iter().enumerate() {
        if let Target::Id(id) = t
            && hits[k] == 0
        {
            return Err(format!("content id {id:?} is not on the pages chosen"));
        }
    }
    Ok(Plan { quads, hits })
}

/// The text-like targets as patterns, for strings outside the page text.
pub fn matcher(targets: &[Target], ignore_case: bool) -> Result<Matcher, String> {
    let mut texts = Vec::new();
    let mut regexes = Vec::new();
    for t in targets {
        match t {
            Target::Text(s) => texts.push(s.clone()),
            Target::Regex(r) => regexes.push(r.clone()),
            Target::Preset(_) => regexes.extend(t.needles(false).into_iter().map(|(r, _)| r)),
            _ => {}
        }
    }
    Matcher::new(&texts, &regexes, ignore_case)
}

/// Applies the plan: each page with quads redacted, in PDF space, the
/// annotations over the areas removed, then the document scrubbed.
pub fn apply(
    doc: &Document,
    plan: &Plan,
    options: RedactOptions,
    m: &Matcher,
) -> Result<nupdf::scrub::Report, String> {
    let mut pages = Vec::new();
    for (i, quads) in plan.quads.iter().enumerate() {
        if quads.is_empty() {
            continue;
        }
        let i = i as i32;
        let page = doc.page(i).map_err(|e| e.0)?;
        let (_, ctm) = doc.page_transform(&page).map_err(|e| e.0)?;
        let pdf: Vec<Quad> = quads
            .iter()
            .map(|q| nupdf::redact::quad_to_pdf(*q, ctm))
            .collect();
        nupdf::redact::redact_page_with(doc, i, &pdf, options, Some(m))?;
        nupdf::redact::redact_all_annotations(doc, i, &pdf)?;
        pages.push(i);
    }
    Ok(nupdf::scrub::scrub(doc, m, &pages))
}

/// What `verify` looks for: the targets' needles and patterns, and the
/// areas the plan redacted.
pub fn verify_targets(
    targets: &[Target],
    ignore_case: bool,
    plan: Option<&Plan>,
) -> Result<Targets, String> {
    Ok(Targets {
        needles: targets
            .iter()
            .flat_map(|t| t.needles(ignore_case))
            .collect(),
        matcher: matcher(targets, ignore_case)?,
        areas: plan.map_or_else(Vec::new, |p| {
            p.quads
                .iter()
                .enumerate()
                .filter(|(_, q)| !q.is_empty())
                .map(|(i, q)| (i as i32, q.clone()))
                .collect()
        }),
    })
}

/// The verification as the report gives it: each class's result and
/// findings.
pub fn verification_json(v: &Verification) -> Value {
    let mut classes = Map::new();
    for class in Class::ALL {
        let findings: Vec<Value> = v
            .findings
            .iter()
            .filter(|f| f.class == class)
            .map(|f| json!({ "page": f.page, "object": f.object, "detail": f.detail }))
            .collect();
        classes.insert(
            class.name().into(),
            json!({ "ok": findings.is_empty(), "findings": findings }),
        );
    }
    json!({ "verified": v.verified(), "revisions": v.revisions, "classes": classes })
}

/// Reads a `redact` report given to `verify --against`: its targets and
/// the areas it redacted.
pub fn parse_against(text: &str) -> Result<(Vec<Target>, Plan), String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("bad report: {e}"))?;
    let mut targets = Vec::new();
    for t in v
        .get("targets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let value = t
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match t.get("kind").and_then(Value::as_str) {
            Some("text") => targets.push(Target::Text(value)),
            Some("regex") => targets.push(Target::Regex(value)),
            Some("preset") => targets.push(Target::Preset(value)),
            _ => {}
        }
    }
    let mut quads: Vec<Vec<Quad>> = Vec::new();
    for p in v
        .get("pages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let page = p.get("page").and_then(Value::as_i64).unwrap_or(0) as usize;
        if page == 0 {
            continue;
        }
        if quads.len() < page {
            quads.resize(page, Vec::new());
        }
        for q in p
            .get("quads")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let n: Vec<f32> = q
                .as_array()
                .into_iter()
                .flatten()
                .map(|x| x.as_f64().unwrap_or(f64::NAN) as f32)
                .collect();
            if let [a, b, c, d, e, f, g, h] = n[..] {
                quads[page - 1].push(Quad {
                    ul: Point { x: a, y: b },
                    ur: Point { x: c, y: d },
                    ll: Point { x: e, y: f },
                    lr: Point { x: g, y: h },
                });
            }
        }
    }
    let hits = vec![0; targets.len()];
    Ok((targets, Plan { quads, hits }))
}

/// The report's `targets` and `pages`.
pub fn describe(args: &Args, plan: &Plan, after: Option<&[usize]>) -> Map<String, Value> {
    let mut k_needle = 0usize;
    let mut body = Map::new();
    let targets: Vec<Value> = args
        .targets
        .iter()
        .enumerate()
        .map(|(k, t)| {
            let mut v = t.describe();
            if let Value::Object(m) = &mut v {
                m.insert("hits_before".into(), plan.hits[k].into());
                // A target's needles are counted together.
                let n = t.needles(args.ignore_case).len();
                let left: Option<usize> = after.map(|a| a.iter().skip(k_needle).take(n).sum());
                k_needle += n;
                m.insert(
                    "hits_after".into(),
                    match (left, n > 0) {
                        (Some(a), true) => a.into(),
                        _ => Value::Null,
                    },
                );
            }
            v
        })
        .collect();
    body.insert("targets".into(), targets.into());
    let pages: Vec<Value> = plan
        .quads
        .iter()
        .enumerate()
        .filter(|(_, q)| !q.is_empty())
        .map(
            |(i, q)| json!({ "page": i + 1, "quads": q.iter().map(quad_json).collect::<Vec<_>>() }),
        )
        .collect();
    body.insert("pages".into(), pages.into());
    let o = args.options;
    body.insert(
        "options".into(),
        json!({
            "boxes": o.black_boxes,
            "images": match o.image_method {
                ImageMethod::None => "none",
                ImageMethod::Remove => "remove",
                ImageMethod::Pixels => "pixels",
                ImageMethod::RemoveUnlessInvisible => "remove-unless-invisible",
            },
            "line_art": match o.line_art {
                LineArt::None => "none",
                LineArt::RemoveIfCovered => "covered",
                LineArt::RemoveIfTouched => "touched",
            },
            "text": match o.text {
                TextMethod::Remove => "remove",
                TextMethod::None => "none",
                TextMethod::RemoveInvisible => "invisible",
            },
        }),
    );
    body
}

/// Parses the image method's name.
pub fn image_method(s: &str) -> Result<ImageMethod, String> {
    Ok(match s {
        "none" => ImageMethod::None,
        "remove" => ImageMethod::Remove,
        "pixels" => ImageMethod::Pixels,
        "remove-unless-invisible" => ImageMethod::RemoveUnlessInvisible,
        _ => return Err(format!("unknown image method: {s}")),
    })
}

/// Parses the line-art policy's name.
pub fn line_art(s: &str) -> Result<LineArt, String> {
    Ok(match s {
        "none" => LineArt::None,
        "covered" => LineArt::RemoveIfCovered,
        "touched" => LineArt::RemoveIfTouched,
        _ => return Err(format!("unknown line art policy: {s}")),
    })
}
