// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-interpret.c, source/pdf/pdf-image.c
// (pdf_load_image_imp for inline images), source/pdf/pdf-stream.c
// (pdf_open_contents_stream, pdf_open_object_array,
// pdf_load_compressed_inline_image), source/fitz/filter-basic.c
// (fz_open_concat), source/fitz/filter-leech.c and source/fitz/image.c
// (fz_decomp_image_from_stream), Copyright (C) 2004-2026 Artifex Software,
// Inc.

//! Content streams, as MuPDF's interpreter reads them.
//!
//! A page's contents are read as one stream (the parts of a contents array
//! joined by spaces), lexed, and each operator is handed to a [`Processor`]
//! with its operands, the way `pdf_process_stream` and
//! `pdf_process_keyword` do it:
//!
//! - Operands live where MuPDF keeps them: up to 32 numbers, one name, one
//!   short string (256 bytes) and one object (an array, a dictionary, a
//!   second name or a longer string). An operator takes the numbers it
//!   needs from the bottom of the stack, missing ones read as 0, and the
//!   stack is cleared after it.
//! - Inside `BT`/`ET`, an array is a `TJ` array: numbers and strings only,
//!   with `Tw` and `Tc` allowed inside it after a number.
//! - A syntax error is reported and lexing carries on after it; after 100
//!   the rest of the page is ignored. Other errors end the page.
//! - An inline image is decoded to find where its data ends: the decoders
//!   take from the stream exactly what they read, through a leech, and
//!   `EI` is searched for after that.
//! - Unbalanced `q` are closed at the end.
//!
//! Resource lookups that fail are errors, as in MuPDF, and so are
//! colorspaces and shadings that do not load: they are loaded here, as
//! `pdf_process_keyword` loads them. Fonts, patterns, images and soft
//! masks are handed over as objects for the processor to load.
//!
//! The processor owns the resource stack ([`ProcState`]), so that a form
//! run from inside a page finds what its own resources lack in the page's,
//! and optional content is evaluated for the processor's usage: a `Do` of
//! a hidden XObject is dropped, and marked content inside a hidden group
//! counts as hidden.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::colorspace::{self, ColorSpace};
use crate::diag::{self, Diagnostics, report};
use crate::document::Document;
use crate::image::Image;
use crate::layer;
use crate::lex::{Lexer, RawToken};
use crate::object::{Dict, Object};
use crate::parse::{parse_array, parse_dict};
use crate::shade::{self, Shade};
use crate::stream::{Next, Source, Stream};

/// MuPDF's MAX_SYNTAX_ERRORS.
pub const MAX_SYNTAX_ERRORS: usize = 100;
/// `pdf_csi`'s operand stack.
const STACK_SIZE: usize = 32;
/// `pdf_csi`'s name and string buffers.
const NAME_SIZE: usize = 256;
const STRING_SIZE: usize = 256;

/// A text operator's string, held apart from the processor while the
/// operator runs.
#[expect(
    clippy::large_enum_variant,
    reason = "a short-lived copy on the stack; boxing it would allocate for every Tj"
)]
enum TextOperand {
    /// The short-string buffer, its length, and where a NUL ends it.
    Short([u8; STRING_SIZE], usize, usize),
    Long(Option<Arc<[u8]>>),
}

impl TextOperand {
    fn text(&self) -> &[u8] {
        match self {
            TextOperand::Short(b, len, _) => &b[..*len],
            TextOperand::Long(s) => s.as_deref().unwrap_or_default(),
        }
    }

    /// The string read as a C string.
    fn c_string(&self) -> &[u8] {
        match self {
            TextOperand::Short(b, _, end) => &b[..*end],
            TextOperand::Long(s) => s.as_deref().unwrap_or_default(),
        }
    }
}
/// `parse_inline_image`'s colorspace name buffer.
const CSNAME_SIZE: usize = 40;

/// An inline image, as `pdf_load_inline_image` made it.
#[derive(Debug)]
pub struct InlineImage<'a> {
    pub dict: Dict,
    pub width: i32,
    pub height: i32,
    pub bpc: i32,
    pub image_mask: bool,
    pub colorspace: Option<ColorSpace>,
    /// Components per pixel: the colorspace's, or 1.
    pub n: usize,
    /// The (still encoded) bytes the decoders consumed.
    pub data: &'a [u8],
}

/// One operator, as `pdf_process_keyword` hands it to a processor. Names
/// are the bytes MuPDF keeps (up to a NUL); objects are as written,
/// unresolved.
#[derive(Debug)]
pub enum Op<'a> {
    LineWidth(f32),
    LineJoin(i32),
    LineCap(i32),
    MiterLimit(f32),
    Dash {
        array: Option<&'a Object>,
        phase: f32,
    },
    Intent(&'a [u8]),
    Flatness(f32),
    /// `gs`: the entries of the ExtGState follow, then `GsEnd`.
    GsBegin {
        name: &'a [u8],
        extgstate: &'a Object,
    },
    GsEnd,
    GsBlendMode(&'a [u8]),
    GsStrokeAlpha(f32),
    GsFillAlpha(f32),
    /// `/SMask`: the mask dictionary, or None for `/None`.
    GsSoftMask(Option<&'a Object>),
    GsOverprintStroke(bool),
    GsOverprintFill(bool),
    GsOverprintMode(i32),
    GsBlackPointCompensation(&'a [u8]),
    Save,
    Restore,
    Transform([f32; 6]),
    MoveTo(f32, f32),
    LineTo(f32, f32),
    CurveTo([f32; 6]),
    CurveToV([f32; 4]),
    CurveToY([f32; 4]),
    ClosePath,
    Rect([f32; 4]),
    Stroke,
    CloseStroke,
    FillCompat,
    Fill,
    FillEvenOdd,
    FillStroke,
    FillStrokeEvenOdd,
    CloseFillStroke,
    CloseFillStrokeEvenOdd,
    EndPath,
    Clip,
    ClipEvenOdd,
    BeginText,
    EndText,
    CharSpacing(f32),
    WordSpacing(f32),
    HorizontalScaling(f32),
    Leading(f32),
    /// `Tf`, or an ExtGState's /Font (named `ExtGState`); `font` is the
    /// font dictionary, or None where MuPDF falls back to its last-resort
    /// font.
    Font {
        name: &'a [u8],
        font: Option<&'a Object>,
        size: f32,
    },
    RenderMode(i32),
    Rise(f32),
    TextMove(f32, f32),
    TextMoveSetLeading(f32, f32),
    TextMatrix([f32; 6]),
    NextLine,
    ShowArray(Option<&'a Object>),
    /// `Tj`. `c_string` is the operand as code reading it as a C string
    /// sees it: a short string is kept in a 256-byte buffer that is never
    /// cleared, so read up to a NUL it runs on into what earlier, longer
    /// strings left there.
    Show {
        text: &'a [u8],
        c_string: &'a [u8],
    },
    NextLineShow {
        text: &'a [u8],
        c_string: &'a [u8],
    },
    NextLineShowSpaced {
        word_spacing: f32,
        char_spacing: f32,
        text: &'a [u8],
        c_string: &'a [u8],
    },
    Type3Width(f32, f32),
    Type3Bbox([f32; 6]),
    /// `CS`/`cs`: None for a pattern space.
    ColorSpace {
        stroke: bool,
        name: &'a [u8],
        colorspace: Option<ColorSpace>,
    },
    ColorPattern {
        stroke: bool,
        name: &'a [u8],
        pattern: &'a Object,
        color: &'a [f32],
    },
    ColorShade {
        stroke: bool,
        name: &'a [u8],
        shading: &'a Object,
        shade: &'a Arc<Shade>,
    },
    Color {
        stroke: bool,
        color: &'a [f32],
    },
    Gray {
        stroke: bool,
        gray: f32,
    },
    Rgb {
        stroke: bool,
        rgb: [f32; 3],
    },
    Cmyk {
        stroke: bool,
        cmyk: [f32; 4],
    },
    /// `BI`, with the /CS name when it is a direct name.
    InlineImage {
        image: &'a InlineImage<'a>,
        colorspace_name: Option<&'a [u8]>,
    },
    Shade {
        name: &'a [u8],
        shading: &'a Object,
        shade: &'a Arc<Shade>,
    },
    DrawImage {
        name: &'a [u8],
        image: &'a Object,
    },
    DrawForm {
        name: &'a [u8],
        form: &'a Object,
    },
    MarkedPoint(&'a [u8]),
    /// `DP`/`BDC`: `cooked` is the properties, a name looked up in the
    /// /Properties resources (None when it is not there).
    MarkedPointProperties {
        tag: &'a [u8],
        raw: Option<&'a Object>,
        cooked: Option<&'a Object>,
    },
    BeginMarked(&'a [u8]),
    BeginMarkedProperties {
        tag: &'a [u8],
        raw: Option<&'a Object>,
        cooked: Option<&'a Object>,
    },
    EndMarked,
    BeginCompatibility,
    EndCompatibility,
    /// The end of the data, before unbalanced `q` are closed.
    EndOfData,
    /// The end of processing.
    End,
}

/// What operators go to (`pdf_processor`). An error is a report line;
/// syntax and format errors are counted and lexing goes on, others end
/// the page.
pub trait Processor {
    fn op(&mut self, op: Op<'_>) -> Result<(), String>;

    /// The resource stack and the rest of the state the interpreter keeps
    /// on the processor.
    fn state(&mut self) -> &mut ProcState;

    /// The usage optional content is evaluated for ("View"), or None to
    /// show everything.
    fn usage(&self) -> Option<&'static str> {
        None
    }

    /// Whether the processor takes `/SMask` from an ExtGState. MuPDF looks
    /// at the mask only for processors that do.
    fn wants_soft_mask(&self) -> bool {
        true
    }
}

/// What `pdf_processor` keeps across the streams it is handed: the
/// resource stack, the depth of hidden marked content, and the objects
/// loaded so far.
#[derive(Debug, Default)]
pub struct ProcState {
    /// Resource dictionaries, innermost last (`pdf_resource_stack`).
    rstack: Vec<Object>,
    /// `pdf_processor.hidden`: how deep inside hidden marked content.
    pub hidden: i32,
    pub store: Store,
}

impl ProcState {
    pub fn with_store(store: Store) -> ProcState {
        ProcState {
            rstack: Vec::new(),
            hidden: 0,
            store,
        }
    }

    /// `pdf_lookup_resource`: the innermost dictionary that has it.
    pub fn lookup(&self, doc: &Document, kind: &[u8], name: &[u8]) -> Option<Object> {
        self.rstack.iter().rev().find_map(|res| {
            let sub = doc.dict_get(res, kind);
            let found = sub.as_dict()?.get(name)?;
            (!matches!(found, Object::Null)).then(|| found.clone())
        })
    }

    /// `pdf_processor_push_resources`, without the processor's hook.
    pub fn push_resources(&mut self, res: Object) {
        self.rstack.push(res);
    }

    /// `pdf_processor_pop_resources`, without the processor's hook.
    pub fn pop_resources(&mut self) -> Option<Object> {
        self.rstack.pop()
    }

    /// The innermost resource dictionary (`proc->rstack->resources`).
    pub fn resources(&self) -> Option<&Object> {
        self.rstack.last()
    }

    /// `pdf_is_ocg_hidden` for this processor's resources.
    pub fn is_hidden(&mut self, doc: &Document, usage: Option<&str>, ocg: &Object) -> bool {
        let desc = self.store.ocg(doc);
        let lookup = |name: &[u8]| self.lookup(doc, b"Properties", name);
        desc.is_hidden(doc, usage, ocg, &lookup)
    }
}

/// Fonts the threads working on one document share, by object number.
pub type SharedFonts = Arc<std::sync::Mutex<HashMap<i32, Arc<crate::fontdesc::FontDesc>>>>;

/// What MuPDF's store keeps for a document while it is interpreted: each
/// object is loaded (and its warnings reported) once.
#[derive(Debug, Default)]
pub struct Store {
    /// By object number; a function-based shading is sampled 66,049
    /// times.
    shades: HashMap<i32, Result<Arc<Shade>, String>>,
    /// Color spaces loaded, by object number (`pdf_find_item`): an ICC
    /// profile is read and inflated once, not at every `cs`. Failures are
    /// not kept, as MuPDF keeps none.
    pub colorspaces: HashMap<i32, ColorSpace>,
    pub images: HashMap<i32, Result<Arc<Image>, String>>,
    pub patterns: HashMap<i32, Arc<crate::run::Pattern>>,
    /// The structure tree's repaired parents, by element number, once it
    /// has been checked (MuPDF writes them into the document).
    pub struct_parents: Option<HashMap<i32, Object>>,
    ocg: Option<Rc<layer::Descriptor>>,
    /// Fonts by object number, and fonts given directly, by value.
    pub fonts: HashMap<i32, Arc<crate::fontdesc::FontDesc>>,
    /// Fonts by object number that the threads working on one document
    /// share, so that each loads a font only if none has yet. Type 3
    /// fonts are not shared: what they load depends on the page.
    pub shared_fonts: Option<SharedFonts>,
    pub direct_fonts: Vec<(Object, Arc<crate::fontdesc::FontDesc>)>,
    /// `pdf_load_hail_mary_font`'s font, once loaded.
    pub hail_mary: Option<Arc<crate::fontdesc::FontDesc>>,
    /// Type 3 fonts whose glyphs are being prepared (`t3loading` while
    /// in the store), by their font object, and fonts whose glyphs are
    /// being drawn, by font.
    pub t3_loading: Vec<Object>,
    pub t3_running: Vec<usize>,
    /// Whether resource dictionaries, by object number, use blending
    /// (`pdf_resources_use_blending`), as far as kept.
    pub blending: crate::run::BlendMemos,
}

impl Store {
    /// An empty store that shares its fonts with the others given `fonts`.
    pub fn sharing_fonts(fonts: &SharedFonts) -> Store {
        Store {
            shared_fonts: Some(Arc::clone(fonts)),
            ..Store::default()
        }
    }

    /// `pdf_read_ocg`, once.
    pub fn ocg(&mut self, doc: &Document) -> Rc<layer::Descriptor> {
        self.ocg
            .get_or_insert_with(|| Rc::new(layer::Descriptor::read(doc)))
            .clone()
    }

    /// `pdf_load_shading`, once per object.
    pub fn shade(&mut self, doc: &Document, obj: &Object) -> Result<Arc<Shade>, String> {
        let Some(r) = obj.as_ref() else {
            return shade::load(doc, obj).map(Arc::new);
        };
        if let Some(done) = self.shades.get(&r.num) {
            return done.clone();
        }
        let loaded = shade::load(doc, obj).map(Arc::new);
        self.shades.insert(r.num, loaded.clone());
        loaded
    }
}

/// Up to the first NUL, as MuPDF's C strings hold names and keywords.
fn cstr(bytes: &[u8]) -> &[u8] {
    bytes.split(|&b| b == 0).next().unwrap_or_default()
}

fn recoverable(line: &str) -> bool {
    line.starts_with("syntax error: ") || line.starts_with("format error: ")
}

/// `pdf_process_contents` for a page or form: `contents` is its /Contents
/// as written (None when there is none), `resources` its resource
/// dictionary. Errors that end the page are returned as report lines.
pub fn process_contents(
    doc: &Document,
    resources: Option<&Object>,
    contents: Option<&Object>,
    proc: &mut dyn Processor,
) -> Result<(), String> {
    let Some(contents) = contents else {
        return Ok(());
    };
    if let Some(res) = resources {
        proc.state().rstack.push(res.clone());
    }
    let out = process_raw(doc, contents, proc);
    if resources.is_some() {
        proc.state().rstack.pop();
    }
    out
}

/// `pdf_process_glyph`: a Type 3 glyph's procedure, with the font's
/// resources. Syntax errors that escape become format errors.
pub fn process_glyph(
    doc: &Document,
    resources: Option<&Object>,
    data: &[u8],
    proc: &mut dyn Processor,
) -> Result<(), String> {
    if let Some(res) = resources {
        proc.state().rstack.push(res.clone());
    }
    let out = {
        let diag = doc.diagnostics();
        let mut csi = Csi::new(doc, proc);
        let mut lx = Lexer::small(data, 0, diag);
        csi.process_stream(&mut lx).and_then(|_| csi.process_end())
    };
    if resources.is_some() {
        proc.state().rstack.pop();
    }
    out.map_err(|line| match line.strip_prefix("syntax error: ") {
        Some(rest) => report::format(rest),
        None => line,
    })
}

/// `pdf_process_raw_contents`: `contents` with the resources already on
/// the processor's stack.
pub fn process_raw_contents(
    doc: &Document,
    contents: &Object,
    proc: &mut dyn Processor,
) -> Result<(), String> {
    process_raw(doc, contents, proc)
}

/// How much of a content stream is decoded ahead of the interpreter. A
/// stream is never held whole: MuPDF reads one through a buffer of a few
/// kilobytes, and a stream that inflates to half a gigabyte of white space
/// costs it nothing. A stream shorter than this is in one window, and is
/// read as it always was.
const WINDOW: usize = 1 << 18;

/// `pdf_process_raw_contents`.
fn process_raw(doc: &Document, contents: &Object, proc: &mut dyn Processor) -> Result<(), String> {
    let diag = doc.diagnostics();
    if SKIP_UNMARKED.with(Cell::get) {
        // Read through once to see whether it is worth running. What that
        // reports is kept only if it is not: run, the stream is read again
        // and reports it then.
        let (marks, reported) = diag::capture(|| {
            Contents::open(doc, contents).and_then(|mut feed| may_mark(&mut feed, diag, WINDOW))
        });
        if !matches!(marks, Ok(true)) {
            diag.replay(reported);
            return marks.map(|_| ());
        }
    }
    let mut feed = Contents::open(doc, contents)?;
    process_feed(doc, &mut feed, proc, WINDOW).map(|_| ())
}

/// Runs the content `feed` gives through `proc`, at most about `want`
/// bytes of it decoded ahead at a time, and returns the most bytes that
/// were held at once.
///
/// The interpreter keeps its state from one window of bytes to the next.
/// A token that reaches the end of a window that more follows may not be
/// whole, so it is put back and lexed again with the next window; so are
/// an array, a dictionary and an inline image, which read on from the
/// lexer. What is run is what the whole stream in one buffer would run,
/// wherever the windows fall.
fn process_feed(
    doc: &Document,
    feed: &mut dyn Feed,
    proc: &mut dyn Processor,
    want: usize,
) -> Result<usize, String> {
    let diag = doc.diagnostics();
    let mut window = Window::new(feed, want);
    let mut csi = Csi::new(doc, proc);
    loop {
        window.fill()?;
        csi.last = !window.more;
        let mut lx = Lexer::small(&window.buf, 0, diag);
        match csi.process_stream(&mut lx)? {
            Lexed::End => break,
            Lexed::More => {
                let consumed = lx.pos();
                window.consumed(consumed);
            }
        }
    }
    csi.process_end()?;
    Ok(window.most)
}

/// `data` run through `proc` as a content stream that comes `step` bytes
/// at a time and is decoded `want` bytes ahead, with `resources`: what
/// [`process_contents`] runs for a stream of those bytes, whatever the two
/// sizes. For tests and the fuzzer, which hold it to that.
#[doc(hidden)]
pub fn process_in_windows(
    doc: &Document,
    resources: Option<&Object>,
    data: &[u8],
    step: usize,
    want: usize,
    proc: &mut dyn Processor,
) -> Result<usize, String> {
    if let Some(res) = resources {
        proc.state().rstack.push(res.clone());
    }
    let mut feed = Slices {
        data,
        step: step.max(1),
    };
    let out = process_feed(doc, &mut feed, proc, want);
    if resources.is_some() {
        proc.state().rstack.pop();
    }
    out
}

/// A feed that hands `data` out `step` bytes at a time.
struct Slices<'a> {
    data: &'a [u8],
    step: usize,
}

impl Feed for Slices<'_> {
    fn read(&mut self, out: &mut Vec<u8>) -> Result<bool, String> {
        let (now, later) = self.data.split_at(self.step.min(self.data.len()));
        out.extend_from_slice(now);
        self.data = later;
        Ok(!later.is_empty())
    }
}

/// Where a content stream's bytes come from, a piece at a time.
trait Feed {
    /// Adds the next bytes to `out`; false once there are no more.
    fn read(&mut self, out: &mut Vec<u8>) -> Result<bool, String>;
}

/// The bytes decoded ahead of whoever lexes them: from the start of a
/// token on, to about `want` bytes.
struct Window<'f> {
    feed: &'f mut dyn Feed,
    buf: Vec<u8>,
    /// Whether the feed may have more.
    more: bool,
    want: usize,
    /// Whether the bytes dropped so far end inside a comment.
    in_comment: bool,
    /// The most bytes held at once.
    most: usize,
}

impl<'f> Window<'f> {
    fn new(feed: &'f mut dyn Feed, want: usize) -> Self {
        Window {
            feed,
            buf: Vec::new(),
            more: true,
            want: want.max(1),
            in_comment: false,
            most: 0,
        }
    }

    /// Reads until `want` bytes are held or the feed ends. White space and
    /// comments before the first token are dropped as they come, as the
    /// lexer skips them: however long, they are never held.
    fn fill(&mut self) -> Result<(), String> {
        self.skip_blank();
        while self.more && self.buf.len() < self.want {
            self.more = self.feed.read(&mut self.buf)?;
            self.most = self.most.max(self.buf.len());
            self.skip_blank();
        }
        Ok(())
    }

    fn skip_blank(&mut self) {
        let mut at = 0;
        loop {
            if self.in_comment {
                // A comment ends with its line.
                match self.buf[at..]
                    .iter()
                    .position(|&b| b == b'\n' || b == b'\r')
                {
                    Some(end) => {
                        at += end + 1;
                        self.in_comment = false;
                    }
                    None => {
                        at = self.buf.len();
                        break;
                    }
                }
            } else {
                match self.buf.get(at) {
                    Some(&b) if crate::lex::is_white(b) => at += 1,
                    Some(b'%') => {
                        at += 1;
                        self.in_comment = true;
                    }
                    _ => break,
                }
            }
        }
        if at > 0 {
            self.buf.drain(..at);
        }
    }

    /// The first `n` bytes are done with. None of them when a full window
    /// does not hold the token at its start: the next is twice as long.
    fn consumed(&mut self, n: usize) {
        if n == 0 && self.buf.len() >= self.want {
            self.want = self.buf.len().saturating_mul(2);
        }
        self.buf.drain(..n.min(self.buf.len()));
    }
}

thread_local! {
    /// Set by [`skipping_unmarked`].
    static SKIP_UNMARKED: Cell<bool> = const { Cell::new(false) };
}

/// `f`, with contents that cannot show text, an image or a shading not
/// run at all: for whoever only asks what a page shows of those.
pub fn skipping_unmarked<R>(f: impl FnOnce() -> R) -> R {
    let before = SKIP_UNMARKED.with(|s| s.replace(true));
    let out = f();
    SKIP_UNMARKED.with(|s| s.set(before));
    out
}

/// Whether the content `feed` gives uses an operator that can show text,
/// an image or a shading, or run anything else: text, XObjects, inline
/// images, shadings, colors that can be patterns, graphics states (soft
/// masks run forms), marked content (its /ActualText is text), and fonts,
/// whose loading can repair the document. Lexed as the interpreter lexes,
/// a window at a time, every keyword is seen, the ones inside arrays and
/// dictionaries too.
fn may_mark(feed: &mut dyn Feed, diag: &Diagnostics, want: usize) -> Result<bool, String> {
    let mut window = Window::new(feed, want);
    let mut buf = Vec::new();
    // What a token put back had reported, not to be reported again.
    let mut reported = 0;
    loop {
        window.fill()?;
        let last = !window.more;
        let mut lx = Lexer::small(&window.buf, 0, diag);
        let consumed = loop {
            let start = lx.pos();
            let before = diag::warned();
            let tok = diag::swallowing(std::mem::take(&mut reported), || lx.next_raw(&mut buf));
            if !last && lx.pos() >= window.buf.len() {
                reported = diag::warned() - before;
                break start;
            }
            match tok {
                RawToken::Eof | RawToken::EndStream => return Ok(false),
                RawToken::Keyword => {
                    if matches!(
                        cstr(&buf),
                        b"Tj"
                            | b"TJ"
                            | b"'"
                            | b"\""
                            | b"Tf"
                            | b"Do"
                            | b"BI"
                            | b"sh"
                            | b"sc"
                            | b"SC"
                            | b"scn"
                            | b"SCN"
                            | b"gs"
                            | b"BDC"
                            | b"BMC"
                            | b"DP"
                            | b"MP"
                            | b"d0"
                            | b"d1"
                    ) {
                        return Ok(true);
                    }
                }
                _ => {}
            }
        };
        window.consumed(consumed);
    }
}

/// `pdf_open_contents_stream`, read as it is needed: a stream, or an
/// array of them, each read to its end before the next is opened.
struct Contents<'d> {
    doc: &'d Document,
    from: From<'d>,
}

enum From<'d> {
    /// One stream.
    Stream(crate::document::ContentStream<'d>),
    /// `pdf_open_object_array`: each part by number, joined with spaces. A
    /// part that does not open is skipped; one whose reading fails ends
    /// the stream there.
    Parts {
        parts: Arc<Vec<Object>>,
        next: usize,
        open: Option<crate::document::ContentStream<'d>>,
    },
    Nothing,
}

impl<'d> Contents<'d> {
    fn open(doc: &'d Document, contents: &Object) -> Result<Self, String> {
        let from = match doc.resolve(contents) {
            Object::Array(parts) => From::Parts {
                parts,
                next: 0,
                open: None,
            },
            Object::Stream(s) => From::Stream(doc.open_content_stream(&s).map_err(|e| e.report())?),
            _ => {
                let num = contents.as_ref().map_or(0, |r| r.num);
                doc.diagnostics().warn(
                    "content.not_stream",
                    format!("content stream is not a stream ({num} 0 R)"),
                );
                From::Nothing
            }
        };
        Ok(Contents { doc, from })
    }
}

impl Feed for Contents<'_> {
    fn read(&mut self, out: &mut Vec<u8>) -> Result<bool, String> {
        let doc = self.doc;
        let diag = doc.diagnostics();
        match &mut self.from {
            From::Nothing => Ok(false),
            From::Stream(s) => s.read_piece(out).map_err(|e| e.report()),
            From::Parts { parts, next, open } => {
                let unloadable = |line: String, part: usize| {
                    diag.warn("error.reported", line);
                    diag.warn(
                        "content.part_unloadable",
                        format!("cannot load content stream part {}/{}", part, parts.len()),
                    );
                };
                loop {
                    if let Some(part) = open {
                        match part.read_piece(out) {
                            Ok(true) => return Ok(true),
                            Ok(false) => {
                                let failed = part.had_error();
                                *open = None;
                                if failed {
                                    *next = parts.len();
                                    return Ok(false);
                                }
                                out.push(b' ');
                            }
                            Err(e) => {
                                *open = None;
                                unloadable(e.report(), *next);
                            }
                        }
                    }
                    let Some(part) = parts.get(*next) else {
                        return Ok(false);
                    };
                    *next += 1;
                    let num = part.as_ref().map_or(0, |r| r.num);
                    let opened = match doc.object(num) {
                        Some(Object::Stream(s)) => {
                            doc.open_content_stream(&s).map_err(|e| e.report())
                        }
                        _ => Err(report::format("object is not a stream")),
                    };
                    match opened {
                        Ok(part) => *open = Some(part),
                        Err(line) => unloadable(line, *next),
                    }
                }
            }
        }
    }
}

/// `pdf_csi`: the interpreter's state while it lexes one stream.
struct Csi<'d, 'p> {
    doc: &'d Document,
    proc: &'p mut dyn Processor,
    gstate: i32,
    xbalance: i32,
    in_text: bool,
    obj: Option<Object>,
    name: Vec<u8>,
    string: [u8; STRING_SIZE],
    string_len: usize,
    top: usize,
    stack: [f32; STACK_SIZE],
    /// Whether the bytes being lexed run to the end of the stream; where
    /// not, they are a window more follows ([`process_feed`]).
    last: bool,
    /// Inside a `TJ` array, and the syntax errors so far: kept here from
    /// one window to the next.
    in_text_array: bool,
    syntax_errors: usize,
    /// Where the token being handled starts, to go back to.
    token_start: usize,
    /// Set by an operator that could not read all it needs in a window.
    need_more: bool,
    /// How many diagnostics the token put back had reported, which it
    /// does not report again.
    reported: u64,
}

/// How lexing a run of bytes ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lexed {
    /// At the end of the stream.
    End,
    /// At a token the bytes do not hold whole, where the lexer was left:
    /// to be lexed again from there with more bytes.
    More,
}

impl<'d, 'p> Csi<'d, 'p> {
    fn new(doc: &'d Document, proc: &'p mut dyn Processor) -> Self {
        Csi {
            doc,
            proc,
            gstate: 0,
            xbalance: 0,
            in_text: false,
            obj: None,
            name: Vec::new(),
            string: [0; STRING_SIZE],
            string_len: 0,
            top: 0,
            stack: [0.0; STACK_SIZE],
            last: true,
            in_text_array: false,
            syntax_errors: 0,
            token_start: 0,
            need_more: false,
            reported: 0,
        }
    }

    /// `f`, which reads on from `lx`, done whole or not at all: None when
    /// it reached the end of a window that more follows, where what it
    /// read may have been cut short. The lexer is then back at `start`,
    /// and nothing `f` reported is kept.
    fn whole<'a, T>(
        &mut self,
        lx: &mut Lexer<'a>,
        start: usize,
        f: impl FnOnce(&mut Self, &mut Lexer<'a>) -> T,
    ) -> Option<T> {
        if self.last {
            return Some(f(self, lx));
        }
        let (out, reported) = diag::capture(|| f(self, lx));
        if lx.pos() >= lx.data().len() {
            lx.seek(start);
            return None;
        }
        self.diag().replay(reported);
        Some(out)
    }

    fn diag(&self) -> &'d Diagnostics {
        self.doc.diagnostics()
    }

    fn clear_stack(&mut self) {
        self.obj = None;
        self.name.clear();
        // The bytes stay in the buffer.
        self.string_len = 0;
        self.stack = [0.0; STACK_SIZE];
        self.top = 0;
    }

    /// `pdf_lookup_resource`: the innermost dictionary that has it.
    fn lookup(&mut self, kind: &[u8], name: &[u8]) -> Option<Object> {
        let doc = self.doc;
        self.proc.state().lookup(doc, kind, name)
    }

    /// `resolve_properties`: a name is looked up in /Properties.
    fn properties(&mut self, obj: Option<&Object>) -> Option<Object> {
        match obj? {
            Object::Name(n) => self.lookup(b"Properties", cstr(n)),
            o => Some(o.clone()),
        }
    }

    fn s(&self, i: usize) -> f32 {
        self.stack[i]
    }

    fn s6(&self) -> [f32; 6] {
        [
            self.s(0),
            self.s(1),
            self.s(2),
            self.s(3),
            self.s(4),
            self.s(5),
        ]
    }

    fn s4(&self) -> [f32; 4] {
        [self.s(0), self.s(1), self.s(2), self.s(3)]
    }

    fn emit(&mut self, op: Op<'_>) -> Result<(), String> {
        self.proc.op(op)
    }

    /// `pdf_process_stream`, over the stream whole or a window of it at a
    /// time: called again with the bytes from where the lexer was left
    /// for as long as it gives [`Lexed::More`].
    fn process_stream(&mut self, lx: &mut Lexer) -> Result<Lexed, String> {
        loop {
            let result = self.lex_ops(lx);
            if matches!(result, Ok(Lexed::More)) {
                // The operands so far wait for their operator.
                return Ok(Lexed::More);
            }
            self.clear_stack();
            match result {
                Ok(_) => break,
                Err(line) if recoverable(&line) => {
                    self.diag().warn("error.reported", line);
                    self.syntax_errors += 1;
                    if self.syntax_errors >= MAX_SYNTAX_ERRORS {
                        self.diag().warn(
                            "content.too_many_errors",
                            "too many syntax errors; ignoring rest of page",
                        );
                        break;
                    }
                    self.in_text_array = false;
                }
                Err(line) => return Err(line),
            }
        }
        if self.syntax_errors > 0 {
            self.diag().warn(
                "content.syntax_errors",
                "encountered syntax errors; page may not be correct",
            );
        }
        Ok(Lexed::End)
    }

    /// The inner loop of `pdf_process_stream`, until the end of the data
    /// (Ok) or an error.
    fn lex_ops(&mut self, lx: &mut Lexer) -> Result<Lexed, String> {
        // Names, strings and keywords are lexed into one buffer.
        let mut buf = Vec::new();
        loop {
            let start = lx.pos();
            let tok = if self.last && self.reported == 0 {
                lx.next_raw(&mut buf)
            } else {
                let before = diag::warned();
                let again = std::mem::take(&mut self.reported);
                let tok = diag::swallowing(again, || lx.next_raw(&mut buf));
                if !self.last && lx.pos() >= lx.data().len() {
                    // It reached the end of a window: it may go on.
                    self.reported = diag::warned() - before;
                    lx.seek(start);
                    return Ok(Lexed::More);
                }
                tok
            };
            self.token_start = start;
            if self.in_text_array {
                match tok {
                    RawToken::CloseArray => self.in_text_array = false,
                    RawToken::Real(f) => self.push_text_item(Object::Real(f)),
                    RawToken::Int(i) => self.push_text_item(Object::Int(i)),
                    RawToken::String => self.push_text_item(Object::String(Arc::from(&buf[..]))),
                    RawToken::Eof => return Ok(Lexed::End),
                    RawToken::Keyword if matches!(cstr(&buf), b"Tw" | b"Tc") => {
                        // A spacing operator inside a TJ array takes the
                        // number before it.
                        let last = match &self.obj {
                            Some(Object::Array(a)) => a.last().filter(|o| o.is_number()).cloned(),
                            _ => None,
                        };
                        let Some(last) = last else {
                            return Err(report::syntax("syntax error in array"));
                        };
                        self.stack[0] = last.to_real();
                        if let Some(Object::Array(a)) = &mut self.obj {
                            Arc::make_mut(a).pop();
                        }
                        self.keyword(lx, cstr(&buf))?;
                    }
                    _ => return Err(report::syntax("syntax error in array")),
                }
                continue;
            }
            match tok {
                RawToken::EndStream | RawToken::Eof => return Ok(Lexed::End),
                RawToken::OpenArray => {
                    self.obj = None;
                    if self.in_text {
                        self.in_text_array = true;
                        self.obj = Some(Object::Array(Arc::new(Vec::new())));
                    } else {
                        let Some(a) = self.whole(lx, start, |_, lx| parse_array(lx, 0)) else {
                            return Ok(Lexed::More);
                        };
                        let a = a.map_err(|e| report::syntax(&e.0))?;
                        self.obj = Some(Object::Array(Arc::new(a)));
                    }
                }
                RawToken::OpenDict => {
                    self.obj = None;
                    let Some(d) = self.whole(lx, start, |_, lx| parse_dict(lx, 0)) else {
                        return Ok(Lexed::More);
                    };
                    let d = d.map_err(|e| report::syntax(&e.0))?;
                    self.obj = Some(Object::Dict(Arc::new(d)));
                }
                RawToken::Name => {
                    let n = cstr(&buf);
                    if self.name.is_empty() {
                        self.name
                            .extend_from_slice(&n[..n.len().min(NAME_SIZE - 1)]);
                    } else {
                        self.obj = Some(Object::name(n));
                    }
                }
                RawToken::Int(i) => self.push_number(i as f32)?,
                RawToken::Real(f) => self.push_number(f)?,
                RawToken::String => {
                    if buf.len() <= STRING_SIZE {
                        self.string[..buf.len()].copy_from_slice(&buf);
                        self.string_len = buf.len();
                    } else {
                        self.obj = Some(Object::String(Arc::from(&buf[..])));
                    }
                }
                RawToken::Keyword => {
                    if crate::interrupt::interrupted() {
                        return Err(crate::interrupt::MESSAGE.to_string());
                    }
                    self.keyword(lx, cstr(&buf))?;
                    if std::mem::take(&mut self.need_more) {
                        // The operator is back at its start, its operands
                        // still on the stack.
                        return Ok(Lexed::More);
                    }
                    self.clear_stack();
                }
                _ => return Err(report::syntax("syntax error in content stream")),
            }
        }
    }

    fn push_text_item(&mut self, item: Object) {
        if let Some(Object::Array(a)) = &mut self.obj {
            Arc::make_mut(a).push(item);
        }
    }

    fn push_number(&mut self, v: f32) -> Result<(), String> {
        if self.top >= STACK_SIZE {
            return Err(report::syntax("stack overflow"));
        }
        self.stack[self.top] = v;
        self.top += 1;
        Ok(())
    }

    /// The string operand of `Tj`, `'` and `"`: the short string, or else
    /// the string in the object slot; and the same read as a C string.
    fn text(&self) -> TextOperand {
        if self.string_len > 0 {
            let end = self
                .string
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(STRING_SIZE);
            return TextOperand::Short(self.string, self.string_len, end);
        }
        match &self.obj {
            Some(Object::String(s)) => TextOperand::Long(Some(s.clone())),
            _ => TextOperand::Long(None),
        }
    }

    /// `pdf_process_keyword`.
    fn keyword(&mut self, lx: &mut Lexer, word: &[u8]) -> Result<(), String> {
        let name = self.name.clone();
        match word {
            b"w" => self.emit(Op::LineWidth(self.s(0))),
            b"j" => self.emit(Op::LineJoin(clampi(self.s(0), 0, 2))),
            b"J" => self.emit(Op::LineCap(clampi(self.s(0), 0, 2))),
            b"M" => self.emit(Op::MiterLimit(self.s(0))),
            b"d" => {
                let obj = self.obj.clone();
                self.emit(Op::Dash {
                    array: obj.as_ref(),
                    phase: self.s(0),
                })
            }
            b"ri" => self.emit(Op::Intent(&name)),
            b"i" => self.emit(Op::Flatness(self.s(0))),
            b"gs" => {
                let Some(gs) = self.lookup(b"ExtGState", &name) else {
                    return Err(report::syntax(&format!(
                        "cannot find ExtGState resource '{}'",
                        String::from_utf8_lossy(&name)
                    )));
                };
                self.emit(Op::GsBegin {
                    name: &name,
                    extgstate: &gs,
                })?;
                self.process_extgstate(&gs)?;
                self.emit(Op::GsEnd)
            }
            b"q" => {
                self.gstate += 1;
                self.emit(Op::Save)
            }
            b"Q" => {
                self.gstate -= 1;
                self.emit(Op::Restore)
            }
            b"cm" => self.emit(Op::Transform(self.s6())),
            b"m" => self.emit(Op::MoveTo(self.s(0), self.s(1))),
            b"l" => self.emit(Op::LineTo(self.s(0), self.s(1))),
            b"c" => self.emit(Op::CurveTo(self.s6())),
            b"v" => self.emit(Op::CurveToV(self.s4())),
            b"y" => self.emit(Op::CurveToY(self.s4())),
            b"h" => self.emit(Op::ClosePath),
            b"re" => self.emit(Op::Rect(self.s4())),
            b"S" => self.emit(Op::Stroke),
            b"s" => self.emit(Op::CloseStroke),
            b"F" => self.emit(Op::FillCompat),
            b"f" => self.emit(Op::Fill),
            b"f*" => self.emit(Op::FillEvenOdd),
            b"B" => self.emit(Op::FillStroke),
            b"B*" => self.emit(Op::FillStrokeEvenOdd),
            b"b" => self.emit(Op::CloseFillStroke),
            b"b*" => self.emit(Op::CloseFillStrokeEvenOdd),
            b"n" => self.emit(Op::EndPath),
            b"W" => self.emit(Op::Clip),
            b"W*" => self.emit(Op::ClipEvenOdd),
            b"BT" => {
                self.in_text = true;
                self.emit(Op::BeginText)
            }
            b"ET" => {
                self.in_text = false;
                self.emit(Op::EndText)
            }
            b"Tc" => self.emit(Op::CharSpacing(self.s(0))),
            b"Tw" => self.emit(Op::WordSpacing(self.s(0))),
            b"Tz" => self.emit(Op::HorizontalScaling(self.s(0))),
            b"TL" => self.emit(Op::Leading(self.s(0))),
            b"Tr" => self.emit(Op::RenderMode(self.s(0) as i32)),
            b"Ts" => self.emit(Op::Rise(self.s(0))),
            b"Tf" => {
                // pdf_is_dict: otherwise MuPDF's last-resort font.
                let font = self
                    .lookup(b"Font", &name)
                    .filter(|f| self.doc.resolve(f).as_dict().is_some());
                self.emit(Op::Font {
                    name: &name,
                    font: font.as_ref(),
                    size: self.s(0),
                })
            }
            b"Td" => self.emit(Op::TextMove(self.s(0), self.s(1))),
            b"TD" => self.emit(Op::TextMoveSetLeading(self.s(0), self.s(1))),
            b"Tm" => self.emit(Op::TextMatrix(self.s6())),
            b"T*" => self.emit(Op::NextLine),
            b"TJ" => {
                let obj = self.obj.clone();
                self.emit(Op::ShowArray(obj.as_ref()))
            }
            b"Tj" => {
                let t = self.text();
                self.emit(Op::Show {
                    text: t.text(),
                    c_string: t.c_string(),
                })
            }
            b"'" => {
                let t = self.text();
                self.emit(Op::NextLineShow {
                    text: t.text(),
                    c_string: t.c_string(),
                })
            }
            b"\"" => {
                let t = self.text();
                self.emit(Op::NextLineShowSpaced {
                    word_spacing: self.s(0),
                    char_spacing: self.s(1),
                    text: t.text(),
                    c_string: t.c_string(),
                })
            }
            b"d0" => self.emit(Op::Type3Width(self.s(0), self.s(1))),
            b"d1" => self.emit(Op::Type3Bbox(self.s6())),
            b"CS" => self.process_cs(true, &name),
            b"cs" => self.process_cs(false, &name),
            b"SC" | b"SCN" => self.process_sc(true, &name),
            b"sc" | b"scn" => self.process_sc(false, &name),
            b"G" => self.emit(Op::Gray {
                stroke: true,
                gray: self.s(0),
            }),
            b"g" => self.emit(Op::Gray {
                stroke: false,
                gray: self.s(0),
            }),
            b"RG" => self.emit(Op::Rgb {
                stroke: true,
                rgb: [self.s(0), self.s(1), self.s(2)],
            }),
            b"rg" => self.emit(Op::Rgb {
                stroke: false,
                rgb: [self.s(0), self.s(1), self.s(2)],
            }),
            b"K" => self.emit(Op::Cmyk {
                stroke: true,
                cmyk: self.s4(),
            }),
            b"k" => self.emit(Op::Cmyk {
                stroke: false,
                cmyk: self.s4(),
            }),
            b"BI" => {
                // The dictionary, the data and the `EI` after it are read
                // from the lexer's bytes: all of them, or none yet.
                let start = self.token_start;
                let read = self.whole(lx, start, |csi, lx| {
                    let (dict, csname) = csi.parse_inline_dict(lx)?;
                    let image = csi.load_inline_image(lx, dict)?;
                    find_ei(lx)?;
                    Ok::<_, String>((image, csname))
                });
                let Some(read) = read else {
                    self.need_more = true;
                    return Ok(());
                };
                let (image, csname) = read?;
                self.emit(Op::InlineImage {
                    image: &image,
                    colorspace_name: (!csname.is_empty()).then_some(&csname[..]),
                })
            }
            b"sh" => {
                let Some(shading) = self.lookup(b"Shading", &name) else {
                    return Err(report::syntax(&format!(
                        "cannot find Shading resource '{}'",
                        String::from_utf8_lossy(&name)
                    )));
                };
                let shade = self.load_shade(&shading)?;
                self.emit(Op::Shade {
                    name: &name,
                    shading: &shading,
                    shade: &shade,
                })
            }
            b"Do" => self.process_do(&name),
            b"MP" => self.emit(Op::MarkedPoint(&name)),
            b"DP" => {
                let obj = self.obj.clone();
                let cooked = self.properties(obj.as_ref());
                self.emit(Op::MarkedPointProperties {
                    tag: &name,
                    raw: obj.as_ref(),
                    cooked: cooked.as_ref(),
                })
            }
            b"BMC" => {
                self.emit(Op::BeginMarked(&name))?;
                let state = self.proc.state();
                if state.hidden > 0 {
                    state.hidden += 1;
                }
                Ok(())
            }
            b"BDC" => {
                let obj = self.obj.clone();
                let cooked = self.properties(obj.as_ref());
                self.emit(Op::BeginMarkedProperties {
                    tag: &name,
                    raw: obj.as_ref(),
                    cooked: cooked.as_ref(),
                })?;
                if self.proc.state().hidden > 0 {
                    self.proc.state().hidden += 1;
                } else if name == b"OC" {
                    let doc = self.doc;
                    let usage = self.proc.usage();
                    let ocg = obj.unwrap_or(Object::Null);
                    if self.proc.state().is_hidden(doc, usage, &ocg) {
                        self.proc.state().hidden += 1;
                    }
                }
                Ok(())
            }
            b"EMC" => {
                self.emit(Op::EndMarked)?;
                let state = self.proc.state();
                if state.hidden > 0 {
                    state.hidden -= 1;
                }
                Ok(())
            }
            b"BX" => {
                self.xbalance += 1;
                self.emit(Op::BeginCompatibility)
            }
            b"EX" => {
                self.xbalance -= 1;
                self.emit(Op::EndCompatibility)
            }
            _ => {
                if self.xbalance == 0 {
                    let message = format!("unknown keyword: '{}'", String::from_utf8_lossy(word));
                    if matches!(word, b"Infinity" | b"NaN" | b"inf" | b"nan") {
                        self.diag().warn("content.unknown_keyword", message);
                    } else {
                        return Err(report::syntax(&message));
                    }
                }
                Ok(())
            }
        }
    }

    /// `pdf_process_extgstate`, for the entries that need no loading.
    fn process_extgstate(&mut self, gs: &Object) -> Result<(), String> {
        let doc = self.doc;
        let get = |key: &[u8]| doc.dict_get(gs, key);
        let lw = get(b"LW");
        if lw.is_number() {
            self.emit(Op::LineWidth(lw.to_real()))?;
        }
        if let Object::Int(_) = get(b"LC") {
            self.emit(Op::LineCap(get(b"LC").to_int().clamp(0, 2)))?;
        }
        if let Object::Int(_) = get(b"LJ") {
            self.emit(Op::LineJoin(get(b"LJ").to_int().clamp(0, 2)))?;
        }
        let ml = get(b"ML");
        if ml.is_number() {
            self.emit(Op::MiterLimit(ml.to_real()))?;
        }
        if let Object::Array(d) = get(b"D") {
            let array = d.first().cloned();
            let phase = d.get(1).map_or(0.0, |p| doc.resolve(p).to_real());
            self.emit(Op::Dash {
                array: array.as_ref(),
                phase,
            })?;
        }
        if let Object::Name(ri) = get(b"RI") {
            self.emit(Op::Intent(&ri))?;
        }
        let fl = get(b"FL");
        if fl.is_number() {
            self.emit(Op::Flatness(fl.to_real()))?;
        }
        if let Object::Array(font) = get(b"Font") {
            let font_ref = font.first().cloned().unwrap_or(Object::Null);
            let size = font.get(1).map_or(0.0, |s| doc.resolve(s).to_real());
            let font_obj = doc
                .resolve(&font_ref)
                .as_dict()
                .is_some()
                .then_some(&font_ref);
            self.emit(Op::Font {
                name: b"ExtGState",
                font: font_obj,
                size,
            })?;
        }
        if let Object::Bool(b) = get(b"OP") {
            self.emit(Op::GsOverprintStroke(b))?;
        }
        if let Object::Bool(b) = get(b"op") {
            self.emit(Op::GsOverprintFill(b))?;
        }
        if let Object::Int(_) = get(b"OPM") {
            self.emit(Op::GsOverprintMode(get(b"OPM").to_int()))?;
        }
        if let Object::Name(n) = get(b"UseBlackPtComp") {
            self.emit(Op::GsBlackPointCompensation(&n))?;
        }
        let tr2 = get(b"TR2");
        if let Object::Name(n) = &tr2
            && !matches!(n.as_ref(), b"Identity" | b"Default")
        {
            self.diag()
                .warn("content.transfer_function", "ignoring transfer function");
        }
        // /TR counts only without /TR2.
        let has_tr2 = doc
            .resolve(gs)
            .as_dict()
            .and_then(|d| d.get(b"TR2"))
            .is_some_and(|v| !matches!(v, Object::Null));
        if !has_tr2
            && let Object::Name(n) = get(b"TR")
            && n.as_ref() != b"Identity"
        {
            self.diag()
                .warn("content.transfer_function", "ignoring transfer function");
        }
        let ca = get(b"CA");
        if ca.is_number() {
            self.emit(Op::GsStrokeAlpha(ca.to_real()))?;
        }
        let ca = get(b"ca");
        if ca.is_number() {
            self.emit(Op::GsFillAlpha(ca.to_real()))?;
        }
        let mut bm = get(b"BM");
        if let Object::Array(a) = &bm {
            bm = a.first().map_or(Object::Null, |b| doc.resolve(b));
        }
        if let Object::Name(n) = &bm {
            self.emit(Op::GsBlendMode(n))?;
        }
        if self.proc.wants_soft_mask() {
            match get(b"SMask") {
                Object::Name(n) if n.as_ref() == b"None" => self.emit(Op::GsSoftMask(None))?,
                mask if mask.as_dict().is_some() => self.emit(Op::GsSoftMask(Some(&mask)))?,
                _ => {}
            }
        }
        Ok(())
    }

    /// `pdf_process_CS`.
    fn process_cs(&mut self, stroke: bool, name: &[u8]) -> Result<(), String> {
        let pattern = |name: &'static [u8]| Op::ColorSpace {
            stroke,
            name,
            colorspace: None,
        };
        let cs = match name {
            b"Pattern" => return self.emit(pattern(b"Pattern")),
            b"DeviceGray" => ColorSpace::Gray,
            b"DeviceRGB" => ColorSpace::Rgb,
            b"DeviceCMYK" => ColorSpace::Cmyk,
            _ => {
                let Some(obj) = self.lookup(b"ColorSpace", name) else {
                    return Err(report::syntax(&format!(
                        "cannot find ColorSpace resource '{}'",
                        String::from_utf8_lossy(name)
                    )));
                };
                if let Object::Array(a) = self.doc.resolve(&obj)
                    && a.len() == 1
                    && self.doc.resolve(&a[0]).is_name(b"Pattern")
                {
                    return self.emit(pattern(b"Pattern"));
                }
                match obj.as_ref().map(|r| r.num) {
                    Some(num) => {
                        if let Some(cs) = self.proc.state().store.colorspaces.get(&num) {
                            cs.clone()
                        } else {
                            let cs = colorspace::load(self.doc, &obj)?;
                            self.proc.state().store.colorspaces.insert(num, cs.clone());
                            cs
                        }
                    }
                    None => colorspace::load(self.doc, &obj)?,
                }
            }
        };
        self.emit(Op::ColorSpace {
            stroke,
            name,
            colorspace: Some(cs),
        })
    }

    /// `pdf_process_SC`.
    fn process_sc(&mut self, stroke: bool, name: &[u8]) -> Result<(), String> {
        let color: Vec<f32> = self.stack[..self.top].to_vec();
        if name.is_empty() {
            return self.emit(Op::Color {
                stroke,
                color: &color,
            });
        }
        let Some(pattern) = self.lookup(b"Pattern", name) else {
            return Err(report::syntax(&format!(
                "cannot find Pattern resource '{}'",
                String::from_utf8_lossy(name)
            )));
        };
        match self.doc.dict_get(&pattern, b"PatternType").to_int() {
            1 => self.emit(Op::ColorPattern {
                stroke,
                name,
                pattern: &pattern,
                color: &color,
            }),
            2 => {
                let shade = self.load_shade(&pattern)?;
                self.emit(Op::ColorShade {
                    stroke,
                    name,
                    shading: &pattern,
                    shade: &shade,
                })
            }
            other => Err(report::syntax(&format!("unknown pattern type: {other}"))),
        }
    }

    /// `pdf_load_shading`, once per object.
    fn load_shade(&mut self, obj: &Object) -> Result<Arc<Shade>, String> {
        let doc = self.doc;
        self.proc.state().store.shade(doc, obj)
    }

    /// `pdf_process_Do`.
    fn process_do(&mut self, name: &[u8]) -> Result<(), String> {
        let Some(xobj) = self.lookup(b"XObject", name) else {
            return Err(report::syntax(&format!(
                "cannot find XObject resource '{}'",
                String::from_utf8_lossy(name)
            )));
        };
        let mut subtype = self.doc.dict_get(&xobj, b"Subtype");
        if subtype.is_name(b"Form") {
            let st = self.doc.dict_get(&xobj, b"Subtype2");
            if !matches!(st, Object::Null) {
                subtype = st;
            }
        }
        let Some(subtype) = subtype.as_name() else {
            return Err(report::syntax("no XObject subtype specified"));
        };
        let doc = self.doc;
        let usage = self.proc.usage();
        let oc = doc
            .resolve(&xobj)
            .as_dict()
            .and_then(|d| d.get(b"OC").cloned())
            .unwrap_or(Object::Null);
        if self.proc.state().is_hidden(doc, usage, &oc) {
            return Ok(());
        }
        match subtype {
            b"Form" => self.emit(Op::DrawForm { name, form: &xobj }),
            b"Image" => self.emit(Op::DrawImage { name, image: &xobj }),
            b"PS" => {
                self.diag()
                    .warn("content.xobject_ps", "ignoring XObject with subtype PS");
                Ok(())
            }
            other => {
                self.diag().warn(
                    "content.xobject_subtype",
                    format!(
                        "ignoring XObject with unknown subtype: '{}'",
                        String::from_utf8_lossy(other)
                    ),
                );
                Ok(())
            }
        }
    }

    /// The first half of `parse_inline_image`: the dictionary up to `ID`,
    /// the /CS name, and the white space after `ID`.
    fn parse_inline_dict(&mut self, lx: &mut Lexer) -> Result<(Dict, Vec<u8>), String> {
        // The dictionary is read with the document's (large) lex buffer.
        let mut big = Lexer::new(lx.data(), lx.pos(), self.diag());
        let parsed = parse_dict(&mut big, 0);
        lx.seek(big.pos());
        let dict = parsed.map_err(|e| report::syntax(&e.0))?;
        let csname = match dict.get(b"CS") {
            Some(Object::Name(n)) => {
                let n = cstr(n);
                n[..n.len().min(CSNAME_SIZE - 1)].to_vec()
            }
            _ => Vec::new(),
        };
        if lx.read_byte() == Some(b'\r') && lx.peek_byte() == Some(b'\n') {
            lx.read_byte();
        }
        Ok((dict, csname))
    }

    /// `pdf_load_image_imp` for an inline image whose data starts at the
    /// lexer's position, and `pdf_load_compressed_inline_image`.
    fn load_inline_image<'a>(
        &mut self,
        lx: &mut Lexer<'a>,
        dict: Dict,
    ) -> Result<InlineImage<'a>, String> {
        let doc = self.doc;
        let get = |key: &[u8]| match dict.get(key) {
            Some(Object::Null) | None => None,
            Some(v) => Some(v.clone()),
        };
        // pdf_dict_geta: the abbreviation wins.
        let geta = |key: &[u8], abbrev: &[u8]| {
            get(abbrev)
                .or_else(|| get(key))
                .map_or(Object::Null, |v| doc.resolve(&v))
        };
        let is_jpx = match get(b"Filter").map(|f| doc.resolve(&f)) {
            Some(Object::Name(n)) => n.as_ref() == b"JPXDecode",
            Some(Object::Array(a)) => a.iter().any(|f| doc.resolve(f).is_name(b"JPXDecode")),
            _ => false,
        };
        if is_jpx {
            return Err(report::syntax("inline JPXDecode image"));
        }
        let width = geta(b"Width", b"W").to_int();
        let height = geta(b"Height", b"H").to_int();
        let mut bpc = geta(b"BitsPerComponent", b"BPC").to_int();
        if bpc == 0 {
            bpc = 8;
        }
        let image_mask = matches!(geta(b"ImageMask", b"IM"), Object::Bool(true));
        if image_mask {
            bpc = 1;
        }
        if width <= 0 {
            return Err(report::syntax("image width is zero (or less)"));
        }
        if height <= 0 {
            return Err(report::syntax("image height is zero (or less)"));
        }
        if bpc <= 0 {
            return Err(report::syntax("image depth is zero (or less)"));
        }
        if bpc > 16 {
            return Err(report::syntax(&format!("image depth is too large: {bpc}")));
        }
        let mut colorspace = None;
        let mut n = 1;
        let cs_obj = get(b"CS").or_else(|| get(b"ColorSpace"));
        if let Some(mut obj) = cs_obj
            && !image_mask
        {
            // Inline images name colorspaces in the resources.
            if let Object::Name(name) = doc.resolve(&obj)
                && let Some(res) = self.lookup(b"ColorSpace", cstr(&name))
            {
                obj = res;
            }
            let cs = colorspace::load(doc, &obj)?;
            n = cs.n();
            colorspace = Some(cs);
        }
        let area = (bpc as u128).div_ceil(8) * width as u128 * height as u128 * n as u128;
        if area > u64::MAX as u128 {
            return Err(report::syntax("image is too large"));
        }
        let diag = self.diag();
        let resolved = |key: &[u8]| get(key).map_or(Object::Null, |v| doc.resolve(&v));
        let mut mask = resolved(b"SMask");
        if mask.as_dict().is_none() {
            mask = resolved(b"Mask");
        }
        if mask.as_dict().is_some() {
            diag.warn(
                "image.inline_soft_mask",
                "Ignoring invalid inline image soft mask",
            );
        } else if let Object::Array(key) = &mask {
            for i in 0..n * 2 {
                if !matches!(key.get(i).map(|v| doc.resolve(v)), Some(Object::Int(_))) {
                    diag.warn("image.color_key", "invalid value in color key mask");
                }
            }
        }
        let overflow = || report::syntax("integer overflow in inline image size");
        let stride = width
            .checked_mul(n as i32)
            .and_then(|s| s.checked_mul(bpc))
            .ok_or_else(overflow)?;
        let stride = (stride as i64 + 7) >> 3;
        let size = i32::try_from(stride)
            .ok()
            .and_then(|s| s.checked_mul(height))
            .ok_or_else(overflow)?;

        let start = lx.pos();
        let consumed = decode_inline(doc, &dict, &lx.data()[start..], size as usize)?;
        lx.seek(start + consumed);
        Ok(InlineImage {
            dict,
            width,
            height,
            bpc,
            image_mask,
            colorspace,
            n,
            data: &lx.data()[start..start + consumed],
        })
    }

    /// `pdf_process_end`.
    fn process_end(&mut self) -> Result<(), String> {
        self.emit(Op::EndOfData)?;
        while self.gstate > 0 {
            self.gstate -= 1;
            self.emit(Op::Restore)?;
        }
        self.emit(Op::End)
    }
}

/// `fz_clampi` of a float operand, which C converts to int first.
fn clampi(v: f32, lo: i32, hi: i32) -> i32 {
    (v as i32).clamp(lo, hi)
}

/// `fz_open_leecher` over the content: hands the decoders what they ask
/// for, and counts what it handed out.
struct Leech<'a> {
    data: &'a [u8],
    pos: Rc<Cell<usize>>,
}

impl Source for Leech<'_> {
    fn next(&mut self, max: usize, out: &mut Vec<u8>, _: &Diagnostics) -> Result<Next, String> {
        let at = self.pos.get();
        if at >= self.data.len() {
            return Ok(Next::Eof);
        }
        let n = max.max(1).min(self.data.len() - at);
        out.extend_from_slice(&self.data[at..at + n]);
        self.pos.set(at + n);
        Ok(Next::Data)
    }
}

/// Decodes an inline image's `size` bytes from `data`, as
/// `fz_decomp_image_from_stream` reads them, and returns how many bytes of
/// `data` the decoders consumed.
fn decode_inline(doc: &Document, dict: &Dict, data: &[u8], size: usize) -> Result<usize, String> {
    let diag = doc.diagnostics();
    let pos = Rc::new(Cell::new(0));
    // Without a filter this reads exactly the image's bytes, as MuPDF's
    // null filter does.
    let leech = Stream::new(
        Leech {
            data,
            pos: Rc::clone(&pos),
        },
        diag,
    );
    let resolve = |o: &Object| doc.resolve(o);
    let load = |o: &Object| match doc.resolve(o) {
        Object::Stream(s) => Some(doc.stream_data(&s)),
        _ => None,
    };
    let mut stm = crate::filter::open_filters(leech, dict, &resolve, Some(&load), None, true)
        .map_err(|e| e.report())?;
    let mut read = 0;
    let mut scratch = vec![0u8; 1 << 16];
    while read < size {
        let want = (size - read).min(scratch.len());
        let got = stm.read(&mut scratch[..want]);
        read += got;
        if got < want {
            break;
        }
    }
    if read < size {
        diag.warn("image.truncated", "padding truncated image");
    }
    drop(stm);
    Ok(pos.get())
}

/// The end of `parse_inline_image`: `EI` followed by white space, a `<`,
/// a `/` or the end.
fn find_ei(lx: &mut Lexer) -> Result<(), String> {
    let mut ch = lx.read_byte();
    loop {
        while !matches!(ch, Some(b'E') | None) {
            ch = lx.read_byte();
        }
        if ch == Some(b'E') {
            ch = lx.read_byte();
            if ch == Some(b'I') {
                // fz_peek_byte's EOF is -1, which is <= 32.
                match lx.peek_byte() {
                    None => return Ok(()),
                    Some(c) if c <= 32 || c == b'<' || c == b'/' => return Ok(()),
                    Some(c) => ch = Some(c),
                }
            }
        }
        if ch.is_none() {
            return Err(report::syntax("syntax error after inline image"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records each operator as text, for tests.
    #[derive(Default)]
    struct Log(Vec<String>, ProcState);

    impl Processor for Log {
        fn state(&mut self) -> &mut ProcState {
            &mut self.1
        }

        fn op(&mut self, op: Op<'_>) -> Result<(), String> {
            let line = match op {
                Op::InlineImage { image, .. } => {
                    format!("BI {}x{} {:?}", image.width, image.height, image.data)
                }
                Op::Show { text, .. } => format!("Tj {}", String::from_utf8_lossy(text)),
                Op::ShowArray(a) => format!("TJ {a:?}"),
                Op::EndOfData | Op::End => return Ok(()),
                other => format!("{other:?}"),
            };
            self.0.push(line);
            Ok(())
        }
    }

    fn run(content: &[u8]) -> (Vec<String>, Vec<String>) {
        let mut file = b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n2 0 obj<</Length ".to_vec();
        file.extend_from_slice(format!("{}>>stream\n", content.len()).as_bytes());
        file.extend_from_slice(content);
        file.extend_from_slice(b"\nendstream endobj\ntrailer<</Root 1 0 R/Size 3>>\n");
        let doc = Document::open(file).unwrap_or_else(|e| panic!("{e:?}"));
        let before = doc.diagnostics().snapshot().len();
        let mut log = Log::default();
        let contents = Object::Ref(crate::ObjRef::new(2, 0));
        let res = Object::Dict(Arc::new(Dict::new()));
        process_contents(&doc, Some(&res), Some(&contents), &mut log)
            .unwrap_or_else(|e| panic!("{e}"));
        let diags = doc.diagnostics().snapshot()[before..]
            .iter()
            .map(|d| d.message.clone())
            .collect();
        (log.0, diags)
    }

    /// What `content` runs as and reports, read `step` bytes at a time
    /// into windows of `want`, and the most bytes held at once.
    fn windowed(content: &[u8], step: usize, want: usize) -> (Vec<String>, Vec<String>, usize) {
        let file = b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\ntrailer<</Root 1 0 R/Size 2>>\n";
        let doc = Document::open(file.to_vec()).unwrap_or_else(|e| panic!("{e:?}"));
        let before = doc.diagnostics().snapshot().len();
        let mut log = Log::default();
        let res = Object::Dict(Arc::new(Dict::new()));
        let most = process_in_windows(&doc, Some(&res), content, step, want, &mut log)
            .unwrap_or_else(|e| panic!("{e}"));
        let diags = doc.diagnostics().snapshot()[before..]
            .iter()
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect();
        (log.0, diags, most)
    }

    /// Contents with everything that can straddle the end of a window.
    fn straddlers() -> Vec<Vec<u8>> {
        let long_name = format!("/{} BMC EMC 3 w", "N".repeat(300));
        let long_string = format!("BT ({}) Tj ET", "s".repeat(700));
        let many_errors = "1 2 ) ".repeat(120);
        let mut binary = b"q BI /W 4 /H 3 /BPC 8 /CS /G ID ".to_vec();
        binary.extend_from_slice(b"EI E\x00EI\nEI abc");
        binary.extend_from_slice(b" EI Q 9 w");
        let mut v: Vec<Vec<u8>> = [
            &b""[..],
            b"   \n\t ",
            b"1 2 3 m 4 l 0.5 w 7 j q 1 0 0 1 5 6 cm Q",
            b"-12.5 +3 .25 -.5e 1e3 123456789012 m 0.000001 -0 l h",
            b"q q q 1 w",
            b"BT [(a) 5 Tw -20 (b) 3 Tc] TJ (c) Tj T* (d) ' 1 2 (e) \" ET",
            b"BT (a(b)c\\)d\\(e\\n\\101\\\n f) Tj <4142 43 4> Tj <4G41zz> Tj () Tj <> Tj ET",
            b"[1 2 [3 (x)] /N] 0 d [] 3 d",
            b"/Tag <</K 1 /A [1 2 (x)] /N <</M 2>> /S (a>>b)>> BDC EMC /T2 /P1 BDC EMC /T3 MP",
            b"% a comment 1 w\n2 w%tail\r3 w % q q q",
            b"1 w %never ends",
            b"%only a comment",
            b"1    \x00\x00\n\n\r\n\t   w\n\n\n\n2\n\n\n\nw",
            b"BI /W 2 /H 2 /BPC 8 /CS /G ID abcd EI 5 w",
            b"BI /W 2 /H 2 /BPC 8 /CS /G ID abcdEI 5 w",
            b"BI /W 2 /H 1 /BPC 8 /CS /G /F /AHx ID 41 42> EI 5 w BI /W 1 /H 1 /IM true ID \x80 EI",
            b"BI /W 2 /H 2 /BPC 8 /CS /G ID ab",
            b"BI /W 2 /H 2",
            b"BI /W 0 /H 2 /BPC 8 ID abcd EI 6 w",
            b"1 w ) 2 w >> 3 w ] 4 w } 5 w nonsense 6 w",
            b"1 w endstream 2 w",
            b"1 0 0 1 5",
            b"BT (unterminated",
            b"BT <4142",
            b"[1 2",
            b"<</A 1 /B [",
            b"BT [(a) 5",
            b"/Name",
            b"1 w /",
        ]
        .iter()
        .map(|c| c.to_vec())
        .collect();
        v.extend([
            long_name.into_bytes(),
            long_string.into_bytes(),
            many_errors.into_bytes(),
            binary,
        ]);
        v
    }

    #[test]
    fn content_runs_the_same_wherever_its_windows_end() {
        for content in straddlers() {
            let show = String::from_utf8_lossy(&content).into_owned();
            // In one window: the stream as it was always read.
            let (ops, diags, _) = windowed(&content, usize::MAX, usize::MAX);
            assert_eq!(run(&content).0, ops, "{show}");
            for step in [1, 2, 3, 5, 7, 16, 61] {
                for want in [1, 2, 3, 4, 8, 13, 64] {
                    let (o, d, _) = windowed(&content, step, want);
                    assert_eq!(o, ops, "{show}: {step} bytes at a time, windows of {want}");
                    assert_eq!(
                        d, diags,
                        "{show}: {step} bytes at a time, windows of {want}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_stream_is_not_held_whole() {
        // Megabytes of white space and comments around a few operators,
        // 1000 bytes at a time: a few thousand bytes at once at most.
        let mut content = b"1 w ".to_vec();
        content.resize(content.len() + 3_000_000, b' ');
        content.extend_from_slice(b"2 w %");
        content.resize(content.len() + 3_000_000, b'c');
        content.extend_from_slice(b"\n3 w\n");
        for _ in 0..200_000 {
            content.extend_from_slice(b"0 0 m 1 1 l S\n");
        }
        let (ops, diags, most) = windowed(&content, 1000, 4096);
        assert_eq!(ops.len(), 3 + 3 * 200_000);
        assert_eq!(
            &ops[..3],
            ["LineWidth(1.0)", "LineWidth(2.0)", "LineWidth(3.0)"]
        );
        assert!(diags.is_empty(), "{diags:?}");
        assert!(most <= 4096 + 1000, "{most} bytes held");
        // A token longer than a window is held, and no more than about it.
        let string = format!("BT ({}) Tj ET 4 w", "s".repeat(200_000));
        let (ops, _, most) = windowed(string.as_bytes(), 1000, 4096);
        assert_eq!(ops.len(), 4);
        assert!(ops[1].len() > 200_000 && ops[3] == "LineWidth(4.0)");
        assert!((200_000..=420_000).contains(&most), "{most} bytes held");
        // So is an inline image.
        let mut image = b"BI /W 300 /H 300 /BPC 8 /CS /G ID ".to_vec();
        image.resize(image.len() + 90_000, b'E');
        image.extend_from_slice(b" EI 5 w");
        let (ops, diags, most) = windowed(&image, 1000, 4096);
        assert_eq!(ops.len(), 2);
        assert!(ops[0].starts_with("BI 300x300") && ops[1] == "LineWidth(5.0)");
        assert!(diags.is_empty(), "{diags:?}");
        assert!((90_000..=190_000).contains(&most), "{most} bytes held");
    }

    #[test]
    fn whether_content_can_mark_is_told_a_window_at_a_time() {
        let mut contents = straddlers();
        contents.extend([
            b"0 0 1 1 re f 5 5 2 2 re f".to_vec(),
            format!("0 0 1 1 re f {} 1 w (text) Tj", " ".repeat(5000)).into_bytes(),
            format!(
                "0 0 1 1 re f %{}\n/N {} gs",
                "c".repeat(5000),
                "\n".repeat(900)
            )
            .into_bytes(),
            format!("/{} 1 w <4G41> 2 w [(Tj)] 0 d", "N".repeat(300)).into_bytes(),
        ]);
        for content in contents {
            let show = String::from_utf8_lossy(&content).into_owned();
            let told = |step: usize, want: usize| {
                let diag = Diagnostics::new();
                let mut feed = Slices {
                    data: &content,
                    step,
                };
                let marks = may_mark(&mut feed, &diag, want);
                (marks, diag.snapshot())
            };
            let whole = told(usize::MAX, usize::MAX);
            assert!(whole.0.is_ok(), "{show}");
            for step in [1, 3, 7, 64, 1000] {
                for want in [1, 4, 13, 256] {
                    assert_eq!(told(step, want), whole, "{show}: {step}, {want}");
                }
            }
        }
    }

    #[test]
    fn operators_take_their_operands_from_the_bottom_of_the_stack() {
        let (ops, diags) = run(b"1 2 3 m 4 l 0.5 w 7 j q 1 0 0 1 5 6 cm Q");
        assert_eq!(
            ops,
            [
                "MoveTo(1.0, 2.0)",
                "LineTo(4.0, 0.0)",
                "LineWidth(0.5)",
                "LineJoin(2)",
                "Save",
                "Transform([1.0, 0.0, 0.0, 1.0, 5.0, 6.0])",
                "Restore",
            ]
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn a_tj_array_holds_spacing_operators_and_text() {
        let (ops, _) = run(b"BT [(a) 5 Tw -20 (b)] TJ (c) Tj ET");
        assert_eq!(
            ops,
            [
                "BeginText",
                "WordSpacing(5.0)",
                "TJ Some(Array([String([97]), Int(-20), String([98])]))",
                "Tj c",
                "EndText",
            ]
        );
    }

    #[test]
    fn short_strings_share_a_buffer_that_is_never_cleared() {
        #[derive(Default)]
        struct Texts(Vec<(Vec<u8>, Vec<u8>)>, ProcState);
        impl Processor for Texts {
            fn state(&mut self) -> &mut ProcState {
                &mut self.1
            }

            fn op(&mut self, op: Op<'_>) -> Result<(), String> {
                if let Op::Show { text, c_string } = op {
                    self.0.push((text.to_vec(), c_string.to_vec()));
                }
                Ok(())
            }
        }
        let doc = Document::open(
            &b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n2 0 obj<</Length 31>>stream\n\
(quoted) Tj (AB) Tj (x) w () Tj\nendstream endobj\ntrailer<</Root 1 0 R/Size 3>>\n"[..],
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        let mut t = Texts::default();
        let contents = Object::Ref(crate::ObjRef::new(2, 0));
        process_contents(&doc, None, Some(&contents), &mut t).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            t.0,
            [
                (b"quoted".to_vec(), b"quoted".to_vec()),
                (b"AB".to_vec(), b"ABoted".to_vec()),
                (Vec::new(), Vec::new()),
            ]
        );
    }

    #[test]
    fn syntax_errors_are_reported_and_lexing_goes_on() {
        let (ops, diags) = run(b"1 w foo 2 w BX bar EX true 3 w nan");
        assert_eq!(
            ops,
            [
                "LineWidth(1.0)",
                "LineWidth(2.0)",
                "BeginCompatibility",
                "EndCompatibility",
                "LineWidth(3.0)",
            ]
        );
        assert_eq!(
            diags,
            [
                "syntax error: unknown keyword: 'foo'",
                "syntax error: syntax error in content stream",
                "unknown keyword: 'nan'",
                "encountered syntax errors; page may not be correct",
            ]
        );
    }

    #[test]
    fn missing_resources_are_errors_and_unbalanced_q_are_closed() {
        let (ops, diags) = run(b"q q /F1 12 Tf /X Do /GS gs /CS0 cs /DeviceRGB cs");
        assert_eq!(
            ops,
            [
                "Save",
                "Save",
                "Font { name: [70, 49], font: None, size: 12.0 }",
                "ColorSpace { stroke: false, name: [68, 101, 118, 105, 99, 101, 82, 71, 66], colorspace: Some(Rgb) }",
                "Restore",
                "Restore",
            ]
        );
        assert_eq!(
            diags[..3],
            [
                "syntax error: cannot find XObject resource 'X'",
                "syntax error: cannot find ExtGState resource 'GS'",
                "syntax error: cannot find ColorSpace resource 'CS0'",
            ]
        );
    }

    #[test]
    fn inline_image_data_is_skipped_by_its_size_then_ei() {
        // Two bytes of data that read as "EI", then the real EI.
        let (ops, diags) = run(b"BI /W 2 /H 1 /CS /G ID EI EI 1 w");
        assert_eq!(ops, ["BI 2x1 [69, 73]", "LineWidth(1.0)"]);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn filtered_inline_images_consume_what_the_decoder_reads() {
        // ASCIIHex stops once it has the two bytes; what follows up to EI
        // is skipped.
        let (ops, diags) = run(b"BI /W 2 /H 1 /CS /G /F /AHx ID 4142> junk EI 2 w");
        assert_eq!(ops[0], "BI 2x1 [52, 49, 52, 50]");
        assert_eq!(ops[1], "LineWidth(2.0)");
        assert!(diags.is_empty(), "{diags:?}");
        // Too little data is padded, with a warning.
        let (_, diags) = run(b"BI /W 4 /H 1 /CS /G /F /AHx ID 41> EI");
        assert_eq!(diags, ["padding truncated image"]);
        // No EI at all.
        let (ops, diags) = run(b"BI /W 1 /H 1 ID x");
        assert!(ops.is_empty());
        assert_eq!(diags[0], "syntax error: syntax error after inline image");
    }

    /// Each operator in a line of its own, names and strings as text.
    #[derive(Default)]
    struct Ops {
        lines: Vec<String>,
        state: ProcState,
        usage: Option<&'static str>,
        soft_mask: bool,
    }

    fn text(b: &[u8]) -> String {
        String::from_utf8_lossy(b).into_owned()
    }

    impl Processor for Ops {
        fn state(&mut self) -> &mut ProcState {
            &mut self.state
        }

        fn usage(&self) -> Option<&'static str> {
            self.usage
        }

        fn wants_soft_mask(&self) -> bool {
            self.soft_mask
        }

        fn op(&mut self, op: Op<'_>) -> Result<(), String> {
            let line = match op {
                Op::Intent(n) => format!("ri {}", text(n)),
                Op::GsBegin { name, .. } => format!("gs {}", text(name)),
                Op::GsBlendMode(n) => format!("BM {}", text(n)),
                Op::GsSoftMask(m) => format!("SMask {}", m.is_some()),
                Op::GsBlackPointCompensation(n) => format!("BPC {}", text(n)),
                Op::Font { name, font, size } => {
                    format!("Tf {} {size} {}", text(name), font.is_some())
                }
                Op::Show { text: t, .. } => format!("Tj {}", text(t)),
                Op::NextLineShow { text: t, .. } => format!("' {}", text(t)),
                Op::NextLineShowSpaced {
                    word_spacing,
                    char_spacing,
                    text: t,
                    ..
                } => format!("\" {word_spacing} {char_spacing} {}", text(t)),
                Op::ColorSpace {
                    stroke,
                    name,
                    colorspace,
                } => format!(
                    "CS {stroke} {} {:?}",
                    text(name),
                    colorspace.map(|c| c.type_name())
                ),
                Op::ColorPattern {
                    stroke,
                    name,
                    color,
                    ..
                } => format!("SCN pattern {stroke} {} {color:?}", text(name)),
                Op::ColorShade { stroke, name, .. } => {
                    format!("SCN shade {stroke} {}", text(name))
                }
                Op::Shade { name, .. } => format!("sh {}", text(name)),
                Op::DrawImage { name, .. } => format!("Do image {}", text(name)),
                Op::DrawForm { name, .. } => format!("Do form {}", text(name)),
                Op::MarkedPoint(t) => format!("MP {}", text(t)),
                Op::MarkedPointProperties { tag, raw, cooked } => {
                    format!(
                        "DP {} {} {}",
                        text(tag),
                        raw.map(|o| crate::print::to_string(o, Default::default()))
                            .unwrap_or_default(),
                        cooked.is_some()
                    )
                }
                Op::BeginMarked(t) => format!("BMC {}", text(t)),
                Op::BeginMarkedProperties { tag, cooked, .. } => format!(
                    "BDC {} {} hidden={}",
                    text(tag),
                    cooked.is_some(),
                    self.state.hidden
                ),
                Op::EndMarked => format!("EMC hidden={}", self.state.hidden),
                Op::InlineImage {
                    image,
                    colorspace_name,
                } => format!(
                    "BI {}x{} n={} cs={:?} {:?}",
                    image.width,
                    image.height,
                    image.n,
                    colorspace_name.map(text),
                    image.data
                ),
                Op::EndOfData | Op::End => return Ok(()),
                other => format!("{other:?}"),
            };
            self.lines.push(line);
            Ok(())
        }
    }

    /// `content` run with `resources` (object 3), objects from 4 on, as
    /// `proc` sees it: its lines, the warnings, and the error that ended
    /// it.
    fn run_in(
        resources: &str,
        objects: &[&[u8]],
        content: &[u8],
        mut proc: Ops,
    ) -> (Vec<String>, Vec<String>, Option<String>) {
        let mut objs = vec![
            b"<</Type/Catalog>>".to_vec(),
            crate::testutil::stream("", content),
            format!("<<{resources}>>").into_bytes(),
        ];
        objs.extend(objects.iter().map(|o| o.to_vec()));
        let doc = crate::testutil::doc_bytes(&objs);
        let before = doc.diagnostics().snapshot().len();
        let contents = Object::Ref(crate::ObjRef::new(2, 0));
        let res = Object::Ref(crate::ObjRef::new(3, 0));
        let err = process_contents(&doc, Some(&res), Some(&contents), &mut proc).err();
        let diags = doc.diagnostics().snapshot()[before..]
            .iter()
            .map(|d| d.message.clone())
            .collect();
        (proc.lines, diags, err)
    }

    /// Found by the wild-tier sweep: a page that set an ICC color space
    /// at every few operators read and inflated its profile each time, for
    /// minutes. The space is loaded once, as MuPDF's store keeps it.
    #[test]
    fn a_color_space_used_again_is_loaded_once() {
        let icc = crate::testutil::stream("/N 3", crate::cms::RGB_ICC);
        let objs = vec![
            b"<</Type/Catalog>>".to_vec(),
            crate::testutil::stream("", b"/C0 cs 1 0 0 sc /C0 CS /C0 cs /C0 cs"),
            b"<</ColorSpace<</C0 4 0 R>>>>".to_vec(),
            b"[/ICCBased 5 0 R]".to_vec(),
            icc,
        ];
        let doc = crate::testutil::doc_bytes(&objs);
        let mut proc = Ops::default();
        let contents = Object::Ref(crate::ObjRef::new(2, 0));
        let res = Object::Ref(crate::ObjRef::new(3, 0));
        assert_eq!(
            process_contents(&doc, Some(&res), Some(&contents), &mut proc),
            Ok(())
        );
        assert_eq!(proc.state.store.colorspaces.len(), 1);
        assert!(doc.diagnostics().snapshot().is_empty());
    }

    fn ops_of(resources: &str, objects: &[&[u8]], content: &[u8]) -> (Vec<String>, Vec<String>) {
        let (lines, diags, err) = run_in(resources, objects, content, Ops::default());
        assert_eq!(err, None);
        (lines, diags)
    }

    #[test]
    fn every_path_and_text_operator_reaches_the_processor() {
        let (ops, diags) = ops_of(
            "",
            &[],
            b"4 M [3 1] 0 d /Perceptual ri 1 i 3 J -1 j \
              1 2 3 4 5 6 c 1 2 3 4 v 1 2 3 4 y h 0 0 1 1 re S s F f f* B B* b b* n W W* \
              BT 1 Tc 2 Tw 90 Tz 14 TL 3 Tr 2 Ts 1 2 Td 3 4 TD 1 0 0 1 5 6 Tm T* \
              (a) ' 1 2 (b) \" ET 5 0 d0 1 0 0 0 1 1 d1 \
              0.5 G 0.5 g 1 0 0 RG 0 1 0 rg 0 0 0 1 K 1 0 0 0 k 0.2 SC 0.3 0.4 sc \
              /Tag MP /Tag <</A 1>> DP /Tag BMC EMC",
        );
        assert_eq!(
            ops,
            [
                "MiterLimit(4.0)",
                "Dash { array: Some(Array([Int(3), Int(1)])), phase: 0.0 }",
                "ri Perceptual",
                "Flatness(1.0)",
                "LineCap(2)",
                "LineJoin(0)",
                "CurveTo([1.0, 2.0, 3.0, 4.0, 5.0, 6.0])",
                "CurveToV([1.0, 2.0, 3.0, 4.0])",
                "CurveToY([1.0, 2.0, 3.0, 4.0])",
                "ClosePath",
                "Rect([0.0, 0.0, 1.0, 1.0])",
                "Stroke",
                "CloseStroke",
                "FillCompat",
                "Fill",
                "FillEvenOdd",
                "FillStroke",
                "FillStrokeEvenOdd",
                "CloseFillStroke",
                "CloseFillStrokeEvenOdd",
                "EndPath",
                "Clip",
                "ClipEvenOdd",
                "BeginText",
                "CharSpacing(1.0)",
                "WordSpacing(2.0)",
                "HorizontalScaling(90.0)",
                "Leading(14.0)",
                "RenderMode(3)",
                "Rise(2.0)",
                "TextMove(1.0, 2.0)",
                "TextMoveSetLeading(3.0, 4.0)",
                "TextMatrix([1.0, 0.0, 0.0, 1.0, 5.0, 6.0])",
                "NextLine",
                "' a",
                "\" 1 2 b",
                "EndText",
                "Type3Width(5.0, 0.0)",
                "Type3Bbox([1.0, 0.0, 0.0, 0.0, 1.0, 1.0])",
                "Gray { stroke: true, gray: 0.5 }",
                "Gray { stroke: false, gray: 0.5 }",
                "Rgb { stroke: true, rgb: [1.0, 0.0, 0.0] }",
                "Rgb { stroke: false, rgb: [0.0, 1.0, 0.0] }",
                "Cmyk { stroke: true, cmyk: [0.0, 0.0, 0.0, 1.0] }",
                "Cmyk { stroke: false, cmyk: [1.0, 0.0, 0.0, 0.0] }",
                "Color { stroke: true, color: [0.2] }",
                "Color { stroke: false, color: [0.3, 0.4] }",
                "MP Tag",
                "DP Tag <</A 1>> true",
                "BMC Tag",
                "EMC hidden=0",
            ]
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn an_extgstate_sends_each_entry_it_sets() {
        let res = "/ExtGState<</G<</LW 2/LC 5/LJ -1/ML 3/D[[2 1]4]/RI/Saturation/FL 0.5\
                   /Font[4 0 R 9]/OP true/op false/OPM 1/UseBlackPtComp/ON\
                   /TR/Custom/CA 0.25/ca 0.75/BM[/Multiply/Normal]/SMask/None>>\
                   /H<</TR2/Custom/TR/Other/SMask<</S/Luminosity>>/BM/Screen>>>>";
        let font: &[u8] = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>";
        let proc = Ops {
            soft_mask: true,
            ..Ops::default()
        };
        let (ops, diags, err) = run_in(res, &[font], b"/G gs /H gs", proc);
        assert_eq!(err, None);
        assert_eq!(
            ops,
            [
                "gs G",
                "LineWidth(2.0)",
                "LineCap(2)",
                "LineJoin(0)",
                "MiterLimit(3.0)",
                "Dash { array: Some(Array([Int(2), Int(1)])), phase: 4.0 }",
                "ri Saturation",
                "Flatness(0.5)",
                "Tf ExtGState 9 true",
                "GsOverprintStroke(true)",
                "GsOverprintFill(false)",
                "GsOverprintMode(1)",
                "BPC ON",
                "GsStrokeAlpha(0.25)",
                "GsFillAlpha(0.75)",
                "BM Multiply",
                "SMask false",
                "GsEnd",
                "gs H",
                "BM Screen",
                "SMask true",
                "GsEnd",
            ]
        );
        // /TR counts once without /TR2; with it, /TR2 alone.
        assert_eq!(
            diags,
            ["ignoring transfer function", "ignoring transfer function"]
        );
        // A processor that takes no soft masks is not sent them.
        let (ops, _, _) = run_in(res, &[font], b"/G gs", Ops::default());
        assert!(!ops.iter().any(|l| l.starts_with("SMask")), "{ops:?}");
    }

    #[test]
    fn colors_patterns_shadings_and_xobjects_are_looked_up() {
        let res = "/ColorSpace<</CS0[/Pattern]/CS1[/Indexed/DeviceRGB 1<000000FFFFFF>]>>\
                   /Pattern<</P1 4 0 R/P2 5 0 R/P3<</PatternType 7>>>>\
                   /Shading<</Sh 6 0 R>>\
                   /XObject<</Im 7 0 R/Fm 8 0 R/PS<</Subtype/PS>>/Odd<</Subtype/Foo>>/None<<>>\
                   /F2<</Subtype/Form/Subtype2/PS>>>>";
        let shading = "<</ShadingType 2/ColorSpace/DeviceGray/Coords[0 0 1 0]/Function<</FunctionType 2/Domain[0 1]/N 1>>>>";
        let objs: Vec<Vec<u8>> = vec![
            crate::testutil::stream(
                "/PatternType 1/PaintType 1/TilingType 1/BBox[0 0 1 1]/XStep 1/YStep 1",
                b"0 0 1 1 re f",
            ),
            format!("<</PatternType 2/Shading {shading}>>").into_bytes(),
            shading.as_bytes().to_vec(),
            crate::testutil::stream(
                "/Subtype/Image/Width 1/Height 1/ColorSpace/DeviceGray/BitsPerComponent 8",
                b"\x00",
            ),
            crate::testutil::stream("/Subtype/Form/BBox[0 0 1 1]", b""),
        ];
        let refs: Vec<&[u8]> = objs.iter().map(|o| &o[..]).collect();
        let (ops, diags) = ops_of(
            res,
            &refs,
            b"/CS0 CS /CS1 cs /DeviceGray cs /DeviceCMYK CS /Pattern cs \
              0.5 /P1 scn /P2 SCN /P3 scn /Nope scn /Sh sh /Gone sh \
              /Im Do /Fm Do /PS Do /Odd Do /None Do /F2 Do",
        );
        assert_eq!(
            ops,
            [
                "CS true Pattern None",
                "CS false CS1 Some(\"Indexed\")",
                "CS false DeviceGray Some(\"Gray\")",
                "CS true DeviceCMYK Some(\"CMYK\")",
                "CS false Pattern None",
                "SCN pattern false P1 [0.5]",
                "SCN shade true P2",
                "sh Sh",
                "Do image Im",
                "Do form Fm",
            ]
        );
        assert_eq!(
            diags,
            [
                "syntax error: unknown pattern type: 7",
                "syntax error: cannot find Pattern resource 'Nope'",
                "syntax error: cannot find Shading resource 'Gone'",
                "ignoring XObject with subtype PS",
                "ignoring XObject with unknown subtype: 'Foo'",
                "syntax error: no XObject subtype specified",
                "ignoring XObject with subtype PS",
                "encountered syntax errors; page may not be correct",
            ]
        );
    }

    #[test]
    fn hidden_optional_content_counts_its_depth_and_drops_its_xobjects() {
        let objs = vec![
            b"<</Type/Catalog/OCProperties<</OCGs[4 0 R]/D<</OFF[4 0 R]>>>>>>".to_vec(),
            crate::testutil::stream(
                "",
                b"/OC /oc BDC /X BMC /Span <<>> BDC EMC EMC /X Do EMC /OC /oc BDC EMC /Y Do",
            ),
            b"<</Properties<</oc 4 0 R>>/XObject<</X 5 0 R/Y 6 0 R>>>>".to_vec(),
            b"<</Type/OCG/Name(Off)>>".to_vec(),
            crate::testutil::stream("/Subtype/Form/BBox[0 0 1 1]/OC 4 0 R", b""),
            crate::testutil::stream("/Subtype/Form/BBox[0 0 1 1]", b""),
        ];
        let doc = crate::testutil::doc_bytes(&objs);
        let mut proc = Ops {
            usage: Some("View"),
            ..Ops::default()
        };
        let res = Object::Ref(crate::ObjRef::new(3, 0));
        let contents = Object::Ref(crate::ObjRef::new(2, 0));
        process_contents(&doc, Some(&res), Some(&contents), &mut proc)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            proc.lines,
            [
                "BDC OC true hidden=0",
                "BMC X",
                "BDC Span true hidden=2",
                "EMC hidden=3",
                "EMC hidden=2",
                "EMC hidden=1",
                "BDC OC true hidden=0",
                "EMC hidden=1",
                "Do form Y",
            ]
        );
    }

    #[test]
    fn text_arrays_and_operand_limits_are_syntax_errors() {
        let many = "1 ".repeat(33);
        let long = "x".repeat(300);
        let content = format!(
            "BT [(a) Tw] TJ [(b) /N] TJ ET {many}w ({long}) Tj ({long}) ' 1 2 ({long}) \" \
             /A /B MP <</K 1>> 3 w BT [(c) 2"
        );
        let (ops, diags) = ops_of("", &[], content.as_bytes());
        // As in MuPDF: after the error the `]` is one too, and the `TJ`
        // runs with no array. After the overflow the stack is empty.
        assert_eq!(
            ops,
            [
                "BeginText".to_string(),
                "ShowArray(None)".to_string(),
                "ShowArray(None)".to_string(),
                "EndText".to_string(),
                "LineWidth(0.0)".to_string(),
                format!("Tj {long}"),
                format!("' {long}"),
                format!("\" 1 2 {long}"),
                "MP A".to_string(),
                "LineWidth(3.0)".to_string(),
                "BeginText".to_string(),
            ]
        );
        assert_eq!(
            diags,
            [
                "syntax error: syntax error in array",
                "syntax error: syntax error in content stream",
                "syntax error: syntax error in array",
                "syntax error: syntax error in content stream",
                "syntax error: stack overflow",
                "encountered syntax errors; page may not be correct",
            ]
        );
    }

    #[test]
    fn too_many_syntax_errors_end_the_page() {
        let content = "foo ".repeat(MAX_SYNTAX_ERRORS + 5) + "1 w";
        let (ops, diags) = ops_of("", &[], content.as_bytes());
        assert!(ops.is_empty());
        assert_eq!(diags.len(), MAX_SYNTAX_ERRORS + 2);
        assert_eq!(
            diags[MAX_SYNTAX_ERRORS..],
            [
                "too many syntax errors; ignoring rest of page",
                "encountered syntax errors; page may not be correct",
            ]
        );
    }

    #[test]
    fn bad_dictionaries_and_arrays_are_syntax_errors() {
        // MuPDF's `mutool trace` strokes this at widths 0 and 2.
        let (ops, diags) = ops_of("", &[], b"[1 2 <</A>> 1 w <</A 1 /B>> d 2 w");
        assert_eq!(ops, ["LineWidth(0.0)", "LineWidth(2.0)"]);
        assert_eq!(
            diags,
            [
                "syntax error: invalid key in dict",
                "syntax error: invalid key in dict",
                "encountered syntax errors; page may not be correct",
            ]
        );
    }

    #[test]
    fn inline_images_check_their_dictionary() {
        let cases: [(&[u8], &str); 7] = [
            (
                b"BI /W 1 /H 1 /Filter /JPXDecode ID x EI",
                "syntax error: inline JPXDecode image",
            ),
            (
                b"BI /W 1 /H 1 /Filter [/AHx /JPXDecode] ID x EI",
                "syntax error: inline JPXDecode image",
            ),
            (
                b"BI /W 0 /H 1 ID x EI",
                "syntax error: image width is zero (or less)",
            ),
            (
                b"BI /W 1 /H 0 ID x EI",
                "syntax error: image height is zero (or less)",
            ),
            (
                b"BI /W 1 /H 1 /BPC -2 ID x EI",
                "syntax error: image depth is zero (or less)",
            ),
            (
                b"BI /W 1 /H 1 /BPC 32 ID x EI",
                "syntax error: image depth is too large: 32",
            ),
            (
                b"BI /W 2000000000 /H 2 /BPC 16 /CS /RGB ID x EI",
                "syntax error: integer overflow in inline image size",
            ),
        ];
        for (content, want) in cases {
            let (ops, diags) = ops_of("", &[], content);
            assert!(ops.is_empty(), "{ops:?}");
            assert_eq!(diags[0], want);
        }
        // A named colorspace from the resources, a soft mask that is
        // ignored, and colour keys with bad entries.
        let (ops, diags) = ops_of(
            "/ColorSpace<</C0/DeviceRGB>>",
            &[],
            b"BI /W 1 /H 1 /CS /C0 /SMask <<>> ID abc EI BI /IM true /W 8 /H 1 /Mask [0 /x] ID \x80 EI \
              BI /W 1 /H 1 /CS /G /Mask [0 1.5] ID \x00 EI",
        );
        assert_eq!(
            ops,
            [
                "BI 1x1 n=3 cs=Some(\"C0\") [97, 98, 99]",
                "BI 8x1 n=1 cs=None [128]",
                "BI 1x1 n=1 cs=Some(\"G\") [0]",
            ]
        );
        assert_eq!(
            diags,
            [
                "Ignoring invalid inline image soft mask",
                "invalid value in color key mask",
                "invalid value in color key mask",
            ]
        );
    }

    #[test]
    fn contents_arrays_join_their_parts_and_skip_what_does_not_open() {
        let objs = vec![
            b"<</Type/Catalog>>".to_vec(),
            b"[3 0 R 5 0 R 4 0 R]".to_vec(),
            crate::testutil::stream("", b"1 w"),
            crate::testutil::stream("", b"2 w"),
            b"<<>>".to_vec(),
            b"(not a stream)".to_vec(),
        ];
        let doc = crate::testutil::doc_bytes(&objs);
        let run = |num: i32| {
            let before = doc.diagnostics().snapshot().len();
            let mut log = Log::default();
            let c = Object::Ref(crate::ObjRef::new(num, 0));
            process_raw_contents(&doc, &c, &mut log).unwrap_or_else(|e| panic!("{e}"));
            let diags: Vec<String> = doc.diagnostics().snapshot()[before..]
                .iter()
                .map(|d| d.message.clone())
                .collect();
            (log.0, diags)
        };
        let (ops, diags) = run(2);
        assert_eq!(ops, ["LineWidth(1.0)", "LineWidth(2.0)"]);
        assert_eq!(
            diags,
            [
                "format error: object is not a stream",
                "cannot load content stream part 2/3",
            ]
        );
        let (ops, diags) = run(6);
        assert!(ops.is_empty());
        assert_eq!(diags, ["content stream is not a stream (6 0 R)"]);
        // No contents at all is nothing to do.
        let mut log = Log::default();
        assert_eq!(process_contents(&doc, None, None, &mut log), Ok(()));
        assert!(log.0.is_empty());
    }

    #[test]
    fn a_glyph_procedure_runs_with_the_fonts_resources() {
        let doc = crate::testutil::doc_of(&["<</Type/Catalog>>", "<</ExtGState<</G<</LW 7>>>>>>"]);
        let res = Object::Ref(crate::ObjRef::new(2, 0));
        let mut log = Log::default();
        process_glyph(&doc, Some(&res), b"/G gs 0 0 d0", &mut log)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(log.0.len(), 4, "{:?}", log.0);
        assert_eq!(log.0[1], "LineWidth(7.0)");
        assert_eq!(log.0[3], "Type3Width(0.0, 0.0)");
        // The resources are gone again afterwards.
        assert!(log.1.resources().is_none());
    }

    #[test]
    fn shadings_load_once_per_object() {
        let doc = crate::testutil::doc_of(&[
            "<</Type/Catalog>>",
            "<</ShadingType 2/ColorSpace/DeviceGray/Coords[0 0 1 0]/Function<</FunctionType 2/Domain[0 1]/N 1>>>>",
        ]);
        let mut store = Store::default();
        let r = Object::Ref(crate::ObjRef::new(2, 0));
        let a = store.shade(&doc, &r).unwrap_or_else(|e| panic!("{e}"));
        let b = store.shade(&doc, &r).unwrap_or_else(|e| panic!("{e}"));
        assert!(Arc::ptr_eq(&a, &b));
        // A direct shading is loaded each time.
        let direct = doc.resolve(&r);
        let c = store.shade(&doc, &direct).unwrap_or_else(|e| panic!("{e}"));
        assert!(!Arc::ptr_eq(&a, &c));
    }
}
