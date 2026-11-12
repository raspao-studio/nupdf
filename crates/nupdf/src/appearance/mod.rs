// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-appearance.c (pdf_write_appearance,
// pdf_update_appearance and the shared writers) and source/pdf/pdf-annot.c
// (pdf_annot_ap, pdf_annot_border_dash_count, pdf_annot_border_dash_item),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Appearance synthesis: the appearance stream MuPDF makes for an
//! annotation that has none, or whose appearance is to be made again, with
//! the /Rect and /RD it writes back.
//!
//! Each type's writer follows its `pdf_write_*_appearance`. Arithmetic is
//! kept in C's order on `f32`, `a * b + c` in one C expression as
//! `madd` (fused where MuPDF's build fuses it: see `fma`), so the numbers
//! printed are MuPDF's.

#![allow(dead_code)] // Until every writer is ported.

use crate::fma::Madd;
use std::sync::Arc;

use crate::annot;
use crate::document::Document;
use crate::geometry::{Matrix, Point, Rect};
use crate::object::{Dict, Object};

mod commit;
mod markup;
mod shapes;
mod text;
mod widget;

pub use commit::Committer;

pub use widget::{ButtonSynthesis, Form, button as synthesize_button};

/// An appearance made for an annotation: the annotation as it is after
/// (its /Rect and /RD as the writer left them) and the form XObject's
/// parts, as `pdf_new_xobject` puts them together.
#[derive(Debug, Clone)]
pub struct Synthesis {
    pub annot: Object,
    pub bbox: Rect,
    pub matrix: Matrix,
    pub resources: Option<Dict>,
    pub content: Vec<u8>,
}

/// `pdf_annot_ap` for an annotation not under the pointer: its normal
/// appearance stream, or the one of its states /AS names.
pub fn appearance(doc: &Document, annot: &Object) -> Option<Object> {
    let ap = doc.dict_get(annot, b"AP");
    // As stored: only a reference to a stream is the appearance itself.
    let n = doc
        .resolve(&ap)
        .as_dict()
        .and_then(|d| d.get(b"N").cloned())
        .unwrap_or(Object::Null);
    if let Object::Ref(_) = n
        && matches!(doc.resolve(&n), Object::Stream(_))
    {
        return Some(n);
    }
    let state = doc.resolve(&doc.dict_get(annot, b"AS"));
    let name = state.as_name()?;
    Some(doc.dict_get(&n, name)).filter(|v| !matches!(v, Object::Null))
}

/// Whether loading the page has MuPDF make an appearance for `annot`
/// (`local_synthesis` in `pdf_update_appearance`): it has none; or it is a
/// widget and the form's /NeedAppearances asks for them all; or it is an
/// unsigned signature, whose own appearance MuPDF ignores. Never popups,
/// links or signed signatures.
pub fn needs_synthesis(doc: &Document, annot: &Object) -> bool {
    match annot::type_name(doc, annot) {
        "Popup" | "Link" => false,
        "Widget" if !widget_made_again(doc, annot) => false,
        "Widget" => {
            crate::form::need_appearances(doc)
                || crate::form::type_string(doc, annot) == "signature"
                || appearance(doc, annot).is_none()
        }
        _ => appearance(doc, annot).is_none(),
    }
}

/// Whether asking for a widget's appearance has MuPDF make it again: not
/// for a signed signature.
pub fn widget_made_again(doc: &Document, widget: &Object) -> bool {
    !crate::form::is_signed(doc, widget)
}

/// Which of a page's annotations (`annot::page_annots`, in order) MuPDF
/// makes again when each is asked to be (`pdf_update_appearance`): all but
/// a Stamp with a non-standard name that has an appearance. That is its
/// own, or the one loading the page made for it; but those live in a
/// local xref that making any annotation before it again throws away.
pub fn made_again(doc: &Document, annots: &[Object], ctm: Matrix) -> Vec<bool> {
    let mut discarded = false;
    annots
        .iter()
        .map(|a| {
            let keeps = annot::type_name(doc, a) == "Stamp"
                && !text::is_standard_stamp(doc, a)
                && (appearance(doc, a).is_some()
                    || (!discarded && synthesize(doc, a, ctm).is_ok()));
            discarded |= !keeps;
            !keeps
        })
        .collect()
}

/// The writer's state: the annotation as it is being changed, the page
/// transform, the content made so far and the resources it needs.
pub(crate) struct Writer<'a> {
    pub doc: &'a Document,
    pub annot: Object,
    pub ctm: Matrix,
    pub buf: Vec<u8>,
    pub res: Option<Dict>,
}

impl Writer<'_> {
    /// `fz_append_string`.
    pub fn s(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
    }

    /// `%g` as `fz_append_printf` writes it.
    pub fn g(&mut self, f: f32) {
        crate::print::fmtfloat(f, &mut self.buf);
    }

    /// Numbers then an operator, as `"%g %g ... op\n"`.
    pub fn op(&mut self, nums: &[f32], op: &str) {
        for &n in nums {
            self.g(n);
            self.buf.push(b' ');
        }
        self.s(op);
        self.buf.push(b'\n');
    }

    /// `pdf_dict_get` on the annotation.
    pub fn get(&self, key: &[u8]) -> Object {
        self.doc.dict_get(&self.annot, key)
    }

    /// `pdf_dict_put` on the annotation.
    pub fn put(&mut self, key: &[u8], value: Object) {
        let mut d = self
            .doc
            .resolve(&self.annot)
            .as_dict()
            .cloned()
            .unwrap_or_default();
        d.put(key, value);
        self.annot = Object::Dict(Arc::new(d));
    }

    /// `pdf_dict_del` on the annotation.
    pub fn del(&mut self, key: &[u8]) {
        let mut d = self
            .doc
            .resolve(&self.annot)
            .as_dict()
            .cloned()
            .unwrap_or_default();
        d.remove(key);
        self.annot = Object::Dict(Arc::new(d));
    }

    /// `pdf_dict_put_rect`.
    pub fn put_rect(&mut self, key: &[u8], r: Rect) {
        self.put(key, rect_object(r));
    }

    pub fn subtype(&self) -> &'static str {
        annot::type_name(self.doc, &self.annot)
    }

    /// `pdf_write_opacity_blend_mode`: an ExtGState /H for the opacity and,
    /// with `multiply`, the blend mode.
    pub fn opacity_blend_mode(&mut self, multiply: bool) {
        let opacity = annot::opacity(self.doc, &self.annot);
        if !multiply && opacity == 1.0 {
            return;
        }
        let mut h = Dict::new();
        h.put(b"Type", Object::name(b"ExtGState"));
        if multiply {
            h.put(b"BM", Object::name(b"Multiply"));
        }
        if opacity < 1.0 {
            h.put(b"CA", Object::Real(opacity));
            h.put(b"ca", Object::Real(opacity));
        }
        let mut egs = Dict::new();
        egs.put(b"H", Object::Dict(Arc::new(h)));
        self.res
            .get_or_insert_with(Dict::new)
            .put(b"ExtGState", Object::Dict(Arc::new(egs)));
        self.s("/H gs\n");
    }

    /// `pdf_write_opacity`.
    pub fn opacity(&mut self) {
        self.opacity_blend_mode(false);
    }

    /// The border style dictionary's dash array, or the legacy /Border's;
    /// an error for types without a border, as MuPDF's getters throw.
    fn dashes(&self) -> Result<Vec<f32>, String> {
        self.check_border()?;
        let bs = self.get(b"BS");
        let mut d = self.doc.resolve(&self.doc.dict_get(&bs, b"D"));
        let border = self.doc.resolve(&self.get(b"Border"));
        if !matches!(d, Object::Array(_))
            && let Object::Array(b) = &border
        {
            d = self.doc.resolve(b.get(3).unwrap_or(&Object::Null));
        }
        Ok(d.as_array()
            .unwrap_or(&[])
            .iter()
            .map(|v| self.doc.resolve(v).to_real())
            .collect())
    }

    fn check_border(&self) -> Result<(), String> {
        annot::border(self.doc, &self.annot)
            .map(|_| ())
            .ok_or_else(|| self.no_property("BS"))
    }

    /// The error `check_allowed_subtypes` throws.
    pub fn no_property(&self, property: &str) -> String {
        let s = self.doc.resolve(&self.get(b"Subtype"));
        format!(
            "{} annotations have no {} property",
            String::from_utf8_lossy(s.as_name().unwrap_or(b"")),
            property
        )
    }

    /// `pdf_write_dash_pattern`.
    pub fn dash_pattern(&mut self) -> Result<(), String> {
        let d = self.dashes()?;
        if d.is_empty() {
            return Ok(());
        }
        self.s("[");
        for (i, &len) in d.iter().enumerate() {
            if i > 0 {
                self.s(" ");
            }
            self.g(len);
        }
        self.s("]0 d\n");
        Ok(())
    }

    /// `pdf_annot_border_width`, an error for types without a border.
    pub fn border_width(&self) -> Result<f32, String> {
        annot::border(self.doc, &self.annot)
            .map(|b| b.1)
            .ok_or_else(|| self.no_property("BS"))
    }

    /// `pdf_write_border_appearance`: the line width, written and given.
    pub fn border(&mut self) -> Result<f32, String> {
        let w = self.border_width()?;
        self.op(&[w], "w");
        Ok(w)
    }

    /// `write_color`: false when there is no colour to write.
    pub fn color(&mut self, c: &[f32], stroke: bool) -> bool {
        match c.len() {
            4 => self.op(c, if stroke { "K" } else { "k" }),
            3 => self.op(c, if stroke { "RG" } else { "rg" }),
            1 => self.op(c, if stroke { "G" } else { "g" }),
            _ => return false,
        }
        true
    }

    /// `write_color0`: black where there is no colour.
    pub fn color0(&mut self, c: &[f32], stroke: bool) {
        if c.is_empty() {
            self.s(if stroke { "0 G\n" } else { "0 g\n" });
        } else {
            self.color(c, stroke);
        }
    }

    /// `pdf_write_stroke_color_appearance`.
    pub fn stroke_color(&mut self) -> bool {
        let c = annot::color(self.doc, &self.annot);
        self.color(&c, true)
    }

    /// `pdf_write_fill_color_appearance`.
    pub fn fill_color(&mut self) -> bool {
        let c = annot::color(self.doc, &self.annot);
        self.color(&c, false)
    }

    /// `pdf_write_interior_fill_color_appearance`, an error for types
    /// without an interior colour.
    pub fn interior_fill_color(&mut self) -> Result<bool, String> {
        let c =
            annot::interior_color(self.doc, &self.annot).ok_or_else(|| self.no_property("IC"))?;
        Ok(self.color(&c, false))
    }

    /// `pdf_is_dark_fill_color`.
    pub fn is_dark_fill_color(&self) -> bool {
        let c = annot::color(self.doc, &self.annot);
        let gray = match c.len() {
            1 => c[0],
            3 => c[2].madd(0.11, c[0].madd(0.3, c[1] * 0.59)),
            4 => {
                let g = c[2].madd(0.11, c[0].madd(0.3, c[1] * 0.59)) + c[3];
                1.0 - g.min(1.0)
            }
            _ => 1.0,
        };
        gray < 0.25
    }

    /// `maybe_stroke_and_fill`.
    pub fn stroke_and_fill(&mut self, sc: bool, ic: bool) {
        self.s(match (sc, ic) {
            (true, true) => "b\n",
            (true, false) => "S\n",
            (false, true) => "f\n",
            (false, false) => "n\n",
        });
    }

    /// `maybe_stroke`.
    pub fn stroke(&mut self, sc: bool) {
        self.s(if sc { "S\n" } else { "n\n" });
    }
}

/// `pdf_new_rect`: four reals.
pub(crate) fn rect_object(r: Rect) -> Object {
    Object::Array(Arc::new(vec![
        Object::Real(r.x0),
        Object::Real(r.y0),
        Object::Real(r.x1),
        Object::Real(r.y1),
    ]))
}

/// `pdf_array_get_real` on a resolved array.
pub(crate) fn real_at(doc: &Document, a: &Object, i: usize) -> f32 {
    doc.resolve(a)
        .as_array()
        .and_then(|a| a.get(i))
        .map_or(0.0, |v| doc.resolve(v).to_real())
}

/// `pdf_array_len` on a resolved array.
pub(crate) fn len(doc: &Document, a: &Object) -> usize {
    doc.resolve(a).as_array().map_or(0, <[Object]>::len)
}

/// `fz_make_point`.
pub(crate) const fn pt(x: f32, y: f32) -> Point {
    Point::new(x, y)
}

/// `pdf_write_appearance` then what `pdf_update_appearance` does with it:
/// the new /Rect put into the annotation. `ctm` is the page's transform.
pub fn synthesize(doc: &Document, annot: &Object, ctm: Matrix) -> Result<Synthesis, String> {
    let field = annot;
    let annot = doc.resolve(annot);
    if annot.as_dict().is_none() {
        return Err("annotation is not a dictionary".into());
    }
    let mut w = Writer {
        doc,
        annot,
        ctm,
        buf: Vec::with_capacity(1024),
        res: None,
    };
    let mut rect = doc.to_rect(Some(&w.get(b"Rect")));
    let mut matrix = Matrix::IDENTITY;
    let bbox = match w.subtype() {
        "Ink" => {
            shapes::ink(&mut w, &mut rect)?;
            rect
        }
        "Polygon" => {
            shapes::polygon(&mut w, &mut rect, true)?;
            rect
        }
        "PolyLine" => {
            shapes::polygon(&mut w, &mut rect, false)?;
            rect
        }
        "Line" => {
            shapes::line(&mut w, &mut rect)?;
            rect
        }
        "Square" => {
            shapes::square(&mut w, &mut rect)?;
            rect
        }
        "Circle" => {
            shapes::circle(&mut w, &mut rect)?;
            rect
        }
        "Caret" => markup::caret(&mut w, &mut rect)?,
        "Text" | "FileAttachment" | "Sound" => markup::icon(&mut w, &mut rect)?,
        "Highlight" => {
            markup::highlight(&mut w, &mut rect)?;
            rect
        }
        "Underline" => {
            markup::underline(&mut w, &mut rect)?;
            rect
        }
        "StrikeOut" => {
            markup::strike_out(&mut w, &mut rect)?;
            rect
        }
        "Squiggly" => {
            markup::squiggly(&mut w, &mut rect)?;
            rect
        }
        "Redact" => {
            markup::redact(&mut w, &mut rect)?;
            rect
        }
        "Stamp" => text::stamp(&mut w, &mut rect, &mut matrix)?,
        "FreeText" => text::free_text(&mut w, &mut rect, &mut matrix)?,
        "Widget" => widget::widget(&mut w, rect, &mut matrix, field)?,
        _ => {
            let s = doc.resolve(&w.get(b"Subtype"));
            return Err(format!(
                "cannot create appearance stream for {} annotations",
                String::from_utf8_lossy(s.as_name().unwrap_or(b""))
            ));
        }
    };
    w.put_rect(b"Rect", rect);
    Ok(Synthesis {
        annot: w.annot,
        bbox,
        matrix,
        resources: w.res,
        content: w.buf,
    })
}
