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
    assert_eq!(report["schema"], "nupdf.meta/v0");
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
    assert_eq!(report["schema"], "nupdf.pages/v0");
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
    assert_eq!(report["schema"], "nupdf.split/v0");
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
    assert_eq!(report["schema"], "nupdf.encrypt/v0");
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
    assert_eq!(report["schema"], "nupdf.decrypt/v0");
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
    assert_eq!(report["schema"], "nupdf.merge/v0");
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
    assert_eq!(report["schema"], "nupdf.fonts/v0");
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
    assert_eq!(report["schema"], "nupdf.images/v0");
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
    assert_eq!(report["schema"], "nupdf.stext/v0");
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
    assert_eq!(report["schema"], "nupdf.extract/v0");
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
    assert_eq!(report["schema"], "nupdf.extract/v0");
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
    assert_eq!(lines[0]["schema"], "nupdf.extract.page/v0");
    assert_eq!(lines[1]["id"], "p2");
    assert_eq!(lines[2]["schema"], "nupdf.extract/v0");
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
    assert_eq!(report["schema"], "nupdf.search/v0");
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
    assert_eq!(report["schema"], "nupdf.redact/v0");
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
    assert_eq!(report["schema"], "nupdf.render/v0");
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
            format!("nupdf.{tool}/v0")
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
    assert_eq!(result["structuredContent"]["schema"], "nupdf.extract/v0");
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
    assert_eq!(summary["schema"], "nupdf.batch/v0");
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
    assert_eq!(same["schema"], "nupdf.diff/v0");
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
