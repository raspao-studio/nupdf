// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `cargo xtask schemas`: the JSON Schema of every report, read off what
//! the commands print. Each command is run on every smoke fixture, in the
//! variants that give its report a different shape; the reports are
//! grouped by their `schema`, and each group's shape becomes that schema:
//! the types each field has had, the fields every report had as required.
//! A few places hold what the shape cannot tell (a PDF object, a map with
//! keys from the document), and those are written by hand here. With
//! `--check`, fails if the committed schemas are out of date.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::proc;

#[path = "../../crates/nupdf-cli/tests/support/schema_check.rs"]
mod schema_check;

/// Where the schemas go, and the module that embeds them.
const DIR: &str = "crates/nupdf-cli/schemas";
const MODULE: &str = "crates/nupdf-cli/src/schemas.rs";

/// How many of the standard tier's files the schemas are read off.
const STANDARD_SAMPLE: usize = 400;

/// An object with more keys than this is a map from the document, not a
/// record.
const MAP_KEYS: usize = 24;

/// What the values seen at one place had.
#[derive(Debug, Default)]
struct Shape {
    types: BTreeSet<&'static str>,
    /// Each key's shape and how many objects had it.
    props: BTreeMap<String, (Shape, usize)>,
    objects: usize,
    items: Option<Box<Shape>>,
}

impl Shape {
    fn add(&mut self, v: &Value) {
        match v {
            Value::Null => {
                self.types.insert("null");
            }
            Value::Bool(_) => {
                self.types.insert("boolean");
            }
            Value::Number(n) => {
                self.types
                    .insert(if n.is_f64() { "number" } else { "integer" });
            }
            Value::String(_) => {
                self.types.insert("string");
            }
            Value::Array(items) => {
                self.types.insert("array");
                let shape = self.items.get_or_insert_with(Box::default);
                for i in items {
                    shape.add(i);
                }
            }
            Value::Object(m) => {
                self.types.insert("object");
                self.objects += 1;
                for (k, v) in m {
                    let (shape, n) = self.props.entry(k.clone()).or_default();
                    shape.add(v);
                    *n += 1;
                }
            }
        }
    }

    /// All the values of this object's keys, as one shape.
    fn values(&self) -> Shape {
        let mut out = Shape::default();
        for (shape, _) in self.props.values() {
            out.merge(shape);
        }
        out
    }

    fn merge(&mut self, other: &Shape) {
        self.types.extend(other.types.iter().copied());
        self.objects += other.objects;
        for (k, (shape, n)) in &other.props {
            let (mine, count) = self.props.entry(k.clone()).or_default();
            mine.merge(shape);
            *count += n;
        }
        if let Some(items) = &other.items {
            self.items.get_or_insert_with(Box::default).merge(items);
        }
    }

    /// Keys found inside a value under the same key (stext's `blocks`).
    fn recursive_keys(&self, stack: &mut Vec<String>, found: &mut BTreeSet<String>) {
        for (k, (shape, _)) in &self.props {
            if stack.contains(k) {
                found.insert(k.clone());
            }
            stack.push(k.clone());
            shape.recursive_keys(stack, found);
            stack.pop();
        }
        if let Some(items) = &self.items {
            items.recursive_keys(stack, found);
        }
    }

    /// Every value under `key`, at any depth, as one shape.
    fn gather(&self, key: &str, out: &mut Shape) {
        for (k, (shape, _)) in &self.props {
            if k == key {
                out.merge(shape);
            }
            shape.gather(key, out);
        }
        if let Some(items) = &self.items {
            items.gather(key, out);
        }
    }

    #[cfg(test)]
    fn schema(&self, path: &str, overrides: &BTreeMap<String, Value>) -> Value {
        self.schema_with(path, overrides, &BTreeSet::new())
    }

    /// The schema, values under the `refs` keys as references to `$defs`.
    fn schema_with(
        &self,
        path: &str,
        overrides: &BTreeMap<String, Value>,
        refs: &BTreeSet<String>,
    ) -> Value {
        if let Some(fixed) = overrides.get(path) {
            return fixed.clone();
        }
        // A page tree that cannot be counted gives null, in every report.
        if path.ends_with("/page_count") || path.ends_with("/page_count_after") {
            return json!({"type": ["integer", "null"]});
        }
        // Only ever an empty array's items: anything.
        if self.types.is_empty() {
            return json!({});
        }
        let mut types: Vec<&str> = self.types.iter().copied().collect();
        // An integer is a number: say number once.
        if types.contains(&"number") {
            types.retain(|t| *t != "integer");
        }
        let mut out = Map::new();
        out.insert(
            "type".into(),
            match types.as_slice() {
                [one] => Value::from(*one),
                _ => types.into(),
            },
        );
        if self.types.contains("object") {
            if self.props.len() > MAP_KEYS {
                out.insert(
                    "additionalProperties".into(),
                    self.values()
                        .schema_with(&format!("{path}/*"), overrides, refs),
                );
            } else {
                let mut props = Map::new();
                let mut required = Vec::new();
                for (k, (shape, n)) in &self.props {
                    let child = if refs.contains(k) {
                        json!({"$ref": format!("#/$defs/{k}")})
                    } else {
                        shape.schema_with(&format!("{path}/{k}"), overrides, refs)
                    };
                    props.insert(k.clone(), child);
                    if *n == self.objects {
                        required.push(Value::from(k.as_str()));
                    }
                }
                out.insert("properties".into(), props.into());
                if !required.is_empty() {
                    out.insert("required".into(), required.into());
                }
            }
        }
        if let Some(items) = &self.items {
            out.insert(
                "items".into(),
                items.schema_with(&format!("{path}/[]"), overrides, refs),
            );
        }
        out.into()
    }
}

/// A PDF object as `objects` and `show` encode it.
fn pdf_object() -> Value {
    let object = json!({"$ref": "#/$defs/object"});
    let with = |key: &str, ty: Value| json!({"type": "object", "required": [key], "properties": {key: ty}});
    json!({"anyOf": [
        {"type": ["null", "boolean", "integer"]},
        with("r", json!({"type": "number", "description": "a real"})),
        with("n", json!({"type": "string", "description": "a name, #-escaped"})),
        with("s", json!({"type": "string", "description": "a string, in hex"})),
        with("ref", json!({"type": "string", "description": "a reference, \"12 0 R\""})),
        {"type": "array", "items": object},
        with("d", json!({"type": "object", "description": "a dictionary", "additionalProperties": object})),
        {"type": "object", "required": ["stream"], "description": "a stream: its dictionary and raw length",
         "properties": {"stream": object, "raw_len": {"type": ["integer", "null"]}}},
    ]})
}

/// What each schema holds that its shape cannot tell, by path from the
/// report (`*` a map's values, `[]` an array's items).
fn overrides(name: &str) -> BTreeMap<String, Value> {
    let object = json!({"$ref": "#/$defs/object"});
    let any_report =
        json!({"type": ["object", "null"], "description": "the command's report, as it prints it"});
    let pairs: Vec<(&str, Value)> = match name {
        "objects" => vec![(
            "/objects",
            json!({"type": "object", "description": "each object by number",
                   "additionalProperties": object}),
        )],
        "show" => vec![("/value", object)],
        "batch.item" => vec![("/report", any_report.clone()), ("/error", any_report)],
        "meta" | "info" => vec![(
            "/info",
            json!({"type": "object", "description": "the Info dictionary's entries, as text",
                   "additionalProperties": {"type": "string"}}),
        )],
        "diff" => vec![
            ("/structure/differences/[]/a", json!({})),
            ("/structure/differences/[]/b", json!({})),
        ],
        "attest" => vec![
            (
                "/pages/[]/spans/[]/glyph_text",
                json!({"type": ["string", "null"]}),
            ),
            (
                "/pages/[]/blind_spots/[]/crop_file",
                json!({"type": ["string", "null"]}),
            ),
        ],
        "patch" => vec![(
            "/patches/[]/new_bbox",
            json!({"type": ["array", "null"], "items": {"type": "number"}}),
        )],
        "apply" => vec![
            (
                "/pages_kept",
                json!({"type": ["array", "null"], "items": {"type": "integer"}}),
            ),
            ("/rotated", json!({"type": ["integer", "null"]})),
            ("/redact_verified", json!({"type": ["boolean", "null"]})),
        ],
        "query" => vec![
            ("/doc_id", json!({"type": ["string", "null"]})),
            ("/matches/[]/file", json!({"type": ["string", "null"]})),
            ("/matches/[]/number", json!({"type": ["number", "null"]})),
            ("/matches/[]/anchor_id", json!({"type": ["string", "null"]})),
            ("/matches/[]/distance", json!({"type": ["number", "null"]})),
            ("/aggregation/sum", json!({"type": ["number", "null"]})),
            ("/aggregation/min", json!({"type": ["number", "null"]})),
            ("/aggregation/max", json!({"type": ["number", "null"]})),
            ("/aggregation/mean", json!({"type": ["number", "null"]})),
        ],
        "audit" => vec![
            ("/results/[]/claim_id", json!({"type": ["string", "null"]})),
            ("/results/[]/cited_id", json!({"type": ["string", "null"]})),
            (
                "/results/[]/resolved_page",
                json!({"type": ["integer", "null"]}),
            ),
            (
                "/results/[]/resolved_bbox",
                json!({"type": ["array", "null"], "items": {"type": "number"}}),
            ),
            (
                "/results/[]/actual_text",
                json!({"type": ["string", "null"]}),
            ),
            (
                "/results/[]/footnote_text",
                json!({"type": ["string", "null"]}),
            ),
            (
                "/results/[]/evidence_file",
                json!({"type": ["string", "null"]}),
            ),
        ],
        "chart" => vec![
            ("/charts/[]/title", json!({"type": ["string", "null"]})),
            (
                "/charts/[]/series/[]/points/[]/x_value",
                json!({"type": ["number", "null"]}),
            ),
            (
                "/diagrams/[]/edges/[]/label",
                json!({"type": ["string", "null"]}),
            ),
        ],
        "flat_forms" => vec![
            ("/verified", json!({"type": ["boolean", "null"]})),
            ("/output", json!({"type": ["object", "null"]})),
            ("/fields/[]/label", json!({"type": ["string", "null"]})),
            ("/fields/[]/checked", json!({"type": ["boolean", "null"]})),
            (
                "/fields/[]/cell_count",
                json!({"type": ["integer", "null"]}),
            ),
            (
                "/fields/[]/current_text",
                json!({"type": ["string", "null"]}),
            ),
            (
                "/applied_fills/[]/value",
                json!({"type": ["string", "null"]}),
            ),
            (
                "/applied_fills/[]/checked",
                json!({"type": ["boolean", "null"]}),
            ),
            (
                "/applied_fills/[]/font_size",
                json!({"type": ["number", "null"]}),
            ),
        ],
        "history" => vec![
            ("/extracted_revision", json!({"type": ["integer", "null"]})),
            ("/extracted_output", json!({"type": ["object", "null"]})),
            (
                "/revisions/[]/delta_from_previous",
                json!({"type": ["object", "null"]}),
            ),
            (
                "/revisions/[]/signatures/[]/signatory",
                json!({"type": ["string", "null"]}),
            ),
        ],
        "cloak" => vec![("/output/path", json!({"type": ["string", "null"]}))],
        "uncloak" => vec![
            ("/doc_id", json!({"type": ["string", "null"]})),
            ("/restored_doc_id", json!({"type": ["string", "null"]})),
            ("/output/path", json!({"type": ["string", "null"]})),
        ],
        "pipeline" => vec![
            ("/aborted", json!({"type": ["object", "null"]})),
            ("/output", json!({"type": ["object", "null"]})),
            (
                "/steps/[]/data",
                json!({"type": "object", "description": "step-specific output payload"}),
            ),
        ],
        _ => vec![],
    };
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// Every place a report has a PDF object, which needs the definition.
fn uses_objects(name: &str) -> bool {
    matches!(name, "objects" | "show")
}

/// A program that answers as Tesseract does, with two words for any page,
/// so `ocr`'s report can be read without Tesseract.
fn stand_in_tesseract(work: &Path) -> String {
    let path = work.join("tesseract.sh");
    let script = "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'tesseract 5 (stand-in)'; exit 0; fi\n\
        printf 'level\\tpage_num\\tblock_num\\tpar_num\\tline_num\\tword_num\\tleft\\ttop\\twidth\\theight\\tconf\\ttext\\n'\n\
        printf '5\\t1\\t1\\t1\\t1\\t1\\t10\\t10\\t50\\t20\\t90\\tStand\\n5\\t1\\t1\\t1\\t1\\t2\\t70\\t10\\t30\\t20\\t80\\tin\\n'\n";
    if !path.exists() {
        let _ = std::fs::create_dir_all(work);
        let _ = std::fs::write(&path, script);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
        }
    }
    path.to_string_lossy().into_owned()
}

/// The runs of `nupdf` on `file` that give the reports their shapes.
fn runs(file: &str, work: &Path) -> Vec<Vec<String>> {
    let w = |p: &str| work.join(p).to_string_lossy().into_owned();
    let tesseract = stand_in_tesseract(work);
    let lines: Vec<Vec<&str>> = vec![
        vec!["info", file],
        vec!["objects", file],
        vec!["pages", file],
        vec!["pages", file, "--rotate", "90", "--dry-run"],
        vec!["meta", file],
        vec!["meta", file, "--set", "Title=x", "--dry-run"],
        vec!["streams", file],
        vec!["operators", file],
        vec!["trace", file],
        vec!["stext", file],
        vec![
            "stext",
            file,
            "--options",
            "preserve-images,vectors,segment,table-hunt",
        ],
        vec!["extract", file],
        vec!["extract", file, "--format", "jsonl"],
        vec!["extract", file, "--format", "text"],
        vec!["extract", file, "--max-chars", "40"],
        vec![
            "extract",
            file,
            "--fields",
            "page,bbox,blocks,type,lines,wmode,dir,spans,font,size,bold,italic,color,text,chars",
        ],
        vec!["search", file, "e"],
        vec!["fonts", file],
        vec!["images", file],
        vec!["images", file, "--digests"],
        vec!["show", file, "trailer"],
        vec!["show", file, "1", "--data"],
        vec!["clean", file, "--dry-run"],
        vec!["clean", file, "-gg", "-z", "--dry-run"],
        vec!["merge", file, file, "--dry-run"],
        vec!["encrypt", file, "--dry-run"],
        vec!["decrypt", file, "--dry-run"],
        vec!["redact", file, "--text", "e", "--dry-run"],
        vec!["verify", file, "--text", "e"],
        vec!["outline", file],
        vec!["tables", file],
        vec!["annots", file],
        vec!["forms", file],
        vec!["signatures", file],
        vec!["flatten", file, "--dry-run"],
        vec!["forms", file, "--fill-first", "--dry-run"],
        vec![
            "annots",
            file,
            "--add",
            r#"{"page":1,"type":"Square","rect":[10,10,60,60],"contents":"x"}"#,
            "--dry-run",
        ],
        vec!["sanitize", file, "--dry-run"],
        vec!["ocr", file, "--all", "--dry-run", "--tesseract", &tesseract],
        vec!["links", file],
        vec!["labels", file],
        vec!["attachments", file],
        vec!["diff", file, file],
        vec![
            "diff",
            file,
            file,
            "--by",
            "text,structure,pixels",
            "--dpi",
            "8",
        ],
    ];
    let mut out: Vec<Vec<String>> = lines
        .into_iter()
        .map(|l| l.into_iter().map(String::from).collect())
        .collect();
    out.push(vec![
        "render".into(),
        file.into(),
        "-o".into(),
        w("render"),
        "--dpi".into(),
        "8".into(),
    ]);
    out.push(vec![
        "redact".into(),
        file.into(),
        "--text".into(),
        "e".into(),
        "-o".into(),
        w("redacted.pdf"),
    ]);
    out.push(vec![
        "clean".into(),
        file.into(),
        "-o".into(),
        w("clean.pdf"),
    ]);
    out.push(vec![
        "split".into(),
        file.into(),
        "-o".into(),
        w("split"),
        "--dry-run".into(),
    ]);
    out.push(vec![
        "attest".into(),
        file.into(),
        "--dpi".into(),
        "36".into(),
        "--crops-dir".into(),
        w("attest_crops"),
    ]);
    out.push(vec![
        "patch".into(),
        file.into(),
        "--replace".into(),
        "p1.b1.l1.s1=x".into(),
        "-o".into(),
        w("patched.pdf"),
    ]);
    out.push(vec![
        "patch".into(),
        file.into(),
        "--replace".into(),
        "p1.b1.l1.s1=x".into(),
        "--dry-run".into(),
        "--no-verify".into(),
    ]);
    out.push(vec![
        "apply".into(),
        file.into(),
        "--inline".into(),
        r#"{"ops":[{"op":"meta","set":{"Title":"x"}}]}"#.into(),
        "--dry-run".into(),
    ]);
    out.push(vec![
        "apply".into(),
        file.into(),
        "--inline".into(),
        r#"{"ops":[{"op":"patch","id":"p1.b1.l1.s1","text":"x"},{"op":"pages","rotate":90}]}"#
            .into(),
        "-o".into(),
        w("applied.pdf"),
    ]);
    out.push(vec!["query".into(), file.into()]);
    out.push(vec![
        "query".into(),
        file.into(),
        "--right-of".into(),
        "e".into(),
        "--nearest-only".into(),
    ]);
    out.push(vec![
        "audit".into(),
        file.into(),
        "--claim".into(),
        r#"{"claim_id":"c1","page":1,"quote":"e"}"#.into(),
        "--claim".into(),
        r#"{"id":"p1.b1.l1.s1","quote":"not-present-quote"}"#.into(),
        "--evidence-dir".into(),
        w("audit_ev"),
        "--dpi".into(),
        "36".into(),
    ]);
    out.push(vec!["chart".into(), file.into()]);
    out.push(vec!["flat-forms".into(), file.into()]);
    out.push(vec!["history".into(), file.into()]);
    out.push(vec![
        "patch".into(),
        file.into(),
        "--replace".into(),
        "p1.b1=Reflowed".into(),
        "--reflow".into(),
        "--dry-run".into(),
    ]);
    out.push(vec![
        "cloak".into(),
        file.into(),
        "--vault".into(),
        w("vault.json"),
        "--entity".into(),
        "WORD=e".into(),
        "-o".into(),
        w("cloaked.pdf"),
    ]);
    out.push(vec![
        "uncloak".into(),
        w("cloaked.pdf"),
        "--vault".into(),
        w("vault.json"),
        "--dry-run".into(),
    ]);
    out.push(vec![
        "pipeline".into(),
        file.into(),
        "--inline".into(),
        r#"[{"op":"info","id":"i1"},{"op":"query","id":"q1","right_of":"e"},{"op":"chart","id":"c1"},{"op":"flat_forms","id":"ff1"},{"op":"history","id":"h1"}]"#.into(),
    ]);
    out
}

/// Writes a compact 1-page PDF into `work` with a calibrated bar chart, a
/// 2-node directed flowchart, flat underline/checkbox/comb fields, and PII so
/// `infer` observes populated array items for `chart`, `flat-forms`, `history`,
/// `cloak`, `uncloak`, and `pipeline`.
fn write_v2_showcase_pdf(work: &Path) -> Result<PathBuf> {
    let stream = "\
BT /F1 12 Tf 180 760 Td (Quarterly Revenue) Tj ET\n\
0.2 0.4 0.8 rg 360 758 10 8 re f\n\
BT /F1 9 Tf 375 759 Td (Revenue) Tj ET\n\
0 0 0 RG 1 w 80 580 m 80 730 l 320 580 m 80 580 l S\n\
BT /F1 9 Tf 55 577 Td (0) Tj 50 652 Td (50) Tj 45 727 Td (100) Tj ET\n\
0.2 0.4 0.8 rg 120 580 36 75 re f 210 580 36 120 re f\n\
BT /F1 9 Tf 128 565 Td (Q1) Tj 218 565 Td (Q2) Tj ET\n\
0 0 0 RG 1 w 80 460 80 32 re S 230 460 80 32 re S\n\
BT /F1 10 Tf 102 472 Td (Start) Tj 254 472 Td (Done) Tj ET\n\
0 0 0 RG 1 w 160 476 m 224 476 l S\n\
0 0 0 rg 230 476 m 222 480 l 222 472 l h f\n\
BT /F1 8 Tf 185 482 Td (ok) Tj ET\n\
BT /F1 10 Tf 80 380 Td (Applicant:) Tj ET\n\
0 0 0 RG 1 w 145 378 m 300 378 l S\n\
BT /F1 10 Tf 80 340 Td (Agree) Tj ET\n\
0 0 0 RG 1 w 64 338 10 10 re S\n\
BT /F1 10 Tf 80 300 Td (Code:) Tj ET\n\
0 0 0 RG 1 w 125 295 80 18 re S 145 295 m 145 313 l 165 295 m 165 313 l 185 295 m 185 313 l S\n\
BT /F1 10 Tf 80 240 Td (SSN 123-45-6789 Email alice@example.com) Tj ET\n";

    let objs = [
        "<</Type/Catalog/Pages 2 0 R>>".to_string(),
        "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 4 0 R>>>>/Contents 5 0 R>>".to_string(),
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_string(),
        format!("<</Length {}>>\nstream\n{stream}endstream", stream.len()),
    ];
    let mut pdf = Vec::from(&b"%PDF-1.7\n"[..]);
    let mut offsets = vec![0usize];
    for (idx, body) in objs.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", idx + 1).as_bytes());
    }
    let xref_pos = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for off in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref_pos}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    let path = work.join("v2_showcase.pdf");
    std::fs::write(&path, pdf)?;
    Ok(path)
}

/// Every report in what a run printed, by its `schema`.
fn collect(out: &[u8], into: &mut BTreeMap<String, Vec<Value>>) {
    for line in String::from_utf8_lossy(out).lines() {
        if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(line)
            && let Some(id) = v["schema"].as_str().map(str::to_string)
        {
            into.entry(id).or_default().push(v);
        }
    }
}

/// Each command's first sentence, from its own `--help`; the commands are
/// those `nupdf guide` names.
fn abouts(nupdf: &Path) -> Result<BTreeMap<String, String>> {
    let r = proc::run(nupdf, &["guide"], Duration::from_secs(30))?;
    let guide = String::from_utf8_lossy(&r.stdout).into_owned();
    let names: Vec<String> = guide
        .split("## Commands")
        .nth(1)
        .unwrap_or("")
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect();
    let mut out = BTreeMap::new();
    for name in names {
        let help = proc::run(nupdf, &[name.as_str(), "--help"], Duration::from_secs(30))?;
        let text = String::from_utf8_lossy(&help.stdout).into_owned();
        // The about: the lines before the first blank one, as one sentence.
        let first: Vec<&str> = text
            .lines()
            .take_while(|l| !l.trim().is_empty())
            .map(str::trim)
            .collect();
        let about = first.join(" ");
        let sentence = about
            .split(". ")
            .next()
            .unwrap_or(&about)
            .trim_end_matches('.');
        if !sentence.is_empty() {
            out.insert(name, format!("{sentence}."));
        }
    }
    Ok(out)
}

/// `nupdf.extract.page/v1` → `extract.page`.
fn short_name(id: &str) -> Option<&str> {
    id.strip_prefix("nupdf.")?.split('/').next()
}

/// The schemas, by name, from running `nupdf` at `nupdf`.
fn infer(root: &Path, nupdf: &Path) -> Result<BTreeMap<String, Value>> {
    let work = root.join("target/schemas");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let mut reports: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut files = crate::corpus::files(root, "smoke")?;
    // The standard tier, where it is fetched, shows what the fixtures do not.
    if let Ok(standard) = crate::corpus::files(root, "standard")
        && !standard.is_empty()
    {
        let step = (standard.len() / STANDARD_SAMPLE).max(1);
        files.extend(standard.into_iter().step_by(step));
    }
    let run = |args: &[String], reports: &mut BTreeMap<String, Vec<Value>>| -> Result<()> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let r = proc::run(nupdf, &args, Duration::from_secs(120))?;
        collect(&r.stdout, reports);
        collect(&r.stderr, reports);
        Ok(())
    };
    for f in &files {
        let file = f.path.to_string_lossy().into_owned();
        for args in runs(&file, &work) {
            run(&args, &mut reports)?;
        }
    }
    let smoke = root.join("corpus/smoke").to_string_lossy().into_owned();
    run(
        &["batch".into(), smoke, "--".into(), "info".into()],
        &mut reports,
    )?;
    run(&["info".into(), "/no/such/file.pdf".into()], &mut reports)?;
    run(&["schema".into()], &mut reports)?;

    let showcase = write_v2_showcase_pdf(&work)?;
    let sc = showcase.to_string_lossy().into_owned();
    let w = |p: &str| work.join(p).to_string_lossy().into_owned();
    run(&["chart".into(), sc.clone()], &mut reports)?;
    run(&["flat-forms".into(), sc.clone()], &mut reports)?;
    run(
        &[
            "flat-forms".into(),
            sc.clone(),
            "--set".into(),
            "applicant=Ada".into(),
            "--check".into(),
            "agree".into(),
            "-o".into(),
            w("v2_ff_filled.pdf"),
        ],
        &mut reports,
    )?;
    run(
        &[
            "flat-forms".into(),
            sc.clone(),
            "--promote".into(),
            "--dry-run".into(),
        ],
        &mut reports,
    )?;
    run(
        &[
            "meta".into(),
            sc.clone(),
            "--set".into(),
            "Title=Rev2".into(),
            "--incremental".into(),
            "-o".into(),
            w("v2_rev2.pdf"),
        ],
        &mut reports,
    )?;
    run(
        &[
            "history".into(),
            w("v2_rev2.pdf"),
            "--extract-revision".into(),
            "1".into(),
            "-o".into(),
            w("v2_rev1.pdf"),
        ],
        &mut reports,
    )?;
    run(
        &[
            "cloak".into(),
            sc.clone(),
            "--vault".into(),
            w("v2_vault.json"),
            "--preset".into(),
            "pii".into(),
            "-o".into(),
            w("v2_cloaked.pdf"),
        ],
        &mut reports,
    )?;
    let txt_path = work.join("v2_summary.txt");
    std::fs::write(&txt_path, "Summary token check\n")?;
    run(
        &[
            "uncloak".into(),
            txt_path.to_string_lossy().into_owned(),
            "--vault".into(),
            w("v2_vault.json"),
            "--text-mode".into(),
            "--dry-run".into(),
        ],
        &mut reports,
    )?;
    run(
        &[
            "pipeline".into(),
            sc,
            "--inline".into(),
            r#"[{"op":"query","id":"q1","right_of":"Applicant"},{"op":"patch","id":"p1","replacements":[{"id":"p1.b1.l1.s1","text":"Edited Revenue"}]}]"#.into(),
            "--dry-run".into(),
        ],
        &mut reports,
    )?;
    let cli = abouts(nupdf)?;
    let mut out = BTreeMap::new();
    for (id, values) in &reports {
        let Some(name) = short_name(id) else { continue };
        let mut shape = Shape::default();
        for v in values {
            shape.add(v);
        }
        let mut recursive = BTreeSet::new();
        shape.recursive_keys(&mut Vec::new(), &mut recursive);
        let fixed = overrides(name);
        let mut schema = match shape.schema_with("", &fixed, &recursive) {
            Value::Object(m) => m,
            _ => bail!("{id}: reports are objects"),
        };
        let mut defs = Map::new();
        for k in &recursive {
            let mut all = Shape::default();
            shape.gather(k, &mut all);
            defs.insert(
                k.clone(),
                all.schema_with(&format!("$defs/{k}"), &fixed, &recursive),
            );
        }
        let mut head = Map::new();
        head.insert(
            "$schema".into(),
            "https://json-schema.org/draft/2020-12/schema".into(),
        );
        head.insert("$id".into(), id.clone().into());
        head.insert("title".into(), id.clone().into());
        let command = name.split('.').next().unwrap_or(name);
        if let Some(about) = cli.get(command) {
            head.insert("description".into(), about.clone().into());
        }
        head.insert(
            "$comment".into(),
            "Generated by `cargo xtask schemas` from the reports on the smoke tier and a sample of the standard tier.".into(),
        );
        head.append(&mut schema);
        if uses_objects(name) {
            defs.insert("object".into(), pdf_object());
        }
        if !defs.is_empty() {
            head.insert("$defs".into(), defs.into());
        }
        out.insert(name.to_string(), Value::Object(head));
    }
    Ok(out)
}

/// The module that embeds the schemas, by name.
fn module(names: &[&String]) -> String {
    let mut s = String::from(
        "// SPDX-License-Identifier: AGPL-3.0-or-later\n// Copyright (C) 2026 hum\n// Generated by `cargo xtask schemas`; do not edit.\n\n//! The JSON Schema of every report, by name (`extract`, `extract.page`).\n\npub const SCHEMAS: &[(&str, &str)] = &[\n",
    );
    for n in names {
        s.push_str(&format!(
            "    (\"{n}\", include_str!(\"../schemas/{n}.json\")),\n"
        ));
    }
    s.push_str("];\n");
    s
}

/// Runs the commands on `tier`'s files (every `step`th) and reports where
/// their reports break the committed schemas.
pub fn validate(root: &Path, tier: &str, sample: usize) -> Result<()> {
    let nupdf = crate::build_nupdf(root)?;
    let schemas: BTreeMap<String, Value> = std::fs::read_dir(root.join(DIR))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter_map(|p| {
            let name = p.file_stem()?.to_string_lossy().into_owned();
            let v = serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
            Some((name, v))
        })
        .collect();
    let work = root.join("target/schemas-validate");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let files = crate::corpus::files(root, tier)?;
    let step = (files.len() / sample.max(1)).max(1);
    let mut broken: BTreeMap<(String, String), (usize, String)> = BTreeMap::new();
    let (mut reports, mut checked) = (0usize, 0usize);
    for f in files.iter().step_by(step) {
        checked += 1;
        let file = f.path.to_string_lossy().into_owned();
        for args in runs(&file, &work) {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            let Ok(r) = proc::run(&nupdf, &args, Duration::from_secs(120)) else {
                continue;
            };
            let mut found = BTreeMap::new();
            collect(&r.stdout, &mut found);
            collect(&r.stderr, &mut found);
            for (id, values) in found {
                let Some(schema) = short_name(&id).and_then(|n| schemas.get(n)) else {
                    broken
                        .entry((id.clone(), "no schema".into()))
                        .or_insert((0, f.name.clone()))
                        .0 += values.len();
                    continue;
                };
                for v in values {
                    reports += 1;
                    for problem in schema_check::violations(schema, &v) {
                        // Array positions and map keys vary; the place does not.
                        let (at, what) = problem.split_once(": ").unwrap_or((&problem, ""));
                        let at: Vec<&str> = at
                            .split('/')
                            .map(|p| {
                                if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
                                    "#"
                                } else {
                                    p
                                }
                            })
                            .collect();
                        let place = format!("{}: {what}", at.join("/"));
                        let e = broken
                            .entry((id.clone(), place))
                            .or_insert((0, f.name.clone()));
                        e.0 += 1;
                    }
                }
            }
        }
    }
    println!("{checked} files, {reports} reports");
    for ((id, place), (n, example)) in &broken {
        println!("{id} {place}: {n} times, e.g. {example}");
    }
    if !broken.is_empty() {
        bail!("{} places break the schemas", broken.len());
    }
    Ok(())
}

pub fn run(root: &Path, check: bool) -> Result<()> {
    if check {
        // The schemas are read off the standard tier too, which is not
        // always at hand: what can be checked anywhere is that the smoke
        // tier's reports fit them, and that every report has one.
        return validate(root, "smoke", usize::MAX);
    }
    let nupdf = crate::build_nupdf(root)?;
    let schemas = infer(root, &nupdf)?;
    let mut files: BTreeMap<PathBuf, String> = schemas
        .iter()
        .map(|(n, v)| {
            let text = serde_json::to_string_pretty(v).unwrap_or_default() + "\n";
            (root.join(DIR).join(format!("{n}.json")), text)
        })
        .collect();
    let names: Vec<&String> = schemas.keys().collect();
    files.insert(root.join(MODULE), module(&names));
    let dir = root.join(DIR);
    std::fs::create_dir_all(&dir)?;
    for e in std::fs::read_dir(&dir)?.filter_map(Result::ok) {
        if !files.contains_key(&e.path()) {
            std::fs::remove_file(e.path())
                .with_context(|| format!("removing {}", e.path().display()))?;
        }
    }
    for (path, text) in files {
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    println!("wrote {} schemas into {DIR}", schemas.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_become_schemas() {
        let mut shape = Shape::default();
        shape.add(&json!({"a": 1, "b": "x", "c": [1, 2.5]}));
        shape.add(&json!({"a": null, "c": []}));
        let s = shape.schema("", &BTreeMap::new());
        assert_eq!(s["type"], "object");
        assert_eq!(s["required"], json!(["a", "c"]));
        assert_eq!(s["properties"]["a"]["type"], json!(["integer", "null"]));
        assert_eq!(s["properties"]["c"]["items"]["type"], "number");
        let mut map = Shape::default();
        map.add(&Value::Object(
            (0..30).map(|i| (i.to_string(), json!(i))).collect(),
        ));
        assert_eq!(
            map.schema("", &BTreeMap::new())["additionalProperties"]["type"],
            "integer"
        );
        assert_eq!(short_name("nupdf.extract.page/v1"), Some("extract.page"));
    }
}
