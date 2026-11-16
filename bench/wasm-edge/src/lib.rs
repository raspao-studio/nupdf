// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Pure `wasm32-unknown-unknown` edge entrypoint for `nupdf`.
//!
//! Exposes a zero-import linear-memory ABI so sandboxed browser,
//! V8 isolate, and Cloudflare Worker agents can run perceptual attestation,
//! vector chart decompilation, flat-form synthesis, surgical reflow, PII
//! cloaking, xref time-travel, compound pipelines, spatial queries, and claim
//! auditing with zero OS/WASI syscalls.
//!
//! The module owns every buffer, so it never builds a slice from a pointer
//! the host passed. The host calls `wasm_pdf_buffer(len)` and
//! `wasm_cmd_buffer(len)`, writes the PDF and the JSON command into linear
//! memory at the addresses they return, calls `wasm_invoke()`, then reads
//! `wasm_result_len()` bytes at `wasm_result_ptr()`. An address stays valid
//! until the next call that sizes the same buffer; the host re-reads
//! `memory.buffer` after each call, since a call may grow memory.
//!
//! The only unsafe code is the export attribute (`#[unsafe(no_mangle)]`),
//! which an export cannot do without.

#![deny(unsafe_code)]

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
    static PDF_INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static CMD_INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
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

/// Sizes `buf` to `len` zero bytes and gives its address for the host to
/// write into.
fn reserve(buf: &'static std::thread::LocalKey<RefCell<Vec<u8>>>, len: usize) -> *mut u8 {
    buf.with(|b| {
        let mut b = b.borrow_mut();
        b.clear();
        b.resize(len, 0);
        b.as_mut_ptr()
    })
}

/// Address of a module-owned buffer of `len` bytes for the PDF.
// The export needs an unmangled symbol name; `no_mangle` is an unsafe attribute.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn wasm_pdf_buffer(len: usize) -> *mut u8 {
    reserve(&PDF_INPUT, len)
}

/// Address of a module-owned buffer of `len` bytes for the JSON command.
// The export needs an unmangled symbol name; `no_mangle` is an unsafe attribute.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn wasm_cmd_buffer(len: usize) -> *mut u8 {
    reserve(&CMD_INPUT, len)
}

/// Address of the last result's JSON.
// The export needs an unmangled symbol name; `no_mangle` is an unsafe attribute.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn wasm_result_ptr() -> *const u8 {
    LAST_RESULT.with(|r| r.borrow().as_ptr())
}

/// Length of the last result's JSON.
// The export needs an unmangled symbol name; `no_mangle` is an unsafe attribute.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn wasm_result_len() -> usize {
    LAST_RESULT.with(|r| r.borrow().len())
}

/// Runs the command in the command buffer on the PDF in the PDF buffer and
/// stores `{"ok": true, "result": ...}` or `{"ok": false, "error": ...}`.
// The export needs an unmangled symbol name; `no_mangle` is an unsafe attribute.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn wasm_invoke() -> i32 {
    let outcome =
        PDF_INPUT.with(|pdf| CMD_INPUT.with(|cmd| run_edge(&pdf.borrow(), &cmd.borrow())));
    let out = match outcome {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What the host does, with the writes into linear memory done through
    /// the module's own buffers.
    fn call(pdf: &[u8], cmd: &str) -> serde_json::Value {
        let pdf_at = wasm_pdf_buffer(pdf.len());
        PDF_INPUT.with(|b| {
            assert_eq!(b.borrow().as_ptr(), pdf_at.cast_const());
            b.borrow_mut().copy_from_slice(pdf);
        });
        wasm_cmd_buffer(cmd.len());
        CMD_INPUT.with(|b| b.borrow_mut().copy_from_slice(cmd.as_bytes()));
        assert_eq!(wasm_invoke(), 0);
        let len = wasm_result_len();
        let out = LAST_RESULT.with(|r| {
            assert_eq!(r.borrow().as_ptr(), wasm_result_ptr());
            r.borrow().clone()
        });
        assert_eq!(out.len(), len);
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn a_command_runs_on_the_buffers_the_module_owns() {
        let pdf = include_bytes!("../../../corpus/smoke/incremental-update.pdf");
        let out = call(pdf, r#"{"op": "history", "run_attest": false}"#);
        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(out["result"]["revision_count"], 2);
    }

    #[test]
    fn a_bad_command_is_an_error_result_not_a_trap() {
        let pdf = include_bytes!("../../../corpus/smoke/incremental-update.pdf");
        let out = call(pdf, r#"{"op": "nope"}"#);
        assert_eq!(out["ok"], false);
        let out = call(b"not a pdf", r#"{"op": "chart"}"#);
        assert_eq!(out["ok"], false);
        // A buffer resized smaller holds only what was asked for.
        let out = call(&pdf[..0], r#"{"op": "chart"}"#);
        assert_eq!(out["ok"], false);
    }
}
