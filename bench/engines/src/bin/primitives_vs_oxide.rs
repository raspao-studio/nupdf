// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Head-to-head native Rust-to-Rust evaluation of `nupdf`'s agent primitives
//! against `pdf_oxide` 0.3.78 in the same process, compiled with thin LTO and
//! `codegen-units = 1` (matching `bench/engines/Cargo.toml`).
//!
//! Usage:
//!     primitives-vs-oxide <work_dir>

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use nupdf::Document;
use nupdf::attest::{self, AttestOptions};
use nupdf::audit::{self, ClaimInput};
use nupdf::patch::{self, PatchOptions, PatchSpec};
use nupdf::query::{self, QuerySpec};
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

fn exp1_attest_native(work: &Path) -> Result<Value, String> {
    let bytes: Arc<[u8]> = std::fs::read(work.join("attest_traps.pdf"))
        .map_err(|e| format!("read attest_traps.pdf: {e}"))?
        .into();

    // 1. Native pdf_oxide: text, spans, and structured extraction
    let oxide_check = {
        let doc = PdfDocument::from_bytes(bytes.to_vec()).map_err(|e| e.to_string())?;
        let plain = doc.extract_text(0).map_err(|e| e.to_string())?;
        let spans = doc.extract_spans(0).map_err(|e| e.to_string())?;
        let structured = doc.extract_structured(0).map_err(|e| e.to_string())?;

        let plain_leaks_occluded = plain.contains("SYSTEM OVERRIDE");
        let spans_leak_occluded = spans.iter().any(|s| s.text.contains("SYSTEM OVERRIDE"));
        let structured_leaks_occluded = structured
            .regions
            .iter()
            .any(|r| r.text.contains("SYSTEM OVERRIDE"));
        let extracted_spoof_amount =
            plain.contains("100,000.00") || spans.iter().any(|s| s.text.contains("100,000.00"));
        let extracted_visual_amount =
            plain.contains("$100.00") || spans.iter().any(|s| s.text.contains("$100.00"));

        json!({
            "plain_leaks_occluded_injection": plain_leaks_occluded,
            "spans_leak_occluded_injection": spans_leak_occluded,
            "structured_leaks_occluded_injection": structured_leaks_occluded,
            "has_span_strikethrough_or_occlusion_api": false,
            "has_actualtext_vs_glyph_diff_api": false,
            "saw_actualtext_100k": extracted_spoof_amount,
            "saw_glyph_100": extracted_visual_amount,
            "span_count": spans.len(),
        })
    };

    let mut oxide_times = Vec::new();
    for _ in 0..15 {
        let copy = bytes.to_vec();
        let t0 = Instant::now();
        let doc = PdfDocument::from_bytes(copy).map_err(|e| e.to_string())?;
        let spans = doc.extract_spans(0).map_err(|e| e.to_string())?;
        black_box(spans);
        oxide_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    // 2. Native nupdf::attest (dual full + NoTextDevice pixel render + vector/ActualText checks)
    let opts = AttestOptions {
        dpi: 72.0,
        contrast_threshold: 12,
        max_pixels: 100_000_000,
    };
    let mut nupdf_times = Vec::new();
    let mut detected_all = false;
    for i in 0..15 {
        let t0 = Instant::now();
        let doc = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
        let res = attest::attest_pages(&doc, &[0], &opts, false, 1);
        let page = res
            .into_iter()
            .next()
            .ok_or_else(|| "no page".to_string())??;
        if i == 0 {
            let struck = page
                .spans
                .iter()
                .any(|s| s.text.contains("Unlimited Indemnification") && s.struck_through);
            let occluded = page.spans.iter().any(|s| {
                s.text.contains("SYSTEM OVERRIDE") && s.visibility != attest::Visibility::Visible
            });
            let spoof = page.spans.iter().any(|s| {
                s.text.contains("100,000")
                    && s.glyph_text
                        .as_deref()
                        .is_some_and(|g| g.contains("100.00"))
            });
            detected_all = struck && occluded && spoof && !page.blind_spots.is_empty();
        }
        black_box(page);
        nupdf_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    Ok(json!({
        "pdf_oxide_native": oxide_check,
        "pdf_oxide_extract_spans_median_ms": round3(median(oxide_times)),
        "nupdf_attest_in_process_median_ms": round3(median(nupdf_times)),
        "nupdf_detected_all_4_traps": detected_all,
    }))
}

fn exp2_patch_native(work: &Path) -> Result<Value, String> {
    let bytes: Arc<[u8]> = std::fs::read(work.join("invoice_patch.pdf"))
        .map_err(|e| format!("read invoice_patch.pdf: {e}"))?
        .into();

    // Locate target content ID in nupdf once
    let base = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
    let stext_page =
        nupdf::parallel::extract_pages(&base, &[0], nupdf::stext::Options::default(), 1)
            .into_iter()
            .next()
            .ok_or_else(|| "no page".to_string())??;
    let mut target_id = String::new();
    for (bi, b) in stext_page.blocks.iter().enumerate() {
        if let nupdf::stext::Block::Text(tb) = b {
            for (li, l) in tb.lines.iter().enumerate() {
                for (si, sp) in nupdf::extract::spans(l).iter().enumerate() {
                    let txt = nupdf::extract::chars_text(&l.chars[sp.start..sp.end]);
                    if txt.contains("14,500.00") {
                        target_id = format!("p1.b{}.l{}.s{}", bi + 1, li + 1, si + 1);
                    }
                }
            }
        }
    }
    if target_id.is_empty() {
        return Err("target span not found in invoice_patch.pdf".into());
    }

    // 1. Native nupdf::patch::patch_and_verify (valid + collision trap)
    let ok_spec = [PatchSpec {
        target_id: target_id.clone(),
        new_text: "Amount: $15,400.00".into(),
    }];
    let patch_opts = PatchOptions::default();
    let write_opts = WriteOptions::default();

    let mut nupdf_patch_times = Vec::new();
    for _ in 0..20 {
        let t0 = Instant::now();
        let d = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
        let (saved, res) = patch::patch_and_verify(&d, &ok_spec, &patch_opts, &write_opts, None)
            .map_err(|e| e.message)?;
        assert!(res.verification.as_ref().is_some_and(|v| v.verified));
        black_box(saved);
        nupdf_patch_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    let bad_spec = [PatchSpec {
        target_id: target_id.clone(),
        new_text: "Amount: $14,500,000,000,000.00 (USD)".into(),
    }];
    let d_bad = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
    let (_, bad_res) = patch::patch_and_verify(&d_bad, &bad_spec, &patch_opts, &write_opts, None)
        .map_err(|e| e.message)?;
    let nupdf_caught_collision = bad_res
        .verification
        .as_ref()
        .is_some_and(|v| !v.verified && v.findings.iter().any(|f| f.code == "verify.collision"));

    // 2. Native pdf_oxide::editor::DocumentEditor DOM `set_text` (valid + collision trap)
    let run_oxide_edit = |replacement: &str| -> Result<(Vec<u8>, usize), String> {
        let mut editor = DocumentEditor::from_bytes(bytes.to_vec()).map_err(|e| e.to_string())?;
        let mut matched = 0usize;
        editor
            .edit_page(0, |page| {
                let hits = page.find_text_containing("14,500.00");
                matched = hits.len();
                for hit in hits {
                    page.set_text(hit.id(), replacement)?;
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        let out = editor.save_to_bytes().map_err(|e| e.to_string())?;
        Ok((out, matched))
    };

    let (oxide_ok_bytes, oxide_matched) = run_oxide_edit("Amount: $15,400.00")?;
    std::fs::write(work.join("oxide_patched_ok.pdf"), &oxide_ok_bytes)
        .map_err(|e| e.to_string())?;

    let mut oxide_patch_times = Vec::new();
    for _ in 0..20 {
        let copy = bytes.to_vec();
        let t0 = Instant::now();
        let mut editor = DocumentEditor::from_bytes(copy).map_err(|e| e.to_string())?;
        editor
            .edit_page(0, |page| {
                for hit in page.find_text_containing("14,500.00") {
                    page.set_text(hit.id(), "Amount: $15,400.00")?;
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        let out = editor.save_to_bytes().map_err(|e| e.to_string())?;
        black_box(out);
        oxide_patch_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    // Collision trap on native pdf_oxide DocumentEditor:
    let oxide_collision_res = run_oxide_edit("Amount: $14,500,000,000,000.00 (USD)");
    let oxide_silently_allowed_collision = match &oxide_collision_res {
        Ok((corrupt_bytes, _)) => {
            std::fs::write(work.join("oxide_patched_collision.pdf"), corrupt_bytes)
                .map_err(|e| e.to_string())?;
            true
        }
        Err(_) => false,
    };

    // Inspect overlapping span bboxes in pdf_oxide's collision output using nupdf!
    let mut oxide_collision_overlaps_neighbor = false;
    if let Ok((ref corrupt_bytes, _)) = oxide_collision_res
        && let Ok(reopened) = Document::open(corrupt_bytes.as_slice())
        && let Some(Ok(sp)) =
            nupdf::parallel::extract_pages(&reopened, &[0], nupdf::stext::Options::default(), 1)
                .into_iter()
                .next()
    {
        let mut spans_boxes = Vec::new();
        for b in &sp.blocks {
            if let nupdf::stext::Block::Text(tb) = b {
                for l in &tb.lines {
                    for s in nupdf::extract::spans(l) {
                        let txt = nupdf::extract::chars_text(&l.chars[s.start..s.end]);
                        spans_boxes.push((txt, [s.bbox.x0, s.bbox.y0, s.bbox.x1, s.bbox.y1]));
                    }
                }
            }
        }
        for i in 0..spans_boxes.len() {
            for j in (i + 1)..spans_boxes.len() {
                let (_, a) = &spans_boxes[i];
                let (_, b) = &spans_boxes[j];
                let x_overlap = a[2].min(b[2]) - a[0].max(b[0]);
                let y_overlap = a[3].min(b[3]) - a[1].max(b[1]);
                if x_overlap > 2.0 && y_overlap > 2.0 {
                    oxide_collision_overlaps_neighbor = true;
                }
            }
        }
    }

    let np_med = median(nupdf_patch_times);
    let ox_med = median(oxide_patch_times);
    Ok(json!({
        "nupdf_patch_and_verify_median_ms": round3(np_med),
        "pdf_oxide_document_editor_median_ms": round3(ox_med),
        "speedup_vs_pdf_oxide": round3(ox_med / np_med.max(0.0001)),
        "nupdf_caught_collision": nupdf_caught_collision,
        "pdf_oxide_matched_elements": oxide_matched,
        "pdf_oxide_silently_allowed_collision": oxide_silently_allowed_collision,
        "pdf_oxide_collision_overlaps_neighbor_spans": oxide_collision_overlaps_neighbor,
    }))
}

fn exp3_branching_native() -> Result<Value, String> {
    // Build a 50-page PDF once via nupdf
    let seed = Document::create().map_err(|e| format!("{e:?}"))?;
    for i in 0..50i32 {
        let content = format!("BT /F1 12 Tf 40 700 Td (Page {i} clause text) Tj ET\n");
        let stream_num = seed.add_stream(nupdf::object::Dict::new(), content.into_bytes());
        let mut font = nupdf::object::Dict::new();
        font.put(b"Type", nupdf::object::Object::name(b"Font"));
        font.put(b"Subtype", nupdf::object::Object::name(b"Type1"));
        font.put(b"BaseFont", nupdf::object::Object::name(b"Helvetica"));
        let font_num = seed.add_object(nupdf::object::Object::Dict(Arc::new(font)));
        let mut fonts = nupdf::object::Dict::new();
        fonts.put(
            b"F1",
            nupdf::object::Object::Ref(nupdf::object::ObjRef::new(font_num, 0)),
        );
        let mut res = nupdf::object::Dict::new();
        res.put(b"Font", nupdf::object::Object::Dict(Arc::new(fonts)));
        let mut page = nupdf::object::Dict::new();
        page.put(b"Type", nupdf::object::Object::name(b"Page"));
        page.put(
            b"MediaBox",
            nupdf::object::Object::Array(Arc::new(vec![
                nupdf::object::Object::Int(0),
                nupdf::object::Object::Int(0),
                nupdf::object::Object::Int(612),
                nupdf::object::Object::Int(792),
            ])),
        );
        page.put(
            b"Contents",
            nupdf::object::Object::Ref(nupdf::object::ObjRef::new(stream_num, 0)),
        );
        page.put(b"Resources", nupdf::object::Object::Dict(Arc::new(res)));
        let page_num = seed.add_object(nupdf::object::Object::Dict(Arc::new(page)));
        nupdf::insert::insert_page(&seed, i, page_num)?;
    }
    let pdf_bytes: Arc<[u8]> = nupdf::write::save(&seed, &WriteOptions::default())
        .map_err(|e| e.message)?
        .into();

    let iterations = 1_000usize;

    // 1. nupdf Document::open x 1,000
    let t_nupdf_open = Instant::now();
    for i in 0..iterations {
        let d = Document::open(pdf_bytes.clone()).map_err(|e| format!("{e:?}"))?;
        let p = d.page(25).map_err(|e| e.0)?;
        nupdf::edit::set_metadata(&d, "Title", &format!("Branch {i}"));
        black_box((d, p));
    }
    let nupdf_open_ms = t_nupdf_open.elapsed().as_secs_f64() * 1000.0;

    // 2. nupdf Document::fork_clean x 1,000 (warm base state shared via Arc)
    let base = Document::open(pdf_bytes.clone()).map_err(|e| format!("{e:?}"))?;
    let _ = base.page(25).map_err(|e| e.0)?;
    let t_nupdf_fork = Instant::now();
    for i in 0..iterations {
        let d = base.fork_clean();
        let p = d.page(25).map_err(|e| e.0)?;
        nupdf::edit::set_metadata(&d, "Title", &format!("Branch {i}"));
        black_box((d, p));
    }
    let nupdf_fork_ms = t_nupdf_fork.elapsed().as_secs_f64() * 1000.0;

    // 3. Native pdf_oxide DocumentEditor::from_bytes x 1,000 (byte copy excluded from clock!)
    let mut oxide_ms = 0.0f64;
    for i in 0..iterations {
        let copy = pdf_bytes.to_vec();
        let t0 = Instant::now();
        let mut ed = DocumentEditor::from_bytes(copy).map_err(|e| e.to_string())?;
        let mb = ed.get_page_media_box(25).map_err(|e| e.to_string())?;
        ed.set_title(format!("Branch {i}"));
        oxide_ms += t0.elapsed().as_secs_f64() * 1000.0;
        black_box((ed, mb));
    }

    Ok(json!({
        "iterations": iterations,
        "pdf_size_bytes": pdf_bytes.len(),
        "pdf_oxide_editor_1000_branches_ms": round3(oxide_ms),
        "nupdf_open_1000_branches_ms": round3(nupdf_open_ms),
        "nupdf_fork_clean_1000_branches_ms": round3(nupdf_fork_ms),
        "fork_clean_speedup_vs_pdf_oxide": round3(oxide_ms / nupdf_fork_ms.max(0.0001)),
        "nupdf_open_speedup_vs_pdf_oxide": round3(oxide_ms / nupdf_open_ms.max(0.0001)),
    }))
}

fn parse_dollar_amount(s: &str) -> Option<f64> {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        cleaned.parse::<f64>().ok()
    }
}

fn oxide_spatial_right_of_total_due(bytes: Vec<u8>) -> Result<f64, String> {
    let doc = PdfDocument::from_bytes(bytes).map_err(|e| e.to_string())?;
    let spans = doc.extract_spans(0).map_err(|e| e.to_string())?;
    let anchor = spans
        .iter()
        .find(|s| s.text.contains("Total Due"))
        .ok_or_else(|| "Total Due anchor not found".to_string())?;
    let ay = anchor.bbox.y + anchor.bbox.height * 0.5;
    let ax_right = anchor.bbox.x + anchor.bbox.width;
    let mut best: Option<(f32, f64)> = None;
    for s in &spans {
        if s.bbox.x >= ax_right - 2.0 {
            let sy = s.bbox.y + s.bbox.height * 0.5;
            if (sy - ay).abs() <= anchor.bbox.height.max(12.0)
                && let Some(val) = parse_dollar_amount(&s.text)
            {
                let dist = s.bbox.x - ax_right;
                if best.is_none_or(|(d, _)| dist < d) {
                    best = Some((dist, val));
                }
            }
        }
    }
    best.map(|(_, v)| v)
        .ok_or_else(|| "no numeric span to the right of Total Due".to_string())
}

fn exp4_query_100_receipts_native(work: &Path) -> Result<Value, String> {
    let dir = work.join("receipts_100");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("read receipts_100: {e}"))?
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("pdf"))
        .collect();
    paths.sort();

    let files_arc: Vec<Arc<[u8]>> = paths
        .iter()
        .map(|p| std::fs::read(p).map(Arc::from))
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;

    let spec = QuerySpec {
        pages: None,
        right_of: Some("Total Due".into()),
        left_of: None,
        below: None,
        above: None,
        inside: None,
        near_widget: None,
        pattern: None,
        bold: None,
        italic: None,
        min_size: None,
        max_size: None,
        color: None,
        nearest_only: true,
        max_items: None,
    };

    // 1. nupdf single-thread across 100 receipts in memory
    let mut nupdf_1t_times = Vec::new();
    let mut nupdf_sum = 0.0f64;
    for run in 0..15 {
        let t0 = Instant::now();
        let mut s = 0.0f64;
        for b in &files_arc {
            let doc = Document::open(b.clone()).map_err(|e| format!("{e:?}"))?;
            let qr = query::run_query(&doc, &spec)?;
            s += qr.aggregation.sum.unwrap_or(0.0);
        }
        if run == 0 {
            nupdf_sum = (s * 100.0).round() / 100.0;
        }
        black_box(s);
        nupdf_1t_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    // 2. nupdf multi-thread (8 threads) across 100 receipts in memory
    let workers = std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4);
    let chunk_size = files_arc.len().div_ceil(workers);
    let mut nupdf_mt_times = Vec::new();
    for _ in 0..15 {
        let t0 = Instant::now();
        let total: f64 = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for chunk in files_arc.chunks(chunk_size) {
                let spec_ref = &spec;
                handles.push(scope.spawn(move || -> Result<f64, String> {
                    let mut local = 0.0f64;
                    for b in chunk {
                        let doc = Document::open(b.clone()).map_err(|e| format!("{e:?}"))?;
                        let qr = query::run_query(&doc, spec_ref)?;
                        local += qr.aggregation.sum.unwrap_or(0.0);
                    }
                    Ok(local)
                }));
            }
            let mut acc = 0.0f64;
            for h in handles {
                acc += h.join().map_err(|_| "panic".to_string())??;
            }
            Ok::<f64, String>(acc)
        })?;
        black_box(total);
        nupdf_mt_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    // 3. Native pdf_oxide single-thread across 100 receipts in memory (excluding Vec copy)
    let mut oxide_1t_times = Vec::new();
    let mut oxide_sum = 0.0f64;
    for run in 0..15 {
        let copies: Vec<Vec<u8>> = files_arc.iter().map(|b| b.to_vec()).collect();
        let t0 = Instant::now();
        let mut s = 0.0f64;
        for c in copies {
            s += oxide_spatial_right_of_total_due(c)?;
        }
        if run == 0 {
            oxide_sum = (s * 100.0).round() / 100.0;
        }
        black_box(s);
        oxide_1t_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    // 4. Native pdf_oxide multi-thread (same worker count) across 100 receipts in memory
    let mut oxide_mt_times = Vec::new();
    for _ in 0..15 {
        let copies: Vec<Vec<u8>> = files_arc.iter().map(|b| b.to_vec()).collect();
        let t0 = Instant::now();
        let total: f64 = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for chunk in copies.chunks(chunk_size) {
                let owned_chunk: Vec<Vec<u8>> = chunk.to_vec();
                handles.push(scope.spawn(move || -> Result<f64, String> {
                    let mut local = 0.0f64;
                    for c in owned_chunk {
                        local += oxide_spatial_right_of_total_due(c)?;
                    }
                    Ok(local)
                }));
            }
            let mut acc = 0.0f64;
            for h in handles {
                acc += h.join().map_err(|_| "panic".to_string())??;
            }
            Ok::<f64, String>(acc)
        })?;
        black_box(total);
        oxide_mt_times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    let n1 = median(nupdf_1t_times);
    let nmt = median(nupdf_mt_times);
    let o1 = median(oxide_1t_times);
    let omt = median(oxide_mt_times);

    Ok(json!({
        "receipt_count": files_arc.len(),
        "nupdf_sum": nupdf_sum,
        "pdf_oxide_custom_rust_loop_sum": oxide_sum,
        "nupdf_1_thread_median_ms": round3(n1),
        "pdf_oxide_1_thread_median_ms": round3(o1),
        "single_thread_speedup_vs_pdf_oxide": round3(o1 / n1.max(0.0001)),
        "workers": workers,
        "nupdf_parallel_median_ms": round3(nmt),
        "pdf_oxide_parallel_median_ms": round3(omt),
        "parallel_speedup_vs_pdf_oxide": round3(omt / nmt.max(0.0001)),
    }))
}

fn exp5_audit_native(work: &Path) -> Result<Value, String> {
    let bytes: Arc<[u8]> = std::fs::read(work.join("audit_contract.pdf"))
        .map_err(|e| format!("read audit_contract.pdf: {e}"))?
        .into();
    let claims_raw = std::fs::read_to_string(work.join("claims.json"))
        .map_err(|e| format!("read claims.json: {e}"))?;
    let claims: Vec<ClaimInput> = serde_json::from_str(&claims_raw).map_err(|e| e.to_string())?;

    let doc_id = nupdf::DocId::of(&bytes).to_string();

    let mut nupdf_no_png = Vec::new();
    let mut nupdf_with_png = Vec::new();
    for _ in 0..15 {
        let t0 = Instant::now();
        let doc = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
        let summary =
            audit::audit_claims_with_options(&doc, &doc_id, &claims, false, 108.0, false)?;
        black_box(summary);
        nupdf_no_png.push(t0.elapsed().as_secs_f64() * 1000.0);

        let t1 = Instant::now();
        let doc2 = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
        let summary2 =
            audit::audit_claims_with_options(&doc2, &doc_id, &claims, true, 108.0, false)?;
        black_box(summary2);
        nupdf_with_png.push(t1.elapsed().as_secs_f64() * 1000.0);
    }

    // Check what native pdf_oxide sees on the struck-through and footnoted clauses
    let oxide_doc = PdfDocument::from_bytes(bytes.to_vec()).map_err(|e| e.to_string())?;
    let p1_spans = oxide_doc.extract_spans(0).map_err(|e| e.to_string())?;
    let sees_struck_clause_as_normal_text = p1_spans
        .iter()
        .any(|s| s.text.contains("Early Termination Fee: $0"));

    Ok(json!({
        "nupdf_audit_5_claims_no_crops_median_ms": round3(median(nupdf_no_png)),
        "nupdf_audit_5_claims_with_4_png_crops_median_ms": round3(median(nupdf_with_png)),
        "pdf_oxide_has_claim_or_hazard_auditor": false,
        "pdf_oxide_extracts_struck_clause_as_active_text": sees_struck_clause_as_normal_text,
    }))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: primitives-vs-oxide <work_dir>");
        std::process::exit(2);
    }
    let work = PathBuf::from(&args[1]);
    let worker = std::thread::Builder::new()
        .stack_size(1 << 26)
        .spawn(move || -> Result<Value, String> {
            Ok(json!({
                "pdf_oxide_version": "0.3.78",
                "exp1_attest": exp1_attest_native(&work)?,
                "exp2_patch": exp2_patch_native(&work)?,
                "exp3_branching": exp3_branching_native()?,
                "exp4_query_100_receipts": exp4_query_100_receipts_native(&work)?,
                "exp5_audit": exp5_audit_native(&work)?,
            }))
        })
        .expect("spawn worker");

    match worker.join() {
        Ok(Ok(val)) => {
            println!("{}", serde_json::to_string_pretty(&val).unwrap());
        }
        Ok(Err(err)) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("error: worker panicked");
            std::process::exit(1);
        }
    }
}
