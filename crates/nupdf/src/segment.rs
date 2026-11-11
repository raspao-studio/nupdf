// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/stext-boxer.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Page segmentation (`segment`): the page split at its widest gaps,
//! across or down, again and again, each part's blocks gathered into a
//! `Div` structure block, as MuPDF's boxer does it.
//!
//! The boxer keeps the rectangles of the page no content touches. MuPDF
//! moves blocks and lines between linked lists while it splits; the page is
//! held here as the same lists, by index, and turned back into blocks at
//! the end.

use crate::geometry::Rect;
use crate::stext::{Block, StextLine, StextPage, TextBlock, VectorBlock, vector_flags};
use crate::stext_tree::{Kind, Lists, PAGE};

/// `MAX_ANALYSIS_DEPTH`.
const MAX_ANALYSIS_DEPTH: usize = 6;

// ---- the boxer ----------------------------------------------------------

/// `boxer_t`: the empty rectangles of an area.
#[derive(Debug, Clone)]
struct Boxer {
    mediabox: Rect,
    list: Vec<Rect>,
    /// How far a rectangle may stick out of another and still count as
    /// inside it: 4 points, or none when `tight`.
    fudge: f64,
    tight: bool,
}

/// `rectlist_append`: `b` added, unless a rectangle already holds it;
/// rectangles it holds are dropped.
fn append(list: &mut Vec<Rect>, fudge: f64, b: Rect) {
    let mut i = 0;
    while i < list.len() {
        let r = list[i];
        let f = |v: f32, d: f64| (f64::from(v) + d) as f32;
        let smaller = Rect::new(
            f(r.x0, fudge),
            f(r.y0, fudge),
            f(r.x1, -fudge),
            f(r.y1, -fudge),
        );
        let larger = Rect::new(
            f(r.x0, -fudge),
            f(r.y0, -fudge),
            f(r.x1, fudge),
            f(r.y1, fudge),
        );
        if larger.contains(b) {
            return;
        }
        if b.contains(smaller) {
            // MuPDF moves the last rectangle into this slot and looks at
            // the slot again.
            list.swap_remove(i);
            continue;
        }
        i += 1;
    }
    list.push(b);
}

impl Boxer {
    /// `boxer_create`.
    fn new(mediabox: Rect, tight: bool) -> Boxer {
        let mut b = Boxer::empty(mediabox, tight);
        append(&mut b.list, b.fudge, mediabox);
        b
    }

    /// `boxer_create_length`.
    fn empty(mediabox: Rect, tight: bool) -> Boxer {
        Boxer {
            mediabox,
            list: Vec::new(),
            fudge: if tight { 0.0 } else { 4.0 },
            tight,
        }
    }

    /// `boxer_feed`: `bbox` marked as taken.
    fn feed(&mut self, bbox: Rect) {
        let m = self.mediabox;
        let mut next = Vec::new();
        for side in [
            Rect::new(m.x0, m.y0, bbox.x0, m.y1),
            Rect::new(bbox.x1, m.y0, m.x1, m.y1),
            Rect::new(m.x0, m.y0, m.x1, bbox.y0),
            Rect::new(m.x0, bbox.y1, m.x1, m.y1),
        ] {
            for &r in &self.list {
                let c = r.intersect(side);
                if c.is_valid() {
                    append(&mut next, self.fudge, c);
                }
            }
        }
        self.list = next;
    }

    /// `boxer_margins`.
    fn margins(&self) -> Rect {
        let mut m = self.mediabox;
        for r in &self.list {
            if r.x0 <= m.x0 && r.y0 <= m.y0 && r.y1 >= m.y1 {
                m.x0 = r.x1;
            } else if r.x1 >= m.x1 && r.y0 <= m.y0 && r.y1 >= m.y1 {
                m.x1 = r.x0;
            } else if r.x0 <= m.x0 && r.x1 >= m.x1 && r.y0 <= m.y0 {
                m.y0 = r.y1;
            } else if r.x0 <= m.x0 && r.x1 >= m.x1 && r.y1 >= m.y1 {
                m.y1 = r.y0;
            }
        }
        m
    }

    /// `boxer_subset`.
    fn subset(&self, rect: Rect) -> Boxer {
        let mut b = Boxer::empty(rect, self.tight);
        b.fudge = self.fudge;
        for &r in &self.list {
            let c = r.intersect(rect);
            if !c.is_empty() {
                append(&mut b.list, b.fudge, c);
            }
        }
        b
    }
}

// ---- feeding ------------------------------------------------------------

/// `line_isnt_all_spaces`.
fn line_isnt_all_spaces(line: &StextLine) -> bool {
    line.chars.iter().any(|c| c.c != 32 && c.c != 160)
}

/// `feed_line`: each word's box, widened by a quarter of its size unless
/// tight.
fn feed_line(boxer: &mut Boxer, line: &StextLine) {
    let chars = &line.chars;
    let mut i = 0;
    while i < chars.len() {
        if chars[i].c == ' ' as i32 {
            i += 1;
            continue;
        }
        let mut r = Rect::EMPTY;
        loop {
            let ch = &chars[i];
            let mut b = Rect::from_quad(ch.quad);
            let margin = if boxer.tight { 0.0 } else { ch.size / 4.0 };
            b.x0 -= margin;
            b.y0 -= margin;
            b.x1 += margin;
            b.y1 += margin;
            r = r.union(b);
            i += 1;
            if i >= chars.len() || chars[i].c == ' ' as i32 {
                break;
            }
        }
        boxer.feed(r);
        // MuPDF steps past the space that ended the word.
        i += 1;
    }
}

/// `fz_collate_small_vector_run`: a thin vector and the ones after it
/// that continue it, as one box; `at` moves to the last of them.
pub(crate) fn collate_small_vector_run(blocks: &[Block], at: &mut usize) -> Rect {
    const MAX_SIZE: f32 = 2.0;
    const MAX_GAP: f32 = 2.0;
    let vector = |i: usize| match blocks.get(i) {
        Some(Block::Vector(v)) => Some(*v),
        _ => None,
    };
    let mut r = blocks.get(*at).map_or(Rect::EMPTY, Block::bbox);
    let (w, h) = (r.x1 - r.x0, r.y1 - r.y0);
    if w < MAX_SIZE {
        while let Some(VectorBlock { bbox: n, .. }) = vector(*at + 1) {
            let joins = n.x0 == r.x0
                && n.x1 == r.x1
                && ((n.y1 > r.y1 && n.y0 <= r.y1 + MAX_GAP)
                    || (n.y0 < r.y0 && n.y1 >= r.y0 - MAX_GAP));
            if !joins {
                break;
            }
            r = r.union(n);
            *at += 1;
        }
    }
    if h < MAX_SIZE {
        while let Some(VectorBlock { bbox: n, .. }) = vector(*at + 1) {
            let joins = n.y0 == r.y0
                && n.y1 == r.y1
                && ((n.x1 > r.x1 && n.x0 <= r.x1 + MAX_GAP)
                    || (n.x0 < r.x0 && n.x1 >= r.x0 - MAX_GAP));
            if !joins {
                break;
            }
            r = r.union(n);
            *at += 1;
        }
    }
    r
}

/// `recurse_and_feed`.
fn feed_blocks(boxer: &mut Boxer, blocks: &[Block]) {
    let mut i = 0;
    while i < blocks.len() {
        match &blocks[i] {
            Block::Text(t) => {
                for line in t.lines.iter().filter(|l| line_isnt_all_spaces(l)) {
                    feed_line(boxer, line);
                }
            }
            Block::Vector(_) => {
                // A point's margin, against hairline cracks between
                // things that should touch.
                let r = collate_small_vector_run(blocks, &mut i).expand(1.0);
                boxer.feed(r);
            }
            Block::Struct(s) => feed_blocks(boxer, &s.blocks),
            // Images and grids, by their boxes.
            b @ (Block::Image(_) | Block::Grid(_)) => boxer.feed(b.bbox()),
        }
        i += 1;
    }
}

// ---- the page as linked lists -------------------------------------------

impl Lists {
    /// `page_subset`: the blocks and lines within `mediabox`, from `parent`
    /// onwards in the page's order, gathered into a new `Div` structure in
    /// the list the first of them came from.
    fn page_subset(&mut self, parent: usize, mediabox: Rect) -> Option<usize> {
        let inside = |r: Rect| {
            r.x0 >= mediabox.x0 && r.y0 >= mediabox.y0 && r.x1 <= mediabox.x1 && r.y1 <= mediabox.y1
        };
        let mut parent = parent;
        let mut block = self.lists[parent].first;
        // The first and last block of the target list, the list the first
        // came from, and the block to insert after.
        let mut target: Option<usize> = None;
        let mut last: Option<usize> = None;
        let mut target_parent = PAGE;
        let mut after: Option<usize> = None;
        // The text block taking the lines of blocks only partly inside.
        let mut newblock: Option<usize> = None;
        let append_target = |l: &mut Lists, last: &mut Option<usize>, n: usize| {
            if let Some(t) = *last {
                l.nodes[t].next = Some(n);
                l.nodes[n].prev = Some(t);
            }
            *last = Some(n);
            l.nodes[n].next = None;
        };
        let mut budget = self.nodes.len().saturating_mul(4) + 16;

        while let Some(b) = block {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let mut next_block = self.nodes[b].next;
            let bbox = self.bbox(b);
            if inside(bbox) {
                let prev = self.nodes[b].prev;
                self.unlink(parent, b);
                if target.is_none() {
                    target = Some(b);
                    target_parent = parent;
                    after = prev;
                    self.nodes[b].prev = None;
                }
                append_target(self, &mut last, b);
            } else if bbox.intersect(mediabox).is_empty() {
            } else if let Kind::Struct { down, .. } = self.nodes[b].kind {
                parent = down;
                next_block = self.lists[down].first;
            } else if let Kind::Leaf(Block::Text(t)) = &mut self.nodes[b].kind {
                let lines = std::mem::take(&mut t.lines);
                let mut kept = Vec::with_capacity(lines.len());
                let mut taken = Vec::new();
                let mut first_taken_at = None;
                for (k, line) in lines.into_iter().enumerate() {
                    if inside(line.bbox) {
                        first_taken_at.get_or_insert(k);
                        taken.push(line);
                    } else {
                        kept.push(line);
                    }
                }
                t.lines = kept;
                if let Some(k) = first_taken_at {
                    let nb = match newblock {
                        Some(nb) => nb,
                        None => {
                            let nb = self.new_node(Kind::Leaf(Block::Text(TextBlock {
                                bbox: Rect::new(0.0, 0.0, 0.0, 0.0),
                                flags: 0,
                                lines: Vec::new(),
                            })));
                            if target.is_none() {
                                target = Some(nb);
                                target_parent = parent;
                                after = if k == 0 { self.nodes[b].prev } else { Some(b) };
                            }
                            append_target(self, &mut last, nb);
                            newblock = Some(nb);
                            nb
                        }
                    };
                    if let Kind::Leaf(Block::Text(t)) = &mut self.nodes[nb].kind {
                        t.lines.extend(taken);
                    }
                }
                if let Some(nb) = newblock {
                    for n in [b, nb] {
                        if let Kind::Leaf(Block::Text(t)) = &mut self.nodes[n].kind {
                            t.bbox = t.lines.iter().fold(Rect::EMPTY, |r, l| r.union(l.bbox));
                        }
                    }
                }
            }
            // Onwards, or up and onwards.
            block = next_block;
            while block.is_none() {
                if parent == PAGE {
                    break;
                }
                block = self.lists[parent].up.and_then(|u| self.nodes[u].next);
                parent = self.lists[parent].parent.unwrap_or(PAGE);
            }
        }

        let target = target?;
        let last = last.unwrap_or(target);

        // The index of the new structure: one past the structure before
        // it; the structures after it renumbered to follow.
        let mut idx = 0;
        let mut block = self.lists[target_parent].first;
        if let Some(a) = after {
            let mut budget = self.nodes.len();
            while let Some(b) = block {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                if let Kind::Struct { index, .. } = self.nodes[b].kind {
                    idx = index + 1;
                }
                if b == a {
                    break;
                }
                block = self.nodes[b].next;
            }
            block = block.and_then(|b| self.nodes[b].next);
        }
        let mut idx2 = idx + 1;
        while let Some(b) = block {
            if let Kind::Struct { index, .. } = &mut self.nodes[b].kind {
                if *index >= idx2 {
                    break;
                }
                *index = idx2;
                idx2 += 1;
            }
            block = self.nodes[b].next;
        }

        // Insert the structure before this block.
        let before = match after {
            Some(a) => self.nodes[a].next,
            None => self.lists[target_parent].first,
        };
        let down = self.new_list(Some(target_parent), "Div", "Split".to_string());
        let sb = self.new_node(Kind::Struct {
            bbox: Rect::EMPTY,
            index: idx,
            down,
        });
        self.lists[down].up = Some(sb);
        match before {
            Some(x) => {
                let prev = self.nodes[x].prev;
                self.nodes[sb].prev = prev;
                self.nodes[sb].next = Some(x);
                match prev {
                    Some(p) => self.nodes[p].next = Some(sb),
                    None => self.lists[target_parent].first = Some(sb),
                }
                self.nodes[x].prev = Some(sb);
            }
            None => {
                let prev = self.lists[target_parent].last;
                self.nodes[sb].prev = prev;
                self.nodes[sb].next = None;
                match prev {
                    Some(p) => self.nodes[p].next = Some(sb),
                    None => self.lists[target_parent].first = Some(sb),
                }
                self.lists[target_parent].last = Some(sb);
            }
        }

        self.lists[down].first = Some(target);
        self.lists[down].last = Some(last);
        self.nodes[target].prev = None;
        let mut bbox = Rect::EMPTY;
        let mut at = Some(target);
        let mut budget = self.nodes.len();
        while let Some(n) = at {
            if budget == 0 {
                break;
            }
            budget -= 1;
            bbox = bbox.union(self.bbox(n));
            if let Kind::Struct { down: d, .. } = self.nodes[n].kind {
                self.lists[d].parent = Some(down);
            }
            at = self.nodes[n].next;
        }
        if let Kind::Struct { bbox: b, .. } = &mut self.nodes[sb].kind {
            *b = bbox;
        }
        Some(down)
    }

    /// `analyse_sub`.
    fn analyse_sub(&mut self, parent: usize, big: &Boxer, depth: usize) -> bool {
        let margins = big.margins();
        let boxer = big.subset(margins);
        let Some(div) = self.page_subset(parent, boxer.mediabox) else {
            return false;
        };
        if depth < MAX_ANALYSIS_DEPTH {
            self.subdivide(div, &boxer, depth + 1);
        }
        true
    }

    /// `boxer_subdivide`: the area split at its widest empty band across
    /// or down, and each side analysed; false when there is none.
    fn subdivide(&mut self, parent: usize, boxer: &Boxer, depth: usize) -> bool {
        let m = boxer.mediabox;
        let mut max_size = 0.0f64;
        let mut largest = None;
        let mut horiz = false;
        for (i, r) in boxer.list.iter().enumerate() {
            if r.x0 <= m.x0 && r.x1 >= m.x1 {
                let size = f64::from(r.y1 - r.y0);
                if size > max_size {
                    max_size = size;
                    largest = Some(i);
                    horiz = true;
                }
            }
            if r.y0 <= m.y0 && r.y1 >= m.y1 {
                let size = f64::from(r.x1 - r.x0);
                if size > max_size {
                    max_size = size;
                    largest = Some(i);
                    horiz = false;
                }
            }
        }
        let Some(largest) = largest else {
            return false;
        };
        let gap = boxer.list[largest];
        let (first, second) = if horiz {
            (
                Rect { y1: gap.y0, ..m },
                Rect {
                    y0: gap.y1,
                    y1: m.y1,
                    ..m
                },
            )
        } else {
            (
                Rect { x1: gap.x0, ..m },
                Rect {
                    x0: gap.x1,
                    x1: m.x1,
                    ..m
                },
            )
        };
        for r in [first, second] {
            let sub = boxer.subset(r);
            self.analyse_sub(parent, &sub, depth);
        }
        true
    }
}

/// `fz_stext_remove_page_fill`: a white or invisible rectangle filling
/// most of the page, at its start, dropped; true if one was. MuPDF means
/// to drop several, but the removed block's link is cleared before its
/// iterator steps on, so the walk ends after the first.
pub fn remove_page_fill(page: &mut StextPage) -> bool {
    let mut coverage = Rect::EMPTY;
    dfs(&page.blocks, &mut |b| {
        coverage = coverage.union(page.mediabox.intersect(b.bbox()))
    });
    let Some(Block::Vector(v)) = page.blocks.first() else {
        return false;
    };
    let v = *v;
    if v.flags & vector_flags::IS_RECTANGLE == 0 || v.flags & vector_flags::IS_STROKED != 0 {
        return false;
    }
    if v.argb & 0xff00_0000 != 0 && v.argb & 0xff_ffff != 0xff_ffff {
        return false;
    }
    // Allowing for rounding.
    if !v.bbox.expand(0.1).contains(coverage) {
        return false;
    }
    let m = page.mediabox;
    if !m.is_infinite()
        && ((v.bbox.y1 - v.bbox.y0) < 0.9 * (m.y1 - m.y0)
            || (v.bbox.x1 - v.bbox.x0) < 0.9 * (m.x1 - m.x0))
    {
        return false;
    }
    page.blocks.remove(0);
    true
}

/// The blocks that are not structures, depth first, as
/// `fz_stext_page_block_iterator_next_dfs` walks them.
fn dfs(blocks: &[Block], f: &mut dyn FnMut(&Block)) {
    for b in blocks {
        match b {
            Block::Struct(s) => dfs(&s.blocks, f),
            b => f(b),
        }
    }
}

/// `fz_segment_stext_page`: false when the page already has structure,
/// which segmenting cannot hope to beat, or no part of it has content.
pub fn segment_page(page: &mut StextPage) -> bool {
    remove_page_fill(page);
    if page.blocks.iter().any(|b| matches!(b, Block::Struct(_))) {
        return false;
    }
    let mut boxer = Boxer::new(page.mediabox, false);
    feed_blocks(&mut boxer, &page.blocks);
    let mut lists = Lists::new(std::mem::take(&mut page.blocks));
    let done = lists.analyse_sub(PAGE, &boxer, 0);
    page.blocks = lists.into_blocks();
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Matrix;
    use crate::stext::ImageBlock;

    fn image(x0: f32, y0: f32, x1: f32, y1: f32) -> Block {
        Block::Image(ImageBlock {
            bbox: Rect::new(x0, y0, x1, y1),
            transform: Matrix::IDENTITY,
        })
    }

    fn fill(r: Rect, argb: u32) -> Block {
        Block::Vector(VectorBlock {
            bbox: r,
            flags: vector_flags::IS_RECTANGLE,
            argb,
        })
    }

    /// The page's structure as a string: `D(...)` for a Div, `i` an
    /// image, `v` a vector.
    fn shape(blocks: &[Block]) -> String {
        blocks
            .iter()
            .map(|b| match b {
                Block::Struct(s) => format!("D{}({})", s.index, shape(&s.blocks)),
                Block::Image(_) => "i".into(),
                Block::Vector(_) => "v".into(),
                Block::Text(_) => "t".into(),
                Block::Grid(_) => "g".into(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn rectangles_inside_others_are_not_kept() {
        let mut list = Vec::new();
        append(&mut list, 0.0, Rect::new(0.0, 0.0, 10.0, 10.0));
        append(&mut list, 0.0, Rect::new(2.0, 2.0, 5.0, 5.0));
        assert_eq!(list.len(), 1);
        // One that holds the others replaces them.
        append(&mut list, 0.0, Rect::new(20.0, 0.0, 30.0, 10.0));
        append(&mut list, 0.0, Rect::new(-1.0, -1.0, 40.0, 11.0));
        assert_eq!(list, [Rect::new(-1.0, -1.0, 40.0, 11.0)]);
        // With a fudge, sticking out a little still counts as inside.
        let mut list = vec![Rect::new(0.0, 0.0, 10.0, 10.0)];
        append(&mut list, 4.0, Rect::new(-3.0, 0.0, 12.0, 10.0));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn feeding_a_box_leaves_the_space_around_it() {
        let mut b = Boxer::new(Rect::new(0.0, 0.0, 100.0, 100.0), true);
        b.feed(Rect::new(40.0, 40.0, 60.0, 60.0));
        assert_eq!(
            b.list,
            [
                Rect::new(0.0, 0.0, 40.0, 100.0),
                Rect::new(60.0, 0.0, 100.0, 100.0),
                Rect::new(0.0, 0.0, 100.0, 40.0),
                Rect::new(0.0, 60.0, 100.0, 100.0),
            ]
        );
        assert_eq!(b.margins(), Rect::new(40.0, 40.0, 60.0, 60.0));
    }

    #[test]
    fn columns_split_down_the_gap_between_them() {
        let mut page = StextPage {
            mediabox: Rect::new(0.0, 0.0, 600.0, 800.0),
            blocks: vec![
                image(50.0, 50.0, 250.0, 750.0),
                image(350.0, 50.0, 550.0, 750.0),
            ],
        };
        assert!(segment_page(&mut page));
        assert_eq!(shape(&page.blocks), "D0(D0(i) D1(i))");
        let Block::Struct(top) = &page.blocks[0] else {
            panic!("no structure");
        };
        assert_eq!((top.standard, top.raw.as_str()), ("Div", "Split"));
        assert_eq!(top.bbox, Rect::new(50.0, 50.0, 550.0, 750.0));
    }

    #[test]
    fn rows_split_across() {
        let mut page = StextPage {
            mediabox: Rect::new(0.0, 0.0, 600.0, 800.0),
            blocks: vec![
                image(50.0, 50.0, 550.0, 300.0),
                image(50.0, 500.0, 550.0, 750.0),
            ],
        };
        assert!(segment_page(&mut page));
        assert_eq!(shape(&page.blocks), "D0(D0(i) D1(i))");
    }

    #[test]
    fn an_empty_page_has_nothing_to_split() {
        let mut page = StextPage {
            mediabox: Rect::new(0.0, 0.0, 600.0, 800.0),
            blocks: Vec::new(),
        };
        assert!(!segment_page(&mut page));
        assert!(page.blocks.is_empty());
    }

    #[test]
    fn a_white_page_fill_is_dropped_once() {
        let m = Rect::new(0.0, 0.0, 600.0, 800.0);
        let mut page = StextPage {
            mediabox: m,
            blocks: vec![
                fill(m, 0xffff_ffff),
                fill(m, 0x0000_0000),
                image(50.0, 50.0, 250.0, 750.0),
            ],
        };
        assert!(remove_page_fill(&mut page));
        assert_eq!(shape(&page.blocks), "v i");
        // A colored fill stays.
        let mut page = StextPage {
            mediabox: m,
            blocks: vec![fill(m, 0xffff_0000), image(50.0, 50.0, 250.0, 750.0)],
        };
        assert!(!remove_page_fill(&mut page));
    }
}
