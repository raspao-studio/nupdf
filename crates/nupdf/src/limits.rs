// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Resource limits for untrusted input.
//!
//! The defaults are MuPDF's own caps where it has them, so default behavior
//! matches MuPDF; the decompression cap is nupdf's addition. Structural
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
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_decoded_bytes: 1 << 30,
            max_xref_entries: MAX_OBJECT_NUMBER as usize + 1,
            max_objstm_objects: MAX_OBJECT_NUMBER as usize,
        }
    }
}

/// A limit was exceeded. `code` is the stable `limit.*` diagnostic code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitError {
    pub code: &'static str,
    pub message: String,
}
