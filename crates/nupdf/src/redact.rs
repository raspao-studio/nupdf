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
        let popup = stored(doc, &annot, b"Popup");
        remove_annots(doc, index, |a| {
            a == &annot || (!matches!(popup, Object::Null) && a == &popup)
        })?;
    }
    Ok(())
}

/// The entry at `key` of the dictionary `obj` as it is stored, a
/// reference left unresolved, so it can be matched against /Annots.
fn stored(doc: &Document, obj: &Object, key: &[u8]) -> Object {
    doc.resolve(obj)
        .as_dict()
        .and_then(|d| d.get(key).cloned())
        .unwrap_or(Object::Null)
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
            let popup = stored(doc, a, b"Popup");
            if !matches!(popup, Object::Null) {
                doomed.push(popup);
            }
            // A popup's parent goes with it too.
            if doc.dict_get(a, b"Subtype").is_name(b"Popup") {
                let parent = stored(doc, a, b"Parent");
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
        let parent = stored(doc, &node, b"Parent");
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

    use crate::object::ObjRef;
    use crate::testutil::{doc_of, stream};

    const HELVETICA: &[u8] = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>";

    fn opts(image_method: ImageMethod, line_art: LineArt, text: TextMethod) -> RedactOptions {
        RedactOptions {
            black_boxes: false,
            image_method,
            line_art,
            text,
        }
    }

    /// The page's contents and resources after redacting `area` from a
    /// page with `resources` drawing `content` (objects 5 on: `extra`).
    fn redacted(
        content: &[u8],
        resources: &str,
        extra: &[Vec<u8>],
        area: Quad,
        options: RedactOptions,
    ) -> (String, String) {
        let doc = crate::testutil::page_doc(content, resources, extra);
        assert_eq!(redact_page(&doc, 0, &[area], options), Ok(true));
        page_contents(&doc)
    }

    fn print(obj: &Object) -> String {
        crate::print::to_string(obj, crate::print::Options::default())
    }

    fn get(doc: &Document, num: i32, key: &[u8]) -> String {
        print(&doc.dict_get(&Object::Ref(ObjRef::new(num, 0)), key))
    }

    fn object(doc: &Document, num: i32) -> String {
        print(&doc.resolve(&Object::Ref(ObjRef::new(num, 0))))
    }

    #[test]
    fn invisible_text_alone_goes_when_asked() {
        // Golden output from MuPDF's pdf_redact_page with
        // PDF_REDACT_TEXT_REMOVE_INVISIBLE: render mode 3, and fills or
        // strokes that are fully transparent, go; the rest stays.
        let content = b"BT /F1 10 Tf 100 700 Td (Seen) Tj 3 Tr 0 -20 Td (Hidden) Tj 0 Tr q /GA gs 0 -20 Td (Clear) Tj Q 1 Tr 0 -20 Td (Stroked) Tj q /GS gs 0 -20 Td (Unstroked) Tj Q 2 Tr q /GA gs 0 -20 Td (Half) Tj /GS gs 0 -20 Td (Both) Tj Q 7 Tr 0 -20 Td (Clip) Tj ET";
        let (data, res) = redacted(
            content,
            "/Font<</F1 5 0 R>>/ExtGState<</GA<</ca 0>>/GS<</CA 0>>>>",
            &[HELVETICA.to_vec()],
            quad(90.0, 500.0, 300.0, 720.0),
            opts(
                ImageMethod::None,
                LineArt::None,
                TextMethod::RemoveInvisible,
            ),
        );
        assert_eq!(
            data,
            "q BT/F1 10 Tf 100 700 TD (Seen)Tj q 31.679993 -20 TD/GA gs -7.7899935 -20 TD ET Q (Stroked)Tj q/GS gs Q q/GA gs (Half)Tj/GS gs Q (Clip)Tj ET Q"
        );
        assert_eq!(
            res,
            "<</Font<</F1 5 0 R>>/ExtGState<</GA<</ca 0>>/GS<</CA 0>>>>>>"
        );
    }

    /// A page drawing a 4x4 grey image twice: through a clip that hides
    /// it right of x 100 at y 690..730, and in full at y 590..630.
    fn two_images(method: ImageMethod) -> (String, String) {
        redacted(
            b"q 0 0 100 800 re W n q 200 0 0 40 60 690 cm /Im1 Do Q Q q 200 0 0 40 60 590 cm /Im1 Do Q",
            "/XObject<</Im1 5 0 R>>",
            &[stream(
                "/Type/XObject/Subtype/Image/Width 4/Height 4/BitsPerComponent 8/ColorSpace/DeviceGray",
                &[0x40; 16],
            )],
            quad(150.0, 580.0, 200.0, 740.0),
            opts(method, LineArt::None, TextMethod::Remove),
        )
    }

    #[test]
    fn images_an_area_meets_go_unless_it_meets_only_what_is_hidden() {
        // Golden output from MuPDF, PDF_REDACT_IMAGE_REMOVE and
        // PDF_REDACT_IMAGE_REMOVE_UNLESS_INVISIBLE.
        assert_eq!(
            two_images(ImageMethod::Remove),
            (
                "q 0 0 100 800 re W n q 200 0 0 40 60 690 cm Q Q q 200 0 0 40 60 590 cm Q q Q"
                    .into(),
                "<<>>".into()
            )
        );
        assert_eq!(
            two_images(ImageMethod::RemoveUnlessInvisible),
            (
                "q 0 0 100 800 re W n q 200 0 0 40 60 690 cm /Im1 Do Q Q q 200 0 0 40 60 590 cm Q q Q"
                    .into(),
                "<</XObject<</Im1 5 0 R>>>>".into()
            )
        );
        // Left alone, both stay, each under a name of its own as forms
        // are instanced.
        assert_eq!(
            two_images(ImageMethod::None),
            (
                "q 0 0 100 800 re W n q 200 0 0 40 60 690 cm /Im1 Do Q Q q 200 0 0 40 60 590 cm /Im2 Do Q q Q"
                    .into(),
                "<</XObject<</Im1 5 0 R/Im2 5 0 R>>>>".into()
            )
        );
        // Blanked the same way, both uses share one new image, as
        // pdf_add_image finds the first again. (MuPDF's is object 7: its
        // Redact annotation took 6.)
        assert_eq!(
            two_images(ImageMethod::Pixels),
            (
                "q 0 0 100 800 re W n q 200 0 0 40 60 690 cm /Im1 Do Q Q q 200 0 0 40 60 590 cm /Im2 Do Q q Q"
                    .into(),
                "<</XObject<</Im1 6 0 R/Im2 6 0 R>>>>".into()
            )
        );
    }

    #[test]
    fn links_and_free_text_over_an_area_go_with_their_popups() {
        // As MuPDF's pdf_redact_page_links and _annotations leave /Annots.
        for annots in ["[6 0 R 7 0 R 8 0 R 9 0 R 10 0 R]", "11 0 R"] {
            let page = format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Annots {annots}/Contents 4 0 R>>"
            );
            let doc = doc_of(&[
                "<</Type/Catalog/Pages 2 0 R>>",
                "<</Type/Pages/Kids[3 0 R]/Count 1>>",
                &page,
                "<</Length 5>>stream\n0 0 m\nendstream",
                "<<>>",
                "<</Type/Annot/Subtype/Link/Rect[100 100 200 200]>>",
                "<</Type/Annot/Subtype/Link/Rect[400 400 500 500]>>",
                "<</Type/Annot/Subtype/FreeText/Rect[120 120 180 180]/Popup 9 0 R>>",
                "<</Type/Annot/Subtype/Popup/Rect[300 300 350 350]/Parent 8 0 R>>",
                "<</Type/Annot/Subtype/FreeText/Rect[400 100 450 150]>>",
                "[6 0 R 7 0 R 8 0 R 9 0 R 10 0 R]",
            ]);
            let area = quad(150.0, 150.0, 160.0, 160.0);
            assert_eq!(
                redact_page(&doc, 0, &[area], RedactOptions::default()),
                Ok(true)
            );
            let page = doc.page(0).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(print(&doc.dict_get(&page, b"Annots")), "[7 0 R 10 0 R]");
            // An indirect array is changed where it is stored.
            if annots == "11 0 R" {
                assert_eq!(object(&doc, 11), "[7 0 R 10 0 R]");
            }
        }
    }

    #[test]
    fn marked_content_properties_lose_the_targets() {
        let m = crate::scrub::Matcher::new(&["SECRET".into()], &[], false)
            .unwrap_or_else(|e| panic!("{e}"));
        let doc = crate::testutil::page_doc(
            b"/Span <</ActualText (SECRET)/Alt (fine)/E (SECRET too)>> BDC BT /F1 10 Tf 100 700 Td (x) Tj ET EMC /Span <</Alt (kept)>> BDC BT /F1 10 Tf 100 600 Td (y) Tj ET EMC",
            "/Font<</F1 5 0 R>>",
            &[HELVETICA.to_vec()],
        );
        let area = quad(400.0, 400.0, 410.0, 410.0);
        assert_eq!(
            redact_page_with(&doc, 0, &[area], RedactOptions::default(), Some(&m)),
            Ok(true)
        );
        let (data, _) = page_contents(&doc);
        assert!(data.contains("/Span<</Alt(fine)>>BDC"), "{data}");
        assert!(data.contains("/Span<</Alt(kept)>>BDC"), "{data}");
        assert!(!data.contains("SECRET"), "{data}");
    }

    #[test]
    fn quads_come_back_to_pdf_space() {
        // A page turned by 90 degrees.
        let ctm = Matrix {
            a: 0.0,
            b: 1.0,
            c: 1.0,
            d: 0.0,
            e: 0.0,
            f: 0.0,
        };
        let q = quad_to_pdf(quad(10.0, 20.0, 30.0, 40.0), ctm);
        assert_eq!(Rect::from_quad(q), Rect::new(20.0, 10.0, 40.0, 30.0));
    }

    /// A page with /Annots `annots` and a form whose /AcroForm is
    /// `acroform`; `objects` are 5 onwards.
    fn form_doc(annots: &str, acroform: &str, objects: &[&str]) -> Document {
        let catalog = format!("<</Type/Catalog/Pages 2 0 R/AcroForm {acroform}>>");
        let page = format!(
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Annots {annots}/Contents 4 0 R>>"
        );
        let mut objs = vec![
            catalog.as_str(),
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
            page.as_str(),
            "<</Length 0>>stream\n\nendstream",
        ];
        objs.extend_from_slice(objects);
        doc_of(&objs)
    }

    #[test]
    fn every_annotation_over_an_area_goes_with_its_field() {
        // 5: a widget of field 9 (with another widget, 6); 7: a widget that
        // is its own field; 8: a popup over the area whose parent (10) is
        // elsewhere; 11: an annotation elsewhere; 13: the only widget of
        // field 12, itself the only kid of field 14.
        let doc = form_doc(
            "[5 0 R 6 0 R 7 0 R 8 0 R 10 0 R 11 0 R 13 0 R null]",
            "15 0 R",
            &[
                "<</Type/Annot/Subtype/Widget/Rect[100 100 120 120]/Parent 9 0 R>>",
                "<</Type/Annot/Subtype/Widget/Rect[400 400 420 420]/Parent 9 0 R>>",
                "<</Type/Annot/Subtype/Widget/FT/Tx/T(a)/Rect[100 130 120 150]>>",
                "<</Type/Annot/Subtype/Popup/Rect[100 160 120 180]/Parent 10 0 R>>",
                "<</FT/Tx/T(b)/Kids[5 0 R 6 0 R]>>",
                "<</Type/Annot/Subtype/Text/Rect[500 500 520 520]/Popup 8 0 R>>",
                "<</Type/Annot/Subtype/Square/Rect[500 100 520 120]>>",
                "<</T(c)/Kids[13 0 R]/Parent 14 0 R>>",
                "<</Type/Annot/Subtype/Widget/FT/Tx/Rect[100 190 120 210]/Parent 12 0 R>>",
                "<</T(d)/Kids[12 0 R]>>",
                "<</Fields[9 0 R 7 0 R 14 0 R]>>",
            ],
        );
        let area = quad(90.0, 90.0, 130.0, 220.0);
        assert_eq!(redact_all_annotations(&doc, 0, &[area]), Ok(5));
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            print(&doc.dict_get(&page, b"Annots")),
            "[6 0 R 11 0 R null]"
        );
        // Field 9 keeps its other widget; 7 and, through 12, 14 leave the
        // form.
        assert_eq!(get(&doc, 9, b"Kids"), "[6 0 R]");
        assert_eq!(get(&doc, 12, b"Kids"), "[]");
        assert_eq!(get(&doc, 15, b"Fields"), "[9 0 R]");
        // Nothing is over another area.
        assert_eq!(
            redact_all_annotations(&doc, 0, &[quad(0.0, 0.0, 1.0, 1.0)]),
            Ok(0)
        );
    }

    #[test]
    fn widgets_leave_a_form_held_in_the_catalog() {
        let widgets = [
            "<</Type/Annot/Subtype/Widget/FT/Tx/T(a)/Rect[100 100 120 120]>>",
            "<</Type/Annot/Subtype/Widget/FT/Tx/T(b)/Rect[400 400 420 420]>>",
            "[5 0 R 6 0 R]",
        ];
        let area = quad(90.0, 90.0, 130.0, 130.0);
        let direct = form_doc("[5 0 R 6 0 R]", "<</Fields[5 0 R 6 0 R]>>", &widgets);
        assert_eq!(redact_all_annotations(&direct, 0, &[area]), Ok(1));
        assert_eq!(get(&direct, 1, b"AcroForm"), "<</Fields[6 0 R]>>");

        let indirect = form_doc("[5 0 R 6 0 R]", "<</Fields 7 0 R>>", &widgets);
        assert_eq!(redact_all_annotations(&indirect, 0, &[area]), Ok(1));
        assert_eq!(object(&indirect, 7), "[6 0 R]");

        // No form at all: the widget still goes from the page.
        let none = doc_of(&[
            "<</Type/Catalog/Pages 2 0 R>>",
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Annots 7 0 R>>",
            "null",
            widgets[0],
            widgets[1],
            widgets[2],
        ]);
        assert_eq!(redact_all_annotations(&none, 0, &[area]), Ok(1));
        assert_eq!(object(&none, 7), "[6 0 R]");
    }

    #[test]
    fn a_page_without_annotations_loses_none() {
        let doc = page_doc(CONTENT);
        assert_eq!(
            redact_all_annotations(&doc, 0, &[quad(0.0, 0.0, 612.0, 792.0)]),
            Ok(0)
        );
        assert!(redact_all_annotations(&doc, 3, &[]).is_err());
        assert!(
            redact_page(
                &doc,
                3,
                &[quad(0.0, 0.0, 1.0, 1.0)],
                RedactOptions::default()
            )
            .is_err()
        );
    }
}
