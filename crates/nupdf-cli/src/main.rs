// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `nupdf`: JSON reports on stdout, JSON errors on stderr, meaningful exit
//! codes. The contract is specified in docs/AGENT-INTERFACE.md.

mod commands;
mod extract;
mod inventory;
mod md5;
mod objects;
mod search;
mod stext;
mod trace;

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
    /// List pages with their inherited MediaBox, CropBox, Rotate and
    /// Resources; or, with --select, --delete or --rotate, write the document
    /// with its pages changed.
    Pages {
        file: PathBuf,
        /// Keep only these pages, in this order, as `clean --pages`
        /// (`1-3,5`, `N` for the last, `5-1` backwards).
        #[arg(long, conflicts_with = "delete")]
        select: Option<String>,
        /// Drop these pages.
        #[arg(long)]
        delete: Option<String>,
        /// Turn pages clockwise by this many degrees, a multiple of 90.
        #[arg(long, allow_negative_numbers = true)]
        rotate: Option<i32>,
        /// The pages --rotate turns, after --select or --delete; all by
        /// default.
        #[arg(long, requires = "rotate")]
        rotate_pages: Option<String>,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Show the document information dictionary; or, with --set or
    /// --strip, write the document with it changed.
    Meta {
        file: PathBuf,
        /// Set an entry, as `KEY=VALUE` (repeatable), as MuPDF's
        /// `pdf_set_metadata` does.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Remove /Info and the catalog's XMP /Metadata, before any --set.
        #[arg(long)]
        strip: bool,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Write each range of pages to its own file in a directory, as
    /// `clean --pages` writes it: one page per file by default.
    Split {
        file: PathBuf,
        /// The directory for the parts, named after the input.
        #[arg(short, long)]
        output: PathBuf,
        /// Pages in each part.
        #[arg(long, default_value_t = 1)]
        pages_per_file: usize,
        /// The parts as page lists separated by `;`, such as `1-3;4-N`.
        #[arg(long)]
        ranges: Option<String>,
        /// Report what would be written, and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Skip reading each part back.
        #[arg(long)]
        no_verify: bool,
    },
    /// Merge the pages of several documents into a new one, with their
    /// external links and outline items, as `mutool merge` does.
    Merge {
        /// Input files, each followed by its pages if not all of them
        /// (`1-3,5`, `N`, `N-1`), as `mutool merge` takes them.
        #[arg(required = true, allow_negative_numbers = true)]
        inputs: Vec<String>,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Encrypt with a new security handler, as `clean -E`.
    Encrypt {
        file: PathBuf,
        #[arg(long, default_value = "aes-256", value_parser = ["rc4-40", "rc4-128", "aes-128", "aes-256"])]
        method: String,
        /// Empty, the output opens without a password.
        #[arg(long, default_value = "")]
        user_password: String,
        #[arg(long, default_value = "")]
        owner_password: String,
        /// Permission flags, as /P (-1 grants everything).
        #[arg(long, default_value_t = -1, allow_negative_numbers = true)]
        permissions: i32,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Write without encryption, as `clean -D`.
    Decrypt {
        file: PathBuf,
        #[command(flatten)]
        write: commands::WriteArgs,
    },
    /// Decode every stream: length and BLAKE3 of the data. Streams with an
    /// image filter are listed but not decoded, unless --images.
    Streams {
        file: PathBuf,
        /// Decode only the streams with an image filter.
        #[arg(long)]
        images: bool,
    },
    /// List each page's content-stream operators with their operands, as
    /// MuPDF's interpreter hands them to a processor.
    Operators {
        file: PathBuf,
        /// Process each page with empty resources, so that the result
        /// depends on its content streams alone.
        #[arg(long)]
        no_resources: bool,
    },
    /// List each page's device calls, as MuPDF's run processor makes them
    /// for a page's contents.
    Trace { file: PathBuf },
    /// Each page's structured text, as MuPDF's stext device gathers it
    /// from the page's contents: blocks, lines and characters with their
    /// quads, origins, fonts and sizes.
    Stext {
        file: PathBuf,
        /// MuPDF's stext options, comma-separated: preserve-ligatures,
        /// preserve-whitespace, preserve-spans, preserve-images,
        /// dehyphenate, inhibit-spaces, clip (on unless =no), clip-rect=
        /// x0:y0:x1:y1, use-cid-for-unknown-unicode,
        /// use-gid-for-unknown-unicode, ignore-actualtext, vectors,
        /// lazy-vectors, fuzzy-vectors.
        #[arg(long, default_value = "")]
        options: String,
        /// Work on this many threads, a page per thread at a time; the
        /// output is the same at every count (DECISIONS D37).
        #[arg(long)]
        threads: Option<usize>,
    },
    /// Extract the pages' text with content ids (p3, p3.b2, p3.b2.l4,
    /// p3.b2.l4.s1): as JSON with blocks, lines and spans and their boxes
    /// and fonts, as JSON lines (a page per line), as plain text (as
    /// `mutool draw -F txt` writes it) or as Markdown.
    Extract {
        file: PathBuf,
        #[arg(long, value_enum, default_value = "json")]
        format: extract::Format,
        /// The pages, in MuPDF's syntax (`1-3,7`, `N` for the last).
        #[arg(long)]
        pages: Option<String>,
        /// MuPDF's stext options, as for `stext`.
        #[arg(long, default_value = "")]
        options: String,
        /// Stop before the block that would take the text past this many
        /// characters; the first block is always given.
        #[arg(long)]
        max_chars: Option<usize>,
        /// Stop after this many blocks (or pages, without blocks).
        #[arg(long)]
        max_items: Option<usize>,
        /// Start where an earlier call's `next_cursor` says.
        #[arg(long)]
        cursor: Option<String>,
        /// The JSON fields to give, comma-separated: page, bbox, blocks,
        /// type, lines, wmode, dir, spans, font, size, bold, italic, color,
        /// text, chars. Text goes at the deepest level kept. Everything but
        /// chars by default.
        #[arg(long)]
        fields: Option<String>,
        /// Work on this many threads, a page per thread at a time; the
        /// output is the same at every count (DECISIONS D37).
        #[arg(long)]
        threads: Option<usize>,
    },
    /// Find a needle in the pages' text, as MuPDF searches: each hit with
    /// its quads, its text and the content ids of its characters.
    Search {
        file: PathBuf,
        needle: String,
        /// MuPDF's search options, comma-separated: exact, ignore-case,
        /// ignore-diacritics, regexp, keep-lines, keep-paragraphs,
        /// keep-hyphens.
        #[arg(long, default_value = "")]
        options: String,
        /// MuPDF's stext options, as for `stext`.
        #[arg(long, default_value = "")]
        stext_options: String,
        /// The pages, in MuPDF's syntax (`1-3,7`, `N` for the last).
        #[arg(long)]
        pages: Option<String>,
        /// Stop after this many hits.
        #[arg(long)]
        max_items: Option<usize>,
        /// Work on this many threads, a page per thread at a time; the
        /// output is the same at every count (DECISIONS D37).
        #[arg(long)]
        threads: Option<usize>,
    },
    /// List the fonts the pages use, as `mutool info` finds them, with
    /// what each loads as; --extract writes the embedded font programs as
    /// `mutool extract` does.
    Fonts {
        file: PathBuf,
        /// Write each embedded program as font-NNNN.ext into this directory.
        #[arg(long, value_name = "DIR")]
        extract: Option<PathBuf>,
    },
    /// List the images the pages use, as `mutool info` finds them;
    /// --extract writes every image XObject as `mutool extract` does.
    Images {
        file: PathBuf,
        /// Decode every image XObject and give its pixmap's size,
        /// components and the MD5 of its samples, as MuPDF's Pixmap
        /// computeMD5 does.
        #[arg(long)]
        digests: bool,
        /// Write each image XObject as image-NNNN.ext into this directory.
        #[arg(long, value_name = "DIR")]
        extract: Option<PathBuf>,
        /// How --extract writes images.
        #[arg(long, value_enum, default_value = "auto")]
        format: inventory::ImageFormat,
    },
    /// Rewrite the whole document as `mutool clean` does: every object in a
    /// plain xref table, object and xref streams unpacked, encryption kept.
    /// The output is read back and compared object by object before it is
    /// written.
    Clean {
        file: PathBuf,
        /// Where to write, replacing the file atomically.
        #[arg(short, long, required_unless_present = "dry_run")]
        output: Option<PathBuf>,
        /// Garbage collect, as `mutool clean -g`: once drops objects
        /// nothing reaches, -gg also renumbers, -ggg also merges duplicate
        /// objects, -gggg also duplicate streams.
        #[arg(short, action = clap::ArgAction::Count)]
        garbage: u8,
        /// Write streams decoded, as `mutool clean -d`. XML metadata and
        /// JPEG 2000 images stay as they are.
        #[arg(short, action = clap::ArgAction::Count)]
        decompress: u8,
        /// Deflate streams that have no filter, as `mutool clean -z`; a
        /// 1-bit image is CCITT G4 encoded instead. Brotli (-zz) is not
        /// supported.
        #[arg(short = 'z', action = clap::ArgAction::Count)]
        compress: u8,
        /// Compress font streams that have no filter, and keep them encoded.
        #[arg(short = 'f', action = clap::ArgAction::Count)]
        compress_fonts: u8,
        /// Compress image streams that have no filter, and keep them encoded.
        #[arg(short = 'i', action = clap::ArgAction::Count)]
        compress_images: u8,
        /// Hex-encode binary streams, and write strings outside printable
        /// ASCII in hex.
        #[arg(short = 'a', action = clap::ArgAction::Count)]
        ascii: u8,
        /// Compression effort: 0 for zlib's default level, 1 to 100 for
        /// levels 0 to 9.
        #[arg(short = 'e', default_value_t = 0, allow_negative_numbers = true)]
        effort: i32,
        /// Object syntax: -t compact, -tt a dictionary entry per line. The
        /// default is compact, or a line per entry with -a or -d and no -z.
        #[arg(short = 't', action = clap::ArgAction::Count)]
        tight: u8,
        /// Pack objects into object streams and write an xref stream, as
        /// `mutool clean -Z`. The version becomes at least 1.5.
        #[arg(short = 'Z', action = clap::ArgAction::Count)]
        objstms: u8,
        /// Rewrite content streams, as `mutool clean -c`: every page's
        /// contents become one new stream, and the forms, patterns and
        /// glyphs they use are rewritten in place.
        #[arg(short = 'c', action = clap::ArgAction::Count)]
        clean_contents: u8,
        /// Sanitize content streams, as `mutool clean -s`: rewritten as
        /// with -c, with redundant graphics state dropped and the
        /// resources cut to what the contents use.
        #[arg(short = 's', action = clap::ArgAction::Count)]
        sanitize: u8,
        /// Write without encryption, as `mutool clean -D`.
        #[arg(short = 'D', conflicts_with = "encrypt")]
        decrypt: bool,
        /// Encrypt with a new security handler: rc4-40, rc4-128, aes-128 or
        /// aes-256, as `mutool clean -E`.
        #[arg(short = 'E', value_parser = ["rc4-40", "rc4-128", "aes-128", "aes-256"])]
        encrypt: Option<String>,
        /// Owner password for -E.
        #[arg(short = 'O', default_value = "")]
        owner_password: String,
        /// User password for -E. Empty, the output opens without one.
        #[arg(short = 'U', default_value = "")]
        user_password: String,
        /// Permission flags for -E, as /P (-1 grants everything).
        #[arg(short = 'P', default_value_t = -1, allow_negative_numbers = true)]
        permissions: i32,
        /// Keep only these pages, in this order, as `mutool clean`'s page
        /// arguments: `1-3,5`, `N` for the last page, `5-1` backwards.
        #[arg(long)]
        pages: Option<String>,
        /// Report what would be written, and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Skip reading the output back. It is then exactly what MuPDF
        /// writes, even where that does not read back the same.
        #[arg(long)]
        no_verify: bool,
    },
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
    Verify = 4,
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
    /// What the document allows, by MuPDF's permission names.
    permissions: serde_json::Map<String, serde_json::Value>,
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

/// The stack commands run on. Content streams nest as deep as MuPDF's
/// graphics state stack lets them (about a thousand forms), and each level
/// is several frames of the interpreter; the pages are only touched as
/// they are used.
const STACK_SIZE: usize = 1 << 30;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || run(cli))
        .map(std::thread::JoinHandle::join)
    {
        Ok(Ok(code)) => code,
        _ => ExitCode::FAILURE,
    }
}

fn run(cli: Cli) -> ExitCode {
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
        Command::Pages {
            file,
            select,
            delete,
            rotate,
            rotate_pages,
            write,
        } => {
            let edits = commands::PageEdits {
                select: select.as_deref(),
                delete: delete.as_deref(),
                rotate: *rotate,
                rotate_pages: rotate_pages.as_deref(),
            };
            open(file).and_then(|(doc, facts)| {
                if edits.any() {
                    commands::edit_pages(&doc, &facts, &edits, write, password)
                } else {
                    Ok(report("nupdf.pages/v0", &facts, &doc, objects::pages(&doc)))
                }
            })
        }
        Command::Meta {
            file,
            set,
            strip,
            write,
        } => open(file)
            .and_then(|(doc, facts)| commands::meta(&doc, &facts, set, *strip, write, password)),
        Command::Split {
            file,
            output,
            pages_per_file,
            ranges,
            dry_run,
            no_verify,
        } => commands::split(
            file,
            &open,
            output,
            *pages_per_file,
            ranges.as_deref(),
            *dry_run,
            *no_verify,
            password,
        ),
        Command::Merge { inputs, write } => commands::merge(inputs, &open, write, password),
        Command::Encrypt {
            file,
            method,
            user_password,
            owner_password,
            permissions,
            write,
        } => {
            let encrypt = nupdf::crypt::NewMethod::from_name(method).map(|method| {
                nupdf::write::Encrypt::New {
                    method,
                    owner: owner_password.clone(),
                    user: user_password.clone(),
                    permissions: *permissions,
                }
            });
            open(file).and_then(|(doc, facts)| {
                let encrypt = encrypt.ok_or_else(|| Failure {
                    exit: Exit::Usage,
                    code: "usage.bad_value",
                    message: "unknown encryption method".into(),
                })?;
                commands::encryption(&doc, &facts, encrypt, write, password)
            })
        }
        Command::Decrypt { file, write } => open(file).and_then(|(doc, facts)| {
            commands::encryption(&doc, &facts, nupdf::write::Encrypt::Remove, write, password)
        }),
        Command::Streams { file, images } => open(file).map(|(doc, facts)| {
            report(
                "nupdf.streams/v0",
                &facts,
                &doc,
                objects::streams(&doc, *images),
            )
        }),
        Command::Operators { file, no_resources } => open(file).map(|(doc, facts)| {
            report(
                "nupdf.operators/v0",
                &facts,
                &doc,
                objects::operators(&doc, *no_resources),
            )
        }),
        Command::Trace { file } => open(file)
            .map(|(doc, facts)| report("nupdf.trace/v0", &facts, &doc, trace::trace(&doc))),
        Command::Extract {
            file,
            format,
            pages,
            options,
            max_chars,
            max_items,
            cursor,
            fields,
            threads,
        } => {
            let usage = |message: String| Failure {
                exit: Exit::Usage,
                code: "usage.bad_option",
                message,
            };
            let opts = match nupdf::stext::Options::parse(options) {
                Ok(o) => o,
                Err(e) => return fail(usage(e), cli.pretty),
            };
            let (doc, facts) = match open(file) {
                Ok(v) => v,
                Err(failure) => return fail(failure, cli.pretty),
            };
            let args = extract::Args {
                format: *format,
                pages: pages.as_deref(),
                options: opts,
                max_chars: *max_chars,
                max_items: *max_items,
                cursor: cursor.as_deref(),
                fields: fields.as_deref(),
                threads: *threads,
            };
            let extracted = match extract::extract(&doc, &facts.doc_id, &args) {
                Ok(e) => e,
                Err(e) => return fail(usage(e), cli.pretty),
            };
            return emit_extracted(extracted, &facts, &doc, cli.pretty);
        }
        Command::Search {
            file,
            needle,
            options,
            stext_options,
            pages,
            max_items,
            threads,
        } => {
            let usage = |message: String| Failure {
                exit: Exit::Usage,
                code: "usage.bad_option",
                message,
            };
            nupdf::search::parse_options(options)
                .map_err(usage)
                .and_then(|o| {
                    nupdf::stext::Options::parse(stext_options)
                        .map_err(usage)
                        .map(|s| (o, s))
                })
                .and_then(|(o, s)| open(file).map(|(doc, facts)| (o, s, doc, facts)))
                .and_then(|(o, s, doc, facts)| {
                    let args = search::Args {
                        needle,
                        options: o,
                        stext: s,
                        pages: pages.as_deref(),
                        max_items: *max_items,
                        threads: *threads,
                    };
                    search::search(&doc, &args)
                        .map_err(usage)
                        .map(|body| report("nupdf.search/v0", &facts, &doc, body))
                })
        }
        Command::Stext {
            file,
            options,
            threads,
        } => {
            let opts = match nupdf::stext::Options::parse(options) {
                Ok(o) => o,
                Err(message) => {
                    let usage = Failure {
                        exit: Exit::Usage,
                        code: "usage.bad_option",
                        message,
                    };
                    return fail(usage, cli.pretty);
                }
            };
            let (doc, facts) = match open(file) {
                Ok(v) => v,
                Err(failure) => return fail(failure, cli.pretty),
            };
            let body = stext::stext(&doc, opts, *threads, cli.pretty);
            let stdout = &mut std::io::stdout().lock();
            if cli.pretty {
                let report = StextReport {
                    facts: &facts,
                    doc: &doc,
                    body,
                };
                emit(stdout, &report, true);
            } else {
                let mut out = std::io::BufWriter::new(stdout);
                // A closed pipe is not ours to report.
                let written = body.write_compact(
                    &mut out,
                    "nupdf.stext/v0",
                    &facts.doc_id,
                    &diagnostics(&doc),
                );
                if written.is_ok() {
                    let _ = writeln!(out);
                }
                let _ = out.flush();
            }
            return ExitCode::SUCCESS;
        }
        Command::Fonts { file, extract } => open(file).and_then(|(doc, facts)| {
            let dir = extract.as_deref().map(output_dir).transpose()?;
            Ok(report(
                "nupdf.fonts/v0",
                &facts,
                &doc,
                inventory::fonts(&doc, dir),
            ))
        }),
        Command::Images {
            file,
            digests,
            extract,
            format,
        } => open(file).and_then(|(doc, facts)| {
            let dir = extract.as_deref().map(output_dir).transpose()?;
            Ok(report(
                "nupdf.images/v0",
                &facts,
                &doc,
                inventory::images(&doc, *digests, dir, *format),
            ))
        }),
        Command::Clean {
            file,
            output,
            garbage,
            decompress,
            compress,
            compress_fonts,
            compress_images,
            ascii,
            effort,
            tight,
            objstms,
            clean_contents,
            sanitize,
            decrypt,
            encrypt,
            owner_password,
            user_password,
            permissions,
            pages,
            dry_run,
            no_verify,
        } => clean_options(&CleanFlags {
            garbage: *garbage,
            decompress: *decompress,
            compress: *compress,
            fonts: *compress_fonts,
            images: *compress_images,
            ascii: *ascii,
            tight: *tight,
            objstms: *objstms,
            effort: *effort,
            encrypt: match (encrypt, *decrypt) {
                (Some(method), _) => nupdf::crypt::NewMethod::from_name(method).map(|method| {
                    nupdf::write::Encrypt::New {
                        method,
                        owner: owner_password.clone(),
                        user: user_password.clone(),
                        permissions: *permissions,
                    }
                }),
                (None, true) => Some(nupdf::write::Encrypt::Remove),
                (None, false) => Some(nupdf::write::Encrypt::Keep),
            },
        })
        .and_then(|options| open(file).map(|opened| (options, opened)))
        .and_then(|(options, (doc, facts))| {
            let kept = match pages {
                Some(spec) => Some(keep_pages(&doc, spec)?),
                None => None,
            };
            if *clean_contents > 0 || *sanitize > 0 {
                // pdf_clean_file loads every object first, so a repair
                // cannot lose the rewritten streams.
                nupdf::write::check_document(&doc);
                nupdf::content_clean::clean_content_streams(
                    &doc,
                    *sanitize > 0,
                    options.ascii,
                    options.pretty,
                )
                .map_err(|e| Failure {
                    exit: Exit::Input,
                    code: "input.content_stream",
                    message: nupdf::diag::report::strip(&e).to_string(),
                })?;
            }
            let plan = Clean {
                options,
                output: output.as_deref(),
                apply: !dry_run,
                verify: !no_verify,
                password,
                pages: kept,
            };
            clean(&doc, &facts, &plan)
        }),
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

/// A `stext` report with its pages already written as JSON.
struct StextReport<'a> {
    facts: &'a FileFacts,
    doc: &'a Document,
    body: stext::Body,
}

impl Serialize for StextReport<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(5))?;
        map.serialize_entry("schema", "nupdf.stext/v0")?;
        map.serialize_entry("doc_id", &self.facts.doc_id)?;
        self.body.serialize_into(&mut map)?;
        map.serialize_entry("diagnostics", &diagnostics(self.doc))?;
        map.end()
    }
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

/// Writes what `extract` gave in its format.
fn emit_extracted(
    extracted: extract::Extracted,
    facts: &FileFacts,
    doc: &Document,
    pretty: bool,
) -> ExitCode {
    const SCHEMA: &str = "nupdf.extract/v0";
    match extracted {
        extract::Extracted::Json(body) => {
            emit(
                &mut std::io::stdout().lock(),
                &report(SCHEMA, facts, doc, body),
                pretty,
            );
        }
        extract::Extracted::Lines(pages, body) => {
            let mut out = std::io::stdout().lock();
            for page in pages {
                let mut line = serde_json::Map::new();
                line.insert("schema".into(), "nupdf.extract.page/v0".into());
                line.insert("doc_id".into(), facts.doc_id.clone().into());
                if let serde_json::Value::Object(fields) = page {
                    line.extend(fields);
                }
                emit(&mut out, &serde_json::Value::Object(line), false);
            }
            emit(&mut out, &report(SCHEMA, facts, doc, body), false);
        }
        extract::Extracted::Text(text, body) => {
            let mut out = std::io::stdout().lock();
            // A closed pipe is not ours to report.
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
            emit(
                &mut std::io::stderr().lock(),
                &report(SCHEMA, facts, doc, body),
                pretty,
            );
        }
    }
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
/// A directory to write into, made if it is missing.
fn output_dir(dir: &Path) -> Result<&Path, Failure> {
    std::fs::create_dir_all(dir).map_err(|e| Failure {
        exit: Exit::Usage,
        code: "usage.bad_output",
        message: format!("cannot create {}: {e}", dir.display()),
    })?;
    Ok(dir)
}

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

/// `mutool clean`'s stream and syntax flags, counted as it counts them.
struct CleanFlags {
    garbage: u8,
    decompress: u8,
    compress: u8,
    fonts: u8,
    images: u8,
    ascii: u8,
    tight: u8,
    objstms: u8,
    effort: i32,
    /// `None` for a method nupdf does not know.
    encrypt: Option<nupdf::write::Encrypt>,
}

/// `pdf_write_options` as `mutool clean` sets them from its flags.
fn clean_options(f: &CleanFlags) -> Result<nupdf::write::WriteOptions, Failure> {
    if f.compress > 1 {
        return Err(Failure {
            exit: Exit::Usage,
            code: "usage.unsupported",
            message: "Brotli compression (-zz) is not supported".into(),
        });
    }
    let pretty = match f.tight {
        0 => (f.ascii > 0 || f.decompress > 0) && f.compress == 0,
        1 => false,
        _ => true,
    };
    Ok(nupdf::write::WriteOptions {
        pretty,
        ascii: f.ascii > 0,
        garbage: f.garbage.min(4),
        decompress: f.decompress > 0,
        compress: f.compress > 0,
        compress_images: f.images > 0,
        compress_fonts: f.fonts > 0,
        effort: f.effort.clamp(0, 100),
        objstms: f.objstms > 0,
        encrypt: f.encrypt.clone().ok_or_else(|| Failure {
            exit: Exit::Usage,
            code: "usage.bad_value",
            message: "unknown encryption method".into(),
        })?,
        incremental: false,
    })
}

/// `pdf_clean_file`'s page subset: every object loaded first, so a repair
/// cannot lose the edits, then the pages kept, as 0-based indices.
fn keep_pages(doc: &Document, spec: &str) -> Result<Vec<i32>, Failure> {
    let fail = |message: String| Failure {
        exit: Exit::Input,
        code: "input.page_tree",
        message,
    };
    nupdf::write::check_document(doc);
    let count = doc.page_count().map_err(|e| fail(e.0))?;
    let pages = nupdf::rearrange::parse_page_ranges(spec, count, doc);
    nupdf::rearrange::rearrange_pages(doc, &pages)
        .map_err(|e| fail(nupdf::diag::report::strip(&e.0).to_string()))?;
    Ok(pages)
}

struct Clean<'a> {
    options: nupdf::write::WriteOptions,
    output: Option<&'a Path>,
    apply: bool,
    verify: bool,
    password: Option<&'a str>,
    /// The pages kept, when only some are.
    pages: Option<Vec<i32>>,
}

fn clean(doc: &Document, facts: &FileFacts, plan: &Clean) -> Result<serde_json::Value, Failure> {
    let body = serde_json::json!({
        "garbage": plan.options.garbage,
        "streams": {
            "decompress": plan.options.decompress,
            "compress": plan.options.compress,
            "compress_fonts": plan.options.compress_fonts,
            "compress_images": plan.options.compress_images,
            "ascii": plan.options.ascii,
            "effort": plan.options.effort,
        },
        "syntax": if plan.options.pretty { "pretty" } else { "compact" },
        "objstms": plan.options.objstms,
        "pages": plan.pages.as_ref().map(|p| p.iter().map(|i| i + 1).collect::<Vec<_>>()),
        "encryption": encryption_json(&plan.options.encrypt),
    });
    write_document(doc, facts, plan, "nupdf.clean/v0", body)
}

fn encryption_json(encrypt: &nupdf::write::Encrypt) -> serde_json::Value {
    match encrypt {
        nupdf::write::Encrypt::Keep => serde_json::json!("keep"),
        nupdf::write::Encrypt::Remove => serde_json::json!("none"),
        nupdf::write::Encrypt::New {
            method,
            permissions,
            ..
        } => serde_json::json!({
            "method": method.name(),
            "permissions": permissions,
        }),
    }
}

/// Saves `doc` as `plan` says, reads it back unless told not to, writes it
/// unless a dry run, and reports `body` with the output and verification.
fn write_document(
    doc: &Document,
    facts: &FileFacts,
    plan: &Clean,
    schema: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, Failure> {
    let (output, verification) = save_checked(doc, plan)?;
    let mut fields = serde_json::Map::new();
    fields.insert("applied".into(), plan.apply.into());
    if let serde_json::Value::Object(b) = body {
        fields.extend(b);
    }
    fields.insert("output".into(), output);
    fields.insert("verification".into(), verification);
    Ok(report(
        schema,
        facts,
        doc,
        serde_json::Value::Object(fields),
    ))
}

/// The save, check and write of `write_document`: the output's report and
/// the verification's.
fn save_checked(
    doc: &Document,
    plan: &Clean,
) -> Result<(serde_json::Value, serde_json::Value), Failure> {
    // pdf_clean_file: "cannot authenticate password".
    if doc.needs_password() && plan.password.is_none() {
        return Err(Failure {
            exit: Exit::Input,
            code: "input.needs_password",
            message: "the empty password does not open the document: pass --password".into(),
        });
    }
    let bytes = nupdf::write::save(doc, &plan.options).map_err(|e| {
        let message =
            nupdf::diag::report::strip(e.message.lines().last().unwrap_or_default()).to_string();
        // MuPDF's refusals of an incremental write are argument errors.
        if plan.options.incremental && message.starts_with("Can't do incremental") {
            Failure {
                exit: Exit::Usage,
                code: "usage.incremental",
                message,
            }
        } else {
            Failure {
                exit: Exit::Input,
                code: "input.unloadable_object",
                message,
            }
        }
    })?;
    let verification = plan.verify.then(|| {
        let v = nupdf::write::verify(doc, &bytes, plan.password, &plan.options);
        if v.verified() {
            Ok(v)
        } else {
            Err(Failure {
                exit: Exit::Verify,
                code: "verify.failed",
                message: verify_failure(&v),
            })
        }
    });
    let verification = verification.transpose()?;
    if plan.apply
        && let Some(path) = plan.output
    {
        write_atomically(path, &bytes).map_err(|e| Failure {
            exit: Exit::Input,
            code: "output.unwritable",
            message: format!("cannot write {}: {e}", path.display()),
        })?;
    }
    let hash = blake3::hash(&bytes);
    let output = serde_json::json!({
        "path": plan.output.map(|p| p.display().to_string()),
        "doc_id": DocId::from_hash(&hash).to_string(),
        "len": bytes.len(),
        "blake3": hash.to_hex().to_string(),
    });
    let verification = verification.map_or(serde_json::Value::Null, |v| {
        serde_json::json!({
            "verified": true,
            "objects": v.objects,
        })
    });
    Ok((output, verification))
}

/// Why the written document failed its self-check.
fn verify_failure(v: &nupdf::write::Verification) -> String {
    if !v.reopened {
        return "the output does not open".into();
    }
    if v.repaired {
        return "the output needs a repair to open".into();
    }
    const SHOWN: usize = 10;
    let mut nums: Vec<String> = v
        .mismatched
        .iter()
        .take(SHOWN)
        .map(|&n| match n {
            0 => "the trailer".to_string(),
            n => n.to_string(),
        })
        .collect();
    if v.mismatched.len() > SHOWN {
        nums.push(format!("and {} more", v.mismatched.len() - SHOWN));
    }
    format!(
        "{} of {} objects read back differently from the input: {}",
        v.mismatched.len(),
        v.objects,
        nums.join(", ")
    )
}

/// Writes `data` next to `path` and renames it into place, so a reader
/// never sees a partial file.
fn write_atomically(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp_name = format!(".{name}.nupdf-{}.tmp", std::process::id());
    let tmp = dir.map_or_else(|| PathBuf::from(&tmp_name), |d| d.join(&tmp_name));
    let written = std::fs::write(&tmp, data).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
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

/// MuPDF's permission names (`Document.hasPermission`).
const PERMISSIONS: [&str; 8] = [
    "print",
    "edit",
    "copy",
    "annotate",
    "form",
    "accessibility",
    "assemble",
    "print-hq",
];

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
        permissions: PERMISSIONS
            .iter()
            .map(|p| (p.to_string(), doc.has_permission(p).into()))
            .collect(),
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
