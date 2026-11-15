// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Reversible format-preserving PII cloaking ("Agent Privacy Firewall").
//!
//! [`cloak_document`] replaces sensitive entities (SSNs, emails, phone numbers,
//! payment cards, IBANs, custom literals, and regex patterns) with deterministic
//! format-preserving synthetic tokens across page content streams, AcroForm
//! field values and synthesized widget appearances, annotations, `/Info`
//! dictionaries, outlines, structure trees, and XMP metadata streams, rewrites
//! the PDF with full orphan garbage collection (`garbage: 4`), and runs a
//! mandatory zero-leak verification pass before returning bytes.
//!
//! [`uncloak_pdf`] and [`uncloak_text`] reverse synthetic tokens back to exact
//! original values in agent-edited PDFs and agent text/JSON outputs.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::annot;
use crate::content::{self, Op, ProcState, Processor, Store};
use crate::content_clean::{self, filter_page_contents};
use crate::content_filter::Hooks;
use crate::doc_id::DocId;
use crate::document::Document;
use crate::extract::{self, Style};
use crate::fill;
use crate::form;
use crate::geometry::{Matrix, Point, Quad, Rect};
use crate::object::{Dict, ObjRef, Object};
use crate::page_run::Parts;
use crate::patch::{
    CjkLang, FontKey, add_font_object, append_patch_layer, baseline_text_matrix,
    encode_replacement_runs, guess_cjk_lang, ink_bbox, match_base14, measure_runs_advance_em,
    next_unique_font_res_name, push_float, push_pdf_hex_utf16, push_pdf_literal,
    trim_synthetic_edges,
};
use crate::redact;
use crate::regexp::{self, Regex};
use crate::scrub::Matcher;
use crate::stext::{self, Block, Options as StextOptions, StextChar, StextLine, StextPage};
use crate::verify;
use crate::write::{self, WriteOptions};

/// Versioned schema identifier stored in [`CloakVault::schema`].
pub const CLOAK_VAULT_SCHEMA: &str = "nupdf.cloak.vault/v1";

const PRESET_EMAIL: (&str, &str) = (
    "EMAIL",
    r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z][A-Za-z]+",
);
const PRESET_IBAN: (&str, &str) = (
    "IBAN",
    r"[A-Z][A-Z][0-9][0-9] ?[A-Z0-9]{4} ?[A-Z0-9]{4} ?[A-Z0-9]{4}( ?[A-Z0-9]{1,4}){0,4}",
);
const PRESET_CARD: (&str, &str) = ("CARD", r"[0-9]{4}[ -]?[0-9]{4}[ -]?[0-9]{4}[ -]?[0-9]{1,7}");
const PRESET_SSN: (&str, &str) = ("SSN", r"[0-9]{3}[- ][0-9]{2}[- ][0-9]{4}");
const PRESET_PHONE: (&str, &str) = (
    "PHONE",
    r"(\+[0-9]{1,3}[ .-]?)?(\([0-9]{2,4}\)[ .-]?|[0-9]{2,4}[ .-])[0-9]{3,4}[ .-]?[0-9]{3,4}",
);

const PRESET_PII_ALL: &[(&str, &str)] = &[
    PRESET_EMAIL,
    PRESET_IBAN,
    PRESET_CARD,
    PRESET_SSN,
    PRESET_PHONE,
];
const PRESET_SSN_SLICE: &[(&str, &str)] = &[PRESET_SSN];
const PRESET_EMAIL_SLICE: &[(&str, &str)] = &[PRESET_EMAIL];
const PRESET_PHONE_SLICE: &[(&str, &str)] = &[PRESET_PHONE];
const PRESET_CARD_SLICE: &[(&str, &str)] = &[PRESET_CARD];
const PRESET_IBAN_SLICE: &[(&str, &str)] = &[PRESET_IBAN];

/// Target specification for [`cloak_document`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CloakTarget {
    /// Named PII preset: `"ssn"`, `"email"`, `"phone"`, `"card"`, `"iban"`, or
    /// `"pii"` (all five presets).
    Preset(String),
    /// Exact literal secret tagged with a category (e.g. `"PERSON"`, `"ORG"`).
    Literal { category: String, secret: String },
    /// Regular expression pattern tagged with a category.
    Regex { category: String, pattern: String },
}

/// One reversible secret-to-token mapping stored inside [`CloakVault`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VaultEntry {
    pub token: String,
    pub secret: String,
    pub category: String,
    pub occurrences: usize,
}

/// Reversible mapping vault produced by [`cloak_document`] and consumed by
/// [`uncloak_pdf`] and [`uncloak_text`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CloakVault {
    pub schema: String,
    pub original_doc_id: String,
    pub cloaked_doc_id: String,
    pub entries: Vec<VaultEntry>,
}

/// Secret-free summary of one cloaked entity safe for stdout and agent logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CloakPreview {
    pub token: String,
    pub category: String,
    pub occurrences: usize,
}

/// Secret-free report returned by [`cloak_document`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CloakReport {
    pub original_doc_id: String,
    pub cloaked_doc_id: String,
    pub replacements: usize,
    pub entities_cloaked: usize,
    pub verified_zero_leaks: bool,
    pub token_preview: Vec<CloakPreview>,
}

/// Summary report returned by [`uncloak_pdf`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UncloakReport {
    pub original_doc_id: String,
    pub cloaked_doc_id: String,
    pub restored_doc_id: String,
    pub replacements: usize,
    pub entities_restored: usize,
}

/// Structured error returned by [`cloak_document`] and [`uncloak_pdf`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloakError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for CloakError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CloakError {}

/// Resolves a preset name (`"ssn"`, `"email"`, `"phone"`, `"card"`, `"iban"`,
/// or `"pii"`) to `(CATEGORY, regex_pattern)` pairs matching `nupdf redact`.
pub fn preset_patterns(name: &str) -> Option<&'static [(&'static str, &'static str)]> {
    match name.trim().to_ascii_lowercase().as_str() {
        "ssn" => Some(PRESET_SSN_SLICE),
        "email" => Some(PRESET_EMAIL_SLICE),
        "phone" => Some(PRESET_PHONE_SLICE),
        "card" => Some(PRESET_CARD_SLICE),
        "iban" => Some(PRESET_IBAN_SLICE),
        "pii" => Some(PRESET_PII_ALL),
        _ => None,
    }
}

fn sanitize_category(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
        } else if (ch == '_' || ch == '-' || ch.is_whitespace()) && !out.ends_with('_') {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "SECRET".to_string()
    } else {
        trimmed.to_string()
    }
}

fn literal_regex_pattern(text: &str) -> String {
    let mut out = String::new();
    let mut in_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !in_space {
                out.push_str("[ \t\r\n]+");
            }
            in_space = true;
            continue;
        }
        in_space = false;
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[derive(Debug)]
struct CompiledRule {
    category: String,
    regex: Regex,
    canonical_literal: Option<String>,
    numeric_boundary: bool,
}

#[derive(Debug, Clone)]
struct RawMatch {
    start: usize,
    end: usize,
    rule_idx: usize,
}

#[derive(Debug)]
struct CloakSession {
    rules: Vec<CompiledRule>,
    literal_secrets: Vec<String>,
    /// Every secret spelling seen, with its category.
    observed_raw_secrets: BTreeMap<String, String>,
    secret_to_entry: HashMap<String, usize>,
    used_tokens: HashSet<String>,
    category_counters: HashMap<String, usize>,
    entries: Vec<VaultEntry>,
}

impl CloakSession {
    fn new(targets: &[CloakTarget]) -> Result<Self, CloakError> {
        let mut rules = Vec::new();
        let mut literal_secrets = Vec::new();

        for target in targets {
            match target {
                CloakTarget::Preset(name) => {
                    let Some(pairs) = preset_patterns(name) else {
                        return Err(CloakError {
                            code: "usage.bad_preset",
                            message: format!(
                                "unknown cloak preset {name:?}; expected one of: ssn, email, phone, card, iban, pii"
                            ),
                        });
                    };
                    for &(cat, pat) in pairs {
                        let regex = Regex::compile(pat.as_bytes(), 0).map_err(|e| CloakError {
                            code: "usage.bad_pattern",
                            message: format!("invalid preset regex for {cat}: {e}"),
                        })?;
                        let numeric_boundary = matches!(cat, "SSN" | "CARD" | "PHONE");
                        rules.push(CompiledRule {
                            category: cat.to_string(),
                            regex,
                            canonical_literal: None,
                            numeric_boundary,
                        });
                    }
                }
                CloakTarget::Literal { category, secret } => {
                    if secret.trim().is_empty() {
                        return Err(CloakError {
                            code: "usage.empty_secret",
                            message: "cloak literal secret must not be empty".to_string(),
                        });
                    }
                    let cat = sanitize_category(category);
                    let pat = literal_regex_pattern(secret);
                    let regex = Regex::compile(pat.as_bytes(), 0).map_err(|e| CloakError {
                        code: "usage.bad_pattern",
                        message: format!("failed to compile literal pattern: {e}"),
                    })?;
                    literal_secrets.push(secret.clone());
                    rules.push(CompiledRule {
                        category: cat,
                        regex,
                        canonical_literal: Some(secret.clone()),
                        numeric_boundary: false,
                    });
                }
                CloakTarget::Regex { category, pattern } => {
                    if pattern.is_empty() {
                        return Err(CloakError {
                            code: "usage.bad_pattern",
                            message: "cloak regex pattern must not be empty".to_string(),
                        });
                    }
                    let cat = sanitize_category(category);
                    let regex = Regex::compile(pattern.as_bytes(), 0).map_err(|e| CloakError {
                        code: "usage.bad_pattern",
                        message: format!("invalid cloak regex {pattern:?}: {e}"),
                    })?;
                    rules.push(CompiledRule {
                        category: cat,
                        regex,
                        canonical_literal: None,
                        numeric_boundary: false,
                    });
                }
            }
        }

        Ok(CloakSession {
            rules,
            literal_secrets,
            observed_raw_secrets: BTreeMap::new(),
            secret_to_entry: HashMap::new(),
            used_tokens: HashSet::new(),
            category_counters: HashMap::new(),
            entries: Vec::new(),
        })
    }

    fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    fn find_matches(&self, text: &str) -> Vec<RawMatch> {
        if self.rules.is_empty() || text.is_empty() {
            return Vec::new();
        }
        let bytes = text.as_bytes();
        let mut candidates: Vec<RawMatch> = Vec::new();

        // Pre-compute byte ranges of any synthetic tokens already present in `text`
        // so format-preserving tokens (e.g. `000-00-0001`) are never double-cloaked.
        let mut protected_ranges: Vec<(usize, usize)> = Vec::new();
        for tok in &self.used_tokens {
            if !tok.is_empty() {
                for (idx, _) in text.match_indices(tok.as_str()) {
                    protected_ranges.push((idx, idx + tok.len()));
                }
            }
        }

        for (rule_idx, rule) in self.rules.iter().enumerate() {
            let mut at = 0usize;
            while at <= bytes.len() {
                let flags = if at > 0 { regexp::NOTBOL } else { 0 } | regexp::RUNAWAY;
                match rule.regex.exec(bytes, at, flags) {
                    Ok(Some((a, b))) if b > a => {
                        if text.is_char_boundary(a) && text.is_char_boundary(b) {
                            let overlaps_token =
                                protected_ranges.iter().any(|&(pa, pb)| a < pb && b > pa);
                            let boundary_ok = if rule.numeric_boundary {
                                let prev_digit = a > 0 && bytes[a - 1].is_ascii_digit();
                                let next_digit = b < bytes.len() && bytes[b].is_ascii_digit();
                                !prev_digit && !next_digit
                            } else {
                                true
                            };
                            if !overlaps_token && boundary_ok {
                                candidates.push(RawMatch {
                                    start: a,
                                    end: b,
                                    rule_idx,
                                });
                            }
                        }
                        at = next_char_boundary(text, a.max(b));
                        if b == a {
                            break;
                        }
                    }
                    Ok(Some((a, _))) => {
                        if a >= bytes.len() {
                            break;
                        }
                        at = next_char_boundary(text, a + 1);
                    }
                    _ => break,
                }
            }
        }

        candidates.sort_by(|x, y| {
            x.start
                .cmp(&y.start)
                .then_with(|| (y.end - y.start).cmp(&(x.end - x.start)))
                .then_with(|| x.rule_idx.cmp(&y.rule_idx))
        });

        let mut selected = Vec::new();
        let mut cursor = 0usize;
        for cand in candidates {
            if cand.start >= cursor {
                cursor = cand.end;
                selected.push(cand);
            }
        }
        selected
    }

    fn token_for(&mut self, category: &str, canonical_secret: &str, raw_matched: &str) -> String {
        let cat = sanitize_category(category);
        if !raw_matched.trim().is_empty() {
            self.observed_raw_secrets
                .insert(raw_matched.to_string(), cat.clone());
        }
        if !canonical_secret.trim().is_empty() {
            self.observed_raw_secrets
                .insert(canonical_secret.to_string(), cat.clone());
        }
        if let Some(&idx) = self.secret_to_entry.get(canonical_secret)
            && let Some(entry) = self.entries.get_mut(idx)
        {
            entry.occurrences += 1;
            return entry.token.clone();
        }

        let secret_char_len = canonical_secret.chars().count();
        // Every counter value formats to a distinct candidate, and each rejected
        // candidate equals a distinct literal, used token or the secret itself,
        // so this many attempts always find a free token.
        let max_attempts = self.literal_secrets.len() + self.used_tokens.len() + 2;
        let mut found = None;
        let mut last_n = 0;
        for _ in 0..max_attempts {
            let next_n = {
                let c = self.category_counters.entry(cat.clone()).or_insert(0);
                *c += 1;
                *c
            };
            last_n = next_n;
            let candidate = format_synthetic_token(&cat, next_n, secret_char_len);
            let clashes_literal = self.literal_secrets.iter().any(|s| s == &candidate);
            if !clashes_literal
                && candidate != canonical_secret
                && self.used_tokens.insert(candidate.clone())
            {
                found = Some(candidate);
                break;
            }
        }
        let token = found.unwrap_or_else(|| {
            let fallback = format!("[{cat}_{last_n}_{}]", self.used_tokens.len());
            self.used_tokens.insert(fallback.clone());
            fallback
        });

        let idx = self.entries.len();
        self.entries.push(VaultEntry {
            token: token.clone(),
            secret: canonical_secret.to_string(),
            category: cat,
            occurrences: 1,
        });
        self.secret_to_entry
            .insert(canonical_secret.to_string(), idx);
        token
    }

    fn has_match(&self, text: &str) -> bool {
        !self.find_matches(text).is_empty()
    }

    fn replace_in_str(&mut self, text: &str) -> Option<(String, usize)> {
        let matches = self.find_matches(text);
        if matches.is_empty() {
            return None;
        }
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;
        for m in &matches {
            out.push_str(&text[cursor..m.start]);
            let raw_slice = &text[m.start..m.end];
            let (cat, canonical) = match self.rules.get(m.rule_idx) {
                Some(rule) => (
                    rule.category.clone(),
                    rule.canonical_literal
                        .clone()
                        .unwrap_or_else(|| raw_slice.to_string()),
                ),
                None => ("SECRET".to_string(), raw_slice.to_string()),
            };
            let tok = self.token_for(&cat, &canonical, raw_slice);
            out.push_str(&tok);
            cursor = m.end;
        }
        out.push_str(&text[cursor..]);
        Some((out, matches.len()))
    }

    /// `(secret, category)` pairs to look for in the output.
    fn all_secrets_for_verification(&self) -> Vec<(String, String)> {
        let mut set: BTreeMap<String, String> = self.observed_raw_secrets.clone();
        for rule in &self.rules {
            if let Some(lit) = &rule.canonical_literal
                && !lit.trim().is_empty()
            {
                set.entry(lit.clone())
                    .or_insert_with(|| rule.category.clone());
            }
        }
        for entry in &self.entries {
            if !entry.secret.trim().is_empty() {
                set.entry(entry.secret.clone())
                    .or_insert_with(|| entry.category.clone());
            }
        }
        // Exclude synthetic tokens themselves so format-preserving tokens never self-flag.
        for tok in &self.used_tokens {
            set.remove(tok);
        }
        set.into_iter().collect()
    }
}

fn next_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn format_synthetic_token(category: &str, index: usize, secret_char_len: usize) -> String {
    match category {
        // Past the format's own width the number just grows a digit, so every
        // index still gives a distinct token.
        "SSN" => format!("000-00-{index:04}"),
        "EMAIL" => format!("u{index:02}@cloak.invalid"),
        "PHONE" => format!("555-010-{index:04}"),
        "CARD" => format!("0000-0000-0000-{index:04}"),
        "IBAN" => format!("XX00CLOAK{index:08}"),
        custom => {
            if secret_char_len <= 6 {
                let short: String = custom
                    .chars()
                    .find(|c| c.is_ascii_alphabetic())
                    .or_else(|| custom.chars().next())
                    .unwrap_or('X')
                    .to_string();
                format!("[{short}_{index:02}]")
            } else {
                format!("[{custom}_{index:02}]")
            }
        }
    }
}

/// Single-pass reverse token-to-secret replacer used by [`uncloak_pdf`] and
/// [`uncloak_text`].
#[derive(Debug)]
struct VaultReplacer {
    /// `(token, secret, entry_idx)` sorted by descending `token.len()`.
    rules: Vec<(String, String, Option<usize>)>,
    hit_entries: HashSet<usize>,
    token_replacements: usize,
}

impl VaultReplacer {
    fn for_pdf(vault: &CloakVault) -> Self {
        let mut rules: Vec<(String, String, Option<usize>)> = vault
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.token.is_empty() && e.token != e.secret)
            .map(|(idx, e)| (e.token.clone(), e.secret.clone(), Some(idx)))
            .collect();
        rules.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        VaultReplacer {
            rules,
            hit_entries: HashSet::new(),
            token_replacements: 0,
        }
    }

    fn for_text(vault: &CloakVault) -> Self {
        let mut replacer = Self::for_pdf(vault);
        if !vault.cloaked_doc_id.is_empty()
            && !vault.original_doc_id.is_empty()
            && vault.cloaked_doc_id != vault.original_doc_id
        {
            replacer.rules.push((
                vault.cloaked_doc_id.clone(),
                vault.original_doc_id.clone(),
                None,
            ));
            replacer
                .rules
                .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        }
        replacer
    }

    fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    fn has_match(&self, text: &str) -> bool {
        if self.rules.is_empty() || text.is_empty() {
            return false;
        }
        let mut pos = 0usize;
        while pos < text.len() {
            let tail = &text[pos..];
            if self
                .rules
                .iter()
                .any(|(tok, _, _)| tail.starts_with(tok.as_str()))
            {
                return true;
            }
            let step = tail.chars().next().map_or(1, char::len_utf8);
            pos += step;
        }
        false
    }

    fn replace_in_str(&mut self, text: &str) -> Option<(String, usize)> {
        if self.rules.is_empty() || text.is_empty() {
            return None;
        }
        let mut out = String::with_capacity(text.len());
        let mut pos = 0usize;
        let mut any_changed = false;
        let mut local_tokens = 0usize;

        while pos < text.len() {
            let tail = &text[pos..];
            if let Some((tok, secret, entry_idx)) = self
                .rules
                .iter()
                .find(|(tok, _, _)| tail.starts_with(tok.as_str()))
            {
                out.push_str(secret);
                pos += tok.len();
                any_changed = true;
                if let Some(idx) = *entry_idx {
                    self.hit_entries.insert(idx);
                    self.token_replacements += 1;
                    local_tokens += 1;
                }
            } else {
                let ch = tail.chars().next().unwrap_or('\0');
                let step = ch.len_utf8().max(1);
                out.push_str(&tail[..step.min(tail.len())]);
                pos += step;
            }
        }

        any_changed.then_some((out, local_tokens))
    }
}

trait TextTransform {
    fn is_empty(&self) -> bool;
    fn has_match(&self, text: &str) -> bool;
    fn transform(&mut self, text: &str) -> Option<(String, usize)>;
}

impl TextTransform for CloakSession {
    fn is_empty(&self) -> bool {
        CloakSession::is_empty(self)
    }

    fn has_match(&self, text: &str) -> bool {
        CloakSession::has_match(self, text)
    }

    fn transform(&mut self, text: &str) -> Option<(String, usize)> {
        self.replace_in_str(text)
    }
}

impl TextTransform for VaultReplacer {
    fn is_empty(&self) -> bool {
        VaultReplacer::is_empty(self)
    }

    fn has_match(&self, text: &str) -> bool {
        VaultReplacer::has_match(self, text)
    }

    fn transform(&mut self, text: &str) -> Option<(String, usize)> {
        self.replace_in_str(text)
    }
}

#[derive(Debug, Clone)]
struct SpanSlot {
    block_idx: usize,
    line_idx: usize,
    span_idx: usize,
    bbox: Rect,
}

#[derive(Debug, Clone)]
struct SpanReplacement {
    new_text: String,
    origin: Point,
    dir: Point,
    style: Style,
    avail_width: f32,
    erase_quads_pdf: Vec<Quad>,
    base14_name: &'static str,
    cjk_lang: CjkLang,
}

struct CloakContentHooks<'a, T: TextTransform> {
    quads: Vec<Quad>,
    xform: &'a mut T,
}

impl<T: TextTransform> Hooks for CloakContentHooks<'_, T> {
    fn filters_text(&self) -> bool {
        !self.quads.is_empty()
    }

    fn text_filter(
        &mut self,
        _ucs: &[i32],
        trm: Matrix,
        ctm: Matrix,
        bbox: Rect,
        _render: i32,
        _fill_alpha: f32,
        _stroke_alpha: f32,
    ) -> bool {
        let trm = trm.concat(ctm);
        let mut b = bbox.transform(trm);
        let w = b.x1 - b.x0;
        let h = b.y1 - b.y0;
        b.x0 += w / 10.0;
        b.x1 -= w / 10.0;
        b.y0 += h / 10.0;
        b.y1 -= h / 10.0;
        self.quads
            .iter()
            .any(|q| b.intersect(Rect::from_quad(*q)).is_valid())
    }

    fn marked_content(&mut self, props: &Object) -> Option<Object> {
        let Object::Dict(d) = props else {
            return None;
        };
        let mut d2 = (**d).clone();
        let mut changed = false;
        for key in [&b"ActualText"[..], b"Alt", b"E"] {
            if let Some(Object::String(s)) = d.get(key) {
                let decoded = crate::text_string::text_string(s);
                if let Some((replaced, _)) = self.xform.transform(&decoded) {
                    d2.put(key, crate::edit::text_string(&replaced));
                    changed = true;
                }
            }
        }
        changed.then(|| Object::Dict(Arc::new(d2)))
    }
}

fn chars_to_string(chars: &[StextChar]) -> String {
    let mut out = String::with_capacity(chars.len());
    for ch in chars {
        if let Some(c) = u32::try_from(ch.c).ok().and_then(char::from_u32) {
            out.push(c);
        }
    }
    out
}

fn collect_page_span_slots(stext_page: &StextPage) -> Vec<SpanSlot> {
    let mut slots = Vec::new();
    for (bi, block) in stext_page.blocks.iter().enumerate() {
        for_each_text_line_in_block(block, bi, &mut |b_idx, l_idx, line| {
            for (s_idx, span) in extract::spans(line).into_iter().enumerate() {
                let slice = line.chars.get(span.start..span.end).unwrap_or(&[]);
                let trimmed = trim_synthetic_edges(slice);
                let bbox = ink_bbox(trimmed, span.bbox);
                if bbox.is_valid() && !bbox.is_empty() {
                    slots.push(SpanSlot {
                        block_idx: b_idx,
                        line_idx: l_idx,
                        span_idx: s_idx,
                        bbox,
                    });
                }
            }
        });
    }
    slots
}

fn for_each_text_line_in_block(
    block: &Block,
    top_block_idx: usize,
    f: &mut impl FnMut(usize, usize, &StextLine),
) {
    match block {
        Block::Text(tb) => {
            for (li, line) in tb.lines.iter().enumerate() {
                f(top_block_idx, li, line);
            }
        }
        Block::Struct(sb) => {
            for nested in &sb.blocks {
                for_each_text_line_in_block(nested, top_block_idx, f);
            }
        }
        _ => {}
    }
}

fn compute_available_span_width(
    all_slots: &[SpanSlot],
    target_loc: (usize, usize, Option<usize>),
    origin: Point,
    dir: Point,
    span_bbox: Rect,
    mediabox: Rect,
) -> f32 {
    let (block_idx, line_idx, span_idx) = target_loc;
    let is_horizontal = dir.x > 0.9 && dir.y.abs() <= 0.1;
    if !is_horizontal {
        let diag = (span_bbox.x1 - span_bbox.x0)
            .hypot(span_bbox.y1 - span_bbox.y0)
            .max(4.0);
        return diag;
    }

    let page_right = (mediabox.x1 - 2.0).max(span_bbox.x1);
    let mut limit_x1 = page_right;
    let band_y0 = span_bbox.y0 + 1.5;
    let band_y1 = (span_bbox.y1 - 1.5).max(band_y0);

    for slot in all_slots {
        let same_target = match span_idx {
            Some(s_idx) => {
                slot.block_idx == block_idx && slot.line_idx == line_idx && slot.span_idx == s_idx
            }
            None => slot.block_idx == block_idx && slot.line_idx == line_idx,
        };
        if same_target {
            continue;
        }
        let overlaps_y = slot.bbox.y0 <= band_y1 && slot.bbox.y1 >= band_y0;
        if overlaps_y && slot.bbox.x0 >= span_bbox.x1 - 1.5 {
            limit_x1 = limit_x1.min(slot.bbox.x0 - 0.75);
        }
    }

    (limit_x1 - origin.x).max(4.0)
}

fn build_page_span_replacements<T: TextTransform>(
    stext_page: &StextPage,
    page_ctm: Matrix,
    xform: &mut T,
) -> Vec<SpanReplacement> {
    let all_slots = collect_page_span_slots(stext_page);
    let mut replacements = Vec::new();

    for (bi, block) in stext_page.blocks.iter().enumerate() {
        for_each_text_line_in_block(block, bi, &mut |b_idx, l_idx, line| {
            let spans = extract::spans(line);
            if spans.is_empty() {
                return;
            }

            let mut line_matched_in_span = false;
            let mut candidate_span_reps = Vec::new();

            for (s_idx, span) in spans.iter().enumerate() {
                let slice = line.chars.get(span.start..span.end).unwrap_or(&[]);
                let trimmed = trim_synthetic_edges(slice);
                if trimmed.is_empty() {
                    continue;
                }
                let span_text = chars_to_string(trimmed);
                if !xform.has_match(&span_text) {
                    continue;
                }
                let Some((new_text, _)) = xform.transform(&span_text) else {
                    continue;
                };
                line_matched_in_span = true;
                let bbox = ink_bbox(trimmed, span.bbox);
                let origin = trimmed[0].origin;
                let avail_width = compute_available_span_width(
                    &all_slots,
                    (b_idx, l_idx, Some(s_idx)),
                    origin,
                    line.dir,
                    bbox,
                    stext_page.mediabox,
                );
                let erase_quads_pdf = trimmed
                    .iter()
                    .filter(|ch| ch.flags & stext::char_flags::SYNTHETIC == 0)
                    .map(|ch| redact::quad_to_pdf(ch.quad, page_ctm))
                    .collect();
                candidate_span_reps.push(SpanReplacement {
                    new_text,
                    origin,
                    dir: line.dir,
                    style: span.style.clone(),
                    avail_width,
                    erase_quads_pdf,
                    base14_name: match_base14(&span.style),
                    cjk_lang: guess_cjk_lang(&span.style.font),
                });
            }

            if line_matched_in_span {
                replacements.extend(candidate_span_reps);
                return;
            }

            // Fallback when a secret straddles adjacent spans on the same line.
            let trimmed_line = trim_synthetic_edges(&line.chars);
            if trimmed_line.is_empty() {
                return;
            }
            let line_text = chars_to_string(trimmed_line);
            if !xform.has_match(&line_text) {
                return;
            }
            let Some((new_text, _)) = xform.transform(&line_text) else {
                return;
            };
            let bbox = ink_bbox(trimmed_line, line.bbox);
            let origin = trimmed_line[0].origin;
            let style = spans[0].style.clone();
            let avail_width = compute_available_span_width(
                &all_slots,
                (b_idx, l_idx, None),
                origin,
                line.dir,
                bbox,
                stext_page.mediabox,
            );
            let erase_quads_pdf = trimmed_line
                .iter()
                .filter(|ch| ch.flags & stext::char_flags::SYNTHETIC == 0)
                .map(|ch| redact::quad_to_pdf(ch.quad, page_ctm))
                .collect();
            replacements.push(SpanReplacement {
                new_text,
                origin,
                dir: line.dir,
                base14_name: match_base14(&style),
                cjk_lang: guess_cjk_lang(&style.font),
                style,
                avail_width,
                erase_quads_pdf,
            });
        });
    }

    replacements
}

fn rewrite_page_contents<T: TextTransform>(
    doc: &Document,
    xform: &mut T,
) -> Result<Vec<i32>, CloakError> {
    if xform.is_empty() {
        return Ok(Vec::new());
    }
    let page_count = doc.page_count().map_err(|e| CloakError {
        code: "input.page_tree",
        message: e.0,
    })?;

    let stext_opts = StextOptions {
        flags: stext::flags::ALL_LAYERS,
        clip: Rect::INFINITE,
    };

    let mut store = Store::default();
    let mut font_objects: HashMap<FontKey, i32> = HashMap::new();
    let mut tounicode_ref: Option<i32> = None;
    let mut modified_pages = Vec::new();

    for page_idx in 0..page_count {
        let page_obj = doc.page(page_idx).map_err(|e| CloakError {
            code: "input.page_tree",
            message: e.0,
        })?;
        let (_, page_ctm) = doc.page_transform(&page_obj).map_err(|e| CloakError {
            code: "input.page_tree",
            message: e.0,
        })?;
        let (res, next_store) = stext::extract_page(doc, &page_obj, stext_opts, store);
        store = next_store;
        let stext_page = res.map_err(|message| CloakError {
            code: "input.page_tree",
            message,
        })?;

        let replacements = build_page_span_replacements(&stext_page, page_ctm, xform);
        let marked_hit = page_has_matching_marked_content(doc, &page_obj, xform);
        if replacements.is_empty() && !marked_hit {
            continue;
        }

        let mut all_erase_quads = Vec::new();
        for rep in &replacements {
            all_erase_quads.extend(rep.erase_quads_pdf.iter().copied());
        }

        let clean_opts = content_clean::Options {
            recurse: false,
            instance_forms: true,
            ascii: true,
            newlines: false,
            sanitize: true,
        };
        let mut hooks = CloakContentHooks {
            quads: all_erase_quads,
            xform,
        };
        filter_page_contents(doc, page_idx, &clean_opts, &mut hooks).map_err(|message| {
            CloakError {
                code: "pdf.content_filter",
                message,
            }
        })?;

        if !replacements.is_empty() {
            let fresh_page = doc.page(page_idx).map_err(|e| CloakError {
                code: "input.page_tree",
                message: e.0,
            })?;
            let existing_fonts = doc
                .inherited(&fresh_page, b"Resources")
                .ok()
                .flatten()
                .map(|r| doc.resolve(&r))
                .and_then(|r| r.as_dict().cloned())
                .and_then(|res_d| {
                    doc.resolve(res_d.get(b"Font").unwrap_or(&Object::Null))
                        .as_dict()
                        .cloned()
                })
                .unwrap_or_default();

            let mut working_fonts = existing_fonts;
            let mut page_font_names: HashMap<FontKey, Vec<u8>> = HashMap::new();
            let mut new_page_fonts: Vec<(Vec<u8>, i32)> = Vec::new();
            let mut name_counter = 0usize;
            let mut layer = Vec::new();

            for rep in &replacements {
                if rep.new_text.is_empty() {
                    continue;
                }
                let (runs, _) =
                    encode_replacement_runs(&rep.new_text, rep.base14_name, rep.cjk_lang);
                if runs.is_empty() {
                    continue;
                }

                let adv_em = measure_runs_advance_em(&runs);
                let base_size = rep.style.size.max(1.0);
                let natural_width = adv_em * base_size;
                let draw_size = if natural_width > rep.avail_width && natural_width > 0.0 {
                    (base_size * (rep.avail_width / natural_width)).max(3.5)
                } else {
                    base_size
                };

                for run in &runs {
                    let key = match run {
                        crate::patch::EncodedRun::Byte { key, .. }
                        | crate::patch::EncodedRun::Utf16 { key, .. } => key,
                    };
                    if !page_font_names.contains_key(key) {
                        let obj_num = *font_objects
                            .entry(key.clone())
                            .or_insert_with(|| add_font_object(doc, key, &mut tounicode_ref));
                        let res_name = next_unique_font_res_name(&working_fonts, &mut name_counter);
                        working_fonts.put(&res_name, Object::Ref(ObjRef::new(obj_num, 0)));
                        new_page_fonts.push((res_name.clone(), obj_num));
                        page_font_names.insert(key.clone(), res_name);
                    }
                }

                let tm = baseline_text_matrix(rep.origin, rep.dir, page_ctm);
                let r = ((rep.style.argb >> 16) & 0xff) as f32 / 255.0;
                let g = ((rep.style.argb >> 8) & 0xff) as f32 / 255.0;
                let b = (rep.style.argb & 0xff) as f32 / 255.0;

                layer.extend_from_slice(b"q\nBT\n0 Tr\n");
                push_float(&mut layer, r);
                layer.push(b' ');
                push_float(&mut layer, g);
                layer.push(b' ');
                push_float(&mut layer, b);
                layer.extend_from_slice(b" rg\n");

                for fval in [tm.a, tm.b, tm.c, tm.d, tm.e, tm.f] {
                    push_float(&mut layer, fval);
                    layer.push(b' ');
                }
                layer.extend_from_slice(b"Tm\n");

                for run in &runs {
                    match run {
                        crate::patch::EncodedRun::Byte { key, bytes } => {
                            if let Some(res_name) = page_font_names.get(key) {
                                layer.push(b'/');
                                layer.extend_from_slice(res_name);
                                layer.push(b' ');
                                push_float(&mut layer, draw_size);
                                layer.extend_from_slice(b" Tf\n");
                                push_pdf_literal(&mut layer, bytes);
                                layer.extend_from_slice(b" Tj\n");
                            }
                        }
                        crate::patch::EncodedRun::Utf16 { key, units } => {
                            if let Some(res_name) = page_font_names.get(key) {
                                layer.push(b'/');
                                layer.extend_from_slice(res_name);
                                layer.push(b' ');
                                push_float(&mut layer, draw_size);
                                layer.extend_from_slice(b" Tf\n");
                                push_pdf_hex_utf16(&mut layer, units);
                                layer.extend_from_slice(b" Tj\n");
                            }
                        }
                    }
                }
                layer.extend_from_slice(b"ET\nQ\n");
            }

            if !layer.is_empty() {
                append_patch_layer(doc, page_idx, &new_page_fonts, layer).map_err(|message| {
                    CloakError {
                        code: "pdf.page_edit",
                        message,
                    }
                })?;
            }
        }

        let _ = crate::edit::edit_page(doc, page_idx, &mut |d| d.remove(b"Thumb"));
        modified_pages.push(page_idx);
    }

    Ok(modified_pages)
}

struct MarkedPropsCollector {
    state: ProcState,
    props: Vec<Object>,
}

impl Processor for MarkedPropsCollector {
    fn op(&mut self, op: Op<'_>) -> Result<(), String> {
        match op {
            Op::BeginMarkedProperties { raw: Some(r), .. }
            | Op::MarkedPointProperties { raw: Some(r), .. } => self.props.push(r.clone()),
            _ => {}
        }
        Ok(())
    }

    fn state(&mut self) -> &mut ProcState {
        &mut self.state
    }

    fn wants_soft_mask(&self) -> bool {
        false
    }
}

fn page_has_matching_marked_content<T: TextTransform>(
    doc: &Document,
    page: &Object,
    xform: &T,
) -> bool {
    let contents = doc.dict_get(page, b"Contents");
    let mut streams = Vec::new();
    match doc.resolve(&contents) {
        Object::Array(items) => streams.extend(items.iter().cloned()),
        Object::Null => return false,
        _ => streams.push(contents),
    }
    for stream_obj in streams {
        let mut collector = MarkedPropsCollector {
            state: ProcState::default(),
            props: Vec::new(),
        };
        let _ = content::process_raw_contents(doc, &stream_obj, &mut collector);
        for prop in &collector.props {
            if let Object::Dict(d) = prop {
                for key in [&b"ActualText"[..], b"Alt", b"E"] {
                    if let Some(Object::String(s)) = d.get(key) {
                        let text = crate::text_string::text_string(s);
                        if xform.has_match(&text) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Promotes any direct annotation/widget dictionaries in each page's `/Annots`
/// array into indirect objects so `fill::fill` and dictionary walks can mutate
/// them uniformly.
fn promote_direct_page_annots(doc: &Document) {
    let Ok(page_count) = doc.page_count() else {
        return;
    };
    for page_idx in 0..page_count {
        let Ok(page) = doc.page(page_idx) else {
            continue;
        };
        let annots_raw = doc.dict_get(&page, b"Annots");
        let Object::Array(items) = doc.resolve(&annots_raw) else {
            continue;
        };
        if !items.iter().any(|it| matches!(it, Object::Dict(_))) {
            continue;
        }
        let mut new_items = Vec::with_capacity(items.len());
        for item in items.iter() {
            match item {
                Object::Dict(d) => {
                    let num = doc.add_object(Object::Dict(Arc::clone(d)));
                    new_items.push(Object::Ref(ObjRef::new(num, 0)));
                }
                other => new_items.push(other.clone()),
            }
        }
        let new_arr = Object::Array(Arc::new(new_items));
        if let Object::Ref(r) = annots_raw {
            doc.alter_object(r.num, new_arr);
        } else {
            let _ = crate::edit::edit_page(doc, page_idx, &mut |d| {
                d.put(b"Annots", new_arr.clone());
            });
        }
    }
}

/// Rewrites AcroForm widget values and regenerates widget appearances via
/// [`crate::fill::fill`], returning the set of object numbers whose `/AP` was
/// freshly regenerated.
fn rewrite_acroform_widgets<T: TextTransform>(doc: &Document, xform: &mut T) -> HashSet<i32> {
    let mut regenerated = HashSet::new();
    if xform.is_empty() {
        return regenerated;
    }
    let Ok(page_count) = doc.page_count() else {
        return regenerated;
    };

    let mut seen_names = HashSet::new();
    let mut edits = Vec::new();
    let mut touched_pages = HashSet::new();

    for page_idx in 0..page_count {
        let Ok(page) = doc.page(page_idx) else {
            continue;
        };
        let (_, widgets) = annot::page_annots(doc, &page);
        for widget in widgets {
            let Ok(field_name) = form::name(doc, &widget) else {
                continue;
            };
            if field_name.is_empty() || !seen_names.insert(field_name.clone()) {
                continue;
            }
            let val = form::value(doc, &widget);
            if let Some((new_val, _)) = xform.transform(&val) {
                edits.push(fill::Edit::Set {
                    name: field_name,
                    value: new_val,
                });
                touched_pages.insert(page_idx);
            }
        }
    }

    if !edits.is_empty() {
        if let Ok(filled) = fill::fill(doc, &edits) {
            for made in &filled.made {
                if made.error.is_none() && made.object > 0 {
                    regenerated.insert(made.object);
                }
            }
        }
        for page_idx in touched_pages {
            let _ = crate::edit::edit_page(doc, page_idx, &mut |d| d.remove(b"Thumb"));
        }
    }

    regenerated
}

const DICT_TEXT_KEYS: &[&[u8]] = &[
    b"Title",
    b"Contents",
    b"T",
    b"TU",
    b"TM",
    b"Subj",
    b"RC",
    b"V",
    b"DV",
    b"CA",
    b"P",
    b"URI",
    b"Alt",
    b"ActualText",
    b"E",
    b"F",
    b"UF",
    b"Desc",
];

struct ShownTextCollector {
    state: ProcState,
    shown: Vec<Vec<u8>>,
}

impl Processor for ShownTextCollector {
    fn op(&mut self, op: Op<'_>) -> Result<(), String> {
        match op {
            Op::Show { text, .. }
            | Op::NextLineShow { text, .. }
            | Op::NextLineShowSpaced { text, .. } => self.shown.push(text.to_vec()),
            Op::ShowArray(Some(Object::Array(a))) => {
                let mut s = Vec::new();
                for item in a.iter() {
                    if let Object::String(t) = item {
                        s.extend_from_slice(t);
                    }
                }
                self.shown.push(s);
            }
            _ => {}
        }
        Ok(())
    }

    fn state(&mut self) -> &mut ProcState {
        &mut self.state
    }

    fn wants_soft_mask(&self) -> bool {
        false
    }
}

fn stream_shows_target<T: TextTransform>(doc: &Document, stream_obj: &Object, xform: &T) -> bool {
    let resolved = doc.resolve(stream_obj);
    if let Object::Stream(s) = &resolved
        && let Ok(data) = doc.stream_data(s)
        && xform.has_match(&String::from_utf8_lossy(&data))
    {
        return true;
    }
    let mut collector = ShownTextCollector {
        state: ProcState::default(),
        shown: Vec::new(),
    };
    let _ = content::process_raw_contents(doc, stream_obj, &mut collector);
    collector
        .shown
        .iter()
        .any(|bytes| xform.has_match(&String::from_utf8_lossy(bytes)))
}

fn appearance_shows_target<T: TextTransform>(doc: &Document, ap: &Object, xform: &T) -> bool {
    let Object::Dict(ap_dict) = doc.resolve(ap) else {
        return false;
    };
    ap_dict.iter().any(|(_, entry)| match doc.resolve(entry) {
        Object::Stream(_) => stream_shows_target(doc, entry, xform),
        Object::Dict(states) => states
            .iter()
            .any(|(_, st)| stream_shows_target(doc, st, xform)),
        _ => false,
    })
}

fn rewrite_dict_in_place<T: TextTransform>(
    doc: &Document,
    d: &mut Dict,
    obj_num: Option<i32>,
    regenerated_widgets: &HashSet<i32>,
    xform: &mut T,
    depth: usize,
) -> bool {
    if depth > 24 {
        return false;
    }
    let mut changed = false;

    let is_annot_or_field = d.get(b"Subtype").is_some_and(|s| {
        doc.resolve(s)
            .as_name()
            .is_some_and(|n| n != b"Form" && n != b"Image" && n != b"Type1" && n != b"TrueType")
    }) && d.get(b"Rect").is_some()
        || d.get(b"FT").is_some()
        || (d.get(b"Kids").is_some() && d.get(b"T").is_some());

    for &key in DICT_TEXT_KEYS {
        if key == b"P" && !matches!(d.get(key), Some(Object::String(_))) {
            continue;
        }
        if let Some(Object::String(s)) = d.get(key).cloned() {
            let decoded = crate::text_string::text_string(&s);
            if let Some((replaced, _)) = xform.transform(&decoded) {
                if key == b"URI" {
                    d.put(key, Object::String(Arc::from(replaced.into_bytes())));
                } else {
                    d.put(key, crate::edit::text_string(&replaced));
                }
                changed = true;
            }
        }
    }

    // Choice field options (`/Opt` array of strings or 2-element export/display arrays).
    if let Some(Object::Array(opt_arr)) = d.get(b"Opt").cloned()
        && let Some(new_opt) = rewrite_direct_object(doc, &Object::Array(opt_arr), xform, depth + 1)
    {
        d.put(b"Opt", new_opt);
        changed = true;
    }

    // Optional content layer names (`/Type /OCG /Name (...)`).
    if d.get(b"Type").is_some_and(|t| t.is_name(b"OCG"))
        && let Some(Object::String(s)) = d.get(b"Name").cloned()
    {
        let decoded = crate::text_string::text_string(&s);
        if let Some((replaced, _)) = xform.transform(&decoded) {
            d.put(b"Name", crate::edit::text_string(&replaced));
            changed = true;
        }
    }

    // Inline JavaScript strings (`/JS (...)`).
    if let Some(Object::String(s)) = d.get(b"JS").cloned() {
        let decoded = crate::text_string::text_string(&s);
        if let Some((replaced, _)) = xform.transform(&decoded) {
            d.put(b"JS", crate::edit::text_string(&replaced));
            changed = true;
        }
    }

    // Stale annotation/widget appearances not already regenerated by `fill::fill`.
    let was_regenerated = obj_num.is_some_and(|n| regenerated_widgets.contains(&n));
    if is_annot_or_field
        && !was_regenerated
        && let Some(ap) = d.get(b"AP").cloned()
        && appearance_shows_target(doc, &ap, xform)
    {
        d.remove(b"AP");
        changed = true;
    }

    // Recurse into direct child dictionaries and arrays (actions, mark-info, etc.).
    let child_keys: Vec<Vec<u8>> = d
        .iter()
        .filter(|(k, v)| {
            *k != b"Parent" && *k != b"Opt" && matches!(v, Object::Dict(_) | Object::Array(_))
        })
        .map(|(k, _)| k.to_vec())
        .collect();
    for key in child_keys {
        if let Some(child) = d.get(&key).cloned()
            && let Some(updated) = rewrite_direct_object(doc, &child, xform, depth + 1)
        {
            d.put(&key, updated);
            changed = true;
        }
    }

    changed
}

fn rewrite_direct_object<T: TextTransform>(
    doc: &Document,
    obj: &Object,
    xform: &mut T,
    depth: usize,
) -> Option<Object> {
    if depth > 24 {
        return None;
    }
    match obj {
        Object::String(s) => {
            let decoded = crate::text_string::text_string(s);
            let (replaced, _) = xform.transform(&decoded)?;
            Some(crate::edit::text_string(&replaced))
        }
        Object::Dict(d) => {
            let mut d2 = (**d).clone();
            let empty_regen = HashSet::new();
            rewrite_dict_in_place(doc, &mut d2, None, &empty_regen, xform, depth)
                .then(|| Object::Dict(Arc::new(d2)))
        }
        Object::Array(items) => {
            let mut changed = false;
            let mut out = Vec::with_capacity(items.len());
            for item in items.iter() {
                if let Some(updated) = rewrite_direct_object(doc, item, xform, depth + 1) {
                    out.push(updated);
                    changed = true;
                } else {
                    out.push(item.clone());
                }
            }
            changed.then(|| Object::Array(Arc::new(out)))
        }
        _ => None,
    }
}

fn collect_text_stream_nums(doc: &Document) -> HashSet<i32> {
    let mut text_streams = HashSet::new();
    let mut seen = HashSet::new();
    let trailer = Object::Dict(Arc::new(doc.trailer()));

    fn walk(
        doc: &Document,
        obj: &Object,
        in_text_slot: bool,
        seen: &mut HashSet<i32>,
        out: &mut HashSet<i32>,
        depth: usize,
    ) {
        if depth > 128 {
            return;
        }
        let (val, num) = match obj {
            Object::Ref(r) => {
                if in_text_slot {
                    // Record even if previously visited via another path.
                    if let Some(Object::Stream(_)) = doc.object(r.num) {
                        out.insert(r.num);
                    }
                }
                if !seen.insert(r.num) {
                    return;
                }
                match doc.object(r.num) {
                    Some(v) => (v, Some(r.num)),
                    None => return,
                }
            }
            v => (v.clone(), None),
        };
        match val {
            Object::Dict(d) => {
                for (k, v) in d.iter() {
                    if k == b"Parent" {
                        continue;
                    }
                    let next_text =
                        in_text_slot || matches!(k, b"Metadata" | b"JS" | b"EF" | b"F" | b"UF");
                    walk(doc, v, next_text, seen, out, depth + 1);
                }
            }
            Object::Stream(s) => {
                let is_typed_text = s
                    .dict
                    .get(b"Type")
                    .is_some_and(|t| t.is_name(b"Metadata") || t.is_name(b"EmbeddedFile"))
                    || s.dict.get(b"Subtype").is_some_and(|t| t.is_name(b"XML"));
                if (in_text_slot || is_typed_text)
                    && let Some(n) = num
                {
                    out.insert(n);
                }
                for (k, v) in s.dict.iter() {
                    if k == b"Parent" {
                        continue;
                    }
                    let next_text = matches!(k, b"Metadata" | b"JS" | b"EF" | b"F" | b"UF");
                    walk(doc, v, next_text, seen, out, depth + 1);
                }
            }
            Object::Array(items) => {
                for item in items.iter() {
                    walk(doc, item, in_text_slot, seen, out, depth + 1);
                }
            }
            _ => {}
        }
    }

    walk(doc, &trailer, false, &mut seen, &mut text_streams, 0);
    text_streams
}

fn rewrite_name_tree<T: TextTransform>(
    doc: &Document,
    node: &Object,
    xform: &mut T,
    visited: &mut HashSet<i32>,
    depth: usize,
) {
    if depth > 64 {
        return;
    }
    let Some(r) = node.as_ref() else {
        return;
    };
    // Each node is rewritten once: a `/Kids` cycle or a node shared by several
    // parents would otherwise be walked once per path to it.
    if !visited.insert(r.num) {
        return;
    }
    let Object::Dict(d) = doc.resolve(node) else {
        return;
    };
    if let Some(Object::Array(names)) = d.get(b"Names").map(|n| doc.resolve(n)) {
        let mut out = Vec::with_capacity(names.len());
        let mut changed = false;
        for pair in names.chunks(2) {
            if pair.len() == 2 {
                if let Object::String(s) = doc.resolve(&pair[0]) {
                    let decoded = crate::text_string::text_string(&s);
                    if let Some((replaced, _)) = xform.transform(&decoded) {
                        out.push(crate::edit::text_string(&replaced));
                        out.push(pair[1].clone());
                        changed = true;
                        continue;
                    }
                }
                out.extend_from_slice(pair);
            } else {
                out.extend_from_slice(pair);
            }
        }
        if changed {
            let arr = Object::Array(Arc::new(out));
            match d.get(b"Names") {
                Some(Object::Ref(a)) => doc.alter_object(a.num, arr),
                _ => {
                    let mut d2 = (*d).clone();
                    d2.put(b"Names", arr);
                    doc.alter_object(r.num, Object::Dict(Arc::new(d2)));
                }
            }
        }
    }
    if let Some(Object::Array(kids)) = d.get(b"Kids").map(|k| doc.resolve(k)) {
        for k in kids.iter() {
            rewrite_name_tree(doc, k, xform, visited, depth + 1);
        }
    }
}

fn rewrite_metadata_and_objects<T: TextTransform>(
    doc: &Document,
    regenerated_widgets: &HashSet<i32>,
    xform: &mut T,
) {
    if xform.is_empty() {
        return;
    }

    // 1. Trailer `/Info` dictionary (both indirect and direct).
    let mut trailer = doc.trailer();
    let mut handled_info_obj: Option<i32> = None;
    if let Some(info) = trailer.get(b"Info").cloned()
        && let Object::Dict(d) = doc.resolve(&info)
    {
        let mut d2 = (*d).clone();
        let mut info_changed = false;
        for (k, v) in d.iter() {
            if let Object::String(s) = doc.resolve(v) {
                let decoded = crate::text_string::text_string(&s);
                if let Some((replaced, _)) = xform.transform(&decoded) {
                    d2.put(k, crate::edit::text_string(&replaced));
                    info_changed = true;
                }
            }
        }
        match info {
            Object::Ref(r) => {
                handled_info_obj = Some(r.num);
                if info_changed {
                    doc.alter_object(r.num, Object::Dict(Arc::new(d2)));
                }
            }
            _ if info_changed => {
                trailer.put(b"Info", Object::Dict(Arc::new(d2)));
                doc.set_trailer(trailer.clone());
            }
            _ => {}
        }
    }

    // 2. Catalog `/Names` trees and legacy `/Dests` keys.
    let root = trailer.get(b"Root").cloned().unwrap_or(Object::Null);
    if let Object::Dict(names) = doc.dict_get(&root, b"Names") {
        let mut visited = HashSet::new();
        for (_, tree) in names.iter() {
            rewrite_name_tree(doc, tree, xform, &mut visited, 0);
        }
    }
    if let Some(dests) = doc
        .resolve(&root)
        .as_dict()
        .and_then(|c| c.get(b"Dests").cloned())
        && let (Some(r), Object::Dict(d)) = (dests.as_ref(), doc.resolve(&dests))
    {
        let mut d2 = Dict::new();
        let mut dest_changed = false;
        for (k, v) in d.iter() {
            let key_str = String::from_utf8_lossy(k);
            if let Some((replaced, _)) = xform.transform(&key_str) {
                d2.put(replaced.as_bytes(), v.clone());
                dest_changed = true;
            } else {
                d2.put(k, v.clone());
            }
        }
        if dest_changed {
            doc.alter_object(r.num, Object::Dict(Arc::new(d2)));
        }
    }

    // 3. Identify text-bearing streams (XMP `/Metadata`, `/JS`, `/EmbeddedFile`).
    let text_stream_nums = collect_text_stream_nums(doc);

    // 4. Rewrite dictionaries, arrays, strings, and text streams across xref.
    let xref_len = doc.xref_len() as i32;
    for num in 1..xref_len {
        if handled_info_obj == Some(num) {
            continue;
        }
        let Some(obj) = doc.object(num) else {
            continue;
        };
        match obj {
            Object::Dict(d) => {
                let mut d2 = (*d).clone();
                if rewrite_dict_in_place(doc, &mut d2, Some(num), regenerated_widgets, xform, 0) {
                    doc.alter_object(num, Object::Dict(Arc::new(d2)));
                }
            }
            Object::Stream(s) => {
                let mut d2 = s.dict.clone();
                let dict_changed =
                    rewrite_dict_in_place(doc, &mut d2, Some(num), regenerated_widgets, xform, 0);

                if text_stream_nums.contains(&num)
                    && let Ok(decoded_bytes) = doc.stream_data(&s)
                    && let Ok(utf8) = std::str::from_utf8(&decoded_bytes)
                    && let Some((replaced_stream, _)) = xform.transform(utf8)
                {
                    let new_bytes = replaced_stream.into_bytes();
                    d2.remove(b"Filter");
                    d2.remove(b"DecodeParms");
                    d2.put(b"Length", Object::Int(new_bytes.len() as i64));
                    doc.replace_stream(num, d2, new_bytes);
                } else if dict_changed && let Ok(raw_data) = doc.raw_stream_data(&s) {
                    doc.replace_stream(num, d2, raw_data.into_owned());
                }
            }
            Object::Array(_) => {
                if let Some(updated) = rewrite_direct_object(doc, &obj, xform, 0) {
                    doc.alter_object(num, updated);
                }
            }
            Object::String(s) => {
                let decoded = crate::text_string::text_string(&s);
                if let Some((replaced, _)) = xform.transform(&decoded) {
                    doc.alter_object(num, crate::edit::text_string(&replaced));
                }
            }
            _ => {}
        }
    }
}

/// Fails with `verify.leak` when any secret survives in `saved_bytes`. The
/// error names the class, page and category of what leaked, never the secret.
/// A locked output is read with `password`; one that cannot be read fails
/// with `verify.unreadable`, since nothing in it was checked.
fn verify_zero_secret_leaks(
    saved_bytes: &[u8],
    secrets_with_category: &[(String, String)],
    password: Option<&str>,
) -> Result<(), CloakError> {
    if secrets_with_category.is_empty() {
        return Ok(());
    }
    let secrets: Vec<String> = secrets_with_category
        .iter()
        .map(|(s, _)| s.clone())
        .collect();
    let secrets = secrets.as_slice();
    let reopened = Document::open(saved_bytes.to_vec()).map_err(|e| CloakError {
        code: "verify.reopen_failed",
        message: format!("failed to reopen cloaked PDF for leak check: {e}"),
    })?;
    if reopened.needs_password() {
        let opened = password.is_some_and(|p| reopened.authenticate(p).is_ok_and(|a| a != 0));
        if !opened {
            return Err(CloakError {
                code: "verify.unreadable",
                message: "the cloaked output needs a password to read back for the leak check: pass the document's password".to_string(),
            });
        }
    }

    let matcher = Matcher::new(secrets, &[], false).map_err(|e| CloakError {
        code: "verify.matcher",
        message: e,
    })?;
    let needles: Vec<(String, u32)> = secrets.iter().map(|s| (s.clone(), 0u32)).collect();
    let targets = verify::Targets {
        needles,
        matcher,
        areas: Vec::new(),
    };

    let verdict = verify::verify(&reopened, &targets);
    if !verdict.verified() {
        // `Finding::detail` quotes the needle, so it never reaches the message.
        let summary: Vec<String> = verdict
            .findings
            .iter()
            .take(3)
            .map(|f| {
                let categories: BTreeSet<&str> = secrets_with_category
                    .iter()
                    .filter(|(s, _)| {
                        f.detail.contains(s.as_str()) || f.detail.contains(&format!("{s:?}"))
                    })
                    .map(|(_, c)| c.as_str())
                    .collect();
                let category = if categories.is_empty() {
                    "unknown".to_string()
                } else {
                    categories.into_iter().collect::<Vec<_>>().join(",")
                };
                let mut where_ = String::new();
                if let Some(p) = f.page {
                    where_.push_str(&format!(" on page {p}"));
                }
                if let Some(o) = f.object {
                    where_.push_str(&format!(" in object {o}"));
                }
                format!("{}{where_} (category {category})", f.class.name())
            })
            .collect();
        return Err(CloakError {
            code: "verify.leak",
            message: format!(
                "zero-leak verification failed ({} finding(s)): {}",
                verdict.findings.len(),
                summary.join("; ")
            ),
        });
    }

    // Also verify full-page rendered text including synthesized/committed widget
    // and annotation appearances (`Parts::ALL` = contents + annots + widgets).
    let page_count = reopened.page_count().unwrap_or(0);
    let stext_opts = StextOptions {
        flags: stext::flags::ALL_LAYERS,
        clip: Rect::INFINITE,
    };
    let mut store = Store::default();
    for page_idx in 0..page_count {
        let Ok(page_obj) = reopened.page(page_idx) else {
            continue;
        };
        let (res, next_store) =
            stext::extract_page_parts(&reopened, &page_obj, stext_opts, store, Parts::ALL);
        store = next_store;
        if let Ok(full_page) = res {
            for (secret, category) in secrets_with_category {
                let hits = crate::search::search_page(&full_page, secret, 0)
                    .map_or(0, |found| found.len());
                if hits > 0 {
                    return Err(CloakError {
                        code: "verify.leak",
                        message: format!(
                            "zero-leak verification failed: {hits} hit(s) of a {category} secret still rendered on page {}",
                            page_idx + 1
                        ),
                    });
                }
            }
        }
    }

    Ok(())
}

/// Cloaks all matching PII entities across `doc` with deterministic,
/// format-preserving synthetic tokens, saves with full orphan garbage
/// collection (`garbage: 4`), reads the output back as every writer does
/// ([`write::save_verified`]), and verifies zero original secrets remain.
/// An incremental write is refused (`usage.incremental`): it would keep
/// the secrets in the bytes it appends to.
pub fn cloak_document(
    doc: &Document,
    targets: &[CloakTarget],
    write_opts: &WriteOptions,
) -> Result<(Vec<u8>, CloakVault, CloakReport), CloakError> {
    cloak_document_with_password(doc, targets, write_opts, None)
}

/// [`cloak_document`] for a document opened with `password`, which the
/// read-back and the zero-leak check need to read an output that stays
/// encrypted.
pub fn cloak_document_with_password(
    doc: &Document,
    targets: &[CloakTarget],
    write_opts: &WriteOptions,
    password: Option<&str>,
) -> Result<(Vec<u8>, CloakVault, CloakReport), CloakError> {
    let staged = stage_cloak(doc, targets, write_opts)?;
    let bytes =
        write::save_verified(&staged.work, &staged.write_options, password).map_err(save_error)?;
    let (vault, report) = staged.finish(&bytes, password)?;
    Ok((bytes, vault, report))
}

fn save_error(e: write::CheckedSaveError) -> CloakError {
    CloakError {
        code: e.code,
        message: e.message,
    }
}

/// A cloak's rewrites made on a fork of the input, for the caller to save
/// with [`StagedCloak::write_options`] (and read back) and hand the bytes
/// to [`StagedCloak::finish`], which runs the zero-leak check.
#[derive(Debug)]
pub struct StagedCloak {
    /// The fork the rewrites were made on: what to save, and whose
    /// diagnostics to report.
    pub work: Document,
    /// The options asked for, with `garbage: 4` so no orphaned stream keeps
    /// a secret.
    pub write_options: WriteOptions,
    session: CloakSession,
    original_doc_id: String,
}

/// The rewrites of [`cloak_document`], with nothing saved yet.
pub fn stage_cloak(
    doc: &Document,
    targets: &[CloakTarget],
    write_opts: &WriteOptions,
) -> Result<StagedCloak, CloakError> {
    let original_doc_id = DocId::of(doc.data()).to_string();
    let mut session = CloakSession::new(targets)?;

    let work = doc.fork_clean();
    if !session.is_empty() {
        promote_direct_page_annots(&work);
        rewrite_page_contents(&work, &mut session)?;
        let regenerated_widgets = rewrite_acroform_widgets(&work, &mut session);
        rewrite_metadata_and_objects(&work, &regenerated_widgets, &mut session);
    }
    let mut write_options = write_opts.clone();
    write_options.garbage = 4;
    Ok(StagedCloak {
        work,
        write_options,
        session,
        original_doc_id,
    })
}

impl StagedCloak {
    /// The vault and report for `bytes`, a save of [`StagedCloak::work`],
    /// once no secret is found in them (read with `password` when locked).
    pub fn finish(
        &self,
        bytes: &[u8],
        password: Option<&str>,
    ) -> Result<(CloakVault, CloakReport), CloakError> {
        let secrets_to_check = self.session.all_secrets_for_verification();
        verify_zero_secret_leaks(bytes, &secrets_to_check, password)?;

        let original_doc_id = self.original_doc_id.clone();
        let cloaked_doc_id = DocId::of(bytes).to_string();
        let entries = self.session.entries.clone();
        let replacements: usize = entries.iter().map(|e| e.occurrences).sum();
        let entities_cloaked = entries.len();
        let token_preview: Vec<CloakPreview> = entries
            .iter()
            .map(|e| CloakPreview {
                token: e.token.clone(),
                category: e.category.clone(),
                occurrences: e.occurrences,
            })
            .collect();

        let vault = CloakVault {
            schema: CLOAK_VAULT_SCHEMA.to_string(),
            original_doc_id: original_doc_id.clone(),
            cloaked_doc_id: cloaked_doc_id.clone(),
            entries,
        };
        let report = CloakReport {
            original_doc_id,
            cloaked_doc_id,
            replacements,
            entities_cloaked,
            verified_zero_leaks: true,
            token_preview,
        };
        Ok((vault, report))
    }
}

/// Reverses synthetic tokens back to original secrets across page content
/// streams, AcroForm values (including new values filled by an agent while the
/// PDF was cloaked), annotations, `/Info`, outlines, and XMP streams, saving
/// with `garbage: 4` and reading the output back as every writer does.
pub fn uncloak_pdf(
    cloaked_doc: &Document,
    vault: &CloakVault,
    write_opts: &WriteOptions,
) -> Result<(Vec<u8>, UncloakReport), CloakError> {
    uncloak_pdf_with_password(cloaked_doc, vault, write_opts, None)
}

/// [`uncloak_pdf`] for a document opened with `password`, which the
/// read-back needs when the output stays encrypted.
pub fn uncloak_pdf_with_password(
    cloaked_doc: &Document,
    vault: &CloakVault,
    write_opts: &WriteOptions,
    password: Option<&str>,
) -> Result<(Vec<u8>, UncloakReport), CloakError> {
    let staged = stage_uncloak(cloaked_doc, vault, write_opts)?;
    let bytes =
        write::save_verified(&staged.work, &staged.write_options, password).map_err(save_error)?;
    let report = staged.finish(&bytes);
    Ok((bytes, report))
}

/// An uncloak's rewrites made on a fork of the cloaked document, for the
/// caller to save with [`StagedUncloak::write_options`] and hand the bytes
/// to [`StagedUncloak::finish`].
#[derive(Debug)]
pub struct StagedUncloak {
    /// The fork the rewrites were made on: what to save, and whose
    /// diagnostics to report.
    pub work: Document,
    /// The options asked for, with `garbage: 4`.
    pub write_options: WriteOptions,
    replacer: VaultReplacer,
    original_doc_id: String,
    input_doc_id: String,
}

/// The rewrites of [`uncloak_pdf`], with nothing saved yet.
pub fn stage_uncloak(
    cloaked_doc: &Document,
    vault: &CloakVault,
    write_opts: &WriteOptions,
) -> Result<StagedUncloak, CloakError> {
    if vault.schema != CLOAK_VAULT_SCHEMA {
        return Err(CloakError {
            code: "input.bad_vault_schema",
            message: format!(
                "unsupported cloak vault schema {:?}; expected {CLOAK_VAULT_SCHEMA:?}",
                vault.schema
            ),
        });
    }

    let input_doc_id = DocId::of(cloaked_doc.data()).to_string();
    let mut replacer = VaultReplacer::for_pdf(vault);

    let work = cloaked_doc.fork_clean();
    if !replacer.is_empty() {
        promote_direct_page_annots(&work);
        rewrite_page_contents(&work, &mut replacer)?;
        let regenerated_widgets = rewrite_acroform_widgets(&work, &mut replacer);
        rewrite_metadata_and_objects(&work, &regenerated_widgets, &mut replacer);
    }
    let mut write_options = write_opts.clone();
    write_options.garbage = 4;
    Ok(StagedUncloak {
        work,
        write_options,
        replacer,
        original_doc_id: vault.original_doc_id.clone(),
        input_doc_id,
    })
}

impl StagedUncloak {
    /// The report for `bytes`, a save of [`StagedUncloak::work`].
    pub fn finish(&self, bytes: &[u8]) -> UncloakReport {
        UncloakReport {
            original_doc_id: self.original_doc_id.clone(),
            cloaked_doc_id: self.input_doc_id.clone(),
            restored_doc_id: DocId::of(bytes).to_string(),
            replacements: self.replacer.token_replacements,
            entities_restored: self.replacer.hit_entries.len(),
        }
    }
}

/// Reverses synthetic tokens back to original secrets in arbitrary text,
/// Markdown summaries, JSON reports, or `apply` edit plans, also rewriting
/// `vault.cloaked_doc_id` back to `vault.original_doc_id` when present.
///
/// Returns `(restored_text, token_replacements)`.
pub fn uncloak_text(input: &str, vault: &CloakVault) -> (String, usize) {
    let mut replacer = VaultReplacer::for_text(vault);
    match replacer.replace_in_str(input) {
        Some((restored, count)) => (restored, count),
        None => (input.to_string(), 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_obj(dict_extra: &str, data: &[u8]) -> String {
        let body: String = data.iter().map(|&b| b as char).collect();
        format!(
            "<<{dict_extra}/Length {}>>stream\n{body}\nendstream",
            data.len()
        )
    }

    fn extract_full_page_text(doc: &Document, page_idx: i32) -> String {
        let page = doc.page(page_idx).unwrap();
        let opts = StextOptions {
            flags: stext::flags::ALL_LAYERS,
            clip: Rect::INFINITE,
        };
        let (res, _) = stext::extract_page(doc, &page, opts, Store::default());
        let stext_page = res.unwrap();
        let mut out = String::new();
        for block in &stext_page.blocks {
            if let Block::Text(tb) = block {
                for line in &tb.lines {
                    out.push_str(&chars_to_string(&line.chars));
                    out.push('\n');
                }
            }
        }
        out
    }

    fn build_enterprise_pii_pdf() -> Document {
        let content = b"BT /F1 11 Tf 50 740 Td (Loan Agreement for Alice Vance and co-signer Bo Lin) Tj ET\n\
            BT /F1 11 Tf 50 718 Td (Primary SSN: 123-45-6789 | Backup SSN: 987-65-4321) Tj ET\n\
            BT /F1 11 Tf 50 696 Td (Confirm SSN 123-45-6789 for Alice Vance on Schedule A) Tj ET\n\
            BT /F1 11 Tf 50 674 Td (Email: alice.vance@acme.example | Phone: (415) 555-0199) Tj ET\n\
            BT /F1 11 Tf 50 652 Td (Card: 4532-0151-1283-0366 | IBAN: DE89 3704 0044 0532 0130 00) Tj ET";

        let xmp = b"<?xpacket begin='' id='W5M0MpCehiHzreSzNTczkc9d'?>\
            <x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>\
            <rdf:Description><dc:creator>Alice Vance</dc:creator><dc:description>SSN 123-45-6789</dc:description>\
            </rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end='w'?>";

        let objs = [
            // 1: Catalog with AcroForm, Outlines, and XMP Metadata
            "<</Type/Catalog/Pages 2 0 R/AcroForm 5 0 R/Outlines 10 0 R/Metadata 12 0 R>>"
                .to_string(),
            // 2: Pages
            "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
            // 3: Page 1 with widgets (6,7,8) and markup annotation (9)
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R\
              /Annots[6 0 R 7 0 R 8 0 R 9 0 R]\
              /Resources<</Font<</F1 14 0 R>>>>>>"
                .to_string(),
            // 4: Page content stream
            stream_obj("", content),
            // 5: AcroForm dictionary
            "<</Fields[6 0 R 7 0 R 8 0 R]/DA(/Helv 10 Tf 0 g)/DR<</Font<</Helv 14 0 R>>>>>>"
                .to_string(),
            // 6: Widget 1 - applicant_name
            "<</Type/Annot/Subtype/Widget/FT/Tx/T(applicant_name)/V(Alice Vance)/DV(Alice Vance)\
              /DA(/Helv 10 Tf 0 g)/Rect[50 580 280 602]>>"
                .to_string(),
            // 7: Widget 2 - applicant_ssn
            "<</Type/Annot/Subtype/Widget/FT/Tx/T(applicant_ssn)/V(123-45-6789)\
              /DA(/Helv 10 Tf 0 g)/Rect[300 580 500 602]>>"
                .to_string(),
            // 8: Widget 3 - agent_notes (initially blank; agent fills while cloaked)
            "<</Type/Annot/Subtype/Widget/FT/Tx/T(agent_notes)/V()\
              /DA(/Helv 10 Tf 0 g)/Rect[50 540 520 562]>>"
                .to_string(),
            // 9: Markup annotation with /T and /Contents containing PII
            "<</Type/Annot/Subtype/Text/Rect[50 500 70 520]/T(Alice Vance)\
              /Contents(Reach Alice Vance at (415) 555-0199 or alice.vance@acme.example)>>"
                .to_string(),
            // 10: Outlines root
            "<</Type/Outlines/First 11 0 R/Last 11 0 R/Count 1>>".to_string(),
            // 11: Outline item with PII
            "<</Title(Loan File: Alice Vance - 123-45-6789)/Parent 10 0 R/Dest[3 0 R /Fit]>>"
                .to_string(),
            // 12: XMP Metadata stream
            stream_obj("/Type/Metadata/Subtype/XML", xmp),
            // 13: Orphan unreferenced object holding raw PII (must be GC'd by garbage: 4)
            "<</OrphanNote(Historical secret for Alice Vance SSN 123-45-6789)>>".to_string(),
            // 14: Font
            "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>".to_string(),
            // 15: Info dictionary
            "<</Author(Alice Vance)/Title(Application 123-45-6789)/Subject(Contact alice.vance@acme.example)>>"
                .to_string(),
        ];

        let mut pdf = b"%PDF-1.7\n".to_vec();
        for (i, o) in objs.iter().enumerate() {
            pdf.extend(format!("{} 0 obj\n{}\nendobj\n", i + 1, o).bytes());
        }
        pdf.extend(
            format!(
                "trailer\n<</Root 1 0 R/Info 15 0 R/Size {}>>\n%%EOF\n",
                objs.len() + 1
            )
            .bytes(),
        );
        Document::open(pdf).unwrap()
    }

    #[test]
    fn multi_entity_pdf_cloak_agent_edit_and_uncloak_roundtrip() {
        let orig_doc = build_enterprise_pii_pdf();
        let targets = vec![
            CloakTarget::Preset("pii".to_string()),
            CloakTarget::Literal {
                category: "PERSON".to_string(),
                secret: "Alice Vance".to_string(),
            },
            CloakTarget::Literal {
                category: "PERSON".to_string(),
                secret: "Bo Lin".to_string(),
            },
        ];

        let (cloaked_bytes, vault, report) =
            cloak_document(&orig_doc, &targets, &WriteOptions::default()).unwrap();

        assert_eq!(vault.schema, CLOAK_VAULT_SCHEMA);
        assert!(report.verified_zero_leaks);
        assert_eq!(report.entities_cloaked, 8);
        assert!(report.replacements >= 16);

        // Verify stdout-safe CloakReport JSON contains ZERO secrets.
        let report_json = serde_json::to_string(&report).unwrap();
        let raw_cloaked_lossy = String::from_utf8_lossy(&cloaked_bytes);
        for secret in [
            "Alice Vance",
            "Bo Lin",
            "123-45-6789",
            "987-65-4321",
            "alice.vance@acme.example",
            "(415) 555-0199",
            "4532-0151-1283-0366",
            "DE89 3704 0044 0532 0130 00",
        ] {
            assert!(
                !report_json.contains(secret),
                "CloakReport leaked secret {secret:?}"
            );
            assert!(
                !raw_cloaked_lossy.contains(secret),
                "raw cloaked PDF bytes leaked secret {secret:?}"
            );
        }

        // Verify deterministic format-preserving tokens & coreference.
        let tok_for = |secret: &str| -> String {
            vault
                .entries
                .iter()
                .find(|e| e.secret == secret)
                .map(|e| e.token.clone())
                .unwrap()
        };
        let alice_tok = tok_for("Alice Vance");
        let bo_tok = tok_for("Bo Lin");
        let ssn1_tok = tok_for("123-45-6789");
        let ssn2_tok = tok_for("987-65-4321");
        let email_tok = tok_for("alice.vance@acme.example");
        let phone_tok = tok_for("(415) 555-0199");
        let card_tok = tok_for("4532-0151-1283-0366");
        let iban_tok = tok_for("DE89 3704 0044 0532 0130 00");

        assert_eq!(alice_tok, "[PERSON_01]");
        assert_eq!(bo_tok, "[P_02]");
        assert_eq!(ssn1_tok, "000-00-0001");
        assert_eq!(ssn2_tok, "000-00-0002");
        assert_eq!(email_tok, "u01@cloak.invalid");
        assert_eq!(phone_tok, "555-010-0001");
        assert_eq!(card_tok, "0000-0000-0000-0001");
        assert_eq!(iban_tok, "XX00CLOAK00000001");

        let cloaked_doc = Document::open(cloaked_bytes).unwrap();
        let cloaked_page_text = extract_full_page_text(&cloaked_doc, 0);
        assert!(cloaked_page_text.contains("[PERSON_01]"));
        assert!(cloaked_page_text.contains("[P_02]"));
        assert_eq!(cloaked_page_text.matches("000-00-0001").count(), 2);
        assert!(cloaked_page_text.contains("000-00-0002"));
        assert!(cloaked_page_text.contains("u01@cloak.invalid"));

        // Simulate an untrusted LLM agent inspecting the cloaked PDF, filling an
        // AcroForm field using synthetic tokens, and emitting a JSON report.
        let cloaked_page = cloaked_doc.page(0).unwrap();
        let (_, cloaked_widgets) = annot::page_annots(&cloaked_doc, &cloaked_page);
        let name_widget = cloaked_widgets
            .iter()
            .find(|w| form::name(&cloaked_doc, w).as_deref() == Ok("applicant_name"))
            .unwrap();
        assert_eq!(form::value(&cloaked_doc, name_widget), "[PERSON_01]");

        let agent_edited_doc = cloaked_doc.fork_clean();
        let _ = fill::fill(
            &agent_edited_doc,
            &[fill::Edit::Set {
                name: "agent_notes".to_string(),
                value:
                    "Underwriting approved for [PERSON_01] (SSN 000-00-0001) at u01@cloak.invalid"
                        .to_string(),
            }],
        );
        let agent_edited_bytes = write::save(&agent_edited_doc, &WriteOptions::default()).unwrap();
        let agent_reopened = Document::open(agent_edited_bytes).unwrap();

        let agent_json_plan = format!(
            "{{\"expect_doc_id\":\"{}\",\"summary\":\"Verified [PERSON_01] and [P_02]; SSN 000-00-0001; email u01@cloak.invalid\"}}",
            vault.cloaked_doc_id
        );
        let (restored_text, text_count) = uncloak_text(&agent_json_plan, &vault);
        assert_eq!(text_count, 4);
        assert!(restored_text.contains(&vault.original_doc_id));
        assert!(!restored_text.contains(&vault.cloaked_doc_id));
        assert!(restored_text.contains(
            "Verified Alice Vance and Bo Lin; SSN 123-45-6789; email alice.vance@acme.example"
        ));

        // Uncloak the agent-edited PDF and verify original PII + agent's new note are restored!
        let (restored_bytes, uncloak_report) =
            uncloak_pdf(&agent_reopened, &vault, &WriteOptions::default()).unwrap();
        assert_eq!(uncloak_report.entities_restored, 8);
        assert!(uncloak_report.replacements >= 17);

        let restored_doc = Document::open(restored_bytes).unwrap();
        let restored_page_text = extract_full_page_text(&restored_doc, 0);
        for expected in [
            "Alice Vance",
            "Bo Lin",
            "123-45-6789",
            "987-65-4321",
            "alice.vance@acme.example",
            "(415) 555-0199",
            "4532-0151-1283-0366",
            "DE89 3704 0044 0532 0130 00",
        ] {
            assert!(
                restored_page_text.contains(expected),
                "restored page text missing {expected:?}:\n{restored_page_text}"
            );
        }

        // Check restored AcroForm fields (including the agent-edited field!).
        let restored_page = restored_doc.page(0).unwrap();
        let (restored_annots, restored_widgets) = annot::page_annots(&restored_doc, &restored_page);
        let field_val = |target_name: &str| -> String {
            restored_widgets
                .iter()
                .find(|w| form::name(&restored_doc, w).as_deref() == Ok(target_name))
                .map(|w| form::value(&restored_doc, w))
                .unwrap()
        };
        assert_eq!(field_val("applicant_name"), "Alice Vance");
        assert_eq!(field_val("applicant_ssn"), "123-45-6789");
        assert_eq!(
            field_val("agent_notes"),
            "Underwriting approved for Alice Vance (SSN 123-45-6789) at alice.vance@acme.example"
        );

        // Check restored markup annotation `/T` and `/Contents`.
        assert_eq!(restored_annots.len(), 1);
        let Object::String(annot_t_bytes) =
            restored_doc.resolve(&restored_doc.dict_get(&restored_annots[0], b"T"))
        else {
            panic!("expected /T string");
        };
        let Object::String(annot_c_bytes) =
            restored_doc.resolve(&restored_doc.dict_get(&restored_annots[0], b"Contents"))
        else {
            panic!("expected /Contents string");
        };
        assert_eq!(
            crate::text_string::text_string(&annot_t_bytes),
            "Alice Vance"
        );
        assert_eq!(
            crate::text_string::text_string(&annot_c_bytes),
            "Reach Alice Vance at (415) 555-0199 or alice.vance@acme.example"
        );

        // Check restored `/Info` dictionary.
        let info_obj = restored_doc
            .resolve(restored_doc.trailer().get(b"Info").unwrap())
            .as_dict()
            .cloned()
            .unwrap();
        let Object::String(author_bytes) = restored_doc.resolve(info_obj.get(b"Author").unwrap())
        else {
            panic!("expected /Author string");
        };
        assert_eq!(
            crate::text_string::text_string(&author_bytes),
            "Alice Vance"
        );
    }

    #[test]
    fn neighbor_spans_never_collide_when_token_is_wider() {
        let content = b"BT /F2 12 Tf 72 700 Td (Al) Tj ET\n\
            BT /F1 12 Tf 92 700 Td (Status: Active) Tj ET";
        let objs = [
            "<</Type/Catalog/Pages 2 0 R>>".to_string(),
            "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R\
              /Resources<</Font<</F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>\
              /F2<</Type/Font/Subtype/Type1/BaseFont/Helvetica-Bold/Encoding/WinAnsiEncoding>>>>>>>>"
                .to_string(),
            stream_obj("", content),
        ];
        let refs: Vec<&str> = objs.iter().map(String::as_str).collect();
        let doc = crate::testutil::doc_of(&refs);

        let (cloaked_bytes, vault, report) = cloak_document(
            &doc,
            &[CloakTarget::Literal {
                category: "PERSON".to_string(),
                secret: "Al".to_string(),
            }],
            &WriteOptions::default(),
        )
        .unwrap();
        assert!(report.verified_zero_leaks);
        assert_eq!(vault.entries[0].token, "[P_01]");

        let reopened = Document::open(cloaked_bytes).unwrap();
        let page = reopened.page(0).unwrap();
        let (res, _) =
            stext::extract_page(&reopened, &page, StextOptions::default(), Store::default());
        let stext_page = res.unwrap();

        let mut token_rect: Option<Rect> = None;
        let mut status_rect: Option<Rect> = None;
        for block in &stext_page.blocks {
            if let Block::Text(tb) = block {
                for line in &tb.lines {
                    for span in extract::spans(line) {
                        let slice = &line.chars[span.start..span.end];
                        let trimmed = trim_synthetic_edges(slice);
                        let text = chars_to_string(trimmed);
                        let bbox = ink_bbox(trimmed, span.bbox);
                        if text.contains("[P_01]") {
                            token_rect = Some(bbox);
                        }
                        if text.contains("Status") {
                            status_rect = Some(bbox);
                        }
                    }
                }
            }
        }

        let t_rect = token_rect.expect("cloaked token span present");
        let s_rect = status_rect.expect("neighbor status span present");
        assert!(
            t_rect.x1 <= s_rect.x0 + 0.5,
            "expected cloaked token x1 ({}) <= neighbor x0 ({})",
            t_rect.x1,
            s_rect.x0
        );
    }

    #[test]
    fn invalid_inputs_return_structured_cloak_errors() {
        let objs = [
            "<</Type/Catalog/Pages 2 0 R>>",
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]>>",
        ];
        let doc = crate::testutil::doc_of(&objs);

        let bad_preset = cloak_document(
            &doc,
            &[CloakTarget::Preset("passport_unknown".to_string())],
            &WriteOptions::default(),
        )
        .unwrap_err();
        assert_eq!(bad_preset.code, "usage.bad_preset");

        let bad_regex = cloak_document(
            &doc,
            &[CloakTarget::Regex {
                category: "ACCT".to_string(),
                pattern: "[unclosed".to_string(),
            }],
            &WriteOptions::default(),
        )
        .unwrap_err();
        assert_eq!(bad_regex.code, "usage.bad_pattern");

        let empty_secret = cloak_document(
            &doc,
            &[CloakTarget::Literal {
                category: "PERSON".to_string(),
                secret: "   ".to_string(),
            }],
            &WriteOptions::default(),
        )
        .unwrap_err();
        assert_eq!(empty_secret.code, "usage.empty_secret");

        let bad_vault = CloakVault {
            schema: "nupdf.cloak.vault/v99".to_string(),
            original_doc_id: String::new(),
            cloaked_doc_id: String::new(),
            entries: Vec::new(),
        };
        let vault_err = uncloak_pdf(&doc, &bad_vault, &WriteOptions::default()).unwrap_err();
        assert_eq!(vault_err.code, "input.bad_vault_schema");
    }

    #[test]
    fn custom_regex_and_xmp_outline_roundtrip() {
        let content = b"BT /F1 11 Tf 50 720 Td (Wire to ACCT-884210 and backup ACCT-991004) Tj ET";
        let xmp = b"<x:xmpmeta><dc:title>Escrow ACCT-884210</dc:title></x:xmpmeta>";
        let objs = [
            "<</Type/Catalog/Pages 2 0 R/Outlines 5 0 R/Metadata 7 0 R>>".to_string(),
            "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R\
              /Resources<</Font<</F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>>>>>>>"
                .to_string(),
            stream_obj("", content),
            "<</Type/Outlines/First 6 0 R/Last 6 0 R/Count 1>>".to_string(),
            "<</Title(Schedule for ACCT-884210)/Parent 5 0 R/Dest[3 0 R /Fit]>>".to_string(),
            stream_obj("/Type/Metadata/Subtype/XML", xmp),
        ];
        let refs: Vec<&str> = objs.iter().map(String::as_str).collect();
        let doc = crate::testutil::doc_of(&refs);

        let (cloaked_bytes, vault, report) = cloak_document(
            &doc,
            &[CloakTarget::Regex {
                category: "ACCT".to_string(),
                pattern: r"ACCT-[0-9]{6}".to_string(),
            }],
            &WriteOptions::default(),
        )
        .unwrap();

        assert!(report.verified_zero_leaks);
        assert_eq!(report.entities_cloaked, 2);
        assert_eq!(vault.entries[0].token, "[ACCT_01]");
        assert_eq!(vault.entries[1].token, "[ACCT_02]");

        let cloaked_doc = Document::open(cloaked_bytes).unwrap();
        let (restored_bytes, uncloak_rep) =
            uncloak_pdf(&cloaked_doc, &vault, &WriteOptions::default()).unwrap();
        assert_eq!(uncloak_rep.entities_restored, 2);

        let restored_doc = Document::open(restored_bytes).unwrap();
        let page_text = extract_full_page_text(&restored_doc, 0);
        assert!(page_text.contains("ACCT-884210"));
        assert!(page_text.contains("ACCT-991004"));

        let root = restored_doc.trailer().get(b"Root").cloned().unwrap();
        let md_obj = restored_doc.resolve(&restored_doc.dict_get(&root, b"Metadata"));
        let Object::Stream(md_stream) = md_obj else {
            panic!("expected /Metadata stream");
        };
        let md_data = restored_doc.stream_data(&md_stream).unwrap();
        assert!(String::from_utf8_lossy(&md_data).contains("Escrow ACCT-884210"));

        let outlines = restored_doc.dict_get(&root, b"Outlines");
        let first_outline = restored_doc.dict_get(&outlines, b"First");
        let Object::String(title_bytes) =
            restored_doc.resolve(&restored_doc.dict_get(&first_outline, b"Title"))
        else {
            panic!("expected outline /Title string");
        };
        assert_eq!(
            crate::text_string::text_string(&title_bytes),
            "Schedule for ACCT-884210"
        );
    }

    #[test]
    fn capped_format_categories_never_run_out_of_tokens() {
        // More distinct secrets than the old four-digit format could express.
        let mut session = CloakSession::new(&[CloakTarget::Preset("phone".to_string())]).unwrap();
        let mut seen = HashSet::new();
        for n in 0..10_050usize {
            let secret = format!("212-{:03}-{:04}", n / 10_000, n % 10_000);
            let token = session.token_for("PHONE", &secret, &secret);
            assert!(seen.insert(token.clone()), "token {token} handed out twice");
        }
        for cat in ["SSN", "CARD"] {
            let mut seen = HashSet::new();
            for n in 0..10_001usize {
                let secret = format!("{cat}-secret-{n}");
                assert!(seen.insert(session.token_for(cat, &secret, &secret)));
            }
        }
    }

    #[test]
    fn cyclic_name_tree_is_walked_once() {
        let content = b"BT /F1 11 Tf 50 720 Td (Wire to ACCT-884210) Tj ET";
        let objs = [
            "<</Type/Catalog/Pages 2 0 R/Names<</Dests 5 0 R>>>>".to_string(),
            "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R\
              /Resources<</Font<</F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>>>>>>>"
                .to_string(),
            stream_obj("", content),
            // A node that lists itself twice and its child, which points back up.
            "<</Kids[5 0 R 5 0 R 6 0 R]>>".to_string(),
            "<</Kids[5 0 R 6 0 R]/Names[(ACCT-884210) [3 0 R /Fit]]>>".to_string(),
        ];
        let refs: Vec<&str> = objs.iter().map(String::as_str).collect();
        let doc = crate::testutil::doc_of(&refs);
        let targets = [CloakTarget::Regex {
            category: "ACCT".to_string(),
            pattern: r"ACCT-[0-9]{6}".to_string(),
        }];
        let (bytes, vault, report) =
            cloak_document(&doc, &targets, &WriteOptions::default()).unwrap();
        assert!(report.verified_zero_leaks);
        let cloaked = Document::open(bytes).unwrap();
        let (_, uncloaked) = uncloak_pdf(&cloaked, &vault, &WriteOptions::default()).unwrap();
        assert_eq!(uncloaked.entities_restored, 1);
    }

    #[test]
    fn leak_error_names_the_category_but_not_the_secret() {
        let content = b"BT /F1 11 Tf 50 720 Td (Wire to ACCT-884210) Tj ET";
        let objs = [
            "<</Type/Catalog/Pages 2 0 R>>".to_string(),
            "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R\
              /Resources<</Font<</F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>>>>>>>"
                .to_string(),
            stream_obj("", content),
        ];
        let refs: Vec<&str> = objs.iter().map(String::as_str).collect();
        let doc = crate::testutil::doc_of(&refs);
        let secrets = [("ACCT-884210".to_string(), "ACCT".to_string())];
        let err = verify_zero_secret_leaks(doc.data(), &secrets, None).unwrap_err();
        assert_eq!(err.code, "verify.leak");
        assert!(!err.to_string().contains("884210"), "{err}");
        assert!(err.message.contains("ACCT"), "{err}");
        assert!(err.message.contains("page 1"), "{err}");
    }

    #[test]
    fn leak_check_reads_a_locked_output_only_with_its_password() {
        // The page says "Encrypted"; the output opens only with "user".
        let bytes = include_bytes!("../../../corpus/smoke/encrypted-user-password.pdf");
        let secrets = [("Encrypted".to_string(), "WORD".to_string())];
        let unread = verify_zero_secret_leaks(bytes, &secrets, None).unwrap_err();
        assert_eq!(unread.code, "verify.unreadable");
        let wrong = verify_zero_secret_leaks(bytes, &secrets, Some("nope")).unwrap_err();
        assert_eq!(wrong.code, "verify.unreadable");
        let leak = verify_zero_secret_leaks(bytes, &secrets, Some("user")).unwrap_err();
        assert_eq!(leak.code, "verify.leak");
    }

    #[test]
    fn cloak_and_uncloak_read_their_output_back_as_every_writer_does() {
        let targets = [CloakTarget::Literal {
            category: "WORD".into(),
            secret: "Encrypted".into(),
        }];
        // An incremental write would keep the secrets in the bytes it
        // appends to, and garbage collection cannot be incremental.
        let doc = build_enterprise_pii_pdf();
        let incremental = WriteOptions {
            incremental: true,
            ..WriteOptions::default()
        };
        let refused = cloak_document(&doc, &targets, &incremental).unwrap_err();
        assert_eq!(refused.code, "usage.incremental", "{refused}");
        let vault = CloakVault {
            schema: CLOAK_VAULT_SCHEMA.into(),
            original_doc_id: String::new(),
            cloaked_doc_id: String::new(),
            entries: Vec::new(),
        };
        let refused = uncloak_pdf(&doc, &vault, &incremental).unwrap_err();
        assert_eq!(refused.code, "usage.incremental", "{refused}");

        // A locked output read back without its password is not the
        // document saved: the read-back fails before the leak check.
        let locked = Document::open(
            include_bytes!("../../../corpus/smoke/encrypted-user-password.pdf").to_vec(),
        )
        .unwrap();
        assert!(locked.authenticate("user").is_ok_and(|a| a != 0));
        let unread = cloak_document(&locked, &targets, &WriteOptions::default()).unwrap_err();
        assert_eq!(unread.code, "verify.failed", "{unread}");
        let unread = uncloak_pdf(&locked, &vault, &WriteOptions::default()).unwrap_err();
        assert_eq!(unread.code, "verify.failed", "{unread}");
        let (bytes, _, rep) =
            cloak_document_with_password(&locked, &targets, &WriteOptions::default(), Some("user"))
                .unwrap();
        assert!(rep.verified_zero_leaks);
        let reopened = Document::open(bytes).unwrap();
        assert!(reopened.authenticate("user").is_ok_and(|a| a != 0));
        uncloak_pdf_with_password(&reopened, &vault, &WriteOptions::default(), Some("user"))
            .unwrap();
    }

    #[test]
    fn uncloak_text_is_single_pass_without_cascading_collision() {
        let vault = CloakVault {
            schema: CLOAK_VAULT_SCHEMA.to_string(),
            original_doc_id: "b3:00000000000000000000000000000001".to_string(),
            cloaked_doc_id: "b3:00000000000000000000000000000002".to_string(),
            entries: vec![
                VaultEntry {
                    token: "[PERSON_1]".to_string(),
                    secret: "[PERSON_10]".to_string(),
                    category: "PERSON".to_string(),
                    occurrences: 1,
                },
                VaultEntry {
                    token: "[PERSON_10]".to_string(),
                    secret: "Dr. Evelyn Sato".to_string(),
                    category: "PERSON".to_string(),
                    occurrences: 1,
                },
            ],
        };

        let (restored, count) = uncloak_text("Pair: [PERSON_1] vs [PERSON_10]", &vault);
        assert_eq!(count, 2);
        assert_eq!(restored, "Pair: [PERSON_10] vs Dr. Evelyn Sato");
    }
}
