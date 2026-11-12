// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-clean.c (pdf_redact_page and its
// filters), Copyright (C) 2004-2025 Artifex Software, Inc.

//! Page redaction as `pdf_redact_page` does it, with the areas given as
//! quads instead of Redact annotations: the page's contents go through the
//! sanitize filter, forms drawn on it get copies of their own, glyphs
//! whose (slightly shrunk) boxes meet an area go, images are left, removed
//! or blanked, line art is left or removed, and links over the areas go.

use crate::content_clean::{Options, filter_page_contents};
use crate::content_filter::{Cull, Hooks, ImageAction, ShownImage};
use crate::document::Document;
use crate::geometry::{Matrix, Point, Quad, Rect};
use crate::object::Object;

/// `PDF_REDACT_IMAGE_*`: what happens to images under an area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageMethod {
    /// Left as they are.
    None,
    /// Removed when an area meets them.
    Remove,
    /// The pixels under the areas blanked, or the whole image removed
    /// when an area covers it.
    Pixels,
    /// Removed when an area meets the part that can be seen.
    RemoveUnlessInvisible,
}

/// `PDF_REDACT_LINE_ART_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineArt {
    None,
    RemoveIfCovered,
    RemoveIfTouched,
}

/// `PDF_REDACT_TEXT_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextMethod {
    Remove,
    None,
    /// Only text that cannot be seen (render mode 3 or fully transparent).
    RemoveInvisible,
}

/// `pdf_redact_options`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedactOptions {
    /// Fill each area in black after the contents.
    pub black_boxes: bool,
    pub image_method: ImageMethod,
    pub line_art: LineArt,
    pub text: TextMethod,
}

impl Default for RedactOptions {
    /// What `pdf_redact_page` does with no options.
    fn default() -> Self {
        RedactOptions {
            black_boxes: false,
            image_method: ImageMethod::Pixels,
            line_art: LineArt::None,
            text: TextMethod::Remove,
        }
    }
}

/// The areas, in PDF user space, as a Redact annotation's /QuadPoints
/// hold them.
#[derive(Debug, Clone)]
struct Redactor<'m> {
    quads: Vec<Quad>,
    options: RedactOptions,
    /// nupdf's own: target text to cut from marked-content properties.
    matcher: Option<&'m crate::scrub::Matcher>,
}

impl Redactor<'_> {
    /// `rect_touches_redactions`: 0 when `area` meets no redaction, 2 when
    /// one contains it, else 1.
    fn touches(&self, area: Rect) -> u8 {
        for q in &self.quads {
            let r = Rect::from_quad(*q);
            if !r.intersect(area).is_empty() {
                return if r.contains(area) { 2 } else { 1 };
            }
        }
        0
    }

    /// `pdf_redact_text_filter`: the glyph's box, shrunk by a tenth on
    /// each side, meeting an area. The test is for a valid intersection,
    /// not a non-empty one, so zero-width glyphs (diacritics) count.
    fn text_hit(&self, trm: Matrix, ctm: Matrix, bbox: Rect) -> bool {
        let trm = trm.concat(ctm);
        let mut b = bbox.transform(trm);
        let w = b.x1 - b.x0;
        let h = b.y1 - b.y0;
        b.x0 += w / 10.0;
        b.x1 -= w / 10.0;
        b.y0 += h / 10.0;
        b.y1 -= h / 10.0;
        self.quads
            .iter()
            .any(|q| b.intersect(Rect::from_quad(*q)).is_valid())
    }
}

impl Hooks for Redactor<'_> {
    fn filters_text(&self) -> bool {
        self.options.text != TextMethod::None
    }

    fn text_filter(
        &mut self,
        _ucs: &[i32],
        trm: Matrix,
        ctm: Matrix,
        bbox: Rect,
        render: i32,
        fill_alpha: f32,
        stroke_alpha: f32,
    ) -> bool {
        if self.options.text == TextMethod::RemoveInvisible {
            let invisible = match render {
                0 => fill_alpha == 0.0,
                1 => stroke_alpha == 0.0,
                2 => fill_alpha == 0.0 && stroke_alpha == 0.0,
                3 => true,
                _ => false,
            };
            if !invisible {
                return false;
            }
        }
        self.text_hit(trm, ctm, bbox)
    }

    fn culls(&self) -> bool {
        true
    }

    fn cull(&mut self, bbox: Rect, kind: Cull) -> bool {
        match kind {
            Cull::PathFill
            | Cull::PathStroke
            | Cull::PathFillStroke
            | Cull::ClipPathFill
            | Cull::ClipPathStroke
            | Cull::ClipPathFillStroke => match self.options.line_art {
                LineArt::RemoveIfCovered => self.touches(bbox) == 2,
                LineArt::RemoveIfTouched => self.touches(bbox) != 0,
                LineArt::None => false,
            },
            _ => false,
        }
    }

    fn filters_images(&self) -> bool {
        self.options.image_method != ImageMethod::None
    }

    fn image_filter(
        &mut self,
        doc: &Document,
        ctm: Matrix,
        image: ShownImage<'_>,
        clip: Rect,
    ) -> Result<ImageAction, String> {
        let unit = Rect::new(0.0, 0.0, 1.0, 1.0);
        match self.options.image_method {
            ImageMethod::None => Ok(ImageAction::Keep),
            ImageMethod::Remove | ImageMethod::RemoveUnlessInvisible => {
                let mut area = unit.transform(ctm);
                if self.options.image_method == ImageMethod::RemoveUnlessInvisible {
                    area = area.intersect(clip);
                }
                let hit = self
                    .quads
                    .iter()
                    .any(|q| !Rect::from_quad(*q).intersect(area).is_empty());
                Ok(if hit {
                    ImageAction::Remove
                } else {
                    ImageAction::Keep
                })
            }
            ImageMethod::Pixels => crate::redact_image::redact_pixels(doc, ctm, image, &self.quads),
        }
    }

    fn complete(&mut self, data: &mut Vec<u8>) {
        if self.options.black_boxes {
            black_boxes(&self.quads, data);
        }
    }

    fn marked_content(&mut self, props: &Object) -> Option<Object> {
        let m = self.matcher?;
        let Object::Dict(d) = props else {
            return None;
        };
        let mut d2 = (**d).clone();
        let mut changed = false;
        for key in [&b"ActualText"[..], b"Alt", b"E"] {
            if let Some(Object::String(s)) = d.get(key)
                && m.matches_text(s)
            {
                d2.remove(key);
                changed = true;
            }
        }
        changed.then(|| Object::Dict(std::sync::Arc::new(d2)))
    }
}

/// `pdf_redact_end_page`: each area filled in black after the contents.
fn black_boxes(quads: &[Quad], data: &mut Vec<u8>) {
    let point = |data: &mut Vec<u8>, p: Point, op: &[u8]| {
        crate::print::fmtfloat(p.x, data);
        data.push(b' ');
        crate::print::fmtfloat(p.y, data);
        data.extend_from_slice(op);
    };
    data.extend_from_slice(b" 0 g\n");
    for q in quads {
        point(data, q.ll, b" m\n");
        point(data, q.lr, b" l\n");
        point(data, q.ur, b" l\n");
        point(data, q.ul, b" l\n");
        data.extend_from_slice(b"f\n");
    }
}

/// A quad in page space, as `pdf_set_annot_quad_points` stores it: in
/// PDF user space, through the inverse of the page transform.
pub fn quad_to_pdf(q: Quad, page_ctm: Matrix) -> Quad {
    let inv = page_ctm.invert();
    let t = |p: Point| inv.transform_xy(p.x, p.y);
    Quad {
        ul: t(q.ul),
        ur: t(q.ur),
        ll: t(q.ll),
        lr: t(q.lr),
    }
}

/// `pdf_redact_page` with `quads` (PDF user space) as the redactions:
/// false, with the page left alone, when there are none.
pub fn redact_page(
    doc: &Document,
    index: i32,
    quads: &[Quad],
    options: RedactOptions,
) -> Result<bool, String> {
    redact_page_with(doc, index, quads, options, None)
}

/// [`redact_page`], cutting the targets `matcher` finds from the page's
/// marked-content properties too (nupdf's own).
pub fn redact_page_with(
    doc: &Document,
    index: i32,
    quads: &[Quad],
    options: RedactOptions,
    matcher: Option<&crate::scrub::Matcher>,
) -> Result<bool, String> {
    if quads.is_empty() {
        return Ok(false);
    }
    let mut red = Redactor {
        quads: quads.to_vec(),
        options,
        matcher,
    };
    let opts = Options {
        recurse: false,
        instance_forms: true,
        ascii: true,
        newlines: false,
        sanitize: true,
    };
    filter_page_contents(doc, index, &opts, &mut red)?;
    redact_page_links(doc, index, &red)?;
    redact_page_annotations(doc, index, &red)?;
    Ok(true)
}

/// `pdf_redact_page_links`: links whose /Rect meets a redaction go from
/// /Annots.
fn redact_page_links(doc: &Document, index: i32, red: &Redactor<'_>) -> Result<(), String> {
    remove_annots(doc, index, |annot| {
        doc.dict_get(annot, b"Subtype").is_name(b"Link")
            && red.touches(doc.to_rect(Some(&doc.dict_get(annot, b"Rect")))) != 0
    })
}

/// `pdf_redact_page_annotations`: FreeText annotations over a redaction
/// go, with their popups.
fn redact_page_annotations(doc: &Document, index: i32, red: &Redactor<'_>) -> Result<(), String> {
    let page = doc.page(index).map_err(|e| e.0)?;
    let doomed: Vec<Object> = crate::content_clean::page_annots(doc, &page)
        .into_iter()
        .filter(|a| {
            doc.dict_get(a, b"Subtype").is_name(b"FreeText")
                && red.touches(doc.to_rect(Some(&doc.dict_get(a, b"Rect")))) != 0
        })
        .collect();
    for annot in doomed {
        let popup = doc.dict_get(&annot, b"Popup");
        remove_annots(doc, index, |a| {
            a == &annot || (!matches!(popup, Object::Null) && a == &popup)
        })?;
    }
    Ok(())
}

/// nupdf's own step after `pdf_redact_page` (the redaction contract):
/// every annotation whose /Rect meets an area goes, whatever its type,
/// with its popup; a widget goes from the form's fields too, and a field
/// left with no widgets goes with it. Returns how many went.
pub fn redact_all_annotations(doc: &Document, index: i32, quads: &[Quad]) -> Result<usize, String> {
    let red = Redactor {
        quads: quads.to_vec(),
        options: RedactOptions::default(),
        matcher: None,
    };
    let page = doc.page(index).map_err(|e| e.0)?;
    let Object::Array(annots) = doc.dict_get(&page, b"Annots") else {
        return Ok(0);
    };
    let mut doomed: Vec<Object> = Vec::new();
    for a in annots.iter() {
        if doc.resolve(a).as_dict().is_none() {
            continue;
        }
        let rect = doc.to_rect(Some(&doc.dict_get(a, b"Rect")));
        if red.touches(rect) != 0 {
            doomed.push(a.clone());
            let popup = doc.dict_get(a, b"Popup");
            if !matches!(popup, Object::Null) {
                doomed.push(popup);
            }
            // A popup's parent goes with it too.
            if doc.dict_get(a, b"Subtype").is_name(b"Popup") {
                let parent = doc.dict_get(a, b"Parent");
                if !matches!(parent, Object::Null) {
                    doomed.push(parent);
                }
            }
        }
    }
    if doomed.is_empty() {
        return Ok(0);
    }
    let same = |a: &Object, b: &Object| match (a.as_ref(), b.as_ref()) {
        (Some(x), Some(y)) => x.num == y.num,
        _ => a == b,
    };
    remove_annots(doc, index, |a| doomed.iter().any(|d| same(a, d)))?;
    for d in &doomed {
        if doc.dict_get(d, b"Subtype").is_name(b"Widget") {
            remove_field(doc, d);
        }
    }
    Ok(doomed.len())
}

/// A widget taken out of the form: from its parent's /Kids, or from
/// /AcroForm /Fields; a parent left with no kids is taken out the same
/// way.
fn remove_field(doc: &Document, widget: &Object) {
    let Some(r) = widget.as_ref() else {
        return;
    };
    let mut target = r.num;
    // Field trees deeper than this are taken as cycles.
    for _ in 0..64 {
        let node = Object::Ref(crate::object::ObjRef::new(target, 0));
        let parent = doc.dict_get(&node, b"Parent");
        let (holder, key) = match parent.as_ref() {
            Some(p) => (Some(p.num), b"Kids".as_slice()),
            None => (None, b"Fields".as_slice()),
        };
        let left = match holder {
            Some(p) => remove_from_array_in(doc, p, key, target),
            None => {
                remove_from_acroform(doc, target);
                return;
            }
        };
        match (left, holder) {
            (Some(0), Some(p)) => target = p,
            _ => return,
        }
    }
}

/// Drops `num` from the array at `key` of object `holder`; how many are
/// left.
fn remove_from_array_in(doc: &Document, holder: i32, key: &[u8], num: i32) -> Option<usize> {
    let Object::Dict(d) = doc.object(holder)? else {
        return None;
    };
    let stored = d.get(key).cloned()?;
    let Object::Array(items) = doc.resolve(&stored) else {
        return None;
    };
    let kept: Vec<Object> = items
        .iter()
        .filter(|o| o.as_ref().is_none_or(|r| r.num != num))
        .cloned()
        .collect();
    let left = kept.len();
    let arr = Object::Array(std::sync::Arc::new(kept));
    match stored {
        Object::Ref(a) => doc.alter_object(a.num, arr),
        _ => {
            let mut d = (*d).clone();
            d.put(key, arr);
            doc.alter_object(holder, Object::Dict(std::sync::Arc::new(d)));
        }
    }
    Some(left)
}

fn remove_from_acroform(doc: &Document, num: i32) {
    let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
    let form = match doc
        .resolve(&root)
        .as_dict()
        .and_then(|d| d.get(b"AcroForm").cloned())
    {
        Some(f) => f,
        None => return,
    };
    match form {
        Object::Ref(f) => {
            remove_from_array_in(doc, f.num, b"Fields", num);
        }
        Object::Dict(_) => {
            if let Some(r) = root.as_ref() {
                // /AcroForm held directly in the catalog.
                let Object::Dict(cat) = doc.resolve(&root) else {
                    return;
                };
                let mut cat = (*cat).clone();
                if let Some(Object::Dict(f)) = cat.get(b"AcroForm").cloned() {
                    let mut f = (*f).clone();
                    if let Some(fields) = f.get(b"Fields").cloned() {
                        match fields {
                            Object::Ref(a) => {
                                if let Object::Array(items) = doc.resolve(&Object::Ref(a)) {
                                    let kept: Vec<Object> = items
                                        .iter()
                                        .filter(|o| o.as_ref().is_none_or(|x| x.num != num))
                                        .cloned()
                                        .collect();
                                    doc.alter_object(
                                        a.num,
                                        Object::Array(std::sync::Arc::new(kept)),
                                    );
                                }
                            }
                            Object::Array(items) => {
                                let kept: Vec<Object> = items
                                    .iter()
                                    .filter(|o| o.as_ref().is_none_or(|x| x.num != num))
                                    .cloned()
                                    .collect();
                                f.put(b"Fields", Object::Array(std::sync::Arc::new(kept)));
                                cat.put(b"AcroForm", Object::Dict(std::sync::Arc::new(f)));
                                doc.alter_object(r.num, Object::Dict(std::sync::Arc::new(cat)));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Each entry of the page's /Annots that `doomed` picks, removed where the
/// array is held.
pub(crate) fn remove_annots(
    doc: &Document,
    index: i32,
    mut doomed: impl FnMut(&Object) -> bool,
) -> Result<(), String> {
    let page = doc.page(index).map_err(|e| e.0)?;
    let stored = match doc
        .resolve(&page)
        .as_dict()
        .and_then(|d| d.get(b"Annots").cloned())
    {
        Some(a) => a,
        None => return Ok(()),
    };
    let Object::Array(items) = doc.resolve(&stored) else {
        return Ok(());
    };
    let kept: Vec<Object> = items.iter().filter(|a| !doomed(a)).cloned().collect();
    if kept.len() == items.len() {
        return Ok(());
    }
    let arr = Object::Array(std::sync::Arc::new(kept));
    match stored {
        Object::Ref(r) => doc.alter_object(r.num, arr),
        _ => crate::edit::edit_page(doc, index, &mut |d| d.put(b"Annots", arr.clone()))?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_clean::tests::{page_contents, page_doc};

    fn quad(x0: f32, y0: f32, x1: f32, y1: f32) -> Quad {
        // As a Redact annotation's /QuadPoints hold it: ul, ur, ll, lr.
        Quad {
            ul: Point { x: x0, y: y1 },
            ur: Point { x: x1, y: y1 },
            ll: Point { x: x0, y: y0 },
            lr: Point { x: x1, y: y0 },
        }
    }

    const CONTENT: &[u8] =
        b"BT /F1 10 Tf 100 700 Td [(Hello) -200 (World)] TJ ET 100 600 m 200 600 l S 300 300 100 100 re f";

    #[test]
    fn redacting_a_word_leaves_the_rest_where_it_was() {
        // Golden output from MuPDF's pdf_redact_page, no boxes, images and
        // line art kept.
        let doc = page_doc(CONTENT);
        let opts = RedactOptions {
            black_boxes: false,
            image_method: ImageMethod::None,
            line_art: LineArt::None,
            text: TextMethod::Remove,
        };
        assert_eq!(
            redact_page(&doc, 0, &[quad(125.0, 692.0, 200.0, 712.0)], opts),
            Ok(true)
        );
        assert_eq!(
            page_contents(&doc).0,
            "q BT/F1 10 Tf 100 700 TD[(Hello)]TJ ET 100 600 m 200 600 l S 300 300 100 100 re f Q"
        );
    }

    #[test]
    fn a_split_array_moves_to_what_is_kept_and_boxes_follow() {
        let doc = page_doc(CONTENT);
        let opts = RedactOptions {
            black_boxes: true,
            image_method: ImageMethod::None,
            line_art: LineArt::RemoveIfTouched,
            text: TextMethod::Remove,
        };
        assert_eq!(
            redact_page(&doc, 0, &[quad(90.0, 692.0, 140.0, 712.0)], opts),
            Ok(true)
        );
        assert_eq!(
            page_contents(&doc).0,
            "q BT/F1 10 Tf 139.78 700 TD[(rld)]TJ ET 100 600 m 200 600 l S 300 300 100 100 re f Q 0 g\n90 692 m\n140 692 l\n140 712 l\n90 712 l\nf\n"
        );
    }

    #[test]
    fn line_art_goes_when_covered_or_touched() {
        let covered = |line_art| {
            let doc = page_doc(CONTENT);
            let opts = RedactOptions {
                black_boxes: false,
                image_method: ImageMethod::None,
                line_art,
                text: TextMethod::None,
            };
            let _ = redact_page(&doc, 0, &[quad(290.0, 290.0, 350.0, 450.0)], opts);
            page_contents(&doc).0
        };
        assert!(covered(LineArt::None).contains("300 300 100 100 re f"));
        // The square is touched, not covered.
        assert!(covered(LineArt::RemoveIfCovered).contains("300 300 100 100 re f"));
        assert!(!covered(LineArt::RemoveIfTouched).contains("re f"));
    }

    #[test]
    fn no_areas_leave_the_page_alone() {
        let doc = page_doc(CONTENT);
        assert_eq!(
            redact_page(&doc, 0, &[], RedactOptions::default()),
            Ok(false)
        );
        assert!(!doc.is_edited());
    }
}
