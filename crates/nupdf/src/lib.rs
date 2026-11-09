// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! nupdf: a memory-safe, parallel PDF engine rewritten in Rust from MuPDF.
//!
//! The roadmap lives in `docs/PLAN.md`; the module-to-MuPDF map in
//! `docs/ARCHITECTURE.md`.

pub mod diag;
mod doc_id;
pub mod document;
pub mod filter;
pub mod header;
pub mod lex;
pub mod limits;
pub mod object;
pub mod page;
pub mod parse;
pub mod repair;
pub mod slots;
pub mod strtof;
pub mod text_string;
pub mod xref;

pub use diag::{Diagnostic, Diagnostics};
pub use doc_id::DocId;
pub use document::{Document, OpenError, PageCountError};
pub use limits::Limits;
pub use object::{Dict, ObjRef, Object, Stream};
pub use page::PageError;
