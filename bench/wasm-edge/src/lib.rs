// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Pure `wasm32-unknown-unknown` edge entrypoint for `nupdf`.
//!
//! Exposes a zero-import linear-memory ABI (`wasm_alloc`, `wasm_dealloc`,
//! `wasm_invoke`, `wasm_result_ptr`, `wasm_result_len`) so sandboxed browser,
//! V8 isolate, and Cloudflare Worker agents can run perceptual attestation,
//! vector chart decompilation, flat-form synthesis, surgical reflow, PII
//! cloaking, xref time-travel, compound pipelines, spatial queries, and claim
//! auditing with zero OS/WASI syscalls.

use std::cell::RefCell;

use nupdf::attest::{self, AttestOptions};
use nupdf::audit::{self, ClaimInput};
use nupdf::chart;
use nupdf::cloak::{self, CloakTarget};
use nupdf::flat_form;
use nupdf::history::{self, HistoryOptions};
use nupdf::patch::{self, PatchOptions, PatchSpec};
use nupdf::pipeline::{self, PipelineSpec};
use nupdf::query::{self, QuerySpec};
use nupdf::write::WriteOptions;
use nupdf::{DocId, Document};
use serde::Deserialize;

thread_local! {
    static LAST_RESULT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum EdgeCommand {
    Attest {
        dpi: Option<f32>,
    },
    Chart {
        pages: Option<String>,
    },
    FlatForms {
        pages: Option<String>,
    },
    Query {
        right_of: Option<String>,
        nearest_only: Option<bool>,
    },
    Patch {
        target_id: String,
        new_text: String,
        #[serde(default)]
        reflow: bool,
    },
    Cloak {
        presets: Vec<String>,
    },
    History {
        #[serde(default = "default_true")]
        run_attest: bool,
    },
    Pipeline {
        spec: PipelineSpec,
    },
    Audit {
        claims: Vec<ClaimInput>,
    },
}

fn default_true() -> bool {
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_alloc(len: usize) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// # Safety
/// `ptr` and `cap` must come from a prior `wasm_alloc(cap)` call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_dealloc(ptr: *mut u8, cap: usize) {
    if !ptr.is_null() && cap > 0 {
        // SAFETY: paired with wasm_alloc(cap) by caller.
        let _ = unsafe { Vec::from_raw_parts(ptr, 0, cap) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_result_ptr() -> *const u8 {
    LAST_RESULT.with(|r| r.borrow().as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_result_len() -> usize {
    LAST_RESULT.with(|r| r.borrow().len())
}

/// # Safety
/// `(pdf_ptr, pdf_len)` and `(cmd_ptr, cmd_len)` must point to valid slices in WASM linear memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_invoke(
    pdf_ptr: *const u8,
    pdf_len: usize,
    cmd_ptr: *const u8,
    cmd_len: usize,
) -> i32 {
    // SAFETY: slices allocated in WASM linear memory by host before call.
    let pdf_bytes = unsafe { std::slice::from_raw_parts(pdf_ptr, pdf_len) };
    let cmd_bytes = unsafe { std::slice::from_raw_parts(cmd_ptr, cmd_len) };

    let out = match run_edge(pdf_bytes, cmd_bytes) {
        Ok(val) => serde_json::to_vec(&serde_json::json!({ "ok": true, "result": val }))
            .unwrap_or_else(|_| b"{\"ok\":false,\"error\":\"serialize\"}".to_vec()),
        Err(err) => serde_json::to_vec(&serde_json::json!({ "ok": false, "error": err }))
            .unwrap_or_else(|_| b"{\"ok\":false,\"error\":\"failed\"}".to_vec()),
    };
    LAST_RESULT.with(|r| *r.borrow_mut() = out);
    0
}

fn run_edge(pdf_bytes: &[u8], cmd_bytes: &[u8]) -> Result<serde_json::Value, String> {
    let cmd: EdgeCommand = serde_json::from_slice(cmd_bytes).map_err(|e| e.to_string())?;
    let doc_id = DocId::of(pdf_bytes).to_string();
    let doc = Document::open(pdf_bytes).map_err(|e| e.message)?;
    match cmd {
        EdgeCommand::Attest { dpi } => {
            let opts = AttestOptions {
                dpi: dpi.unwrap_or(72.0),
                ..AttestOptions::default()
            };
            let rep = attest::attest_document(&doc, &opts, false)?;
            serde_json::to_value(rep).map_err(|e| e.to_string())
        }
        EdgeCommand::Chart { pages } => {
            let rep = chart::decompile_charts(&doc, pages.as_deref())?;
            serde_json::to_value(rep).map_err(|e| e.to_string())
        }
        EdgeCommand::FlatForms { pages } => {
            let rep = flat_form::detect_flat_forms(&doc, pages.as_deref())?;
            serde_json::to_value(rep).map_err(|e| e.to_string())
        }
        EdgeCommand::Query {
            right_of,
            nearest_only,
        } => {
            let spec = QuerySpec {
                right_of,
                nearest_only: nearest_only.unwrap_or(true),
                ..QuerySpec::default()
            };
            let rep = query::run_query(&doc, &spec)?;
            serde_json::to_value(rep).map_err(|e| e.to_string())
        }
        EdgeCommand::Patch {
            target_id,
            new_text,
            reflow,
        } => {
            let specs = [PatchSpec {
                target_id,
                new_text,
            }];
            let (saved, res) = patch::patch_and_verify_with_reflow(
                &doc,
                &specs,
                &PatchOptions::default(),
                reflow,
                &WriteOptions::default(),
                None,
            )
            .map_err(|e| e.message)?;
            Ok(serde_json::json!({
                "patched_bytes_len": saved.len(),
                "report": res,
            }))
        }
        EdgeCommand::Cloak { presets } => {
            let targets: Vec<CloakTarget> = presets.into_iter().map(CloakTarget::Preset).collect();
            let (saved, vault, rep) =
                cloak::cloak_document(&doc, &targets, &WriteOptions::default())
                    .map_err(|e| e.message)?;
            Ok(serde_json::json!({
                "cloaked_bytes_len": saved.len(),
                "vault_entries": vault.entries.len(),
                "report": rep,
            }))
        }
        EdgeCommand::History { run_attest } => {
            let rep = history::inspect_history(&doc, &HistoryOptions { run_attest })?;
            serde_json::to_value(rep).map_err(|e| e.to_string())
        }
        EdgeCommand::Pipeline { spec } => {
            let out = pipeline::run_pipeline(&doc, &spec, &WriteOptions::default())
                .map_err(|e| e.to_string())?;
            serde_json::to_value(out).map_err(|e| e.to_string())
        }
        EdgeCommand::Audit { claims } => {
            let rep = audit::audit_claims(&doc, &doc_id, &claims, false, 72.0)?;
            serde_json::to_value(rep).map_err(|e| e.to_string())
        }
    }
}
