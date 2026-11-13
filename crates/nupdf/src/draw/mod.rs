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
#[cfg(test)]
pub(crate) mod testkit;

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
/// each band. The pixels are MuPDF's for the same bands, which can differ
/// from one piece by a few levels where strokes and shadings cross a
/// band's edge: the rasterizer and shadings start from the band.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Point, Quad};

    fn sink() -> (Document, Object) {
        let doc = Document::open(testkit::sink()).unwrap_or_else(|e| panic!("{e:?}"));
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e:?}"));
        (doc, page)
    }

    fn whole(doc: &Document, page: &Object, zoom: f32, region: Option<IRect>) -> Pix {
        let ctm = Matrix::scale(zoom, zoom);
        let (r, _) = render_page_with(doc, page, ctm, 1 << 26, region, Store::default(), true);
        r.unwrap_or_else(|e| panic!("{e}")).pix
    }

    #[test]
    fn bands_draw_what_mupdf_draws_band_by_band() {
        // 1024 pixels square, a megapixel: eight bands of 128 rows, each
        // a replay of the page's display list on a thread of its own.
        // The rasterizer and shadings start from each band's scissor, so
        // bands are not quite the page drawn whole, in MuPDF as here: the
        // MD5s are of MuPDF's bands (a DrawDevice on a pixmap of each
        // band's rows, run by `mutool run`).
        let (doc, page) = sink();
        let ctm = Matrix::scale(8.0, 8.0);
        let (banded, _) =
            render_page_banded(&doc, &page, ctm, 1 << 26, None, Store::default(), 8, true);
        let banded = banded.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(banded.error, None);
        assert_eq!(
            testkit::md5(&banded.pix),
            "40b5da6c7c235844c175a297d4252032"
        );
        assert!(banded.pix != whole(&doc, &page, 8.0, None));
        // Regions: under a megapixel in one piece, over it in bands.
        for (zoom, r, want) in [
            (
                8.0,
                IRect::new(100, 0, 1024, 900),
                "83b0b41ff668d11c09285807396b5e22",
            ),
            (
                10.0,
                IRect::new(0, 100, 1280, 1280),
                "98af2eba3779c496d8bce3c6cd2abf6b",
            ),
        ] {
            let ctm = Matrix::scale(zoom, zoom);
            let (part, _) = render_page_banded(
                &doc,
                &page,
                ctm,
                1 << 26,
                Some(r),
                Store::default(),
                4,
                true,
            );
            let part = part.unwrap_or_else(|e| panic!("{e}")).pix;
            assert_eq!(part.bbox(), r);
            assert_eq!(testkit::md5(&part), want);
        }
    }

    #[test]
    fn small_pages_draw_in_one_piece_whatever_the_bands() {
        let (doc, page) = sink();
        for bands in [1, 8] {
            let (r, _) = render_page_banded(
                &doc,
                &page,
                Matrix::IDENTITY,
                1 << 26,
                None,
                Store::default(),
                bands,
                false,
            );
            let r = r.unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(testkit::md5(&r.pix), "52c0e8802b9702cc6bc3fae69dbbcf65");
        }
    }

    #[test]
    fn pages_past_the_pixel_limit_or_outside_the_region_fail() {
        let (doc, page) = sink();
        let big = render_page(&doc, &page, Matrix::scale(10.0, 10.0), 1000);
        assert_eq!(
            big.map(|_| ()),
            Err("limit error: 1280x1280 pixels is more than 1000".to_string())
        );
        for bands in [1, 4] {
            let (r, _) = render_page_banded(
                &doc,
                &page,
                Matrix::scale(10.0, 10.0),
                1000,
                None,
                Store::default(),
                bands,
                true,
            );
            assert!(r.is_err_and(|e| e.starts_with("limit error")));
            let off = Some(IRect::new(5000, 5000, 5100, 5100));
            let (r, _) = render_page_banded(
                &doc,
                &page,
                Matrix::IDENTITY,
                1 << 26,
                off,
                Store::default(),
                bands,
                true,
            );
            assert_eq!(r.map(|_| ()), Err("the region is outside the page".into()));
        }
        let q = Quad {
            ul: Point::new(0.0, 0.0),
            ur: Point::new(1.0, 0.0),
            ll: Point::new(0.0, 1.0),
            lr: Point::new(1.0, 1.0),
        };
        let marked = render_page_marked(&doc, &page, Matrix::scale(10.0, 10.0), 1000, &[q], true);
        assert!(marked.is_err_and(|e| e.starts_with("limit error")));
    }

    #[test]
    fn marked_quads_shade_and_outline_what_they_cover() {
        let doc =
            Document::open(testkit::page(64, 64, "", "", &[])).unwrap_or_else(|e| panic!("{e:?}"));
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e:?}"));
        // Page space: y up from the bottom; the pixmap's rows run down.
        let q = Quad {
            ul: Point::new(8.0, 40.0),
            ur: Point::new(40.0, 40.0),
            ll: Point::new(8.0, 8.0),
            lr: Point::new(40.0, 8.0),
        };
        let ctm = Matrix::scale(1.0, -1.0).concat(Matrix::translate(0.0, 64.0));
        let r = render_page_marked(&doc, &page, ctm, 1 << 20, &[q], true)
            .unwrap_or_else(|e| panic!("{e}"));
        let inside = testkit::px(&r.pix, 24, 40);
        assert!(inside[0] > inside[1] && inside[1] < 255, "{inside:?}");
        assert_eq!(testkit::px(&r.pix, 56, 8), [255, 255, 255]);
    }
}
