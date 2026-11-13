// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `cargo xtask bench --against engines`: nupdf's library against PDF
//! Oxide's, Rust to Rust, timed in process by `bench/engines` (a crate of its
//! own, outside the workspace, so no product crate depends on pdf_oxide).
//! Each file, operation and library runs in a process of its own, so that a
//! crash, an abort or a hang in one is that file's failure and nothing
//! else's; what is timed inside it is only the library's work on bytes
//! already in memory.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::against::Check;
use crate::bench::{geomean, sample_evenly};
use crate::{corpus, proc};

/// The tool `--against` takes.
pub const ENGINES: &str = "engines";

pub struct Op {
    pub name: &'static str,
    pub check: Check,
    pub units: &'static str,
    pub about: &'static str,
}

pub const OPS: &[Op] = &[
    Op {
        name: "open",
        check: Check::Equal,
        units: "pages",
        about: "open the document from memory and count its pages: `Document::open` and `page_count`, `PdfDocument::from_bytes` and `page_count`",
    },
    Op {
        name: "pages",
        check: Check::Equal,
        units: "pages",
        about: "each page's MediaBox and Rotate: `page` and `inherited`, `get_page_media_box` and `get_page_rotation`",
    },
    Op {
        name: "text",
        check: Check::NonEmpty,
        units: "non-whitespace characters",
        about: "every page's text into one String: `parallel::extract_pages` on one thread and `StextPage::text`, `extract_text` (which looks for tables)",
    },
    Op {
        name: "text-fast",
        check: Check::NonEmpty,
        units: "non-whitespace characters",
        about: "as `text`, with PDF Oxide's speed-first `extract_tables: false`; nupdf has no such mode and does the same as for `text`",
    },
    Op {
        name: "render",
        check: Check::Equal,
        units: "pages with ink",
        about: "every page at 72 dpi to a pixmap in memory, no encoding, no annotations: `draw::render_page_with` through `parallel::run_pages` on one thread (RGB, ICC on, as the CLI's default), `PageRenderer::render_page` with `RawRgba8` and `render_annotations: false` (premultiplied RGBA, ICC through qcms, its default)",
    },
    Op {
        name: "tables",
        check: Check::Count,
        units: "tables",
        about: "every page's tables: structured text with table hunting and vectors, as `nupdf tables`; `extract_tables`",
    },
    Op {
        name: "annots",
        check: Check::Count,
        units: "annotations other than links, popups and widgets",
        about: "every page's annotations with their type, contents, rectangle and author: `annot::page_annots` and its getters, `get_annotations`",
    },
];

/// What `nupdf-engines-bench` prints.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Outcome {
    pub ok: bool,
    #[serde(default)]
    pub ms: f64,
    #[serde(default)]
    pub units: u64,
    #[serde(default)]
    pub pages: u64,
    #[serde(default)]
    pub error: Option<String>,
}

/// The outcome on the last line of `stdout`, or why there is none.
pub fn parse_outcome(run: &proc::Run) -> std::result::Result<Outcome, String> {
    match run.status {
        proc::Status::TimedOut => return Err("timeout".into()),
        proc::Status::Signaled(s) => return Err(format!("killed by signal {s}")),
        proc::Status::Exited(_) => {}
    }
    let text = String::from_utf8_lossy(&run.stdout);
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .ok_or("no output")?;
    let out: Outcome = serde_json::from_str(line).map_err(|e| e.to_string())?;
    if out.ok {
        Ok(out)
    } else {
        Err(out.error.unwrap_or_else(|| "failed".into()))
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Row {
    pub file: String,
    /// Median in-process times in ms; `None` where that library failed.
    pub nupdf_ms: Option<f64>,
    pub oxide_ms: Option<f64>,
    pub nupdf_units: Option<u64>,
    pub oxide_units: Option<u64>,
    pub nupdf_error: Option<String>,
    pub oxide_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub files: usize,
    pub failed_nupdf: usize,
    pub failed_oxide: usize,
    /// Files both did on which their outputs disagree.
    pub disagree: usize,
    /// Files in the ratios: both did them and agreed.
    pub counted: usize,
    /// PDF Oxide's time over nupdf's: the geometric mean, and the ratio of
    /// the totals. Above 1, nupdf is faster.
    pub ratio: f64,
    pub total_ratio: f64,
    pub nupdf_total: f64,
    pub oxide_total: f64,
    /// Output totals over the files both did.
    pub nupdf_units: u64,
    pub oxide_units: u64,
}

pub fn summarize(check: Check, rows: &[Row]) -> Summary {
    let mut s = Summary {
        files: rows.len(),
        ..Summary::default()
    };
    let mut counted = Vec::new();
    for r in rows {
        s.failed_nupdf += usize::from(r.nupdf_ms.is_none());
        s.failed_oxide += usize::from(r.oxide_ms.is_none());
        let (Some(n), Some(o)) = (r.nupdf_ms, r.oxide_ms) else {
            continue;
        };
        s.nupdf_units += r.nupdf_units.unwrap_or(0);
        s.oxide_units += r.oxide_units.unwrap_or(0);
        if check.agrees(r.nupdf_units, r.oxide_units) {
            counted.push((n, o));
        } else {
            s.disagree += 1;
        }
    }
    s.counted = counted.len();
    // A run under a microsecond is the clock's grain, not the work.
    let floor = |t: f64| t.max(0.001);
    s.ratio = geomean(counted.iter().map(|&(n, o)| floor(o) / floor(n)));
    s.nupdf_total = counted.iter().map(|c| c.0).sum();
    s.oxide_total = counted.iter().map(|c| c.1).sum();
    s.total_ratio = if counted.is_empty() {
        f64::NAN
    } else {
        s.oxide_total / floor(s.nupdf_total)
    };
    s
}

fn times(ratio: f64) -> String {
    if ratio.is_nan() {
        "-".into()
    } else {
        format!("{ratio:.2}x")
    }
}

fn ms(v: Option<f64>) -> String {
    v.map_or("fail".into(), |v| format!("{v:.3}"))
}

fn count(v: Option<u64>) -> String {
    v.map_or("-".into(), |v| v.to_string())
}

pub fn summary_table(results: &[(&Op, Summary)]) -> String {
    let mut out = String::from(
        "| Operation | Files | PDF Oxide / nupdf | Of totals | Files in them | nupdf total ms | PDF Oxide total ms | \
         Failed: nupdf | PDF Oxide | Disagree | Units: nupdf / PDF Oxide |\n\
         |---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for (op, s) in results {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {:.1} | {:.1} | {} | {} | {} | {} / {} |",
            op.name,
            s.files,
            times(s.ratio),
            times(s.total_ratio),
            s.counted,
            s.nupdf_total,
            s.oxide_total,
            s.failed_nupdf,
            s.failed_oxide,
            if op.check == Check::Count {
                "-".to_string()
            } else {
                s.disagree.to_string()
            },
            s.nupdf_units,
            s.oxide_units,
        );
    }
    out.push_str(
        "\nIn-process library time, the median of the timed runs, from opening the document \
         in memory to the last page: PDF Oxide's over nupdf's, the geometric mean and the ratio \
         of the totals, over the files both did and, where checked, agree on. Above 1x, nupdf \
         is faster.\n",
    );
    out
}

pub fn render(op: &Op, tier: &str, runs: usize, rows: &[Row]) -> String {
    let mut out = format!(
        "# Engines `{}` on `{tier}` (median of {runs} runs)\n\n{}.\n\nTimes in ms, in process; \
         `fail` where a library failed. Units: {}.\n\n\
         | File | nupdf | PDF Oxide | nupdf units | PDF Oxide units | |\n|---|---|---|---|---|---|\n",
        op.name, op.about, op.units
    );
    for r in rows {
        let both = r.nupdf_ms.is_some() && r.oxide_ms.is_some();
        let note = if both && !op.check.agrees(r.nupdf_units, r.oxide_units) {
            "disagree".to_string()
        } else {
            [&r.nupdf_error, &r.oxide_error]
                .iter()
                .zip(["nupdf", "PDF Oxide"])
                .filter_map(|(e, who)| e.as_ref().map(|e| format!("{who}: {}", one_line(e))))
                .collect::<Vec<_>>()
                .join("; ")
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {note} |",
            r.file,
            ms(r.nupdf_ms),
            ms(r.oxide_ms),
            count(r.nupdf_units),
            count(r.oxide_units),
        );
    }
    let _ = write!(
        out,
        "\n{}",
        summary_table(&[(op, summarize(op.check, rows))])
    );
    out
}

/// An error message fit for a table cell.
fn one_line(e: &str) -> String {
    let e: String = e.replace(['|', '\n'], " ");
    match e.char_indices().nth(120) {
        Some((i, _)) => format!("{}...", &e[..i]),
        None => e,
    }
}

fn build(root: &Path) -> Result<PathBuf> {
    let dir = root.join("bench/engines");
    let status = Command::new(env!("CARGO"))
        .current_dir(&dir)
        .args(["build", "--quiet", "--release", "--locked"])
        .status()
        .context("running cargo build in bench/engines")?;
    if !status.success() {
        bail!("cargo build --release in bench/engines failed");
    }
    Ok(dir.join("target/release/nupdf-engines-bench"))
}

pub fn run(
    root: &Path,
    tier: &str,
    only: Option<&str>,
    runs: usize,
    sample: Option<usize>,
    all: &[corpus::CorpusFile],
) -> Result<()> {
    let ops: Vec<&Op> = OPS
        .iter()
        .filter(|o| only.is_none_or(|n| o.name == n))
        .collect();
    if ops.is_empty() {
        bail!(
            "no engines operation named {}; there are: {}",
            only.unwrap_or_default(),
            OPS.iter().map(|o| o.name).collect::<Vec<_>>().join(", ")
        );
    }
    let bin = build(root)?;
    let runs = runs.max(1);
    let out_dir = root.join("target/bench").join(tier).join("engines");
    std::fs::create_dir_all(&out_dir)?;
    let files = sample_evenly(all.iter().collect(), sample);
    let timeout = Duration::from_secs(600);
    let mut results = Vec::new();
    for op in ops {
        let mut rows = Vec::new();
        for file in &files {
            let mut row = Row {
                file: file.name.clone(),
                ..Row::default()
            };
            for lib in ["nupdf", "pdf_oxide"] {
                let args: Vec<OsString> = vec![
                    op.name.into(),
                    lib.into(),
                    file.path.clone().into_os_string(),
                    runs.to_string().into(),
                ];
                let outcome = proc::run(&bin, &args, timeout)
                    .map_err(|e| e.to_string())
                    .and_then(|r| parse_outcome(&r));
                let (ms, units, error) = match outcome {
                    Ok(o) => (Some(o.ms), Some(o.units), None),
                    Err(e) => (None, None, Some(e)),
                };
                if lib == "nupdf" {
                    (row.nupdf_ms, row.nupdf_units, row.nupdf_error) = (ms, units, error);
                } else {
                    (row.oxide_ms, row.oxide_units, row.oxide_error) = (ms, units, error);
                }
            }
            rows.push(row);
        }
        let report = render(op, tier, runs, &rows);
        std::fs::write(
            out_dir.join(format!("{}.json", op.name)),
            serde_json::to_string_pretty(&rows)?,
        )?;
        std::fs::write(out_dir.join(format!("{}.md", op.name)), &report)?;
        println!("{report}");
        results.push((op, summarize(op.check, &rows)));
    }
    let summary = format!(
        "# nupdf and PDF Oxide, library to library, on `{tier}` ({} files, median of {runs} runs)\n\n\
         nupdf from this tree; pdf_oxide 0.3.78 (features `rendering`, `logging` and its defaults); \
         both built with the root workspace's release profile (`bench/engines`).\n\n{}",
        files.len(),
        summary_table(&results)
    );
    std::fs::write(out_dir.join("summary.md"), &summary)?;
    println!("{summary}");
    println!("results: {}", out_dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exited(stdout: &str) -> proc::Run {
        proc::Run {
            status: proc::Status::Exited(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
            wall: Duration::ZERO,
            max_rss_bytes: 0,
        }
    }

    #[test]
    fn outcomes_come_from_the_last_line_and_failures_say_why() {
        let ok = exited("a warning\n{\"ok\":true,\"ms\":1.5,\"units\":7,\"pages\":2}\n");
        let o = parse_outcome(&ok).unwrap();
        assert_eq!((o.ms, o.units, o.pages), (1.5, 7, 2));
        let failed = exited("{\"ok\":false,\"error\":\"bad xref\"}");
        assert_eq!(parse_outcome(&failed).unwrap_err(), "bad xref");
        assert_eq!(parse_outcome(&exited("")).unwrap_err(), "no output");
        let mut killed = exited("");
        killed.status = proc::Status::Signaled(6);
        assert_eq!(parse_outcome(&killed).unwrap_err(), "killed by signal 6");
        killed.status = proc::Status::TimedOut;
        assert_eq!(parse_outcome(&killed).unwrap_err(), "timeout");
    }

    fn row(n: Option<f64>, o: Option<f64>, units: (u64, u64)) -> Row {
        Row {
            file: "f".into(),
            nupdf_ms: n,
            oxide_ms: o,
            nupdf_units: n.map(|_| units.0),
            oxide_units: o.map(|_| units.1),
            ..Row::default()
        }
    }

    #[test]
    fn ratios_leave_out_failures_and_disagreements() {
        let rows = [
            row(Some(1.0), Some(2.0), (3, 3)),
            row(Some(10.0), Some(80.0), (3, 3)),
            row(Some(1.0), None, (3, 0)),
            row(None, Some(1.0), (0, 3)),
            // Fast and empty: out of the ratio.
            row(Some(5.0), Some(0.1), (3, 0)),
        ];
        let s = summarize(Check::NonEmpty, &rows);
        assert_eq!(
            (
                s.files,
                s.counted,
                s.disagree,
                s.failed_nupdf,
                s.failed_oxide
            ),
            (5, 2, 1, 1, 1)
        );
        assert!((s.ratio - 4.0).abs() < 1e-9, "{s:?}");
        assert!((s.total_ratio - 82.0 / 11.0).abs() < 1e-9, "{s:?}");
        assert_eq!((s.nupdf_units, s.oxide_units), (9, 6));
        // Not checked: every file both did is in.
        assert_eq!(summarize(Check::Count, &rows).counted, 3);
        let none = summarize(Check::Equal, &rows[2..4]);
        assert!(none.ratio.is_nan() && none.total_ratio.is_nan());
    }

    #[test]
    fn reports_show_failures_with_their_reasons() {
        let op = OPS.iter().find(|o| o.name == "open").unwrap();
        let mut failed = row(Some(1.0), None, (2, 0));
        failed.oxide_error = Some("Invalid | PDF\nheader".into());
        let rows = [row(Some(1.0), Some(3.0), (2, 2)), failed];
        let out = render(op, "smoke", 3, &rows);
        assert!(out.contains("| f | 1.000 | 3.000 | 2 | 2 |  |"), "{out}");
        assert!(
            out.contains("| f | 1.000 | fail | 2 | - | PDF Oxide: Invalid   PDF header |"),
            "{out}"
        );
        assert!(out.contains("| open | 2 | 3.00x | 3.00x | 1 |"), "{out}");
    }

    #[test]
    fn long_errors_are_cut() {
        let long = "x".repeat(200);
        assert_eq!(one_line(&long).len(), 123);
        assert_eq!(one_line("short"), "short");
    }

    #[test]
    fn the_engines_crate_knows_every_operation() {
        let main = include_str!("../../bench/engines/src/main.rs");
        for op in OPS {
            assert!(main.contains(&format!("\"{}\"", op.name)), "{}", op.name);
        }
    }
}
