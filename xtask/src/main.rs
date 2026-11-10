// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `cargo xtask <command>`: everything that compares nupdf against MuPDF.

mod bench;
mod corpus;
mod encode;
mod encrypt;
mod fixtures;
mod harness;
mod jpeg;
mod oracle;
mod proc;

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
    /// Regenerate corpus/smoke, or verify it is up to date with --check.
    Fixtures {
        #[arg(long)]
        check: bool,
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
    },
    /// Time paired oracle/nupdf commands on a corpus tier.
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
    },
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
        Cmd::Harness {
            tier,
            check,
            update_baseline,
            timeout,
        } => harness::run(
            &root,
            &tier,
            check.as_deref(),
            update_baseline,
            Duration::from_secs(timeout),
        ),
        Cmd::Bench {
            tier,
            pair,
            runs,
            sample,
        } => bench::run(&root, &tier, pair.as_deref(), runs, sample),
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
