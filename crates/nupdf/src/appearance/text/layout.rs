// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-appearance.c (the text walk,
// measure_string, break_string, write_string and its quadding and comb
// forms, the layout_* functions and write_variable_text),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Text as MuPDF lays it out in appearance streams: walked one character
//! at a time, each in the font of its script (the base-14 font in WinAnsi,
//! ISO 8859-7 or KOI8-U, or a CJK font), broken into lines at spaces, and
//! written as `Tj` runs. Free text and line captions write it; text
//! widgets will write and lay it out with the same code.

use std::sync::Arc;

use super::super::Writer;
use super::fonts::{self, LANG_ZH_HANS};
use crate::fontdesc::Font;
use crate::glyphs;
use crate::object::{Dict, Object};
use crate::unicode as ucd;

/// The character written for one no font has: a middle dot.
pub const REPLACEMENT: i32 = 0xB7;

/// The font a character is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Enc {
    Latin = 1,
    Greek,
    Cyrillic,
    Korean,
    Japanese,
    Hant,
    Hans,
}

impl Enc {
    fn is_cjk(self) -> bool {
        self >= Enc::Korean
    }
}

/// `find_initial_script`: the script of the first character that has one,
/// Latin when none does.
fn initial_script(text: &str) -> u8 {
    let mut script = ucd::SCRIPT_COMMON;
    for c in text.chars() {
        script = ucd::script(c as i32);
        if script != ucd::SCRIPT_COMMON && script != ucd::SCRIPT_INHERITED {
            break;
        }
    }
    if script == ucd::SCRIPT_COMMON || script == ucd::SCRIPT_INHERITED {
        ucd::SCRIPT_LATIN
    } else {
        script
    }
}

/// `struct text_walk_state`: the walk over `text[..end]`, at `pos` the
/// current character `u` (`n` bytes), written as code `c` in `enc`, `w`
/// ems wide.
pub struct Walk<'a> {
    text: &'a str,
    end: usize,
    font: &'a Font,
    lang: i32,
    pub pos: usize,
    pub n: usize,
    pub enc: Enc,
    pub u: i32,
    pub c: i32,
    pub w: f32,
    last_script: u8,
}

impl<'a> Walk<'a> {
    /// `init_text_walk`. The initial script is found in all of `text`, not
    /// just the part walked, as in C.
    pub fn new(lang: i32, font: &'a Font, text: &'a str, end: Option<usize>) -> Walk<'a> {
        Walk {
            text,
            end: end.unwrap_or(text.len()).min(text.len()),
            font,
            lang,
            pos: 0,
            n: 0,
            enc: Enc::Latin,
            u: 0,
            c: 0,
            w: 0.0,
            last_script: initial_script(text),
        }
    }

    /// The byte offset after the current character.
    pub fn next_pos(&self) -> usize {
        self.pos + self.n
    }

    /// `next_text_walk`: false at the end.
    pub fn advance(&mut self) -> bool {
        self.pos += self.n;
        let Some(ch) = self
            .text
            .get(self.pos..self.end)
            .and_then(|s| s.chars().next())
        else {
            self.n = 0;
            return false;
        };
        self.n = ch.len_utf8();
        self.u = ch as i32;
        let mut script = ucd::script(self.u);
        if fonts::is_han_form(self.u) {
            script = ucd::SCRIPT_HAN;
        }
        if script == ucd::SCRIPT_COMMON || script == ucd::SCRIPT_INHERITED {
            script = self.last_script;
        }
        self.last_script = script;
        (self.enc, self.c) = match script {
            ucd::SCRIPT_LATIN => (Enc::Latin, glyphs::windows_1252_from_unicode(self.u)),
            ucd::SCRIPT_GREEK => (Enc::Greek, glyphs::iso8859_7_from_unicode(self.u)),
            ucd::SCRIPT_CYRILLIC => (Enc::Cyrillic, glyphs::koi8u_from_unicode(self.u)),
            ucd::SCRIPT_HANGUL => (Enc::Korean, self.u),
            ucd::SCRIPT_HIRAGANA | ucd::SCRIPT_KATAKANA => (Enc::Japanese, self.u),
            ucd::SCRIPT_BOPOMOFO if self.lang == LANG_ZH_HANS => (Enc::Hans, self.u),
            ucd::SCRIPT_BOPOMOFO => (Enc::Hant, self.u),
            ucd::SCRIPT_HAN => (
                match self.lang {
                    fonts::LANG_KO => Enc::Korean,
                    fonts::LANG_ZH | fonts::LANG_ZH_HANT => Enc::Hant,
                    LANG_ZH_HANS => Enc::Hans,
                    _ => Enc::Japanese,
                },
                self.u,
            ),
            _ => (Enc::Latin, REPLACEMENT),
        };
        if self.c < 0 {
            self.enc = Enc::Latin;
            self.c = REPLACEMENT;
        }
        self.w = if self.enc.is_cjk() {
            1.0
        } else {
            let g = fonts::encode_character(self.font, self.u);
            fonts::advance(self.font, g)
        };
        true
    }
}

/// `measure_string`: the text's width in ems.
pub fn measure_string(lang: i32, font: &Font, text: &str) -> f32 {
    let mut walk = Walk::new(lang, font, text, None);
    let mut w = 0.0f32;
    while walk.advance() {
        w += walk.w;
    }
    w
}

/// `break_string`: the first line of `text` at `size` that fits in `maxw`
/// (broken after a space, else at a newline or the end): its width, and
/// where the next line starts.
pub fn break_string(lang: i32, font: &Font, size: f32, text: &str, maxw: f32) -> (f32, usize) {
    let mut walk = Walk::new(lang, font, text, None);
    let mut space: Option<(usize, f32)> = None;
    let mut x = 0.0f32;
    while walk.advance() {
        if walk.u == i32::from(b'\n') || walk.u == i32::from(b'\r') {
            break;
        }
        if walk.u == i32::from(b' ') {
            space = Some((walk.next_pos(), x));
        }
        x = walk.w.mul_add(size, x);
        if let Some((at, space_x)) = space
            && x > maxw
        {
            return (space_x, at);
        }
    }
    (x, walk.next_pos())
}

/// The font resource a run in `enc` uses.
fn select_font(w: &mut Writer, enc: Enc, fontname: &str, size: f32) {
    match enc {
        Enc::Latin => w.s(&format!("/{fontname} ")),
        Enc::Greek => w.s(&format!("/{fontname}GRK ")),
        Enc::Cyrillic => w.s(&format!("/{fontname}CYR ")),
        Enc::Korean => w.s("/Batang "),
        Enc::Japanese => w.s("/Mincho "),
        Enc::Hant => w.s("/Ming "),
        Enc::Hans => w.s("/Song "),
    }
    w.g(size);
    w.s(" Tf\n");
}

/// A string byte, escaped as `write_string` escapes it.
fn push_escaped(w: &mut Writer, c: i32) {
    if c == i32::from(b'(') || c == i32::from(b')') || c == i32::from(b'\\') {
        w.buf.push(b'\\');
    }
    w.buf.push(c as u8);
}

/// `write_string`: `text[..end]` as one `Tj` run per change of font.
pub fn write_string(
    w: &mut Writer,
    lang: i32,
    font: &Font,
    fontname: &str,
    size: f32,
    text: &str,
    end: usize,
) {
    let mut walk = Walk::new(lang, font, text, Some(end));
    let mut last: Option<Enc> = None;
    let close = |w: &mut Writer, enc: Enc| {
        w.buf.push(if enc.is_cjk() { b'>' } else { b')' });
        w.s(" Tj\n");
    };
    while walk.advance() {
        if last != Some(walk.enc) {
            if let Some(enc) = last {
                close(w, enc);
            }
            select_font(w, walk.enc, fontname, size);
            w.buf.push(if walk.enc.is_cjk() { b'<' } else { b'(' });
            last = Some(walk.enc);
        }
        if walk.enc.is_cjk() {
            w.s(&format!("{:04x}", walk.c));
        } else {
            push_escaped(w, walk.c);
        }
    }
    if let Some(enc) = last {
        close(w, enc);
    }
}

fn is_newline(b: Option<&u8>) -> bool {
    matches!(b, Some(b'\n' | b'\r'))
}

/// A line as `break_string` breaks it.
struct Line {
    start: usize,
    /// Before its newline, if it has one.
    end: usize,
    width: f32,
    newline: bool,
    /// Where the next line starts.
    next: usize,
}

/// The lines of `text` `maxw` wide.
fn lines(lang: i32, font: &Font, size: f32, text: &str, maxw: f32) -> Vec<Line> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut a = 0;
    while a < text.len() {
        let (width, n) = break_string(lang, font, size, &text[a..], maxw);
        let mut b = a + n;
        if b <= a {
            break;
        }
        let newline = is_newline(bytes.get(b - 1));
        let end = if newline { b - 1 } else { b };
        if bytes.get(b - 1) == Some(&b'\r') && bytes.get(b) == Some(&b'\n') {
            b += 1;
        }
        out.push(Line {
            start: a,
            end,
            width,
            newline,
            next: b,
        });
        a = b;
    }
    out
}

/// `write_string_with_quadding`: the text broken into lines `maxw` wide,
/// each aligned by `q` (0 left, 1 centre, 2 right).
#[allow(clippy::too_many_arguments)]
pub fn write_string_with_quadding(
    w: &mut Writer,
    lang: i32,
    fontname: &str,
    font: &Font,
    size: f32,
    lineheight: f32,
    text: &str,
    maxw: f32,
    q: i32,
) {
    let mut px = 0.0f32;
    for line in lines(lang, font, size, text, maxw) {
        let x = match q {
            0 => 0.0,
            1 => (maxw - line.width) / 2.0,
            _ => maxw - line.width,
        };
        w.op(&[x - px, -lineheight], "Td");
        let (a, end) = (line.start, line.end);
        write_string(w, lang, font, fontname, size, &text[a..], end - a);
        px = x;
    }
}

/// `write_comb_string`: each character centred in its cell of a comb
/// field, `cell_w` thousandths of an em wide.
#[allow(clippy::too_many_arguments)]
pub fn write_comb_string(
    w: &mut Writer,
    lang: i32,
    fontname: &str,
    font: &Font,
    size: f32,
    text: &str,
    cell_w: f32,
) {
    let mut walk = Walk::new(lang, font, text, None);
    let mut last: Option<Enc> = None;
    let mut carry = 0.0f32;
    while walk.advance() {
        if last != Some(walk.enc) {
            if last.is_some() {
                w.s("] TJ\n");
            }
            select_font(w, walk.enc, fontname, size);
            w.buf.push(b'[');
            last = Some(walk.enc);
        }
        let pad = (-walk.w).mul_add(1000.0, cell_w) / 2.0;
        w.g(-(carry + pad));
        carry = pad;
        if walk.enc.is_cjk() {
            w.s(&format!("<{:04x}>", walk.c));
        } else {
            w.buf.push(b'(');
            push_escaped(w, walk.c);
            w.buf.push(b')');
        }
    }
    if last.is_some() {
        w.s("] TJ\n");
    }
}

/// How text is set in a box: the parameters `write_variable_text` and
/// `layout_variable_text` share.
#[derive(Debug, Clone, Copy)]
pub struct TextBox {
    pub size: f32,
    pub q: i32,
    pub w: f32,
    pub h: f32,
    pub padding: f32,
    /// Baseline and line height, in ems.
    pub baseline: f32,
    pub lineheight: f32,
    pub multiline: bool,
    pub comb: i32,
    pub adjust_baseline: bool,
}

/// The box less its padding, the font size (0 is fitted), and the baseline
/// and line height at that size.
struct Fitted {
    w: f32,
    h: f32,
    size: f32,
    baseline: f32,
    lineheight: f32,
}

fn fit(lang: i32, font: &Font, text: &str, b: &TextBox) -> Fitted {
    let w = b.w - b.padding * 2.0;
    let h = b.h - b.padding * 2.0;
    let mut size = b.size;
    if size == 0.0 {
        if b.multiline {
            size = 12.0;
        } else {
            let ms = measure_string(lang, font, text);
            size = if ms != 0.0 { w / ms } else { 12.0 };
            if size > h {
                size = h;
            }
        }
    }
    let lineheight = size * b.lineheight;
    let mut baseline = size * b.baseline;
    if b.adjust_baseline {
        // Make sure the baseline is inside the rectangle.
        if 0.2f32.mul_add(size, baseline) > h {
            baseline = (-0.2f32).mul_add(size, h);
        }
    }
    Fitted {
        w,
        h,
        size,
        baseline,
        lineheight,
    }
}

/// `write_variable_text`: the text in `fontname` (one of MuPDF's base-14
/// resource names) set in the box, with the font resources it needs.
pub fn write_variable_text(
    w: &mut Writer,
    lang: i32,
    text: &str,
    fontname: &str,
    color: &[f32],
    b: &TextBox,
) -> Result<(), String> {
    let (fontname, base) = fonts::full_font_name(fontname);
    let font = fonts::base14(base)?;
    let mut res_font = Dict::new();
    fonts::add_required_fonts(&mut res_font, lang, &font, base, fontname, text);
    w.res
        .get_or_insert_with(Dict::new)
        .put(b"Font", Object::Dict(Arc::new(res_font)));
    let f = fit(lang, &font, text, b);
    w.s("BT\n");
    w.color0(color, false);
    let padding = b.padding;
    if b.multiline {
        w.op(&[padding, padding + f.h - f.baseline + f.lineheight], "Td");
        write_string_with_quadding(
            w,
            lang,
            fontname,
            &font,
            f.size,
            f.lineheight,
            text,
            f.w,
            b.q,
        );
    } else if b.comb > 0 {
        let ty = (f.h - f.size) / 2.0;
        w.op(&[padding, padding + f.h - f.baseline - ty], "Td");
        let cell = (f.w * 1000.0 / f.size) / b.comb as f32;
        write_comb_string(w, lang, fontname, &font, f.size, text, cell);
    } else {
        let mut tx = 0.0;
        let ty = (f.h - f.size) / 2.0;
        if b.q > 0 {
            let tw = measure_string(lang, &font, text) * f.size;
            tx = if b.q == 1 { (f.w - tw) / 2.0 } else { f.w - tw };
        }
        w.op(&[padding + tx, padding + f.h - f.baseline - ty], "Td");
        write_string(w, lang, &font, fontname, f.size, text, text.len());
    }
    w.s("ET\n");
    Ok(())
}

/// `fz_layout_char`: a character's position and advance, and its byte
/// offset in the text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutChar {
    pub x: f32,
    pub advance: f32,
    pub offset: usize,
}

/// `fz_layout_line`.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutLine {
    pub x: f32,
    pub y: f32,
    pub font_size: f32,
    pub offset: usize,
    pub chars: Vec<LayoutChar>,
}

/// `fz_layout_block`'s lines, as an editor places the cursor in text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    pub lines: Vec<LayoutLine>,
}

impl Layout {
    fn line(&mut self, x: f32, y: f32, font_size: f32, offset: usize) {
        self.lines.push(LayoutLine {
            x,
            y,
            font_size,
            offset,
            chars: Vec::new(),
        });
    }

    fn char(&mut self, x: f32, advance: f32, offset: usize) {
        if let Some(line) = self.lines.last_mut() {
            line.chars.push(LayoutChar { x, advance, offset });
        }
    }
}

/// `layout_string`: `text[a..b]` as one line at (x, y).
#[allow(clippy::too_many_arguments)]
fn layout_string(
    out: &mut Layout,
    lang: i32,
    font: &Font,
    size: f32,
    mut x: f32,
    y: f32,
    text: &str,
    a: usize,
    b: usize,
) {
    out.line(x, y, size, a);
    let mut walk = Walk::new(lang, font, &text[a..], Some(b - a));
    while walk.advance() {
        out.char(x, walk.w * size, a + walk.pos);
        x = walk.w.mul_add(size, x);
    }
}

/// `layout_string_with_quadding`.
#[allow(clippy::too_many_arguments)]
fn layout_string_with_quadding(
    out: &mut Layout,
    lang: i32,
    font: &Font,
    size: f32,
    lineheight: f32,
    xorig: f32,
    mut y: f32,
    text: &str,
    maxw: f32,
    q: i32,
) {
    let mut x = 0.0f32;
    let mut add_line_at_end = text.is_empty();
    let mut next = 0;
    for line in lines(lang, font, size, text, maxw) {
        if q > 0 {
            x = if q == 1 {
                (maxw - line.width) / 2.0
            } else {
                maxw - line.width
            };
        }
        layout_string(
            out,
            lang,
            font,
            size,
            xorig + x,
            y,
            text,
            line.start,
            line.end,
        );
        add_line_at_end = line.newline;
        next = line.next;
        y -= lineheight;
    }
    if add_line_at_end {
        out.line(xorig, y, size, next);
    }
}

/// `layout_comb_string`.
#[allow(clippy::too_many_arguments)]
fn layout_comb_string(
    out: &mut Layout,
    mut x: f32,
    y: f32,
    text: &str,
    font: &Font,
    size: f32,
    cell_w: f32,
) {
    if text.is_empty() {
        out.line(x + cell_w / 2.0, y, size, 0);
    }
    let mut first = true;
    for (offset, ch) in text.char_indices() {
        let mut c = glyphs::windows_1252_from_unicode(ch as i32);
        if c < 0 {
            c = REPLACEMENT;
        }
        let g = fonts::encode_character(font, c);
        let w = fonts::advance(font, g) * size;
        if first {
            out.line(x + (cell_w - w) / 2.0, y, size, offset);
            first = false;
        }
        out.char(x + (cell_w - w) / 2.0, w, offset);
        x += cell_w;
    }
}

/// `layout_variable_text`: where `write_variable_text` puts each line and
/// character, in a box at (x, y).
pub fn layout_variable_text(
    lang: i32,
    text: &str,
    fontname: &str,
    x: f32,
    y: f32,
    b: &TextBox,
) -> Result<Layout, String> {
    let (_, base) = fonts::full_font_name(fontname);
    let font = fonts::base14(base)?;
    let f = fit(lang, &font, text, b);
    let mut out = Layout::default();
    let padding = b.padding;
    if b.multiline {
        layout_string_with_quadding(
            &mut out,
            lang,
            &font,
            f.size,
            f.lineheight,
            x + padding,
            y + (padding + f.h - f.baseline),
            text,
            f.w,
            b.q,
        );
    } else if b.comb > 0 {
        let ty = (f.h - f.size) / 2.0;
        let y = y + (padding + f.h - f.baseline - ty);
        layout_comb_string(
            &mut out,
            x + padding,
            y,
            text,
            &font,
            f.size,
            f.w / b.comb as f32,
        );
    } else {
        let mut tx = 0.0;
        let ty = (f.h - f.size) / 2.0;
        if b.q > 0 {
            let tw = measure_string(lang, &font, text) * f.size;
            tx = if b.q == 1 { (f.w - tw) / 2.0 } else { f.w - tw };
        }
        let x = x + (padding + tx);
        let y = y + (padding + f.h - f.baseline - ty);
        layout_string(&mut out, lang, &font, f.size, x, y, text, 0, text.len());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn helv() -> Arc<Font> {
        fonts::base14("Helvetica").unwrap()
    }

    #[test]
    fn break_string_breaks_after_the_last_space_that_fits() {
        let font = helv();
        let text = "aaa bbb ccc";
        let w = measure_string(0, &font, "aaa bbb ") * 10.0;
        let (_, next) = break_string(0, &font, 10.0, text, w);
        assert_eq!(&text[..next], "aaa bbb ");
        // A word longer than the line is not broken.
        let (_, next) = break_string(0, &font, 10.0, "aaaaaaaa", 1.0);
        assert_eq!(next, 8);
        // A newline ends the line and is part of it.
        let (_, next) = break_string(0, &font, 10.0, "ab\ncd", 1000.0);
        assert_eq!(next, 3);
    }

    #[test]
    fn crlf_is_one_line_break() {
        let font = helv();
        let ls = lines(0, &font, 10.0, "one\r\ntwo\rthree", 1000.0);
        let spans: Vec<(usize, usize, bool, usize)> = ls
            .iter()
            .map(|l| (l.start, l.end, l.newline, l.next))
            .collect();
        assert_eq!(
            spans,
            [(0, 3, true, 5), (5, 8, true, 9), (9, 14, false, 14)]
        );
    }

    #[test]
    fn walk_picks_the_font_by_script() {
        let font = helv();
        let mut walk = Walk::new(0, &font, "a\u{3b2}1\u{65e5}\u{5d0}", None);
        let mut got = Vec::new();
        while walk.advance() {
            got.push((walk.enc, walk.c));
        }
        // The digit keeps the script before it; Hebrew has no font.
        assert_eq!(
            got,
            [
                (Enc::Latin, 0x61),
                (Enc::Greek, 0xe2),
                (Enc::Greek, 0x31),
                (Enc::Japanese, 0x65e5),
                (Enc::Latin, REPLACEMENT),
            ]
        );
    }

    #[test]
    fn layout_places_lines_and_characters() {
        let b = TextBox {
            size: 10.0,
            q: 0,
            w: 1000.0,
            h: 100.0,
            padding: 0.0,
            baseline: 0.8,
            lineheight: 1.2,
            multiline: true,
            comb: 0,
            adjust_baseline: false,
        };
        let out = layout_variable_text(0, "ab\ncd\n", "Helv", 0.0, 0.0, &b).unwrap();
        let lines: Vec<(f32, usize, usize)> = out
            .lines
            .iter()
            .map(|l| (l.y, l.offset, l.chars.len()))
            .collect();
        // Two lines, and the empty one after the last newline.
        assert_eq!(lines, [(92.0, 0, 2), (80.0, 3, 2), (68.0, 6, 0)]);
        let (a, c) = (out.lines[0].chars[0], out.lines[0].chars[1]);
        assert_eq!((a.x, a.offset, c.offset), (0.0, 0, 1));
        assert_eq!(c.x, a.advance);
    }
}
