// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! An improved reading order (`reading-order`, nupdf's own, opt-in per
//! DECISIONS D38): a recursive XY-cut of each list of blocks. Where the
//! blocks leave empty bands across or down the area, the widest band cuts
//! it in two, read top before bottom or left before right, and each part
//! is ordered the same way. Blocks no band separates keep the order the
//! content stream gave them, which is MuPDF's.
//!
//! Three refinements make columns come out whole. A text block whose lines
//! stand on both sides of a gutter (MuPDF puts lines drawn in turn across
//! two columns into one block) is first split at it, when the lines on each
//! side are at least as wide as the gutter: lines of text, not a form's
//! labels and values. Bands are weighed in what they would hold: a band
//! across in lines of text, a band down in characters, taken as half a
//! line wide. And a heading above columns, which can reach into the gutter
//! and narrow it, is taken off first when what is left divides into
//! columns.
//!
//! Scored against the hand-labeled pages in `corpus/reading-order` by
//! `cargo xtask reading-order`, not against MuPDF.

use crate::geometry::Rect;
use crate::stext::{Block, StextPage, TextBlock};

/// Recursion bound: each cut leaves both parts non-empty, so a list of n
/// blocks is cut at most n times; this only guards degenerate input.
const MAX_DEPTH: usize = 512;

/// How many columns deep a block is split.
const MAX_SPLIT_DEPTH: usize = 8;

/// How many blocks a heading (or a footer) taken off columns may have.
const MAX_HEADER: usize = 4;

/// Reorders the page's blocks, and the blocks within each structure.
pub fn reading_order(page: &mut StextPage) {
    order_list(&mut page.blocks, 0);
}

fn order_list(blocks: &mut Vec<Block>, depth: usize) {
    if depth < MAX_DEPTH {
        for b in blocks.iter_mut() {
            if let Block::Struct(s) = b {
                order_list(&mut s.blocks, depth + 1);
            }
        }
    }
    split_columns(blocks);
    let unit = line_height(blocks);
    let boxes: Vec<(usize, Rect)> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (i, b.bbox()))
        .collect();
    // Blocks without a usable box (empty, or not finite) stay where they
    // are relative to the ones before them: they go after them.
    let (placed, loose): (Vec<_>, Vec<_>) = boxes
        .into_iter()
        .partition(|(_, r)| r.is_valid() && [r.x0, r.y0, r.x1, r.y1].iter().all(|v| v.is_finite()));
    let mut order = Vec::with_capacity(blocks.len());
    cut(&placed, &mut order, unit, 0);
    order.extend(loose.iter().map(|&(i, _)| i));
    let mut taken: Vec<Option<Block>> = std::mem::take(blocks).into_iter().map(Some).collect();
    for i in order {
        if let Some(b) = taken.get_mut(i).and_then(Option::take) {
            blocks.push(b);
        }
    }
}

/// The median height of the text lines among `blocks`, structures left
/// aside; 10 where there are none.
fn line_height(blocks: &[Block]) -> f32 {
    let mut h: Vec<f32> = blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(t),
            _ => None,
        })
        .flat_map(|t| &t.lines)
        .map(|l| l.bbox.y1 - l.bbox.y0)
        .filter(|h| h.is_finite() && *h > 0.0)
        .collect();
    if h.is_empty() {
        return 10.0;
    }
    h.sort_by(f32::total_cmp);
    h[h.len() / 2]
}

/// Each text block split where its lines stand on both sides of a gutter.
fn split_columns(blocks: &mut Vec<Block>) {
    if !blocks
        .iter()
        .any(|b| matches!(b, Block::Text(t) if t.lines.len() >= 4))
    {
        return;
    }
    let mut out = Vec::with_capacity(blocks.len());
    for b in std::mem::take(blocks) {
        match b {
            Block::Text(t) if t.lines.len() >= 4 => {
                let mut parts = Vec::new();
                split_text(t, &mut parts, 0);
                out.extend(parts.into_iter().map(Block::Text));
            }
            b => out.push(b),
        }
    }
    *blocks = out;
}

/// The median width of lines.
fn median_width(lines: &[crate::stext::StextLine]) -> f32 {
    let mut w: Vec<f32> = lines.iter().map(|l| l.bbox.x1 - l.bbox.x0).collect();
    w.sort_by(f32::total_cmp);
    w.get(w.len() / 2).copied().unwrap_or(0.0)
}

fn split_text(t: TextBlock, out: &mut Vec<TextBlock>, depth: usize) {
    let finite = t.lines.iter().all(|l| {
        let r = l.bbox;
        r.is_valid() && [r.x0, r.y0, r.x1, r.y1].iter().all(|v| v.is_finite())
    });
    let boxes: Vec<(usize, Rect)> = t
        .lines
        .iter()
        .enumerate()
        .map(|(i, l)| (i, l.bbox))
        .collect();
    let gap = if finite && depth < MAX_SPLIT_DEPTH {
        widest_gap(&boxes, |r| r.x0, |r| r.x1)
    } else {
        None
    };
    let Some((size, at)) = gap else {
        out.push(t);
        return;
    };
    let unit = {
        let mut h: Vec<f32> = t.lines.iter().map(|l| l.bbox.y1 - l.bbox.y0).collect();
        h.sort_by(f32::total_cmp);
        h[h.len() / 2]
    };
    let (left, right): (Vec<_>, Vec<_>) = t.lines.iter().cloned().partition(|l| l.bbox.x1 <= at);
    let columns = size >= unit
        && left.len() >= 2
        && right.len() >= 2
        && median_width(&left) >= size
        && median_width(&right) >= size;
    if !columns {
        out.push(t);
        return;
    }
    for lines in [left, right] {
        let bbox = lines.iter().fold(Rect::EMPTY, |r, l| r.union(l.bbox));
        split_text(
            TextBlock {
                bbox,
                flags: t.flags,
                lines,
            },
            out,
            depth + 1,
        );
    }
}

/// The widest empty band between the boxes along one axis: its size and
/// the position that splits them. `lo`/`hi` give a box's extent on the
/// axis.
fn widest_gap(
    items: &[(usize, Rect)],
    lo: fn(&Rect) -> f32,
    hi: fn(&Rect) -> f32,
) -> Option<(f32, f32)> {
    let mut spans: Vec<(f32, f32)> = items.iter().map(|(_, r)| (lo(r), hi(r))).collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut best: Option<(f32, f32)> = None;
    let mut reach = spans.first()?.1;
    for &(a, b) in &spans[1..] {
        if a > reach {
            let size = a - reach;
            if best.is_none_or(|(s, _)| size > s) {
                best = Some((size, (a + reach) / 2.0));
            }
        }
        reach = reach.max(b);
    }
    best
}

/// Whether a band down of `down` outweighs one across of `across`: a band
/// across counts in lines, one down in characters of half a line.
fn columns_win(down: f32, across: Option<f32>) -> bool {
    2.0 * down > across.unwrap_or(0.0)
}

/// Where a heading of up to [`MAX_HEADER`] blocks, or a footer, ends: the
/// band across that leaves the rest divided into columns, when the rest
/// does not divide so with the heading in.
fn header_cut(items: &[(usize, Rect)], unit: f32) -> Option<f32> {
    let mut by_top: Vec<(usize, Rect)> = items.to_vec();
    by_top.sort_by(|a, b| a.1.y0.total_cmp(&b.1.y0));
    let mut by_bottom: Vec<(usize, Rect)> = items.to_vec();
    by_bottom.sort_by(|a, b| b.1.y1.total_cmp(&a.1.y1));
    for k in 1..=MAX_HEADER.min(items.len().saturating_sub(2)) {
        // A heading: k blocks above everything else.
        let (head, rest) = by_top.split_at(k);
        let bottom = head
            .iter()
            .map(|(_, r)| r.y1)
            .fold(f32::NEG_INFINITY, f32::max);
        let top = rest.iter().map(|(_, r)| r.y0).fold(f32::INFINITY, f32::min);
        if bottom <= top && divides(rest, unit) {
            return Some((bottom + top) / 2.0);
        }
        // A footer: k blocks below everything else.
        let (foot, rest) = by_bottom.split_at(k);
        let top = foot.iter().map(|(_, r)| r.y0).fold(f32::INFINITY, f32::min);
        let bottom = rest
            .iter()
            .map(|(_, r)| r.y1)
            .fold(f32::NEG_INFINITY, f32::max);
        if bottom <= top && divides(rest, unit) {
            return Some((bottom + top) / 2.0);
        }
    }
    None
}

/// Whether the boxes divide into columns: a band down at least a line wide
/// that outweighs any band across.
fn divides(items: &[(usize, Rect)], unit: f32) -> bool {
    let across = widest_gap(items, |r| r.y0, |r| r.y1).map(|(a, _)| a);
    widest_gap(items, |r| r.x0, |r| r.x1).is_some_and(|(d, _)| d >= unit && columns_win(d, across))
}

fn cut(items: &[(usize, Rect)], order: &mut Vec<usize>, unit: f32, depth: usize) {
    if items.len() <= 1 || depth >= MAX_DEPTH {
        order.extend(items.iter().map(|&(i, _)| i));
        return;
    }
    let across = widest_gap(items, |r| r.y0, |r| r.y1);
    let down = widest_gap(items, |r| r.x0, |r| r.x1);
    // The weightier band wins; across (reading on down the page) on a
    // tie. Without columns, a heading may be what keeps them out.
    let split = match (across, down) {
        (_, Some((d, xd))) if columns_win(d, across.map(|(a, _)| a)) => Some((false, xd)),
        _ => match header_cut(items, unit) {
            Some(y) => Some((true, y)),
            None => across.map(|(_, y)| (true, y)),
        },
    };
    let Some((horizontal, at)) = split else {
        // Nothing separates them: the content stream's order.
        order.extend(items.iter().map(|&(i, _)| i));
        return;
    };
    let (first, second): (Vec<_>, Vec<_>) = items
        .iter()
        .partition(|(_, r)| if horizontal { r.y1 <= at } else { r.x1 <= at });
    cut(&first, order, unit, depth + 1);
    cut(&second, order, unit, depth + 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Matrix;
    use crate::stext::ImageBlock;

    fn block(x0: f32, y0: f32, x1: f32, y1: f32) -> Block {
        Block::Image(ImageBlock {
            bbox: Rect::new(x0, y0, x1, y1),
            transform: Matrix::IDENTITY,
        })
    }

    fn order_of(blocks: Vec<Block>) -> Vec<(f32, f32)> {
        let mut page = StextPage {
            mediabox: Rect::new(0.0, 0.0, 600.0, 800.0),
            blocks,
        };
        reading_order(&mut page);
        page.blocks
            .iter()
            .map(|b| (b.bbox().x0, b.bbox().y0))
            .collect()
    }

    #[test]
    fn columns_read_down_before_across() {
        // Drawn row by row; read column by column.
        let got = order_of(vec![
            block(50.0, 100.0, 280.0, 300.0),
            block(320.0, 100.0, 550.0, 300.0),
            block(50.0, 310.0, 280.0, 500.0),
            block(320.0, 310.0, 550.0, 500.0),
        ]);
        assert_eq!(
            got,
            [(50.0, 100.0), (50.0, 310.0), (320.0, 100.0), (320.0, 310.0)]
        );
    }

    #[test]
    fn a_title_across_the_columns_comes_first() {
        let got = order_of(vec![
            block(320.0, 100.0, 550.0, 500.0),
            block(50.0, 100.0, 280.0, 500.0),
            block(50.0, 40.0, 550.0, 70.0),
        ]);
        assert_eq!(got, [(50.0, 40.0), (50.0, 100.0), (320.0, 100.0)]);
    }

    #[test]
    fn rows_of_a_form_read_left_to_right() {
        // The rows are further apart than the fields in a row.
        let got = order_of(vec![
            block(140.0, 220.0, 240.0, 230.0),
            block(50.0, 100.0, 100.0, 110.0),
            block(50.0, 220.0, 100.0, 230.0),
            block(140.0, 100.0, 240.0, 110.0),
        ]);
        assert_eq!(
            got,
            [(50.0, 100.0), (140.0, 100.0), (50.0, 220.0), (140.0, 220.0)]
        );
    }

    #[test]
    fn overlapping_blocks_keep_their_order() {
        let got = order_of(vec![
            block(100.0, 100.0, 300.0, 300.0),
            block(50.0, 50.0, 350.0, 350.0),
        ]);
        assert_eq!(got, [(100.0, 100.0), (50.0, 50.0)]);
    }

    fn line(x0: f32, y0: f32, x1: f32, y1: f32) -> crate::stext::StextLine {
        crate::stext::StextLine {
            wmode: 0,
            flags: 0,
            dir: crate::geometry::Point::new(1.0, 0.0),
            bbox: Rect::new(x0, y0, x1, y1),
            chars: Vec::new(),
        }
    }

    fn text(lines: Vec<crate::stext::StextLine>) -> Block {
        let bbox = lines.iter().fold(Rect::EMPTY, |r, l| r.union(l.bbox));
        Block::Text(crate::stext::TextBlock {
            bbox,
            flags: 0,
            lines,
        })
    }

    /// The lines' left and top edges, in the order the page reads them.
    fn lines_in_order(blocks: Vec<Block>) -> Vec<(f32, f32)> {
        let mut page = StextPage {
            mediabox: Rect::new(0.0, 0.0, 600.0, 800.0),
            blocks,
        };
        reading_order(&mut page);
        let mut out = Vec::new();
        for b in &page.blocks {
            if let Block::Text(t) = b {
                out.extend(t.lines.iter().map(|l| (l.bbox.x0, l.bbox.y0)));
            }
        }
        out
    }

    #[test]
    fn columns_drawn_line_by_line_across_read_column_by_column() {
        // A title, then two columns whose lines the content stream drew in
        // turn, left and right, so that each block holds both columns. The
        // paragraphs leave wider bands across than the gutter between the
        // columns, and the title reaches further right than the left column.
        let left = |y: f32| line(72.0, y, 270.0, y + 14.0);
        let right = |y: f32| line(320.0, y, 510.0, y + 14.0);
        let got = lines_in_order(vec![
            text(vec![line(72.0, 36.0, 310.0, 57.0)]),
            text(vec![
                left(71.0),
                right(71.0),
                left(85.0),
                right(85.0),
                left(99.0),
            ]),
            text(vec![left(197.0), right(197.0), left(211.0), right(211.0)]),
        ]);
        let ys = |x: f32| -> Vec<f32> { got.iter().filter(|p| p.0 == x).map(|p| p.1).collect() };
        assert_eq!(got[0], (72.0, 36.0), "{got:?}");
        assert_eq!(ys(72.0), [36.0, 71.0, 85.0, 99.0, 197.0, 211.0]);
        assert_eq!(ys(320.0), [71.0, 85.0, 197.0, 211.0]);
        // Every left line before every right one.
        let last_left = got.iter().rposition(|p| p.0 == 72.0);
        let first_right = got.iter().position(|p| p.0 == 320.0);
        assert!(last_left < first_right, "{got:?}");
    }

    #[test]
    fn labels_and_values_in_one_block_stay_together_in_rows() {
        // Short labels with their values far to the right: a form, not two
        // columns of text.
        let got = lines_in_order(vec![text(vec![
            line(72.0, 100.0, 110.0, 114.0),
            line(300.0, 100.0, 360.0, 114.0),
            line(72.0, 114.0, 100.0, 128.0),
            line(300.0, 114.0, 380.0, 128.0),
        ])]);
        assert_eq!(
            got,
            [(72.0, 100.0), (300.0, 100.0), (72.0, 114.0), (300.0, 114.0)]
        );
    }
}
