// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Stopping a call partway: a flag another thread sets, or a deadline.
//!
//! The work of a call checks [`interrupted`] where it runs long (each
//! content-stream operator, each page), and the thread running it, with
//! the threads it starts, carries the call's [`Interrupt`]. Interrupted
//! work does not stop cleanly: a content stream breaks off as a damaged one
//! would. So whoever set the interrupt discards what the call made and
//! reports why instead.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

/// Why a call was stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Cancelled,
    TimedOut,
}

/// A call's interrupt: cancelled from outside, or past its deadline.
#[derive(Debug)]
pub struct Interrupt {
    /// 0 while running, else the [`Reason`] it was stopped for, plus one.
    state: AtomicU8,
    deadline: Option<Instant>,
}

impl Interrupt {
    pub fn new(deadline: Option<Instant>) -> Arc<Interrupt> {
        Arc::new(Interrupt {
            state: AtomicU8::new(0),
            deadline,
        })
    }

    /// Stops the call, unless it was stopped already.
    pub fn cancel(&self) {
        let _ = self.state.compare_exchange(
            0,
            Reason::Cancelled as u8 + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// Why the call was stopped, if it was; `now` passes the deadline.
    fn check(&self, now: bool) -> Option<Reason> {
        if now
            && self.state.load(Ordering::Relaxed) == 0
            && self.deadline.is_some_and(|d| Instant::now() >= d)
        {
            let _ = self.state.compare_exchange(
                0,
                Reason::TimedOut as u8 + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        match self.state.load(Ordering::Relaxed) {
            1 => Some(Reason::Cancelled),
            2 => Some(Reason::TimedOut),
            _ => None,
        }
    }

    /// Why the call was stopped, looking at the clock now.
    pub fn reason(&self) -> Option<Reason> {
        self.check(true)
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Interrupt>>> = const { RefCell::new(None) };
    /// Checks since the clock was last read.
    static TICKS: Cell<u32> = const { Cell::new(0) };
}

/// How many checks go by between readings of the clock.
const CLOCK_EVERY: u32 = 64;

/// The interrupt this thread works under, to hand to the threads it starts.
pub fn current() -> Option<Arc<Interrupt>> {
    CURRENT.with(|c| c.borrow().clone())
}

/// `f`, with this thread working under `interrupt`.
pub fn within<R>(interrupt: Option<Arc<Interrupt>>, f: impl FnOnce() -> R) -> R {
    let before = CURRENT.with(|c| c.replace(interrupt));
    let out = f();
    CURRENT.with(|c| *c.borrow_mut() = before);
    out
}

/// Whether the work on this thread should stop.
pub fn interrupted() -> bool {
    CURRENT.with(|c| {
        let c = c.borrow();
        let Some(interrupt) = c.as_ref() else {
            return false;
        };
        let ticks = TICKS.with(|t| {
            let n = t.get().wrapping_add(1);
            t.set(n);
            n
        });
        interrupt.check(ticks.is_multiple_of(CLOCK_EVERY)).is_some()
    })
}

/// The error a stopped piece of work returns.
pub const MESSAGE: &str = "interrupted";

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn nothing_is_interrupted_outside_a_call() {
        assert!(!interrupted());
        assert!(current().is_none());
    }

    #[test]
    fn a_cancelled_call_stays_cancelled_and_threads_can_carry_it() {
        let i = Interrupt::new(None);
        within(Some(i.clone()), || {
            assert!(!interrupted());
            i.cancel();
            assert!(interrupted());
            let carried = current();
            let seen = std::thread::spawn(move || within(carried, interrupted))
                .join()
                .unwrap();
            assert!(seen);
        });
        assert!(!interrupted());
        assert_eq!(i.reason(), Some(Reason::Cancelled));
    }

    #[test]
    fn a_deadline_is_seen_within_a_few_checks() {
        let i = Interrupt::new(Some(Instant::now()));
        std::thread::sleep(Duration::from_millis(1));
        within(Some(i.clone()), || {
            assert!((0..=CLOCK_EVERY).any(|_| interrupted()));
        });
        // A deadline that passed is not taken back by a cancel.
        i.cancel();
        assert_eq!(i.reason(), Some(Reason::TimedOut));
    }
}
