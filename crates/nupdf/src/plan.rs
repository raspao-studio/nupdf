// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Composable, conflict-checked multi-agent edit plans (`nupdf apply`).
//!
//! Multiple agents (or one agent staging several heterogeneous edits) express
//! mutations against a single base `doc_id`: surgical text patches, highlights,
//! annotation deletions, form fills, redactions, metadata updates, sanitization
//! and page selection/rotation. Every content ID (`p2.b1.l3.s1`) and page index
//! is resolved against the immutable base [`Document`] *before* any mutation
//! runs, semantic/spatial conflicts across operations or agents are caught up
//! front, and the merged transaction is executed on an `O(1)` [`Document::fork_clean`]
//! overlay branch and verified before a single byte touches disk.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::annot;
use crate::annot_edit::Editor;
use crate::document::Document;
use crate::edit;
use crate::extract;
use crate::fill;
use crate::geometry::{Point, Quad, Rect};
use crate::patch::{self, AppliedPatch, PatchOptions, PatchSpec, PatchVerification};
use crate::rearrange;
use crate::redact::{self, ImageMethod, LineArt, RedactOptions, TextMethod};
use crate::sanitize;
use crate::scrub::{self, Matcher};
use crate::search;
use crate::stext::{self, Block, StextPage};
use crate::verify::{self, Targets, Verification};
use crate::write::{self, WriteOptions};

/// Maximum operations across combined plans in a single transaction.
const MAX_PLAN_OPS: usize = 10_000;

/// A declarative edit plan produced by an agent or tool session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EditPlan {
    /// Optional agent or branch name for conflict attribution.
    #[serde(default)]
    pub agent: Option<String>,
    /// Optional expected `b3:...` document id; rejected if the base file changed.
    #[serde(default)]
    pub doc_id: Option<String>,
    /// Ordered operations staged against the base document.
    pub ops: Vec<PlanOp>,
}

/// One staged operation anchored to the base document's coordinates and IDs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PlanOp {
    /// Surgically replace text at a span or line content ID (`p1.b2.l1.s1`).
    Patch { id: String, text: String },
    /// Highlight a content ID (`id`) or all hits of `text` on optional `pages`.
    Highlight {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        pages: Option<String>,
        #[serde(default)]
        contents: Option<String>,
    },
    /// Delete an annotation by ID (`p1.a2`).
    AnnotDelete { id: String },
    /// Set an AcroForm field value (`name=value`).
    Fill { field: String, value: String },
    /// Toggle a checkbox or button widget by field name.
    Toggle { field: String },
    /// Permanently redact text, regex, content ID, or region (`p1:[x0,y0,x1,y1]`).
    Redact {
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        regex: Option<String>,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        boxes: bool,
    },
    /// Update or strip document metadata (`/Info` and XMP).
    Meta {
        #[serde(default)]
        set: BTreeMap<String, String>,
        #[serde(default)]
        strip: bool,
    },
    /// Remove active content (JavaScript, Launch, SubmitForm, Remote GoTo, XFA).
    Sanitize {
        #[serde(default)]
        attachments: bool,
    },
    /// Select, delete, or rotate pages (applied last, using base 1-based page numbers).
    Pages {
        #[serde(default)]
        select: Option<String>,
        #[serde(default)]
        delete: Option<String>,
        #[serde(default)]
        rotate: Option<i32>,
        #[serde(default)]
        rotate_pages: Option<String>,
    },
}

/// A semantic or spatial conflict detected between two staged operations.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlanConflict {
    pub code: String,
    pub op_a: usize,
    pub agent_a: Option<String>,
    pub op_b: usize,
    pub agent_b: Option<String>,
    pub target: String,
    pub message: String,
}

/// Summary of an executed (or dry-run) transactional edit plan.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlanExecution {
    pub plan_count: usize,
    pub op_count: usize,
    pub conflicts: Vec<PlanConflict>,
    pub patches: Vec<AppliedPatch>,
    pub highlights_added: usize,
    pub annotations_deleted: usize,
    pub fields_changed: usize,
    pub redaction_quads: usize,
    pub sanitized_items: usize,
    pub pages_kept: Option<Vec<i32>>,
    pub rotated: Option<i32>,
    pub verified: bool,
    pub patch_verification: Option<PatchVerification>,
    pub redact_verified: Option<bool>,
}

/// Why parsing, resolving, or executing a plan failed before verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    pub code: &'static str,
    pub is_conflict_or_verify: bool,
    pub message: String,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PlanError {}

#[derive(Debug, Clone)]
struct ResolvedSpan {
    page_idx: i32,
    bbox: Rect,
    quads: Vec<Quad>,
}

#[derive(Debug, Clone)]
struct IndexedOp {
    global_idx: usize,
    agent: Option<String>,
    op: PlanOp,
}

/// Parses a JSON edit plan from text (either a full `EditPlan` object or a bare array of `PlanOp`s).
pub fn parse_plan(json_text: &str) -> Result<EditPlan, PlanError> {
    if let Ok(plan) = serde_json::from_str::<EditPlan>(json_text) {
        return Ok(plan);
    }
    if let Ok(ops) = serde_json::from_str::<Vec<PlanOp>>(json_text) {
        return Ok(EditPlan {
            agent: None,
            doc_id: None,
            ops,
        });
    }
    let err = serde_json::from_str::<EditPlan>(json_text)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_else(|| "invalid edit plan JSON".into());
    Err(PlanError {
        code: "usage.bad_plan",
        is_conflict_or_verify: false,
        message: format!("invalid plan JSON: {err}"),
    })
}

fn parse_region_str(s: &str) -> Result<(i32, Rect), PlanError> {
    let bad = || PlanError {
        code: "usage.bad_region",
        is_conflict_or_verify: false,
        message: format!("bad region {s:?}: expected p<page>:[x0,y0,x1,y1]"),
    };
    let (page_part, rest) = s.split_once(':').ok_or_else(bad)?;
    let page: i32 = page_part
        .trim()
        .strip_prefix('p')
        .ok_or_else(bad)?
        .parse()
        .map_err(|_| bad())?;
    let inner = rest
        .trim()
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .ok_or_else(bad)?;
    let coords: Vec<f32> = inner
        .split(',')
        .map(|v| v.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .map_err(|_| bad())?;
    let [x0, y0, x1, y1] = coords[..] else {
        return Err(bad());
    };
    if page < 1 || !(x0 <= x1 && y0 <= y1) || coords.iter().any(|x| !x.is_finite()) {
        return Err(bad());
    }
    Ok((page - 1, Rect::new(x0, y0, x1, y1)))
}

fn rect_to_quad(r: Rect) -> Quad {
    Quad {
        ul: Point::new(r.x0, r.y0),
        ur: Point::new(r.x1, r.y0),
        ll: Point::new(r.x0, r.y1),
        lr: Point::new(r.x1, r.y1),
    }
}

fn page_stext(
    doc: &Document,
    page_obj: &crate::object::Object,
    opts: stext::Options,
) -> Result<StextPage, PlanError> {
    stext::extract_page(doc, page_obj, opts, crate::content::Store::default())
        .0
        .map_err(|e| PlanError {
            code: "input.page_tree",
            is_conflict_or_verify: false,
            message: e,
        })
}

fn resolve_content_id(
    id: &str,
    pages_cache: &mut HashMap<i32, StextPage>,
    doc: &Document,
    page_count: i32,
    stext_opts: stext::Options,
) -> Result<ResolvedSpan, PlanError> {
    let bad = || PlanError {
        code: "usage.bad_id",
        is_conflict_or_verify: false,
        message: format!("bad content id {id:?}"),
    };
    let (base_id, char_range) = match id.split_once(':') {
        Some((b, r)) => {
            let (s, e) = r.split_once('-').ok_or_else(bad)?;
            let start: usize = s.parse().map_err(|_| bad())?;
            let end: usize = e.parse().map_err(|_| bad())?;
            if start >= end {
                return Err(bad());
            }
            (b, Some((start, end)))
        }
        None => (id, None),
    };
    let parts: Vec<&str> = base_id.split('.').collect();
    let Some(p_str) = parts.first().and_then(|p| p.strip_prefix('p')) else {
        return Err(bad());
    };
    let page_num: i32 = p_str.parse().map_err(|_| bad())?;
    if page_num < 1 || page_num > page_count {
        return Err(PlanError {
            code: "input.target_not_found",
            is_conflict_or_verify: false,
            message: format!("page {page_num} out of range 1..={page_count} in {id}"),
        });
    }
    let page_idx = page_num - 1;
    if let std::collections::hash_map::Entry::Vacant(e) = pages_cache.entry(page_idx) {
        let page_obj = doc.page(page_idx).map_err(|e| PlanError {
            code: "input.page_tree",
            is_conflict_or_verify: false,
            message: e.0,
        })?;
        let sp = page_stext(doc, &page_obj, stext_opts)?;
        e.insert(sp);
    }
    let Some(sp) = pages_cache.get(&page_idx) else {
        return Err(bad());
    };
    if parts.len() == 1 {
        let r = sp.mediabox;
        return Ok(ResolvedSpan {
            page_idx,
            bbox: r,
            quads: vec![rect_to_quad(r)],
        });
    }
    let b_num: usize = parts
        .get(1)
        .and_then(|s| s.strip_prefix('b'))
        .ok_or_else(bad)?
        .parse()
        .map_err(|_| bad())?;
    let refs: Vec<&Block> = sp.blocks.iter().collect();
    let text_blocks = extract::text_blocks(&refs);
    let Some(tb) = b_num.checked_sub(1).and_then(|i| text_blocks.get(i)) else {
        return Err(PlanError {
            code: "input.target_not_found",
            is_conflict_or_verify: false,
            message: format!("block not found for {id}"),
        });
    };
    if parts.len() == 2 {
        let quads: Vec<Quad> = tb
            .lines
            .iter()
            .flat_map(|l| l.chars.iter().map(|c| c.quad))
            .collect();
        return Ok(ResolvedSpan {
            page_idx,
            bbox: tb.bbox,
            quads,
        });
    }
    let l_num: usize = parts
        .get(2)
        .and_then(|s| s.strip_prefix('l'))
        .ok_or_else(bad)?
        .parse()
        .map_err(|_| bad())?;
    let Some(line) = l_num.checked_sub(1).and_then(|i| tb.lines.get(i)) else {
        return Err(PlanError {
            code: "input.target_not_found",
            is_conflict_or_verify: false,
            message: format!("line not found for {id}"),
        });
    };
    if parts.len() == 3 {
        let quads: Vec<Quad> = line.chars.iter().map(|c| c.quad).collect();
        return Ok(ResolvedSpan {
            page_idx,
            bbox: line.bbox,
            quads,
        });
    }
    if parts.len() != 4 {
        return Err(bad());
    }
    let s_num: usize = parts
        .get(3)
        .and_then(|s| s.strip_prefix('s'))
        .ok_or_else(bad)?
        .parse()
        .map_err(|_| bad())?;
    let line_spans = extract::spans(line);
    let Some(span) = s_num.checked_sub(1).and_then(|i| line_spans.get(i)) else {
        return Err(PlanError {
            code: "input.target_not_found",
            is_conflict_or_verify: false,
            message: format!("span not found for {id}"),
        });
    };
    let slice = &line.chars[span.start..span.end];
    let selected = match char_range {
        Some((cs, ce)) => {
            if ce > slice.len() {
                return Err(PlanError {
                    code: "input.target_not_found",
                    is_conflict_or_verify: false,
                    message: format!("char range {cs}-{ce} exceeds span length {}", slice.len()),
                });
            }
            &slice[cs..ce]
        }
        None => slice,
    };
    let mut bbox = Rect::EMPTY;
    let mut quads = Vec::with_capacity(selected.len());
    for ch in selected {
        bbox = bbox.union(Rect::from_quad(ch.quad));
        quads.push(ch.quad);
    }
    Ok(ResolvedSpan {
        page_idx,
        bbox,
        quads,
    })
}

fn boxes_overlap(a: Rect, b: Rect) -> bool {
    let inter = a.intersect(b);
    !inter.is_empty() && (inter.x1 - inter.x0) > 0.5 && (inter.y1 - inter.y0) > 0.5
}

/// Executes one or more [`EditPlan`]s atomically on a forked overlay of `base_doc`,
/// checking `doc_id` freshness, cross-agent conflicts, and post-write verification.
pub fn execute_plans(
    base_doc: &Document,
    base_doc_id: &str,
    plans: &[EditPlan],
    write_opts: &WriteOptions,
    patch_opts: &PatchOptions,
    password: Option<&str>,
    verify_output: bool,
) -> Result<(Vec<u8>, PlanExecution), PlanError> {
    for p in plans {
        if let Some(expected) = &p.doc_id
            && expected != base_doc_id
        {
            return Err(PlanError {
                code: "plan.stale_doc_id",
                is_conflict_or_verify: true,
                message: format!(
                    "plan{} expects doc_id {expected}, but document is {base_doc_id}",
                    p.agent
                        .as_deref()
                        .map(|a| format!(" from {a}"))
                        .unwrap_or_default()
                ),
            });
        }
    }

    let mut indexed = Vec::new();
    for p in plans {
        for op in &p.ops {
            if indexed.len() >= MAX_PLAN_OPS {
                return Err(PlanError {
                    code: "limit.plan_ops",
                    is_conflict_or_verify: false,
                    message: format!("plan exceeds maximum of {MAX_PLAN_OPS} operations"),
                });
            }
            indexed.push(IndexedOp {
                global_idx: indexed.len(),
                agent: p.agent.clone(),
                op: op.clone(),
            });
        }
    }

    let page_count = base_doc.page_count().map_err(|e| PlanError {
        code: "input.page_tree",
        is_conflict_or_verify: false,
        message: e.0,
    })?;

    let mut stext_cache: HashMap<i32, StextPage> = HashMap::new();
    let mut redact_stext_cache: HashMap<i32, StextPage> = HashMap::new();
    let redact_stext_opts = stext::Options {
        flags: stext::flags::ALL_LAYERS,
        clip: Rect::INFINITE,
    };

    type ResolvedHighlight = (
        usize,
        Option<String>,
        i32,
        Rect,
        Vec<[Point; 4]>,
        Option<String>,
    );
    type ResolvedRedaction = (usize, Option<String>, i32, Rect, Vec<Quad>, bool);

    // Pre-resolve spatial footprints & track conflicts before mutating anything.
    let mut patches_resolved: Vec<(usize, Option<String>, PatchSpec, i32, Rect)> = Vec::new();
    let mut highlights_resolved: Vec<ResolvedHighlight> = Vec::new();
    let mut redactions_resolved: Vec<ResolvedRedaction> = Vec::new();
    let mut redact_texts: Vec<String> = Vec::new();
    let mut redact_regexes: Vec<String> = Vec::new();
    let mut redact_needles: Vec<(String, u32)> = Vec::new();
    let mut annot_deletes: Vec<(usize, Option<String>, i32, String, crate::object::Object)> =
        Vec::new();
    let mut fills: Vec<(usize, Option<String>, fill::Edit)> = Vec::new();
    let mut meta_sets: Vec<(usize, Option<String>, String, String)> = Vec::new();
    let mut meta_strip = false;
    let mut sanitize_policies: Vec<bool> = Vec::new();
    let mut page_kept: Option<(usize, Option<String>, Vec<i32>)> = None;
    let mut page_rotations: Vec<(i32, Vec<i32>)> = Vec::new();
    let mut conflicts: Vec<PlanConflict> = Vec::new();

    for item in &indexed {
        match &item.op {
            PlanOp::Patch { id, text } => {
                let resolved = resolve_content_id(
                    id,
                    &mut stext_cache,
                    base_doc,
                    page_count,
                    stext::Options::default(),
                )?;
                patches_resolved.push((
                    item.global_idx,
                    item.agent.clone(),
                    PatchSpec {
                        target_id: id.clone(),
                        new_text: text.clone(),
                    },
                    resolved.page_idx,
                    resolved.bbox,
                ));
            }
            PlanOp::Highlight {
                id,
                text,
                pages,
                contents,
            } => {
                if let Some(cid) = id {
                    let resolved = resolve_content_id(
                        cid,
                        &mut stext_cache,
                        base_doc,
                        page_count,
                        stext::Options::default(),
                    )?;
                    let pts: Vec<[Point; 4]> = resolved
                        .quads
                        .iter()
                        .map(|q| [q.ul, q.ur, q.ll, q.lr])
                        .collect();
                    highlights_resolved.push((
                        item.global_idx,
                        item.agent.clone(),
                        resolved.page_idx,
                        resolved.bbox,
                        pts,
                        contents.clone(),
                    ));
                } else if let Some(needle) = text {
                    let target_pages = match pages {
                        Some(spec) => rearrange::parse_page_ranges(spec, page_count, base_doc),
                        None => (0..page_count).collect(),
                    };
                    for p_idx in target_pages {
                        let page_obj = base_doc.page(p_idx).map_err(|e| PlanError {
                            code: "input.page_tree",
                            is_conflict_or_verify: false,
                            message: e.0,
                        })?;
                        let sp = page_stext(base_doc, &page_obj, stext::Options::default())?;
                        let hits = search::search_page(&sp, needle, 0).map_err(|e| PlanError {
                            code: "usage.bad_pattern",
                            is_conflict_or_verify: false,
                            message: e,
                        })?;
                        for h in hits {
                            let mut bbox = Rect::EMPTY;
                            let mut pts = Vec::new();
                            for q in &h.quads {
                                bbox = bbox.union(Rect::from_quad(*q));
                                pts.push([q.ul, q.ur, q.ll, q.lr]);
                            }
                            highlights_resolved.push((
                                item.global_idx,
                                item.agent.clone(),
                                p_idx,
                                bbox,
                                pts,
                                contents.clone(),
                            ));
                        }
                    }
                } else {
                    return Err(PlanError {
                        code: "usage.bad_plan",
                        is_conflict_or_verify: false,
                        message: "highlight requires either `id` or `text`".into(),
                    });
                }
            }
            PlanOp::AnnotDelete { id } => {
                let bad = || PlanError {
                    code: "usage.bad_id",
                    is_conflict_or_verify: false,
                    message: format!("bad annotation id {id:?}: expected p<page>.a<num>"),
                };
                let (p_s, a_s) = id.split_once('.').ok_or_else(bad)?;
                let p_num: i32 = p_s
                    .strip_prefix('p')
                    .ok_or_else(bad)?
                    .parse()
                    .map_err(|_| bad())?;
                let a_num: usize = a_s
                    .strip_prefix('a')
                    .ok_or_else(bad)?
                    .parse()
                    .map_err(|_| bad())?;
                if p_num < 1 || p_num > page_count {
                    return Err(bad());
                }
                // By the base's listing, as `annots --delete` resolves its
                // ids: an earlier delete must not renumber a later one.
                let page_obj = base_doc.page(p_num - 1).map_err(|e| PlanError {
                    code: "input.page_tree",
                    is_conflict_or_verify: false,
                    message: e.0,
                })?;
                let annot = a_num
                    .checked_sub(1)
                    .and_then(|i| annot::page_annots(base_doc, &page_obj).0.get(i).cloned())
                    .ok_or_else(|| PlanError {
                        code: "input.target_not_found",
                        is_conflict_or_verify: false,
                        message: format!("no annotation {id}"),
                    })?;
                annot_deletes.push((
                    item.global_idx,
                    item.agent.clone(),
                    p_num - 1,
                    id.clone(),
                    annot,
                ));
            }
            PlanOp::Fill { field, value } => {
                fills.push((
                    item.global_idx,
                    item.agent.clone(),
                    fill::Edit::Set {
                        name: field.clone(),
                        value: value.clone(),
                    },
                ));
            }
            PlanOp::Toggle { field } => {
                fills.push((
                    item.global_idx,
                    item.agent.clone(),
                    fill::Edit::Toggle {
                        name: field.clone(),
                    },
                ));
            }
            PlanOp::Redact {
                text,
                regex,
                id,
                region,
                boxes,
            } => {
                if let Some(cid) = id {
                    let resolved = resolve_content_id(
                        cid,
                        &mut redact_stext_cache,
                        base_doc,
                        page_count,
                        redact_stext_opts,
                    )?;
                    redactions_resolved.push((
                        item.global_idx,
                        item.agent.clone(),
                        resolved.page_idx,
                        resolved.bbox,
                        resolved.quads,
                        *boxes,
                    ));
                }
                if let Some(reg_s) = region {
                    let (p_idx, rect) = parse_region_str(reg_s)?;
                    if p_idx >= page_count {
                        return Err(PlanError {
                            code: "input.target_not_found",
                            is_conflict_or_verify: false,
                            message: format!("region page {} out of range", p_idx + 1),
                        });
                    }
                    redactions_resolved.push((
                        item.global_idx,
                        item.agent.clone(),
                        p_idx,
                        rect,
                        vec![rect_to_quad(rect)],
                        *boxes,
                    ));
                }
                let search_pair = match (text, regex) {
                    (Some(_), Some(_)) => {
                        return Err(PlanError {
                            code: "usage.bad_plan",
                            is_conflict_or_verify: false,
                            message:
                                "redact takes `text` or `regex`, not both: stage two redact ops"
                                    .into(),
                        });
                    }
                    (Some(t), None) => {
                        redact_texts.push(t.clone());
                        Some((t.clone(), 0u32))
                    }
                    (None, Some(r)) => {
                        redact_regexes.push(r.clone());
                        let flags = search::parse_options("regexp").unwrap_or(0);
                        Some((r.clone(), flags))
                    }
                    (None, None) => None,
                };
                if let Some((needle, sflags)) = search_pair {
                    redact_needles.push((needle.clone(), sflags));
                    for p_idx in 0..page_count {
                        if let std::collections::hash_map::Entry::Vacant(e) =
                            redact_stext_cache.entry(p_idx)
                        {
                            let page_obj = base_doc.page(p_idx).map_err(|e| PlanError {
                                code: "input.page_tree",
                                is_conflict_or_verify: false,
                                message: e.0,
                            })?;
                            let sp = page_stext(base_doc, &page_obj, redact_stext_opts)?;
                            e.insert(sp);
                        }
                        if let Some(sp) = redact_stext_cache.get(&p_idx) {
                            let hits = search::search_page(sp, &needle, sflags).map_err(|e| {
                                PlanError {
                                    code: "usage.bad_pattern",
                                    is_conflict_or_verify: false,
                                    message: e,
                                }
                            })?;
                            for h in hits {
                                let mut bbox = Rect::EMPTY;
                                for q in &h.quads {
                                    bbox = bbox.union(Rect::from_quad(*q));
                                }
                                redactions_resolved.push((
                                    item.global_idx,
                                    item.agent.clone(),
                                    p_idx,
                                    bbox,
                                    h.quads,
                                    *boxes,
                                ));
                            }
                        }
                    }
                }
                if id.is_none() && region.is_none() && text.is_none() && regex.is_none() {
                    return Err(PlanError {
                        code: "usage.bad_plan",
                        is_conflict_or_verify: false,
                        message: "redact requires one of `text`, `regex`, `id`, or `region`".into(),
                    });
                }
            }
            PlanOp::Meta { set, strip } => {
                if *strip {
                    meta_strip = true;
                }
                for (k, v) in set {
                    meta_sets.push((item.global_idx, item.agent.clone(), k.clone(), v.clone()));
                }
            }
            PlanOp::Sanitize { attachments } => {
                sanitize_policies.push(*attachments);
            }
            PlanOp::Pages {
                select,
                delete,
                rotate,
                rotate_pages,
            } => {
                if let Some(deg) = rotate
                    && deg % 90 != 0
                {
                    return Err(PlanError {
                        code: "usage.bad_value",
                        is_conflict_or_verify: false,
                        message: format!("rotate must be a multiple of 90, got {deg}"),
                    });
                }
                let kept_now = match (select, delete) {
                    (Some(spec), _) => {
                        Some(rearrange::parse_page_ranges(spec, page_count, base_doc))
                    }
                    (None, Some(spec)) => {
                        let gone = rearrange::parse_page_ranges(spec, page_count, base_doc);
                        Some((0..page_count).filter(|p| !gone.contains(p)).collect())
                    }
                    (None, None) => None,
                };
                if let Some(k) = kept_now {
                    if let Some((prev_idx, prev_agent, prev_k)) = &page_kept
                        && *prev_k != k
                    {
                        conflicts.push(PlanConflict {
                            code: "conflict.page_selection".into(),
                            op_a: *prev_idx,
                            agent_a: prev_agent.clone(),
                            op_b: item.global_idx,
                            agent_b: item.agent.clone(),
                            target: "pages".into(),
                            message: "conflicting page selection/deletion lists across operations"
                                .into(),
                        });
                    }
                    page_kept = Some((item.global_idx, item.agent.clone(), k));
                }
                if let Some(deg) = rotate {
                    let targets = match rotate_pages {
                        Some(spec) => rearrange::parse_page_ranges(spec, page_count, base_doc),
                        None => (0..page_count).collect(),
                    };
                    page_rotations.push((*deg, targets));
                }
            }
        }
    }

    // Check pairwise spatial & semantic conflicts.
    for i in 0..patches_resolved.len() {
        let (idx_a, ref ag_a, ref spec_a, p_a, bbox_a) = patches_resolved[i];
        for (idx_b, ag_b, spec_b, p_b, bbox_b) in patches_resolved.iter().skip(i + 1) {
            if p_a == *p_b
                && (spec_a.target_id == spec_b.target_id || boxes_overlap(bbox_a, *bbox_b))
                && spec_a.new_text != spec_b.new_text
            {
                conflicts.push(PlanConflict {
                    code: "conflict.overlapping_patch".into(),
                    op_a: idx_a,
                    agent_a: ag_a.clone(),
                    op_b: *idx_b,
                    agent_b: ag_b.clone(),
                    target: spec_a.target_id.clone(),
                    message: format!(
                        "conflicting patches on page {} ({:?} vs {:?})",
                        p_a + 1,
                        spec_a.new_text,
                        spec_b.new_text
                    ),
                });
            }
        }
        for (idx_r, ag_r, p_r, bbox_r, _, _) in &redactions_resolved {
            if p_a == *p_r && boxes_overlap(bbox_a, *bbox_r) {
                conflicts.push(PlanConflict {
                    code: "conflict.patch_vs_redact".into(),
                    op_a: idx_a,
                    agent_a: ag_a.clone(),
                    op_b: *idx_r,
                    agent_b: ag_r.clone(),
                    target: spec_a.target_id.clone(),
                    message: format!(
                        "patch on {} overlaps a redaction region on page {}",
                        spec_a.target_id,
                        p_a + 1
                    ),
                });
            }
        }
    }

    for (idx_h, ag_h, p_h, bbox_h, _, _) in &highlights_resolved {
        for (idx_r, ag_r, p_r, bbox_r, _, _) in &redactions_resolved {
            if p_h == p_r && boxes_overlap(*bbox_h, *bbox_r) {
                conflicts.push(PlanConflict {
                    code: "conflict.highlight_vs_redact".into(),
                    op_a: *idx_h,
                    agent_a: ag_h.clone(),
                    op_b: *idx_r,
                    agent_b: ag_r.clone(),
                    target: format!("p{}", p_h + 1),
                    message: format!("highlight on page {} overlaps a redaction target", p_h + 1),
                });
            }
        }
    }

    for i in 0..fills.len() {
        let (idx_a, ref ag_a, ref ed_a) = fills[i];
        for (idx_b, ag_b, ed_b) in fills.iter().skip(i + 1) {
            if ed_a.name() == ed_b.name() && ed_a != ed_b {
                conflicts.push(PlanConflict {
                    code: "conflict.field_value".into(),
                    op_a: idx_a,
                    agent_a: ag_a.clone(),
                    op_b: *idx_b,
                    agent_b: ag_b.clone(),
                    target: ed_a.name().to_string(),
                    message: format!("conflicting form edits on field {:?}", ed_a.name()),
                });
            }
        }
    }

    for i in 0..meta_sets.len() {
        let (idx_a, ref ag_a, ref k_a, ref v_a) = meta_sets[i];
        for (idx_b, ag_b, k_b, v_b) in meta_sets.iter().skip(i + 1) {
            if k_a == k_b && v_a != v_b {
                conflicts.push(PlanConflict {
                    code: "conflict.metadata_key".into(),
                    op_a: idx_a,
                    agent_a: ag_a.clone(),
                    op_b: *idx_b,
                    agent_b: ag_b.clone(),
                    target: k_a.clone(),
                    message: format!("conflicting metadata values for {k_a:?}"),
                });
            }
        }
    }

    if let Some((idx_p, ref ag_p, ref kept)) = page_kept {
        for (idx_op, ag_op, p_op, label) in patches_resolved
            .iter()
            .map(|(i, a, s, p, _)| (*i, a.clone(), *p, s.target_id.clone()))
            .chain(
                highlights_resolved
                    .iter()
                    .map(|(i, a, p, _, _, _)| (*i, a.clone(), *p, format!("p{}", p + 1))),
            )
        {
            if !kept.contains(&p_op) {
                conflicts.push(PlanConflict {
                    code: "conflict.page_deleted".into(),
                    op_a: idx_op,
                    agent_a: ag_op,
                    op_b: idx_p,
                    agent_b: ag_p.clone(),
                    target: label,
                    message: format!(
                        "operation targets page {}, which another operation drops",
                        p_op + 1
                    ),
                });
            }
        }
    }

    if !conflicts.is_empty() {
        return Ok((
            Vec::new(),
            PlanExecution {
                plan_count: plans.len(),
                op_count: indexed.len(),
                conflicts,
                patches: Vec::new(),
                highlights_added: 0,
                annotations_deleted: 0,
                fields_changed: 0,
                redaction_quads: 0,
                sanitized_items: 0,
                pages_kept: None,
                rotated: None,
                verified: false,
                patch_verification: None,
                redact_verified: None,
            },
        ));
    }

    // Fork an isolated O(1) overlay branch sharing the base document's parsed State.
    let work = base_doc.fork_clean();
    write::check_document(&work);

    // 1. Form fills
    let mut fields_changed = 0usize;
    if !fills.is_empty() {
        let fill_edits: Vec<fill::Edit> = fills.iter().map(|(_, _, e)| e.clone()).collect();
        let filled = fill::fill(&work, &fill_edits).map_err(|e| PlanError {
            code: "input.forms",
            is_conflict_or_verify: false,
            message: format!("{e:?}"),
        })?;
        fields_changed = filled.changes.iter().filter(|c| c.changed).count();
    }

    // 2. Annotations (deletes + highlights)
    let mut annotations_deleted = 0usize;
    let mut highlights_added = 0usize;
    if !annot_deletes.is_empty() || !highlights_resolved.is_empty() {
        let mut editor = Editor::new(&work);
        for (_, _, p_idx, id_str, target_annot) in &annot_deletes {
            editor.load_page(*p_idx).map_err(|e| PlanError {
                code: "input.annots",
                is_conflict_or_verify: false,
                message: e,
            })?;
            if editor.delete(*p_idx, target_annot).map_err(|e| PlanError {
                code: "input.annots",
                is_conflict_or_verify: false,
                message: e,
            })? {
                annotations_deleted += 1;
            } else {
                // As `annots --delete`: see Editor::delete.
                work.diagnostics().warn(
                    "annot.not_deleted",
                    format!(
                        "{id_str} is not deleted: MuPDF does not find a direct annotation without an appearance"
                    ),
                );
            }
        }
        for (_, _, p_idx, _, pts, note) in &highlights_resolved {
            if pts.is_empty() {
                continue;
            }
            editor.load_page(*p_idx).map_err(|e| PlanError {
                code: "input.annots",
                is_conflict_or_verify: false,
                message: e,
            })?;
            let num = editor.create(*p_idx, "Highlight").map_err(|e| PlanError {
                code: "input.annots",
                is_conflict_or_verify: false,
                message: e,
            })?;
            editor
                .set_quad_points(*p_idx, num, pts)
                .map_err(|e| PlanError {
                    code: "input.annots",
                    is_conflict_or_verify: false,
                    message: e,
                })?;
            if let Some(c) = note {
                editor.set_contents(num, c);
            }
            highlights_added += 1;
        }
        editor.update();
    }

    // 3. Redactions (before patches so redaction's sanitize pass doesn't disturb new patch layers,
    // and we already proved zero overlap between patches and redactions!)
    let mut redaction_quads = 0usize;
    let mut redaction_pages: BTreeMap<i32, (Vec<Quad>, bool)> = BTreeMap::new();
    for (_, _, p_idx, _, quads, boxes) in &redactions_resolved {
        let entry = redaction_pages
            .entry(*p_idx)
            .or_insert_with(|| (Vec::new(), false));
        entry.0.extend(quads.iter().copied());
        if *boxes {
            entry.1 = true;
        }
        redaction_quads += quads.len();
    }
    let matcher = Matcher::new(&redact_texts, &redact_regexes, false).ok();
    for (&p_idx, (quads, boxes)) in &redaction_pages {
        let page_obj = work.page(p_idx).map_err(|e| PlanError {
            code: "input.page_tree",
            is_conflict_or_verify: false,
            message: e.0,
        })?;
        let (_, page_ctm) = work.page_transform(&page_obj).map_err(|e| PlanError {
            code: "input.page_tree",
            is_conflict_or_verify: false,
            message: e.0,
        })?;
        let pdf_quads: Vec<Quad> = quads
            .iter()
            .map(|&q| redact::quad_to_pdf(q, page_ctm))
            .collect();
        let ropts = RedactOptions {
            black_boxes: *boxes,
            image_method: ImageMethod::Pixels,
            line_art: LineArt::RemoveIfCovered,
            text: TextMethod::Remove,
        };
        let _ = redact::redact_all_annotations(&work, p_idx, &pdf_quads);
        redact::redact_page_with(&work, p_idx, &pdf_quads, ropts, matcher.as_ref()).map_err(
            |e| PlanError {
                code: "input.redact",
                is_conflict_or_verify: false,
                message: e,
            },
        )?;
    }
    if let Some(m) = &matcher
        && !redact_needles.is_empty()
    {
        let all_pages: Vec<i32> = (0..page_count).collect();
        let _ = scrub::scrub(&work, m, &all_pages);
    }

    // 4. Surgical text patches (snapshot untouched spans on affected pages first for verification!)
    let patch_specs: Vec<PatchSpec> = patches_resolved
        .iter()
        .map(|(_, _, s, _, _)| s.clone())
        .collect();
    let mut untouched_by_page = if patch_specs.is_empty() {
        HashMap::new()
    } else {
        patch::snapshot_untouched_spans(&work, &patch_specs).map_err(|e| PlanError {
            code: e.code,
            is_conflict_or_verify: false,
            message: e.message,
        })?
    };
    let mut applied_patches = if patch_specs.is_empty() {
        Vec::new()
    } else {
        patch::apply_patches(&work, &patch_specs, patch_opts).map_err(|e| PlanError {
            code: e.code,
            is_conflict_or_verify: false,
            message: e.message,
        })?
    };

    // 5. Metadata
    if meta_strip {
        edit::strip_metadata(&work);
    }
    for (_, _, k, v) in &meta_sets {
        edit::set_metadata(&work, k, v);
    }

    // 6. Sanitize
    let mut sanitized_items = 0usize;
    if !sanitize_policies.is_empty() {
        let drop_attachments = sanitize_policies.iter().any(|&a| a);
        let policy = sanitize::Policy {
            attachments: drop_attachments,
            ..sanitize::Policy::default()
        };
        let removed = sanitize::sanitize(&work, &policy);
        sanitized_items = removed.len();
    }

    // 7. Page rotations and selection/deletion (applied last; remap 1-based patched page numbers if rearranged!)
    let mut last_rotated = None;
    for (deg, targets) in &page_rotations {
        edit::rotate_pages(&work, targets, *deg).map_err(|e| PlanError {
            code: "input.page_tree",
            is_conflict_or_verify: false,
            message: e,
        })?;
        last_rotated = Some(*deg);
    }

    let mut pages_kept_1based = None;
    if let Some((_, _, ref kept)) = page_kept {
        rearrange::rearrange_pages(&work, kept).map_err(|e| PlanError {
            code: "input.page_tree",
            is_conflict_or_verify: false,
            message: crate::diag::report::strip(&e.0).to_string(),
        })?;
        // `untouched_by_page` is keyed by 1-based page number!
        let mut remapped_untouched: HashMap<i32, Vec<(String, Rect)>> = HashMap::new();
        for (new_idx, &old_idx) in kept.iter().enumerate() {
            if let Some(items) = untouched_by_page.get(&(old_idx + 1)) {
                remapped_untouched.insert((new_idx + 1) as i32, items.clone());
            }
        }
        untouched_by_page = remapped_untouched;
        for ap in &mut applied_patches {
            if let Some(pos) = kept.iter().position(|&old| old == ap.page - 1) {
                ap.page = (pos + 1) as i32;
            }
        }
        pages_kept_1based = Some(kept.iter().map(|i| i + 1).collect());
    }

    // Full rewrite with GC is forced whenever redaction ran so no unreferenced stream or history leaks!
    let mut effective_write = write_opts.clone();
    if !redactions_resolved.is_empty() {
        effective_write.incremental = false;
        effective_write.compress = true;
    }
    if !effective_write.incremental {
        effective_write.garbage = effective_write.garbage.max(1);
    }

    let bytes = write::save(&work, &effective_write).map_err(|e| PlanError {
        code: "input.write",
        is_conflict_or_verify: false,
        message: e.message,
    })?;

    let mut patch_verif = None;
    let mut redact_verif = None;
    let mut all_ok = true;

    if verify_output {
        if !applied_patches.is_empty() {
            let pv = patch::verify_patched_bytes(
                &bytes,
                password,
                &mut applied_patches,
                &untouched_by_page,
                patch_opts,
            )
            .map_err(|e| PlanError {
                code: e.code,
                is_conflict_or_verify: true,
                message: e.message,
            })?;
            if !pv.verified {
                all_ok = false;
            }
            patch_verif = Some(pv);
        }
        if !redact_needles.is_empty()
            && let Ok(verify_matcher) = Matcher::new(&redact_texts, &redact_regexes, false)
        {
            let reopened = Document::open(bytes.clone()).map_err(|e| PlanError {
                code: e.code,
                is_conflict_or_verify: true,
                message: e.message,
            })?;
            if let Some(pw) = password {
                let _ = reopened.authenticate(pw);
            }
            let targets = Targets {
                needles: redact_needles,
                matcher: verify_matcher,
                areas: Vec::new(),
            };
            let v: Verification = verify::verify(&reopened, &targets);
            let ok = v.verified();
            if !ok {
                all_ok = false;
            }
            redact_verif = Some(ok);
        }
    }

    Ok((
        bytes,
        PlanExecution {
            plan_count: plans.len(),
            op_count: indexed.len(),
            conflicts: Vec::new(),
            patches: applied_patches,
            highlights_added,
            annotations_deleted,
            fields_changed,
            redaction_quads,
            sanitized_items,
            pages_kept: pages_kept_1based,
            rotated: last_rotated,
            verified: all_ok,
            patch_verification: patch_verif,
            redact_verified: redact_verif,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{doc_bytes, stream};

    const FONT_RES: &str =
        "/Font<</F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>>>";

    fn multi_page_fixture() -> Document {
        let objs = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R 5 0 R 7 0 R]/Count 3>>".to_vec(),
            format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<<{FONT_RES}>>/Contents 4 0 R>>"
            )
            .into_bytes(),
            stream(
                "",
                b"BT /F1 12 Tf 72 720 Td (Indemnity Cap: Mutual) Tj ET \
                  BT /F1 12 Tf 72 680 Td (Service Fee: $10,000) Tj ET",
            ),
            format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<<{FONT_RES}>>/Contents 6 0 R>>"
            )
            .into_bytes(),
            stream("", b"BT /F1 12 Tf 72 720 Td (Tax ID: 999-00-1234) Tj ET"),
            format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<<{FONT_RES}>>/Contents 8 0 R>>"
            )
            .into_bytes(),
            stream("", b"BT /F1 12 Tf 72 720 Td (INTERNAL SCRATCHPAD) Tj ET"),
        ];
        doc_bytes(&objs)
    }

    #[test]
    fn fork_clean_isolates_edits_and_shares_immutable_state() {
        let base = multi_page_fixture();
        let branch_a = base.fork_clean();
        let branch_b = base.fork_clean();

        edit::set_metadata(&branch_a, "Title", "Branch A");
        edit::set_metadata(&branch_b, "Title", "Branch B");
        let _ = rearrange::rearrange_pages(&branch_a, &[0]);

        assert_eq!(base.metadata("Title"), None);
        assert_eq!(base.page_count().unwrap_or(0), 3);
        assert_eq!(branch_a.metadata("Title").as_deref(), Some("Branch A"));
        assert_eq!(branch_a.page_count().unwrap_or(0), 1);
        assert_eq!(branch_b.metadata("Title").as_deref(), Some("Branch B"));
        assert_eq!(branch_b.page_count().unwrap_or(0), 3);
    }

    #[test]
    fn multi_agent_plans_commit_atomically_and_verify() {
        let base = multi_page_fixture();
        let p_legal = parse_plan(
            r#"{
                "agent": "legal",
                "doc_id": "b3:base",
                "ops": [
                    {"op": "highlight", "id": "p1.b1.l1.s1", "contents": "Approved"},
                    {"op": "patch", "id": "p1.b2.l1.s1", "text": "Service Fee: $25,000"}
                ]
            }"#,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let p_privacy = parse_plan(
            r#"{
                "agent": "privacy",
                "doc_id": "b3:base",
                "ops": [
                    {"op": "redact", "text": "999-00-1234", "boxes": true},
                    {"op": "pages", "delete": "3"},
                    {"op": "meta", "set": {"Title": "Executed Agreement"}}
                ]
            }"#,
        )
        .unwrap_or_else(|e| panic!("{e}"));

        let (bytes, exec) = execute_plans(
            &base,
            "b3:base",
            &[p_legal, p_privacy],
            &WriteOptions::default(),
            &PatchOptions::default(),
            None,
            true,
        )
        .unwrap_or_else(|e| panic!("{e}"));

        assert!(exec.conflicts.is_empty());
        assert!(exec.verified);
        assert_eq!(exec.plan_count, 2);
        assert_eq!(exec.highlights_added, 1);
        assert_eq!(exec.patches.len(), 1);
        assert_eq!(exec.pages_kept, Some(vec![1, 2]));
        assert_eq!(exec.redact_verified, Some(true));

        // Base document must remain completely untouched!
        assert_eq!(base.page_count().unwrap_or(0), 3);
        assert_eq!(base.metadata("Title"), None);

        let out = Document::open(bytes).unwrap_or_else(|e| panic!("{}", e.message));
        assert_eq!(out.page_count().unwrap_or(0), 2);
        assert_eq!(out.metadata("Title").as_deref(), Some("Executed Agreement"));
    }

    #[test]
    fn detects_stale_doc_id_and_cross_agent_conflicts() {
        let base = multi_page_fixture();

        let stale = parse_plan(r#"{"agent":"a","doc_id":"b3:wrong","ops":[]}"#)
            .unwrap_or_else(|e| panic!("{e}"));
        let err = execute_plans(
            &base,
            "b3:base",
            &[stale],
            &WriteOptions::default(),
            &PatchOptions::default(),
            None,
            true,
        )
        .unwrap_err();
        assert_eq!(err.code, "plan.stale_doc_id");
        assert!(err.is_conflict_or_verify);

        let p1 = parse_plan(
            r#"{"agent":"legal","doc_id":"b3:base","ops":[{"op":"patch","id":"p1.b2.l1.s1","text":"Service Fee: $25,000"}]}"#,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let p2 = parse_plan(
            r#"{"agent":"rogue","doc_id":"b3:base","ops":[{"op":"redact","id":"p1.b2.l1.s1"}]}"#,
        )
        .unwrap_or_else(|e| panic!("{e}"));

        let (bytes, exec) = execute_plans(
            &base,
            "b3:base",
            &[p1, p2],
            &WriteOptions::default(),
            &PatchOptions::default(),
            None,
            true,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(bytes.is_empty());
        assert!(!exec.verified);
        assert_eq!(exec.conflicts.len(), 1);
        assert_eq!(exec.conflicts[0].code, "conflict.patch_vs_redact");

        let p_del = parse_plan(
            r#"{"agent":"cleaner","doc_id":"b3:base","ops":[{"op":"pages","delete":"1"}]}"#,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let p_hl = parse_plan(
            r#"{"agent":"reviewer","doc_id":"b3:base","ops":[{"op":"highlight","id":"p1.b1.l1.s1"}]}"#,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let (_, exec2) = execute_plans(
            &base,
            "b3:base",
            &[p_del, p_hl],
            &WriteOptions::default(),
            &PatchOptions::default(),
            None,
            true,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(!exec2.verified);
        assert_eq!(exec2.conflicts[0].code, "conflict.page_deleted");
    }

    fn run(base: &Document, plans: &[&str]) -> Result<(Vec<u8>, PlanExecution), PlanError> {
        let plans: Vec<EditPlan> = plans
            .iter()
            .map(|p| parse_plan(p).unwrap_or_else(|e| panic!("{e}")))
            .collect();
        execute_plans(
            base,
            "b3:base",
            &plans,
            &WriteOptions::default(),
            &PatchOptions::default(),
            None,
            true,
        )
    }

    #[test]
    fn a_redaction_with_both_text_and_regex_is_refused() {
        let base = multi_page_fixture();
        let err = run(
            &base,
            &[r#"[{"op":"redact","text":"Mutual","regex":"[0-9]{3}-[0-9]{2}-[0-9]{4}"}]"#],
        )
        .unwrap_err();
        assert_eq!(err.code, "usage.bad_plan");
        assert!(!err.is_conflict_or_verify);
    }

    fn highlights_doc() -> Document {
        let annot = |n: &str| {
            format!(
                "<</Type/Annot/Subtype/Highlight/Rect[72 700 200 714]/QuadPoints[72 714 200 714 72 700 200 700]/Contents({n})>>"
            )
            .into_bytes()
        };
        doc_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<<{FONT_RES}>>/Contents 4 0 R/Annots[5 0 R 6 0 R 7 0 R]>>"
            )
            .into_bytes(),
            stream("", b"BT /F1 12 Tf 72 704 Td (Highlighted words) Tj ET"),
            annot("first"),
            annot("second"),
            annot("third"),
        ])
    }

    fn annot_contents(bytes: &[u8]) -> Vec<String> {
        let out = Document::open(bytes.to_vec()).unwrap_or_else(|e| panic!("{}", e.message));
        let page = out.page(0).unwrap_or_else(|e| panic!("{}", e.0));
        annot::page_annots(&out, &page)
            .0
            .iter()
            .map(|a| annot::contents(&out, a))
            .collect()
    }

    #[test]
    fn annotation_deletes_resolve_by_the_base_listing() {
        let base = highlights_doc();
        let (bytes, exec) = run(
            &base,
            &[r#"[{"op":"annot_delete","id":"p1.a1"},{"op":"annot_delete","id":"p1.a2"}]"#],
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(exec.annotations_deleted, 2);
        assert_eq!(annot_contents(&bytes), vec!["third".to_string()]);
    }

    #[test]
    fn an_annotation_delete_that_names_nothing_is_an_error() {
        let base = highlights_doc();
        let err = run(
            &base,
            &[r#"[{"op":"annot_delete","id":"p1.a1"},{"op":"annot_delete","id":"p1.a9"}]"#],
        )
        .unwrap_err();
        assert_eq!(err.code, "input.target_not_found");
    }

    #[test]
    #[ignore = "benchmark: run with `cargo test --release -p nupdf --lib -- fork_clean_microbenchmark --ignored --nocapture`"]
    fn fork_clean_microbenchmark_1000_branches() {
        let mut kids = String::new();
        let mut objs = vec![b"<</Type/Catalog/Pages 2 0 R>>".to_vec(), Vec::new()];
        for i in 0..50usize {
            let page_obj = 3 + i * 2;
            let stream_obj = page_obj + 1;
            if !kids.is_empty() {
                kids.push(' ');
            }
            kids.push_str(&format!("{page_obj} 0 R"));
            objs.push(
                format!(
                    "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<<{FONT_RES}>>/Contents {stream_obj} 0 R>>"
                )
                .into_bytes(),
            );
            objs.push(stream(
                "",
                format!("BT /F1 12 Tf 40 700 Td (Page {i} clause text) Tj ET\n").as_bytes(),
            ));
        }
        objs[1] = format!("<</Type/Pages/Kids[{kids}]/Count 50>>").into_bytes();
        let seed_doc = doc_bytes(&objs);
        let pdf_bytes = crate::write::save(&seed_doc, &WriteOptions::default())
            .unwrap_or_else(|e| panic!("{}", e.message));

        let iterations = 1_000usize;
        let t_open = std::time::Instant::now();
        for i in 0..iterations {
            let d = Document::open(pdf_bytes.as_slice()).unwrap_or_else(|e| panic!("{e}"));
            let _ = d.page(25).unwrap_or_else(|e| panic!("{}", e.0));
            edit::set_metadata(&d, "Title", &format!("Branch {i}"));
        }
        let open_ms = t_open.elapsed().as_secs_f64() * 1000.0;

        let base = Document::open(pdf_bytes.as_slice()).unwrap_or_else(|e| panic!("{e}"));
        let _ = base.page(25).unwrap_or_else(|e| panic!("{}", e.0));
        let t_fork = std::time::Instant::now();
        for i in 0..iterations {
            let d = base.fork_clean();
            let _ = d.page(25).unwrap_or_else(|e| panic!("{}", e.0));
            edit::set_metadata(&d, "Title", &format!("Branch {i}"));
        }
        let fork_ms = t_fork.elapsed().as_secs_f64() * 1000.0;

        println!(
            "BENCH_FORK_CLEAN iterations={iterations} pdf_bytes={} open_ms={open_ms:.3} fork_clean_ms={fork_ms:.3} speedup={:.1}x",
            pdf_bytes.len(),
            open_ms / fork_ms.max(0.0001)
        );
        assert!(fork_ms < open_ms);
    }
}
