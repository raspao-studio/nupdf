// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! nupdf: a memory-safe, parallel PDF engine rewritten in Rust from MuPDF.
//!
//! Milestone M0 provides only header detection and document identity. The
//! roadmap lives in `docs/PLAN.md`.

mod doc_id;
pub mod header;

pub use doc_id::DocId;
