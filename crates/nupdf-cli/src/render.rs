//! `render`: pages as pixels, as `mutool draw` paints them.

use std::path::Path;

use nupdf::Document;
use nupdf::draw::{self, Pix};
use nupdf::geometry::Matrix;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Png,
    /// Binary PPM (P6), samples as they are.
    Ppm,
}

impl Format {
    fn ext(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Ppm => "ppm",
        }
    }
}

pub fn ppm(pix: &Pix) -> Vec<u8> {
    let mut out = format!("P6\n{} {}\n255\n", pix.w, pix.h).into_bytes();
    out.extend_from_slice(&pix.samples);
    out
}

/// `x0,y0,x1,y1` in page space (points, the origin at the top left as
/// `bounds` gives it).
pub fn parse_region(s: &str) -> Result<nupdf::geometry::Rect, String> {
    let bad = || format!("bad region {s:?}: expected x0,y0,x1,y1");
    let v: Vec<f32> = s
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|x| x.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .map_err(|_| bad())?;
    let [x0, y0, x1, y1] = v[..] else {
        return Err(bad());
    };
    if !(x0 < x1 && y0 < y1) || v.iter().any(|x| !x.is_finite()) {
        return Err(bad());
    }
    Ok(nupdf::geometry::Rect::new(x0, y0, x1, y1))
}

/// How to render: resolution, an optional region, the pixel budget and the
/// threads pages run on.
pub struct Settings {
    pub dpi: f32,
    pub region: Option<nupdf::geometry::Rect>,
    pub max_pixels: u64,
    pub threads: usize,
    /// Color management, as `mutool draw` has it on unless `-N`.
    pub icc: bool,
    /// What of each page to draw: everything, as `mutool draw`, or the contents alone (`--no-annots`).
    pub parts: nupdf::draw::Parts,
}

/// Each page in `spec` (all pages without one), written into `dir` as
/// page-NNNN.ext, numbered from 1.
pub fn render(
    doc: &Document,
    spec: Option<&str>,
    dir: &Path,
    format: Format,
    settings: &Settings,
) -> Result<Value, String> {
    let count = doc.page_count().map_err(|e| e.0)?;
    let pages = match spec {
        Some(spec) => nupdf::rearrange::parse_page_ranges(spec, count, doc),
        None => (0..count).collect(),
    };
    let scale = settings.dpi / 72.0;
    let ctm = Matrix::scale(scale, scale);
    let region = settings
        .region
        .map(|r| nupdf::geometry::IRect::round(r.transform(ctm)));
    // Threads go to pages first, and those left over to bands of a page.
    let bands = (settings.threads / pages.len().max(1)).max(1);
    let pages_threads = settings.threads.min(pages.len().max(1));
    let job =
        |i: usize, doc: &Document, page: &Result<nupdf::Object, String>, store| -> (Value, _) {
            let (rendered, store) = match page {
                Ok(page) => draw::render_page_banded(
                    doc,
                    page,
                    ctm,
                    settings.max_pixels,
                    region,
                    store,
                    bands,
                    settings.icc,
                    settings.parts,
                ),
                Err(e) => (Err(e.clone()), store),
            };
            let index = pages[i];
            let path = dir.join(format!("page-{:04}.{}", index + 1, format.ext()));
            (page_entry(index, rendered, &path, format), store)
        };
    let out = nupdf::parallel::run_pages(doc, &pages, pages_threads, &job);
    Ok(json!({ "dpi": settings.dpi, "page_count": count, "pages": out }))
}

/// A page's report entry, its image written to `path`.
fn page_entry(
    index: i32,
    rendered: Result<draw::Rendered, String>,
    path: &Path,
    format: Format,
) -> Value {
    let mut entry = json!({ "page": index + 1 });
    match rendered {
        Ok(r) => {
            let bytes = match format {
                Format::Png => r
                    .pix
                    .to_pixmap(Some(nupdf::colorspace::ColorSpace::Rgb))
                    .png(),
                Format::Ppm => Ok(ppm(&r.pix)),
            };
            entry["width"] = r.pix.w.into();
            entry["height"] = r.pix.h.into();
            entry["x"] = r.pix.x.into();
            entry["y"] = r.pix.y.into();
            match bytes.and_then(|b| crate::write_file(path, &b).map_err(|e| e.to_string())) {
                Ok(()) => entry["file"] = path.display().to_string().into(),
                Err(e) => entry["error"] = e.into(),
            }
            if let Some(e) = r.error {
                entry["run_error"] = e.into();
            }
        }
        Err(e) => entry["error"] = e.into(),
    }
    entry
}
