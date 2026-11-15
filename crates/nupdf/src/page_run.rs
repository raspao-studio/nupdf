// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/document.c (fz_run_page),
// source/pdf/pdf-run.c (pdf_run_page_with_usage, pdf_run_annot_with_usage,
// pdf_run_page_annots_with_usage, pdf_run_page_widgets_with_usage),
// source/pdf/pdf-interpret.c (pdf_process_annot), source/pdf/pdf-annot.c
// (pdf_annot_ap, pdf_annot_transform, pdf_annot_display_rect,
// pdf_annot_display_rotate) and source/pdf/pdf-appearance.c
// (pdf_update_appearance's local synthesis), Copyright (C) 2004-2026
// Artifex Software, Inc.

//! A whole page as `fz_run_page` runs it: the contents, then the
//! annotations, then the widgets, each appearance drawn onto its /Rect.
//!
//! What has no appearance when the page loads gets the one MuPDF makes for
//! it (`pdf_update_appearance`): MuPDF keeps those in a local xref that
//! the document never sees, and so does this, in the document's local
//! streams.

use crate::annot;
use crate::appearance::{self, Form};
use crate::content::Store;
use crate::device::Device;
use crate::document::{Document, LocalStream};
use crate::geometry::{Matrix, Rect};
use crate::object::Object;
use crate::run::PageRun;

const INVISIBLE: i32 = 1;
const HIDDEN: i32 = 2;
const PRINT: i32 = 4;
const NO_ZOOM: i32 = 8;
const NO_ROTATE: i32 = 16;
const NO_VIEW: i32 = 32;

/// What to run of a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parts {
    pub contents: bool,
    pub annots: bool,
    pub widgets: bool,
}

impl Parts {
    /// `fz_run_page`: everything.
    pub const ALL: Parts = Parts {
        contents: true,
        annots: true,
        widgets: true,
    };
    /// `fz_run_page_contents`.
    pub const CONTENTS: Parts = Parts {
        contents: true,
        annots: false,
        widgets: false,
    };
}

/// An annotation as the page shows it: the dictionary its getters read
/// (a local copy when its appearance was made on loading), and the
/// appearance, if any.
struct Shown<'a> {
    annot: Object,
    ap: Option<Object>,
    /// Keeps a made appearance's data while the page runs.
    _local: Option<LocalStream<'a>>,
}

/// `pdf_annot_ap` for an annotation neither hot nor active: /AP /N when it
/// is a stream object, else its state /AS names. As stored, not resolved,
/// so a form keeps its number.
pub fn annot_ap(doc: &Document, annot: &Object) -> Option<Object> {
    let raw = |container: &Object, key: &[u8]| {
        doc.resolve(container)
            .as_dict()
            .and_then(|d| d.get(key).cloned())
            .unwrap_or(Object::Null)
    };
    let n = raw(&raw(annot, b"AP"), b"N");
    if let Object::Ref(_) = n
        && matches!(doc.resolve(&n), Object::Stream(_))
    {
        return Some(n);
    }
    let state = doc.resolve(&raw(annot, b"AS"));
    let found = raw(&n, state.as_name()?);
    matches!(doc.resolve(&found), Object::Stream(_)).then_some(found)
}

/// `pdf_annot_display_rotate`: the page's /Rotate for a NoRotate
/// annotation, else 0.
fn display_rotate(doc: &Document, page: &Object, flags: i32) -> i32 {
    if flags & NO_ROTATE == 0 {
        return 0;
    }
    doc.inherited(page, b"Rotate")
        .ok()
        .flatten()
        .map_or(0, |v| doc.resolve(&v).to_int())
}

/// `pdf_annot_display_rect`: /Rect, sized to the appearance for NoZoom,
/// turned about its top left for NoRotate.
fn display_rect(doc: &Document, page: &Object, annot: &Object, ap: &Object, flags: i32) -> Rect {
    let mut rect = doc.to_rect(Some(&doc.dict_get(annot, b"Rect")));
    if flags & NO_ZOOM != 0 {
        let bbox = doc.to_rect(Some(&doc.dict_get(ap, b"BBox")));
        rect = Rect::new(
            rect.x0,
            rect.y1 - (bbox.y1 - bbox.y0),
            rect.x0 + (bbox.x1 - bbox.x0),
            rect.y1,
        );
    }
    let rotate = display_rotate(doc, page, flags);
    if rotate != 0 {
        let (tx, ty) = (rect.x0, rect.y1);
        let m = Matrix::translate(-tx, -ty)
            .concat(Matrix::IDENTITY.pre_rotate(rotate as f32))
            .concat(Matrix::translate(tx, ty));
        rect = rect.transform(m);
    }
    rect
}

/// `pdf_annot_transform`: the appearance's box, through its matrix, onto
/// the annotation's display rectangle.
pub fn annot_transform(doc: &Document, page: &Object, annot: &Object, ap: &Object) -> Matrix {
    use crate::fma::Madd;
    let flags = annot::flags(doc, annot);
    let rect = display_rect(doc, page, annot, ap, flags);
    let rotate = display_rotate(doc, page, flags);
    let matrix = crate::run::to_matrix(doc, &doc.dict_get(ap, b"Matrix"));
    let bbox = doc
        .to_rect(Some(&doc.dict_get(ap, b"BBox")))
        .transform(matrix);
    let w = bbox.x1 - bbox.x0;
    let h = bbox.y1 - bbox.y0;
    let rotmat = match rotate {
        90 => {
            let mut m = Matrix::scale(w / h, h / w).pre_rotate(90.0);
            m.e = w;
            m
        }
        180 => {
            let mut m = Matrix::IDENTITY.pre_rotate(180.0);
            m.e = w;
            m.f = h;
            m
        }
        270 => {
            let mut m = Matrix::scale(w / h, h / w).pre_rotate(270.0);
            m.f = h;
            m
        }
        _ => Matrix::IDENTITY,
    };
    let sx = if bbox.x1 == bbox.x0 {
        0.0
    } else {
        (rect.x1 - rect.x0) / w
    };
    let sy = if bbox.y1 == bbox.y0 {
        0.0
    } else {
        (rect.y1 - rect.y0) / h
    };
    // `rect.x0 - (bbox.x0 * sx)`, contracted.
    let x = (-bbox.x0).madd(sx, rect.x0);
    let y = (-bbox.y0).madd(sy, rect.y0);
    rotmat.concat(Matrix::translate(x, y).pre_scale(sx, sy))
}

/// A made form as a local stream.
fn local_form<'a>(doc: &'a Document, f: &Form) -> LocalStream<'a> {
    let dict = appearance::xobject_dict(f, f.resources.clone());
    doc.add_local_stream(dict, f.content.clone())
}

fn warn_unmade(doc: &Document, e: &str) {
    doc.diagnostics().warn(
        "annot.appearance",
        format!("cannot create appearance stream: {e}"),
    );
}

/// `pdf_update_appearance` on loading the page, for one annotation: the
/// appearance made for it when it needs one (into the local xref), else
/// its own.
fn shown<'a>(doc: &'a Document, annot: &Object, ctm: Matrix) -> Shown<'a> {
    let own = || Shown {
        annot: annot.clone(),
        ap: annot_ap(doc, annot),
        _local: None,
    };
    if !appearance::needs_synthesis(doc, annot) {
        return own();
    }
    let is_widget = annot::type_name(doc, annot) == "Widget";
    // An unsigned signature: MuPDF's sign-here arrow, whatever it has.
    if is_widget && crate::form::inheritable(doc, annot, b"FT").is_name(b"Sig") {
        return match appearance::unsigned_signature(doc, annot, ctm) {
            Ok(f) => {
                let local = local_form(doc, &f);
                Shown {
                    annot: annot.clone(),
                    ap: Some(local.object().clone()),
                    _local: Some(local),
                }
            }
            Err(e) => {
                warn_unmade(doc, &e);
                own()
            }
        };
    }
    // A button by its /FT as stored, as MuPDF compares it.
    if is_widget && crate::form::inheritable(doc, annot, b"FT").is_name(b"Btn") {
        return match appearance::synthesize_button(doc, annot, ctm) {
            Ok(made) => {
                let state = doc.resolve(&doc.dict_get(&made.annot, b"AS"));
                let pick = made.forms.iter().filter(|(key, _, _)| *key == "N").find(
                    |(_, st, _)| match st {
                        None => true,
                        Some(st) => state.as_name() == Some(st.as_slice()),
                    },
                );
                let local = pick.map(|(_, _, f)| local_form(doc, f));
                Shown {
                    annot: made.annot,
                    ap: local.as_ref().map(|l| l.object().clone()),
                    _local: local,
                }
            }
            Err(e) => {
                warn_unmade(doc, &e);
                own()
            }
        };
    }
    match appearance::synthesize(doc, annot, ctm) {
        Ok(made) => {
            let local = local_form(
                doc,
                &Form {
                    bbox: made.bbox,
                    matrix: made.matrix,
                    resources: made.resources,
                    content: made.content,
                },
            );
            Shown {
                annot: made.annot,
                ap: Some(local.object().clone()),
                _local: Some(local),
            }
        }
        Err(e) => {
            warn_unmade(doc, &e);
            own()
        }
    }
}

/// Whether `pdf_run_annot_with_usage` and `pdf_process_annot` draw it at
/// all: a widget needs a field type and name; hidden and invisible ones
/// never show; for "View", NoView ones do not, and for "Print" only those
/// flagged to print.
fn drawn(doc: &Document, original: &Object, shown: &Object, usage: Option<&str>) -> bool {
    if annot::type_name(doc, original) == "Widget" {
        let ft = crate::form::inheritable(doc, original, b"FT");
        let t = crate::form::inheritable(doc, original, b"T");
        if matches!(ft, Object::Null) || matches!(t, Object::Null) {
            return false;
        }
    }
    let flags = annot::flags(doc, shown);
    if flags & (INVISIBLE | HIDDEN) != 0 || annot::type_name(doc, shown) == "Popup" {
        return false;
    }
    match usage {
        Some("Print") => flags & PRINT != 0,
        Some("View") => flags & NO_VIEW == 0,
        _ => true,
    }
}

/// [`run_page_parts`] of everything: `fz_run_page`.
pub fn run_page(
    doc: &Document,
    page: &Object,
    dev: &mut dyn Device,
    store: Store,
    usage: Option<&'static str>,
    user: Matrix,
) -> PageRun {
    run_page_parts(doc, page, dev, store, usage, user, Parts::ALL)
}

/// `fz_run_page_contents`, `fz_run_page_annots` and `fz_run_page_widgets`,
/// those `parts` asks for, in that order, with `user` after the page's own
/// transform. An error ends the run there, as it leaves `fz_run_page`
/// (only an abort is ignored, and nupdf has none).
pub fn run_page_parts(
    doc: &Document,
    page: &Object,
    dev: &mut dyn Device,
    store: Store,
    usage: Option<&'static str>,
    user: Matrix,
    parts: Parts,
) -> PageRun {
    // Loading the page makes the appearances it lacks (pdf_load_annots).
    let ctm = doc
        .page_transform(page)
        .map(|t| t.1)
        .unwrap_or(Matrix::IDENTITY);
    let (annots, widgets) = if parts.annots || parts.widgets {
        annot::page_annots(doc, page)
    } else {
        (Vec::new(), Vec::new())
    };
    let made = |list: &[Object]| -> Vec<(Object, Shown<'_>)> {
        list.iter()
            .map(|a| (a.clone(), shown(doc, a, ctm)))
            .collect()
    };
    let annots = made(&annots);
    let widgets = made(&widgets);
    let mut store = store;
    // The page's glyphs and operators are counted from here, whichever
    // parts are run.
    store.page_glyphs = 0;
    store.page_operators = 0;
    let mut store = Some(store);
    let mut error = None;
    if parts.contents {
        let run = crate::run::run_page_contents_with(
            doc,
            page,
            &mut *dev,
            store.take().unwrap_or_default(),
            usage,
            user,
        );
        error = run.error;
        store = Some(run.store);
    }
    for (on, list) in [(parts.annots, &annots), (parts.widgets, &widgets)] {
        if !on || error.is_some() {
            continue;
        }
        for (original, s) in list {
            if !drawn(doc, original, &s.annot, usage) {
                continue;
            }
            let Some(ap) = &s.ap else { continue };
            let matrix = annot_transform(doc, page, &s.annot, ap);
            let oc = doc
                .resolve(&s.annot)
                .as_dict()
                .and_then(|d| d.get(b"OC").cloned())
                .filter(|v| !matches!(v, Object::Null));
            if let Err(e) = crate::run::run_annot_appearance(
                doc,
                page,
                &mut *dev,
                &mut store,
                usage,
                user,
                oc.as_ref(),
                ap,
                matrix,
            ) {
                error.get_or_insert(e);
                break;
            }
            if crate::interrupt::interrupted() {
                break;
            }
        }
    }
    PageRun {
        error,
        store: store.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::testkit::{pdf_of, px, stream};
    use crate::draw::{Parts, Pix, render_page_with};

    const WHITE: [u8; 3] = [255, 255, 255];
    const RED: [u8; 3] = [255, 0, 0];

    /// A 100 by 100 page with nothing in its contents and these
    /// annotations, each [x0 y0 x1 y1] in page space: a square whose
    /// appearance fills its box red at [10 10 30 30]; the same hidden
    /// (/F 2) at [40 10 60 30] and NoView (/F 32) at [70 10 90 30]; a text
    /// widget with the same appearance at [10 60 30 80], one without a
    /// name (/T) at [40 60 60 80]; a blue square with no appearance at
    /// [70 60 90 80]; and the red square again at [40 40 60 50] in
    /// optional content that is off.
    fn annotated() -> Vec<u8> {
        let red = stream(
            "/Type/XObject/Subtype/Form/BBox[0 0 10 10]",
            "1 0 0 rg 0 0 10 10 re f",
        );
        pdf_of(&[
            "<</Type/Catalog/Pages 2 0 R/OCProperties<</OCGs[13 0 R]/D<</OFF[13 0 R]>>>>>>",
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]/Contents 4 0 R\
             /Annots[5 0 R 6 0 R 7 0 R 8 0 R 10 0 R 11 0 R 12 0 R]>>",
            &stream("", ""),
            "<</Type/Annot/Subtype/Square/Rect[10 10 30 30]/AP<</N 9 0 R>>>>",
            "<</Type/Annot/Subtype/Square/Rect[40 10 60 30]/F 2/AP<</N 9 0 R>>>>",
            "<</Type/Annot/Subtype/Square/Rect[70 10 90 30]/F 32/AP<</N 9 0 R>>>>",
            "<</Type/Annot/Subtype/Widget/FT/Tx/T(a)/Rect[10 60 30 80]/AP<</N 9 0 R>>>>",
            &red,
            "<</Type/Annot/Subtype/Widget/FT/Tx/Rect[40 60 60 80]/AP<</N 9 0 R>>>>",
            "<</Type/Annot/Subtype/Square/Rect[70 60 90 80]/C[0 0 1]/Border[0 0 4]>>",
            "<</Type/Annot/Subtype/Square/Rect[40 40 60 50]/OC 13 0 R/AP<</N 9 0 R>>>>",
            "<</Type/OCG/Name(off)>>",
        ])
    }

    /// Two pages that each show "abcde", the first with a stamp whose
    /// appearance shows "xyz", opened with `limit` glyphs a page.
    fn lettered(limit: usize) -> Document {
        lettered_within(crate::Limits {
            max_page_glyphs: limit,
            ..Default::default()
        })
    }

    fn lettered_within(limits: crate::Limits) -> Document {
        let content = stream("", "BT /F1 10 Tf 10 50 Td (ab) Tj (cde) Tj ET");
        let stamp = stream(
            "/Type/XObject/Subtype/Form/BBox[0 0 50 20]/Resources<</Font<</F1 6 0 R>>>>",
            "BT /F1 10 Tf 2 5 Td (xyz) Tj ET",
        );
        let pdf = pdf_of(&[
            "<</Type/Catalog/Pages 2 0 R>>",
            "<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2>>",
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]/Contents 5 0 R             /Resources<</Font<</F1 6 0 R>>>>/Annots[7 0 R]>>",
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]/Contents 5 0 R             /Resources<</Font<</F1 6 0 R>>>>>>",
            &content,
            "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
            "<</Type/Annot/Subtype/Stamp/Rect[10 10 60 30]/AP<</N 8 0 R>>>>",
            &stamp,
        ]);
        Document::open_with(pdf, limits).unwrap_or_else(|e| panic!("{e:?}"))
    }

    /// Each page's text through `parts`, or the error that ended it.
    fn texts(doc: &Document, parts: Parts) -> Vec<Result<String, String>> {
        let mut store = Store::default();
        (0..2)
            .map(|i| {
                let page = doc.page(i).unwrap_or_else(|e| panic!("{e:?}"));
                let opts = crate::stext::Options::default();
                let before = std::mem::take(&mut store);
                let (text, kept) =
                    crate::stext::extract_page_parts(doc, &page, opts, before, parts);
                store = kept;
                text.map(|p| {
                    let mut out = String::new();
                    crate::stext::blocks_text(&p.blocks, &mut out);
                    out.split_whitespace().collect::<Vec<_>>().join(" ")
                })
            })
            .collect()
    }

    #[test]
    fn a_page_shows_no_more_glyphs_than_its_limit() {
        let over = |n: usize| {
            Err(format!(
                "limit error: page shows more than {n} glyphs (--max-page-glyphs)"
            ))
        };
        let both = |a: &str, b: &str| vec![Ok(a.to_string()), Ok(b.to_string())];
        // Each page shows five glyphs: the count starts again on each.
        let doc = lettered(5);
        assert_eq!(texts(&doc, Parts::CONTENTS), both("abcde", "abcde"));
        assert!(!doc.diagnostics().has("limit.page_glyphs"));
        // One fewer ends each page where it shows its fifth.
        let doc = lettered(4);
        assert_eq!(texts(&doc, Parts::CONTENTS), [over(4), over(4)]);
        assert!(doc.diagnostics().has("limit.page_glyphs"));
        // An annotation's glyphs count with its page's: five and three.
        let doc = lettered(7);
        assert_eq!(texts(&doc, Parts::ALL), [over(7), Ok("abcde".into())]);
        let doc = lettered(8);
        assert_eq!(texts(&doc, Parts::ALL), both("abcde xyz", "abcde"));
        // The default is far from any page: a page of five is read.
        assert_eq!(crate::Limits::default().max_page_glyphs, 2_000_000);
    }

    #[test]
    fn a_page_runs_no_more_operators_than_its_limit() {
        let open = |n: usize| {
            lettered_within(crate::Limits {
                max_page_operators: n,
                ..Default::default()
            })
        };
        let over = |n: usize| {
            Err(format!(
                "limit error: page runs more than {n} operators (--max-page-operators)"
            ))
        };
        let both = |a: &str, b: &str| vec![Ok(a.to_string()), Ok(b.to_string())];
        // Each page's contents are six operators (BT Tf Td Tj Tj ET): the
        // count starts again on each page.
        let doc = open(6);
        assert_eq!(texts(&doc, Parts::CONTENTS), both("abcde", "abcde"));
        assert!(!doc.diagnostics().has("limit.page_operators"));
        let doc = open(5);
        assert_eq!(texts(&doc, Parts::CONTENTS), [over(5), over(5)]);
        assert!(doc.diagnostics().has("limit.page_operators"));
        // What an annotation's appearance runs counts with its page's: the
        // stamp's five operators and the four it is drawn with.
        let doc = open(14);
        assert_eq!(texts(&doc, Parts::ALL), [over(14), Ok("abcde".into())]);
        let doc = open(15);
        assert_eq!(texts(&doc, Parts::ALL), both("abcde xyz", "abcde"));
        assert_eq!(crate::Limits::default().max_page_operators, 5_000_000);
    }

    #[test]
    fn a_page_over_its_limit_is_drawn_as_far_as_it_got() {
        // The run ends with the error, as any error ends it, and whoever
        // drew the page has what was drawn before.
        let doc = lettered(2);
        let page = doc.page(1).unwrap_or_else(|e| panic!("{e:?}"));
        let mut list = crate::draw::list::DisplayList::default();
        let run = run_page_parts(
            &doc,
            &page,
            &mut list,
            Store::default(),
            Some("View"),
            Matrix::IDENTITY,
            Parts::ALL,
        );
        assert!(run.error.is_some_and(|e| e.starts_with("limit error: ")));
    }

    fn render(doc: &Document, parts: Parts) -> Pix {
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e:?}"));
        let (out, _) = render_page_with(
            doc,
            &page,
            Matrix::IDENTITY,
            1 << 20,
            None,
            Store::default(),
            false,
            parts,
        );
        let out = out.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(out.error, None);
        out.pix
    }

    /// The pixel at page point (x, y): rows run down from the page's top.
    fn at(pix: &Pix, x: i32, y: i32) -> Vec<u8> {
        px(pix, x, 100 - y)
    }

    #[test]
    fn the_whole_page_shows_what_mutool_draw_shows() {
        let doc = Document::open(annotated()).unwrap_or_else(|e| panic!("{e:?}"));
        let pix = render(&doc, Parts::ALL);
        // The appearance's box fills the /Rect.
        assert_eq!(at(&pix, 11, 11), RED);
        assert_eq!(at(&pix, 28, 28), RED);
        assert_eq!(at(&pix, 31, 20), WHITE);
        // Hidden, NoView and hidden optional content are not drawn.
        assert_eq!(at(&pix, 50, 20), WHITE);
        assert_eq!(at(&pix, 80, 20), WHITE);
        assert_eq!(at(&pix, 50, 45), WHITE);
        // A widget is, unless it has no field name.
        assert_eq!(at(&pix, 20, 70), RED);
        assert_eq!(at(&pix, 50, 70), WHITE);
        // The square without an appearance gets MuPDF's: a blue border
        // 4 wide inside its /Rect, nothing in the middle.
        let edge = at(&pix, 71, 70);
        assert!(edge[2] == 255 && edge[0] < 64 && edge[1] < 64, "{edge:?}");
        assert_eq!(at(&pix, 80, 70), WHITE);
        // MuPDF's pixels: `page.run` into a DrawDevice, ICC off.
        assert_eq!(
            crate::draw::testkit::md5(&pix),
            "d7b2a250f610968a1628868fd8f7d22b"
        );
    }

    #[test]
    fn contents_alone_leave_the_annotations_out() {
        let doc = Document::open(annotated()).unwrap_or_else(|e| panic!("{e:?}"));
        let pix = render(&doc, Parts::CONTENTS);
        assert!(pix.samples.iter().all(|&s| s == 255));
        // Widgets alone draw only the widget.
        let pix = render(
            &doc,
            Parts {
                contents: false,
                annots: false,
                widgets: true,
            },
        );
        assert_eq!(at(&pix, 20, 70), RED);
        assert_eq!(at(&pix, 20, 20), WHITE);
    }

    #[test]
    fn appearances_made_for_drawing_stay_out_of_the_document() {
        let doc = Document::open(annotated()).unwrap_or_else(|e| panic!("{e:?}"));
        let _ = render(&doc, Parts::ALL);
        assert!(!doc.is_edited());
        let square = Object::Ref(crate::object::ObjRef::new(11, 0));
        assert_eq!(annot_ap(&doc, &square), None);
        assert!(appearance::needs_synthesis(&doc, &square));
    }

    #[test]
    fn the_appearance_box_maps_onto_the_rect() {
        // A box turned by its matrix: [0 0 20 10] turned 90 degrees is
        // [-10 0 0 20], drawn onto [100 100 110 140].
        let pdf = pdf_of(&[
            "<</Type/Catalog/Pages 2 0 R>>",
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Annots[4 0 R 5 0 R]>>",
            "<</Subtype/Square/Rect[100 100 110 140]/AP<</N 6 0 R>>>>",
            "<</Subtype/Square/F 8/Rect[10 10 100 100]/AP<</N 6 0 R>>>>",
            &stream("/BBox[0 0 20 10]/Matrix[0 1 -1 0 0 0]", ""),
        ]);
        let doc = Document::open(pdf).unwrap_or_else(|e| panic!("{e:?}"));
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e:?}"));
        let ap = Object::Ref(crate::object::ObjRef::new(6, 0));
        let a = Object::Ref(crate::object::ObjRef::new(4, 0));
        let m = annot_transform(&doc, &page, &a, &ap);
        assert_eq!(
            [m.a, m.b, m.c, m.d, m.e, m.f],
            [1.0, 0.0, 0.0, 2.0, 110.0, 100.0]
        );
        // NoZoom: /Rect becomes the box's size as stored (20 by 10, not
        // turned), from its top left, [10 90 30 100]; the turned box is
        // drawn onto that.
        let z = Object::Ref(crate::object::ObjRef::new(5, 0));
        let m = annot_transform(&doc, &page, &z, &ap);
        assert_eq!(
            [m.a, m.b, m.c, m.d, m.e, m.f],
            [2.0, 0.0, 0.0, 0.5, 30.0, 90.0]
        );
    }

    /// A form with /NeedAppearances on a 300 by 200 page turned by
    /// `rotate`, and annotations with no appearances but one: a NoZoom
    /// NoRotate note, a stamp, a free text, a check box, a text field whose
    /// own appearance (a red box) the form asks to be made again, an
    /// unsigned signature, a line with ends and a highlight.
    fn made_on_loading(rotate: i32) -> Vec<u8> {
        let page = format!(
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 200]/Rotate {rotate}/Contents 4 0 R\
             /Annots[5 0 R 6 0 R 7 0 R 8 0 R 9 0 R 10 0 R 11 0 R 12 0 R]>>"
        );
        pdf_of(&[
            "<</Type/Catalog/Pages 2 0 R/AcroForm<</Fields[8 0 R 9 0 R 10 0 R]\
             /NeedAppearances true/DA(/Helv 0 Tf 0 g)/DR<</Font<</Helv 14 0 R>>>>>>>>",
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
            &page,
            &stream("", "0 0 1 rg 0 0 5 5 re f"),
            "<</Type/Annot/Subtype/Text/Rect[20 150 40 170]/F 24/C[1 1 0]/Name/Comment>>",
            "<</Type/Annot/Subtype/Stamp/Rect[60 120 200 170]/Name/Approved/C[0 0.5 0]>>",
            "<</Type/Annot/Subtype/FreeText/Rect[20 20 140 60]/DA(/Helv 12 Tf 1 0 0 rg)\
             /Contents(Hello there)>>",
            "<</Type/Annot/Subtype/Widget/FT/Btn/T(cb)/V/Yes/AS/Yes/Rect[160 20 180 40]\
             /MK<</BC[0 0 0]/CA(4)>>/DA(/ZaDb 0 Tf 0 g)>>",
            "<</Type/Annot/Subtype/Widget/FT/Tx/T(name)/V(Jane Doe)/Rect[200 20 290 40]\
             /DA(/Helv 10 Tf 0 0 1 rg)/AP<</N 13 0 R>>>>",
            "<</Type/Annot/Subtype/Widget/FT/Sig/T(sig)/Rect[200 60 290 100]>>",
            "<</Type/Annot/Subtype/Line/L[20 100 140 110]/Rect[10 90 150 120]/C[1 0 1]\
             /LE[/OpenArrow/Circle]>>",
            "<</Type/Annot/Subtype/Highlight/Rect[150 130 250 150]\
             /QuadPoints[150 150 250 150 150 130 250 130]/C[1 1 0]>>",
            &stream("/BBox[0 0 90 20]", "1 0 0 rg 0 0 90 20 re f"),
            "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>",
        ])
    }

    #[test]
    fn appearances_made_on_loading_draw_as_mupdf_draws_them() {
        // MuPDF's pixels (`page.run`, ICC off) at each rotation: the note
        // stays upright on the turned page, and the signature field shows
        // MuPDF's sign-here arrow.
        for (rotate, want) in [
            (0, "67a3017ce607246a722b1781b7dace10"),
            (90, "05e918ed1a610154a16afcd98f30a2f2"),
        ] {
            let doc = Document::open(made_on_loading(rotate)).unwrap_or_else(|e| panic!("{e:?}"));
            let pix = render(&doc, Parts::ALL);
            assert_eq!(crate::draw::testkit::md5(&pix), want, "rotate {rotate}");
            assert!(!doc.is_edited());
        }
    }

    #[test]
    fn text_of_the_whole_page_is_what_mutool_draw_writes() {
        let doc = Document::open(made_on_loading(0)).unwrap_or_else(|e| panic!("{e:?}"));
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e:?}"));
        let text = |parts| {
            let (p, _) = crate::stext::extract_page_parts(
                &doc,
                &page,
                crate::stext::Options::default(),
                Store::default(),
                parts,
            );
            let mut out = String::new();
            crate::stext::blocks_text(&p.unwrap_or_else(|e| panic!("{e}")).blocks, &mut out);
            out
        };
        // `mutool draw -F txt`, less its form feed.
        assert_eq!(
            text(Parts::ALL),
            "APPROVED\n\nHello there\n\n3\nJane Doe\n\nSIGN\n\n"
        );
        assert_eq!(text(Parts::CONTENTS), "");
    }

    #[test]
    fn the_appearance_is_the_state_as_names() {
        let pdf = pdf_of(&[
            "<</Type/Catalog/Pages 2 0 R>>",
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Annots[4 0 R]>>",
            "<</Subtype/Widget/FT/Btn/T(c)/AS/On/Rect[0 0 10 10]/AP<</N<</On 5 0 R/Off 6 0 R>>>>>>",
            &stream("/BBox[0 0 1 1]", ""),
            &stream("/BBox[0 0 1 1]", ""),
        ]);
        let doc = Document::open(pdf).unwrap_or_else(|e| panic!("{e:?}"));
        let w = Object::Ref(crate::object::ObjRef::new(4, 0));
        assert_eq!(
            annot_ap(&doc, &w),
            Some(Object::Ref(crate::object::ObjRef::new(5, 0)))
        );
    }
}
