// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Head-to-head native Rust-to-Rust evaluation of `nupdf`'s v2 agent primitives
//! (`chart`, `flat_form`, `reflow`, `history`, `cloak`/`uncloak`, `pipeline`)
//! against `pdf_oxide` 0.3.78 in the same process, compiled with thin LTO and
//! `codegen-units = 1` (`bench/engines/Cargo.toml`).
//!
//! Usage:
//!     primitives_v2_vs_oxide <work_dir>

use std::hint::black_box;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use nupdf::Document;
use nupdf::chart;
use nupdf::cloak::{self, CloakTarget};
use nupdf::flat_form::{self, FlatFillOp};
use nupdf::history::{self, HistoryOptions};
use nupdf::patch::{self, PatchOptions, PatchSpec};
use nupdf::pipeline;
use nupdf::write::WriteOptions;
use pdf_oxide::PdfDocument;
use pdf_oxide::editor::DocumentEditor;
use serde_json::{Value, json};

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return 0.0;
    }
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn exp1_chart_native(work: &Path) -> Result<Value, String> {
    let bytes: Arc<[u8]> = std::fs::read(work.join("chart_showcase.pdf"))
        .map_err(|e| format!("read chart_showcase.pdf: {e}"))?
        .into();

    // 1. Native pdf_oxide: text, spans, and structured extraction across both pages
    let mut oxide_times = Vec::new();
    let mut oxide_saw_exact_bar_42_37 = false;
    let mut oxide_saw_exact_bar_68_91 = false;
    let mut oxide_span_count = 0usize;
    for iter in 0..15 {
        let copy = bytes.to_vec();
        let t0 = Instant::now();
        let doc = PdfDocument::from_bytes(copy).map_err(|e| e.to_string())?;
        let pc = doc.page_count().map_err(|e| e.to_string())?;
        let mut combined = String::new();
        let mut spans_total = 0usize;
        for p in 0..pc {
            let txt = doc.extract_text(p).map_err(|e| e.to_string())?;
            let spans = doc.extract_spans(p).map_err(|e| e.to_string())?;
            combined.push_str(&txt);
            spans_total += spans.len();
        }
        if iter == 0 {
            oxide_saw_exact_bar_42_37 = combined.contains("42.37");
            oxide_saw_exact_bar_68_91 = combined.contains("68.91");
            oxide_span_count = spans_total;
        }
        black_box(combined);
        oxide_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    // 2. Native nupdf::chart::decompile_charts
    let mut nupdf_times = Vec::new();
    let mut last_rep = None;
    for iter in 0..15 {
        let t0 = Instant::now();
        let doc = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
        let rep = chart::decompile_charts(&doc, None)?;
        if iter == 0 {
            last_rep = Some(rep.clone());
        }
        black_box(rep);
        nupdf_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    let rep = last_rep.ok_or_else(|| "missing chart report".to_string())?;

    Ok(json!({
        "pdf_oxide_extract_median_ms": round3(median(oxide_times)),
        "pdf_oxide_span_count": oxide_span_count,
        "pdf_oxide_recovered_bar_42_37": oxide_saw_exact_bar_42_37,
        "pdf_oxide_recovered_bar_68_91": oxide_saw_exact_bar_68_91,
        "pdf_oxide_charts_api_exists": false,
        "pdf_oxide_diagrams_api_exists": false,
        "nupdf_chart_in_process_median_ms": round3(median(nupdf_times)),
        "nupdf_charts_found": rep.charts.len(),
        "nupdf_diagrams_found": rep.diagrams.len(),
        "nupdf_report": rep,
    }))
}

fn exp2_flat_forms_native(work: &Path) -> Result<Value, String> {
    let bytes: Arc<[u8]> = std::fs::read(work.join("flat_intake.pdf"))
        .map_err(|e| format!("read flat_intake.pdf: {e}"))?
        .into();

    // 1. Native pdf_oxide on the flat (0 AcroForm) PDF:
    let mut oxide_ed = DocumentEditor::from_bytes(bytes.to_vec()).map_err(|e| e.to_string())?;
    let oxide_flat_fields = oxide_ed.get_form_fields().map_err(|e| e.to_string())?.len();

    // 2. Native nupdf::flat_form detect + fill + promote:
    let mut detect_times = Vec::new();
    let mut detected_count = 0usize;
    for iter in 0..15 {
        let t0 = Instant::now();
        let doc = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
        let rep = flat_form::detect_flat_forms(&doc, None)?;
        if iter == 0 {
            detected_count = rep.fields.len();
        }
        black_box(rep);
        detect_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    let doc = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
    let ops = [
        FlatFillOp::SetText {
            target: "applicant_name".into(),
            value: "Ada Lovelace".into(),
        },
        FlatFillOp::SetText {
            target: "ssn".into(),
            value: "987654321".into(),
        },
        FlatFillOp::SetCheck {
            target: "us_citizen".into(),
            checked: true,
        },
    ];
    let t_fill = Instant::now();
    let (filled_bytes, fill_rep) =
        flat_form::fill_flat_forms(&doc, &ops, &WriteOptions::default())
            .map_err(|e| e.to_string())?;
    let fill_ms = t_fill.elapsed().as_secs_f64() * 1000.0;
    std::fs::write(work.join("oxide_check_nupdf_flat_filled.pdf"), &filled_bytes)
        .map_err(|e| e.to_string())?;

    // Verify comb overflow rejection (12 chars into 9-cell SSN comb):
    let bad_ops = [FlatFillOp::SetText {
        target: "ssn".into(),
        value: "123456789012".into(),
    }];
    let comb_overflow_rejected =
        flat_form::fill_flat_forms(&doc, &bad_ops, &WriteOptions::default())
            .err()
            .is_some_and(|e| e.code == "verify.comb_overflow");

    // Promote to real /AcroForm widgets and verify pdf_oxide now sees all 6 form fields!
    let t_prom = Instant::now();
    let (promoted_bytes, prom_rep) =
        flat_form::promote_flat_forms(&doc, &WriteOptions::default())
            .map_err(|e| e.to_string())?;
    let promote_ms = t_prom.elapsed().as_secs_f64() * 1000.0;
    let mut oxide_after_promote =
        DocumentEditor::from_bytes(promoted_bytes).map_err(|e| e.to_string())?;
    let oxide_promoted_fields = oxide_after_promote
        .get_form_fields()
        .map_err(|e| e.to_string())?
        .len();

    Ok(json!({
        "pdf_oxide_flat_fields_detected": oxide_flat_fields,
        "nupdf_detect_median_ms": round3(median(detect_times)),
        "nupdf_flat_fields_detected": detected_count,
        "nupdf_fill_ms": round3(fill_ms),
        "nupdf_fill_verified": fill_rep.verified,
        "nupdf_comb_overflow_rejected": comb_overflow_rejected,
        "nupdf_promote_ms": round3(promote_ms),
        "nupdf_promoted_widgets": prom_rep.promoted_widgets,
        "pdf_oxide_fields_after_nupdf_promote": oxide_promoted_fields,
        "pdf_oxide_sees_acroform_after_nupdf_promote": oxide_promoted_fields == prom_rep.promoted_widgets && oxide_promoted_fields > 0,
    }))
}

fn exp3_reflow_native(work: &Path) -> Result<Value, String> {
    let bytes: Arc<[u8]> = std::fs::read(work.join("contract_reflow.pdf"))
        .map_err(|e| format!("read contract_reflow.pdf: {e}"))?
        .into();

    let expanded = "2. Indemnification. Provider shall defend, indemnify, and hold harmless Customer and its affiliates, officers, and directors from and against any and all third-party claims, damages, losses, regulatory fines, liabilities, costs, and reasonable attorneys fees arising out of intellectual property infringement, security breaches, or gross negligence.";

    // 1. Native pdf_oxide DocumentEditor `set_text` on Clause 2:
    let mut oxide_ed = DocumentEditor::from_bytes(bytes.to_vec()).map_err(|e| e.to_string())?;
    let mut oxide_matched = 0usize;
    let t_ox = Instant::now();
    oxide_ed
        .edit_page(0, |page| {
            let hits = page.find_text_containing("2. Indemnification");
            oxide_matched = hits.len();
            if let Some(first) = hits.first() {
                page.set_text(first.id(), expanded)?;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    let oxide_out = oxide_ed.save_to_bytes().map_err(|e| e.to_string())?;
    let oxide_ms = t_ox.elapsed().as_secs_f64() * 1000.0;
    std::fs::write(work.join("oxide_reflow_attempt.pdf"), &oxide_out).map_err(|e| e.to_string())?;

    // Check right-margin overflow of pdf_oxide's single-line edit using nupdf stext:
    let mut oxide_max_x1 = 0.0f32;
    if let Ok(reopened) = Document::open(oxide_out.as_slice())
        && let Some(Ok(sp)) =
            nupdf::parallel::extract_pages(&reopened, &[0], nupdf::stext::Options::default(), 1)
                .into_iter()
                .next()
    {
        for b in &sp.blocks {
            if let nupdf::stext::Block::Text(tb) = b {
                for l in &tb.lines {
                    oxide_max_x1 = oxide_max_x1.max(l.bbox.x1);
                }
            }
        }
    }

    // 2. Native nupdf::patch::patch_and_verify_with_reflow(..., true, ...)
    let ok_spec = [PatchSpec {
        target_id: "p1.b2".into(),
        new_text: expanded.into(),
    }];
    let mut nupdf_times = Vec::new();
    let mut nupdf_verified = false;
    let mut nupdf_max_x1 = 0.0f32;
    for iter in 0..15 {
        let t0 = Instant::now();
        let doc = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
        let (saved, res) = patch::patch_and_verify_with_reflow(
            &doc,
            &ok_spec,
            &PatchOptions::default(),
            true,
            &WriteOptions::default(),
            None,
        )
        .map_err(|e| e.message)?;
        if iter == 0 {
            nupdf_verified = res.verification.as_ref().is_some_and(|v| v.verified);
            std::fs::write(work.join("nupdf_reflow_ok.pdf"), &saved)
                .map_err(|e| e.to_string())?;
            if let Ok(reopened) = Document::open(saved.as_slice())
                && let Some(Ok(sp)) = nupdf::parallel::extract_pages(
                    &reopened,
                    &[0],
                    nupdf::stext::Options::default(),
                    1,
                )
                .into_iter()
                .next()
            {
                for b in &sp.blocks {
                    if let nupdf::stext::Block::Text(tb) = b {
                        for l in &tb.lines {
                            nupdf_max_x1 = nupdf_max_x1.max(l.bbox.x1);
                        }
                    }
                }
            }
        }
        black_box(saved);
        nupdf_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    // 45-line overflow trap:
    let huge = "2. Indemnification. ".to_string()
        + &"Expanded indemnity obligation sentence pushing downstream clauses past page bottom. "
            .repeat(40);
    let trap_doc = Document::open(bytes).map_err(|e| format!("{e:?}"))?;
    let trap_spec = [PatchSpec {
        target_id: "p1.b2".into(),
        new_text: huge,
    }];
    let trap_caught = match patch::patch_and_verify_with_reflow(
        &trap_doc,
        &trap_spec,
        &PatchOptions::default(),
        true,
        &WriteOptions::default(),
        None,
    ) {
        Err(e) => e.code == "verify.page_overflow",
        Ok((_, r)) => r
            .verification
            .as_ref()
            .is_some_and(|v| !v.verified && v.findings.iter().any(|f| f.code == "verify.page_overflow")),
    };

    Ok(json!({
        "pdf_oxide_edit_ms": round3(oxide_ms),
        "pdf_oxide_matched_hits": oxide_matched,
        "pdf_oxide_max_line_x1_pt": round3(f64::from(oxide_max_x1)),
        "pdf_oxide_overflowed_612pt_page_right_edge": oxide_max_x1 > 612.0,
        "nupdf_reflow_median_ms": round3(median(nupdf_times)),
        "nupdf_reflow_verified": nupdf_verified,
        "nupdf_max_line_x1_pt": round3(f64::from(nupdf_max_x1)),
        "nupdf_caught_45_line_page_overflow": trap_caught,
    }))
}

fn exp4_history_native(work: &Path) -> Result<Value, String> {
    let bytes: Arc<[u8]> = std::fs::read(work.join("signed_history_3rev.pdf"))
        .map_err(|e| format!("read signed_history_3rev.pdf: {e}"))?
        .into();

    // 1. Native pdf_oxide on 3-revision tampered signed PDF:
    let ox_doc = PdfDocument::from_bytes(bytes.to_vec()).map_err(|e| e.to_string())?;
    let ox_text = ox_doc.extract_text(0).map_err(|e| e.to_string())?;

    // 2. Native nupdf::history::inspect_history:
    let mut nupdf_times = Vec::new();
    let mut last_rep = None;
    for iter in 0..10 {
        let t0 = Instant::now();
        let doc = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
        let rep = history::inspect_history(&doc, &HistoryOptions { run_attest: true })?;
        if iter == 0 {
            last_rep = Some(rep.clone());
        }
        black_box(rep);
        nupdf_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    let rep = last_rep.ok_or_else(|| "missing history report".to_string())?;

    Ok(json!({
        "pdf_oxide_revision_api_exists": false,
        "pdf_oxide_signature_verify_api_exists": false,
        "pdf_oxide_final_text_contains_tampered_90000": ox_text.contains("90,000"),
        "nupdf_history_median_ms": round3(median(nupdf_times)),
        "nupdf_revision_count": rep.revision_count,
        "nupdf_tampered": rep.tampered,
        "nupdf_classifications": rep.revisions.iter().map(|r| &r.classification).collect::<Vec<_>>(),
        "nupdf_rev3_hazards": rep.revisions.last().and_then(|r| r.delta_from_previous.as_ref()).map(|d| &d.forensic_hazards),
    }))
}

fn exp5_cloak_native(work: &Path) -> Result<Value, String> {
    let bytes: Arc<[u8]> = std::fs::read(work.join("nda_pii.pdf"))
        .map_err(|e| format!("read nda_pii.pdf: {e}"))?
        .into();

    let targets = [
        CloakTarget::Preset("pii".into()),
        CloakTarget::Literal {
            category: "PERSON".into(),
            secret: "Evelyn Vance".into(),
        },
        CloakTarget::Literal {
            category: "PERSON".into(),
            secret: "Marcus Sterling".into(),
        },
    ];

    let mut cloak_times = Vec::new();
    let mut cloaked_bytes = Vec::new();
    let mut vault_opt = None;
    let mut rep_opt = None;
    for iter in 0..10 {
        let t0 = Instant::now();
        let doc = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
        let (saved, vault, rep) =
            cloak::cloak_document(&doc, &targets, &WriteOptions::default())
                .map_err(|e| e.to_string())?;
        if iter == 0 {
            cloaked_bytes = saved.clone();
            vault_opt = Some(vault);
            rep_opt = Some(rep);
        }
        black_box(saved);
        cloak_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    let vault = vault_opt.ok_or_else(|| "missing vault".to_string())?;
    let rep = rep_opt.ok_or_else(|| "missing cloak report".to_string())?;

    // Audit cloaked PDF with native pdf_oxide to prove zero secret leaks:
    let ox_cloaked =
        PdfDocument::from_bytes(cloaked_bytes.clone()).map_err(|e| e.to_string())?;
    let ox_cloaked_text = ox_cloaked.extract_text(0).map_err(|e| e.to_string())?;
    let secrets = [
        "Evelyn Vance",
        "Marcus Sterling",
        "412-88-9014",
        "evelyn.vance@heliosbio.example",
    ];
    let oxide_leaked_any = secrets.iter().any(|s| ox_cloaked_text.contains(s));
    let oxide_saw_person_01 = ox_cloaked_text.contains("[PERSON_01]");
    let oxide_saw_synthetic_ssn = ox_cloaked_text.contains("000-00-0001");

    // Uncloak back and verify via pdf_oxide that real entities returned!
    let t_unc = Instant::now();
    let cloaked_doc = Document::open(cloaked_bytes.as_slice()).map_err(|e| format!("{e:?}"))?;
    let (restored_bytes, uncloak_rep) =
        cloak::uncloak_pdf(&cloaked_doc, &vault, &WriteOptions::default())
            .map_err(|e| e.to_string())?;
    let uncloak_ms = t_unc.elapsed().as_secs_f64() * 1000.0;

    let ox_restored = PdfDocument::from_bytes(restored_bytes).map_err(|e| e.to_string())?;
    let ox_restored_text = ox_restored.extract_text(0).map_err(|e| e.to_string())?;
    let oxide_all_restored = secrets.iter().all(|s| ox_restored_text.contains(s));

    Ok(json!({
        "nupdf_cloak_median_ms": round3(median(cloak_times)),
        "nupdf_uncloak_ms": round3(uncloak_ms),
        "nupdf_verified_zero_leaks": rep.verified_zero_leaks,
        "nupdf_entities_cloaked": rep.entities_cloaked,
        "nupdf_replacements": rep.replacements,
        "nupdf_restored_replacements": uncloak_rep.replacements,
        "pdf_oxide_leaked_any_secret_in_cloaked_pdf": oxide_leaked_any,
        "pdf_oxide_saw_coreference_token_person_01": oxide_saw_person_01,
        "pdf_oxide_saw_format_preserving_ssn": oxide_saw_synthetic_ssn,
        "pdf_oxide_verified_all_secrets_restored_after_uncloak": oxide_all_restored,
    }))
}

fn exp6_pipeline_native(work: &Path) -> Result<Value, String> {
    let bytes: Arc<[u8]> = std::fs::read(work.join("contract_reflow.pdf"))
        .map_err(|e| format!("read contract_reflow.pdf: {e}"))?
        .into();

    let spec_json = r#"{
        "compact": true,
        "steps": [
            {"op": "info", "id": "i1"},
            {"op": "attest", "id": "att1", "abort_on_hazards": ["hazard.invisible_or_occluded"]},
            {"op": "query", "id": "q1", "right_of": "Contract Value:", "nearest_only": true},
            {"op": "patch", "id": "p1", "replacements": [{"id": "${q1.first_id}", "text": "$42,500.00"}]},
            {"op": "audit", "id": "aud1", "strict": true, "claims": [{"claim_id": "c1", "page": 1, "quote": "$42,500.00"}]}
        ]
    }"#;
    let spec = pipeline::parse_pipeline_spec(spec_json).map_err(|e| e.to_string())?;

    let mut pipe_times = Vec::new();
    let mut steps_done = 0usize;
    let mut mutated = false;
    for iter in 0..15 {
        let t0 = Instant::now();
        let doc = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
        let out = pipeline::run_pipeline(&doc, &spec, &WriteOptions::default())
            .map_err(|e| e.to_string())?;
        if iter == 0 {
            steps_done = out.steps.len();
            mutated = out.mutated;
        }
        black_box(out);
        pipe_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    Ok(json!({
        "nupdf_5_step_pipeline_in_process_median_ms": round3(median(pipe_times)),
        "steps_completed": steps_done,
        "mutated": mutated,
    }))
}

fn main() {
    let Some(dir_arg) = std::env::args().nth(1) else {
        eprintln!("usage: primitives_v2_vs_oxide <work_dir>");
        std::process::exit(2);
    };
    let work = Path::new(&dir_arg);
    let run = || -> Result<Value, String> {
        Ok(json!({
            "engine": "pdf_oxide 0.3.78 (native Rust crate, thin LTO) vs nupdf v2 (native Rust crate, thin LTO)",
            "exp1_chart": exp1_chart_native(work)?,
            "exp2_flat_forms": exp2_flat_forms_native(work)?,
            "exp3_reflow": exp3_reflow_native(work)?,
            "exp4_history": exp4_history_native(work)?,
            "exp5_cloak": exp5_cloak_native(work)?,
            "exp6_pipeline": exp6_pipeline_native(work)?,
        }))
    };
    match run() {
        Ok(val) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&val).unwrap_or_else(|_| "{}".into())
            );
        }
        Err(e) => {
            eprintln!("primitives_v2_vs_oxide error: {e}");
            std::process::exit(1);
        }
    }
}
