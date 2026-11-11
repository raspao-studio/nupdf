// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! An improved reading order (`reading-order`, nupdf's own, opt-in per
//! DECISIONS D38): a recursive XY-cut of each list of blocks. Where the
//! blocks leave empty bands across or down the area, the widest band cuts
//! it in two, read top before bottom or left before right, and each part
//! is ordered the same way. Blocks no band separates keep the order the
//! content stream gave them, which is MuPDF's.
//!
//! Scored against the hand-labeled pages in `corpus/reading-order` by
//! `cargo xtask reading-order`, not against MuPDF.

use crate::geometry::Rect;
use crate::stext::{Block, StextPage};

/// Recursion bound: each cut leaves both parts non-empty, so a list of n
/// blocks is cut at most n times; this only guards degenerate input.
const MAX_DEPTH: usize = 512;

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
    cut(&placed, &mut order, 0);
    order.extend(loose.iter().map(|&(i, _)| i));
    let mut taken: Vec<Option<Block>> = std::mem::take(blocks).into_iter().map(Some).collect();
    for i in order {
        if let Some(b) = taken.get_mut(i).and_then(Option::take) {
            blocks.push(b);
        }
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

fn cut(items: &[(usize, Rect)], order: &mut Vec<usize>, depth: usize) {
    if items.len() <= 1 || depth >= MAX_DEPTH {
        order.extend(items.iter().map(|&(i, _)| i));
        return;
    }
    let across = widest_gap(items, |r| r.y0, |r| r.y1);
    let down = widest_gap(items, |r| r.x0, |r| r.x1);
    // The wider band wins; across (reading on down the page) on a tie.
    let split = match (across, down) {
        (Some((a, ya)), Some((d, xd))) => Some(if d > a { (false, xd) } else { (true, ya) }),
        (Some((_, y)), None) => Some((true, y)),
        (None, Some((_, x))) => Some((false, x)),
        (None, None) => None,
    };
    let Some((horizontal, at)) = split else {
        // Nothing separates them: the content stream's order.
        order.extend(items.iter().map(|&(i, _)| i));
        return;
    };
    let (first, second): (Vec<_>, Vec<_>) = items
        .iter()
        .partition(|(_, r)| if horizontal { r.y1 <= at } else { r.x1 <= at });
    cut(&first, order, depth + 1);
    cut(&second, order, depth + 1);
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
}
