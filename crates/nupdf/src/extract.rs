// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! What `nupdf extract` makes of structured text: the spans of a line,
//! the text of each level, and Markdown. Content ids follow the order of
//! [`StextPage`]: `p3` is page 3, `p3.b2` its second block, `p3.b2.l4` that
//! block's fourth line and `p3.b2.l4.s1` the line's first span, all
//! counted from 1 (DECISIONS D35).

use crate::geometry::Rect;
use crate::stext::{Block, StextChar, StextLine, StextPage, TextBlock, char_flags, rune_char};

/// What a span's characters share.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    /// The font's name, as MuPDF keeps it.
    pub font: Vec<u8>,
    pub size: f32,
    /// The character's bold flag: the font is bold, or text was drawn
    /// twice to look it.
    pub bold: bool,
    pub italic: bool,
    pub argb: u32,
}

impl Style {
    fn of(ch: &StextChar) -> Style {
        Style {
            font: ch.font.name.clone(),
            size: ch.size,
            bold: ch.flags & char_flags::BOLD != 0,
            italic: ch.font.is_italic,
            argb: ch.argb,
        }
    }
}

/// A run of a line's characters in one style: `start..end` in the line.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub style: Style,
    pub bbox: Rect,
}

/// A line's spans. Spaces the device adds take the style of the
/// characters around them, so they never start a span of their own.
pub fn spans(line: &StextLine) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    for (i, ch) in line.chars.iter().enumerate() {
        let bbox = Rect::from_quad(ch.quad);
        let synthetic = ch.flags & char_flags::SYNTHETIC != 0;
        match out.last_mut() {
            Some(span) if synthetic || span.style == Style::of(ch) => {
                span.end = i + 1;
                span.bbox = span.bbox.union(bbox);
            }
            _ => out.push(Span {
                start: i,
                end: i + 1,
                style: Style::of(ch),
                bbox,
            }),
        }
    }
    out
}

/// The text of characters, as MuPDF writes each.
pub fn chars_text(chars: &[StextChar]) -> String {
    chars.iter().map(|c| rune_char(c.c)).collect()
}

/// A block's text as `fz_print_stext_page_as_text` writes it, without the
/// empty line that ends the block.
pub fn block_text(block: &TextBlock) -> String {
    let mut out = String::new();
    crate::stext::block_text(block, &mut out);
    out
}

/// Characters of text a block holds, for budgets.
pub fn block_len(block: &Block) -> usize {
    match block {
        Block::Text(t) => t.lines.iter().map(|l| l.chars.len()).sum(),
        Block::Struct(s) => s.blocks.iter().map(block_len).sum(),
        Block::Image(_) | Block::Vector(_) => 0,
    }
}

/// The text blocks among `blocks`, those within structures in place.
pub fn text_blocks<'a>(blocks: &[&'a Block]) -> Vec<&'a TextBlock> {
    fn walk<'a>(b: &'a Block, out: &mut Vec<&'a TextBlock>) {
        match b {
            Block::Text(t) => out.push(t),
            Block::Struct(s) => s.blocks.iter().for_each(|c| walk(c, out)),
            Block::Image(_) | Block::Vector(_) => {}
        }
    }
    let mut out = Vec::new();
    for b in blocks {
        walk(b, &mut out);
    }
    out
}

/// The page's body text size: the size most of its characters have,
/// spaces left out, to the nearest half point.
pub fn body_size(blocks: &[&Block]) -> f32 {
    let mut counts: Vec<(i32, usize)> = Vec::new();
    for t in text_blocks(blocks) {
        for ch in t.lines.iter().flat_map(|l| &l.chars) {
            if ch.c == ' ' as i32 {
                continue;
            }
            let k = (ch.size * 2.0).round() as i32;
            match counts.iter_mut().find(|(s, _)| *s == k) {
                Some((_, n)) => *n += 1,
                None => counts.push((k, 1)),
            }
        }
    }
    // The most common size; among equals, the smallest.
    counts
        .iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map_or(0.0, |&(k, _)| k as f32 / 2.0)
}

/// The characters Markdown would read as syntax, escaped.
fn escape(text: &str, line_start: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, c) in text.chars().enumerate() {
        let lead = line_start && i == 0;
        if matches!(c, '\\' | '*' | '_' | '`' | '[' | ']' | '<' | '>')
            || (lead && matches!(c, '#' | '+' | '-' | '|' | '='))
        {
            out.push('\\');
        }
        out.push(c);
    }
    // A leading "1." would start an ordered list.
    if line_start
        && let Some(dot) = out.find('.')
        && dot > 0
        && out[..dot].bytes().all(|b| b.is_ascii_digit())
    {
        out.insert(dot, '\\');
    }
    out
}

const BULLETS: &[char] = &[
    '\u{2022}', '\u{2023}', '\u{25E6}', '\u{2043}', '\u{2219}', '\u{00B7}', '\u{25AA}', '\u{25CF}',
];

/// One block as Markdown: a heading when its text is clearly larger than
/// the page's body text, list items for lines that start with a bullet,
/// otherwise a paragraph whose lines are joined by spaces (or by nothing,
/// where a hyphen was taken out). Bold and italic runs are marked.
/// Gives the heading level (0 for none), the block's size and its
/// Markdown, without the heading's marks.
fn block_markdown(block: &TextBlock, body: f32) -> (usize, f32, String) {
    // The block's own size: the largest of its characters but spaces.
    let size = block
        .lines
        .iter()
        .flat_map(|l| &l.chars)
        .filter(|c| c.c != ' ' as i32)
        .map(|c| c.size)
        .fold(0.0f32, f32::max);
    let level = if body > 0.0 {
        let r = size / body;
        if r >= 1.8 {
            1
        } else if r >= 1.4 {
            2
        } else if r >= 1.2 {
            3
        } else {
            0
        }
    } else {
        0
    };
    // Each item is a paragraph, a heading or a list item: its text with
    // inline marks.
    let mut items: Vec<(bool, String)> = Vec::new();
    // The line before was joined to this one: no space between.
    let mut glue = false;
    // Where the current list item's text starts.
    let mut item_x: Option<f32> = None;
    for line in &block.lines {
        let joined = line.flags & crate::stext::LINE_JOINED != 0;
        let mut chars = line.chars.as_slice();
        if joined && !chars.is_empty() {
            chars = &chars[..chars.len() - 1];
        }
        let text = chars_text(chars);
        let trimmed = text.trim_start();
        let bullet = level == 0
            && trimmed.chars().next().is_some_and(|c| BULLETS.contains(&c))
            && trimmed.chars().nth(1).is_some_and(char::is_whitespace);
        let skip = if bullet {
            text.len() - trimmed.len() + trimmed.chars().next().map_or(0, char::len_utf8)
        } else {
            0
        };
        let marked = mark_line(line, chars.len(), skip, level == 0);
        let marked = if bullet {
            marked.trim_start().to_string()
        } else {
            marked
        };
        // Where the line's text starts: after the bullet and its space.
        let first_x = |chars: &[StextChar]| {
            chars
                .iter()
                .find(|c| c.c != ' ' as i32 && !BULLETS.contains(&rune_char(c.c)))
                .map(|c| c.origin.x)
        };
        let x = first_x(chars);
        // A line under a list item continues it only when it is indented
        // to the item's text.
        let continues = match (items.last(), item_x, x) {
            (Some((true, _)), Some(ix), Some(x)) if line.wmode == 0 => {
                x >= ix - 0.5 * line_size(line)
            }
            (Some(_), ..) => true,
            (None, ..) => false,
        };
        if bullet {
            item_x = x;
        } else if !continues {
            item_x = None;
        }
        match items.last_mut() {
            Some((_, prev)) if !bullet && continues => {
                if !glue {
                    prev.push(' ');
                }
                prev.push_str(&marked);
            }
            _ => items.push((bullet, marked)),
        }
        glue = joined;
    }
    let mut out = String::new();
    let mut last_bullet = None;
    for (bullet, text) in items {
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        // List items run on; anything else stands apart, as a paragraph
        // after a list item would otherwise belong to it.
        match last_bullet {
            Some(true) if bullet => out.push('\n'),
            Some(_) => out.push_str("\n\n"),
            None => {}
        }
        last_bullet = Some(bullet);
        if bullet && level == 0 {
            out.push_str("- ");
        }
        out.push_str(text);
    }
    (level, size, out)
}

/// The size of a line's first character.
fn line_size(line: &StextLine) -> f32 {
    line.chars.first().map_or(0.0, |c| c.size)
}

/// A line's text from byte `skip` of its first `n` characters' text, with
/// bold and italic runs marked when `marks`.
fn mark_line(line: &StextLine, n: usize, skip: usize, marks: bool) -> String {
    // The line's runs as (mark, escaped text), from byte `skip` on.
    let mut runs: Vec<(&str, String)> = Vec::new();
    let mut at = 0usize;
    for span in spans(line) {
        let end = span.end.min(n);
        if span.start >= end {
            continue;
        }
        let raw = chars_text(&line.chars[span.start..end]);
        let len = raw.len();
        let raw = if at + len <= skip {
            at += len;
            continue;
        } else if at < skip {
            raw[skip - at..].to_string()
        } else {
            raw
        };
        at += len;
        let mark = match (marks, span.style.bold, span.style.italic) {
            (true, true, true) => "***",
            (true, true, false) => "**",
            (true, false, true) => "*",
            _ => "",
        };
        // Only words are marked: a lone symbol from a math font is not.
        let mark = if raw.chars().any(char::is_alphanumeric) {
            mark
        } else {
            ""
        };
        let text = escape(&raw, runs.is_empty());
        match runs.last_mut() {
            // Neighbouring runs in one style, or spaces between them, are
            // one run.
            Some((m, prev)) if *m == mark || text.trim().is_empty() => prev.push_str(&text),
            Some((m, prev)) if prev.trim().is_empty() => {
                prev.push_str(&text);
                *m = mark;
            }
            _ => runs.push((mark, text)),
        }
    }
    let mut out = String::new();
    for (mark, text) in runs {
        let core = text.trim();
        if mark.is_empty() || core.is_empty() {
            out.push_str(&text);
            continue;
        }
        out.push_str(&text[..text.len() - text.trim_start().len()]);
        out.push_str(mark);
        out.push_str(core);
        out.push_str(mark);
        out.push_str(&text[text.trim_end().len()..]);
    }
    out
}

/// Blocks as Markdown, a blank line between them; image blocks are left
/// out. `body` is the page's body text size ([`body_size`]).
pub fn markdown(blocks: &[&Block], body: f32) -> String {
    // (heading level, size, text): a heading that wraps into blocks of
    // its own size is one heading.
    let mut parts: Vec<(usize, f32, String)> = Vec::new();
    for t in text_blocks(blocks) {
        let (level, size, md) = block_markdown(t, body);
        if md.is_empty() {
            continue;
        }
        match parts.last_mut() {
            Some((l, s, text)) if level > 0 && *l == level && *s == size => {
                text.push(' ');
                text.push_str(&md);
            }
            _ => parts.push((level, size, md)),
        }
    }
    let parts: Vec<String> = parts
        .into_iter()
        .map(|(level, _, text)| {
            if level > 0 {
                format!("{} {}", "#".repeat(level), text.replace('\n', " "))
            } else {
                text
            }
        })
        .collect();
    parts.join("\n\n")
}

/// A page's Markdown.
pub fn page_markdown(page: &StextPage) -> String {
    let blocks: Vec<&Block> = page.blocks.iter().collect();
    markdown(&blocks, body_size(&blocks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::Store;
    use crate::document::Document;
    use crate::stext::{Options, extract_page};

    /// A one-page document with Helvetica as F1 and Helvetica-Bold as F2.
    fn page(content: &str) -> StextPage {
        let pdf = format!(
            "%PDF-1.7\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
             2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
             3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 400 400]/Contents 4 0 R/Resources<</Font<<\
             /F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>\
             /F2<</Type/Font/Subtype/Type1/BaseFont/Helvetica-Bold>>>>>>>>endobj\n\
             4 0 obj<</Length {}>>stream\n{content}\nendstream endobj\n\
             trailer<</Root 1 0 R/Size 5>>\n",
            content.len()
        );
        let doc = Document::open(pdf.into_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
        let obj = doc.page(0).unwrap_or_else(|e| panic!("{e:?}"));
        extract_page(&doc, &obj, Options::default(), Store::default())
            .0
            .unwrap_or_else(|e| panic!("{e}"))
    }

    fn lines(p: &StextPage) -> Vec<&StextLine> {
        p.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.lines.iter()),
                Block::Image(_) | Block::Vector(_) | Block::Struct(_) => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn a_change_of_font_starts_a_span_and_added_spaces_do_not() {
        let p = page("BT /F1 10 Tf 20 380 Td [(plain) -300 (words)] TJ /F2 10 Tf ( bold) Tj ET");
        let line = lines(&p)[0];
        let s = spans(line);
        assert_eq!(s.len(), 2);
        assert_eq!(chars_text(&line.chars[s[0].start..s[0].end]), "plain words");
        assert!(!s[0].style.bold);
        assert_eq!(chars_text(&line.chars[s[1].start..s[1].end]), " bold");
        assert!(s[1].style.bold);
    }

    #[test]
    fn markdown_finds_headings_lists_and_bold_runs() {
        let p = page(
            "BT /F2 20 Tf 20 370 Td (A Title) Tj ET \
             BT /F1 10 Tf 20 330 Td (Body text with a ) Tj /F2 10 Tf (bold) Tj /F1 10 Tf ( word.) Tj \
             0 -12 Td (More body text here.) Tj ET \
             BT /F1 10 Tf 20 280 Td (\\225 first item) Tj 0 -12 Td (\\225 second item) Tj ET",
        );
        let md = page_markdown(&p);
        assert_eq!(
            md,
            "# A Title\n\nBody text with a **bold** word. More body text here.\n\n- first item\n- second item"
        );
    }

    #[test]
    fn the_body_size_is_the_most_common_one() {
        let p = page(
            "BT /F1 20 Tf 20 370 Td (Big) Tj ET BT /F1 9 Tf 20 330 Td (small body text) Tj ET",
        );
        let blocks: Vec<&Block> = p.blocks.iter().collect();
        assert_eq!(body_size(&blocks), 9.0);
    }

    #[test]
    fn escaping_keeps_markdown_syntax_out() {
        assert_eq!(escape("a*b_c", false), "a\\*b\\_c");
        assert_eq!(escape("# not a heading", true), "\\# not a heading");
        assert_eq!(escape("1. not a list", true), "1\\. not a list");
        assert_eq!(escape("1. fine", false), "1. fine");
    }
}
