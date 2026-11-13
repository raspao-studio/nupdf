// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `cargo xtask <command>`: everything that compares nupdf against MuPDF.

mod adversarial;
mod against;
mod arlington;
mod bench;
mod corpus;
mod crashes;
mod encode;
mod encrypt;
mod engines;
mod fixtures;
mod harness;
mod jpeg;
mod oracle;
mod proc;
mod reading_order;

mod redaction;
mod schemas;
mod tables;
mod triage;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Manage the pinned MuPDF reference build.
    Oracle {
        #[command(subcommand)]
        action: OracleCmd,
    },
    /// Regenerate corpus/smoke and corpus/redaction, or verify they are
    /// up to date with --check.
    Fixtures {
        #[arg(long)]
        check: bool,
    },
    /// Regenerate the tables taken from the oracle's sources (the glyph
    /// list, encodings, CMaps, FreeType's name tables, bidi classes), or
    /// verify they are up to date with --check.
    Tables {
        #[arg(long)]
        check: bool,
    },
    /// Regenerate the JSON Schemas of the reports from what the commands
    /// print on the smoke tier, or verify they are up to date with --check.
    Schemas {
        #[arg(long)]
        check: bool,
        /// Instead, check the reports on this tier's files against them.
        #[arg(long)]
        validate: Option<String>,
        /// How many of the tier's files --validate runs on.
        #[arg(long, default_value_t = 200)]
        sample: usize,
    },
    /// Run differential checks of nupdf against the oracle.
    Harness {
        #[arg(long, default_value = "smoke")]
        tier: String,
        /// Run one check instead of all of them.
        #[arg(long)]
        check: Option<String>,
        /// Accept the current results as the new baseline.
        #[arg(long)]
        update_baseline: bool,
        /// Per-process timeout in seconds.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// An evenly spaced sample of this many files.
        #[arg(long)]
        sample: Option<usize>,
    },
    /// Time paired oracle/nupdf commands on a corpus tier.
    /// Run nupdf alone on every file of a tier, each command in its own
    /// process, and count crashes, internal errors and timeouts.
    Crashes {
        #[arg(long, default_value = "wild")]
        tier: String,
        /// An evenly spaced sample of this many files.
        #[arg(long)]
        sample: Option<usize>,
        /// Processes at once; 0 for one per core.
        #[arg(long, default_value_t = 0)]
        jobs: usize,
        /// Seconds each run may take.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// Keep the runs already in target/crashes/<tier>.jsonl.
        #[arg(long)]
        resume: bool,
        /// Only these commands, comma-separated.
        #[arg(long)]
        only: Option<String>,
        /// Summarize the log only.
        #[arg(long)]
        summary: bool,
    },
    Bench {
        #[arg(long, default_value = "smoke")]
        tier: String,
        #[arg(long)]
        pair: Option<String>,
        #[arg(long, default_value_t = 5)]
        runs: usize,
        /// Bench an evenly spaced sample of this many files.
        #[arg(long)]
        sample: Option<usize>,
        /// Also time PDF Oxide: `pdf_oxide` runs its CLI and Python bindings
        /// on the pairs they can do, MuPDF staying the reference; `engines`
        /// times nupdf's library against pdf_oxide's in process
        /// (bench/engines).
        #[arg(long)]
        against: Option<String>,
    },
    /// Render one page with MuPDF, nupdf and hayro (a second reference),
    /// and report how far each pair is apart.
    Triage {
        file: std::path::PathBuf,
        /// The page, from 1.
        #[arg(long, default_value_t = 1)]
        page: usize,
        #[arg(long, default_value_t = 72.0)]
        dpi: f32,
        /// The nupdf to run.
        #[arg(long, default_value = "target/release/nupdf")]
        nupdf: std::path::PathBuf,
    },
    /// The redaction tier: each adversarial fixture checked to leak, then
    /// redacted and judged by `nupdf verify`, MuPDF and pdftotext.
    Redaction {
        /// The nupdf to run.
        #[arg(long, default_value = "target/release/nupdf")]
        nupdf: std::path::PathBuf,
        /// Another directory of fixtures (default corpus/redaction).
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
        /// Score the pdf-redaction-benchmarks cases instead, generating
        /// them first if need be.
        #[arg(long)]
        benchmarks: bool,
    },
    /// Check files against the Arlington rules the harness applies, and
    /// print each one's violations.
    Arlington { files: Vec<std::path::PathBuf> },
    /// Score nupdf's reading order, MuPDF's and the `reading-order`
    /// option's, against the hand-labeled pages in corpus/reading-order.
    ReadingOrder,
    /// List or fetch public corpora.
    Corpus {
        #[command(subcommand)]
        action: CorpusCmd,
    },
}

#[derive(Debug, Subcommand)]
enum OracleCmd {
    /// Clone and build MuPDF at the version pinned in oracle/MUPDF_VERSION.
    Build,
    /// Print the path of the oracle's mutool.
    Path,
}

#[derive(Debug, Subcommand)]
enum CorpusCmd {
    List,
    Fetch { name: String },
}

fn main() -> Result<()> {
    let root = repo_root();
    match Cli::parse().command {
        Cmd::Oracle { action } => match action {
            OracleCmd::Build => oracle::build(&root),
            OracleCmd::Path => {
                println!("{}", oracle::mutool(&root)?.display());
                Ok(())
            }
        },
        Cmd::Fixtures { check } => fixtures::run(&root, check),
        Cmd::Tables { check } => tables::run(&root, check),
        Cmd::Schemas {
            check,
            validate,
            sample,
        } => match validate {
            Some(tier) => schemas::validate(&root, &tier, sample),
            None => schemas::run(&root, check),
        },
        Cmd::Arlington { files } => arlington::report(&files),
        Cmd::Triage {
            file,
            page,
            dpi,
            nupdf,
        } => {
            let nupdf = if nupdf.is_absolute() {
                nupdf
            } else {
                root.join(nupdf)
            };
            triage::run(&root, &nupdf, &file, page, dpi)
        }
        Cmd::Redaction {
            nupdf,
            dir,
            benchmarks,
        } => {
            let nupdf = if nupdf.is_absolute() {
                nupdf
            } else {
                root.join(nupdf)
            };
            if benchmarks {
                redaction::benchmarks(&root, &nupdf, &oracle::mutool(&root)?)
            } else {
                redaction::run(&root, &nupdf, &oracle::mutool(&root)?, dir)
            }
        }
        Cmd::Harness {
            tier,
            check,
            update_baseline,
            timeout,
            sample,
        } => harness::run(
            &root,
            &tier,
            check.as_deref(),
            update_baseline,
            Duration::from_secs(timeout),
            sample,
        ),
        Cmd::Bench {
            tier,
            pair,
            runs,
            sample,
            against,
        } => bench::run(
            &root,
            &tier,
            pair.as_deref(),
            runs,
            sample,
            against.as_deref(),
        ),
        Cmd::Crashes {
            tier,
            sample,
            jobs,
            timeout,
            resume,
            only,
            summary,
        } => {
            let dir = root.join("target/crashes");
            if summary {
                return crashes::summarize(
                    &dir.join(format!("{tier}.jsonl")),
                    &dir.join(format!("{tier}-summary.json")),
                );
            }
            let nupdf = build_nupdf(&root)?;
            let jobs = if jobs == 0 {
                std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
            } else {
                jobs
            };
            crashes::run_all(
                &root,
                &nupdf,
                &crashes::Options {
                    tier: &tier,
                    sample,
                    jobs,
                    timeout: Duration::from_secs(timeout),
                    resume,
                    only: only.as_deref(),
                },
            )
        }
        Cmd::ReadingOrder => reading_order::run(&root),
        Cmd::Corpus { action } => match action {
            CorpusCmd::List => corpus::list(&root),
            CorpusCmd::Fetch { name } => corpus::fetch(&root, &name),
        },
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Builds the release `nupdf` binary and returns its path.
fn build_nupdf(root: &Path) -> Result<PathBuf> {
    let status = Command::new(env!("CARGO"))
        .current_dir(root)
        .args(["build", "--quiet", "--release", "--package", "nupdf-cli"])
        .status()
        .context("running cargo build")?;
    if !status.success() {
        bail!("cargo build --release -p nupdf-cli failed");
    }
    Ok(root.join("target/release/nupdf"))
}
