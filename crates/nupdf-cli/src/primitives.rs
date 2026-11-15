// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Agent-native PDF primitives: `attest`, `patch`, `apply`, `query`, and `audit`.

use std::path::{Path, PathBuf};

use nupdf::attest::{self, AttestOptions};
use nupdf::audit::{self, ClaimInput};
use nupdf::patch::{self, PatchOptions, PatchSpec};
use nupdf::plan::{self, EditPlan};
use nupdf::query::{self, Aggregation, QuerySpec};
use nupdf::{DocId, Document};
use serde_json::{Map, Value, json};

use super::commands::WriteArgs;
use super::{
    Clean, Exit, Failure, FileFacts, diagnostics, emit, output_dir, report, signatures_kept,
    standard_output, write_atomically, write_file,
};

type OpenFn<'a> = &'a (dyn Fn(&Path) -> Result<(Document, FileFacts), Failure> + Sync);

fn usage(code: &'static str, message: String) -> Failure {
    Failure {
        exit: Exit::Usage,
        code,
        message,
    }
}

fn input_err(code: &'static str, message: String) -> Failure {
    Failure {
        exit: Exit::Input,
        code,
        message,
    }
}

/// `attest`: perceptual-vs-stream attestation per span and blind-spot region.
pub fn attest_cmd(
    doc: &Document,
    facts: &FileFacts,
    pages: Option<&str>,
    dpi: f32,
    contrast_threshold: u8,
    crops_dir: Option<&Path>,
    threads: usize,
) -> Result<Value, Failure> {
    if !(dpi.is_finite() && dpi > 0.0 && dpi <= 1200.0) {
        return Err(usage(
            "usage.bad_option",
            format!("bad --dpi {dpi}: expected more than 0, at most 1200"),
        ));
    }
    let crop_dir = match crops_dir {
        Some(d) => Some(output_dir(d)?),
        None => None,
    };

    let page_count = doc.page_count().ok();
    let count = page_count.unwrap_or(0);
    let indices: Vec<i32> = match pages {
        Some(spec) => nupdf::rearrange::parse_page_ranges(spec, count, doc),
        None => (0..count).collect(),
    };

    let opts = AttestOptions {
        dpi,
        contrast_threshold,
        max_pixels: 100_000_000,
    };
    let results = attest::attest_pages(doc, &indices, &opts, crop_dir.is_some(), threads.max(1));

    let mut pages_json = Vec::with_capacity(results.len());
    for res in results {
        let page_att = res.map_err(|msg| {
            if msg.starts_with("limit error:") {
                Failure {
                    exit: Exit::Limit,
                    code: "limit.pixels",
                    message: msg,
                }
            } else {
                input_err("input.page_tree", msg)
            }
        })?;
        let pngs: Vec<Option<Vec<u8>>> =
            page_att.blind_spots.iter().map(|b| b.png.clone()).collect();
        let mut val = serde_json::to_value(&page_att).map_err(|e| Failure {
            exit: Exit::Internal,
            code: "internal.serialize",
            message: e.to_string(),
        })?;
        if let Some(spots) = val.get_mut("blind_spots").and_then(Value::as_array_mut) {
            for (idx, spot_val) in spots.iter_mut().enumerate() {
                let mut crop_path: Option<String> = None;
                if let (Some(dir), Some(Some(png))) = (crop_dir, pngs.get(idx)) {
                    let spot_id = spot_val.get("id").and_then(Value::as_str).unwrap_or("spot");
                    let path = dir.join(format!("spot-{spot_id}.png"));
                    write_file(&path, png).map_err(|e| {
                        input_err(
                            "output.unwritable",
                            format!("cannot write {}: {e}", path.display()),
                        )
                    })?;
                    crop_path = Some(path.display().to_string());
                }
                if let Some(obj) = spot_val.as_object_mut() {
                    obj.insert(
                        "crop_file".into(),
                        crop_path.map_or(Value::Null, Value::String),
                    );
                }
            }
        }
        pages_json.push(val);
    }

    let body = json!({
        "page_count": page_count,
        "pages": pages_json,
    });
    Ok(report("nupdf.attest/v1", facts, doc, body))
}

/// `patch`: verified surgical text replacement by content ID.
#[allow(clippy::too_many_arguments)]
pub fn patch_cmd(
    doc: &Document,
    facts: &FileFacts,
    replace: &[String],
    strict_font: bool,
    no_collision_check: bool,
    no_overflow_check: bool,
    write: &WriteArgs,
    password: Option<&str>,
) -> Result<Value, Failure> {
    if replace.is_empty() {
        return Err(usage(
            "usage.bad_option",
            "nothing to patch: pass at least one --replace ID=TEXT".into(),
        ));
    }
    let mut specs = Vec::with_capacity(replace.len());
    for entry in replace {
        let Some((id, new_text)) = entry.split_once('=') else {
            return Err(usage(
                "usage.bad_value",
                format!("--replace takes ID=TEXT, got {entry:?}"),
            ));
        };
        let id = id.trim();
        if id.is_empty() {
            return Err(usage(
                "usage.bad_value",
                format!("--replace needs a non-empty content ID, got {entry:?}"),
            ));
        }
        specs.push(PatchSpec {
            target_id: id.to_string(),
            new_text: new_text.to_string(),
        });
    }

    let plan: Clean<'_> = write.plan(nupdf::write::WriteOptions::default(), password)?;
    if doc.needs_password() && password.is_none() {
        return Err(input_failure_password());
    }

    let patch_opts = PatchOptions {
        strict_font,
        check_collisions: !no_collision_check,
        check_overflow: !no_overflow_check,
    };

    let work = doc.fork_clean();
    nupdf::write::check_document(&work);

    let (bytes, res) = if plan.verify {
        patch::patch_and_verify(&work, &specs, &patch_opts, &plan.options, password)
            .map_err(map_patch_error)?
    } else {
        let applied = patch::apply_patches(&work, &specs, &patch_opts).map_err(map_patch_error)?;
        let mut write_opts = plan.options.clone();
        if !write_opts.incremental {
            write_opts.garbage = write_opts.garbage.max(1);
        }
        let saved = nupdf::write::save(&work, &write_opts).map_err(|e| {
            input_err(
                "input.unloadable_object",
                nupdf::diag::report::strip(e.message.lines().last().unwrap_or_default())
                    .to_string(),
            )
        })?;
        (
            saved,
            patch::PatchResult {
                patches: applied,
                verification: None,
            },
        )
    };

    if plan.verify
        && let Some(ref v) = res.verification
        && !v.verified
    {
        let details: Vec<String> = v
            .findings
            .iter()
            .map(|f| format!("{} ({}): {}", f.code, f.id, f.message))
            .collect();
        return Err(Failure {
            exit: Exit::Verify,
            code: "verify.failed",
            message: format!(
                "patch verification failed ({}); nothing was written",
                details.join("; ")
            ),
        });
    }

    if plan.apply
        && let Some(path) = plan.output
    {
        write_atomically(path, &bytes).map_err(|e| {
            input_err(
                "output.unwritable",
                format!("cannot write {}: {e}", path.display()),
            )
        })?;
    }

    let output = output_summary(doc, &bytes, &plan);
    let body = json!({
        "applied": plan.apply,
        "patches": res.patches,
        "output": output,
        "verification": res.verification,
    });
    Ok(report("nupdf.patch/v1", facts, doc, body))
}

fn map_patch_error(e: patch::PatchError) -> Failure {
    let exit = if e.code.starts_with("usage.") {
        Exit::Usage
    } else if e.code.starts_with("verify.") {
        Exit::Verify
    } else {
        Exit::Input
    };
    Failure {
        exit,
        code: e.code,
        message: e.message,
    }
}

/// `apply`: multi-agent transactional edit plans anchored to base `doc_id`.
pub fn apply_cmd(
    doc: &Document,
    facts: &FileFacts,
    plan_files: &[PathBuf],
    inline_plans: &[String],
    strict_font: bool,
    write: &WriteArgs,
    password: Option<&str>,
) -> Result<Value, Failure> {
    if plan_files.is_empty() && inline_plans.is_empty() {
        return Err(usage(
            "usage.bad_option",
            "nothing to apply: pass --plan FILE or --inline JSON".into(),
        ));
    }
    let clean_plan: Clean<'_> = write.plan(nupdf::write::WriteOptions::default(), password)?;
    if doc.needs_password() && password.is_none() {
        return Err(input_failure_password());
    }

    let mut plans: Vec<EditPlan> = Vec::with_capacity(plan_files.len() + inline_plans.len());
    for path in plan_files {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            input_err(
                "input.unreadable",
                format!("cannot read {}: {e}", path.display()),
            )
        })?;
        let parsed = plan::parse_plan(&raw).map_err(map_plan_error)?;
        plans.push(parsed);
    }
    for raw in inline_plans {
        let parsed = plan::parse_plan(raw).map_err(map_plan_error)?;
        plans.push(parsed);
    }

    let patch_opts = PatchOptions {
        strict_font,
        check_collisions: true,
        check_overflow: true,
    };

    let (bytes, exec) = plan::execute_plans(
        doc,
        &facts.doc_id,
        &plans,
        &clean_plan.options,
        &patch_opts,
        password,
        clean_plan.verify,
    )
    .map_err(map_plan_error)?;

    if !exec.conflicts.is_empty() {
        let summary: Vec<String> = exec
            .conflicts
            .iter()
            .map(|c| format!("{}: {}", c.code, c.message))
            .collect();
        return Err(Failure {
            exit: Exit::Verify,
            code: "plan.conflict",
            message: format!(
                "{} cross-plan conflict(s) detected ({}); nothing was written",
                exec.conflicts.len(),
                summary.join("; ")
            ),
        });
    }

    if clean_plan.verify && exec.verified != Some(true) {
        return Err(Failure {
            exit: Exit::Verify,
            code: "verify.failed",
            message: "transactional plan verification failed; nothing was written".into(),
        });
    }

    if clean_plan.apply
        && let Some(path) = clean_plan.output
    {
        write_atomically(path, &bytes).map_err(|e| {
            input_err(
                "output.unwritable",
                format!("cannot write {}: {e}", path.display()),
            )
        })?;
    }

    let output = output_summary(doc, &bytes, &clean_plan);
    let mut body = Map::new();
    body.insert("applied".into(), clean_plan.apply.into());
    if let Ok(Value::Object(exec_fields)) = serde_json::to_value(&exec) {
        body.extend(exec_fields);
    }
    body.insert("output".into(), output);
    Ok(report("nupdf.apply/v1", facts, doc, Value::Object(body)))
}

fn map_plan_error(e: plan::PlanError) -> Failure {
    let exit = if e.is_conflict_or_verify {
        Exit::Verify
    } else if e.code.starts_with("usage.") {
        Exit::Usage
    } else {
        Exit::Input
    };
    Failure {
        exit,
        code: e.code,
        message: e.message,
    }
}

fn input_failure_password() -> Failure {
    Failure {
        exit: Exit::Input,
        code: "input.needs_password",
        message: "the empty password does not open the document: pass --password".into(),
    }
}

fn output_summary(doc: &Document, bytes: &[u8], plan: &Clean<'_>) -> Value {
    let hash = blake3::hash(bytes);
    let mut output = json!({
        "path": plan.output.map(|p| p.display().to_string()),
        "doc_id": DocId::from_hash(&hash).to_string(),
        "len": bytes.len(),
        "blake3": hash.to_hex().to_string(),
    });
    if let Some(sigs) = signatures_kept(doc, bytes, plan) {
        output["signatures"] = sigs;
    }
    output
}

/// Options for `nupdf query`.
pub struct QueryArgs<'a> {
    pub pages: Option<&'a str>,
    pub right_of: Option<&'a str>,
    pub left_of: Option<&'a str>,
    pub below: Option<&'a str>,
    pub above: Option<&'a str>,
    pub inside: Option<&'a str>,
    pub near_widget: Option<&'a str>,
    pub pattern: Option<&'a str>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub min_size: Option<f32>,
    pub max_size: Option<f32>,
    pub color: Option<&'a str>,
    pub nearest_only: bool,
    pub max_items: Option<usize>,
    pub summary_only: bool,
}

/// `query`: spatial, structural, and numeric aggregation over a PDF or directory of PDFs.
pub fn query_cmd(target: &Path, args: &QueryArgs<'_>, open: OpenFn<'_>) -> Result<Value, Failure> {
    let inside = match args.inside {
        Some(spec) => {
            Some(query::parse_region_spec(spec).map_err(|msg| usage("usage.bad_option", msg))?)
        }
        None => None,
    };

    let spec = QuerySpec {
        pages: args.pages.map(str::to_string),
        right_of: args.right_of.map(str::to_string),
        left_of: args.left_of.map(str::to_string),
        below: args.below.map(str::to_string),
        above: args.above.map(str::to_string),
        inside,
        near_widget: args.near_widget.map(str::to_string),
        pattern: args.pattern.map(str::to_string),
        bold: args.bold,
        italic: args.italic,
        min_size: args.min_size,
        max_size: args.max_size,
        color: args.color.map(str::to_string),
        nearest_only: args.nearest_only,
        max_items: args.max_items,
    };

    let target_str = target.to_string_lossy().into_owned();
    let is_multi = target.is_dir()
        || target_str.contains('*')
        || target_str.contains('?')
        || target_str.contains('[');

    if !is_multi {
        let (doc, facts) = open(target)?;
        let mut qres = query::run_query(&doc, &spec).map_err(map_query_error)?;
        let mut matches_json = serialize_matches(&qres.matches, None)?;
        if args.summary_only {
            matches_json.clear();
            qres.aggregation.values.clear();
        }
        let body = json!({
            "file_count": 1usize,
            "match_count": qres.match_count,
            "truncated": qres.truncated,
            "matches": matches_json,
            "aggregation": qres.aggregation,
        });
        return Ok(report("nupdf.query/v1", &facts, &doc, body));
    }

    let files = super::batch::files(std::slice::from_ref(&target_str))?;
    if files.is_empty() {
        return Err(usage(
            "usage.bad_option",
            format!("no PDF files found in {}", target.display()),
        ));
    }

    // Run per-file queries across worker threads in deterministic file order.
    let mut per_file_spec = spec.clone();
    per_file_spec.max_items = None;

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(files.len().max(1));

    let chunk_size = files.len().div_ceil(workers);
    type QueryFileItem = Result<(String, query::QueryResult, Value), Failure>;
    let mut chunk_results: Vec<Vec<QueryFileItem>> = Vec::with_capacity(workers);

    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in files.chunks(chunk_size) {
            let spec_ref = &per_file_spec;
            handles.push(scope.spawn(move || {
                let mut local = Vec::with_capacity(chunk.len());
                for path in chunk {
                    let item = open(path).and_then(|(doc, _facts)| {
                        let qr = query::run_query(&doc, spec_ref).map_err(map_query_error)?;
                        let diags = diagnostics(&doc);
                        Ok((path.display().to_string(), qr, diags))
                    });
                    local.push(item);
                }
                local
            }));
        }
        for h in handles {
            if let Ok(v) = h.join() {
                chunk_results.push(v);
            }
        }
    });

    let mut all_matches: Vec<Value> = Vec::new();
    let mut all_values: Vec<f64> = Vec::new();
    let mut total_count = 0usize;
    let mut truncated = false;
    let mut all_diags: Vec<Value> = Vec::new();

    for group in chunk_results {
        for item in group {
            let (file_name, qr, diags) = item?;
            if let Value::Array(ds) = diags {
                for mut d in ds {
                    d["file"] = Value::String(file_name.clone());
                    all_diags.push(d);
                }
            }
            for m in &qr.matches {
                if let Some(cap) = args.max_items
                    && total_count >= cap
                {
                    truncated = true;
                    break;
                }
                total_count += 1;
                if let Some(n) = m.number {
                    all_values.push(n);
                }
                if !args.summary_only {
                    let mut mv = serde_json::to_value(m).map_err(|e| Failure {
                        exit: Exit::Internal,
                        code: "internal.serialize",
                        message: e.to_string(),
                    })?;
                    if let Some(obj) = mv.as_object_mut() {
                        obj.insert("file".into(), Value::String(file_name.clone()));
                    }
                    all_matches.push(mv);
                }
            }
        }
    }

    let agg = build_aggregation(total_count, all_values, args.summary_only);
    let mut out = Map::new();
    out.insert("schema".into(), "nupdf.query/v1".into());
    out.insert("doc_id".into(), Value::Null);
    out.insert("file_count".into(), files.len().into());
    out.insert("match_count".into(), total_count.into());
    out.insert("truncated".into(), truncated.into());
    out.insert("matches".into(), Value::Array(all_matches));
    out.insert(
        "aggregation".into(),
        serde_json::to_value(agg).unwrap_or(Value::Null),
    );
    out.insert("diagnostics".into(), Value::Array(all_diags));
    Ok(Value::Object(out))
}

fn serialize_matches(
    matches: &[query::QueryMatch],
    file_name: Option<&str>,
) -> Result<Vec<Value>, Failure> {
    let mut out = Vec::with_capacity(matches.len());
    for m in matches {
        let mut mv = serde_json::to_value(m).map_err(|e| Failure {
            exit: Exit::Internal,
            code: "internal.serialize",
            message: e.to_string(),
        })?;
        if let Some(obj) = mv.as_object_mut() {
            obj.insert(
                "file".into(),
                file_name.map_or(Value::Null, |f| Value::String(f.to_string())),
            );
        }
        out.push(mv);
    }
    Ok(out)
}

fn build_aggregation(count: usize, values: Vec<f64>, summary_only: bool) -> Aggregation {
    let numeric_count = values.len();
    if values.is_empty() {
        return Aggregation {
            count,
            numeric_count: 0,
            sum: None,
            min: None,
            max: None,
            mean: None,
            values: Vec::new(),
        };
    }
    let mut sum = 0.0f64;
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for &v in &values {
        sum += v;
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
    }
    let mean = sum / (numeric_count as f64);
    Aggregation {
        count,
        numeric_count,
        sum: Some(sum),
        min: Some(min),
        max: Some(max),
        mean: Some(mean),
        values: if summary_only { Vec::new() } else { values },
    }
}

fn map_query_error(msg: String) -> Failure {
    if msg.starts_with("invalid ") {
        usage("usage.bad_option", msg)
    } else {
        input_err("input.page_tree", msg)
    }
}

/// `audit`: deterministic zero-token claim & citation verifier.
#[allow(clippy::too_many_arguments)]
pub fn audit_cmd(
    doc: &Document,
    facts: &FileFacts,
    claims_file: Option<&Path>,
    inline_claims: &[String],
    evidence_dir: Option<&Path>,
    dpi: f32,
    allow_hazards: bool,
    strict: bool,
    pretty: bool,
) -> Result<Value, Failure> {
    if claims_file.is_none() && inline_claims.is_empty() {
        return Err(usage(
            "usage.bad_option",
            "nothing to audit: pass --claims FILE or --claim JSON".into(),
        ));
    }
    if !(dpi.is_finite() && dpi > 0.0 && dpi <= 1200.0) {
        return Err(usage(
            "usage.bad_option",
            format!("bad --dpi {dpi}: expected more than 0, at most 1200"),
        ));
    }

    let mut claims: Vec<ClaimInput> = Vec::new();
    if let Some(path) = claims_file {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            input_err(
                "input.unreadable",
                format!("cannot read {}: {e}", path.display()),
            )
        })?;
        let parsed = parse_claims_payload(&raw)?;
        claims.extend(parsed);
    }
    for raw in inline_claims {
        let c: ClaimInput = serde_json::from_str(raw).map_err(|e| {
            usage(
                "usage.bad_value",
                format!("invalid --claim JSON {raw:?}: {e}"),
            )
        })?;
        claims.push(c);
    }
    if claims.is_empty() {
        return Err(usage("usage.bad_option", "claims list is empty".into()));
    }

    let ev_dir = match evidence_dir {
        Some(d) => Some(output_dir(d)?),
        None => None,
    };

    let summary = audit::audit_claims_with_options(
        doc,
        &facts.doc_id,
        &claims,
        ev_dir.is_some(),
        dpi,
        allow_hazards,
    )
    .map_err(|msg| input_err("input.page_tree", msg))?;

    let pngs: Vec<Option<Vec<u8>>> = summary
        .results
        .iter()
        .map(|r| r.evidence_png.clone())
        .collect();

    let mut body = serde_json::to_value(&summary).map_err(|e| Failure {
        exit: Exit::Internal,
        code: "internal.serialize",
        message: e.to_string(),
    })?;

    if let Some(results) = body.get_mut("results").and_then(Value::as_array_mut) {
        for (idx, r_val) in results.iter_mut().enumerate() {
            let mut ev_path: Option<String> = None;
            if let (Some(dir), Some(Some(png))) = (ev_dir, pngs.get(idx)) {
                let path = dir.join(format!("claim-{:04}.png", idx + 1));
                write_file(&path, png).map_err(|e| {
                    input_err(
                        "output.unwritable",
                        format!("cannot write {}: {e}", path.display()),
                    )
                })?;
                ev_path = Some(path.display().to_string());
            }
            if let Some(obj) = r_val.as_object_mut() {
                obj.insert(
                    "evidence_file".into(),
                    ev_path.map_or(Value::Null, Value::String),
                );
            }
        }
    }

    let out = report("nupdf.audit/v1", facts, doc, body);
    if strict && !summary.all_verified {
        emit(&mut standard_output(), &out, pretty);
        return Err(Failure {
            exit: Exit::Verify,
            code: "audit.failed",
            message: format!(
                "{} of {} claim(s) did not pass strict verification",
                summary.failed, summary.total
            ),
        });
    }
    Ok(out)
}

fn parse_claims_payload(raw: &str) -> Result<Vec<ClaimInput>, Failure> {
    let val: Value = serde_json::from_str(raw)
        .map_err(|e| usage("usage.bad_value", format!("invalid claims JSON: {e}")))?;
    match val {
        Value::Array(_) => serde_json::from_value(val)
            .map_err(|e| usage("usage.bad_value", format!("invalid claims array: {e}"))),
        Value::Object(mut m) => {
            let Some(arr) = m.remove("claims") else {
                return Err(usage(
                    "usage.bad_value",
                    "claims JSON object must contain a `claims` array".into(),
                ));
            };
            serde_json::from_value(arr)
                .map_err(|e| usage("usage.bad_value", format!("invalid `claims` array: {e}")))
        }
        _ => Err(usage(
            "usage.bad_value",
            "claims file must be a JSON array or {\"claims\": [...]}".into(),
        )),
    }
}
