// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Diagnostics: what was tolerated, repaired or skipped, as data.
//!
//! These are MuPDF's `fz_warn` calls with stable codes. The message text
//! follows MuPDF's wording so the harness can line the two up.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::sync::Mutex;

thread_local! {
    /// Where this thread's diagnostics go while [`capture`] runs.
    static CAPTURE: RefCell<Option<Vec<Diagnostic>>> = const { RefCell::new(None) };
}

thread_local! {
    /// How many diagnostics this thread has reported: see [`warned`].
    static WARNED: Cell<u64> = const { Cell::new(0) };
    /// How many of this thread's next diagnostics are dropped: see
    /// [`swallowing`].
    static SWALLOW: Cell<u64> = const { Cell::new(0) };
}

/// How many diagnostics this thread has reported so far, those captured
/// or swallowed among them: the difference across a piece of work is how
/// many it reported.
pub fn warned() -> u64 {
    WARNED.with(Cell::get)
}

/// Runs `f` with the first `n` diagnostics it reports on this thread
/// dropped. For work that is done again from its start after it reported
/// `n` of them the first time, as a token is lexed again once more of its
/// stream has been read: what it reports again is not reported twice.
pub fn swallowing<R>(n: u64, f: impl FnOnce() -> R) -> R {
    let before = SWALLOW.with(|s| s.replace(n));
    let r = f();
    SWALLOW.with(|s| s.set(before));
    r
}

/// Runs `f`, keeping the diagnostics it reports on this thread instead of
/// letting them reach their sinks, so that work done on several threads
/// can report in a fixed order ([`Diagnostics::replay`]).
pub fn capture<R>(f: impl FnOnce() -> R) -> (R, Vec<Diagnostic>) {
    let outer = CAPTURE.with(|c| c.replace(Some(Vec::new())));
    let r = f();
    let got = CAPTURE.with(|c| c.replace(outer)).unwrap_or_default();
    (r, got)
}

/// Keep memory bounded on pathological files that warn per byte: every
/// diagnostic up to this many,
const MAX_DIAGNOSTICS: usize = 1000;
/// then only ones not seen before, up to this many in all.
const MAX_DISTINCT: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
}

/// A thread-safe sink. Documents own one; every subsystem reports into it.
#[derive(Debug, Default)]
pub struct Diagnostics {
    items: Mutex<Vec<Diagnostic>>,
    /// The messages stored, to tell new ones from repeats past the cap.
    seen: Mutex<HashSet<String>>,
    dropped: Mutex<usize>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn warn(&self, code: &'static str, message: impl Into<String>) {
        WARNED.with(|w| w.set(w.get().wrapping_add(1)));
        let swallowed = SWALLOW.with(|s| {
            let left = s.get();
            s.set(left.saturating_sub(1));
            left > 0
        });
        if swallowed {
            return;
        }
        let message = message.into();
        let Some(message) = CAPTURE.with(|c| match c.borrow_mut().as_mut() {
            Some(buf) => {
                buf.push(Diagnostic { code, message });
                None
            }
            None => Some(message),
        }) else {
            return;
        };
        let (Ok(mut items), Ok(mut seen)) = (self.items.lock(), self.seen.lock()) else {
            return;
        };
        let fresh = !seen.contains(&message);
        if items.len() < MAX_DIAGNOSTICS || (fresh && items.len() < MAX_DISTINCT) {
            if fresh {
                seen.insert(message.clone());
            }
            items.push(Diagnostic { code, message });
        } else if let Ok(mut dropped) = self.dropped.lock() {
            *dropped += 1;
        }
    }

    /// Reports diagnostics [`capture`] kept, in their order.
    pub fn replay(&self, diagnostics: Vec<Diagnostic>) {
        for d in diagnostics {
            self.warn(d.code, d.message);
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
    use super::{Diagnostics, MAX_DIAGNOSTICS, capture, report, swallowing, warned};

    #[test]
    fn work_done_again_does_not_report_again_what_it_reported() {
        let d = Diagnostics::new();
        let codes = |d: &Diagnostics| d.snapshot().iter().map(|x| x.code).collect::<Vec<_>>();
        // A first try reports two and is given up.
        let before = warned();
        d.warn("t.one", "one");
        d.warn("t.two", "two");
        let reported = warned() - before;
        assert_eq!(reported, 2);
        // The second try reports the same two, then a third.
        let before = warned();
        swallowing(reported, || {
            d.warn("t.one", "one");
            d.warn("t.two", "two");
            d.warn("t.three", "three");
        });
        // All three calls count, so a third try would swallow three.
        assert_eq!(warned() - before, 3);
        assert_eq!(codes(&d), ["t.one", "t.two", "t.three"]);
        // Nothing is left to swallow afterwards, even if less was reported
        // than was to be dropped; and captured diagnostics count too.
        swallowing(5, || d.warn("t.dropped", "dropped"));
        let before = warned();
        let ((), kept) = capture(|| d.warn("t.kept", "kept"));
        assert_eq!((warned() - before, kept.len()), (1, 1));
        d.warn("t.four", "four");
        assert_eq!(codes(&d), ["t.one", "t.two", "t.three", "t.four"]);
    }

    #[test]
    fn captured_diagnostics_replay_in_order() {
        let d = Diagnostics::new();
        d.warn("t.before", "before");
        let ((), got) = capture(|| {
            d.warn("t.a", "a");
            let ((), inner) = capture(|| d.warn("t.inner", "inner"));
            assert_eq!(inner.len(), 1);
            d.warn("t.b", "b");
        });
        assert_eq!(d.snapshot().len(), 1);
        let codes: Vec<_> = got.iter().map(|x| x.code).collect();
        assert_eq!(codes, ["t.a", "t.b"]);
        d.replay(got);
        let codes: Vec<_> = d.snapshot().iter().map(|x| x.code).collect();
        assert_eq!(codes, ["t.before", "t.a", "t.b"]);
    }

    #[test]
    fn past_the_cap_only_new_messages_are_kept() {
        let d = Diagnostics::new();
        for _ in 0..MAX_DIAGNOSTICS + 5 {
            d.warn("t.repeat", "same");
        }
        d.warn("t.new", "different");
        d.warn("t.repeat", "same");
        let all = d.snapshot();
        assert_eq!(all.len(), MAX_DIAGNOSTICS + 1);
        assert_eq!(all.last().map(|x| x.message.as_str()), Some("different"));
        assert_eq!(d.dropped(), 6);
    }

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
