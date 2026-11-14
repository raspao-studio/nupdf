// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! A JSON array written while it is made, so that a report with one entry
//! per operator of a page never holds the page's entries at once: a
//! hostile page of ten million operators is 100 KB on disk, and its report
//! as a tree of values was 5 GB.

use std::cell::RefCell;

use serde::Serialize;
use serde::ser::{SerializeMap, SerializeSeq, Serializer};
use serde_json::Value;

/// A report's own entries, written into the report while they are made.
pub trait Body {
    fn serialize_into<M: SerializeMap>(&self, map: &mut M) -> Result<(), M::Error>;
}

/// What takes an array's rows: each is written, and dropped, before the
/// next is made.
pub trait Sink {
    fn push<T: Serialize>(&mut self, row: T);
}

/// What makes an array's rows.
pub trait Fill {
    fn fill<K: Sink>(self, sink: &mut K);
}

/// Rows made as values, by a function.
impl<F: FnOnce(&mut dyn FnMut(Value))> Fill for F {
    fn fill<K: Sink>(self, sink: &mut K) {
        self(&mut |row| sink.push(row));
    }
}

/// An array whose rows `fill` hands over one at a time. It can be written
/// once: a second time it is empty.
pub struct Rows<F>(RefCell<Option<F>>);

impl<F: Fill> Rows<F> {
    pub fn new(fill: F) -> Self {
        Rows(RefCell::new(Some(fill)))
    }
}

/// The rows going into an array being written, and the write that failed.
struct Writing<Q: SerializeSeq> {
    seq: Q,
    failed: Option<Q::Error>,
}

impl<Q: SerializeSeq> Sink for Writing<Q> {
    fn push<T: Serialize>(&mut self, row: T) {
        if self.failed.is_some() {
            return;
        }
        if let Err(e) = self.seq.serialize_element(&row) {
            self.failed = Some(e);
            // Nobody is reading: what makes the rows stops at its next
            // operator, as an interrupted call does.
            nupdf::interrupt::stop();
        }
    }
}

impl<F: Fill> Serialize for Rows<F> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut to = Writing {
            seq: s.serialize_seq(None)?,
            failed: None,
        };
        if let Some(fill) = self.0.borrow_mut().take() {
            fill.fill(&mut to);
        }
        match to.failed {
            Some(e) => {
                let _ = nupdf::interrupt::stopped();
                Err(e)
            }
            None => to.seq.end(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::rc::Rc;

    /// A writer whose bytes can be looked at while it is written to.
    #[derive(Clone, Default)]
    struct Shared(Rc<RefCell<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn rows_are_written_as_they_are_made() {
        let out = Shared::default();
        let seen = out.clone();
        let rows = Rows::new(|push: &mut dyn FnMut(Value)| {
            push(json!(["Tj", "a"]));
            assert_eq!(seen.0.borrow().as_slice(), br#"{"ops":[["Tj","a"]"#);
            push(json!(1));
        });
        #[derive(Serialize)]
        struct Page<T> {
            ops: T,
        }
        serde_json::to_writer(out.clone(), &Page { ops: rows }).unwrap();
        assert_eq!(out.0.borrow().as_slice(), br#"{"ops":[["Tj","a"],1]}"#);
    }

    #[test]
    fn rows_read_as_the_array_of_their_values() {
        let rows = Rows::new(|push: &mut dyn FnMut(Value)| {
            for i in 0..3 {
                push(json!([i, "x"]));
            }
        });
        let value = serde_json::to_value(&rows).unwrap();
        assert_eq!(value, json!([[0, "x"], [1, "x"], [2, "x"]]));
        // Written once: there is nothing to make them from again.
        assert_eq!(serde_json::to_value(&rows).unwrap(), json!([]));
        let none = Rows::new(|_: &mut dyn FnMut(Value)| {});
        assert_eq!(serde_json::to_string(&none).unwrap(), "[]");
    }

    /// A writer that takes `0` bytes and then fails, as a closed pipe does.
    struct Closing(usize);

    impl Write for Closing {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if data.len() > self.0 {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.0 -= data.len();
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failed_write_stops_what_makes_the_rows() {
        let made = std::cell::Cell::new(0);
        let rows = Rows::new(|push: &mut dyn FnMut(Value)| {
            // As the interpreter does at each operator.
            while !nupdf::interrupt::interrupted() && made.get() < 1000 {
                made.set(made.get() + 1);
                push(json!("row"));
            }
        });
        assert!(serde_json::to_writer(Closing(20), &rows).is_err());
        assert!(
            made.get() < 10,
            "{} rows made for a closed pipe",
            made.get()
        );
        // The stop does not outlive the rows it was for.
        assert!(!nupdf::interrupt::interrupted());
    }
}
