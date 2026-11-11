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
}

const DEFAULT: Pair = Pair {
    name: "",
    about: "",
    baseline: Tool::Mutool,
    oracle: &[],
    nupdf: &[],
    min_pages: 0,
};

/// The threads the parallel pairs use: the 8 cores of M5's exit criterion.
const THREADS: &str = "8";

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
        name: "stext-scaling",
        about: "nupdf's structured text on 1 thread against 8, for documents of 50 pages or more",
        baseline: Tool::Nupdf,
        oracle: &["stext", "--threads", "1", "{file}"],
        nupdf: &["stext", "--threads", THREADS, "{file}"],
        min_pages: 50,
    },
    Pair {
        name: "extract-scaling",
        about: "nupdf's `extract` of every page as JSON on 1 thread against 8, for documents of 50 pages or more",
        baseline: Tool::Nupdf,
        oracle: &["extract", "--threads", "1", "{file}"],
        nupdf: &["extract", "--threads", THREADS, "{file}"],
        min_pages: 50,
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
        for file in files {
            let expand = |args: &[&str]| -> Vec<OsString> {
                args.iter()
                    .map(|a| match *a {
                        "{file}" => file.path.clone().into_os_string(),
                        "{out}" => scratch.clone().into_os_string(),
                        "{dir}" => scratch_dir.clone().into_os_string(),
                        a if a.starts_with("oracle/") => root.join(a).into_os_string(),
                        a => OsString::from(a),
                    })
                    .collect()
            };
            let (oracle_ms, oracle_rss) = measure(baseline, &expand(pair.oracle), runs)?;
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
        let report = render(pair, tier, runs, &rows);
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
fn page_count(nupdf: &Path, file: &Path) -> usize {
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

fn render(pair: &Pair, tier: &str, runs: usize, rows: &[Row]) -> String {
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
    let _ = writeln!(
        out,
        "\nFiles: {}. Geometric mean speedup: {:.2}x. Total time: {base} {:.0} ms, nupdf {:.0} ms.",
        rows.len(),
        geomean(rows.iter().map(|r| r.speedup)),
        total(|r| r.oracle_ms),
        total(|r| r.nupdf_ms),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_handles_odd_and_even_counts() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
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
