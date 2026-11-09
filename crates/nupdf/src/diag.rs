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

/// How MuPDF prints a caught exception: `<kind> error: <message>`. nupdf
/// keeps the same lines so diagnostics can be compared with MuPDF's.
pub mod report {
    const KINDS: [&str; 8] = [
        "format",
        "syntax",
        "generic",
        "library",
        "limit",
        "argument",
        "system",
        "unsupported",
    ];

    pub fn syntax(message: &str) -> String {
        format!("syntax error: {message}")
    }

    pub fn format(message: &str) -> String {
        format!("format error: {message}")
    }

    /// `message` as a report line, defaulting to a format error.
    pub fn line(message: &str) -> String {
        if has_kind(message) {
            message.to_string()
        } else {
            format(message)
        }
    }

    pub fn has_kind(message: &str) -> bool {
        KINDS.iter().any(|k| {
            message
                .strip_prefix(k)
                .is_some_and(|rest| rest.starts_with(" error: "))
        })
    }

    /// The message without its `<kind> error: ` prefix.
    pub fn strip(message: &str) -> &str {
        if has_kind(message) {
            message.split_once(" error: ").map_or(message, |(_, m)| m)
        } else {
            message
        }
    }
}

#[cfg(test)]
mod tests {
    use super::report;

    #[test]
    fn report_lines_carry_mupdf_error_kinds() {
        assert_eq!(
            report::syntax("invalid key in dict"),
            "syntax error: invalid key in dict"
        );
        assert_eq!(
            report::line("cannot find startxref"),
            "format error: cannot find startxref"
        );
        assert_eq!(
            report::line("library error: zlib error: x"),
            "library error: zlib error: x"
        );
        assert_eq!(
            report::strip("syntax error: invalid key in dict"),
            "invalid key in dict"
        );
        assert_eq!(report::strip("plain message"), "plain message");
    }
}
