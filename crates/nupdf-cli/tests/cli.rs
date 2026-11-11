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
