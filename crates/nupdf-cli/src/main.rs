// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf`: JSON reports on stdout, JSON errors on stderr, meaningful exit
//! codes. The contract is specified in docs/AGENT-INTERFACE.md.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use nupdf::DocId;
use nupdf::header::{self, Kind};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "nupdf",
    version,
    about = "Memory-safe PDF toolkit built for agents"
)]
struct Cli {
    /// Pretty-print JSON output.
    #[arg(long, global = true)]
    pretty: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Identify a file and report what nupdf can tell about it.
    Info { file: PathBuf },
}

/// Exit codes from docs/AGENT-INTERFACE.md. Usage errors (2) come from clap.
#[derive(Debug, Clone, Copy)]
enum Exit {
    Input = 3,
}

#[derive(Debug, Serialize)]
struct Diagnostic {
    code: &'static str,
    severity: &'static str,
    message: String,
}

#[derive(Debug, Serialize)]
struct InfoReport {
    schema: &'static str,
    doc_id: String,
    file_size: usize,
    kind: &'static str,
    /// Version from the header. Until M1 parses the catalog, this ignores
    /// /Root/Version overrides.
    pdf_version: String,
    header_offset: usize,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Serialize)]
struct ErrorReport<'a> {
    schema: &'static str,
    error: ErrorBody<'a>,
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: String,
}

struct Failure {
    exit: Exit,
    code: &'static str,
    message: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Info { file } => info(file),
    };
    match result {
        Ok(report) => {
            emit(&mut std::io::stdout(), &report, cli.pretty);
            ExitCode::SUCCESS
        }
        Err(failure) => {
            let report = ErrorReport {
                schema: "nupdf.error/v0",
                error: ErrorBody {
                    code: failure.code,
                    message: failure.message,
                },
            };
            emit(&mut std::io::stderr(), &report, cli.pretty);
            ExitCode::from(failure.exit as u8)
        }
    }
}

fn emit(out: &mut impl Write, value: &impl Serialize, pretty: bool) {
    let json = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    // Serializing these plain structs cannot fail; a closed pipe is not ours to report.
    if let Ok(json) = json {
        let _ = writeln!(out, "{json}");
    }
}

fn info(path: &Path) -> Result<InfoReport, Failure> {
    let data = std::fs::read(path).map_err(|e| Failure {
        exit: Exit::Input,
        code: "input.unreadable",
        message: format!("cannot read {}: {e}", path.display()),
    })?;
    let header = header::find(&data).map_err(|e| Failure {
        exit: Exit::Input,
        code: "input.not_pdf",
        message: e.to_string(),
    })?;

    let mut diagnostics = Vec::new();
    if header.offset > 0 {
        diagnostics.push(Diagnostic {
            code: "header.garbage_prefix",
            severity: "warning",
            message: format!("{} bytes before the %PDF- marker", header.offset),
        });
    }
    if !header.version.is_known() {
        diagnostics.push(Diagnostic {
            code: "header.unknown_version",
            severity: "warning",
            message: format!("unknown PDF version {}", header.version),
        });
    }

    Ok(InfoReport {
        schema: "nupdf.info/v0",
        doc_id: DocId::of(&data).to_string(),
        file_size: data.len(),
        kind: match header.kind {
            Kind::Pdf => "pdf",
            Kind::Fdf => "fdf",
        },
        pdf_version: header.version.to_string(),
        header_offset: header.offset,
        diagnostics,
    })
}
