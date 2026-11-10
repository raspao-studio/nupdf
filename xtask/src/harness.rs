// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Differential harness. Each check runs an oracle script under `mutool run`
//! and the matching `nupdf` command on every file in a corpus tier, compares
//! the JSON they print, and ratchets the results against a committed baseline.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::proc::{self, Status};
use crate::{corpus, oracle};

#[derive(Debug)]
pub struct Check {
    pub name: &'static str,
    pub about: &'static str,
    /// Script under oracle/js, run as `mutool run <script> <file>`.
    pub oracle_script: &'static str,
    /// `nupdf` arguments; the file path is appended.
    pub nupdf_args: &'static [&'static str],
    pub compare: Compare,
}

#[derive(Debug, Clone, Copy)]
pub enum Compare {
    /// These JSON pointers must be equal.
    Fields(&'static [&'static str]),
    /// The `counts` must be equal, then every entry of the `items`
    /// collection (an object or an array), reported item by item.
    Collection {
        counts: &'static [&'static str],
        items: &'static str,
    },
    /// The distinct warnings and reported errors on MuPDF's stderr must
    /// equal the distinct `/diagnostics/*/message` values.
    Warnings,
}

pub const CHECKS: &[Check] = &[
    Check {
        name: "info",
        about: "document-level facts: effective version, encryption, page count, xref size, repair",
        oracle_script: "oracle/js/info.js",
        nupdf_args: &["info"],
        compare: Compare::Fields(&[
            "/pdf_version",
            "/encryption",
            "/needs_password",
            "/page_count",
            "/object_count",
            "/repaired",
            "/revisions",
            "/metadata",
        ]),
    },
    Check {
        name: "objects",
        about: "every object in the xref, canonically encoded, with raw stream lengths",
        oracle_script: "oracle/js/objects.js",
        nupdf_args: &["objects"],
        compare: Compare::Collection {
            counts: &["/object_count"],
            items: "objects",
        },
    },
    Check {
        name: "pages",
        about: "page tree lookup and inherited MediaBox, CropBox, Rotate, Resources",
        oracle_script: "oracle/js/pages.js",
        nupdf_args: &["pages"],
        compare: Compare::Collection {
            counts: &["/page_count", "/page_count_after"],
            items: "pages",
        },
    },
    Check {
        name: "warnings",
        about: "distinct warnings and recovered errors while opening and inspecting a file",
        oracle_script: "oracle/js/info.js",
        nupdf_args: &["info"],
        compare: Compare::Warnings,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Both succeeded and every compared field is equal.
    Match,
    /// Both succeeded but some field differs.
    Mismatch,
    /// The oracle failed and nupdf did not. Could be better or worse; look.
    OracleFailed,
    /// nupdf failed on a file the oracle handles.
    NupdfFailed,
    /// Both rejected the file.
    BothFailed,
    Timeout,
    /// A process died from a signal. For nupdf this is always a bug.
    Crash,
}

#[derive(Debug, Serialize)]
pub struct FieldDiff {
    pub field: String,
    pub oracle: Value,
    pub nupdf: Value,
}

#[derive(Debug, Serialize)]
pub struct FileResult {
    pub file: String,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub diffs: Vec<FieldDiff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// MuPDF's warnings, kept so later milestones can compare diagnostics.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub oracle_warnings: Vec<String>,
    pub oracle_ms: f64,
    pub nupdf_ms: f64,
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Baseline {
    pub oracle: String,
    pub checks: BTreeMap<String, BTreeMap<String, Outcome>>,
}

struct Side {
    status: Status,
    json: Option<Value>,
    stderr: String,
    ms: f64,
}

pub fn run(
    root: &Path,
    tier: &str,
    only: Option<&str>,
    update_baseline: bool,
    timeout: Duration,
) -> Result<()> {
    let mutool = oracle::mutool(root)?;
    let oracle_version = oracle::version(root)?;
    let nupdf = crate::build_nupdf(root)?;
    let files = corpus::files(root, tier)?;
    if files.is_empty() {
        bail!("tier {tier} has no files; see `cargo xtask corpus list`");
    }
    let checks: Vec<&Check> = CHECKS
        .iter()
        .filter(|c| only.is_none_or(|name| c.name == name))
        .collect();
    if checks.is_empty() {
        bail!("no check named {}", only.unwrap_or_default());
    }

    let out_dir = root.join("target/harness").join(tier);
    std::fs::create_dir_all(&out_dir)?;
    let mut current = Baseline {
        oracle: oracle_version.clone(),
        ..Baseline::default()
    };
    let mut summary = format!(
        "# Harness: tier `{tier}`, oracle MuPDF {oracle_version}\n\n\
         | Check | Files | Match | Mismatch | nupdf failed | Oracle failed | Both failed | Timeout | Crash | Parity |\n\
         |---|---|---|---|---|---|---|---|---|---|\n"
    );

    let mut notes = String::from("\n");
    for check in &checks {
        let _ = writeln!(notes, "- `{}`: {}", check.name, check.about);
        let results = run_check(root, check, &mutool, &nupdf, &files, timeout);
        std::fs::write(
            out_dir.join(format!("{}.json", check.name)),
            serde_json::to_string_pretty(&results)?,
        )?;
        summary.push_str(&summary_row(check.name, &results));
        current.checks.insert(
            check.name.to_string(),
            results
                .iter()
                .map(|r| (r.file.clone(), r.outcome))
                .collect(),
        );
        for r in results.iter().filter(|r| r.outcome != Outcome::Match) {
            let _ = writeln!(
                notes,
                "- `{}` {}: {:?}{}",
                check.name,
                r.file,
                r.outcome,
                describe(r)
            );
        }
    }

    summary.push_str(&notes);
    std::fs::write(out_dir.join("summary.md"), &summary)?;
    println!("{summary}");
    println!("full results: {}", out_dir.display());

    let baseline_path = root.join("harness/baselines").join(format!("{tier}.json"));
    if update_baseline {
        let mut baseline = load_baseline(&baseline_path)?.unwrap_or_default();
        baseline.oracle = current.oracle.clone();
        // Only the checks that ran are replaced.
        baseline.checks.extend(current.checks);
        std::fs::create_dir_all(baseline_path.parent().unwrap_or(root))?;
        std::fs::write(
            &baseline_path,
            serde_json::to_string_pretty(&baseline)? + "\n",
        )?;
        println!("baseline updated: {}", baseline_path.display());
        return Ok(());
    }
    match load_baseline(&baseline_path)? {
        None => println!("no baseline for tier {tier}; create one with --update-baseline"),
        Some(baseline) => ratchet(&baseline, &current)?,
    }
    Ok(())
}

fn run_check(
    root: &Path,
    check: &Check,
    mutool: &Path,
    nupdf: &Path,
    files: &[corpus::CorpusFile],
    timeout: Duration,
) -> Vec<FileResult> {
    let script = root.join(check.oracle_script);
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(files.len()));
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some(file) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let oracle = side(
                        mutool,
                        &[
                            std::ffi::OsStr::new("run"),
                            script.as_os_str(),
                            file.path.as_os_str(),
                        ],
                        timeout,
                    );
                    let mut args: Vec<&std::ffi::OsStr> =
                        check.nupdf_args.iter().map(std::ffi::OsStr::new).collect();
                    args.push(file.path.as_os_str());
                    let ours = side(nupdf, &args, timeout);
                    let result = judge(check, &file.name, oracle, ours);
                    if let Ok(mut results) = results.lock() {
                        results.push(result);
                    }
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap_or_default();
    results.sort_by(|a, b| a.file.cmp(&b.file));
    results
}

fn side(program: &Path, args: &[&std::ffi::OsStr], timeout: Duration) -> Side {
    match proc::run(program, args, timeout) {
        Ok(run) => Side {
            status: run.status,
            // MuPDF's text conversion can emit bytes that are not UTF-8
            // (an overlong NUL, lone surrogates); read both sides lossily.
            json: run
                .succeeded()
                .then(|| serde_json::from_str(&String::from_utf8_lossy(&run.stdout)).ok())
                .flatten(),
            stderr: String::from_utf8_lossy(&run.stderr).into_owned(),
            ms: run.wall.as_secs_f64() * 1000.0,
        },
        Err(e) => Side {
            status: Status::Exited(-1),
            json: None,
            stderr: format!("could not run: {e:#}"),
            ms: 0.0,
        },
    }
}

fn judge(check: &Check, file: &str, oracle: Side, ours: Side) -> FileResult {
    let oracle_warnings = oracle
        .stderr
        .lines()
        .filter_map(|l| l.strip_prefix("warning: "))
        .map(str::to_string)
        .collect();
    let mut result = FileResult {
        file: file.to_string(),
        outcome: Outcome::Match,
        diffs: Vec::new(),
        detail: None,
        oracle_warnings,
        oracle_ms: oracle.ms,
        nupdf_ms: ours.ms,
    };
    let failure = |side: &Side, who: &str| -> Option<String> {
        if side.json.is_some() {
            return None;
        }
        let last = side.stderr.lines().last().unwrap_or("").trim().to_string();
        Some(format!("{who}: {:?} {last}", side.status))
    };

    let crashed = |s: &Side| matches!(s.status, Status::Signaled(_));
    let timed_out = |s: &Side| s.status == Status::TimedOut;
    result.outcome = if crashed(&oracle) || crashed(&ours) {
        Outcome::Crash
    } else if timed_out(&oracle) || timed_out(&ours) {
        Outcome::Timeout
    } else {
        match (&oracle.json, &ours.json) {
            (None, None) => Outcome::BothFailed,
            (None, Some(_)) => Outcome::OracleFailed,
            (Some(_), None) => Outcome::NupdfFailed,
            (Some(a), Some(b)) => {
                result.diffs = match check.compare {
                    Compare::Fields(fields) => compare(fields, a, b),
                    Compare::Collection { counts, items } => {
                        compare_collection(counts, items, a, b)
                    }
                    Compare::Warnings => compare_warnings(&oracle.stderr, b),
                };
                if result.diffs.is_empty() {
                    Outcome::Match
                } else {
                    Outcome::Mismatch
                }
            }
        }
    };
    let details: Vec<String> = [failure(&oracle, "oracle"), failure(&ours, "nupdf")]
        .into_iter()
        .flatten()
        .collect();
    if !details.is_empty() {
        result.detail = Some(details.join("; "));
    }
    result
}

pub fn compare(fields: &[&str], oracle: &Value, ours: &Value) -> Vec<FieldDiff> {
    fields
        .iter()
        .filter_map(|&field| {
            let a = oracle.pointer(field).cloned().unwrap_or(Value::Null);
            let b = ours.pointer(field).cloned().unwrap_or(Value::Null);
            (!json_eq(&a, &b)).then(|| FieldDiff {
                field: field.to_string(),
                oracle: a,
                nupdf: b,
            })
        })
        .collect()
}

/// MuPDF's stderr lines that are diagnostics: `warning: X` and caught
/// errors reported as `<kind> error: X`. Its "... repeated N times" lines
/// are a printing artifact, not diagnostics.
pub fn mupdf_diagnostics(stderr: &str) -> std::collections::BTreeSet<String> {
    const KINDS: [&str; 8] = [
        "format",
        "syntax",
        "generic",
        "library",
        "limit",
        "argument",
        "system",
        "unsupported",
    ];
    stderr
        .lines()
        .filter_map(|line| {
            if let Some(w) = line.strip_prefix("warning: ") {
                return (!w.starts_with("... repeated")).then(|| w.to_string());
            }
            KINDS
                .iter()
                .any(|k| line.starts_with(&format!("{k} error: ")))
                .then(|| line.to_string())
        })
        .collect()
}

pub fn compare_warnings(oracle_stderr: &str, ours: &Value) -> Vec<FieldDiff> {
    let theirs = mupdf_diagnostics(oracle_stderr);
    let mine: std::collections::BTreeSet<String> = ours
        .get("diagnostics")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|d| d.get("message").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let only = |a: &std::collections::BTreeSet<String>, b: &std::collections::BTreeSet<String>| {
        Value::from(a.difference(b).cloned().collect::<Vec<_>>())
    };
    if theirs == mine {
        Vec::new()
    } else {
        vec![FieldDiff {
            field: "/diagnostics".into(),
            oracle: only(&theirs, &mine),
            nupdf: only(&mine, &theirs),
        }]
    }
}

/// Item-by-item comparison of a collection; reports the first few
/// differences.
pub fn compare_collection(
    counts: &[&str],
    items: &str,
    oracle: &Value,
    ours: &Value,
) -> Vec<FieldDiff> {
    const MAX_REPORTED: usize = 5;
    let mut diffs = compare(counts, oracle, ours);
    let as_map = |v: &Value| -> serde_json::Map<String, Value> {
        match v.get(items) {
            Some(Value::Object(m)) => m.clone(),
            Some(Value::Array(list)) => list
                .iter()
                .enumerate()
                .map(|(i, v)| (i.to_string(), v.clone()))
                .collect(),
            _ => serde_json::Map::new(),
        }
    };
    let (a, b) = (as_map(oracle), as_map(ours));
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort_by_key(|k| k.parse::<u64>().unwrap_or(u64::MAX));
    keys.dedup();
    let mut differing = 0;
    for key in keys {
        let x = a.get(key).unwrap_or(&Value::Null);
        let y = b.get(key).unwrap_or(&Value::Null);
        if !json_eq(x, y) {
            differing += 1;
            if diffs.len() < MAX_REPORTED {
                diffs.push(FieldDiff {
                    field: format!("/{items}/{key}"),
                    oracle: truncate(x),
                    nupdf: truncate(y),
                });
            }
        }
    }
    if differing > MAX_REPORTED {
        diffs.push(FieldDiff {
            field: format!("/{items} (more)"),
            oracle: Value::Null,
            nupdf: Value::from(format!("{differing} {items} differ in total")),
        });
    }
    diffs
}

fn truncate(v: &Value) -> Value {
    let text = v.to_string();
    if text.len() <= 400 {
        return v.clone();
    }
    let mut cut = 400;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Value::from(format!("{}...", &text[..cut]))
}

/// JSON equality where numbers compare by value, so 1 and 1.0 are equal.
/// Non-integers compare at f32 precision: MuPDF stores reals as floats and
/// mujs prints only 16 significant digits of them.
pub fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (Some(p), Some(q)) = (x.as_f64(), y.as_f64()) else {
                return false;
            };
            // Integers beyond 2^53 come from JS with only 16 digits.
            let big = p.abs() > 9.007_199_254_740_992e15;
            p == q
                || ((x.is_f64() || y.is_f64()) && p as f32 == q as f32)
                || (big && (p - q).abs() <= p.abs() * 1e-15)
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_eq(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

fn summary_row(name: &str, results: &[FileResult]) -> String {
    let count = |o: Outcome| results.iter().filter(|r| r.outcome == o).count();
    let matched = count(Outcome::Match);
    // Parity counts the files the oracle handled.
    let oracle_ok = matched + count(Outcome::Mismatch) + count(Outcome::NupdfFailed);
    let parity = if oracle_ok == 0 {
        "n/a".to_string()
    } else {
        format!("{:.1}%", 100.0 * matched as f64 / oracle_ok as f64)
    };
    format!(
        "| {name} | {} | {matched} | {} | {} | {} | {} | {} | {} | {parity} |\n",
        results.len(),
        count(Outcome::Mismatch),
        count(Outcome::NupdfFailed),
        count(Outcome::OracleFailed),
        count(Outcome::BothFailed),
        count(Outcome::Timeout),
        count(Outcome::Crash),
    )
}

fn describe(r: &FileResult) -> String {
    let mut parts: Vec<String> = r
        .diffs
        .iter()
        .map(|d| format!("{} oracle={} nupdf={}", d.field, d.oracle, d.nupdf))
        .collect();
    parts.extend(r.detail.clone());
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join("; "))
    }
}

fn load_baseline(path: &PathBuf) -> Result<Option<Baseline>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Fails when a file that matched in the baseline no longer does.
pub fn ratchet(baseline: &Baseline, current: &Baseline) -> Result<()> {
    if baseline.oracle != current.oracle {
        bail!(
            "baseline was recorded with MuPDF {}, oracle is {}; re-baseline with --update-baseline",
            baseline.oracle,
            current.oracle
        );
    }
    let mut regressions = Vec::new();
    let mut improvements = 0;
    for (check, now) in &current.checks {
        let Some(before) = baseline.checks.get(check) else {
            continue;
        };
        for (file, outcome) in now {
            match before.get(file) {
                Some(Outcome::Match) if *outcome != Outcome::Match => {
                    regressions.push(format!("{check} {file}: match -> {outcome:?}"));
                }
                Some(previous) if *previous != Outcome::Match && *outcome == Outcome::Match => {
                    improvements += 1;
                }
                _ => {}
            }
        }
    }
    if improvements > 0 {
        println!("{improvements} file(s) newly match; run with --update-baseline to lock them in");
    }
    if !regressions.is_empty() {
        bail!("parity regressed:\n  {}", regressions.join("\n  "));
    }
    println!("no regressions against baseline");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn side(status: Status, json: Option<Value>) -> Side {
        Side {
            status,
            json,
            stderr: String::new(),
            ms: 1.0,
        }
    }

    #[test]
    fn compares_only_declared_fields() {
        let diffs = compare(
            &["/pdf_version"],
            &json!({"pdf_version": "1.7", "page_count": 3}),
            &json!({"pdf_version": "1.7", "doc_id": "b3:00"}),
        );
        assert!(diffs.is_empty());
        let diffs = compare(
            &["/pdf_version"],
            &json!({"pdf_version": "1.7"}),
            &json!({}),
        );
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].nupdf, Value::Null);
    }

    #[test]
    fn mupdf_stderr_becomes_a_diagnostic_set() {
        let set = mupdf_diagnostics(
            "format error: cannot find startxref\nwarning: trying to repair broken xref\n\
             warning: ... repeated 3 times...\nwarning: trying to repair broken xref\nnoise\n",
        );
        assert_eq!(
            set.into_iter().collect::<Vec<_>>(),
            vec![
                "format error: cannot find startxref",
                "trying to repair broken xref"
            ]
        );
    }

    #[test]
    fn numbers_compare_by_value() {
        assert!(json_eq(&json!({"r": 1}), &json!({"r": 1.0})));
        assert!(json_eq(
            &json!(1.200000047683716),
            &json!(1.2000000476837158)
        ));
        assert!(!json_eq(&json!(16777217), &json!(16777216)));
        assert!(json_eq(
            &json!(-8546744073709553000i64),
            &json!(-8546744073709551616i64)
        ));
        assert!(!json_eq(&json!([1, 2]), &json!([1, 3])));
    }

    #[test]
    fn object_diffs_name_the_objects() {
        let a = json!({"object_count": 3, "objects": {"1": 5, "2": {"n": "A"}}});
        let b = json!({"object_count": 3, "objects": {"1": 5.0, "2": {"n": "B"}}});
        let diffs = compare_collection(&["/object_count"], "objects", &a, &b);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].field, "/objects/2");
    }

    #[test]
    fn classifies_outcomes() {
        let check = &CHECKS[0];
        let ok = |v: &str| side(Status::Exited(0), Some(json!({"pdf_version": v})));
        let failed = || side(Status::Exited(3), None);
        let judge = |a, b| super::judge(check, "f.pdf", a, b).outcome;

        assert_eq!(judge(ok("1.7"), ok("1.7")), Outcome::Match);
        assert_eq!(judge(ok("1.7"), ok("1.4")), Outcome::Mismatch);
        assert_eq!(judge(ok("1.7"), failed()), Outcome::NupdfFailed);
        assert_eq!(judge(failed(), ok("1.7")), Outcome::OracleFailed);
        assert_eq!(judge(failed(), failed()), Outcome::BothFailed);
        assert_eq!(
            judge(ok("1.7"), side(Status::Signaled(11), None)),
            Outcome::Crash
        );
        assert_eq!(
            judge(side(Status::TimedOut, None), ok("1.7")),
            Outcome::Timeout
        );
    }

    #[test]
    fn collects_oracle_warnings() {
        let mut oracle = side(Status::Exited(0), Some(json!({"pdf_version": "1.4"})));
        oracle.stderr =
            "format error: cannot find startxref\nwarning: trying to repair broken xref\n".into();
        let result = super::judge(
            &CHECKS[0],
            "f.pdf",
            oracle,
            side(Status::Exited(0), Some(json!({"pdf_version": "1.4"}))),
        );
        assert_eq!(result.oracle_warnings, vec!["trying to repair broken xref"]);
    }

    fn baseline(entries: &[(&str, Outcome)]) -> Baseline {
        let files = entries.iter().map(|(f, o)| (f.to_string(), *o)).collect();
        Baseline {
            oracle: "1.28.6".into(),
            checks: BTreeMap::from([("info".to_string(), files)]),
        }
    }

    #[test]
    fn ratchet_rejects_lost_matches_and_accepts_gains() {
        let before = baseline(&[("a.pdf", Outcome::Match), ("b.pdf", Outcome::Mismatch)]);
        let gained = baseline(&[("a.pdf", Outcome::Match), ("b.pdf", Outcome::Match)]);
        assert!(ratchet(&before, &gained).is_ok());
        let lost = baseline(&[
            ("a.pdf", Outcome::NupdfFailed),
            ("b.pdf", Outcome::Mismatch),
        ]);
        assert!(ratchet(&before, &lost).is_err());
    }

    #[test]
    fn ratchet_refuses_a_different_oracle() {
        let before = baseline(&[("a.pdf", Outcome::Match)]);
        let mut after = baseline(&[("a.pdf", Outcome::Match)]);
        after.oracle = "1.29.0".into();
        assert!(ratchet(&before, &after).is_err());
    }
}
