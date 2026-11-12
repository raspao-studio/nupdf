// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-op-filter.c (pdf_new_sanitize_filter),
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! The sanitize filter: operators pass through to the content writer with
//! the graphics state they need, sent only once something is drawn, so
//! redundant state goes and `q`/`Q` balance. The resources the output
//! uses are collected into a new resource dictionary. Hooks decide, glyph
//! by glyph and path segment by segment, what to drop; redaction is built
//! on them. `TJ` arrays are split around dropped glyphs so the rest keep
//! their positions.

use crate::fma::Madd;
use std::rc::Rc;
use std::sync::Arc;

use crate::colorspace::{ColorSpace, MAX_COLORS};
use crate::content::{Op, ProcState, Processor};
use crate::content_clean::{Cycle, Options, filter_xobject_instance};
use crate::content_write::{BiImage, Writer, bi_image};
use crate::document::Document;
use crate::fontdesc::{FontDesc, advance_glyph};
use crate::geometry::{Matrix, Rect};
use crate::object::{Dict, ObjRef, Object};
use crate::path::{FullWalker, Path, StrokeState};
use crate::shade::Shade;

/// `fz_cull_type`: what a culled box belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cull {
    PathFill,
    PathStroke,
    PathFillStroke,
    ClipPathFill,
    ClipPathStroke,
    ClipPathFillStroke,
    Glyph,
    Image,
    Shading,
    /// A path that is neither filled nor stroked (`n`).
    PathDrop,
    ClipPathDrop,
}

impl Cull {
    /// The clip variant of a path type.
    fn clip(self) -> Cull {
        match self {
            Cull::PathFill => Cull::ClipPathFill,
            Cull::PathStroke => Cull::ClipPathStroke,
            Cull::PathFillStroke => Cull::ClipPathFillStroke,
            Cull::PathDrop => Cull::ClipPathDrop,
            other => other,
        }
    }
}

/// What the image filter does with an image.
#[derive(Debug)]
pub enum ImageAction {
    Keep,
    Remove,
    /// Draw this inline image instead.
    Replace(BiImage),
    /// Draw the image XObject with this number instead.
    ReplaceWith(i32),
}

/// The image the filter is shown.
#[derive(Debug)]
pub enum ShownImage<'a> {
    /// An image XObject.
    XObject(&'a Object),
    /// An inline image, as it would be written.
    Inline(&'a BiImage),
}

/// `pdf_sanitize_filter_options`: the callbacks, each off by default.
pub trait Hooks {
    /// Whether `text_filter` is set.
    fn filters_text(&self) -> bool {
        false
    }
    /// Whether to drop a glyph: its Unicode, matrix, the ctm, its box in
    /// glyph space, render mode and alphas.
    #[allow(clippy::too_many_arguments)]
    fn text_filter(
        &mut self,
        _ucs: &[i32],
        _trm: Matrix,
        _ctm: Matrix,
        _bbox: Rect,
        _render: i32,
        _fill_alpha: f32,
        _stroke_alpha: f32,
    ) -> bool {
        false
    }
    /// Whether `culler` is set.
    fn culls(&self) -> bool {
        false
    }
    /// Whether to drop what covers `bbox` (in page space).
    fn cull(&mut self, _bbox: Rect, _kind: Cull) -> bool {
        false
    }
    /// Whether `image_filter` is set.
    fn filters_images(&self) -> bool {
        false
    }
    /// What to do with an image drawn with `ctm` inside `clip`.
    fn image_filter(
        &mut self,
        _doc: &Document,
        _ctm: Matrix,
        _image: ShownImage<'_>,
        _clip: Rect,
    ) -> Result<ImageAction, String> {
        Ok(ImageAction::Keep)
    }
    /// `complete`: the page's finished content stream.
    fn complete(&mut self, _data: &mut Vec<u8>) {}
    /// nupdf's own: a marked-content property list (`BDC`'s operand) to
    /// write instead of `props`, or None to keep it.
    fn marked_content(&mut self, _props: &Object) -> Option<Object> {
        None
    }
}

/// No callbacks: the plain sanitize filter (`mutool clean -s`).
#[derive(Debug, Default)]
pub struct NoHooks;

impl Hooks for NoHooks {}

const FLUSH_CTM: u32 = 1;
const FLUSH_COLOR_F: u32 = 2;
const FLUSH_COLOR_S: u32 = 4;
const FLUSH_TEXT: u32 = 8;
const FLUSH_OP: u32 = 16;
const FLUSH_STROKE_STATE: u32 = 32;
const FLUSH_ALL: u32 = 31;
const FLUSH_STROKE: u32 = FLUSH_CTM + FLUSH_COLOR_S + FLUSH_STROKE_STATE;
const FLUSH_FILL: u32 = FLUSH_CTM + FLUSH_COLOR_F;
const FLUSH_CLIP: u32 = FLUSH_CTM + FLUSH_TEXT + FLUSH_OP;

/// `pdf_filter_gstate_sc`.
#[derive(Debug, Clone)]
struct Sc {
    name: Vec<u8>,
    pat: Option<Object>,
    shd: Option<(Object, Arc<Shade>)>,
    n: usize,
    c: [f32; MAX_COLORS],
}

impl Sc {
    fn new() -> Sc {
        Sc {
            name: Vec::new(),
            pat: None,
            shd: None,
            n: 0,
            c: [0.0; MAX_COLORS],
        }
    }

    fn colors(&self) -> &[f32] {
        &self.c[..self.n.min(MAX_COLORS)]
    }
}

#[derive(Debug, Clone, Default)]
struct Cs {
    name: Vec<u8>,
    cs: Option<ColorSpace>,
    /// Whether `cs` is MuPDF's device gray, RGB or CMYK itself, which the
    /// flush compares against by identity.
    device: bool,
}

impl Cs {
    fn is(&self, family: &ColorSpace) -> bool {
        self.device && self.cs.as_ref() == Some(family)
    }
}

#[derive(Debug, Clone)]
struct Stroke {
    linecap: i32,
    linejoin: i32,
    linewidth: f32,
    miterlimit: f32,
    phase: f32,
    dash: Option<Object>,
}

/// `pdf_text_state`. The font name is compared by identity, as MuPDF
/// compares its `fz_string`: each `Tf` makes a new one.
#[derive(Debug, Clone)]
struct Text {
    char_space: f32,
    word_space: f32,
    scale: f32,
    leading: f32,
    font: Option<Arc<FontDesc>>,
    fontname: Option<Rc<Vec<u8>>>,
    size: f32,
    render: i32,
    rise: f32,
}

/// `pdf_filter_gstate`.
#[derive(Debug, Clone)]
struct G {
    ctm: Matrix,
    cs: Cs,
    big_cs: Cs,
    sc: Sc,
    big_sc: Sc,
    stroke: Stroke,
    text: Text,
}

impl G {
    fn new() -> G {
        G {
            ctm: Matrix::IDENTITY,
            cs: Cs::default(),
            big_cs: Cs::default(),
            sc: Sc::new(),
            big_sc: Sc::new(),
            stroke: Stroke {
                linecap: 0,
                linejoin: 0,
                linewidth: 1.0,
                miterlimit: 10.0,
                phase: 0.0,
                dash: None,
            },
            text: Text {
                char_space: 0.0,
                word_space: 0.0,
                scale: 1.0,
                leading: 0.0,
                font: None,
                fontname: None,
                size: -1.0,
                render: 0,
                rise: 0.0,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipOp {
    None,
    W,
    WStar,
}

/// `filter_gstate`.
#[derive(Debug, Clone)]
struct FilterGState {
    pushed: bool,
    clip_rect: Rect,
    pending: G,
    sent: G,
    clip_op: ClipOp,
    leading: f32,
    ca: f32,
    stroke_ca: f32,
}

/// `editable_str`: an Alt or ActualText being walked alongside the glyphs.
#[derive(Debug, Clone, Default)]
struct Editable {
    text: Option<Vec<char>>,
    edited: bool,
    /// None once the walk lost its place (MuPDF's -1).
    pos: Option<usize>,
}

/// `tag_record`.
#[derive(Debug, Clone)]
struct Tag {
    bdc: bool,
    tag: Vec<u8>,
    raw: Option<Object>,
    mcid_obj: Option<Object>,
    alt: Editable,
    actualtext: Editable,
}

/// `pdf_text_object_state`, as much of it as the filter keeps.
#[derive(Debug, Clone, Copy)]
struct Tos {
    tm: Matrix,
    tlm: Matrix,
    char_tx: f32,
    char_ty: f32,
    gid: i32,
}

/// `pdf_sanitize_processor`, writing to a [`Writer`].
pub struct Sanitize<'a, 'h> {
    doc: &'a Document,
    state: ProcState,
    chain: Writer<'a>,
    structarray: Option<Object>,
    /// The gstates, innermost last; never empty.
    gstate: Vec<FilterGState>,
    tos: Tos,
    /// The font the current text is shown in (`tos.fontdesc`).
    tos_font: Option<Arc<FontDesc>>,
    sent_tlm: Matrix,
    sent_tm: Matrix,
    bt_pending: bool,
    in_bt: bool,
    /// Innermost last.
    current_tags: Vec<Tag>,
    pending_tags: Vec<Tag>,
    /// The resources the output uses (`new_rstack`), innermost last.
    new_res: Vec<Dict>,
    options: Options,
    hooks: &'h mut dyn Hooks,
    transform: Matrix,
    text_sent: bool,
    text_removed: bool,
    path: Path,
    cycle: Cycle,
}

impl std::fmt::Debug for Sanitize<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sanitize").finish_non_exhaustive()
    }
}

fn is_identity(m: &Matrix) -> bool {
    m.a == 1.0 && m.b == 0.0 && m.c == 0.0 && m.d == 1.0 && m.e == 0.0 && m.f == 0.0
}

fn same_matrix(a: &Matrix, b: &Matrix) -> bool {
    a.a == b.a && a.b == b.b && a.c == b.c && a.d == b.d && a.e == b.e && a.f == b.f
}

fn same_rc<T>(a: &Option<Rc<T>>, b: &Option<Rc<T>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn same_arc<T>(a: &Option<Arc<T>>, b: &Option<Arc<T>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// The text-space move `(tx, ty)` that takes `tm` by `(x, y)` in user
/// space, `scale` being 1/det: MuPDF's formula, fused as clang fuses it.
fn untransform(tm: Matrix, x: f32, y: f32, scale: f32) -> (f32, f32) {
    let tx = scale * x.madd(tm.d, -(y * tm.c));
    let ty = scale * x.madd(-tm.b, y * tm.a);
    (tx, ty)
}

/// `set_default_cs_values`: setting a colorspace by one of these names
/// resets the color to the space's initial one.
fn set_default_cs_values(sc: &mut Sc, name: &[u8], cs: Option<&ColorSpace>) {
    let n = cs.map_or(0, ColorSpace::n).min(MAX_COLORS);
    match name {
        b"Separation" | b"DeviceN" => sc.c[..n].fill(1.0),
        b"DeviceGray" | b"DeviceRGB" | b"CalGray" | b"CalRGB" | b"Indexed" | b"Lab"
        | b"ICCBased" => sc.c[..n].fill(0.0),
        b"DeviceCMYK" => {
            sc.c[0] = 0.0;
            sc.c[1] = 0.0;
            sc.c[2] = 0.0;
            sc.c[3] = 1.0;
        }
        _ => return,
    }
    sc.pat = None;
    sc.shd = None;
    sc.name.clear();
    sc.n = n;
}

/// `walk_string`: steps over `uni` in the Alt or ActualText string,
/// deleting it when `remove`.
fn walk_string(uni: i32, remove: bool, s: &mut Editable) {
    let (Some(text), Some(mut pos)) = (s.text.as_mut(), s.pos) else {
        return;
    };
    loop {
        // MuPDF reads the terminating NUL as a rune of 0.
        let rune = text.get(pos).map_or(0, |&c| c as i32);
        if rune == uni {
            // A match: step over it.
        } else if uni == 32 {
            // A space in the glyphs, not in the string: nothing to do.
            break;
        } else if rune == 32 {
            // A space in the string that the glyphs lack: step over it
            // too, removing it when removing.
        } else {
            s.pos = None;
            return;
        }
        if remove {
            if pos < text.len() {
                text.remove(pos);
            }
            s.edited = true;
        } else {
            pos += usize::from(pos < text.len());
        }
        if rune == uni {
            break;
        }
        if pos >= text.len() && rune == 0 {
            break;
        }
    }
    s.pos = Some(pos);
}

impl<'a, 'h> Sanitize<'a, 'h> {
    /// `pdf_new_sanitize_filter`.
    pub fn new(
        doc: &'a Document,
        chain: Writer<'a>,
        structparents: i32,
        transform: Matrix,
        options: Options,
        hooks: &'h mut dyn Hooks,
        cycle: &Cycle,
    ) -> Sanitize<'a, 'h> {
        let structarray = (structparents != -1)
            .then(|| {
                let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
                let tree = doc.dict_get(&doc.dict_get(&root, b"StructTreeRoot"), b"ParentTree");
                crate::run::lookup_number(doc, &tree, structparents)
            })
            .flatten();
        Sanitize {
            doc,
            state: ProcState::default(),
            chain,
            structarray,
            gstate: vec![FilterGState {
                pushed: false,
                clip_rect: Rect::INFINITE,
                pending: G::new(),
                sent: G::new(),
                clip_op: ClipOp::None,
                leading: 0.0,
                ca: 1.0,
                stroke_ca: 1.0,
            }],
            // Zero, as calloc leaves them: text operators before any BT
            // move nothing.
            tos: Tos {
                tm: Matrix::ZERO,
                tlm: Matrix::ZERO,
                char_tx: 0.0,
                char_ty: 0.0,
                gid: 0,
            },
            tos_font: None,
            sent_tlm: Matrix::ZERO,
            sent_tm: Matrix::ZERO,
            bt_pending: false,
            in_bt: false,
            current_tags: Vec::new(),
            pending_tags: Vec::new(),
            new_res: Vec::new(),
            options,
            hooks,
            transform,
            text_sent: false,
            text_removed: false,
            path: Path::new(),
            cycle: cycle.clone(),
        }
    }

    /// `pdf_processor_push_resources`: the old resources for lookups, and a
    /// new dictionary for what the output uses.
    pub fn push_resources(&mut self, res: Object) {
        self.state.push_resources(res);
        self.new_res.push(Dict::new());
    }

    /// `pdf_processor_pop_resources`: the resources the output used.
    pub fn pop_resources(&mut self) -> Object {
        self.state.pop_resources();
        Object::Dict(Arc::new(self.new_res.pop().unwrap_or_default()))
    }

    /// `pdf_close_sanitize_processor`.
    pub fn close(&mut self) {
        while !self.filter_pop() {}
        self.chain.close();
    }

    /// `pdf_reset_sanitize_processor`: the writer's output only.
    pub fn reset(&mut self) {
        self.chain.reset();
    }

    pub fn output(&self) -> &[u8] {
        &self.chain.out
    }

    pub fn into_output(self) -> Vec<u8> {
        self.chain.out
    }

    fn top(&self) -> &FilterGState {
        &self.gstate[self.gstate.len() - 1]
    }

    fn top_mut(&mut self) -> &mut FilterGState {
        let n = self.gstate.len();
        &mut self.gstate[n - 1]
    }

    fn clipped_away(&self) -> bool {
        self.top().clip_rect.is_empty()
    }

    fn emit(&mut self, op: Op<'_>) {
        self.chain.op(&op);
    }

    // Resources.

    fn new_resources(&mut self) -> &mut Dict {
        if self.new_res.is_empty() {
            self.new_res.push(Dict::new());
        }
        let n = self.new_res.len();
        &mut self.new_res[n - 1]
    }

    /// `copy_resource`.
    fn copy_resource(&mut self, key: &[u8], name: &[u8]) {
        if name.is_empty() || self.new_res.is_empty() {
            return;
        }
        let Some(obj) = self.state.lookup(self.doc, key, name) else {
            return;
        };
        self.add_resource(key, name, obj);
    }

    /// `add_resource`.
    fn add_resource(&mut self, key: &[u8], name: &[u8], val: Object) {
        let res = self.new_resources();
        let mut sub = match res.get(key) {
            Some(Object::Dict(d)) => (**d).clone(),
            _ => Dict::new(),
        };
        sub.put(name, val);
        res.put(key, Object::Dict(Arc::new(sub)));
    }

    /// `create_resource_name`: the first of `prefix1`, `prefix2`, ... not
    /// in the new resources.
    fn create_resource_name(&mut self, key: &[u8], prefix: &str) -> Result<Vec<u8>, String> {
        let res = self.new_resources();
        if res.get(key).is_none() {
            res.put(key, Object::Dict(Arc::new(Dict::new())));
        }
        let sub = match res.get(key) {
            Some(Object::Dict(d)) => d.clone(),
            _ => Arc::new(Dict::new()),
        };
        for i in 1..65536 {
            let name = format!("{prefix}{i}").into_bytes();
            if sub.get(&name).is_none() {
                return Ok(name);
            }
        }
        Err("limit error: Cannot create unique resource name".into())
    }

    // Gstates.

    /// `filter_push`.
    fn filter_push(&mut self) {
        let mut g = self.top().clone();
        g.pushed = false;
        self.gstate.push(g);
    }

    /// `filter_pop`: true at the top, where there is nothing to pop.
    fn filter_pop(&mut self) -> bool {
        if self.gstate.len() <= 1 {
            return true;
        }
        if self.top().pushed {
            self.emit(Op::Restore);
        }
        self.gstate.pop();
        false
    }

    /// `flush_tags`: the pending tags written, oldest first, and made
    /// current.
    fn flush_tags(&mut self) {
        let tags = std::mem::take(&mut self.pending_tags);
        for tag in tags {
            if tag.bdc {
                self.emit(Op::BeginMarkedProperties {
                    tag: &tag.tag,
                    raw: tag.raw.as_ref(),
                    cooked: tag.raw.as_ref(),
                });
            } else {
                self.emit(Op::BeginMarked(&tag.tag));
            }
            self.current_tags.push(tag);
        }
    }

    /// `ensure_pushed`: a `q` before the first change in this gstate; at
    /// the top, a new gstate for it.
    fn ensure_pushed(&mut self) {
        if self.gstate.len() > 1 {
            if !self.top().pushed {
                self.top_mut().pushed = true;
                self.emit(Op::Save);
            }
            return;
        }
        self.filter_push();
        self.top_mut().pushed = true;
        self.emit(Op::Save);
        let n = self.gstate.len();
        self.gstate[n - 2].pending = self.gstate[n - 2].sent.clone();
    }

    /// `filter_flush`: the pending state `flush` asks for, sent.
    fn filter_flush(&mut self, mut flush: u32) {
        if self.clipped_away() {
            return;
        }
        if flush != 0 {
            self.flush_tags();
        }
        if flush & FLUSH_OP != 0 {
            self.ensure_pushed();
        }
        if flush & FLUSH_TEXT != 0 {
            let sane_tr = self.top().pending.text.render + 1;
            if sane_tr & 1 != 0 {
                flush |= FLUSH_COLOR_F;
            }
            if sane_tr & 2 != 0 {
                flush |= FLUSH_COLOR_S;
            }
        }
        if flush & FLUSH_CTM != 0 && !is_identity(&self.top().pending.ctm) {
            let current = self.top().sent.ctm;
            self.ensure_pushed();
            let m = self.top().pending.ctm;
            self.emit(Op::Transform([m.a, m.b, m.c, m.d, m.e, m.f]));
            let g = self.top_mut();
            g.sent.ctm = m.concat(current);
            g.pending.ctm = Matrix::IDENTITY;
        }
        if flush & FLUSH_COLOR_F != 0 {
            self.flush_color(false);
        }
        if flush & FLUSH_COLOR_S != 0 {
            self.flush_color(true);
        }
        if flush & FLUSH_STROKE_STATE != 0 {
            self.flush_stroke_state();
        }
        if flush & FLUSH_TEXT != 0 {
            self.flush_text();
        }
    }

    fn flush_color(&mut self, stroke: bool) {
        let pick = |g: &G| -> (Cs, Sc) {
            if stroke {
                (g.big_cs.clone(), g.big_sc.clone())
            } else {
                (g.cs.clone(), g.sc.clone())
            }
        };
        let (pcs, psc) = pick(&self.top().pending);
        let (scs, ssc) = pick(&self.top().sent);
        let plain = psc.pat.is_none() && psc.shd.is_none();
        let sent_plain = ssc.pat.is_none() && ssc.shd.is_none();
        let gray = pcs.is(&ColorSpace::Gray)
            && plain
            && psc.n == 1
            && (!scs.is(&ColorSpace::Gray)
                || !sent_plain
                // MuPDF checks the stroke color's count against 0.
                || ssc.n != if stroke { 0 } else { 1 }
                || psc.c[0] != ssc.c[0]);
        let rgb = pcs.is(&ColorSpace::Rgb)
            && plain
            && psc.n == 3
            && (!scs.is(&ColorSpace::Rgb)
                || !sent_plain
                || ssc.n != 3
                || psc.c[0] != ssc.c[0]
                // MuPDF compares the second component twice, not the third.
                || psc.c[1] != ssc.c[1]);
        let cmyk = pcs.is(&ColorSpace::Cmyk)
            && plain
            && psc.n == 4
            && (!scs.is(&ColorSpace::Cmyk) || !sent_plain || psc.c[..4] != ssc.c[..4]);
        if gray {
            self.ensure_pushed();
            self.emit(Op::Gray {
                stroke,
                gray: psc.c[0],
            });
        } else if rgb {
            self.ensure_pushed();
            self.emit(Op::Rgb {
                stroke,
                rgb: [psc.c[0], psc.c[1], psc.c[2]],
            });
        } else if cmyk {
            self.ensure_pushed();
            self.emit(Op::Cmyk {
                stroke,
                cmyk: [psc.c[0], psc.c[1], psc.c[2], psc.c[3]],
            });
        } else {
            if pcs.name != scs.name {
                self.ensure_pushed();
                self.emit(Op::ColorSpace {
                    stroke,
                    name: &pcs.name,
                    colorspace: pcs.cs.clone(),
                });
            }
            let differs = psc.n != ssc.n || psc.colors() != ssc.colors();
            if !psc.name.is_empty() {
                if psc.name != ssc.name || differs {
                    self.ensure_pushed();
                    if let Some(pat) = &psc.pat {
                        self.emit(Op::ColorPattern {
                            stroke,
                            name: &psc.name,
                            pattern: pat,
                            color: psc.colors(),
                        });
                    }
                    if let Some((obj, shade)) = &psc.shd {
                        self.emit(Op::ColorShade {
                            stroke,
                            name: &psc.name,
                            shading: obj,
                            shade,
                        });
                    }
                }
            } else if differs {
                self.ensure_pushed();
                self.emit(Op::Color {
                    stroke,
                    color: psc.colors(),
                });
            }
        }
        let g = self.top_mut();
        if stroke {
            g.sent.big_cs = pcs;
            g.sent.big_sc = psc;
        } else {
            g.sent.cs = pcs;
            g.sent.sc = psc;
        }
    }

    fn flush_stroke_state(&mut self) {
        let p = self.top().pending.stroke.clone();
        let s = self.top().sent.stroke.clone();
        if p.linecap != s.linecap {
            self.ensure_pushed();
            self.emit(Op::LineCap(p.linecap));
        }
        if p.linejoin != s.linejoin {
            self.ensure_pushed();
            self.emit(Op::LineJoin(p.linejoin));
        }
        if p.linewidth != s.linewidth {
            self.ensure_pushed();
            self.emit(Op::LineWidth(p.linewidth));
        }
        if p.miterlimit != s.miterlimit {
            self.ensure_pushed();
            self.emit(Op::MiterLimit(p.miterlimit));
        }
        if p.phase != s.phase || p.dash != s.dash {
            self.ensure_pushed();
            self.emit(Op::Dash {
                array: p.dash.as_ref(),
                phase: p.phase,
            });
        }
        self.top_mut().sent.stroke = p;
    }

    fn flush_text(&mut self) {
        if self.bt_pending {
            self.ensure_pushed();
            self.emit(Op::BeginText);
            self.bt_pending = false;
            self.in_bt = true;
            self.text_sent = false;
            self.text_removed = false;
        }
        if !self.in_bt {
            return;
        }
        let p = self.top().pending.text.clone();
        let s = self.top().sent.text.clone();
        if p.char_space != s.char_space {
            self.ensure_pushed();
            self.emit(Op::CharSpacing(p.char_space));
        }
        if p.word_space != s.word_space {
            self.ensure_pushed();
            self.emit(Op::WordSpacing(p.word_space));
        }
        if p.scale != s.scale {
            self.ensure_pushed();
            self.emit(Op::HorizontalScaling(p.scale * 100.0));
        }
        if !same_arc(&p.font, &s.font) || p.size != s.size || !same_rc(&p.fontname, &s.fontname) {
            self.ensure_pushed();
            let name: &[u8] = p.fontname.as_deref().map_or(&[], |v| v.as_slice());
            self.emit(Op::Font {
                name,
                font: None,
                size: p.size,
            });
        }
        if p.render != s.render {
            self.ensure_pushed();
            self.emit(Op::RenderMode(p.render));
        }
        if p.rise != s.rise {
            self.ensure_pushed();
            self.emit(Op::Rise(p.rise));
        }
        // Everything has been sent but the leading.
        let sent_leading = self.top().sent.text.leading;
        let g = self.top_mut();
        g.sent.text = p.clone();
        g.sent.text.leading = sent_leading;
        if let Some(name) = p.fontname.as_deref() {
            self.copy_resource(b"Font", name);
        }
        let tm = self.tos.tm;
        if !same_matrix(&tm, &self.sent_tm) {
            self.ensure_pushed();
            let tlm = self.sent_tlm;
            let mut done = false;
            if tm.a == tlm.a && tm.b == tlm.b && tm.c == tlm.c && tm.d == tlm.d {
                let x = tm.e - tlm.e;
                let y = tm.f - tlm.f;
                let det = tm.a.madd(tm.d, -(tm.b * tm.c));
                if !(-0.001..=0.001).contains(&det) {
                    let scale = 1.0 / det;
                    let (tx, ty) = untransform(tm, x, y, scale);
                    if tx == 0.0 && self.top().sent.text.leading == -ty {
                        self.emit(Op::NextLine);
                    } else {
                        self.top_mut().sent.text.leading = -ty;
                        self.emit(Op::TextMoveSetLeading(tx, ty));
                    }
                    done = true;
                }
            }
            if !done {
                self.emit(Op::TextMatrix([tm.a, tm.b, tm.c, tm.d, tm.e, tm.f]));
            }
            self.sent_tlm = tm;
            self.sent_tm = tm;
        }
    }

    // Text.

    /// `pdf_tos_make_trm` with the pending text state: the glyph's matrix.
    fn make_trm(&mut self, fd: &FontDesc, cid: i32) -> Matrix {
        let t = &self.top().pending.text;
        let (size, scale, rise, char_space) = (t.size, t.scale, t.rise, t.char_space);
        let mut tsm = Matrix {
            a: size * scale,
            b: 0.0,
            c: 0.0,
            d: size,
            e: 0.0,
            f: rise,
        };
        // As clang fuses them in MuPDF.
        if fd.wmode == 0 {
            let h = fd.lookup_hmtx(cid);
            let w0 = h.w as f32 * 0.001;
            self.tos.char_tx = w0.madd(size, char_space) * scale;
            self.tos.char_ty = 0.0;
        } else {
            let v = fd.lookup_vmtx(cid);
            let w1 = v.w as f32 * 0.001;
            tsm.e = (-(v.x as f32 * size * scale)).madd(0.001, tsm.e);
            tsm.f = (-(v.y as f32 * size)).madd(0.001, tsm.f);
            self.tos.char_tx = 0.0;
            self.tos.char_ty = w1.madd(size, char_space);
        }
        self.tos.gid = fd.cid_to_gid(cid);
        tsm.concat(self.tos.tm)
    }

    fn move_after_char(&mut self) {
        self.tos.tm = self
            .tos
            .tm
            .pre_translate(self.tos.char_tx, self.tos.char_ty);
    }

    /// `filter_show_char`: whether to drop the glyph, and its first
    /// Unicode value.
    fn filter_show_char(&mut self, fd: &FontDesc, cid: i32) -> (bool, i32) {
        let trm = self.make_trm(fd, cid);
        let mut ucs = Vec::new();
        if let Some(u) = fd.to_unicode.as_ref() {
            u.lookup_full_into(cid as u32, &mut ucs);
        }
        if ucs.is_empty()
            && let Some(&u) = fd
                .cid_to_ucs
                .as_ref()
                .and_then(|m| usize::try_from(cid).ok().and_then(|c| m.get(c)))
        {
            ucs.push(i32::from(u));
        }
        if ucs.is_empty() || (ucs.len() == 1 && ucs[0] == 0) {
            ucs.clear();
            ucs.push(crate::glyphs::REPLACEMENT as i32);
        }
        let mut remove = false;
        let filters = self.hooks.filters_text();
        let culls = self.hooks.culls();
        if filters || culls {
            let g = self.top();
            let ctm = g.pending.ctm.concat(g.sent.ctm).concat(self.transform);
            let font = &fd.font;
            let gid = self.tos.gid;
            let bbox = if fd.wmode == 0 {
                Rect::new(
                    0.0,
                    font.descender,
                    advance_glyph(font, gid, 0),
                    font.ascender,
                )
            } else {
                Rect::new(font.bbox.x0, 0.0, font.bbox.x1, advance_glyph(font, gid, 1))
            };
            let (render, ca, stroke_ca) = (g.pending.text.render, g.ca, g.stroke_ca);
            if filters {
                remove = self
                    .hooks
                    .text_filter(&ucs, trm, ctm, bbox, render, ca, stroke_ca);
            }
            if culls && !remove {
                let r = bbox.transform(trm.concat(ctm));
                remove = self.hooks.cull(r, Cull::Glyph);
            }
        }
        self.move_after_char();
        (remove, ucs[0])
    }

    fn filter_show_space(&mut self, tadj: f32, fd: &FontDesc) {
        let scale = self.top().pending.text.scale;
        self.tos.tm = if fd.wmode == 0 {
            self.tos.tm.pre_translate(tadj * scale, 0.0)
        } else {
            self.tos.tm.pre_translate(0.0, tadj)
        };
    }

    /// `mcid_char_imp` for every pending and current tag.
    fn mcid_char(&mut self, uni: i32, remove: bool) {
        let doc = self.doc;
        let n_pending = self.pending_tags.len();
        let n_current = self.current_tags.len();
        let order = (0..n_pending)
            .rev()
            .map(|i| (true, i))
            .chain((0..n_current).rev().map(|i| (false, i)));
        for (pending, i) in order.collect::<Vec<_>>() {
            let tr = if pending {
                &mut self.pending_tags[i]
            } else {
                &mut self.current_tags[i]
            };
            mcid_char_imp(doc, tr, uni, remove);
        }
    }

    /// `update_mcid`: the edited strings written back. MuPDF writes the
    /// edited ActualText to /Alt too.
    fn update_mcid(&mut self) {
        let Some(tag) = self.current_tags.last() else {
            return;
        };
        let Some(obj) = tag.mcid_obj.clone() else {
            return;
        };
        let text_of = |e: &Editable| -> String {
            e.text
                .as_ref()
                .map_or_else(String::new, |t| t.iter().collect())
        };
        if tag.alt.edited {
            let v = text_of(&tag.alt);
            edit_dict(self.doc, &obj, |d| {
                d.put(b"Alt", crate::edit::text_string(&v))
            });
        }
        if tag.actualtext.edited {
            let v = text_of(&tag.actualtext);
            edit_dict(self.doc, &obj, |d| {
                d.put(b"Alt", crate::edit::text_string(&v))
            });
        }
    }

    /// `filter_string_to_segment`: from `*pos`, the glyphs kept, up to the
    /// first one dropped; `inc` is that glyph's length (0 at the end).
    fn string_to_segment(&mut self, fd: &FontDesc, buf: &[u8], pos: &mut usize) -> usize {
        let mut at = *pos;
        let mut inc = 0;
        while at < buf.len() {
            let (cpt, w) = fd.encoding.decode(&buf[at..]);
            inc = w.max(1);
            at += inc;
            let cid = fd.encoding.lookup(cpt);
            let (remove, uni) = if cid < 0 {
                self.doc
                    .diagnostics()
                    .warn("run.encode_character", "cannot encode character");
                (false, crate::glyphs::REPLACEMENT as i32)
            } else {
                self.filter_show_char(fd, cid)
            };
            if cpt != 32 {
                if remove {
                    self.text_removed = true;
                } else {
                    self.text_sent = true;
                }
            }
            if cpt == 32 && inc == 1 {
                let ws = self.top().pending.text.word_space;
                self.filter_show_space(ws, fd);
            }
            self.mcid_char(uni, remove);
            if remove {
                return inc;
            }
            *pos += inc;
            inc = 0;
        }
        inc
    }

    /// `filter_string_in_segments`: each run of kept glyphs sent, with the
    /// text matrix as it was before the run. Returns `first`, cleared once
    /// something was sent.
    fn string_in_segments(
        &mut self,
        fd: &FontDesc,
        buf: &[u8],
        mut array: Option<&mut Vec<Object>>,
        mut first: bool,
    ) -> bool {
        let mut j = 0;
        while j < buf.len() {
            let start = j;
            let initial_tm = self.tos.tm;
            let inc = self.string_to_segment(fd, buf, &mut j);
            if start != j {
                let new_tm = self.tos.tm;
                self.tos.tm = initial_tm;
                if first {
                    self.filter_flush(FLUSH_ALL);
                }
                first = false;
                let seg = &buf[start..j];
                match array.as_deref_mut() {
                    Some(arr) => self.segment_to_array(fd, seg, arr),
                    None => self.segment_to_chain(seg),
                }
                self.tos.tm = new_tm;
                self.sent_tm = self.tos.tm;
            }
            j += inc;
        }
        first
    }

    fn segment_to_chain(&mut self, seg: &[u8]) {
        self.filter_flush(FLUSH_CTM | FLUSH_TEXT | FLUSH_OP);
        self.emit(Op::Show {
            text: seg,
            c_string: seg,
        });
    }

    /// `segment_to_array`: the adjustment that moves from what was sent to
    /// where the segment starts, then the segment.
    fn segment_to_array(&mut self, fd: &FontDesc, seg: &[u8], arr: &mut Vec<Object>) {
        let tm = self.tos.tm;
        let x = tm.e - self.sent_tm.e;
        let y = tm.f - self.sent_tm.f;
        let mut adjust = 0.0;
        let det = tm.a.madd(tm.d, -(tm.b * tm.c));
        if !(-0.001..=0.001).contains(&det) {
            let scale = 1.0 / det;
            let (tx, ty) = untransform(tm, x, y, scale);
            adjust = if fd.wmode == 0 {
                tx * self.top().pending.text.scale
            } else {
                ty
            };
        }
        if adjust != 0.0 {
            let size = self.top().pending.text.size;
            arr.push(Object::Real(-adjust * 1000.0 / size));
        }
        arr.push(Object::String(Arc::from(seg)));
    }

    /// `filter_show_string`.
    fn filter_show_string(&mut self, buf: &[u8]) {
        let Some(fd) = self.top().pending.text.font.clone() else {
            return;
        };
        self.tos_font = Some(fd.clone());
        self.string_in_segments(&fd, buf, None, true);
    }

    /// `filter_show_text`: a `TJ` array, split around dropped glyphs.
    fn filter_show_text(&mut self, text: Option<&Object>) {
        let Some(fd) = self.top().pending.text.font.clone() else {
            return;
        };
        let doc = self.doc;
        let text = text.map_or(Object::Null, |t| doc.resolve(t));
        let items = match &text {
            Object::String(s) => {
                let s = s.clone();
                self.filter_show_string(&s);
                return;
            }
            Object::Array(items) => items.clone(),
            _ => return,
        };
        self.tos_font = Some(fd.clone());
        let mut new_arr = Vec::new();
        let mut first = true;
        for item in items.iter() {
            match doc.resolve(item) {
                Object::String(s) => {
                    first = self.string_in_segments(&fd, &s, Some(&mut new_arr), first);
                }
                other => {
                    let size = self.top().pending.text.size;
                    let tadj = -other.to_real() * size * 0.001;
                    self.filter_show_space(tadj, &fd);
                }
            }
        }
        if !new_arr.is_empty() {
            let arr = Object::Array(Arc::new(new_arr));
            self.emit(Op::ShowArray(Some(&arr)));
        }
    }

    // Paths.

    /// `cull_path`: the path written, segment by segment where a culler
    /// drops some. Returns true when nothing was written, so the painting
    /// operator goes too.
    fn cull_path(&mut self, kind: Cull, flush: u32) -> bool {
        let path = std::mem::take(&mut self.path);
        if !self.hooks.culls() {
            self.filter_flush(flush | FLUSH_OP);
            path.walk_full(&mut Replay {
                chain: &mut self.chain,
            });
            self.finish_clip(&path);
            return false;
        }
        let g = self.top();
        let ctm = g.pending.ctm.concat(g.sent.ctm).concat(self.transform);
        let stroke = matches!(kind, Cull::PathStroke | Cull::PathFillStroke).then(|| {
            let s = &g.pending.stroke;
            StrokeState {
                start_cap: s.linecap,
                dash_cap: s.linecap,
                end_cap: s.linecap,
                linejoin: s.linejoin,
                linewidth: s.linewidth,
                miterlimit: s.miterlimit,
                ..StrokeState::default()
            }
        });
        let kind = if g.clip_op != ClipOp::None {
            kind.clip()
        } else {
            kind
        };
        let mut seg = Segmenter {
            segments: Vec::new(),
            current: None,
        };
        path.walk_full(&mut seg);
        seg.end();
        let mut flush = flush | FLUSH_OP;
        let mut any_sent = false;
        for segment in seg.segments {
            let r = segment.bound(stroke.as_ref(), ctm);
            if !r.is_valid() || self.hooks.cull(r, kind) {
                continue;
            }
            if flush != 0 {
                self.filter_flush(flush);
                flush = 0;
            }
            segment.walk_full(&mut Replay {
                chain: &mut self.chain,
            });
            any_sent = true;
        }
        if any_sent {
            self.finish_clip(&path);
        }
        !any_sent
    }

    /// The pending `W` or `W*`, after the path that sets it.
    fn finish_clip(&mut self, path: &Path) {
        let op = self.top().clip_op;
        if op == ClipOp::None {
            return;
        }
        let r = path.bound(None, self.top().sent.ctm);
        let g = self.top_mut();
        g.clip_rect = g.clip_rect.intersect(r);
        g.clip_op = ClipOp::None;
        self.emit(if op == ClipOp::W {
            Op::Clip
        } else {
            Op::ClipEvenOdd
        });
    }

    fn paint(&mut self, kind: Cull, flush: u32, op: Op<'_>) {
        if self.clipped_away() {
            return;
        }
        if self.cull_path(kind, flush) {
            return;
        }
        self.emit(op);
    }

    // Text objects.

    fn filter_et(&mut self) {
        if self.clipped_away() {
            return;
        }
        if !self.bt_pending {
            self.filter_flush(FLUSH_OP);
            self.emit(Op::EndText);
            self.in_bt = false;
        }
        if self.top().pending.text.render & 4 != 0 && self.text_removed && !self.text_sent {
            // A clipping text object that lost all its glyphs would clip
            // nothing; clip everything instead.
            self.top_mut().clip_rect = Rect::EMPTY;
        }
        self.bt_pending = false;
    }

    // Colors.

    fn set_cs(&mut self, stroke: bool, name: &[u8], cs: Option<ColorSpace>) {
        if self.clipped_away() {
            return;
        }
        // pdf_process_CS: the device names are the device spaces; any other
        // name is a resource, which may name one too.
        let device = match name {
            b"DeviceGray" | b"DeviceRGB" | b"DeviceCMYK" => true,
            b"Pattern" => false,
            _ => self
                .state
                .lookup(self.doc, b"ColorSpace", name)
                .is_some_and(|o| {
                    crate::content_write::is_device_colorspace(self.doc, &self.doc.resolve(&o))
                }),
        };
        let g = self.top_mut();
        let (cs_slot, sc_slot) = if stroke {
            (&mut g.pending.big_cs, &mut g.pending.big_sc)
        } else {
            (&mut g.pending.cs, &mut g.pending.sc)
        };
        cs_slot.name = name.to_vec();
        cs_slot.cs = cs.clone();
        cs_slot.device = device;
        set_default_cs_values(sc_slot, name, cs.as_ref());
        self.copy_resource(b"ColorSpace", name);
    }

    fn set_color(&mut self, stroke: bool, color: &[f32]) {
        if self.clipped_away() {
            return;
        }
        let g = self.top_mut();
        let sc = if stroke {
            &mut g.pending.big_sc
        } else {
            &mut g.pending.sc
        };
        sc.name.clear();
        sc.pat = None;
        sc.shd = None;
        sc.n = color.len().min(MAX_COLORS);
        sc.c[..sc.n].copy_from_slice(&color[..sc.n]);
    }

    fn device_color(&mut self, stroke: bool, name: &[u8], cs: ColorSpace, color: &[f32]) {
        if self.clipped_away() {
            return;
        }
        self.set_cs(stroke, name, Some(cs));
        self.set_color(stroke, color);
    }

    // Images and forms.

    /// The page-space box of the unit square under the current ctm.
    fn unit_box(&self) -> Rect {
        let g = self.top();
        let ctm = g.pending.ctm.concat(g.sent.ctm).concat(self.transform);
        Rect::new(0.0, 0.0, 1.0, 1.0).transform(ctm)
    }

    fn inline_image(&mut self, img: BiImage, csname: &[u8]) -> Result<(), String> {
        if self.clipped_away() {
            return Ok(());
        }
        if self.hooks.culls() {
            let r = self.unit_box();
            if self.hooks.cull(r, Cull::Image) {
                return Ok(());
            }
        }
        self.filter_flush(FLUSH_ALL);
        let img = if self.hooks.filters_images() {
            let ctm = self.top().sent.ctm.concat(self.transform);
            let clip = self.top().clip_rect;
            match self
                .hooks
                .image_filter(self.doc, ctm, ShownImage::Inline(&img), clip)?
            {
                ImageAction::Keep => img,
                ImageAction::Remove | ImageAction::ReplaceWith(_) => return Ok(()),
                ImageAction::Replace(new) => new,
            }
        } else {
            img
        };
        self.copy_resource(b"ColorSpace", csname);
        self.chain.inline_image(&img, csname)
    }

    fn draw_image(&mut self, name: &[u8], image: &Object) -> Result<(), String> {
        if self.clipped_away() {
            return Ok(());
        }
        if self.hooks.culls() {
            let r = self.unit_box();
            if self.hooks.cull(r, Cull::Image) {
                return Ok(());
            }
        }
        self.filter_flush(FLUSH_ALL);
        let action = if self.hooks.filters_images() {
            let ctm = self.top().sent.ctm.concat(self.transform);
            let clip = self.top().clip_rect;
            self.hooks
                .image_filter(self.doc, ctm, ShownImage::XObject(image), clip)?
        } else {
            ImageAction::Keep
        };
        match action {
            ImageAction::Keep => {
                if self.options.instance_forms {
                    let obj = self
                        .state
                        .lookup(self.doc, b"XObject", name)
                        .unwrap_or(Object::Null);
                    let buf = self.create_resource_name(b"XObject", "Im")?;
                    self.add_resource(b"XObject", &buf, obj);
                    self.emit(Op::DrawImage { name: &buf, image });
                } else {
                    self.copy_resource(b"XObject", name);
                    self.emit(Op::DrawImage { name, image });
                }
            }
            ImageAction::Remove => {}
            ImageAction::Replace(_) => {
                return Err("argument error: an inline image cannot replace an XObject".into());
            }
            ImageAction::ReplaceWith(num) => {
                let buf = self.create_resource_name(b"XObject", "Im")?;
                let obj = Object::Ref(ObjRef::new(num, 0));
                self.add_resource(b"XObject", &buf, obj.clone());
                self.emit(Op::DrawImage {
                    name: &buf,
                    image: &obj,
                });
            }
        }
        Ok(())
    }

    fn draw_form(&mut self, name: &[u8], form: &Object) -> Result<(), String> {
        if self.clipped_away() {
            return Ok(());
        }
        self.filter_flush(FLUSH_ALL);
        if self.options.instance_forms {
            let buf = self.create_resource_name(b"XObject", "Fm")?;
            let transform = self.top().sent.ctm.concat(self.transform);
            let page_res = Object::Dict(Arc::new(self.new_resources().clone()));
            let opts = self.options;
            let cycle = self.cycle.clone();
            let new_xobj = filter_xobject_instance(
                self.doc,
                form,
                Some(&page_res),
                transform,
                &opts,
                &mut *self.hooks,
                &cycle,
            )?;
            self.add_resource(b"XObject", &buf, new_xobj.clone());
            self.emit(Op::DrawForm {
                name: &buf,
                form: &new_xobj,
            });
        } else {
            self.copy_resource(b"XObject", name);
            self.emit(Op::DrawForm { name, form });
        }
        Ok(())
    }

    // Marked content.

    fn begin_marked(&mut self, tag: &[u8], raw: Option<&Object>, cooked: Option<&Object>) {
        let doc = self.doc;
        let raw = match raw {
            Some(r @ Object::Dict(_)) => {
                Some(self.hooks.marked_content(r).unwrap_or_else(|| r.clone()))
            }
            r => r.cloned(),
        };
        let mut t = Tag {
            bdc: raw.is_some() || cooked.is_some(),
            tag: tag.to_vec(),
            raw,
            mcid_obj: None,
            alt: Editable::default(),
            actualtext: Editable::default(),
        };
        if t.bdc {
            let mcid = cooked.map_or(Object::Null, |c| doc.dict_get(c, b"MCID"));
            if mcid.is_number()
                && let Some(mcids) = &self.structarray
            {
                t.mcid_obj = crate::run::lookup_mcid_in_mcids(doc, mcid.to_int(), mcids);
                if let Some(obj) = &t.mcid_obj {
                    let text = |key: &[u8]| match doc.dict_get(obj, key) {
                        Object::String(s) => Some(
                            String::from_utf8_lossy(&crate::text_string::utf8_from_pdf_string(&s))
                                .chars()
                                .collect(),
                        ),
                        _ => None,
                    };
                    t.alt.text = text(b"Alt");
                    t.alt.pos = Some(0);
                    t.actualtext.text = text(b"ActualText");
                    t.actualtext.pos = Some(0);
                }
            }
        }
        self.pending_tags.push(t);
    }

    fn end_marked(&mut self) {
        if !self.pending_tags.is_empty() {
            self.pending_tags.pop();
        } else if !self.current_tags.is_empty() {
            self.update_mcid();
            let name = self
                .current_tags
                .last()
                .and_then(|t| t.raw.as_ref())
                .and_then(|r| r.as_name().map(<[u8]>::to_vec));
            if let Some(name) = name {
                self.copy_resource(b"Properties", &name);
            }
            self.current_tags.pop();
            self.emit(Op::EndMarked);
        }
    }

    /// `pdf_filter_Tf`.
    fn set_font(&mut self, name: &[u8], font: Option<&Object>, size: f32) -> Result<(), String> {
        if self.clipped_away() {
            return Ok(());
        }
        self.filter_flush(FLUSH_OP);
        let fd = crate::run::load_font_for(self.doc, &mut self.state, font)?;
        let t = &mut self.top_mut().pending.text;
        t.fontname = Some(Rc::new(name.to_vec()));
        t.font = Some(fd);
        t.size = size;
        Ok(())
    }
}

/// `mcid_char_imp`.
fn mcid_char_imp(doc: &Document, tr: &mut Tag, uni: i32, remove: bool) {
    let Some(obj) = tr.mcid_obj.clone() else {
        return;
    };
    if remove {
        edit_dict(doc, &obj, |d| {
            d.remove(b"E");
            d.remove(b"T");
        });
    }
    walk_string(uni, remove, &mut tr.alt);
    walk_string(uni, remove, &mut tr.actualtext);
    let lost = |e: &Editable| e.text.is_some() && e.pos.is_none();
    // A string MuPDF never read has a position of 0, not -1.
    let remove = if remove {
        lost(&tr.alt) || lost(&tr.actualtext)
    } else {
        false
    };
    if remove {
        let (alt_lost, at_lost) = (lost(&tr.alt), lost(&tr.actualtext));
        edit_dict(doc, &obj, |d| {
            if at_lost {
                d.remove(b"ActualText");
            }
            if alt_lost {
                d.remove(b"Alt");
            }
        });
        tr.mcid_obj = None;
        tr.alt.text = None;
        tr.actualtext.text = None;
    }
}

/// A dictionary changed in place (`pdf_dict_del` and kin): an indirect one
/// in the document's edits. A direct one is left alone.
fn edit_dict(doc: &Document, obj: &Object, f: impl FnOnce(&mut Dict)) {
    let Some(r) = obj.as_ref() else {
        return;
    };
    if let Object::Dict(d) = doc.resolve(obj) {
        let mut d = (*d).clone();
        f(&mut d);
        doc.alter_object(r.num, Object::Dict(Arc::new(d)));
    }
}

/// `cull_replay_walker`: path segments written as operators.
struct Replay<'w, 'd> {
    chain: &'w mut Writer<'d>,
}

impl FullWalker for Replay<'_, '_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.chain.op(&Op::MoveTo(x, y));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.chain.op(&Op::LineTo(x, y));
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) {
        self.chain.op(&Op::CurveTo([x1, y1, x2, y2, x3, y3]));
    }
    fn close_path(&mut self) {
        self.chain.op(&Op::ClosePath);
    }
    fn curve_to_v(&mut self, x2: f32, y2: f32, x3: f32, y3: f32) {
        self.chain.op(&Op::CurveToV([x2, y2, x3, y3]));
    }
    fn curve_to_y(&mut self, x1: f32, y1: f32, x3: f32, y3: f32) {
        self.chain.op(&Op::CurveToY([x1, y1, x3, y3]));
    }
    fn rect_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32) {
        self.chain.op(&Op::Rect([x1, y1, x2 - x1, y2 - y1]));
    }
}

/// `cull_segmenter`: the path cut into subpaths, a new one at each move
/// and each rectangle.
struct Segmenter {
    segments: Vec<Path>,
    current: Option<Path>,
}

impl Segmenter {
    fn end(&mut self) {
        if let Some(p) = self.current.take() {
            self.segments.push(p);
        }
    }

    fn cur(&mut self) -> &mut Path {
        self.current.get_or_insert_with(Path::new)
    }
}

impl FullWalker for Segmenter {
    fn move_to(&mut self, x: f32, y: f32) {
        self.end();
        let mut p = Path::new();
        p.move_to(x, y);
        self.current = Some(p);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.cur().line_to(x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) {
        self.cur().curve_to(x1, y1, x2, y2, x3, y3);
    }
    fn close_path(&mut self) {
        self.cur().close_path();
    }
    fn curve_to_v(&mut self, x2: f32, y2: f32, x3: f32, y3: f32) {
        self.cur().curve_to_v(x2, y2, x3, y3);
    }
    fn curve_to_y(&mut self, x1: f32, y1: f32, x3: f32, y3: f32) {
        self.cur().curve_to_y(x1, y1, x3, y3);
    }
    fn rect_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32) {
        self.end();
        let mut p = Path::new();
        p.rect_to(x1, y1, x2, y2);
        self.current = Some(p);
    }
}

impl Processor for Sanitize<'_, '_> {
    fn op(&mut self, op: Op<'_>) -> Result<(), String> {
        match op {
            Op::LineWidth(w) => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_OP);
                    self.top_mut().pending.stroke.linewidth = w;
                }
            }
            Op::LineJoin(j) => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_OP);
                    self.top_mut().pending.stroke.linejoin = j;
                }
            }
            Op::LineCap(c) => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_OP);
                    self.top_mut().pending.stroke.linecap = c;
                }
            }
            Op::MiterLimit(m) => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_OP);
                    self.top_mut().pending.stroke.miterlimit = m;
                }
            }
            Op::Dash { array, phase } => {
                if !self.clipped_away() {
                    let s = &mut self.top_mut().pending.stroke;
                    s.dash = array.cloned();
                    s.phase = phase;
                }
            }
            Op::Intent(_)
            | Op::Flatness(_)
            | Op::GsBlendMode(_)
            | Op::GsSoftMask(_)
            | Op::GsOverprintStroke(_)
            | Op::GsOverprintFill(_)
            | Op::GsOverprintMode(_)
            | Op::GsBlackPointCompensation(_)
            | Op::Type3Width(..)
            | Op::Type3Bbox(_)
            | Op::MarkedPoint(_)
            | Op::MarkedPointProperties { .. } => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_OP);
                    self.emit(op);
                }
            }
            Op::GsBegin { name, .. } => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_ALL);
                    self.emit(op);
                    self.copy_resource(b"ExtGState", name);
                }
            }
            Op::GsEnd => {
                if !self.clipped_away() {
                    self.emit(op);
                }
            }
            Op::GsStrokeAlpha(a) => {
                self.filter_flush(FLUSH_OP);
                self.top_mut().stroke_ca = a;
            }
            Op::GsFillAlpha(a) => {
                self.filter_flush(FLUSH_OP);
                self.top_mut().ca = a;
            }
            Op::Save => self.filter_push(),
            Op::Restore => {
                self.filter_flush(FLUSH_OP | FLUSH_TEXT);
                if self.in_bt {
                    self.filter_et();
                }
                self.filter_pop();
            }
            Op::Transform(m) => {
                if !self.clipped_away() && m != [1.0, 0.0, 0.0, 1.0, 0.0, 0.0] {
                    let m = Matrix {
                        a: m[0],
                        b: m[1],
                        c: m[2],
                        d: m[3],
                        e: m[4],
                        f: m[5],
                    };
                    let g = self.top_mut();
                    g.pending.ctm = m.concat(g.pending.ctm);
                }
            }
            Op::MoveTo(x, y) => {
                if !self.clipped_away() {
                    self.path.move_to(x, y);
                }
            }
            Op::LineTo(x, y) => {
                if !self.clipped_away() {
                    self.path.line_to(x, y);
                }
            }
            Op::CurveTo(c) => {
                if !self.clipped_away() {
                    self.path.curve_to(c[0], c[1], c[2], c[3], c[4], c[5]);
                }
            }
            Op::CurveToV(c) => {
                if !self.clipped_away() {
                    self.path.curve_to_v(c[0], c[1], c[2], c[3]);
                }
            }
            Op::CurveToY(c) => {
                if !self.clipped_away() {
                    self.path.curve_to_y(c[0], c[1], c[2], c[3]);
                }
            }
            Op::ClosePath => {
                if !self.clipped_away() {
                    self.path.close_path();
                }
            }
            Op::Rect(r) => {
                if !self.clipped_away() {
                    self.path.rect_to(r[0], r[1], r[0] + r[2], r[1] + r[3]);
                }
            }
            Op::Stroke | Op::CloseStroke => self.paint(Cull::PathStroke, FLUSH_STROKE, op),
            Op::FillCompat | Op::Fill | Op::FillEvenOdd => {
                self.paint(Cull::PathFill, FLUSH_FILL, op)
            }
            Op::FillStroke
            | Op::FillStrokeEvenOdd
            | Op::CloseFillStroke
            | Op::CloseFillStrokeEvenOdd => {
                self.paint(Cull::PathFillStroke, FLUSH_ALL | FLUSH_STROKE_STATE, op)
            }
            Op::EndPath => {
                let flush = if self.top().clip_op == ClipOp::None {
                    FLUSH_OP
                } else {
                    FLUSH_CLIP
                };
                self.paint(Cull::PathDrop, flush, op);
            }
            Op::Clip => {
                if !self.clipped_away() {
                    self.top_mut().clip_op = ClipOp::W;
                }
            }
            Op::ClipEvenOdd => {
                if !self.clipped_away() {
                    self.top_mut().clip_op = ClipOp::WStar;
                }
            }
            Op::BeginText => {
                if !self.clipped_away() {
                    self.tos.tm = Matrix::IDENTITY;
                    self.tos.tlm = Matrix::IDENTITY;
                    self.sent_tm = Matrix::IDENTITY;
                    self.sent_tlm = Matrix::IDENTITY;
                    self.bt_pending = true;
                    self.text_sent = false;
                    self.text_removed = false;
                }
            }
            Op::EndText => self.filter_et(),
            Op::CharSpacing(v) => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_OP);
                    self.top_mut().pending.text.char_space = v;
                }
            }
            Op::WordSpacing(v) => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_OP);
                    self.top_mut().pending.text.word_space = v;
                }
            }
            Op::HorizontalScaling(v) => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_OP);
                    self.top_mut().pending.text.scale = v / 100.0;
                }
            }
            Op::Leading(v) => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_OP);
                    let g = self.top_mut();
                    g.leading = v;
                    g.pending.text.leading = v;
                }
            }
            Op::Font { name, font, size } => self.set_font(name, font, size)?,
            Op::RenderMode(r) => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_OP);
                    self.top_mut().pending.text.render = r;
                }
            }
            Op::Rise(v) => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_OP);
                    self.top_mut().pending.text.rise = v;
                }
            }
            Op::TextMove(tx, ty) => {
                if !self.clipped_away() {
                    self.tos.tlm = self.tos.tlm.pre_translate(tx, ty);
                    self.tos.tm = self.tos.tlm;
                }
            }
            Op::TextMoveSetLeading(tx, ty) => {
                if !self.clipped_away() {
                    self.top_mut().leading = -ty;
                    self.tos.tlm = self.tos.tlm.pre_translate(tx, ty);
                    self.tos.tm = self.tos.tlm;
                }
            }
            Op::TextMatrix(m) => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_OP);
                    let m = Matrix {
                        a: m[0],
                        b: m[1],
                        c: m[2],
                        d: m[3],
                        e: m[4],
                        f: m[5],
                    };
                    self.tos.tm = m;
                    self.tos.tlm = m;
                }
            }
            Op::NextLine => {
                if !self.clipped_away() {
                    self.filter_flush(FLUSH_OP);
                    let leading = self.top().leading;
                    self.tos.tlm = self.tos.tlm.pre_translate(0.0, -leading);
                    self.tos.tm = self.tos.tlm;
                }
            }
            Op::ShowArray(array) => {
                if !self.clipped_away() {
                    self.filter_show_text(array);
                }
            }
            Op::Show { text, .. } => {
                if !self.clipped_away() {
                    self.filter_show_string(text);
                }
            }
            Op::NextLineShow { text, .. } => {
                if !self.clipped_away() {
                    let leading = self.top().pending.text.leading;
                    self.tos.tlm = self.tos.tlm.pre_translate(0.0, -leading);
                    self.tos.tm = self.tos.tlm;
                    self.filter_show_string(text);
                }
            }
            Op::NextLineShowSpaced {
                word_spacing,
                char_spacing,
                text,
                ..
            } => {
                if !self.clipped_away() {
                    let t = &mut self.top_mut().pending.text;
                    t.word_space = word_spacing;
                    t.char_space = char_spacing;
                    let leading = t.leading;
                    self.tos.tlm = self.tos.tlm.pre_translate(0.0, -leading);
                    self.tos.tm = self.tos.tlm;
                    self.filter_show_string(text);
                }
            }
            Op::ColorSpace {
                stroke,
                name,
                colorspace,
            } => self.set_cs(stroke, name, colorspace),
            Op::ColorPattern {
                stroke,
                name,
                pattern,
                color,
            } => {
                if !self.clipped_away() {
                    let g = self.top_mut();
                    let sc = if stroke {
                        &mut g.pending.big_sc
                    } else {
                        &mut g.pending.sc
                    };
                    sc.name = name.to_vec();
                    sc.pat = Some(pattern.clone());
                    sc.shd = None;
                    sc.n = color.len().min(MAX_COLORS);
                    sc.c[..sc.n].copy_from_slice(&color[..sc.n]);
                    self.copy_resource(b"Pattern", name);
                }
            }
            Op::ColorShade {
                stroke,
                name,
                shading,
                shade,
            } => {
                if !self.clipped_away() {
                    let g = self.top_mut();
                    let sc = if stroke {
                        &mut g.pending.big_sc
                    } else {
                        &mut g.pending.sc
                    };
                    sc.name = name.to_vec();
                    sc.pat = None;
                    sc.shd = Some((shading.clone(), shade.clone()));
                    sc.n = 0;
                    self.copy_resource(b"Pattern", name);
                }
            }
            Op::Color { stroke, color } => self.set_color(stroke, color),
            Op::Gray { stroke, gray } => {
                self.device_color(stroke, b"DeviceGray", ColorSpace::Gray, &[gray])
            }
            Op::Rgb { stroke, rgb } => {
                self.device_color(stroke, b"DeviceRGB", ColorSpace::Rgb, &rgb)
            }
            Op::Cmyk { stroke, cmyk } => {
                self.device_color(stroke, b"DeviceCMYK", ColorSpace::Cmyk, &cmyk)
            }
            Op::InlineImage {
                image,
                colorspace_name,
            } => {
                if !self.clipped_away() {
                    let doc = self.doc;
                    let state = &self.state;
                    let lookup = |n: &[u8]| state.lookup(doc, b"ColorSpace", n);
                    let img = bi_image(doc, image, &lookup)?;
                    self.inline_image(img, colorspace_name.unwrap_or_default())?;
                }
            }
            Op::Shade {
                name,
                shading,
                shade,
            } => {
                if !self.clipped_away() {
                    if self.hooks.culls() {
                        let g = self.top();
                        let ctm = g.pending.ctm.concat(g.sent.ctm).concat(self.transform);
                        let r = shade.bound(ctm).intersect(g.clip_rect);
                        if self.hooks.cull(r, Cull::Shading) {
                            return Ok(());
                        }
                    }
                    self.filter_flush(FLUSH_ALL);
                    self.emit(Op::Shade {
                        name,
                        shading,
                        shade,
                    });
                    self.copy_resource(b"Shading", name);
                }
            }
            Op::DrawImage { name, image } => {
                crate::content_clean::load_image(self.doc, &mut self.state, image)?;
                self.draw_image(name, image)?
            }
            Op::DrawForm { name, form } => self.draw_form(name, form)?,
            Op::BeginMarked(tag) => self.begin_marked(tag, None, None),
            Op::BeginMarkedProperties { tag, raw, cooked } => {
                // A BDC is a BDC even with no properties found.
                let raw = raw.or(Some(&Object::Null));
                self.begin_marked(tag, raw, cooked)
            }
            Op::EndMarked => self.end_marked(),
            Op::BeginCompatibility | Op::EndCompatibility => {
                self.filter_flush(FLUSH_OP);
                self.emit(op);
            }
            Op::EndOfData => {}
            Op::End => self.filter_flush(FLUSH_OP | FLUSH_TEXT),
        }
        Ok(())
    }

    fn state(&mut self) -> &mut ProcState {
        &mut self.state
    }
}
