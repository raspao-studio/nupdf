// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Runs the real `nupdf` binary against the smoke corpus.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

fn smoke(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../corpus/smoke")
        .join(name)
}

fn nupdf(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nupdf"))
        .args(args)
        .output()
        .expect("nupdf runs")
}

fn info(name: &str) -> Value {
    let path = smoke(name);
    let out = nupdf(&["info", path.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout is one JSON object")
}

#[test]
fn info_reports_document_facts() {
    let report = info("minimal.pdf");
    assert_eq!(report["schema"], "nupdf.info/v0");
    assert_eq!(report["kind"], "pdf");
    assert_eq!(report["pdf_version"], "1.7");
    assert_eq!(report["header_offset"], 0);
    assert_eq!(report["page_count"], 1);
    assert_eq!(report["object_count"], 6);
    assert_eq!(report["revisions"], 1);
    assert_eq!(report["repaired"], false);
    assert_eq!(report["encrypted"], false);
    assert_eq!(report["diagnostics"], Value::Array(vec![]));
    let size = std::fs::metadata(smoke("minimal.pdf")).unwrap().len();
    assert_eq!(report["file_size"], size);
}

#[test]
fn catalog_version_overrides_the_header() {
    let report = info("catalog-version.pdf");
    assert_eq!(report["pdf_version"], "1.7");
    assert_eq!(report["header_version"], "1.4");
}

#[test]
fn incremental_updates_count_as_revisions() {
    let report = info("incremental-update.pdf");
    assert_eq!(report["revisions"], 2);
    assert_eq!(report["object_count"], 7);
}

#[test]
fn object_streams_and_inherited_pages() {
    assert_eq!(info("xref-stream.pdf")["page_count"], 1);
    assert_eq!(info("multipage-inherit.pdf")["page_count"], 3);
}

#[test]
fn broken_xrefs_are_repaired_and_reported() {
    for name in ["broken-xref-offsets.pdf", "no-xref.pdf"] {
        let report = info(name);
        assert_eq!(report["repaired"], true, "{name}");
        assert_eq!(report["page_count"], 1, "{name}");
        let codes: Vec<&str> = report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["code"].as_str().unwrap())
            .collect();
        assert!(codes.contains(&"xref.repairing"), "{name}: {codes:?}");
    }
}

#[test]
fn unrepairable_files_fail_like_mupdf() {
    let path = smoke("truncated.pdf");
    let out = nupdf(&["info", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "input.unrepairable");
    assert_eq!(err["error"]["message"], "invalid key in dict");
}

#[test]
fn doc_id_is_stable_and_content_derived() {
    let a = info("minimal.pdf");
    let b = info("minimal.pdf");
    let c = info("version-2.0.pdf");
    assert_eq!(a["doc_id"], b["doc_id"]);
    assert_ne!(a["doc_id"], c["doc_id"]);
    assert!(a["doc_id"].as_str().unwrap().starts_with("b3:"));
}

#[test]
fn junk_before_header_is_a_diagnostic_not_a_failure() {
    let report = info("garbage-prefix.pdf");
    assert_eq!(report["header_offset"], 64);
    assert_eq!(report["pdf_version"], "1.6");
    assert_eq!(report["page_count"], 1);
    assert_eq!(report["repaired"], false);
    assert_eq!(report["diagnostics"][0]["code"], "header.garbage_prefix");
    assert_eq!(report["diagnostics"][0]["severity"], "warning");
}

#[test]
fn unreadable_file_is_an_input_error_on_stderr() {
    let out = nupdf(&["info", "/definitely/not/here.pdf"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(out.stdout.is_empty());
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["schema"], "nupdf.error/v0");
    assert_eq!(err["error"]["code"], "input.unreadable");
}

#[test]
fn non_pdf_is_rejected_with_a_stable_code() {
    // Not the smoke README: it mentions "%PDF-", which MuPDF's lenient
    // header search would accept.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let out = nupdf(&["info", manifest.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "input.not_pdf");
}

#[test]
fn usage_errors_exit_2() {
    assert_eq!(nupdf(&["frobnicate"]).status.code(), Some(2));
}

#[test]
fn pretty_flag_indents_output() {
    let path = smoke("minimal.pdf");
    let out = nupdf(&["--pretty", "info", path.to_str().unwrap()]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("{\n  \"schema\""));
}

fn run_json(args: &[&str]) -> Value {
    let out = nupdf(args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn pages_report_inherited_attributes() {
    let path = smoke("multipage-inherit.pdf");
    let report = run_json(&["pages", path.to_str().unwrap()]);
    assert_eq!(report["page_count"], 3);
    let pages = report["pages"].as_array().unwrap();
    assert_eq!(pages[0]["object"], "3 0 R");
    assert_eq!(pages[0]["MediaBox"], serde_json::json!([0, 0, 595, 842]));
    assert_eq!(pages[0]["Resources"]["keys"], serde_json::json!(["Font"]));
    assert_eq!(pages[1]["Rotate"], 90);
    assert_eq!(pages[2]["MediaBox"], serde_json::json!([0, 0, 300, 300]));
    assert_eq!(report["page_count_after"], 3);
}

#[test]
fn objects_dump_every_slot() {
    let path = smoke("xref-stream.pdf");
    let report = run_json(&["objects", path.to_str().unwrap()]);
    assert_eq!(report["object_count"], 8);
    let objects = report["objects"].as_object().unwrap();
    assert_eq!(objects.len(), 7);
    assert_eq!(objects["1"]["d"]["Type"]["n"], "Catalog");
    assert_eq!(objects["6"]["stream"]["d"]["Type"]["n"], "ObjStm");
    assert!(objects["6"]["raw_len"].as_u64().unwrap() > 0);
}

#[test]
fn every_report_carries_the_envelope() {
    let path = smoke("minimal.pdf");
    for command in ["info", "objects", "pages"] {
        let report = run_json(&[command, path.to_str().unwrap()]);
        assert!(
            report["schema"].as_str().unwrap().starts_with("nupdf."),
            "{command}"
        );
        assert!(
            report["doc_id"].as_str().unwrap().starts_with("b3:"),
            "{command}"
        );
        assert!(report["diagnostics"].is_array(), "{command}");
    }
}

#[test]
fn show_prints_one_object_and_its_stream() {
    let path = smoke("flate-content.pdf");
    let report = run_json(&["show", path.to_str().unwrap(), "4 0 R", "--data"]);
    assert_eq!(report["schema"], "nupdf.show/v0");
    assert_eq!(report["object"], "4 0 R");
    assert_eq!(report["value"]["d"]["Filter"]["n"], "FlateDecode");
    assert!(
        report["stream"]["data"]
            .as_str()
            .unwrap()
            .contains("(Compressed) Tj")
    );
    assert_eq!(report["stream"]["truncated"], false);

    let trailer = run_json(&["show", path.to_str().unwrap(), "trailer"]);
    assert_eq!(trailer["value"]["d"]["Root"]["ref"], "1 0 R");

    let missing = nupdf(&["show", path.to_str().unwrap(), "99"]);
    assert_eq!(missing.status.code(), Some(3));
    let bad = nupdf(&["show", path.to_str().unwrap(), "xyz"]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn info_reports_metadata_and_revisions() {
    let report = info("incremental-update.pdf");
    assert_eq!(report["metadata"]["Title"], "Revision 2");
    assert_eq!(report["metadata"]["Producer"], "nupdf fixtures");
    assert!(report["metadata"].get("Author").is_none());
    assert_eq!(report["revisions"], 2);
    assert_eq!(report["linearized"], false);
}
