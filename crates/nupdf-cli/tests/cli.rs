// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Runs the real `nupdf` binary against the smoke corpus.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

#[path = "support/schema_check.rs"]
mod schema_check;

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
    assert_eq!(report["schema"], "nupdf.info/v1");
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
    assert_eq!(err["schema"], "nupdf.error/v1");
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
    assert_eq!(report["schema"], "nupdf.show/v1");
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
    assert_eq!(report["revision_offsets"].as_array().unwrap().len(), 2);
    assert_eq!(report["linearized"], false);
}

#[test]
fn limit_errors_exit_5() {
    let path = smoke("minimal.pdf");
    let out = nupdf(&["--max-xref-entries", "3", "info", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(5));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "limit.xref_entries");
}

#[test]
fn decode_limits_surface_as_stream_errors() {
    let path = smoke("flate-content.pdf");
    let report = run_json(&[
        "--max-decoded-bytes",
        "4",
        "show",
        path.to_str().unwrap(),
        "4",
    ]);
    assert_eq!(
        report["stream"]["decode_error"],
        "decoded stream exceeds the size limit"
    );
}

#[test]
fn lazy_repairs_report_the_error_that_caused_them() {
    let report = info("broken-xref-offsets.pdf");
    let messages: Vec<&str> = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["message"].as_str().unwrap())
        .collect();
    assert!(
        messages.contains(&"syntax error: expected object number"),
        "{messages:?}"
    );
    assert!(messages.contains(&"repairing PDF document"), "{messages:?}");
}

#[test]
fn unencrypted_files_report_no_encryption() {
    let report = info("minimal.pdf");
    assert_eq!(report["encryption"], "None");
    assert_eq!(report["needs_password"], false);
}

#[test]
fn every_handler_revision_decrypts_with_the_empty_password() {
    for (name, encryption) in [
        ("encrypted-rc4-40.pdf", "Standard V1 R2 40-bit RC4"),
        ("encrypted-rc4-128.pdf", "Standard V2 R3 128-bit RC4"),
        (
            "encrypted-rc4-crypt-filter.pdf",
            "Standard V4 R4 128-bit RC4",
        ),
        ("encrypted-aes-128.pdf", "Standard V4 R4 128-bit AES"),
        (
            "encrypted-aes-128-clear-metadata.pdf",
            "Standard V4 R4 128-bit AES",
        ),
        ("encrypted-aes-256-r5.pdf", "Standard V5 R5 256-bit AES"),
        ("encrypted-aes-256.pdf", "Standard V5 R6 256-bit AES"),
    ] {
        let report = info(name);
        assert_eq!(report["encryption"], encryption, "{name}");
        assert_eq!(report["needs_password"], false, "{name}");
        assert_eq!(report["page_count"], 1, "{name}");
        assert_eq!(report["metadata"]["Title"], format!("Encrypted: {name}"));
        assert_eq!(report["metadata"]["Producer"], "nupdf fixtures");

        // The content stream decrypts, then inflates.
        let path = smoke(name);
        let shown = run_json(&["show", path.to_str().unwrap(), "4", "--data"]);
        let data = shown["stream"]["data"].as_str().unwrap();
        assert!(data.contains("(Encrypted) Tj"), "{name}: {data}");
    }
}

#[test]
fn a_user_password_opens_what_the_empty_one_does_not() {
    let path = smoke("encrypted-user-password.pdf");
    let file = path.to_str().unwrap();

    // Like MuPDF, nupdf still opens the file; pages are not counted.
    let locked = run_json(&["info", file]);
    assert_eq!(locked["needs_password"], true);
    assert_eq!(locked["page_count"], Value::Null);

    for password in ["user", "owner"] {
        let report = run_json(&["--password", password, "info", file]);
        assert_eq!(report["needs_password"], true);
        assert_eq!(report["page_count"], 1, "{password}");
        assert_eq!(
            report["metadata"]["Title"],
            "Encrypted: encrypted-user-password.pdf"
        );
    }

    let out = nupdf(&["--password", "wrong", "info", file]);
    assert_eq!(out.status.code(), Some(3));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "input.wrong_password");
}

#[test]
fn writing_a_locked_document_needs_its_password() {
    // As pdf_clean_file: the empty password failed, so nothing is written,
    // even where it left the right key behind.
    let path = smoke("encrypted-user-password.pdf");
    let file = path.to_str().unwrap();
    for argv in [
        &["clean", "--dry-run", file][..],
        &["decrypt", "--dry-run", file],
        &["meta", "--strip", "--dry-run", file],
    ] {
        let out = nupdf(argv);
        assert_eq!(out.status.code(), Some(3), "{argv:?}");
        let err: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err["error"]["code"], "input.needs_password", "{argv:?}");
    }
    let report = run_json(&["--password", "user", "decrypt", "--dry-run", file]);
    assert_eq!(report["verification"]["verified"], true);
}

#[test]
fn objects_report_decrypted_strings() {
    let path = smoke("encrypted-rc4-128.pdf");
    let report = run_json(&["objects", path.to_str().unwrap()]);
    let title = report["objects"]["6"]["d"]["Title"]["s"].as_str().unwrap();
    let expected: String = b"Encrypted: encrypted-rc4-128.pdf"
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(title, expected);
    // The /Encrypt dictionary itself is never decrypted.
    assert!(report["objects"]["7"]["d"]["O"]["s"].is_string());
}

#[test]
fn streams_report_decoded_lengths_and_hashes() {
    let path = smoke("flate-content.pdf");
    let report = run_json(&["streams", path.to_str().unwrap()]);
    assert_eq!(report["schema"], "nupdf.streams/v1");
    let content = &report["streams"]["4"];
    let data = b"BT /F1 24 Tf 72 720 Td (Compressed) Tj ET";
    assert_eq!(content["len"], data.len());
    assert_eq!(content["blake3"], blake3::hash(data).to_hex().to_string());
}

#[test]
fn image_streams_are_decoded_only_with_images() {
    let path = smoke("filter-dct-gray.pdf");
    let file = path.to_str().unwrap();
    let all = run_json(&["streams", file]);
    assert_eq!(all["streams"]["6"], serde_json::json!({ "image": true }));
    assert!(all["streams"]["4"]["len"].is_number());
    let images = run_json(&["streams", "--images", file]);
    let streams = images["streams"].as_object().unwrap();
    assert_eq!(streams.keys().collect::<Vec<_>>(), ["6"]);
    // 61 x 37 grayscale samples.
    assert_eq!(streams["6"]["len"], 61 * 37);
}

#[test]
fn permissions_follow_p_unless_the_owner_password_opened_it() {
    let path = smoke("encrypted-restricted.pdf");
    let file = path.to_str().unwrap();
    let user = run_json(&["info", file]);
    assert_eq!(user["permissions"]["print"], true);
    assert_eq!(user["permissions"]["accessibility"], true);
    for denied in ["edit", "copy", "annotate", "form", "assemble", "print-hq"] {
        assert_eq!(user["permissions"][denied], false, "{denied}");
    }
    let owner = run_json(&["--password", "owner", "info", file]);
    assert!(
        owner["permissions"]
            .as_object()
            .unwrap()
            .values()
            .all(|v| v == true)
    );
    assert!(info("minimal.pdf")["permissions"]["copy"] == true);
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cli");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn clean_writes_a_verified_copy_atomically() {
    let input = smoke("minimal.pdf");
    let output = scratch("clean-hello.pdf");
    let _ = std::fs::remove_file(&output);
    let report = run_json(&[
        "clean",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(report["schema"], "nupdf.clean/v1");
    assert_eq!(report["applied"], true);
    assert_eq!(report["verification"]["verified"], true);
    let written = std::fs::read(&output).unwrap();
    assert_eq!(report["output"]["len"], written.len());
    assert!(written.starts_with(b"%PDF-"));
    assert!(written.windows(18).any(|w| w == b"% Written by nupdf"));
    // The report's doc_id is the output's, as `info` sees it.
    assert_eq!(report["output"]["doc_id"], info_at(&output)["doc_id"]);
    // Nothing is left beside it. Other tests write to the same directory,
    // so only this output's temporary names count.
    let leftovers: Vec<_> = std::fs::read_dir(output.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with(".clean-hello.pdf.") && name.ends_with(".tmp")
        })
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

fn info_at(path: &std::path::Path) -> Value {
    run_json(&["info", path.to_str().unwrap()])
}

#[test]
fn clean_dry_run_writes_nothing() {
    let input = smoke("minimal.pdf");
    let output = scratch("clean-dry-run.pdf");
    let _ = std::fs::remove_file(&output);
    let report = run_json(&[
        "clean",
        "--dry-run",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(report["applied"], false);
    assert_eq!(report["verification"]["verified"], true);
    assert!(!output.exists());
    let again = run_json(&["clean", "--dry-run", input.to_str().unwrap()]);
    assert_eq!(again["output"]["blake3"], report["output"]["blake3"]);
    assert_eq!(again["output"]["path"], Value::Null);
}

#[test]
fn clean_needs_an_output_or_a_dry_run() {
    let input = smoke("minimal.pdf");
    let out = nupdf(&["clean", input.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}

#[test]
fn clean_refuses_output_that_reads_back_differently() {
    let input = smoke("write-lossy-name.pdf");
    let output = scratch("clean-lossy.pdf");
    let _ = std::fs::remove_file(&output);
    let out = nupdf(&[
        "clean",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(4));
    assert!(out.stdout.is_empty());
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "verify.failed");
    assert!(!output.exists());

    // --no-verify writes MuPDF's output regardless.
    let report = run_json(&[
        "clean",
        "--no-verify",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(report["verification"], Value::Null);
    assert!(output.exists());
}

#[test]
fn clean_collects_garbage_at_the_level_given() {
    let input = smoke("gc-reach.pdf");
    let output = scratch("clean-gc.pdf");
    let report = run_json(&[
        "clean",
        "-gg",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(report["garbage"], 2);
    assert_eq!(report["verification"]["verified"], true);
    assert_eq!(report["verification"]["objects"], 5);
    let written = std::fs::read(&output).unwrap();
    assert!(String::from_utf8_lossy(&written).contains("xref\n0 6\n"));

    // Levels past -gggg are -gggg.
    let report = run_json(&["clean", "-ggggg", "--dry-run", input.to_str().unwrap()]);
    assert_eq!(report["garbage"], 4);
    let report = run_json(&["clean", "--dry-run", input.to_str().unwrap()]);
    assert_eq!(report["garbage"], 0);
}

#[test]
fn clean_refuses_what_renumbering_breaks() {
    let input = smoke("gc-renumber.pdf");
    let out = nupdf(&["clean", "-gg", "--dry-run", input.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(4));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "verify.failed");
    assert_eq!(
        err["error"]["message"],
        "2 of 10 objects read back differently from the input: 8, 12"
    );
    let report = run_json(&["clean", "-g", "--dry-run", input.to_str().unwrap()]);
    assert_eq!(report["verification"]["verified"], true);
}

#[test]
fn clean_writes_streams_as_the_options_say() {
    let input = smoke("streams-raw.pdf");
    let path = input.to_str().unwrap();
    let report = run_json(&["clean", "-dz", "--dry-run", path]);
    assert_eq!(report["streams"]["decompress"], true);
    assert_eq!(report["streams"]["compress"], true);
    assert_eq!(report["syntax"], "compact");
    assert_eq!(report["verification"]["verified"], true);
    // -d and -a turn on a line per dictionary entry, unless -z or -t.
    for (args, syntax) in [
        (&["-d"][..], "pretty"),
        (&["-a"], "pretty"),
        (&["-d", "-t"], "compact"),
        (&["-tt"], "pretty"),
        (&["-z"], "compact"),
    ] {
        let mut argv = vec!["clean", "--dry-run", path];
        argv.extend_from_slice(args);
        assert_eq!(run_json(&argv)["syntax"], syntax, "{args:?}");
    }
    let report = run_json(&["clean", "-i", "-e", "250", "--dry-run", path]);
    assert_eq!(report["streams"]["compress_images"], true);
    assert_eq!(report["streams"]["effort"], 100);
}

#[test]
fn clean_packs_objects_into_object_streams() {
    let input = smoke("multipage-inherit.pdf");
    let path = input.to_str().unwrap();
    let report = run_json(&["clean", "-Z", "--dry-run", path]);
    assert_eq!(report["objstms"], true);
    assert_eq!(report["verification"]["verified"], true);
    let input = smoke("objstm-gap.pdf");
    let out = nupdf(&["clean", "-Z", "--dry-run", input.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(
        err["error"]["message"],
        "cannot find object in xref (6 0 R)"
    );
}

#[test]
fn clean_changes_the_security_handler() {
    let input = smoke("encrypted-rc4-128.pdf");
    let path = input.to_str().unwrap();
    let report = run_json(&["clean", "-D", "--dry-run", path]);
    assert_eq!(report["encryption"], "none");
    assert_eq!(report["verification"]["verified"], true);
    let output = scratch("clean-encrypted.pdf");
    let report = run_json(&[
        "clean",
        "-E",
        "aes-256",
        "-O",
        "own",
        "-U",
        "usr",
        "-P",
        "-4",
        path,
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(report["encryption"]["method"], "aes-256");
    assert_eq!(report["verification"]["verified"], true);
    let reopened = run_json(&["--password", "usr", "info", output.to_str().unwrap()]);
    assert_eq!(reopened["encryption"], "Standard V5 R6 256-bit AES");
    // -D and -E conflict; an unknown method is a usage error.
    let out = nupdf(&["clean", "-D", "-E", "rc4-40", "--dry-run", path]);
    assert_eq!(out.status.code(), Some(2));
    let out = nupdf(&["clean", "-E", "rot13", "--dry-run", path]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn clean_keeps_the_pages_given() {
    let input = smoke("pages-rearrange.pdf");
    let path = input.to_str().unwrap();
    let report = run_json(&["clean", "--pages", "4-3,1", "--dry-run", path]);
    assert_eq!(report["pages"], serde_json::json!([4, 3, 1]));
    assert_eq!(report["verification"]["verified"], true);
    let output = scratch("clean-pages.pdf");
    run_json(&[
        "clean",
        "--pages",
        "N",
        path,
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(info_at(&output)["page_count"], 1);
}

#[test]
fn clean_refuses_brotli() {
    let input = smoke("streams-raw.pdf");
    let out = nupdf(&["clean", "-zz", "--dry-run", input.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "usage.unsupported");
}

#[test]
fn clean_refuses_streams_that_editing_shared_filters_breaks() {
    // -z takes ASCIIHexDecode off an array two streams share, so the
    // second stream's hex data is read as flate (MuPDF writes the same).
    let input = smoke("streams-hex.pdf");
    let out = nupdf(&["clean", "-z", "--dry-run", input.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(4));
    let report = run_json(&[
        "clean",
        "-z",
        "--dry-run",
        "--no-verify",
        input.to_str().unwrap(),
    ]);
    assert_eq!(report["verification"], Value::Null);
}

#[test]
fn clean_keeps_encryption_and_the_password_reopens_it() {
    let input = smoke("encrypted-user-password.pdf");
    let output = scratch("clean-user-password.pdf");
    let report = run_json(&[
        "--password",
        "user",
        "clean",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(report["verification"]["verified"], true);
    let reopened = run_json(&["--password", "user", "info", output.to_str().unwrap()]);
    assert_eq!(reopened["encryption"], "Standard V5 R6 256-bit AES");
    assert_eq!(
        reopened["metadata"]["Title"],
        "Encrypted: encrypted-user-password.pdf"
    );
}

#[test]
fn clean_reports_an_unwritable_output() {
    let input = smoke("minimal.pdf");
    let output = scratch("no-such-dir/out.pdf");
    let out = nupdf(&[
        "clean",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(3));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "output.unwritable");
}

fn pages_at(path: &std::path::Path) -> Value {
    run_json(&["pages", path.to_str().unwrap()])
}

#[test]
fn meta_reads_sets_and_strips_the_information_dictionary() {
    let input = smoke("incremental-update.pdf");
    let file = input.to_str().unwrap();
    let report = run_json(&["meta", file]);
    assert_eq!(report["schema"], "nupdf.meta/v1");
    assert_eq!(report["info"]["Title"], "Revision 2");
    assert_eq!(report["xmp"], false);

    let output = scratch("meta-set.pdf");
    let out = output.to_str().unwrap();
    let report = run_json(&[
        "meta",
        file,
        "--set",
        "info:Title=Été",
        "--set",
        "Subject=a=b",
        "-o",
        out,
    ]);
    assert_eq!(report["applied"], true);
    assert_eq!(report["verification"]["verified"], true);
    let written = run_json(&["meta", out]);
    assert_eq!(written["info"]["Title"], "Été");
    assert_eq!(written["info"]["Subject"], "a=b");
    assert_eq!(written["info"]["Producer"], "nupdf fixtures");

    // --strip drops /Info and the catalog's XMP before --set adds to a new
    // /Info.
    let xmp = smoke("streams-raw.pdf");
    assert_eq!(run_json(&["meta", xmp.to_str().unwrap()])["xmp"], true);
    let output = scratch("meta-strip.pdf");
    let out = output.to_str().unwrap();
    run_json(&[
        "meta",
        xmp.to_str().unwrap(),
        "--strip",
        "--set",
        "Title=t",
        "-o",
        out,
    ]);
    let written = run_json(&["meta", out]);
    assert_eq!(written["xmp"], false);
    assert_eq!(written["info"], serde_json::json!({ "Title": "t" }));

    for (argv, code) in [
        (&["meta", file, "--set", "Title"][..], "usage.bad_value"),
        (&["meta", file, "--strip"], "usage.missing_output"),
    ] {
        let out = nupdf(argv);
        assert_eq!(out.status.code(), Some(2), "{argv:?}");
        let err: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err["error"]["code"], code);
    }
}

#[test]
fn pages_selects_deletes_and_rotates() {
    // Pages 3 and 4 inherit /Rotate 90 from their /Pages node.
    let input = smoke("pages-rearrange.pdf");
    let file = input.to_str().unwrap();
    let boxes = |report: &Value| -> Vec<(i64, i64)> {
        report["pages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["MediaBox"][2].as_i64().unwrap(),
                    p["Rotate"].as_i64().unwrap_or(0),
                )
            })
            .collect()
    };
    assert_eq!(
        boxes(&run_json(&["pages", file])),
        [(612, 0), (612, 0), (300, 90), (300, 90)]
    );

    let output = scratch("pages-select.pdf");
    let out = output.to_str().unwrap();
    let report = run_json(&[
        "pages", file, "--select", "4,1", "--rotate", "-90", "-o", out,
    ]);
    assert_eq!(report["schema"], "nupdf.pages/v1");
    assert_eq!(report["kept"], serde_json::json!([4, 1]));
    assert_eq!(report["page_count"], 2);
    assert_eq!(report["verification"]["verified"], true);
    assert_eq!(boxes(&pages_at(&output)), [(300, 0), (612, 270)]);

    let output = scratch("pages-delete.pdf");
    let out = output.to_str().unwrap();
    let report = run_json(&[
        "pages",
        file,
        "--delete",
        "2-3",
        "--rotate",
        "450",
        "--rotate-pages",
        "N",
        "-o",
        out,
    ]);
    assert_eq!(report["kept"], serde_json::json!([1, 4]));
    assert_eq!(boxes(&pages_at(&output)), [(612, 0), (300, 180)]);

    for argv in [
        &["pages", file, "--rotate", "45", "--dry-run"][..],
        &["pages", file, "--select", "1", "--delete", "2", "--dry-run"],
        &["pages", file, "--select", "1"],
    ] {
        assert_eq!(nupdf(argv).status.code(), Some(2), "{argv:?}");
    }
}

#[test]
fn split_writes_each_range_to_its_own_file() {
    let input = smoke("pages-rearrange.pdf");
    let file = input.to_str().unwrap();
    let dir = scratch("split-ranges");
    let _ = std::fs::remove_dir_all(&dir);
    let out = dir.to_str().unwrap();

    let report = run_json(&["split", file, "-o", out, "--dry-run"]);
    assert_eq!(report["schema"], "nupdf.split/v1");
    assert_eq!(report["applied"], false);
    assert_eq!(report["parts"].as_array().unwrap().len(), 4);
    assert!(!dir.exists());

    let report = run_json(&["split", file, "-o", out, "--pages-per-file", "3"]);
    let pages: Vec<&Value> = report["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["pages"])
        .collect();
    assert_eq!(
        pages,
        [&serde_json::json!([1, 2, 3]), &serde_json::json!([4])]
    );
    let second = dir.join("pages-rearrange-2.pdf");
    assert_eq!(pages_at(&second)["page_count"], 1);

    let report = run_json(&["split", file, "-o", out, "--ranges", "N-3;1,1"]);
    assert_eq!(report["parts"][0]["pages"], serde_json::json!([4, 3]));
    assert_eq!(report["parts"][1]["pages"], serde_json::json!([1, 1]));
    for part in report["parts"].as_array().unwrap() {
        assert_eq!(part["verification"]["verified"], true);
    }
}

#[test]
fn encrypt_and_decrypt_round_trip() {
    let input = smoke("minimal.pdf");
    let locked = scratch("encrypt-locked.pdf");
    let report = run_json(&[
        "encrypt",
        input.to_str().unwrap(),
        "--method",
        "aes-128",
        "--user-password",
        "u",
        "--owner-password",
        "o",
        "--permissions",
        "-4",
        "-o",
        locked.to_str().unwrap(),
    ]);
    assert_eq!(report["schema"], "nupdf.encrypt/v1");
    assert_eq!(report["encryption"]["method"], "aes-128");
    assert_eq!(report["verification"]["verified"], true);
    let facts = info_at(&locked);
    assert_eq!(facts["encryption"], "Standard V4 R4 128-bit AES");
    assert_eq!(facts["needs_password"], true);

    let open = scratch("encrypt-open.pdf");
    let report = run_json(&[
        "--password",
        "o",
        "decrypt",
        locked.to_str().unwrap(),
        "-o",
        open.to_str().unwrap(),
    ]);
    assert_eq!(report["schema"], "nupdf.decrypt/v1");
    assert_eq!(report["encryption"], "none");
    let facts = info_at(&open);
    assert_eq!(facts["encrypted"], false);
    assert_eq!(facts["page_count"], 1);

    let out = nupdf(&[
        "encrypt",
        input.to_str().unwrap(),
        "--method",
        "rot13",
        "--dry-run",
    ]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn merge_grafts_each_inputs_pages_into_a_new_document() {
    let a = smoke("minimal.pdf");
    let b = smoke("multipage-inherit.pdf");
    let output = scratch("merge.pdf");
    let report = run_json(&[
        "merge",
        "-o",
        output.to_str().unwrap(),
        a.to_str().unwrap(),
        b.to_str().unwrap(),
        "3-2",
    ]);
    assert_eq!(report["schema"], "nupdf.merge/v1");
    assert_eq!(report["doc_id"], info("minimal.pdf")["doc_id"]);
    assert_eq!(report["page_count"], 3);
    assert_eq!(report["inputs"][0]["pages"], serde_json::json!([1]));
    assert_eq!(report["inputs"][1]["pages"], serde_json::json!([3, 2]));
    assert_eq!(report["inputs"][1]["error"], Value::Null);
    assert_eq!(report["verification"]["verified"], true);
    let merged = pages_at(&output);
    let boxes: Vec<_> = merged["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["MediaBox"][2].as_i64().unwrap())
        .collect();
    assert_eq!(boxes, [612, 300, 595]);
    // A new document names its producer in the catalog, as MuPDF's do.
    let written = std::fs::read(&output).unwrap();
    assert!(
        written
            .windows(25)
            .any(|w| w == b"/Info<</Producer(nupdf)>>")
    );
}

#[test]
fn merge_skips_an_input_that_fails_and_keeps_what_it_gave() {
    // Page 1 refers past the xref, which fails it; page 2 merges first.
    let graft = smoke("merge-graft.pdf");
    let locked = smoke("encrypted-user-password.pdf");
    let report = run_json(&[
        "merge",
        "--dry-run",
        graft.to_str().unwrap(),
        "N-1",
        locked.to_str().unwrap(),
    ]);
    assert_eq!(report["applied"], false);
    assert_eq!(report["page_count"], 1);
    assert_eq!(report["inputs"][0]["pages"], serde_json::json!([2]));
    assert_eq!(
        report["inputs"][0]["error"]["message"],
        "source object number out of range"
    );
    assert_eq!(report["inputs"][1]["error"]["code"], "input.needs_password");
    let codes: Vec<_> = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["merge.input_failed", "merge.input_failed"]);

    // With no input that opens, nothing is written.
    let missing = scratch("no-such-input.pdf");
    let out = nupdf(&["merge", "--dry-run", missing.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3));
    let out = nupdf(&["merge", graft.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn merge_copies_external_links_and_the_outline() {
    let input = smoke("merge-links.pdf");
    let output = scratch("merge-links.pdf");
    let report = run_json(&[
        "merge",
        "-o",
        output.to_str().unwrap(),
        input.to_str().unwrap(),
    ]);
    assert_eq!(report["verification"]["verified"], true);
    let written = String::from_utf8_lossy(&std::fs::read(&output).unwrap()).into_owned();
    // Six external links on page 1 and one on page 2; the link to page 2
    // stays behind, as in MuPDF.
    assert_eq!(written.matches("/Subtype/Link").count(), 7);
    assert!(written.contains("/URI(https://example.com/docs/rel/page.html)"));
    assert!(written.contains("/S/GoToR/D[2/FitH 500]"));
    assert!(!written.contains("/Dest[4 0 R/Fit]"));
    // Every outline item leads to a merged page or has a child that does.
    for title in [
        "One",
        "One, child",
        "Two",
        "Three",
        "Three, last",
        "Four",
        "Five",
    ] {
        assert!(written.contains(&format!("/Title({title})")), "{title}");
    }
    assert!(written.contains("/Type/Outlines/Count -7"));
}

#[test]
fn incremental_edits_append_a_revision() {
    let input = smoke("minimal.pdf");
    let original = std::fs::read(&input).unwrap();
    let output = scratch("meta-incremental.pdf");
    let report = run_json(&[
        "meta",
        "--set",
        "Title=inc",
        "--incremental",
        "-o",
        output.to_str().unwrap(),
        input.to_str().unwrap(),
    ]);
    assert_eq!(report["verification"]["verified"], true);
    let written = std::fs::read(&output).unwrap();
    assert_eq!(&written[..original.len()], &original[..]);
    let facts = info_at(&output);
    assert_eq!(facts["revisions"], 2);
    assert_eq!(facts["metadata"]["Title"], "inc");

    // A new document has no file to add to; a repaired one, MuPDF will not
    // append to.
    for argv in [
        &[
            "merge",
            "--incremental",
            "--dry-run",
            input.to_str().unwrap(),
        ][..],
        &[
            "pages",
            "--rotate",
            "90",
            "--incremental",
            "--dry-run",
            smoke("broken-xref-offsets.pdf").to_str().unwrap(),
        ],
    ] {
        let out = nupdf(argv);
        assert_eq!(out.status.code(), Some(2), "{argv:?}");
        let err: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err["error"]["code"], "usage.incremental", "{argv:?}");
    }
}

#[test]
fn fonts_list_load_and_extract() {
    let path = smoke("fonts-images.pdf");
    let dir = std::env::temp_dir().join(format!("nupdf-fonts-{}", std::process::id()));
    let report = run_json(&[
        "fonts",
        path.to_str().unwrap(),
        "--extract",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(report["schema"], "nupdf.fonts/v1");
    let fonts = report["fonts"].as_array().unwrap();
    let helvetica = fonts.iter().find(|f| f["name"] == "Helvetica").unwrap();
    assert_eq!(helvetica["page"], 1);
    assert_eq!(helvetica["load"]["program"], "CFF");
    assert_eq!(helvetica["load"]["embedded"], false);
    // Fonts in a Type 3 font's resources and in a form are found too.
    assert!(fonts.iter().any(|f| f["name"] == "Times-Roman"));
    assert!(fonts.iter().any(|f| f["name"] == "Courier"));
    let dingbats = fonts
        .iter()
        .find(|f| f["name"] == "ABCDEF+Dingbats")
        .unwrap();
    assert_eq!(dingbats["load"]["embedded"], true);
    let extracted = &report["extracted"];
    assert_eq!(extracted["15"]["file"], "font-0015.cff");
    assert!(dir.join("font-0015.cff").exists());
    assert_eq!(extracted["17"]["error"], "unhandled font type 'Bogus'");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn images_list_decode_and_extract() {
    let path = smoke("fonts-images.pdf");
    let dir = std::env::temp_dir().join(format!("nupdf-images-{}", std::process::id()));
    let report = run_json(&[
        "images",
        path.to_str().unwrap(),
        "--digests",
        "--extract",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(report["schema"], "nupdf.images/v1");
    let images = report["images"].as_array().unwrap();
    // The image in a pattern on page 2 is found there.
    let in_pattern = images.iter().find(|i| i["object"] == 55).unwrap();
    assert_eq!(in_pattern["page"], 2);
    assert_eq!(in_pattern["filters"][0], "DCTDecode");
    // An indexed image is decoded in its base space.
    let indexed = &report["pixmaps"]["44"];
    assert_eq!(indexed["colorspace"][0], "RGB");
    assert_eq!(indexed["n"], 3);
    // A color key adds alpha.
    assert_eq!(report["pixmaps"]["43"]["alpha"], true);
    let extracted = &report["extracted"];
    // JPEG data behind Flate is written as JPEG; gray as PNG, CMYK as PAM.
    assert_eq!(extracted["51"]["file"], "image-0051.jpg");
    assert_eq!(extracted["40"]["file"], "image-0040.png");
    assert_eq!(extracted["45"]["file"], "image-0045.pam");
    let jpg = std::fs::read(dir.join("image-0051.jpg")).unwrap();
    assert_eq!(&jpg[..2], &[0xFF, 0xD8]);
    let png = std::fs::read(dir.join("image-0040.png")).unwrap();
    assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A page's lines as text, a list per block.
fn stext_lines(page: &Value) -> Vec<Vec<String>> {
    page["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "text")
        .map(|b| {
            b["lines"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| {
                    l["chars"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|c| c[0].as_str().unwrap())
                        .collect()
                })
                .collect()
        })
        .collect()
}

#[test]
fn stext_gathers_lines_blocks_and_characters() {
    let path = smoke("stext.pdf");
    let report = run_json(&["stext", path.to_str().unwrap()]);
    assert_eq!(report["schema"], "nupdf.stext/v1");
    assert_eq!(report["page_count"], 2);
    let page = &report["pages"][0];
    let blocks = stext_lines(page);
    // A wide gap becomes a space; a narrow one does not.
    assert_eq!(blocks[0][0], "Hello world!");
    let chars = page["blocks"][0]["lines"][0]["chars"].as_array().unwrap();
    let space = &chars[5];
    // [text, origin, font, size, quad, flags, bidi]: a synthetic space
    // (0.3 em is not past the large-space threshold) in a filled line.
    assert_eq!(space[0], " ");
    assert_eq!(space[2], "Helvetica");
    assert_eq!(space[5].as_i64().unwrap(), 16 | 4);
    // A distant baseline starts a block; ligatures come apart.
    assert_eq!(blocks[1][0], "A new paragraph");
    assert!(blocks[1].contains(&"fiflffffi ligatures".to_string()));
    assert!(blocks[1].contains(&"Replaced and Hi".to_string()));
    // Glyphs past the page and outside a clip path are dropped.
    assert_eq!(blocks[2], ["past th"]);
    assert_eq!(blocks[3], ["cut by a clip p"]);
    let flat: Vec<&String> = blocks.iter().flatten().collect();
    assert!(flat.iter().any(|l| *l == "\u{3042}\u{3044}"));
    // Vertical CJK text runs down a line of its own per character.
    let vertical = page["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| b["lines"].as_array().into_iter().flatten())
        .find(|l| l["wmode"] == 1)
        .unwrap();
    assert_eq!(vertical["dir"], serde_json::json!([0.0, 1.0]));
    // ActualText with nothing but an image takes the image's box.
    assert_eq!(stext_lines(&report["pages"][1])[0], ["Picture"]);
}

#[test]
fn stext_options_follow_mupdf() {
    let path = smoke("stext.pdf");
    let file = path.to_str().unwrap();
    let raw = run_json(&[
        "stext",
        file,
        "--options",
        "preserve-ligatures,preserve-images,clip=no",
    ]);
    let page = &raw["pages"][0];
    let flat: Vec<String> = stext_lines(page).into_iter().flatten().collect();
    assert!(flat.contains(&"\u{fb01}\u{fb02}\u{fb00}\u{fb03} ligatures".to_string()));
    assert!(flat.contains(&"past the page edge".to_string()));
    let images = page["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "image")
        .count();
    // The image and the shading.
    assert_eq!(images, 2);
    let out = nupdf(&["stext", file, "--options", "bogus"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage.bad_option"));
}

#[test]
fn extract_gives_ids_spans_and_fonts() {
    let path = smoke("stext.pdf");
    let report = run_json(&["extract", path.to_str().unwrap()]);
    assert_eq!(report["schema"], "nupdf.extract/v1");
    assert_eq!(report["truncated"], false);
    let block = &report["pages"][0]["blocks"][0];
    assert_eq!(block["id"], "p1.b1");
    assert_eq!(block["type"], "text");
    let line = &block["lines"][0];
    assert_eq!(line["id"], "p1.b1.l1");
    // Text sits at the deepest level given: the spans.
    assert!(line.get("text").is_none());
    let span = &line["spans"][0];
    assert_eq!(span["id"], "p1.b1.l1.s1");
    assert_eq!(span["text"], "Hello world!");
    assert_eq!(span["font"], "Helvetica");
    assert_eq!(span["size"], 12.0);
    assert_eq!(span["color"], "#000000");
    assert!(span.get("chars").is_none());
    // Two runs of one line in two fonts are two spans.
    let page = &report["pages"][0];
    let spans_of = |text: &str| {
        page["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|b| b["lines"].as_array().into_iter().flatten())
            .find(|l| {
                l["spans"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s["text"].as_str().unwrap())
                    .collect::<String>()
                    == text
            })
            .map(|l| l["spans"].as_array().unwrap().len())
    };
    assert_eq!(spans_of("fiflffffi ligatures"), Some(1));
    // Characters with their quads, on request.
    let chars = run_json(&[
        "extract",
        path.to_str().unwrap(),
        "--pages",
        "1",
        "--fields",
        "blocks,lines,spans,text,chars",
    ]);
    let first = &chars["pages"][0]["blocks"][0]["lines"][0]["spans"][0]["chars"][0];
    assert_eq!(first[0], "H");
    assert_eq!(first[1].as_array().unwrap().len(), 8);
}

#[test]
fn extract_budgets_truncate_at_blocks_and_resume_by_cursor() {
    let path = smoke("stext.pdf");
    let file = path.to_str().unwrap();
    let whole = run_json(&["extract", file, "--fields", "blocks,text"]);
    let all: Vec<String> = whole["pages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["blocks"].as_array().unwrap().iter())
        .map(|b| b["id"].as_str().unwrap().to_string())
        .collect();
    // Two blocks at a time until the cursor runs out.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..all.len() {
        let mut args = vec![
            "extract",
            file,
            "--fields",
            "blocks,text",
            "--max-items",
            "2",
        ];
        if let Some(c) = &cursor {
            args.extend(["--cursor", c.as_str()]);
        }
        let part = run_json(&args);
        for p in part["pages"].as_array().unwrap() {
            for b in p["blocks"].as_array().unwrap() {
                seen.push(b["id"].as_str().unwrap().to_string());
            }
        }
        if part["truncated"] == false {
            assert!(part["next_cursor"].is_null());
            break;
        }
        cursor = Some(part["next_cursor"].as_str().unwrap().to_string());
    }
    assert_eq!(seen, all);
    // A character budget always gives the first block.
    let small = run_json(&[
        "extract",
        file,
        "--fields",
        "blocks,text",
        "--max-chars",
        "1",
    ]);
    assert_eq!(small["pages"][0]["blocks"].as_array().unwrap().len(), 1);
    assert_eq!(small["truncated"], true);
    // A cursor belongs to its document.
    let other = smoke("minimal.pdf");
    let out = nupdf(&[
        "extract",
        other.to_str().unwrap(),
        "--cursor",
        small["next_cursor"].as_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2));
}

/// A one-page invoice whose values are drawn before their labels, at the
/// right of the page, as many generated forms draw them.
fn invoice_pdf(name: &str) -> PathBuf {
    let content = "BT /F1 12 Tf 400 600 Td (393.25) Tj ET BT /F1 12 Tf 72 600 Td (Total) Tj ET \
                   BT /F1 12 Tf 400 640 Td (2026-02-11) Tj ET BT /F1 12 Tf 72 640 Td (Date) Tj ET";
    let pdf = format!(
        "%PDF-1.7\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
         2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
         3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<<\
         /F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>>>>>>>endobj\n\
         4 0 obj<</Length {}>>stream\n{content}\nendstream endobj\n\
         trailer<</Root 1 0 R/Size 5>>\n",
        content.len()
    );
    let path = scratch(name);
    std::fs::write(&path, pdf).unwrap();
    path
}

#[test]
fn extract_layout_keeps_labels_and_values_on_one_row() {
    let path = invoice_pdf("layout-invoice.pdf");
    let file = path.to_str().unwrap();
    // Plain text keeps MuPDF's order: the value comes before its label.
    let plain = String::from_utf8(nupdf(&["extract", file, "--format", "text"]).stdout).unwrap();
    assert!(
        !plain
            .lines()
            .any(|l| l.starts_with("Total") && l.contains("393.25"))
    );
    let out = nupdf(&["extract", file, "--format", "layout"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let rows: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(rows.len(), 2, "{text:?}");
    assert!(
        rows[0].starts_with("Date ") && rows[0].ends_with("2026-02-11"),
        "{text:?}"
    );
    assert!(
        rows[1].starts_with("Total ") && rows[1].ends_with("393.25"),
        "{text:?}"
    );
    assert!(text.ends_with("393.25\n\u{c}\n"), "{text:?}");
    let report: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(report["schema"], "nupdf.extract/v1");
    assert_eq!(report["truncated"], false);
}

#[test]
fn extract_writes_text_jsonl_and_markdown() {
    let path = smoke("stext.pdf");
    let file = path.to_str().unwrap();
    // Plain text as mutool draw -F txt writes it, the report on stderr.
    let out = nupdf(&["extract", file, "--format", "text"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("Hello world!\na line that ends in a hyphen-\nated word\n\n"));
    assert_eq!(text.matches("\u{c}\n").count(), 2);
    let report: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(report["schema"], "nupdf.extract/v1");
    // Joined hyphens, with dehyphenate.
    let out = nupdf(&[
        "extract",
        file,
        "--format",
        "text",
        "--options",
        "dehyphenate",
    ]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("a line that ends in a hyphenated word\n"));
    // A page per line, then the report.
    let out = nupdf(&["extract", file, "--format", "jsonl"]);
    let lines: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["schema"], "nupdf.extract.page/v1");
    assert_eq!(lines[1]["id"], "p2");
    assert_eq!(lines[2]["schema"], "nupdf.extract/v1");
    // Markdown: a comment per page, list items, escaped syntax.
    let out = nupdf(&["extract", file, "--format", "markdown"]);
    let md = String::from_utf8(out.stdout).unwrap();
    assert!(md.starts_with("<!-- p1 -->\n\nHello world!"));
    assert!(md.contains("\n- a bullet indented after it\n"));
    assert!(md.contains("<!-- p2 -->"));
}

#[test]
fn search_finds_hits_with_quads_and_ids() {
    let path = smoke("stext.pdf");
    let file = path.to_str().unwrap();
    let report = run_json(&["search", file, "world"]);
    assert_eq!(report["schema"], "nupdf.search/v1");
    let hits = report["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["page"], 1);
    assert_eq!(hits[0]["id"], "p1.b1.l1.s1:6-11");
    assert_eq!(hits[0]["text"], "world");
    assert_eq!(hits[0]["quads"][0].as_array().unwrap().len(), 8);
    // Case and diacritics, and ligatures read as their letters.
    let hits = |needle: &str, options: &str| {
        run_json(&["search", file, needle, "--options", options])["hit_count"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(hits("HELLO", ""), 0);
    assert_eq!(hits("HELLO", "ignore-case"), 1);
    assert_eq!(hits("e combined", "ignore-diacritics"), 1);
    assert_eq!(hits("fl", ""), 1);
    // Across a hyphen at a line end, joined by dehyphenate.
    let joined = run_json(&[
        "search",
        file,
        "hyphenated",
        "--stext-options",
        "dehyphenate",
    ]);
    let hit = &joined["hits"][0];
    assert_eq!(hit["begin"], "p1.b1.l2.s1:22");
    assert_eq!(hit["quads"].as_array().unwrap().len(), 2);
    // Regular expressions, and their errors per page.
    assert_eq!(hits("w[a-z]+d", "regexp"), 2);
    let bad = run_json(&["search", file, "(", "--options", "regexp"]);
    assert_eq!(
        bad["page_errors"][0]["error"],
        "regcomp failure: unmatched '('"
    );
    assert_eq!(
        nupdf(&["search", file, "x", "--options", "fuzzy"])
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn threads_do_not_change_the_output() {
    let files = [
        "stext.pdf",
        "multipage-inherit.pdf",
        "pages-rearrange.pdf",
        "broken-xref-offsets.pdf",
        // Repairs while a page runs.
        "wrong-object-at-offset.pdf",
        "fonts-images.pdf",
    ];
    let commands: [&[&str]; 5] = [
        &["stext"],
        &["extract"],
        &["extract", "--format", "markdown", "--max-chars", "40"],
        &[
            "extract",
            "--format",
            "jsonl",
            "--fields",
            "blocks,lines,spans,chars",
        ],
        &["search", "e"],
    ];
    for name in files {
        let path = smoke(name);
        let file = path.to_str().unwrap();
        for command in commands {
            let at = |threads: Option<&str>| {
                let mut args: Vec<&str> = command.to_vec();
                args.insert(1, file);
                if let Some(t) = threads {
                    args.extend(["--threads", t]);
                }
                let out = nupdf(&args);
                assert!(out.status.success(), "{name} {command:?}");
                (out.stdout, out.stderr)
            };
            let one = at(Some("1"));
            for t in ["2", "8"] {
                assert!(at(Some(t)) == one, "{name} {command:?} --threads {t}");
            }
            // The pages are MuPDF's, as without threads.
            if command == ["stext"] {
                let pages = |b: &[u8]| serde_json::from_slice::<Value>(b).unwrap()["pages"].clone();
                assert_eq!(pages(&one.0), pages(&at(None).0), "{name}");
            }
        }
    }
}

/// More pages than `extract` works on at a time, so that it goes through
/// several windows on the same threads.
#[test]
fn threads_do_not_change_long_output() {
    let long = scratch("threads-long.pdf");
    let mut args = vec!["merge", "-o", long.to_str().unwrap()];
    let parts: Vec<String> = ["stext.pdf", "fonts-images.pdf", "multipage-inherit.pdf"]
        .iter()
        .map(|n| smoke(n).to_str().unwrap().to_string())
        .collect();
    for _ in 0..25 {
        args.extend(parts.iter().map(String::as_str));
    }
    assert!(nupdf(&args).status.success());
    let file = long.to_str().unwrap();
    let commands: [&[&str]; 5] = [
        &["stext"],
        &["extract"],
        &["extract", "--format", "text"],
        &["extract", "--format", "markdown", "--max-chars", "3000"],
        &["extract", "--format", "jsonl", "--max-items", "150"],
    ];
    for command in commands {
        let at = |threads: Option<&str>| {
            let mut args: Vec<&str> = command.to_vec();
            args.insert(1, file);
            if let Some(t) = threads {
                args.extend(["--threads", t]);
            }
            let out = nupdf(&args);
            assert!(out.status.success(), "{command:?}");
            (out.stdout, out.stderr)
        };
        let one = at(Some("1"));
        for t in ["3", "8"] {
            assert!(at(Some(t)) == one, "{command:?} --threads {t}");
        }
        if command[0] == "extract" {
            assert!(at(None) == one, "{command:?} without threads");
        }
    }
}

#[test]
fn stext_pretty_is_the_same_report_laid_out() {
    // Compact output writes each page on its thread; --pretty writes them
    // all at the end. Both must say the same thing.
    for name in [
        "stext.pdf",
        "fonts-images.pdf",
        "wrong-object-at-offset.pdf",
    ] {
        let path = smoke(name);
        let file = path.to_str().unwrap();
        for extra in [&[][..], &["--options", "preserve-images,preserve-spans"]] {
            let mut args = vec!["stext", file];
            args.extend(extra);
            let compact = nupdf(&args);
            let mut pretty_args = vec!["--pretty"];
            pretty_args.extend(&args);
            let pretty = nupdf(&pretty_args);
            assert!(
                compact.status.success() && pretty.status.success(),
                "{name}"
            );
            assert!(String::from_utf8_lossy(&pretty.stdout).lines().count() > 1);
            let parse = |b: &[u8]| serde_json::from_slice::<Value>(b).unwrap();
            assert_eq!(
                parse(&compact.stdout),
                parse(&pretty.stdout),
                "{name} {extra:?}"
            );
        }
    }
}

fn redaction(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../corpus/redaction")
        .join(name)
}

#[test]
fn redact_removes_the_target_and_verify_agrees() {
    let input = redaction("content-text.pdf");
    let out = scratch("content-text-redacted.pdf");
    let run = nupdf(&[
        "redact",
        input.to_str().unwrap(),
        "--text",
        "SECRET-4711",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(report["schema"], "nupdf.redact/v1");
    assert_eq!(report["verified"], true);
    assert_eq!(report["targets"][0]["hits_before"], 1);
    assert_eq!(report["targets"][0]["hits_after"], 0);
    assert_eq!(report["pages"][0]["page"], 1);
    // The rest of the text is still there.
    let text = nupdf(&["extract", out.to_str().unwrap(), "--format", "text"]);
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(
        text.contains("Account") && text.contains("closed"),
        "{text}"
    );
    assert!(!text.contains("SECRET-4711"));
    // The input leaks; the output does not.
    let leaks = nupdf(&["verify", input.to_str().unwrap(), "--text", "SECRET-4711"]);
    assert_eq!(leaks.status.code(), Some(4));
    let clean = nupdf(&["verify", out.to_str().unwrap(), "--text", "SECRET-4711"]);
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stdout)
    );
}

#[test]
fn redact_dry_run_gives_the_plan_and_writes_nothing() {
    let input = redaction("split-text.pdf");
    let run = nupdf(&[
        "redact",
        input.to_str().unwrap(),
        "--text",
        "SECRET-4711",
        "--dry-run",
    ]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(report["applied"], false);
    assert_eq!(report["targets"][0]["hits_before"], 2);
    assert!(report["pages"][0]["quads"].as_array().unwrap().len() >= 2);
    assert_eq!(report["output"], Value::Null);
}

#[test]
fn redact_refuses_hits_of_another_document() {
    let input = redaction("content-text.pdf");
    let hits = scratch("other-hits.json");
    std::fs::write(&hits, r#"{"doc_id":"b3:00","hits":[]}"#).unwrap();
    let run = nupdf(&[
        "redact",
        input.to_str().unwrap(),
        "--hits",
        hits.to_str().unwrap(),
        "--dry-run",
    ]);
    assert_eq!(run.status.code(), Some(2));
}

#[test]
fn segment_gathers_blocks_into_structures() {
    let path = smoke("stext.pdf");
    let file = path.to_str().unwrap();
    let count = |blocks: &Value| -> (usize, usize) {
        fn walk(v: &Value, structs: &mut usize, chars: &mut usize) {
            for b in v.as_array().into_iter().flatten() {
                if b["type"] == "struct" {
                    *structs += 1;
                    assert_eq!(
                        (b["standard"].as_str(), b["raw"].as_str()),
                        (Some("Div"), Some("Split"))
                    );
                    walk(&b["blocks"], structs, chars);
                }
                for l in b["lines"].as_array().into_iter().flatten() {
                    *chars += l["chars"].as_array().map_or(0, Vec::len);
                }
            }
        }
        let (mut s, mut c) = (0, 0);
        walk(blocks, &mut s, &mut c);
        (s, c)
    };
    let flat = run_json(&["stext", file]);
    let split = run_json(&["stext", file, "--options", "segment"]);
    let (s0, c0) = count(&flat["pages"][0]["blocks"]);
    let (s1, c1) = count(&split["pages"][0]["blocks"]);
    assert_eq!(s0, 0);
    assert!(s1 > 1, "{s1}");
    // Every character is still there, within the structures.
    assert_eq!(c0, c1);

    let ex = run_json(&[
        "extract",
        file,
        "--options",
        "segment",
        "--fields",
        "blocks",
    ]);
    // A structure's blocks are within it, with ids under its own.
    let first = &ex["pages"][0]["blocks"][0];
    assert_eq!(first["id"], "p1.b1");
    assert_eq!(first["blocks"][0]["id"], "p1.b1.b1");
}

#[test]
fn clean_sanitize_rewrites_contents() {
    let input = smoke("minimal.pdf");
    let run = nupdf(&["clean", "-s", input.to_str().unwrap(), "--dry-run"]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

fn ppm_pixels(path: &std::path::Path) -> (usize, usize, Vec<u8>) {
    let data = std::fs::read(path).unwrap();
    let mut parts = data.splitn(4, |&b| b == b'\n');
    assert_eq!(parts.next().unwrap(), b"P6");
    let size = String::from_utf8(parts.next().unwrap().to_vec()).unwrap();
    let mut wh = size.split(' ').map(|v| v.parse::<usize>().unwrap());
    let (w, h) = (wh.next().unwrap(), wh.next().unwrap());
    parts.next();
    (w, h, parts.next().unwrap().to_vec())
}

#[test]
fn render_writes_each_page_at_its_size() {
    let dir = scratch("render-pages");
    let input = smoke("content-run.pdf");
    let run = nupdf(&[
        "render",
        input.to_str().unwrap(),
        "-o",
        dir.to_str().unwrap(),
        "--format",
        "ppm",
        "--dpi",
        "144",
    ]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(report["schema"], "nupdf.render/v1");
    assert_eq!(report["page_count"], 5);
    let first = &report["pages"][0];
    // A 90-point page at 144 dpi.
    assert_eq!(
        (first["width"].as_u64(), first["height"].as_u64()),
        (Some(180 * 2), Some(180 * 2))
    );
    let (w, h, px) = ppm_pixels(std::path::Path::new(first["file"].as_str().unwrap()));
    assert_eq!((w, h), (360, 360));
    // The page is drawn on white and has something on it.
    assert!(px.contains(&255) && px.iter().any(|&v| v < 128));
}

#[test]
fn render_and_text_show_annotations_unless_asked_not_to() {
    // A FreeText annotation whose appearance shows a secret, over a page
    // whose contents do not.
    let input = redaction("annotations.pdf");
    let input = input.to_str().unwrap();
    let mut images = Vec::new();
    for extra in [None, Some("--no-annots")] {
        let dir = scratch(&format!("render-annots-{}", extra.is_some()));
        let mut args = vec![
            "render",
            input,
            "-o",
            dir.to_str().unwrap(),
            "--format",
            "ppm",
        ];
        args.extend(extra);
        let run = nupdf(&args);
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        images.push(std::fs::read(dir.join("page-0001.ppm")).unwrap());
    }
    assert_ne!(images[0], images[1]);
    // `mutool draw -F txt`: the appearance's text after the contents'.
    let text = |extra: &[&str]| {
        let mut args = vec!["extract", input, "--format", "text"];
        args.extend_from_slice(extra);
        String::from_utf8(nupdf(&args).stdout).unwrap()
    };
    assert_eq!(text(&[]), "Nothing to see here.\n\nSECRET-4711\n\n\u{c}\n");
    assert_eq!(text(&["--no-annots"]), "Nothing to see here.\n\n\u{c}\n");
    // JSON keeps to the contents, as `Page.toStructuredText`.
    let json: Value =
        serde_json::from_slice(&nupdf(&["extract", input, "--fields", "text"]).stdout).unwrap();
    assert!(!json.to_string().contains("SECRET"));
}

#[test]
fn render_threads_and_bands_do_not_change_the_pixels() {
    let input = smoke("stext.pdf");
    let mut images = Vec::new();
    for threads in ["1", "8"] {
        let dir = scratch(&format!("render-threads-{threads}"));
        let run = nupdf(&[
            "render",
            input.to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--format",
            "ppm",
            "--dpi",
            "300",
            "--pages",
            "1",
            "--threads",
            threads,
        ]);
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        images.push(std::fs::read(dir.join("page-0001.ppm")).unwrap());
    }
    assert_eq!(images[0], images[1]);
}

#[test]
fn render_region_and_pixel_budget() {
    let input = smoke("stext.pdf");
    let dir = scratch("render-region");
    let run = nupdf(&[
        "render",
        input.to_str().unwrap(),
        "-o",
        dir.to_str().unwrap(),
        "--pages",
        "1",
        "--region",
        "0,0,100,50",
        "--dpi",
        "144",
    ]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(report["pages"][0]["width"], 200);
    assert_eq!(report["pages"][0]["height"], 100);
    let run = nupdf(&[
        "render",
        input.to_str().unwrap(),
        "-o",
        dir.to_str().unwrap(),
        "--pages",
        "1",
        "--max-pixels",
        "1000",
    ]);
    assert!(run.status.success());
    let report: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert!(
        report["pages"][0]["error"]
            .as_str()
            .unwrap()
            .contains("limit error")
    );
    let bad = nupdf(&[
        "render",
        input.to_str().unwrap(),
        "-o",
        dir.to_str().unwrap(),
        "--region",
        "1,2,3",
    ]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn render_without_color_management_uses_mupdfs_formulas() {
    // 0 0 1 1 k: cyan through the SWOP profile, or 1 - c, 1 - m, 1 - y.
    let input = smoke("content-run.pdf");
    let mut cyan = Vec::new();
    for flag in [None, Some("--no-icc")] {
        let dir = scratch(&format!("render-icc-{}", flag.is_some()));
        let mut args = vec![
            "render",
            input.to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--format",
            "ppm",
            "--pages",
            "1",
        ];
        args.extend(flag);
        let run = nupdf(&args);
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let (w, _, px) = ppm_pixels(&dir.join("page-0001.ppm"));
        // Inside the clipped triangle the page fills with 0 0 1 1 k.
        let at = (180 - 30) * w + 50;
        cyan.push(px[at * 3..at * 3 + 3].to_vec());
    }
    assert_ne!(cyan[0], cyan[1]);
}

#[test]
fn redact_dry_run_previews_the_areas() {
    let input = redaction("split-text.pdf");
    let dir = scratch("redact-preview");
    let run = nupdf(&[
        "redact",
        input.to_str().unwrap(),
        "--text",
        "SECRET-4711",
        "--dry-run",
        "--preview",
        dir.to_str().unwrap(),
    ]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report: Value = serde_json::from_slice(&run.stdout).unwrap();
    let previews = report["previews"].as_array().unwrap();
    assert_eq!(previews.len(), 1);
    let file = previews[0]["file"].as_str().unwrap();
    assert!(std::fs::read(file).unwrap().starts_with(b"\x89PNG"));
    // --preview needs --dry-run.
    let run = nupdf(&[
        "redact",
        input.to_str().unwrap(),
        "--text",
        "x",
        "--preview",
        dir.to_str().unwrap(),
        "-o",
        "/dev/null",
    ]);
    assert_eq!(run.status.code(), Some(2));
}

/// A PDF whose one page draws `ops` lines: slow to run when `ops` is large.
fn heavy_pdf(name: &str, ops: usize) -> PathBuf {
    let mut content = Vec::new();
    for i in 0..ops {
        content.extend_from_slice(format!("{} 0 m 100 {} l S\n", i % 97, i % 89).as_bytes());
    }
    let objects = [
        "<</Type/Catalog/Pages 2 0 R>>".to_string(),
        "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Contents 4 0 R>>".to_string(),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    offsets.push(out.len());
    out.extend_from_slice(format!("4 0 obj\n<</Length {}>>\nstream\n", content.len()).as_bytes());
    out.extend_from_slice(&content);
    out.extend_from_slice(b"\nendstream\nendobj\n");
    let xref = out.len();
    out.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<</Size 5/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    let path = scratch(name);
    std::fs::write(&path, out).unwrap();
    path
}

/// A running `nupdf mcp`, spoken to a line at a time.
struct Mcp {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    lines: std::sync::mpsc::Receiver<Value>,
}

impl Mcp {
    fn start() -> Mcp {
        use std::io::BufRead;
        let mut child = Command::new(env!("CARGO_BIN_EXE_nupdf"))
            .arg("mcp")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("nupdf mcp runs");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                let _ = tx.send(serde_json::from_str(&line).unwrap());
            }
        });
        let stdin = child.stdin.take();
        let mut mcp = Mcp {
            child,
            stdin,
            lines,
        };
        mcp.send(
            serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "test", "version": "1"}}}),
        );
        assert_eq!(mcp.next()["result"]["serverInfo"]["name"], "nupdf");
        mcp.send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        mcp
    }

    fn send(&mut self, message: Value) {
        use std::io::Write;
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn next(&self) -> Value {
        self.lines
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("an answer")
    }

    fn call(&mut self, id: u64, tool: &str, args: Value) -> Value {
        self.send(
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": args}}),
        );
        let answer = self.next();
        assert_eq!(answer["id"], id);
        answer["result"].clone()
    }

    /// Closes stdin and waits for the server to finish.
    fn close(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        self.child.wait().unwrap()
    }
}

#[test]
fn mcp_tools_give_the_cli_reports() {
    let mut mcp = Mcp::start();
    mcp.send(serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    let tools = mcp.next()["result"]["tools"].clone();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for name in [
        "info", "stext", "extract", "search", "render", "redact", "clean",
    ] {
        assert!(names.contains(&name), "{name}");
    }
    let file = smoke("stext.pdf");
    let file = file.to_str().unwrap();
    let cases: [(&str, Value, Vec<&str>); 4] = [
        (
            "info",
            serde_json::json!({"file": file}),
            vec!["info", file],
        ),
        (
            "extract",
            serde_json::json!({"file": file, "max_chars": 20, "fields": "blocks,lines"}),
            vec![
                "extract",
                file,
                "--max-chars",
                "20",
                "--fields",
                "blocks,lines",
            ],
        ),
        (
            "search",
            serde_json::json!({"file": file, "needle": "word", "options": "ignore-case"}),
            vec!["search", file, "word", "--options", "ignore-case"],
        ),
        (
            "stext",
            serde_json::json!({"file": file, "threads": 2}),
            vec!["stext", file, "--threads", "2"],
        ),
    ];
    for (i, (tool, args, line)) in cases.into_iter().enumerate() {
        let result = mcp.call(10 + i as u64, tool, args);
        let cli = nupdf(&line);
        assert_eq!(result["isError"], false, "{tool}: {result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            text,
            String::from_utf8_lossy(&cli.stdout).trim_end(),
            "{tool}"
        );
        assert_eq!(
            result["structuredContent"]["schema"],
            format!("nupdf.{tool}/v1")
        );
    }
    // Text output: the text, then the report.
    let result = mcp.call(
        20,
        "extract",
        serde_json::json!({"file": file, "format": "text"}),
    );
    let cli = nupdf(&["extract", file, "--format", "text"]);
    assert_eq!(
        result["content"][0]["text"].as_str().unwrap(),
        String::from_utf8_lossy(&cli.stdout).trim_end()
    );
    // The text alone: a client shows a model structured content in place of
    // the text, so a report there would hide it.
    assert_eq!(result["structuredContent"], Value::Null);
    assert!(
        result["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("nupdf.extract/v1")
    );
    // Failures are tool errors with nupdf's error report.
    let result = mcp.call(21, "info", serde_json::json!({"file": "/no/such.pdf"}));
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "input.unreadable"
    );
    let result = mcp.call(22, "extract", serde_json::json!({"file": file, "nope": 1}));
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "usage.bad_argument"
    );
    mcp.send(serde_json::json!({"jsonrpc": "2.0", "id": 23, "method": "no/such"}));
    assert_eq!(mcp.next()["error"]["code"], -32601);
    assert!(mcp.close().success());
}

#[test]
fn mcp_calls_stop_at_their_timeout_or_when_cancelled() {
    let heavy = heavy_pdf("mcp-heavy.pdf", 200_000);
    let heavy = heavy.to_str().unwrap();
    let mut mcp = Mcp::start();
    let result = mcp.call(
        1,
        "stext",
        serde_json::json!({"file": heavy, "timeout": 0.05}),
    );
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "limit.timeout"
    );

    // A cancelled call gets no answer, and the server goes on answering.
    let start = std::time::Instant::now();
    mcp.send(serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "render", "arguments": {"file": heavy, "output": scratch("mcp-render").to_str().unwrap()}}}));
    std::thread::sleep(std::time::Duration::from_millis(100));
    mcp.send(
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": 2, "reason": "test"}}),
    );
    mcp.send(serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}));
    let answer = mcp.next();
    assert_eq!(answer["id"], 3, "{answer}");
    assert!(mcp.close().success());
    let stopped = start.elapsed();
    // The cancelled call stopped rather than ran to its end.
    let full = {
        let t = std::time::Instant::now();
        nupdf(&[
            "render",
            heavy,
            "-o",
            scratch("mcp-render-full").to_str().unwrap(),
        ]);
        t.elapsed()
    };
    assert!(stopped < full / 2, "{stopped:?} against {full:?}");
    assert!(!scratch("mcp-render").join("page-1.png").exists());
}

#[test]
fn mcp_sees_a_file_that_changed() {
    let path = scratch("mcp-changing.pdf");
    std::fs::copy(smoke("stext.pdf"), &path).unwrap();
    let file = path.to_str().unwrap();
    let mut mcp = Mcp::start();
    let first = mcp.call(1, "info", serde_json::json!({"file": file}));
    std::fs::copy(smoke("multipage-inherit.pdf"), &path).unwrap();
    let second = mcp.call(2, "info", serde_json::json!({"file": file}));
    assert_ne!(
        first["structuredContent"]["doc_id"],
        second["structuredContent"]["doc_id"]
    );
    let cli = nupdf(&["info", file]);
    assert_eq!(
        second["content"][0]["text"].as_str().unwrap(),
        String::from_utf8_lossy(&cli.stdout).trim_end()
    );
    assert!(mcp.close().success());
}

#[test]
fn a_command_out_of_time_writes_nothing() {
    let heavy = heavy_pdf("timeout-heavy.pdf", 200_000);
    let heavy = heavy.to_str().unwrap();
    let out = nupdf(&["--timeout", "0.01", "stext", heavy]);
    assert_eq!(out.status.code(), Some(5));
    assert!(out.stdout.is_empty());
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "limit.timeout");
    let written = scratch("timeout-clean.pdf");
    let _ = std::fs::remove_file(&written);
    let out = nupdf(&[
        "--timeout",
        "0",
        "clean",
        "-c",
        heavy,
        "-o",
        written.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(5));
    assert!(!written.exists());
    // Without a timeout the same commands succeed.
    assert!(
        nupdf(&["stext", smoke("stext.pdf").to_str().unwrap()])
            .status
            .success()
    );
}

fn lines(out: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(out)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn batch_runs_a_command_on_each_file_in_order() {
    let dir = smoke("stext.pdf").parent().unwrap().to_path_buf();
    let pattern = dir.join("s*.pdf");
    let pattern = pattern.to_str().unwrap();
    let out = nupdf(&[
        "batch",
        pattern,
        "--jobs",
        "3",
        "--",
        "extract",
        "--max-chars",
        "40",
    ]);
    assert!(out.status.success());
    let lines = lines(&out.stdout);
    let (summary, items) = lines.split_last().unwrap();
    assert_eq!(summary["schema"], "nupdf.batch/v1");
    assert_eq!(summary["files"], items.len());
    let files: Vec<&str> = items.iter().map(|l| l["file"].as_str().unwrap()).collect();
    let mut sorted = files.clone();
    sorted.sort_unstable();
    assert_eq!(files, sorted);
    // Each line carries the command's own report.
    for item in items {
        let cli = nupdf(&[
            "extract",
            item["file"].as_str().unwrap(),
            "--max-chars",
            "40",
        ]);
        let report: Value = serde_json::from_slice(&cli.stdout).unwrap();
        assert_eq!(item["report"], report, "{}", item["file"]);
    }
    // The same at one file at a time.
    let one = nupdf(&[
        "batch",
        pattern,
        "--jobs",
        "1",
        "--",
        "extract",
        "--max-chars",
        "40",
    ]);
    assert_eq!(one.stdout, out.stdout);
    // Commands that take no single file are refused.
    assert_eq!(
        nupdf(&["batch", pattern, "--", "merge"]).status.code(),
        Some(2)
    );
    assert_eq!(
        nupdf(&["batch", "/no/such/*.pdf", "--", "info"])
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn batch_names_outputs_and_takes_up_where_it_stopped() {
    let work = scratch("batch-work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("in/sub")).unwrap();
    for (name, to) in [
        ("stext.pdf", "in/a.pdf"),
        ("minimal.pdf", "in/sub/b.pdf"),
        ("multipage-inherit.pdf", "in/c.pdf"),
    ] {
        std::fs::copy(smoke(name), work.join(to)).unwrap();
    }
    std::fs::write(work.join("in/broken.pdf"), b"not a pdf").unwrap();
    let journal = work.join("run.jsonl");
    let inputs = work.join("in");
    let out_pattern = work.join("out/{stem}-{index}.pdf");
    std::fs::create_dir_all(work.join("out")).unwrap();
    let run = |resume: bool| {
        let mut args = vec![
            "batch",
            inputs.to_str().unwrap(),
            "-o",
            journal.to_str().unwrap(),
        ];
        if resume {
            args.push("--resume");
        }
        args.extend(["--", "clean", "-g", "-o", out_pattern.to_str().unwrap()]);
        let out = nupdf(&args);
        assert!(out.status.success());
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let first = run(false);
    assert_eq!(
        (
            first["files"].as_u64(),
            first["ok"].as_u64(),
            first["failed"].as_u64()
        ),
        (Some(4), Some(3), Some(1))
    );
    for made in ["a-1.pdf", "c-3.pdf", "b-4.pdf"] {
        assert!(work.join("out").join(made).exists(), "{made}");
    }
    let items = lines(&std::fs::read(&journal).unwrap());
    assert_eq!(items.len(), 4);
    assert_eq!(items[1]["error"]["error"]["code"], "input.not_pdf");
    // Nothing changed: nothing to do.
    let again = run(true);
    assert_eq!(
        (again["skipped"].as_u64(), again["ok"].as_u64()),
        (Some(4), Some(0))
    );
    // A changed file runs again, and its new line is added.
    std::fs::copy(smoke("fonts-images.pdf"), work.join("in/c.pdf")).unwrap();
    let changed = run(true);
    assert_eq!(
        (changed["skipped"].as_u64(), changed["ok"].as_u64()),
        (Some(3), Some(1))
    );
    assert_eq!(lines(&std::fs::read(&journal).unwrap()).len(), 5);
}

#[test]
fn batch_files_stop_at_their_timeout() {
    let heavy = heavy_pdf("batch-heavy.pdf", 200_000);
    let out = nupdf(&[
        "batch",
        heavy.to_str().unwrap(),
        smoke("stext.pdf").to_str().unwrap(),
        "--item-timeout",
        "0.05",
        "--",
        "stext",
    ]);
    assert!(out.status.success());
    let lines = lines(&out.stdout);
    assert_eq!(lines[0]["exit"], 5);
    assert_eq!(lines[0]["error"]["error"]["code"], "limit.timeout");
    assert_eq!(lines[2]["failed"], 2);
}

fn diff(a: &str, b: &str, extra: &[&str]) -> Value {
    let mut args = vec!["diff", a, b];
    args.extend(extra);
    let out = nupdf(&args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn diff_finds_what_an_edit_changed() {
    let file = smoke("multipage-inherit.pdf");
    let file = file.to_str().unwrap();
    let same = diff(file, file, &["--by", "text,structure,pixels"]);
    assert_eq!(same["schema"], "nupdf.diff/v1");
    assert_eq!(same["identical"], true);
    assert_eq!(same["a"]["doc_id"], same["b"]["doc_id"]);

    let titled = scratch("diff-titled.pdf");
    assert!(
        nupdf(&[
            "meta",
            file,
            "--set",
            "Title=New title",
            "-o",
            titled.to_str().unwrap()
        ])
        .status
        .success()
    );
    let d = diff(file, titled.to_str().unwrap(), &[]);
    assert_eq!(d["identical"], false);
    assert_eq!(d["text"]["same"], true);
    assert_eq!(d["structure"]["differences"][0]["what"], "info.Title");
    assert_eq!(d["structure"]["differences"][0]["b"], "New title");
    assert_eq!(d["pixels"], Value::Null);

    // A page taken out is a page deleted, not every later page changed.
    let shorter = scratch("diff-shorter.pdf");
    assert!(
        nupdf(&[
            "pages",
            file,
            "--delete",
            "2",
            "-o",
            shorter.to_str().unwrap()
        ])
        .status
        .success()
    );
    let d = diff(file, shorter.to_str().unwrap(), &[]);
    let pages = d["text"]["pages"].as_array().unwrap();
    assert_eq!(pages.len(), 1, "{pages:?}");
    assert_eq!(pages[0]["a_page"], 2);
    assert_eq!(pages[0]["op"], "delete");
    assert!(
        d["structure"]["differences"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["what"] == "page_count" && x["a"] == 3 && x["b"] == 2)
    );
}

#[test]
fn diff_names_the_lines_and_pixels_that_changed() {
    let file = smoke("stext.pdf");
    let file = file.to_str().unwrap();
    let redacted = scratch("diff-redacted.pdf");
    assert!(
        nupdf(&[
            "redact",
            file,
            "--text",
            "world",
            "-o",
            redacted.to_str().unwrap()
        ])
        .status
        .success()
    );
    let d = diff(file, redacted.to_str().unwrap(), &["--by", "text,pixels"]);
    let changes = d["text"]["pages"][0]["changes"].as_array().unwrap();
    assert!(
        changes
            .iter()
            .any(|c| c["op"] == "delete" && c["a"] == "p1.b1.l1" && c["text"] == "Hello world!")
    );
    assert!(
        changes
            .iter()
            .all(|c| c["a"].is_string() || c["b"].is_string())
    );
    let pixels = &d["pixels"]["pages"][0];
    assert_eq!(pixels["page"], 1);
    assert!(pixels["differing"].as_u64().unwrap() > 0);
    assert_eq!(d["structure"], Value::Null);
    // A budget stops the list.
    let d = diff(file, redacted.to_str().unwrap(), &["--max-items", "1"]);
    assert_eq!(d["text"]["truncated"], true);
}

#[test]
fn every_report_fits_its_schema() {
    let listed = nupdf(&["schema"]);
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let names: Vec<String> = listed["schemas"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n.as_str().map(String::from))
        .collect();
    for want in [
        "info",
        "extract",
        "extract.page",
        "search",
        "redact",
        "verify",
        "diff",
        "batch.item",
        "error",
    ] {
        assert!(names.iter().any(|n| n == want), "{want}");
    }
    let schema = |id: &str| -> Value {
        let out = nupdf(&["schema", id]);
        assert!(out.status.success(), "{id}");
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let work = scratch("schema-work");
    let _ = std::fs::remove_dir_all(&work);
    for name in [
        "stext.pdf",
        "multipage-inherit.pdf",
        "fonts-images.pdf",
        "truncated.pdf",
        "encrypted-aes-256.pdf",
    ] {
        let file = smoke(name);
        let file = file.to_str().unwrap();
        let out = work.join(name);
        let runs: Vec<Vec<&str>> = vec![
            vec!["info", file],
            vec!["pages", file],
            vec!["extract", file],
            vec!["extract", file, "--format", "jsonl"],
            vec!["extract", file, "--format", "markdown"],
            vec!["search", file, "e"],
            vec!["stext", file],
            vec!["fonts", file],
            vec!["images", file],
            vec!["redact", file, "--text", "e", "--dry-run"],
            vec!["verify", file, "--text", "e"],
            vec!["diff", file, file],
            vec!["render", file, "-o", out.to_str().unwrap(), "--dpi", "10"],
            vec!["clean", file, "--dry-run"],
        ];
        for args in runs {
            let ran = nupdf(&args);
            for line in String::from_utf8_lossy(&ran.stdout)
                .lines()
                .chain(String::from_utf8_lossy(&ran.stderr).lines())
            {
                let Ok(report) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let Some(id) = report["schema"].as_str() else {
                    continue;
                };
                let problems = schema_check::violations(&schema(id), &report);
                assert!(problems.is_empty(), "{name} {args:?} {id}: {problems:?}");
            }
        }
    }
    // A name it does not know, as the report's own id or not.
    assert_eq!(nupdf(&["schema", "nope"]).status.code(), Some(2));
    assert_eq!(
        schema("nupdf.extract/v1")["title"],
        schema("extract")["title"]
    );
}

#[test]
fn the_guide_is_text_for_models() {
    let out = nupdf(&["guide"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("# nupdf") && text.contains("`extract`"));
    let one = nupdf(&["guide", "redact"]);
    assert!(String::from_utf8_lossy(&one.stdout).contains("--preset"));
    assert_eq!(nupdf(&["guide", "nope"]).status.code(), Some(2));
    let skill = nupdf(&["guide", "--skill"]);
    assert!(String::from_utf8_lossy(&skill.stdout).starts_with("---\nname: nupdf\n"));
}

fn redaction_fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../corpus/redaction")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn report_of(args: &[&str]) -> Value {
    let out = nupdf(args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn navigation_reports_outline_labels_links_and_attachments() {
    let outline = report_of(&["outline", &redaction_fixture("outline.pdf")]);
    assert_eq!(outline["schema"], "nupdf.outline/v1");
    assert_eq!(outline["items"][0]["title"], "SECRET-4711 chapter");
    assert_eq!(outline["items"][0]["page"], 1);

    let labels = report_of(&["labels", &redaction_fixture("page-labels.pdf")]);
    assert_eq!(labels["pages"][0]["label"], "SECRET-4711-1");
    // A document without labels: page numbers.
    let plain = report_of(&["labels", smoke("multipage-inherit.pdf").to_str().unwrap()]);
    let got: Vec<&str> = plain["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["label"].as_str().unwrap())
        .collect();
    assert_eq!(got, ["1", "2", "3"]);

    let links = report_of(&["links", smoke("merge-links.pdf").to_str().unwrap()]);
    let first = &links["pages"][0]["links"][0];
    assert_eq!(first["uri"], "http://example.com/a?b=c");
    assert_eq!(first["page"], Value::Null);
    assert_eq!(first["bbox"].as_array().unwrap().len(), 4);

    let dir = scratch("attachments-out");
    let _ = std::fs::remove_dir_all(&dir);
    let found = report_of(&[
        "attachments",
        &redaction_fixture("embedded-file.pdf"),
        "--extract",
        dir.to_str().unwrap(),
    ]);
    let a = &found["attachments"][0];
    assert_eq!(
        (
            a["name"].as_str(),
            a["embedded"].as_bool(),
            a["checksum_ok"].as_bool()
        ),
        (Some("SECRET-4711.txt"), Some(true), Some(true))
    );
    let written = std::fs::read(dir.join(a["file"].as_str().unwrap())).unwrap();
    assert_eq!(written.len() as u64, a["length"].as_u64().unwrap());
    // None embedded: an empty list, not an error.
    let none = report_of(&["attachments", smoke("minimal.pdf").to_str().unwrap()]);
    assert_eq!(none["attachments"], serde_json::json!([]));
}

/// A page with a FileAttachment annotation that embeds budget.csv, and no
/// /EmbeddedFiles tree.
fn annotation_attachment_pdf(name: &str) -> PathBuf {
    let data = "a,b\n1,2\n";
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Annots [4 0 R] >>".to_string(),
        "<< /Type /Annot /Subtype /FileAttachment /Rect [10 10 30 30] /Contents (the budget) \
         /FS 5 0 R >>"
            .to_string(),
        "<< /Type /Filespec /F (budget.csv) /UF (budget.csv) /EF << /F 6 0 R >> >>".to_string(),
        format!(
            "<< /Type /EmbeddedFile /Subtype /text#2Fcsv /Params << /Size {} >> /Length {} >>\nstream\n{data}\nendstream",
            data.len(),
            data.len()
        ),
    ];
    let mut pdf = String::from("%PDF-1.7\n");
    for (i, o) in objs.iter().enumerate() {
        pdf.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
    }
    pdf.push_str("trailer\n<< /Size 7 /Root 1 0 R >>\n");
    let path = scratch(name);
    std::fs::write(&path, pdf).unwrap();
    path
}

#[test]
fn files_attached_to_annotations_are_listed_and_extracted() {
    let path = annotation_attachment_pdf("annot-attachment.pdf");
    let file = path.to_str().unwrap();
    // `annots` names the file, as `getFilespec` and `getFilespecParams` read it.
    let annots = report_of(&["annots", file]);
    let a = &annots["pages"][0]["annots"][0];
    assert_eq!(a["type"], "FileAttachment");
    assert_eq!(a["file"]["filename"], "budget.csv");
    assert_eq!(a["file"]["embedded"], true);
    assert_eq!(a["file"]["mimetype"], "text/csv");
    // `attachments` keeps the name tree's list as MuPDF's, and adds the
    // annotations' files beside it.
    let dir = scratch("annot-attachment-out");
    let _ = std::fs::remove_dir_all(&dir);
    let found = report_of(&["attachments", file, "--extract", dir.to_str().unwrap()]);
    assert_eq!(found["attachments"], serde_json::json!([]));
    let b = &found["annotation_attachments"][0];
    assert_eq!(b["annot"], "p1.a1");
    assert_eq!(b["page"], 1);
    assert_eq!(b["filename"], "budget.csv");
    assert_eq!(b["length"], 8);
    let written = std::fs::read(dir.join(b["file"].as_str().unwrap())).unwrap();
    assert_eq!(written, b"a,b\n1,2\n");
}

/// A page with a ruled 3x3 table: a header row and two rows of numbers.
fn table_pdf(name: &str) -> PathBuf {
    let rows = [
        ["Name", "Qty", "Price"],
        ["Apple", "3", "1.20"],
        ["Pear", "10", "0.85"],
    ];
    let mut content = String::from("0.5 w\n");
    for (r, row) in rows.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            content += &format!(
                "BT /F1 10 Tf {} {} Td ({cell}) Tj ET\n",
                76 + c * 100,
                706 - r * 20
            );
        }
    }
    for r in 0..=3 {
        content += &format!("72 {y} m 372 {y} l S\n", y = 720 - r * 20);
    }
    for c in 0..=3 {
        content += &format!("{x} 720 m {x} 660 l S\n", x = 72 + c * 100);
    }
    let objects = [
        "<</Type/Catalog/Pages 2 0 R>>".to_string(),
        "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>".to_string(),
        format!("<</Length {}>>\nstream\n{content}\nendstream", content.len()),
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_string(),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<</Size 6/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    let path = scratch(name);
    std::fs::write(&path, out).unwrap();
    path
}

#[test]
fn tables_come_out_as_rows_of_cells() {
    let file = table_pdf("table.pdf");
    let file = file.to_str().unwrap();
    let report = report_of(&["tables", file]);
    assert_eq!(report["schema"], "nupdf.tables/v1");
    let table = &report["pages"][0]["tables"][0];
    assert_eq!(table["columns"], 3);
    let texts: Vec<Vec<&str>> = table["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r["cells"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["text"].as_str().unwrap())
                .collect()
        })
        .collect();
    assert_eq!(
        texts,
        [
            ["Name", "Qty", "Price"],
            ["Apple", "3", "1.20"],
            ["Pear", "10", "0.85"]
        ]
    );
    let csv = nupdf(&["tables", file, "--format", "csv"]);
    assert_eq!(
        String::from_utf8_lossy(&csv.stdout),
        "Name,Qty,Price\nApple,3,1.20\nPear,10,0.85\n"
    );
    let reported: Value = serde_json::from_slice(&csv.stderr).unwrap();
    assert_eq!(reported["schema"], "nupdf.tables/v1");
    // Threads do not change the tables.
    assert_eq!(
        report_of(&["tables", file, "--threads", "4"])["pages"],
        report["pages"]
    );
}

/// A PDF of `objects`, numbered from 1, the first the catalog.
fn pdf_of(objects: &[&str]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    let n = objects.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<</Size {n}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    out
}

fn signed_pdf() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/signed.pdf")
}

#[test]
fn a_signature_is_checked_against_the_bytes_it_signs() {
    let report = report_of(&["signatures", signed_pdf().to_str().unwrap()]);
    assert_eq!(report["schema"], "nupdf.signatures/v1");
    let s = &report["pages"][0]["signatures"][0];
    assert_eq!(s["name"], "Approval");
    assert_eq!(s["signed"], true);
    assert_eq!(s["digest"], "OK");
    assert_eq!(s["changed_since_signing"], false);
    assert_eq!(
        s["signatory"],
        "cn=Test Signer, o=nupdf tests, ou=, email=signer@example.org, c=US"
    );
    assert_eq!(s["reason"], "Testing");
    assert_eq!(s["sub_filter"], "adbe.pkcs7.detached");

    // A changed byte inside the signed range breaks the digest.
    let mut bytes = std::fs::read(signed_pdf()).unwrap();
    let at = bytes.windows(7).position(|w| w == b"Testing").unwrap();
    bytes[at] = b'R';
    let tampered = scratch("signed-tampered.pdf");
    std::fs::write(&tampered, &bytes).unwrap();
    let report = report_of(&["signatures", tampered.to_str().unwrap()]);
    assert_eq!(
        report["pages"][0]["signatures"][0]["digest"],
        "Signature invalidated by change to document."
    );

    // Bytes after the signed ones leave it valid, and say the file changed.
    let mut bytes = std::fs::read(signed_pdf()).unwrap();
    bytes.extend_from_slice(b"\n% appended\n");
    let grown = scratch("signed-grown.pdf");
    std::fs::write(&grown, &bytes).unwrap();
    let s = &report_of(&["signatures", grown.to_str().unwrap()])["pages"][0]["signatures"][0];
    assert_eq!(s["digest"], "OK");
    assert_eq!(s["changed_since_signing"], true);
}

#[test]
fn editing_a_signed_file_says_whether_its_signatures_survive() {
    let input = signed_pdf();
    let out = scratch("signed-edited.pdf");
    let args = |extra: &[&'static str]| {
        let mut a = vec![
            "meta",
            input.to_str().unwrap(),
            "--set",
            "Title=Edited",
            "-o",
            out.to_str().unwrap(),
        ];
        a.extend_from_slice(extra);
        a.iter().map(|s| s.to_string()).collect::<Vec<_>>()
    };
    let rewrite = run_json(&args(&[]).iter().map(String::as_str).collect::<Vec<_>>());
    let sig = &rewrite["output"]["signatures"][0];
    assert_eq!(sig["before"], "OK");
    assert_ne!(sig["after"], "OK");
    assert!(
        rewrite["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "signature.invalidated")
    );
    let kept = run_json(
        &args(&["--incremental"])
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    assert_eq!(kept["output"]["signatures"][0]["after"], "OK");
    let report = report_of(&["signatures", out.to_str().unwrap()]);
    let s = &report["pages"][0]["signatures"][0];
    assert_eq!(
        (s["digest"].as_str(), s["changed_since_signing"].as_bool()),
        (Some("OK"), Some(true))
    );
}

#[test]
fn form_fields_read_through_their_parents() {
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R/AcroForm<</Fields[4 0 R 7 0 R]/NeedAppearances true/XFA 8 0 R>>>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Annots[5 0 R 6 0 R 7 0 R]>>",
        "<</T(person)/FT/Tx/Kids[5 0 R 6 0 R]/Ff 1>>",
        "<</Type/Annot/Subtype/Widget/Parent 4 0 R/T(name)/V(Ada)/MaxLen 20/Rect[10 10 90 30]/TU(Your name)>>",
        "<</Type/Annot/Subtype/Widget/Parent 4 0 R/T(city)/Rect[10 40 90 60]>>",
        "<</Type/Annot/Subtype/Widget/T(fruit)/FT/Ch/Ff 131072/V(b)/Opt[(a)[(b)(Banana)]]/Rect[10 70 90 90]>>",
        "<</Length 0>>\nstream\n\nendstream",
    ]);
    let file = scratch("form.pdf");
    std::fs::write(&file, pdf).unwrap();
    let report = report_of(&["forms", file.to_str().unwrap()]);
    assert_eq!(report["schema"], "nupdf.forms/v1");
    assert_eq!(
        (
            report["xfa"].as_bool(),
            report["need_appearances"].as_bool()
        ),
        (Some(true), Some(true))
    );
    let w = report["pages"][0]["widgets"].as_array().unwrap();
    assert_eq!(w.len(), 3);
    assert_eq!(w[0]["name"], "person.name");
    assert_eq!(w[0]["type"], "text");
    assert_eq!(w[0]["value"], "Ada");
    assert_eq!(w[0]["max_len"], 20);
    assert_eq!(w[0]["label"], "Your name");
    assert_eq!(w[0]["read_only"], true);
    assert_eq!(w[1]["label"], "city");
    assert_eq!(w[2]["type"], "combobox");
    assert_eq!(
        w[2]["options"],
        serde_json::json!([{"export": "a", "text": "a"}, {"export": "b", "text": "Banana"}])
    );
}

/// A form with a text field of two widgets (one read-only), a combo box, a
/// check box and a radio group of two, for the filling tests.
fn fill_form() -> PathBuf {
    fill_form_with("", "fill-form.pdf")
}

/// `fill_form` with `extra` in the text field's dictionary.
fn fill_form_with(extra: &str, name: &str) -> PathBuf {
    let person = format!("<</T(person)/FT/Tx{extra}/V(Ada)/Kids[5 0 R 6 0 R]>>");
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R/AcroForm<</Fields[4 0 R 7 0 R 8 0 R 9 0 R]/DA(/Helv 0 Tf 0 g)>>>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 300]/Annots[5 0 R 6 0 R 7 0 R 8 0 R 10 0 R 11 0 R]>>",
        &person,
        "<</Type/Annot/Subtype/Widget/Parent 4 0 R/Rect[10 10 150 30]/DA(/TiRo 10 Tf)>>",
        "<</Type/Annot/Subtype/Widget/Parent 4 0 R/Rect[10 40 150 60]/Ff 1>>",
        "<</Type/Annot/Subtype/Widget/T(fruit)/FT/Ch/Ff 131072/V(a)/Opt[(a)[(b)(Banana)]]/Rect[10 70 150 90]>>",
        "<</Type/Annot/Subtype/Widget/T(agree)/FT/Btn/AS/Off/Rect[10 100 30 120]/AP<</N<</Off 12 0 R/Sure 12 0 R>>>>>>",
        "<</T(pick)/FT/Btn/Ff 49152/V/One/Kids[10 0 R 11 0 R]>>",
        "<</Type/Annot/Subtype/Widget/Parent 9 0 R/AS/One/Rect[10 130 30 150]/AP<</N<</One 12 0 R/Off 12 0 R>>>>>>",
        "<</Type/Annot/Subtype/Widget/Parent 9 0 R/AS/Off/Rect[40 130 60 150]/AP<</N<</Two 12 0 R/Off 12 0 R>>>>>>",
        "<</Type/XObject/Subtype/Form/BBox[0 0 1 1]/Length 0>>\nstream\n\nendstream",
    ]);
    let file = scratch(name);
    std::fs::write(&file, pdf).unwrap();
    file
}

fn widget<'a>(report: &'a Value, name: &str, i: usize) -> &'a Value {
    report["pages"][0]["widgets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["name"] == name)
        .nth(i)
        .unwrap()
}

#[test]
fn forms_set_fills_fields_and_makes_their_appearances() {
    let file = fill_form();
    let input = file.to_str().unwrap();
    let output = scratch("fill-set.pdf");
    let out = output.to_str().unwrap();
    let report = report_of(&[
        "forms",
        input,
        "--set",
        "person=Grace",
        "--set",
        "fruit=Cherry",
        "-o",
        out,
    ]);
    assert_eq!(report["schema"], "nupdf.fill/v1");
    assert_eq!(report["scripts_run"], false);
    assert_eq!(report["applied"], true);
    let f = &report["fields"];
    assert_eq!(
        (
            f[0]["name"].as_str(),
            f[0]["old"].as_str(),
            f[0]["new"].as_str()
        ),
        (Some("person"), Some("Ada"), Some("Grace"))
    );
    assert_eq!(f[0]["changed"], true);
    // MuPDF sets a choice to a value none of its options has, and says
    // nothing; the report does.
    assert_eq!(f[1]["new"], "Cherry");
    assert_eq!(f[1]["notes"].as_array().unwrap().len(), 1);
    // Both widgets of the text field and the combo box are made again.
    let made: Vec<i64> = report["appearances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["object"].as_i64().unwrap())
        .collect();
    assert_eq!(made, [5, 6, 7]);

    let written = report_of(&["forms", out, "--synthesize"]);
    assert_eq!(widget(&written, "person", 0)["value"], "Grace");
    assert_eq!(widget(&written, "person", 1)["value"], "Grace");
    // The new appearance shows the value, in the font its DA names.
    let s = &widget(&written, "person", 0)["synthesis"];
    assert!(
        s["content"]
            .as_str()
            .unwrap()
            .contains("/TiRo 10 Tf\n(Grace) Tj")
    );
}

#[test]
fn forms_toggle_turns_radio_groups_together() {
    let file = fill_form();
    let input = file.to_str().unwrap();
    let output = scratch("fill-toggle.pdf");
    let out = output.to_str().unwrap();
    let report = report_of(&["forms", input, "--toggle", "agree", "-o", out]);
    assert_eq!(report["fields"][0]["new"], "Sure");
    // A toggle changes /AS and /V only: no appearance is made.
    assert_eq!(report["appearances"], serde_json::json!([]));
    let written = report_of(&["forms", out]);
    assert_eq!(widget(&written, "agree", 0)["value"], "Sure");

    // Setting the radio group to the second button's state turns that one
    // on through /V; the first keeps its /AS, as MuPDF leaves it.
    let output = scratch("fill-radio.pdf");
    let out = output.to_str().unwrap();
    let report = report_of(&["forms", input, "--set", "pick=Two", "-o", out]);
    assert_eq!(report["fields"][0]["old"], "One");
    assert_eq!(report["fields"][0]["new"], "Two");
    // The group may not be turned off (NoToggleToOff): toggling the first
    // button, which is on, leaves it on.
    let report = report_of(&["forms", input, "--toggle", "pick", "--dry-run"]);
    assert_eq!(report["fields"][0]["new"], "One");
    assert_eq!(report["fields"][0]["changed"], false);
    assert_eq!(report["applied"], false);
    // Toggling the second turns it on and the first off.
    let output = scratch("fill-radio-2.pdf");
    let out = output.to_str().unwrap();
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R/AcroForm<</Fields[4 0 R]>>>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 300]/Annots[5 0 R 6 0 R]>>",
        "<</T(pick)/FT/Btn/Ff 32768/V/One/Kids[5 0 R 6 0 R]>>",
        "<</Type/Annot/Subtype/Widget/Parent 4 0 R/AS/Off/Rect[10 130 30 150]/AP<</N<</Two 7 0 R/Off 7 0 R>>>>>>",
        "<</Type/Annot/Subtype/Widget/Parent 4 0 R/AS/One/Rect[40 130 60 150]/AP<</N<</One 7 0 R/Off 7 0 R>>>>>>",
        "<</Type/XObject/Subtype/Form/BBox[0 0 1 1]/Length 0>>\nstream\n\nendstream",
    ]);
    let radio = scratch("fill-radio-2-in.pdf");
    std::fs::write(&radio, pdf).unwrap();
    let report = report_of(&[
        "forms",
        radio.to_str().unwrap(),
        "--toggle",
        "pick",
        "-o",
        out,
    ]);
    assert_eq!(report["fields"][0]["new"], "Two");
    let objects = report_of(&["objects", out]);
    assert_eq!(objects["objects"]["5"]["d"]["AS"]["n"], "Two");
    assert_eq!(objects["objects"]["6"]["d"]["AS"]["n"], "Off");
    assert_eq!(objects["objects"]["4"]["d"]["V"]["n"], "Two");
}

#[test]
fn forms_fill_refuses_unknown_fields_and_notes_read_only_ones() {
    let file = fill_form();
    let input = file.to_str().unwrap();
    for (argv, code) in [
        (
            &["forms", input, "--set", "nobody=x", "--dry-run"][..],
            "usage.unknown_field",
        ),
        (&["forms", input, "--set", "person"], "usage.bad_value"),
        (
            &["forms", input, "--set", "person=x"],
            "usage.missing_output",
        ),
    ] {
        let out = nupdf(argv);
        assert_eq!(out.status.code(), Some(2), "{argv:?}");
        let err: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err["error"]["code"], code);
    }
    // Read-only: MuPDF's widget API sets it anyway, so nupdf does, and says.
    let ro = fill_form_with("/Ff 1", "fill-readonly.pdf");
    let report = report_of(&[
        "forms",
        ro.to_str().unwrap(),
        "--set",
        "person=x",
        "--dry-run",
    ]);
    assert_eq!(report["fields"][0]["new"], "x");
    assert!(
        report["fields"][0]["notes"][0]
            .as_str()
            .unwrap()
            .contains("read-only")
    );
    // A push button does not toggle.
    let report = report_of(&["forms", input, "--toggle", "person", "--dry-run"]);
    assert_eq!(report["fields"][0]["changed"], false);
}

#[test]
fn annotations_list_with_what_their_getters_read() {
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Annots[4 0 R 5 0 R 6 0 R]>>",
        "<</Type/Annot/Subtype/Square/Rect[10 10 50 50]/C[1 0 0]/IC[0 0 1]/BS<</W 2/S/D/D[3]>>/T(Ada)/Contents(Look)/CA 0.5>>",
        "<</Type/Annot/Subtype/Highlight/Rect[0 0 0 0]/QuadPoints[10 100 60 100 10 90 60 90]/C[1 1 0]>>",
        "<</Type/Annot/Subtype/Link/Rect[0 0 10 10]>>",
    ]);
    let file = scratch("annots.pdf");
    std::fs::write(&file, pdf).unwrap();
    let report = report_of(&["annots", file.to_str().unwrap()]);
    assert_eq!(report["schema"], "nupdf.annots/v1");
    let a = report["pages"][0]["annots"].as_array().unwrap();
    // Links are not annotations to MuPDF's list.
    assert_eq!(a.len(), 2);
    assert_eq!(a[0]["type"], "Square");
    assert_eq!(a[0]["author"], "Ada");
    assert_eq!(a[0]["contents"], "Look");
    assert_eq!(a[0]["opacity"], 0.5);
    assert_eq!(
        a[0]["border"],
        serde_json::json!({"style": "Dashed", "width": 2.0})
    );
    assert_eq!(a[0]["interior_color"], serde_json::json!([0.0, 0.0, 1.0]));
    // Page space, y down from the top of the 200-point page, less the /RD
    // that making the missing appearance puts in (half the border width).
    assert_eq!(a[0]["rect"], serde_json::json!([11.0, 151.0, 49.0, 189.0]));
    assert_eq!(
        a[1]["quad_points"],
        serde_json::json!([[10.0, 100.0, 60.0, 100.0, 10.0, 110.0, 60.0, 110.0]])
    );
    // --synthesize gives the appearance MuPDF would make.
    let made = report_of(&["annots", "--synthesize", file.to_str().unwrap()]);
    let content = made["pages"][0]["annots"][0]["synthesis"]["content"]
        .as_str()
        .unwrap();
    assert!(
        content.starts_with("/H gs\n[3]0 d\n2 w\n1 0 0 RG\n0 0 1 rg\n"),
        "{content}"
    );
}

#[test]
fn sanitize_removes_what_runs_and_proves_it() {
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R/OpenAction<</S/JavaScript/JS(app.alert(1))>>/Names<</JavaScript 9 0 R>>/AcroForm<</Fields[6 0 R]/XFA 10 0 R/CO[6 0 R]>>>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Annots[4 0 R 5 0 R 6 0 R 7 0 R]/AA<</O 8 0 R>>>>",
        "<</Type/Annot/Subtype/Link/Rect[0 0 10 10]/A<</S/URI/URI(https://evil.example.com/x)>>>>",
        "<</Type/Annot/Subtype/Link/Rect[0 20 10 30]/A<</S/URI/URI(https://docs.good.org/)/Next[<</S/Launch/F(calc.exe)>>]>>>>",
        "<</Type/Annot/Subtype/Widget/FT/Tx/T(f)/Rect[0 40 10 50]/AA<</K<</S/JavaScript/JS(event.rc=1)>>>>>>",
        "<</Type/Annot/Subtype/Link/Rect[0 60 10 70]/A<</S/GoToR/F(other.pdf)/D[0/Fit]>>>>",
        "<</S/SubmitForm/F(https://collect.example.com)>>",
        "<</Names[(init)<</S/JavaScript/JS(this.print())>>]>>",
        "<</Length 0>>\nstream\n\nendstream",
    ]);
    let file = scratch("risky.pdf");
    std::fs::write(&file, pdf).unwrap();
    let out = scratch("risky-clean.pdf");
    let report = run_json(&[
        "sanitize",
        file.to_str().unwrap(),
        "--uri",
        "allow:good.org",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(report["schema"], "nupdf.sanitize/v1");
    assert_eq!(report["clean"], true);
    let counts = &report["counts"];
    assert_eq!(counts["javascript"], 4, "{report}");
    assert_eq!(counts["launch"], 1);
    assert_eq!(counts["submit"], 1);
    assert_eq!(counts["remote"], 1);
    assert_eq!(counts["uri"], 1);
    assert_eq!(counts["xfa"], 1);
    // Nothing is left to find, and the allowed link still leads somewhere.
    let again = run_json(&[
        "sanitize",
        out.to_str().unwrap(),
        "--uri",
        "allow:good.org",
        "--dry-run",
    ]);
    assert_eq!(again["removed"], serde_json::json!([]));
    let links = report_of(&["links", out.to_str().unwrap()]);
    let uris: Vec<&Value> = links["pages"][0]["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| &l["uri"])
        .collect();
    assert!(
        uris.contains(&&Value::from("https://docs.good.org/")),
        "{links}"
    );
    assert!(!links.to_string().contains("evil.example.com"));
    // --keep keeps; a bad policy is a usage error.
    let kept = run_json(&[
        "sanitize",
        file.to_str().unwrap(),
        "--keep",
        "javascript,xfa",
        "--dry-run",
    ]);
    assert!(kept["counts"].get("javascript").is_none());
    assert_eq!(
        nupdf(&[
            "sanitize",
            file.to_str().unwrap(),
            "--uri",
            "maybe",
            "--dry-run"
        ])
        .status
        .code(),
        Some(2)
    );
}

/// A page that is one gray image and no text, as a scanner makes them.
fn scanned_pdf(name: &str) -> PathBuf {
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 100]/Contents 4 0 R/Resources<</XObject<</Im0 5 0 R>>>>>>",
        "<</Length 26>>\nstream\nq 200 0 0 100 0 0 cm /Im0 Do Q\nendstream",
        "<</Type/XObject/Subtype/Image/Width 2/Height 2/ColorSpace/DeviceGray/BitsPerComponent 8/Length 4>>\nstream\n\u{80}\u{90}\u{a0}\u{b0}\nendstream",
    ]);
    let file = scratch(name);
    std::fs::write(&file, pdf).unwrap();
    file
}

#[test]
fn info_tells_pages_with_text_from_scanned_ones() {
    // Text and then a page's worth of image; the image alone; nothing.
    let page = |contents: u32| {
        format!(
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 100]/Contents {contents} 0 R/Resources<</XObject<</Im0 6 0 R>>/Font<</F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>>>>>>>"
        )
    };
    let text = "BT /F1 12 Tf 10 10 Td (Hello) Tj ET q 200 0 0 100 0 0 cm /Im0 Do Q";
    let image = "q 200 0 0 100 0 0 cm /Im0 Do Q";
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R>>",
        "<</Type/Pages/Kids[3 0 R 4 0 R 5 0 R]/Count 3>>",
        &page(7),
        &page(8),
        &page(9),
        "<</Type/XObject/Subtype/Image/Width 2/Height 2/ColorSpace/DeviceGray/BitsPerComponent 8/Length 4>>\nstream\n\u{80}\u{90}\u{a0}\u{b0}\nendstream",
        &format!("<</Length {}>>\nstream\n{text}\nendstream", text.len()),
        &format!("<</Length {}>>\nstream\n{image}\nendstream", image.len()),
        "<</Length 0>>\nstream\n\nendstream",
    ]);
    let file = scratch("text-and-scanned.pdf");
    std::fs::write(&file, pdf).unwrap();
    let info = run_json(&["info", file.to_str().unwrap()]);
    assert_eq!(info["text"]["checked"], true);
    assert_eq!(info["text"]["pages_with_text"], 1);
    assert_eq!(info["text"]["scanned_pages"], serde_json::json!([2]));
}

#[test]
fn ocr_puts_what_tesseract_reads_on_scanned_pages() {
    let file = scanned_pdf("scanned.pdf");
    let info = run_json(&["info", file.to_str().unwrap()]);
    assert_eq!(info["text"]["scanned_pages"], serde_json::json!([1]));
    assert_eq!(info["text"]["pages_with_text"], 0);

    // A stand-in for Tesseract: two words, wherever it is pointed.
    let fake = scratch("fake-tesseract.sh");
    std::fs::write(
        &fake,
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'tesseract 5.0.0 (stand-in)'; exit 0; fi\n\
         printf 'level\\tpage_num\\tblock_num\\tpar_num\\tline_num\\tword_num\\tleft\\ttop\\twidth\\theight\\tconf\\ttext\\n'\n\
         printf '5\\t1\\t1\\t1\\t1\\t1\\t100\\t100\\t200\\t60\\t95\\tScanned\\n'\n\
         printf '5\\t1\\t1\\t1\\t1\\t2\\t320\\t100\\t150\\t60\\t40\\twords\\n'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = scratch("scanned-ocr.pdf");
    let report = run_json(&[
        "ocr",
        file.to_str().unwrap(),
        "--tesseract",
        fake.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(report["schema"], "nupdf.ocr/v1");
    assert_eq!(report["tesseract"], "tesseract 5.0.0 (stand-in)");
    assert_eq!(report["pages"][0]["words"], 2);
    let text = nupdf(&["extract", out.to_str().unwrap(), "--format", "text"]);
    assert!(
        String::from_utf8_lossy(&text.stdout).contains("Scanned words"),
        "{}",
        String::from_utf8_lossy(&text.stdout)
    );
    // The words sit where Tesseract saw them: 300 dpi pixels, in points.
    let hits = report_of(&["search", out.to_str().unwrap(), "Scanned"]);
    let quad = &hits["hits"][0]["quads"][0];
    assert!((quad[0].as_f64().unwrap() - 24.0).abs() < 0.5, "{hits}");
    // Too unsure: left out.
    let report = run_json(&[
        "ocr",
        file.to_str().unwrap(),
        "--tesseract",
        fake.to_str().unwrap(),
        "--min-confidence",
        "50",
        "--dry-run",
    ]);
    assert_eq!(report["pages"][0]["words"], 1);
    // No Tesseract: a clear error.
    let missing = nupdf(&[
        "ocr",
        file.to_str().unwrap(),
        "--tesseract",
        "/nonexistent/tesseract",
        "--dry-run",
    ]);
    assert_eq!(missing.status.code(), Some(3));
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&missing.stdout),
        String::from_utf8_lossy(&missing.stderr)
    );
    assert!(said.contains("ocr.unavailable"), "{said}");
}

#[test]
fn flatten_draws_annotations_into_the_page() {
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R/AcroForm<</Fields[6 0 R]>>>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Annots[4 0 R 5 0 R 6 0 R]>>",
        "<</Type/Annot/Subtype/Square/Rect[10 10 50 50]/C[1 0 0]>>",
        "<</Type/Annot/Subtype/Link/Rect[0 0 10 10]/A<</S/URI/URI(https://example.com)>>>>",
        "<</Type/Annot/Subtype/Widget/FT/Tx/T(name)/V(Ada)/DA(/Helv 10 Tf 0 g)/Rect[60 60 150 80]>>",
    ]);
    let file = scratch("to-flatten.pdf");
    std::fs::write(&file, pdf).unwrap();
    let out = scratch("flattened.pdf");
    let report = run_json(&[
        "flatten",
        file.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(report["schema"], "nupdf.flatten/v1");
    assert_eq!(
        (report["annots"].as_u64(), report["widgets"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(report["unmade"], serde_json::json!([]));
    // What is left: the link. The form is gone, the field's text is page text.
    let annots = report_of(&["annots", out.to_str().unwrap()]);
    assert_eq!(annots["pages"][0]["annots"], serde_json::json!([]));
    let links = report_of(&["links", out.to_str().unwrap()]);
    assert_eq!(links["pages"][0]["links"].as_array().unwrap().len(), 1);
    let forms = report_of(&["forms", out.to_str().unwrap()]);
    assert_eq!(forms["pages"][0]["widgets"], serde_json::json!([]));
    let text = nupdf(&["extract", out.to_str().unwrap(), "--format", "text"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("Ada"));
    // --annots-only keeps the field.
    let kept = run_json(&[
        "flatten",
        file.to_str().unwrap(),
        "--annots-only",
        "--dry-run",
    ]);
    assert_eq!(kept["widgets"], 0);
}

/// With Tesseract on the path: a page of text read back by OCR.
#[test]
fn ocr_reads_a_real_page_with_tesseract() {
    if std::process::Command::new("tesseract")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("tesseract not found; skipped");
        return;
    }
    let content = "BT /F1 36 Tf 40 120 Td (Invoice 2026) Tj ET";
    let page = format!(
        "<</Length {}>>\nstream\n{content}\nendstream",
        content.len()
    );
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 400 200]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
        &page,
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
    ]);
    let file = scratch("ocr-real.pdf");
    std::fs::write(&file, pdf).unwrap();
    let out = scratch("ocr-real-out.pdf");
    let report = run_json(&[
        "ocr",
        file.to_str().unwrap(),
        "--all",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(report["pages"][0]["words"], 2, "{report}");
    // The page now has its text twice: drawn, and read.
    let hits = report_of(&["search", out.to_str().unwrap(), "Invoice"]);
    assert_eq!(hits["hits"].as_array().unwrap().len(), 2, "{hits}");
}

fn annotated_pdf() -> PathBuf {
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Annots[4 0 R 5 0 R]>>",
        "<</Type/Annot/Subtype/Text/Rect[10 10 30 30]/Contents(old)/Popup 5 0 R>>",
        "<</Type/Annot/Subtype/Popup/Parent 4 0 R/Rect[40 10 140 60]>>",
    ]);
    let file = scratch("annot-edit.pdf");
    std::fs::write(&file, pdf).unwrap();
    file
}

#[test]
fn annots_adds_and_deletes_and_the_listing_shows_it() {
    let file = annotated_pdf();
    let out = scratch("annot-edit-out.pdf");
    let _ = std::fs::remove_file(&out);
    let report = report_of(&[
        "annots",
        file.to_str().unwrap(),
        "--delete",
        "p1.a1",
        "--add",
        r#"{"page":1,"type":"Square","rect":[20,20,80,60],"color":[0,0,1],"contents":"box"}"#,
        "--add",
        r#"{"page":1,"type":"Highlight","quad_points":[[10,100,90,100,10,112,90,112]]}"#,
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(report["schema"], "nupdf.annots-edit/v1");
    assert_eq!(report["deleted"], serde_json::json!(["p1.a1"]));
    assert_eq!(report["added"][0]["id"], "p1.a1");
    assert_eq!(report["added"][0]["object"], 6);
    assert_eq!(report["added"][1]["id"], "p1.a2");
    assert_eq!(report["added"][1]["type"], "Highlight");
    assert_eq!(report["unmade"], serde_json::json!([]));
    // The note and its popup are gone; the new ones list as given.
    let listed = report_of(&["annots", out.to_str().unwrap()]);
    let annots = listed["pages"][0]["annots"].as_array().unwrap();
    assert_eq!(annots.len(), 2);
    assert_eq!(annots[0]["type"], "Square");
    assert_eq!(
        annots[0]["rect"],
        serde_json::json!([20.0, 20.0, 80.0, 60.0])
    );
    assert_eq!(annots[0]["color"], serde_json::json!([0.0, 0.0, 1.0]));
    assert_eq!(annots[0]["contents"], "box");
    assert_eq!(annots[0]["flags"], 4);
    assert_eq!(
        annots[1]["quad_points"],
        serde_json::json!([[10.0, 100.0, 90.0, 100.0, 10.0, 112.0, 90.0, 112.0]])
    );
    assert_eq!(annots[1]["color"], serde_json::json!([1.0, 1.0, 0.0]));
}

#[test]
fn annots_without_edits_still_lists_and_edits_need_somewhere_to_go() {
    let file = annotated_pdf();
    let listed = report_of(&["annots", file.to_str().unwrap()]);
    assert_eq!(listed["schema"], "nupdf.annots/v1");
    let run = nupdf(&["annots", file.to_str().unwrap(), "--delete", "p1.a1"]);
    assert_eq!(run.status.code(), Some(2));
}

#[test]
fn annots_refuses_bad_edits() {
    let file = annotated_pdf();
    let f = file.to_str().unwrap();
    let code = |args: &[&str]| nupdf(args).status.code();
    // Not an id, no such annotation, no such page.
    assert_eq!(code(&["annots", f, "--delete", "a1", "--dry-run"]), Some(2));
    assert_eq!(
        code(&["annots", f, "--delete", "p1.a9", "--dry-run"]),
        Some(3)
    );
    assert_eq!(
        code(&[
            "annots",
            f,
            "--add",
            r#"{"page":4,"type":"Ink"}"#,
            "--dry-run"
        ]),
        Some(3)
    );
    // An unknown field, and a property the type does not have.
    assert_eq!(
        code(&[
            "annots",
            f,
            "--add",
            r#"{"page":1,"type":"Ink","colour":[1]}"#,
            "--dry-run"
        ]),
        Some(2)
    );
    let run = nupdf(&[
        "annots",
        f,
        "--add",
        r#"{"page":1,"type":"Ink","rect":[0,0,1,1]}"#,
        "--dry-run",
    ]);
    assert_eq!(run.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&run.stderr).contains("Ink annotations have no Rect property"));
}

#[test]
fn annots_highlights_each_search_hit() {
    let input = smoke("minimal.pdf");
    let report = report_of(&[
        "annots",
        input.to_str().unwrap(),
        "--highlight",
        "nupdf",
        "--dry-run",
    ]);
    let added = report["added"].as_array().unwrap();
    assert_eq!(added.len(), 1);
    assert_eq!(added[0]["type"], "Highlight");
    assert_eq!(added[0]["id"], "p1.a1");
}

#[test]
fn mcp_runs_a_tool_on_many_files_in_one_call_and_teaches_itself() {
    let mut mcp = Mcp::start();
    // The instructions are the guide, so a model starts knowing the tools.
    mcp.send(
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "test", "version": "1"}}}),
    );
    let init = mcp.next();
    let said = init["result"]["instructions"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        said.contains("redact") && said.contains("list of paths"),
        "{said}"
    );
    mcp.send(serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = mcp.next()["result"]["tools"].clone();
    let search = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "search")
        .unwrap();
    assert_eq!(
        search["inputSchema"]["properties"]["file"]["type"],
        serde_json::json!(["string", "array"])
    );
    let a = smoke("stext.pdf");
    let b = smoke("minimal.pdf");
    let result = mcp.call(
        3,
        "search",
        serde_json::json!({"file": [a.to_str().unwrap(), b.to_str().unwrap()], "needle": "Hello"}),
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // A line for each file, then the summary.
    assert_eq!(lines.len(), 3, "{text}");
    assert_eq!(lines[0]["schema"], "nupdf.batch.item/v1");
    assert!(
        lines[0]["report"]["hits"]
            .as_array()
            .is_some_and(|h| !h.is_empty()),
        "{text}"
    );
    assert_eq!(lines[2]["schema"], "nupdf.batch/v1");
    mcp.close();
}

/// Found by the stext fuzz target: structure parents in a cycle, which
/// MuPDF climbs forever. nupdf stops, says so, and finishes the page.
#[test]
fn a_structure_tree_cycle_ends() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/structure-cycle.pdf");
    let started = std::time::Instant::now();
    let out = nupdf(&["stext", file.to_str().unwrap(), "--timeout", "60"]);
    assert!(
        started.elapsed().as_secs() < 30,
        "took {:?}",
        started.elapsed()
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "structure.cycle"),
        "{report}"
    );
}

#[test]
fn attest_detects_occluded_text_vector_strikethrough_and_actualtext_spoof() {
    // Stream draws (PDF user space: origin at bottom-left, y up):
    // 1) "SECRET" at y=250 covered by a black rectangle (faux redaction / occlusion)
    // 2) "OLD CLAUSE" at y=200 crossed out by a horizontal vector stroke at y=204
    // 3) "(Total: 900 USD)" wrapped in /Span << /ActualText (Fee is $100.) >>
    // 4) Dense vector rectangle cluster at y=40..90 with no text (blind spot)
    let content = b"BT /F1 12 Tf 20 250 Td (SECRET) Tj ET\n\
0 0 0 rg 18 246 65 18 re f\n\
0 0 0 rg BT /F1 12 Tf 20 200 Td (OLD CLAUSE) Tj ET\n\
0 0 0 RG 1.5 w 18 204 m 105 204 l S\n\
/Span <</ActualText (Fee is $100.)>> BDC\n\
BT /F1 12 Tf 20 150 Td (Total: 900 USD) Tj ET\n\
EMC\n\
0.2 0.4 0.8 rg 180 40 15 45 re f 200 40 15 35 re f 220 40 15 50 re f 240 40 15 30 re f 260 40 15 40 re f\n";
    let page_stream = format!(
        "<</Length {}>>\nstream\n{}endstream",
        content.len(),
        std::str::from_utf8(content).unwrap()
    );
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 300]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
        &page_stream,
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
    ]);
    let file = scratch("attest-hazards.pdf");
    std::fs::write(&file, pdf).unwrap();
    let crops = scratch("attest-crops");
    let _ = std::fs::remove_dir_all(&crops);

    let report = report_of(&[
        "attest",
        file.to_str().unwrap(),
        "--crops-dir",
        crops.to_str().unwrap(),
    ]);
    assert_eq!(report["schema"], "nupdf.attest/v1");
    let spans = report["pages"][0]["spans"].as_array().unwrap();
    assert!(
        spans.iter().any(
            |s| s["text"].as_str().unwrap_or_default().contains("SECRET")
                && matches!(
                    s["visibility"].as_str(),
                    Some("zero_contrast_or_occluded" | "invisible_mode")
                )
        ),
        "expected occluded SECRET in {spans:?}"
    );
    assert!(
        spans.iter().any(|s| s["text"]
            .as_str()
            .unwrap_or_default()
            .contains("OLD CLAUSE")
            && s["struck_through"] == true),
        "expected struck_through OLD CLAUSE in {spans:?}"
    );
    assert!(
        spans.iter().any(|s| s["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Fee is $100.")
            && s["glyph_text"]
                .as_str()
                .unwrap_or_default()
                .contains("Total: 900 USD")),
        "expected ActualText spoof detection in {spans:?}"
    );
}

#[test]
fn patch_replaces_text_in_place_and_refuses_neighbor_collision() {
    let content = b"BT /F1 12 Tf 20 200 Td (Amount: $100) Tj ET\n\
BT /F1 12 Tf 140 200 Td (Status: OPEN) Tj ET\n";
    let page_stream = format!(
        "<</Length {}>>\nstream\n{}endstream",
        content.len(),
        std::str::from_utf8(content).unwrap()
    );
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 300]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
        &page_stream,
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
    ]);
    let file = scratch("patch-in.pdf");
    std::fs::write(&file, pdf).unwrap();
    let extracted = report_of(&["extract", file.to_str().unwrap()]);
    let amount_id = extracted["pages"][0]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| b["lines"].as_array().into_iter().flatten())
        .flat_map(|l| l["spans"].as_array().into_iter().flatten())
        .find(|s| s["text"].as_str().unwrap_or_default().contains("$100"))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let out = scratch("patch-out.pdf");
    let _ = std::fs::remove_file(&out);
    let replace_ok = format!("{amount_id}=Amount: $250");
    let report = report_of(&[
        "patch",
        file.to_str().unwrap(),
        "--replace",
        &replace_ok,
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(report["schema"], "nupdf.patch/v1");
    assert_eq!(report["applied"], true);
    assert_eq!(report["verification"]["verified"], true);
    let hits_new = report_of(&["search", out.to_str().unwrap(), "$250"]);
    assert_eq!(hits_new["hit_count"], 1, "{hits_new}");
    let hits_old = report_of(&["search", out.to_str().unwrap(), "$100"]);
    assert_eq!(hits_old["hit_count"], 0, "{hits_old}");

    // Oversized replacement that collides horizontally with "Status: OPEN" must exit 4 and not write.
    let bad_out = scratch("patch-collision-out.pdf");
    let _ = std::fs::remove_file(&bad_out);
    let replace_bad = format!("{amount_id}=Amount: $999,999,999,999.99 USD EXTRA WIDE");
    let bad = nupdf(&[
        "patch",
        file.to_str().unwrap(),
        "--replace",
        &replace_bad,
        "-o",
        bad_out.to_str().unwrap(),
    ]);
    assert_eq!(
        bad.status.code(),
        Some(4),
        "stderr={}",
        String::from_utf8_lossy(&bad.stderr)
    );
    assert!(!bad_out.exists(), "colliding patch must not write output");
}

#[test]
fn apply_commits_disjoint_multi_agent_plans_and_rejects_conflicts() {
    let content = b"BT /F1 12 Tf 20 240 Td (SSN: 123-45-6789) Tj ET\n\
BT /F1 12 Tf 20 180 Td (Total: $100) Tj ET\n";
    let page_stream = format!(
        "<</Length {}>>\nstream\n{}endstream",
        content.len(),
        std::str::from_utf8(content).unwrap()
    );
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 300]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
        &page_stream,
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
    ]);
    let file = scratch("apply-in.pdf");
    std::fs::write(&file, pdf).unwrap();
    let extracted = report_of(&["extract", file.to_str().unwrap()]);
    let doc_id = extracted["doc_id"].as_str().unwrap().to_string();
    let total_id = extracted["pages"][0]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| b["lines"].as_array().into_iter().flatten())
        .flat_map(|l| l["spans"].as_array().into_iter().flatten())
        .find(|s| s["text"].as_str().unwrap_or_default().contains("$100"))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let plan_a = scratch("plan-a.json");
    let plan_b = scratch("plan-b.json");
    std::fs::write(
        &plan_a,
        serde_json::to_vec(&serde_json::json!({
            "doc_id": doc_id,
            "agent": "redactor",
            "ops": [{"op": "redact", "text": "123-45-6789"}]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        &plan_b,
        serde_json::to_vec(&serde_json::json!({
            "doc_id": doc_id,
            "agent": "editor",
            "ops": [{"op": "patch", "id": total_id, "text": "Total: $250"}]
        }))
        .unwrap(),
    )
    .unwrap();

    let out = scratch("apply-out.pdf");
    let _ = std::fs::remove_file(&out);
    let report = report_of(&[
        "apply",
        file.to_str().unwrap(),
        "--plan",
        plan_a.to_str().unwrap(),
        "--plan",
        plan_b.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(report["schema"], "nupdf.apply/v1");
    assert_eq!(report["applied"], true);
    assert_eq!(report["verified"], true);
    assert!(report["conflicts"].as_array().unwrap().is_empty());

    // Conflicting second plan patching the exact same target with a different value exits 4.
    let plan_c = scratch("plan-c.json");
    std::fs::write(
        &plan_c,
        serde_json::to_vec(&serde_json::json!({
            "doc_id": doc_id,
            "agent": "rival-editor",
            "ops": [{"op": "patch", "id": total_id, "text": "Total: $999"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let conflict_out = scratch("apply-conflict.pdf");
    let _ = std::fs::remove_file(&conflict_out);
    let conflict = nupdf(&[
        "apply",
        file.to_str().unwrap(),
        "--plan",
        plan_b.to_str().unwrap(),
        "--plan",
        plan_c.to_str().unwrap(),
        "-o",
        conflict_out.to_str().unwrap(),
    ]);
    assert_eq!(
        conflict.status.code(),
        Some(4),
        "stderr={}",
        String::from_utf8_lossy(&conflict.stderr)
    );
    assert!(!conflict_out.exists());
}

#[test]
fn query_extracts_spatial_values_and_aggregates_across_directory() {
    let dir = scratch("query-corpus");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, amt) in [("inv1.pdf", "$120.00"), ("inv2.pdf", "$180.00")] {
        let content = format!(
            "BT /F1 12 Tf 20 200 Td (Total Due) Tj ET\nBT /F1 12 Tf 140 200 Td ({amt}) Tj ET\n"
        );
        let stream = format!("<</Length {}>>\nstream\n{content}endstream", content.len());
        let pdf = pdf_of(&[
            "<</Type/Catalog/Pages 2 0 R>>",
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 300]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
            &stream,
            "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
        ]);
        std::fs::write(dir.join(name), pdf).unwrap();
    }

    let one = report_of(&[
        "query",
        dir.join("inv1.pdf").to_str().unwrap(),
        "--right-of",
        "Total Due",
        "--nearest-only",
    ]);
    assert_eq!(one["schema"], "nupdf.query/v1");
    assert_eq!(one["aggregation"]["sum"], 120.0, "{one}");

    let corpus = report_of(&[
        "query",
        dir.to_str().unwrap(),
        "--right-of",
        "Total Due",
        "--nearest-only",
        "--summary-only",
    ]);
    assert_eq!(corpus["schema"], "nupdf.query/v1");
    assert_eq!(corpus["file_count"], 2, "{corpus}");
    assert_eq!(corpus["aggregation"]["sum"], 300.0, "{corpus}");
}

#[test]
fn audit_verifies_claims_relocates_pages_flags_hazards_and_enforces_strict() {
    let p1 = b"BT /F1 12 Tf 20 250 Td (Introductory summary.) Tj ET\n";
    let p2 = b"BT /F1 12 Tf 20 250 Td (Liability capped at $500) Tj ET\n\
BT /F1 7 Tf 147 254 Td (*) Tj ET\n\
BT /F1 12 Tf 20 200 Td (Old warranty applies) Tj ET\n\
0 0 0 RG 1.5 w 18 204 m 140 204 l S\n\
BT /F1 9 Tf 20 30 Td (* Excludes gross negligence.) Tj ET\n";
    let s1 = format!(
        "<</Length {}>>\nstream\n{}endstream",
        p1.len(),
        std::str::from_utf8(p1).unwrap()
    );
    let s2 = format!(
        "<</Length {}>>\nstream\n{}endstream",
        p2.len(),
        std::str::from_utf8(p2).unwrap()
    );
    let pdf = pdf_of(&[
        "<</Type/Catalog/Pages 2 0 R>>",
        "<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 300]/Contents 5 0 R/Resources<</Font<</F1 7 0 R>>>>>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 300]/Contents 6 0 R/Resources<</Font<</F1 7 0 R>>>>>>",
        &s1,
        &s2,
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
    ]);
    let file = scratch("audit-in.pdf");
    std::fs::write(&file, pdf).unwrap();
    let claims = scratch("audit-claims.json");
    std::fs::write(
        &claims,
        serde_json::to_vec(&serde_json::json!([
            {"claim_id": "c1", "page": 1, "quote": "Liability capped at $500"},
            {"claim_id": "c2", "page": 2, "quote": "Liability capped at $500"},
            {"claim_id": "c3", "page": 2, "quote": "Old warranty applies"}
        ]))
        .unwrap(),
    )
    .unwrap();

    let ev_dir = scratch("audit-ev");
    let _ = std::fs::remove_dir_all(&ev_dir);
    let report = report_of(&[
        "audit",
        file.to_str().unwrap(),
        "--claims",
        claims.to_str().unwrap(),
        "--evidence-dir",
        ev_dir.to_str().unwrap(),
    ]);
    assert_eq!(report["schema"], "nupdf.audit/v1");
    let results = report["results"].as_array().unwrap();
    assert_eq!(results[0]["status"], "wrong_location", "{report}");
    assert!(
        results[0]["found_elsewhere"]
            .as_array()
            .unwrap()
            .iter()
            .any(|loc| loc.as_str().unwrap_or_default().starts_with("p2.")),
        "{report}"
    );
    assert_eq!(results[1]["status"], "verified_with_hazards", "{report}");
    assert!(
        results[1]["hazards"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h == "hazard.unreferenced_footnote"),
        "expected footnote hazard: {report}"
    );
    assert_eq!(results[2]["status"], "verified_with_hazards", "{report}");
    assert!(
        results[2]["hazards"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h == "hazard.struck_through"),
        "expected strikethrough hazard: {report}"
    );

    let strict = nupdf(&[
        "audit",
        file.to_str().unwrap(),
        "--claims",
        claims.to_str().unwrap(),
        "--strict",
    ]);
    assert_eq!(strict.status.code(), Some(4));
}

#[test]
fn mcp_exposes_all_five_agent_primitives() {
    let mut mcp = Mcp::start();
    mcp.send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"}
        }
    }));
    let _ = mcp.next();
    mcp.send(serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = mcp.next()["result"]["tools"].clone();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in ["attest", "patch", "apply", "query", "audit"] {
        assert!(
            names.contains(&expected),
            "missing {expected} in MCP tools: {names:?}"
        );
    }
    let minimal = smoke("minimal.pdf");
    let res = mcp.call(
        3,
        "attest",
        serde_json::json!({"file": minimal.to_str().unwrap()}),
    );
    assert_eq!(
        res["structuredContent"]["schema"], "nupdf.attest/v1",
        "{res}"
    );
    mcp.close();
}

#[test]
fn patch_accepts_a_line_on_single_spaced_text() {
    // stext.pdf's paragraph lines are 16.5pt boxes 14pt apart: each box
    // already overlaps the next one's, which is no collision of a patch.
    let input = smoke("stext.pdf");
    for text in ["A new paragraph", "A new and longer paragraph"] {
        let replace = format!("p1.b2.l1.s1={text}");
        let report = report_of(&[
            "patch",
            input.to_str().unwrap(),
            "--replace",
            &replace,
            "--dry-run",
        ]);
        assert_eq!(report["verification"]["verified"], true, "{report}");
    }
}
