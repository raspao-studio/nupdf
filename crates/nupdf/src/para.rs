// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/stext-para.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Paragraph breaking (`paragraph-break`): text blocks split into
//! paragraphs at titles, indents, changes of line height, words that would
//! have fitted on the line before, and the short lines of justified text;
//! titles marked as `H` and list items as `LI`, as MuPDF does it.

use std::sync::Arc;

use crate::fontdesc::Font;
use crate::geometry::Rect;
use crate::stext::{Block, StextLine, StextPage, StructBlock, TextBlock, char_flags};

/// `FZ_STEXT_TEXT_JUSTIFY_UNKNOWN` and `_FULL`.
const JUSTIFY_UNKNOWN: i32 = 0;
const JUSTIFY_FULL: i32 = 4;
/// `JUSTIFY_THRESHOLD`.
const JUSTIFY_THRESHOLD: f32 = 1.0;

/// Where the blocks being broken are: their list's structure index count.
struct Pos {
    idx: i32,
}

/// `recalc_bbox`.
fn recalc_bbox(t: &mut TextBlock) {
    t.bbox = t.lines.iter().fold(Rect::EMPTY, |r, l| r.union(l.bbox));
}

/// `font_is_bold`: some crude heuristics.
fn font_is_bold(font: &Font) -> bool {
    if font.is_bold {
        return true;
    }
    // The name as C reads it, to its first NUL.
    let name = font.name.split(|&b| b == 0).next().unwrap_or_default();
    let has = |s: &[u8]| name.windows(s.len()).any(|w| w.eq_ignore_ascii_case(s));
    if has(b"Bold") || has(b"Black") {
        return true;
    }
    if has(b"Medium") || has(b"Light") {
        return false;
    }
    // " B" ending a word, case sensitive.
    if let Some(at) = name.windows(2).position(|w| w == b" B") {
        return matches!(name.get(at + 2), None | Some(b' '));
    }
    false
}

/// `lines_move_plausibly_like_paragraph`: lines moving down the page, or
/// right along one.
fn lines_move_plausibly_like_paragraph(t: &TextBlock) -> bool {
    let mut first = true;
    let (mut line_height, mut line_x, mut line_y) = (0.0f32, 0.0f32, 0.0f32);
    for line in &t.lines {
        let x = (line.bbox.x0 + line.bbox.x1) / 2.0;
        let y = (line.bbox.y0 + line.bbox.y1) / 2.0;
        let height = line.bbox.y1 - line.bbox.y0;
        if line.chars.iter().all(|c| c.c == ' ' as i32) {
            continue;
        }
        if first {
            (line_height, line_x, line_y) = (height, x, y);
            first = false;
        } else if line_y - line_height / 2.0 < y && line_y + line_height / 2.0 > y {
            // Plausibly the same line: it must move right.
            if x < line_x {
                return false;
            }
            line_x = x;
        } else if line_y < y {
            (line_y, line_height, line_x) = (y, height, x);
        } else {
            return false;
        }
    }
    true
}

/// `split_block_at_line`: the lines from `at` on moved into a new block
/// after `blocks[i]`.
fn split_block_at_line(blocks: &mut Vec<Block>, i: usize, at: usize) {
    let Some(Block::Text(t)) = blocks.get_mut(i) else {
        return;
    };
    let rest = t.lines.split_off(at.min(t.lines.len()));
    let mut newblock = TextBlock {
        bbox: Rect::EMPTY,
        flags: t.flags,
        lines: rest,
    };
    recalc_bbox(t);
    recalc_bbox(&mut newblock);
    blocks.insert(i + 1, Block::Text(newblock));
}

/// `block_to_struct`: `blocks[i]` made a structure holding just itself.
fn block_to_struct(blocks: &mut [Block], i: usize, pos: &mut Pos, standard: &'static str) {
    let Some(slot) = blocks.get_mut(i) else {
        return;
    };
    let inner = std::mem::replace(
        slot,
        Block::Struct(StructBlock {
            bbox: Rect::EMPTY,
            index: 0,
            standard,
            raw: String::new(),
            blocks: Vec::new(),
        }),
    );
    let bbox = inner.bbox();
    if let Block::Struct(s) = slot {
        s.bbox = bbox;
        s.index = pos.idx;
        s.blocks.push(inner);
    }
    pos.idx += 1;
}

/// What a line walker's callbacks did.
#[derive(PartialEq)]
enum Walk {
    Go,
    Stop,
}

/// The callbacks of `line_walker`.
trait Walker {
    /// A move down onto a new horizontal line, which starts at `line`.
    fn newline(&mut self, b: &mut Ctx, line: usize, line_height: f32) -> Walk {
        let _ = (b, line, line_height);
        Walk::Go
    }
    fn line(&mut self, b: &mut Ctx, line: usize) -> Walk {
        let _ = (b, line);
        Walk::Go
    }
    fn end(&mut self, b: &mut Ctx) {
        let _ = b;
    }
}

/// The block being walked, in its list.
struct Ctx<'a> {
    blocks: &'a mut Vec<Block>,
    i: usize,
    pos: &'a mut Pos,
    changed: bool,
}

impl Ctx<'_> {
    fn text(&self) -> Option<&TextBlock> {
        match self.blocks.get(self.i) {
            Some(Block::Text(t)) => Some(t),
            _ => None,
        }
    }

    fn line(&self, l: usize) -> Option<&StextLine> {
        self.text().and_then(|t| t.lines.get(l))
    }

    fn line_count(&self) -> usize {
        self.text().map_or(0, |t| t.lines.len())
    }

    fn split(&mut self, at: usize) {
        split_block_at_line(self.blocks, self.i, at);
        self.changed = true;
    }

    fn make_struct(&mut self, standard: &'static str) {
        block_to_struct(self.blocks, self.i, self.pos, standard);
    }
}

/// `line_walker`.
fn line_walker(c: &mut Ctx, w: &mut dyn Walker) {
    let n = c.line_count();
    if n == 0 {
        return;
    }
    let mut first = true;
    let (mut line_height, mut line_y) = (0.0f32, 0.0f32);
    for l in 0..n {
        let Some(line) = c.line(l) else {
            return;
        };
        let y = (line.bbox.y0 + line.bbox.y1) / 2.0;
        let height = line.bbox.y1 - line.bbox.y0;
        if line.chars.is_empty() {
            continue;
        }
        if first {
            line_height = height;
            first = false;
            line_y = y;
        } else if line_y - line_height / 2.0 < y && line_y + line_height / 2.0 > y {
            // The same horizontal line.
        } else if line_y < y {
            line_height = height;
            line_y = y;
            if w.newline(c, l, line_height) == Walk::Stop {
                return;
            }
        }
        if w.line(c, l) == Walk::Stop {
            return;
        }
    }
    w.end(c);
}

// ---- titles ---------------------------------------------------------------

/// `underlined_break` and `font_break`: a run of title lines split off,
/// and marked as a heading unless what follows still has to be looked at.
fn title_break(c: &mut Ctx, start: usize, end: usize) -> Walk {
    let last = c.line_count().saturating_sub(1);
    let at = if start != 0 {
        Some(start)
    } else if end != last {
        Some(end + 1)
    } else {
        None
    };
    match at {
        Some(at) => {
            c.split(at);
            // The latter part is looked at again once the walk steps in.
            if at != start {
                c.make_struct("H");
            }
        }
        None => c.make_struct("H"),
    }
    Walk::Stop
}

#[derive(Clone, Copy, PartialEq)]
enum Underline {
    Unknown,
    Yes,
    No,
    Mixed,
}

/// `underlined_data`: titles as whole lines underlined.
struct Underlined {
    start: Option<usize>,
    end: usize,
    underlined: Underline,
}

impl Walker for Underlined {
    fn newline(&mut self, c: &mut Ctx, line: usize, _: f32) -> Walk {
        if self.underlined == Underline::Yes {
            let prev = line.saturating_sub(1);
            self.start.get_or_insert(prev);
            self.end = prev;
        } else if let Some(start) = self.start {
            return title_break(c, start, self.end);
        }
        self.underlined = Underline::Unknown;
        Walk::Go
    }

    fn line(&mut self, c: &mut Ctx, l: usize) -> Walk {
        if self.underlined == Underline::Mixed {
            return Walk::Go;
        }
        let Some(line) = c.line(l) else {
            return Walk::Go;
        };
        let under = |f: i32| f & char_flags::UNDERLINE != 0;
        if self.underlined == Underline::Unknown {
            self.underlined = if line.chars.first().is_some_and(|ch| under(ch.flags)) {
                Underline::Yes
            } else {
                Underline::No
            };
        }
        if line
            .chars
            .iter()
            .any(|ch| under(ch.flags) != (self.underlined == Underline::Yes))
        {
            self.underlined = Underline::Mixed;
        }
        Walk::Go
    }

    fn end(&mut self, c: &mut Ctx) {
        if self.underlined == Underline::Yes {
            let last = c.line_count().saturating_sub(1);
            self.start.get_or_insert(last);
            self.end = last;
        }
        if let Some(start) = self.start {
            title_break(c, start, self.end);
        }
    }
}

/// The font of a line run: none yet, one, or several.
#[derive(Clone)]
enum RunFont {
    None,
    One(Arc<Font>),
    Mixed,
}

/// `font_data`: titles as whole lines in one bold font.
struct FontTitles {
    start: Option<usize>,
    end: usize,
    font: RunFont,
}

impl FontTitles {
    fn bold(&self) -> bool {
        matches!(&self.font, RunFont::One(f) if font_is_bold(f))
    }
}

impl Walker for FontTitles {
    fn newline(&mut self, c: &mut Ctx, line: usize, _: f32) -> Walk {
        if self.bold() {
            let prev = line.saturating_sub(1);
            self.start.get_or_insert(prev);
            self.end = prev;
        } else if let Some(start) = self.start {
            return title_break(c, start, self.end);
        }
        self.font = RunFont::None;
        Walk::Go
    }

    fn line(&mut self, c: &mut Ctx, l: usize) -> Walk {
        if matches!(self.font, RunFont::Mixed) {
            return Walk::Go;
        }
        let Some(line) = c.line(l) else {
            return Walk::Go;
        };
        if matches!(self.font, RunFont::None)
            && let Some(ch) = line.chars.first()
        {
            self.font = RunFont::One(ch.font.clone());
        }
        if let RunFont::One(f) = &self.font
            && line.chars.iter().any(|ch| !Arc::ptr_eq(&ch.font, f))
        {
            self.font = RunFont::Mixed;
        }
        Walk::Go
    }

    fn end(&mut self, c: &mut Ctx) {
        if self.bold() {
            let last = c.line_count().saturating_sub(1);
            self.start.get_or_insert(last);
            self.end = last;
        }
        if let Some(start) = self.start {
            title_break(c, start, self.end);
        }
    }
}

// ---- paragraph breaks -----------------------------------------------------

/// `indent_data`: a line indented by more than its height starts a
/// paragraph.
struct Indent {
    bbox: Rect,
}

impl Walker for Indent {
    fn newline(&mut self, c: &mut Ctx, line: usize, line_height: f32) -> Walk {
        let Some(l) = c.line(line) else {
            return Walk::Go;
        };
        if l.bbox.x0 - self.bbox.x0 > line_height {
            c.split(line);
            return Walk::Stop;
        }
        Walk::Go
    }
}

/// `linegap_data`: a change of line height by a quarter or more.
struct LineGap {
    num_lines: usize,
    line_height: f32,
    prev_line: usize,
}

impl Walker for LineGap {
    fn newline(&mut self, c: &mut Ctx, line: usize, line_height: f32) -> Walk {
        if self.num_lines == 0 {
            self.line_height = line_height;
        }
        let diff = (self.line_height - line_height).abs();
        // A quarter, so sub- and superscripts do not count.
        if diff >= 0.25 * line_height {
            // Smaller: split before the line before; bigger: here.
            let at = if self.line_height > line_height {
                self.prev_line
            } else {
                line
            };
            c.split(at);
            return Walk::Stop;
        }
        self.num_lines += 1;
        self.prev_line = line;
        Walk::Go
    }
}

/// `trailing_data`: a first word that would have fitted at the end of the
/// line before, which so must have ended a paragraph.
struct Trailing {
    bbox: Rect,
    line_gap: f32,
    prev_line_gap: f32,
    looking_for_space: bool,
    space_size: f32,
    maybe_ends_paragraph: bool,
}

impl Walker for Trailing {
    fn newline(&mut self, c: &mut Ctx, line: usize, _: f32) -> Walk {
        self.prev_line_gap = self.line_gap;
        if self.looking_for_space {
            // No space on the line before: it was one word.
            let len = c.line(line).map_or(0.0, |l| l.bbox.x1 - l.bbox.x0);
            if len + self.space_size < self.prev_line_gap {
                c.split(line);
                return Walk::Stop;
            }
            self.looking_for_space = false;
        }
        self.looking_for_space = self.maybe_ends_paragraph;
        Walk::Go
    }

    fn line(&mut self, c: &mut Ctx, l: usize) -> Walk {
        let Some(line) = c.line(l) else {
            return Walk::Go;
        };
        self.line_gap = self.bbox.x1 - line.bbox.x1;
        // In Latin text a paragraph ends in punctuation.
        self.maybe_ends_paragraph = !line.chars.last().is_some_and(|ch| {
            u32::try_from(ch.c)
                .ok()
                .and_then(char::from_u32)
                .is_some_and(|c| c.is_ascii_alphanumeric())
        });
        let x0 = line.bbox.x0;
        let mut split = false;
        for ch in &line.chars {
            if ch.c != ' ' as i32 {
                continue;
            }
            let r = Rect::from_quad(ch.quad);
            let w = r.x1 - r.x0;
            if w < self.space_size {
                self.space_size = w;
            }
            if !self.looking_for_space {
                continue;
            }
            if (r.x0 - x0) + self.space_size < self.prev_line_gap {
                split = true;
                break;
            }
            self.looking_for_space = false;
        }
        if split {
            c.split(l);
            return Walk::Stop;
        }
        Walk::Go
    }
}

/// `justify_data`: whether the block is fully justified text.
struct Justify {
    bbox: Rect,
    count_lines: usize,
    count_justified: usize,
    non_digits_exist_in_this_line: bool,
    fragment_box: Rect,
    line_box: Rect,
    gap_count_this_line: usize,
    gap_size_this_line: f32,
    bad_gap: bool,
    last_min_space: f32,
}

impl Justify {
    fn line_is_justified(&self) -> bool {
        self.line_box.x0 < self.bbox.x0 + JUSTIFY_THRESHOLD
            && self.line_box.x1 > self.bbox.x1 - JUSTIFY_THRESHOLD
    }

    /// `fragment_end`.
    fn fragment_end(&mut self) {
        if self.fragment_box.is_empty() {
            return;
        }
        let mut gap = if self.line_box.is_empty() {
            0.0
        } else if self.fragment_box.x0 > self.line_box.x1 {
            self.fragment_box.x0 - self.line_box.x1
        } else if self.fragment_box.x1 < self.line_box.x0 {
            self.line_box.x1 - self.fragment_box.x0
        } else {
            0.0
        };
        self.line_box = self.line_box.union(self.fragment_box);
        self.fragment_box = Rect::EMPTY;
        if gap < self.last_min_space {
            return;
        }
        if self.gap_count_this_line > 0 {
            // Some layouts put double spaces before full stops.
            let size = f64::from(self.gap_size_this_line);
            if (f64::from(gap) - size).abs() > 1.0 && (f64::from(gap) / 2.0 - size).abs() < 1.0 {
                gap /= 2.0;
            }
            if (f64::from(gap) - size).abs() > 1.0 {
                self.bad_gap = true;
            }
        }
        let n = self.gap_count_this_line as f32;
        // Fused, as clang compiles MuPDF's `size * n + gap`.
        self.gap_size_this_line = self.gap_size_this_line.mul_add(n, gap) / (n + 1.0);
        self.gap_count_this_line += 1;
    }
}

impl Walker for Justify {
    fn newline(&mut self, _: &mut Ctx, _: usize, _: f32) -> Walk {
        self.line_box = self.line_box.union(self.fragment_box);
        if self.line_is_justified()
            && self.gap_count_this_line > 0
            && self.non_digits_exist_in_this_line
        {
            self.count_justified += 1;
        }
        self.non_digits_exist_in_this_line = false;
        self.count_lines += 1;
        self.gap_size_this_line = 0.0;
        self.gap_count_this_line = 0;
        self.fragment_box = Rect::EMPTY;
        self.line_box = Rect::EMPTY;
        Walk::Go
    }

    /// Walks the line a fragment at a time, so that text running in either
    /// direction is measured by its gaps.
    fn line(&mut self, c: &mut Ctx, l: usize) -> Walk {
        let Some(line) = c.line(l) else {
            return Walk::Go;
        };
        for ch in &line.chars {
            let r = Rect::from_quad(ch.quad);
            // SPACE_DIST, as in the stext device.
            let min_space = ch.size * 0.15;
            if ch.c == ' ' as i32 {
                self.last_min_space = min_space;
                continue;
            }
            if (ch.c <= '0' as i32 || ch.c >= '9' as i32) && ch.c != '.' as i32 {
                self.non_digits_exist_in_this_line = true;
            }
            if !self.fragment_box.is_empty()
                && (r.x0 > self.fragment_box.x1 + self.last_min_space
                    || r.x1 < self.fragment_box.x0 - self.last_min_space)
            {
                self.fragment_end();
            }
            self.fragment_box = self.fragment_box.union(r);
            self.last_min_space = min_space;
        }
        Walk::Go
    }

    fn end(&mut self, _: &mut Ctx) {
        self.fragment_end();
        self.line_box = self.line_box.union(self.fragment_box);
        if self.line_is_justified()
            && self.gap_count_this_line > 0
            && self.non_digits_exist_in_this_line
        {
            self.count_justified += 1;
        }
        self.count_lines += 1;
    }
}

/// The second walk of justified text: a line short of either side ends a
/// paragraph. MuPDF reuses the first walk's state, line box and all.
struct Justify2<'a>(&'a mut Justify);

impl Walker for Justify2<'_> {
    fn newline(&mut self, c: &mut Ctx, line: usize, _: f32) -> Walk {
        if !self.0.line_is_justified() {
            c.split(line);
            return Walk::Stop;
        }
        self.0.line_box = Rect::EMPTY;
        Walk::Go
    }

    fn line(&mut self, c: &mut Ctx, l: usize) -> Walk {
        if let Some(line) = c.line(l) {
            for ch in line.chars.iter().filter(|ch| ch.c != ' ' as i32) {
                self.0.line_box = self.0.line_box.union(Rect::from_quad(ch.quad));
            }
        }
        Walk::Go
    }
}

/// `text_block_marked_bbox`: the box of the block's characters, spaces
/// left out.
fn marked_bbox(t: &TextBlock) -> Rect {
    t.lines
        .iter()
        .flat_map(|l| &l.chars)
        .filter(|ch| ch.c != ' ' as i32)
        .fold(Rect::EMPTY, |r, ch| r.union(Rect::from_quad(ch.quad)))
}

// ---- lists ----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum ListState {
    LookingForBullet,
    LookingForPostBullet,
    LookingForPostNumericalBullet,
    FoundBullet,
    ContinuationLine,
    NoBullet,
}

#[derive(Clone, Copy, PartialEq)]
enum Bullet {
    Not,
    Plain,
    Numerical,
}

const BULLETS: &[i32] = &[
    '*' as i32, 0x00B7, 0x2022, 0x2023, 0x2043, 0x204C, 0x204D, 0x2219, 0x25C9, 0x25CB, 0x25CF,
    0x25D8, 0x25E6, 0x2619, 0x261a, 0x261b, 0x261c, 0x261d, 0x261e, 0x261f, 0x2765, 0x2767, 0x29BE,
    0x29BF, 0x2660, 0x2661, 0x2662, 0x2663, 0x2664, 0x2665, 0x2666, 0x2667, 0x1F446, 0x1F447,
    0x1F448, 0x1F449, 0x1f597, 0x1F598, 0x1F599, 0x1F59A, 0x1F59B, 0x1F59C, 0x1F59D, 0x1F59E,
    0x1F59F, 0x1F5A0, 0x1F5A1, 0x1F5A2, 0x1F5A3, 0x1FBC1, 0x1FBC2, 0x1FBC3, 0xFFFD,
];

fn is_roman(c: i32) -> bool {
    u8::try_from(c).is_ok_and(|b| b"mMcClLxXvViI".contains(&b))
}

/// `is_bullet_aux`.
fn is_bullet_aux(buf: &[i32], contained: bool) -> Bullet {
    let len = buf.len();
    if len == 1 && BULLETS.contains(&buf[0]) {
        return Bullet::Plain;
    }
    let inner = |b: &[i32]| {
        if is_bullet_aux(b, true) != Bullet::Not {
            Bullet::Plain
        } else {
            Bullet::Not
        }
    };
    if !contained {
        for (open, close) in [('(', ')'), ('<', '>'), ('[', ']'), ('{', '}')] {
            if len > 2 && buf[0] == open as i32 && buf[len - 1] == close as i32 {
                return inner(&buf[1..len - 1]);
            }
        }
        if len > 1 && (buf[len - 1] == ':' as i32 || buf[len - 1] == ')' as i32) {
            return inner(&buf[..len - 1]);
        }
    }
    // Numbers, but not rows of them like "10.02 12.03".
    let (mut decimal_pos, mut decimals_found) = (0, 0);
    let mut i = 0;
    while i < len {
        if ('0' as i32..='9' as i32).contains(&buf[i]) {
        } else if buf[i] == '.' as i32 {
            decimal_pos = i;
            decimals_found += 1;
        } else {
            break;
        }
        i += 1;
    }
    if i == len && decimals_found <= 1 {
        return Bullet::Numerical;
    }
    // number.something
    if decimals_found > 0 && i == decimal_pos + 1 && i < len {
        return if is_bullet_aux(&buf[i..], false) != Bullet::Not {
            Bullet::Plain
        } else {
            Bullet::Not
        };
    }
    // Roman numerals, or roman.something.
    let i = buf.iter().take_while(|&&c| is_roman(c)).count();
    if i == len {
        return Bullet::Plain;
    }
    if buf[i] == '.' as i32 && i + 1 < len {
        return if is_bullet_aux(&buf[i + 1..], false) != Bullet::Not {
            Bullet::Plain
        } else {
            Bullet::Not
        };
    }
    Bullet::Not
}

/// `list_data`: list items found by their bullets.
struct List {
    state: ListState,
    buffer: Vec<i32>,
    bullet_r: f32,
    post_bullet_indent: f32,
    l: f32,
    bullet_line_start: Option<usize>,
    this_line_start: usize,
    block_bbox: Rect,
}

impl List {
    /// `eval_buffer_for_bullet`: true if the line is decided.
    fn eval_buffer(&mut self, size: f32) -> bool {
        match is_bullet_aux(&self.buffer, false) {
            Bullet::Numerical => self.state = ListState::LookingForPostNumericalBullet,
            Bullet::Plain => self.state = ListState::LookingForPostBullet,
            Bullet::Not => {
                self.state = if approx_eq(self.l, self.post_bullet_indent, size / 2.0) {
                    ListState::ContinuationLine
                } else {
                    ListState::NoBullet
                };
                return true;
            }
        }
        false
    }

    fn found(&mut self, x0: f32) {
        self.state = ListState::FoundBullet;
        self.bullet_line_start.get_or_insert(self.this_line_start);
        self.post_bullet_indent = x0;
    }
}

fn approx_eq(a: f32, b: f32, c: f32) -> bool {
    f64::from(a - b).abs() <= f64::from(c)
}

impl Walker for List {
    fn newline(&mut self, c: &mut Ctx, line: usize, _: f32) -> Walk {
        if self.state == ListState::FoundBullet {
            let start = self.bullet_line_start.unwrap_or(0);
            if start != 0 {
                // Split before the bullet started.
                c.split(start);
                return Walk::Stop;
            }
            if start != self.this_line_start {
                // A second bullet: break before the line before.
                c.split(self.this_line_start);
                c.make_struct("LI");
                return Walk::Stop;
            }
        } else if self.state == ListState::NoBullet && self.bullet_line_start.is_some() {
            // Neither a bullet nor a continuation after a bullet.
            c.split(self.this_line_start);
            c.make_struct("LI");
            return Walk::Stop;
        }
        self.this_line_start = line;
        self.state = ListState::LookingForBullet;
        self.buffer.clear();
        self.l = self.block_bbox.x1;
        self.bullet_r = self.block_bbox.x0;
        Walk::Go
    }

    fn line(&mut self, c: &mut Ctx, l: usize) -> Walk {
        let Some(line) = c.line(l) else {
            return Walk::Go;
        };
        let x0 = line.bbox.x0;
        for ch in &line.chars {
            let r = Rect::from_quad(ch.quad);
            if r.x0 < self.l {
                self.l = x0;
            }
            match self.state {
                ListState::LookingForBullet => {
                    if ch.c == ' ' as i32 {
                        // Leading spaces are skipped.
                        if self.buffer.is_empty() {
                            continue;
                        }
                        if self.eval_buffer(ch.size) {
                            return Walk::Go;
                        }
                    } else if !self.buffer.is_empty() && r.x0 - self.bullet_r > ch.size / 2.0 {
                        // A gap as big as a space.
                        if self.eval_buffer(ch.size) {
                            return Walk::Go;
                        }
                    } else if self.buffer.len() < 10 {
                        self.buffer.push(ch.c);
                    } else {
                        // Too long to be a bullet.
                        self.state = if approx_eq(self.l, self.post_bullet_indent, ch.size) {
                            ListState::ContinuationLine
                        } else {
                            ListState::NoBullet
                        };
                        return Walk::Go;
                    }
                    self.bullet_r = r.x1;
                }
                ListState::LookingForPostBullet => {
                    if ch.c != ' ' as i32 {
                        self.found(r.x0);
                    }
                }
                ListState::LookingForPostNumericalBullet => {
                    if ('0' as i32..='9' as i32).contains(&ch.c) {
                        // A numerical bullet is not followed by digits.
                        self.state = if approx_eq(self.l, self.post_bullet_indent, ch.size) {
                            ListState::ContinuationLine
                        } else {
                            ListState::NoBullet
                        };
                        return Walk::Go;
                    }
                    if ch.c != ' ' as i32 {
                        self.found(r.x0);
                    }
                }
                _ => {}
            }
        }
        Walk::Go
    }

    fn end(&mut self, c: &mut Ctx) {
        if self.state == ListState::LookingForBullet {
            self.eval_buffer(0.0);
            // A bullet with nothing after it is fine at the end.
            if matches!(
                self.state,
                ListState::LookingForPostNumericalBullet | ListState::LookingForPostBullet
            ) {
                self.state = ListState::FoundBullet;
                self.bullet_line_start.get_or_insert(self.this_line_start);
            }
        }
        if self.state == ListState::FoundBullet {
            let start = self.bullet_line_start.unwrap_or(0);
            if start != 0 {
                c.split(start);
                return;
            }
            if start != self.this_line_start {
                c.split(self.this_line_start);
                c.make_struct("LI");
                return;
            }
            c.make_struct("LI");
        } else if self.state == ListState::NoBullet && self.bullet_line_start.is_some() {
            c.split(self.this_line_start);
            c.make_struct("LI");
        } else if self.bullet_line_start.is_some() {
            // The block ends within the list item.
            c.make_struct("LI");
        }
    }
}

// ---- the page -------------------------------------------------------------

/// The standard types MuPDF counts as headings.
fn is_header(standard: &str) -> bool {
    matches!(standard, "H" | "H1" | "H2" | "H3" | "H4" | "H5" | "H6")
}

/// Runs a walker over `blocks[i]`; true if it changed the blocks.
fn walk(blocks: &mut Vec<Block>, i: usize, pos: &mut Pos, w: &mut dyn Walker) -> bool {
    let mut c = Ctx {
        blocks,
        i,
        pos,
        changed: false,
    };
    line_walker(&mut c, w);
    c.changed
}

fn is_text(blocks: &[Block], i: usize) -> bool {
    matches!(blocks.get(i), Some(Block::Text(_)))
}

/// `do_para_break`.
fn do_para_break(blocks: &mut Vec<Block>, in_header: bool, depth: usize) {
    let mut pos = Pos { idx: 0 };
    let mut i = 0;
    // A block is looked at again at most once, as the structure it became.
    let mut budget = blocks.len().saturating_mul(64) + 64;
    while i < blocks.len() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        match &mut blocks[i] {
            Block::Struct(s) => {
                if s.index < pos.idx {
                    s.index = pos.idx;
                    pos.idx += 1;
                } else {
                    pos.idx = s.index + 1;
                }
                // Structures nest no deeper than segmentation and this
                // pass make them.
                if depth < 64 {
                    let header = in_header || is_header(s.standard);
                    do_para_break(&mut s.blocks, header, depth + 1);
                }
                i += 1;
            }
            Block::Text(t) => {
                if !lines_move_plausibly_like_paragraph(t) {
                    i += 1;
                    continue;
                }
                // Trailing spaces left out, so lines ending "foo " end where
                // lines ending "ba-" do.
                let bbox = marked_bbox(t);
                if !in_header {
                    walk(
                        blocks,
                        i,
                        &mut pos,
                        &mut Underlined {
                            start: None,
                            end: 0,
                            underlined: Underline::Unknown,
                        },
                    );
                    if !is_text(blocks, i) {
                        continue;
                    }
                    walk(
                        blocks,
                        i,
                        &mut pos,
                        &mut FontTitles {
                            start: None,
                            end: 0,
                            font: RunFont::None,
                        },
                    );
                    if !is_text(blocks, i) {
                        continue;
                    }
                }
                walk(blocks, i, &mut pos, &mut Indent { bbox });
                if !is_text(blocks, i) {
                    continue;
                }
                walk(
                    blocks,
                    i,
                    &mut pos,
                    &mut LineGap {
                        num_lines: 0,
                        line_height: 0.0,
                        prev_line: 0,
                    },
                );
                if !is_text(blocks, i) {
                    continue;
                }
                walk(
                    blocks,
                    i,
                    &mut pos,
                    &mut Trailing {
                        bbox,
                        line_gap: 0.0,
                        prev_line_gap: 0.0,
                        looking_for_space: false,
                        space_size: 99999.0,
                        maybe_ends_paragraph: false,
                    },
                );
                if !is_text(blocks, i) {
                    continue;
                }
                let flags = |blocks: &[Block]| match blocks.get(i) {
                    Some(Block::Text(t)) => (t.flags, t.bbox),
                    _ => (JUSTIFY_UNKNOWN, Rect::EMPTY),
                };
                if flags(blocks).0 == JUSTIFY_UNKNOWN {
                    let mut j = Justify {
                        bbox,
                        count_lines: 0,
                        count_justified: 0,
                        non_digits_exist_in_this_line: false,
                        fragment_box: Rect::EMPTY,
                        line_box: Rect::EMPTY,
                        gap_count_this_line: 0,
                        gap_size_this_line: 0.0,
                        bad_gap: false,
                        last_min_space: 0.0,
                    };
                    walk(blocks, i, &mut pos, &mut j);
                    if j.count_lines >= 2 && j.count_justified * 2 >= j.count_lines && !j.bad_gap {
                        if let Some(Block::Text(t)) = blocks.get_mut(i) {
                            t.flags = JUSTIFY_FULL;
                        }
                        walk(blocks, i, &mut pos, &mut Justify2(&mut j));
                    }
                }
                if !is_text(blocks, i) {
                    continue;
                }
                // The list walk reads the block's box as it is now.
                let (flags, block_bbox) = flags(blocks);
                if flags == JUSTIFY_UNKNOWN {
                    let this_line_start = 0;
                    walk(
                        blocks,
                        i,
                        &mut pos,
                        &mut List {
                            state: ListState::LookingForBullet,
                            buffer: Vec::new(),
                            bullet_r: block_bbox.x0,
                            post_bullet_indent: 0.0,
                            l: block_bbox.x1,
                            bullet_line_start: None,
                            this_line_start,
                            block_bbox,
                        },
                    );
                }
                i += 1;
            }
            Block::Image(_) | Block::Vector(_) | Block::Grid(_) => i += 1,
        }
    }
}

/// `fz_paragraph_break`.
pub fn paragraph_break(page: &mut StextPage) {
    do_para_break(&mut page.blocks, false, 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Point, Quad};
    use crate::stext::StextChar;

    fn bullet(s: &str) -> Bullet {
        let v: Vec<i32> = s.chars().map(|c| c as i32).collect();
        is_bullet_aux(&v, false)
    }

    #[test]
    fn bullets_numbers_and_numerals() {
        assert!(bullet("•") == Bullet::Plain);
        assert!(bullet("*") == Bullet::Plain);
        assert!(bullet("1.") == Bullet::Numerical);
        assert!(bullet("12") == Bullet::Numerical);
        assert!(bullet("(iv)") == Bullet::Plain);
        assert!(bullet("iv)") == Bullet::Plain);
        assert!(bullet("[•]") == Bullet::Plain);
        assert!(bullet("1.a") == Bullet::Not);
        assert!(bullet("1.2.3") == Bullet::Not);
        assert!(bullet("word") == Bullet::Not);
        // A contained numeral counts as a plain bullet.
        assert!(bullet("(3)") == Bullet::Plain);
    }

    #[test]
    fn bold_fonts_by_flag_or_name() {
        let named = |n: &str| font_is_bold(&Font::new(n.as_bytes()));
        assert!(named("Helvetica-Bold"));
        assert!(named("ArialBLACK"));
        assert!(named("Times B"));
        assert!(named("Times B Roman"));
        assert!(!named("Times Bk"));
        assert!(!named("Roboto Medium"));
        assert!(!named("Courier"));
        let mut f = Font::new(b"Plain");
        f.is_bold = true;
        assert!(font_is_bold(&f));
    }

    /// A line of characters `size` wide each, starting at `x`.
    fn line(font: &Arc<Font>, text: &str, x: f32, y: f32, size: f32) -> StextLine {
        let chars: Vec<StextChar> = text
            .chars()
            .enumerate()
            .map(|(i, c)| {
                let x0 = x + i as f32 * size * 0.5;
                let x1 = x0 + size * 0.5;
                StextChar {
                    c: c as i32,
                    bidi: 0,
                    flags: 0,
                    argb: 0,
                    origin: Point::new(x0, y + size),
                    quad: Quad {
                        ul: Point::new(x0, y),
                        ur: Point::new(x1, y),
                        ll: Point::new(x0, y + size),
                        lr: Point::new(x1, y + size),
                    },
                    size,
                    font: font.clone(),
                }
            })
            .collect();
        let bbox = chars
            .iter()
            .fold(Rect::EMPTY, |r, c| r.union(Rect::from_quad(c.quad)));
        StextLine {
            wmode: 0,
            flags: 0,
            dir: Point::new(1.0, 0.0),
            bbox,
            chars,
        }
    }

    fn page(lines: Vec<StextLine>) -> StextPage {
        let mut t = TextBlock {
            bbox: Rect::EMPTY,
            flags: 0,
            lines,
        };
        recalc_bbox(&mut t);
        StextPage {
            mediabox: Rect::new(0.0, 0.0, 600.0, 800.0),
            blocks: vec![Block::Text(t)],
        }
    }

    fn texts(blocks: &[Block]) -> Vec<String> {
        blocks
            .iter()
            .map(|b| match b {
                Block::Text(t) => t
                    .lines
                    .iter()
                    .map(|l| {
                        l.chars
                            .iter()
                            .filter_map(|c| char::from_u32(c.c as u32))
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("/"),
                Block::Struct(s) => format!("{}[{}]", s.standard, texts(&s.blocks).join("|")),
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn an_indented_line_starts_a_paragraph() {
        let f = Arc::new(Font::new(b"Body"));
        let mut p = page(vec![
            line(&f, "aaaa aaaa aaaa.", 50.0, 100.0, 10.0),
            line(&f, "bbbb bbbb bbbb.", 50.0, 112.0, 10.0),
            line(&f, "eeee eeee ee", 80.0, 124.0, 10.0),
            line(&f, "dddd dddd dddd.", 50.0, 136.0, 10.0),
        ]);
        paragraph_break(&mut p);
        // (A first word of Roman numerals, like "cccc", would read as a
        // bullet and make the line a list item, as in MuPDF.)
        assert_eq!(
            texts(&p.blocks),
            [
                "aaaa aaaa aaaa./bbbb bbbb bbbb.",
                "eeee eeee ee/dddd dddd dddd."
            ]
        );
    }

    #[test]
    fn a_bold_first_line_is_a_title() {
        let body = Arc::new(Font::new(b"Body"));
        let bold = Arc::new(Font::new(b"Body-Bold"));
        let mut p = page(vec![
            line(&bold, "Title", 50.0, 100.0, 10.0),
            line(&body, "text text text", 50.0, 112.0, 10.0),
            line(&body, "more text more", 50.0, 124.0, 10.0),
        ]);
        paragraph_break(&mut p);
        let got = texts(&p.blocks);
        assert_eq!(got[0], "H[Title]");
        assert_eq!(got[1], "text text text/more text more");
        let Block::Struct(s) = &p.blocks[0] else {
            panic!("no structure");
        };
        // MuPDF numbers the heading as it makes it and again as it steps
        // into it.
        assert_eq!((s.index, s.raw.as_str()), (1, ""));
    }

    #[test]
    fn bulleted_lines_are_list_items() {
        let f = Arc::new(Font::new(b"Body"));
        // Three lines, so the one reaching the right edge does not make
        // the block look justified, which would leave lists alone. The
        // two left after the first item split off do look justified, so
        // MuPDF leaves them together.
        let mut p = page(vec![
            line(&f, "• one", 50.0, 100.0, 10.0),
            line(&f, "• second item here", 50.0, 112.0, 10.0),
            line(&f, "• three", 50.0, 124.0, 10.0),
        ]);
        paragraph_break(&mut p);
        assert_eq!(
            texts(&p.blocks),
            ["LI[• one]", "• second item here/• three"]
        );
    }
}
