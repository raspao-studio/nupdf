// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/list-device.c (the node rectangles of
// fz_append_display_node and the fz_list_* callers, and the culling in
// fz_run_display_list), Copyright (C) 2004-2026 Artifex Software, Inc.

//! A display list played as `fz_run_display_list` plays it: each call has
//! the rectangle the list device gave it, a clip's narrowed to what was
//! drawn inside it, and calls with nothing to show are left out, with
//! everything inside a clip that shows nothing. Drawing does not change
//! for it, but a device that reads its clips does: the structured text
//! `mutool draw -F txt` gathers comes through a display list, and so
//! leaves out text outside the box of what its clip holds.

use crate::device::Device;
use crate::draw::list::{Cmd, DisplayList};
use crate::geometry::Rect;

/// The list device's clip stack depth (`STACK_SIZE`).
const STACK_SIZE: usize = 96;

/// The rectangle the list device gives a call, before clips are narrowed.
fn own_rect(cmd: &Cmd) -> Option<Rect> {
    Some(match cmd {
        Cmd::FillPath(p, _, m, _) => p.bound(None, *m),
        Cmd::Drawn(r) => *r,
        Cmd::StrokePath(p, s, m, _) => p.bound(Some(s), *m),
        Cmd::ClipPath(p, _, m, r) => p.bound(None, *m).intersect(*r),
        Cmd::ClipStrokePath(p, s, m, r) => p.bound(Some(s), *m).intersect(*r),
        Cmd::FillShade(s, m, _, _) => s.bound(*m),
        Cmd::FillImage(_, m, _, _) | Cmd::FillImageMask(_, m, _) => Rect::UNIT.transform(*m),
        Cmd::ClipImageMask(_, m, r) => Rect::UNIT.transform(*m).intersect(*r),
        Cmd::FillText(t, m, _) | Cmd::IgnoreText(t, m) => t.bound(None, *m),
        Cmd::StrokeText(t, s, m, _) => t.bound(Some(s), *m),
        Cmd::ClipText(t, m, r) => t.bound(None, *m).intersect(*r),
        Cmd::ClipStrokeText(t, s, m, r) => t.bound(Some(s), *m).intersect(*r),
        Cmd::BeginMask(r, ..) | Cmd::BeginGroup(r, ..) | Cmd::BeginTile(r, ..) => *r,
        _ => return None,
    })
}

fn is_clip(cmd: &Cmd) -> bool {
    matches!(
        cmd,
        Cmd::ClipPath(..)
            | Cmd::ClipStrokePath(..)
            | Cmd::ClipText(..)
            | Cmd::ClipStrokeText(..)
            | Cmd::ClipImageMask(..)
    )
}

/// `fz_append_display_node`'s rectangles: each call's own, a clip's
/// narrowed when it is popped to the union of what was drawn inside it,
/// a pop given that union. None where the list keeps the last one.
fn node_rects(cmds: &[Cmd]) -> Vec<Option<Rect>> {
    let mut rects: Vec<Option<Rect>> = cmds.iter().map(own_rect).collect();
    // Each open clip or mask: what was drawn inside, and the clip's node.
    let mut stack: Vec<(Rect, Option<usize>)> = Vec::new();
    let mut top = 0usize;
    let mut tiled = 0usize;
    for (i, cmd) in cmds.iter().enumerate() {
        let mut rect = rects[i];
        let mut union = true;
        match cmd {
            c if is_clip(c) => {
                if top < STACK_SIZE {
                    stack.truncate(top);
                    stack.push((Rect::EMPTY, Some(i)));
                }
                top += 1;
                union = false;
            }
            Cmd::EndMask(_) => {
                if top < STACK_SIZE {
                    stack.truncate(top);
                    stack.push((Rect::EMPTY, None));
                }
                top += 1;
                union = false;
            }
            Cmd::BeginTile(..) => {
                tiled += 1;
                if top > 0 && top <= STACK_SIZE {
                    stack[top - 1].0 = Rect::INFINITE;
                }
                union = false;
            }
            Cmd::EndTile => {
                tiled = tiled.saturating_sub(1);
                union = false;
            }
            Cmd::EndGroup => union = false,
            Cmd::PopClip => {
                if top > STACK_SIZE {
                    top -= 1;
                    rect = Some(Rect::INFINITE);
                } else if top > 0 {
                    top -= 1;
                    let (inside, update) = stack[top];
                    rect = Some(if tiled == 0 {
                        match update {
                            Some(j) => {
                                let narrowed = rects[j].unwrap_or(Rect::INFINITE).intersect(inside);
                                rects[j] = Some(narrowed);
                                narrowed
                            }
                            None => inside,
                        }
                    } else {
                        Rect::INFINITE
                    });
                }
            }
            _ => {}
        }
        if union
            && top > 0
            && tiled == 0
            && top <= STACK_SIZE
            && let Some(r) = rect
        {
            stack[top - 1].0 = stack[top - 1].0.union(r);
        }
        rects[i] = rect;
    }
    rects
}

/// `fz_run_display_list` with an identity matrix and no scissor: the calls
/// onto `dev`, culled as MuPDF culls them, each clip given its narrowed
/// rectangle as its scissor.
pub fn replay(list: &DisplayList, dev: &mut dyn Device) {
    let rects = node_rects(&list.cmds);
    let mut rect = Rect::new(0.0, 0.0, 0.0, 0.0);
    let mut clipped = 0usize;
    let mut tiled = 0usize;
    let mut skip_tile = 0usize;
    for (cmd, own) in list.cmds.iter().zip(&rects) {
        if let Some(r) = own {
            rect = *r;
        }
        if skip_tile > 0 {
            match cmd {
                Cmd::BeginTile(..) => skip_tile += 1,
                Cmd::EndTile => skip_tile -= 1,
                _ => {}
            }
            if skip_tile > 0 {
                continue;
            }
        }
        let empty = match cmd {
            _ if tiled > 0 => false,
            Cmd::BeginTile(..)
            | Cmd::EndTile
            | Cmd::DefaultColorSpaces(_)
            | Cmd::BeginLayer(_)
            | Cmd::EndLayer
            | Cmd::BeginStructure(..)
            | Cmd::EndStructure
            | Cmd::BeginMetatext(..)
            | Cmd::EndMetatext => false,
            // Zero-area paths and text (spaces) pass.
            Cmd::FillPath(..)
            | Cmd::StrokePath(..)
            | Cmd::Drawn(_)
            | Cmd::FillText(..)
            | Cmd::StrokeText(..)
            | Cmd::ClipText(..)
            | Cmd::ClipStrokeText(..) => !rect.is_valid(),
            _ => rect.is_empty(),
        };
        if clipped > 0 || empty {
            match cmd {
                c if is_clip(c) => {
                    clipped += 1;
                    continue;
                }
                Cmd::BeginMask(..) | Cmd::BeginGroup(..) => {
                    clipped += 1;
                    continue;
                }
                Cmd::BeginStructure(..)
                | Cmd::EndStructure
                | Cmd::BeginMetatext(..)
                | Cmd::EndMetatext => {}
                Cmd::PopClip | Cmd::EndGroup => {
                    if clipped > 0 {
                        clipped -= 1;
                        continue;
                    }
                }
                Cmd::EndMask(_) => {
                    if clipped > 0 {
                        continue;
                    }
                }
                _ => continue,
            }
        }
        match cmd {
            Cmd::FillPath(p, eo, m, c) => {
                dev.fill_path(p, *eo, *m, &c.cs, &c.color, c.alpha, c.params)
            }
            Cmd::StrokePath(p, s, m, c) => {
                dev.stroke_path(p, s, *m, &c.cs, &c.color, c.alpha, c.params)
            }
            Cmd::ClipPath(p, eo, m, _) => dev.clip_path(p, *eo, *m, rect),
            Cmd::ClipStrokePath(p, s, m, _) => dev.clip_stroke_path(p, s, *m, rect),
            Cmd::FillShade(s, m, a, cp) => dev.fill_shade(s, *m, *a, *cp),
            Cmd::FillImage(i, m, a, cp) => dev.fill_image(i, *m, *a, *cp),
            Cmd::FillImageMask(i, m, c) => {
                dev.fill_image_mask(i, *m, &c.cs, &c.color, c.alpha, c.params)
            }
            Cmd::ClipImageMask(i, m, _) => dev.clip_image_mask(i, *m, rect),
            Cmd::FillText(t, m, c) => dev.fill_text(t, *m, &c.cs, &c.color, c.alpha, c.params),
            Cmd::StrokeText(t, s, m, c) => {
                dev.stroke_text(t, s, *m, &c.cs, &c.color, c.alpha, c.params)
            }
            Cmd::ClipText(t, m, _) => dev.clip_text(t, *m, rect),
            Cmd::ClipStrokeText(t, s, m, _) => dev.clip_stroke_text(t, s, *m, rect),
            Cmd::IgnoreText(t, m) => dev.ignore_text(t, *m),
            // The device this was kept for reads no drawn path.
            Cmd::Drawn(_) => {}
            Cmd::PopClip => dev.pop_clip(),
            Cmd::BeginMask(_, l, cs, bc, cp) => dev.begin_mask(rect, *l, cs.as_ref(), bc, *cp),
            Cmd::EndMask(f) => dev.end_mask(f.as_ref()),
            Cmd::BeginGroup(_, cs, i, k, b, a) => {
                dev.begin_group(rect, cs.as_ref(), *i, *k, *b, *a)
            }
            Cmd::EndGroup => dev.end_group(),
            Cmd::BeginTile(_, v, xs, ys, m, id) => {
                tiled += 1;
                if dev.begin_tile(rect, *v, *xs, *ys, *m, *id) {
                    skip_tile = 1;
                }
            }
            Cmd::EndTile => {
                tiled = tiled.saturating_sub(1);
                dev.end_tile();
            }
            Cmd::DefaultColorSpaces(d) => dev.set_default_colorspaces(d),
            Cmd::BeginLayer(n) => dev.begin_layer(n),
            Cmd::EndLayer => dev.end_layer(),
            Cmd::BeginStructure(s, raw, i) => dev.begin_structure(s, raw, *i),
            Cmd::EndStructure => dev.end_structure(),
            Cmd::BeginMetatext(m, t) => dev.begin_metatext(*m, t),
            Cmd::EndMetatext => dev.end_metatext(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colorspace::ColorSpace;
    use crate::device::ColorParams;
    use crate::document::Document;
    use crate::draw::testkit::{pdf_of, stream};
    use crate::geometry::Matrix;
    use crate::page_run::Parts;
    use crate::path::{Path, StrokeState};

    /// Each clip's scissor and each fill, as they arrive.
    #[derive(Default)]
    struct Log(Vec<String>);

    impl Device for Log {
        fn fill_path(
            &mut self,
            p: &Path,
            _: bool,
            m: Matrix,
            _: &ColorSpace,
            _: &[f32],
            _: f32,
            _: ColorParams,
        ) {
            let r = p.bound(None, m);
            self.0
                .push(format!("fill {} {} {} {}", r.x0, r.y0, r.x1, r.y1));
        }
        fn clip_path(&mut self, _: &Path, _: bool, _: Matrix, s: Rect) {
            self.0
                .push(format!("clip {} {} {} {}", s.x0, s.y0, s.x1, s.y1));
        }
        fn pop_clip(&mut self) {
            self.0.push("pop".into());
        }
    }

    fn square(x0: f32, y0: f32, x1: f32, y1: f32) -> Path {
        let mut p = Path::new();
        p.rect_to(x0, y0, x1, y1);
        p
    }

    #[test]
    fn clips_narrow_to_what_they_hold_and_empty_ones_go() {
        let mut list = DisplayList::default();
        let id = Matrix::IDENTITY;
        let gray = ColorSpace::Gray;
        let cp = ColorParams::default();
        // A clip holding one small square, then one holding nothing that
        // shows (an empty path), then a square outside any clip.
        list.clip_path(&square(0.0, 0.0, 100.0, 100.0), false, id, Rect::INFINITE);
        list.fill_path(
            &square(10.0, 10.0, 20.0, 20.0),
            false,
            id,
            &gray,
            &[0.0],
            1.0,
            cp,
        );
        list.pop_clip();
        list.clip_path(&square(0.0, 0.0, 100.0, 100.0), false, id, Rect::INFINITE);
        list.fill_path(&Path::new(), false, id, &gray, &[0.0], 1.0, cp);
        list.pop_clip();
        list.fill_path(
            &square(30.0, 30.0, 40.0, 40.0),
            false,
            id,
            &gray,
            &[0.0],
            1.0,
            cp,
        );
        let mut log = Log::default();
        replay(&list, &mut log);
        assert_eq!(
            log.0,
            [
                "clip 10 10 20 20",
                "fill 10 10 20 20",
                "pop",
                "fill 30 30 40 40",
            ]
        );
    }

    /// What a device that reads clips and not paths is told: each clip's,
    /// mask's, group's and tile's rectangle, and each end.
    #[derive(Default)]
    struct Clips(Vec<String>);

    impl Clips {
        fn rect(&mut self, what: &str, r: Rect) {
            self.0
                .push(format!("{what} {} {} {} {}", r.x0, r.y0, r.x1, r.y1));
        }
    }

    impl Device for Clips {
        fn clip_path(&mut self, _: &Path, _: bool, _: Matrix, s: Rect) {
            self.rect("clip", s);
        }
        fn clip_stroke_path(&mut self, _: &Path, _: &StrokeState, _: Matrix, s: Rect) {
            self.rect("clip-stroke", s);
        }
        fn pop_clip(&mut self) {
            self.0.push("pop".into());
        }
        fn begin_mask(
            &mut self,
            r: Rect,
            _: bool,
            _: Option<&ColorSpace>,
            _: &[f32],
            _: ColorParams,
        ) {
            self.rect("mask", r);
        }
        fn end_mask(&mut self, _: Option<&crate::function::Function>) {
            self.0.push("end-mask".into());
        }
        fn begin_group(
            &mut self,
            r: Rect,
            _: Option<&ColorSpace>,
            _: bool,
            _: bool,
            _: u8,
            _: f32,
        ) {
            self.rect("group", r);
        }
        fn end_group(&mut self) {
            self.0.push("end-group".into());
        }
        fn begin_tile(&mut self, r: Rect, _: Rect, _: f32, _: f32, _: Matrix, _: i32) -> bool {
            self.rect("tile", r);
            false
        }
        fn end_tile(&mut self) {
            self.0.push("end-tile".into());
        }
        fn begin_layer(&mut self, _: &[u8]) {
            self.0.push("layer".into());
        }
    }

    /// The same calls into a list that keeps every path and into one that
    /// keeps drawn paths as rectangles.
    fn both(calls: impl Fn(&mut DisplayList)) -> (DisplayList, DisplayList) {
        let mut whole = DisplayList::default();
        let mut rects = DisplayList {
            drawn_as_rects: true,
            ..DisplayList::default()
        };
        calls(&mut whole);
        calls(&mut rects);
        (whole, rects)
    }

    fn clips_of(list: &DisplayList) -> Vec<String> {
        let mut clips = Clips::default();
        replay(list, &mut clips);
        clips.0
    }

    #[test]
    fn a_run_of_drawn_paths_is_kept_as_one_rectangle() {
        let id = Matrix::IDENTITY;
        let gray = ColorSpace::Gray;
        let cp = ColorParams::default();
        let stroke = StrokeState::default();
        let (whole, rects) = both(|list| {
            list.clip_path(&square(0.0, 0.0, 500.0, 500.0), false, id, Rect::INFINITE);
            for i in 0..1000u16 {
                let at = f32::from(i % 97);
                let p = square(at, at + 1.0, at + 3.0, at + 9.0);
                if i % 3 == 0 {
                    list.stroke_path(&p, &stroke, id, &gray, &[0.0], 1.0, cp);
                } else {
                    list.fill_path(&p, false, id, &gray, &[0.0], 1.0, cp);
                }
            }
            list.pop_clip();
        });
        assert_eq!(whole.cmds.len(), 1002);
        assert_eq!(rects.cmds.len(), 3);
        // The clip is narrowed to the same box either way.
        let narrowed = clips_of(&whole);
        assert_eq!(narrowed.len(), 2);
        assert!(narrowed[0].starts_with("clip ") && narrowed[0] != "clip 0 0 500 500");
        assert_eq!(clips_of(&rects), narrowed);
    }

    #[test]
    fn paths_kept_as_rectangles_leave_every_clip_as_it_was() {
        // Sequences of every call that opens, closes or fills a clip,
        // balanced or not, with paths that are empty, tiny, huge and
        // not-a-number among them: a device that reads no path is told
        // the same with the paths kept or not.
        let id = Matrix::IDENTITY;
        let gray = ColorSpace::Gray;
        let cp = ColorParams::default();
        let stroke = StrokeState::default();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut drawn = 0;
        for round in 0..400 {
            let calls: Vec<(u64, [f32; 4])> = (0..40 + round % 60)
                .map(|_| {
                    let mut at = || match next(12) {
                        0 => f32::NAN,
                        1 => f32::INFINITY,
                        2 => -1e30,
                        _ => next(400) as f32 - 100.0,
                    };
                    let (x, y) = (at(), at());
                    let size = [0.0, 0.5, 40.0, 1e9][next(4) as usize];
                    (next(16), [x, y, x + size, y + size])
                })
                .collect();
            let (whole, rects) = both(|list| {
                for &(what, [x0, y0, x1, y1]) in &calls {
                    let p = match what % 5 {
                        0 => Path::new(),
                        _ => square(x0, y0, x1, y1),
                    };
                    let area = Rect::new(x0, y0, x1, y1);
                    match what {
                        0..=4 => list.fill_path(&p, false, id, &gray, &[0.0], 1.0, cp),
                        5 | 6 => list.stroke_path(&p, &stroke, id, &gray, &[0.0], 1.0, cp),
                        7 | 8 => list.clip_path(&p, false, id, Rect::INFINITE),
                        9 => list.clip_stroke_path(&p, &stroke, id, area),
                        10 | 11 => list.pop_clip(),
                        12 => list.begin_group(area, None, false, false, 0, 1.0),
                        13 => list.end_group(),
                        14 => {
                            list.begin_mask(area, false, None, &[], cp);
                            list.end_mask(None);
                        }
                        _ => {
                            if x0 > 100.0 {
                                list.begin_tile(area, area, 10.0, 10.0, id, 0);
                            } else if y0 > 100.0 {
                                list.end_tile();
                            } else {
                                list.begin_layer(b"L");
                            }
                        }
                    }
                }
            });
            drawn += whole.cmds.len() - rects.cmds.len();
            assert_eq!(clips_of(&rects), clips_of(&whole), "round {round}");
        }
        assert!(drawn > 2000, "{drawn} paths joined make no test");
    }

    #[test]
    fn text_outside_what_its_clip_holds_is_left_out_as_mutool_draw_leaves_it() {
        // A space inside a clip that also holds a line: the clip narrows
        // to the line and the space's box, and the space's character,
        // whose quad is mostly outside that, is not gathered.
        // `mutool draw -F txt` gives "Hello"; `mutool draw -D`, which
        // has no display list, gives the space too.
        let pdf = pdf_of(&[
            "<</Type/Catalog/Pages 2 0 R>>",
            "<</Type/Pages/Kids[3 0 R]/Count 1>>",
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Contents 4 0 R\
             /Resources<</Font<</F1 5 0 R>>>>>>",
            &stream(
                "",
                "q 0 100 200 50 re W n BT /F1 10 Tf 50 120 Td ( ) Tj ET \
                 0 0 0 RG 10 140 m 190 140 l S Q BT /F1 10 Tf 20 50 Td (Hello) Tj ET",
            ),
            "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
        ]);
        let doc = Document::open(pdf).unwrap_or_else(|e| panic!("{e:?}"));
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e:?}"));
        let text = |parts| {
            let (p, _) = crate::stext::extract_page_parts(
                &doc,
                &page,
                crate::stext::Options::default(),
                crate::content::Store::default(),
                parts,
            );
            let mut out = String::new();
            crate::stext::blocks_text(&p.unwrap_or_else(|e| panic!("{e}")).blocks, &mut out);
            out
        };
        assert_eq!(text(Parts::ALL), "Hello\n\n");
        assert_eq!(text(Parts::CONTENTS), " \n\nHello\n\n");
    }
}
