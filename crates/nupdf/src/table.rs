// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/stext-table.c and source/fitz/stext-raft.c
// (the flotilla), Copyright (C) 2004-2026 Artifex Software, Inc.

//! Table hunting (`table-hunt`), as MuPDF does it. Candidate areas come
//! from the rafts of touching rectangles and from the page's segments.
//! Each is gridded from where its content leaves gaps, refined by the
//! lines drawn on it, its cells marked as full, crossed and bordered, its
//! rows and columns merged, and scored. The best tables that do not overlap
//! are transcribed into `Table`, `TR` and `TD` structures with a grid
//! block.

use crate::geometry::Rect;
use crate::stext::{
    Block, GridBlock, GridDivider, GridPositions, StextLine, StextPage, TextBlock, char_flags,
    vector_flags,
};
use crate::stext_tree::{Iter, Kind, Lists, PAGE};

/// `FZ_STEXT_TEXT_JUSTIFY_FULL`.
const JUSTIFY_FULL: i32 = 4;
/// `FZ_STEXT_VECTOR_IS_HIGHLIGHT`, `_UNDERLINE` and `_STRIKEOUT`.
const VECTOR_STYLED: u32 = 8 | 16 | 32;
/// `FZ_STEXT_GRID_*`.
mod grid_flags {
    pub const H_CROSSED: u32 = 1;
    pub const V_CROSSED: u32 = 2;
    pub const L_BORDER: u32 = 4;
    pub const T_BORDER: u32 = 8;
    pub const FULL: u32 = 16;
}

// ---- edges and positions --------------------------------------------------

/// A `div_list` entry: where content starts (`left`) or stops, how many
/// pieces do, and whether only runs of spaces do (`weak`).
#[derive(Debug, Clone, Copy)]
struct Edge {
    left: bool,
    weak: bool,
    pos: f32,
    freq: i32,
}

/// `div_list_push`: kept sorted, stops before starts at the same place.
fn div_push(div: &mut Vec<Edge>, left: bool, weak: bool, pos: f32) {
    let mut i = 0;
    while i < div.len() {
        if div[i].pos > pos {
            break;
        } else if div[i].pos == pos {
            if div[i].left == left {
                div[i].freq += 1;
                div[i].weak &= weak;
                return;
            }
            if !div[i].left {
                break;
            }
        }
        i += 1;
    }
    div.insert(
        i,
        Edge {
            left,
            weak,
            pos,
            freq: 1,
        },
    );
}

/// `make_table_positions`: a divider at each local minimum of the "wind"
/// (how many pieces of content cover a position) between `min` and `max`,
/// uncertain by the wind there.
fn make_positions(xs: &[Edge], min: f32, max: f32) -> Option<GridPositions> {
    let len = xs.len();
    if len == 0 {
        return None;
    }
    let mut list = vec![GridDivider {
        reinforcement: 0,
        pos: min,
        min,
        max: xs[0].pos.max(min),
        uncertainty: 0,
    }];
    let mut wind = 0;
    let mut hi = 0;
    let mut local_min = false;
    let mut i = 0;
    while i < len {
        if xs[i].pos >= min {
            break;
        }
        if xs[i].left {
            wind += xs[i].freq;
        } else {
            wind -= xs[i].freq;
        }
        i += 1;
    }
    while i < len {
        if xs[i].pos >= max {
            break;
        }
        if xs[i].left {
            if local_min {
                list.push(GridDivider {
                    reinforcement: 0,
                    min: xs[i - 1].pos,
                    max: xs[i].pos,
                    pos: (xs[i - 1].pos + xs[i].pos) / 2.0,
                    uncertainty: wind,
                });
            }
            wind += xs[i].freq;
            if wind > hi {
                hi = wind;
            }
        } else {
            wind -= xs[i].freq;
            local_min = true;
        }
        i += 1;
    }
    list.push(GridDivider {
        reinforcement: 0,
        pos: max,
        min: if i == 0 { max } else { xs[i - 1].pos.min(max) },
        max,
        uncertainty: 0,
    });
    Some(GridPositions {
        list,
        max_uncertainty: hi,
    })
}

/// `sanitize_positions`: runs of starts or stops combined, and minima
/// bounded only by the edges of runs of spaces taken away.
fn sanitize(xs: &mut Vec<Edge>) {
    if xs.is_empty() {
        return;
    }
    // Each pass removes frequency, so the passes end.
    for _ in 0..=xs.iter().map(|e| e.freq.max(0) as usize).sum::<usize>() + 1 {
        let len = xs.len();
        let mut i = 0;
        while i < len {
            if xs[i].left {
                let j = i;
                while i + 1 < len && xs[i + 1].left {
                    i += 1;
                    xs[j].freq += xs[i].freq;
                    xs[j].weak &= xs[i].weak;
                    xs[i].freq = 0;
                }
            } else {
                while i + 1 < len && !xs[i + 1].left {
                    i += 1;
                    xs[i].freq += xs[i - 1].freq;
                    xs[i].weak &= xs[i - 1].weak;
                    xs[i - 1].freq = 0;
                }
            }
            i += 1;
        }
        xs.retain(|e| e.freq != 0);
        let len = xs.len();
        let mut wind = 0;
        let mut changed = false;
        let mut i = 0;
        loop {
            while i < len && xs[i].left {
                wind += xs[i].freq;
                i += 1;
            }
            while i < len && !xs[i].left {
                wind -= xs[i].freq;
                i += 1;
            }
            if i >= len || i == 0 {
                break;
            }
            if wind != 0 && (xs[i - 1].weak || xs[i].weak) {
                let m = xs[i - 1].freq.min(xs[i].freq);
                xs[i - 1].freq -= m;
                xs[i].freq -= m;
                changed = true;
            }
        }
        if !changed {
            return;
        }
    }
}

// ---- cells --------------------------------------------------------------

/// `cell_t`: the edges above and to the left of a cell, whether content
/// crosses them, and the cell's content.
#[derive(Debug, Clone, Copy)]
struct Cell {
    h_line: i32,
    v_line: i32,
    h_crossed: i32,
    v_crossed: i32,
    full: i32,
    content: Rect,
    fontsize: f32,
}

impl Default for Cell {
    fn default() -> Cell {
        Cell {
            h_line: 0,
            v_line: 0,
            h_crossed: 0,
            v_crossed: 0,
            full: 0,
            content: Rect::new(0.0, 0.0, 0.0, 0.0),
            fontsize: 0.0,
        }
    }
}

/// `cells_t`: `w` by `h` cells, one more each way than the table has, for
/// the borders on its right and bottom.
#[derive(Debug, Clone, Default)]
struct Cells {
    w: usize,
    h: usize,
    cell: Vec<Cell>,
}

impl Cells {
    fn new(w: usize, h: usize) -> Cells {
        Cells {
            w,
            h,
            cell: vec![Cell::default(); w * h],
        }
    }

    fn at(&self, x: usize, y: usize) -> &Cell {
        &self.cell[x + y * self.w]
    }

    fn at_mut(&mut self, x: usize, y: usize) -> &mut Cell {
        let w = self.w;
        &mut self.cell[x + y * w]
    }
}

/// `grid_walker_data`.
#[derive(Debug, Clone)]
struct Grid {
    cells: Cells,
    xpos: GridPositions,
    ypos: GridPositions,
    has_positions: bool,
    bounds: Rect,
    score: f32,
}

impl Default for Grid {
    fn default() -> Grid {
        Grid {
            cells: Cells::default(),
            xpos: GridPositions::default(),
            ypos: GridPositions::default(),
            has_positions: false,
            bounds: Rect::EMPTY,
            score: 0.0,
        }
    }
}

impl Grid {
    fn pos(&self, row: bool) -> &GridPositions {
        if row { &self.ypos } else { &self.xpos }
    }

    fn pos_mut(&mut self, row: bool) -> &mut GridPositions {
        if row { &mut self.ypos } else { &mut self.xpos }
    }

    /// `split_grid_pos`: divider `i` doubled, with its cells.
    fn split(&mut self, row: bool, i: usize, early: bool) {
        let pos = self.pos_mut(row);
        let d = pos.list[i];
        pos.list.insert(i, d);
        let (w, h) = (self.cells.w, self.cells.h);
        let (mut i, mut early) = (i, early);
        if early && i > 0 {
            i -= 1;
            early = false;
        }
        let old = std::mem::take(&mut self.cells.cell);
        if row {
            // Row i duplicated.
            let mut cells = Vec::with_capacity(w * (h + 1));
            for y in 0..=h {
                let src = if y <= i { y } else { y - 1 };
                cells.extend_from_slice(&old[src * w..src * w + w]);
            }
            self.cells.cell = cells;
            self.cells.h = h + 1;
            if early {
                for x in 0..w {
                    let c = self.cells.at_mut(x, 0);
                    c.h_line = 0;
                    c.v_line = 0;
                }
            } else {
                for x in 0..w {
                    self.cells.at_mut(x, i + 1).h_line = 0;
                }
            }
        } else {
            // Column i duplicated.
            let mut cells = Vec::with_capacity((w + 1) * h);
            for y in 0..h {
                for x in 0..=w {
                    let src = if x <= i { x } else { x - 1 };
                    cells.push(old[src + y * w]);
                }
            }
            self.cells.cell = cells;
            self.cells.w = w + 1;
            if early {
                for y in 0..h {
                    let c = self.cells.at_mut(0, y);
                    c.h_line = 0;
                    c.v_line = 0;
                }
            } else {
                for y in 0..h {
                    self.cells.at_mut(i + 1, y).v_line = 0;
                }
            }
        }
    }

    /// `find_grid_pos`: the divider a line at `x` is on, reinforcing or
    /// splitting dividers unless `inaccurate`.
    fn find(&mut self, row: bool, x: f32, inaccurate: bool) -> Option<usize> {
        const WIGGLE_ROOM: f32 = 2.0;
        let pos = self.pos(row);
        let n = pos.list.len();
        if n == 0 {
            return None;
        }
        if inaccurate {
            let l = &pos.list;
            let outside = x < l[0].min
                || (x < l[0].pos - WIGGLE_ROOM && l[0].reinforcement > 0)
                || x > l[n - 1].max
                || (x > l[n - 1].pos + WIGGLE_ROOM && l[n - 1].reinforcement > 0);
            if !outside {
                // The closest one, without reinforcing.
                let mut prev = 0.0f32;
                for (i, d) in l.iter().enumerate() {
                    if x < d.min {
                        let mid = (prev + d.min) / 2.0;
                        return if x < mid { i.checked_sub(1) } else { Some(i) };
                    }
                    prev = d.max;
                    if x <= prev {
                        return Some(i);
                    }
                }
                return None;
            }
        }
        for i in 0..n {
            let l = &self.pos(row).list;
            let d = l[i];
            if x < d.min {
                if i == 0 {
                    return None;
                }
                if x >= d.min - WIGGLE_ROOM {
                    // Never split into regions too small.
                    return Some(i);
                }
                self.split(row, i, true);
                let l = &mut self.pos_mut(row).list;
                let m = (l[i - 1].max + x) / 2.0;
                l[i - 1].max = m;
                l[i].min = m;
                l[i].pos = x;
                let m2 = (l[i + 1].pos + x) / 2.0;
                l[i].max = m2;
                l[i + 1].min = m2;
                l[i].reinforcement = 1;
                return Some(i);
            } else if x <= d.max {
                let split = if i == 0 {
                    // The first is never moved down, but may be split.
                    if d.pos + WIGGLE_ROOM < x {
                        true
                    } else {
                        return Some(0);
                    }
                } else if i == n - 1 {
                    if d.pos - WIGGLE_ROOM > x {
                        true
                    } else {
                        return Some(i);
                    }
                } else if d.reinforcement == 0 {
                    let l = &mut self.pos_mut(row).list;
                    l[i].pos = x;
                    l[i].reinforcement = 1;
                    return Some(i);
                } else if d.pos - WIGGLE_ROOM < x && x < d.pos + WIGGLE_ROOM {
                    // Close to where it was reinforced before.
                    let l = &mut self.pos_mut(row).list;
                    // Fused, as clang compiles MuPDF's `pos * n + x`.
                    l[i].pos = l[i].pos.mul_add(l[i].reinforcement as f32, x);
                    l[i].reinforcement += 1;
                    l[i].pos /= l[i].reinforcement as f32;
                    return Some(i);
                } else {
                    true
                };
                if split {
                    let early = d.pos > x;
                    self.split(row, i, early);
                    let l = &mut self.pos_mut(row).list;
                    if l[i].pos > x {
                        l[i].pos = x;
                        let m = (l[i + 1].pos + x) / 2.0;
                        l[i].max = m;
                        l[i + 1].min = m;
                        l[i].reinforcement = 1;
                        return Some(i);
                    }
                    l[i + 1].pos = x;
                    let m = (l[i].pos + x) / 2.0;
                    l[i].max = m;
                    l[i + 1].min = m;
                    l[i].reinforcement = 1;
                    return Some(i + 1);
                }
            } else if x <= d.max + WIGGLE_ROOM {
                return Some(i);
            }
        }
        None
    }

    /// `add_h_line2`: a horizontal line, the top border of the cells along it.
    fn h_line2(&mut self, x0: f32, x1: f32, y0: f32, y1: f32) {
        let start = self.find(false, x0, true);
        let end = self.find(false, x1, true);
        let y = self.find(true, (y0 + y1) / 2.0, true);
        if let (Some(start), Some(end), Some(y)) = (start, end, y) {
            for i in start..end {
                self.cells.at_mut(i, y).h_line += 1;
            }
        }
    }

    /// `add_v_line2`.
    fn v_line2(&mut self, y0: f32, y1: f32, x0: f32, x1: f32) {
        let start = self.find(true, y0, true);
        let end = self.find(true, y1, true);
        let x = self.find(false, (x0 + x1) / 2.0, true);
        if let (Some(start), Some(end), Some(x)) = (start, end, x) {
            for i in start..end {
                self.cells.at_mut(x, i).v_line += 1;
            }
        }
    }

    /// `add_hv_line2`: a rectangle, the border of the cells it bounds.
    fn hv_line2(&mut self, x0: f32, x1: f32, y0: f32, y1: f32) {
        let ix0 = self.find(false, x0, true);
        let ix1 = self.find(false, x1, true);
        let iy0 = self.find(true, y0, true);
        let iy1 = self.find(true, y1, true);
        let (Some(ix0), Some(ix1), Some(iy0), Some(iy1)) = (ix0, ix1, iy0, iy1) else {
            return;
        };
        for i in ix0..ix1 {
            self.cells.at_mut(i, iy0).h_line += 1;
            self.cells.at_mut(i, iy1).h_line += 1;
        }
        for i in iy0..iy1 {
            self.cells.at_mut(ix0, i).v_line += 1;
            self.cells.at_mut(ix1, i).v_line += 1;
        }
    }
}

fn find_cell_l(pos: &GridPositions, x: f32) -> Option<usize> {
    let i = pos.list.iter().position(|d| x < d.pos)?;
    i.checked_sub(1)
}

fn find_cell_r(pos: &GridPositions, x: f32) -> Option<usize> {
    let i = pos.list.iter().position(|d| x <= d.pos)?;
    i.checked_sub(1)
}

// ---- reading the page -------------------------------------------------------

fn is_header(standard: &str) -> bool {
    matches!(standard, "H" | "H1" | "H2" | "H3" | "H4" | "H5" | "H6")
}

/// `all_blocks_are_justified_or_headers`: a column of justified text,
/// with headings but not only headings.
fn all_justified_or_headers(t: &Lists, list: usize, bounds: Rect) -> bool {
    let mut just_headers = true;
    for n in t.children(list) {
        if t.bbox(n).intersect(bounds).is_empty() {
            continue;
        }
        if let Some(d) = t.down(n) {
            if is_header(t.lists[d].standard) {
                continue;
            }
            if !all_justified_or_headers(t, d, bounds) {
                return false;
            }
        }
        just_headers = false;
        if let Some(Block::Text(b)) = t.leaf(n)
            && b.flags != JUSTIFY_FULL
        {
            return false;
        }
    }
    !just_headers
}

fn clamp(x: f32, lo: f32, hi: f32) -> f32 {
    if x < lo {
        lo
    } else if x > hi {
        hi
    } else {
        x
    }
}

/// `walk_to_find_content`: where text starts and stops, across and down.
fn walk_to_find_content(
    t: &Lists,
    xs: &mut Vec<Edge>,
    ys: &mut Vec<Edge>,
    list: usize,
    bounds: Rect,
    depth: usize,
) {
    for n in t.children(list) {
        if let Some(d) = t.down(n) {
            if depth < 64 && !bounds.intersect(t.bbox(n)).is_empty() {
                walk_to_find_content(t, xs, ys, d, bounds, depth + 1);
            }
            continue;
        }
        let Some(Block::Text(b)) = t.leaf(n) else {
            continue;
        };
        let mut justified = Rect::EMPTY;
        for line in &b.lines {
            let mut region = Rect::EMPTY;
            region.y0 = line.bbox.y0.max(bounds.y0);
            region.y1 = line.bbox.y1.min(bounds.y1);
            if region.y0 >= region.y1 {
                continue;
            }
            let chars = &line.chars;
            let mut i = chars
                .iter()
                .position(|c| c.c != ' ' as i32)
                .unwrap_or(chars.len());
            while i < chars.len() {
                let ch = &chars[i];
                if ch.c == ' ' as i32 {
                    let mut last = i;
                    while last + 1 < chars.len() && chars[last + 1].c == ' ' as i32 {
                        last += 1;
                    }
                    // Not trailing: a weak gap for the run of spaces.
                    if let Some(after) = chars.get(last + 1) {
                        let mut rpos = ch.quad.ll.x.min(ch.quad.ul.x);
                        // MuPDF takes the lower left twice here.
                        let mut lpos = after.quad.ll.x.min(after.quad.ll.x);
                        rpos = clamp(rpos, bounds.x0, bounds.x1);
                        lpos = clamp(lpos, bounds.x0, bounds.x1);
                        if lpos >= region.x1 {
                            if rpos >= region.x0 && rpos < region.x1 {
                                rpos = region.x1;
                            }
                        } else if rpos <= region.x0 {
                            if lpos > region.x0 {
                                lpos = region.x0;
                            }
                        } else {
                            rpos = lpos;
                        }
                        if rpos < lpos {
                            div_push(xs, false, true, rpos);
                            div_push(xs, true, true, lpos);
                        }
                        if rpos < region.x0 {
                            region.x0 = rpos;
                        }
                        if lpos > region.x1 {
                            region.x1 = lpos;
                        }
                    }
                    i = last + 1;
                    continue;
                }
                let lpos = ch.quad.ll.x.min(ch.quad.ul.x);
                let rpos = ch.quad.lr.x.max(ch.quad.ur.x);
                if lpos < region.x0 {
                    region.x0 = lpos;
                }
                if rpos > region.x1 {
                    region.x1 = rpos;
                }
                i += 1;
            }
            if !region.is_empty() {
                div_push(xs, true, false, region.x0);
                div_push(xs, false, false, region.x1);
                // Justified text breaks around the whole region, not each
                // line.
                if b.flags != JUSTIFY_FULL {
                    div_push(ys, true, false, region.y0);
                    div_push(ys, false, false, region.y1);
                } else {
                    justified = justified.union(region);
                }
            }
        }
        if !justified.is_empty() && b.flags == JUSTIFY_FULL {
            div_push(ys, true, false, justified.y0);
            div_push(ys, false, false, justified.y1);
        }
    }
}

// ---- backgrounds ----------------------------------------------------------

#[derive(PartialEq)]
enum Background {
    KeepSearching,
    Found,
    Content,
}

/// `whittle_background`: the biggest margin around `bounds` no content
/// outside it reaches into.
fn whittle_background(
    t: &Lists,
    list: usize,
    mut obounds: Rect,
    bounds: Rect,
    depth: usize,
) -> Rect {
    for n in t.children(list) {
        if let Some(d) = t.down(n) {
            if depth < 64 {
                obounds = whittle_background(t, d, obounds, bounds, depth + 1);
            }
            continue;
        }
        if !matches!(t.leaf(n), Some(Block::Vector(_) | Block::Text(_))) {
            continue;
        }
        let r = t.bbox(n);
        if r.y1 >= bounds.y0 && r.y0 <= bounds.y1 {
            if r.x1 <= bounds.x0 {
                obounds.x0 = obounds.x0.max(r.x1);
            }
            if r.x0 >= bounds.x1 {
                obounds.x1 = obounds.x1.min(r.x0);
            }
        }
        if r.x1 >= bounds.x0 && r.x0 <= bounds.x1 {
            if r.y1 <= bounds.y0 {
                obounds.y0 = obounds.y0.max(r.y1);
            }
            if r.y0 >= bounds.y1 {
                obounds.y1 = obounds.y1.min(r.y0);
            }
        }
    }
    obounds
}

/// `background_hunt`: a fill covering `bounds` but not past `obounds`,
/// before any content inside, which the table is then stretched to.
fn background_hunt(
    t: &Lists,
    g: &mut Grid,
    list: usize,
    bounds: Rect,
    obounds: Rect,
    depth: usize,
) -> Background {
    let outside =
        |r: Rect| r.x0 < obounds.x0 || r.x1 > obounds.x1 || r.y0 < obounds.y0 || r.y1 > obounds.y1;
    for n in t.children(list) {
        if let Some(d) = t.down(n) {
            if depth < 64 {
                let found = background_hunt(t, g, d, bounds, obounds, depth + 1);
                if found != Background::KeepSearching {
                    return found;
                }
            }
            continue;
        }
        match t.leaf(n) {
            Some(Block::Vector(v)) => {
                let r = v.bbox.intersect(g.bounds);
                let s = r.intersect(bounds);
                if !s.is_valid() {
                    continue;
                }
                if v.flags & VECTOR_STYLED != 0 {
                    continue;
                }
                // Only unstroked rectangles can be fills.
                if v.flags & vector_flags::IS_RECTANGLE == 0
                    || v.flags & vector_flags::IS_STROKED != 0
                {
                    // MuPDF's check for a fill too big to matter does not
                    // change the answer.
                    let _ = outside(r);
                    return Background::Content;
                }
                if r.x0 <= bounds.x0 && r.x1 >= bounds.x1 && r.y0 <= bounds.y0 && r.y1 >= bounds.y1
                {
                    // Covers the table; too big for it is a page fill.
                    if outside(r) {
                        continue;
                    }
                    let set = |d: &mut GridDivider, v: f32| {
                        d.min = v;
                        d.pos = v;
                        d.max = v;
                    };
                    for (list, lo, hi) in [
                        (&mut g.xpos.list, r.x0, r.x1),
                        (&mut g.ypos.list, r.y0, r.y1),
                    ] {
                        if let Some(d) = list.first_mut() {
                            set(d, lo);
                        }
                        if let Some(d) = list.last_mut() {
                            set(d, hi);
                        }
                    }
                    return Background::Found;
                }
                return Background::Content;
            }
            Some(Block::Text(b)) => {
                let s = b.bbox.intersect(g.bounds).intersect(bounds);
                if s.is_valid() {
                    return Background::Content;
                }
            }
            _ => {}
        }
    }
    Background::KeepSearching
}

/// `walk_for_background`.
fn walk_for_background(t: &Lists, g: &mut Grid, list: usize) -> Background {
    let (xl, yl) = (g.xpos.list.len(), g.ypos.list.len());
    let bounds = Rect::new(
        g.xpos.list[0].min,
        g.ypos.list[0].min,
        g.xpos.list[xl - 1].max,
        g.ypos.list[yl - 1].max,
    );
    let obounds = whittle_background(t, list, Rect::INFINITE, bounds, 0);
    background_hunt(t, g, list, bounds, obounds, 0)
}

// ---- grid lines -----------------------------------------------------------

/// `fz_collate_small_vector_run` over a list: `at` moves to the last
/// vector of the run.
fn collate_small_vector_run(t: &Lists, at: &mut usize) -> Rect {
    const MAX_SIZE: f32 = 2.0;
    const MAX_GAP: f32 = 2.0;
    let vector = |n: Option<usize>| match n.and_then(|n| t.leaf(n)) {
        Some(Block::Vector(v)) => Some(v.bbox),
        _ => None,
    };
    let mut r = t.bbox(*at);
    let (w, h) = (r.x1 - r.x0, r.y1 - r.y0);
    if w < MAX_SIZE {
        while let Some(nb) = vector(t.nodes[*at].next) {
            let joins = nb.x0 == r.x0
                && nb.x1 == r.x1
                && ((nb.y1 > r.y1 && nb.y0 <= r.y1 + MAX_GAP)
                    || (nb.y0 < r.y0 && nb.y1 >= r.y0 - MAX_GAP));
            if !joins {
                break;
            }
            r = r.union(nb);
            *at = t.nodes[*at].next.unwrap_or(*at);
        }
    }
    if h < MAX_SIZE {
        while let Some(nb) = vector(t.nodes[*at].next) {
            let joins = nb.y0 == r.y0
                && nb.y1 == r.y1
                && ((nb.x1 > r.x1 && nb.x0 <= r.x1 + MAX_GAP)
                    || (nb.x0 < r.x0 && nb.x1 >= r.x0 - MAX_GAP));
            if !joins {
                break;
            }
            r = r.union(nb);
            *at = t.nodes[*at].next.unwrap_or(*at);
        }
    }
    r
}

/// `walk_grid_lines` (`mark` false: dividers placed and reinforced) and
/// `walk_grid_lines2` (`mark` true: cell borders recorded).
fn walk_grid_lines(t: &Lists, g: &mut Grid, list: usize, mark: bool, depth: usize) {
    let mut at = t.lists[list].first;
    let mut budget = t.nodes.len() + 1;
    while let Some(mut n) = at {
        if budget == 0 {
            break;
        }
        budget -= 1;
        if let Some(d) = t.down(n) {
            if depth < 64 {
                walk_grid_lines(t, g, d, mark, depth + 1);
            }
            at = t.nodes[n].next;
            continue;
        }
        if let Some(Block::Vector(v)) = t.leaf(n)
            && v.flags & vector_flags::IS_RECTANGLE != 0
            && v.flags & VECTOR_STYLED == 0
        {
            let r = collate_small_vector_run(t, &mut n).intersect(g.bounds);
            if r.is_valid() {
                let (w, h) = (r.x1 - r.x0, r.y1 - r.y0);
                if w > h && h < 2.0 {
                    if mark {
                        g.h_line2(r.x0, r.x1, r.y0, r.y1);
                    } else {
                        g.find(true, (r.y0 + r.y1) / 2.0, false);
                    }
                } else if w < h && w < 2.0 {
                    if mark {
                        g.v_line2(r.y0, r.y1, r.x0, r.x1);
                    } else {
                        g.find(false, (r.x0 + r.x1) / 2.0, false);
                    }
                } else if mark {
                    g.hv_line2(r.x0, r.x1, r.y0, r.y1);
                } else {
                    g.find(false, r.x0, false);
                    g.find(false, r.x1, false);
                    g.find(true, r.y0, false);
                    g.find(true, r.y1, false);
                }
            }
        }
        at = t.nodes[n].next;
    }
}

// ---- content --------------------------------------------------------------

/// `mark_cells_for_content`: the cells `s` covers are full, and the edges
/// it crosses crossed; true if it lies outside the grid.
fn mark_cells(g: &mut Grid, s: Rect, baseline: f32, fontsize: f32) -> bool {
    let r = g.bounds.intersect(s);
    let line_ext = Rect::new(r.x0, r.y0.max(baseline - fontsize), r.x1, baseline);
    // Valid, not merely not empty: spaces have width but no height.
    if !r.is_valid() {
        return false;
    }
    let (Some(x0), Some(x1), Some(y0), Some(y1)) = (
        find_cell_l(&g.xpos, r.x0),
        find_cell_r(&g.xpos, r.x1),
        find_cell_l(&g.ypos, r.y0),
        find_cell_r(&g.ypos, r.y1),
    ) else {
        return true;
    };
    let c = &mut g.cells;
    if x0 < x1 {
        for y in y0..=y1 {
            for x in x0..x1 {
                c.at_mut(x + 1, y).v_crossed += 1;
            }
        }
    }
    if y0 < y1 {
        for y in y0..y1 {
            for x in x0..=x1 {
                c.at_mut(x, y + 1).h_crossed += 1;
            }
        }
    }
    for y in y0..=y1 {
        for x in x0..=x1 {
            let cell = c.at_mut(x, y);
            cell.full += 1;
            if fontsize != 0.0 {
                cell.content = cell.content.union(line_ext);
                if fontsize > cell.fontsize {
                    cell.fontsize = fontsize;
                }
            }
        }
    }
    false
}

#[derive(Clone, Copy, PartialEq)]
enum In {
    Cell,
    Border,
    Unknown,
}

const SPLIT_MARGIN: f32 = 4.0;

/// `where_is`: the cell or border a position falls in.
fn where_is(pos: &GridPositions, x: f32) -> (i64, In) {
    let l = &pos.list;
    let n = l.len();
    if n == 0 {
        return (-1, In::Unknown);
    }
    if x <= l[0].pos - SPLIT_MARGIN || x >= l[n - 1].max + SPLIT_MARGIN {
        return (-1, In::Unknown);
    }
    for i in 0..n {
        let prev = if i > 0 { l[i - 1].max } else { l[0].min };
        let next = if i + 1 < n { l[i + 1].min } else { l[i].max };
        let prev = (prev + l[i].pos) / 2.0;
        let next = (next + l[i].pos) / 2.0;
        let below = (l[i].pos - SPLIT_MARGIN).max(prev);
        let above = (l[i].pos + SPLIT_MARGIN).min(next);
        // MuPDF clamps with plain comparisons; max and min agree.
        if x < below {
            return (i as i64 - 1, In::Cell);
        } else if x <= above {
            return (i as i64, In::Border);
        }
    }
    (n as i64 - 1, In::Border)
}

#[derive(PartialEq)]
enum VectorIs {
    Content,
    Border,
    Unknown,
    Ignorable,
}

/// `classify_vector`: a border, content within cells, or neither.
fn classify_vector(g: &Grid, r: Rect, is_rect: bool) -> VectorIs {
    let r = r.intersect(g.bounds);
    if r.is_empty() {
        return VectorIs::Ignorable;
    }
    let (ix0, x0) = where_is(&g.xpos, r.x0);
    let (ix1, x1) = where_is(&g.xpos, r.x1);
    let (iy0, y0) = where_is(&g.ypos, r.y0);
    let (iy1, y1) = where_is(&g.ypos, r.y1);
    if [x0, x1, y0, y1].contains(&In::Unknown) {
        return VectorIs::Ignorable;
    }
    let border = || {
        if is_rect {
            VectorIs::Border
        } else {
            VectorIs::Content
        }
    };
    if (x0 == In::Border && x1 == In::Border) || (y0 == In::Border && y1 == In::Border) {
        return border();
    }
    if (x0 == In::Cell && x1 == In::Cell) || (y0 == In::Cell && y1 == In::Cell) {
        return VectorIs::Content;
    }
    if (x0 == In::Border && x1 == In::Cell && ix0 == ix1)
        || (x0 == In::Cell && x1 == In::Border && ix0 + 1 == ix1)
        || (y0 == In::Border && y1 == In::Cell && iy0 == iy1)
        || (y0 == In::Cell && y1 == In::Border && iy0 + 1 == iy1)
    {
        return border();
    }
    if is_rect {
        return VectorIs::Ignorable;
    }
    VectorIs::Unknown
}

/// `calculate_spanned_content`: content marked into the cells; the count
/// of vectors that do not look like a table's.
fn calculate_spanned(t: &Lists, g: &mut Grid, list: usize, depth: usize) -> usize {
    let (xl, yl) = (g.xpos.list.len(), g.ypos.list.len());
    let bounds = Rect::new(
        g.xpos.list[0].pos,
        g.ypos.list[0].pos,
        g.xpos.list[xl - 1].pos,
        g.ypos.list[yl - 1].pos,
    );
    let mut duff = 0;
    for n in t.children(list) {
        if let Some(d) = t.down(n) {
            if depth < 64 {
                duff += calculate_spanned(t, g, d, depth + 1);
            }
            continue;
        }
        match t.leaf(n) {
            Some(Block::Vector(v)) => {
                match classify_vector(g, v.bbox, v.flags & vector_flags::IS_RECTANGLE != 0) {
                    VectorIs::Content => {
                        mark_cells(g, v.bbox, 0.0, 0.0);
                    }
                    VectorIs::Border | VectorIs::Ignorable => {}
                    VectorIs::Unknown => duff += 1,
                }
            }
            Some(Block::Text(b)) => {
                let bb = b.bbox;
                if bb.x0 >= bounds.x1
                    || bb.y0 >= bounds.y1
                    || bb.x1 <= bounds.x0
                    || bb.y1 <= bounds.y0
                {
                    continue;
                }
                for line in &b.lines {
                    let chars = &line.chars;
                    let mut i = chars
                        .iter()
                        .position(|c| c.c != ' ' as i32)
                        .unwrap_or(chars.len());
                    let mut was_numeric = false;
                    let numeric = |c: i32| ('0' as i32..='9' as i32).contains(&c);
                    while i < chars.len() {
                        let ch = &chars[i];
                        if ch.c == 32 {
                            let Some(next) = chars.get(i + 1) else {
                                // A trailing space.
                                break;
                            };
                            if next.c == 32 {
                                // A run of spaces is skipped.
                                while chars.get(i + 1).is_some_and(|c| c.c == 32) {
                                    i += 1;
                                }
                                was_numeric = false;
                                i += 1;
                                continue;
                            }
                            if was_numeric || numeric(next.c) {
                                // Single spaces around numbers are ignored.
                                was_numeric = false;
                                i += 1;
                                continue;
                            }
                            if ch.flags & char_flags::SYNTHETIC_LARGE != 0 {
                                i += 1;
                                continue;
                            }
                            was_numeric = false;
                        } else {
                            was_numeric = numeric(ch.c);
                        }
                        if mark_cells(g, Rect::from_quad(ch.quad), ch.origin.y, ch.size) {
                            duff += 1;
                        }
                        i += 1;
                    }
                }
            }
            _ => {}
        }
    }
    duff
}

// ---- moving content into cells --------------------------------------------

fn recalc_bbox(b: &mut TextBlock) {
    b.bbox = b.lines.iter().fold(Rect::EMPTY, |r, l| r.union(l.bbox));
}

fn text_mut(t: &mut Lists, n: usize) -> Option<&mut TextBlock> {
    match t.leaf_mut(n) {
        Some(Block::Text(b)) => Some(b),
        _ => None,
    }
}

/// A new, empty text block with these flags, in `dest` before `before`.
fn new_text_before(t: &mut Lists, before: Option<usize>, dest: usize, flags: i32) -> usize {
    let nb = t.new_node(Kind::Leaf(Block::Text(TextBlock {
        // A block from MuPDF's pool starts zeroed.
        bbox: Rect::new(0.0, 0.0, 0.0, 0.0),
        flags,
        lines: Vec::new(),
    })));
    t.insert_before(nb, before, dest);
    nb
}

/// `move_contained_content`: what of `src` lies within `r` moved to the
/// start of `dest`: whole blocks, lines and characters.
fn move_contained(t: &mut Lists, dest: usize, src: usize, r: Rect, depth: usize) {
    let before = t.lists[dest].first;
    let trivially_out = |b: Rect| b.x0 > b.x1 || b.y0 > b.y1;
    for n in t.children(src) {
        let bb = t.bbox(n);
        let bbox = bb.intersect(r);
        // Not is_empty: that would leave out spaces, which have no height.
        if trivially_out(bbox) {
            continue;
        }
        if bbox == bb {
            if let Some(d) = t.down(n) {
                if t.lists[d].standard == "TD" {
                    // A TD's contents move, not the TD.
                    if depth < 64 {
                        move_contained(t, dest, d, r, depth + 1);
                    }
                    continue;
                }
                t.lists[d].parent = Some(dest);
            }
            t.unlink(src, n);
            t.insert_before(n, before, dest);
            continue;
        }
        if let Some(d) = t.down(n)
            && depth < 64
        {
            move_contained(t, dest, d, r, depth + 1);
        }
        let Some(block) = text_mut(t, n) else {
            continue;
        };
        let flags = block.flags;
        let lines = std::mem::take(&mut block.lines);
        let mut kept: Vec<StextLine> = Vec::with_capacity(lines.len());
        let mut taken: Vec<StextLine> = Vec::new();
        for mut line in lines {
            let lrect = line.bbox.intersect(r);
            if trivially_out(lrect) {
                kept.push(line);
                continue;
            }
            if lrect == line.bbox {
                taken.push(line);
                continue;
            }
            // Characters sharing a baseline are judged by their combined
            // height, so a line is not cut through.
            let chars = std::mem::take(&mut line.chars);
            let mut left = Vec::with_capacity(chars.len());
            let mut newline: Option<StextLine> = None;
            let mut i = 0;
            while i < chars.len() {
                let mut end = i + 1;
                let first = Rect::from_quad(chars[i].quad);
                let (mut vmin, mut vmax) = (first.y0, first.y1);
                while end < chars.len() && chars[end].origin.y == chars[i].origin.y {
                    let c = Rect::from_quad(chars[end].quad);
                    vmin = vmin.min(c.y0);
                    vmax = vmax.max(c.y1);
                    end += 1;
                }
                let y = (vmin + vmax) / 2.0;
                for ch in &chars[i..end] {
                    let c = Rect::from_quad(ch.quad);
                    let x = (c.x0 + c.x1) / 2.0;
                    if r.x0 > x || r.x1 < x || r.y0 > y || r.y1 < y {
                        left.push(ch.clone());
                        continue;
                    }
                    let nl = newline.get_or_insert_with(|| StextLine {
                        wmode: line.wmode,
                        flags: 0,
                        dir: line.dir,
                        bbox: Rect::EMPTY,
                        chars: Vec::new(),
                    });
                    nl.chars.push(ch.clone());
                    nl.bbox = nl.bbox.union(c);
                }
                i = end;
            }
            // The line keeps its box, as in MuPDF.
            line.chars = left;
            if !line.chars.is_empty() {
                kept.push(line);
            }
            if let Some(nl) = newline {
                taken.push(nl);
            }
        }
        let moved = !taken.is_empty();
        if let Some(block) = text_mut(t, n) {
            block.lines = kept;
        }
        if moved {
            let nb = new_text_before(t, before, dest, flags);
            if let Some(newblock) = text_mut(t, nb) {
                newblock.lines = taken;
                recalc_bbox(newblock);
            }
            if let Some(block) = text_mut(t, n) {
                recalc_bbox(block);
            }
        }
        if text_mut(t, n).is_some_and(|b| b.lines.is_empty()) {
            t.unlink(src, n);
        }
    }
    // Indexes in order again.
    let mut max = 0;
    for n in t.children(dest) {
        if let Kind::Struct { index, .. } = &mut t.nodes[n].kind {
            if *index < max {
                *index = max;
            }
            max = *index + 1;
        }
    }
}

/// `find_table_insertion_point`: the last block wholly above `r`, within
/// the structures that hold `r`.
fn insertion_point(
    t: &Lists,
    r: Rect,
    current: (Option<usize>, usize),
    mut best: (Option<usize>, usize),
    depth: usize,
) -> (Option<usize>, usize) {
    let mut at = current.0;
    let mut budget = t.nodes.len() + 1;
    while let Some(n) = at {
        if budget == 0 {
            break;
        }
        budget -= 1;
        at = t.nodes[n].next;
        let bb = t.bbox(n);
        if let Some(d) = t.down(n)
            && r.is_valid()
            && bb.is_valid()
            && r.x0 >= bb.x0
            && r.x1 <= bb.x1
            && r.y0 >= bb.y0
            && r.y1 <= bb.y1
        {
            if depth < 64 {
                best = insertion_point(t, r, (t.lists[d].first, d), best, depth + 1);
            }
            continue;
        }
        if bb.y1 < r.y0 && best.0.is_none_or(|b| t.bbox(b).y1 < bb.y1) {
            best = (Some(n), current.1);
        }
    }
    best
}

fn tr_is_empty(t: &Lists, list: usize) -> bool {
    t.children(list).into_iter().all(|n| match t.down(n) {
        None => t.is_struct(n),
        Some(d) => t.lists[d].standard == "TD" && t.lists[d].first.is_none(),
    })
}

fn table_is_empty(t: &Lists, list: usize) -> bool {
    t.children(list).into_iter().all(|n| {
        if matches!(t.leaf(n), Some(Block::Grid(_))) {
            return true;
        }
        match t.down(n) {
            None => false,
            Some(d) => t.lists[d].standard == "TR" && tr_is_empty(t, d),
        }
    })
}

/// `tidy_td_divs`: a TD holding just a Div holds the Div's blocks.
fn tidy_td_divs(t: &mut Lists, parent: usize) {
    for _ in 0..=t.lists.len() {
        let Some(b) = t.lists[parent].first else {
            return;
        };
        if t.nodes[b].next.is_some() {
            return;
        }
        let Some(d) = t.down(b) else {
            return;
        };
        if t.lists[d].standard != "Div" {
            return;
        }
        t.lists[parent].first = t.lists[d].first;
        t.lists[parent].last = t.lists[d].last;
        for n in t.children(parent) {
            if let Some(dd) = t.down(n) {
                t.lists[dd].parent = Some(parent);
            }
        }
    }
}

fn tidy_tr_divs(t: &mut Lists, list: usize) {
    for n in t.children(list) {
        if let Some(d) = t.down(n)
            && t.lists[d].standard == "TD"
        {
            tidy_td_divs(t, d);
        }
    }
}

fn tidy_table_divs(t: &mut Lists, list: usize) {
    for n in t.children(list) {
        if let Some(d) = t.down(n)
            && t.lists[d].standard == "TR"
        {
            tidy_tr_divs(t, d);
        }
    }
}

fn struct_is_empty(t: &Lists, list: usize, depth: usize) -> bool {
    t.children(list).into_iter().all(|n| match t.down(n) {
        None => t.is_struct(n),
        Some(d) => depth >= 64 || struct_is_empty(t, d, depth + 1),
    })
}

/// `div_is_empty`. MuPDF answers for the first table it meets.
fn div_is_empty(t: &mut Lists, list: usize, depth: usize) -> bool {
    for n in t.children(list) {
        let Some(d) = t.down(n) else {
            return false;
        };
        match t.lists[d].standard {
            "Table" => {
                tidy_table_divs(t, d);
                return table_is_empty(t, d);
            }
            "Div" => {
                if depth < 64 && !div_is_empty(t, d, depth + 1) {
                    return false;
                }
            }
            _ => {
                if !struct_is_empty(t, d, depth + 1) {
                    return false;
                }
            }
        }
    }
    true
}

/// `tidy_orphaned_tables`: tables and Divs left empty removed.
fn tidy_orphaned_tables(t: &mut Lists, parent: usize, depth: usize) {
    if depth >= 64 {
        return;
    }
    for n in t.children(parent) {
        let Some(d) = t.down(n) else {
            continue;
        };
        if t.lists[d].standard == "Table" {
            tidy_table_divs(t, d);
            if table_is_empty(t, d) {
                t.unlink(parent, n);
            } else {
                tidy_orphaned_tables(t, d, depth + 1);
            }
        }
        if t.lists[d].standard == "Div" {
            tidy_orphaned_tables(t, d, depth + 1);
            if div_is_empty(t, d, depth + 1) {
                t.unlink(parent, n);
            }
        }
    }
}

/// `tidy_td`: text blocks in a cell that continue one another, by their
/// font sizes and baselines, glued back together.
fn tidy_td(t: &mut Lists, parent: usize, depth: usize) {
    let mut at = t.lists[parent].first;
    let mut budget = t.nodes.len() * 2 + 2;
    while let Some(b) = at {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let next = t.nodes[b].next;
        at = next;
        if let Some(d) = t.down(b) {
            if depth < 64 {
                tidy_td(t, d, depth + 1);
            }
            continue;
        }
        let Some(nx) = next else {
            continue;
        };
        let (Some(Block::Text(a)), Some(Block::Text(c))) = (t.leaf(b), t.leaf(nx)) else {
            continue;
        };
        if (c.bbox.x0 - a.bbox.x0).abs() > 36.0 || c.bbox.y0 < a.bbox.y1 {
            continue;
        }
        let (Some(last), Some(first)) = (a.lines.last(), c.lines.first()) else {
            continue;
        };
        let biggest = |l: &StextLine| {
            let (mut fs, mut base) = (0.0f32, 0.0f32);
            for ch in &l.chars {
                if fs < ch.size {
                    fs = ch.size;
                    base = ch.origin.y;
                }
            }
            (fs, base)
        };
        let (fs0, baseline0) = biggest(last);
        let (fs1, baseline1) = biggest(first);
        if fs0 == 0.0 || fs1 == 0.0 || (fs0 - fs1).abs() >= 0.5 {
            continue;
        }
        // Line spacing of up to 150%.
        let bot = baseline0 + fs0 / 2.0;
        let top = (baseline1 - fs1).max(c.bbox.y0);
        if bot >= top {
            let next_bbox = c.bbox;
            let moved = match t.leaf_mut(nx) {
                Some(Block::Text(c)) => std::mem::take(&mut c.lines),
                _ => Vec::new(),
            };
            if let Some(a) = text_mut(t, b) {
                a.lines.extend(moved);
                a.bbox = a.bbox.union(next_bbox);
            }
            t.unlink(parent, nx);
            // And look at this block again.
            at = Some(b);
        }
    }
}

/// `transcribe_table`: the table's cells made TDs in TRs in a Table, with
/// what lies within each moved into it.
fn transcribe(t: &mut Lists, g: &Grid, parent: usize) {
    let (w, h) = (g.xpos.list.len(), g.ypos.list.len());
    let xs = &g.xpos.list;
    let ys = &g.ypos.list;
    let mut sent = vec![false; w * h];
    let r = Rect::new(xs[0].pos, ys[0].pos, xs[w - 1].pos, ys[h - 1].pos);
    let (block, iparent) = insertion_point(t, r, (t.lists[parent].first, parent), (None, PAGE), 0);
    let (before, iparent) = match block {
        Some(b) => (t.nodes[b].next, iparent),
        None => (t.lists[parent].first, parent),
    };
    let table = t.add_struct_before(before, iparent, "Table", "Table");
    let cells = &g.cells;
    for y in 0..h - 1 {
        if (0..w - 1).all(|x| sent[x + y * w]) {
            continue;
        }
        let tr = t.add_struct_before(None, table, "TR", "TR");
        for x in 0..w - 1 {
            if sent[x + y * w] {
                continue;
            }
            let mut cellw = 1;
            let mut x2 = x + 1;
            while x2 < w - 1 {
                let c = cells.at(x2, y);
                if c.v_line != 0 || xs[x2].uncertainty == 0 || c.v_crossed == 0 {
                    break;
                }
                cellw += 1;
                x2 += 1;
            }
            let mut cellh = 1;
            let mut y2 = y + 1;
            while y2 < h - 1 {
                if ys[y2].uncertainty == 0 {
                    break;
                }
                let c = cells.at(x, y2);
                if c.h_line != 0 {
                    break;
                }
                let mut h_crossed = c.h_crossed != 0;
                let mut x2 = x + 1;
                while x2 < x + cellw {
                    let c = cells.at(x2, y2);
                    if c.h_line != 0 || c.v_line != 0 || xs[x2].uncertainty == 0 || c.v_crossed == 0
                    {
                        break;
                    }
                    if c.h_crossed != 0 {
                        h_crossed = true;
                    }
                    x2 += 1;
                }
                if x2 == x + cellw && h_crossed {
                    cellh += 1;
                } else {
                    break;
                }
                y2 += 1;
            }
            let td = t.add_struct_before(None, tr, "TD", "TD");
            let r = Rect::new(xs[x].pos, ys[y].pos, xs[x + cellw].pos, ys[y + cellh].pos);
            // The cell's box, not its content's, so spanned rows are not
            // left empty.
            set_struct_bbox(t, td, r);
            move_contained(t, td, parent, r, 0);
            tidy_td(t, td, 0);
            for yy in y..y + cellh {
                for xx in x..x + cellw {
                    sent[xx + yy * w] = true;
                }
            }
        }
        let r = Rect::new(xs[0].pos, ys[y].pos, xs[w - 1].pos, ys[y + 1].pos);
        set_struct_bbox(t, tr, r);
        let tb = struct_bbox(t, table).union(r);
        set_struct_bbox(t, table, tb);
    }
    // The grid goes first in the table.
    let mut flags = Vec::with_capacity(cells.w * cells.h);
    for c in cells.cell.iter().take(cells.w * cells.h) {
        let mut f = 0;
        if c.full != 0 {
            f |= grid_flags::FULL;
        }
        if c.h_crossed != 0 {
            f |= grid_flags::H_CROSSED;
        }
        if c.v_crossed != 0 {
            f |= grid_flags::V_CROSSED;
        }
        if c.h_line != 0 {
            f |= grid_flags::T_BORDER;
        }
        if c.v_line != 0 {
            f |= grid_flags::L_BORDER;
        }
        flags.push(f);
    }
    let grid = t.new_node(Kind::Leaf(Block::Grid(GridBlock {
        bbox: Rect::new(xs[0].pos, ys[0].pos, xs[w - 1].pos, ys[h - 1].pos),
        xs: g.xpos.clone(),
        ys: g.ypos.clone(),
        w: cells.w,
        h: cells.h,
        cells: flags,
    })));
    let first = t.lists[table].first;
    t.insert_before(grid, first, table);
    tidy_orphaned_tables(t, parent, 0);
}

fn struct_node(t: &Lists, list: usize) -> Option<usize> {
    t.lists[list].up
}

fn struct_bbox(t: &Lists, list: usize) -> Rect {
    struct_node(t, list).map_or(Rect::EMPTY, |n| t.bbox(n))
}

fn set_struct_bbox(t: &mut Lists, list: usize, r: Rect) {
    if let Some(n) = struct_node(t, list)
        && let Kind::Struct { bbox, .. } = &mut t.nodes[n].kind
    {
        *bbox = r;
    }
}

// ---- merging ----------------------------------------------------------------

/// `plausibly_bordered_spanned_cell`: within a bordered cell made of
/// several.
fn plausibly_bordered(c: &Cells, x: usize, y: usize) -> bool {
    let (w, h) = (c.w, c.h);
    let Some(minx) = (0..=x).rev().find(|&mx| c.at(mx, y).v_line != 0) else {
        return false;
    };
    let mut maxx = x + 1;
    while maxx < w - 1 && c.at(maxx, y).v_line == 0 {
        maxx += 1;
    }
    let Some(miny) = (0..=y).rev().find(|&my| c.at(x, my).h_line != 0) else {
        return false;
    };
    let mut maxy = y + 1;
    while maxy < h - 1 && c.at(x, maxy).h_line == 0 {
        maxy += 1;
    }
    for xx in minx..maxx {
        if c.at(xx, miny).h_line == 0 || c.at(xx, maxy).h_line == 0 {
            return false;
        }
    }
    for yy in miny..maxy {
        if c.at(minx, yy).v_line == 0 || c.at(maxx, yy).v_line == 0 {
            return false;
        }
    }
    for yy in miny + 1..maxy.saturating_sub(1) {
        for xx in minx + 1..maxx.saturating_sub(1) {
            let cell = c.at(xx, yy);
            if cell.h_line != 0 || cell.v_line != 0 {
                return false;
            }
        }
    }
    true
}

/// Removes divider `i` as MuPDF's `memmove` does: the one at `i` unless it
/// is the last, which is dropped instead.
fn remove_divider(pos: &mut GridPositions, i: usize) {
    if i + 1 < pos.list.len() {
        pos.list.remove(i);
    } else {
        pos.list.pop();
    }
}

/// `merge_column`: columns `x` and `x+1` made one.
fn merge_column(g: &mut Grid, x: usize) {
    let (w, h) = (g.cells.w, g.cells.h);
    let cell = &mut g.cells.cell;
    for y in 0..h {
        let d = x + y * (w - 1);
        let s = x + y * w;
        if x > 0 {
            cell.copy_within(s - x..s, d - x);
        }
        let (a, b) = (cell[s], cell[s + 1]);
        cell[d] = Cell {
            full: i32::from(a.full != 0 || b.full != 0),
            h_crossed: i32::from(a.h_crossed != 0 || b.h_crossed != 0),
            h_line: a.h_line,
            v_crossed: a.v_crossed,
            v_line: a.v_line,
            content: a.content.union(b.content),
            fontsize: a.fontsize.max(b.fontsize),
        };
        if x + 2 < w {
            cell.copy_within(s + 2..s + 2 + (w - 2 - x), d + 1);
        }
    }
    g.cells.w -= 1;
    cell.truncate(g.cells.w * h);
    remove_divider(&mut g.xpos, x + 1);
}

/// `merge_columns`.
fn merge_columns(g: &mut Grid) {
    const MIN_COL_DIVIDER_WIDTH: f32 = 4.0;
    let mut x = g.cells.w as i64 - 3;
    while x >= 0 {
        let xu = x as usize;
        let h = g.cells.h;
        let c = &g.cells;
        let all =
            |f: &dyn Fn(&Cell, &Cell) -> bool| (0..h - 1).all(|y| f(c.at(xu, y), c.at(xu + 1, y)));
        let same_h = |a: &Cell, b: &Cell| (a.h_line != 0) == (b.h_line != 0);
        let d = g.xpos.list[xu + 1];
        let merge = (d.max - d.min <= MIN_COL_DIVIDER_WIDTH && all(&|a, b| same_h(a, b) && b.v_line == 0))
            // An empty column merges if the h_lines agree and no v_line.
            || all(&|a, b| a.full == 0 && same_h(a, b) && b.v_line == 0)
            || all(&|a, b| b.full == 0 && same_h(a, b) && b.v_line == 0)
            || ((0..h - 1).any(|y| c.at(xu + 1, y).v_crossed == 1)
                && all(&|a, b| {
                    if b.v_line != 0 {
                        return false;
                    }
                    if a.full == 0 || b.full == 0 {
                        return true;
                    }
                    same_h(a, b) && b.v_crossed != 0
                }));
        if merge {
            merge_column(g, xu);
        }
        x -= 1;
    }
}

/// `remove_empty_column`.
fn remove_empty_column(g: &mut Grid, x: usize) {
    let (w, h) = (g.cells.w, g.cells.h);
    let cell = &mut g.cells.cell;
    for y in 0..h {
        let d = x + y * (w - 1);
        let s = x + y * w;
        if x > 0 {
            cell.copy_within(s - x..s, d - x);
        }
        if x + 1 < w {
            cell.copy_within(s + 1..s + 1 + (w - 1 - x), d);
        }
    }
    g.cells.w -= 1;
    cell.truncate(g.cells.w * h);
    remove_divider(&mut g.xpos, x);
}

/// `remove_empty_row`.
fn remove_empty_row(g: &mut Grid, y: usize) {
    let (w, h) = (g.cells.w, g.cells.h);
    let cell = &mut g.cells.cell;
    cell.copy_within((y + 1) * w..(y + 1) * w + w * (h - y - 1), y * w);
    g.cells.h -= 1;
    cell.truncate(w * g.cells.h);
    remove_divider(&mut g.ypos, y);
}

/// `trim_empty_margins`: empty columns and rows around the edge removed;
/// true if the table went with them.
fn trim_empty_margins(g: &mut Grid) -> bool {
    loop {
        let c = &g.cells;
        if (0..c.h - 1).any(|y| {
            let a = c.at(0, y);
            a.h_line != 0 || a.v_line != 0 || a.full != 0
        }) {
            break;
        }
        remove_empty_column(g, 0);
        if g.cells.w <= 2 {
            return true;
        }
    }
    loop {
        let c = &g.cells;
        if (0..c.h - 1).any(|y| {
            let (a, b) = (c.at(c.w - 2, y), c.at(c.w - 1, y));
            a.full != 0 || b.v_line != 0 || b.h_line != 0
        }) {
            break;
        }
        let w = g.cells.w;
        remove_empty_column(g, w - 1);
        if g.cells.w <= 2 {
            return true;
        }
    }
    loop {
        let c = &g.cells;
        if (0..c.w - 1).any(|x| {
            let a = c.at(x, 0);
            a.h_line != 0 || a.v_line != 0 || a.full != 0
        }) {
            break;
        }
        remove_empty_row(g, 0);
        if g.cells.h <= 2 {
            return true;
        }
    }
    loop {
        let c = &g.cells;
        if (0..c.w - 1).any(|x| {
            let (a, b) = (c.at(x, c.h - 2), c.at(x, c.h - 1));
            a.full != 0 || b.v_line != 0 || b.h_line != 0
        }) {
            break;
        }
        let h = g.cells.h;
        remove_empty_row(g, h - 1);
        if g.cells.h <= 2 {
            return true;
        }
    }
    false
}

/// `merge_row`: rows `y` and `y+1` made one.
fn merge_row(g: &mut Grid, y: usize) {
    let (w, h) = (g.cells.w, g.cells.h);
    let cell = &mut g.cells.cell;
    let d0 = y * w;
    for x in 0..w - 1 {
        let b = cell[d0 + x + w];
        let a = &mut cell[d0 + x];
        if a.full == 0 {
            a.full = b.full;
        }
        if a.v_crossed == 0 {
            a.v_crossed = b.v_crossed;
        }
        a.content = a.content.union(b.content);
        a.fontsize = a.fontsize.max(b.fontsize);
    }
    // MuPDF moves on from the row's last cell.
    let d = d0 + w - 1;
    if y + 2 < h {
        cell.copy_within(d + w..d + w + (h - 2 - y) * w, d);
    }
    g.cells.h -= 1;
    cell.truncate(w * g.cells.h);
    remove_divider(&mut g.ypos, y + 1);
}

/// `merge_rows`.
fn merge_rows(g: &mut Grid) {
    let mut y = g.cells.h as i64 - 3;
    while y >= 0 {
        let yu = y as usize;
        let w = g.cells.w;
        let c = &g.cells;
        let same_v = |a: &Cell, b: &Cell| (a.v_line != 0) == (b.v_line != 0);
        let all =
            |f: &dyn Fn(&Cell, &Cell) -> bool| (0..w - 1).all(|x| f(c.at(x, yu), c.at(x, yu + 1)));
        if all(&|a, b| a.full == 0 && same_v(a, b) && b.h_line == 0)
            || all(&|a, b| b.full == 0 && same_v(a, b) && b.h_line == 0)
        {
            merge_row(g, yu);
            y -= 1;
            continue;
        }
        // (The option to collapse bordered cells vertically is off.)
        let mut blocked = false;
        let mut mergeable = true;
        for x in 0..w - 1 {
            let (a, b) = (c.at(x, yu), c.at(x, yu + 1));
            if b.h_line != 0 {
                blocked = true;
                break;
            }
            if a.full == 0 || b.full == 0 {
                continue;
            }
            if !same_v(a, b) {
                blocked = true;
                break;
            }
            if b.h_crossed != 0 {
                continue;
            }
            let fs = a.fontsize.max(b.fontsize);
            if a.content.y1 + fs / 2.0 >= b.content.y0 {
                continue;
            }
            mergeable = false;
            break;
        }
        if blocked {
            y -= 1;
            continue;
        }
        if mergeable {
            merge_row(g, yu);
            y -= 1;
            continue;
        }
        // No line in the way: a row whose every full cell is crossed into
        // from above or crosses into the row below straddles; merge it.
        if yu + 2 < g.cells.h {
            let c = &g.cells;
            let straddles = (0..w - 1).all(|x| {
                let (b, d) = (c.at(x, yu + 1), c.at(x, yu + 2));
                b.full == 0 || b.h_crossed != 0 || d.h_crossed != 0
            });
            if straddles {
                merge_row(g, yu);
            }
        }
        y -= 1;
    }
}

/// `remove_bordered_empty_cells`: an empty region bordered all round,
/// whose borders go no further, made one full cell.
fn remove_bordered_empty_cells(g: &mut Grid) -> bool {
    let mut changed = false;
    let c = &mut g.cells;
    let (w, h) = (c.w, c.h);
    for y in 0..h - 1 {
        'region: for x in 0..w - 1 {
            {
                let cell = *c.at(x, y);
                if cell.full != 0 || cell.h_line == 0 || cell.v_line == 0 {
                    continue;
                }
                if x > 0 && c.at(x - 1, y).h_line != 0 {
                    continue;
                }
                if y > 0 && c.at(x, y - 1).v_line != 0 {
                    continue;
                }
            }
            let mut x1 = x + 1;
            while x1 < w {
                if y > 0 && c.at(x1, y - 1).v_line != 0 {
                    continue 'region;
                }
                let cell = c.at(x1, y);
                if cell.h_line != 0 && cell.v_line == 0 {
                    x1 += 1;
                    continue;
                }
                if cell.h_line != 0 || cell.v_line == 0 {
                    continue 'region;
                }
                break;
            }
            if x1 == w {
                continue;
            }
            let mut y1 = y + 1;
            while y1 < h {
                let cell = *c.at(x, y1);
                if cell.h_line == 0 && cell.v_line != 0 {
                    for x2 in x + 1..x1 {
                        let cc = c.at(x2, y1);
                        if cc.full != 0 || cc.h_line != 0 || cc.v_line != 0 {
                            continue 'region;
                        }
                    }
                    let cc = c.at(x1, y1);
                    if cc.h_line != 0 || cc.v_line == 0 {
                        continue 'region;
                    }
                } else if cell.h_line != 0 && cell.v_line == 0 {
                    for x2 in x + 1..x1 {
                        let cc = c.at(x2, y1);
                        if cc.h_line == 0 || cc.v_line != 0 {
                            continue 'region;
                        }
                    }
                    let cc = c.at(x1, y1);
                    if cc.h_line != 0 || cc.v_line != 0 {
                        continue 'region;
                    }
                    break;
                } else {
                    continue 'region;
                }
                y1 += 1;
            }
            if y1 == h {
                continue;
            }
            for y2 in y..y1 {
                for x2 in x..x1 {
                    let cc = c.at_mut(x2, y2);
                    cc.h_line = 0;
                    cc.v_line = 0;
                    cc.full = 1;
                    if x2 > x {
                        cc.v_crossed = 1;
                    }
                    if y2 > y {
                        cc.h_crossed = 1;
                    }
                }
                c.at_mut(x1, y2).v_line = 0;
            }
            for x2 in x..x1 {
                c.at_mut(x2, y1).h_line = 0;
            }
            changed = true;
        }
    }
    changed
}

/// `score_table`: the average badness of a cell, lower being better.
fn score_table(g: &Grid) -> f32 {
    let c = &g.cells;
    let (w, h) = (c.w, c.h);
    let mut score = 0;
    let num_cells = (w - 1) * (h - 1);
    for y in 0..h - 1 {
        for x in 0..w - 1 {
            let (cell, right, below) = (c.at(x, y), c.at(x + 1, y), c.at(x, y + 1));
            score += i32::from(cell.h_crossed != 0) + i32::from(cell.v_crossed != 0);
            if cell.full != 0
                || (cell.h_line != 0 && cell.v_line != 0 && right.v_line != 0 && below.h_line != 0)
                || plausibly_bordered(c, x, y)
            {
            } else if (cell.h_line != 0 && below.h_line != 0)
                || (cell.v_line != 0 && right.v_line != 0)
            {
                score += 1;
            } else {
                score += 2;
            }
        }
    }
    (f64::from(score as f32 / num_cells as f32) / 2.0) as f32
}

/// `find_table`: whether `g.bounds` holds a table, and its grid.
fn find_table(t: &Lists, g: &mut Grid) -> bool {
    if all_justified_or_headers(t, PAGE, g.bounds) {
        return false;
    }
    if !g.has_positions {
        let (mut xs, mut ys) = (Vec::new(), Vec::new());
        walk_to_find_content(t, &mut xs, &mut ys, PAGE, g.bounds, 0);
        sanitize(&mut xs);
        sanitize(&mut ys);
        if xs.len() < 2 || ys.len() < 2 {
            return false;
        }
        let (Some(xp), Some(yp)) = (
            make_positions(&xs, g.bounds.x0, g.bounds.x1),
            make_positions(&ys, g.bounds.y0, g.bounds.y1),
        ) else {
            return false;
        };
        g.xpos = xp;
        g.ypos = yp;
        g.has_positions = true;
    }
    g.cells = Cells::new(g.xpos.list.len(), g.ypos.list.len());
    walk_for_background(t, g, PAGE);
    // Lines refine the dividers, then mark the cells' borders.
    walk_grid_lines(t, g, PAGE, false, 0);
    walk_grid_lines(t, g, PAGE, true, 0);
    if g.xpos.list.len() <= 2 || g.ypos.list.len() <= 2 {
        return false;
    }
    for c in &mut g.cells.cell {
        c.content = Rect::EMPTY;
    }
    if calculate_spanned(t, g, PAGE, 0) > 0 {
        return false;
    }
    if trim_empty_margins(g) {
        return false;
    }
    // Over-segmented? Merge columns and rows while bordered empty cells
    // keep turning up.
    for _ in 0..=g.cells.w * g.cells.h {
        merge_columns(g);
        merge_rows(g);
        if !remove_bordered_empty_cells(g) {
            break;
        }
    }
    if g.xpos.list.len() <= 2 || g.ypos.list.len() <= 2 {
        return false;
    }
    g.score = score_table(g);
    true
}

// ---- candidates -----------------------------------------------------------

/// `fz_potential_table`.
#[derive(Debug, Clone, Default)]
struct Potential {
    data: Grid,
    from_raft: bool,
}

fn potential(bounds: Rect, from_raft: bool) -> Potential {
    Potential {
        data: Grid {
            bounds,
            ..Grid::default()
        },
        from_raft,
    }
}

fn contains(a: Rect, b: Rect) -> bool {
    a.contains(b)
}

/// `drop_contained_tables`: those in `lo..hi` within `at` dropped; the new
/// end of the range.
fn drop_contained(list: &mut [Potential], lo: usize, hi: usize, at: usize) -> usize {
    let mut j = lo;
    for i in lo..hi {
        if i != at && contains(list[at].data.bounds, list[i].data.bounds) {
            continue;
        }
        if i != j {
            list[j] = list[i].clone();
        }
        j += 1;
    }
    j
}

/// `cull_unhelpful_background_fills`: an opaque fill within another of the
/// same color, with nothing between them over it, removed.
fn cull_unhelpful_background_fills(t: &mut Lists) {
    let opaque = |b: Option<&Block>| match b {
        Some(Block::Vector(v))
            if v.flags & vector_flags::IS_RECTANGLE != 0
                && v.flags & vector_flags::IS_STROKED == 0 =>
        {
            Some(*v)
        }
        _ => None,
    };
    let mut iter = t.begin_dfs();
    let mut budget = t.nodes.len() * 4 + 4;
    while !t.eod_dfs(iter) {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let Some(b) = iter.block else {
            break;
        };
        if let Some(v) = opaque(t.leaf(b)).filter(|v| v.argb >> 24 == 0xff) {
            let start = Iter {
                block: Some(b),
                parent: iter.parent,
                top: iter.parent,
            };
            let mut iter2 = t.next(start);
            let mut budget2 = t.nodes.len() * 4 + 4;
            while !t.eod_dfs(iter2) && budget2 > 0 {
                budget2 -= 1;
                let Some(b2) = iter2.block else {
                    break;
                };
                let candidate = opaque(t.leaf(b2))
                    .is_some_and(|v2| v2.argb == v.argb && v.bbox.contains(v2.bbox));
                if candidate {
                    let b2box = t.bbox(b2);
                    let mut iter3 = t.next(start);
                    let mut clear = true;
                    let mut budget3 = t.nodes.len() * 4 + 4;
                    while iter3.block != iter2.block && budget3 > 0 {
                        budget3 -= 1;
                        let Some(b3) = iter3.block else {
                            clear = false;
                            break;
                        };
                        if !b2box.intersect(t.bbox(b3)).is_empty() {
                            clear = false;
                            break;
                        }
                        iter3 = t.next_dfs(iter3);
                    }
                    // MuPDF unlinks through the block before; there is one,
                    // since the outer fill comes first.
                    if clear && t.nodes[b2].prev.is_some() {
                        t.unlink(iter2.parent, b2);
                    }
                }
                iter2 = t.next_dfs(iter2);
            }
        }
        iter = t.next_dfs(iter);
    }
}

/// `table_size_cmp`: by area, a stable sort keeping ties in order.
fn area(r: Rect) -> f32 {
    let (w, h) = (r.x1 - r.x0, r.y1 - r.y0);
    if w > 0.0 && h > 0.0 { w * h } else { 0.0 }
}

/// `hunt_potential_tables`: the candidates found, culled to the best that
/// do not overlap, and transcribed.
fn hunt(t: &mut Lists, mut list: Vec<Potential>, limit: f32) {
    cull_unhelpful_background_fills(t);
    list.retain_mut(|p| find_table(t, &mut p.data));
    // Tables within ones from rafts go.
    let rafts: Vec<Rect> = list
        .iter()
        .filter(|p| p.from_raft)
        .map(|p| p.data.bounds)
        .collect();
    list.retain(|p| p.from_raft || !rafts.iter().any(|&r| contains(r, p.data.bounds)));
    list.sort_by(|a, b| area(a.data.bounds).total_cmp(&area(b.data.bounds)));

    // A table containing others is kept, or they are, by the average
    // badness of their cells.
    let mut n = list.len();
    let mut j = 0;
    let mut i = 0;
    while i < n {
        let mut overlapped = 0.0f32;
        let mut num_overlaps = 0usize;
        let mut k = 0;
        while k < n {
            if k == j {
                k = i + 1;
                if k == n {
                    break;
                }
            }
            if contains(list[i].data.bounds, list[k].data.bounds) {
                let c = &list[k].data.cells;
                let cells = (c.w - 1) * (c.h - 1);
                if cells > 0 {
                    // Fused, as clang compiles MuPDF's `+= score * cells`.
                    overlapped = list[k].data.score.mul_add(cells as f32, overlapped);
                }
                num_overlaps += cells;
            }
            k += 1;
        }
        if num_overlaps > 0 {
            overlapped /= num_overlaps as f32;
            if overlapped <= list[i].data.score {
                i += 1;
                continue;
            }
            j = drop_contained(&mut list, 0, j, i);
            n = drop_contained(&mut list, i + 1, n, i);
        }
        if i != j {
            list[j] = list[i].clone();
        }
        j += 1;
        i += 1;
    }
    list.truncate(j);

    // Of two that overlap, the better (or, scoring the same, the bigger).
    let mut n = list.len();
    let mut j = 0;
    let mut i = 0;
    while i < n {
        let mut k = 0;
        let mut dropped = false;
        while k < n {
            if k == j {
                k = i + 1;
                if k == n {
                    break;
                }
            }
            if !list[i]
                .data
                .bounds
                .intersect(list[k].data.bounds)
                .is_empty()
            {
                let (si, sk) = (list[i].data.score, list[k].data.score);
                if si > sk || (si == sk && i < k) {
                    dropped = true;
                    break;
                }
            }
            k += 1;
        }
        if dropped {
            i += 1;
            continue;
        }
        if i != j {
            list[j] = list[i].clone();
        }
        j += 1;
        i += 1;
    }
    list.truncate(j);
    n = list.len();
    let _ = n;
    list.retain(|p| p.data.score <= limit);
    for p in &list {
        transcribe(t, &p.data, PAGE);
    }
}

/// `push_trimmed_segment`: the segment without single lines across its
/// top and bottom, titles and captions, as another candidate.
fn push_trimmed_segment(t: &Lists, list: &mut Vec<Potential>, parent: usize, mut bounds: Rect) {
    let mut reduced = false;
    let mut iter = t.begin_from_dfs(t.lists[parent].first, parent);
    let mut budget = t.nodes.len() * 4 + 4;
    'prefix: while !t.eod_dfs(iter) && budget > 0 {
        budget -= 1;
        loop {
            let Some(b) = iter.block else {
                break 'prefix;
            };
            let Some(Block::Text(tb)) = t.leaf(b) else {
                break;
            };
            if tb.bbox.y0 != bounds.y0 {
                break;
            }
            let lines = &tb.lines;
            for (li, line) in lines.iter().enumerate() {
                if line.bbox.y0 != bounds.y0 {
                    break 'prefix;
                }
                match lines.get(li + 1) {
                    None => {
                        bounds.y0 = line.bbox.y1;
                        reduced = true;
                    }
                    Some(next) if next.bbox.y0 >= line.bbox.y1 => {
                        bounds.y0 = next.bbox.y0;
                        reduced = true;
                    }
                    Some(_) => break 'prefix,
                }
            }
            iter = t.next_dfs(iter);
            if t.eod_dfs(iter) {
                break 'prefix;
            }
            let Some(nb) = iter.block else {
                break 'prefix;
            };
            bounds.y0 = t.bbox(nb).y0;
            if budget == 0 {
                break 'prefix;
            }
            budget -= 1;
        }
        iter = t.next_dfs(iter);
    }
    let mut iter = t.begin_from_rdfs(t.lists[parent].last, parent);
    let mut budget = t.nodes.len() * 4 + 4;
    'suffix: while !t.eod_dfs(iter) && budget > 0 {
        budget -= 1;
        loop {
            let Some(b) = iter.block else {
                break 'suffix;
            };
            let Some(Block::Text(tb)) = t.leaf(b) else {
                break;
            };
            if tb.bbox.y1 != bounds.y1 {
                break;
            }
            let lines = &tb.lines;
            for li in (0..lines.len()).rev() {
                let line = &lines[li];
                if line.bbox.y1 != bounds.y1 {
                    break 'suffix;
                }
                match li.checked_sub(1).map(|p| &lines[p]) {
                    None => {
                        bounds.y1 = line.bbox.y0;
                        reduced = true;
                    }
                    Some(prev) if prev.bbox.y1 <= line.bbox.y0 => {
                        bounds.y1 = prev.bbox.y1;
                        reduced = true;
                    }
                    Some(_) => break 'suffix,
                }
            }
            iter = t.next_rdfs(iter);
            if t.eod_dfs(iter) {
                break 'suffix;
            }
            let Some(pb) = iter.block else {
                break 'suffix;
            };
            bounds.y1 = t.bbox(pb).y1;
            if budget == 0 {
                break 'suffix;
            }
            budget -= 1;
        }
        iter = t.next_rdfs(iter);
    }
    if reduced {
        list.push(potential(bounds, false));
    }
}

/// `push_segment_areas`: each segment, trimmed and not, as a candidate.
fn push_segment_areas(
    t: &Lists,
    list: &mut Vec<Potential>,
    parent: usize,
    bounds: Rect,
    depth: usize,
) {
    if depth >= 64 {
        return;
    }
    for n in t.children(parent) {
        let Some(d) = t.down(n) else {
            continue;
        };
        let bb = t.bbox(n);
        let r = bb.intersect(bounds);
        if r.is_empty() {
            continue;
        }
        push_segment_areas(t, list, d, bounds, depth + 1);
        if t.lists[d].standard == "Div" && t.lists[d].raw == "Split" && contains(bounds, bb) {
            list.push(potential(bb, false));
            push_trimmed_segment(t, list, d, r);
        }
    }
}

// ---- rafts ------------------------------------------------------------------

/// `overlap_or_abut`, forgiving a tenth of a point.
fn overlap_or_abut(r: Rect, s: Rect) -> bool {
    const FUDGE: f32 = 0.1;
    r.x1 >= s.x0 - FUDGE && r.x0 <= s.x1 + FUDGE && r.y1 >= s.y0 - FUDGE && r.y0 <= s.y1 + FUDGE
}

/// `add_plank_to_flotilla`: a rectangle joining the raft it touches, and
/// rafts it bridges joining up.
fn add_plank(rafts: &mut Vec<Rect>, rect: Rect) {
    let mut found = None;
    for i in (0..rafts.len()).rev() {
        if overlap_or_abut(rect, rafts[i]) {
            let r = rafts[i].union(rect);
            if r == rafts[i] {
                return;
            }
            rafts[i] = r;
            found = Some(i);
            break;
        }
    }
    let Some(mut i) = found else {
        rafts.push(rect);
        return;
    };
    for _ in 0..=rafts.len() * rafts.len() + 1 {
        let mut changed = false;
        let mut j = rafts.len();
        while j > i + 1 {
            j -= 1;
            if overlap_or_abut(rafts[j], rafts[i]) {
                rafts[i] = rafts[j].union(rafts[i]);
                let last = rafts.len() - 1;
                if j != last {
                    rafts[j] = rafts[last];
                }
                rafts.pop();
                changed = true;
            }
        }
        let mut j = i;
        while j > 0 {
            j -= 1;
            if overlap_or_abut(rafts[j], rafts[i]) {
                rafts[j] = rafts[j].union(rafts[i]);
                let last = rafts.len() - 1;
                if i != last {
                    rafts[i] = rafts[last];
                }
                rafts.pop();
                i = j;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// `fz_new_flotilla_from_stext_page_vectors`: the areas of rectangles
/// that touch.
fn flotilla(t: &Lists) -> Vec<Rect> {
    let mut rafts = Vec::new();
    let mut it = t.begin_dfs();
    let mut budget = t.nodes.len() * 4 + 4;
    while !t.eod_dfs(it) && budget > 0 {
        budget -= 1;
        if let Some(b) = it.block
            && let Some(Block::Vector(v)) = t.leaf(b)
            && v.flags & vector_flags::IS_RECTANGLE != 0
        {
            add_plank(&mut rafts, v.bbox);
        }
        it = t.next_dfs(it);
    }
    rafts
}

/// `fz_table_hunt`.
pub fn table_hunt(page: &mut StextPage) {
    let bounds = Rect::INFINITE;
    let mut t = Lists::new(std::mem::take(&mut page.blocks));
    let mut list: Vec<Potential> = flotilla(&t)
        .into_iter()
        .filter(|&r| contains(bounds, r))
        .map(|r| potential(r, true))
        .collect();
    push_segment_areas(&t, &mut list, PAGE, bounds, 0);
    // A nasty heuristic constant, as MuPDF says.
    hunt(&mut t, list, 0.3);
    page.blocks = t.into_blocks();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(list: &[(bool, f32)]) -> Vec<Edge> {
        let mut v = Vec::new();
        for &(left, pos) in list {
            div_push(&mut v, left, false, pos);
        }
        v
    }

    #[test]
    fn edges_stay_sorted_and_count_repeats() {
        let v = edges(&[
            (true, 10.0),
            (false, 20.0),
            (true, 10.0),
            (true, 30.0),
            (false, 40.0),
        ]);
        let got: Vec<(bool, f32, i32)> = v.iter().map(|e| (e.left, e.pos, e.freq)).collect();
        assert_eq!(
            got,
            [
                (true, 10.0, 2),
                (false, 20.0, 1),
                (true, 30.0, 1),
                (false, 40.0, 1)
            ]
        );
        // A start and a stop at one place: the start first, whichever came
        // first.
        let v = edges(&[(true, 10.0), (true, 20.0), (false, 20.0)]);
        assert!(v[1].left && !v[2].left);
        let v = edges(&[(true, 10.0), (false, 20.0), (true, 20.0)]);
        assert!(v[1].left && !v[2].left);
    }

    #[test]
    fn dividers_sit_in_the_gaps_between_content() {
        // Two columns: 10..20 and 30..40, each holding two pieces.
        let mut v = edges(&[(true, 10.0), (false, 20.0), (true, 30.0), (false, 40.0)]);
        div_push(&mut v, true, false, 10.0);
        div_push(&mut v, false, false, 20.0);
        sanitize(&mut v);
        let p = make_positions(&v, 0.0, 50.0).unwrap_or_default();
        let pos: Vec<f32> = p.list.iter().map(|d| d.pos).collect();
        assert_eq!(pos, [0.0, 25.0, 50.0]);
        assert_eq!(p.list[1].uncertainty, 0);
        assert_eq!(p.max_uncertainty, 2);
    }

    #[test]
    fn gaps_bounded_only_by_spaces_are_taken_away() {
        // One piece spanning everything, with a weak gap inside it from a
        // run of spaces: no divider at the gap.
        let mut v = Vec::new();
        div_push(&mut v, true, false, 0.0);
        div_push(&mut v, false, true, 10.0);
        div_push(&mut v, true, true, 12.0);
        div_push(&mut v, false, false, 20.0);
        div_push(&mut v, true, false, 5.0);
        div_push(&mut v, false, false, 15.0);
        sanitize(&mut v);
        let p = make_positions(&v, 0.0, 20.0).unwrap_or_default();
        assert_eq!(p.list.len(), 2);
    }

    #[test]
    fn rafts_join_touching_rectangles_and_bridge() {
        let mut rafts = Vec::new();
        add_plank(&mut rafts, Rect::new(0.0, 0.0, 10.0, 10.0));
        add_plank(&mut rafts, Rect::new(20.0, 0.0, 30.0, 10.0));
        assert_eq!(rafts.len(), 2);
        // Within one: nothing changes.
        add_plank(&mut rafts, Rect::new(2.0, 2.0, 3.0, 3.0));
        assert_eq!(rafts.len(), 2);
        // A bridge joins both.
        add_plank(&mut rafts, Rect::new(10.0, 4.0, 20.0, 5.0));
        assert_eq!(rafts, [Rect::new(0.0, 0.0, 30.0, 10.0)]);
    }

    #[test]
    fn a_page_without_content_has_no_tables() {
        let mut page = StextPage {
            mediabox: Rect::new(0.0, 0.0, 600.0, 800.0),
            blocks: Vec::new(),
        };
        table_hunt(&mut page);
        assert!(page.blocks.is_empty());
    }
}
