// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-write.c and source/pdf/pdf-object.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Garbage collection on save (`do_garbage`), as `pdf_save_document` does
//! it on its in-memory copy of the objects. Level 1 keeps only what the
//! trailer reaches, turning references to missing objects into null and
//! indirect stream lengths into numbers. Level 2 also renumbers the objects
//! from 1 with generation 0. Level 3 also merges duplicate objects (never
//! pages, nor streams), and level 4 merges streams whose data is the same.
//! Merging repeats until nothing changes, since renumbering can make more
//! objects equal.
//!
//! The quirks that show in the output are kept: duplicates are found by a
//! CRC-32 of the object in MuPDF's memory layout, so dictionaries that are
//! equal in a different order are not merged; renumbering puts each changed
//! value back by key, which sorts a dictionary of 101 entries and, in a
//! sorted dictionary, appends a duplicate of an interned last key.

use std::collections::HashMap;
use std::sync::Arc;

use crate::document::Document;
use crate::object::{Dict, ObjRef, Object, Stream};
use crate::write::WriteError;

/// An xref slot of the writer's working copy.
#[derive(Debug, Clone, Default)]
pub(crate) enum Slot {
    /// Free, or nothing to load.
    #[default]
    Empty,
    /// Failed to load; the number is the input's. MuPDF tries again, with a
    /// warning, wherever it resolves a reference to it.
    Failed(i32),
    /// The object, and for a stream the stream as loaded, whose dictionary
    /// still locates its data in the input.
    Loaded(Object, Option<Arc<Stream>>),
}

impl Slot {
    pub(crate) fn object(&self) -> Option<&Object> {
        match self {
            Slot::Loaded(o, _) => Some(o),
            _ => None,
        }
    }

    pub(crate) fn data(&self) -> Option<&Stream> {
        match self {
            Slot::Loaded(_, s) => s.as_deref(),
            _ => None,
        }
    }
}

/// The working copy and `use_list` as garbage collection leaves them, and
/// the input's number of each slot, whose xref entry renumbering moves.
pub(crate) struct Collected {
    pub slots: Vec<Slot>,
    pub use_list: Vec<bool>,
    pub origin: Vec<i32>,
}

/// Runs `do_garbage` at `level` (1 to 4) over `slots`, the document's
/// objects by number, and `trailer`.
pub(crate) fn collect(
    doc: &Document,
    slots: Vec<Slot>,
    trailer: &mut Dict,
    level: u8,
) -> Result<Collected, WriteError> {
    let len = slots.len();
    let mut c = Collector {
        doc,
        slots,
        use_list: vec![false; len],
        renumber: (0..len as i32).collect(),
        origin: (0..len as i32).collect(),
    };
    loop {
        c.bake_stream_lengths();
        c.mark(trailer);
        let changed = level >= 3 && c.dedup(level >= 4)?;
        if level >= 2 {
            c.compact();
            c.renumber(trailer);
        }
        if !changed {
            break;
        }
    }
    Ok(Collected {
        slots: c.slots,
        use_list: c.use_list,
        origin: c.origin,
    })
}

struct Collector<'a> {
    doc: &'a Document,
    slots: Vec<Slot>,
    use_list: Vec<bool>,
    renumber: Vec<i32>,
    origin: Vec<i32>,
}

/// What marking a reference found (`markref`).
enum Visit {
    /// Nothing there: the reference becomes null.
    Duff,
    /// Marked before, or not a container.
    Done,
    /// Newly marked: this object's contents are next.
    Into(usize),
}

impl Collector<'_> {
    /// `pdf_resolve_indirect` on the working copy.
    fn resolve(&mut self, num: i32) -> Option<Object> {
        let slot = usize::try_from(num).ok().filter(|&n| n > 0)?;
        match self.slots.get(slot)? {
            Slot::Empty => None,
            Slot::Loaded(o, _) => Some(o.clone()),
            Slot::Failed(original) => {
                let o = self.doc.object(*original)?;
                let data = match &o {
                    Object::Stream(s) => Some(Arc::clone(s)),
                    _ => None,
                };
                self.slots[slot] = Slot::Loaded(o.clone(), data);
                Some(o)
            }
        }
    }

    /// `pdf_resolve_indirect_chain`, as RESOLVE does it.
    fn resolve_chain(&mut self, obj: &Object) -> Option<Object> {
        let mut obj = obj.clone();
        let mut sanity = 10;
        while let Object::Ref(r) = obj {
            sanity -= 1;
            if sanity == 0 {
                self.doc.diagnostics().warn(
                    "object.indirection_cycle",
                    format!(
                        "too many indirections (possible indirection cycle involving {} 0 R)",
                        r.num
                    ),
                );
                return None;
            }
            obj = self.resolve(r.num)?;
        }
        Some(obj)
    }

    /// `bake_stream_length`: an indirect /Length becomes its number, so the
    /// object holding it is no longer reached.
    fn bake_stream_lengths(&mut self) {
        for num in 0..self.slots.len() {
            let Slot::Loaded(Object::Stream(s), _) = &self.slots[num] else {
                continue;
            };
            let Some(length @ Object::Ref(_)) = s.dict.get(b"Length").cloned() else {
                continue;
            };
            let value = self.resolve_chain(&length).map_or(0, |o| o.to_int());
            if let Slot::Loaded(Object::Stream(s), _) = &mut self.slots[num] {
                Arc::make_mut(s)
                    .dict
                    .put(b"Length", Object::Int(i64::from(value)));
            }
        }
    }

    /// `markref`, following a chain of references to references. An object
    /// is duff when it reads as null, and `pdf_is_null` reads a reference
    /// to the end of its chain.
    fn visit(&mut self, mut num: i32) -> Visit {
        loop {
            let Some(n) = usize::try_from(num)
                .ok()
                .filter(|&n| n > 0 && n < self.slots.len())
            else {
                return Visit::Duff;
            };
            if self.use_list[n] {
                return Visit::Done;
            }
            self.use_list[n] = true;
            let obj = self.resolve(num);
            let null = match &obj {
                None | Some(Object::Null) => true,
                Some(r @ Object::Ref(_)) => {
                    let end = self.resolve_chain(r);
                    matches!(end, None | Some(Object::Null))
                }
                Some(_) => false,
            };
            if null {
                self.use_list[n] = false;
                return Visit::Duff;
            }
            match obj {
                Some(Object::Ref(r)) => num = r.num,
                Some(Object::Dict(_) | Object::Array(_) | Object::Stream(_)) => {
                    return Visit::Into(n);
                }
                _ => return Visit::Done,
            }
        }
    }

    /// `markobj` from the trailer: marks everything it reaches and makes
    /// references to nothing null where they appear. The walk is MuPDF's
    /// depth-first order, on its own stack rather than by recursion.
    fn mark(&mut self, trailer: &mut Dict) {
        // Owner None is the trailer.
        let mut duff: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
        let mut first = Vec::new();
        for (_, v) in trailer.iter() {
            refs_in(v, &mut first);
        }
        let mut stack: Vec<(Option<usize>, Vec<i32>, usize)> = vec![(None, first, 0)];
        while let Some(top) = stack.last_mut() {
            let Some(&num) = top.1.get(top.2) else {
                stack.pop();
                continue;
            };
            let (owner, at) = (top.0, top.2);
            top.2 += 1;
            match self.visit(num) {
                Visit::Duff => duff.entry(owner).or_default().push(at),
                Visit::Done => {}
                Visit::Into(n) => {
                    let mut refs = Vec::new();
                    if let Some(o) = self.slots[n].object() {
                        refs_in(o, &mut refs);
                    }
                    stack.push((Some(n), refs, 0));
                }
            }
        }
        for (owner, at) in duff {
            let mut nth = 0;
            let mut next = at.iter().copied().peekable();
            match owner {
                None => {
                    for i in 0..trailer.len() {
                        if let Some(v) = trailer.value_mut(i) {
                            null_refs(v, &mut nth, &mut next);
                        }
                    }
                }
                Some(n) => {
                    if let Slot::Loaded(o, _) = &mut self.slots[n] {
                        null_refs_inside(o, &mut nth, &mut next);
                    }
                }
            }
        }
    }

    /// `removeduplicateobjs`: maps each object equal to an earlier one onto
    /// it. Candidates are kept by hash, in number order, so the earliest
    /// equal object wins as in MuPDF's scan.
    fn dedup(&mut self, check_streams: bool) -> Result<bool, WriteError> {
        let mut by_crc: HashMap<u32, Vec<usize>> = HashMap::new();
        let mut changed = false;
        for num in 1..self.slots.len() {
            if !self.use_list[num] {
                continue;
            }
            let a = self.slots[num].object().cloned().unwrap_or(Object::Null);
            let crc = self.hash(num, &a, check_streams)?;
            // Never common up pages.
            let ty = a.as_dict().and_then(|d| d.get(b"Type")).cloned();
            if ty
                .and_then(|t| self.resolve_chain(&t))
                .is_some_and(|t| t.is_name(b"Page"))
            {
                by_crc.entry(crc).or_default().push(num);
                continue;
            }
            let mut found = None;
            for &other in by_crc.get(&crc).map_or(&[][..], Vec::as_slice) {
                if self.use_list[other] && !self.differ_top(num, other, check_streams)? {
                    found = Some(other);
                    break;
                }
            }
            match found {
                Some(other) => {
                    self.renumber[num] = other as i32;
                    self.renumber[other] = other as i32;
                    self.use_list[num] = false;
                    changed = true;
                }
                None => by_crc.entry(crc).or_default().push(num),
            }
        }
        Ok(changed)
    }

    /// `pdf_hash_obj`.
    fn hash(&self, num: usize, obj: &Object, check_streams: bool) -> Result<u32, WriteError> {
        let mut crc = flate2::Crc::new();
        hash_obj(obj, &mut crc);
        if check_streams && let Some(s) = self.slots[num].data() {
            crc.update(&self.raw(s)?);
        }
        Ok(crc.sum())
    }

    fn raw(&self, s: &Stream) -> Result<Vec<u8>, WriteError> {
        self.doc
            .raw_stream_data(s)
            .map(|d| d.into_owned())
            .map_err(|message| WriteError { message })
    }

    /// `do_objcmp` on two whole objects: the stream data decides only once
    /// the dictionaries are equal.
    fn differ_top(&self, a: usize, b: usize, check_streams: bool) -> Result<bool, WriteError> {
        let (Some(oa), Some(ob)) = (self.slots[a].object(), self.slots[b].object()) else {
            return Ok(true);
        };
        let (Some(da), Some(db)) = (oa.as_dict(), ob.as_dict()) else {
            return Ok(differ(oa, ob));
        };
        if dict_differ(da, db) {
            return Ok(true);
        }
        match (self.slots[a].data(), self.slots[b].data()) {
            (None, None) => Ok(false),
            (Some(sa), Some(sb)) if check_streams => Ok(self.raw(sa)? != self.raw(sb)?),
            _ => Ok(true),
        }
    }

    /// `compactxref`: used objects numbered from 1, merged ones following
    /// the object they merged into.
    fn compact(&mut self) {
        let mut next = 1;
        for num in 1..self.slots.len() {
            let to = self.renumber[num] as usize;
            if !self.use_list[to] {
                self.renumber[num] = 0;
            } else if to == num {
                self.renumber[num] = next;
                next += 1;
            } else {
                self.renumber[num] = self.renumber[to];
            }
        }
    }

    /// `renumberobjs`: rewrites every reference and moves the used objects
    /// to their new numbers.
    fn renumber(&mut self, trailer: &mut Dict) {
        let map = std::mem::take(&mut self.renumber);
        renumber_dict(trailer, &map);
        for (num, slot) in self.slots.iter_mut().enumerate() {
            if map[num] == 0 || !self.use_list[num] {
                continue;
            }
            if let Slot::Loaded(o, _) = slot {
                match o {
                    Object::Ref(r) => *o = new_ref(r, &map),
                    _ => renumber_obj(o, &map),
                }
            }
        }
        let mut len = 1;
        let mut slots = vec![Slot::Empty; self.slots.len()];
        let mut use_list = vec![false; self.slots.len()];
        let mut origin = vec![0; self.slots.len()];
        for (num, &to) in map.iter().enumerate().skip(1) {
            if self.use_list[num] {
                let to = to as usize;
                len = len.max(to + 1);
                slots[to] = std::mem::take(&mut self.slots[num]);
                use_list[to] = true;
                origin[to] = self.origin[num];
            }
        }
        slots.truncate(len);
        use_list.truncate(len);
        origin.truncate(len);
        self.slots = slots;
        self.use_list = use_list;
        self.origin = origin;
        self.renumber = (0..len as i32).collect();
    }
}

/// The references in `obj`, in the order `markobj` meets them.
fn refs_in(obj: &Object, out: &mut Vec<i32>) {
    match obj {
        Object::Ref(r) => out.push(r.num),
        Object::Array(a) => a.iter().for_each(|v| refs_in(v, out)),
        Object::Dict(d) => d.iter().for_each(|(_, v)| refs_in(v, out)),
        Object::Stream(s) => s.dict.iter().for_each(|(_, v)| refs_in(v, out)),
        _ => {}
    }
}

/// Makes null the references in `obj` whose positions in [`refs_in`] order
/// come next in `at`. `nth` counts the references passed.
fn null_refs(
    obj: &mut Object,
    nth: &mut usize,
    at: &mut std::iter::Peekable<impl Iterator<Item = usize>>,
) {
    if at.peek().is_none() {
        return;
    }
    match obj {
        Object::Ref(_) => {
            if at.peek() == Some(nth) {
                at.next();
                *obj = Object::Null;
            }
            *nth += 1;
        }
        _ => null_refs_inside(obj, nth, at),
    }
}

fn null_refs_inside(
    obj: &mut Object,
    nth: &mut usize,
    at: &mut std::iter::Peekable<impl Iterator<Item = usize>>,
) {
    let dict = match obj {
        Object::Array(a) => {
            for v in Arc::make_mut(a).iter_mut() {
                null_refs(v, nth, at);
            }
            return;
        }
        Object::Dict(d) => Arc::make_mut(d),
        Object::Stream(s) => &mut Arc::make_mut(s).dict,
        _ => return,
    };
    for i in 0..dict.len() {
        if let Some(v) = dict.value_mut(i) {
            null_refs(v, nth, at);
        }
    }
}

fn has_refs(obj: &Object) -> bool {
    match obj {
        Object::Ref(_) => true,
        Object::Array(a) => a.iter().any(has_refs),
        Object::Dict(d) => d.iter().any(|(_, v)| has_refs(v)),
        Object::Stream(s) => s.dict.iter().any(|(_, v)| has_refs(v)),
        _ => false,
    }
}

/// A reference under the new numbering, or null for an object dropped.
fn new_ref(r: &ObjRef, map: &[i32]) -> Object {
    match usize::try_from(r.num).ok().and_then(|n| map.get(n)) {
        Some(&to) if r.num > 0 && to != 0 => Object::Ref(ObjRef::new(to, 0)),
        _ => Object::Null,
    }
}

/// `renumberobj` on a value, in place.
fn renumber_obj(obj: &mut Object, map: &[i32]) {
    if !has_refs(obj) {
        return;
    }
    match obj {
        Object::Dict(d) => renumber_dict(Arc::make_mut(d), map),
        Object::Stream(s) => renumber_dict(&mut Arc::make_mut(s).dict, map),
        Object::Array(a) => {
            for v in Arc::make_mut(a).iter_mut() {
                match v {
                    Object::Ref(r) => *v = new_ref(r, map),
                    _ => renumber_obj(v, map),
                }
            }
        }
        _ => {}
    }
}

/// `renumberobj` on a dictionary. A changed reference goes back with
/// `pdf_dict_put`, by key, so the dictionary can sort itself or gain a
/// duplicate key under MuPDF's lookup rules; entry `i` is read again each
/// time, as MuPDF reads it.
fn renumber_dict(d: &mut Dict, map: &[i32]) {
    for i in 0..d.len() {
        let Some((key, value)) = d.entry(i) else {
            break;
        };
        match value {
            Object::Ref(r) => {
                let (key, value) = (Arc::clone(key), new_ref(r, map));
                d.insert(key, value);
            }
            _ => {
                if let Some(v) = d.value_mut(i) {
                    renumber_obj(v, map);
                }
            }
        }
    }
}

/// `do_hash`: zlib's CRC-32 over the object as MuPDF holds it, where null,
/// true, false and interned names are small pointer values.
fn hash_obj(obj: &Object, crc: &mut flate2::Crc) {
    let pointer = |crc: &mut flate2::Crc, value: usize| crc.update(&(value as u64).to_le_bytes());
    match obj {
        Object::Null => pointer(crc, 0),
        Object::Bool(true) => pointer(crc, 1),
        Object::Bool(false) => pointer(crc, 2),
        Object::Int(i) => crc.update(&i.to_le_bytes()),
        Object::Real(f) => crc.update(&f.to_le_bytes()),
        Object::String(s) => crc.update(s),
        Object::Name(n) => hash_name(n, crc),
        Object::Array(a) => a.iter().for_each(|v| hash_obj(v, crc)),
        Object::Dict(d) => hash_dict(d, crc),
        Object::Stream(s) => hash_dict(&s.dict, crc),
        Object::Ref(r) => {
            crc.update(&r.num.to_le_bytes());
            crc.update(&r.generation.to_le_bytes());
        }
    }
}

fn hash_name(name: &[u8], crc: &mut flate2::Crc) {
    match crate::names::constant(name) {
        Some(value) => crc.update(&(value as u64).to_le_bytes()),
        None => crc.update(name),
    }
}

fn hash_dict(d: &Dict, crc: &mut flate2::Crc) {
    for (k, v) in d.iter() {
        hash_name(k, crc);
        hash_obj(v, crc);
    }
}

/// `pdf_objcmp` below the top of an object: whether `a` and `b` differ.
pub(crate) fn differ(a: &Object, b: &Object) -> bool {
    match (a, b) {
        (Object::Null, Object::Null) => false,
        (Object::Bool(x), Object::Bool(y)) => x != y,
        // MuPDF returns the difference as an int.
        (Object::Int(x), Object::Int(y)) => x.wrapping_sub(*y) as i32 != 0,
        // Neither less nor greater is equal, so NaN equals everything.
        (Object::Real(x), Object::Real(y)) => x.partial_cmp(y).is_some_and(|o| o.is_ne()),
        (Object::String(x), Object::String(y)) | (Object::Name(x), Object::Name(y)) => x != y,
        (Object::Ref(x), Object::Ref(y)) => x != y,
        (Object::Array(x), Object::Array(y)) => {
            x.len() != y.len() || x.iter().zip(y.iter()).any(|(p, q)| differ(p, q))
        }
        _ => match (a.as_dict(), b.as_dict()) {
            (Some(x), Some(y)) => dict_differ(x, y),
            _ => true,
        },
    }
}

/// `do_objcmp` on dictionaries: entry by entry when both are sorted,
/// otherwise each key of `a` is looked for in `b` from its own position on,
/// then from the start, and the first match decides.
fn dict_differ(a: &Dict, b: &Dict) -> bool {
    let len = a.len();
    if len != b.len() {
        return true;
    }
    if a.is_sorted() && b.is_sorted() {
        return a
            .iter()
            .zip(b.iter())
            .any(|((ka, va), (kb, vb))| ka != kb || differ(va, vb));
    }
    a.iter().enumerate().any(|(i, (key, value))| {
        let found = (i..len)
            .chain(0..i)
            .find_map(|j| b.entry(j).filter(|(k, _)| k.as_ref() == key));
        found.is_none_or(|(_, v)| differ(value, v))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Object {
        Object::name(s.as_bytes())
    }

    fn dict(entries: &[(&str, Object)]) -> Object {
        let mut d = Dict::new();
        for (k, v) in entries {
            d.put(k.as_bytes(), v.clone());
        }
        Object::Dict(Arc::new(d))
    }

    fn crc(obj: &Object) -> u32 {
        let mut c = flate2::Crc::new();
        hash_obj(obj, &mut c);
        c.sum()
    }

    #[test]
    fn hashes_follow_mupdf_memory_layout() {
        // An interned name hashes as its pointer value, others by bytes.
        let mut c = flate2::Crc::new();
        c.update(&((crate::names::constant(b"Type").unwrap_or(0)) as u64).to_le_bytes());
        assert_eq!(crc(&name("Type")), c.sum());
        let mut c = flate2::Crc::new();
        c.update(b"K000");
        assert_eq!(crc(&name("K000")), c.sum());
        // Ints are 64 bits, reals 32.
        let mut c = flate2::Crc::new();
        c.update(&7i64.to_le_bytes());
        assert_eq!(crc(&Object::Int(7)), c.sum());
        let mut c = flate2::Crc::new();
        c.update(&1.5f32.to_le_bytes());
        assert_eq!(crc(&Object::Real(1.5)), c.sum());
        // Order matters to the hash, not to the comparison.
        let ab = dict(&[("A", Object::Int(1)), ("B", Object::Int(2))]);
        let ba = dict(&[("B", Object::Int(2)), ("A", Object::Int(1))]);
        assert_ne!(crc(&ab), crc(&ba));
        assert!(!differ(&ab, &ba));
    }

    #[test]
    fn comparison_follows_mupdf() {
        assert!(
            !differ(&Object::Int(0), &Object::Int(1 << 32)),
            "int difference"
        );
        assert!(!differ(&Object::Real(f32::NAN), &Object::Real(1.0)), "NaN");
        assert!(differ(&Object::Int(1), &Object::Real(1.0)), "kinds");
        assert!(differ(
            &dict(&[("A", Object::Int(1))]),
            &dict(&[("B", Object::Int(1))])
        ));
        let r = |n| Object::Ref(ObjRef::new(n, 0));
        assert!(differ(&r(1), &r(2)));
        assert!(!differ(
            &Object::Array(Arc::new(vec![r(1)])),
            &Object::Array(Arc::new(vec![r(1)]))
        ));
    }

    #[test]
    fn renumbering_puts_by_key() {
        let map = [0, 5, 6];
        // 101 entries, unsorted: the put of a changed reference sorts it.
        let mut d = Dict::new();
        for i in (0..100).rev() {
            d.put(format!("K{i:03}").as_bytes(), Object::Int(i));
        }
        d.put(b"A", Object::Ref(ObjRef::new(1, 0)));
        assert!(!d.is_sorted());
        renumber_dict(&mut d, &map);
        assert!(d.is_sorted());
        assert_eq!(d.get(b"A"), Some(&Object::Ref(ObjRef::new(5, 0))));

        // Sorted, with an interned last key: the new value is appended.
        let mut d = Dict::new();
        for i in 0..101 {
            d.put(format!("K{i:03}").as_bytes(), Object::Int(i));
        }
        d.put(b"ca", Object::Ref(ObjRef::new(2, 0)));
        assert!(d.is_sorted());
        renumber_dict(&mut d, &map);
        assert_eq!(d.len(), 103);
        assert_eq!(
            d.entry(101).map(|(_, v)| v.clone()),
            Some(Object::Ref(ObjRef::new(2, 0)))
        );
        assert_eq!(
            d.entry(102).map(|(_, v)| v.clone()),
            Some(Object::Ref(ObjRef::new(6, 0)))
        );
    }

    #[test]
    fn duff_references_become_null_where_they_appear() {
        let mut a = Object::Array(Arc::new(vec![
            Object::Ref(ObjRef::new(1, 0)),
            dict(&[
                ("X", Object::Ref(ObjRef::new(2, 0))),
                ("Y", Object::Ref(ObjRef::new(3, 0))),
            ]),
            Object::Ref(ObjRef::new(4, 0)),
        ]));
        let mut refs = Vec::new();
        refs_in(&a, &mut refs);
        assert_eq!(refs, [1, 2, 3, 4]);
        let mut nth = 0;
        null_refs_inside(&mut a, &mut nth, &mut [1usize, 3].into_iter().peekable());
        let shown = crate::print::to_string(&a, Default::default());
        assert_eq!(shown, "[1 0 R<</X null/Y 3 0 R>>null]");
    }
}
