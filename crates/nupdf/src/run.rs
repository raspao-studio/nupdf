// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-op-run.c, source/pdf/pdf-run.c
// (pdf_run_page_contents, pdf_structure_type), source/pdf/pdf-page.c
// (the blending scan, pdf_load_default_colorspaces), source/pdf/pdf-pattern.c,
// source/pdf/pdf-xobject.c, source/pdf/pdf-interpret.c (the SMask entry of
// pdf_process_extgstate) and source/pdf/pdf-nametree.c (pdf_lookup_number),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! The run processor: content-stream operators to device calls, as
//! `pdf_new_run_processor` maps them, and a page run as
//! `pdf_run_page_contents` does it.
//!
//! The graphics state stack, clipping, soft masks, transparency groups,
//! tiling patterns, shadings, images, form XObjects, optional content
//! layers, marked-content structure and metatext follow MuPDF. Text is not
//! shown yet: the text operators change no state a device sees, so text
//! and what MuPDF draws around it (its groups, masks and clips) are
//! missing from the device calls.

use std::sync::Arc;

use crate::colorspace::{self, ColorSpace, MAX_COLORS};
use crate::content::{self, Op, ProcState, Processor, Store};
use crate::device::{self, ColorParams, DefaultColorSpaces, Device, Metatext};
use crate::diag::report;
use crate::document::Document;
use crate::function::{self, Function};
use crate::geometry::{Matrix, Rect};
use crate::image::{self, Image};
use crate::object::Object;
use crate::path::{Path, StrokeState};
use crate::shade::Shade;

/// `pdf_gsave`'s limit: the stack grows by doubling and stops short of
/// 4096 entries.
const MAX_GSTATES: usize = 2048;
/// `nest_mark`'s size.
const MAX_NEST: usize = 1024;
/// Pattern cells drawn for one fill before giving up; MuPDF draws them
/// all, however many.
const MAX_TILES: usize = 1 << 20;
/// Resource and XObject chains deeper than this are treated as cycles by
/// the blending scan.
const MAX_SCAN_DEPTH: usize = 256;

// The device flags the processor reads (FZ_DEVFLAG_*).
const DEVFLAG_MASK: u32 = 1;
const DEVFLAG_COLOR: u32 = 2;
const DEVFLAG_BBOX_DEFINED: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Color,
    Pattern,
    Shade,
}

/// `pdf_material`.
#[derive(Debug, Clone)]
struct Material {
    kind: Kind,
    colorspace: Arc<ColorSpace>,
    pattern: Option<Arc<Pattern>>,
    shade: Option<Arc<Shade>>,
    /// The gstate whose ctm patterns and shadings are drawn with (-1 for
    /// none).
    gstate_num: i32,
    params: ColorParams,
    alpha: f32,
    v: [f32; MAX_COLORS],
}

impl Material {
    fn new() -> Material {
        Material {
            kind: Kind::Color,
            colorspace: Arc::new(ColorSpace::Gray),
            pattern: None,
            shade: None,
            gstate_num: -1,
            params: ColorParams::default(),
            alpha: 1.0,
            v: [0.0; MAX_COLORS],
        }
    }

    fn color(&self) -> &[f32] {
        &self.v[..self.colorspace.n().min(MAX_COLORS)]
    }
}

/// The font a gstate's text uses, as far as text without glyphs needs to
/// know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Font {
    None,
    Simple,
    /// Type 3 glyphs are never clips.
    Type3,
}

/// `pdf_gstate`, with only the part of the text state that decides
/// whether text is shown and clips.
#[derive(Debug, Clone)]
struct GState {
    ctm: Matrix,
    clip_depth: i32,
    stroke_state: StrokeState,
    stroke: Material,
    fill: Material,
    /// Drawing an uncolored pattern's cell: colors do not change.
    ismask: bool,
    blendmode: u8,
    softmask: Option<Object>,
    softmask_resources: Option<Object>,
    softmask_tr: Option<Object>,
    softmask_ctm: Matrix,
    softmask_cs: Option<ColorSpace>,
    softmask_bc: [f32; MAX_COLORS],
    luminosity: bool,
    font: Font,
    render: i32,
}

impl GState {
    /// `pdf_init_gstate`.
    fn new(ctm: Matrix) -> GState {
        GState {
            ctm,
            clip_depth: 0,
            stroke_state: StrokeState::default(),
            stroke: Material::new(),
            fill: Material::new(),
            ismask: false,
            blendmode: 0,
            softmask: None,
            softmask_resources: None,
            softmask_tr: None,
            softmask_ctm: Matrix::IDENTITY,
            softmask_cs: None,
            softmask_bc: [0.0; MAX_COLORS],
            luminosity: false,
            font: Font::None,
            render: 0,
        }
    }

    fn material(&mut self, stroke: bool) -> &mut Material {
        if stroke {
            &mut self.stroke
        } else {
            &mut self.fill
        }
    }
}

/// `pdf_pattern`: a tiling pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    ismask: bool,
    xstep: f32,
    ystep: f32,
    bbox: Rect,
    matrix: Matrix,
    resources: Option<Object>,
    contents: Object,
    id: i32,
    uses_blending: bool,
}

/// `pdf_load_pattern`.
fn load_pattern(doc: &Document, obj: &Object) -> Pattern {
    let get = |key: &[u8]| doc.dict_get(obj, key);
    let diag = doc.diagnostics();
    let mut xstep = get(b"XStep").to_real();
    let mut ystep = get(b"YStep").to_real();
    if xstep.abs() <= f32::EPSILON {
        diag.warn(
            "pattern.zero_step",
            "tiling pattern must have non-zero xstep",
        );
        xstep = 1.0;
    }
    if ystep.abs() <= f32::EPSILON {
        diag.warn(
            "pattern.zero_step",
            "tiling pattern must have non-zero ystep",
        );
        ystep = 1.0;
    }
    Pattern {
        ismask: get(b"PaintType").to_int() == 2,
        xstep,
        ystep,
        bbox: doc.to_rect(Some(&get(b"BBox"))),
        matrix: to_matrix(doc, &get(b"Matrix")),
        resources: stored(doc, obj, b"Resources"),
        contents: obj.clone(),
        id: obj.as_ref().map_or(0, |r| r.num),
        uses_blending: pattern_uses_blending(doc, obj, &mut Vec::new()),
    }
}

/// `pdf_to_matrix`.
fn to_matrix(doc: &Document, v: &Object) -> Matrix {
    let Object::Array(a) = doc.resolve(v) else {
        return Matrix::IDENTITY;
    };
    let n = |i: usize| a.get(i).map_or(0.0, |v| doc.resolve(v).to_real());
    Matrix {
        a: n(0),
        b: n(1),
        c: n(2),
        d: n(3),
        e: n(4),
        f: n(5),
    }
}

/// `pdf_dict_get`: the value as stored (possibly a reference), None for a
/// missing or null entry.
fn stored(doc: &Document, dict: &Object, key: &[u8]) -> Option<Object> {
    match doc.resolve(dict).as_dict()?.get(key)? {
        Object::Null => None,
        v => Some(v.clone()),
    }
}

/// `pdf_cycle`: direct objects never repeat.
fn cycles(stack: &[i32], obj: &Object) -> bool {
    let num = obj.as_ref().map_or(0, |r| r.num);
    num > 0 && stack.contains(&num)
}

fn num(obj: &Object) -> i32 {
    obj.as_ref().map_or(0, |r| r.num)
}

// The blending scan (pdf-page.c). MuPDF memoizes the answer for each
// resource dictionary; it is computed afresh here.

fn extgstate_uses_blending(doc: &Document, dict: &Object) -> bool {
    match stored(doc, dict, b"BM") {
        Some(bm) => !doc.resolve(&bm).is_name(b"Normal"),
        None => false,
    }
}

/// `pdf_pattern_uses_blending`.
pub fn pattern_uses_blending(doc: &Document, dict: &Object, cycle: &mut Vec<i32>) -> bool {
    if cycles(cycle, dict) || cycle.len() >= MAX_SCAN_DEPTH {
        return false;
    }
    cycle.push(num(dict));
    let res = stored(doc, dict, b"Resources");
    let out = res.is_some_and(|r| resources_use_blending(doc, &r, cycle))
        || stored(doc, dict, b"ExtGState").is_some_and(|g| extgstate_uses_blending(doc, &g));
    cycle.pop();
    out
}

fn xobject_uses_blending(doc: &Document, dict: &Object, cycle: &mut Vec<i32>) -> bool {
    if cycles(cycle, dict) || cycle.len() >= MAX_SCAN_DEPTH {
        return false;
    }
    cycle.push(num(dict));
    let group = doc.dict_get(dict, b"Group");
    let out = doc.dict_get(&group, b"S").is_name(b"Transparency")
        || (doc.dict_get(dict, b"Subtype").is_name(b"Image")
            && stored(doc, dict, b"SMask").is_some())
        || stored(doc, dict, b"Resources").is_some_and(|r| resources_use_blending(doc, &r, cycle));
    cycle.pop();
    out
}

/// `pdf_resources_use_blending`.
pub fn resources_use_blending(doc: &Document, rdb: &Object, cycle: &mut Vec<i32>) -> bool {
    if cycles(cycle, rdb) || cycle.len() >= MAX_SCAN_DEPTH {
        return false;
    }
    cycle.push(num(rdb));
    let values = |key: &[u8]| -> Vec<Object> {
        match doc.dict_get(rdb, key) {
            Object::Dict(d) => d.iter().map(|(_, v)| v.clone()).collect(),
            _ => Vec::new(),
        }
    };
    let out = values(b"ExtGState")
        .iter()
        .any(|g| extgstate_uses_blending(doc, g))
        || values(b"Pattern")
            .iter()
            .any(|p| pattern_uses_blending(doc, p, cycle))
        || values(b"XObject")
            .iter()
            .any(|x| xobject_uses_blending(doc, x, cycle));
    cycle.pop();
    out
}

/// `pdf_lookup_number`.
pub fn lookup_number(doc: &Document, node: &Object, needle: i32) -> Option<Object> {
    lookup_number_imp(doc, node, needle, &mut Vec::new())
}

fn lookup_number_imp(
    doc: &Document,
    node: &Object,
    needle: i32,
    cycle: &mut Vec<i32>,
) -> Option<Object> {
    if let Object::Array(kids) = doc.dict_get(node, b"Kids") {
        let (mut l, mut r) = (0i64, kids.len() as i64 - 1);
        while l <= r {
            let m = (l + r) >> 1;
            let kid = kids.get(m as usize).cloned().unwrap_or(Object::Null);
            let limits = doc.dict_get(&kid, b"Limits");
            let bound = |i: usize| match doc.resolve(&limits) {
                Object::Array(a) => a.get(i).map_or(0, |v| doc.resolve(v).to_int()),
                _ => 0,
            };
            if needle < bound(0) {
                r = m - 1;
            } else if needle > bound(1) {
                l = m + 1;
            } else {
                if cycles(cycle, node) || cycle.len() >= MAX_SCAN_DEPTH {
                    break;
                }
                cycle.push(num(node));
                let out = lookup_number_imp(doc, &kid, needle, cycle);
                cycle.pop();
                return out;
            }
        }
    }
    if let Object::Array(nums) = doc.dict_get(node, b"Nums") {
        let key = |i: usize| nums.get(i * 2).map_or(0, |v| doc.resolve(v).to_int());
        let val = |i: usize| nums.get(i * 2 + 1).cloned();
        let (mut l, mut r) = (0i64, (nums.len() / 2) as i64 - 1);
        while l <= r {
            let m = (l + r) >> 1;
            let k = key(m as usize);
            if needle < k {
                r = m - 1;
            } else if needle > k {
                l = m + 1;
            } else {
                return val(m as usize);
            }
        }
        // Lists that are not sorted are searched too.
        for i in 0..nums.len() / 2 {
            if key(i) == needle {
                return val(i);
            }
        }
    }
    None
}

/// `pdf_structure_type`'s names, and `fz_structure_to_string`'s for them
/// (NonStruct is misspelled there).
const STRUCTURE: &[(&[u8], &str)] = &[
    (b"Document", "Document"),
    (b"Part", "Part"),
    (b"Art", "Art"),
    (b"Sect", "Sect"),
    (b"Div", "Div"),
    (b"BlockQuote", "BlockQuote"),
    (b"Caption", "Caption"),
    (b"TOC", "TOC"),
    (b"TOCI", "TOCI"),
    (b"Index", "Index"),
    (b"NonStruct", "NonDtruct"),
    (b"Private", "Private"),
    (b"DocumentFragment", "DocumentFragment"),
    (b"Aside", "Aside"),
    (b"Title", "Title"),
    (b"FENote", "FENote"),
    (b"Sub", "Sub"),
    (b"P", "P"),
    (b"H", "H"),
    (b"H1", "H1"),
    (b"H2", "H2"),
    (b"H3", "H3"),
    (b"H4", "H4"),
    (b"H5", "H5"),
    (b"H6", "H6"),
    (b"L", "L"),
    (b"LI", "LI"),
    (b"Lbl", "Lbl"),
    (b"LBody", "LBody"),
    (b"Table", "Table"),
    (b"TR", "TR"),
    (b"TH", "TH"),
    (b"TD", "TD"),
    (b"THead", "THead"),
    (b"TBody", "TBody"),
    (b"TFoot", "TFoot"),
    (b"Span", "Span"),
    (b"Quote", "Quote"),
    (b"Note", "Note"),
    (b"Reference", "Reference"),
    (b"BibEntry", "BibEntry"),
    (b"Code", "Code"),
    (b"Link", "Link"),
    (b"Annot", "Annot"),
    (b"Em", "Em"),
    (b"Strong", "Strong"),
    (b"Ruby", "Ruby"),
    (b"RB", "RB"),
    (b"RT", "RT"),
    (b"RP", "RP"),
    (b"Warichu", "Warichu"),
    (b"WT", "WT"),
    (b"WP", "WP"),
    (b"Figure", "Figure"),
    (b"Formula", "Formula"),
    (b"Form", "Form"),
    (b"Artifact", "Artifact"),
];

/// `pdf_structure_type`: None for FZ_STRUCTURE_INVALID.
fn structure_type(
    doc: &Document,
    role_map: &Option<Object>,
    tag: &Option<Object>,
) -> Option<&'static str> {
    let mut tag = tag.as_ref().map(|t| doc.resolve(t));
    if let (Some(map), Some(Object::Name(t))) = (role_map, &tag)
        && let Some(o) = stored(doc, map, t)
    {
        tag = Some(doc.resolve(&o));
    }
    let Some(Object::Name(name)) = tag else {
        return None;
    };
    STRUCTURE
        .iter()
        .find(|(n, _)| *n == name.as_ref())
        .map(|(_, s)| *s)
}

/// `nest_mark` entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nest {
    /// Finished: removed when exposed.
    None,
    LiveClip,
    LiveLayer,
    /// Closed, waiting for its EMC.
    DeadLayer,
    /// Past its EMC but not closed: closed when exposed.
    ZombieLayer,
}

/// `marked_content_stack`.
#[derive(Debug, Clone)]
struct Marked {
    tag: Vec<u8>,
    val: Option<Object>,
    structure_pushed: bool,
}

/// What `begin_softmask` saves.
#[derive(Debug, Default)]
struct SoftmaskSave {
    softmask: Option<Object>,
    softmask_cs: Option<ColorSpace>,
    page_resources: Option<Object>,
    ctm: Option<Matrix>,
}

/// `pdf_run_processor`.
#[allow(missing_debug_implementations)]
pub struct Run<'a> {
    doc: &'a Document,
    dev: &'a mut dyn Device,
    state: ProcState,
    usage: &'static str,
    flags: u32,
    default_cs: Arc<DefaultColorSpaces>,
    path: Path,
    clip: bool,
    clip_even_odd: bool,
    gstate: Vec<GState>,
    gbot: usize,
    gparent: usize,
    /// XObjects being run, by number.
    cycle: Vec<i32>,
    role_map: Option<Object>,
    marked: Vec<Marked>,
    mcid_sent: Option<Object>,
    pending_mcid_pop: Option<Object>,
    struct_parent: i32,
    mcids: Option<Object>,
    broken_struct_tree: bool,
    struct_tree_root: Option<Object>,
    begin_layers: Vec<Vec<u8>>,
    nest: Vec<Nest>,
    process_layers: bool,
    process_structure: bool,
    /// A content stream failed: MuPDF drops its close callback, so the
    /// end of the run unwinds nothing.
    aborted: bool,
    tiles: usize,
    /// Text shown since the last flush (`tos.text`), in this render mode.
    text_pending: bool,
    text_mode: i32,
    /// Glyphs shown in a clipping mode since the last `ET`
    /// (`tos.clip_text`).
    clip_pending: bool,
}

impl<'a> Run<'a> {
    /// `pdf_new_run_processor` with no initial gstates given.
    pub fn new(
        doc: &'a Document,
        dev: &'a mut dyn Device,
        ctm: Matrix,
        struct_parent: i32,
        default_cs: Arc<DefaultColorSpaces>,
        store: Store,
    ) -> Result<Run<'a>, String> {
        let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
        let struct_tree_root = stored(doc, &root, b"StructTreeRoot");
        let role_map = struct_tree_root
            .as_ref()
            .and_then(|s| stored(doc, s, b"RoleMap"));
        let mut run = Run {
            doc,
            dev,
            state: ProcState::with_store(store),
            usage: "View",
            flags: 0,
            default_cs,
            path: Path::new(),
            clip: false,
            clip_even_odd: false,
            gstate: vec![GState::new(ctm)],
            gbot: 0,
            gparent: 0,
            cycle: Vec::new(),
            role_map,
            marked: Vec::new(),
            mcid_sent: None,
            pending_mcid_pop: None,
            struct_parent: -1,
            mcids: None,
            broken_struct_tree: false,
            struct_tree_root,
            begin_layers: Vec::new(),
            nest: Vec::new(),
            process_layers: true,
            process_structure: true,
            aborted: false,
            tiles: 0,
            text_pending: false,
            text_mode: 0,
            clip_pending: false,
        };
        // An extra level for the parent gstate.
        run.gsave()?;
        run.set_struct_parent(struct_parent);
        if run.process_structure
            && struct_parent != -1
            && let Some(root) = run.struct_tree_root.clone()
        {
            let tree = doc.dict_get(&root, b"ParentTree");
            if let Some(obj) = lookup_number(doc, &tree, struct_parent)
                && doc.resolve(&obj).as_dict().is_some()
            {
                run.send_begin_structure(&obj)?;
            }
        }
        Ok(run)
    }

    /// The store, for the next page.
    pub fn into_store(self) -> Store {
        self.state.store
    }

    fn gs(&self) -> &GState {
        // The stack is never empty.
        &self.gstate[self.gstate.len() - 1]
    }

    fn gs_mut(&mut self) -> &mut GState {
        let top = self.gstate.len() - 1;
        &mut self.gstate[top]
    }

    fn gtop(&self) -> usize {
        self.gstate.len() - 1
    }

    fn warn(&self, code: &'static str, message: impl Into<String>) {
        self.doc.diagnostics().warn(code, message);
    }

    // Nesting of clips and layers.

    fn trim_nest(&mut self) {
        while let Some(&mark) = self.nest.last() {
            match mark {
                Nest::None => {
                    self.nest.pop();
                }
                Nest::ZombieLayer => {
                    self.dev.end_layer();
                    self.nest.pop();
                }
                _ => break,
            }
        }
    }

    fn push_nest(&mut self, mark: Nest) -> Result<(), String> {
        if let Some(&top) = self.nest.last()
            && top != Nest::LiveClip
            && top != Nest::LiveLayer
        {
            self.warn(
                "run.bad_nesting",
                "invalid marked content sequence / clip nesting",
            );
        }
        if self.nest.len() == MAX_NEST {
            return Err("limit error: layer/clip nesting too deep".into());
        }
        self.nest.push(mark);
        Ok(())
    }

    fn pop_nest_until_empty(&mut self) {
        while let Some(mark) = self.nest.pop() {
            match mark {
                Nest::LiveClip => self.dev.pop_clip(),
                Nest::LiveLayer | Nest::ZombieLayer => self.dev.end_layer(),
                _ => {}
            }
        }
    }

    fn pop_nest_clip(&mut self) {
        for i in (0..self.nest.len()).rev() {
            match self.nest[i] {
                Nest::LiveClip => {
                    self.nest[i] = Nest::None;
                    self.dev.pop_clip();
                    self.trim_nest();
                    return;
                }
                Nest::LiveLayer => {
                    self.nest[i] = Nest::DeadLayer;
                    self.dev.end_layer();
                }
                Nest::ZombieLayer => {
                    self.warn(
                        "run.zombie_layer",
                        "zombie layer inside clip (should never happen)",
                    );
                    self.nest[i] = Nest::DeadLayer;
                    self.dev.end_layer();
                }
                _ => {}
            }
        }
        self.warn("run.too_many_clips", "popped too many clips!");
    }

    fn pop_nest_layer(&mut self) {
        for i in (0..self.nest.len()).rev() {
            match self.nest[i] {
                Nest::LiveLayer => {
                    self.nest[i] = Nest::ZombieLayer;
                    self.trim_nest();
                    return;
                }
                Nest::DeadLayer => {
                    self.nest[i] = Nest::None;
                    self.trim_nest();
                    return;
                }
                _ => {}
            }
        }
        self.warn("run.too_many_layers", "popped too many layers!");
    }

    fn flush_begin_layer(&mut self) -> Result<(), String> {
        if !self.process_layers {
            return Ok(());
        }
        for name in std::mem::take(&mut self.begin_layers) {
            self.push_nest(Nest::LiveLayer)?;
            self.dev.begin_layer(&name);
        }
        Ok(())
    }

    fn do_end_layer(&mut self) {
        if self.process_layers {
            self.pop_nest_layer();
        }
    }

    // Text. No glyphs are drawn, but text shown flushes pending layers and
    // structure where MuPDF flushes it, and text clips are made at `ET`.

    /// `pdf_flush_text`.
    fn flush_text(&mut self) -> Result<(), String> {
        if !std::mem::take(&mut self.text_pending) {
            return Ok(());
        }
        self.pop_any_pending_mcid_changes();
        self.flush_begin_layer()
    }

    /// `pdf_flush_clip_text`.
    fn flush_clip_text(&mut self) -> Result<(), String> {
        if !std::mem::take(&mut self.clip_pending) {
            return Ok(());
        }
        self.pop_any_pending_mcid_changes();
        self.flush_begin_layer()?;
        self.push_nest(Nest::LiveClip)?;
        self.gs_mut().clip_depth += 1;
        let ctm = self.gs().ctm;
        self.dev.clip_text(ctm);
        Ok(())
    }

    /// Characters shown, as `pdf_show_char` handles each: text in a new
    /// render mode flushes what came before.
    fn show_chars(&mut self) -> Result<(), String> {
        let (font, render) = (self.gs().font, self.gs().render);
        if font == Font::None {
            self.warn(
                "run.no_font",
                "cannot draw text since font and size not set",
            );
            return Ok(());
        }
        let mut mode = render;
        if font == Font::Type3 {
            if render >= 4 {
                mode -= 4;
            }
            if mode != 3 {
                mode = 0;
            }
        }
        if !self.text_pending || mode != self.text_mode {
            self.flush_text()?;
        }
        self.text_pending = true;
        self.text_mode = mode;
        if mode & 4 != 0 {
            self.clip_pending = true;
        }
        Ok(())
    }

    fn show_string(&mut self, text: &[u8]) -> Result<(), String> {
        if self.gs().font == Font::None {
            return self.show_chars();
        }
        if text.is_empty() {
            return Ok(());
        }
        self.show_chars()
    }

    fn show_array(&mut self, array: Option<&Object>) -> Result<(), String> {
        let doc = self.doc;
        if self.gs().font == Font::None {
            return self.show_chars();
        }
        let items = match array.map(|a| doc.resolve(a)) {
            Some(Object::Array(items)) => items,
            Some(Object::String(s)) => return self.show_string(&s),
            _ => return Ok(()),
        };
        for item in items.iter() {
            if let Object::String(s) = doc.resolve(item)
                && !s.is_empty()
            {
                self.show_chars()?;
            }
        }
        Ok(())
    }

    // The graphics state stack.

    fn gsave(&mut self) -> Result<(), String> {
        if self.gstate.len() >= MAX_GSTATES {
            return Err("limit error: too many nested graphics states".into());
        }
        let top = self.gs().clone();
        self.gstate.push(top);
        Ok(())
    }

    fn grestore(&mut self) {
        if self.gtop() <= self.gbot {
            self.warn("run.gstate_underflow", "gstate underflow in content stream");
            return;
        }
        let mut clip_depth = self.gs().clip_depth;
        self.gstate.pop();
        while clip_depth > self.gs().clip_depth {
            self.pop_nest_clip();
            clip_depth -= 1;
        }
    }

    // Soft masks and groups.

    fn begin_softmask(&mut self, save: &mut SoftmaskSave, bbox: Rect) -> Result<(), String> {
        let gs = self.gs().clone();
        let Some(softmask) = gs.softmask.clone() else {
            return Ok(());
        };
        let doc = self.doc;
        save.softmask = Some(softmask.clone());
        save.softmask_cs = gs.softmask_cs.clone();
        save.page_resources = gs.softmask_resources.clone();
        save.ctm = Some(gs.softmask_ctm);
        let save_ctm = gs.ctm;
        let mut mask_bbox = doc.to_rect(Some(&doc.dict_get(&softmask, b"BBox")));
        let mask_matrix = to_matrix(doc, &doc.dict_get(&softmask, b"Matrix"));
        let mut mask_cs = gs.softmask_cs.clone();
        if gs.luminosity && mask_cs.is_none() {
            mask_cs = Some(ColorSpace::Gray);
        }
        if gs.luminosity {
            mask_bbox = Rect::INFINITE;
        } else {
            mask_bbox = mask_bbox.transform(mask_matrix).transform(gs.softmask_ctm);
        }
        mask_bbox = mask_bbox.intersect(bbox);
        let saved_blendmode = gs.blendmode;
        {
            let g = self.gs_mut();
            g.softmask = None;
            g.softmask_cs = None;
            g.softmask_resources = None;
            g.ctm = gs.softmask_ctm;
            g.fill.alpha = 1.0;
            g.stroke.alpha = 1.0;
        }
        let mut tr = None;
        if let Some(t) = self.gs_mut().softmask_tr.take() {
            tr = load_transfer(doc, &t)?;
        }
        let n = mask_cs.as_ref().map_or(0, |c| c.n().min(MAX_COLORS));
        let bc = gs.softmask_bc;
        self.dev.begin_mask(
            mask_bbox,
            gs.luminosity,
            mask_cs.as_ref(),
            &bc[..n],
            gs.fill.params,
        );
        self.gs_mut().blendmode = 0;
        self.run_xobject(
            &softmask,
            save.page_resources.clone(),
            Matrix::IDENTITY,
            true,
        )?;
        self.gs_mut().blendmode = saved_blendmode;
        self.dev.end_mask(tr.as_ref());
        let g = self.gs_mut();
        g.ctm = save_ctm;
        g.fill.alpha = gs.fill.alpha;
        g.stroke.alpha = gs.stroke.alpha;
        Ok(())
    }

    fn end_softmask(&mut self, save: &mut SoftmaskSave) {
        let Some(softmask) = save.softmask.take() else {
            return;
        };
        let g = self.gs_mut();
        g.softmask = Some(softmask);
        g.softmask_cs = save.softmask_cs.take();
        g.softmask_resources = save.page_resources.take();
        if let Some(ctm) = save.ctm {
            g.softmask_ctm = ctm;
        }
        self.dev.pop_clip();
    }

    fn begin_group(&mut self, bbox: Rect, save: &mut SoftmaskSave) -> Result<(), String> {
        self.begin_softmask(save, bbox)?;
        let blendmode = self.gs().blendmode;
        if blendmode != 0 {
            self.dev
                .begin_group(bbox, None, false, false, blendmode, 1.0);
        }
        Ok(())
    }

    fn end_group(&mut self, save: &mut SoftmaskSave) {
        if self.gs().blendmode != 0 {
            self.dev.end_group();
        }
        self.end_softmask(save);
    }

    // Drawing.

    fn show_shade(&mut self, shade: &Shade) -> Result<(), String> {
        if self.state.hidden > 0 {
            return Ok(());
        }
        let bbox = shade.bound(self.gs().ctm);
        let mut save = SoftmaskSave::default();
        self.begin_group(bbox, &mut save)?;
        let gs = self.gs();
        let (ctm, alpha, params) = (gs.ctm, gs.fill.alpha, gs.fill.params);
        self.dev.fill_shade(shade, ctm, alpha, params);
        self.end_group(&mut save);
        Ok(())
    }

    fn unset_pattern(&mut self, stroke: bool) {
        let mat = self.gs_mut().material(stroke);
        if mat.kind == Kind::Pattern {
            mat.pattern = None;
            mat.kind = Kind::Color;
        }
    }

    /// `pdf_show_pattern`: `area` is the device-space box to cover.
    fn show_pattern(
        &mut self,
        pat: &Pattern,
        pat_gstate_num: i32,
        area: Rect,
        stroke: bool,
    ) -> Result<(), String> {
        self.gsave()?;
        let pat_index = usize::try_from(pat_gstate_num)
            .ok()
            .filter(|&i| i < self.gstate.len())
            .unwrap_or(0);
        let pat_gs = self.gstate[pat_index].clone();
        {
            // Patterns are run with the gstate of their parent.
            let g = self.gs_mut();
            g.ctm = pat_gs.ctm;
            g.stroke_state = pat_gs.stroke_state.clone();
            g.fill.alpha = pat_gs.fill.alpha;
            g.stroke.alpha = pat_gs.stroke.alpha;
            g.blendmode = pat_gs.blendmode;
            g.softmask = pat_gs.softmask.clone();
            g.softmask_resources = pat_gs.softmask_resources.clone();
            g.softmask_tr = pat_gs.softmask_tr.clone();
            g.softmask_ctm = pat_gs.softmask_ctm;
            g.softmask_cs = pat_gs.softmask_cs.clone();
            g.softmask_bc = pat_gs.softmask_bc;
            g.luminosity = pat_gs.luminosity;
            g.font = pat_gs.font;
            g.render = pat_gs.render;
        }
        // Uncolored patterns are not cached: they are colored when drawn.
        let id = if pat.ismask {
            self.gs_mut().ismask = true;
            self.unset_pattern(false);
            self.unset_pattern(true);
            let g = self.gs_mut();
            if stroke {
                g.fill = g.stroke.clone();
            } else {
                g.stroke = g.fill.clone();
            }
            0
        } else {
            self.unset_pattern(stroke);
            pat.id
        };
        {
            let g = self.gs_mut();
            g.softmask = None;
            g.softmask_resources = None;
            g.softmask_cs = None;
        }
        let ptm = pat.matrix.concat(pat_gs.ctm);
        let invptm = ptm.invert();
        let gparent_save = self.gparent;
        self.gparent = self.gtop() - 1;
        let gparent_save_ctm = self.gstate[self.gparent].ctm;
        self.gstate[self.gparent].ctm = ptm;

        let local = area.transform(invptm);
        let mut fx0 = (local.x0 - pat.bbox.x0) / pat.xstep;
        let mut fy0 = (local.y0 - pat.bbox.y0) / pat.ystep;
        let mut fx1 = (local.x1 - pat.bbox.x0) / pat.xstep;
        let mut fy1 = (local.y1 - pat.bbox.y0) / pat.ystep;
        if fx0 > fx1 {
            std::mem::swap(&mut fx0, &mut fx1);
        }
        if fy0 > fy1 {
            std::mem::swap(&mut fy0, &mut fy1);
        }
        let result = if (fx1 - fx0 > 1.0 || fy1 - fy0 > 1.0) && !pat.uses_blending {
            let cached =
                self.dev
                    .begin_tile(local, pat.bbox, pat.xstep.abs(), pat.ystep.abs(), ptm, id);
            let mut out = Ok(());
            if !cached {
                self.gs_mut().ctm = ptm;
                out = self.run_cell(pat);
            }
            self.dev.end_tile();
            out
        } else {
            let x0 = (fx0 + 0.001).floor() as i32;
            let y0 = (fy0 + 0.001).floor() as i32;
            let mut x1 = (fx1 - 0.001).ceil() as i32;
            let mut y1 = (fy1 - 0.001).ceil() as i32;
            if fx1 > fx0 && x1 == x0 {
                x1 = x0.saturating_add(1);
            }
            if fy1 > fy0 && y1 == y0 {
                y1 = y0.saturating_add(1);
            }
            let mut out = Ok(());
            'cells: for y in y0..y1 {
                for x in x0..x1 {
                    self.tiles += 1;
                    if self.tiles > MAX_TILES {
                        out = Err("limit error: too many pattern cells".into());
                        break 'cells;
                    }
                    self.gs_mut().ctm =
                        ptm.pre_translate(x as f32 * pat.xstep, y as f32 * pat.ystep);
                    out = self.run_cell(pat);
                    if out.is_err() {
                        break 'cells;
                    }
                }
            }
            out
        };
        result?;
        self.gstate[self.gparent].ctm = gparent_save_ctm;
        self.gparent = gparent_save;
        self.grestore();
        Ok(())
    }

    /// One run of a pattern's content, with its own floor on the stack.
    fn run_cell(&mut self, pat: &Pattern) -> Result<(), String> {
        let oldbot = self.gbot;
        self.gbot = self.gtop();
        self.gsave()?;
        self.process(pat.resources.as_ref(), Some(&pat.contents))?;
        self.grestore();
        while self.gtop() > self.gbot {
            self.grestore();
        }
        self.gbot = oldbot;
        Ok(())
    }

    /// `pdf_process_contents`, noting a failure as MuPDF does.
    fn process(
        &mut self,
        resources: Option<&Object>,
        contents: Option<&Object>,
    ) -> Result<(), String> {
        let doc = self.doc;
        let out = content::process_contents(doc, resources, contents, self);
        if out.is_err() {
            self.aborted = true;
        }
        out
    }

    fn show_image_imp(
        &mut self,
        image: &Image,
        image_ctm: Matrix,
        bbox: Rect,
    ) -> Result<(), String> {
        if !bbox.is_valid() {
            return Ok(());
        }
        let gs = self.gs().clone();
        let mut params = gs.fill.params;
        if let Some(intent) = image.intent {
            params.ri = intent as u8;
        }
        if image.colorspace.is_some() {
            self.dev.fill_image(image, image_ctm, gs.fill.alpha, params);
        } else if gs.fill.kind == Kind::Color {
            self.dev.fill_image_mask(
                image,
                image_ctm,
                &gs.fill.colorspace,
                gs.fill.color(),
                gs.fill.alpha,
                params,
            );
        } else if gs.fill.kind == Kind::Pattern
            && let Some(pat) = &gs.fill.pattern
        {
            self.dev.clip_image_mask(image, image_ctm, bbox);
            self.show_pattern(pat, gs.fill.gstate_num, bbox, false)?;
            self.dev.pop_clip();
        } else if gs.fill.kind == Kind::Shade
            && let Some(shade) = &gs.fill.shade
        {
            self.dev.clip_image_mask(image, image_ctm, bbox);
            let ctm = self.gstate_ctm(gs.fill.gstate_num);
            self.dev.fill_shade(shade, ctm, gs.fill.alpha, params);
            self.dev.pop_clip();
        }
        Ok(())
    }

    fn gstate_ctm(&self, num: i32) -> Matrix {
        usize::try_from(num)
            .ok()
            .and_then(|i| self.gstate.get(i))
            .map_or(Matrix::IDENTITY, |g| g.ctm)
    }

    fn show_image(&mut self, image: &Image) -> Result<(), String> {
        if self.state.hidden > 0 {
            return Ok(());
        }
        self.pop_any_pending_mcid_changes();
        self.flush_begin_layer()?;
        // Images are bottom-up: flip them.
        let image_ctm = self.gs().ctm.pre_translate(0.0, 1.0).pre_scale(1.0, -1.0);
        let bbox = Rect::UNIT.transform(image_ctm);
        let blendmode = self.gs().blendmode;
        if let Some(mask) = &image.mask {
            if blendmode != 0 {
                self.dev
                    .begin_group(bbox, None, false, false, blendmode, 1.0);
            }
            self.dev.clip_image_mask(mask, image_ctm, bbox);
            self.show_image_imp(image, image_ctm, bbox)?;
            self.dev.pop_clip();
            if blendmode != 0 {
                self.dev.end_group();
            }
        } else {
            let mut save = SoftmaskSave::default();
            self.begin_group(bbox, &mut save)?;
            self.show_image_imp(image, image_ctm, bbox)?;
            self.end_group(&mut save);
        }
        Ok(())
    }

    fn show_path(
        &mut self,
        close: bool,
        fill: bool,
        stroke: bool,
        even_odd: bool,
    ) -> Result<(), String> {
        self.pop_any_pending_mcid_changes();
        self.flush_begin_layer()?;
        let mut path = std::mem::take(&mut self.path);
        if close && !path.close_path() {
            self.warn("path.no_current_point", "closepath with no current point");
        }
        let (mut dofill, mut dostroke) = (fill, stroke);
        let gs = self.gs().clone();
        let bbox = path.bound(dostroke.then_some(&gs.stroke_state), gs.ctm);
        if !bbox.is_valid() || self.state.hidden > 0 {
            dofill = false;
            dostroke = false;
        }
        let mut save = SoftmaskSave::default();
        if dofill || dostroke {
            self.begin_group(bbox, &mut save)?;
        }
        let gs = self.gs().clone();
        let mut knockout = false;
        if dofill
            && dostroke
            && gs.stroke.alpha != 0.0
            && !(gs.stroke.alpha == 1.0 && gs.blendmode == 0)
        {
            knockout = true;
            self.dev.begin_group(bbox, None, false, true, 0, 1.0);
        }
        if dofill {
            let m = &gs.fill;
            match m.kind {
                Kind::Color => self.dev.fill_path(
                    &path,
                    even_odd,
                    gs.ctm,
                    &m.colorspace,
                    m.color(),
                    m.alpha,
                    m.params,
                ),
                Kind::Pattern => {
                    if let Some(pat) = &m.pattern
                        && m.alpha != 0.0
                    {
                        if m.alpha != 1.0 {
                            self.dev.begin_group(bbox, None, false, false, 0, m.alpha);
                        }
                        self.dev.clip_path(&path, even_odd, gs.ctm, bbox);
                        self.show_pattern(pat, m.gstate_num, bbox, false)?;
                        self.dev.pop_clip();
                        if self.gs().fill.alpha != 1.0 {
                            self.dev.end_group();
                        }
                    }
                }
                Kind::Shade => {
                    if let Some(shade) = &m.shade {
                        self.dev.clip_path(&path, even_odd, gs.ctm, bbox);
                        let ctm = self.gstate_ctm(m.gstate_num);
                        self.dev.fill_shade(shade, ctm, m.alpha, m.params);
                        self.dev.pop_clip();
                    }
                }
            }
        }
        if dostroke {
            let gs = self.gs().clone();
            let m = &gs.stroke;
            match m.kind {
                Kind::Color => self.dev.stroke_path(
                    &path,
                    &gs.stroke_state,
                    gs.ctm,
                    &m.colorspace,
                    m.color(),
                    m.alpha,
                    m.params,
                ),
                Kind::Pattern => {
                    if let Some(pat) = &m.pattern
                        && m.alpha != 0.0
                    {
                        if m.alpha != 1.0 {
                            self.dev.begin_group(bbox, None, false, false, 0, m.alpha);
                        }
                        self.dev
                            .clip_stroke_path(&path, &gs.stroke_state, gs.ctm, bbox);
                        self.show_pattern(pat, m.gstate_num, bbox, true)?;
                        self.dev.pop_clip();
                        if self.gs().stroke.alpha != 1.0 {
                            self.dev.end_group();
                        }
                    }
                }
                Kind::Shade => {
                    if let Some(shade) = &m.shade {
                        self.dev
                            .clip_stroke_path(&path, &gs.stroke_state, gs.ctm, bbox);
                        let ctm = self.gstate_ctm(m.gstate_num);
                        self.dev.fill_shade(shade, ctm, m.alpha, m.params);
                        self.dev.pop_clip();
                    }
                }
            }
        }
        if knockout {
            self.dev.end_group();
        }
        if dofill || dostroke {
            self.end_group(&mut save);
        }
        if self.clip {
            self.push_nest(Nest::LiveClip)?;
            self.gs_mut().clip_depth += 1;
            let ctm = self.gs().ctm;
            self.dev.clip_path(&path, self.clip_even_odd, ctm, bbox);
            self.clip = false;
        }
        Ok(())
    }

    // Materials.

    fn set_colorspace(&mut self, stroke: bool, cs: ColorSpace) -> Result<(), String> {
        self.flush_text()?;
        if self.gs().ismask {
            return Ok(());
        }
        let n = cs.n();
        let tint = matches!(cs, ColorSpace::Separation { .. });
        let mat = self.gs_mut().material(stroke);
        mat.kind = Kind::Color;
        mat.colorspace = Arc::new(cs);
        mat.v[..4].copy_from_slice(&[0.0, 0.0, 0.0, 1.0]);
        if tint {
            for v in mat.v.iter_mut().take(n) {
                *v = 1.0;
            }
        }
        Ok(())
    }

    /// `pdf_set_color`: `v` holds MAX_COLORS values.
    fn set_color(&mut self, stroke: bool, v: &[f32]) -> Result<(), String> {
        self.flush_text()?;
        if self.gs().ismask {
            return Ok(());
        }
        let gparent = self.gparent as i32;
        let mat = self.gs_mut().material(stroke);
        match mat.kind {
            Kind::Pattern | Kind::Color => {
                let cs = mat.colorspace.clone();
                clamp_color(&cs, v, &mut mat.v);
            }
            _ => self
                .doc
                .diagnostics()
                .warn("run.color_material", "color incompatible with material"),
        }
        self.gs_mut().material(stroke).gstate_num = gparent;
        Ok(())
    }

    fn set_shade(&mut self, stroke: bool, shade: Arc<Shade>) -> Result<(), String> {
        self.flush_text()?;
        let gparent = self.gparent as i32;
        let mat = self.gs_mut().material(stroke);
        mat.kind = Kind::Shade;
        mat.shade = Some(shade);
        mat.gstate_num = gparent;
        Ok(())
    }

    fn set_pattern(
        &mut self,
        stroke: bool,
        pat: Option<Arc<Pattern>>,
        v: Option<&[f32]>,
    ) -> Result<(), String> {
        self.flush_text()?;
        let gparent = self.gparent as i32;
        {
            let mat = self.gs_mut().material(stroke);
            mat.pattern = pat;
            mat.kind = Kind::Pattern;
        }
        if let Some(v) = v {
            self.set_color(stroke, v)?;
        }
        self.gs_mut().material(stroke).gstate_num = gparent;
        Ok(())
    }

    fn load_pattern(&mut self, obj: &Object) -> Arc<Pattern> {
        let key = num(obj);
        if key > 0
            && let Some(p) = self.state.store.patterns.get(&key)
        {
            return p.clone();
        }
        let pat = Arc::new(load_pattern(self.doc, obj));
        if key > 0 {
            self.state.store.patterns.insert(key, pat.clone());
        }
        pat
    }

    fn load_image(&mut self, obj: &Object) -> Result<Arc<Image>, String> {
        let key = num(obj);
        if key > 0
            && let Some(done) = self.state.store.images.get(&key)
        {
            return done.clone();
        }
        let loaded = image::load(self.doc, obj).map(Arc::new);
        if key > 0 {
            self.state.store.images.insert(key, loaded.clone());
        }
        loaded
    }

    // Marked content and structure.

    fn set_struct_parent(&mut self, sp: i32) {
        self.struct_parent = sp;
        let doc = self.doc;
        self.mcids = self.struct_tree_root.as_ref().and_then(|root| {
            let tree = doc.dict_get(root, b"ParentTree");
            lookup_number(doc, &tree, sp)
        });
    }

    /// `lookup_mcid`.
    fn lookup_mcid(&self, val: Option<&Object>) -> Option<Object> {
        if self.struct_parent == -1 {
            return None;
        }
        let doc = self.doc;
        let mcid = doc.dict_get(val?, b"MCID");
        if !mcid.is_number() {
            return None;
        }
        let id = mcid.to_int();
        let mcids = match self.mcids.as_ref().map(|m| doc.resolve(m)) {
            Some(Object::Array(a)) => a,
            _ => return None,
        };
        let is_id = |o: &Object| matches!(o, Object::Int(_)) && o.to_int() == id;
        let in_k = |entry: &Object| {
            let k = doc.dict_get(entry, b"K");
            match &k {
                Object::Array(items) => items.iter().any(|o| {
                    let o = doc.resolve(o);
                    if matches!(o, Object::Int(_)) {
                        is_id(&o)
                    } else {
                        is_id(&doc.dict_get(&o, b"MCID"))
                    }
                }),
                other => is_id(other),
            }
        };
        if let Some(entry) = usize::try_from(id).ok().and_then(|i| mcids.get(i))
            && in_k(entry)
        {
            return Some(entry.clone());
        }
        mcids.iter().find(|o| in_k(o)).cloned()
    }

    fn pop_structure_to(&mut self, common: Option<&Object>) {
        let doc = self.doc;
        while let Some(sent) = self.mcid_sent.clone() {
            if common == Some(&sent) {
                break;
            }
            let p = self.parent(&sent);
            let tag = stored(doc, &sent, b"S");
            if structure_type(doc, &self.role_map, &tag).is_some() {
                self.dev.end_structure();
            }
            self.mcid_sent = p.clone();
            if p.is_some() && p == self.struct_tree_root {
                self.mcid_sent = None;
                break;
            }
        }
    }

    fn pop_any_pending_mcid_changes(&mut self) {
        if !self.process_structure {
            return;
        }
        if let Some(pending) = self.pending_mcid_pop.take() {
            self.pop_structure_to(Some(&pending));
        }
    }

    /// `get_lineage`.
    fn lineage(&self, a: &Object) -> Result<Vec<Object>, String> {
        let mut line = vec![a.clone()];
        let mut a = a.clone();
        loop {
            match self.parent(&a) {
                None => return Ok(line),
                Some(p) => {
                    if line.get(line.len() >> 1) == Some(&p) {
                        return Err(report::format("cycle in structure tree"));
                    }
                    line.push(p.clone());
                    a = p;
                }
            }
        }
    }

    /// A structure element's /P, as the structure tree check left it.
    fn parent(&self, obj: &Object) -> Option<Object> {
        if let (Some(r), Some(fixes)) = (obj.as_ref(), &self.state.store.struct_parents)
            && let Some(p) = fixes.get(&r.num)
        {
            return Some(p.clone());
        }
        stored(self.doc, obj, b"P")
    }

    fn common_ancestor(
        &mut self,
        a: Option<&Object>,
        b: &Object,
    ) -> Result<Option<Object>, String> {
        let doc = self.doc;
        let Some(a) = a else {
            return Ok(None);
        };
        if doc.resolve(a).as_dict().is_none() || doc.resolve(b).as_dict().is_none() {
            return Ok(None);
        }
        let same = |x: &Object, y: &Object| x == y || doc.resolve(x) == doc.resolve(y);
        let mut line_a = self.lineage(a)?;
        let mut line_b = self.lineage(b)?;
        if !same(&line_a[line_a.len() - 1], &line_b[line_b.len() - 1]) {
            if self.state.store.struct_parents.is_some() {
                return Err(report::format("No common ancestor in structure tree"));
            }
            self.check_structure_tree()?;
            line_a = self.lineage(a)?;
            line_b = self.lineage(b)?;
            if !same(&line_a[line_a.len() - 1], &line_b[line_b.len() - 1]) {
                return Err(report::format("No common ancestor in structure tree"));
            }
        }
        let (mut la, mut lb) = (line_a.len(), line_b.len());
        while la > 0 && lb > 0 && same(&line_a[la - 1], &line_b[lb - 1]) {
            la -= 1;
            lb -= 1;
        }
        Ok(line_a.get(la).cloned())
    }

    /// `pdf_check_structure_tree`, once per document: missing parents are
    /// repaired (MuPDF writes them into the document; they are kept with
    /// the store here).
    fn check_structure_tree(&mut self) -> Result<(), String> {
        self.state.store.struct_parents = Some(Default::default());
        let Some(root) = self.struct_tree_root.clone() else {
            return Ok(());
        };
        let doc = self.doc;
        match stored(doc, &root, b"K") {
            None => {}
            Some(k) => match doc.resolve(&k) {
                Object::Array(items) => {
                    for item in items.iter() {
                        self.check_element(&root, item, &mut Vec::new())?;
                    }
                }
                _ => self.check_element(&root, &k, &mut Vec::new())?,
            },
        }
        let tree = stored(doc, &root, b"ParentTree").unwrap_or(Object::Null);
        self.check_parent_tree(&root, &tree, &mut Vec::new());
        Ok(())
    }

    fn set_parent(&mut self, obj: &Object, parent: Object) {
        if let (Some(r), Some(fixes)) = (obj.as_ref(), self.state.store.struct_parents.as_mut()) {
            fixes.insert(r.num, parent);
        }
    }

    /// `ste`.
    fn check_element(
        &mut self,
        parent: &Object,
        obj: &Object,
        cycle: &mut Vec<i32>,
    ) -> Result<(), String> {
        let doc = self.doc;
        if cycles(cycle, obj) || cycle.len() >= MAX_SCAN_DEPTH {
            return Ok(());
        }
        let value = doc.resolve(obj);
        if value.is_number() {
            return Ok(());
        }
        let kind = stored(doc, obj, b"Type").map(|t| doc.resolve(&t));
        if let Some(t) = &kind {
            if t.is_name(b"OBJR") || t.is_name(b"MCR") {
                return Ok(());
            }
            if !t.is_name(b"StructElem") {
                self.warn("structure.check", "Type should be StructElem");
                return Ok(());
            }
        }
        if stored(doc, obj, b"S").is_none() {
            self.warn("structure.check", "Missing structure type (S)");
            return Ok(());
        }
        match self.parent(obj) {
            None => {
                self.warn("structure.check", "Repairing missing parent (P)");
                if parent.as_ref().is_none() {
                    return Err("argument error: Indirect object required".into());
                }
                self.set_parent(obj, parent.clone());
            }
            Some(p) => {
                if doc.resolve(&p) != doc.resolve(parent) {
                    self.warn("structure.check", "Bad parent (P)");
                    return Ok(());
                }
            }
        }
        let Some(k) = stored(doc, obj, b"K") else {
            return Ok(());
        };
        cycle.push(num(obj));
        let out = match doc.resolve(&k) {
            Object::Array(items) => items
                .iter()
                .try_for_each(|item| self.check_element(obj, item, cycle)),
            _ => self.check_element(obj, &k, cycle),
        };
        cycle.pop();
        out
    }

    /// `pte`: each parent tree entry must lead up to the root.
    fn check_parent_tree(&mut self, root: &Object, tree: &Object, cycle: &mut Vec<i32>) {
        let doc = self.doc;
        if cycles(cycle, tree) || cycle.len() >= MAX_SCAN_DEPTH {
            return;
        }
        cycle.push(num(tree));
        if let Some(kids) = stored(doc, tree, b"Kids") {
            match doc.resolve(&kids) {
                Object::Array(items) => {
                    for kid in items.iter() {
                        self.check_parent_tree(root, kid, cycle);
                    }
                }
                _ => self.check_parent_tree(root, &kids, cycle),
            }
        }
        if let Some(Object::Array(nums)) = stored(doc, tree, b"Nums").map(|n| doc.resolve(&n)) {
            for entry in nums.iter().skip(1).step_by(2) {
                match doc.resolve(entry) {
                    Object::Array(items) => {
                        for item in items.iter() {
                            self.check_num(root, item);
                        }
                    }
                    _ => self.check_num(root, entry),
                }
            }
        }
        cycle.pop();
    }

    /// `check_num`: an element with no parent is made a child of the root.
    fn check_num(&mut self, root: &Object, num: &Object) {
        let doc = self.doc;
        if matches!(num, Object::Null) {
            return;
        }
        let mut at = num.clone();
        // MuPDF follows a cycle for ever.
        for _ in 0..MAX_SCAN_DEPTH * 16 {
            if doc.resolve(&at) == doc.resolve(root) {
                return;
            }
            match self.parent(&at) {
                Some(p) => at = p,
                None => {
                    self.warn(
                        "structure.check",
                        "Repairing missing parent (P) in parent tree nodes",
                    );
                    let r = root.as_ref().unwrap_or(crate::ObjRef::new(0, 0));
                    self.set_parent(&at, Object::Ref(r));
                    return;
                }
            }
        }
    }

    /// `get_struct_index`.
    fn struct_index(&self, send: &Object) -> i32 {
        let doc = self.doc;
        let Some(p) = self.parent(send) else {
            return 0;
        };
        let k = stored(doc, &p, b"K");
        match k.as_ref().map(|k| doc.resolve(k)) {
            Some(Object::Array(items)) if !items.is_empty() => items
                .iter()
                .position(|o| o == send)
                .map_or(-1, |i| i as i32),
            _ => {
                if k.as_ref() == Some(send) {
                    0
                } else {
                    -1
                }
            }
        }
    }

    /// `send_begin_structure`: false when the tree is broken.
    fn send_begin_structure(&mut self, mc_dict: &Object) -> Result<bool, String> {
        let doc = self.doc;
        let common = self.common_ancestor(self.mcid_sent.clone().as_ref(), mc_dict)?;
        self.pop_structure_to(common.as_ref());
        while self.mcid_sent.as_ref() != Some(mc_dict) {
            let mut send = mc_dict.clone();
            let mut slow_ptr = Some(send.clone());
            let mut slow = false;
            loop {
                let p = self.parent(&send);
                if doc.resolve(&send).as_dict().is_none() {
                    self.warn(
                        "structure.bad_parent",
                        "Bad parent link in structure tree. Ignoring structure.",
                    );
                    self.broken_struct_tree = true;
                    return Ok(false);
                }
                if p == self.mcid_sent {
                    break;
                }
                let Some(next) = p else {
                    // MuPDF steps to a null parent and fails the dict test.
                    self.warn(
                        "structure.bad_parent",
                        "Bad parent link in structure tree. Ignoring structure.",
                    );
                    self.broken_struct_tree = true;
                    return Ok(false);
                };
                send = next;
                slow = !slow;
                if !slow {
                    slow_ptr = slow_ptr.and_then(|s| self.parent(&s));
                }
                if slow_ptr.as_ref() == Some(&send) {
                    self.warn(
                        "structure.loop",
                        "Loop found in structure tree. Ignoring structure.",
                    );
                    self.broken_struct_tree = true;
                    return Ok(false);
                }
            }
            let idx = self.struct_index(&send);
            let tag = stored(doc, &send, b"S");
            if let Some(standard) = structure_type(doc, &self.role_map, &tag) {
                let raw = tag
                    .as_ref()
                    .map(|t| doc.resolve(t))
                    .and_then(|t| t.as_name().map(<[u8]>::to_vec))
                    .unwrap_or_default();
                self.dev.begin_structure(standard, &raw, idx);
            }
            self.mcid_sent = Some(send);
        }
        Ok(true)
    }

    /// `begin_metatext`.
    fn begin_metatext(
        &mut self,
        val: Option<&Object>,
        mcid: Option<&Object>,
        meta: Metatext,
        key: &[u8],
    ) -> Result<(), String> {
        if let Some(text) = self.metatext(val, mcid, key) {
            self.flush_text()?;
            self.dev.begin_metatext(meta, &text);
        }
        Ok(())
    }

    fn end_metatext(
        &mut self,
        val: Option<&Object>,
        mcid: Option<&Object>,
        key: &[u8],
    ) -> Result<(), String> {
        if self.metatext(val, mcid, key).is_some() {
            self.flush_text()?;
            self.dev.end_metatext();
        }
        Ok(())
    }

    /// The text `key` gives in `val` or the structure element, as
    /// `pdf_to_text_string` reads it (up to a NUL).
    fn metatext(&self, val: Option<&Object>, mcid: Option<&Object>, key: &[u8]) -> Option<Vec<u8>> {
        let doc = self.doc;
        let text = val
            .and_then(|v| stored(doc, v, key))
            .or_else(|| mcid.and_then(|m| stored(doc, m, key)))?;
        Some(text_bytes(doc, &text))
    }

    fn push_begin_layer(&mut self, name: Vec<u8>) {
        self.begin_layers.push(name);
    }

    fn begin_oc(&mut self, val: &Object, cycle: &mut Vec<i32>) -> Result<(), String> {
        let doc = self.doc;
        if cycles(cycle, val) || cycle.len() >= MAX_SCAN_DEPTH {
            return Ok(());
        }
        cycle.push(num(val));
        if let Some(name) = stored(doc, val, b"Name") {
            self.flush_text()?;
            let name = match doc.resolve(&name) {
                Object::Name(n) => c_string(&n).to_vec(),
                Object::String(_) => text_bytes(doc, &name),
                _ => Vec::new(),
            };
            self.push_begin_layer(name);
        } else if let Object::Array(ocgs) = doc.dict_get(val, b"OCGs") {
            for o in ocgs.iter() {
                self.begin_oc(o, cycle)?;
            }
        }
        cycle.pop();
        Ok(())
    }

    fn end_oc(&mut self, val: &Object, cycle: &mut Vec<i32>) -> Result<(), String> {
        let doc = self.doc;
        if cycles(cycle, val) || cycle.len() >= MAX_SCAN_DEPTH {
            return Ok(());
        }
        cycle.push(num(val));
        if stored(doc, val, b"Name").is_some() {
            self.flush_begin_layer()?;
            self.do_end_layer();
        } else if let Object::Array(ocgs) = doc.dict_get(val, b"OCGs") {
            for o in ocgs.iter().rev() {
                self.end_oc(o, cycle)?;
            }
        }
        cycle.pop();
        Ok(())
    }

    fn push_marked_content(&mut self, tag: &[u8], val: Option<&Object>) -> Result<(), String> {
        self.pending_mcid_pop = None;
        self.flush_text()?;
        self.marked.push(Marked {
            tag: tag.to_vec(),
            val: val.cloned(),
            structure_pushed: false,
        });
        let mc_dict = self.lookup_mcid(val);
        if self.process_layers {
            if tag == b"OC"
                && let Some(v) = val
            {
                self.begin_oc(v, &mut Vec::new())?;
            }
            if tag == b"Layer"
                && let Some(title) = val.and_then(|v| stored(self.doc, v, b"Title"))
            {
                self.flush_text()?;
                let name = text_bytes(self.doc, &title);
                self.push_begin_layer(name);
            }
        }
        if self.process_structure
            && let Some(mc) = &mc_dict
            && !self.broken_struct_tree
        {
            match self.send_begin_structure(mc) {
                Ok(pushed) => {
                    if let Some(top) = self.marked.last_mut() {
                        top.structure_pushed = pushed;
                    }
                }
                Err(line) => {
                    self.warn("error.reported", line);
                    self.warn(
                        "structure.broken",
                        "structure tree broken, assume tree is missing",
                    );
                    self.broken_struct_tree = true;
                }
            }
        }
        let mc = mc_dict.as_ref();
        self.begin_metatext(val, mc, Metatext::ActualText, b"ActualText")?;
        self.begin_metatext(val, mc, Metatext::Alt, b"Alt")?;
        self.begin_metatext(val, mc, Metatext::Abbreviation, b"E")?;
        self.begin_metatext(val, mc, Metatext::Title, b"T")
    }

    fn pop_marked_content(&mut self, neat: bool) -> Result<(), String> {
        let Some(mc) = self.marked.pop() else {
            return Ok(());
        };
        if !neat {
            return Ok(());
        }
        self.flush_text()?;
        let val = mc.val.as_ref();
        let mc_dict = self.lookup_mcid(val);
        let m = mc_dict.as_ref();
        self.end_metatext(val, m, b"T")?;
        self.end_metatext(val, m, b"E")?;
        self.end_metatext(val, m, b"Alt")?;
        self.end_metatext(val, m, b"ActualText")?;
        if mc_dict.is_some() && !self.broken_struct_tree && mc.structure_pushed {
            let previous = self
                .marked
                .iter()
                .rev()
                .find_map(|outer| self.lookup_mcid(outer.val.as_ref()));
            self.pending_mcid_pop = previous;
        }
        if mc.tag == b"Layer"
            && let Some(v) = val
            && stored(self.doc, v, b"Title").is_some()
        {
            self.flush_text()?;
            self.flush_begin_layer()?;
            self.do_end_layer();
        }
        if mc.tag == b"OC"
            && let Some(v) = val
        {
            self.end_oc(v, &mut Vec::new())?;
        }
        Ok(())
    }

    fn clear_marked_content(&mut self) -> Result<(), String> {
        while !self.marked.is_empty() {
            self.pop_marked_content(true)?;
        }
        Ok(())
    }

    // Forms.

    /// `pdf_run_xobject`.
    fn run_xobject(
        &mut self,
        xobj: &Object,
        page_resources: Option<Object>,
        transform: Matrix,
        is_smask: bool,
    ) -> Result<(), String> {
        if cycles(&self.cycle, xobj) {
            return Ok(());
        }
        self.cycle.push(num(xobj));
        self.pop_any_pending_mcid_changes();
        let out = match self.flush_begin_layer() {
            Ok(()) => {
                let save_marked = std::mem::take(&mut self.marked);
                let save_struct_parent = self.struct_parent;
                let save_mcids = self.mcids.clone();
                let save_default_cs = self.default_cs.clone();
                let out = self.run_xobject_imp(xobj, page_resources, transform, is_smask);
                self.marked = save_marked;
                self.default_cs = save_default_cs;
                self.struct_parent = save_struct_parent;
                self.mcids = save_mcids;
                out
            }
            Err(e) => Err(e),
        };
        self.cycle.pop();
        // Syntax errors leave as format errors.
        out.map_err(|e| match e.strip_prefix("syntax error: ") {
            Some(rest) => report::format(rest),
            None => e,
        })
    }

    fn run_xobject_imp(
        &mut self,
        xobj: &Object,
        page_resources: Option<Object>,
        mut transform: Matrix,
        is_smask: bool,
    ) -> Result<(), String> {
        let doc = self.doc;
        let gparent_save = self.gparent;
        self.gparent = self.gtop();
        let oldtop = self.gtop();
        let save_default_cs = self.default_cs.clone();

        let sp = match doc.dict_get(xobj, b"StructParent") {
            v if v.is_number() => v.to_int(),
            _ => -1,
        };
        self.set_struct_parent(sp);
        let oc = stored(doc, xobj, b"OC");
        if let Some(oc) = &oc {
            self.begin_oc(oc, &mut Vec::new())?;
        }
        self.gsave()?;
        let bbox = doc.to_rect(Some(&doc.dict_get(xobj, b"BBox")));
        let matrix = to_matrix(doc, &doc.dict_get(xobj, b"Matrix"));
        let group = doc.dict_get(xobj, b"Group");
        let transparency = doc.dict_get(&group, b"S").is_name(b"Transparency");
        transform = matrix.concat(transform);
        let ctm = transform.concat(self.gs().ctm);
        self.gs_mut().ctm = ctm;
        let gparent_save_ctm = self.gstate[self.gparent].ctm;
        self.gstate[self.gparent].ctm = ctm;

        let mut softmask = SoftmaskSave::default();
        if transparency {
            let isolated = matches!(doc.dict_get(&group, b"I"), Object::Bool(true));
            let knockout = matches!(doc.dict_get(&group, b"K"), Object::Bool(true));
            let tbox = bbox.transform(ctm);
            self.begin_softmask(&mut softmask, tbox)?;
            let cs = if isolated {
                xobject_colorspace(doc, xobj)
            } else {
                None
            };
            let gs = self.gs();
            let (blendmode, alpha) = (gs.blendmode, gs.fill.alpha);
            self.dev.begin_group(
                tbox,
                cs.as_ref(),
                is_smask || isolated,
                knockout,
                blendmode,
                alpha,
            );
            let g = self.gs_mut();
            g.blendmode = 0;
            g.stroke.alpha = 1.0;
            g.fill.alpha = 1.0;
        }

        // Save here so the clip does not persist.
        self.gsave()?;
        self.path.move_to(bbox.x0, bbox.y0);
        self.line_to(bbox.x1, bbox.y0);
        self.line_to(bbox.x1, bbox.y1);
        self.line_to(bbox.x0, bbox.y1);
        if !self.path.close_path() {
            self.warn("path.no_current_point", "closepath with no current point");
        }
        self.clip = true;
        self.show_path(false, false, false, false)?;

        let resources = stored(doc, xobj, b"Resources").or(page_resources);
        let updated = resources
            .as_ref()
            .and_then(|r| update_default_colorspaces(doc, &self.default_cs, r));
        if let Some(dcs) = &updated {
            self.dev.set_default_colorspaces(dcs);
            self.default_cs = dcs.clone();
        }
        let oldbot = self.gbot;
        self.gbot = self.gtop();
        self.process(resources.as_ref(), Some(xobj))?;
        while self.gtop() > self.gbot {
            self.grestore();
        }
        self.gbot = oldbot;
        // The clip's state.
        self.grestore();
        if transparency {
            self.dev.end_group();
            self.end_softmask(&mut softmask);
        }
        self.gstate[self.gparent].ctm = gparent_save_ctm;
        self.gparent = gparent_save;
        while oldtop < self.gtop() {
            self.grestore();
        }
        if let Some(oc) = &oc {
            self.end_oc(oc, &mut Vec::new())?;
        }
        if updated.is_some() {
            self.dev.set_default_colorspaces(&save_default_cs);
        }
        self.clear_marked_content()
    }

    fn line_to(&mut self, x: f32, y: f32) {
        if !self.path.line_to(x, y) {
            self.warn("path.no_current_point", "lineto with no current point");
        }
    }

    fn curve_result(&self, ok: bool) {
        if !ok {
            self.warn("path.no_current_point", "curveto with no current point");
        }
    }

    /// `pdf_run_gs_SMask`, with the mask dictionary's entries read as
    /// `pdf_process_extgstate` reads them.
    fn soft_mask(&mut self, mask: Option<&Object>) -> Result<(), String> {
        self.flush_text()?;
        let doc = self.doc;
        let resources = self.state.resources().cloned();
        let mut smask = None;
        let mut smask_cs = None;
        let mut bc = [0.0f32; MAX_COLORS];
        let mut luminosity = false;
        let mut tr = None;
        if let Some(obj) = mask {
            smask = stored(doc, obj, b"G");
            smask_cs = smask.as_ref().and_then(|g| xobject_colorspace(doc, g));
            let n = smask_cs.as_ref().map_or(1, |c| c.n());
            if matches!(
                smask_cs.as_ref().and_then(ColorSpace::family),
                Some(colorspace::Family::Cmyk)
            ) {
                bc[3] = 1.0;
            }
            if let Object::Array(a) = doc.dict_get(obj, b"BC") {
                for (k, slot) in bc.iter_mut().enumerate().take(n.min(MAX_COLORS)) {
                    *slot = a.get(k).map_or(0.0, |v| doc.resolve(v).to_real());
                }
            }
            luminosity = doc.dict_get(obj, b"S").is_name(b"Luminosity");
            tr = stored(doc, obj, b"TR").filter(|t| !doc.resolve(t).is_name(b"Identity"));
        }
        let g = self.gs_mut();
        g.softmask = None;
        g.softmask_cs = None;
        g.softmask_resources = None;
        if let Some(sm) = smask {
            let cs_n = smask_cs.as_ref().map_or(0, |c| c.n().min(MAX_COLORS));
            g.softmask_ctm = g.ctm;
            g.softmask = Some(sm);
            g.softmask_cs = smask_cs;
            g.softmask_resources = resources;
            g.softmask_tr = tr;
            g.softmask_bc[..cs_n].copy_from_slice(&bc[..cs_n]);
            g.luminosity = luminosity;
        }
        Ok(())
    }

    /// `pdf_close_run_processor`.
    pub fn close(&mut self) -> Result<(), String> {
        if self.aborted {
            return Ok(());
        }
        while self.gtop() > 0 {
            if self.gtop() <= self.gbot {
                // MuPDF would loop here for ever.
                break;
            }
            self.grestore();
        }
        self.pop_nest_until_empty();
        if self.process_structure {
            self.pop_structure_to(None);
        }
        self.clear_marked_content()
    }
}

/// `load_transfer_function`.
fn load_transfer(doc: &Document, obj: &Object) -> Result<Option<Function>, String> {
    if doc.resolve(obj).is_name(b"Identity") {
        return Ok(None);
    }
    function::load(doc, obj, 1, 1).map(Some)
}

/// `pdf_xobject_colorspace`: the group's /CS, if it loads and can be
/// blended in.
fn xobject_colorspace(doc: &Document, xobj: &Object) -> Option<ColorSpace> {
    let group = stored(doc, xobj, b"Group")?;
    let cs = stored(doc, &group, b"CS")?;
    let diag = doc.diagnostics();
    match colorspace::load(doc, &cs) {
        Ok(cs) => {
            if valid_blend(&cs) {
                Some(cs)
            } else {
                diag.warn(
                    "run.blend_colorspace",
                    format!(
                        "Ignoring invalid XObject blending colorspace: {}.",
                        cs.type_name()
                    ),
                );
                None
            }
        }
        Err(line) => {
            diag.warn("error.reported", line);
            diag.warn(
                "run.blend_colorspace",
                "Ignoring XObject blending colorspace.",
            );
            None
        }
    }
}

/// `fz_is_valid_blend_colorspace`.
fn valid_blend(cs: &ColorSpace) -> bool {
    matches!(
        cs.family(),
        Some(colorspace::Family::Gray | colorspace::Family::Rgb | colorspace::Family::Cmyk)
    )
}

/// `fz_clamp_color`.
fn clamp_color(cs: &ColorSpace, input: &[f32], out: &mut [f32; MAX_COLORS]) {
    let at = |i: usize| input.get(i).copied().unwrap_or(0.0);
    let clamp = |v: f32, lo: f32, hi: f32| {
        if v < lo {
            lo
        } else if v > hi {
            hi
        } else {
            v
        }
    };
    match cs {
        ColorSpace::Lab => {
            out[0] = clamp(at(0), 0.0, 100.0);
            out[1] = clamp(at(1), -128.0, 127.0);
            out[2] = clamp(at(2), -128.0, 127.0);
        }
        ColorSpace::Icc(colorspace::Family::Lab) => {
            out[0] = clamp(at(0), 0.0, 100.0);
            out[1] = clamp(at(1), -128.0, 127.0);
            out[2] = clamp(at(2), -128.0, 127.0);
        }
        ColorSpace::Indexed { high, .. } => {
            // Rounded to an index first, as C converts to int.
            let i = (at(0) + 0.5) as i32;
            out[0] = i.clamp(0, i32::from(*high)) as f32 / 255.0;
        }
        _ => {
            for (i, slot) in out.iter_mut().enumerate().take(cs.n().min(MAX_COLORS)) {
                *slot = clamp(at(i), 0.0, 1.0);
            }
        }
    }
}

/// A text string as `pdf_to_text_string` makes it, up to a NUL; any
/// other object gives "".
fn text_bytes(doc: &Document, obj: &Object) -> Vec<u8> {
    match doc.resolve(obj) {
        Object::String(s) => {
            let bytes = crate::text_string::utf8_from_pdf_string(&s);
            c_string(&bytes).to_vec()
        }
        _ => Vec::new(),
    }
}

fn c_string(bytes: &[u8]) -> &[u8] {
    bytes.split(|&b| b == 0).next().unwrap_or_default()
}

/// `pdf_load_default_colorspaces_imp` into a copy of `defaults`: each
/// Default space that loads and has the right family.
fn load_defaults(doc: &Document, defaults: &mut DefaultColorSpaces, dict: &Object) {
    let diag = doc.diagnostics();
    let slots: [(&[u8], colorspace::Family); 3] = [
        (b"DefaultGray", colorspace::Family::Gray),
        (b"DefaultRGB", colorspace::Family::Rgb),
        (b"DefaultCMYK", colorspace::Family::Cmyk),
    ];
    for (key, family) in slots {
        let Some(obj) = stored(doc, dict, key) else {
            continue;
        };
        match colorspace::load(doc, &obj) {
            Ok(cs) => {
                if cs.family() == Some(family) {
                    let slot = match family {
                        colorspace::Family::Gray => &mut defaults.gray,
                        colorspace::Family::Rgb => &mut defaults.rgb,
                        _ => &mut defaults.cmyk,
                    };
                    *slot = Some(cs);
                }
            }
            Err(line) => diag.warn("error.reported", line),
        }
    }
}

/// `pdf_update_default_colorspaces`: None when the resources have no
/// /ColorSpace (the defaults stay as they are).
fn update_default_colorspaces(
    doc: &Document,
    old: &DefaultColorSpaces,
    resources: &Object,
) -> Option<Arc<DefaultColorSpaces>> {
    let dict = stored(doc, resources, b"ColorSpace")?;
    let mut new = old.clone();
    load_defaults(doc, &mut new, &dict);
    Some(Arc::new(new))
}

/// `pdf_load_output_intent`: the first intent's destination profile.
fn output_intent(doc: &Document) -> Option<ColorSpace> {
    let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
    let intents = stored(doc, &root, b"OutputIntents")?;
    let first = match doc.resolve(&intents) {
        Object::Array(a) => a.first().cloned()?,
        _ => return None,
    };
    if matches!(doc.resolve(&first), Object::Null) {
        return None;
    }
    let profile = stored(doc, &first, b"DestOutputProfile")?;
    let diag = doc.diagnostics();
    let loaded = match doc.resolve(&profile) {
        Object::Stream(s) => doc.stream_data(&s).map_err(|e| e.report()),
        _ => Err(report::format("object is not a stream")),
    }
    .and_then(|data| colorspace::icc_colorspace(doc, &data, None));
    let n = doc.dict_get(&profile, b"N").to_int();
    match loaded {
        Ok(family) => {
            let pn = family.n() as i32;
            if pn > n {
                diag.warn(
                    "colorspace.icc_n_mismatch",
                    format!(
                        "ICC colorspace N={n} does not match profile N={pn} (ignoring profile)"
                    ),
                );
            } else {
                if pn < n {
                    diag.warn(
                        "colorspace.icc_n_mismatch",
                        format!(
                            "ICC colorspace N={n} does not match profile N={pn} (using profile)"
                        ),
                    );
                }
                return Some(ColorSpace::Icc(family));
            }
        }
        Err(line) => {
            diag.warn("error.reported", line);
            diag.warn("colorspace.broken_icc", "ignoring broken ICC profile");
        }
    }
    match n {
        1 => Some(ColorSpace::Gray),
        3 => Some(ColorSpace::Rgb),
        4 => Some(ColorSpace::Cmyk),
        _ => {
            diag.warn("error.reported", report::syntax("invalid ICC colorspace"));
            diag.warn(
                "colorspace.output_intent",
                "Attempt to read Output Intent failed",
            );
            None
        }
    }
}

/// `pdf_load_default_colorspaces` for a page.
fn page_defaults(doc: &Document, resources: Option<&Object>) -> DefaultColorSpaces {
    let mut defaults = DefaultColorSpaces::default();
    if let Some(dict) = resources.and_then(|r| stored(doc, r, b"ColorSpace")) {
        load_defaults(doc, &mut defaults, &dict);
    }
    if let Some(oi) = output_intent(doc) {
        match oi.family() {
            Some(
                f @ (colorspace::Family::Gray | colorspace::Family::Rgb | colorspace::Family::Cmyk),
            ) => {
                let slot = match f {
                    colorspace::Family::Gray => &mut defaults.gray,
                    colorspace::Family::Rgb => &mut defaults.rgb,
                    _ => &mut defaults.cmyk,
                };
                if slot.is_none() {
                    *slot = Some(oi.clone());
                }
                defaults.output_intent = Some(oi);
            }
            _ => doc.diagnostics().warn(
                "colorspace.output_intent",
                format!("Ignoring incompatible output intent: {}.", oi.type_name()),
            ),
        }
    }
    defaults
}

impl Processor for Run<'_> {
    fn state(&mut self) -> &mut ProcState {
        &mut self.state
    }

    fn usage(&self) -> Option<&'static str> {
        Some(self.usage)
    }

    fn op(&mut self, op: Op<'_>) -> Result<(), String> {
        let doc = self.doc;
        match op {
            Op::LineWidth(w) => {
                self.flush_text()?;
                self.pop_any_pending_mcid_changes();
                self.flush_begin_layer()?;
                self.gs_mut().stroke_state.linewidth = w;
            }
            Op::LineJoin(j) => {
                self.flush_text()?;
                self.gs_mut().stroke_state.linejoin = j;
            }
            Op::LineCap(c) => {
                self.flush_text()?;
                let s = &mut self.gs_mut().stroke_state;
                s.start_cap = c;
                s.dash_cap = c;
                s.end_cap = c;
            }
            Op::MiterLimit(m) => {
                self.flush_text()?;
                self.gs_mut().stroke_state.miterlimit = m;
            }
            Op::Dash { array, phase } => {
                self.flush_text()?;
                let dash = match array.map(|a| doc.resolve(a)) {
                    Some(Object::Array(a)) => a.iter().map(|v| doc.resolve(v).to_real()).collect(),
                    _ => Vec::new(),
                };
                let s = &mut self.gs_mut().stroke_state;
                s.dash = dash;
                s.dash_phase = phase;
            }
            Op::Intent(name) => {
                self.flush_text()?;
                let ri = device::lookup_intent(name);
                let g = self.gs_mut();
                g.fill.params.ri = ri;
                g.stroke.params.ri = ri;
            }
            Op::GsOverprintStroke(b) => {
                self.flush_text()?;
                let g = self.gs_mut();
                g.stroke.params.op = b;
                g.fill.params.op = b;
            }
            Op::GsOverprintFill(b) => {
                self.flush_text()?;
                self.gs_mut().fill.params.op = b;
            }
            Op::GsOverprintMode(i) => {
                self.flush_text()?;
                let opm = (i as u8) != 0;
                let g = self.gs_mut();
                g.stroke.params.opm = opm;
                g.fill.params.opm = opm;
            }
            Op::GsBlackPointCompensation(name) => {
                self.flush_text()?;
                let on = name == b"ON";
                let g = self.gs_mut();
                g.stroke.params.bp = on;
                g.fill.params.bp = on;
            }
            Op::GsBlendMode(name) => {
                self.flush_text()?;
                self.gs_mut().blendmode = device::lookup_blendmode(name);
            }
            Op::GsStrokeAlpha(a) => {
                self.flush_text()?;
                self.gs_mut().stroke.alpha = clamp01(a);
            }
            Op::GsFillAlpha(a) => {
                self.flush_text()?;
                self.gs_mut().fill.alpha = clamp01(a);
            }
            Op::GsSoftMask(mask) => self.soft_mask(mask)?,
            Op::Flatness(_) | Op::GsBegin { .. } | Op::GsEnd => {}
            Op::Save => {
                self.flush_begin_layer()?;
                self.gsave()?;
            }
            Op::Restore => {
                self.flush_text()?;
                self.grestore();
            }
            Op::Transform([a, b, c, d, e, f]) => {
                self.flush_text()?;
                if self.clip_pending {
                    self.warn(
                        "run.clip_text_ctm",
                        "ignoring CTM change for accumulated glyph clip paths",
                    );
                    return Err("generic error: CTM CHANGE IN CLIP TEXT!!!".into());
                }
                let m = Matrix { a, b, c, d, e, f };
                let g = self.gs_mut();
                g.ctm = m.concat(g.ctm);
            }
            Op::MoveTo(x, y) => self.path.move_to(x, y),
            Op::LineTo(x, y) => self.line_to(x, y),
            Op::CurveTo([x1, y1, x2, y2, x3, y3]) => {
                let ok = self.path.curve_to(x1, y1, x2, y2, x3, y3);
                self.curve_result(ok);
            }
            Op::CurveToV([x2, y2, x3, y3]) => {
                let ok = self.path.curve_to_v(x2, y2, x3, y3);
                self.curve_result(ok);
            }
            Op::CurveToY([x1, y1, x3, y3]) => {
                let ok = self.path.curve_to_y(x1, y1, x3, y3);
                self.curve_result(ok);
            }
            Op::ClosePath => {
                if !self.path.close_path() {
                    self.warn("path.no_current_point", "closepath with no current point");
                }
            }
            Op::Rect([x, y, w, h]) => self.path.rect_to(x, y, x + w, y + h),
            Op::Stroke => self.show_path(false, false, true, false)?,
            Op::CloseStroke => self.show_path(true, false, true, false)?,
            Op::FillCompat | Op::Fill => self.show_path(false, true, false, false)?,
            Op::FillEvenOdd => self.show_path(false, true, false, true)?,
            Op::FillStroke => self.show_path(false, true, true, false)?,
            Op::FillStrokeEvenOdd => self.show_path(false, true, true, true)?,
            Op::CloseFillStroke => self.show_path(true, true, true, false)?,
            Op::CloseFillStrokeEvenOdd => self.show_path(true, true, true, true)?,
            Op::EndPath => self.show_path(false, false, false, false)?,
            Op::Clip => {
                self.flush_text()?;
                self.clip = true;
                self.clip_even_odd = false;
            }
            Op::ClipEvenOdd => {
                self.flush_text()?;
                self.clip = true;
                self.clip_even_odd = true;
            }
            Op::EndText => {
                self.flush_text()?;
                self.flush_clip_text()?;
            }
            Op::Font { font, .. } => {
                let type3 = font.is_some_and(|f| doc.dict_get(f, b"Subtype").is_name(b"Type3"));
                self.gs_mut().font = if type3 { Font::Type3 } else { Font::Simple };
            }
            Op::RenderMode(r) => self.gs_mut().render = r,
            Op::Show { text, .. }
            | Op::NextLineShow { text, .. }
            | Op::NextLineShowSpaced { text, .. } => self.show_string(text)?,
            Op::ShowArray(array) => self.show_array(array)?,
            // Positions and spacing only move glyphs.
            Op::BeginText
            | Op::CharSpacing(_)
            | Op::WordSpacing(_)
            | Op::HorizontalScaling(_)
            | Op::Leading(_)
            | Op::Rise(_)
            | Op::TextMove(..)
            | Op::TextMoveSetLeading(..)
            | Op::TextMatrix(_)
            | Op::NextLine => {}
            Op::Type3Width(..) => self.flags |= DEVFLAG_COLOR,
            Op::Type3Bbox(_) => self.flags |= DEVFLAG_MASK | DEVFLAG_BBOX_DEFINED,
            Op::ColorSpace {
                stroke,
                name,
                colorspace,
            } => {
                if self.flags & DEVFLAG_MASK != 0 {
                    return Ok(());
                }
                match colorspace {
                    Some(cs) if name != b"Pattern" => self.set_colorspace(stroke, cs)?,
                    _ => self.set_pattern(stroke, None, None)?,
                }
            }
            Op::Color { stroke, color } => {
                if self.flags & DEVFLAG_MASK != 0 {
                    return Ok(());
                }
                self.set_color(stroke, &padded(color))?;
            }
            Op::ColorPattern {
                stroke,
                pattern,
                color,
                ..
            } => {
                let pat = self.load_pattern(pattern);
                if self.flags & DEVFLAG_MASK != 0 {
                    return Ok(());
                }
                self.set_pattern(stroke, Some(pat), Some(&padded(color)))?;
            }
            Op::ColorShade { stroke, shade, .. } => {
                if self.flags & DEVFLAG_MASK != 0 {
                    return Ok(());
                }
                self.set_shade(stroke, shade.clone())?;
            }
            Op::Gray { stroke, gray } => {
                if self.flags & DEVFLAG_MASK != 0 {
                    return Ok(());
                }
                self.set_colorspace(stroke, ColorSpace::Gray)?;
                self.set_color(stroke, &padded(&[gray]))?;
            }
            Op::Rgb { stroke, rgb } => {
                if self.flags & DEVFLAG_MASK != 0 {
                    return Ok(());
                }
                self.set_colorspace(stroke, ColorSpace::Rgb)?;
                self.set_color(stroke, &padded(&rgb))?;
            }
            Op::Cmyk { stroke, cmyk } => {
                if self.flags & DEVFLAG_MASK != 0 {
                    return Ok(());
                }
                self.set_colorspace(stroke, ColorSpace::Cmyk)?;
                self.set_color(stroke, &padded(&cmyk))?;
            }
            Op::InlineImage { image, .. } => {
                let image = image::from_inline(doc, image);
                self.show_image(&image)?;
            }
            Op::Shade { shade, .. } => {
                self.pop_any_pending_mcid_changes();
                self.flush_begin_layer()?;
                self.show_shade(shade)?;
            }
            Op::DrawImage { image, .. } => {
                let image = self.load_image(image)?;
                self.show_image(&image)?;
            }
            Op::DrawForm { form, .. } => {
                let resources = self.state.resources().cloned();
                self.run_xobject(form, resources, Matrix::IDENTITY, false)?;
            }
            Op::MarkedPoint(tag) => {
                self.push_marked_content(tag, None)?;
                self.pop_marked_content(true)?;
            }
            Op::MarkedPointProperties { tag, cooked, .. } => {
                self.push_marked_content(tag, cooked)?;
                self.pop_marked_content(true)?;
            }
            Op::BeginMarked(tag) => self.push_marked_content(tag, None)?,
            Op::BeginMarkedProperties { tag, cooked, .. } => {
                self.push_marked_content(tag, cooked)?
            }
            Op::EndMarked => self.pop_marked_content(true)?,
            Op::End => self.flush_text()?,
            Op::BeginCompatibility | Op::EndCompatibility | Op::EndOfData => {}
        }
        Ok(())
    }
}

/// `fz_clamp` to 0..1: NaN stays NaN.
fn clamp01(a: f32) -> f32 {
    a.clamp(0.0, 1.0)
}

/// Operands as the interpreter's stack holds them: zeros past the end.
fn padded(v: &[f32]) -> [f32; MAX_COLORS] {
    let mut out = [0.0; MAX_COLORS];
    for (o, &x) in out.iter_mut().zip(v) {
        *o = x;
    }
    out
}

/// What a page run ended with.
#[derive(Debug)]
pub struct PageRun {
    /// The error that ended it, as a report line.
    pub error: Option<String>,
    pub store: Store,
}

/// `pdf_run_page_contents` with an identity matrix and usage "View", for
/// page `index` (annotations are not run).
pub fn run_page_contents(
    doc: &Document,
    page: &Object,
    dev: &mut dyn Device,
    store: Store,
) -> PageRun {
    let mut store = Some(store);
    let error = run_page_imp(doc, page, dev, &mut store).err();
    PageRun {
        error,
        store: store.unwrap_or_default(),
    }
}

fn run_page_imp(
    doc: &Document,
    page: &Object,
    dev: &mut dyn Device,
    store: &mut Option<Store>,
) -> Result<(), String> {
    let inherited = |key: &[u8]| doc.inherited(page, key).map_err(|e| report::line(&e.0));
    let resources = inherited(b"Resources")?;
    let defaults = Arc::new(page_defaults(doc, resources.as_ref()));
    dev.set_default_colorspaces(&defaults);

    let (fitzbox, ctm) = doc.page_transform(page).map_err(|e| report::line(&e.0))?;
    let fitzbox = fitzbox.transform(ctm);
    let contents = stored(doc, page, b"Contents");
    let mediabox = doc.to_rect(inherited(b"MediaBox")?.as_ref());
    let cropbox = match inherited(b"CropBox")? {
        Some(c) => doc.to_rect(Some(&c)),
        None => mediabox,
    };

    let group = stored(doc, page, b"Group");
    let transparency = group
        .as_ref()
        .is_some_and(|g| doc.dict_get(g, b"S").is_name(b"Transparency"))
        || resources
            .as_ref()
            .is_some_and(|r| resources_use_blending(doc, r, &mut Vec::new()));
    if transparency {
        let cs = match &group {
            Some(g) => match stored(doc, g, b"CS") {
                Some(cs) => match colorspace::load(doc, &cs) {
                    Ok(cs) if valid_blend(&cs) => Some(cs),
                    Ok(cs) => {
                        doc.diagnostics().warn(
                            "run.blend_colorspace",
                            format!(
                                "Ignoring invalid Page blending colorspace: {}.",
                                cs.type_name()
                            ),
                        );
                        None
                    }
                    Err(line) => {
                        doc.diagnostics().warn("error.reported", line);
                        doc.diagnostics()
                            .warn("run.blend_colorspace", "Ignoring Page blending colorspace.");
                        None
                    }
                },
                None => None,
            },
            None => defaults.output_intent.clone(),
        };
        dev.begin_group(fitzbox, cs.as_ref(), true, false, 0, 1.0);
    }

    let struct_parent = match doc.dict_get(page, b"StructParents") {
        v if v.is_number() => v.to_int(),
        _ => -1,
    };
    let crop = cropbox.x0 > mediabox.x0
        || cropbox.x1 < mediabox.x1
        || cropbox.y0 > mediabox.y0
        || cropbox.y1 < mediabox.y1;
    if crop {
        let mut path = Path::new();
        path.rect_to(cropbox.x0, cropbox.y0, cropbox.x1, cropbox.y1);
        dev.clip_path(&path, true, ctm, Rect::INFINITE);
    }

    let out = {
        let mut run = Run::new(
            doc,
            &mut *dev,
            ctm,
            struct_parent,
            defaults,
            store.take().unwrap_or_default(),
        )?;
        let out = run
            .process(resources.as_ref(), contents.as_ref())
            .and_then(|()| run.close());
        *store = Some(run.into_store());
        out
    };
    out?;
    if crop {
        dev.pop_clip();
    }
    if transparency {
        dev.end_group();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records each call by name, with a short detail.
    #[derive(Default)]
    struct Log(Vec<String>);

    impl Device for Log {
        fn fill_path(
            &mut self,
            _: &Path,
            _: bool,
            ctm: Matrix,
            cs: &ColorSpace,
            color: &[f32],
            alpha: f32,
            _: ColorParams,
        ) {
            self.0.push(format!(
                "fill {} {color:?} {alpha} e={}",
                cs.type_name(),
                ctm.e
            ));
        }
        fn stroke_path(
            &mut self,
            _: &Path,
            s: &StrokeState,
            _: Matrix,
            cs: &ColorSpace,
            color: &[f32],
            _: f32,
            _: ColorParams,
        ) {
            self.0.push(format!(
                "stroke {} {color:?} w={}",
                cs.type_name(),
                s.linewidth
            ));
        }
        fn clip_path(&mut self, _: &Path, even_odd: bool, _: Matrix, _: Rect) {
            self.0.push(format!("clip eo={even_odd}"));
        }
        fn pop_clip(&mut self) {
            self.0.push("pop".into());
        }
        fn fill_shade(&mut self, _: &Shade, _: Matrix, _: f32, _: ColorParams) {
            self.0.push("shade".into());
        }
        fn begin_group(
            &mut self,
            _: Rect,
            _: Option<&ColorSpace>,
            i: bool,
            k: bool,
            bm: u8,
            a: f32,
        ) {
            self.0.push(format!("group i={i} k={k} bm={bm} a={a}"));
        }
        fn end_group(&mut self) {
            self.0.push("end group".into());
        }
        fn begin_mask(
            &mut self,
            _: Rect,
            lum: bool,
            _: Option<&ColorSpace>,
            bc: &[f32],
            _: ColorParams,
        ) {
            self.0.push(format!("mask lum={lum} {bc:?}"));
        }
        fn end_mask(&mut self, _: Option<&Function>) {
            self.0.push("end mask".into());
        }
        fn begin_layer(&mut self, name: &[u8]) {
            self.0
                .push(format!("layer {}", String::from_utf8_lossy(name)));
        }
        fn end_layer(&mut self) {
            self.0.push("end layer".into());
        }
        fn begin_structure(&mut self, standard: &'static str, raw: &[u8], index: i32) {
            self.0.push(format!(
                "struct {standard} {} {index}",
                String::from_utf8_lossy(raw)
            ));
        }
        fn end_structure(&mut self) {
            self.0.push("end struct".into());
        }
        fn begin_metatext(&mut self, meta: Metatext, text: &[u8]) {
            self.0
                .push(format!("{} {}", meta.name(), String::from_utf8_lossy(text)));
        }
        fn end_metatext(&mut self) {
            self.0.push("end meta".into());
        }
    }

    /// A one-page document: `catalog` and `resources` are spliced into the
    /// catalog and page, `objects` numbered from 5.
    fn run(
        catalog: &str,
        resources: &str,
        content: &str,
        objects: &[&str],
    ) -> (Vec<String>, Vec<String>) {
        let mut pdf = format!(
            "%PDF-1.7\n1 0 obj<</Type/Catalog/Pages 2 0 R{catalog}>>endobj\n\
             2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
             3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]/Resources<<{resources}>>/Contents 4 0 R/StructParents 0>>endobj\n\
             4 0 obj<</Length {}>>stream\n{content}\nendstream endobj\n",
            content.len()
        );
        for (i, o) in objects.iter().enumerate() {
            pdf.push_str(&format!("{} 0 obj{o}endobj\n", i + 5));
        }
        pdf.push_str(&format!(
            "trailer<</Root 1 0 R/Size {}>>\n",
            objects.len() + 5
        ));
        let doc = Document::open(pdf.into_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e:?}"));
        let mut log = Log::default();
        let out = run_page_contents(&doc, &page, &mut log, Store::default());
        if let Some(e) = out.error {
            log.0.push(format!("error {e}"));
        }
        let warnings = doc
            .diagnostics()
            .snapshot()
            .into_iter()
            .map(|d| d.message)
            .collect();
        (log.0, warnings)
    }

    #[test]
    fn paths_fill_and_stroke_with_the_current_colors() {
        let (calls, _) = run(
            "",
            "",
            "1 0 0 rg 2 w 0 0 10 10 re B 0.5 G 0 0 m 5 5 l S",
            &[],
        );
        assert_eq!(
            calls,
            [
                "fill RGB [1.0, 0.0, 0.0] 1 e=0",
                "stroke Gray [0.0] w=2",
                "stroke Gray [0.5] w=2",
            ]
        );
    }

    #[test]
    fn clips_end_with_their_gstate_and_unbalanced_ones_at_the_end() {
        let (calls, _) = run("", "", "q 0 0 5 5 re W n Q 0 0 5 5 re W* n", &[]);
        assert_eq!(calls, ["clip eo=false", "pop", "clip eo=true", "pop"]);
    }

    #[test]
    fn transparency_groups_wrap_a_forms_clip_and_content() {
        let form = "<</Subtype/Form/BBox[0 0 10 10]/Matrix[1 0 0 1 5 0]/Group<</S/Transparency/I true>>/Length 13>>stream\n0 0 1 1 re f\nendstream ";
        let (calls, _) = run(
            "",
            "/XObject<</F 5 0 R>>/ExtGState<</G<</ca 0.5>>>>",
            "/G gs /F Do",
            &[form],
        );
        // The form's group makes the page use transparency too.
        assert_eq!(
            calls,
            [
                "group i=true k=false bm=0 a=1",
                "group i=true k=false bm=0 a=0.5",
                "clip eo=false",
                "fill Gray [0.0] 1 e=5",
                "pop",
                "end group",
                "end group",
            ]
        );
    }

    #[test]
    fn soft_masks_run_their_group_then_clip() {
        let mask = "<</Subtype/Form/BBox[0 0 10 10]/Group<</S/Transparency/CS/DeviceCMYK>>/Length 13>>stream\n0 0 1 1 re f\nendstream ";
        let (calls, _) = run(
            "",
            "/ExtGState<</M<</SMask<</S/Luminosity/G 5 0 R>>>>>>",
            "/M gs 0 0 5 5 re f",
            &[mask],
        );
        assert_eq!(
            calls,
            [
                "mask lum=true [0.0, 0.0, 0.0, 1.0]",
                "group i=true k=false bm=0 a=1",
                "clip eo=false",
                "fill Gray [0.0] 1 e=0",
                "pop",
                "end group",
                "end mask",
                "fill Gray [0.0] 1 e=0",
                "pop",
            ]
        );
    }

    #[test]
    fn patterns_draw_their_cells_inside_the_shape() {
        let pattern = "<</PatternType 1/PaintType 1/TilingType 1/BBox[0 0 4 4]/XStep 4/YStep 4/Length 21>>stream\n0 1 0 rg 0 0 2 2 re f\nendstream ";
        let (calls, _) = run(
            "",
            "/Pattern<</P 5 0 R>>",
            "/Pattern cs /P scn 0 0 2 2 re f",
            &[pattern],
        );
        assert_eq!(
            calls,
            ["clip eo=false", "fill RGB [0.0, 1.0, 0.0] 1 e=0", "pop"]
        );
    }

    #[test]
    fn hidden_optional_content_draws_nothing_but_keeps_its_layer() {
        let catalog = "/OCProperties<</OCGs[5 0 R]/D<</OFF[5 0 R]>>>>";
        let (calls, _) = run(
            catalog,
            "/Properties<</oc 5 0 R>>",
            "/OC /oc BDC 0 0 5 5 re f EMC 0 0 1 1 re f",
            &["<</Type/OCG/Name(Hidden)>>"],
        );
        assert_eq!(
            calls,
            ["layer Hidden", "end layer", "fill Gray [0.0] 1 e=0"]
        );
    }

    #[test]
    fn marked_content_sends_its_structure_and_actual_text() {
        let catalog = "/StructTreeRoot 5 0 R";
        let objects = [
            "<</Type/StructTreeRoot/K 6 0 R/ParentTree<</Nums[0[7 0 R]]>>>>",
            "<</Type/StructElem/S/Document/P 5 0 R/K 7 0 R>>",
            "<</Type/StructElem/S/P/P 6 0 R/K 0/ActualText(Hi)>>",
        ];
        let (calls, _) = run(catalog, "", "/P <</MCID 0>> BDC 0 0 1 1 re f EMC", &objects);
        assert_eq!(
            calls,
            [
                "struct Document Document 0",
                "struct P P 0",
                "ActualText Hi",
                "fill Gray [0.0] 1 e=0",
                "end meta",
                "end struct",
                "end struct",
            ]
        );
    }

    #[test]
    fn colors_clamp_and_a_failed_form_leaves_its_state() {
        let (calls, warnings) = run(
            "",
            "/XObject<</F 5 0 R>>",
            "2 -1 0.5 rg 0 0 1 1 re f /F Do 0 0 1 1 re f",
            &["<</Subtype/Form/BBox[0 0 1 1]/Length 3>>stream\nfoo\nendstream "],
        );
        assert_eq!(calls[0], "fill RGB [1.0, 0.0, 0.5] 1 e=0");
        assert!(warnings.contains(&"syntax error: unknown keyword: 'foo'".to_string()));
    }
}
