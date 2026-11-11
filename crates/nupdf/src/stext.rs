// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/stext-device.c, source/fitz/device.c (the
// container stack and its scissors), source/fitz/stext-output.c (do_as_text),
// source/fitz/glyphbox.c and source/fitz/string.c (fz_chartorune),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Structured text: the characters a page shows, gathered into lines and
//! blocks as MuPDF's stext device gathers them, each with its quad, origin,
//! font and size.

use std::sync::Arc;

use crate::colorspace::{ColorSpace, Family};
use crate::content::Store;
use crate::device::{ColorParams, Device, Metatext};
use crate::document::Document;
use crate::fontdesc::{self, Font};
use crate::function::Function;
use crate::geometry::{Matrix, Point, Quad, Rect};
use crate::image::Image;
use crate::object::Object;
use crate::path::{self, LINECAP_ROUND, Path, StrokeState, Walker};
use crate::shade::Shade;
use crate::text::{Text, TextItem, TextSpan};
use crate::unicode::{CATEGORY_MN, compat_decompose, general_category, is_hyphen};

/// `fz_stext_options` flags.
pub mod flags {
    pub const PRESERVE_LIGATURES: u32 = 1;
    pub const PRESERVE_WHITESPACE: u32 = 2;
    pub const PRESERVE_IMAGES: u32 = 4;
    pub const INHIBIT_SPACES: u32 = 8;
    pub const DEHYPHENATE: u32 = 16;
    pub const PRESERVE_SPANS: u32 = 32;
    pub const CLIP: u32 = 64;
    pub const USE_CID_FOR_UNKNOWN_UNICODE: u32 = 128;
    pub const COLLECT_STRUCTURE: u32 = 256;
    pub const ACCURATE_BBOXES: u32 = 512;
    pub const COLLECT_VECTORS: u32 = 1024;
    pub const IGNORE_ACTUALTEXT: u32 = 2048;
    pub const SEGMENT: u32 = 4096;
    pub const PARAGRAPH_BREAK: u32 = 8192;
    pub const TABLE_HUNT: u32 = 16384;
    pub const COLLECT_STYLES: u32 = 32768;
    pub const USE_GID_FOR_UNKNOWN_UNICODE: u32 = 65536;
    pub const CLIP_RECT: u32 = 1 << 17;
    pub const ACCURATE_ASCENDERS: u32 = 1 << 18;
    pub const ACCURATE_SIDE_BEARINGS: u32 = 1 << 19;
    pub const LAZY_VECTORS: u32 = 1 << 20;
    pub const FUZZY_VECTORS: u32 = 1 << 21;
}

/// Vector block flags (`FZ_STEXT_VECTOR_IS_STROKED` and the rest).
pub mod vector_flags {
    pub const IS_STROKED: u32 = 1;
    pub const IS_RECTANGLE: u32 = 2;
    pub const CONTINUES: u32 = 4;
}

/// Character flags (`FZ_STEXT_BOLD` and the rest).
pub mod char_flags {
    pub const STRIKEOUT: i32 = 1;
    pub const UNDERLINE: i32 = 2;
    pub const SYNTHETIC: i32 = 4;
    pub const BOLD: i32 = 8;
    pub const FILLED: i32 = 16;
    pub const STROKED: i32 = 32;
    pub const CLIPPED: i32 = 64;
    pub const UNICODE_IS_CID: i32 = 128;
    pub const UNICODE_IS_GID: i32 = 256;
    pub const SYNTHETIC_LARGE: i32 = 512;
}

/// `FZ_STEXT_LINE_FLAGS_JOINED`.
pub const LINE_JOINED: i32 = 1;

const PARAGRAPH_DIST: f32 = 1.5;
const SPACE_DIST: f32 = 0.15;
const SPACE_MAX_DIST: f32 = 0.8;
const BASE_MAX_DIST: f32 = 0.8;
const FAKE_BOLD_MAX_DIST: f32 = 0.1;

const NON_ACCURATE_GLYPH_ADDED_SPACE: i32 = -2;
const NON_ACCURATE_GLYPH: i32 = -1;

/// `fz_stext_options`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    pub flags: u32,
    /// For `CLIP_RECT`.
    pub clip: Rect,
}

impl Default for Options {
    /// `fz_init_stext_options`: clipping on.
    fn default() -> Options {
        Options {
            flags: flags::CLIP,
            clip: Rect::INFINITE,
        }
    }
}

impl Options {
    /// `fz_parse_stext_options`: a comma-separated list of MuPDF's option
    /// names, each optionally `=yes` or `=no`; `clip-rect=x0:y0:x1:y1`.
    pub fn parse(s: &str) -> Result<Options, String> {
        let mut o = Options::default();
        for item in s.split(',').map(str::trim).filter(|i| !i.is_empty()) {
            let (key, val) = item.split_once('=').unwrap_or((item, "yes"));
            if key == "clip-rect" {
                let v: Vec<f32> = val
                    .split(':')
                    .filter_map(|n| n.trim().parse().ok())
                    .collect();
                let [x0, y0, x1, y1] = v[..] else {
                    return Err(format!("bad clip-rect: {val}"));
                };
                o.clip = Rect::new(x0, y0, x1, y1);
                o.flags |= flags::CLIP_RECT;
                continue;
            }
            let on = match val {
                "yes" | "true" | "1" => true,
                "no" | "false" | "0" => false,
                _ => return Err(format!("bad value for {key}: {val}")),
            };
            let flag = match key {
                "preserve-ligatures" => flags::PRESERVE_LIGATURES,
                "preserve-whitespace" => flags::PRESERVE_WHITESPACE,
                "preserve-images" => flags::PRESERVE_IMAGES,
                "inhibit-spaces" => flags::INHIBIT_SPACES,
                "dehyphenate" => flags::DEHYPHENATE,
                "preserve-spans" => flags::PRESERVE_SPANS,
                "clip" | "mediabox-clip" => flags::CLIP,
                "use-cid-for-unknown-unicode" => flags::USE_CID_FOR_UNKNOWN_UNICODE,
                "use-gid-for-unknown-unicode" => flags::USE_GID_FOR_UNKNOWN_UNICODE,
                "accurate-bboxes" => flags::ACCURATE_BBOXES,
                "accurate-side-bearings" => flags::ACCURATE_SIDE_BEARINGS,
                "ignore-actualtext" => flags::IGNORE_ACTUALTEXT,
                "vectors" => flags::COLLECT_VECTORS,
                "lazy-vectors" => flags::LAZY_VECTORS,
                "fuzzy-vectors" => flags::FUZZY_VECTORS,
                _ => return Err(format!("unknown stext option: {key}")),
            };
            if on {
                o.flags |= flag;
            } else {
                o.flags &= !flag;
            }
        }
        Ok(o)
    }
}

/// `fz_stext_char`.
#[derive(Debug, Clone)]
pub struct StextChar {
    pub c: i32,
    pub bidi: i32,
    pub flags: i32,
    pub argb: u32,
    pub origin: Point,
    pub quad: Quad,
    pub size: f32,
    pub font: Arc<Font>,
}

/// `fz_stext_line`.
#[derive(Debug, Clone)]
pub struct StextLine {
    pub wmode: i32,
    pub flags: i32,
    pub dir: Point,
    pub bbox: Rect,
    pub chars: Vec<StextChar>,
}

/// A text block.
#[derive(Debug, Clone)]
pub struct TextBlock {
    pub bbox: Rect,
    pub flags: i32,
    pub lines: Vec<StextLine>,
}

/// An image block (with `PRESERVE_IMAGES`).
#[derive(Debug, Clone)]
pub struct ImageBlock {
    pub bbox: Rect,
    pub transform: Matrix,
}

/// A vector block (with `COLLECT_VECTORS`): the box of a path, or of a
/// rectangle within one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VectorBlock {
    pub bbox: Rect,
    /// [`vector_flags`].
    pub flags: u32,
    pub argb: u32,
}

/// `fz_stext_block`.
#[derive(Debug, Clone)]
pub enum Block {
    Text(TextBlock),
    Image(ImageBlock),
    Vector(VectorBlock),
}

/// `fz_stext_page`.
#[derive(Debug, Clone)]
pub struct StextPage {
    pub mediabox: Rect,
    pub blocks: Vec<Block>,
}

impl StextPage {
    /// The page's text as `fz_print_stext_page_as_text` writes it: a line
    /// per line and an empty line after each block; a line joined to the
    /// next (`dehyphenate`) loses its last character and its line break.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            if let Block::Text(t) = b {
                block_text(t, &mut out);
                out.push('\n');
            }
        }
        out
    }
}

/// `do_as_text` for one block's lines.
pub fn block_text(block: &TextBlock, out: &mut String) {
    for line in &block.lines {
        let joined = line.flags & LINE_JOINED != 0;
        let n = line.chars.len();
        for (i, ch) in line.chars.iter().enumerate() {
            if joined && i + 1 == n {
                continue;
            }
            out.push(rune_char(ch.c));
        }
        if !joined {
            out.push('\n');
        }
    }
}

/// A character as `fz_runetochar` writes it, as Rust can hold it: U+FFFD
/// past Unicode and for surrogates.
pub fn rune_char(c: i32) -> char {
    u32::try_from(c)
        .ok()
        .and_then(char::from_u32)
        .unwrap_or('\u{fffd}')
}

/// `fz_chartorune`: one character of UTF-8, U+FFFD and one byte for
/// anything that does not decode (code points up to 21 bits pass). Past
/// the end reads as the NUL that ends a C string.
pub fn chartorune(s: &[u8]) -> (i32, usize) {
    const BAD: (i32, usize) = (0xfffd, 1);
    let at = |i: usize| i32::from(s.get(i).copied().unwrap_or(0));
    let c = at(0);
    if c == 0xc0 && at(1) == 0x80 {
        return (0, 2);
    }
    if c < 0x80 {
        return (c, 1);
    }
    let c1 = at(1) ^ 0x80;
    if c1 & 0xc0 != 0 {
        return BAD;
    }
    if c < 0xe0 {
        let l = ((c << 6) | c1) & 0x7ff;
        return if c < 0xc0 || l <= 0x7f { BAD } else { (l, 2) };
    }
    let c2 = at(2) ^ 0x80;
    if c2 & 0xc0 != 0 {
        return BAD;
    }
    if c < 0xf0 {
        let l = ((((c << 6) | c1) << 6) | c2) & 0xffff;
        return if l <= 0x7ff { BAD } else { (l, 3) };
    }
    let c3 = at(3) ^ 0x80;
    if c3 & 0xc0 != 0 {
        return BAD;
    }
    if c < 0xf8 {
        let l = ((((((c << 6) | c1) << 6) | c2) << 6) | c3) & 0x1f_ffff;
        return if l <= 0xffff { BAD } else { (l, 4) };
    }
    BAD
}

/// `fz_utflen`.
fn utflen(s: &[u8]) -> usize {
    let mut n = 0;
    let mut at = 0;
    loop {
        let c = s.get(at).copied().unwrap_or(0);
        if c < 0x80 {
            if c == 0 {
                return n;
            }
            at += 1;
        } else {
            at += chartorune(&s[at..]).1;
        }
        n += 1;
    }
}

/// The bytes of a C string: up to its NUL.
fn cstr(s: &[u8]) -> &[u8] {
    &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())]
}

/// `rune_index`: the `idx`th character, -1 when the string ends first.
fn rune_index(s: &[u8], idx: usize) -> i32 {
    let mut at = 0;
    let mut idx = idx;
    loop {
        let (rune, len) = chartorune(&s[at.min(s.len())..]);
        if rune == 0 {
            return -1;
        }
        at += len;
        if idx == 0 {
            return rune;
        }
        idx -= 1;
    }
}

/// `may_add_space`.
fn may_add_space(lastchar: i32) -> bool {
    lastchar != ' ' as i32 && (lastchar < 0x700 || (0x2000..=0x20CF).contains(&lastchar))
}

/// `plausible_bullet`.
fn plausible_bullet(c: i32) -> bool {
    matches!(
        c,
        0x2A | 0x00B7
            | 0x2022
            | 0x2023
            | 0x2043
            | 0x204C
            | 0x204D
            | 0x2219
            | 0x25C9
            | 0x25CB
            | 0x25CF
            | 0x25D8
            | 0x25E6
            | 0x2619
            | 0x261A..=0x261F
            | 0x2765
            | 0x2767
            | 0x29BE
            | 0x29BF
            | 0x2660..=0x2667
            | 0x1F446..=0x1F449
            | 0x1F597..=0x1F5A3
            | 0x1FBC1..=0x1FBC3
            | 0xFFFD
            | 0x30..=0x39
    )
}

/// `fz_glyph_entirely_outside_box`.
fn glyph_outside(ctm: Matrix, span: &TextSpan, item: &TextItem, r: Rect) -> bool {
    let trm = Matrix {
        e: span.trm.e + item.x,
        f: span.trm.f + item.y,
        ..span.trm
    }
    .concat(ctm);
    let g = fontdesc::bound_glyph(&span.font, item.gid, trm);
    g.x1 <= r.x0 || g.y1 <= r.y0 || g.x0 >= r.x1 || g.y0 >= r.y1
}

/// The kinds of `fz_device_container_stack` entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Clip,
    Mask,
    Group,
    Tile,
}

#[derive(Debug, Clone)]
struct MetatextEntry {
    meta: Metatext,
    /// UTF-8, what is left of it to send.
    text: Vec<u8>,
    bounds: Rect,
}

#[derive(Debug, Clone)]
struct Last {
    valid: bool,
    clipped: bool,
    trm: Matrix,
    wmode: i32,
    bidi_level: i32,
    font: Option<Arc<Font>>,
    flags: i32,
}

/// `fz_stext_device`.
#[derive(Debug)]
pub struct StextDevice {
    page: StextPage,
    flags: u32,
    clip_rect: Rect,
    pen: Point,
    start: Point,
    lag_pen: Point,
    lastchar: i32,
    /// The line the last character went into, by block and line.
    lastline: Option<(usize, usize)>,
    lastbidi: i32,
    maybe_bullet: bool,
    color: u32,
    lasttext: Option<u64>,
    metatext: Vec<MetatextEntry>,
    last: Last,
    container: Vec<(Rect, Container)>,
    /// A device call that did not balance: MuPDF disables the device.
    disabled: bool,
    /// Vector blocks held back until the device closes (`LAZY_VECTORS`).
    lazy_vectors: Vec<Block>,
}

impl StextDevice {
    /// `fz_new_stext_device` for a page with these bounds.
    pub fn new(mediabox: Rect, opts: Options) -> StextDevice {
        StextDevice {
            page: StextPage {
                mediabox,
                blocks: Vec::new(),
            },
            flags: opts.flags,
            clip_rect: opts.clip,
            pen: Point::new(0.0, 0.0),
            start: Point::new(0.0, 0.0),
            lag_pen: Point::new(0.0, 0.0),
            lastchar: ' ' as i32,
            lastline: None,
            lastbidi: 0,
            maybe_bullet: false,
            color: 0,
            lasttext: None,
            metatext: Vec::new(),
            last: Last {
                valid: false,
                clipped: false,
                trm: Matrix::IDENTITY,
                wmode: 0,
                bidi_level: 0,
                font: None,
                flags: 0,
            },
            container: Vec::new(),
            disabled: false,
            lazy_vectors: Vec::new(),
        }
    }

    /// `fz_stext_close_device`: the page, its boxes made.
    pub fn finish(mut self) -> StextPage {
        // A disabled device is not closed.
        if self.disabled {
            return self.page;
        }
        if self.flags & flags::DEHYPHENATE != 0 && is_hyphen(self.lastchar) {
            self.join_last_line();
        }
        self.flush_lazy_vectors();
        fixup_bboxes_and_bidi(&mut self.page.blocks);
        self.page
    }

    /// `flush_lazy_vectors`.
    fn flush_lazy_vectors(&mut self) {
        let lazy = std::mem::take(&mut self.lazy_vectors);
        self.page.blocks.extend(lazy);
    }

    fn join_last_line(&mut self) {
        if let Some((b, l)) = self.lastline
            && let Some(Block::Text(t)) = self.page.blocks.get_mut(b)
            && let Some(line) = t.lines.get_mut(l)
        {
            line.flags |= LINE_JOINED;
        }
    }

    // The container stack (fz_device's).

    fn push(&mut self, rect: Rect, kind: Container) {
        let scissor = match self.container.last() {
            Some(&(top, _)) => top.intersect(rect),
            None => rect,
        };
        self.container.push((scissor, kind));
    }

    fn pop(&mut self, kind: Container) {
        match self.container.last() {
            Some(&(_, k)) if k == kind => {
                self.container.pop();
            }
            _ => self.disabled = true,
        }
    }

    /// `fz_device_current_scissor`.
    fn scissor(&self) -> Rect {
        self.container.last().map_or(Rect::INFINITE, |&(r, _)| r)
    }

    /// `current_clip`.
    fn current_clip(&self) -> Rect {
        let mut r = Rect::INFINITE;
        if self.flags & flags::CLIP != 0 {
            r = self.scissor().intersect(self.page.mediabox);
        }
        if self.flags & flags::CLIP_RECT != 0 {
            r = r.intersect(self.clip_rect);
        }
        r
    }

    // Metatext.

    /// `find_actualtext`.
    fn find_actualtext(&self) -> Option<usize> {
        self.metatext
            .iter()
            .rposition(|m| m.meta == Metatext::ActualText)
    }

    /// `metatext_bounds`: inner bounds folded into `mt`'s.
    fn metatext_bounds(&mut self, mt: usize) -> &mut Rect {
        for i in (mt + 1..self.metatext.len()).rev() {
            let inner = self.metatext[i].bounds;
            self.metatext[i - 1].bounds = self.metatext[i - 1].bounds.union(inner);
        }
        &mut self.metatext[mt].bounds
    }

    /// `actualtext_bounds`, widened by `r`.
    fn widen_actualtext(&mut self, r: Rect) {
        if let Some(mt) = self.find_actualtext() {
            let b = self.metatext_bounds(mt);
            *b = b.union(r);
        }
    }

    /// `pop_metatext`.
    fn pop_metatext(&mut self) {
        let Some(top) = self.metatext.pop() else {
            return;
        };
        if let Some(prev) = self.metatext.last_mut() {
            prev.bounds = prev.bounds.union(top.bounds);
        }
    }

    // Extraction.

    /// `fz_stext_extract`.
    fn extract(&mut self, span: &TextSpan, ctm: Matrix, flags: i32) {
        if span.items.is_empty() {
            return;
        }
        let mt = if self.flags & flags::IGNORE_ACTUALTEXT == 0 {
            self.find_actualtext()
        } else {
            None
        };
        match mt {
            Some(mt) => self.extract_within_actualtext(span, ctm, mt, flags),
            None => self.do_extract(span, ctm, 0, span.items.len(), flags),
        }
    }

    /// Where the pen is for `item`, as `do_extract` sets `last` for it.
    fn set_last(&mut self, span: &TextSpan, item: &TextItem, ctm: Matrix, flags: i32) {
        let tm = Matrix {
            e: item.x,
            f: item.y,
            ..span.trm
        };
        self.last.trm = tm.concat(ctm);
        self.last.bidi_level = span.bidi_level as i32;
        self.last.wmode = span.wmode as i32;
        if self
            .last
            .font
            .as_ref()
            .is_none_or(|f| !Arc::ptr_eq(f, &span.font))
        {
            self.last.font = Some(span.font.clone());
        }
        self.last.valid = true;
        self.last.flags = flags;
    }

    /// `do_extract`.
    fn do_extract(
        &mut self,
        span: &TextSpan,
        ctm: Matrix,
        start: usize,
        end: usize,
        mut flags: i32,
    ) {
        for i in start..end {
            let item = &span.items[i];
            if self.flags & (flags::CLIP | flags::CLIP_RECT) != 0 {
                let r = self.current_clip();
                if glyph_outside(ctm, span, item, r) {
                    self.last.clipped = true;
                    continue;
                }
            }
            self.last.clipped = false;
            self.set_last(span, item, ctm, flags);
            let adv = if item.gid >= 0 { item.adv } else { 0.0 };
            let mut unicode = item.ucs;
            if unicode == 0xfffd {
                if self.flags & flags::USE_CID_FOR_UNKNOWN_UNICODE != 0 {
                    unicode = item.cid;
                    flags |= char_flags::UNICODE_IS_CID;
                } else if self.flags & flags::USE_GID_FOR_UNKNOWN_UNICODE != 0 {
                    unicode = item.gid;
                    flags |= char_flags::UNICODE_IS_GID;
                }
            }
            let trm = self.last.trm;
            let (wmode, bidi) = (self.last.wmode, self.last.bidi_level);
            let force = i == 0 && self.flags & flags::PRESERVE_SPANS != 0;
            self.add_char(
                &span.font, unicode, item.gid, trm, adv, wmode, bidi, force, flags,
            );
        }
    }

    /// `flush_actualtext`: the characters of `text` from the `i`th, up to
    /// the `end`th (all when negative), at the last pen position.
    fn flush_actualtext(&mut self, text: &[u8], mut i: i64, end: i64) {
        let text = cstr(text);
        if text.is_empty() || !self.last.valid {
            return;
        }
        if self.flags & (flags::CLIP | flags::CLIP_RECT) != 0 && self.last.clipped {
            return;
        }
        let Some(font) = self.last.font.clone() else {
            return;
        };
        let mut glyph = -2;
        let mut at = 0;
        while end < 0 || i < end {
            let (rune, len) = chartorune(&text[at.min(text.len())..]);
            at += len;
            if rune == 0 {
                break;
            }
            let (trm, wmode, bidi, flags) = (
                self.last.trm,
                self.last.wmode,
                self.last.bidi_level,
                self.last.flags,
            );
            let force = i == 0 && self.flags & flags::PRESERVE_SPANS != 0;
            self.add_char(&font, rune, glyph, trm, 0.0, wmode, bidi, force, flags);
            i += 1;
            glyph = -1;
        }
    }

    /// `do_extract_within_actualtext`.
    fn extract_within_actualtext(&mut self, span: &TextSpan, ctm: Matrix, mt: usize, flags: i32) {
        let text = cstr(&self.metatext[mt].text).to_vec();
        let mut z = utflen(&text);
        if z == 0 {
            return;
        }
        let len = span.items.len();
        let mut at = 0usize;
        let mut start = 0;
        while start < len {
            let (rune, l) = chartorune(&text[at.min(text.len())..]);
            if span.items[start].ucs != rune || rune == 0 {
                break;
            }
            at += l;
            z -= 1;
            start += 1;
        }
        if start != 0 {
            self.do_extract(span, ctm, 0, start, flags);
        }
        if start == len {
            self.metatext[mt].text = text[at.min(text.len())..].to_vec();
            return;
        }
        let mut end = len;
        while end > start {
            let rune = if z == 0 {
                -1
            } else {
                rune_index(&text[at.min(text.len())..], z - 1)
            };
            if span.items[end - 1].ucs != rune {
                break;
            }
            z -= 1;
            end -= 1;
        }
        let mut i = start;
        while i < end {
            let item = &span.items[i];
            let mut rune = -1;
            if self.flags & (flags::CLIP | flags::CLIP_RECT) != 0 {
                let r = self.current_clip();
                if glyph_outside(ctm, span, item, r) {
                    self.last.clipped = true;
                    i += 1;
                    continue;
                }
            }
            self.last.clipped = false;
            if i < z {
                let (r, l) = chartorune(&text[at.min(text.len())..]);
                rune = r;
                at += l;
            }
            self.set_last(span, item, ctm, flags);
            let adv = if item.gid >= 0 { item.adv } else { 0.0 };
            let trm = self.last.trm;
            let (wmode, bidi) = (self.last.wmode, self.last.bidi_level);
            let force = i == 0 && self.flags & flags::PRESERVE_SPANS != 0;
            self.add_char(
                &span.font, rune, item.gid, trm, adv, wmode, bidi, force, flags,
            );
            i += 1;
        }
        let rest = text[at.min(text.len())..].to_vec();
        if end == len {
            self.metatext[mt].text = rest;
            return;
        }
        if self.last.font.is_none() {
            self.last.font = Some(span.font.clone());
        }
        let stop = i as i64 + rest.len() as i64 - (len - end) as i64;
        self.flush_actualtext(&rest, i as i64, stop);
        if end != len {
            self.do_extract(span, ctm, end, len, flags);
        }
        self.metatext[mt].text.clear();
    }

    /// `fz_add_stext_char`: ligatures taken apart and spaces made plain
    /// unless the options keep them.
    #[allow(clippy::too_many_arguments)]
    fn add_char(
        &mut self,
        font: &Arc<Font>,
        c: i32,
        glyph: i32,
        trm: Matrix,
        adv: f32,
        wmode: i32,
        bidi: i32,
        force: bool,
        flags: i32,
    ) {
        if c == -1 {
            return;
        }
        if self.flags & flags::PRESERVE_LIGATURES == 0 {
            let parts: Option<&[u32]> = match c {
                0xFB00 => Some(&[0x66, 0x66]),
                0xFB01 => Some(&[0x66, 0x69]),
                0xFB02 => Some(&[0x66, 0x6C]),
                0xFB03 => Some(&[0x66, 0x66, 0x69]),
                0xFB04 => Some(&[0x66, 0x66, 0x6C]),
                0xFB05 | 0xFB06 => Some(&[0x73, 0x74]),
                0xFB07..=0xFDFF | 0xFE70..=0xFEFC => Some(compat_decompose(c).unwrap_or(&[])),
                _ => None,
            };
            if let Some(parts) = parts {
                // A form without a decomposition passes as itself (MuPDF
                // reads an uninitialized array there).
                let first = parts.first().map_or(c, |&p| p as i32);
                self.add_char_imp(font, first, glyph, trm, adv, wmode, bidi, force, flags);
                for &p in parts.iter().skip(1) {
                    self.add_char_imp(font, p as i32, -1, trm, 0.0, wmode, bidi, false, flags);
                }
                return;
            }
        }
        let mut c = c;
        if self.flags & flags::PRESERVE_WHITESPACE == 0
            && matches!(
                c,
                0x09 | 0x20 | 0xA0 | 0x1680 | 0x180E | 0x2000..=0x200A | 0x202F | 0x205F | 0x3000
            )
        {
            c = ' ' as i32;
        }
        self.add_char_imp(font, c, glyph, trm, adv, wmode, bidi, force, flags);
    }

    /// The text block characters go into: the page's last block, if it is
    /// one.
    fn current_text_block(&self) -> Option<usize> {
        match self.page.blocks.last() {
            Some(Block::Text(_)) => Some(self.page.blocks.len() - 1),
            _ => None,
        }
    }

    /// `fz_add_stext_char_imp`.
    #[allow(clippy::too_many_arguments)]
    fn add_char_imp(
        &mut self,
        font: &Arc<Font>,
        c: i32,
        glyph: i32,
        trm: Matrix,
        adv: f32,
        wmode: i32,
        bidi: i32,
        force: bool,
        flags: i32,
    ) {
        let mut bidi = bidi & 1;
        let dir = if wmode == 0 {
            Point::new(1.0, 0.0)
        } else {
            Point::new(0.0, -1.0)
        };
        let dir = dir.transform_vector(trm);
        let ndir = dir.normalize();
        let size = trm.expansion();
        let (p, q) = if wmode == 0 {
            (
                Point::new(trm.e, trm.f),
                Point::new(trm.e + adv * dir.x, trm.f + adv * dir.y),
            )
        } else {
            (
                Point::new(trm.e - adv * dir.x, trm.f - adv * dir.y),
                Point::new(trm.e, trm.f),
            )
        };

        let mut cur_block = self.current_text_block();
        let cur_line = cur_block.and_then(|b| match &self.page.blocks[b] {
            Block::Text(t) if !t.lines.is_empty() => Some(t.lines.len() - 1),
            _ => None,
        });
        let accurate = self.flags & flags::ACCURATE_BBOXES != 0;

        if let (Some(b), Some(l)) = (cur_block, cur_line)
            && (glyph == -1 || general_category(c) == CATEGORY_MN)
        {
            let g = if accurate { glyph } else { NON_ACCURATE_GLYPH };
            let pen = self.pen;
            self.add_char_to_line(b, l, trm, font, size, c, g, pen, pen, bidi, 0, flags);
            self.lastbidi = bidi;
            self.lastchar = c;
            self.lastline = Some((b, l));
            return;
        }

        let mut new_para = false;
        let new_line;
        let mut add_space = 0;
        let line_info = match (cur_block, cur_line) {
            (Some(b), Some(l)) => match &self.page.blocks[b] {
                Block::Text(t) => Some((t.lines[l].wmode, t.lines[l].dir)),
                _ => None,
            },
            _ => None,
        };
        match line_info {
            Some((lw, ld)) if lw == wmode && ndir.x * ld.x + ndir.y * ld.y >= 0.999 => {
                let dist = (p.x - self.lag_pen.x).hypot(p.y - self.lag_pen.y) / size;
                if dist < FAKE_BOLD_MAX_DIST && c == self.lastchar && glyph >= 0 {
                    return;
                }
                let delta = Point::new(p.x - self.pen.x, p.y - self.pen.y);
                let spacing = (ndir.x * delta.x + ndir.y * delta.y) / size;
                let base_offset = (-ndir.y * delta.x + ndir.x * delta.y) / size;
                if base_offset.abs() < BASE_MAX_DIST {
                    if (bidi & 1) != (self.lastbidi & 1) {
                        new_line = false;
                    } else if bidi & 1 != 0 {
                        let logical = Point::new(p.x - self.lag_pen.x, p.y - self.lag_pen.y);
                        let logical_spacing =
                            (ndir.x * logical.x + ndir.y * logical.y) / size + adv;
                        if logical_spacing.abs() < SPACE_DIST {
                            new_line = false;
                        } else if spacing.abs() < SPACE_DIST {
                            bidi = 3;
                            new_line = false;
                        } else if logical_spacing < 0.0 && logical_spacing > -SPACE_MAX_DIST {
                            if wmode == 0 && may_add_space(self.lastchar) {
                                add_space = 1;
                            }
                            new_line = false;
                        } else if spacing < 0.0 && spacing > -SPACE_MAX_DIST {
                            new_line = false;
                        } else if spacing > 0.0 && spacing < SPACE_MAX_DIST {
                            bidi = 3;
                            if wmode == 0 && may_add_space(self.lastchar) {
                                add_space = 1 + i32::from(spacing > SPACE_DIST * 2.0);
                            }
                            new_line = false;
                        } else {
                            new_line = true;
                        }
                    } else if spacing.abs() < SPACE_DIST
                        || (spacing < 0.0 && spacing > -SPACE_MAX_DIST)
                    {
                        // Small, or backwards in the line: overlapping
                        // characters.
                        new_line = false;
                    } else if spacing > 0.0 && spacing < SPACE_MAX_DIST {
                        if wmode == 0 && may_add_space(self.lastchar) {
                            add_space = 1 + i32::from(spacing > SPACE_DIST * 2.0);
                        }
                        new_line = false;
                    } else {
                        new_line = true;
                    }
                } else if base_offset.abs() <= PARAGRAPH_DIST {
                    if wmode == 0 && (p.x - self.start.x) > 0.5 && !self.maybe_bullet {
                        new_para = true;
                    }
                    new_line = true;
                } else {
                    new_para = true;
                    new_line = true;
                }
            }
            _ => {
                new_para = true;
                new_line = true;
            }
        }

        let mut cur_line = cur_line;
        // A new block at the start of a text object, or before a new line
        // while vectors are held back.
        if new_para
            || cur_block.is_none()
            || (!self.lazy_vectors.is_empty() && (new_line || cur_line.is_none() || force))
        {
            self.flush_lazy_vectors();
            self.page.blocks.push(Block::Text(TextBlock {
                bbox: Rect::EMPTY,
                flags: 0,
                lines: Vec::new(),
            }));
            cur_block = Some(self.page.blocks.len() - 1);
            cur_line = None;
        }
        let Some(b) = cur_block else {
            return;
        };
        if new_line && self.flags & flags::DEHYPHENATE != 0 && is_hyphen(self.lastchar) {
            self.join_last_line();
        }
        if new_line || cur_line.is_none() || force {
            if let Block::Text(t) = &mut self.page.blocks[b] {
                t.lines.push(StextLine {
                    wmode,
                    flags: 0,
                    dir: ndir,
                    bbox: Rect::new(0.0, 0.0, 0.0, 0.0),
                    chars: Vec::new(),
                });
                cur_line = Some(t.lines.len() - 1);
            }
            self.start = p;
            self.maybe_bullet = glyph == -2 || plausible_bullet(c);
        }
        let Some(l) = cur_line else {
            return;
        };
        let glyph = if glyph == -2 { -1 } else { glyph };
        if c != ' ' as i32 && add_space != 0 && self.flags & flags::INHIBIT_SPACES == 0 {
            let g = if accurate {
                NON_ACCURATE_GLYPH_ADDED_SPACE
            } else {
                NON_ACCURATE_GLYPH
            };
            let pen = self.pen;
            self.add_char_to_line(
                b, l, trm, font, size, ' ' as i32, g, pen, p, bidi, add_space, flags,
            );
        }
        let g = if accurate { glyph } else { NON_ACCURATE_GLYPH };
        self.add_char_to_line(b, l, trm, font, size, c, g, p, q, bidi, 0, flags);
        self.lastchar = c;
        self.lastbidi = bidi;
        self.lastline = Some((b, l));
        self.lag_pen = p;
        self.pen = q;
    }

    /// `add_char_to_line`.
    #[allow(clippy::too_many_arguments)]
    fn add_char_to_line(
        &mut self,
        b: usize,
        l: usize,
        trm: Matrix,
        font: &Arc<Font>,
        size: f32,
        c: i32,
        glyph: i32,
        p: Point,
        q: Point,
        bidi: i32,
        synthetic: i32,
        flags: i32,
    ) {
        let Some(Block::Text(t)) = self.page.blocks.get_mut(b) else {
            return;
        };
        let Some(line) = t.lines.get_mut(l) else {
            return;
        };
        let mut flags = flags;
        if synthetic != 0 {
            flags |= char_flags::SYNTHETIC;
        }
        if synthetic > 1 {
            flags |= char_flags::SYNTHETIC_LARGE;
        }
        if font.is_bold {
            flags |= char_flags::BOLD;
        }
        let (mut a, mut d) = (Point::new(0.0, 0.0), Point::new(0.0, 0.0));
        if line.wmode == 0 {
            let mut bounds = None;
            if glyph == NON_ACCURATE_GLYPH_ADDED_SPACE {
            } else if glyph == NON_ACCURATE_GLYPH {
                a.y = font.ascender;
                d.y = font.descender;
            } else {
                let r = fontdesc::bound_glyph(font, glyph, Matrix::IDENTITY);
                a.y = r.y1;
                d.y = r.y0;
                bounds = Some(r);
            }
            if self.flags & flags::ACCURATE_SIDE_BEARINGS != 0 {
                let r =
                    bounds.unwrap_or_else(|| fontdesc::bound_glyph(font, glyph, Matrix::IDENTITY));
                if a.x > r.x0 {
                    a.x = r.x0;
                }
                // MuPDF compares the descent with the right edge here.
                if d.y < r.x1 {
                    d.y = r.x1;
                }
            }
        } else {
            a.x = 1.0;
        }
        let a = a.transform_vector(trm);
        let d = d.transform_vector(trm);
        line.chars.push(StextChar {
            c,
            bidi,
            flags,
            argb: self.color,
            origin: p,
            quad: Quad {
                ll: Point::new(p.x + d.x, p.y + d.y),
                ul: Point::new(p.x + a.x, p.y + a.y),
                lr: Point::new(q.x + d.x, q.y + d.y),
                ur: Point::new(q.x + a.x, q.y + a.y),
            },
            size,
            font: font.clone(),
        });
    }

    fn text(&mut self, text: &Text, ctm: Matrix, color: u32, flags: i32) {
        if self.disabled {
            return;
        }
        if self.lasttext == Some(text.id) && self.flags & flags::COLLECT_STYLES == 0 {
            return;
        }
        self.color = color;
        for span in &text.spans {
            self.extract(span, ctm, flags);
        }
        self.lasttext = Some(text.id);
    }
}

/// `fixup_bboxes_and_bidi`.
fn fixup_bboxes_and_bidi(blocks: &mut [Block]) {
    for block in blocks {
        let Block::Text(t) = block else {
            continue;
        };
        for line in &mut t.lines {
            let mut reorder = false;
            for (i, ch) in line.chars.iter().enumerate() {
                let r = Rect::from_quad(ch.quad);
                line.bbox = if i == 0 { r } else { line.bbox.union(r) };
                reorder |= ch.bidi == 3;
            }
            t.bbox = t.bbox.union(line.bbox);
            if reorder {
                reverse_bidi_runs(&mut line.chars);
            }
        }
    }
}

/// `reverse_bidi_line`: each run of right-to-left characters reversed.
fn reverse_bidi_runs(chars: &mut [StextChar]) {
    let mut i = 0;
    while i < chars.len() {
        if chars[i].bidi != 0 {
            let mut j = i;
            while j + 1 < chars.len() && chars[j + 1].bidi != 0 {
                j += 1;
            }
            chars[i..=j].reverse();
            i = j + 1;
        } else {
            i += 1;
        }
    }
}

/// `hexrgba_from_color`, for the spaces structured text can convert
/// without color management (others are black).
fn argb(cs: &ColorSpace, color: &[f32], alpha: f32) -> u32 {
    let v = |i: usize| color.get(i).copied().unwrap_or(0.0);
    let rgb = match cs.family() {
        Some(Family::Gray) => [v(0), v(0), v(0)],
        Some(Family::Rgb) => [v(0), v(1), v(2)],
        Some(Family::Cmyk) => [
            1.0 - (v(0) + v(3)).min(1.0),
            1.0 - (v(1) + v(3)).min(1.0),
            1.0 - (v(2) + v(3)).min(1.0),
        ],
        _ => [0.0, 0.0, 0.0],
    };
    let byte = |f: f32| (f * 255.0 + 0.5).clamp(0.0, 255.0) as u32;
    (byte(alpha) << 24) | (byte(rgb[0]) << 16) | (byte(rgb[1]) << 8) | byte(rgb[2])
}

/// `feq`.
fn feq(a: f32, b: f32) -> bool {
    f64::from((a - b).abs()) < 0.00001
}

/// Where a vector block is: the page's blocks, the held-back ones, or
/// neither once merged into another.
#[derive(Debug, Clone, Copy, PartialEq)]
enum VectorAt {
    Page(usize),
    Lazy(usize),
    Gone,
}

impl StextDevice {
    fn vector_mut(&mut self, at: VectorAt) -> Option<&mut VectorBlock> {
        let block = match at {
            VectorAt::Page(i) => self.page.blocks.get_mut(i),
            VectorAt::Lazy(i) => self.lazy_vectors.get_mut(i),
            VectorAt::Gone => None,
        };
        match block {
            Some(Block::Vector(v)) => Some(v),
            _ => None,
        }
    }

    /// `add_vector_imp`: the block the vector went into, if it was not
    /// clipped away, and the block a merge removed, if any.
    fn add_vector_imp(
        &mut self,
        mut bbox: Rect,
        flags: u32,
        argb: u32,
        exp: f32,
    ) -> (Option<VectorAt>, Option<VectorAt>) {
        if exp != 0.0 {
            bbox = Rect::new(bbox.x0 - exp, bbox.y0 - exp, bbox.x1 + exp, bbox.y1 + exp);
        }
        if self.flags & (flags::CLIP_RECT | flags::CLIP) != 0 {
            bbox = bbox.intersect(self.current_clip());
            if !bbox.is_valid() {
                return (None, None);
            }
        }
        let lazy = self.flags & flags::LAZY_VECTORS != 0;
        let fuzzy = self.flags & flags::FUZZY_VECTORS != 0;
        let at = |i: usize| {
            if lazy {
                VectorAt::Lazy(i)
            } else {
                VectorAt::Page(i)
            }
        };
        let list = if lazy {
            &mut self.lazy_vectors
        } else {
            &mut self.page.blocks
        };
        let mut removed = None;
        // Only a small rectangle may join the block before it.
        if flags & vector_flags::IS_RECTANGLE != 0
            && bbox.x1 - bbox.x0 <= 2.0
            && bbox.y1 - bbox.y0 <= 2.0
            && let Some(bi) = list.len().checked_sub(1)
            && let Some(&Block::Vector(b)) = list.get(bi)
            && b.argb == argb
            && b.flags == flags
        {
            let fudge = 0.001;
            let stacks_v = |a: Rect, b: Rect| {
                a.x0 == b.x0 && a.x1 == b.x1 && a.y1 + fudge >= b.y0 && a.y0 - fudge <= b.y1
            };
            let stacks_h = |a: Rect, b: Rect| {
                a.y0 == b.y0 && a.y1 == b.y1 && a.x1 + fudge >= b.x0 && a.x0 - fudge <= b.x1
            };
            let join_v = |a: Rect, b: Rect| Rect::new(a.x0, a.y0.min(b.y0), a.x1, a.y1.max(b.y1));
            let join_h = |a: Rect, b: Rect| Rect::new(a.x0.min(b.x0), a.y0, a.x1.max(b.x1), a.y1);
            if stacks_v(b.bbox, bbox) || stacks_h(b.bbox, bbox) {
                if let Some(Block::Vector(v)) = list.get_mut(bi) {
                    v.bbox = if stacks_v(b.bbox, bbox) {
                        join_v(b.bbox, bbox)
                    } else {
                        join_h(b.bbox, bbox)
                    };
                }
                return (Some(at(bi)), None);
            }
            // Can the two blocks before this one merge instead? MuPDF
            // keeps looking further back while the blocks are rectangles.
            let mut pi = bi;
            while pi > 0 {
                pi -= 1;
                let Some(&Block::Vector(prev)) = list.get(pi) else {
                    break;
                };
                if prev.flags & vector_flags::IS_RECTANGLE == 0 {
                    break;
                }
                if stacks_v(prev.bbox, b.bbox) || stacks_h(prev.bbox, b.bbox) {
                    // Lossless: the earlier block takes in this one's
                    // neighbour, and the new vector goes nowhere.
                    if let Some(Block::Vector(v)) = list.get_mut(pi) {
                        v.bbox = if stacks_v(prev.bbox, b.bbox) {
                            join_v(prev.bbox, b.bbox)
                        } else {
                            join_h(prev.bbox, b.bbox)
                        };
                    }
                    return (Some(at(pi)), None);
                }
                if fuzzy {
                    let fudge2 = 2.0;
                    let (p, q) = (prev.bbox, b.bbox);
                    let vertical = ((p.x0 - q.x0).abs() <= fudge2 || (p.x1 - q.x1).abs() <= fudge2)
                        && p.y1 + fudge >= q.y0
                        && p.y0 - fudge <= q.y1;
                    let horizontal = ((p.y0 - q.y0).abs() <= fudge2
                        || (p.y1 - q.y1).abs() <= fudge2)
                        && p.x1 + fudge >= q.x0
                        && p.x0 - fudge <= q.x1;
                    if vertical || horizontal {
                        if let Some(Block::Vector(v)) = list.get_mut(pi) {
                            v.bbox = Rect::new(
                                p.x0.min(q.x0),
                                p.y0.min(q.y0),
                                p.x1.max(q.x1),
                                p.y1.max(q.y1),
                            );
                        }
                        list.truncate(bi);
                        removed = Some(at(bi));
                        break;
                    }
                }
            }
        }
        list.push(Block::Vector(VectorBlock { bbox, flags, argb }));
        (Some(at(list.len() - 1)), removed)
    }

    /// `add_vectors_from_path`.
    fn add_vectors_from_path(
        &mut self,
        path: &Path,
        ctm: Matrix,
        argb: u32,
        stroke: Option<&StrokeState>,
        exp: f32,
    ) {
        let trailing_moves_acceptable = stroke.is_none_or(|s| s.end_cap != LINECAP_ROUND);
        let mut sp = Split {
            dev: self,
            ctm,
            argb,
            flags: if stroke.is_some() {
                vector_flags::IS_STROKED
            } else {
                0
            },
            count: 0,
            p: [Point::new(0.0, 0.0); 5],
            seg_bounds: Rect::EMPTY,
            leftovers: Rect::EMPTY,
            exp,
            prev: None,
            stroke,
        };
        path.walk(&mut sp);
        // A last move that cannot show anything is dropped.
        if !(trailing_moves_acceptable && sp.count == 1) {
            sp.segment_finished();
        }
        if !sp.leftovers.is_empty() {
            if let Some(s) = sp.stroke {
                sp.leftovers = path::adjust_for_stroke(sp.leftovers, s, Matrix::IDENTITY);
            }
            let leftovers = sp.leftovers;
            sp.add_vector(leftovers, 0);
        }
    }
}

/// `split_path_data`: a path walked to find the rectangles in it.
struct Split<'a> {
    dev: &'a mut StextDevice,
    ctm: Matrix,
    argb: u32,
    flags: u32,
    /// Points of the segment so far while it may be a rectangle, or -1.
    count: i32,
    p: [Point; 5],
    seg_bounds: Rect,
    leftovers: Rect,
    exp: f32,
    prev: Option<VectorAt>,
    stroke: Option<&'a StrokeState>,
}

impl Split<'_> {
    /// `add_vector`.
    fn add_vector(&mut self, bbox: Rect, flags: u32) {
        // Filling empty bounds does nothing.
        if self.stroke.is_none() && bbox.is_empty() {
            return;
        }
        if !bbox.is_valid() {
            return;
        }
        let flags = flags | self.flags;
        if self.dev.flags & flags::COLLECT_VECTORS == 0 {
            return;
        }
        let (b, removed) = self.dev.add_vector_imp(bbox, flags, self.argb, self.exp);
        let Some(b) = b else {
            return;
        };
        if removed.is_some() && self.prev == removed {
            self.prev = Some(VectorAt::Gone);
        }
        // The previous block of this path continues into this one.
        if let Some(prev) = self.prev
            && prev != b
            && let Some(v) = self.dev.vector_mut(prev)
        {
            v.flags |= vector_flags::CONTINUES;
        }
        self.prev = Some(b);
    }

    /// `segment_finished`: the segment sent as a rectangle if it is one,
    /// else added to the leftovers.
    fn segment_finished(&mut self) {
        let mut bounds = self.seg_bounds;
        self.seg_bounds = Rect::EMPTY;
        if self.count >= 0 {
            let p = self.p;
            if self.count == 3 && feq(p[0].x, p[2].x) && feq(p[0].y, p[2].y) {
                // move A, line B, line A, close.
                self.count = 2;
            }
            let rect = match self.count {
                2 => feq(p[0].x, p[1].x) || feq(p[0].y, p[1].y),
                4 | 5 => {
                    (feq(p[0].x, p[1].x)
                        && feq(p[2].x, p[3].x)
                        && feq(p[0].y, p[3].y)
                        && feq(p[1].y, p[2].y))
                        || (feq(p[0].x, p[3].x)
                            && feq(p[1].x, p[2].x)
                            && feq(p[0].y, p[1].y)
                            && feq(p[2].y, p[3].y))
                }
                _ => false,
            };
            if rect {
                if let Some(s) = self.stroke {
                    bounds = bounds.expand(s.linewidth / 2.0);
                }
                self.add_vector(bounds, vector_flags::IS_RECTANGLE);
                return;
            }
        }
        if let Some(s) = self.stroke {
            bounds = path::adjust_for_stroke(bounds, s, Matrix::IDENTITY);
        }
        if self.dev.flags & (flags::CLIP_RECT | flags::CLIP) != 0 {
            bounds = bounds.intersect(self.dev.current_clip());
        }
        if bounds.is_valid() {
            self.leftovers = self.leftovers.union(bounds);
        }
        self.count = -1;
    }
}

impl Walker for Split<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let p = self.ctm.transform_xy(x, y);
        self.segment_finished();
        self.p[0] = p;
        self.count = 1;
        self.seg_bounds = Rect::new(p.x, p.y, p.x, p.y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.ctm.transform_xy(x, y);
        self.seg_bounds = self.seg_bounds.include_point(p);
        if self.count < 0 {
            return;
        }
        let n = self.count as usize;
        // A line to the same point.
        if n > 0 && feq(self.p[n - 1].x, p.x) && feq(self.p[n - 1].y, p.y) {
            return;
        }
        if n >= 2 {
            // Several points along one horizontal or vertical line, as
            // table borders drawn a cell at a time give.
            let (a, b) = (self.p[n - 2], self.p[n - 1]);
            if feq(p.x, b.x)
                && feq(b.x, a.x)
                && ((p.y <= b.y && b.y <= a.y) || (p.y >= b.y && b.y >= a.y))
            {
                self.p[n - 1].y = p.y;
                return;
            }
            if feq(p.y, b.y)
                && feq(b.y, a.y)
                && ((p.x <= b.x && b.x <= a.x) || (p.x >= b.x && b.x >= a.x))
            {
                self.p[n - 1].x = p.x;
                return;
            }
        }
        if n < 4 {
            self.p[n] = p;
            self.count += 1;
            return;
        }
        // A line back to the start closes a rectangle.
        if n == 4 && feq(self.p[0].x, p.x) && feq(self.p[0].y, p.y) {
            self.p[4] = p;
            self.count = 5;
            return;
        }
        self.count = -1;
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) {
        for (x, y) in [(x1, y1), (x2, y2), (x3, y3)] {
            self.seg_bounds = self.seg_bounds.include_point(self.ctm.transform_xy(x, y));
        }
        self.count = -1;
    }

    fn close_path(&mut self) {
        self.segment_finished();
        self.count = 0;
    }
}

#[allow(unused_variables)]
impl Device for StextDevice {
    fn fill_path(
        &mut self,
        path: &Path,
        even_odd: bool,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        params: ColorParams,
    ) {
        // MuPDF's device only has path calls when it collects vectors.
        if self.disabled || self.flags & flags::COLLECT_VECTORS == 0 {
            return;
        }
        self.widen_actualtext(path.bound(None, ctm));
        self.add_vectors_from_path(path, ctm, argb(cs, color, alpha), None, 0.0);
    }

    fn stroke_path(
        &mut self,
        path: &Path,
        stroke: &StrokeState,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        params: ColorParams,
    ) {
        // MuPDF's device only has path calls when it collects vectors.
        if self.disabled || self.flags & flags::COLLECT_VECTORS == 0 {
            return;
        }
        self.widen_actualtext(path.bound(Some(stroke), ctm));
        let exp = stroke.linewidth / 2.0;
        self.add_vectors_from_path(path, ctm, argb(cs, color, alpha), Some(stroke), exp);
    }

    fn clip_path(&mut self, path: &Path, even_odd: bool, ctm: Matrix, scissor: Rect) {
        self.push(path.bound(None, ctm).intersect(scissor), Container::Clip);
    }

    fn clip_stroke_path(&mut self, path: &Path, stroke: &StrokeState, ctm: Matrix, scissor: Rect) {
        self.push(
            path.bound(Some(stroke), ctm).intersect(scissor),
            Container::Clip,
        );
    }

    fn fill_text(
        &mut self,
        text: &Text,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        params: ColorParams,
    ) {
        self.text(text, ctm, argb(cs, color, alpha), char_flags::FILLED);
    }

    fn stroke_text(
        &mut self,
        text: &Text,
        stroke: &StrokeState,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        params: ColorParams,
    ) {
        self.text(text, ctm, argb(cs, color, alpha), char_flags::STROKED);
    }

    fn clip_text(&mut self, text: &Text, ctm: Matrix, scissor: Rect) {
        self.push(text.bound(None, ctm).intersect(scissor), Container::Clip);
        self.text(text, ctm, 0, char_flags::FILLED | char_flags::CLIPPED);
    }

    fn clip_stroke_text(&mut self, text: &Text, stroke: &StrokeState, ctm: Matrix, scissor: Rect) {
        self.push(
            text.bound(Some(stroke), ctm).intersect(scissor),
            Container::Clip,
        );
        self.text(text, ctm, 0, char_flags::STROKED | char_flags::CLIPPED);
    }

    fn ignore_text(&mut self, text: &Text, ctm: Matrix) {
        self.text(text, ctm, 0, 0);
    }

    fn fill_shade(&mut self, shade: &Shade, ctm: Matrix, alpha: f32, params: ColorParams) {
        if self.disabled {
            return;
        }
        let bounds = shade.bound(ctm);
        if self.flags & flags::PRESERVE_IMAGES == 0 {
            self.widen_actualtext(bounds);
            return;
        }
        // fz_new_image_from_shade: a pixmap over the whole pixels the
        // visible part covers (an invalid area gives an empty one at the
        // origin). MuPDF tests the base device's flags for the clip
        // rectangle here, so it never applies.
        let area = bounds.intersect(self.scissor().intersect(self.page.mediabox));
        let (x0, y0, w, h) = if area.is_valid() {
            let (x0, y0) = (area.x0.floor(), area.y0.floor());
            (
                x0,
                y0,
                (area.x1.ceil() - x0).max(0.0),
                (area.y1.ceil() - y0).max(0.0),
            )
        } else {
            (0.0, 0.0, 0.0, 0.0)
        };
        let ctm = Matrix {
            a: w,
            b: 0.0,
            c: 0.0,
            d: h,
            e: x0,
            f: y0,
        };
        self.image(ctm, alpha);
    }

    fn fill_image(&mut self, image: &Image, ctm: Matrix, alpha: f32, params: ColorParams) {
        self.image(ctm, alpha);
    }

    fn fill_image_mask(
        &mut self,
        image: &Image,
        ctm: Matrix,
        cs: &ColorSpace,
        color: &[f32],
        alpha: f32,
        params: ColorParams,
    ) {
        self.image(ctm, alpha);
    }

    fn clip_image_mask(&mut self, image: &Image, ctm: Matrix, scissor: Rect) {
        self.push(
            Rect::UNIT.transform(ctm).intersect(scissor),
            Container::Clip,
        );
    }

    fn pop_clip(&mut self) {
        self.pop(Container::Clip);
    }

    fn begin_mask(
        &mut self,
        area: Rect,
        luminosity: bool,
        cs: Option<&ColorSpace>,
        bc: &[f32],
        params: ColorParams,
    ) {
        self.push(area, Container::Mask);
    }

    fn end_mask(&mut self, transfer: Option<&Function>) {
        match self.container.last_mut() {
            Some((_, k)) if *k == Container::Mask => *k = Container::Clip,
            _ => self.disabled = true,
        }
    }

    fn begin_group(
        &mut self,
        area: Rect,
        cs: Option<&ColorSpace>,
        isolated: bool,
        knockout: bool,
        blendmode: u8,
        alpha: f32,
    ) {
        self.push(area, Container::Group);
    }

    fn end_group(&mut self) {
        self.pop(Container::Group);
    }

    fn begin_tile(
        &mut self,
        area: Rect,
        view: Rect,
        xstep: f32,
        ystep: f32,
        ctm: Matrix,
        id: i32,
    ) -> bool {
        self.push(area, Container::Tile);
        false
    }

    fn end_tile(&mut self) {
        self.pop(Container::Tile);
    }

    fn begin_metatext(&mut self, meta: Metatext, text: &[u8]) {
        if meta == Metatext::ActualText {
            if let Some(mt) = self.find_actualtext() {
                let t = self.metatext[mt].text.clone();
                self.flush_actualtext(&t, 0, -1);
            }
            self.last.valid = false;
        }
        self.metatext.push(MetatextEntry {
            meta,
            text: cstr(text).to_vec(),
            bounds: Rect::EMPTY,
        });
    }

    fn end_metatext(&mut self) {
        let Some(top) = self.metatext.last() else {
            return;
        };
        if top.meta != Metatext::ActualText || self.flags & flags::IGNORE_ACTUALTEXT != 0 {
            self.pop_metatext();
            return;
        }
        let text = top.text.clone();
        let bounds = top.bounds;
        if self.last.valid {
            self.last.trm.e = self.pen.x;
            self.last.trm.f = self.pen.y;
            self.flush_actualtext(&text, 0, -1);
            self.pop_metatext();
            self.last.valid = false;
            return;
        }
        if bounds.is_empty() {
            self.pop_metatext();
            return;
        }
        self.last.trm = Matrix {
            a: bounds.x1 - bounds.x0,
            b: 0.0,
            c: 0.0,
            d: bounds.y0 - bounds.y1,
            e: bounds.x0,
            f: bounds.y1,
        };
        self.last.valid = true;
        let borrowed = self.last.font.is_none();
        if borrowed {
            self.last.font = fontdesc::base14_font("Helvetica");
        }
        self.flush_actualtext(&text, 0, -1);
        self.pop_metatext();
        if borrowed {
            self.last.font = None;
        }
    }
}

impl StextDevice {
    /// `fz_stext_fill_image`.
    fn image(&mut self, ctm: Matrix, alpha: f32) {
        if self.disabled {
            return;
        }
        let r = Rect::UNIT.transform(ctm);
        self.widen_actualtext(r);
        if self.flags & flags::PRESERVE_IMAGES == 0 {
            return;
        }
        if alpha >= 0.5 {
            let mut bbox = r;
            if self.flags & flags::CLIP != 0 {
                bbox = bbox.intersect(self.scissor().intersect(self.page.mediabox));
            }
            self.flush_lazy_vectors();
            self.page.blocks.push(Block::Image(ImageBlock {
                bbox,
                transform: ctm,
            }));
        }
    }
}

/// `fz_new_stext_page_from_page`: a page's structured text from its
/// contents (annotations are not run), or the error that stopped the run.
/// The store carries fonts and the rest from page to page.
pub fn extract_page(
    doc: &Document,
    page: &Object,
    opts: Options,
    store: Store,
) -> (Result<StextPage, String>, Store) {
    let mediabox = match doc.page_transform(page) {
        Ok((rect, ctm)) => rect.transform(ctm),
        Err(e) => return (Err(crate::diag::report::line(&e.0)), store),
    };
    let mut dev = StextDevice::new(mediabox, opts);
    let run = crate::run::run_page_contents(doc, page, &mut dev, store);
    let page = match run.error {
        Some(e) => Err(e),
        None => Ok(dev.finish()),
    };
    (page, run.store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_reads_as_mupdf_reads_it() {
        assert_eq!(chartorune(b"A"), (0x41, 1));
        assert_eq!(chartorune("é".as_bytes()), (0xe9, 2));
        assert_eq!(chartorune(b"\xc0\x80"), (0, 2));
        assert_eq!(chartorune(b"\xff"), (0xfffd, 1));
        assert_eq!(utflen(b"ab\xe2\x82\xac\0zz"), 3);
        assert_eq!(rune_index("x€y".as_bytes(), 1), 0x20ac);
        assert_eq!(rune_index(b"x", 3), -1);
    }

    #[test]
    fn options_parse_like_mupdf_option_strings() {
        let o = Options::parse("preserve-ligatures,dehyphenate,clip=no")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(o.flags, flags::PRESERVE_LIGATURES | flags::DEHYPHENATE);
        let o = Options::parse("clip-rect=0:0:10:20").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(o.clip, Rect::new(0.0, 0.0, 10.0, 20.0));
        assert!(Options::parse("bogus").is_err());
        let o =
            Options::parse("vectors,lazy-vectors,fuzzy-vectors").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            o.flags,
            flags::CLIP | flags::COLLECT_VECTORS | flags::LAZY_VECTORS | flags::FUZZY_VECTORS
        );
    }

    fn vectors_of(flags: u32, draw: impl FnOnce(&mut StextDevice)) -> Vec<VectorBlock> {
        let opts = Options {
            flags: flags::CLIP | flags,
            ..Options::default()
        };
        let mut dev = StextDevice::new(Rect::new(0.0, 0.0, 600.0, 800.0), opts);
        draw(&mut dev);
        dev.finish()
            .blocks
            .into_iter()
            .filter_map(|b| match b {
                Block::Vector(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    fn fill(dev: &mut StextDevice, path: &Path, gray: f32) {
        dev.fill_path(
            path,
            false,
            Matrix::IDENTITY,
            &ColorSpace::Gray,
            &[gray],
            1.0,
            ColorParams::default(),
        );
    }

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Path {
        let mut p = Path::new();
        p.rect_to(x0, y0, x1, y1);
        p
    }

    #[test]
    fn a_filled_rectangle_is_a_rectangle_block() {
        let got = vectors_of(flags::COLLECT_VECTORS, |d| {
            fill(d, &rect(10.0, 20.0, 110.0, 70.0), 0.0)
        });
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].bbox, Rect::new(10.0, 20.0, 110.0, 70.0));
        assert_eq!(got[0].flags, vector_flags::IS_RECTANGLE);
        assert_eq!(got[0].argb, 0xff00_0000);
    }

    #[test]
    fn paths_place_actualtext_only_when_vectors_are_collected() {
        // MuPDF's device has no path calls otherwise, so an ActualText
        // around only a path has no position, and its text is lost.
        for (flags, placed) in [(0, false), (flags::COLLECT_VECTORS, true)] {
            let mut dev = StextDevice::new(
                Rect::new(0.0, 0.0, 600.0, 800.0),
                Options {
                    flags: flags::CLIP | flags,
                    ..Options::default()
                },
            );
            dev.begin_metatext(Metatext::ActualText, b"ABC");
            fill(&mut dev, &rect(50.0, 700.0, 100.0, 750.0), 0.0);
            assert_eq!(dev.metatext[0].bounds.is_valid(), placed, "{flags}");
        }
    }

    #[test]
    fn vectors_are_kept_only_when_asked_for() {
        assert!(vectors_of(0, |d| fill(d, &rect(10.0, 20.0, 110.0, 70.0), 0.0)).is_empty());
    }

    #[test]
    fn a_stroked_triangle_is_one_block_widened_for_the_stroke() {
        let mut p = Path::new();
        p.move_to(100.0, 100.0);
        p.line_to(200.0, 100.0);
        p.line_to(150.0, 180.0);
        p.close_path();
        let stroke = StrokeState {
            linewidth: 4.0,
            ..StrokeState::default()
        };
        let got = vectors_of(flags::COLLECT_VECTORS, |d| {
            d.stroke_path(
                &p,
                &stroke,
                Matrix::IDENTITY,
                &ColorSpace::Gray,
                &[0.0],
                1.0,
                ColorParams::default(),
            )
        });
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].flags, vector_flags::IS_STROKED);
        // Stroke widening from adjust_rect_for_stroke (mitred), then the
        // half line width add_vector_imp adds.
        let b = got[0].bbox;
        assert!(b.x0 < 100.0 - 2.0 && b.y1 > 180.0 + 2.0, "{b:?}");
    }

    #[test]
    fn rectangles_of_one_path_continue_into_each_other() {
        let mut p = rect(10.0, 10.0, 100.0, 50.0);
        p.rect_to(10.0, 300.0, 100.0, 350.0);
        let got = vectors_of(flags::COLLECT_VECTORS, |d| fill(d, &p, 0.0));
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[0].flags,
            vector_flags::IS_RECTANGLE | vector_flags::CONTINUES
        );
        assert_eq!(got[1].flags, vector_flags::IS_RECTANGLE);
    }

    #[test]
    fn small_rectangles_in_a_row_join() {
        let got = vectors_of(flags::COLLECT_VECTORS, |d| {
            for i in 0..5 {
                let x = 10.0 + 2.0 * i as f32;
                fill(d, &rect(x, 10.0, x + 2.0, 11.0), 0.0);
            }
            // Another color does not join.
            fill(d, &rect(20.0, 10.0, 22.0, 11.0), 1.0);
        });
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].bbox, Rect::new(10.0, 10.0, 20.0, 11.0));
    }

    #[test]
    fn vectors_outside_the_page_are_clipped_away() {
        let got = vectors_of(flags::COLLECT_VECTORS, |d| {
            fill(d, &rect(700.0, 900.0, 800.0, 1000.0), 0.0);
            fill(d, &rect(500.0, 10.0, 700.0, 50.0), 0.0);
        });
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].bbox, Rect::new(500.0, 10.0, 600.0, 50.0));
    }

    #[test]
    fn lazy_vectors_reach_the_page_when_it_closes() {
        let got = vectors_of(flags::COLLECT_VECTORS | flags::LAZY_VECTORS, |d| {
            fill(d, &rect(10.0, 20.0, 110.0, 70.0), 0.0);
            assert!(d.page.blocks.is_empty());
        });
        assert_eq!(got.len(), 1);
    }
}
