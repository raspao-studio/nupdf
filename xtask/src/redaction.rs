// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `cargo xtask redaction`: the redaction tier. Each file of the
//! adversarial corpus (`corpus/redaction`) is first checked to leak, as
//! `nupdf verify` must see, then redacted with `nupdf redact --text
//! SECRET`, and the output judged by `nupdf verify`, by MuPDF (page text
//! with annotations, every object's strings and text streams) and by
//! pdftotext. Any leak fails the run.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::adversarial::SECRET;
use crate::proc::{self, Status};

const TIMEOUT: Duration = Duration::from_secs(120);

struct Row {
    name: String,
    class: String,
    seen_leaking: bool,
    redacted: bool,
    nupdf: Option<bool>,
    mupdf: Option<bool>,
    pdftotext: Option<bool>,
    note: String,
}

fn exit_code(s: &Status) -> Option<i32> {
    match s {
        Status::Exited(c) => Some(*c),
        _ => None,
    }
}

pub fn run(root: &Path, nupdf: &Path, mutool: &Path, dir: Option<PathBuf>) -> Result<()> {
    let dir = dir.unwrap_or_else(|| root.join("corpus/redaction"));
    let scratch = std::env::temp_dir().join(format!("nupdf-redaction-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    let pdftotext = which("pdftotext");
    let classes: std::collections::BTreeMap<String, String> = crate::adversarial::all()
        .into_iter()
        .map(|f| (f.name.to_string(), f.class.to_string()))
        .collect();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "pdf"))
        .collect();
    files.sort();
    let mut rows = Vec::new();
    for file in &files {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let class = classes.get(&name).cloned().unwrap_or_default();
        let mut row = Row {
            name: name.clone(),
            class: class.clone(),
            seen_leaking: false,
            redacted: false,
            nupdf: None,
            mupdf: None,
            pdftotext: None,
            note: String::new(),
        };
        // Redacted first, for the report's areas.
        let input = file.as_os_str();
        let out = scratch.join(&name);
        let report_path = scratch.join(format!("{name}.json"));
        let run = proc::run(
            nupdf,
            &[
                "redact".as_ref(),
                input,
                "--text".as_ref(),
                SECRET.as_ref(),
                "-o".as_ref(),
                out.as_os_str(),
            ],
            TIMEOUT,
        )?;
        row.redacted = exit_code(&run.status) == Some(0);
        if !row.redacted {
            row.note = String::from_utf8_lossy(&run.stderr)
                .chars()
                .take(200)
                .collect();
            rows.push(row);
            continue;
        }
        std::fs::write(&report_path, &run.stdout)?;
        let verify = |file: &Path| {
            proc::run(
                nupdf,
                &[
                    "verify".as_ref(),
                    file.as_os_str(),
                    "--text".as_ref(),
                    SECRET.as_ref(),
                    "--against".as_ref(),
                    report_path.as_os_str(),
                ],
                TIMEOUT,
            )
        };
        // The input leaks, in its class.
        let run = verify(file)?;
        let report: Option<Value> = serde_json::from_slice(&run.stdout).ok();
        let failing = report
            .as_ref()
            .and_then(|r| r.pointer("/verification/classes"))
            .and_then(Value::as_object)
            .map(|c| {
                c.iter()
                    .filter(|(_, v)| v.get("ok") == Some(&Value::Bool(false)))
                    .map(|(k, _)| k.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        row.seen_leaking =
            exit_code(&run.status) == Some(4) && (class.is_empty() || failing.contains(&class));
        if !row.seen_leaking {
            row.note = format!("verify on the input: {:?}, failing {failing:?}", run.status);
        }
        let v = verify(&out)?;
        row.nupdf = Some(exit_code(&v.status) == Some(0));
        let script = root.join("oracle/js/leaks.js");
        let m = proc::run(
            mutool,
            &[
                "run".as_ref(),
                script.as_os_str(),
                out.as_os_str(),
                SECRET.as_ref(),
            ],
            TIMEOUT,
        )?;
        let leaks: Option<Value> = serde_json::from_slice(&m.stdout).ok();
        row.mupdf = leaks.map(|l| {
            let empty = |k: &str| {
                l.get(k)
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
            };
            empty("pages") && empty("objects")
        });
        if let Some(p) = &pdftotext {
            let t = proc::run(p, &[out.as_os_str(), "-".as_ref()], TIMEOUT)?;
            let text = String::from_utf8_lossy(&t.stdout)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            row.pdftotext = Some(exit_code(&t.status) == Some(0) && !text.contains(SECRET));
        }
        rows.push(row);
    }
    let mark = |b: Option<bool>| match b {
        Some(true) => "clean",
        Some(false) => "LEAK",
        None => "-",
    };
    println!("| File | Class | Input leaks | Redacted | nupdf verify | MuPDF | pdftotext | Note |");
    println!("|---|---|---|---|---|---|---|---|");
    let mut bad = 0;
    for r in &rows {
        let ok = r.seen_leaking
            && r.redacted
            && r.nupdf == Some(true)
            && r.mupdf != Some(false)
            && r.pdftotext != Some(false);
        if !ok {
            bad += 1;
        }
        println!(
            "| `{}` | {} | {} | {} | {} | {} | {} | {} |",
            r.name,
            r.class,
            if r.seen_leaking { "yes" } else { "NO" },
            if r.redacted { "yes" } else { "FAILED" },
            mark(r.nupdf),
            mark(r.mupdf),
            mark(r.pdftotext),
            r.note
        );
    }
    let _ = std::fs::remove_dir_all(&scratch);
    if bad > 0 {
        bail!("{bad} of {} files leak or fail", rows.len());
    }
    println!("\nzero leaks in {} files", rows.len());
    Ok(())
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

/// The cases generated from pdf-redaction-benchmarks (fetched with `cargo
/// xtask corpus fetch redaction-benchmarks`): its generator run for seeds
/// 1 to 10 of each family, at a pinned time so the cases are the same each
/// run. Generated once, into the fetched checkout's `cases/`.
fn benchmark_cases(scratch: &Path) -> Result<PathBuf> {
    let src = crate::corpus::corpus_dir().join("redaction-benchmarks");
    if !src.join("src/pdfredeval").is_dir() {
        bail!(
            "{} has no generator; run `cargo xtask corpus fetch redaction-benchmarks`",
            src.display()
        );
    }
    let cases = src.join("cases");
    if cases.is_dir() {
        return Ok(cases);
    }
    // The generator is run in isolation (-I), from a directory of its own,
    // so nothing it ships is picked up by accident.
    let runner = scratch.join("generate.py");
    std::fs::write(
        &runner,
        format!(
            "import sys\nsys.path.insert(0, {:?})\nfrom pdfredeval import cli\nsys.exit(cli.main(sys.argv[1:]))\n",
            src.join("src").display().to_string()
        ),
    )?;
    let status = std::process::Command::new("python3")
        .arg("-I")
        .arg(&runner)
        .args(["generate", "all", "-s", "1-10", "--force", "-o"])
        .arg(&cases)
        .current_dir(scratch)
        .env("SOURCE_DATE_EPOCH", "1790000000")
        .status()?;
    if !status.success() {
        bail!("the benchmark generator failed: {status}");
    }
    Ok(cases)
}

/// One benchmark case scored: its must-redact probes redacted by value
/// (text) or by box (text drawn as an image), then judged for each value
/// by `nupdf verify`, MuPDF and pdftotext; distractors left counted.
struct BenchRow {
    case: String,
    probes: usize,
    redacted: bool,
    nupdf_leaks: usize,
    mupdf_leaks: Vec<String>,
    pdftotext_leaks: Vec<String>,
    distractors_kept: usize,
    distractors: usize,
    note: String,
}

pub fn benchmarks(root: &Path, nupdf: &Path, mutool: &Path) -> Result<()> {
    let scratch = std::env::temp_dir().join(format!("nupdf-redbench-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    let cases = benchmark_cases(&scratch)?;
    let pdftotext = which("pdftotext");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&cases)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("ground_truth.json").is_file())
        .collect();
    dirs.sort();
    let mut rows = Vec::new();
    for dir in dirs {
        let gt: Value = serde_json::from_slice(&std::fs::read(dir.join("ground_truth.json"))?)?;
        let case = gt["case_id"].as_str().unwrap_or_default().to_string();
        let pdf = dir.join(gt["pdf"].as_str().unwrap_or_default());
        let height = gt["page_size"][1].as_f64().unwrap_or(842.0);
        let probes = gt["probes"].as_array().cloned().unwrap_or_default();
        let mut args: Vec<String> = vec!["redact".into(), pdf.display().to_string()];
        let mut values = Vec::new();
        let mut distractors = Vec::new();
        for p in &probes {
            let value = p["value"].as_str().unwrap_or_default().to_string();
            if p["must_redact"] != Value::Bool(true) {
                if p["channel"] == "text_layer" && !value.is_empty() {
                    distractors.push(value);
                }
                continue;
            }
            if p["channel"] == "image_text" {
                let b = &p["bbox"];
                let page = b["page"].as_i64().unwrap_or(0) + 1;
                let (x0, y0, x1, y1) = (
                    b["x0"].as_f64().unwrap_or(0.0),
                    b["y0"].as_f64().unwrap_or(0.0),
                    b["x1"].as_f64().unwrap_or(0.0),
                    b["y1"].as_f64().unwrap_or(0.0),
                );
                args.push("--region".into());
                args.push(format!(
                    "p{page}:[{x0},{},{x1},{}]",
                    height - y1,
                    height - y0
                ));
            } else if !value.is_empty() {
                args.push("--text".into());
                args.push(value.clone());
                values.push(value);
            }
        }
        values.sort();
        values.dedup();
        let out = scratch.join(format!("{case}.pdf"));
        let report = scratch.join(format!("{case}.json"));
        args.push("-o".into());
        args.push(out.display().to_string());
        let run = proc::run(nupdf, &args, TIMEOUT)?;
        let mut row = BenchRow {
            case: case.clone(),
            probes: probes.len(),
            redacted: exit_code(&run.status) == Some(0),
            nupdf_leaks: 0,
            mupdf_leaks: Vec::new(),
            pdftotext_leaks: Vec::new(),
            distractors_kept: 0,
            distractors: distractors.len(),
            note: String::new(),
        };
        if !row.redacted {
            row.note = String::from_utf8_lossy(&run.stderr)
                .chars()
                .take(200)
                .collect();
            rows.push(row);
            continue;
        }
        std::fs::write(&report, &run.stdout)?;
        let v = proc::run(
            nupdf,
            &[
                "verify".into(),
                out.display().to_string(),
                "--against".into(),
                report.display().to_string(),
            ],
            TIMEOUT,
        )?;
        if exit_code(&v.status) != Some(0) {
            let r: Option<Value> = serde_json::from_slice(&v.stdout).ok();
            row.nupdf_leaks = r
                .as_ref()
                .and_then(|r| r.pointer("/verification/classes"))
                .and_then(Value::as_object)
                .map_or(1, |c| {
                    c.values()
                        .map(|v| v["findings"].as_array().map_or(0, Vec::len))
                        .sum()
                });
        }
        let script = root.join("oracle/js/leaks.js");
        let mut margs = vec![
            "run".to_string(),
            script.display().to_string(),
            out.display().to_string(),
        ];
        margs.extend(values.iter().cloned());
        let m = proc::run(mutool, &margs, TIMEOUT)?;
        if let Ok(l) = serde_json::from_slice::<Value>(&m.stdout) {
            row.mupdf_leaks = l["found"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        } else {
            row.note = "MuPDF could not read the output".into();
        }
        if let Some(p) = &pdftotext {
            let t = proc::run(p, &[out.as_os_str(), "-".as_ref()], TIMEOUT)?;
            let text = String::from_utf8_lossy(&t.stdout)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            row.pdftotext_leaks = values
                .iter()
                .filter(|v| text.contains(v.as_str()))
                .cloned()
                .collect();
            row.distractors_kept = distractors
                .iter()
                .filter(|d| text.contains(d.as_str()))
                .count();
        }
        rows.push(row);
    }
    println!(
        "| Case | Probes | Redacted | nupdf verify | MuPDF | pdftotext | Distractors kept | Note |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    let mut bad = 0;
    for r in &rows {
        let ok = r.redacted
            && r.nupdf_leaks == 0
            && r.mupdf_leaks.is_empty()
            && r.pdftotext_leaks.is_empty();
        if !ok {
            bad += 1;
        }
        let list = |v: &[String]| {
            if v.is_empty() {
                "clean".to_string()
            } else {
                format!("LEAK {v:?}")
            }
        };
        println!(
            "| `{}` | {} | {} | {} | {} | {} | {}/{} | {} |",
            r.case,
            r.probes,
            if r.redacted { "yes" } else { "FAILED" },
            if r.nupdf_leaks == 0 {
                "clean".into()
            } else {
                format!("LEAK ({})", r.nupdf_leaks)
            },
            list(&r.mupdf_leaks),
            list(&r.pdftotext_leaks),
            r.distractors_kept,
            r.distractors,
            r.note
        );
    }
    let _ = std::fs::remove_dir_all(&scratch);
    if bad > 0 {
        bail!("{bad} of {} cases leak or fail", rows.len());
    }
    println!("\nzero leaks in {} cases", rows.len());
    Ok(())
}
