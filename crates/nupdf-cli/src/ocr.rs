// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `ocr`: scanned pages made searchable. Each page is drawn, Tesseract
//! (a subprocess) reads the image, and the words go on the page as an
//! invisible text layer (DECISIONS D50). `info` says which pages look
//! scanned: no text, and an image over most of the page.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nupdf::Document;
use nupdf::stext::{Block, Options};
use serde_json::{Value, json};

use super::{Exit, Failure, FileFacts, write_document};
use crate::commands::WriteArgs;

/// Images covering at least this share of a page with no text make it a
/// scanned page.
const SCANNED_COVER: f32 = 0.5;
/// Documents with more pages than this are not looked at page by page in
/// `info`.
pub const MAX_INFO_PAGES: i32 = 2000;

/// What a page holds: its characters, and how much of it images cover.
pub struct PageText {
    pub chars: usize,
    pub image_cover: f32,
}

impl PageText {
    pub fn scanned(&self) -> bool {
        self.chars == 0 && self.image_cover >= SCANNED_COVER
    }
}

fn count(blocks: &[Block], chars: &mut usize, images: &mut Vec<nupdf::geometry::Rect>) {
    for b in blocks {
        match b {
            Block::Text(t) => *chars += t.lines.iter().map(|l| l.chars.len()).sum::<usize>(),
            Block::Image(i) => images.push(i.bbox),
            Block::Struct(s) => count(&s.blocks, chars, images),
            _ => {}
        }
    }
}

/// Each page's text and image cover, or why it could not be read. With
/// `quick`, a page's run stops at its first character, so `chars` is 1 on a
/// page with text: whether a page has text and whether it looks scanned
/// are the same, the warnings the rest of the page would give are not.
pub fn scan(doc: &Document, indices: &[i32], quick: bool) -> Vec<Result<PageText, String>> {
    let mut flags = nupdf::stext::flags::PRESERVE_IMAGES;
    if quick {
        flags |= nupdf::stext::flags::STOP_AT_TEXT;
    }
    let opts = Options {
        flags,
        ..Options::default()
    };
    // A long document's pages on threads, as the commands that read
    // text take them unasked (DECISIONS D53).
    let threads = crate::threads_for(None, indices.len());
    nupdf::parallel::map(doc, indices, opts, threads, &|i, page| {
        let page = page?;
        let mut chars = 0;
        let mut images = Vec::new();
        count(&page.blocks, &mut chars, &mut images);
        let area = doc
            .page(indices[i])
            .ok()
            .and_then(|p| doc.page_transform(&p).ok())
            .map(|(b, _)| (b.x1 - b.x0) * (b.y1 - b.y0))
            .filter(|a| *a > 0.0);
        let cover = area.map_or(0.0, |area| {
            let covered: f32 = images
                .iter()
                .map(|r| (r.x1 - r.x0).max(0.0) * (r.y1 - r.y0).max(0.0))
                .sum();
            (covered / area).min(1.0)
        });
        Ok(PageText {
            chars,
            image_cover: cover,
        })
    })
}

/// For `info`: pages with text, and the scanned pages to run `ocr` on.
pub fn info_text(doc: &Document) -> Value {
    let Ok(n) = doc.page_count() else {
        return Value::Null;
    };
    if n > MAX_INFO_PAGES {
        return json!({ "checked": false });
    }
    let indices: Vec<i32> = (0..n).collect();
    // On `info`'s own copy, so the warnings left out are not missed.
    let pages = scan(doc, &indices, true);
    let with_text = pages
        .iter()
        .filter(|p| p.as_ref().is_ok_and(|p| p.chars > 0))
        .count();
    let scanned: Vec<i32> = pages
        .iter()
        .enumerate()
        .filter(|(_, p)| p.as_ref().is_ok_and(PageText::scanned))
        .map(|(i, _)| i as i32 + 1)
        .collect();
    let mut v = json!({ "checked": true, "pages_with_text": with_text, "scanned_pages": scanned });
    if !scanned.is_empty() {
        v["next"] =
            json!("ocr <file> -o <out> (Tesseract reads the scanned pages; --all for every page)");
    }
    v
}

fn unavailable(tesseract: &Path, why: &str) -> Failure {
    Failure {
        exit: Exit::Input,
        code: "ocr.unavailable",
        message: format!(
            "cannot run {} ({why}): install Tesseract (macOS: brew install tesseract; Debian: apt install tesseract-ocr) or pass --tesseract PATH",
            tesseract.display()
        ),
    }
}

/// Runs `cmd` to its end, or kills it after `timeout`.
fn run(mut cmd: Command, timeout: Duration) -> Result<std::process::Output, String> {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    let id = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    loop {
        if let Ok(out) = rx.recv_timeout(Duration::from_millis(100)) {
            return out.map_err(|e| e.to_string());
        }
        if start.elapsed() > timeout {
            let _ = Command::new("kill").arg("-9").arg(id.to_string()).status();
            return Err(format!("timed out after {}s", timeout.as_secs()));
        }
    }
}

/// The options `ocr` takes besides the file and where to write.
pub struct OcrArgs<'a> {
    pub pages: Option<&'a str>,
    pub all: bool,
    pub lang: &'a str,
    pub dpi: f32,
    pub tesseract: &'a Path,
    pub min_confidence: f32,
    pub timeout: u64,
}

pub fn ocr(
    doc: &Document,
    facts: &FileFacts,
    args: &OcrArgs,
    write: &WriteArgs,
    password: Option<&str>,
) -> Result<Value, Failure> {
    let plan = write.plan(nupdf::write::WriteOptions::default(), password)?;
    let version = run(
        {
            let mut c = Command::new(args.tesseract);
            c.arg("--version");
            c
        },
        Duration::from_secs(30),
    )
    .map_err(|e| unavailable(args.tesseract, &e))?;
    let version = String::from_utf8_lossy(&version.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    let count = doc.page_count().map_err(|e| Failure {
        exit: Exit::Input,
        code: "input.page_tree",
        message: e.0,
    })?;
    let indices: Vec<i32> = match args.pages {
        Some(spec) => nupdf::rearrange::parse_page_ranges(spec, count, doc),
        None => (0..count).collect(),
    };
    nupdf::write::check_document(doc);
    // Unless asked, only the pages that look scanned.
    let scanned = scan(doc, &indices, false);
    let dir = std::env::temp_dir().join(format!("nupdf-ocr-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| Failure {
        exit: Exit::Input,
        code: "output.unwritable",
        message: format!("cannot make {}: {e}", dir.display()),
    })?;
    let mut font = None;
    let mut pages = Vec::new();
    for (k, &index) in indices.iter().enumerate() {
        let mut entry = json!({ "page": index + 1 });
        let looks = scanned[k].as_ref().is_ok_and(PageText::scanned);
        if !args.all && args.pages.is_none() && !looks {
            entry["skipped"] = "has text or no page-sized image".into();
            pages.push(entry);
            continue;
        }
        let ctm = nupdf::geometry::Matrix::scale(args.dpi / 72.0, args.dpi / 72.0);
        let png = doc
            .page(index)
            .map_err(|e| e.0)
            .and_then(|page| nupdf::draw::render_page(doc, &page, ctm, 400_000_000))
            .and_then(|r| {
                r.pix
                    .to_pixmap(Some(nupdf::colorspace::ColorSpace::Rgb))
                    .png_at(crate::render::PNG_LEVEL)
            });
        let png = match png {
            Ok(p) => p,
            Err(e) => {
                entry["error"] = e.into();
                pages.push(entry);
                continue;
            }
        };
        let image = dir.join(format!("page-{:04}.png", index + 1));
        if let Err(e) = std::fs::write(&image, png) {
            entry["error"] = e.to_string().into();
            pages.push(entry);
            continue;
        }
        let mut cmd = Command::new(args.tesseract);
        cmd.arg(&image)
            .arg("stdout")
            .args(["-l", args.lang, "--dpi"])
            .arg(format!("{}", args.dpi.round()))
            .arg("tsv");
        let out = run(cmd, Duration::from_secs(args.timeout));
        let _ = std::fs::remove_file(&image);
        let out = match out {
            Ok(o) if o.status.success() => o,
            Ok(o) => {
                entry["error"] = format!(
                    "tesseract failed: {}",
                    String::from_utf8_lossy(&o.stderr)
                        .lines()
                        .last()
                        .unwrap_or_default()
                )
                .into();
                pages.push(entry);
                continue;
            }
            Err(e) => {
                entry["error"] = e.into();
                pages.push(entry);
                continue;
            }
        };
        let words: Vec<nupdf::ocr::Word> =
            nupdf::ocr::parse_tsv(&String::from_utf8_lossy(&out.stdout))
                .into_iter()
                .filter(|w| w.confidence >= args.min_confidence)
                .collect();
        entry["words"] = words.len().into();
        entry["chars"] = words
            .iter()
            .map(|w| w.text.chars().count())
            .sum::<usize>()
            .into();
        if !words.is_empty() {
            let mean = words.iter().map(|w| w.confidence).sum::<f32>() / words.len() as f32;
            entry["mean_confidence"] = json!((f64::from(mean) * 10.0).round() / 10.0);
            let f = *font.get_or_insert_with(|| nupdf::ocr::add_font(doc));
            let page_ctm = doc
                .page(index)
                .ok()
                .and_then(|p| doc.page_transform(&p).ok())
                .map_or(nupdf::geometry::Matrix::IDENTITY, |t| t.1);
            let layer = nupdf::ocr::text_layer(&words, args.dpi, page_ctm, b"FOCR");
            if let Err(e) = nupdf::ocr::add_layer(doc, index, f, b"FOCR", layer) {
                entry["error"] = e.into();
            }
        }
        pages.push(entry);
    }
    let _ = std::fs::remove_dir(&dir);
    let body = json!({
        "tesseract": version,
        "lang": args.lang,
        "dpi": args.dpi,
        "pages": pages,
    });
    write_document(doc, facts, &plan, "nupdf.ocr/v1", body)
}
