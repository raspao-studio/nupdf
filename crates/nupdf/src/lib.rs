// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! nupdf: a memory-safe, parallel PDF engine rewritten in Rust from MuPDF.
//!
//! The roadmap lives in `docs/PLAN.md`; the module-to-MuPDF map in
//! `docs/ARCHITECTURE.md`.

pub mod crypt;
pub mod dct;
pub mod deflate;
pub mod diag;
mod doc_id;
pub mod document;
pub mod fax;
pub mod fax_encode;
pub mod filter;
mod garbage;
pub mod header;
pub mod jbig2;
pub mod jpeg;
pub mod lex;
pub mod limits;
mod names;
pub mod object;
pub mod page;
pub mod parse;
pub mod print;
pub mod rearrange;
pub mod repair;
pub mod slots;
pub mod stream;
pub mod strtof;
pub mod text_string;
pub mod write;
pub mod xref;

pub use diag::{Diagnostic, Diagnostics};
pub use doc_id::DocId;
pub use document::{Document, OpenError, PageCountError};
pub use limits::Limits;
pub use object::{Dict, ObjRef, Object, Stream};
pub use page::PageError;
