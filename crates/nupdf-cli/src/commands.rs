// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! The structural editing commands: `meta`, `split`, page edits, `encrypt`
//! and `decrypt`. Each edits the document's overlay, then writes it as
//! `clean` does with its defaults, read back and verified first.

use std::path::{Path, PathBuf};

use clap::Args;
use nupdf::Document;

use super::{Clean, Exit, Failure, FileFacts, report, write_document};

/// Where an edit goes: `-o` and `--dry-run`, and the read-back check.
#[derive(Debug, Args)]
pub struct WriteArgs {
    /// Where to write, replacing the file atomically.
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Report what would be written, and write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip reading the output back.
    #[arg(long)]
    pub no_verify: bool,
    /// Append the changes to the file as a new revision, leaving its bytes
    /// as they were, instead of rewriting it.
    #[arg(long)]
    pub incremental: bool,
}

impl WriteArgs {
    /// The plan for writing with `options`, once -o or --dry-run is given.
    pub fn plan<'a>(
        &'a self,
        options: nupdf::write::WriteOptions,
        password: Option<&'a str>,
    ) -> Result<Clean<'a>, Failure> {
        if self.output.is_none() && !self.dry_run {
            return Err(Failure {
                exit: Exit::Usage,
                code: "usage.missing_output",
                message: "an edit needs -o or --dry-run".into(),
            });
        }
        let options = nupdf::write::WriteOptions {
            incremental: self.incremental,
            ..options
        };
        Ok(Clean {
            options,
            output: self.output.as_deref(),
            apply: !self.dry_run,
            verify: !self.no_verify,
            password,
            pages: None,
        })
    }
}

fn input_failure(code: &'static str, message: String) -> Failure {
    Failure {
        exit: Exit::Input,
        code,
        message,
    }
}

/// `meta`: the /Info entries, or the document with them changed.
pub fn meta(
    doc: &Document,
    facts: &FileFacts,
    set: &[String],
    strip: bool,
    write: &WriteArgs,
    password: Option<&str>,
) -> Result<serde_json::Value, Failure> {
    if set.is_empty() && !strip {
        return Ok(report("nupdf.meta/v1", facts, doc, info_json(doc)));
    }
    let mut pairs = Vec::new();
    for entry in set {
        let Some((key, value)) = entry.split_once('=') else {
            return Err(Failure {
                exit: Exit::Usage,
                code: "usage.bad_value",
                message: format!("--set takes KEY=VALUE, got {entry:?}"),
            });
        };
        pairs.push((key, value));
    }
    let plan = write.plan(nupdf::write::WriteOptions::default(), password)?;
    nupdf::write::check_document(doc);
    if strip {
        nupdf::edit::strip_metadata(doc);
    }
    for (key, value) in pairs {
        nupdf::edit::set_metadata(doc, key, value);
    }
    let mut body = info_json(doc);
    if let serde_json::Value::Object(b) = &mut body {
        b.insert("stripped".into(), strip.into());
    }
    write_document(doc, facts, &plan, "nupdf.meta/v1", body)
}

/// The /Info entries as text, and whether the catalog has XMP metadata.
fn info_json(doc: &Document) -> serde_json::Value {
    let trailer = doc.trailer();
    let info = trailer.get(b"Info").map(|i| doc.resolve(i));
    let mut entries = serde_json::Map::new();
    if let Some(nupdf::Object::Dict(d)) = &info {
        for (key, value) in d.iter() {
            let value = match doc.resolve(value) {
                nupdf::Object::String(s) => nupdf::text_string::text_string(&s).into(),
                other => nupdf::print::to_string(&other, nupdf::print::Options::default()).into(),
            };
            entries.insert(String::from_utf8_lossy(key).into_owned(), value);
        }
    }
    let root = trailer.get(b"Root").cloned().unwrap_or(nupdf::Object::Null);
    let xmp = !matches!(doc.dict_get(&root, b"Metadata"), nupdf::Object::Null);
    serde_json::json!({ "info": entries, "xmp": xmp })
}

/// Page edits on `pages`: keep or drop pages, then rotate some.
pub struct PageEdits<'a> {
    pub select: Option<&'a str>,
    pub delete: Option<&'a str>,
    pub rotate: Option<i32>,
    pub rotate_pages: Option<&'a str>,
}

impl PageEdits<'_> {
    pub fn any(&self) -> bool {
        self.select.is_some() || self.delete.is_some() || self.rotate.is_some()
    }
}

/// `pages` with edits: the pages kept or dropped (as `clean --pages`), then
/// rotated.
pub fn edit_pages(
    doc: &Document,
    facts: &FileFacts,
    edits: &PageEdits,
    write: &WriteArgs,
    password: Option<&str>,
) -> Result<serde_json::Value, Failure> {
    if let Some(degrees) = edits.rotate
        && degrees % 90 != 0
    {
        return Err(Failure {
            exit: Exit::Usage,
            code: "usage.bad_value",
            message: format!("--rotate takes a multiple of 90, got {degrees}"),
        });
    }
    let plan = write.plan(nupdf::write::WriteOptions::default(), password)?;
    let tree = |e: String| input_failure("input.page_tree", e);
    nupdf::write::check_document(doc);
    let count = doc.page_count().map_err(|e| tree(e.0))?;
    let kept = match (edits.select, edits.delete) {
        (Some(spec), _) => Some(nupdf::rearrange::parse_page_ranges(spec, count, doc)),
        (None, Some(spec)) => {
            let gone = nupdf::rearrange::parse_page_ranges(spec, count, doc);
            Some((0..count).filter(|p| !gone.contains(p)).collect())
        }
        (None, None) => None,
    };
    if let Some(pages) = &kept {
        nupdf::rearrange::rearrange_pages(doc, pages)
            .map_err(|e| tree(nupdf::diag::report::strip(&e.0).to_string()))?;
    }
    if let Some(degrees) = edits.rotate {
        let count = doc.page_count().map_err(|e| tree(e.0))?;
        let pages = match edits.rotate_pages {
            Some(spec) => nupdf::rearrange::parse_page_ranges(spec, count, doc),
            None => (0..count).collect(),
        };
        nupdf::edit::rotate_pages(doc, &pages, degrees).map_err(tree)?;
    }
    let body = serde_json::json!({
        "kept": kept.map(|p| p.iter().map(|i| i + 1).collect::<Vec<_>>()),
        "rotated": edits.rotate,
        "page_count": doc.page_count().ok(),
    });
    write_document(doc, facts, &plan, "nupdf.pages/v1", body)
}

/// A document opened as every command opens it, with its file facts.
type Opened = Result<(Document, FileFacts), Failure>;

/// `split`: one output per range of pages, each written as `clean --pages`
/// writes it, from a fresh copy of the input.
#[allow(clippy::too_many_arguments)]
pub fn split(
    path: &Path,
    open: &dyn Fn(&Path) -> Opened,
    out_dir: &Path,
    per_file: usize,
    ranges: Option<&str>,
    dry_run: bool,
    no_verify: bool,
    password: Option<&str>,
) -> Result<serde_json::Value, Failure> {
    let (doc, facts) = open(path)?;
    let tree = |e: String| input_failure("input.page_tree", e);
    let count = doc.page_count().map_err(|e| tree(e.0))?;
    let specs: Vec<String> = match ranges {
        Some(r) => r.split(';').map(str::to_string).collect(),
        None => {
            let per = per_file.max(1) as i32;
            (0..count)
                .step_by(per as usize)
                .map(|first| format!("{}-{}", first + 1, (first + per).min(count)))
                .collect()
        }
    };
    if !dry_run {
        std::fs::create_dir_all(out_dir).map_err(|e| Failure {
            exit: Exit::Input,
            code: "output.unwritable",
            message: format!("cannot create {}: {e}", out_dir.display()),
        })?;
    }
    let stem = path
        .file_stem()
        .map_or_else(|| "part".into(), |s| s.to_string_lossy().into_owned());
    let width = specs.len().to_string().len();
    let mut parts = Vec::new();
    for (i, spec) in specs.iter().enumerate() {
        let (part, _) = open(path)?;
        nupdf::write::check_document(&part);
        let pages = nupdf::rearrange::parse_page_ranges(spec, count, &part);
        nupdf::rearrange::rearrange_pages(&part, &pages)
            .map_err(|e| tree(nupdf::diag::report::strip(&e.0).to_string()))?;
        let file = out_dir.join(format!("{stem}-{:0width$}.pdf", i + 1));
        let plan = Clean {
            options: nupdf::write::WriteOptions::default(),
            output: Some(&file),
            apply: !dry_run,
            verify: !no_verify,
            password,
            pages: None,
        };
        let (output, verification) = super::save_checked(&part, &plan)?;
        parts.push(serde_json::json!({
            "pages": pages.iter().map(|p| p + 1).collect::<Vec<_>>(),
            "output": output,
            "verification": verification,
        }));
    }
    let body = serde_json::json!({ "applied": !dry_run, "parts": parts });
    Ok(report("nupdf.split/v1", &facts, &doc, body))
}

/// `encrypt` and `decrypt`: `clean -E` and `clean -D` under their own names.
pub fn encryption(
    doc: &Document,
    facts: &FileFacts,
    encrypt: nupdf::write::Encrypt,
    write: &WriteArgs,
    password: Option<&str>,
) -> Result<serde_json::Value, Failure> {
    let schema = match encrypt {
        nupdf::write::Encrypt::Remove => "nupdf.decrypt/v1",
        _ => "nupdf.encrypt/v1",
    };
    let body = serde_json::json!({ "encryption": super::encryption_json(&encrypt) });
    let options = nupdf::write::WriteOptions {
        encrypt,
        ..Default::default()
    };
    let plan = write.plan(options, password)?;
    write_document(doc, facts, &plan, schema, body)
}

/// `fz_is_page_range`: only digits, `N`, `-` and `,`.
fn is_page_range(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'N' | b'-' | b','))
}

/// `merge`: the pages of each input, in its range, grafted into a new
/// document, as `mutool merge` does. An input that fails is reported and
/// skipped, keeping the pages it gave before failing; only when none can
/// be opened does the command fail.
pub fn merge(
    args: &[String],
    open: &dyn Fn(&Path) -> Opened,
    write: &WriteArgs,
    password: Option<&str>,
) -> Result<serde_json::Value, Failure> {
    let mut inputs: Vec<(PathBuf, &str)> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let path = PathBuf::from(&args[i]);
        i += 1;
        let range = match args.get(i) {
            Some(r) if is_page_range(r) => {
                i += 1;
                r.as_str()
            }
            _ => "1-N",
        };
        inputs.push((path, range));
    }
    let plan = write.plan(nupdf::write::WriteOptions::default(), None)?;
    let dst = Document::create().map_err(|e| Failure {
        exit: Exit::Internal,
        code: "internal.create",
        message: e.message,
    })?;
    let mut merger = nupdf::merge::Merger::new(&dst);
    let mut first: Option<FileFacts> = None;
    let mut first_failure: Option<Failure> = None;
    let mut reports = Vec::new();
    for (path, range) in &inputs {
        let mut merged = Vec::new();
        let mut diagnostics = serde_json::Value::Array(Vec::new());
        let outcome = merge_one(
            &mut merger,
            path,
            range,
            open,
            password,
            &mut merged,
            &mut diagnostics,
        );
        let (doc_id, error) = match outcome {
            Ok(facts) => {
                let id = facts.doc_id.clone();
                first.get_or_insert(facts);
                (Some(id), None)
            }
            Err((facts, failure)) => {
                let id = facts.as_ref().map(|f| f.doc_id.clone());
                if let Some(facts) = facts {
                    first.get_or_insert(facts);
                }
                dst.diagnostics().warn(
                    "merge.input_failed",
                    format!(
                        "cannot merge document '{}': {}",
                        path.display(),
                        failure.message
                    ),
                );
                let error = serde_json::json!({ "code": failure.code, "message": failure.message });
                first_failure.get_or_insert(failure);
                (id, Some(error))
            }
        };
        reports.push(serde_json::json!({
            "path": path.display().to_string(),
            "doc_id": doc_id,
            "pages": merged.iter().map(|p| p + 1).collect::<Vec<_>>(),
            "error": error,
            "diagnostics": diagnostics,
        }));
    }
    let Some(facts) = first else {
        return Err(first_failure.unwrap_or(Failure {
            exit: Exit::Usage,
            code: "usage.missing_input",
            message: "merge needs at least one input".into(),
        }));
    };
    let body = serde_json::json!({
        "inputs": reports,
        "page_count": dst.page_count().ok(),
    });
    write_document(&dst, &facts, &plan, "nupdf.merge/v1", body)
}

/// `merge_range` for one input: its facts once it opened, the pages merged
/// in `merged` (0-based), whether or not it failed partway, and what was
/// noticed reading it in `diagnostics`.
fn merge_one(
    merger: &mut nupdf::merge::Merger,
    path: &Path,
    range: &str,
    open: &dyn Fn(&Path) -> Opened,
    password: Option<&str>,
    merged: &mut Vec<i32>,
    diagnostics: &mut serde_json::Value,
) -> Result<FileFacts, (Option<FileFacts>, Failure)> {
    let (src, facts) = open(path).map_err(|f| (None, f))?;
    let result = graft_input(merger, &src, range, password, merged);
    *diagnostics = super::diagnostics(&src);
    match result {
        Ok(()) => Ok(facts),
        Err(failure) => Err((Some(facts), failure)),
    }
}

fn graft_input(
    merger: &mut nupdf::merge::Merger,
    src: &Document,
    range: &str,
    password: Option<&str>,
    merged: &mut Vec<i32>,
) -> Result<(), Failure> {
    if src.needs_password() && password.is_none() {
        return Err(Failure {
            exit: Exit::Input,
            code: "input.needs_password",
            message: "the empty password does not open the document: pass --password".into(),
        });
    }
    let outcome = merger.merge_range(src, range);
    *merged = outcome.pages;
    match outcome.error {
        None => Ok(()),
        Some(e) => Err(input_failure(
            "input.page_tree",
            nupdf::diag::report::strip(&e).to_string(),
        )),
    }
}

/// What `sanitize` removes, from its flags.
pub fn sanitize_policy(
    keep: &[String],
    uri: &str,
    attachments: bool,
) -> Result<nupdf::sanitize::Policy, Failure> {
    let mut p = nupdf::sanitize::Policy {
        attachments,
        ..nupdf::sanitize::Policy::default()
    };
    for k in keep {
        match k.as_str() {
            "javascript" => p.javascript = false,
            "launch" => p.launch = false,
            "data" => p.data = false,
            "remote" => p.remote = false,
            "xfa" => p.xfa = false,
            other => {
                return Err(Failure {
                    exit: Exit::Usage,
                    code: "usage.bad_value",
                    message: format!(
                        "--keep takes javascript, launch, data, remote or xfa, got {other:?}"
                    ),
                });
            }
        }
    }
    p.uri = match uri {
        "keep" => nupdf::sanitize::UriPolicy::Keep,
        "remove" => nupdf::sanitize::UriPolicy::Remove,
        other => match other.strip_prefix("allow:") {
            Some(hosts) => nupdf::sanitize::UriPolicy::Allow(
                hosts
                    .split(',')
                    .map(str::trim)
                    .filter(|h| !h.is_empty())
                    .map(String::from)
                    .collect(),
            ),
            None => {
                return Err(Failure {
                    exit: Exit::Usage,
                    code: "usage.bad_value",
                    message: format!("--uri takes keep, remove or allow:HOST,..., got {other:?}"),
                });
            }
        },
    };
    Ok(p)
}

fn findings_json(found: &[nupdf::sanitize::Finding]) -> serde_json::Value {
    found
        .iter()
        .map(|f| {
            serde_json::json!({
                "object": f.object,
                "path": f.path,
                "kind": f.kind,
                "detail": f.detail,
            })
        })
        .collect()
}

/// `sanitize`: the document without what `policy` removes, checked by
/// scanning the output again before it is written.
pub fn sanitize(
    doc: &Document,
    facts: &FileFacts,
    policy: &nupdf::sanitize::Policy,
    write: &WriteArgs,
    password: Option<&str>,
) -> Result<serde_json::Value, Failure> {
    let plan = write.plan(nupdf::write::WriteOptions::default(), password)?;
    nupdf::write::check_document(doc);
    let removed = nupdf::sanitize::sanitize(doc, policy);
    let mut counts = serde_json::Map::new();
    for f in &removed {
        let n = counts
            .get(f.kind)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        counts.insert(f.kind.into(), (n + 1).into());
    }
    // The output, read back and scanned again: nothing may be left. A save
    // that said nothing is the one written: the same document saves the
    // same again, and a large one takes long to.
    let mut saved = None;
    let left = if plan.verify {
        let (bytes, said) = nupdf::diag::capture(|| nupdf::write::save(doc, &plan.options));
        let quiet = said.is_empty();
        doc.diagnostics().replay(said);
        let bytes = bytes.map_err(|e| input_failure("input.unloadable_object", e.message))?;
        if quiet {
            saved = Some(bytes.clone());
        }
        let out = Document::open(bytes).map_err(|e| {
            input_failure("verify.failed", format!("the output does not open: {e:?}"))
        })?;
        if let Some(pw) = password {
            let _ = out.authenticate(pw);
        }
        Some(nupdf::sanitize::scan(&out, policy))
    } else {
        None
    };
    if let Some(left) = &left
        && !left.is_empty()
    {
        return Err(Failure {
            exit: Exit::Verify,
            code: "verify.failed",
            message: format!(
                "the output still has {} things the policy removes, first {} at object {} {}",
                left.len(),
                left[0].kind,
                left[0].object,
                left[0].path
            ),
        });
    }
    let uri = match &policy.uri {
        nupdf::sanitize::UriPolicy::Keep => serde_json::json!("keep"),
        nupdf::sanitize::UriPolicy::Remove => serde_json::json!("remove"),
        nupdf::sanitize::UriPolicy::Allow(h) => serde_json::json!({ "allow": h }),
    };
    let body = serde_json::json!({
        "policy": {
            "javascript": policy.javascript,
            "launch": policy.launch,
            "data": policy.data,
            "remote": policy.remote,
            "xfa": policy.xfa,
            "attachments": policy.attachments,
            "uri": uri,
        },
        "removed": findings_json(&removed),
        "counts": counts,
        "clean": left.as_ref().map(Vec::is_empty),
    });
    super::write_saved(doc, facts, &plan, "nupdf.sanitize/v1", body, saved)
}

/// `flatten`: annotations and form fields drawn into their pages and
/// removed, as `pdf_bake_document` bakes them.
/// `forms --set` and `--toggle`: the fields filled as MuPDF's widget API
/// fills them with JavaScript off, their widgets' appearances made again,
/// and the document written as `clean` writes it. Sets go first, in the
/// order given, then toggles.
pub fn fill(
    doc: &Document,
    facts: &FileFacts,
    set: &[String],
    toggle: &[String],
    fill_first: bool,
    write: &WriteArgs,
    password: Option<&str>,
) -> Result<serde_json::Value, Failure> {
    use nupdf::fill::Edit;
    let mut edits = Vec::new();
    for entry in set {
        let Some((name, value)) = entry.split_once('=') else {
            return Err(Failure {
                exit: Exit::Usage,
                code: "usage.bad_value",
                message: format!("--set takes NAME=VALUE, got {entry:?}"),
            });
        };
        edits.push(Edit::Set {
            name: name.to_string(),
            value: value.to_string(),
        });
    }
    edits.extend(
        toggle
            .iter()
            .map(|name| Edit::Toggle { name: name.clone() }),
    );
    let plan = write.plan(nupdf::write::WriteOptions::default(), password)?;
    nupdf::write::check_document(doc);
    let filled = if fill_first {
        nupdf::fill::fill_first(doc, "nupdf")
    } else {
        nupdf::fill::fill(doc, &edits)
    }
    .map_err(|e| match e {
        nupdf::fill::FillError::UnknownField(_) => Failure {
            exit: Exit::Usage,
            code: "usage.unknown_field",
            message: e.to_string(),
        },
        nupdf::fill::FillError::Page(_) => Failure {
            exit: Exit::Input,
            code: "input.page_tree",
            message: e.to_string(),
        },
    })?;
    let fields: Vec<serde_json::Value> = filled
        .changes
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "op": c.op,
                "type": c.kind,
                "page": c.page + 1,
                "object": c.object,
                "old": c.old,
                "new": c.new,
                "changed": c.changed,
                "notes": c.notes,
            })
        })
        .collect();
    let made: Vec<serde_json::Value> = filled
        .made
        .iter()
        .map(|m| serde_json::json!({ "page": m.page + 1, "object": m.object, "error": m.error }))
        .collect();
    let body = serde_json::json!({
        // JavaScript is off: no keystroke, format, validate or calculate
        // script runs, as with `mutool run`.
        "scripts_run": false,
        "fields": fields,
        "appearances": made,
    });
    write_document(doc, facts, &plan, "nupdf.fill/v1", body)
}

pub fn flatten(
    doc: &Document,
    facts: &FileFacts,
    annots: bool,
    widgets: bool,
    write: &WriteArgs,
    password: Option<&str>,
) -> Result<serde_json::Value, Failure> {
    let plan = write.plan(nupdf::write::WriteOptions::default(), password)?;
    nupdf::write::check_document(doc);
    let baked = nupdf::bake::bake_document(doc, annots, widgets);
    let unmade: Vec<serde_json::Value> = baked
        .unmade
        .iter()
        .map(|(page, object, why)| serde_json::json!({ "page": page + 1, "object": object, "error": why }))
        .collect();
    let body = serde_json::json!({
        "annots": baked.annots,
        "widgets": baked.widgets,
        "pages": baked.pages,
        "unmade": unmade,
        "stopped": baked.stopped,
    });
    write_document(doc, facts, &plan, "nupdf.flatten/v1", body)
}
