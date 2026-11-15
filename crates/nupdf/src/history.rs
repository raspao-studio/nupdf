// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Temporal cross-reference time-travel and cryptographic provenance forensics.
//!
//! Incrementally saved PDFs (multi-party signature envelopes, redline rounds,
//! form fills, or post-signature tampered invoices) retain stacked historical
//! `%%EOF` revisions inside one byte buffer. [`inspect_history`] opens each
//! chronological revision via [`Document::open_revision`], verifies PKCS #7
//! cryptographic signatures at every historical state, diffs page content lines,
//! AcroForm fields, annotations, and perceptual span visibility, and classifies
//! each revision with deterministic forensic hazard codes.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::DocId;
use crate::annot;
use crate::attest::{self, AttestOptions, PageAttestation, Visibility};
use crate::content::Store;
use crate::document::Document;
use crate::extract;
use crate::form;
use crate::geometry::Rect;
use crate::object::Object;
use crate::signature;
use crate::stext::{self, Options as StextOptions};

/// Most objects hashed into one page's fingerprint; past it the rest of the
/// page hashes as a marker, the same in every revision.
const MAX_FINGERPRINT_NODES: usize = 200_000;

/// Deepest object nesting followed when fingerprinting.
const MAX_FINGERPRINT_DEPTH: usize = 64;

/// Maximum incremental revisions inspected on a single document.
const MAX_REVISIONS_INSPECTED: usize = 1024;

/// Threshold above which per-page line diff switches from quadratic LCS to
/// linear multiset comparison so pathological pages remain strictly bounded.
const MAX_LCS_LINES: usize = 512;

/// Hazard code: a revision altered page content streams, page resources
/// (fonts, images, forms) or page boxes after a valid signature.
pub const TAMPER_POST_SIGNATURE_CONTENT_EDIT: &str = "tamper.post_signature_content_edit";
/// Hazard code: a revision visually occluded previously visible text after a valid signature.
pub const TAMPER_POST_SIGNATURE_VISUAL_OCCLUSION: &str = "tamper.post_signature_visual_occlusion";
/// Hazard code: a signature that was cryptographically valid (`"OK"`) in a prior revision broke.
pub const TAMPER_SIGNATURE_BROKEN: &str = "tamper.signature_broken";
/// Hazard code: annotations other than form-field widgets were added, removed
/// or changed, or form fields were added or removed, after a valid signature.
pub const TAMPER_POST_SIGNATURE_ANNOTATION_EDIT: &str = "tamper.post_signature_annotation_edit";

/// Revision classification: initial base revision (`rev == 1`).
pub const CLASS_INITIAL: &str = "initial";
/// Revision classification: incremental form fill, signature, or annotation update with clean content streams.
pub const CLASS_CLEAN_INCREMENTAL_FILL: &str = "clean_incremental_fill";
/// Revision classification: unsigned content or structure edit with no prior valid signature violated.
pub const CLASS_UNSIGNED_EDIT: &str = "unsigned_edit";
/// Revision classification: post-signature content alteration, visual occlusion, or broken signature digest.
pub const CLASS_TAMPERED_AFTER_SIGNATURE: &str = "tampered_after_signature";

fn default_true() -> bool {
    true
}

/// Configuration for [`inspect_history`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryOptions {
    /// When `true` (the default), runs perceptual page attestation on each
    /// revision to detect post-signature visual occlusion attacks (e.g. a white
    /// rectangle or opaque annotation painted over a signed dollar amount).
    #[serde(default = "default_true")]
    pub run_attest: bool,
}

impl Default for HistoryOptions {
    fn default() -> Self {
        Self { run_attest: true }
    }
}

/// Status of a signature widget at a specific historical revision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevisionSignature {
    /// Full AcroForm field name (`""` if unnamed).
    pub name: String,
    /// Whether the field holds a signature dictionary (`/V`).
    pub signed: bool,
    /// Certificate subject summary (`"cn=..., o=..., ..."`), if signed and readable.
    pub signatory: Option<String>,
    /// `true` strictly when `status == "OK"`.
    pub valid: bool,
    /// MuPDF-compatible digest verification status (`"OK"`, `"Not signed."`, etc.).
    pub status: String,
    /// `true` when `/ByteRange` starts at byte `0` and extends to the exact end
    /// of this revision's byte slice.
    pub covers_whole_revision: bool,
}

/// One line of structured page text added or removed between consecutive revisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevisionLineChange {
    /// 1-based page number.
    pub page: i32,
    /// Trimmed line text.
    pub text: String,
}

/// One non-signature AcroForm field value change between consecutive revisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevisionFieldChange {
    /// Full dotted AcroForm field name.
    pub name: String,
    /// Value in `rev - 1` (`None` if the field was newly added in `rev`).
    pub old_value: Option<String>,
    /// Value in `rev` (`None` if the field was removed in `rev`).
    pub new_value: Option<String>,
}

/// Structural, textual, form, annotation, and perceptual diff from `rev - 1` to `rev`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevisionDelta {
    pub pages_added: i32,
    pub pages_removed: i32,
    pub lines_added: Vec<RevisionLineChange>,
    pub lines_removed: Vec<RevisionLineChange>,
    pub fields_changed: Vec<RevisionFieldChange>,
    pub annots_added: usize,
    pub annots_removed: usize,
    pub newly_occluded_spans: Vec<String>,
    pub forensic_hazards: Vec<String>,
}

/// Forensic snapshot of a single chronological PDF revision (`1..=N`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevisionSnapshot {
    /// 1-based chronological revision index (`1` = oldest, `N` = latest).
    pub revision: usize,
    /// Exact ending byte offset in the container file (`1..=data.len()`).
    pub byte_end: usize,
    /// Content hash (`b3:...`) of `&data[..byte_end]`.
    pub doc_id: String,
    /// Page count at this revision.
    pub page_count: i32,
    /// Verdict: `"initial"`, `"clean_incremental_fill"`, `"unsigned_edit"`, or `"tampered_after_signature"`.
    pub classification: String,
    /// Signature fields present at this revision, in page order.
    pub signatures: Vec<RevisionSignature>,
    /// Diff relative to `revision - 1` (`None` for `revision == 1`).
    pub delta_from_previous: Option<RevisionDelta>,
}

/// Complete temporal cross-reference and tamper-forensics report for a PDF.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryReport {
    /// Number of chronological revisions inspected (`1..=N`).
    pub revision_count: usize,
    /// Whether the outer document required xref repair (collapsing history to 1 revision).
    pub repaired: bool,
    /// `true` if any revision is classified as `"tampered_after_signature"`.
    pub tampered: bool,
    /// Chronological snapshots from revision `1` through `revision_count`.
    pub revisions: Vec<RevisionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AnnotKey {
    page: i32,
    kind: &'static str,
    rect_q: [i32; 4],
    contents: String,
    /// Fingerprint of the annotation's `/AP`.
    appearance: [u8; 32],
}

struct ExtractedRevState {
    page_count: i32,
    lines_by_page: Vec<Vec<String>>,
    /// Per page: a hash of its content streams, resources and boxes, with
    /// everything they reach.
    page_fingerprints: Vec<[u8; 32]>,
    fields: BTreeMap<String, String>,
    annots: BTreeMap<AnnotKey, usize>,
    signatures: Vec<RevisionSignature>,
    attestations: Option<Vec<PageAttestation>>,
}

/// Inspects every chronological revision of `doc`, verifying signatures and
/// computing inter-revision deltas and post-signature tamper classifications.
pub fn inspect_history(doc: &Document, opts: &HistoryOptions) -> Result<HistoryReport, String> {
    let ends = doc.revision_byte_ends();
    if ends.len() > MAX_REVISIONS_INSPECTED {
        return Err(format!(
            "limit error: document has {} revisions (max {MAX_REVISIONS_INSPECTED})",
            ends.len()
        ));
    }

    let mut snapshots: Vec<RevisionSnapshot> = Vec::with_capacity(ends.len());
    let mut prev_state: Option<ExtractedRevState> = None;
    let mut any_prior_valid_sig = false;

    for (idx, &byte_end) in ends.iter().enumerate() {
        let rev_num = idx + 1;
        let rev_bytes = doc.revision_bytes(rev_num).map_err(|e| e.message)?;
        let doc_id = DocId::of(rev_bytes).to_string();
        let rev_doc = doc.open_revision(rev_num).map_err(|e| e.message)?;
        let curr_state = extract_rev_state(&rev_doc, opts.run_attest && ends.len() > 1)?;

        let (classification, delta) = match &prev_state {
            None => {
                let broken_initial = curr_state
                    .signatures
                    .iter()
                    .any(|s| s.signed && s.status == signature::DIGEST_FAILURE);
                let class = if broken_initial {
                    CLASS_TAMPERED_AFTER_SIGNATURE.to_string()
                } else {
                    CLASS_INITIAL.to_string()
                };
                (class, None)
            }
            Some(prev) => {
                let prev_had_valid = prev.signatures.iter().any(|s| s.valid);
                let had_valid_sig = any_prior_valid_sig || prev_had_valid;
                let d = compute_delta(prev, &curr_state, had_valid_sig);
                let class = classify_delta(&d, &curr_state, had_valid_sig);
                (class, Some(d))
            }
        };

        if curr_state.signatures.iter().any(|s| s.valid) {
            any_prior_valid_sig = true;
        }

        snapshots.push(RevisionSnapshot {
            revision: rev_num,
            byte_end,
            doc_id,
            page_count: curr_state.page_count,
            classification,
            signatures: curr_state.signatures.clone(),
            delta_from_previous: delta,
        });
        prev_state = Some(curr_state);
    }

    let tampered = snapshots
        .iter()
        .any(|s| s.classification == CLASS_TAMPERED_AFTER_SIGNATURE);

    Ok(HistoryReport {
        revision_count: snapshots.len(),
        repaired: doc.repaired(),
        tampered,
        revisions: snapshots,
    })
}

fn extract_rev_state(doc: &Document, run_attest: bool) -> Result<ExtractedRevState, String> {
    let page_count = doc.page_count().map_err(|e| e.0)?;
    let cap = usize::try_from(page_count.max(0)).unwrap_or(0);
    let mut lines_by_page = Vec::with_capacity(cap);
    let mut fields = BTreeMap::new();
    let mut annots: BTreeMap<AnnotKey, usize> = BTreeMap::new();
    let mut signatures = Vec::new();
    let mut seen_sig_objs: Vec<i32> = Vec::new();
    let mut store = Store::default();
    let mut hasher = ObjHasher::new(doc);
    let mut page_fingerprints = Vec::with_capacity(cap);

    for p_idx in 0..page_count {
        let page_obj = doc.page(p_idx).map_err(|e| e.0)?;
        let (_, ctm) = doc.page_transform(&page_obj).map_err(|e| e.0)?;

        // 1. Page content-stream text lines (`Parts::CONTENTS`).
        let (stext_res, next_store) =
            stext::extract_page(doc, &page_obj, StextOptions::default(), store);
        store = next_store;
        let stext_page = stext_res?;
        let block_refs: Vec<&stext::Block> = stext_page.blocks.iter().collect();
        let mut page_lines = Vec::new();
        for tb in extract::text_blocks(&block_refs) {
            for line in &tb.lines {
                let text = extract::chars_text(&line.chars).trim().to_string();
                if !text.is_empty() {
                    page_lines.push(text);
                }
            }
        }
        lines_by_page.push(page_lines);
        page_fingerprints.push(hasher.page(&page_obj));

        // 2. Non-widget annotations and form/signature widgets on this page.
        let (page_annots, page_widgets) = annot::page_annots(doc, &page_obj);
        for a in &page_annots {
            let kind = annot::type_name(doc, a);
            let r = annot::rect(doc, a, ctm).unwrap_or(Rect::EMPTY);
            let rect_q = [
                (r.x0 * 10.0).round() as i32,
                (r.y0 * 10.0).round() as i32,
                (r.x1 * 10.0).round() as i32,
                (r.y1 * 10.0).round() as i32,
            ];
            let contents = annot::contents(doc, a);
            let appearance = hasher.value_hash(&doc.dict_get(a, b"AP"));
            let key = AnnotKey {
                page: p_idx + 1,
                kind,
                rect_q,
                contents,
                appearance,
            };
            *annots.entry(key).or_insert(0) += 1;
        }

        for w in &page_widgets {
            if form::type_string(doc, w) == "signature" {
                if let Object::Ref(r) = w {
                    if seen_sig_objs.contains(&r.num) {
                        continue;
                    }
                    seen_sig_objs.push(r.num);
                }
                signatures.push(inspect_signature_field(doc, w));
            } else if let Ok(name) = form::name(doc, w)
                && !name.is_empty()
            {
                fields.entry(name).or_insert_with(|| form::value(doc, w));
            }
        }
    }

    // Also inspect top-level `/AcroForm /Fields` in case a form or signature
    // field has no widget placed in a page `/Annots` array.
    let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
    let acro = doc.dict_get(&root, b"AcroForm");
    let acro_fields = doc.resolve(&doc.dict_get(&acro, b"Fields"));
    for f_obj in acro_fields.as_array().unwrap_or(&[]).iter().take(4096) {
        if form::type_string(doc, f_obj) == "signature" {
            if let Object::Ref(r) = f_obj {
                if seen_sig_objs.contains(&r.num) {
                    continue;
                }
                seen_sig_objs.push(r.num);
            }
            signatures.push(inspect_signature_field(doc, f_obj));
        } else if let Ok(name) = form::name(doc, f_obj)
            && !name.is_empty()
        {
            fields
                .entry(name)
                .or_insert_with(|| form::value(doc, f_obj));
        }
    }

    let attestations = if run_attest && page_count > 0 {
        Some(attest::attest_document(
            doc,
            &AttestOptions::default(),
            false,
        )?)
    } else {
        None
    };

    Ok(ExtractedRevState {
        page_count,
        lines_by_page,
        page_fingerprints,
        fields,
        annots,
        signatures,
        attestations,
    })
}

/// Content hashes of objects in one revision, each reachable object hashed
/// once (`memo`), cycles cut (`open`), and the whole walk bounded per page.
struct ObjHasher<'a> {
    doc: &'a Document,
    memo: HashMap<i32, [u8; 32]>,
    open: HashSet<i32>,
    budget: usize,
}

impl<'a> ObjHasher<'a> {
    fn new(doc: &'a Document) -> Self {
        ObjHasher {
            doc,
            memo: HashMap::new(),
            open: HashSet::new(),
            budget: MAX_FINGERPRINT_NODES,
        }
    }

    /// A page's content streams, resources and boxes, as drawn.
    fn page(&mut self, page: &Object) -> [u8; 32] {
        self.budget = MAX_FINGERPRINT_NODES;
        let mut h = blake3::Hasher::new();
        let contents = self.doc.dict_get(page, b"Contents");
        h.update(b"Contents");
        self.value(&contents, &mut h, 0);
        for key in [&b"Resources"[..], b"MediaBox", b"CropBox", b"Rotate"] {
            let v = self.doc.inherited(page, key).ok().flatten();
            h.update(key);
            self.value(&v.unwrap_or(Object::Null), &mut h, 0);
        }
        *h.finalize().as_bytes()
    }

    fn value_hash(&mut self, obj: &Object) -> [u8; 32] {
        self.budget = MAX_FINGERPRINT_NODES;
        let mut h = blake3::Hasher::new();
        self.value(obj, &mut h, 0);
        *h.finalize().as_bytes()
    }

    fn reference(&mut self, num: i32, depth: usize) -> [u8; 32] {
        if let Some(done) = self.memo.get(&num) {
            return *done;
        }
        let mut h = blake3::Hasher::new();
        if !self.open.insert(num) {
            h.update(b"cycle");
            h.update(&num.to_le_bytes());
            return *h.finalize().as_bytes();
        }
        let obj = self.doc.object(num).unwrap_or(Object::Null);
        self.value(&obj, &mut h, depth + 1);
        self.open.remove(&num);
        let out = *h.finalize().as_bytes();
        self.memo.insert(num, out);
        out
    }

    fn value(&mut self, obj: &Object, h: &mut blake3::Hasher, depth: usize) {
        if self.budget == 0 || depth > MAX_FINGERPRINT_DEPTH {
            h.update(b"~");
            return;
        }
        self.budget -= 1;
        match obj {
            Object::Null => {
                h.update(b"n");
            }
            Object::Bool(b) => {
                h.update(if *b { b"T" } else { b"F" });
            }
            Object::Int(i) => {
                h.update(b"i");
                h.update(&i.to_le_bytes());
            }
            Object::Real(f) => {
                h.update(b"r");
                h.update(&f.to_bits().to_le_bytes());
            }
            Object::Name(n) => {
                h.update(b"/");
                h.update(&(n.len() as u64).to_le_bytes());
                h.update(n);
            }
            Object::String(st) => {
                h.update(b"(");
                h.update(&(st.len() as u64).to_le_bytes());
                h.update(st);
            }
            Object::Array(a) => {
                h.update(b"[");
                h.update(&(a.len() as u64).to_le_bytes());
                for item in a.iter() {
                    self.value(item, h, depth + 1);
                }
            }
            Object::Dict(d) => self.dict(d, h, depth),
            Object::Ref(r) => {
                let sub = self.reference(r.num, depth);
                h.update(b"R");
                h.update(&sub);
            }
            Object::Stream(st) => {
                self.dict(&st.dict, h, depth);
                h.update(b"stream");
                match self.doc.raw_stream_data(st) {
                    Ok(data) => {
                        h.update(&(data.len() as u64).to_le_bytes());
                        h.update(&data);
                    }
                    Err(_) => {
                        h.update(b"unreadable");
                    }
                }
            }
        }
    }

    fn dict(&mut self, d: &crate::object::Dict, h: &mut blake3::Hasher, depth: usize) {
        h.update(b"<<");
        for (k, v) in d.iter() {
            // A form XObject or pattern pointing back at its page would pull
            // the page tree in; what it draws is what matters.
            if k == b"Parent" {
                continue;
            }
            h.update(&(k.len() as u64).to_le_bytes());
            h.update(k);
            self.value(v, h, depth + 1);
        }
        h.update(b">>");
    }
}

fn inspect_signature_field(doc: &Document, field: &Object) -> RevisionSignature {
    let name = form::name(doc, field).unwrap_or_default();
    let signed = form::is_signed(doc, field);
    if !signed {
        return RevisionSignature {
            name,
            signed: false,
            signatory: None,
            valid: false,
            status: "Not signed.".to_string(),
            covers_whole_revision: false,
        };
    }

    let signatory = signature::signatory(doc, field);
    let status = match signature::check_digest(doc, field) {
        Ok(s) => s.to_string(),
        Err(e) => e,
    };
    let valid = status == signature::OK;
    let file_len = doc.data().len();
    let covers_whole_revision = signature::byte_range(doc, field).is_ok_and(|ranges| {
        let starts_at_zero = ranges.first().is_some_and(|&(o, _)| o == 0);
        let ends_at_eof = ranges
            .last()
            .and_then(|&(o, l)| usize::try_from(o.saturating_add(l)).ok())
            .is_some_and(|end| end == file_len);
        starts_at_zero && ends_at_eof
    });

    RevisionSignature {
        name,
        signed: true,
        signatory,
        valid,
        status,
        covers_whole_revision,
    }
}

fn compute_delta(
    prev: &ExtractedRevState,
    curr: &ExtractedRevState,
    had_valid_sig: bool,
) -> RevisionDelta {
    let pages_added = (curr.page_count - prev.page_count).max(0);
    let pages_removed = (prev.page_count - curr.page_count).max(0);

    let mut lines_added = Vec::new();
    let mut lines_removed = Vec::new();
    let common_pages = prev.lines_by_page.len().min(curr.lines_by_page.len());

    for p in 0..common_pages {
        let page_num = (p as i32).saturating_add(1);
        let (removed, added) = diff_page_lines(&prev.lines_by_page[p], &curr.lines_by_page[p]);
        for text in removed {
            lines_removed.push(RevisionLineChange {
                page: page_num,
                text,
            });
        }
        for text in added {
            lines_added.push(RevisionLineChange {
                page: page_num,
                text,
            });
        }
    }
    for (p, lines) in curr.lines_by_page.iter().enumerate().skip(common_pages) {
        let page_num = (p as i32).saturating_add(1);
        for text in lines {
            lines_added.push(RevisionLineChange {
                page: page_num,
                text: text.clone(),
            });
        }
    }
    for (p, lines) in prev.lines_by_page.iter().enumerate().skip(common_pages) {
        let page_num = (p as i32).saturating_add(1);
        for text in lines {
            lines_removed.push(RevisionLineChange {
                page: page_num,
                text: text.clone(),
            });
        }
    }

    // Form field value changes.
    let mut fields_changed = Vec::new();
    for (name, old_val) in &prev.fields {
        match curr.fields.get(name) {
            Some(new_val) if new_val != old_val => {
                fields_changed.push(RevisionFieldChange {
                    name: name.clone(),
                    old_value: Some(old_val.clone()),
                    new_value: Some(new_val.clone()),
                });
            }
            None => {
                fields_changed.push(RevisionFieldChange {
                    name: name.clone(),
                    old_value: Some(old_val.clone()),
                    new_value: None,
                });
            }
            Some(_) => {}
        }
    }
    for (name, new_val) in &curr.fields {
        if !prev.fields.contains_key(name) {
            fields_changed.push(RevisionFieldChange {
                name: name.clone(),
                old_value: None,
                new_value: Some(new_val.clone()),
            });
        }
    }

    // Annotation multiset diff.
    let mut annots_added = 0usize;
    let mut annots_removed = 0usize;
    for (k, &prev_n) in &prev.annots {
        let curr_n = curr.annots.get(k).copied().unwrap_or(0);
        annots_removed += prev_n.saturating_sub(curr_n);
    }
    for (k, &curr_n) in &curr.annots {
        let prev_n = prev.annots.get(k).copied().unwrap_or(0);
        annots_added += curr_n.saturating_sub(prev_n);
    }

    // Perceptual visibility occlusion diff.
    let newly_occluded_spans = match (&prev.attestations, &curr.attestations) {
        (Some(pa), Some(ca)) => detect_newly_occluded(pa, ca),
        _ => Vec::new(),
    };

    // Deterministic forensic hazard evaluation. Text is not the only thing a
    // revision can change: an image, a font or a vector drawn over the page
    // changes no extracted line, so the objects each page reaches are compared.
    let mut forensic_hazards = Vec::new();
    let pages_redrawn = prev
        .page_fingerprints
        .iter()
        .zip(&curr.page_fingerprints)
        .any(|(a, b)| a != b);
    let content_edited = pages_added > 0
        || pages_removed > 0
        || !lines_added.is_empty()
        || !lines_removed.is_empty()
        || pages_redrawn;
    if had_valid_sig && content_edited {
        forensic_hazards.push(TAMPER_POST_SIGNATURE_CONTENT_EDIT.to_string());
    }
    // Filling existing fields and adding signatures is what may follow a
    // signature; any other annotation, or a field added or removed, may not.
    let fields_added_or_removed = fields_changed
        .iter()
        .any(|f| f.old_value.is_none() || f.new_value.is_none());
    if had_valid_sig && (annots_added > 0 || annots_removed > 0 || fields_added_or_removed) {
        forensic_hazards.push(TAMPER_POST_SIGNATURE_ANNOTATION_EDIT.to_string());
    }
    if had_valid_sig && !newly_occluded_spans.is_empty() {
        forensic_hazards.push(TAMPER_POST_SIGNATURE_VISUAL_OCCLUSION.to_string());
    }
    if signature_was_broken(&prev.signatures, &curr.signatures) {
        forensic_hazards.push(TAMPER_SIGNATURE_BROKEN.to_string());
    }

    RevisionDelta {
        pages_added,
        pages_removed,
        lines_added,
        lines_removed,
        fields_changed,
        annots_added,
        annots_removed,
        newly_occluded_spans,
        forensic_hazards,
    }
}

fn classify_delta(delta: &RevisionDelta, curr: &ExtractedRevState, had_valid_sig: bool) -> String {
    if !delta.forensic_hazards.is_empty() {
        return CLASS_TAMPERED_AFTER_SIGNATURE.to_string();
    }
    let content_untouched = delta.pages_added == 0
        && delta.pages_removed == 0
        && delta.lines_added.is_empty()
        && delta.lines_removed.is_empty()
        && delta.newly_occluded_spans.is_empty();
    let form_annot_or_sig_activity = had_valid_sig
        || curr.signatures.iter().any(|s| s.valid)
        || !delta.fields_changed.is_empty()
        || delta.annots_added > 0
        || delta.annots_removed > 0;
    if content_untouched && form_annot_or_sig_activity {
        CLASS_CLEAN_INCREMENTAL_FILL.to_string()
    } else {
        CLASS_UNSIGNED_EDIT.to_string()
    }
}

fn signature_was_broken(prev: &[RevisionSignature], curr: &[RevisionSignature]) -> bool {
    for (idx, ps) in prev.iter().enumerate() {
        if !ps.valid {
            continue;
        }
        let matched = if !ps.name.is_empty() {
            curr.iter().find(|cs| cs.name == ps.name)
        } else {
            curr.get(idx)
        };
        match matched {
            Some(cs) if cs.signed && cs.valid => {}
            _ => return true,
        }
    }
    false
}

fn detect_newly_occluded(prev: &[PageAttestation], curr: &[PageAttestation]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let common = prev.len().min(curr.len());
    for p in 0..common {
        let prev_page = &prev[p];
        let curr_page = &curr[p];
        for cs in &curr_page.spans {
            if cs.visibility != Visibility::ZeroContrastOrOccluded {
                continue;
            }
            let c_text = cs.text.trim();
            if c_text.is_empty() {
                continue;
            }
            let c_rect = Rect::new(cs.bbox[0], cs.bbox[1], cs.bbox[2], cs.bbox[3]);
            let already_occluded = prev_page.spans.iter().any(|ps| {
                ps.visibility == Visibility::ZeroContrastOrOccluded
                    && ps.text.trim() == c_text
                    && boxes_overlap(
                        Rect::new(ps.bbox[0], ps.bbox[1], ps.bbox[2], ps.bbox[3]),
                        c_rect,
                    )
            });
            if already_occluded {
                continue;
            }
            let was_visible = prev_page.spans.iter().any(|ps| {
                if ps.visibility != Visibility::Visible {
                    return false;
                }
                let p_rect = Rect::new(ps.bbox[0], ps.bbox[1], ps.bbox[2], ps.bbox[3]);
                boxes_overlap(p_rect, c_rect) || ps.text.trim() == c_text
            });
            if was_visible && !out.iter().any(|existing| existing == c_text) {
                out.push(c_text.to_string());
            }
        }
    }
    out
}

fn boxes_overlap(a: Rect, b: Rect) -> bool {
    let inter = a.intersect(b);
    !inter.is_empty() && (inter.x1 - inter.x0) > 0.5 && (inter.y1 - inter.y0) > 0.5
}

/// Computes `(removed, added)` line sequences for a single page.
fn diff_page_lines(old: &[String], new: &[String]) -> (Vec<String>, Vec<String>) {
    if old == new {
        return (Vec::new(), Vec::new());
    }
    if old.len() > MAX_LCS_LINES || new.len() > MAX_LCS_LINES {
        let mut counts: BTreeMap<&str, isize> = BTreeMap::new();
        for s in old {
            *counts.entry(s.as_str()).or_insert(0) -= 1;
        }
        for s in new {
            *counts.entry(s.as_str()).or_insert(0) += 1;
        }
        let mut removed = Vec::new();
        let mut added = Vec::new();
        for s in old {
            if let Some(c) = counts.get_mut(s.as_str())
                && *c < 0
            {
                *c += 1;
                removed.push(s.clone());
            }
        }
        for s in new {
            if let Some(c) = counts.get_mut(s.as_str())
                && *c > 0
            {
                *c -= 1;
                added.push(s.clone());
            }
        }
        return (removed, added);
    }

    let n = old.len();
    let m = new.len();
    let cols = m + 1;
    let mut dp = vec![0u16; (n + 1) * cols];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * cols + j] = if old[i] == new[j] {
                dp[(i + 1) * cols + (j + 1)].saturating_add(1)
            } else {
                dp[(i + 1) * cols + j].max(dp[i * cols + (j + 1)])
            };
        }
    }

    let mut removed = Vec::new();
    let mut added = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old[i] == new[j] {
            i += 1;
            j += 1;
        } else if dp[(i + 1) * cols + j] >= dp[i * cols + (j + 1)] {
            removed.push(old[i].clone());
            i += 1;
        } else {
            added.push(new[j].clone());
            j += 1;
        }
    }
    while i < n {
        removed.push(old[i].clone());
        i += 1;
    }
    while j < m {
        added.push(new[j].clone());
        j += 1;
    }
    (removed, added)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::annot_edit::Editor;
    use crate::fill;
    use crate::object::Dict;
    use crate::patch::{self, PatchOptions, PatchSpec};
    use crate::write::{self, WriteOptions};

    // Deterministic 512-bit (64-byte) test RSA keypair (e = 65537).
    const TEST_RSA_N_HEX: &str = concat!(
        "900000000000000000000000000000000000000000000000000000000000eccd",
        "000000000000000000000000000000000000000000000000000000003271d3cb"
    );
    const TEST_RSA_D_HEX: &str = concat!(
        "16cea93156cea93156cea93156cea93156cea93156cea93156cea93156ceceb2",
        "7c9783687c9783687c9783687c9783687c9783687c9783687c9783688494a8c1"
    );

    fn unhex(s: &str) -> Vec<u8> {
        let clean: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        clean
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let hi = (pair[0] as char).to_digit(16).unwrap() as u8;
                let lo = (pair[1] as char).to_digit(16).unwrap() as u8;
                (hi << 4) | lo
            })
            .collect()
    }

    fn hex_upper(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            out.push_str(&format!("{b:02X}"));
        }
        out
    }

    /// Minimal big-int pow_mod for signing PKCS#1 v1.5 test digests with `(n, d)`.
    fn rsa_raw_sign(msg_be: &[u8], d_be: &[u8], n_be: &[u8]) -> Vec<u8> {
        #[derive(Clone)]
        struct Big(Vec<u32>);
        impl Big {
            fn from_be(b: &[u8]) -> Self {
                let mut v: Vec<u32> = b
                    .rchunks(4)
                    .map(|c| c.iter().fold(0u32, |acc, &x| (acc << 8) | u32::from(x)))
                    .collect();
                while v.last() == Some(&0) {
                    v.pop();
                }
                Big(v)
            }
            fn to_be(&self, len: usize) -> Vec<u8> {
                let mut raw: Vec<u8> = self.0.iter().rev().flat_map(|l| l.to_be_bytes()).collect();
                let lead = raw.iter().take_while(|&&b| b == 0).count();
                raw.drain(..lead);
                let mut out = vec![0u8; len.saturating_sub(raw.len())];
                out.extend(raw);
                out
            }
            fn bits(&self) -> usize {
                self.0.last().map_or(0, |&top| {
                    (self.0.len() - 1) * 32 + (32 - top.leading_zeros() as usize)
                })
            }
            fn bit(&self, i: usize) -> bool {
                self.0.get(i / 32).is_some_and(|l| (l >> (i % 32)) & 1 == 1)
            }
            fn cmp(&self, o: &Self) -> std::cmp::Ordering {
                self.0
                    .len()
                    .cmp(&o.0.len())
                    .then_with(|| self.0.iter().rev().cmp(o.0.iter().rev()))
            }
            fn trim(&mut self) {
                while self.0.last() == Some(&0) {
                    self.0.pop();
                }
            }
            fn mul(&self, b: &Self) -> Self {
                let mut out = vec![0u32; self.0.len() + b.0.len()];
                for (i, &x) in self.0.iter().enumerate() {
                    let mut carry = 0u64;
                    for (j, &y) in b.0.iter().enumerate() {
                        let t = u64::from(x) * u64::from(y) + u64::from(out[i + j]) + carry;
                        out[i + j] = t as u32;
                        carry = t >> 32;
                    }
                    out[i + b.0.len()] = carry as u32;
                }
                let mut r = Big(out);
                r.trim();
                r
            }
            fn sub_assign(&mut self, b: &Self) {
                let mut borrow = 0i64;
                for i in 0..self.0.len() {
                    let t =
                        i64::from(self.0[i]) - i64::from(b.0.get(i).copied().unwrap_or(0)) - borrow;
                    self.0[i] = t.rem_euclid(1 << 32) as u32;
                    borrow = i64::from(t < 0);
                }
                self.trim();
            }
            fn rem(&self, m: &Self) -> Self {
                let mut r = Big(Vec::new());
                for i in (0..self.bits()).rev() {
                    let mut carry = u32::from(self.bit(i));
                    for l in &mut r.0 {
                        let next = *l >> 31;
                        *l = (*l << 1) | carry;
                        carry = next;
                    }
                    if carry != 0 {
                        r.0.push(carry);
                    }
                    if r.cmp(m) != std::cmp::Ordering::Less {
                        r.sub_assign(m);
                    }
                }
                r
            }
            fn pow_mod(&self, e: &Self, m: &Self) -> Self {
                let mut res = Big(vec![1]).rem(m);
                let base = self.rem(m);
                for i in (0..e.bits()).rev() {
                    res = res.mul(&res).rem(m);
                    if e.bit(i) {
                        res = res.mul(&base).rem(m);
                    }
                }
                res
            }
        }
        let m = Big::from_be(msg_be);
        let d = Big::from_be(d_be);
        let n = Big::from_be(n_be);
        m.pow_mod(&d, &n).to_be(n_be.len())
    }

    fn der_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = content.len();
        if len < 128 {
            out.push(len as u8);
        } else if len < 256 {
            out.push(0x81);
            out.push(len as u8);
        } else {
            out.push(0x82);
            out.push((len >> 8) as u8);
            out.push((len & 0xff) as u8);
        }
        out.extend_from_slice(content);
        out
    }

    fn der_seq(parts: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = parts.iter().flat_map(|p| p.iter().copied()).collect();
        der_tlv(0x30, &body)
    }

    fn der_set(parts: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = parts.iter().flat_map(|p| p.iter().copied()).collect();
        der_tlv(0x31, &body)
    }

    fn der_int(be: &[u8]) -> Vec<u8> {
        let mut v = be.to_vec();
        if v.first().is_some_and(|&b| b & 0x80 != 0) {
            v.insert(0, 0x00);
        }
        der_tlv(0x02, &v)
    }

    /// Builds a detached PKCS #7 (`signedData`) DER signature over `signed_bytes`
    /// using SHA-256 and our 512-bit RSA keypair, with CN="Alice Signer", O="Acme Corp".
    fn build_pkcs7_detached(signed_bytes: &[u8]) -> Vec<u8> {
        let n_bytes = unhex(TEST_RSA_N_HEX);
        let d_bytes = unhex(TEST_RSA_D_HEX);

        // OIDs
        let oid_signed_data = der_tlv(0x06, &unhex("2a864886f70d010702")); // 1.2.840.113549.1.7.2
        let oid_data = der_tlv(0x06, &unhex("2a864886f70d010701")); // 1.2.840.113549.1.7.1
        let oid_sha256 = der_tlv(0x06, &unhex("608648016503040201")); // 2.16.840.1.101.3.4.2.1
        let oid_rsa_encryption = der_tlv(0x06, &unhex("2a864886f70d010101")); // 1.2.840.113549.1.1.1
        let oid_sha256_with_rsa = der_tlv(0x06, &unhex("2a864886f70d01010b")); // 1.2.840.113549.1.1.11
        let oid_content_type = der_tlv(0x06, &unhex("2a864886f70d010903")); // 1.2.840.113549.1.9.3
        let oid_message_digest = der_tlv(0x06, &unhex("2a864886f70d010904")); // 1.2.840.113549.1.9.4
        let oid_cn = der_tlv(0x06, &unhex("550403")); // 2.5.4.3
        let oid_org = der_tlv(0x06, &unhex("55040a")); // 2.5.4.10
        let der_null = vec![0x05, 0x00];

        // Distinguished Name: CN=Alice Signer, O=Acme Corp
        let rdn_cn = der_set(&[der_seq(&[oid_cn, der_tlv(0x0c, b"Alice Signer")])]);
        let rdn_org = der_set(&[der_seq(&[oid_org, der_tlv(0x0c, b"Acme Corp")])]);
        let name = der_seq(&[rdn_cn, rdn_org]);
        let serial = der_int(&[0x01]);

        // SubjectPublicKeyInfo
        let rsa_pub_seq = der_seq(&[der_int(&n_bytes), der_int(&[0x01, 0x00, 0x01])]);
        let mut bit_string_body = vec![0x00];
        bit_string_body.extend_from_slice(&rsa_pub_seq);
        let spki = der_seq(&[
            der_seq(&[oid_rsa_encryption, der_null.clone()]),
            der_tlv(0x03, &bit_string_body),
        ]);

        // Self-signed X.509 certificate container
        let validity = der_seq(&[
            der_tlv(0x17, b"260101000000Z"),
            der_tlv(0x17, b"360101000000Z"),
        ]);
        let sig_alg = der_seq(&[oid_sha256_with_rsa.clone(), der_null.clone()]);
        let tbs_cert = der_seq(&[
            der_tlv(0xa0, &der_int(&[0x02])),
            serial.clone(),
            sig_alg.clone(),
            name.clone(),
            validity,
            name.clone(),
            spki,
        ]);
        let cert = der_seq(&[tbs_cert, sig_alg.clone(), der_tlv(0x03, &[0x00, 0x01])]);

        // Signed attributes (contentType + messageDigest over `signed_bytes`)
        let data_digest = Sha256::digest(signed_bytes).to_vec();
        let attr_ct = der_seq(&[oid_content_type, der_set(std::slice::from_ref(&oid_data))]);
        let attr_md = der_seq(&[oid_message_digest, der_set(&[der_tlv(0x04, &data_digest)])]);
        let attrs_inner: Vec<u8> = [attr_ct, attr_md].concat();
        let signed_attrs_implicit = der_tlv(0xa0, &attrs_inner);
        let signed_attrs_as_set = der_tlv(0x31, &attrs_inner);

        // DigestInfo(SHA-256, SHA256(SET(signed_attrs))) -> PKCS#1 v1.5 pad -> RSA sign
        let attrs_digest = Sha256::digest(&signed_attrs_as_set).to_vec();
        let digest_info = der_seq(&[
            der_seq(&[oid_sha256.clone(), der_null.clone()]),
            der_tlv(0x04, &attrs_digest),
        ]);
        let k = n_bytes.len();
        let ff_len = k - 3 - digest_info.len();
        let mut em = Vec::with_capacity(k);
        em.push(0x00);
        em.push(0x01);
        em.resize(2 + ff_len, 0xff);
        em.push(0x00);
        em.extend_from_slice(&digest_info);
        let sig_bytes = rsa_raw_sign(&em, &d_bytes, &n_bytes);

        let signer_info = der_seq(&[
            der_int(&[0x01]),
            der_seq(&[name, serial]),
            der_seq(&[oid_sha256, der_null]),
            signed_attrs_implicit,
            sig_alg,
            der_tlv(0x04, &sig_bytes),
        ]);

        let signed_data = der_seq(&[
            der_int(&[0x01]),
            der_set(&[der_seq(&[der_tlv(0x06, &unhex("608648016503040201"))])]),
            der_seq(&[oid_data]),
            der_tlv(0xa0, &cert),
            der_set(&[signer_info]),
        ]);

        der_seq(&[oid_signed_data, der_tlv(0xa0, &signed_data)])
    }

    /// Assembles a valid 1-page signed PDF (Revision 1) with:
    /// - Page text `"Approved Total: $1,000.00"` at `(72, 700)`
    /// - AcroForm text field `"notes"` (initially `"Pending"`)
    /// - AcroForm signature widget `"approval_sig"` whose `/ByteRange` + PKCS #7
    ///   RSA signature covers the entire Revision 1 byte buffer.
    pub(crate) fn build_signed_invoice_rev1() -> Vec<u8> {
        let content_stream = b"BT /F1 14 Tf 72 700 Td (Approved Total: $1,000.00) Tj ET";
        // Reserve fixed-width ByteRange and hex placeholder so object offsets never shift.
        let hex_cap = 2048usize; // 1024 DER bytes max
        let br_placeholder = "[0 0000000000 0000000000 0000000000]";
        let hex_placeholder = "0".repeat(hex_cap);

        let obj1 = b"<</Type/Catalog/Pages 2 0 R/AcroForm<</Fields[6 0 R 7 0 R]/DA(/F1 10 Tf 0 g)/DR<</Font<</F1 5 0 R>>>>>>>>".to_vec();
        let obj2 = b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec();
        let obj3 = b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R/Annots[6 0 R 7 0 R]>>".to_vec();
        let mut obj4 = format!("<</Length {}>>\nstream\n", content_stream.len()).into_bytes();
        obj4.extend_from_slice(content_stream);
        obj4.extend_from_slice(b"\nendstream");
        let obj5 =
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>".to_vec();
        let obj6 = b"<</Type/Annot/Subtype/Widget/FT/Tx/T(notes)/V(Pending)/Rect[72 620 300 645]>>"
            .to_vec();
        let obj7 =
            b"<</Type/Annot/Subtype/Widget/FT/Sig/T(approval_sig)/V 8 0 R/Rect[72 560 260 600]>>"
                .to_vec();
        let obj8 = format!(
            "<</Type/Sig/Filter/Adobe.PPKLite/SubFilter/adbe.pkcs7.detached/ByteRange{br_placeholder}/Contents<{hex_placeholder}>>>"
        )
        .into_bytes();

        let objs = [obj1, obj2, obj3, obj4, obj5, obj6, obj7, obj8];
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::with_capacity(objs.len());
        for (i, body) in objs.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref_pos = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes(),
        );
        for ofs in &offsets {
            pdf.extend_from_slice(format!("{ofs:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref_pos}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );

        // Locate `/ByteRange[...]` and `/Contents<...>` inside object 8.
        let br_needle = format!("/ByteRange{br_placeholder}").into_bytes();
        let br_at = memchr::memmem::find(&pdf, &br_needle).expect("ByteRange placeholder");
        let br_val_start = br_at + b"/ByteRange".len();

        let contents_needle = b"/Contents<";
        let c_at = memchr::memmem::find(&pdf[br_at..], contents_needle).expect("Contents<") + br_at;
        let hole_start = c_at + b"/Contents".len(); // points to `<`
        let hole_end = hole_start + 1 + hex_cap + 1; // points right after `>`
        let tail_len = pdf.len() - hole_end;

        let actual_br = format!("[0 {hole_start:<10} {hole_end:<10} {tail_len:<10}]");
        assert_eq!(actual_br.len(), br_placeholder.len());
        pdf[br_val_start..br_val_start + br_placeholder.len()]
            .copy_from_slice(actual_br.as_bytes());

        let mut to_sign = Vec::with_capacity(hole_start + tail_len);
        to_sign.extend_from_slice(&pdf[..hole_start]);
        to_sign.extend_from_slice(&pdf[hole_end..]);

        let der_sig = build_pkcs7_detached(&to_sign);
        let sig_hex = hex_upper(&der_sig);
        assert!(
            sig_hex.len() <= hex_cap,
            "DER signature hex ({}) exceeds placeholder ({hex_cap})",
            sig_hex.len()
        );
        pdf[hole_start + 1..hole_start + 1 + sig_hex.len()].copy_from_slice(sig_hex.as_bytes());

        pdf
    }

    #[test]
    fn unsigned_incremental_history_tracks_line_diffs_chronologically() {
        let bytes = include_bytes!("../../../corpus/smoke/incremental-update.pdf");
        let doc = Document::open(bytes.to_vec()).unwrap();
        let report = inspect_history(&doc, &HistoryOptions::default()).unwrap();

        assert_eq!(report.revision_count, 2);
        assert!(!report.repaired);
        assert!(!report.tampered);

        let r1 = &report.revisions[0];
        assert_eq!(r1.revision, 1);
        assert_eq!(r1.page_count, 1);
        assert_eq!(r1.classification, CLASS_INITIAL);
        assert!(r1.delta_from_previous.is_none());

        let r2 = &report.revisions[1];
        assert_eq!(r2.revision, 2);
        assert_eq!(r2.byte_end, bytes.len());
        assert_ne!(r1.doc_id, r2.doc_id);
        assert_eq!(r2.classification, CLASS_UNSIGNED_EDIT);

        let d2 = r2.delta_from_previous.as_ref().unwrap();
        assert_eq!(
            d2.lines_removed,
            vec![RevisionLineChange {
                page: 1,
                text: "Revision 1".to_string()
            }]
        );
        assert_eq!(
            d2.lines_added,
            vec![RevisionLineChange {
                page: 1,
                text: "Revision 2".to_string()
            }]
        );
        assert!(d2.forensic_hazards.is_empty());
    }

    #[test]
    fn signed_pdf_clean_incremental_fill_then_visual_occlusion_and_content_tamper() {
        let inc_opts = WriteOptions {
            incremental: true,
            ..WriteOptions::default()
        };

        // --- Revision 1: Cryptographically signed base PDF ---
        let rev1_bytes = build_signed_invoice_rev1();
        let rev1_doc = Document::open(rev1_bytes.clone()).unwrap();
        assert_eq!(
            signature::summary(&rev1_doc),
            vec![("approval_sig".to_string(), "OK".to_string())]
        );

        // --- Revision 2: Clean incremental form fill (an annotation would be a hazard) ---
        let rev2_work = rev1_doc.fork_clean();
        fill::fill(
            &rev2_work,
            &[fill::Edit::Set {
                name: "notes".to_string(),
                value: "Reviewed by accounting".to_string(),
            }],
        )
        .unwrap();
        let rev2_bytes = write::save(&rev2_work, &inc_opts).unwrap();
        let rev2_doc = Document::open(rev2_bytes.clone()).unwrap();
        assert_eq!(rev2_doc.revisions(), 2);

        // --- Revision 3A: Post-signature visual occlusion tamper (white rectangle over signed total) ---
        let rev3_occlude_work = rev2_doc.fork_clean();
        {
            // Append an opaque white rectangle over `(72, 700)` in PDF space without any text ops.
            let whiteout = b"q 1 1 1 rg 65 690 260 30 re f Q\n".to_vec();
            let mut sdict = Dict::new();
            sdict.put(b"Length", Object::Int(whiteout.len() as i64));
            let s_num = rev3_occlude_work.add_stream(sdict, whiteout);
            crate::edit::edit_page(&rev3_occlude_work, 0, &mut |d| {
                d.put(
                    b"Contents",
                    Object::Array(Arc::new(vec![
                        Object::Ref(crate::object::ObjRef::new(4, 0)),
                        Object::Ref(crate::object::ObjRef::new(s_num, 0)),
                    ])),
                );
            })
            .unwrap();
        }
        let rev3_occlude_bytes = write::save(&rev3_occlude_work, &inc_opts).unwrap();
        let rev3_occlude_doc = Document::open(rev3_occlude_bytes).unwrap();
        assert_eq!(rev3_occlude_doc.revisions(), 3);

        let report_occlude =
            inspect_history(&rev3_occlude_doc, &HistoryOptions::default()).unwrap();
        assert_eq!(report_occlude.revision_count, 3);
        assert!(report_occlude.tampered);

        // Check Rev 1 snapshot
        let s1 = &report_occlude.revisions[0];
        assert_eq!(s1.revision, 1);
        assert_eq!(s1.byte_end, rev1_bytes.len());
        assert_eq!(s1.classification, CLASS_INITIAL);
        assert_eq!(s1.signatures.len(), 1);
        assert!(s1.signatures[0].valid);
        assert_eq!(s1.signatures[0].status, "OK");
        assert!(s1.signatures[0].covers_whole_revision);
        assert_eq!(
            s1.signatures[0].signatory.as_deref(),
            Some("cn=Alice Signer, o=Acme Corp, ou=, email=, c=")
        );

        // Check Rev 2 snapshot: Rev 1 signature stays "OK", classified as clean incremental fill!
        let s2 = &report_occlude.revisions[1];
        assert_eq!(s2.revision, 2);
        assert_eq!(s2.byte_end, rev2_bytes.len());
        assert_eq!(s2.classification, CLASS_CLEAN_INCREMENTAL_FILL);
        assert!(s2.signatures[0].valid);
        assert_eq!(s2.signatures[0].status, "OK");
        assert!(!s2.signatures[0].covers_whole_revision);
        let d2 = s2.delta_from_previous.as_ref().unwrap();
        assert!(d2.forensic_hazards.is_empty());
        assert_eq!(
            d2.fields_changed,
            vec![RevisionFieldChange {
                name: "notes".to_string(),
                old_value: Some("Pending".to_string()),
                new_value: Some("Reviewed by accounting".to_string()),
            }]
        );
        assert_eq!(d2.annots_added, 0);

        // Check Rev 3A snapshot: White rectangle occlusion caught despite zero text operator edits!
        let s3a = &report_occlude.revisions[2];
        assert_eq!(s3a.revision, 3);
        assert_eq!(s3a.classification, CLASS_TAMPERED_AFTER_SIGNATURE);
        let d3a = s3a.delta_from_previous.as_ref().unwrap();
        assert_eq!(
            d3a.newly_occluded_spans,
            vec!["Approved Total: $1,000.00".to_string()]
        );
        assert!(
            d3a.forensic_hazards
                .contains(&TAMPER_POST_SIGNATURE_VISUAL_OCCLUSION.to_string())
        );

        // --- Revision 3B: Post-signature content-stream text alteration ---
        let rev3_text_work = rev2_doc.fork_clean();
        let (rev3_text_bytes, _) = patch::patch_and_verify(
            &rev3_text_work,
            &[PatchSpec {
                target_id: "p1.b1.l1.s1".to_string(),
                new_text: "Approved Total: $9,000.00".to_string(),
            }],
            &PatchOptions::default(),
            &inc_opts,
            None,
        )
        .unwrap();
        let rev3_text_doc = Document::open(rev3_text_bytes).unwrap();
        let report_text = inspect_history(&rev3_text_doc, &HistoryOptions::default()).unwrap();
        assert_eq!(report_text.revision_count, 3);
        assert!(report_text.tampered);

        let s3b = &report_text.revisions[2];
        assert_eq!(s3b.classification, CLASS_TAMPERED_AFTER_SIGNATURE);
        let d3b = s3b.delta_from_previous.as_ref().unwrap();
        assert!(
            d3b.forensic_hazards
                .contains(&TAMPER_POST_SIGNATURE_CONTENT_EDIT.to_string())
        );
        assert_eq!(
            d3b.lines_added,
            vec![RevisionLineChange {
                page: 1,
                text: "Approved Total: $9,000.00".to_string(),
            }]
        );
    }

    /// Classification and hazards of revision 2, made from the signed
    /// invoice by `edit` and saved incrementally.
    fn signed_then(edit: impl FnOnce(&Document)) -> (String, Vec<String>) {
        let rev1_doc = Document::open(build_signed_invoice_rev1()).unwrap();
        let work = rev1_doc.fork_clean();
        edit(&work);
        let inc_opts = WriteOptions {
            incremental: true,
            ..WriteOptions::default()
        };
        let rev2 = Document::open(write::save(&work, &inc_opts).unwrap()).unwrap();
        let report = inspect_history(&rev2, &HistoryOptions { run_attest: true }).unwrap();
        assert_eq!(report.revision_count, 2);
        let s2 = &report.revisions[1];
        assert!(
            s2.signatures[0].valid,
            "the edit is not meant to break the digest"
        );
        let hazards = s2
            .delta_from_previous
            .as_ref()
            .unwrap()
            .forensic_hazards
            .clone();
        assert_eq!(
            report.tampered,
            s2.classification == CLASS_TAMPERED_AFTER_SIGNATURE
        );
        (s2.classification.clone(), hazards)
    }

    #[test]
    fn edits_after_a_signature_that_change_no_text_are_still_hazards() {
        // A FreeText annotation stating a new amount.
        let (class, hazards) = signed_then(|doc| {
            let mut ed = Editor::new(doc);
            ed.load_page(0).unwrap();
            let n = ed.create(0, "FreeText").unwrap();
            ed.set_contents(n, "Approved Total: $9,000.00");
            ed.update();
        });
        assert_eq!(class, CLASS_TAMPERED_AFTER_SIGNATURE);
        assert!(hazards.contains(&TAMPER_POST_SIGNATURE_ANNOTATION_EDIT.to_string()));

        // The page's font swapped for another.
        let (class, hazards) = signed_then(|doc| {
            doc.alter_object(
                5,
                Object::Dict(Arc::new({
                    let mut d = Dict::new();
                    d.put(b"Type", Object::name(b"Font"));
                    d.put(b"Subtype", Object::name(b"Type1"));
                    d.put(b"BaseFont", Object::name(b"Courier"));
                    d.put(b"Encoding", Object::name(b"WinAnsiEncoding"));
                    d
                })),
            );
        });
        assert_eq!(class, CLASS_TAMPERED_AFTER_SIGNATURE);
        assert!(hazards.contains(&TAMPER_POST_SIGNATURE_CONTENT_EDIT.to_string()));

        // Vector content added away from any text.
        let (class, hazards) = signed_then(|doc| {
            let data = b"BT /F1 14 Tf 72 700 Td (Approved Total: $1,000.00) Tj ET 0 0 1 rg 400 100 50 50 re f".to_vec();
            let mut d = Dict::new();
            d.put(b"Length", Object::Int(data.len() as i64));
            doc.replace_stream(4, d, data);
        });
        assert_eq!(class, CLASS_TAMPERED_AFTER_SIGNATURE);
        assert!(hazards.contains(&TAMPER_POST_SIGNATURE_CONTENT_EDIT.to_string()));

        // Filling the existing field stays a clean fill.
        let (class, hazards) = signed_then(|doc| {
            fill::fill(
                doc,
                &[fill::Edit::Set {
                    name: "notes".to_string(),
                    value: "Reviewed".to_string(),
                }],
            )
            .unwrap();
        });
        assert_eq!(class, CLASS_CLEAN_INCREMENTAL_FILL, "{hazards:?}");
    }

    #[test]
    fn broken_signature_digest_in_later_revision_is_detected() {
        let inc_opts = WriteOptions {
            incremental: true,
            ..WriteOptions::default()
        };
        let rev1_bytes = build_signed_invoice_rev1();
        let rev1_doc = Document::open(rev1_bytes).unwrap();

        // Incrementally alter the signature field's /V dictionary to point to a corrupted sig dict.
        let rev2_work = rev1_doc.fork_clean();
        let bad_sig_num = rev2_work.add_object(Object::Dict(Arc::new({
            let mut d = Dict::new();
            d.put(b"Type", Object::name(b"Sig"));
            d.put(
                b"ByteRange",
                Object::Array(Arc::new(vec![
                    Object::Int(0),
                    Object::Int(10),
                    Object::Int(20),
                    Object::Int(10),
                ])),
            );
            d.put(b"Contents", Object::String(Arc::from(&b"00"[..])));
            d
        })));
        let mut sig_widget = rev2_work.object(7).unwrap().as_dict().unwrap().clone();
        sig_widget.put(
            b"V",
            Object::Ref(crate::object::ObjRef::new(bad_sig_num, 0)),
        );
        rev2_work.alter_object(7, Object::Dict(Arc::new(sig_widget)));

        let rev2_bytes = write::save(&rev2_work, &inc_opts).unwrap();
        let rev2_doc = Document::open(rev2_bytes).unwrap();
        let report = inspect_history(&rev2_doc, &HistoryOptions { run_attest: false }).unwrap();

        assert_eq!(report.revision_count, 2);
        assert!(report.tampered);
        assert_eq!(report.revisions[0].classification, CLASS_INITIAL);
        assert_eq!(
            report.revisions[1].classification,
            CLASS_TAMPERED_AFTER_SIGNATURE
        );
        let d2 = report.revisions[1].delta_from_previous.as_ref().unwrap();
        assert!(
            d2.forensic_hazards
                .contains(&TAMPER_SIGNATURE_BROKEN.to_string())
        );
    }
}
