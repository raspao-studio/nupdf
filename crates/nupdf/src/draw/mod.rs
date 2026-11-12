// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! The rasterizer: MuPDF's draw device and what it paints with, ported so
//! pixels match `mutool draw` (DECISIONS D41).

pub mod color;
pub mod device;
pub mod edge;
pub mod glyph;
pub mod grays;
pub mod image;
pub mod list;
pub mod paint;
pub mod path;
pub mod shade;

use crate::content::Store;
use crate::document::Document;
use crate::geometry::{IRect, Matrix};
use crate::object::Object;

pub use device::DrawDevice;
pub use paint::Pix;

/// A rendered page, and the error that ended its run, if one did.
#[derive(Debug)]
pub struct Rendered {
    pub pix: Pix,
    pub error: Option<String>,
}

/// The page's pixel box under `ctm` (after the page's own transform), as
/// `fz_new_pixmap_from_page` sizes it.
pub fn page_bbox(doc: &Document, page: &Object, ctm: Matrix) -> Result<IRect, String> {
    let (fitzbox, page_ctm) = doc.page_transform(page).map_err(|e| e.0)?;
    Ok(IRect::round(fitzbox.transform(page_ctm).transform(ctm)))
}

/// `page.runPageContents` into a white RGB pixmap: the page's contents,
/// without annotations, under `ctm`. Fails when the box holds more than
/// `max_pixels`.
pub fn render_page(
    doc: &Document,
    page: &Object,
    ctm: Matrix,
    max_pixels: u64,
) -> Result<Rendered, String> {
    render_page_with(doc, page, ctm, max_pixels, None, Store::default(), true).0
}

/// [`render_page`] of the part of the page in `region` (pixels), with a
/// store kept from page to page.
pub fn render_page_with(
    doc: &Document,
    page: &Object,
    ctm: Matrix,
    max_pixels: u64,
    region: Option<IRect>,
    store: Store,
    icc: bool,
) -> (Result<Rendered, String>, Store) {
    let bbox = match page_bbox(doc, page, ctm) {
        Ok(b) => b,
        Err(e) => return (Err(e), store),
    };
    let bbox = match region {
        Some(r) => bbox.intersect(r),
        None => bbox,
    };
    if bbox.is_empty() {
        return (Err("the region is outside the page".into()), store);
    }
    let pixels = bbox.width() as u64 * bbox.height() as u64;
    if pixels > max_pixels {
        let e = format!(
            "limit error: {}x{} pixels is more than {max_pixels}",
            bbox.width(),
            bbox.height()
        );
        return (Err(e), store);
    }
    let mut pix = match Pix::new(bbox, 3, false) {
        Ok(p) => p,
        Err(e) => return (Err(e), store),
    };
    pix.clear_with(255);
    let mut dev = DrawDevice::new(Some(doc), pix);
    dev.cc.icc = icc;
    let run = crate::run::run_page_contents_with(doc, page, &mut dev, store, Some("View"), ctm);
    (
        Ok(Rendered {
            pix: dev.into_pix(),
            error: run.error,
        }),
        run.store,
    )
}

/// [`render_page_with`] in `bands` horizontal bands on as many threads:
/// the page's device calls kept once (a display list) and played into
/// each band. The pixels are the same as in one piece.
#[allow(clippy::too_many_arguments)]
pub fn render_page_banded(
    doc: &Document,
    page: &Object,
    ctm: Matrix,
    max_pixels: u64,
    region: Option<IRect>,
    store: Store,
    bands: usize,
    icc: bool,
) -> (Result<Rendered, String>, Store) {
    if bands <= 1 {
        return render_page_with(doc, page, ctm, max_pixels, region, store, icc);
    }
    let bbox = match page_bbox(doc, page, ctm) {
        Ok(b) => region.map_or(b, |r| b.intersect(r)),
        Err(e) => return (Err(e), store),
    };
    if bbox.is_empty() {
        return (Err("the region is outside the page".into()), store);
    }
    if bbox.width() as u64 * bbox.height() as u64 > max_pixels {
        let e = format!(
            "limit error: {}x{} pixels is more than {max_pixels}",
            bbox.width(),
            bbox.height()
        );
        return (Err(e), store);
    }
    // Bands pay for themselves only on large pages: below a megapixel,
    // or bands of fewer than 128 rows, the page is drawn in one piece.
    let bands = bands.min((bbox.height() / 128).max(1) as usize);
    if bands <= 1 || (bbox.width() as u64 * bbox.height() as u64) < 1 << 20 {
        return render_page_with(doc, page, ctm, max_pixels, region, store, icc);
    }
    let mut list = list::DisplayList::default();
    let run = crate::run::run_page_contents_with(doc, page, &mut list, store, Some("View"), ctm);
    let bands = bands.min(bbox.height().max(1) as usize);
    let h = bbox.height() as usize;
    let rows: Vec<IRect> = (0..bands)
        .map(|i| {
            let y0 = bbox.y0 + (h * i / bands) as i32;
            let y1 = bbox.y0 + (h * (i + 1) / bands) as i32;
            IRect::new(bbox.x0, y0, bbox.x1, y1)
        })
        .collect();
    let mut parts: Vec<Option<Pix>> = (0..bands).map(|_| None).collect();
    std::thread::scope(|s| {
        let handles: Vec<_> = rows
            .iter()
            .map(|&r| {
                let list = &list;
                let interrupt = crate::interrupt::current();
                std::thread::Builder::new()
                    .stack_size(crate::parallel::STACK_SIZE)
                    .spawn_scoped(s, move || {
                        crate::interrupt::within(interrupt, || {
                            let mut pix = Pix::new(r, 3, false).ok()?;
                            pix.clear_with(255);
                            let mut dev = DrawDevice::new(Some(doc), pix);
                            dev.cc.icc = icc;
                            list.replay(&mut dev);
                            Some(dev.into_pix())
                        })
                    })
                    .ok()
            })
            .collect();
        for (slot, h) in parts.iter_mut().zip(handles) {
            *slot = h.and_then(|h| h.join().ok()).flatten();
        }
    });
    let mut pix = match Pix::new(bbox, 3, false) {
        Ok(p) => p,
        Err(e) => return (Err(e), run.store),
    };
    for (part, r) in parts.into_iter().zip(&rows) {
        match part {
            Some(p) => pix.copy_rect(&p, *r),
            None => return (Err("a band failed to render".into()), run.store),
        }
    }
    (
        Ok(Rendered {
            pix,
            error: run.error,
        }),
        run.store,
    )
}

/// The page rendered with `quads` (page space) shaded in translucent red
/// and outlined: a preview of what a redaction removes.
pub fn render_page_marked(
    doc: &Document,
    page: &Object,
    ctm: Matrix,
    max_pixels: u64,
    quads: &[crate::geometry::Quad],
    icc: bool,
) -> Result<Rendered, String> {
    let bbox = page_bbox(doc, page, ctm)?;
    if bbox.width() as u64 * bbox.height() as u64 > max_pixels {
        return Err(format!(
            "limit error: {}x{} pixels is more than {max_pixels}",
            bbox.width(),
            bbox.height()
        ));
    }
    let mut pix = Pix::new(bbox, 3, false)?;
    pix.clear_with(255);
    let mut dev = DrawDevice::new(Some(doc), pix);
    dev.cc.icc = icc;
    let run = crate::run::run_page_contents_with(
        doc,
        page,
        &mut dev,
        Store::default(),
        Some("View"),
        ctm,
    );
    use crate::device::{ColorParams, Device};
    let red = [0.86f32, 0.1, 0.1];
    let stroke = crate::path::StrokeState {
        linewidth: 1.0,
        ..Default::default()
    };
    for q in quads {
        let mut path = crate::path::Path::new();
        path.move_to(q.ul.x, q.ul.y);
        path.line_to(q.ur.x, q.ur.y);
        path.line_to(q.lr.x, q.lr.y);
        path.line_to(q.ll.x, q.ll.y);
        path.close_path();
        let cs = crate::colorspace::ColorSpace::Rgb;
        dev.fill_path(&path, false, ctm, &cs, &red, 0.35, ColorParams::default());
        let s = crate::path::StrokeState {
            linewidth: stroke.linewidth / ctm.expansion().max(f32::EPSILON),
            ..stroke.clone()
        };
        dev.stroke_path(&path, &s, ctm, &cs, &red, 1.0, ColorParams::default());
    }
    Ok(Rendered {
        pix: dev.into_pix(),
        error: run.error,
    })
}
