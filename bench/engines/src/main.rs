// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! One operation on one file with one library, timed in process:
//!
//!     nupdf-engines-bench <op> <nupdf|pdf_oxide> <file> <runs>
//!
//! The file is read into memory first. Each run then opens the document
//! from those bytes and does the operation on every page, so that neither
//! library is timed on what an earlier run left in its caches. One run
//! that must succeed comes first: its output is checked (the units below)
//! and not timed. The median of the `runs` timed ones is reported, as one
//! JSON line: `{"ok":true,"ms":..,"units":..,"pages":..}`, or
//! `{"ok":false,"error":..}` when the library returned an error on the
//! document or a page, or panicked.
//!
//! Units: `open` the page count; `pages` the pages whose MediaBox and Rotate
//! were read; `text` and `text-fast` non-whitespace characters; `render`
//! pages with a pixel that is not white; `tables` tables; `annots`
//! annotations other than links, popups and widgets.

use std::hint::black_box;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Instant;

use serde_json::json;

/// nupdf's CLI runs on a stack this size, as content streams nest deep.
const STACK: usize = 1 << 30;

/// The largest page either library renders, in pixels: nupdf's CLI default.
const MAX_PIXELS: u64 = 100_000_000;

/// Whether a run's output is counted (the first run) or only kept alive
/// for `black_box` (the timed ones).
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Check,
    Timed,
}

struct Done {
    units: u64,
    pages: u64,
}

fn non_whitespace(s: &str) -> u64 {
    s.chars().filter(|c| !c.is_whitespace()).count() as u64
}

/// Whether premultiplied or plain samples of `n` channels (the last one
/// alpha when `alpha`) hold a pixel that is not white.
fn has_ink(samples: &[u8], n: usize, alpha: bool) -> bool {
    if n == 0 {
        return false;
    }
    samples.chunks_exact(n).any(|px| {
        let (color, a) = if alpha {
            (&px[..n - 1], px[n - 1])
        } else {
            (px, 255)
        };
        a > 0 && color.iter().any(|&c| c != 255)
    })
}

mod nupdf_side {
    use super::*;
    use nupdf::Document;
    use nupdf::geometry::Matrix;
    use nupdf::stext::{Block, Options, flags};

    fn open(bytes: &Arc<[u8]>) -> Result<(Document, i32), String> {
        let doc = Document::open(bytes.clone()).map_err(|e| format!("{e:?}"))?;
        let n = doc.page_count().map_err(|e| e.0)?;
        Ok((doc, n))
    }

    fn tables(blocks: &[Block]) -> u64 {
        blocks
            .iter()
            .map(|b| match b {
                Block::Struct(s) if s.standard == "Table" => 1,
                Block::Struct(s) => tables(&s.blocks),
                _ => 0,
            })
            .sum()
    }

    pub fn run(op: &str, bytes: &Arc<[u8]>, mode: Mode) -> Result<Done, String> {
        let (doc, n) = open(bytes)?;
        let indices: Vec<i32> = (0..n).collect();
        let pages = n as u64;
        let units = match op {
            "open" => pages,
            "pages" => {
                let mut read = 0;
                for i in 0..n {
                    let page = doc.page(i).map_err(|e| e.0)?;
                    let media = doc.inherited(&page, b"MediaBox").map_err(|e| e.0)?;
                    let rotate = doc.inherited(&page, b"Rotate").map_err(|e| e.0)?;
                    black_box((media, rotate));
                    read += 1;
                }
                read
            }
            "text" | "text-fast" => {
                let mut text = String::new();
                for page in nupdf::parallel::extract_pages(&doc, &indices, Options::default(), 1) {
                    text.push_str(&page?.text());
                    text.push('\x0c');
                }
                let units = if mode == Mode::Check {
                    non_whitespace(&text)
                } else {
                    0
                };
                black_box(text);
                units
            }
            "tables" => {
                let opts = Options {
                    flags: Options::default().flags | flags::TABLE_HUNT | flags::COLLECT_VECTORS,
                    ..Options::default()
                };
                let mut found = 0;
                for page in nupdf::parallel::extract_pages(&doc, &indices, opts, 1) {
                    found += tables(&page?.blocks);
                }
                found
            }
            "annots" => {
                use nupdf::annot as a;
                let mut found = 0;
                for i in 0..n {
                    let page = doc.page(i).map_err(|e| e.0)?;
                    for obj in a::page_annots(&doc, &page).0 {
                        black_box((
                            a::type_name(&doc, &obj),
                            a::contents(&doc, &obj),
                            a::rect(&doc, &obj, Matrix::IDENTITY),
                            a::author(&doc, &obj),
                        ));
                        found += 1;
                    }
                }
                found
            }
            "render" => {
                let job = |_: usize,
                           doc: &Document,
                           page: &Result<nupdf::Object, String>,
                           store|
                 -> (Result<bool, String>, _) {
                    let (rendered, store) = match page {
                        Ok(page) => nupdf::draw::render_page_with(
                            doc,
                            page,
                            Matrix::IDENTITY,
                            MAX_PIXELS,
                            None,
                            store,
                            true,
                            // Page contents only, as pdf_oxide renders here
                            // (render_annotations: false).
                            nupdf::page_run::Parts::CONTENTS,
                        ),
                        Err(e) => (Err(e.clone()), store),
                    };
                    let ink = rendered.map(|r| {
                        let ink =
                            mode == Mode::Check && has_ink(&r.pix.samples, r.pix.n, r.pix.alpha);
                        black_box(r);
                        ink
                    });
                    (ink, store)
                };
                let mut inked = 0;
                for page in nupdf::parallel::run_pages(&doc, &indices, 1, &job) {
                    inked += u64::from(page?);
                }
                inked
            }
            _ => return Err(format!("unknown operation {op}")),
        };
        black_box(&doc);
        Ok(Done { units, pages })
    }
}

mod oxide_side {
    use super::*;
    use pdf_oxide::PdfDocument;
    use pdf_oxide::rendering::{ImageFormat, PageRenderer, RenderOptions};

    pub fn run(op: &str, bytes: Vec<u8>, mode: Mode) -> Result<Done, String> {
        let s = |e: pdf_oxide::error::Error| e.to_string();
        let doc = PdfDocument::from_bytes(bytes).map_err(s)?;
        let n = doc.page_count().map_err(s)?;
        let pages = n as u64;
        let units = match op {
            "open" => pages,
            "pages" => {
                for i in 0..n {
                    black_box((
                        doc.get_page_media_box(i).map_err(s)?,
                        doc.get_page_rotation(i).map_err(s)?,
                    ));
                }
                pages
            }
            "text" | "text-fast" => {
                let options = pdf_oxide::converters::ConversionOptions {
                    extract_tables: op == "text",
                    ..Default::default()
                };
                let mut text = String::new();
                for i in 0..n {
                    text.push_str(&doc.extract_text_with_options(i, &options).map_err(s)?);
                    text.push('\x0c');
                }
                let units = if mode == Mode::Check {
                    non_whitespace(&text)
                } else {
                    0
                };
                black_box(text);
                units
            }
            "tables" => {
                let mut found = 0;
                for i in 0..n {
                    found += doc.extract_tables(i).map_err(s)?.len() as u64;
                }
                found
            }
            "annots" => {
                let mut found = 0;
                for i in 0..n {
                    for a in doc.get_annotations(i).map_err(s)? {
                        let kind = a.subtype.as_deref();
                        if !matches!(kind, Some("Link" | "Popup" | "Widget")) {
                            found += 1;
                        }
                        black_box(a);
                    }
                }
                found
            }
            "render" => {
                let mut options = RenderOptions::with_dpi(72);
                options.format = ImageFormat::RawRgba8;
                // nupdf draws page contents only.
                options.render_annotations = false;
                options.max_output_pixels = MAX_PIXELS;
                let mut renderer = PageRenderer::new(options);
                let mut inked = 0;
                for i in 0..n {
                    let img = renderer.render_page(&doc, i).map_err(s)?;
                    if mode == Mode::Check && has_ink(&img.data, 4, true) {
                        inked += 1;
                    }
                    black_box(img);
                }
                inked
            }
            _ => return Err(format!("unknown operation {op}")),
        };
        black_box(&doc);
        Ok(Done { units, pages })
    }
}

/// One run, with a panic caught as an error.
fn once(op: &str, lib: &str, bytes: &Arc<[u8]>, mode: Mode) -> Result<Done, String> {
    // pdf_oxide takes its bytes by value: the copy is made before the
    // clock starts, by the caller of `timed`.
    catch_unwind(AssertUnwindSafe(|| match lib {
        "nupdf" => nupdf_side::run(op, bytes, mode),
        "pdf_oxide" => oxide_side::run(op, bytes.to_vec(), mode),
        _ => Err(format!("unknown library {lib}")),
    }))
    .unwrap_or_else(|p| {
        let msg = p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        Err(format!("panic: {msg}"))
    })
}

/// A timed run: the clock covers opening the document through the last
/// page, and not pdf_oxide's copy of the bytes.
fn timed(op: &str, lib: &str, bytes: &Arc<[u8]>) -> Result<f64, String> {
    let (start, result) = if lib == "pdf_oxide" {
        let copy = bytes.to_vec();
        let start = Instant::now();
        let r = catch_unwind(AssertUnwindSafe(|| oxide_side::run(op, copy, Mode::Timed)))
            .unwrap_or_else(|_| Err("panic".into()));
        (start, r)
    } else {
        let start = Instant::now();
        (start, once(op, lib, bytes, Mode::Timed))
    };
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    black_box(result?);
    Ok(elapsed)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    }
}

fn bench(op: &str, lib: &str, bytes: Arc<[u8]>, runs: usize) -> serde_json::Value {
    let first = match once(op, lib, &bytes, Mode::Check) {
        Ok(d) => d,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let mut times = Vec::new();
    for _ in 0..runs.max(1) {
        match timed(op, lib, &bytes) {
            Ok(t) => times.push(t),
            Err(e) => return json!({ "ok": false, "error": e }),
        }
    }
    json!({
        "ok": true,
        "ms": median(times.clone()),
        "runs_ms": times,
        "units": first.units,
        "pages": first.pages,
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: nupdf-engines-bench <op> <nupdf|pdf_oxide> <file> <runs>");
        std::process::exit(2);
    }
    let (op, lib, file) = (args[1].clone(), args[2].clone(), args[3].clone());
    let runs: usize = args[4].parse().unwrap_or(3);
    let bytes: Arc<[u8]> = match std::fs::read(&file) {
        Ok(b) => b.into(),
        Err(e) => {
            println!("{}", json!({ "ok": false, "error": e.to_string() }));
            return;
        }
    };
    let worker = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || bench(&op, &lib, bytes, runs));
    let out = match worker.map(|w| w.join()) {
        Ok(Ok(v)) => v,
        _ => json!({ "ok": false, "error": "panic" }),
    };
    println!("{out}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ink_is_any_pixel_not_white() {
        assert!(!has_ink(&[255; 6], 3, false));
        assert!(has_ink(&[255, 255, 255, 0, 255, 255], 3, false));
        // Transparent pixels are no ink, whatever their colour.
        assert!(!has_ink(&[255, 255, 255, 255, 0, 0, 0, 0], 4, true));
        assert!(has_ink(&[10, 10, 10, 255], 4, true));
        assert!(!has_ink(&[], 0, false));
    }

    #[test]
    fn medians() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(vec![4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    fn minimal_pdf() -> Arc<[u8]> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../corpus/smoke/stext.pdf");
        std::fs::read(path).unwrap().into()
    }

    #[test]
    fn both_libraries_agree_on_a_smoke_file() {
        let bytes = minimal_pdf();
        for op in ["open", "pages", "text", "render"] {
            let a = once(op, "nupdf", &bytes, Mode::Check).unwrap();
            let b = once(op, "pdf_oxide", &bytes, Mode::Check).unwrap();
            assert_eq!(a.pages, b.pages, "{op}");
            assert!(a.units > 0 && b.units > 0, "{op}");
        }
        assert!(timed("text", "pdf_oxide", &bytes).unwrap() > 0.0);
    }

    #[test]
    fn errors_are_reported_not_raised() {
        let junk: Arc<[u8]> = b"not a pdf".to_vec().into();
        let v = bench("text", "pdf_oxide", junk, 1);
        assert_eq!(v["ok"], false);
        assert!(once("nope", "nupdf", &minimal_pdf(), Mode::Check).is_err());
    }
}
