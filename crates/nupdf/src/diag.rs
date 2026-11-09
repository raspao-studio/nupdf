// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Diagnostics: what was tolerated, repaired or skipped, as data.
//!
//! These are MuPDF's `fz_warn` calls with stable codes. The message text
//! follows MuPDF's wording so the harness can line the two up.

use std::sync::Mutex;

/// Keep memory bounded on pathological files that warn per byte.
const MAX_DIAGNOSTICS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
}

/// A thread-safe sink. Documents own one; every subsystem reports into it.
#[derive(Debug, Default)]
pub struct Diagnostics {
    items: Mutex<Vec<Diagnostic>>,
    dropped: Mutex<usize>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn warn(&self, code: &'static str, message: impl Into<String>) {
        let Ok(mut items) = self.items.lock() else {
            return;
        };
        if items.len() < MAX_DIAGNOSTICS {
            items.push(Diagnostic {
                code,
                message: message.into(),
            });
        } else if let Ok(mut dropped) = self.dropped.lock() {
            *dropped += 1;
        }
    }

    pub fn snapshot(&self) -> Vec<Diagnostic> {
        self.items.lock().map(|v| v.clone()).unwrap_or_default()
    }

    pub fn has(&self, code: &str) -> bool {
        self.items
            .lock()
            .map(|v| v.iter().any(|d| d.code == code))
            .unwrap_or(false)
    }

    /// Diagnostics beyond the cap that were counted but not stored.
    pub fn dropped(&self) -> usize {
        self.dropped.lock().map(|d| *d).unwrap_or(0)
    }
}
