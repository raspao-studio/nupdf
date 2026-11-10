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
    /// Largest decoded size of a single stream, in bytes.
    #[arg(long, global = true, default_value_t = nupdf::Limits::default().max_decoded_bytes)]
    max_decoded_bytes: usize,
    /// Largest cross-reference table a document may have, in object slots.
    #[arg(long, global = true, default_value_t = nupdf::Limits::default().max_xref_entries)]
    max_xref_entries: usize,
    /// Password for an encrypted document (user or owner).
    #[arg(long, global = true)]
    password: Option<String>,
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
    Limit = 5,
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
    /// MuPDF's description of the security handler, or "None".
    encryption: String,
    /// The empty password does not open the document. Pages are not
    /// counted then, unless --password opened it.
    needs_password: bool,
    page_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_count_error: Option<String>,
    /// Slots in the cross-reference table.
    object_count: usize,
    revisions: usize,
    /// Absolute offsets of the xref sections, newest first (empty after repair).
    revision_offsets: Vec<u64>,
    linearized: bool,
    repaired: bool,
    encrypted: bool,
    /// Info dictionary entries that are non-empty text strings.
    metadata: serde_json::Map<String, serde_json::Value>,
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
    let limits = nupdf::Limits {
        max_decoded_bytes: cli.max_decoded_bytes,
        max_xref_entries: cli.max_xref_entries,
        ..nupdf::Limits::default()
    };
    let password = cli.password.as_deref();
    let open = |file: &Path| open(file, limits, password);
    let result = match &cli.command {
        Command::Info { file } => open(file)
            .and_then(|opened| info(opened, password.is_some()))
            .and_then(to_json),
        Command::Objects { file } => match open(file) {
            Ok((doc, facts)) => return stream_objects(&doc, &facts, cli.pretty),
            Err(failure) => Err(failure),
        },
        Command::Pages { file } => open(file)
            .map(|(doc, facts)| report("nupdf.pages/v0", &facts, &doc, objects::pages(&doc))),
        Command::Show {
            file,
            target,
            data,
            max_bytes,
        } => open(file).and_then(|opened| show(opened, target, *data, *max_bytes)),
    };
    match result {
        Ok(report) => {
            emit(&mut std::io::stdout().lock(), &report, cli.pretty);
            ExitCode::SUCCESS
        }
        Err(failure) => fail(failure, cli.pretty),
    }
}

fn fail(failure: Failure, pretty: bool) -> ExitCode {
    let report = ErrorReport {
        schema: "nupdf.error/v0",
        error: ErrorBody {
            code: failure.code,
            message: failure.message,
        },
    };
    emit(&mut std::io::stderr().lock(), &report, pretty);
    ExitCode::from(failure.exit as u8)
}

/// Writes `value` as one line of JSON. Serializing can only fail on a write
/// error, and a closed pipe is not ours to report.
fn emit(out: &mut impl Write, value: &impl Serialize, pretty: bool) {
    let mut out = std::io::BufWriter::new(out);
    let written = if pretty {
        serde_json::to_writer_pretty(&mut out, value)
    } else {
        serde_json::to_writer(&mut out, value)
    };
    if written.is_ok() {
        let _ = writeln!(out);
    }
    let _ = out.flush();
}

/// `objects` reports can be far larger than the file (one entry per declared
/// slot), so they are written while objects load instead of built first.
fn stream_objects(doc: &Document, facts: &FileFacts, pretty: bool) -> ExitCode {
    struct Report<'a> {
        facts: &'a FileFacts,
        doc: &'a Document,
    }
    impl Serialize for Report<'_> {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeMap;
            let dump = objects::Dump::new(self.doc);
            let mut map = s.serialize_map(Some(5))?;
            map.serialize_entry("schema", "nupdf.objects/v0")?;
            map.serialize_entry("doc_id", &self.facts.doc_id)?;
            map.serialize_entry("object_count", &dump.count)?;
            map.serialize_entry("objects", &dump)?;
            // Last, so it includes what loading the objects reported.
            map.serialize_entry("diagnostics", &diagnostics(self.doc))?;
            map.end()
        }
    }
    emit(
        &mut std::io::stdout().lock(),
        &Report { facts, doc },
        pretty,
    );
    ExitCode::SUCCESS
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

fn open(
    path: &Path,
    limits: nupdf::Limits,
    password: Option<&str>,
) -> Result<(Document, FileFacts), Failure> {
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
    let doc = Document::open_with(data, limits).map_err(|e| Failure {
        exit: if e.code.starts_with("limit.") {
            Exit::Limit
        } else {
            Exit::Input
        },
        code: if facts.header.is_none() {
            "input.not_pdf"
        } else {
            e.code
        },
        message: e.message,
    })?;
    if let Some(password) = password {
        let access = doc.authenticate(password).map_err(|e| Failure {
            exit: Exit::Input,
            code: "input.encryption",
            message: nupdf::diag::report::strip(&e).to_string(),
        })?;
        if access == 0 {
            return Err(Failure {
                exit: Exit::Input,
                code: "input.wrong_password",
                message: "the password opens the document neither as user nor as owner".into(),
            });
        }
    }
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
    (doc, facts): (Document, FileFacts),
    target: &str,
    data: bool,
    max_bytes: usize,
) -> Result<serde_json::Value, Failure> {
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
        match doc.raw_stream_data(stream) {
            Ok(raw) => info.insert("raw_len".into(), raw.len().into()),
            Err(e) => info.insert("raw_error".into(), e.into()),
        };
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

fn info((doc, facts): (Document, FileFacts), authenticated: bool) -> Result<InfoReport, Failure> {
    let FileFacts {
        header,
        doc_id,
        file_size,
    } = facts;

    // Same order as oracle/js/info.js: a repair triggered by one call is
    // visible to the next.
    let pdf_version = doc.version().to_string();
    let encryption = doc.encryption();
    let needs_password = doc.needs_password();
    let (page_count, page_count_error) = if needs_password && !authenticated {
        (None, None)
    } else {
        match doc.page_count() {
            Ok(n) => (Some(n), None),
            Err(e) => (None, Some(e.0)),
        }
    };
    let object_count = doc.xref_len();
    let repaired = doc.repaired();
    let revisions = doc.revisions();
    let metadata = [
        "Title",
        "Author",
        "Subject",
        "Keywords",
        "Creator",
        "Producer",
        "CreationDate",
        "ModDate",
    ]
    .into_iter()
    .filter_map(|key| doc.metadata(key).map(|v| (key.to_string(), v.into())))
    .collect();

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
        encryption,
        needs_password,
        page_count,
        page_count_error,
        object_count,
        revisions,
        revision_offsets: doc.revision_offsets(),
        linearized: doc.linearized(),
        repaired,
        encrypted: doc.is_encrypted(),
        metadata,
        diagnostics,
    })
}
