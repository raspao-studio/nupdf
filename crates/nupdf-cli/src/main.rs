// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf`: JSON reports on stdout, JSON errors on stderr, meaningful exit
//! codes. The contract is specified in docs/AGENT-INTERFACE.md.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use nupdf::header::{self, Kind};
use nupdf::{DocId, Document};
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
    /// Effective version: the header's unless /Root/Version overrides it.
    pdf_version: String,
    /// Version in the header; 1.7 when there is no header.
    header_version: String,
    /// Bytes of junk before the header, or null when there is no header.
    header_offset: Option<usize>,
    page_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_count_error: Option<String>,
    /// Slots in the cross-reference table.
    object_count: usize,
    revisions: usize,
    repaired: bool,
    encrypted: bool,
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
    let header = header::find(&data).ok();
    let doc_id = DocId::of(&data).to_string();
    let file_size = data.len();
    let doc = Document::open(data).map_err(|e| Failure {
        exit: Exit::Input,
        code: if header.is_none() {
            "input.not_pdf"
        } else {
            e.code
        },
        message: e.message,
    })?;

    // Same order as oracle/js/info.js: a repair triggered by one call is
    // visible to the next.
    let pdf_version = doc.version().to_string();
    let (page_count, page_count_error) = match doc.page_count() {
        Ok(n) => (Some(n), None),
        Err(e) => (None, Some(e.0)),
    };
    let object_count = doc.xref_len();

    let diagnostics = doc
        .diagnostics()
        .snapshot()
        .into_iter()
        .map(|d| Diagnostic {
            code: d.code,
            severity: "warning",
            message: d.message,
        })
        .collect();

    Ok(InfoReport {
        schema: "nupdf.info/v0",
        doc_id,
        file_size,
        kind: match header.map(|h| h.kind) {
            Some(Kind::Fdf) => "fdf",
            _ => "pdf",
        },
        pdf_version,
        header_version: doc.header_version().to_string(),
        header_offset: header.map(|h| h.offset),
        page_count,
        page_count_error,
        object_count,
        revisions: doc.revisions(),
        repaired: doc.repaired(),
        encrypted: doc.is_encrypted(),
        diagnostics,
    })
}
