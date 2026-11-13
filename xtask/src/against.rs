// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `cargo xtask bench --against pdf_oxide`: the bench pairs that pdf_oxide
//! can also do, timed three ways on the same files: MuPDF, nupdf, and
//! pdf_oxide through its Python bindings (`oracle/bench/oxide.py`, in
//! `target/bench-venv`). pdf_oxide is timed as a process, as an agent pays
//! for it, and in process, from opening the document to its last byte of
//! output, with the interpreter and the import left out.
//!
//! Each side's output is checked as well as timed: a side that exits with
//! an error (or, for pdf_oxide, raises on a page) has failed on the file,
//! and where nupdf's and pdf_oxide's outputs should agree (the page count,
//! whether there is text, which pages have ink) a file on which they do not
//! is counted apart. Ratios are over the files every side did and agreed
//! on, so a fast failure is not counted as a win.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::bench::{expand, geomean, median, sample_evenly};
use crate::{corpus, proc};

/// The tool `--against` takes.
pub const OXIDE: &str = "pdf_oxide";

/// How nupdf's output and pdf_oxide's are held to agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// The same count.
    Equal,
    /// Both empty or both not.
    NonEmpty,
    /// Not at all: the tools count different things (pdf_oxide lists
    /// widgets as annotations, finds tables its own way). Totals are given
    /// for context.
    Count,
}

#[derive(Debug)]
pub struct Against {
    pub name: &'static str,
    /// The operation `oxide.py` runs.
    pub op: &'static str,
    pub about: &'static str,
    /// What the units of output count.
    pub units: &'static str,
    pub check: Check,
    /// MuPDF's and nupdf's arguments, expanded as a bench pair's are; the
    /// same as the bench pair of the same name, where there is one.
    pub oracle: &'static [&'static str],
    pub nupdf: &'static [&'static str],
}

const TEXT_MUTOOL: &[&str] = &["draw", "-q", "-F", "txt", "-o", "{out}", "{file}"];
const TEXT_NUPDF: &[&str] = &["extract", "{file}", "--format", "text"];

pub const AGAINST: &[Against] = &[
    Against {
        name: "info",
        op: "info",
        about: "open and count pages, as each tool's info command does it: `nupdf info` (version, page count, objects, permissions, metadata, and every page's text scanned for the pages that need OCR, up to 2,000 pages), MuPDF's `info.js` (the same without the text scan, JS overhead included), pdf_oxide `PdfDocument`, `version()` and `page_count`",
        units: "pages",
        check: Check::Equal,
        oracle: &["run", "oracle/js/info.js", "{file}"],
        nupdf: &["info", "{file}"],
    },
    Against {
        name: "pages",
        op: "pages",
        about: "open and walk the page tree: `nupdf pages` and `pages.js` (each page's object and inherited MediaBox, CropBox, Rotate and Resources, JS overhead included on the MuPDF side), pdf_oxide `page_media_box(i)`, `page_crop_box(i)` and `page_rotation(i)` for every page",
        units: "pages",
        check: Check::Equal,
        oracle: &["run", "oracle/js/pages.js", "{file}"],
        nupdf: &["pages", "{file}"],
    },
    Against {
        name: "text",
        op: "text",
        about: "every page's text as plain text to a file or stdout: `nupdf extract --format text`, `mutool draw -F txt`, pdf_oxide `extract_text(i)` with its defaults (which look for tables)",
        units: "non-whitespace characters",
        check: Check::NonEmpty,
        oracle: TEXT_MUTOOL,
        nupdf: TEXT_NUPDF,
    },
    Against {
        name: "text-fast",
        op: "text-fast",
        about: "as `text`, with pdf_oxide in its speed-first mode, `extract_text(i, extract_tables=False)`",
        units: "non-whitespace characters",
        check: Check::NonEmpty,
        oracle: TEXT_MUTOOL,
        nupdf: TEXT_NUPDF,
    },
    Against {
        name: "render",
        op: "render",
        about: "every page at 72 dpi to an image file per page: `nupdf render --format ppm` (RGB, no annotations), `mutool draw -N` (RGB PPM, with annotations), pdf_oxide `render_pixmap(i, dpi=72)` (premultiplied RGBA, with annotations) written as PAM",
        units: "pages with ink",
        check: Check::Equal,
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
    },
    Against {
        name: "tables",
        op: "tables",
        about: "every page's tables as rows of cells: `nupdf tables`, MuPDF's table hunting (`tables.js`), pdf_oxide `extract_tables(i)` as JSON",
        units: "tables",
        check: Check::Count,
        oracle: &["run", "oracle/js/tables.js", "{file}"],
        nupdf: &["tables", "{file}"],
    },
    Against {
        name: "annots",
        op: "annots",
        about: "every page's annotations: `nupdf annots` and `annots.js` (widgets left out, as MuPDF does), pdf_oxide `get_annotations(i)` (widgets included) as JSON",
        units: "annotations",
        check: Check::Count,
        oracle: &["run", "oracle/js/annots.js", "{file}"],
        nupdf: &["annots", "{file}"],
    },
    Against {
        name: "forms",
        op: "forms",
        about: "form fields: `nupdf forms` and `forms.js` (each page's widgets), pdf_oxide `get_form_fields()` (the AcroForm's fields) as JSON",
        units: "widgets or fields",
        check: Check::Count,
        oracle: &["run", "oracle/js/forms.js", "{file}"],
        nupdf: &["forms", "{file}"],
    },
    Against {
        name: "signatures",
        op: "signatures",
        about: "signatures checked: `nupdf signatures` and `signatures.js` (each page's signature widgets, byte ranges and digest), pdf_oxide `signatures()`, each with `verify_detached` (digest and signer attributes)",
        units: "signatures",
        check: Check::Count,
        oracle: &["run", "oracle/js/signatures.js", "{file}"],
        nupdf: &["signatures", "{file}"],
    },
];

/// What `oxide.py` reports on its last line.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct OxideStats {
    pub work_ms: f64,
    pub import_ms: f64,
    pub pages: u64,
    pub failed_pages: u64,
    pub units: u64,
}

/// The stats on the last non-empty line of `oxide.py`'s stdout.
pub fn parse_stats(stdout: &[u8]) -> Option<OxideStats> {
    let text = std::str::from_utf8(stdout).ok()?;
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
    serde_json::from_str(line).ok()
}

/// Whether a binary PPM (P6, maxval 255) has any pixel that is not white;
/// `None` if it is not one.
pub fn ppm_has_ink(bytes: &[u8]) -> Option<bool> {
    let rest = bytes.strip_prefix(b"P6")?;
    // Three numbers (width, height, maxval) after the magic, then one
    // whitespace byte and the samples.
    let mut pos = 0;
    let mut fields = Vec::new();
    while fields.len() < 3 {
        while rest.get(pos)?.is_ascii_whitespace() {
            pos += 1;
        }
        let start = pos;
        while rest.get(pos)?.is_ascii_digit() {
            pos += 1;
        }
        let n: usize = std::str::from_utf8(&rest[start..pos]).ok()?.parse().ok()?;
        fields.push(n);
    }
    let samples = rest.get(pos + 1..)?;
    let len = fields[0].checked_mul(fields[1])?.checked_mul(3)?;
    if fields[2] != 255 || samples.len() < len {
        return None;
    }
    Some(samples[..len].iter().any(|&b| b != 255))
}

/// The units of nupdf's output for `op`: from its report on stdout, or, for
/// `render`, from the images it wrote into `dir`.
pub fn nupdf_units(op: &str, stdout: &[u8], dir: &Path) -> Option<u64> {
    let first_line = |s: &[u8]| -> Option<serde_json::Value> {
        let end = s.iter().position(|&b| b == b'\n').unwrap_or(s.len());
        serde_json::from_slice(&s[..end]).ok()
    };
    let per_page = |key: &str| -> Option<u64> {
        let report = first_line(stdout)?;
        Some(
            report["pages"]
                .as_array()?
                .iter()
                .map(|p| p[key].as_array().map_or(0, |a| a.len() as u64))
                .sum(),
        )
    };
    match op {
        "info" => first_line(stdout)?["page_count"].as_u64(),
        "pages" => Some(first_line(stdout)?["pages"].as_array()?.len() as u64),
        // The text is all of stdout; the report goes to stderr.
        "text" | "text-fast" => Some(
            String::from_utf8_lossy(stdout)
                .chars()
                .filter(|c| !c.is_whitespace())
                .count() as u64,
        ),
        "render" => {
            let mut inked = 0;
            for entry in std::fs::read_dir(dir).ok()? {
                let path = entry.ok()?.path();
                if path.extension().is_some_and(|e| e == "ppm")
                    && ppm_has_ink(&std::fs::read(&path).ok()?)?
                {
                    inked += 1;
                }
            }
            Some(inked)
        }
        "tables" => per_page("tables"),
        "annots" => per_page("annots"),
        "forms" => per_page("widgets"),
        "signatures" => per_page("signatures"),
        _ => None,
    }
}

/// Whether nupdf's report, on the first line of stdout or (when stdout has
/// the text) of stderr, says pages failed (`page_errors`), which its exit
/// status does not.
fn nupdf_page_errors(run: &proc::Run) -> bool {
    [&run.stdout, &run.stderr].iter().any(|out| {
        let end = out.iter().position(|&b| b == b'\n').unwrap_or(out.len());
        serde_json::from_slice::<serde_json::Value>(&out[..end])
            .ok()
            .and_then(|v| v["page_errors"].as_array().map(|a| !a.is_empty()))
            .unwrap_or(false)
    })
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Row {
    pub file: String,
    /// Median process times in ms; `None` where that side failed.
    pub mupdf_ms: Option<f64>,
    pub nupdf_ms: Option<f64>,
    pub oxide_ms: Option<f64>,
    /// pdf_oxide's median time in process, without the interpreter and the
    /// import.
    pub oxide_work_ms: Option<f64>,
    pub oxide_import_ms: Option<f64>,
    pub nupdf_units: Option<u64>,
    pub oxide_units: Option<u64>,
}

impl Row {
    /// Whether nupdf's and pdf_oxide's outputs agree as `check` asks.
    pub fn agrees(&self, check: Check) -> bool {
        match (check, self.nupdf_units, self.oxide_units) {
            (Check::Count, _, _) => true,
            (Check::Equal, Some(a), Some(b)) => a == b,
            (Check::NonEmpty, Some(a), Some(b)) => (a > 0) == (b > 0),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Summary {
    pub files: usize,
    pub failed_mupdf: usize,
    pub failed_nupdf: usize,
    pub failed_oxide: usize,
    /// Files nupdf and pdf_oxide both did but on which they disagree.
    pub disagree: usize,
    /// Files in MuPDF's ratio: MuPDF and nupdf did them.
    pub counted_mupdf: usize,
    /// Files in pdf_oxide's ratios: nupdf and pdf_oxide did them and agreed.
    pub counted: usize,
    /// Geometric means of each side's process time over nupdf's, each over
    /// its own counted files: above 1, nupdf is faster.
    pub mupdf_ratio: f64,
    pub oxide_ratio: f64,
    /// The same in process: pdf_oxide's time in process over nupdf's
    /// process time less its startup (`nupdf_startup_ms`), that floored at
    /// [`FLOOR_MS`].
    pub work_ratio: f64,
    /// The ratio of the in-process totals, which small files sway less.
    pub work_total_ratio: f64,
    /// Total ms: MuPDF's over its counted files, the others' over
    /// pdf_oxide's.
    pub nupdf_total: f64,
    pub mupdf_total: f64,
    pub oxide_total: f64,
    pub oxide_work_total: f64,
    /// What nupdf takes to start and exit with nothing to do.
    pub nupdf_startup_ms: f64,
    /// Median of pdf_oxide's interpreter start and import, over every file
    /// it did: the cost an agent pays once per process.
    pub oxide_startup_ms: f64,
    /// Output totals over the files both did.
    pub nupdf_units: u64,
    pub oxide_units: u64,
}

/// The least nupdf's time in process is taken to be: below it, what is left
/// after its startup is noise.
pub const FLOOR_MS: f64 = 0.1;

pub fn summarize(check: Check, rows: &[Row], nupdf_startup_ms: f64) -> Summary {
    let mut s = Summary {
        files: rows.len(),
        nupdf_startup_ms,
        ..Summary::default()
    };
    let mut counted = Vec::new();
    let mut with_mupdf = Vec::new();
    let mut startup = Vec::new();
    for r in rows {
        s.failed_mupdf += usize::from(r.mupdf_ms.is_none());
        s.failed_nupdf += usize::from(r.nupdf_ms.is_none());
        s.failed_oxide += usize::from(r.oxide_ms.is_none());
        if let (Some(total), Some(work)) = (r.oxide_ms, r.oxide_work_ms) {
            startup.push(total - work);
        }
        if let (Some(_), Some(_), Some(a), Some(b)) =
            (r.nupdf_ms, r.oxide_ms, r.nupdf_units, r.oxide_units)
        {
            s.nupdf_units += a;
            s.oxide_units += b;
        }
        if let (Some(n), Some(m)) = (r.nupdf_ms, r.mupdf_ms) {
            with_mupdf.push((n, m));
        }
        let (Some(n), Some(o), Some(w)) = (r.nupdf_ms, r.oxide_ms, r.oxide_work_ms) else {
            continue;
        };
        if !r.agrees(check) {
            s.disagree += 1;
            continue;
        }
        counted.push((n, o, w));
    }
    s.counted = counted.len();
    s.counted_mupdf = with_mupdf.len();
    let over = |t: f64, n: f64| t / n.max(f64::EPSILON);
    s.mupdf_ratio = geomean(with_mupdf.iter().map(|&(n, m)| over(m, n)));
    s.oxide_ratio = geomean(counted.iter().map(|&(n, o, _)| over(o, n)));
    let in_process = |n: f64| (n - nupdf_startup_ms).max(FLOOR_MS);
    s.work_ratio = geomean(counted.iter().map(|&(n, _, w)| w / in_process(n)));
    s.mupdf_total = with_mupdf.iter().map(|c| c.1).sum();
    s.nupdf_total = counted.iter().map(|c| c.0).sum();
    s.oxide_total = counted.iter().map(|c| c.1).sum();
    s.oxide_work_total = counted.iter().map(|c| c.2).sum();
    s.work_total_ratio = if counted.is_empty() {
        f64::NAN
    } else {
        s.oxide_work_total / counted.iter().map(|c| in_process(c.0)).sum::<f64>()
    };
    s.oxide_startup_ms = if startup.is_empty() {
        f64::NAN
    } else {
        median(&mut startup)
    };
    s
}

fn ms(v: Option<f64>) -> String {
    v.map_or("fail".into(), |v| format!("{v:.1}"))
}

fn count(v: Option<u64>) -> String {
    v.map_or("-".into(), |v| v.to_string())
}

/// One pair's report: a row per file, then the summary.
pub fn render(a: &Against, tier: &str, runs: usize, rows: &[Row], nupdf_startup_ms: f64) -> String {
    let s = summarize(a.check, rows, nupdf_startup_ms);
    let mut out = format!(
        "# Bench `{}` against pdf_oxide on `{tier}` (median of {runs} runs)\n\n{}.\n\n\
         Times in ms; `fail` where a side failed. pdf_oxide is timed as a process \
         and in process (without the interpreter and its import). Units: {}.\n\n\
         | File | MuPDF | nupdf | pdf_oxide | pdf_oxide in process | nupdf units | pdf_oxide units | |\n\
         |---|---|---|---|---|---|---|---|\n",
        a.name, a.about, a.units
    );
    for r in rows {
        let both = r.nupdf_ms.is_some() && r.oxide_ms.is_some();
        let flag = if both && !r.agrees(a.check) {
            "disagree"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {flag} |",
            r.file,
            ms(r.mupdf_ms),
            ms(r.nupdf_ms),
            ms(r.oxide_ms),
            ms(r.oxide_work_ms),
            count(r.nupdf_units),
            count(r.oxide_units),
        );
    }
    let _ = write!(out, "\n{}", summary_table(&[(a, s)]));
    out
}

fn times(ratio: f64) -> String {
    if ratio.is_nan() {
        "-".into()
    } else {
        format!("{ratio:.2}x")
    }
}

/// The table of every pair run: time over nupdf's, as geometric means, and
/// what each side failed on.
pub fn summary_table(results: &[(&Against, Summary)]) -> String {
    let mut out = String::from(
        "| Operation | Files | MuPDF / nupdf | Files in it | pdf_oxide / nupdf | In process | Of totals | Files in them | \
         Failed: MuPDF | nupdf | pdf_oxide | Disagree | Units: nupdf | pdf_oxide |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    let mut startup = f64::NAN;
    for (a, s) in results {
        startup = s.nupdf_startup_ms;
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            a.name,
            s.files,
            times(s.mupdf_ratio),
            s.counted_mupdf,
            times(s.oxide_ratio),
            times(s.work_ratio),
            times(s.work_total_ratio),
            s.counted,
            s.failed_mupdf,
            s.failed_nupdf,
            s.failed_oxide,
            if a.check == Check::Count {
                "-".to_string()
            } else {
                s.disagree.to_string()
            },
            s.nupdf_units,
            s.oxide_units,
        );
    }
    let _ = writeln!(
        out,
        "\nRatios are each side's time over nupdf's, geometric means: above 1x, nupdf is faster. \
         MuPDF's is over the files MuPDF and nupdf did (`mutool` exits with 1 when any page has \
         an error, which counts as failing), pdf_oxide's over the files nupdf and pdf_oxide did \
         and, where checked, agree on. In process, pdf_oxide's time runs from opening the document to its last \
         byte of output, and nupdf's is its process time less {startup:.1} ms, what it takes \
         to start and exit with nothing to do (`nupdf --version`), and at least {FLOOR_MS} ms; \
         \"of totals\" is the ratio of the summed in-process times. Units are totals over the \
         files both nupdf and pdf_oxide did."
    );
    out
}

/// A side's runs on one file: a first run that must succeed (also the
/// warmup), then `runs` timed ones. `None` if the first failed.
fn runs_of(program: &Path, args: &[OsString], runs: usize) -> Result<Option<Vec<proc::Run>>> {
    let timeout = Duration::from_secs(300);
    let first = proc::run(program, args, timeout)?;
    if !first.succeeded() {
        return Ok(None);
    }
    let mut all = vec![first];
    for _ in 0..runs {
        let run = proc::run(program, args, timeout)?;
        if !run.succeeded() {
            return Ok(None);
        }
        all.push(run);
    }
    Ok(Some(all))
}

/// The median wall time in ms of the timed runs (all but the first).
fn wall_ms(runs: &[proc::Run]) -> f64 {
    let mut t: Vec<f64> = runs[1..]
        .iter()
        .map(|r| r.wall.as_secs_f64() * 1000.0)
        .collect();
    median(&mut t)
}

fn fresh(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::create_dir_all(dir);
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    root: &Path,
    tier: &str,
    only: Option<&str>,
    runs: usize,
    sample: Option<usize>,
    nupdf: &Path,
    mutool: &Path,
    all: &[corpus::CorpusFile],
) -> Result<()> {
    let python = root.join("target/bench-venv/bin/python");
    let version = proc::run(
        &python,
        &[
            "-c",
            "import importlib.metadata as m, sys; print(m.version('pdf_oxide'), sys.version.split()[0])",
        ],
        Duration::from_secs(60),
    );
    let version = match version {
        Ok(r) if r.succeeded() => String::from_utf8_lossy(&r.stdout).trim().to_string(),
        _ => bail!(
            "pdf_oxide is not installed in target/bench-venv (see oracle/bench/requirements.txt)"
        ),
    };
    let pairs: Vec<&Against> = AGAINST
        .iter()
        .filter(|a| only.is_none_or(|name| a.name == name))
        .collect();
    if pairs.is_empty() {
        bail!(
            "no pair named {} to bench against pdf_oxide; there are: {}",
            only.unwrap_or_default(),
            AGAINST
                .iter()
                .map(|a| a.name)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let runs = runs.max(1);
    let out_dir = root
        .join("target/bench")
        .join(tier)
        .join("against-pdf_oxide");
    std::fs::create_dir_all(&out_dir)?;
    let scratch = out_dir.join("scratch.out");
    let dir = out_dir.join("scratch-dir");
    let script: PathBuf = root.join("oracle/bench/oxide.py");
    let files = sample_evenly(all.iter().collect(), sample);
    let startup = runs_of(nupdf, &[OsString::from("--version")], 21)?.map_or(0.0, |r| wall_ms(&r));
    let mut results = Vec::new();
    for a in pairs {
        let mut rows = Vec::new();
        for file in &files {
            let mut row = Row {
                file: file.name.clone(),
                ..Row::default()
            };
            fresh(&dir);
            let args = expand(root, a.oracle, &file.path, &scratch, &dir);
            row.mupdf_ms = runs_of(mutool, &args, runs)?.map(|r| wall_ms(&r));

            fresh(&dir);
            let args = expand(root, a.nupdf, &file.path, &scratch, &dir);
            if let Some(r) = runs_of(nupdf, &args, runs)?
                && !nupdf_page_errors(&r[0])
            {
                row.nupdf_units = nupdf_units(a.op, &r[0].stdout, &dir);
                row.nupdf_ms = Some(wall_ms(&r));
            }

            fresh(&dir);
            let out = if a.op == "render" { &dir } else { &scratch };
            let args: Vec<OsString> = vec![
                script.clone().into_os_string(),
                a.op.into(),
                file.path.clone().into_os_string(),
                out.clone().into_os_string(),
            ];
            if let Some(r) = runs_of(&python, &args, runs)? {
                let stats: Option<Vec<OxideStats>> =
                    r.iter().map(|run| parse_stats(&run.stdout)).collect();
                if let Some(stats) = stats.filter(|s| s.iter().all(|s| s.failed_pages == 0)) {
                    let mut work: Vec<f64> = stats[1..].iter().map(|s| s.work_ms).collect();
                    let mut import: Vec<f64> = stats[1..].iter().map(|s| s.import_ms).collect();
                    row.oxide_units = Some(stats[0].units);
                    row.oxide_work_ms = Some(median(&mut work));
                    row.oxide_import_ms = Some(median(&mut import));
                    row.oxide_ms = Some(wall_ms(&r));
                }
            }
            rows.push(row);
        }
        let report = render(a, tier, runs, &rows, startup);
        std::fs::write(
            out_dir.join(format!("{}.json", a.name)),
            serde_json::to_string_pretty(&rows)?,
        )?;
        std::fs::write(out_dir.join(format!("{}.md", a.name)), &report)?;
        println!("{report}");
        results.push((a, summarize(a.check, &rows, startup)));
    }
    let mut summary = format!(
        "# nupdf, MuPDF and pdf_oxide on `{tier}` ({} files, median of {runs} runs)\n\n\
         pdf_oxide and Python: {version}. MuPDF: {}.\n\n",
        files.len(),
        mupdf_version(mutool)
    );
    summary.push_str(&summary_table(&results));
    let oxide_startup: Vec<String> = results
        .iter()
        .map(|(a, s)| format!("{} {:.0} ms", a.name, s.oxide_startup_ms))
        .collect();
    let _ = writeln!(
        summary,
        "\npdf_oxide's interpreter start and import, the median of its process time less its time in process: {}.",
        oxide_startup.join(", ")
    );
    std::fs::write(out_dir.join("summary.md"), &summary)?;
    println!("{summary}");
    println!("results: {}", out_dir.display());
    Ok(())
}

fn mupdf_version(mutool: &Path) -> String {
    proc::run(mutool, &["-v"], Duration::from_secs(10))
        .map(|r| {
            let all = [r.stdout, r.stderr].concat();
            String::from_utf8_lossy(&all)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::PAIRS;

    #[test]
    fn pairs_run_what_the_bench_pairs_of_the_same_name_run() {
        let mut names = std::collections::HashSet::new();
        for a in AGAINST {
            assert!(names.insert(a.name), "{}", a.name);
            assert!(a.nupdf.contains(&"{file}") && a.oracle.contains(&"{file}"));
            if let Some(p) = PAIRS.iter().find(|p| p.name == a.name) {
                assert_eq!(p.oracle, a.oracle, "{}", a.name);
                assert_eq!(p.nupdf, a.nupdf, "{}", a.name);
            }
        }
        let script = include_str!("../../oracle/bench/oxide.py");
        for a in AGAINST {
            assert!(script.contains(&format!("\"{}\"", a.op)), "{}", a.op);
        }
    }

    #[test]
    fn stats_come_from_the_last_line() {
        let out = b"warning: something\n{\"work_ms\": 1.5, \"import_ms\": 20.0, \"pages\": 3, \"failed_pages\": 0, \"units\": 7}\n\n";
        let s = parse_stats(out).unwrap();
        assert_eq!((s.work_ms, s.pages, s.units), (1.5, 3, 7));
        assert!(parse_stats(b"Traceback (most recent call last):\n").is_none());
        assert!(parse_stats(b"").is_none());
    }

    #[test]
    fn ppm_ink() {
        let white = b"P6\n2 1\n255\n\xff\xff\xff\xff\xff\xff";
        let inked = b"P6 2 1 255\n\xff\xff\xff\xff\x00\xff";
        assert_eq!(ppm_has_ink(white), Some(false));
        assert_eq!(ppm_has_ink(inked), Some(true));
        assert_eq!(ppm_has_ink(b"P6\n2 1\n255\n\xff"), None, "short");
        assert_eq!(ppm_has_ink(b"P5\n1 1\n255\n\x00"), None, "not RGB");
    }

    #[test]
    fn nupdf_units_read_its_reports() {
        let dir = Path::new("/nonexistent");
        let info = br#"{"schema":"nupdf.info/v1","page_count":4}"#;
        assert_eq!(nupdf_units("info", info, dir), Some(4));
        let pages = br#"{"page_count":3,"pages":[{"object":"3 0 R"},{"object":"4 0 R"}]}"#;
        assert_eq!(nupdf_units("pages", pages, dir), Some(2));
        // Text on stdout, the report on stderr.
        assert_eq!(
            nupdf_units("text", b"Hello world\n\x0c\n  a\n", dir),
            Some(11)
        );
        let tables = br#"{"pages":[{"tables":[{},{}]},{"tables":[]},{"tables":[{}]}]}"#;
        assert_eq!(nupdf_units("tables", tables, dir), Some(3));
        let forms = br#"{"pages":[{"widgets":[{}]},{"page":2}]}"#;
        assert_eq!(nupdf_units("forms", forms, dir), Some(1));
        assert_eq!(nupdf_units("info", b"not json", dir), None);
    }

    #[test]
    fn nupdf_units_count_rendered_pages_with_ink() {
        let dir = std::env::temp_dir().join(format!("nupdf-against-{}", std::process::id()));
        fresh(&dir);
        std::fs::write(dir.join("page-0001.ppm"), b"P6\n1 1\n255\n\x00\x00\x00").unwrap();
        std::fs::write(dir.join("page-0002.ppm"), b"P6\n1 1\n255\n\xff\xff\xff").unwrap();
        std::fs::write(dir.join("page-0003.ppm"), b"P6\n1 1\n255\n\x10\xff\xff").unwrap();
        assert_eq!(nupdf_units("render", b"{}", &dir), Some(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_errors_fail_nupdf() {
        let run = |stdout: &[u8], stderr: &[u8]| proc::Run {
            status: proc::Status::Exited(0),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            wall: Duration::ZERO,
            max_rss_bytes: 0,
        };
        let failed = br#"{"page_errors":[{"page":2}]}"#;
        assert!(nupdf_page_errors(&run(b"text", failed)));
        assert!(nupdf_page_errors(&run(failed, b"")));
        assert!(!nupdf_page_errors(&run(b"text", br#"{"page_errors":[]}"#)));
        assert!(!nupdf_page_errors(&run(br#"{"schema":"x"}"#, b"")));
    }

    fn row(n: Option<f64>, m: Option<f64>, o: Option<(f64, f64)>, units: (u64, u64)) -> Row {
        Row {
            file: "f".into(),
            nupdf_ms: n,
            mupdf_ms: m,
            oxide_ms: o.map(|o| o.0),
            oxide_work_ms: o.map(|o| o.1),
            oxide_import_ms: None,
            nupdf_units: n.map(|_| units.0),
            oxide_units: o.map(|_| units.1),
        }
    }

    #[test]
    fn summaries_count_failures_and_disagreements_apart_from_the_ratios() {
        let rows = [
            row(Some(2.0), Some(4.0), Some((40.0, 10.0)), (5, 5)),
            row(Some(8.0), Some(8.0), Some((40.0, 16.0)), (5, 5)),
            // pdf_oxide failed: in no ratio.
            row(Some(1.0), Some(9.0), None, (5, 0)),
            // pdf_oxide was fast but found nothing: a disagreement.
            row(Some(10.0), Some(10.0), Some((21.0, 0.1)), (5, 0)),
            row(None, None, Some((30.0, 5.0)), (0, 3)),
        ];
        let s = summarize(Check::NonEmpty, &rows, 1.0);
        assert_eq!(
            (s.files, s.counted, s.counted_mupdf, s.disagree),
            (5, 2, 4, 1),
            "{s:?}"
        );
        assert_eq!((s.failed_mupdf, s.failed_nupdf, s.failed_oxide), (1, 1, 1));
        // MuPDF over every file it and nupdf did, pdf_oxide failing or not.
        assert!((s.mupdf_ratio - 18f64.powf(0.25)).abs() < 1e-9, "{s:?}");
        assert!((s.oxide_ratio - 10.0).abs() < 1e-9, "{s:?}");
        // In process: 10 / (2 - 1) and 16 / (8 - 1).
        let work = (10.0f64 * 16.0 / 7.0).sqrt();
        assert!((s.work_ratio - work).abs() < 1e-9, "{s:?}");
        assert!((s.work_total_ratio - 26.0 / 8.0).abs() < 1e-9, "{s:?}");
        assert_eq!((s.nupdf_total, s.oxide_work_total), (10.0, 26.0));
        assert_eq!((s.nupdf_units, s.oxide_units), (15, 10));
        // Startup: process less in process, the median of 30, 24, 20.9, 25.
        assert!((s.oxide_startup_ms - 24.5).abs() < 1e-9, "{s:?}");
        // Counted, not checked: the disagreeing file is in the ratios.
        assert_eq!(summarize(Check::Count, &rows, 1.0).counted, 3);
        // Startup above the process time leaves the floor.
        let floored = summarize(Check::NonEmpty, &rows[..1], 5.0);
        assert!((floored.work_ratio - 10.0 / FLOOR_MS).abs() < 1e-6);
    }

    #[test]
    fn checks() {
        let r = |a, b| Row {
            nupdf_units: Some(a),
            oxide_units: Some(b),
            ..Row::default()
        };
        assert!(r(3, 3).agrees(Check::Equal) && !r(3, 2).agrees(Check::Equal));
        assert!(r(3, 9).agrees(Check::NonEmpty) && !r(3, 0).agrees(Check::NonEmpty));
        assert!(r(0, 0).agrees(Check::NonEmpty) && r(0, 7).agrees(Check::Count));
        assert!(!Row::default().agrees(Check::Equal));
    }

    #[test]
    fn reports_flag_disagreements_and_failures() {
        let a = AGAINST.iter().find(|a| a.name == "info").unwrap();
        let rows = [
            row(Some(2.0), Some(4.0), Some((40.0, 1.0)), (3, 2)),
            row(Some(2.0), None, Some((40.0, 1.0)), (3, 3)),
        ];
        let out = render(a, "smoke", 3, &rows, 1.0);
        assert!(
            out.contains("| f | 4.0 | 2.0 | 40.0 | 1.0 | 3 | 2 | disagree |"),
            "{out}"
        );
        assert!(out.contains("| f | fail | 2.0 |"), "{out}");
        assert!(
            out.contains(
                "| info | 2 | 2.00x | 1 | 20.00x | 1.00x | 1.00x | 1 | 1 | 0 | 0 | 1 | 6 | 5 |"
            ) && out.contains("less 1.0 ms"),
            "{out}"
        );
    }
}
