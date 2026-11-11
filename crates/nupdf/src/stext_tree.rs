// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/stext-iterator.c and the list handling of
// source/fitz/stext-boxer.c and source/fitz/stext-table.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! A structured text page held as MuPDF holds it while segmentation and
//! table hunting rework it: linked lists of blocks, structure blocks
//! pointing down to their own lists and those lists back up, by index. The
//! passes move blocks and lines between lists exactly as MuPDF does, then
//! the page turns back into blocks.

use crate::geometry::Rect;
use crate::stext::{Block, StructBlock, VectorBlock};

/// A block in one of the lists.
#[derive(Debug)]
pub(crate) struct Node {
    pub(crate) prev: Option<usize>,
    pub(crate) next: Option<usize>,
    pub(crate) kind: Kind,
}

#[derive(Debug)]
pub(crate) enum Kind {
    /// A text, image, vector or grid block.
    Leaf(Block),
    /// A structure block and the list of its blocks.
    Struct { bbox: Rect, index: i32, down: usize },
}

/// `fz_stext_struct`, or the page's own list.
#[derive(Debug)]
pub(crate) struct List {
    pub(crate) first: Option<usize>,
    pub(crate) last: Option<usize>,
    /// The list holding this one's block; `None` for the page.
    pub(crate) parent: Option<usize>,
    /// This list's structure block.
    pub(crate) up: Option<usize>,
    pub(crate) standard: &'static str,
    pub(crate) raw: String,
}

/// The page's list.
pub(crate) const PAGE: usize = 0;

#[derive(Debug)]
pub(crate) struct Lists {
    pub(crate) nodes: Vec<Node>,
    pub(crate) lists: Vec<List>,
}

/// `fz_stext_page_block_iterator`: a block (or the end of a run), the list
/// it is in, and the list the walk stays within.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Iter {
    pub(crate) block: Option<usize>,
    pub(crate) parent: usize,
    pub(crate) top: usize,
}

impl Lists {
    pub(crate) fn new(blocks: Vec<Block>) -> Lists {
        let mut l = Lists {
            nodes: Vec::new(),
            lists: Vec::new(),
        };
        l.new_list(None, "", String::new());
        l.fill(PAGE, blocks);
        l
    }

    fn fill(&mut self, list: usize, blocks: Vec<Block>) {
        for b in blocks {
            let node = match b {
                Block::Struct(s) => {
                    let down = self.new_list(Some(list), s.standard, s.raw);
                    let n = self.new_node(Kind::Struct {
                        bbox: s.bbox,
                        index: s.index,
                        down,
                    });
                    self.lists[down].up = Some(n);
                    self.fill(down, s.blocks);
                    n
                }
                b => self.new_node(Kind::Leaf(b)),
            };
            self.push_back(list, node);
        }
    }

    pub(crate) fn new_node(&mut self, kind: Kind) -> usize {
        self.nodes.push(Node {
            prev: None,
            next: None,
            kind,
        });
        self.nodes.len() - 1
    }

    pub(crate) fn new_list(
        &mut self,
        parent: Option<usize>,
        standard: &'static str,
        raw: String,
    ) -> usize {
        self.lists.push(List {
            first: None,
            last: None,
            parent,
            up: None,
            standard,
            raw,
        });
        self.lists.len() - 1
    }

    pub(crate) fn push_back(&mut self, list: usize, node: usize) {
        let last = self.lists[list].last;
        self.nodes[node].prev = last;
        self.nodes[node].next = None;
        match last {
            Some(l) => self.nodes[l].next = Some(node),
            None => self.lists[list].first = Some(node),
        }
        self.lists[list].last = Some(node);
    }

    pub(crate) fn bbox(&self, n: usize) -> Rect {
        match &self.nodes[n].kind {
            Kind::Leaf(b) => b.bbox(),
            Kind::Struct { bbox, .. } => *bbox,
        }
    }

    /// The list a structure block points down to.
    pub(crate) fn down(&self, n: usize) -> Option<usize> {
        match self.nodes[n].kind {
            Kind::Struct { down, .. } => Some(down),
            Kind::Leaf(_) => None,
        }
    }

    pub(crate) fn is_struct(&self, n: usize) -> bool {
        matches!(self.nodes[n].kind, Kind::Struct { .. })
    }

    pub(crate) fn leaf(&self, n: usize) -> Option<&Block> {
        match &self.nodes[n].kind {
            Kind::Leaf(b) => Some(b),
            Kind::Struct { .. } => None,
        }
    }

    pub(crate) fn leaf_mut(&mut self, n: usize) -> Option<&mut Block> {
        match &mut self.nodes[n].kind {
            Kind::Leaf(b) => Some(b),
            Kind::Struct { .. } => None,
        }
    }

    /// The nodes of `list` in order. The count bounds the walk, since each
    /// node is in one list at most once.
    pub(crate) fn children(&self, list: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut at = self.lists[list].first;
        while let Some(n) = at {
            if out.len() > self.nodes.len() {
                break;
            }
            out.push(n);
            at = self.nodes[n].next;
        }
        out
    }

    /// The page as blocks again.
    pub(crate) fn into_blocks(mut self) -> Vec<Block> {
        self.take_list(PAGE, 0)
    }

    fn take_list(&mut self, list: usize, depth: usize) -> Vec<Block> {
        let mut out = Vec::new();
        for n in self.children(list) {
            let kind = std::mem::replace(
                &mut self.nodes[n].kind,
                Kind::Leaf(Block::Vector(VectorBlock {
                    bbox: Rect::EMPTY,
                    flags: 0,
                    argb: 0,
                })),
            );
            match kind {
                Kind::Leaf(b) => out.push(b),
                Kind::Struct { bbox, index, down } => {
                    // Lists made from a page nest as deep as its blocks do,
                    // and the passes add a few levels.
                    let blocks = if depth < 256 {
                        self.take_list(down, depth + 1)
                    } else {
                        Vec::new()
                    };
                    out.push(Block::Struct(StructBlock {
                        bbox,
                        index,
                        standard: self.lists[down].standard,
                        raw: std::mem::take(&mut self.lists[down].raw),
                        blocks,
                    }));
                }
            }
        }
        out
    }

    /// Unlinks `n` from `list`.
    pub(crate) fn unlink(&mut self, list: usize, n: usize) {
        let (prev, next) = (self.nodes[n].prev, self.nodes[n].next);
        match prev {
            Some(p) => self.nodes[p].next = next,
            None => self.lists[list].first = next,
        }
        match next {
            Some(x) => self.nodes[x].prev = prev,
            None => self.lists[list].last = prev,
        }
    }

    /// `insert_block_before`: `n` into `list` before `before`, or at its end.
    pub(crate) fn insert_before(&mut self, n: usize, before: Option<usize>, list: usize) {
        match before {
            Some(b) => {
                let prev = self.nodes[b].prev;
                self.nodes[n].next = Some(b);
                self.nodes[n].prev = prev;
                match prev {
                    Some(p) => self.nodes[p].next = Some(n),
                    None => self.lists[list].first = Some(n),
                }
                self.nodes[b].prev = Some(n);
            }
            None => self.push_back(list, n),
        }
    }

    /// `add_struct_block_before`: a new, empty structure inserted into
    /// `list` before `before` (or at its end), with the index after the
    /// structures before it and the ones after it renumbered to follow.
    pub(crate) fn add_struct_before(
        &mut self,
        before: Option<usize>,
        list: usize,
        standard: &'static str,
        raw: &str,
    ) -> usize {
        let mut idx = 0;
        let mut at = self.lists[list].first;
        let mut budget = self.nodes.len() + 1;
        while at != before {
            let Some(b) = at else {
                break;
            };
            if budget == 0 {
                break;
            }
            budget -= 1;
            if let Kind::Struct { index, .. } = self.nodes[b].kind {
                idx = index + 1;
            }
            at = self.nodes[b].next;
        }
        if before.is_some() {
            let mut idx2 = idx + 1;
            let mut at = before;
            while let Some(b) = at {
                if let Kind::Struct { index, .. } = &mut self.nodes[b].kind {
                    if *index > idx2 {
                        break;
                    }
                    *index = idx2;
                    idx2 += 1;
                }
                at = self.nodes[b].next;
            }
        }
        let down = self.new_list(Some(list), standard, raw.to_string());
        let n = self.new_node(Kind::Struct {
            bbox: Rect::EMPTY,
            index: idx,
            down,
        });
        self.lists[down].up = Some(n);
        self.insert_before(n, before, list);
        down
    }

    // ---- iterators --------------------------------------------------------

    fn up_of(&self, list: usize) -> Option<usize> {
        self.lists[list].up
    }

    fn parent_of(&self, list: usize) -> usize {
        self.lists[list].parent.unwrap_or(PAGE)
    }

    /// `fz_stext_page_block_iterator_begin_from_dfs`: from `block` in `top`,
    /// down into structures, never stopping on one.
    pub(crate) fn begin_from_dfs(&self, block: Option<usize>, top: usize) -> Iter {
        let mut pos = Iter {
            block,
            parent: top,
            top,
        };
        for _ in 0..=self.nodes.len() {
            while let Some(b) = pos.block {
                let Some(d) = self.down(b) else {
                    break;
                };
                pos.parent = d;
                pos.block = self.lists[d].first;
            }
            if pos.block.is_some() || pos.parent == pos.top {
                return pos;
            }
            pos.block = self.up_of(pos.parent).and_then(|u| self.nodes[u].next);
            pos.parent = self.parent_of(pos.parent);
        }
        pos
    }

    /// `fz_stext_page_block_iterator_begin_dfs`.
    pub(crate) fn begin_dfs(&self) -> Iter {
        self.begin_from_dfs(self.lists[PAGE].first, PAGE)
    }

    /// `fz_stext_page_block_iterator_begin_from_rdfs`.
    pub(crate) fn begin_from_rdfs(&self, block: Option<usize>, top: usize) -> Iter {
        let mut pos = Iter {
            block,
            parent: top,
            top,
        };
        for _ in 0..=self.nodes.len() {
            while let Some(b) = pos.block {
                let Some(d) = self.down(b) else {
                    break;
                };
                pos.parent = d;
                pos.block = self.lists[d].last;
            }
            if pos.block.is_some() || pos.parent == pos.top {
                return pos;
            }
            pos.block = self.up_of(pos.parent).and_then(|u| self.nodes[u].prev);
            pos.parent = self.parent_of(pos.parent);
        }
        pos
    }

    /// `fz_stext_page_block_iterator_next`: along the run, stopping at
    /// every block.
    pub(crate) fn next(&self, pos: Iter) -> Iter {
        Iter {
            block: pos.block.and_then(|b| self.nodes[b].next),
            ..pos
        }
    }

    /// `fz_stext_page_block_iterator_next_dfs`.
    pub(crate) fn next_dfs(&self, mut pos: Iter) -> Iter {
        for _ in 0..=self.nodes.len() {
            pos = self.next(pos);
            while let Some(b) = pos.block {
                let Some(d) = self.down(b) else {
                    return pos;
                };
                pos.parent = d;
                pos.block = self.lists[d].first;
            }
            if pos.parent == pos.top {
                return pos;
            }
            pos.block = self.up_of(pos.parent);
            pos.parent = self.parent_of(pos.parent);
        }
        Iter { block: None, ..pos }
    }

    /// `fz_stext_page_block_iterator_next_rdfs`.
    pub(crate) fn next_rdfs(&self, mut pos: Iter) -> Iter {
        for _ in 0..=self.nodes.len() {
            pos.block = pos.block.and_then(|b| self.nodes[b].prev);
            while let Some(b) = pos.block {
                let Some(d) = self.down(b) else {
                    return pos;
                };
                pos.parent = d;
                pos.block = self.lists[d].last;
            }
            if pos.parent == pos.top {
                return pos;
            }
            pos.block = self.up_of(pos.parent);
            pos.parent = self.parent_of(pos.parent);
        }
        Iter { block: None, ..pos }
    }

    /// `fz_stext_page_block_iterator_eod_dfs` (and `_rdfs`).
    pub(crate) fn eod_dfs(&self, mut pos: Iter) -> bool {
        for _ in 0..=self.lists.len() {
            if pos.block.is_some() {
                return false;
            }
            if pos.parent == pos.top {
                return true;
            }
            pos.block = self.up_of(pos.parent);
            pos.parent = self.parent_of(pos.parent);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Matrix;
    use crate::stext::ImageBlock;

    fn image(x: f32) -> Block {
        Block::Image(ImageBlock {
            bbox: Rect::new(x, 0.0, x + 1.0, 1.0),
            transform: Matrix::IDENTITY,
        })
    }

    fn div(blocks: Vec<Block>) -> Block {
        Block::Struct(StructBlock {
            bbox: Rect::EMPTY,
            index: 0,
            standard: "Div",
            raw: String::new(),
            blocks,
        })
    }

    #[test]
    fn depth_first_walks_skip_structures_and_come_back_up() {
        let t = Lists::new(vec![
            image(0.0),
            div(vec![image(1.0), div(vec![image(2.0)])]),
            image(3.0),
        ]);
        let mut seen = Vec::new();
        let mut it = t.begin_dfs();
        while !t.eod_dfs(it) {
            if let Some(b) = it.block {
                seen.push(t.bbox(b).x0);
            }
            it = t.next_dfs(it);
        }
        assert_eq!(seen, [0.0, 1.0, 2.0, 3.0]);
        let mut back = Vec::new();
        let mut it = t.begin_from_rdfs(t.lists[PAGE].last, PAGE);
        while !t.eod_dfs(it) {
            if let Some(b) = it.block {
                back.push(t.bbox(b).x0);
            }
            it = t.next_rdfs(it);
        }
        assert_eq!(back, [3.0, 2.0, 1.0, 0.0]);
    }

    #[test]
    fn structures_inserted_take_the_next_index_and_renumber() {
        let mut t = Lists::new(vec![div(vec![]), image(5.0), div(vec![])]);
        let kids = t.children(PAGE);
        let d = t.add_struct_before(Some(kids[1]), PAGE, "Table", "Table");
        let indexes: Vec<i32> = t
            .children(PAGE)
            .into_iter()
            .filter_map(|n| match t.nodes[n].kind {
                Kind::Struct { index, .. } => Some(index),
                Kind::Leaf(_) => None,
            })
            .collect();
        assert_eq!(indexes, [0, 1, 2]);
        assert_eq!(t.lists[d].standard, "Table");
        let blocks = t.into_blocks();
        assert_eq!(blocks.len(), 4);
    }
}
