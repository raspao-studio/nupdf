// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/tools/pdfmerge.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Merging documents as `mutool merge` does: each input's pages, in its
//! range, grafted into the destination with the external links they
//! carry, then the outline items that lead to the merged pages, with
//! their ancestors, appended to the destination's outline.

use crate::document::Document;
use crate::graft::GraftMap;
use crate::link::{create_links, is_external_link, load_links, resolve_link_page};
use crate::outline::{OutlineItem, OutlineIter};

/// Outline levels copied before giving up; MuPDF recurses without a
/// bound.
const MAX_OUTLINE_DEPTH: usize = 256;

/// A merge in progress: the destination and how many pages it has from
/// earlier inputs, which outline targets are counted from.
#[derive(Debug)]
pub struct Merger<'a> {
    dst: &'a Document,
    output_page_count: i32,
}

/// What one input gave: the pages merged (0-based), and the error that
/// stopped it, if any.
#[derive(Debug, Default)]
pub struct Merged {
    pub pages: Vec<i32>,
    pub error: Option<String>,
}

impl<'a> Merger<'a> {
    pub fn new(dst: &'a Document) -> Merger<'a> {
        Merger {
            dst,
            output_page_count: 0,
        }
    }

    /// `merge_range`: the pages `range` names (MuPDF's syntax), then the
    /// outline. An error keeps what came before it, as in MuPDF; the
    /// outline then is not copied and later inputs number their outline
    /// targets as if this input had given no pages.
    pub fn merge_range(&mut self, src: &Document, range: &str) -> Merged {
        let mut merged = Merged::default();
        if let Err(e) = self.merge_pages_and_outline(src, range, &mut merged.pages) {
            merged.error = Some(e);
        } else {
            self.output_page_count += merged.pages.len() as i32;
        }
        merged
    }

    fn merge_pages_and_outline(
        &mut self,
        src: &Document,
        range: &str,
        merged: &mut Vec<i32>,
    ) -> Result<(), String> {
        let count = src.page_count().map_err(|e| e.0)?;
        let mut map = GraftMap::new(self.dst);
        for page in crate::rearrange::parse_page_ranges(range, count, src) {
            self.page_merge(src, page, &mut map)?;
            merged.push(page);
        }

        let it_src = OutlineIter::new(src)?;
        let mut it_dst = OutlineIter::new(self.dst)?;
        // Run to the end of the destination's outline.
        if it_dst.item()?.is_some() {
            while it_dst.next_sibling() == 0 {}
        }
        if it_src.item()?.is_some() {
            let mut cor = CopyOutline {
                src,
                it_dst,
                it_src,
                range,
                page_count: count,
                items: Vec::new(),
                copied_to_depth: 0,
                page_output_base: self.output_page_count,
            };
            cor.copy_range(0)?;
        }
        Ok(())
    }

    /// `page_merge`: page `page` (0-based) of `src` appended to the
    /// destination, then its external links recreated on it. Links into
    /// the document are not copied, as in MuPDF.
    fn page_merge(&mut self, src: &Document, page: i32, map: &mut GraftMap) -> Result<(), String> {
        map.graft_page(src, page, -1)?;
        // pdf_load_page loads a page's links with its annotations.
        let src_page = src.page(page).map_err(|e| e.0)?;
        let links = if crate::link::has_annots(src, &src_page) {
            let ctm = src.page_transform(&src_page).map_err(|e| e.0)?.1;
            load_links(src, &src_page, page, ctm)
        } else {
            Vec::new()
        };
        let last = self.dst.page_count().map_err(|e| e.0)? - 1;
        let dst_page = self.dst.page(last).map_err(|e| e.0)?;
        let external = links
            .iter()
            .filter(|link| is_external_link(&link.uri))
            .map(|link| (link.rect, link.uri.as_str()));
        create_links(self.dst, &dst_page, external)
    }
}

/// `cor_state`: copying an outline range. `items` are the source items
/// from the top level down to where the copy is; the first
/// `copied_to_depth` of them are in the destination already.
struct CopyOutline<'a, 'b> {
    src: &'a Document,
    it_dst: OutlineIter<'b>,
    it_src: OutlineIter<'a>,
    range: &'a str,
    page_count: i32,
    items: Vec<OutlineItem>,
    copied_to_depth: usize,
    page_output_base: i32,
}

impl CopyOutline<'_, '_> {
    /// `do_copy_outline_range`: this level of the source outline, items
    /// leading to a merged page copied with any ancestors not copied yet.
    fn copy_range(&mut self, depth: usize) -> Result<(), String> {
        if depth > MAX_OUTLINE_DEPTH {
            return Err("outline tree too deep".into());
        }
        loop {
            let item = self.it_src.item()?.ok_or("lost the outline item")?;
            let page = resolve_link_page(self.src, item.uri.as_deref().unwrap_or(""))?;
            let in_range = position_in_range(self.range, self.page_count, page + 1);
            let new_page = in_range + self.page_output_base;
            self.items.push(OutlineItem {
                title: item.title.clone(),
                uri: rewrite_page(item.uri.as_deref(), new_page),
                is_open: item.is_open,
            });
            if in_range != 0 {
                self.copy_item()?;
            }
            let has_children = self.it_src.down();
            if has_children == 0 {
                self.copy_range(depth + 1)?;
            }
            if has_children >= 0 {
                self.it_src.up();
            }
            self.items.pop();
            if self.copied_to_depth > self.items.len() {
                self.copied_to_depth = self.items.len();
                self.it_dst.up();
            }
            self.it_dst.next_sibling();
            if self.it_src.next_sibling() != 0 {
                return Ok(());
            }
        }
    }

    /// `copy_item`: the items not copied yet, down to this one, all with
    /// this one's URI.
    fn copy_item(&mut self) -> Result<(), String> {
        let uri = self.items.last().and_then(|i| i.uri.clone());
        while self.copied_to_depth < self.items.len() {
            let mut item = self.items[self.copied_to_depth].clone();
            item.uri = uri.clone();
            self.it_dst.insert(&item)?;
            self.copied_to_depth += 1;
            self.it_dst.prev_sibling();
            self.it_dst.down();
        }
        Ok(())
    }
}

/// `position_in_range`: where page `page` (1-based) lands among the pages
/// `range` lists, from 1, or 0 when it is not there. A backward range
/// counts from its low end, as MuPDF counts it.
fn position_in_range(range: &str, count: i32, page: i32) -> i32 {
    let mut n = 0;
    for (start, end) in crate::rearrange::parse_page_range_pairs(range, count, None) {
        if start < end {
            if start <= page && page <= end {
                return n + page - start + 1;
            }
            n += end - start + 1;
        } else {
            if end <= page && page <= start {
                return n + page - end + 1;
            }
            n += start - end + 1;
        }
    }
    0
}

/// `rewrite_page`: a `#page=` URI with its page number replaced.
fn rewrite_page(uri: Option<&str>, n: i32) -> Option<String> {
    let uri = uri?;
    let Some(rest) = uri.strip_prefix("#page=") else {
        return Some(uri.to_string());
    };
    Some(match rest.find('&') {
        None => format!("#page={n}"),
        Some(i) => format!("#page={n}{}", &rest[i..]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_backward_ranges_from_their_low_end() {
        assert_eq!(position_in_range("2-4", 10, 3), 2);
        assert_eq!(position_in_range("1,4-2", 10, 3), 3);
        assert_eq!(position_in_range("1", 10, 3), 0);
        assert_eq!(
            rewrite_page(Some("#page=3&zoom=1,2,3"), 7).as_deref(),
            Some("#page=7&zoom=1,2,3")
        );
        assert_eq!(
            rewrite_page(Some("http://x"), 7).as_deref(),
            Some("http://x")
        );
    }
}
