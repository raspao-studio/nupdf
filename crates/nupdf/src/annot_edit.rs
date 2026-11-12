// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-annot.c (pdf_create_annot,
// pdf_create_annot_raw, pdf_delete_annot, pdf_add_popup_annot and the
// pdf_set_annot_* setters) and source/pdf/pdf-appearance.c
// (pdf_update_appearance, update_all_appearances),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Adding and deleting annotations as MuPDF's annotation API does it, and
//! making the new ones' appearances as updating the page makes them.
//!
//! Coordinates given to the setters are in page space (MuPDF's: origin at
//! the top left, y down), and go into the document through the inverse of
//! the page transform, as MuPDF's setters take them.

use std::sync::Arc;

use crate::appearance;
use crate::document::Document;
use crate::geometry::{Matrix, Point, Rect};
use crate::object::{Dict, ObjRef, Object};

const PRINT: i32 = 1 << 2;
const NO_ZOOM: i32 = 1 << 3;
const NO_ROTATE: i32 = 1 << 4;

const RECT: &[&str] = &[
    "Text",
    "FreeText",
    "Square",
    "Circle",
    "Redact",
    "Stamp",
    "Caret",
    "Popup",
    "FileAttachment",
    "Sound",
    "Movie",
    "Widget",
    "3D",
    "RichMedia",
];
const MARKUP: &[&str] = &[
    "Text",
    "FreeText",
    "Line",
    "Square",
    "Circle",
    "Polygon",
    "PolyLine",
    "Highlight",
    "Underline",
    "Squiggly",
    "StrikeOut",
    "Redact",
    "Stamp",
    "Caret",
    "Ink",
    "FileAttachment",
    "Sound",
];
const POPUP: &[&str] = &[
    "Text",
    "FreeText",
    "Line",
    "Square",
    "Circle",
    "Polygon",
    "PolyLine",
    "Highlight",
    "Underline",
    "Squiggly",
    "StrikeOut",
    "Stamp",
    "Caret",
    "Ink",
    "FileAttachment",
    "Sound",
    "Redact",
];
const BORDER: &[&str] = &[
    "Circle", "FreeText", "Ink", "Line", "Polygon", "PolyLine", "Square", "Widget",
];
const INTERIOR: &[&str] = &["Circle", "Line", "PolyLine", "Polygon", "Square"];
const ICON: &[&str] = &["FileAttachment", "Sound", "Stamp", "Text"];
const QUADS: &[&str] = &[
    "Highlight",
    "Link",
    "Squiggly",
    "StrikeOut",
    "Underline",
    "Redact",
];
const VERTICES: &[&str] = &["PolyLine", "Polygon"];

fn reference(num: i32) -> Object {
    Object::Ref(ObjRef::new(num, 0))
}

fn reals(v: &[f32]) -> Object {
    Object::Array(Arc::new(v.iter().map(|&f| Object::Real(f)).collect()))
}

fn rect_object(r: Rect) -> Object {
    reals(&[r.x0, r.y0, r.x1, r.y1])
}

/// The annotation edits made in one go, and what they leave to do: the
/// pages loaded, in order, and the annotations whose appearance is to be
/// made again when the pages are updated.
#[derive(Debug)]
pub struct Editor<'a> {
    doc: &'a Document,
    loaded: Vec<i32>,
    dirty: Vec<i32>,
    created: Vec<(i32, i32)>,
    /// New appearances written as MuPDF writes them, fonts first.
    commit: appearance::Committer,
}

/// An appearance that could not be made: page (from 0), object, why.
pub type Unmade = (i32, i32, String);

impl<'a> Editor<'a> {
    pub fn new(doc: &'a Document) -> Editor<'a> {
        Editor {
            doc,
            loaded: Vec::new(),
            dirty: Vec::new(),
            created: Vec::new(),
            commit: appearance::Committer::new(),
        }
    }

    /// `pdf_load_page`, for the edits to come: the page must load. Pages
    /// are updated in the reverse of the order they were loaded in, as
    /// MuPDF keeps its open pages.
    pub fn load_page(&mut self, index: i32) -> Result<(), String> {
        if self.loaded.contains(&index) {
            return Ok(());
        }
        // What pdf_load_page looks up, and fails on: the page object; with
        // /Annots, its transform; then its inherited /Resources.
        let page = self.doc.page(index).map_err(|e| e.0)?;
        if crate::link::has_annots(self.doc, &page) {
            self.doc.page_transform(&page).map_err(|e| e.0)?;
        }
        self.doc.inherited(&page, b"Resources").map_err(|e| e.0)?;
        if self.loaded.is_empty() {
            self.doc.settle_page_count();
        }
        self.loaded.push(index);
        Ok(())
    }

    fn page(&self, index: i32) -> Result<Object, String> {
        self.doc.page(index).map_err(|e| e.0)
    }

    /// `pdf_page_transform`.
    fn ctm(&self, index: i32) -> Result<Matrix, String> {
        let page = self.page(index)?;
        Ok(self.doc.page_transform(&page).map_err(|e| e.0)?.1)
    }

    fn subtype(&self, num: i32) -> String {
        let s = self.doc.dict_get(&reference(num), b"Subtype");
        String::from_utf8_lossy(s.as_name().unwrap_or(b"")).into_owned()
    }

    /// `check_allowed_subtypes`.
    fn check(&self, num: i32, property: &str, allowed: &[&str]) -> Result<(), String> {
        let s = self.subtype(num);
        if allowed.contains(&s.as_str()) {
            Ok(())
        } else {
            Err(format!("{s} annotations have no {property} property"))
        }
    }

    fn dict(&self, num: i32) -> Dict {
        self.doc
            .resolve(&reference(num))
            .as_dict()
            .cloned()
            .unwrap_or_default()
    }

    /// Changes the annotation, and asks for its appearance to be made
    /// again (`pdf_dirty_annot`).
    fn edit(&mut self, num: i32, f: impl FnOnce(&mut Dict)) {
        let mut d = self.dict(num);
        f(&mut d);
        self.doc.alter_object(num, Object::Dict(Arc::new(d)));
        if !self.dirty.contains(&num) {
            self.dirty.push(num);
        }
    }

    /// `pdf_create_annot_raw`: a new annotation of type `subtype` at the
    /// end of the page's /Annots. Its object number.
    pub fn create_raw(&mut self, index: i32, subtype: &str) -> Result<i32, String> {
        if !crate::annot::TYPES.contains(&subtype) {
            return Err("cannot create unknown annotation".into());
        }
        self.load_page(index)?;
        let doc = self.doc;
        let num = doc.create_object();
        let mut d = Dict::new();
        d.put(b"Type", Object::name(b"Annot"));
        d.put(b"Subtype", Object::name(subtype.as_bytes()));
        let mut err = None;
        crate::edit::edit_page(doc, index, &mut |page| {
            // An /Annots of its own object is copied into the page.
            let mut list = match page.get(b"Annots").cloned() {
                Some(v @ Object::Ref(_)) => match doc.resolve(&v) {
                    Object::Array(a) => (*a).clone(),
                    _ => Vec::new(),
                },
                Some(Object::Array(a)) => (*a).clone(),
                _ => Vec::new(),
            };
            list.push(reference(num));
            page.put(b"Annots", Object::Array(Arc::new(list)));
        })
        .unwrap_or_else(|e| err = Some(e));
        if let Some(e) = err {
            return Err(e);
        }
        doc.alter_object(num, Object::Dict(Arc::new(d)));
        self.dirty.push(num);
        self.created.push((index, num));
        Ok(num)
    }

    /// `pdf_create_annot`: [`Editor::create_raw`] and each type's
    /// defaults: flags, rectangle, colour, the popup of a note, the
    /// default appearance of free text.
    pub fn create(&mut self, index: i32, subtype: &str) -> Result<i32, String> {
        const BLACK: [f32; 3] = [0.0, 0.0, 0.0];
        const RED: [f32; 3] = [1.0, 0.0, 0.0];
        const GREEN: [f32; 3] = [0.0, 1.0, 0.0];
        const BLUE: [f32; 3] = [0.0, 0.0, 1.0];
        const YELLOW: [f32; 3] = [1.0, 1.0, 0.0];
        const MAGENTA: [f32; 3] = [1.0, 0.0, 1.0];
        let num = self.create_raw(index, subtype)?;
        let mut flags = PRINT;
        match subtype {
            "Text" | "FileAttachment" | "Sound" => {
                flags = PRINT | NO_ZOOM | NO_ROTATE;
                self.set_rect(index, num, Rect::new(12.0, 12.0, 28.0, 28.0))?;
                self.set_color(num, &YELLOW)?;
                self.set_popup(index, num, Rect::new(32.0, 12.0, 232.0, 112.0))?;
            }
            "FreeText" => {
                // Adobe's undocumented /Rotate, to match the page's.
                let page = self.page(index)?;
                let rot = self
                    .doc
                    .inherited(&page, b"Rotate")
                    .map_err(|e| e.0)?
                    .map_or(0, |v| self.doc.resolve(&v).to_int());
                if rot != 0 {
                    self.edit(num, |d| d.put(b"Rotate", Object::Int(i64::from(rot))));
                }
                self.set_rect(index, num, Rect::new(12.0, 12.0, 212.0, 112.0))?;
                self.set_border_width(num, 0.0)?;
                self.set_default_appearance(num, "Helv", 12.0, &BLACK)?;
            }
            "Stamp" => {
                self.set_rect(index, num, Rect::new(12.0, 12.0, 202.0, 62.0))?;
                self.set_color(num, &RED)?;
                self.set_icon(num, Some("Draft"))?;
            }
            "Caret" => {
                self.set_rect(index, num, Rect::new(12.0, 12.0, 30.0, 27.0))?;
                self.set_color(num, &BLUE)?;
            }
            "Line" => {
                let (a, b) = (Point::new(12.0, 12.0), Point::new(112.0, 62.0));
                self.set_line(index, num, a, b)?;
                self.set_border_width(num, 1.0)?;
                self.set_color(num, &RED)?;
            }
            "Square" | "Circle" => {
                self.set_rect(index, num, Rect::new(12.0, 12.0, 112.0, 62.0))?;
                self.edit(num, |d| d.put(b"RD", reals(&[0.5; 4])));
                self.set_border_width(num, 1.0)?;
                self.set_color(num, &RED)?;
            }
            "Polygon" | "PolyLine" | "Ink" => {
                self.set_border_width(num, 1.0)?;
                self.edit(num, |d| d.put(b"RD", reals(&[0.5; 4])));
                self.set_color(num, &RED)?;
            }
            "Highlight" => self.set_color(num, &YELLOW)?,
            "Underline" => self.set_color(num, &GREEN)?,
            "StrikeOut" => self.set_color(num, &RED)?,
            "Squiggly" => self.set_color(num, &MAGENTA)?,
            _ => {}
        }
        let page = self.page(index)?;
        self.edit(num, |d| {
            d.put(b"P", page);
            d.put(b"F", Object::Int(i64::from(flags)));
        });
        Ok(num)
    }

    /// `pdf_annot_rect_diff`.
    fn rect_diff(&self, num: i32) -> Rect {
        let rd = self.doc.dict_get(&reference(num), b"RD");
        match rd {
            Object::Array(a) => {
                let n = |i: usize| a.get(i).map_or(0.0, |v| self.doc.resolve(v).to_real());
                Rect::new(n(0), n(1), n(2), n(3))
            }
            _ => Rect::new(0.0, 0.0, 0.0, 0.0),
        }
    }

    fn rect_of(&self, num: i32) -> Rect {
        self.doc
            .to_rect(Some(&self.doc.dict_get(&reference(num), b"Rect")))
    }

    /// `pdf_set_annot_rect`: `rect` is the design box, in page space; the
    /// /Rect is it with /RD around it.
    pub fn set_rect(&mut self, index: i32, num: i32, rect: Rect) -> Result<(), String> {
        self.check(num, "Rect", RECT)?;
        let ctm = self.ctm(index)?;
        let mut rect = rect.transform(ctm.invert());
        let rd = self.rect_diff(num);
        rect.x0 -= rd.x0;
        rect.x1 += rd.x1;
        rect.y0 -= rd.y0;
        rect.y1 += rd.y1;
        self.edit(num, |d| d.put(b"Rect", rect_object(rect)));
        // The callout line is made again from where it points.
        if self.subtype(num) == "FreeText" {
            let p = self.callout_point(ctm, num);
            self.set_callout_point(ctm, num, p);
        }
        Ok(())
    }

    /// `pdf_annot_rect`: the design box in page space.
    fn design_rect(&self, ctm: Matrix, num: i32) -> Rect {
        let mut rect = self.rect_of(num);
        let rd = self.rect_diff(num);
        rect.x0 += rd.x0;
        rect.x1 -= rd.x1;
        rect.y0 += rd.y0;
        rect.y1 -= rd.y1;
        rect.transform(ctm)
    }

    /// `pdf_annot_callout_point`: the first point of /CL, in page space.
    fn callout_point(&self, ctm: Matrix, num: i32) -> Point {
        match self.doc.dict_get(&reference(num), b"CL") {
            Object::Array(a) if a.len() == 4 || a.len() == 6 => {
                let n = |i: usize| a.get(i).map_or(0.0, |v| self.doc.resolve(v).to_real());
                ctm.transform_xy(n(0), n(1))
            }
            _ => Point::new(0.0, 0.0),
        }
    }

    /// `pdf_set_annot_callout_point`: a callout line from `p` to where the
    /// line from the text box's centre to `p` leaves the box.
    fn set_callout_point(&mut self, ctm: Matrix, num: i32, p: Point) {
        let rect = self.design_rect(ctm, num);
        let a = Point::new((rect.x0 + rect.x1) / 2.0, (rect.y0 + rect.y1) / 2.0);
        let mut b = p;
        let inside = p.x >= rect.x0 && p.x < rect.x1 && p.y >= rect.y0 && p.y < rect.y1;
        if inside {
            self.set_callout_line(ctm, num, &[p, a]);
            return;
        }
        // Cohen-Sutherland, simplified. Each step clamps a coordinate, so
        // a few are enough; the bound only guards against the unexpected.
        let m = (b.y - a.y) / (b.x - a.x);
        for _ in 0..64 {
            if b.y < rect.y0 {
                b.x = a.x + (rect.y0 - a.y) / m;
                b.y = rect.y0;
            } else if b.y > rect.y1 {
                b.x = a.x + (rect.y1 - a.y) / m;
                b.y = rect.y1;
            } else if b.x < rect.x0 {
                b.y = (rect.x0 - a.x).mul_add(m, a.y);
                b.x = rect.x0;
            } else if b.x > rect.x1 {
                b.y = (rect.x1 - a.x).mul_add(m, a.y);
                b.x = rect.x1;
            } else {
                break;
            }
        }
        self.set_callout_line(ctm, num, &[p, b]);
    }

    /// `pdf_set_annot_callout_line`. MuPDF takes the points through the
    /// page transform itself, not its inverse.
    fn set_callout_line(&mut self, ctm: Matrix, num: i32, line: &[Point]) {
        let mut v = Vec::new();
        for p in line {
            let q = ctm.transform_xy(p.x, p.y);
            v.push(q.x);
            v.push(q.y);
        }
        self.edit(num, |d| d.put(b"CL", reals(&v)));
    }

    /// `pdf_set_annot_contents`.
    pub fn set_contents(&mut self, num: i32, text: &str) {
        self.edit(num, |d| {
            d.put(b"Contents", crate::edit::text_string(text));
            d.remove(b"RC");
        });
    }

    /// `pdf_set_annot_author` (/T).
    pub fn set_author(&mut self, num: i32, author: &str) -> Result<(), String> {
        self.check(num, "T", MARKUP)?;
        self.edit(num, |d| d.put(b"T", crate::edit::text_string(author)));
        Ok(())
    }

    /// `pdf_set_annot_subject` (/Subj).
    pub fn set_subject(&mut self, num: i32, subject: &str) -> Result<(), String> {
        self.check(num, "Subj", MARKUP)?;
        self.edit(num, |d| d.put(b"Subj", crate::edit::text_string(subject)));
        Ok(())
    }

    /// `pdf_set_annot_flags` (/F).
    pub fn set_flags(&mut self, num: i32, flags: i32) {
        self.edit(num, |d| d.put(b"F", Object::Int(i64::from(flags))));
    }

    fn color_imp(&mut self, num: i32, key: &[u8], color: &[f32]) -> Result<(), String> {
        if ![0, 1, 3, 4].contains(&color.len()) {
            return Err("color must be 0, 1, 3 or 4 components".into());
        }
        self.edit(num, |d| d.put(key, reals(color)));
        Ok(())
    }

    /// `pdf_set_annot_color` (/C).
    pub fn set_color(&mut self, num: i32, color: &[f32]) -> Result<(), String> {
        self.color_imp(num, b"C", color)
    }

    /// `pdf_set_annot_interior_color` (/IC).
    pub fn set_interior_color(&mut self, num: i32, color: &[f32]) -> Result<(), String> {
        self.check(num, "IC", INTERIOR)?;
        self.color_imp(num, b"IC", color)
    }

    /// `pdf_set_annot_opacity` (/CA, removed at 1).
    pub fn set_opacity(&mut self, num: i32, opacity: f32) {
        self.edit(num, |d| {
            if opacity != 1.0 {
                d.put(b"CA", Object::Real(opacity));
            } else {
                d.remove(b"CA");
            }
        });
    }

    /// `pdf_set_annot_icon_name` (/Name).
    pub fn set_icon(&mut self, num: i32, name: Option<&str>) -> Result<(), String> {
        self.check(num, "Name", ICON)?;
        self.edit(num, |d| match name {
            Some(n) => d.put(b"Name", Object::name(n.as_bytes())),
            None => d.remove(b"Name"),
        });
        Ok(())
    }

    /// `pdf_set_annot_border_width`: /BS /W, with /Rect grown or shrunk by
    /// half the change (and, for squares and circles, /RD with it, so the
    /// border stays centred where it was); the legacy /Border goes.
    pub fn set_border_width(&mut self, num: i32, width: f32) -> Result<(), String> {
        self.check(num, "BS", BORDER)?;
        let doc = self.doc;
        let bs_value = self.dict(num).get(b"BS").cloned();
        let mut bs = match bs_value.as_ref().map(|v| doc.resolve(v)) {
            Some(Object::Dict(d)) => (*d).clone(),
            _ => Dict::new(),
        };
        bs.put(b"Type", Object::name(b"Border"));
        let old = bs.get(b"W").map_or(0.0, |v| doc.resolve(v).to_real());
        bs.put(b"W", Object::Real(width));
        let bs = Object::Dict(Arc::new(bs));
        let bs_direct = match &bs_value {
            Some(Object::Ref(r)) if matches!(doc.resolve(&reference(r.num)), Object::Dict(_)) => {
                doc.alter_object(r.num, bs);
                None
            }
            _ => Some(bs),
        };
        let rect_value = doc.dict_get(&reference(num), b"Rect");
        let is_square = matches!(self.subtype(num).as_str(), "Square" | "Circle");
        let rd = self.rect_diff(num);
        let mut rect = self.rect_of(num);
        self.edit(num, |d| {
            if let Some(bs) = bs_direct {
                d.put(b"BS", bs);
            }
            if let Object::Array(_) = rect_value {
                let adj = (width - old) / 2.0;
                rect.x0 -= adj;
                rect.x1 += adj;
                rect.y0 -= adj;
                rect.y1 += adj;
                d.put(b"Rect", rect_object(rect));
                if is_square {
                    let rd = Rect::new(rd.x0 + adj, rd.y0 + adj, rd.x1 + adj, rd.y1 + adj);
                    d.put(b"RD", rect_object(rd));
                }
            }
            d.remove(b"Border");
        });
        Ok(())
    }

    /// `pdf_set_annot_default_appearance` (/DA), for free text.
    fn set_default_appearance(
        &mut self,
        num: i32,
        font: &str,
        size: f32,
        color: &[f32],
    ) -> Result<(), String> {
        self.check(num, "DA", &["FreeText", "Widget"])?;
        let mut buf = format!("/{font} ").into_bytes();
        crate::print::fmtfloat(size, &mut buf);
        buf.extend_from_slice(b" Tf");
        if [1, 3, 4].contains(&color.len()) {
            for &c in color {
                buf.push(b' ');
                crate::print::fmtfloat(c, &mut buf);
            }
            buf.extend_from_slice(match color.len() {
                4 => b" k",
                3 => b" rg",
                _ => b" g",
            });
        }
        self.edit(num, |d| {
            d.put(b"DA", Object::String(Arc::from(buf)));
            d.remove(b"DS");
            d.remove(b"RC");
        });
        Ok(())
    }

    /// `pdf_set_annot_line` (/L), for lines, from page space.
    pub fn set_line(&mut self, index: i32, num: i32, a: Point, b: Point) -> Result<(), String> {
        self.check(num, "L", &["Line"])?;
        let inv = self.ctm(index)?.invert();
        let a = inv.transform_xy(a.x, a.y);
        let b = inv.transform_xy(b.x, b.y);
        self.edit(num, |d| d.put(b"L", reals(&[a.x, a.y, b.x, b.y])));
        Ok(())
    }

    fn points(&self, index: i32, points: &[Point]) -> Result<Vec<f32>, String> {
        let inv = self.ctm(index)?.invert();
        Ok(points
            .iter()
            .flat_map(|p| {
                let q = inv.transform_xy(p.x, p.y);
                [q.x, q.y]
            })
            .collect())
    }

    /// `setVertices` in MuPDF's JavaScript: `pdf_clear_annot_vertices`,
    /// then `pdf_add_annot_vertex` for each point.
    pub fn set_vertices(&mut self, index: i32, num: i32, v: &[Point]) -> Result<(), String> {
        self.check(num, "Vertices", VERTICES)?;
        let v = self.points(index, v)?;
        self.edit(num, |d| {
            d.remove(b"Vertices");
            if !v.is_empty() {
                d.put(b"Vertices", reals(&v));
            }
        });
        Ok(())
    }

    /// `pdf_set_annot_quad_points`: each quad as upper left, upper right,
    /// lower left, lower right.
    pub fn set_quad_points(
        &mut self,
        index: i32,
        num: i32,
        quads: &[[Point; 4]],
    ) -> Result<(), String> {
        self.check(num, "QuadPoints", QUADS)?;
        if quads.is_empty() {
            return Err("invalid number of quadrilaterals".into());
        }
        let flat: Vec<Point> = quads.iter().flatten().copied().collect();
        let v = self.points(index, &flat)?;
        self.edit(num, |d| d.put(b"QuadPoints", reals(&v)));
        Ok(())
    }

    /// `pdf_set_annot_ink_list` (/InkList), for ink.
    pub fn set_ink_list(&mut self, index: i32, num: i32, ink: &[Vec<Point>]) -> Result<(), String> {
        self.check(num, "InkList", &["Ink"])?;
        let mut strokes = Vec::new();
        for s in ink {
            strokes.push(reals(&self.points(index, s)?));
        }
        self.edit(num, |d| d.put(b"InkList", Object::Array(Arc::new(strokes))));
        Ok(())
    }

    /// `pdf_set_annot_is_open`: the popup's /Open, else a note's own.
    pub fn set_is_open(&mut self, num: i32, open: bool) {
        let popup = self.dict(num).get(b"Popup").cloned();
        match popup {
            Some(p) if !matches!(p, Object::Null) => {
                if let Object::Ref(r) = &p
                    && let Some(mut d) = self.doc.resolve(&p).as_dict().cloned()
                {
                    d.put(b"Open", Object::Bool(open));
                    self.doc.alter_object(r.num, Object::Dict(Arc::new(d)));
                } else if let Object::Dict(pd) = &p {
                    let mut pd = (**pd).clone();
                    pd.put(b"Open", Object::Bool(open));
                    self.edit(num, |d| d.put(b"Popup", Object::Dict(Arc::new(pd))));
                }
                self.mark_dirty(num);
            }
            _ if self.subtype(num) == "Text" => {
                self.edit(num, |d| d.put(b"Open", Object::Bool(open)));
            }
            _ => {}
        }
    }

    fn mark_dirty(&mut self, num: i32) {
        if !self.dirty.contains(&num) {
            self.dirty.push(num);
        }
    }

    /// `pdf_set_annot_popup`: the popup's rectangle, in page space; the
    /// popup is made (at the end of the page's /Annots) if there is none.
    pub fn set_popup(&mut self, index: i32, num: i32, rect: Rect) -> Result<(), String> {
        self.check(num, "Popup", POPUP)?;
        let rect = rect.transform(self.ctm(index)?.invert());
        let doc = self.doc;
        let existing = self
            .dict(num)
            .get(b"Popup")
            .cloned()
            .filter(|v| !matches!(v, Object::Null));
        let popup = match existing {
            Some(p) => Some(p),
            None => self.add_popup(index, num)?,
        };
        match &popup {
            Some(Object::Ref(r)) => {
                if let Some(mut d) = doc.resolve(&reference(r.num)).as_dict().cloned() {
                    d.put(b"Rect", rect_object(rect));
                    doc.alter_object(r.num, Object::Dict(Arc::new(d)));
                }
            }
            Some(Object::Dict(pd)) => {
                let mut pd = (**pd).clone();
                pd.put(b"Rect", rect_object(rect));
                self.edit(num, |d| d.put(b"Popup", Object::Dict(Arc::new(pd))));
            }
            _ => {}
        }
        self.mark_dirty(num);
        Ok(())
    }

    /// `pdf_add_popup_annot`: a new popup object at the end of the page's
    /// /Annots (none without /Annots), and the annotation's /Popup.
    fn add_popup(&mut self, index: i32, num: i32) -> Result<Option<Object>, String> {
        let doc = self.doc;
        let page = self.page(index)?;
        let annots = doc
            .resolve(&page)
            .as_dict()
            .and_then(|d| d.get(b"Annots").cloned());
        let Some(annots) = annots.filter(|v| !matches!(v, Object::Null)) else {
            return Ok(None);
        };
        let mut popup = Dict::new();
        popup.put(b"Type", Object::name(b"Annot"));
        popup.put(b"Subtype", Object::name(b"Popup"));
        popup.put(b"Parent", reference(num));
        popup.put(b"Rect", reals(&[0.0; 4]));
        let p = doc.add_object(Object::Dict(Arc::new(popup)));
        push_annot(doc, index, &annots, reference(p))?;
        self.edit(num, |d| d.put(b"Popup", reference(p)));
        Ok(Some(reference(p)))
    }

    /// `pdf_delete_annot`: `annot` and its popup out of the page's /Annots.
    /// The objects stay, for garbage collection to take. Whether `annot`
    /// was found and removed.
    pub fn delete(&mut self, index: i32, annot: &Object) -> Result<bool, String> {
        self.load_page(index)?;
        let doc = self.doc;
        // Loading the page made an appearance for a direct annotation that
        // has none in MuPDF's local xref, and the annotation MuPDF holds is
        // that copy: it is no longer the one in /Annots, and stays there.
        if !matches!(annot, Object::Ref(_)) && appearance::appearance(doc, annot).is_none() {
            return Ok(false);
        }
        let page = self.page(index)?;
        let value = doc
            .resolve(&page)
            .as_dict()
            .and_then(|d| d.get(b"Annots").cloned())
            .unwrap_or(Object::Null);
        let mut list = match doc.resolve(&value) {
            Object::Array(a) => (*a).clone(),
            _ => return Ok(false),
        };
        let found = list.iter().position(|v| v == annot);
        if let Some(i) = found {
            list.remove(i);
        }
        let popup = doc
            .resolve(annot)
            .as_dict()
            .and_then(|d| d.get(b"Popup").cloned())
            .filter(|v| !matches!(v, Object::Null));
        if let Some(popup) = popup
            && let Some(i) = list.iter().position(|v| *v == popup)
        {
            list.remove(i);
        }
        let new = Object::Array(Arc::new(list));
        match &value {
            Object::Ref(r) => doc.alter_object(r.num, new),
            _ => crate::edit::edit_page(doc, index, &mut |d| d.put(b"Annots", new.clone()))?,
        }
        if let Object::Ref(r) = annot {
            self.dirty.retain(|&n| n != r.num);
        }
        Ok(found.is_some())
    }

    /// `pdf_update_page` after the edits: on each loaded page, the newest
    /// first, the appearance of each annotation that changed made again
    /// and written into the document. Those that could not be made.
    pub fn update(&mut self) -> Vec<Unmade> {
        let doc = self.doc;
        let mut unmade = Vec::new();
        // A direct page is one object in MuPDF, shared by the /P of the
        // annotations made on it: later changes to the page show there.
        for &(index, num) in &self.created {
            if let Ok(page @ Object::Dict(_)) = doc.page(index) {
                let mut d = self.dict(num);
                if let Some(Object::Dict(_)) = d.get(b"P") {
                    d.put(b"P", page);
                    doc.alter_object(num, Object::Dict(Arc::new(d)));
                }
            }
        }
        let loaded = self.loaded.clone();
        for &index in loaded.iter().rev() {
            let Ok(page) = doc.page(index) else { continue };
            let ctm = doc
                .page_transform(&page)
                .map(|t| t.1)
                .unwrap_or(Matrix::IDENTITY);
            let (annots, widgets) = crate::annot::page_annots(doc, &page);
            for annot in annots.iter().chain(&widgets) {
                let Object::Ref(r) = annot else { continue };
                if !self.dirty.contains(&r.num) {
                    continue;
                }
                self.dirty.retain(|&n| n != r.num);
                if matches!(self.subtype(r.num).as_str(), "Popup" | "Link") {
                    continue;
                }
                match appearance::synthesize(doc, annot, ctm) {
                    Ok(made) => self.commit.write_back(doc, annot, &made),
                    Err(e) => {
                        doc.diagnostics().warn(
                            "annot.appearance",
                            format!("cannot create appearance stream: {e}"),
                        );
                        unmade.push((index, r.num, e));
                    }
                }
            }
        }
        unmade
    }
}

/// `pdf_array_push` onto the page's /Annots: the array as stored, its own
/// object or in the page.
fn push_annot(doc: &Document, index: i32, annots: &Object, item: Object) -> Result<(), String> {
    match annots {
        Object::Ref(r) => {
            let mut list = match doc.resolve(annots) {
                Object::Array(a) => (*a).clone(),
                _ => return Ok(()),
            };
            list.push(item);
            doc.alter_object(r.num, Object::Array(Arc::new(list)));
            Ok(())
        }
        Object::Array(a) => {
            let mut list = (**a).clone();
            list.push(item);
            let new = Object::Array(Arc::new(list));
            crate::edit::edit_page(doc, index, &mut |d| d.put(b"Annots", new.clone()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    //! The objects expected are those MuPDF 1.28.6 writes for the same
    //! edits through its JavaScript (oracle/js/clean.js, "annots").

    use super::*;

    /// One 612 by 792 page, object 3, with its next free number 6.
    const MINIMAL: &[u8] = b"%PDF-1.7\n\
        1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
        2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
        3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R>>endobj\n\
        4 0 obj<</Length 0>>stream\n\nendstream endobj\n\
        5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\n\
        trailer<</Root 1 0 R/Size 6>>\n";

    fn open(bytes: &[u8]) -> Document {
        Document::open(bytes.to_vec()).unwrap()
    }

    fn text(doc: &Document, num: i32) -> String {
        let mut out = Vec::new();
        crate::print::print(
            &doc.resolve(&reference(num)),
            crate::print::Options::default(),
            &mut out,
        );
        String::from_utf8_lossy(&out).into_owned()
    }

    fn created(subtype: &str) -> (Document, i32) {
        let doc = open(MINIMAL);
        let mut ed = Editor::new(&doc);
        let num = ed.create(0, subtype).unwrap();
        ed.update();
        (doc, num)
    }

    #[test]
    fn a_square_gets_its_defaults_and_the_border_grows_its_box() {
        let (doc, num) = created("Square");
        assert_eq!(num, 6);
        assert_eq!(
            text(&doc, 6),
            "<</Type/Annot/Subtype/Square/Rect[11.5 729.5 112.5 780.5]/RD[1 1 1 1]\
             /BS<</Type/Border/W 1>>/C[1 0 0]/P 3 0 R/F 4/AP<</N 7 0 R>>>>"
        );
        assert_eq!(text(&doc, 3).matches("/Annots[6 0 R]").count(), 1);
    }

    #[test]
    fn a_note_gets_a_popup_after_it() {
        let (doc, num) = created("Text");
        assert_eq!(
            text(&doc, num),
            "<</Type/Annot/Subtype/Text/Rect[12 764 28 780]/C[1 1 0]/Popup 7 0 R\
             /P 3 0 R/F 28/AP<</N 8 0 R>>>>"
        );
        assert_eq!(
            text(&doc, 7),
            "<</Type/Annot/Subtype/Popup/Parent 6 0 R/Rect[32 680 232 780]>>"
        );
        assert!(text(&doc, 3).contains("/Annots[6 0 R 7 0 R]"));
    }

    #[test]
    fn free_text_gets_a_callout_line_and_its_font_as_an_object() {
        let (doc, num) = created("FreeText");
        assert_eq!(
            text(&doc, num),
            "<</Type/Annot/Subtype/FreeText/Rect[12 680 212 780]/CL[0 792 21.677414 780]\
             /BS<</Type/Border/W 0>>/DA(/Helv 12 Tf 0 0 0 rg)/P 3 0 R/F 4/RD[0 0 0 0]\
             /AP<</N 8 0 R>>>>"
        );
        assert_eq!(
            text(&doc, 7),
            "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>"
        );
    }

    #[test]
    fn a_line_has_no_box_until_its_appearance_is_made() {
        let (doc, num) = created("Line");
        assert_eq!(
            text(&doc, num),
            "<</Type/Annot/Subtype/Line/L[12 780 112 730]/BS<</Type/Border/W 1>>/C[1 0 0]\
             /P 3 0 R/F 4/Rect[11 729 113 781]/AP<</N 7 0 R>>>>"
        );
    }

    #[test]
    fn ink_points_go_through_the_inverse_page_transform() {
        let doc = open(MINIMAL);
        let mut ed = Editor::new(&doc);
        let num = ed.create(0, "Ink").unwrap();
        let stroke = vec![Point::new(10.0, 10.0), Point::new(20.0, 30.0)];
        ed.set_ink_list(0, num, &[stroke]).unwrap();
        ed.update();
        assert_eq!(
            text(&doc, num),
            "<</Type/Annot/Subtype/Ink/BS<</Type/Border/W 1>>/RD[7 7 7 7]/C[1 0 0]/P 3 0 R\
             /F 4/InkList[[10 782 20 762]]/Rect[3 755 27 789]/AP<</N 7 0 R>>>>"
        );
    }

    #[test]
    fn setters_refuse_types_without_the_property() {
        let doc = open(MINIMAL);
        let mut ed = Editor::new(&doc);
        let num = ed.create(0, "Highlight").unwrap();
        assert_eq!(
            ed.set_rect(0, num, Rect::new(0.0, 0.0, 1.0, 1.0)),
            Err("Highlight annotations have no Rect property".into())
        );
        assert_eq!(
            ed.set_interior_color(num, &[1.0]),
            Err("Highlight annotations have no IC property".into())
        );
        assert_eq!(
            ed.set_color(num, &[1.0, 0.0]),
            Err("color must be 0, 1, 3 or 4 components".into())
        );
        assert_eq!(
            ed.create(0, "Bogus"),
            Err("cannot create unknown annotation".into())
        );
    }

    #[test]
    fn deleting_takes_the_popup_out_too_and_leaves_the_objects() {
        let doc = open(MINIMAL);
        let mut ed = Editor::new(&doc);
        let note = ed.create(0, "Text").unwrap();
        let square = ed.create(0, "Square").unwrap();
        assert_eq!(ed.delete(0, &reference(note)), Ok(true));
        ed.update();
        assert!(text(&doc, 3).contains(&format!("/Annots[{square} 0 R]")));
        // The deleted note was never updated: no appearance.
        assert!(!text(&doc, note).contains("/AP"));
    }

    #[test]
    fn a_direct_annotation_goes_only_when_it_has_an_appearance() {
        let bytes = b"%PDF-1.7\n\
            1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
            3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Annots[\
            <</Type/Annot/Subtype/Square/Rect[10 10 50 50]/AP<</N 4 0 R>>>>\
            <</Type/Annot/Subtype/Square/Rect[60 10 90 50]>>]>>endobj\n\
            4 0 obj<</Type/XObject/Subtype/Form/BBox[0 0 40 40]/Length 0>>stream\n\nendstream endobj\n\
            trailer<</Root 1 0 R/Size 5>>\n";
        let doc = open(bytes);
        let page = doc.page(0).unwrap();
        let list = crate::annot::page_annots(&doc, &page).0;
        let mut ed = Editor::new(&doc);
        // MuPDF made the second one's appearance on loading the page, in a
        // copy it then fails to find.
        assert_eq!(ed.delete(0, &list[1]), Ok(false));
        assert_eq!(ed.delete(0, &list[0]), Ok(true));
        assert_eq!(
            text(&doc, 3),
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Annots[\
             <</Type/Annot/Subtype/Square/Rect[60 10 90 50]>>]>>"
        );
    }

    #[test]
    fn an_annots_array_of_its_own_is_copied_into_the_page() {
        let bytes = b"%PDF-1.7\n\
            1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
            3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]/Annots 4 0 R>>endobj\n\
            4 0 obj[]endobj\n\
            trailer<</Root 1 0 R/Size 5>>\n";
        let doc = open(bytes);
        let mut ed = Editor::new(&doc);
        let num = ed.create(0, "Caret").unwrap();
        assert!(text(&doc, 3).contains(&format!("/Annots[{num} 0 R]")));
        assert_eq!(text(&doc, 4), "[]");
    }
}
