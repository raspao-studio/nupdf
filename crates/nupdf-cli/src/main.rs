// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf`: JSON reports on stdout, JSON errors on stderr, meaningful exit
//! codes. The contract is specified in docs/AGENT-INTERFACE.md.

mod objects;

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
    /// Dump every object in the cross-reference table as canonical JSON.
    Objects { file: PathBuf },
    /// List pages with their inherited MediaBox, CropBox, Rotate and Resources.
    Pages { file: PathBuf },
    /// Show one object (`12`, `12 0 R`) or the trailer (`trailer`).
    Show {
        file: PathBuf,
        target: String,
        /// Include the decoded stream data as text (lossy UTF-8).
        #[arg(long)]
        data: bool,
        /// Cap on the bytes of stream data included with --data.
        #[arg(long, default_value_t = 65536)]
        max_bytes: usize,
    },
}

/// Exit codes from docs/AGENT-INTERFACE.md. Usage errors (2) come from clap.
#[derive(Debug, Clone, Copy)]
enum Exit {
    Internal = 1,
    Usage = 2,
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
        Command::Info { file } => info(file).and_then(to_json),
        Command::Objects { file } => open(file)
            .map(|(doc, facts)| report("nupdf.objects/v0", &facts, &doc, objects::dump(&doc))),
        Command::Pages { file } => open(file)
            .map(|(doc, facts)| report("nupdf.pages/v0", &facts, &doc, objects::pages(&doc))),
        Command::Show {
            file,
            target,
            data,
            max_bytes,
        } => show(file, target, *data, *max_bytes),
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

fn to_json(report: InfoReport) -> Result<serde_json::Value, Failure> {
    serde_json::to_value(report).map_err(|e| Failure {
        exit: Exit::Internal,
        code: "internal.serialize",
        message: e.to_string(),
    })
}

/// What `open` learned about the file before parsing it.
struct FileFacts {
    header: Option<header::Header>,
    doc_id: String,
    file_size: usize,
}

fn open(path: &Path) -> Result<(Document, FileFacts), Failure> {
    let data = std::fs::read(path).map_err(|e| Failure {
        exit: Exit::Input,
        code: "input.unreadable",
        message: format!("cannot read {}: {e}", path.display()),
    })?;
    let facts = FileFacts {
        header: header::find(&data).ok(),
        doc_id: DocId::of(&data).to_string(),
        file_size: data.len(),
    };
    let doc = Document::open(data).map_err(|e| Failure {
        exit: Exit::Input,
        code: if facts.header.is_none() {
            "input.not_pdf"
        } else {
            e.code
        },
        message: e.message,
    })?;
    Ok((doc, facts))
}

/// Wraps a command's fields in the report envelope every command shares:
/// `schema` and `doc_id` first, `diagnostics` last.
fn report(
    schema: &str,
    facts: &FileFacts,
    doc: &Document,
    body: serde_json::Value,
) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    out.insert("schema".into(), schema.into());
    out.insert("doc_id".into(), facts.doc_id.clone().into());
    if let serde_json::Value::Object(fields) = body {
        out.extend(fields);
    }
    out.insert("diagnostics".into(), diagnostics(doc));
    serde_json::Value::Object(out)
}

fn diagnostics(doc: &Document) -> serde_json::Value {
    doc.diagnostics()
        .snapshot()
        .into_iter()
        .map(|d| serde_json::json!({ "code": d.code, "severity": "warning", "message": d.message }))
        .collect()
}

fn show(
    path: &Path,
    target: &str,
    data: bool,
    max_bytes: usize,
) -> Result<serde_json::Value, Failure> {
    let (doc, facts) = open(path)?;
    let mut body = serde_json::Map::new();
    if target.trim() == "trailer" {
        body.insert("object".into(), "trailer".into());
        body.insert(
            "value".into(),
            objects::encode(&nupdf::Object::Dict(doc.trailer().into())),
        );
        return Ok(report("nupdf.show/v0", &facts, &doc, body.into()));
    }
    let num = target
        .split_whitespace()
        .next()
        .and_then(|n| n.parse::<i32>().ok())
        .ok_or_else(|| Failure {
            exit: Exit::Usage,
            code: "usage.bad_target",
            message: format!("expected an object number, `N G R` or `trailer`, got {target:?}"),
        })?;
    if num <= 0 || num as usize >= doc.xref_len() {
        return Err(Failure {
            exit: Exit::Input,
            code: "input.no_such_object",
            message: format!("object {num} is outside the xref (1..{})", doc.xref_len()),
        });
    }
    body.insert("object".into(), format!("{num} 0 R").into());
    let value = doc.object(num);
    body.insert(
        "value".into(),
        value
            .as_ref()
            .map_or(serde_json::Value::Null, objects::encode),
    );
    if let Some(nupdf::Object::Stream(stream)) = &value {
        let mut info = serde_json::Map::new();
        info.insert("raw_len".into(), doc.raw_stream_data(stream).len().into());
        match doc.stream_data(stream) {
            Ok(decoded) => {
                info.insert("decoded_len".into(), decoded.len().into());
                if data {
                    let shown = &decoded[..decoded.len().min(max_bytes)];
                    info.insert(
                        "data".into(),
                        String::from_utf8_lossy(shown).into_owned().into(),
                    );
                    info.insert("truncated".into(), (decoded.len() > max_bytes).into());
                }
            }
            Err(e) => {
                info.insert("decode_error".into(), e.to_string().into());
            }
        }
        body.insert("stream".into(), info.into());
    }
    Ok(report("nupdf.show/v0", &facts, &doc, body.into()))
}

fn info(path: &Path) -> Result<InfoReport, Failure> {
    let (doc, facts) = open(path)?;
    let FileFacts {
        header,
        doc_id,
        file_size,
    } = facts;

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
