// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Zero-token deterministic claim and citation verifier over PDF documents.
//!
//! [`audit_claims`] resolves cited content IDs (`p3`, `p3.b2`, `p3.b2.l4`,
//! `p3.b2.l4.s1`, `p3.b2.l4.s1:0-5`) or 1-based page numbers, checks exact or
//! whitespace/ligature-normalized quote fidelity, locates quotes moved to
//! other pages/spans, detects five epistemic hazards (vector/character
//! strikethrough, invisible text, `/ActualText` spoofing, omitted superscript
//! footnotes, and post-signature incremental edits), and optionally renders a
//! highlighted crop PNG as visual evidence.

use serde::{Deserialize, Serialize};

use crate::annot;
use crate::colorspace::ColorSpace;
use crate::content::Store;
use crate::device::{ColorParams, Device};
use crate::document::Document;
use crate::draw::{self, DrawDevice, Pix};
use crate::extract;
use crate::form;
use crate::geometry::{IRect, Matrix, Rect};
use crate::page_run::{self, Parts};
use crate::path::{Path, StrokeState};
use crate::search;
use crate::signature;
use crate::stext::{
    self, Block, Options, StextChar, StextLine, StextPage, TextBlock, char_flags, flags, rune_char,
};

/// Stable hazard code: horizontal vector stroke, StrikeOut annotation, or character strikeout flag crosses cited text.
pub const HAZARD_STRUCK_THROUGH: &str = "hazard.struck_through";
/// Stable hazard code: cited characters are neither filled nor stroked (`3 Tr`) or have zero alpha.
pub const HAZARD_INVISIBLE_TEXT: &str = "hazard.invisible_text";
/// Stable hazard code: `/ActualText` replacement differs from underlying glyph characters.
pub const HAZARD_ACTUAL_TEXT_MISMATCH: &str = "hazard.actual_text_mismatch";
/// Stable hazard code: adjacent/trailing superscript footnote marker points to a footnote omitted from `quote`.
pub const HAZARD_UNREFERENCED_FOOTNOTE: &str = "hazard.unreferenced_footnote";
/// Stable hazard code: document contains unsigned modifications after a digital signature.
pub const HAZARD_MODIFIED_AFTER_SIGNING: &str = "hazard.modified_after_signing";

/// Maximum pixel count for a single rendered evidence crop.
const MAX_EVIDENCE_PIXELS: u64 = 16_000_000;

/// Single claim and citation submitted for deterministic verification.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ClaimInput {
    pub claim_id: Option<String>,
    pub doc_id: Option<String>,
    pub id: Option<String>,
    pub page: Option<i32>,
    pub quote: String,
}

/// Outcome category for an audited claim.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditStatus {
    Verified,
    VerifiedWithHazards,
    WrongLocation,
    Mismatch,
    NotFound,
    StaleDocId,
}

/// Detailed deterministic audit result for a single [`ClaimInput`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaimAudit {
    pub index: usize,
    pub claim_id: Option<String>,
    pub status: AuditStatus,
    pub verified: bool,
    pub cited_id: Option<String>,
    pub resolved_page: Option<i32>,
    pub resolved_bbox: Option<[f32; 4]>,
    pub expected_quote: String,
    pub actual_text: Option<String>,
    pub found_elsewhere: Vec<String>,
    pub hazards: Vec<String>,
    pub footnote_text: Option<String>,
    #[serde(skip_serializing, default)]
    pub evidence_png: Option<Vec<u8>>,
}

/// Aggregate summary across all audited claims.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditSummary {
    pub total: usize,
    pub verified_clean: usize,
    pub verified_with_hazards: usize,
    pub failed: usize,
    pub all_verified: bool,
    pub results: Vec<ClaimAudit>,
}

/// Audit a batch of claims against `doc` and its expected `doc_id_str` (`b3:...`).
///
/// `ClaimAudit::verified` is `true` strictly for [`AuditStatus::Verified`] (zero hazards).
pub fn audit_claims(
    doc: &Document,
    doc_id_str: &str,
    claims: &[ClaimInput],
    render_evidence: bool,
    evidence_dpi: f32,
) -> Result<AuditSummary, String> {
    audit_claims_with_options(
        doc,
        doc_id_str,
        claims,
        render_evidence,
        evidence_dpi,
        false,
    )
}

/// Audit a batch of claims with explicit control over whether [`AuditStatus::VerifiedWithHazards`]
/// counts as `verified: true` (`allow_hazards`).
pub fn audit_claims_with_options(
    doc: &Document,
    doc_id_str: &str,
    claims: &[ClaimInput],
    render_evidence: bool,
    evidence_dpi: f32,
    allow_hazards: bool,
) -> Result<AuditSummary, String> {
    let page_count = doc.page_count().map_err(|e| e.0)?;
    let modified_after_signing = check_modified_after_signing(doc, page_count);
    let mut cache = PageCache::new(page_count.max(0) as usize);

    let mut results = Vec::with_capacity(claims.len());
    let mut verified_clean = 0usize;
    let mut verified_with_hazards = 0usize;
    let mut failed = 0usize;

    for (index, claim) in claims.iter().enumerate() {
        let audit = audit_single_claim(
            doc,
            doc_id_str,
            page_count,
            modified_after_signing,
            &mut cache,
            index,
            claim,
            render_evidence,
            evidence_dpi,
            allow_hazards,
        )?;
        match audit.status {
            AuditStatus::Verified => verified_clean += 1,
            AuditStatus::VerifiedWithHazards => {
                verified_with_hazards += 1;
                if !allow_hazards {
                    failed += 1;
                }
            }
            AuditStatus::WrongLocation
            | AuditStatus::Mismatch
            | AuditStatus::NotFound
            | AuditStatus::StaleDocId => {
                failed += 1;
            }
        }
        results.push(audit);
    }

    let total = claims.len();
    let all_verified = results.iter().all(|r| r.verified);

    Ok(AuditSummary {
        total,
        verified_clean,
        verified_with_hazards,
        failed,
        all_verified,
        results,
    })
}

struct PageCache {
    primary: Vec<Option<StextPage>>,
    styled: Vec<Option<StextPage>>,
    raw_glyphs: Vec<Option<StextPage>>,
    store: Store,
}

impl PageCache {
    fn new(pages: usize) -> Self {
        Self {
            primary: std::iter::repeat_with(|| None).take(pages).collect(),
            styled: std::iter::repeat_with(|| None).take(pages).collect(),
            raw_glyphs: std::iter::repeat_with(|| None).take(pages).collect(),
            store: Store::default(),
        }
    }

    fn primary_page(&mut self, doc: &Document, page_num: i32) -> Result<&StextPage, String> {
        let idx = page_num
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .ok_or_else(|| format!("page {page_num} out of range"))?;
        if idx >= self.primary.len() {
            return Err(format!("page {page_num} out of range"));
        }
        if self.primary[idx].is_none() {
            let page_obj = doc.page(page_num - 1).map_err(|e| e.0)?;
            let prev = std::mem::take(&mut self.store);
            let (res, next) = stext::extract_page(doc, &page_obj, Options::default(), prev);
            self.store = next;
            self.primary[idx] = Some(res?);
        }
        self.primary[idx]
            .as_ref()
            .ok_or_else(|| format!("failed to cache page {page_num}"))
    }

    fn styled_page(&mut self, doc: &Document, page_num: i32) -> Result<&StextPage, String> {
        let idx = page_num
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .ok_or_else(|| format!("page {page_num} out of range"))?;
        if idx >= self.styled.len() {
            return Err(format!("page {page_num} out of range"));
        }
        if self.styled[idx].is_none() {
            let page_obj = doc.page(page_num - 1).map_err(|e| e.0)?;
            let opts = Options {
                flags: flags::CLIP | flags::COLLECT_VECTORS | flags::COLLECT_STYLES,
                clip: Rect::INFINITE,
            };
            let prev = std::mem::take(&mut self.store);
            let (res, next) = stext::extract_page(doc, &page_obj, opts, prev);
            self.store = next;
            self.styled[idx] = Some(res?);
        }
        self.styled[idx]
            .as_ref()
            .ok_or_else(|| format!("failed to cache styled page {page_num}"))
    }

    fn raw_glyphs_page(&mut self, doc: &Document, page_num: i32) -> Result<&StextPage, String> {
        let idx = page_num
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .ok_or_else(|| format!("page {page_num} out of range"))?;
        if idx >= self.raw_glyphs.len() {
            return Err(format!("page {page_num} out of range"));
        }
        if self.raw_glyphs[idx].is_none() {
            let page_obj = doc.page(page_num - 1).map_err(|e| e.0)?;
            let opts = Options {
                flags: flags::CLIP | flags::IGNORE_ACTUALTEXT,
                clip: Rect::INFINITE,
            };
            let prev = std::mem::take(&mut self.store);
            let (res, next) = stext::extract_page(doc, &page_obj, opts, prev);
            self.store = next;
            self.raw_glyphs[idx] = Some(res?);
        }
        self.raw_glyphs[idx]
            .as_ref()
            .ok_or_else(|| format!("failed to cache raw-glyph page {page_num}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedContentId {
    page: i32,
    blocks: Vec<usize>,
    line: Option<usize>,
    span: Option<usize>,
    slice: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetGranularity {
    Page,
    Block,
    Line,
    Span,
    Slice,
}

#[derive(Debug, Clone)]
struct IndexedChar {
    ch: StextChar,
    span_id: String,
    line_break_before: bool,
}

#[derive(Debug, Clone)]
struct ResolvedTarget {
    granularity: TargetGranularity,
    actual_text: String,
    bbox: Option<Rect>,
    chars: Vec<IndexedChar>,
}

#[derive(Debug, Clone)]
struct MatchedQuote {
    page: i32,
    bbox: Rect,
    actual_text: String,
    matched_chars: Vec<StextChar>,
}

#[allow(clippy::too_many_arguments)]
fn audit_single_claim(
    doc: &Document,
    doc_id_str: &str,
    page_count: i32,
    modified_after_signing: bool,
    cache: &mut PageCache,
    index: usize,
    claim: &ClaimInput,
    render_evidence: bool,
    evidence_dpi: f32,
    allow_hazards: bool,
) -> Result<ClaimAudit, String> {
    if let Some(ref claim_doc_id) = claim.doc_id
        && !doc_id_str.is_empty()
        && claim_doc_id != doc_id_str
    {
        return Ok(ClaimAudit {
            index,
            claim_id: claim.claim_id.clone(),
            status: AuditStatus::StaleDocId,
            verified: false,
            cited_id: claim.id.clone(),
            resolved_page: None,
            resolved_bbox: None,
            expected_quote: claim.quote.clone(),
            actual_text: None,
            found_elsewhere: Vec::new(),
            hazards: Vec::new(),
            footnote_text: None,
            evidence_png: None,
        });
    }

    let quote_norm = normalize_text(&claim.quote);
    if quote_norm.is_empty() || page_count <= 0 {
        return Ok(ClaimAudit {
            index,
            claim_id: claim.claim_id.clone(),
            status: AuditStatus::NotFound,
            verified: false,
            cited_id: claim.id.clone(),
            resolved_page: None,
            resolved_bbox: None,
            expected_quote: claim.quote.clone(),
            actual_text: None,
            found_elsewhere: Vec::new(),
            hazards: Vec::new(),
            footnote_text: None,
            evidence_png: None,
        });
    }

    // Case 1: Specific content ID cited (`p3`, `p3.b2`, `p3.b2.l4`, `p3.b2.l4.s1`, `p3.b2.l4.s1:0-5`).
    if let Some(ref cited_id_str) = claim.id {
        let Some(parsed_id) = parse_content_id(cited_id_str) else {
            let elsewhere = search_document_elsewhere(doc, page_count, cache, &claim.quote, None)?;
            let status = if elsewhere.is_empty() {
                AuditStatus::NotFound
            } else {
                AuditStatus::WrongLocation
            };
            return Ok(ClaimAudit {
                index,
                claim_id: claim.claim_id.clone(),
                status,
                verified: false,
                cited_id: Some(cited_id_str.clone()),
                resolved_page: None,
                resolved_bbox: None,
                expected_quote: claim.quote.clone(),
                actual_text: None,
                found_elsewhere: elsewhere,
                hazards: Vec::new(),
                footnote_text: None,
                evidence_png: None,
            });
        };

        // Contradictory explicit `page` alongside `id` (e.g. id="p1.b1", page=Some(2)).
        if let Some(explicit_page) = claim.page
            && explicit_page != parsed_id.page
        {
            let elsewhere = search_document_elsewhere(doc, page_count, cache, &claim.quote, None)?;
            let status = if elsewhere.is_empty() {
                AuditStatus::NotFound
            } else {
                AuditStatus::WrongLocation
            };
            return Ok(ClaimAudit {
                index,
                claim_id: claim.claim_id.clone(),
                status,
                verified: false,
                cited_id: Some(cited_id_str.clone()),
                resolved_page: None,
                resolved_bbox: None,
                expected_quote: claim.quote.clone(),
                actual_text: None,
                found_elsewhere: elsewhere,
                hazards: Vec::new(),
                footnote_text: None,
                evidence_png: None,
            });
        }

        if parsed_id.page < 1 || parsed_id.page > page_count {
            let elsewhere = search_document_elsewhere(doc, page_count, cache, &claim.quote, None)?;
            let status = if elsewhere.is_empty() {
                AuditStatus::NotFound
            } else {
                AuditStatus::WrongLocation
            };
            return Ok(ClaimAudit {
                index,
                claim_id: claim.claim_id.clone(),
                status,
                verified: false,
                cited_id: Some(cited_id_str.clone()),
                resolved_page: None,
                resolved_bbox: None,
                expected_quote: claim.quote.clone(),
                actual_text: None,
                found_elsewhere: elsewhere,
                hazards: Vec::new(),
                footnote_text: None,
                evidence_png: None,
            });
        }

        let target_opt = {
            let stext_page = cache.primary_page(doc, parsed_id.page)?;
            resolve_target(stext_page, &parsed_id)
        };

        let Some(target) = target_opt else {
            let elsewhere = search_document_elsewhere(doc, page_count, cache, &claim.quote, None)?;
            let status = if elsewhere.is_empty() {
                AuditStatus::NotFound
            } else {
                AuditStatus::WrongLocation
            };
            return Ok(ClaimAudit {
                index,
                claim_id: claim.claim_id.clone(),
                status,
                verified: false,
                cited_id: Some(cited_id_str.clone()),
                resolved_page: None,
                resolved_bbox: None,
                expected_quote: claim.quote.clone(),
                actual_text: None,
                found_elsewhere: elsewhere,
                hazards: Vec::new(),
                footnote_text: None,
                evidence_png: None,
            });
        };

        if let Some(matched) = match_quote_in_target(&target, parsed_id.page, &claim.quote) {
            return finalize_matched_claim(
                doc,
                cache,
                modified_after_signing,
                index,
                claim,
                Some(cited_id_str.clone()),
                matched,
                render_evidence,
                evidence_dpi,
                allow_hazards,
            );
        }

        // Target resolved, but its text did not match `claim.quote`.
        // Check whether `claim.quote` actually lives elsewhere in the document.
        let elsewhere = search_document_elsewhere(
            doc,
            page_count,
            cache,
            &claim.quote,
            Some(cited_id_str.as_str()),
        )?;
        let resolved_bbox = target.bbox.map(rect_to_array);
        let status = if elsewhere.is_empty() {
            AuditStatus::Mismatch
        } else {
            AuditStatus::WrongLocation
        };
        let evidence_png = if render_evidence && let Some(b) = target.bbox {
            render_evidence_crop(doc, parsed_id.page, b, false, evidence_dpi).ok()
        } else {
            None
        };

        return Ok(ClaimAudit {
            index,
            claim_id: claim.claim_id.clone(),
            status,
            verified: false,
            cited_id: Some(cited_id_str.clone()),
            resolved_page: Some(parsed_id.page),
            resolved_bbox,
            expected_quote: claim.quote.clone(),
            actual_text: Some(target.actual_text),
            found_elsewhere: elsewhere,
            hazards: Vec::new(),
            footnote_text: None,
            evidence_png,
        });
    }

    // Case 2: Page-level citation (`id` is None, `page` is Some(p)).
    if let Some(cited_page) = claim.page {
        if cited_page >= 1 && cited_page <= page_count {
            let matched_on_page = {
                let stext_page = cache.primary_page(doc, cited_page)?;
                find_quote_on_page(stext_page, cited_page, &claim.quote)
            };
            if let Some((matched, _span_id)) = matched_on_page {
                return finalize_matched_claim(
                    doc,
                    cache,
                    modified_after_signing,
                    index,
                    claim,
                    None,
                    matched,
                    render_evidence,
                    evidence_dpi,
                    allow_hazards,
                );
            }
        }

        let elsewhere = search_document_elsewhere(doc, page_count, cache, &claim.quote, None)?;
        let status = if elsewhere.is_empty() {
            AuditStatus::NotFound
        } else {
            AuditStatus::WrongLocation
        };
        return Ok(ClaimAudit {
            index,
            claim_id: claim.claim_id.clone(),
            status,
            verified: false,
            cited_id: None,
            resolved_page: None,
            resolved_bbox: None,
            expected_quote: claim.quote.clone(),
            actual_text: None,
            found_elsewhere: elsewhere,
            hazards: Vec::new(),
            footnote_text: None,
            evidence_png: None,
        });
    }

    // Case 3: Unanchored claim (`id` is None and `page` is None) -> search all pages in order.
    for p in 1..=page_count {
        let found = {
            let stext_page = cache.primary_page(doc, p)?;
            find_quote_on_page(stext_page, p, &claim.quote)
        };
        if let Some((matched, span_id)) = found {
            return finalize_matched_claim(
                doc,
                cache,
                modified_after_signing,
                index,
                claim,
                Some(span_id),
                matched,
                render_evidence,
                evidence_dpi,
                allow_hazards,
            );
        }
    }

    Ok(ClaimAudit {
        index,
        claim_id: claim.claim_id.clone(),
        status: AuditStatus::NotFound,
        verified: false,
        cited_id: None,
        resolved_page: None,
        resolved_bbox: None,
        expected_quote: claim.quote.clone(),
        actual_text: None,
        found_elsewhere: Vec::new(),
        hazards: Vec::new(),
        footnote_text: None,
        evidence_png: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn finalize_matched_claim(
    doc: &Document,
    cache: &mut PageCache,
    modified_after_signing: bool,
    index: usize,
    claim: &ClaimInput,
    cited_id: Option<String>,
    matched: MatchedQuote,
    render_evidence: bool,
    evidence_dpi: f32,
    allow_hazards: bool,
) -> Result<ClaimAudit, String> {
    let (hazards, footnote_text) =
        detect_hazards(doc, cache, modified_after_signing, &matched, &claim.quote)?;

    let status = if hazards.is_empty() {
        AuditStatus::Verified
    } else {
        AuditStatus::VerifiedWithHazards
    };
    let verified = match status {
        AuditStatus::Verified => true,
        AuditStatus::VerifiedWithHazards => allow_hazards,
        _ => false,
    };

    let evidence_png = if render_evidence {
        render_evidence_crop(
            doc,
            matched.page,
            matched.bbox,
            status == AuditStatus::Verified,
            evidence_dpi,
        )
        .ok()
    } else {
        None
    };

    Ok(ClaimAudit {
        index,
        claim_id: claim.claim_id.clone(),
        status,
        verified,
        cited_id,
        resolved_page: Some(matched.page),
        resolved_bbox: Some(rect_to_array(matched.bbox)),
        expected_quote: claim.quote.clone(),
        actual_text: Some(matched.actual_text),
        found_elsewhere: Vec::new(),
        hazards,
        footnote_text,
        evidence_png,
    })
}

fn check_modified_after_signing(doc: &Document, page_count: i32) -> bool {
    for p in 0..page_count {
        let Ok(page_obj) = doc.page(p) else {
            continue;
        };
        let (_, widgets) = annot::page_annots(doc, &page_obj);
        for w in &widgets {
            if form::is_signed(doc, w)
                && (signature::changed_since_signing(doc, w).unwrap_or(false)
                    || signature::check_digest(doc, w).ok() == Some(signature::DIGEST_FAILURE))
            {
                return true;
            }
        }
    }
    false
}

/// Parse content IDs:
/// - `p3`
/// - `p3.b2` (and nested `p3.b2.b1`)
/// - `p3.b2.l4`
/// - `p3.b2.l4.s1`
/// - `p3.b2.l4.s1:0-5`
fn parse_content_id(raw: &str) -> Option<ParsedContentId> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return None;
    }

    let (id_part, slice) = if let Some((left, right)) = trimmed.split_once(':') {
        let (a_str, b_str) = right.split_once('-')?;
        let a = a_str.parse::<usize>().ok()?;
        let b = b_str.parse::<usize>().ok()?;
        if a >= b {
            return None;
        }
        (left, Some((a, b)))
    } else {
        (trimmed, None)
    };

    let mut parts = id_part.split('.');
    let page_seg = parts.next()?;
    let page_num_str = page_seg
        .strip_prefix('p')
        .or_else(|| page_seg.strip_prefix('P'))?;
    let page = page_num_str.parse::<i32>().ok()?;
    if page < 1 {
        return None;
    }

    let mut blocks: Vec<usize> = Vec::new();
    let mut line: Option<usize> = None;
    let mut span: Option<usize> = None;

    for seg in parts {
        if let Some(num) = seg.strip_prefix('b').or_else(|| seg.strip_prefix('B')) {
            if line.is_some() || span.is_some() {
                return None;
            }
            let b_idx = num.parse::<usize>().ok()?.checked_sub(1)?;
            blocks.push(b_idx);
        } else if let Some(num) = seg.strip_prefix('l').or_else(|| seg.strip_prefix('L')) {
            if blocks.is_empty() || line.is_some() || span.is_some() {
                return None;
            }
            let l_idx = num.parse::<usize>().ok()?.checked_sub(1)?;
            line = Some(l_idx);
        } else {
            let num = seg.strip_prefix('s').or_else(|| seg.strip_prefix('S'))?;
            if line.is_none() || span.is_some() {
                return None;
            }
            let s_idx = num.parse::<usize>().ok()?.checked_sub(1)?;
            span = Some(s_idx);
        }
    }

    if slice.is_some() && span.is_none() {
        return None;
    }

    Some(ParsedContentId {
        page,
        blocks,
        line,
        span,
        slice,
    })
}

fn resolve_target(page: &StextPage, parsed: &ParsedContentId) -> Option<ResolvedTarget> {
    if parsed.blocks.is_empty() {
        let mut chars = Vec::new();
        let prefix = format!("p{}", parsed.page);
        collect_blocks_indexed(&page.blocks, &prefix, &mut chars);
        let actual_text = extract_page_text(&page.blocks);
        let bbox = indexed_chars_bbox(&chars);
        return Some(ResolvedTarget {
            granularity: TargetGranularity::Page,
            actual_text,
            bbox,
            chars,
        });
    }

    let mut cur_blocks = &page.blocks[..];
    let mut block_prefix = format!("p{}", parsed.page);
    let mut target_block: Option<&Block> = None;

    for (depth, &b_idx) in parsed.blocks.iter().enumerate() {
        let b = cur_blocks.get(b_idx)?;
        block_prefix = format!("{block_prefix}.b{}", b_idx + 1);
        if depth + 1 == parsed.blocks.len() {
            target_block = Some(b);
        } else {
            match b {
                Block::Struct(sb) => cur_blocks = &sb.blocks,
                _ => return None,
            }
        }
    }

    let block = target_block?;
    let Some(l_idx) = parsed.line else {
        let mut chars = Vec::new();
        let actual_text = match block {
            Block::Text(tb) => {
                collect_text_block_indexed(tb, &block_prefix, &mut chars);
                extract::block_text(tb)
            }
            Block::Struct(sb) => {
                collect_blocks_indexed(&sb.blocks, &block_prefix, &mut chars);
                extract_page_text(&sb.blocks)
            }
            Block::Image(_) | Block::Vector(_) | Block::Grid(_) => return None,
        };
        let bbox = indexed_chars_bbox(&chars);
        return Some(ResolvedTarget {
            granularity: TargetGranularity::Block,
            actual_text,
            bbox,
            chars,
        });
    };

    let Block::Text(tb) = block else {
        return None;
    };
    let line = tb.lines.get(l_idx)?;
    let line_id = format!("{block_prefix}.l{}", l_idx + 1);
    let spans = extract::spans(line);

    let Some(s_idx) = parsed.span else {
        let mut chars = Vec::with_capacity(line.chars.len());
        for (si, sp) in spans.iter().enumerate() {
            let sp_id = format!("{line_id}.s{}", si + 1);
            if let Some(slice) = line.chars.get(sp.start..sp.end) {
                for ch in slice {
                    chars.push(IndexedChar {
                        ch: ch.clone(),
                        span_id: sp_id.clone(),
                        line_break_before: false,
                    });
                }
            }
        }
        let actual_text = extract::chars_text(&line.chars);
        return Some(ResolvedTarget {
            granularity: TargetGranularity::Line,
            actual_text,
            bbox: Some(line.bbox),
            chars,
        });
    };

    let sp = spans.get(s_idx)?;
    let span_id = format!("{line_id}.s{}", s_idx + 1);
    let span_chars = line.chars.get(sp.start..sp.end)?;

    if let Some((a, b)) = parsed.slice {
        if a >= span_chars.len() {
            return None;
        }
        let end = b.min(span_chars.len());
        let sub = span_chars.get(a..end)?;
        let chars: Vec<IndexedChar> = sub
            .iter()
            .map(|ch| IndexedChar {
                ch: ch.clone(),
                span_id: format!("{span_id}:{a}-{end}"),
                line_break_before: false,
            })
            .collect();
        let bbox = indexed_chars_bbox(&chars);
        let actual_text = extract::chars_text(sub);
        return Some(ResolvedTarget {
            granularity: TargetGranularity::Slice,
            actual_text,
            bbox,
            chars,
        });
    }

    let chars: Vec<IndexedChar> = span_chars
        .iter()
        .map(|ch| IndexedChar {
            ch: ch.clone(),
            span_id: span_id.clone(),
            line_break_before: false,
        })
        .collect();
    let actual_text = extract::chars_text(span_chars);

    Some(ResolvedTarget {
        granularity: TargetGranularity::Span,
        actual_text,
        bbox: Some(sp.bbox),
        chars,
    })
}

fn collect_blocks_indexed(blocks: &[Block], prefix: &str, out: &mut Vec<IndexedChar>) {
    for (bi, b) in blocks.iter().enumerate() {
        let block_id = format!("{prefix}.b{}", bi + 1);
        match b {
            Block::Text(tb) => collect_text_block_indexed(tb, &block_id, out),
            Block::Struct(sb) => collect_blocks_indexed(&sb.blocks, &block_id, out),
            Block::Image(_) | Block::Vector(_) | Block::Grid(_) => {}
        }
    }
}

fn collect_text_block_indexed(tb: &TextBlock, block_id: &str, out: &mut Vec<IndexedChar>) {
    for (li, line) in tb.lines.iter().enumerate() {
        let line_id = format!("{block_id}.l{}", li + 1);
        let spans = extract::spans(line);
        let mut first_in_line = !out.is_empty();
        for (si, sp) in spans.iter().enumerate() {
            let sp_id = format!("{line_id}.s{}", si + 1);
            if let Some(slice) = line.chars.get(sp.start..sp.end) {
                for ch in slice {
                    out.push(IndexedChar {
                        ch: ch.clone(),
                        span_id: sp_id.clone(),
                        line_break_before: first_in_line,
                    });
                    first_in_line = false;
                }
            }
        }
    }
}

fn extract_page_text(blocks: &[Block]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for b in blocks {
        match b {
            Block::Text(tb) => {
                let t = extract::block_text(tb);
                if !t.is_empty() {
                    parts.push(t);
                }
            }
            Block::Struct(sb) => {
                let t = extract_page_text(&sb.blocks);
                if !t.is_empty() {
                    parts.push(t);
                }
            }
            Block::Image(_) | Block::Vector(_) | Block::Grid(_) => {}
        }
    }
    parts.join("\n\n")
}

fn match_quote_in_target(target: &ResolvedTarget, page: i32, quote: &str) -> Option<MatchedQuote> {
    let quote_norm = normalize_text(quote);
    if quote_norm.is_empty() || target.chars.is_empty() {
        return None;
    }

    let (norm_target, byte_to_char) = normalize_indexed_chars(&target.chars);
    if norm_target.is_empty() {
        return None;
    }

    let matched_byte_range = if norm_target == quote_norm {
        Some(0..norm_target.len())
    } else if matches!(target.granularity, TargetGranularity::Slice) {
        None
    } else {
        find_clean_substring(&norm_target, &quote_norm)
    };

    let range = matched_byte_range?;
    let first_idx = *byte_to_char.get(range.start)?;
    let last_idx = *byte_to_char.get(range.end.checked_sub(1)?)?;
    let slice = target.chars.get(first_idx..=last_idx)?;
    let bbox = indexed_chars_bbox(slice)?;
    let matched_chars: Vec<StextChar> = slice.iter().map(|ic| ic.ch.clone()).collect();
    let actual_text = reconstruct_matched_text(slice);

    Some(MatchedQuote {
        page,
        bbox,
        actual_text,
        matched_chars,
    })
}

fn find_quote_on_page(
    page: &StextPage,
    page_num: i32,
    quote: &str,
) -> Option<(MatchedQuote, String)> {
    let mut chars = Vec::new();
    let prefix = format!("p{page_num}");
    collect_blocks_indexed(&page.blocks, &prefix, &mut chars);
    if chars.is_empty() {
        return None;
    }

    let quote_norm = normalize_text(quote);
    if !quote_norm.is_empty() {
        let (norm_page, byte_to_char) = normalize_indexed_chars(&chars);
        if let Some(range) = find_clean_substring(&norm_page, &quote_norm)
            && let (Some(&first_idx), Some(&last_idx)) = (
                byte_to_char.get(range.start),
                range.end.checked_sub(1).and_then(|e| byte_to_char.get(e)),
            )
            && let Some(slice) = chars.get(first_idx..=last_idx)
            && let Some(bbox) = indexed_chars_bbox(slice)
        {
            let span_id = common_content_id(slice);
            let matched = MatchedQuote {
                page: page_num,
                bbox,
                actual_text: reconstruct_matched_text(slice),
                matched_chars: slice.iter().map(|ic| ic.ch.clone()).collect(),
            };
            return Some((matched, span_id));
        }
    }

    // Fallback to MuPDF's page search (`search::search_page`) for soft-hyphen / diacritic edge cases.
    if let Ok(hits) = search::search_page(page, quote, search::options::EXACT)
        && let Some(hit) = hits.first()
        && let Some((matched, span_id)) = matched_from_search_hit(page, page_num, hit)
    {
        return Some((matched, span_id));
    }

    None
}

fn search_document_elsewhere(
    doc: &Document,
    page_count: i32,
    cache: &mut PageCache,
    quote: &str,
    exclude_id: Option<&str>,
) -> Result<Vec<String>, String> {
    let quote_norm = normalize_text(quote);
    if quote_norm.is_empty() {
        return Ok(Vec::new());
    }

    let mut found_ids: Vec<String> = Vec::new();
    for p in 1..=page_count {
        let stext_page = cache.primary_page(doc, p)?;
        let mut chars = Vec::new();
        let prefix = format!("p{p}");
        collect_blocks_indexed(&stext_page.blocks, &prefix, &mut chars);
        if chars.is_empty() {
            continue;
        }
        let (norm_page, byte_to_char) = normalize_indexed_chars(&chars);
        let mut search_from = 0usize;
        while search_from < norm_page.len() {
            let Some(rel_range) = find_clean_substring(&norm_page[search_from..], &quote_norm)
            else {
                break;
            };
            let abs_start = search_from + rel_range.start;
            let abs_end = search_from + rel_range.end;
            if let (Some(&first_idx), Some(&last_idx)) = (
                byte_to_char.get(abs_start),
                abs_end.checked_sub(1).and_then(|e| byte_to_char.get(e)),
            ) && let Some(slice) = chars.get(first_idx..=last_idx)
            {
                let cid = common_content_id(slice);
                if exclude_id.is_none_or(|ex| ex != cid) && !found_ids.contains(&cid) {
                    found_ids.push(cid);
                }
            }
            search_from = abs_end.max(search_from + 1);
            while search_from < norm_page.len() && !norm_page.is_char_boundary(search_from) {
                search_from += 1;
            }
        }

        if found_ids.is_empty()
            && let Ok(hits) = search::search_page(stext_page, quote, search::options::EXACT)
        {
            for hit in &hits {
                if let Some((_, cid)) = matched_from_search_hit(stext_page, p, hit)
                    && exclude_id.is_none_or(|ex| ex != cid)
                    && !found_ids.contains(&cid)
                {
                    found_ids.push(cid);
                }
            }
        }
    }

    Ok(found_ids)
}

fn matched_from_search_hit(
    page: &StextPage,
    page_num: i32,
    hit: &search::Hit,
) -> Option<(MatchedQuote, String)> {
    let mut bbox_opt: Option<Rect> = None;
    for q in &hit.quads {
        let r = Rect::from_quad(*q);
        bbox_opt = Some(match bbox_opt {
            Some(prev) => prev.union(r),
            None => r,
        });
    }
    let bbox = bbox_opt?;

    let mut matched_chars = Vec::new();
    let mut span_ids: Vec<String> = Vec::new();
    for (bi, block) in page.blocks.iter().enumerate() {
        if bi < hit.begin.block || bi > hit.end.block {
            continue;
        }
        let Block::Text(tb) = block else {
            continue;
        };
        for (li, line) in tb.lines.iter().enumerate() {
            if bi == hit.begin.block && li < hit.begin.line {
                continue;
            }
            if bi == hit.end.block && li > hit.end.line {
                continue;
            }
            let c_start = if bi == hit.begin.block && li == hit.begin.line {
                hit.begin.ch
            } else {
                0
            };
            let c_end = if bi == hit.end.block && li == hit.end.line {
                hit.end.ch.min(line.chars.len())
            } else {
                line.chars.len()
            };
            if c_start < c_end
                && let Some(slice) = line.chars.get(c_start..c_end)
            {
                let spans = extract::spans(line);
                for (offset, ch) in slice.iter().enumerate() {
                    let abs_ci = c_start + offset;
                    if let Some((si, _)) = spans
                        .iter()
                        .enumerate()
                        .find(|(_, sp)| abs_ci >= sp.start && abs_ci < sp.end)
                    {
                        let sid = format!("p{page_num}.b{}.l{}.s{}", bi + 1, li + 1, si + 1);
                        if !span_ids.contains(&sid) {
                            span_ids.push(sid);
                        }
                    }
                    matched_chars.push(ch.clone());
                }
            }
        }
    }

    if matched_chars.is_empty() {
        return None;
    }

    let actual_text = extract::chars_text(&matched_chars);
    let cid = if span_ids.len() == 1 {
        span_ids[0].clone()
    } else {
        format!("p{page_num}.b{}", hit.begin.block + 1)
    };
    Some((
        MatchedQuote {
            page: page_num,
            bbox,
            actual_text,
            matched_chars,
        },
        cid,
    ))
}

fn common_content_id(slice: &[IndexedChar]) -> String {
    let Some(first) = slice.first() else {
        return String::new();
    };
    let first_span = &first.span_id;
    if slice.iter().all(|ic| &ic.span_id == first_span) {
        return first_span.clone();
    }
    // Check common line prefix (`pN.bB.lL`).
    if let Some((first_line, _)) = first_span.rsplit_once(".s")
        && slice.iter().all(|ic| {
            ic.span_id
                .rsplit_once(".s")
                .is_some_and(|(l, _)| l == first_line)
        })
    {
        return first_line.to_owned();
    }
    // Check common block prefix (`pN.bB`).
    if let Some((first_block, _)) = first_span.split_once(".l")
        && slice.iter().all(|ic| {
            ic.span_id
                .split_once(".l")
                .is_some_and(|(b, _)| b == first_block)
        })
    {
        return first_block.to_owned();
    }
    first_span
        .split_once('.')
        .map_or_else(|| first_span.clone(), |(p, _)| p.to_owned())
}

fn reconstruct_matched_text(slice: &[IndexedChar]) -> String {
    let mut out = String::with_capacity(slice.len());
    for (idx, ic) in slice.iter().enumerate() {
        if idx > 0 && ic.line_break_before && !out.ends_with(' ') {
            out.push(' ');
        }
        out.push(rune_char(ic.ch.c));
    }
    out.trim().to_owned()
}

fn indexed_chars_bbox(chars: &[IndexedChar]) -> Option<Rect> {
    let mut acc: Option<Rect> = None;
    for ic in chars {
        if rune_char(ic.ch.c).is_whitespace() && (ic.ch.flags & char_flags::SYNTHETIC != 0) {
            continue;
        }
        let r = Rect::from_quad(ic.ch.quad);
        acc = Some(match acc {
            Some(prev) => prev.union(r),
            None => r,
        });
    }
    if acc.is_none() {
        for ic in chars {
            let r = Rect::from_quad(ic.ch.quad);
            acc = Some(match acc {
                Some(prev) => prev.union(r),
                None => r,
            });
        }
    }
    acc
}

/// Normalize whitespace, common Unicode ligatures, typographic quotes, and dashes.
fn normalize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for ch in s.chars() {
        push_normalized_char(ch, &mut out, &mut prev_space);
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn normalize_indexed_chars(chars: &[IndexedChar]) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(chars.len());
    let mut map = Vec::with_capacity(chars.len());
    let mut prev_space = true;

    for (idx, ic) in chars.iter().enumerate() {
        if ic.line_break_before && !prev_space {
            out.push(' ');
            map.push(idx);
            prev_space = true;
        }
        let before_len = out.len();
        push_normalized_char(rune_char(ic.ch.c), &mut out, &mut prev_space);
        let added = out.len() - before_len;
        map.resize(map.len() + added, idx);
    }

    if out.ends_with(' ') {
        out.pop();
        map.pop();
    }

    (out, map)
}

fn push_normalized_char(ch: char, out: &mut String, prev_space: &mut bool) {
    if ch == '\u{00ad}' {
        return;
    }
    if ch.is_whitespace() || ch == '\u{00a0}' {
        if !*prev_space {
            out.push(' ');
            *prev_space = true;
        }
        return;
    }
    *prev_space = false;
    match ch {
        '\u{fb00}' => out.push_str("ff"),
        '\u{fb01}' => out.push_str("fi"),
        '\u{fb02}' => out.push_str("fl"),
        '\u{fb03}' => out.push_str("ffi"),
        '\u{fb04}' => out.push_str("ffl"),
        '\u{fb05}' | '\u{fb06}' => out.push_str("st"),
        '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{02bc}' => out.push('\''),
        '\u{201c}' | '\u{201d}' | '\u{201e}' => out.push('"'),
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => {
            out.push('-')
        }
        other => out.push(other),
    }
}

/// Find `needle` in `haystack`, preferring token/word boundaries so short quotes
/// do not match inside unrelated words while still allowing trailing superscript
/// digits (`"capped at $1,000.1"` matching `"capped at $1,000."`).
fn find_clean_substring(haystack: &str, needle: &str) -> Option<std::ops::Range<usize>> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(needle) {
        let start = from + rel;
        let end = start + needle.len();

        let left_ok = if start == 0 {
            true
        } else {
            let prev = haystack[..start].chars().next_back();
            let first = needle.chars().next();
            match (prev, first) {
                (Some(p), Some(f)) => !(p.is_alphanumeric() && f.is_alphanumeric()),
                _ => true,
            }
        };

        let right_ok = if end == haystack.len() {
            true
        } else {
            let next = haystack[end..].chars().next();
            let last = needle.chars().next_back();
            match (last, next) {
                // If quote ends with punctuation (e.g. '.'), allow immediately adjacent superscript digits/markers.
                (Some(l), Some(n)) => !(l.is_alphabetic() && n.is_alphabetic()),
                _ => true,
            }
        };

        if left_ok && right_ok {
            return Some(start..end);
        }

        from = start + 1;
        while from < haystack.len() && !haystack.is_char_boundary(from) {
            from += 1;
        }
    }
    None
}

/// Evaluate all five deterministic epistemic hazards on a matched quote.
fn detect_hazards(
    doc: &Document,
    cache: &mut PageCache,
    modified_after_signing: bool,
    matched: &MatchedQuote,
    expected_quote: &str,
) -> Result<(Vec<String>, Option<String>), String> {
    let mut hazards: Vec<String> = Vec::new();
    let mut footnote_text: Option<String> = None;

    // 1. Strikethrough hazard (character flag, StrikeOut annotation, or vector bar crossing mid-body).
    if check_struck_through(doc, cache, matched)? {
        hazards.push(HAZARD_STRUCK_THROUGH.to_owned());
    }

    // 2. Invisible text hazard (`3 Tr` neither filled nor stroked, or alpha == 0).
    if check_invisible_text(&matched.matched_chars) {
        hazards.push(HAZARD_INVISIBLE_TEXT.to_owned());
    }

    // 3. `/ActualText` mismatch hazard.
    if check_actual_text_mismatch(doc, cache, matched)? {
        hazards.push(HAZARD_ACTUAL_TEXT_MISMATCH.to_owned());
    }

    // 4. Unreferenced superscript footnote hazard.
    let primary = cache.primary_page(doc, matched.page)?;
    if let Some(fn_text) = check_unreferenced_footnote(primary, matched, expected_quote) {
        hazards.push(HAZARD_UNREFERENCED_FOOTNOTE.to_owned());
        footnote_text = Some(fn_text);
    }

    // 5. Post-signature incremental modification hazard.
    if modified_after_signing {
        hazards.push(HAZARD_MODIFIED_AFTER_SIGNING.to_owned());
    }

    Ok((hazards, footnote_text))
}

fn check_invisible_text(chars: &[StextChar]) -> bool {
    for ch in chars {
        if (ch.flags & char_flags::SYNTHETIC != 0) || rune_char(ch.c).is_whitespace() {
            continue;
        }
        let visible_paint = ch.flags & (char_flags::FILLED | char_flags::STROKED) != 0;
        let alpha = ch.argb >> 24;
        if !visible_paint || alpha == 0 {
            return true;
        }
    }
    false
}

fn check_struck_through(
    doc: &Document,
    cache: &mut PageCache,
    matched: &MatchedQuote,
) -> Result<bool, String> {
    if matched
        .matched_chars
        .iter()
        .any(|c| !rune_char(c.c).is_whitespace() && (c.flags & char_flags::STRIKEOUT != 0))
    {
        return Ok(true);
    }

    // Check PDF StrikeOut markup annotations on the page.
    let page_obj = doc.page(matched.page - 1).map_err(|e| e.0)?;
    let (_, page_ctm) = doc.page_transform(&page_obj).map_err(|e| e.0)?;
    let (annots, _) = annot::page_annots(doc, &page_obj);
    let target = matched.bbox;
    let target_w = (target.x1 - target.x0).max(1.0);

    for a in &annots {
        if annot::type_name(doc, a) == "StrikeOut"
            && let Some(ar) = annot::rect(doc, a, page_ctm)
        {
            let ov_x = ar.x1.min(target.x1) - ar.x0.max(target.x0);
            let ov_y = ar.y1.min(target.y1) - ar.y0.max(target.y0);
            if ov_x >= 0.25 * target_w && ov_y > 0.0 {
                return Ok(true);
            }
        }
    }

    // Extract styled page (with `COLLECT_VECTORS | COLLECT_STYLES`) to inspect
    // both MuPDF's character strikeout flag and raw horizontal vector strokes/bars.
    let styled = cache.styled_page(doc, matched.page)?;

    for ch in iter_page_chars(&styled.blocks) {
        if rune_char(ch.c).is_whitespace() || (ch.flags & char_flags::STRIKEOUT == 0) {
            continue;
        }
        let cr = Rect::from_quad(ch.quad);
        let cx = 0.5 * (cr.x0 + cr.x1);
        let cy = 0.5 * (cr.y0 + cr.y1);
        if cx >= target.x0 - 1.0
            && cx <= target.x1 + 1.0
            && cy >= target.y0 - 1.0
            && cy <= target.y1 + 1.0
        {
            return Ok(true);
        }
    }

    // Compute average font size and baseline `origin.y` of non-space matched characters.
    let mut sum_size = 0.0f32;
    let mut sum_origin_y = 0.0f32;
    let mut n_chars = 0usize;
    for ch in &matched.matched_chars {
        if !rune_char(ch.c).is_whitespace() {
            sum_size += ch.size;
            sum_origin_y += ch.origin.y;
            n_chars += 1;
        }
    }
    let avg_size = if n_chars > 0 {
        sum_size / (n_chars as f32)
    } else {
        (target.y1 - target.y0).max(10.0)
    };
    let avg_origin_y = if n_chars > 0 {
        Some(sum_origin_y / (n_chars as f32))
    } else {
        None
    };

    // Interior vertical band of the glyph body: strictly above the baseline/underline
    // zone and strictly below the top border zone.
    let (band_y0, band_y1) = match avg_origin_y {
        Some(oy) => (oy - 0.68 * avg_size, oy - 0.12 * avg_size),
        None => {
            let h = (target.y1 - target.y0).max(1.0);
            (target.y0 + 0.18 * h, target.y0 + 0.72 * h)
        }
    };
    let max_bar_thickness = (avg_size * 0.35).max(3.5);
    let min_bar_width = (avg_size * 1.2).max(6.0);
    let min_overlap_x = (0.25 * target_w).clamp(4.0, 24.0);

    for block in &styled.blocks {
        if let Block::Vector(vb) = block {
            let vw = vb.bbox.x1 - vb.bbox.x0;
            let vh = vb.bbox.y1 - vb.bbox.y0;
            if vh <= max_bar_thickness && vw >= min_bar_width {
                let vy = 0.5 * (vb.bbox.y0 + vb.bbox.y1);
                let ov_x = vb.bbox.x1.min(target.x1) - vb.bbox.x0.max(target.x0);
                if vy >= band_y0 && vy <= band_y1 && ov_x >= min_overlap_x {
                    return Ok(true);
                }
            }
        }
    }

    Ok(false)
}

fn check_actual_text_mismatch(
    doc: &Document,
    cache: &mut PageCache,
    matched: &MatchedQuote,
) -> Result<bool, String> {
    let primary_all = {
        let p = cache.primary_page(doc, matched.page)?;
        normalize_text(&extract_page_text(&p.blocks))
    };
    let raw_page = cache.raw_glyphs_page(doc, matched.page)?;
    let raw_all = normalize_text(&extract_page_text(&raw_page.blocks));

    // Fast path: if `IGNORE_ACTUALTEXT` produces identical normalized text on the
    // entire page, `/ActualText` did not alter any glyph text on this page.
    if primary_all == raw_all {
        return Ok(false);
    }

    // Collect raw glyph characters falling inside `matched.bbox`.
    let box_exp = matched.bbox.expand(2.0);
    let mut raw_chars: Vec<StextChar> = Vec::new();
    for ch in iter_page_chars(&raw_page.blocks) {
        let cr = Rect::from_quad(ch.quad);
        let cx = 0.5 * (cr.x0 + cr.x1);
        let cy = 0.5 * (cr.y0 + cr.y1);
        if cx >= box_exp.x0 && cx <= box_exp.x1 && cy >= box_exp.y0 && cy <= box_exp.y1 {
            raw_chars.push(ch.clone());
        }
    }

    let raw_text = normalize_text(&extract::chars_text(&raw_chars));
    let matched_norm = normalize_text(&matched.actual_text);

    Ok(!raw_text.contains(&matched_norm))
}

/// Detect an immediately adjacent or trailing superscript footnote marker (`1`, `2`, `*`, `†`, ...)
/// at `< 82%` body font size and raised baseline whose corresponding bottom-of-page footnote
/// text is absent from `expected_quote`.
fn check_unreferenced_footnote(
    page: &StextPage,
    matched: &MatchedQuote,
    expected_quote: &str,
) -> Option<String> {
    let non_ws: Vec<&StextChar> = matched
        .matched_chars
        .iter()
        .filter(|c| !rune_char(c.c).is_whitespace())
        .collect();
    if non_ws.is_empty() {
        return None;
    }

    // Determine dominant body font size of the cited clause (falling back to page body size).
    let mut sizes: Vec<f32> = non_ws.iter().map(|c| c.size).collect();
    sizes.sort_by(f32::total_cmp);
    let clause_body_size = sizes[sizes.len() / 2];
    let block_refs: Vec<&Block> = page.blocks.iter().collect();
    let page_body_size = extract::body_size(&block_refs);
    let ref_size = if clause_body_size > 0.0 {
        clause_body_size.max(page_body_size)
    } else {
        page_body_size
    };
    if ref_size <= 0.0 {
        return None;
    }

    let max_sup_size = 0.82 * ref_size;

    // Last body-sized character in `matched.matched_chars` anchors baseline and right edge.
    let last_body_ch = non_ws
        .iter()
        .rev()
        .copied()
        .find(|c| c.size >= max_sup_size)
        .unwrap_or(non_ws[non_ws.len() - 1]);
    let ref_origin_y = last_body_ch.origin.y;
    let mut ref_x1 = Rect::from_quad(last_body_ch.quad).x1;

    let all_chars: Vec<&StextChar> = iter_page_chars(&page.blocks).collect();

    // Allow a single trailing punctuation character (e.g. '.', ',', '"') immediately
    // after the matched span on the same baseline when `expected_quote` omitted punctuation.
    for ch in &all_chars {
        let c = rune_char(ch.c);
        if matches!(c, '.' | ',' | ';' | ':' | '"' | '\'' | ')')
            && (ch.origin.y - ref_origin_y).abs() <= 0.15 * ref_size
        {
            let cr = Rect::from_quad(ch.quad);
            if cr.x0 >= ref_x1 - 2.0 && cr.x0 <= ref_x1 + 0.35 * ref_size && cr.x1 > ref_x1 {
                ref_x1 = cr.x1;
            }
        }
    }

    // Gather superscript characters immediately adjacent to `ref_x1` (within `< 82%` size
    // and raised baseline in top-down fitz page coordinates: `origin.y < ref_origin_y`).
    let mut sup_cands: Vec<&StextChar> = all_chars
        .into_iter()
        .filter(|ch| {
            let c = rune_char(ch.c);
            if c.is_whitespace() || ch.size >= max_sup_size {
                return false;
            }
            let raised = ch.origin.y < ref_origin_y - 0.04 * ref_size
                && ch.origin.y >= ref_origin_y - 1.15 * ref_size;
            if !raised {
                return false;
            }
            let cr = Rect::from_quad(ch.quad);
            cr.x0 >= ref_x1 - 3.5 && cr.x0 <= ref_x1 + 1.25 * ref_size
        })
        .collect();

    if sup_cands.is_empty() {
        return None;
    }

    sup_cands.sort_by(|a, b| {
        Rect::from_quad(a.quad)
            .x0
            .total_cmp(&Rect::from_quad(b.quad).x0)
    });

    // Chain contiguous adjacent superscript characters into the marker token (max 4 chars).
    let mut marker_raw = String::new();
    let mut prev_x1 = ref_x1;
    for ch in sup_cands {
        let cr = Rect::from_quad(ch.quad);
        if marker_raw.is_empty() {
            if cr.x0 > ref_x1 + 0.85 * ref_size {
                break;
            }
        } else if cr.x0 > prev_x1 + 0.6 * ref_size {
            break;
        }
        marker_raw.push(rune_char(ch.c));
        prev_x1 = cr.x1.max(prev_x1);
        if marker_raw.chars().count() >= 4 {
            break;
        }
    }

    let marker = marker_raw
        .trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']'))
        .trim();
    if !is_plausible_footnote_marker(marker) {
        return None;
    }

    let fn_text = find_footnote_at_bottom(page, marker, ref_origin_y, ref_size)?;
    let fn_body = strip_leading_marker(&fn_text, marker);
    let fn_body_norm = normalize_text(fn_body);
    let quote_norm = normalize_text(expected_quote);

    if !fn_body_norm.is_empty() && !quote_norm.contains(&fn_body_norm) {
        Some(fn_text)
    } else {
        None
    }
}

fn is_plausible_footnote_marker(marker: &str) -> bool {
    if marker.is_empty() || marker.chars().count() > 3 {
        return false;
    }
    if marker.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    if marker
        .chars()
        .all(|c| matches!(c, '*' | '\u{2020}' | '\u{2021}' | '\u{00a7}' | '\u{00b6}'))
    {
        return true;
    }
    marker.len() == 1 && marker.chars().all(|c| c.is_ascii_lowercase())
}

/// Find the footnote text corresponding to `marker` below `cited_y` on `page`.
fn find_footnote_at_bottom(
    page: &StextPage,
    marker: &str,
    cited_y: f32,
    ref_size: f32,
) -> Option<String> {
    let mut lines_below: Vec<(Rect, String, usize)> = Vec::new();
    for (bi, block) in page.blocks.iter().enumerate() {
        walk_block_lines_below(block, bi, cited_y + 0.5 * ref_size, &mut lines_below);
    }
    if lines_below.is_empty() {
        return None;
    }

    lines_below.sort_by(|a, b| {
        a.0.y0
            .total_cmp(&b.0.y0)
            .then_with(|| a.0.x0.total_cmp(&b.0.x0))
    });

    // Prefer matches starting from the lower part of the page if multiple candidates exist,
    // while still matching single-paragraph pages cleanly.
    for (idx, &(l_bbox, ref l_text, block_idx)) in lines_below.iter().enumerate() {
        let trimmed = l_text.trim();
        if trimmed == marker
            || trimmed
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .is_some_and(|m| m == marker)
        {
            // Superscript footnote marker placed in its own tiny block just left of the footnote body!
            let mut row_parts = vec![trimmed.to_owned()];
            let row_cy = 0.5 * (l_bbox.y0 + l_bbox.y1);
            let mut body_block_idx = block_idx;
            for (other_bbox, other_text, other_bi) in lines_below.iter().skip(idx + 1) {
                let other_cy = 0.5 * (other_bbox.y0 + other_bbox.y1);
                if (other_cy - row_cy).abs() <= ref_size && other_bbox.x0 >= l_bbox.x1 - 3.0 {
                    row_parts.push(other_text.trim().to_owned());
                    body_block_idx = *other_bi;
                }
            }
            if row_parts.len() > 1 {
                let mut combined = row_parts.join(" ");
                append_continuation_lines(
                    &lines_below,
                    idx + 1,
                    body_block_idx,
                    l_bbox.y1,
                    &mut combined,
                );
                return Some(combined);
            }
        }

        if line_starts_with_footnote_marker(trimmed, marker) {
            let mut full = trimmed.to_owned();
            append_continuation_lines(&lines_below, idx + 1, block_idx, l_bbox.y1, &mut full);
            return Some(full);
        }
    }

    None
}

fn append_continuation_lines(
    lines_below: &[(Rect, String, usize)],
    start_idx: usize,
    block_idx: usize,
    first_y1: f32,
    out: &mut String,
) {
    let mut prev_y1 = first_y1;
    for (bbox, text, bi) in lines_below.iter().skip(start_idx) {
        if *bi != block_idx || bbox.y0 <= prev_y1 - 1.0 {
            continue;
        }
        let t = text.trim();
        if t.is_empty() {
            continue;
        }
        // Stop if the next line begins another numbered footnote entry.
        if starts_new_numbered_footnote(t) {
            break;
        }
        out.push(' ');
        out.push_str(t);
        prev_y1 = bbox.y1;
    }
}

fn starts_new_numbered_footnote(line: &str) -> bool {
    let digits_len = line.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=2).contains(&digits_len) {
        let rest = &line[digits_len..];
        rest.starts_with(' ') || rest.starts_with('.') || rest.starts_with(')')
    } else {
        line.starts_with("* ") || line.starts_with("\u{2020} ") || line.starts_with("\u{2021} ")
    }
}

fn line_starts_with_footnote_marker(trimmed: &str, marker: &str) -> bool {
    if let Some(rest) = trimmed.strip_prefix(marker) {
        if rest.is_empty() {
            return false;
        }
        if rest.starts_with(' ')
            || rest.starts_with('.')
            || rest.starts_with(')')
            || rest.starts_with(':')
        {
            return rest
                .trim_start_matches([' ', '.', ')', ':'])
                .chars()
                .next()
                .is_some();
        }
        // Also allow superscript marker directly followed by an uppercase word (`"1Excludes..."`).
        if rest.chars().next().is_some_and(char::is_uppercase) {
            return true;
        }
    }
    if let Some(bracketed) = trimmed.strip_prefix('[')
        && let Some(after_m) = bracketed.strip_prefix(marker)
        && let Some(rest) = after_m.strip_prefix(']')
    {
        return !rest.trim().is_empty();
    }
    if let Some(paren) = trimmed.strip_prefix('(')
        && let Some(after_m) = paren.strip_prefix(marker)
        && let Some(rest) = after_m.strip_prefix(')')
    {
        return !rest.trim().is_empty();
    }
    false
}

fn strip_leading_marker<'a>(footnote: &'a str, marker: &str) -> &'a str {
    let trimmed = footnote.trim();
    if let Some(rest) = trimmed.strip_prefix(marker) {
        return rest.trim_start_matches([' ', '.', ')', ':']);
    }
    if let Some(b) = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_prefix(marker))
        .and_then(|s| s.strip_prefix(']'))
    {
        return b.trim_start_matches([' ', '.', ':']);
    }
    if let Some(p) = trimmed
        .strip_prefix('(')
        .and_then(|s| s.strip_prefix(marker))
        .and_then(|s| s.strip_prefix(')'))
    {
        return p.trim_start_matches([' ', '.', ':']);
    }
    trimmed
}

fn walk_block_lines_below(
    block: &Block,
    block_idx: usize,
    min_y: f32,
    out: &mut Vec<(Rect, String, usize)>,
) {
    match block {
        Block::Text(tb) => {
            for line in &tb.lines {
                if line.bbox.y0 > min_y {
                    let text = extract::chars_text(&line.chars);
                    if !text.trim().is_empty() {
                        out.push((line.bbox, text, block_idx));
                    }
                }
            }
        }
        Block::Struct(sb) => {
            for child in &sb.blocks {
                walk_block_lines_below(child, block_idx, min_y, out);
            }
        }
        Block::Image(_) | Block::Vector(_) | Block::Grid(_) => {}
    }
}

fn iter_page_chars(blocks: &[Block]) -> impl Iterator<Item = &StextChar> {
    let mut lines: Vec<&StextLine> = Vec::new();
    collect_lines_ref(blocks, &mut lines);
    lines.into_iter().flat_map(|l| l.chars.iter())
}

fn collect_lines_ref<'a>(blocks: &'a [Block], out: &mut Vec<&'a StextLine>) {
    for b in blocks {
        match b {
            Block::Text(tb) => out.extend(tb.lines.iter()),
            Block::Struct(sb) => collect_lines_ref(&sb.blocks, out),
            Block::Image(_) | Block::Vector(_) | Block::Grid(_) => {}
        }
    }
}

/// Render a padded crop PNG around `bbox` with a translucent yellow (`clean == true`)
/// or red (`clean == false`) highlight rectangle painted over `bbox`.
fn render_evidence_crop(
    doc: &Document,
    page_num: i32,
    bbox: Rect,
    clean: bool,
    evidence_dpi: f32,
) -> Result<Vec<u8>, String> {
    let page_obj = doc.page(page_num - 1).map_err(|e| e.0)?;
    let dpi = if evidence_dpi.is_finite() && evidence_dpi > 0.0 {
        evidence_dpi.clamp(36.0, 600.0)
    } else {
        144.0
    };
    let scale = dpi / 72.0;
    let ctm = Matrix::scale(scale, scale);

    let full_page_px = draw::page_bbox(doc, &page_obj, ctm)?;
    let padded_page_space = Rect::new(
        bbox.x0 - 28.0,
        bbox.y0 - 20.0,
        bbox.x1 + 28.0,
        bbox.y1 + 20.0,
    );
    let mut crop_irect = IRect::round(padded_page_space.transform(ctm)).intersect(full_page_px);
    if crop_irect.is_empty() {
        crop_irect = IRect::round(padded_page_space.transform(ctm));
    }
    if crop_irect.width() <= 0 || crop_irect.height() <= 0 {
        return Err("empty evidence crop region".to_owned());
    }
    let pixels = (crop_irect.width() as u64).saturating_mul(crop_irect.height() as u64);
    if pixels > MAX_EVIDENCE_PIXELS {
        return Err("evidence crop exceeds maximum pixel budget".to_owned());
    }

    let mut pix = Pix::new(crop_irect, 3, false)?;
    pix.clear_with(255);
    let mut dev = DrawDevice::new(Some(doc), pix);
    dev.cc.icc = true;
    let _run = page_run::run_page_parts(
        doc,
        &page_obj,
        &mut dev,
        Store::default(),
        Some("View"),
        ctm,
        Parts::ALL,
    );

    let mut path = Path::new();
    let hl = bbox.expand(1.5);
    path.move_to(hl.x0, hl.y0);
    path.line_to(hl.x1, hl.y0);
    path.line_to(hl.x1, hl.y1);
    path.line_to(hl.x0, hl.y1);
    path.close_path();

    let cs = ColorSpace::Rgb;
    let (rgb, fill_alpha) = if clean {
        ([1.0f32, 0.90, 0.15], 0.32f32)
    } else {
        ([0.90f32, 0.20, 0.15], 0.30f32)
    };
    dev.fill_path(
        &path,
        false,
        ctm,
        &cs,
        &rgb,
        fill_alpha,
        ColorParams::default(),
    );

    let stroke = StrokeState {
        linewidth: 1.0 / ctm.expansion().max(f32::EPSILON),
        ..StrokeState::default()
    };
    dev.stroke_path(&path, &stroke, ctm, &cs, &rgb, 0.90, ColorParams::default());

    dev.into_pix().to_pixmap(Some(ColorSpace::Rgb)).png()
}

fn rect_to_array(r: Rect) -> [f32; 4] {
    [r.x0, r.y0, r.x1, r.y1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{doc_bytes, page_doc, stream};

    const FONT_RES: &str = "/Font<<\
        /F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>\
        /F2<</Type/Font/Subtype/Type1/BaseFont/Helvetica-Bold/Encoding/WinAnsiEncoding>>\
    >>";

    fn two_page_doc(p1_content: &[u8], p2_content: &[u8]) -> Document {
        let objs = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R 5 0 R]/Count 2>>".to_vec(),
            format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<<{FONT_RES}>>/Contents 4 0 R>>"
            )
            .into_bytes(),
            stream("", p1_content),
            format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<<{FONT_RES}>>/Contents 6 0 R>>"
            )
            .into_bytes(),
            stream("", p2_content),
        ];
        doc_bytes(&objs)
    }

    #[test]
    fn clean_exact_citation_verifies_and_renders_evidence_png() {
        let doc = page_doc(
            b"BT /F1 12 Tf 50 700 Td (Governing Law shall be the State of Delaware.) Tj ET",
            FONT_RES,
            &[],
        );
        let claims = vec![ClaimInput {
            claim_id: Some("c1".to_owned()),
            doc_id: Some("b3:testdoc".to_owned()),
            id: Some("p1.b1.l1.s1".to_owned()),
            page: Some(1),
            quote: "Governing Law shall be the State of Delaware.".to_owned(),
        }];

        let summary = audit_claims(&doc, "b3:testdoc", &claims, true, 144.0)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(summary.total, 1);
        assert_eq!(summary.verified_clean, 1);
        assert_eq!(summary.verified_with_hazards, 0);
        assert_eq!(summary.failed, 0);
        assert!(summary.all_verified);

        let item = &summary.results[0];
        assert_eq!(item.status, AuditStatus::Verified);
        assert!(item.verified);
        assert!(item.hazards.is_empty());
        assert_eq!(item.resolved_page, Some(1));
        let png = item
            .evidence_png
            .as_ref()
            .unwrap_or_else(|| panic!("expected evidence PNG"));
        assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
    }

    #[test]
    fn citation_on_wrong_page_reports_wrong_location_and_found_elsewhere() {
        let doc = two_page_doc(
            b"BT /F1 12 Tf 50 700 Td (Definitions and Interpretation.) Tj ET",
            b"BT /F1 12 Tf 50 700 Td (Payment is due within thirty days.) Tj ET",
        );
        let claims = vec![
            ClaimInput {
                claim_id: Some("wrong-page".to_owned()),
                doc_id: None,
                id: None,
                page: Some(1),
                quote: "Payment is due within thirty days.".to_owned(),
            },
            ClaimInput {
                claim_id: Some("wrong-span".to_owned()),
                doc_id: None,
                id: Some("p1.b1.l1.s1".to_owned()),
                page: None,
                quote: "Payment is due within thirty days.".to_owned(),
            },
        ];

        let summary = audit_claims(&doc, "b3:testdoc", &claims, false, 72.0)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(summary.failed, 2);
        assert!(!summary.all_verified);

        for res in &summary.results {
            assert_eq!(res.status, AuditStatus::WrongLocation);
            assert!(!res.verified);
            assert_eq!(res.found_elsewhere, vec!["p2.b1.l1.s1".to_owned()]);
        }
    }

    #[test]
    fn hallucinated_quotes_report_mismatch_and_not_found_and_stale_doc_id() {
        let doc = page_doc(
            b"BT /F1 12 Tf 50 700 Td (Annual fee is $500.) Tj ET",
            FONT_RES,
            &[],
        );
        let claims = vec![
            ClaimInput {
                claim_id: Some("mismatch".to_owned()),
                doc_id: None,
                id: Some("p1.b1.l1.s1".to_owned()),
                page: None,
                quote: "Annual fee is $5,000.".to_owned(),
            },
            ClaimInput {
                claim_id: Some("not-found".to_owned()),
                doc_id: None,
                id: None,
                page: Some(1),
                quote: "Unlimited free renewals forever.".to_owned(),
            },
            ClaimInput {
                claim_id: Some("stale".to_owned()),
                doc_id: Some("b3:oldhash".to_owned()),
                id: Some("p1.b1.l1.s1".to_owned()),
                page: None,
                quote: "Annual fee is $500.".to_owned(),
            },
        ];

        let summary = audit_claims(&doc, "b3:currenthash", &claims, false, 72.0)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(summary.failed, 3);
        assert_eq!(summary.results[0].status, AuditStatus::Mismatch);
        assert_eq!(
            summary.results[0].actual_text.as_deref(),
            Some("Annual fee is $500.")
        );
        assert_eq!(summary.results[1].status, AuditStatus::NotFound);
        assert_eq!(summary.results[2].status, AuditStatus::StaleDocId);
    }

    #[test]
    fn superscript_footnote_omitted_flags_hazard_and_includes_footnote_text() {
        // Clause at top of page (y=700) with 7pt raised superscript "1" via `4 Ts`,
        // pointing to bottom-of-page footnote (y=72) "1 Excludes consequential damages."
        let content = b"BT /F1 12 Tf 50 700 Td (Liability is capped at $1,000.) Tj \
            4 Ts /F1 7 Tf (1) Tj 0 Ts ET \
            BT /F1 9 Tf 50 72 Td (1 Excludes consequential damages.) Tj ET";
        let doc = page_doc(content, FONT_RES, &[]);

        let claims = vec![ClaimInput {
            claim_id: Some("fn-omitted".to_owned()),
            doc_id: None,
            id: Some("p1.b1.l1.s1".to_owned()),
            page: Some(1),
            quote: "Liability is capped at $1,000.".to_owned(),
        }];

        let summary =
            audit_claims(&doc, "b3:doc", &claims, false, 72.0).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(summary.verified_clean, 0);
        assert_eq!(summary.verified_with_hazards, 1);
        assert_eq!(summary.failed, 1);
        assert!(!summary.all_verified);

        let item = &summary.results[0];
        assert_eq!(item.status, AuditStatus::VerifiedWithHazards);
        assert!(!item.verified);
        assert!(
            item.hazards
                .contains(&HAZARD_UNREFERENCED_FOOTNOTE.to_owned())
        );
        assert_eq!(
            item.footnote_text.as_deref(),
            Some("1 Excludes consequential damages.")
        );
    }

    #[test]
    fn vector_strikethrough_and_invisible_text_flag_hazards() {
        // Clause 1 at y=700 crossed by a horizontal stroke at y=704 (through x-height),
        // Clause 2 at y=660 rendered with `3 Tr` (invisible text mode).
        let content = b"BT /F1 12 Tf 50 700 Td (Tenant may sublease without consent.) Tj ET \
            0.75 w 48 704 m 255 704 l S \
            BT 3 Tr /F1 12 Tf 50 660 Td (Hidden prompt injection text.) Tj ET";
        let doc = page_doc(content, FONT_RES, &[]);

        let claims = vec![
            ClaimInput {
                claim_id: Some("struck".to_owned()),
                doc_id: None,
                id: Some("p1.b1.l1.s1".to_owned()),
                page: Some(1),
                quote: "Tenant may sublease without consent.".to_owned(),
            },
            ClaimInput {
                claim_id: Some("invisible".to_owned()),
                doc_id: None,
                id: None,
                page: Some(1),
                quote: "Hidden prompt injection text.".to_owned(),
            },
        ];

        let summary =
            audit_claims(&doc, "b3:doc", &claims, false, 72.0).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(summary.verified_with_hazards, 2);

        assert_eq!(summary.results[0].status, AuditStatus::VerifiedWithHazards);
        assert!(
            summary.results[0]
                .hazards
                .contains(&HAZARD_STRUCK_THROUGH.to_owned())
        );

        assert_eq!(summary.results[1].status, AuditStatus::VerifiedWithHazards);
        assert!(
            summary.results[1]
                .hazards
                .contains(&HAZARD_INVISIBLE_TEXT.to_owned())
        );
    }

    #[test]
    fn actual_text_spoof_flags_hazard() {
        let content = b"/Span <</ActualText (Fee is $100.)>> BDC \
            BT /F1 12 Tf 50 700 Td (Total: 900 USD) Tj ET \
            EMC";
        let doc = page_doc(content, FONT_RES, &[]);

        let claims = vec![ClaimInput {
            claim_id: Some("spoof".to_owned()),
            doc_id: None,
            id: Some("p1.b1.l1.s1".to_owned()),
            page: Some(1),
            quote: "Fee is $100.".to_owned(),
        }];

        let summary =
            audit_claims(&doc, "b3:doc", &claims, false, 72.0).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(summary.results[0].status, AuditStatus::VerifiedWithHazards);
        assert!(
            summary.results[0]
                .hazards
                .contains(&HAZARD_ACTUAL_TEXT_MISMATCH.to_owned())
        );
    }
}
