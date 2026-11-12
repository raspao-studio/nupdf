// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Paired benchmarks: the same task done by the oracle and by nupdf, timed
//! end to end as processes (what an agent actually pays), with peak RSS.
//! Runs are sequential to keep timings quiet.
//!
//! Run the scaling pairs on mains power: on battery, macOS holds cores back
//! under load from many threads, and independent processes slow down as much.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::{corpus, oracle, proc};

/// Which program a side of a pair runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Mutool,
    Nupdf,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::Mutool => "MuPDF",
            Tool::Nupdf => "nupdf",
        }
    }
}

#[derive(Debug)]
pub struct Pair {
    pub name: &'static str,
    pub about: &'static str,
    /// What the first side runs: MuPDF, or nupdf itself to measure one of
    /// its modes against another.
    pub baseline: Tool,
    /// Arguments to the baseline; `{file}` is replaced by the input path,
    /// `{out}` by a scratch output path and `{dir}` by a scratch directory
    /// (or, for a script, a prefix for the files it writes).
    pub oracle: &'static [&'static str],
    /// Arguments to `nupdf`, expanded the same way.
    pub nupdf: &'static [&'static str],
    /// Only files with at least this many pages.
    pub min_pages: usize,
    /// Only files on which the baseline takes at least this long: a file
    /// that is mostly starting the process shows nothing of what threads
    /// do.
    pub min_baseline_ms: f64,
}

const DEFAULT: Pair = Pair {
    name: "",
    about: "",
    baseline: Tool::Mutool,
    oracle: &[],
    nupdf: &[],
    min_pages: 0,
    min_baseline_ms: 0.0,
};

/// The threads the parallel pairs use: the 8 cores of M5's exit criterion.
const THREADS: &str = "8";

/// The single-thread time a scaling pair needs of a file to count it.
const SCALING_MIN_MS: f64 = 100.0;

pub const PAIRS: &[Pair] = &[
    Pair {
        name: "info",
        about: "open, read the effective version, count pages and xref slots (JS overhead included on the MuPDF side)",
        oracle: &["run", "oracle/js/info.js", "{file}"],
        nupdf: &["info", "{file}"],
        ..DEFAULT
    },
    Pair {
        name: "clean",
        about: "rewrite the document with `clean`'s defaults; nupdf also reads its output back to verify it",
        oracle: &["clean", "{file}", "{out}"],
        nupdf: &["clean", "{file}", "-o", "{out}"],
        ..DEFAULT
    },
    Pair {
        name: "clean-gggg",
        about: "rewrite the document with every garbage collection level; nupdf also reads its output back to verify it",
        oracle: &["clean", "-gggg", "{file}", "{out}"],
        nupdf: &["clean", "-gggg", "{file}", "-o", "{out}"],
        ..DEFAULT
    },
    Pair {
        name: "clean-z",
        about: "rewrite the document deflating every stream that has no filter; nupdf also reads its output back to verify it",
        oracle: &["clean", "-z", "{file}", "{out}"],
        nupdf: &["clean", "-z", "{file}", "-o", "{out}"],
        ..DEFAULT
    },
    Pair {
        name: "merge",
        about: "merge the document with itself into a new one: pages, links and outline; nupdf also reads its output back to verify it",
        oracle: &["merge", "-o", "{out}", "{file}", "{file}"],
        nupdf: &["merge", "-o", "{out}", "{file}", "{file}"],
        ..DEFAULT
    },
    Pair {
        name: "split",
        about: "write each page to a file of its own from a fresh copy of the input; MuPDF has no split command, so a script does it (JS overhead included); nupdf also reads each part back",
        oracle: &["run", "oracle/js/split.js", "{file}", "{dir}"],
        nupdf: &["split", "{file}", "-o", "{dir}"],
        ..DEFAULT
    },
    Pair {
        name: "stext",
        about: "structured text of every page as JSON, on one thread: `mutool draw -F stext.json` against `nupdf stext`",
        oracle: &["draw", "-q", "-F", "stext.json", "-o", "{out}", "{file}"],
        nupdf: &["stext", "{file}"],
        ..DEFAULT
    },
    Pair {
        name: "stext-threads",
        about: "structured text of every page as JSON, MuPDF on one thread against nupdf on 8",
        oracle: &["draw", "-q", "-F", "stext.json", "-o", "{out}", "{file}"],
        nupdf: &["stext", "--threads", THREADS, "{file}"],
        ..DEFAULT
    },
    Pair {
        name: "render",
        about: "every page at 72 dpi as PPM, color management off, on one thread: `mutool draw -N` (which also draws annotations) against `nupdf render`",
        oracle: &[
            "draw",
            "-q",
            "-N",
            "-r",
            "72",
            "-o",
            "{dir}/p%d.ppm",
            "{file}",
        ],
        nupdf: &["render", "{file}", "-o", "{dir}", "--format", "ppm"],
        ..DEFAULT
    },
    Pair {
        name: "render-threads",
        about: "every page at 72 dpi as PPM, MuPDF on one thread (the oracle is built without threads) against nupdf on 8",
        oracle: &[
            "draw",
            "-q",
            "-N",
            "-r",
            "72",
            "-o",
            "{dir}/p%d.ppm",
            "{file}",
        ],
        nupdf: &[
            "render",
            "{file}",
            "-o",
            "{dir}",
            "--format",
            "ppm",
            "--threads",
            THREADS,
        ],
        ..DEFAULT
    },
    Pair {
        name: "stext-scaling",
        about: "nupdf's structured text on 1 thread against 8, for documents of 50 pages or more that take 100 ms or more on one",
        baseline: Tool::Nupdf,
        oracle: &["stext", "--threads", "1", "{file}"],
        nupdf: &["stext", "--threads", THREADS, "{file}"],
        min_pages: 50,
        min_baseline_ms: SCALING_MIN_MS,
    },
    Pair {
        name: "extract-scaling",
        about: "nupdf's `extract` of every page as JSON on 1 thread against 8, for documents of 50 pages or more that take 100 ms or more on one",
        baseline: Tool::Nupdf,
        oracle: &["extract", "--threads", "1", "{file}"],
        nupdf: &["extract", "--threads", THREADS, "{file}"],
        min_pages: 50,
        min_baseline_ms: SCALING_MIN_MS,
    },
];

#[derive(Debug, Serialize)]
struct Row {
    file: String,
    oracle_ms: f64,
    nupdf_ms: f64,
    speedup: f64,
    oracle_rss_mb: f64,
    nupdf_rss_mb: f64,
}

pub fn run(
    root: &Path,
    tier: &str,
    only: Option<&str>,
    runs: usize,
    sample: Option<usize>,
) -> Result<()> {
    let mutool = oracle::mutool(root)?;
    let nupdf = crate::build_nupdf(root)?;
    let all = corpus::files(root, tier)?;
    if only == Some(CONTEXT) {
        return run_context(root, tier, &nupdf, &mutool, &all, runs, sample);
    }
    let pairs: Vec<&Pair> = PAIRS
        .iter()
        .filter(|p| only.is_none_or(|name| p.name == name))
        .collect();
    if pairs.is_empty() {
        bail!("no benchmark pair named {}", only.unwrap_or_default());
    }
    let runs = runs.max(1);
    let out_dir = root.join("target/bench").join(tier);
    std::fs::create_dir_all(&out_dir)?;

    let scratch = out_dir.join("scratch.pdf");
    let scratch_dir = out_dir.join("scratch-split");
    let mut pages = std::collections::HashMap::new();
    for pair in pairs {
        let mut files = Vec::new();
        for file in &all {
            if pair.min_pages > 0 {
                let n = *pages
                    .entry(file.name.clone())
                    .or_insert_with(|| page_count(&nupdf, &file.path));
                if n < pair.min_pages {
                    continue;
                }
            }
            files.push(file);
        }
        let files = sample_evenly(files, sample);
        let baseline = match pair.baseline {
            Tool::Mutool => &mutool,
            Tool::Nupdf => &nupdf,
        };
        let mut rows = Vec::new();
        let mut short = 0;
        for file in files {
            let expand = |args: &[&str]| -> Vec<OsString> {
                args.iter()
                    .map(|a| match *a {
                        "{file}" => file.path.clone().into_os_string(),
                        "{out}" => scratch.clone().into_os_string(),
                        "{dir}" => scratch_dir.clone().into_os_string(),
                        a if a.starts_with("oracle/") => root.join(a).into_os_string(),
                        a if a.starts_with("{dir}/") => {
                            let _ = std::fs::create_dir_all(&scratch_dir);
                            scratch_dir.join(&a["{dir}/".len()..]).into_os_string()
                        }
                        a => OsString::from(a),
                    })
                    .collect()
            };
            let (oracle_ms, oracle_rss) = measure(baseline, &expand(pair.oracle), runs)?;
            if oracle_ms < pair.min_baseline_ms {
                short += 1;
                continue;
            }
            let (nupdf_ms, nupdf_rss) = measure(&nupdf, &expand(pair.nupdf), runs)?;
            rows.push(Row {
                file: file.name.clone(),
                oracle_ms,
                nupdf_ms,
                speedup: oracle_ms / nupdf_ms.max(f64::EPSILON),
                oracle_rss_mb: oracle_rss as f64 / 1_048_576.0,
                nupdf_rss_mb: nupdf_rss as f64 / 1_048_576.0,
            });
        }
        let report = render(pair, tier, runs, &rows, short);
        std::fs::write(
            out_dir.join(format!("{}.json", pair.name)),
            serde_json::to_string_pretty(&rows)?,
        )?;
        std::fs::write(out_dir.join(format!("{}.md", pair.name)), &report)?;
        println!("{report}");
    }
    println!("results: {}", out_dir.display());
    Ok(())
}

/// The pair that times nupdf's plain text against MuPDF's and, for
/// context, other tools'.
pub const CONTEXT: &str = "text-context";

/// The libraries `oracle/bench/text.py` drives, by its names for them.
const PYTHON_TOOLS: &[&str] = &["pymupdf", "pypdf", "pdfplumber", "pdfminer", "pdf_oxide"];

/// One program in the context bench: a name and how to run it.
struct ContextTool {
    name: String,
    program: std::path::PathBuf,
    args: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ContextRow {
    file: String,
    /// Each tool's median ms, in the tools' order; `None` where it failed.
    ms: Vec<Option<f64>>,
}

/// `text-context`: every page's text as plain text, by nupdf (`extract
/// --format text`), MuPDF (`draw -F txt`), pdftotext when on the PATH, and
/// the Python libraries installed in `target/bench-venv`
/// (`oracle/bench/requirements.txt`). Each is timed as a process, as an
/// agent would run it.
fn run_context(
    root: &Path,
    tier: &str,
    nupdf: &Path,
    mutool: &Path,
    all: &[corpus::CorpusFile],
    runs: usize,
    sample: Option<usize>,
) -> Result<()> {
    let out_dir = root.join("target/bench").join(tier);
    std::fs::create_dir_all(&out_dir)?;
    let scratch = out_dir.join("scratch.txt");
    let scratch = scratch.to_string_lossy().into_owned();
    let mut tools = vec![
        ContextTool {
            name: "nupdf".into(),
            program: nupdf.to_path_buf(),
            args: vec![
                "extract".into(),
                "{file}".into(),
                "--format".into(),
                "text".into(),
            ],
        },
        ContextTool {
            name: "MuPDF".into(),
            program: mutool.to_path_buf(),
            args: vec![
                "draw".into(),
                "-q".into(),
                "-F".into(),
                "txt".into(),
                "-o".into(),
                scratch.clone(),
                "{file}".into(),
            ],
        },
    ];
    let mut notes = Vec::new();
    match which("pdftotext") {
        Some(p) => tools.push(ContextTool {
            name: "pdftotext".into(),
            program: p,
            args: vec!["-q".into(), "{file}".into(), scratch.clone()],
        }),
        None => notes.push("pdftotext is not on the PATH; left out".to_string()),
    }
    let python = root.join("target/bench-venv/bin/python");
    let script = root
        .join("oracle/bench/text.py")
        .to_string_lossy()
        .into_owned();
    for lib in PYTHON_TOOLS {
        let module = if *lib == "pdfminer" { "pdfminer" } else { lib };
        let ok = python.exists()
            && proc::run(
                &python,
                &["-c", &format!("import {module}")],
                Duration::from_secs(60),
            )
            .is_ok_and(|r| r.succeeded());
        if ok {
            tools.push(ContextTool {
                name: (*lib).into(),
                program: python.clone(),
                args: vec![script.clone(), (*lib).into(), "{file}".into()],
            });
        } else {
            notes.push(format!(
                "{lib} is not installed in target/bench-venv; left out"
            ));
        }
    }
    let files = sample_evenly(all.iter().collect(), sample);
    let runs = runs.max(1);
    let mut rows = Vec::new();
    for file in files {
        let path = file.path.to_string_lossy().into_owned();
        let mut ms = Vec::new();
        for t in &tools {
            let args: Vec<OsString> = t
                .args
                .iter()
                .map(|a| {
                    OsString::from(if a == "{file}" {
                        path.as_str()
                    } else {
                        a.as_str()
                    })
                })
                .collect();
            ms.push(measure_ok(&t.program, &args, runs)?);
        }
        rows.push(ContextRow {
            file: file.name.clone(),
            ms,
        });
    }
    let report = render_context(tier, runs, &tools, &rows, &notes);
    std::fs::write(
        out_dir.join(format!("{CONTEXT}.json")),
        serde_json::to_string_pretty(&rows)?,
    )?;
    std::fs::write(out_dir.join(format!("{CONTEXT}.md")), &report)?;
    println!("{report}");
    println!("results: {}", out_dir.display());
    Ok(())
}

/// `program` on the PATH.
fn which(program: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(program))
        .find(|p| p.is_file())
}

/// [`measure`]'s median time, or `None` if the program failed.
fn measure_ok(program: &Path, args: &[OsString], runs: usize) -> Result<Option<f64>> {
    let timeout = Duration::from_secs(300);
    if !proc::run(program, args, timeout)?.succeeded() {
        return Ok(None);
    }
    Ok(Some(measure(program, args, runs)?.0))
}

fn render_context(
    tier: &str,
    runs: usize,
    tools: &[ContextTool],
    rows: &[ContextRow],
    notes: &[String],
) -> String {
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    let mut out = format!(
        "# Bench `{CONTEXT}` on `{tier}` (median of {runs} runs)\n\n\
         Every page's text as plain text, each tool timed as a process, as an \
         agent pays for it: the Python libraries' times include starting the \
         interpreter and importing the library. Times in ms; `-` where a tool \
         failed.\n\n| File | {} |\n|---|{}\n",
        names.join(" | "),
        "---|".repeat(names.len())
    );
    for r in rows {
        let cells: Vec<String> =
            r.ms.iter()
                .map(|m| m.map_or("-".into(), |v| format!("{v:.1}")))
                .collect();
        let _ = writeln!(out, "| {} | {} |", r.file, cells.join(" | "));
    }
    let _ = writeln!(
        out,
        "\n| Tool | Files done | Total ms | Geometric mean time vs nupdf |\n|---|---|---|---|"
    );
    for (i, name) in names.iter().enumerate() {
        let done: Vec<&ContextRow> = rows.iter().filter(|r| r.ms[i].is_some()).collect();
        let total: f64 = done.iter().filter_map(|r| r.ms[i]).sum();
        let ratio = geomean(
            done.iter()
                .filter_map(|r| Some(r.ms[i]? / r.ms[0]?.max(f64::EPSILON))),
        );
        let _ = writeln!(
            out,
            "| {name} | {} | {total:.0} | {ratio:.2}x |",
            done.len()
        );
    }
    for n in notes {
        let _ = writeln!(out, "\nNote: {n}.");
    }
    out
}

/// An evenly spaced sample of `n` of `files`, or all of them.
fn sample_evenly<T>(files: Vec<T>, n: Option<usize>) -> Vec<T> {
    let len = files.len();
    match n.filter(|&n| n > 0 && n < len) {
        Some(n) => {
            let step = len as f64 / n as f64;
            let keep: std::collections::HashSet<usize> =
                (0..n).map(|i| (i as f64 * step) as usize).collect();
            files
                .into_iter()
                .enumerate()
                .filter(|(i, _)| keep.contains(i))
                .map(|(_, f)| f)
                .collect()
        }
        None => files,
    }
}

/// The page count `nupdf info` reports, or 0 if it reports none.
pub fn page_count(nupdf: &Path, file: &Path) -> usize {
    proc::run(
        nupdf,
        &[OsString::from("info"), file.as_os_str().to_owned()],
        Duration::from_secs(60),
    )
    .ok()
    .and_then(|run| serde_json::from_slice::<serde_json::Value>(&run.stdout).ok())
    .and_then(|v| v["page_count"].as_u64())
    .map_or(0, |n| n as usize)
}

/// Median wall time in ms and the largest peak RSS across `runs`, after one warmup.
fn measure(program: &Path, args: &[OsString], runs: usize) -> Result<(f64, u64)> {
    let timeout = Duration::from_secs(300);
    proc::run(program, args, timeout)?;
    let mut times = Vec::with_capacity(runs);
    let mut rss = 0;
    for _ in 0..runs {
        let run = proc::run(program, args, timeout)?;
        times.push(run.wall.as_secs_f64() * 1000.0);
        rss = rss.max(run.max_rss_bytes);
    }
    Ok((median(&mut times), rss))
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

fn geomean(values: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = values.fold((0.0, 0usize), |(s, n), v| (s + v.ln(), n + 1));
    if n == 0 {
        f64::NAN
    } else {
        (sum / n as f64).exp()
    }
}

/// The report; `short` files were left out for taking less than the
/// pair's `min_baseline_ms`.
fn render(pair: &Pair, tier: &str, runs: usize, rows: &[Row], short: usize) -> String {
    let base = match pair.baseline {
        Tool::Nupdf => "baseline",
        tool => tool.label(),
    };
    let mut out = format!(
        "# Bench `{}` on `{tier}` (median of {runs} runs)\n\n{}\n\n\
         | File | {base} ms | nupdf ms | Speedup | {base} RSS MB | nupdf RSS MB |\n|---|---|---|---|---|---|\n",
        pair.name, pair.about
    );
    for r in rows {
        let _ = writeln!(
            out,
            "| {} | {:.2} | {:.2} | {:.2}x | {:.1} | {:.1} |",
            r.file, r.oracle_ms, r.nupdf_ms, r.speedup, r.oracle_rss_mb, r.nupdf_rss_mb
        );
    }
    let total = |f: fn(&Row) -> f64| rows.iter().map(f).sum::<f64>();
    let (base_ms, nupdf_ms) = (total(|r| r.oracle_ms), total(|r| r.nupdf_ms));
    let _ = writeln!(
        out,
        "\nFiles: {}. Geometric mean speedup: {:.2}x. Total time: {base} {base_ms:.0} ms, nupdf {nupdf_ms:.0} ms ({:.2}x).",
        rows.len(),
        geomean(rows.iter().map(|r| r.speedup)),
        base_ms / nupdf_ms.max(f64::EPSILON),
    );
    if short > 0 {
        let _ = writeln!(
            out,
            "\nLeft out: {short} file(s) on which {base} takes less than {} ms.",
            pair.min_baseline_ms
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_give_the_total_speedup_and_what_was_left_out() {
        let pair = PAIRS.iter().find(|p| p.name == "stext-scaling").unwrap();
        let row = |oracle_ms, nupdf_ms| Row {
            file: "f".into(),
            oracle_ms,
            nupdf_ms,
            speedup: oracle_ms / nupdf_ms,
            oracle_rss_mb: 1.0,
            nupdf_rss_mb: 1.0,
        };
        let out = render(
            pair,
            "standard",
            3,
            &[row(400.0, 50.0), row(200.0, 50.0)],
            2,
        );
        assert!(
            out.contains("Total time: baseline 600 ms, nupdf 100 ms (6.00x)"),
            "{out}"
        );
        assert!(out.contains("Left out: 2 file(s) on which baseline takes less than 100 ms"));
        assert!(!render(pair, "standard", 3, &[row(400.0, 50.0)], 0).contains("Left out"));
    }

    #[test]
    fn median_handles_odd_and_even_counts() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn context_totals_leave_out_failures() {
        let tool = |name: &str| ContextTool {
            name: name.into(),
            program: "x".into(),
            args: Vec::new(),
        };
        let rows = [
            ContextRow {
                file: "a".into(),
                ms: vec![Some(2.0), Some(8.0)],
            },
            ContextRow {
                file: "b".into(),
                ms: vec![Some(4.0), None],
            },
        ];
        let out = render_context(
            "smoke",
            1,
            &[tool("nupdf"), tool("slow")],
            &rows,
            &["n".into()],
        );
        assert!(out.contains("| b | 4.0 | - |"), "{out}");
        assert!(out.contains("| slow | 1 | 8 | 4.00x |"), "{out}");
        assert!(out.contains("Note: n."));
    }

    #[test]
    fn evenly_spaced_samples() {
        assert_eq!(sample_evenly((0..10).collect(), Some(5)), [0, 2, 4, 6, 8]);
        assert_eq!(sample_evenly((0..3).collect(), Some(5)), [0, 1, 2]);
        assert_eq!(sample_evenly((0..3).collect(), None), [0, 1, 2]);
    }

    #[test]
    fn pairs_have_unique_names_and_both_sides() {
        let mut names = std::collections::HashSet::new();
        for p in PAIRS {
            assert!(names.insert(p.name), "{}", p.name);
            assert!(!p.oracle.is_empty() && !p.nupdf.is_empty(), "{}", p.name);
            assert!(p.nupdf.contains(&"{file}"), "{}", p.name);
        }
    }

    #[test]
    fn geomean_of_ratios() {
        let g = geomean([2.0, 8.0].into_iter());
        assert!((g - 4.0).abs() < 1e-9);
        assert!(geomean(std::iter::empty()).is_nan());
    }
}
