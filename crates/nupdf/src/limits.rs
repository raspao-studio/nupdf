// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Resource limits for untrusted input.
//!
//! The defaults are MuPDF's own caps where it has them, so default behavior
//! matches MuPDF; the decompression cap and the cap on a page's glyphs
//! are nupdf's additions. Structural
//! depth limits (object nesting, page tree depth, reference chains) are
//! fixed constants rather than options: they exist to keep the stack safe.

use crate::object::MAX_OBJECT_NUMBER;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest decoded size of a single stream.
    pub max_decoded_bytes: usize,
    /// Largest cross-reference table (object slots) a document may have.
    pub max_xref_entries: usize,
    /// Most objects a single object stream may declare.
    pub max_objstm_objects: usize,
    /// Most glyphs a page may show, its annotations' and form fields'
    /// with its contents'. nupdf's addition (DECISIONS D56): what reads a
    /// page holds each glyph it shows, as MuPDF does, so a few hundred
    /// kilobytes of content that show ten million glyphs take gigabytes.
    /// A page that shows more ends there with a limit error.
    pub max_page_glyphs: usize,
    /// Most content-stream operators a page's run may take: its contents,
    /// the forms, patterns and glyph procedures they draw, and its
    /// annotations. nupdf's addition (DECISIONS D58): it bounds the time a
    /// page of paths can take, and the size of one path, as the glyph
    /// limit bounds text. A page that runs more ends there with a limit
    /// error.
    pub max_page_operators: usize,
}

/// The default [`Limits::max_page_glyphs`]: twelve times what the fullest
/// of 73,250 real pages shows.
pub const MAX_PAGE_GLYPHS: usize = 2_000_000;

/// The default [`Limits::max_page_operators`]: nearly five times what
/// the busiest of 73,250 real pages runs.
pub const MAX_PAGE_OPERATORS: usize = 5_000_000;

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_decoded_bytes: 1 << 30,
            max_xref_entries: MAX_OBJECT_NUMBER as usize + 1,
            max_objstm_objects: MAX_OBJECT_NUMBER as usize,
            max_page_glyphs: MAX_PAGE_GLYPHS,
            max_page_operators: MAX_PAGE_OPERATORS,
        }
    }
}

/// A limit was exceeded. `code` is the stable `limit.*` diagnostic code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitError {
    pub code: &'static str,
    pub message: String,
}
