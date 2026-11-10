// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Paired benchmarks: the same task done by the oracle and by nupdf, timed
//! end to end as processes (what an agent actually pays), with peak RSS.
//! Runs are sequential to keep timings quiet.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::{corpus, oracle, proc};

#[derive(Debug)]
pub struct Pair {
    pub name: &'static str,
    pub about: &'static str,
    /// Arguments to `mutool`; `{file}` is replaced by the input path and
    /// `{out}` by a scratch output path.
    pub oracle: &'static [&'static str],
    /// Arguments to `nupdf`, expanded the same way.
    pub nupdf: &'static [&'static str],
}

pub const PAIRS: &[Pair] = &[
    Pair {
        name: "info",
        about: "open, read the effective version, count pages and xref slots (JS overhead included on the MuPDF side)",
        oracle: &["run", "oracle/js/info.js", "{file}"],
        nupdf: &["info", "{file}"],
    },
    Pair {
        name: "clean",
        about: "rewrite the document with `clean`'s defaults; nupdf also reads its output back to verify it",
        oracle: &["clean", "{file}", "{out}"],
        nupdf: &["clean", "{file}", "-o", "{out}"],
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
    let mut files = corpus::files(root, tier)?;
    if let Some(n) = sample.filter(|&n| n > 0 && n < files.len()) {
        let step = files.len() as f64 / n as f64;
        files = (0..n)
            .map(|i| files[(i as f64 * step) as usize].clone())
            .collect();
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
    for pair in pairs {
        let mut rows = Vec::new();
        for file in &files {
            let expand = |args: &[&str]| -> Vec<OsString> {
                args.iter()
                    .map(|a| match *a {
                        "{file}" => file.path.clone().into_os_string(),
                        "{out}" => scratch.clone().into_os_string(),
                        a if a.starts_with("oracle/") => root.join(a).into_os_string(),
                        a => OsString::from(a),
                    })
                    .collect()
            };
            let (oracle_ms, oracle_rss) = measure(&mutool, &expand(pair.oracle), runs)?;
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
    let mut out = format!(
        "# Bench `{}` on `{tier}` (median of {runs} runs)\n\n{}\n\n\
         | File | MuPDF ms | nupdf ms | Speedup | MuPDF RSS MB | nupdf RSS MB |\n|---|---|---|---|---|---|\n",
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
        "\nFiles: {}. Geometric mean speedup: {:.2}x. Total time: MuPDF {:.0} ms, nupdf {:.0} ms.",
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
    fn geomean_of_ratios() {
        let g = geomean([2.0, 8.0].into_iter());
        assert!((g - 4.0).abs() < 1e-9);
        assert!(geomean(std::iter::empty()).is_nan());
    }
}
