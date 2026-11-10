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
    assert_eq!(report["schema"], "nupdf.streams/v0");
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
    assert_eq!(report["schema"], "nupdf.clean/v0");
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
