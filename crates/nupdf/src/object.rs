// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-object.c,
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! PDF objects.
//!
//! Reals are `f32` because MuPDF stores them as `float`; matching it bit for
//! bit is what makes object dumps comparable (DECISIONS D10). Compound
//! objects sit behind `Arc`, so cloning an object is cheap and objects move
//! freely between threads.

use std::sync::Arc;

/// Largest object number MuPDF accepts (PDF_MAX_OBJECT_NUMBER).
pub const MAX_OBJECT_NUMBER: i32 = 8_388_607;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjRef {
    pub num: i32,
    pub generation: i32,
}

impl ObjRef {
    pub fn new(num: i32, generation: i32) -> Self {
        ObjRef { num, generation }
    }

    /// MuPDF parses references as 64-bit numbers and stores them in `int`.
    pub(crate) fn from_i64(num: i64, generation: i64) -> Self {
        ObjRef {
            num: num as i32,
            generation: generation as i32,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Object {
    Null,
    Bool(bool),
    Int(i64),
    Real(f32),
    Name(Arc<[u8]>),
    String(Arc<[u8]>),
    Array(Arc<Vec<Object>>),
    Dict(Arc<Dict>),
    Ref(ObjRef),
    Stream(Arc<Stream>),
}

impl Object {
    pub fn name(bytes: &[u8]) -> Object {
        Object::Name(Arc::from(bytes))
    }

    pub fn as_name(&self) -> Option<&[u8]> {
        match self {
            Object::Name(n) => Some(n),
            _ => None,
        }
    }

    pub fn is_name(&self, name: &[u8]) -> bool {
        self.as_name() == Some(name)
    }

    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Object::Dict(d) => Some(d),
            Object::Stream(s) => Some(&s.dict),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Object]> {
        match self {
            Object::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_ref(&self) -> Option<ObjRef> {
        match self {
            Object::Ref(r) => Some(*r),
            _ => None,
        }
    }

    pub fn is_number(&self) -> bool {
        matches!(self, Object::Int(_) | Object::Real(_))
    }

    /// MuPDF's `pdf_to_int` on an already resolved object: ints truncate to
    /// 32 bits, reals round half up, everything else is 0.
    pub fn to_int(&self) -> i32 {
        match self {
            Object::Int(i) => *i as i32,
            Object::Real(f) => (f + 0.5).floor() as i32,
            _ => 0,
        }
    }

    /// MuPDF's `pdf_to_int64` on an already resolved object.
    pub fn to_int64(&self) -> i64 {
        match self {
            Object::Int(i) => *i,
            Object::Real(f) => (f + 0.5).floor() as i64,
            _ => 0,
        }
    }

    /// `pdf_to_int_default`: numbers convert, anything else is `default`.
    pub fn to_int_or(&self, default: i32) -> i32 {
        if self.is_number() {
            self.to_int()
        } else {
            default
        }
    }
}

/// A dictionary that keeps insertion order. Re-inserting a key replaces its
/// value in place, as `pdf_dict_put` does, so the last duplicate wins.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dict {
    entries: Vec<(Arc<[u8]>, Object)>,
}

impl Dict {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &[u8]) -> Option<&Object> {
        self.entries
            .iter()
            .find(|(k, _)| k.as_ref() == key)
            .map(|(_, v)| v)
    }

    pub fn insert(&mut self, key: Arc<[u8]>, value: Object) {
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.entries.push((key, value)),
        }
    }

    pub fn put(&mut self, key: &[u8], value: Object) {
        self.insert(Arc::from(key), value);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &Object)> {
        self.entries.iter().map(|(k, v)| (k.as_ref(), v))
    }
}

/// A stream object: its dictionary and where its data starts in the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    pub dict: Dict,
    /// Absolute file offset of the first data byte.
    pub data_start: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dict_replaces_duplicates_in_place() {
        let mut d = Dict::new();
        d.put(b"A", Object::Int(1));
        d.put(b"B", Object::Int(2));
        d.put(b"A", Object::Int(3));
        assert_eq!(d.len(), 2);
        assert_eq!(d.get(b"A"), Some(&Object::Int(3)));
        let keys: Vec<&[u8]> = d.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![b"A".as_slice(), b"B".as_slice()]);
    }

    #[test]
    fn int_conversions_match_mupdf() {
        assert_eq!(Object::Real(2.5).to_int(), 3);
        assert_eq!(Object::Real(-2.5).to_int(), -2);
        assert_eq!(Object::Int(1 << 32).to_int(), 0);
        assert_eq!(Object::Null.to_int(), 0);
        assert_eq!(Object::Null.to_int_or(7), 7);
        assert_eq!(Object::Real(1.4).to_int_or(7), 1);
    }
}
