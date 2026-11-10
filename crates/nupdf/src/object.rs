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

/// A dictionary in MuPDF's order: insertion order, until a put into one of
/// more than 100 entries sorts it by key (`pdf_dict_get_put`), after which
/// new keys go into their sorted place. Re-inserting a key replaces its value
/// in place, so the last duplicate wins. The order is what gets written.
///
/// Lookups follow MuPDF's two search paths. A name MuPDF interns as a
/// constant goes through `pdf_dict_find`, which in a sorted dictionary
/// reports its last key as absent when that key is the one sought; `get`
/// then misses it and `insert` appends a duplicate. Other names, and
/// `remove`, go through `pdf_dict_finds`, which has no such case.
#[derive(Debug, Clone, Default)]
pub struct Dict {
    entries: Vec<(Arc<[u8]>, Object)>,
    sorted: bool,
}

impl PartialEq for Dict {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Dict {
    pub fn new() -> Self {
        Self::default()
    }

    /// `pdf_dict_find` for interned names, else `pdf_dict_finds`.
    fn find(&self, key: &[u8]) -> Result<usize, usize> {
        let len = self.entries.len();
        if self.sorted
            && self.entries.last().is_some_and(|(k, _)| k.as_ref() == key)
            && crate::names::is_known(key)
        {
            return Err(len);
        }
        self.finds(key)
    }

    /// `pdf_dict_finds`: the key's index, or where it would go.
    fn finds(&self, key: &[u8]) -> Result<usize, usize> {
        let Some((last, _)) = self.entries.last() else {
            return Err(0);
        };
        if !self.sorted {
            return self
                .entries
                .iter()
                .position(|(k, _)| k.as_ref() == key)
                .ok_or(self.entries.len());
        }
        if last.as_ref() < key {
            return Err(self.entries.len());
        }
        // MuPDF's own bisection, so that duplicates resolve as it does.
        let (mut l, mut r) = (0, self.entries.len());
        while l < r {
            let m = (l + r - 1) / 2;
            match key.cmp(&self.entries[m].0) {
                std::cmp::Ordering::Less => r = m,
                std::cmp::Ordering::Greater => l = m + 1,
                std::cmp::Ordering::Equal => return Ok(m),
            }
        }
        Err(l)
    }

    pub fn get(&self, key: &[u8]) -> Option<&Object> {
        self.find(key).ok().map(|i| &self.entries[i].1)
    }

    pub fn insert(&mut self, key: Arc<[u8]>, value: Object) {
        if self.entries.len() > 100 && !self.sorted {
            self.entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            self.sorted = true;
        }
        match self.find(&key) {
            Ok(i) => self.entries[i].1 = value,
            Err(i) => self.entries.insert(i, (key, value)),
        }
    }

    /// `pdf_dict_dels`: the last entry takes the removed one's place.
    pub fn remove(&mut self, key: &[u8]) {
        if let Ok(i) = self.finds(key) {
            self.entries.swap_remove(i);
            self.sorted = false;
        }
    }

    /// A copy without `key`, the other entries in their order. Unlike
    /// `remove`, this is not a MuPDF operation; it is for comparisons.
    pub fn without(&self, key: &[u8]) -> Dict {
        Dict {
            entries: self
                .entries
                .iter()
                .filter(|(k, _)| k.as_ref() != key)
                .cloned()
                .collect(),
            sorted: self.sorted,
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

    /// Entry `i` (`pdf_dict_get_key` and `pdf_dict_get_val`).
    pub(crate) fn entry(&self, i: usize) -> Option<(&Arc<[u8]>, &Object)> {
        self.entries.get(i).map(|(k, v)| (k, v))
    }

    /// The value of entry `i`, to change in place (`pdf_dict_put_val_null`
    /// and its kin, which go by index rather than by key).
    pub(crate) fn value_mut(&mut self, i: usize) -> Option<&mut Object> {
        self.entries.get_mut(i).map(|(_, v)| v)
    }

    /// Whether a put has sorted it (`PDF_FLAGS_SORTED`).
    pub(crate) fn is_sorted(&self) -> bool {
        self.sorted
    }

    /// Replaces every value with `f(key, value)`, keeping the order.
    pub fn map_values(&mut self, mut f: impl FnMut(&[u8], Object) -> Object) {
        for (k, v) in &mut self.entries {
            let value = std::mem::replace(v, Object::Null);
            *v = f(k, value);
        }
    }
}

/// A stream object: its dictionary and where its data starts in the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    pub dict: Dict,
    /// Absolute file offset of the first data byte.
    pub data_start: usize,
    /// The object number it was loaded as. Its xref entry seeds the
    /// decryption key, as in MuPDF's `pdf_open_raw_filter`.
    pub num: i32,
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

    fn keys(d: &Dict) -> Vec<String> {
        d.iter()
            .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
            .collect()
    }

    /// 102 entries put in reverse, so the last put sorts the dictionary.
    fn big(extra: &[&[u8]]) -> Dict {
        let mut d = Dict::new();
        for key in extra {
            d.put(key, Object::Bool(true));
        }
        for i in (0..102 - extra.len()).rev() {
            d.put(format!("K{i:03}").as_bytes(), Object::Int(i as i64));
        }
        d
    }

    #[test]
    fn dict_sorts_once_it_outgrows_100_entries() {
        let mut d = Dict::new();
        for i in (0..101).rev() {
            d.put(format!("K{i:03}").as_bytes(), Object::Int(i));
        }
        assert_eq!(keys(&d)[0], "K100", "101 entries stay in insertion order");
        d.put(b"A", Object::Null);
        let k = keys(&d);
        assert_eq!(&k[..3], ["A", "K000", "K001"]);
        assert_eq!(k.last().map(String::as_str), Some("K100"));
        assert_eq!(d.get(b"K050"), Some(&Object::Int(50)));
        d.put(b"K050", Object::Int(-1));
        assert_eq!(d.len(), 102);
        assert_eq!(d.get(b"K050"), Some(&Object::Int(-1)));
    }

    #[test]
    fn dict_remove_moves_the_last_entry_and_unsorts() {
        let mut d = big(&[]);
        d.remove(b"K000");
        let k = keys(&d);
        assert_eq!(k[0], "K101");
        assert_eq!(d.len(), 101);
        d.put(b"K000", Object::Int(0));
        assert_eq!(keys(&d)[0], "K000", "the next put sorts it again");
    }

    #[test]
    fn sorted_dict_hides_an_interned_last_key() {
        // pdf_dict_find returns "not found" when the last key of a sorted
        // dictionary is the interned name sought.
        let mut d = big(&[b"Type", b"Subtype"]);
        assert_eq!(keys(&d)[100..], ["Subtype", "Type"]);
        assert_eq!(d.get(b"Type"), None);
        assert_eq!(d.get(b"Subtype"), Some(&Object::Bool(true)));
        d.put(b"Type", Object::Int(1));
        assert_eq!(keys(&d)[100..], ["Subtype", "Type", "Type"]);
        d.remove(b"Type");
        assert_eq!(d.len(), 102, "pdf_dict_dels finds it by string");
        assert!(d.get(b"Type").is_some(), "unsorted again, so found");
    }

    #[test]
    fn sorted_dict_finds_an_uninterned_last_key() {
        let mut d = big(&[b"Zz"]);
        assert_eq!(keys(&d).last().map(String::as_str), Some("Zz"));
        assert_eq!(d.get(b"Zz"), Some(&Object::Bool(true)));
        d.put(b"Zz", Object::Int(1));
        assert_eq!(d.len(), 102);
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
