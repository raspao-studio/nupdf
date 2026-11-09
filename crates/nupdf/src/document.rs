// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-xref.c (pdf_init_document,
// pdf_cache_object, pdf_load_obj_stm, pdf_version), source/pdf/pdf-repair.c
// (pdf_repair_xref_aux and helpers) and source/pdf/pdf-page.c
// (pdf_count_pages), Copyright (C) 2004-2025 Artifex Software, Inc.

//! An open PDF document: the xref, lazy object loading with MuPDF's repair
//! on demand, object streams, and document-level facts.
//!
//! The xref lives in an immutable `State` behind an `Arc`. Objects are
//! parsed on first use into per-slot `OnceLock`s, so any number of threads
//! can resolve objects without locks. A repair builds a new `State` and swaps
//! it in; threads holding the old one keep a consistent snapshot.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::diag::Diagnostics;
use crate::filter::{self, DecodeError};
use crate::header::{self, Kind as HeaderKind, Version};
use crate::lex::{Lexer, Token, fz_atof};
use crate::object::{Dict, MAX_OBJECT_NUMBER, ObjRef, Object, Stream};
use crate::parse::{IndirectError, IndirectObject, parse_indirect, parse_stm_obj};
use crate::repair;
use crate::xref::{self, Entry, Kind};

/// Why a document could not be opened at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OpenError {}

#[derive(Debug, Clone)]
enum Loaded {
    Obj(Object),
    /// Loaded nothing, silently (MuPDF returns NULL without a warning).
    Missing,
    Failed(String),
    /// The xref looks wrong for this object; a repair may fix it.
    NeedsRepair(String),
}

#[derive(Debug)]
struct ObjStm {
    objects: Vec<(i32, Object)>,
    error: Option<String>,
}

#[derive(Debug)]
struct State {
    bias: usize,
    entries: Vec<Entry>,
    cache: Vec<OnceLock<Loaded>>,
    objstms: Mutex<HashMap<i32, Arc<ObjStm>>>,
    trailer: Dict,
    revisions: usize,
    repaired: bool,
}

impl State {
    fn new(
        bias: usize,
        entries: Vec<Entry>,
        trailer: Dict,
        revisions: usize,
        repaired: bool,
    ) -> Self {
        let cache = (0..entries.len()).map(|_| OnceLock::new()).collect();
        State {
            bias,
            entries,
            cache,
            objstms: Mutex::new(HashMap::new()),
            trailer,
            revisions,
            repaired,
        }
    }

    fn empty() -> Self {
        State::new(0, Vec::new(), Dict::new(), 0, true)
    }

    fn precache(&self, num: i32, obj: Object) {
        if let Some(cell) = usize::try_from(num).ok().and_then(|i| self.cache.get(i)) {
            let _ = cell.set(Loaded::Obj(obj));
        }
    }

    fn xref_len(&self) -> usize {
        self.entries.len()
    }
}

thread_local! {
    /// (state address, slot) pairs being loaded on this thread, to break
    /// cycles such as object streams that contain each other.
    static LOADING: RefCell<Vec<(usize, i32)>> = const { RefCell::new(Vec::new()) };
}

struct LoadGuard(usize, i32);

impl LoadGuard {
    fn enter(state: &State, key: i32) -> Option<LoadGuard> {
        let addr = std::ptr::from_ref(state) as usize;
        LOADING.with(|l| {
            let mut l = l.borrow_mut();
            if l.contains(&(addr, key)) || l.len() > 256 {
                None
            } else {
                l.push((addr, key));
                Some(LoadGuard(addr, key))
            }
        })
    }
}

impl Drop for LoadGuard {
    fn drop(&mut self) {
        LOADING.with(|l| {
            let mut l = l.borrow_mut();
            if let Some(i) = l.iter().rposition(|e| *e == (self.0, self.1)) {
                l.remove(i);
            }
        });
    }
}

#[derive(Debug)]
pub struct Document {
    data: Arc<[u8]>,
    header_version: Version,
    is_fdf: bool,
    diag: Diagnostics,
    state: RwLock<Arc<State>>,
    /// MuPDF repairs at most once per document.
    repair_attempted: AtomicBool,
    /// Serializes repairs so other threads wait for the new xref.
    repair_lock: Mutex<()>,
}

/// Page counting failed the way MuPDF's `pdf_count_pages` fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCountError(pub String);

impl Document {
    /// `pdf_init_document`: read the header and xref, repairing if needed.
    pub fn open(data: impl Into<Arc<[u8]>>) -> Result<Document, OpenError> {
        let data: Arc<[u8]> = data.into();
        let diag = Diagnostics::new();
        let mut broken: Option<String> = None;
        let mut bias = 0;
        let mut version = Version(17);
        let mut is_fdf = false;
        match header::find(&data) {
            Ok(h) => {
                version = h.version;
                is_fdf = h.kind == HeaderKind::Fdf;
                if !version.is_known() {
                    diag.warn(
                        "header.unknown_version",
                        format!("unknown PDF version: {version}"),
                    );
                }
                if h.offset > 0 {
                    diag.warn(
                        "header.garbage_prefix",
                        "garbage bytes before version marker",
                    );
                    bias = h.offset;
                }
            }
            Err(_) => broken = Some("cannot find version marker".into()),
        }

        let doc = Document {
            data,
            header_version: version,
            is_fdf,
            diag,
            state: RwLock::new(Arc::new(State::empty())),
            repair_attempted: AtomicBool::new(false),
            repair_lock: Mutex::new(()),
        };

        let loaded = if broken.is_none() && !is_fdf {
            match xref::load(&doc.data, bias, &doc.diag) {
                Ok(loaded) => Some(loaded),
                Err(e) => {
                    broken = Some(e);
                    None
                }
            }
        } else {
            None
        };

        match loaded {
            Some(l) => {
                let state = State::new(bias, l.entries, l.trailer, l.revisions, false);
                for (num, obj) in l.cached {
                    state.precache(num, obj);
                }
                doc.install(state);
            }
            None => {
                if let Some(e) = &broken {
                    doc.diag.warn("xref.broken", format!("format error: {e}"));
                    doc.diag.warn("xref.repair", "trying to repair broken xref");
                }
                doc.repair_attempted.store(true, Ordering::SeqCst);
                match doc.repair() {
                    Ok(state) => doc.install(state),
                    Err(e) => {
                        return Err(OpenError {
                            code: "input.unrepairable",
                            message: e,
                        });
                    }
                }
            }
        }
        Ok(doc)
    }

    fn install(&self, state: State) {
        if let Ok(mut s) = self.state.write() {
            *s = Arc::new(state);
        }
    }

    fn current(&self) -> Arc<State> {
        self.state
            .read()
            .map(|s| Arc::clone(&s))
            .unwrap_or_else(|_| Arc::new(State::empty()))
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diag
    }

    pub fn trailer(&self) -> Dict {
        self.current().trailer.clone()
    }

    /// Number of slots in the xref (MuPDF's `pdf_xref_len`).
    pub fn xref_len(&self) -> usize {
        self.current().xref_len()
    }

    /// Whether the xref had to be rebuilt by scanning the file.
    pub fn repaired(&self) -> bool {
        self.current().repaired
    }

    /// Number of xref sections (revisions) read; 1 after a repair.
    pub fn revisions(&self) -> usize {
        let s = self.current();
        if s.repaired { 1 } else { s.revisions }
    }

    pub fn is_encrypted(&self) -> bool {
        self.current().trailer.get(b"Encrypt").is_some()
    }

    /// Loads object `num`. `None` when it cannot be loaded (with a
    /// diagnostic, as in `pdf_resolve_indirect`).
    pub fn object(&self, num: i32) -> Option<Object> {
        loop {
            let state = self.current();
            match self.load_in(&state, num, true) {
                Loaded::Obj(o) => return Some(o),
                Loaded::Missing => return None,
                Loaded::Failed(_) => {
                    self.diag.warn(
                        "object.unloadable",
                        format!("cannot load object ({num} 0 R) into cache"),
                    );
                    return None;
                }
                Loaded::NeedsRepair(_) => {
                    if !self.repair_on_demand(&state) {
                        self.diag.warn(
                            "object.unloadable",
                            format!("cannot load object ({num} 0 R) into cache"),
                        );
                        return None;
                    }
                }
            }
        }
    }

    /// Resolves references (one level, as MuPDF's RESOLVE does); other
    /// objects come back unchanged. Unloadable references become null.
    pub fn resolve(&self, obj: &Object) -> Object {
        match obj {
            Object::Ref(r) => self.object(r.num).unwrap_or(Object::Null),
            other => other.clone(),
        }
    }

    /// `pdf_dict_get` on a possibly indirect dictionary, resolving the value.
    pub fn dict_get(&self, obj: &Object, key: &[u8]) -> Object {
        match self.resolve(obj).as_dict().and_then(|d| d.get(key)) {
            Some(v) => self.resolve(v),
            None => Object::Null,
        }
    }

    /// The effective PDF version: the header's, unless the catalog's
    /// /Version overrides it (`pdf_version`).
    pub fn version(&self) -> Version {
        let root = self.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
        match self.dict_get(&root, b"Version").as_name() {
            Some(name) if !name.is_empty() => Version((10.0f32 * (fz_atof(name) + 0.05f32)) as i32),
            _ => self.header_version,
        }
    }

    /// The version in the header (1.7 when the header is missing).
    pub fn header_version(&self) -> Version {
        self.header_version
    }

    /// `pdf_count_pages`: /Root/Pages/Count, sanity-checked.
    pub fn page_count(&self) -> Result<i32, PageCountError> {
        if self.is_fdf {
            return Ok(0);
        }
        let root = self.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
        let pages = self.dict_get(&root, b"Pages");
        let count = self.dict_get(&pages, b"Count").to_int();
        if count < 0 || i64::from(count) >= self.xref_len() as i64 {
            return Err(PageCountError("Invalid number of pages".into()));
        }
        Ok(count)
    }

    /// Decoded data of a stream object, for non-image use.
    pub fn stream_data(&self, stream: &Stream) -> Result<Vec<u8>, DecodeError> {
        self.stream_data_in(&self.current(), stream)
    }

    // ---- loading -------------------------------------------------------

    fn attempted(&self) -> bool {
        self.repair_attempted.load(Ordering::SeqCst)
    }

    /// Resolve against one state, without triggering a repair.
    fn resolve_in(&self, st: &State, obj: &Object) -> Object {
        let Object::Ref(r) = obj else {
            return obj.clone();
        };
        match self.load_in(st, r.num, false) {
            Loaded::Obj(o) => o,
            Loaded::Missing => Object::Null,
            Loaded::Failed(_) | Loaded::NeedsRepair(_) => {
                self.diag.warn(
                    "object.unloadable",
                    format!("cannot load object ({} 0 R) into cache", r.num),
                );
                Object::Null
            }
        }
    }

    fn stream_data_in(&self, st: &State, stream: &Stream) -> Result<Vec<u8>, DecodeError> {
        let length = self
            .resolve_in(st, stream.dict.get(b"Length").unwrap_or(&Object::Null))
            .to_int64();
        let raw = xref::raw_stream(&self.data, stream.data_start, length, &self.diag);
        filter::decode(&stream.dict, raw, &|o| self.resolve_in(st, o), &self.diag)
    }

    /// `pdf_cache_object` against one state.
    fn load_in(&self, st: &State, num: i32, allow_repair: bool) -> Loaded {
        let Some(cell) = usize::try_from(num)
            .ok()
            .filter(|&i| i > 0)
            .and_then(|i| st.cache.get(i))
        else {
            return Loaded::Missing;
        };
        if let Some(v) = cell.get() {
            return v.clone();
        }
        let Some(_guard) = LoadGuard::enter(st, num) else {
            return Loaded::Failed(format!("recursive object lookup ({num} 0 R)"));
        };
        let loaded = self.load_slot(st, num, allow_repair);
        let _ = cell.set(loaded);
        cell.get().cloned().unwrap_or(Loaded::Missing)
    }

    fn parse_at(&self, st: &State, ofs: i64) -> Result<IndirectObject, IndirectError> {
        let at = usize::try_from(ofs)
            .unwrap_or(usize::MAX)
            .saturating_add(st.bias);
        let mut lx = Lexer::new(&self.data, at, &self.diag);
        parse_indirect(&mut lx)
    }

    fn load_slot(&self, st: &State, num: i32, allow_repair: bool) -> Loaded {
        let entry = st.entries[num as usize];
        let can_repair = allow_repair && !self.attempted();
        match entry.kind {
            Kind::Free => Loaded::Obj(Object::Null),
            Kind::None => Loaded::Failed(format!("cannot find object in xref ({num} 0 R)")),
            Kind::InFile => match self.parse_at(st, entry.ofs) {
                Ok(ind) if ind.num == num => Loaded::Obj(into_object(ind)),
                Ok(ind) => {
                    if can_repair {
                        Loaded::NeedsRepair(format!(
                            "found object ({} 0 R) instead of ({num} 0 R)",
                            ind.num
                        ))
                    } else {
                        Loaded::Missing
                    }
                }
                Err(e) if e.try_repair => {
                    if can_repair {
                        Loaded::NeedsRepair(e.message)
                    } else {
                        Loaded::Failed(format!("cannot parse object ({num} 0 R)"))
                    }
                }
                Err(e) => Loaded::Failed(e.message),
            },
            Kind::InStream => {
                let objstm = self.objstm(st, entry.ofs as i32);
                if let Some((_, obj)) = objstm.objects.iter().find(|(n, _)| *n == num) {
                    return Loaded::Obj(obj.clone());
                }
                if let Some(e) = &objstm.error {
                    return Loaded::Failed(e.clone());
                }
                if can_repair {
                    Loaded::NeedsRepair(format!(
                        "object ({num} 0 R) was not found in its object stream"
                    ))
                } else {
                    Loaded::Failed(format!(
                        "object ({num} 0 R) was not found in its object stream"
                    ))
                }
            }
        }
    }

    /// `pdf_load_obj_stm`, cached per state.
    fn objstm(&self, st: &State, stm: i32) -> Arc<ObjStm> {
        if let Some(found) = st.objstms.lock().ok().and_then(|m| m.get(&stm).cloned()) {
            return found;
        }
        let computed = Arc::new(self.load_objstm(st, stm));
        if let Ok(mut map) = st.objstms.lock() {
            return Arc::clone(map.entry(stm).or_insert(computed));
        }
        computed
    }

    fn load_objstm(&self, st: &State, stm: i32) -> ObjStm {
        let fail = |e: String| ObjStm {
            objects: Vec::new(),
            error: Some(e),
        };
        let Some(_guard) = LoadGuard::enter(st, -1 - stm) else {
            return fail("recursive object stream lookup".into());
        };
        let xref_len = st.xref_len() as i64;
        if stm <= 0 || i64::from(stm) >= xref_len {
            return fail(format!("corrupt object stream {stm}"));
        }
        if st.entries[stm as usize].kind == Kind::Free {
            return fail(format!("corrupt object stream {stm}"));
        }
        let stream = match self.load_in(st, stm, true) {
            Loaded::Obj(Object::Stream(s)) => s,
            Loaded::Obj(_) => return fail(format!("object is not a stream ({stm} 0 R)")),
            _ => return fail(format!("corrupt object stream {stm}")),
        };
        let count = self
            .resolve_in(st, stream.dict.get(b"N").unwrap_or(&Object::Null))
            .to_int();
        let first = self
            .resolve_in(st, stream.dict.get(b"First").unwrap_or(&Object::Null))
            .to_int();
        if !(0..=MAX_OBJECT_NUMBER).contains(&count) {
            return fail("number of objects in object stream out of range".into());
        }
        let data = match self.stream_data_in(st, &stream) {
            Ok(d) => d,
            Err(e) => return fail(e.to_string()),
        };

        let mut lx = Lexer::new(&data, 0, &self.diag);
        let mut header: Vec<(i32, i64)> = Vec::new();
        for _ in 0..count {
            let Token::Int(n) = lx.next_token() else {
                return fail(format!("corrupt object stream ({stm} 0 R)"));
            };
            let Token::Int(o) = lx.next_token() else {
                return fail(format!("corrupt object stream ({stm} 0 R)"));
            };
            let n = n as i32;
            if n <= 0 || i64::from(n) >= xref_len {
                self.diag.warn(
                    "objstm.out_of_range",
                    "object stream object out of range, skipping",
                );
            } else {
                header.push((n, o));
            }
        }

        // Objects are read through a forward-only decoded stream: a target
        // behind the current position cannot be reached, so MuPDF warns and
        // reads from where it is.
        let mut position = lx.pos();
        let mut objects = Vec::with_capacity(header.len());
        for (i, &(n, ofs)) in header.iter().enumerate() {
            let target = i64::from(first).saturating_add(ofs);
            let start = if target < position as i64 {
                self.diag
                    .warn("objstm.seek_backwards", "cannot seek backwards");
                position
            } else {
                usize::try_from(target)
                    .unwrap_or(usize::MAX)
                    .min(data.len())
            };
            let end = match header.get(i + 1) {
                Some(&(_, next)) => {
                    let len = next.wrapping_sub(ofs) as u64;
                    start
                        .saturating_add(usize::try_from(len).unwrap_or(usize::MAX))
                        .min(data.len())
                }
                None => data.len(),
            };
            let mut sub = Lexer::new(&data[..end], start, &self.diag);
            match parse_stm_obj(&mut sub) {
                Ok(obj) => objects.push((n, obj)),
                Err(e) => {
                    return ObjStm {
                        objects,
                        error: Some(e.0),
                    };
                }
            }
            position = end;
        }
        ObjStm {
            objects,
            error: None,
        }
    }

    // ---- repair --------------------------------------------------------

    /// Repair triggered by a bad object; true if a newer state is in place.
    fn repair_on_demand(&self, seen: &Arc<State>) -> bool {
        let Ok(_serial) = self.repair_lock.lock() else {
            return false;
        };
        if self.attempted() {
            return !Arc::ptr_eq(seen, &self.current());
        }
        self.repair_attempted.store(true, Ordering::SeqCst);
        match self.repair() {
            Ok(state) => {
                self.install(state);
                true
            }
            Err(e) => {
                // MuPDF has already dropped the old xref at this point, so the
                // document is left empty.
                self.diag.warn("xref.repair_failed", e);
                self.install(State::empty());
                false
            }
        }
    }

    /// `pdf_repair_xref_aux`: scan, rebuild, then fix object streams,
    /// /Root and /Info.
    fn repair(&self) -> Result<State, String> {
        if !self.is_fdf {
            self.diag.warn("xref.repairing", "repairing PDF document");
        }
        let scan = repair::scan(&self.data, &self.diag)?;
        let encrypted = scan.encrypt.is_some();

        let len = scan.maxnum as usize + 1;
        let mut entries = vec![Entry::free(0); len];
        let mut fixed: HashMap<i32, Object> = HashMap::new();
        let probe = State::new(0, entries.clone(), Dict::new(), 1, true);
        for item in &scan.list {
            let entry = Entry {
                kind: Kind::InFile,
                ofs: item.ofs,
                generation: item.generation,
                stm_ofs: item.stm_ofs,
            };
            entries[item.num as usize] = entry;
            // Correct stream lengths measured by the scan (unencrypted only).
            // MuPDF patches the object it has cached for this number, even if
            // that came from an earlier definition.
            if !encrypted && item.stm_len >= 0 {
                let obj = match fixed.remove(&item.num) {
                    Some(o) => o,
                    None => match self.parse_at(&probe, item.ofs) {
                        Ok(ind) if ind.num == item.num => into_object(ind),
                        Ok(_) => return Err(format!("not a dict ({} 0 R)", item.num)),
                        Err(_) => return Err(format!("cannot parse object ({} 0 R)", item.num)),
                    },
                };
                let obj = with_length(obj, item.stm_len).ok_or_else(|| "not a dict".to_string())?;
                fixed.insert(item.num, obj);
            }
        }
        entries[0] = Entry::free(65535);

        let mut trailer = Dict::new();
        trailer.put(b"Size", Object::Int(i64::from(scan.maxnum) + 1));
        if let Some(info) = &scan.info {
            trailer.put(b"Info", info.clone());
        }
        if let Some(e) = &scan.encrypt {
            trailer.put(b"Encrypt", e.clone());
        }
        if let Some(id) = &scan.id {
            trailer.put(b"ID", id.clone());
        }

        // Object streams: claim their objects, unless a plain object later in
        // the file already defines the number.
        let first = State::new(0, entries.clone(), trailer.clone(), 1, true);
        for (&num, obj) in &fixed {
            first.precache(num, obj.clone());
        }
        let initial_len = entries.len();
        let mut changed: Vec<bool> = vec![false; initial_len];
        for i in 0..initial_len {
            if entries[i].stm_ofs == 0 {
                continue;
            }
            let obj = match self.load_in(&first, i as i32, false) {
                Loaded::Obj(o) => o,
                Loaded::Missing => continue,
                Loaded::Failed(e) | Loaded::NeedsRepair(e) => return Err(e),
            };
            let is_objstm = obj
                .as_dict()
                .and_then(|d| d.get(b"Type"))
                .is_some_and(|t| self.resolve_in(&first, t).is_name(b"ObjStm"));
            if is_objstm
                && self
                    .repair_objstm(&first, i as i32, &obj, &mut entries, &mut changed)
                    .is_err()
            {
                self.diag.warn(
                    "repair.broken_objstm",
                    format!("ignoring broken object stream ({i} 0 R)"),
                );
            }
        }
        for i in 0..initial_len {
            if entries[i].kind == Kind::InStream
                && entries
                    .get(entries[i].ofs as usize)
                    .is_none_or(|e| e.kind != Kind::InFile)
            {
                self.diag.warn(
                    "repair.bad_objstm_ref",
                    format!(
                        "invalid reference to non-object-stream: {}, assuming {i} 0 R is a freed object",
                        entries[i].ofs as i32
                    ),
                );
                entries[i].kind = Kind::Free;
                changed[i] = true;
            }
        }

        let mut second = State::new(0, entries, trailer, 1, true);
        for (i, cell) in first.cache.iter().enumerate() {
            if !changed.get(i).copied().unwrap_or(true)
                && let Some(v) = cell.get()
            {
                let _ = second.cache[i].set(v.clone());
            }
        }

        // /Root: the last candidate that is a reference to a dictionary.
        let mut root = None;
        for candidate in scan.roots.iter().rev() {
            if let Object::Ref(r) = candidate
                && matches!(
                    self.load_in(&second, r.num, false),
                    Loaded::Obj(Object::Dict(_) | Object::Stream(_))
                )
            {
                root = Some(candidate.clone());
                break;
            }
        }
        let mut trailer = second.trailer.clone();
        if let Some(r) = root {
            trailer.put(b"Root", r);
        }

        // Still missing /Root or /Info: look for a catalog and an info-like
        // dictionary, newest objects first.
        let mut has_root = trailer.get(b"Root").is_some();
        let mut has_info = trailer.get(b"Info").is_some();
        let mut i = second.xref_len() as i32 - 1;
        while i > 0 && (!has_root || !has_info) {
            let kind = second.entries[i as usize].kind;
            if kind != Kind::None && kind != Kind::Free {
                match self.load_in(&second, i, false) {
                    Loaded::Obj(obj) => {
                        if let Some(d) = obj.as_dict() {
                            if !has_root && d.get(b"Type").is_some_and(|t| t.is_name(b"Catalog")) {
                                trailer.put(b"Root", Object::Ref(ObjRef::new(i, 0)));
                                has_root = true;
                            }
                            if !has_info
                                && (d.get(b"Creator").is_some() || d.get(b"Producer").is_some())
                            {
                                trailer.put(b"Info", Object::Ref(ObjRef::new(i, 0)));
                                has_info = true;
                            }
                        }
                    }
                    Loaded::Missing => {}
                    Loaded::Failed(_) | Loaded::NeedsRepair(_) => {
                        self.diag.warn(
                            "repair.broken_object",
                            format!("ignoring broken object ({i} 0 R)"),
                        );
                    }
                }
            }
            i -= 1;
        }

        second.trailer = trailer;
        Ok(second)
    }

    /// `pdf_repair_obj_stm`.
    fn repair_objstm(
        &self,
        st: &State,
        stm: i32,
        obj: &Object,
        entries: &mut Vec<Entry>,
        changed: &mut [bool],
    ) -> Result<(), String> {
        let Object::Stream(stream) = obj else {
            return Err("object is not a stream".into());
        };
        let count = self
            .resolve_in(st, stream.dict.get(b"N").unwrap_or(&Object::Null))
            .to_int();
        let data = self.stream_data_in(st, stream).map_err(|e| e.to_string())?;
        let mut lx = Lexer::small(&data, 0, &self.diag);
        let mut i = 0;
        while i < count {
            let Token::Int(n) = lx.next_token() else {
                return Err(format!("corrupt object stream ({stm} 0 R)"));
            };
            let n = n as i32;
            if !(0..MAX_OBJECT_NUMBER).contains(&n) {
                // MuPDF skips without reading the offset, so the offset is
                // read as the next object number.
                self.diag.warn(
                    "repair.bad_object_number",
                    format!("ignoring object with invalid object number ({n} {i} R)"),
                );
                i += 1;
                continue;
            }
            if n as usize >= entries.len() {
                entries.resize(n as usize + 1, Entry::NONE);
            }
            let mut replace = true;
            let existing = entries[n as usize];
            if existing.kind != Kind::None && existing.kind != Kind::Free {
                let here = entry_offset(entries, n);
                if here >= 0 && here > entry_offset(entries, stm) {
                    replace = false;
                }
            }
            if replace {
                entries[n as usize] = Entry {
                    kind: Kind::InStream,
                    ofs: i64::from(stm),
                    generation: i,
                    stm_ofs: 0,
                };
                if let Some(c) = changed.get_mut(n as usize) {
                    *c = true;
                }
            }
            let Token::Int(_) = lx.next_token() else {
                return Err(format!("corrupt object stream ({stm} 0 R)"));
            };
            i += 1;
        }
        Ok(())
    }
}

/// Offset used to decide whether an object stream may claim a number.
fn entry_offset(entries: &[Entry], num: i32) -> i64 {
    let Some(e) = usize::try_from(num).ok().and_then(|i| entries.get(i)) else {
        return 0;
    };
    match e.kind {
        Kind::None | Kind::Free => 0,
        Kind::InFile => e.ofs,
        Kind::InStream => match usize::try_from(e.ofs).ok().and_then(|i| entries.get(i)) {
            Some(s) if s.kind == Kind::InFile => s.ofs,
            _ => -1,
        },
    }
}

fn into_object(ind: IndirectObject) -> Object {
    match (ind.stream_start, ind.obj) {
        (Some(data_start), Object::Dict(dict)) => Object::Stream(Arc::new(Stream {
            dict: Arc::unwrap_or_clone(dict),
            data_start,
        })),
        (_, obj) => obj,
    }
}

fn with_length(obj: Object, length: i64) -> Option<Object> {
    match obj {
        Object::Stream(s) => {
            let mut s = Arc::unwrap_or_clone(s);
            s.dict.put(b"Length", Object::Int(length));
            Some(Object::Stream(Arc::new(s)))
        }
        Object::Dict(d) => {
            let mut d = Arc::unwrap_or_clone(d);
            d.put(b"Length", Object::Int(length));
            Some(Object::Dict(Arc::new(d)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(bytes: &[u8]) -> Document {
        Document::open(bytes.to_vec()).unwrap()
    }

    const SIMPLE: &[u8] = b"%PDF-1.4\n1 0 obj <</Type/Catalog/Pages 2 0 R>> endobj\n2 0 obj <</Type/Pages/Kids[]/Count 0>> endobj\ntrailer <</Root 1 0 R>>\n";

    #[test]
    fn missing_xref_is_repaired() {
        let d = doc(SIMPLE);
        assert!(d.repaired());
        assert!(d.diagnostics().has("xref.repairing"));
        assert_eq!(d.page_count(), Ok(0));
        assert_eq!(d.version(), Version(14));
    }

    #[test]
    fn unrepairable_files_fail_to_open() {
        let e = Document::open(b"%PDF-1.4\nnothing here\n".to_vec()).unwrap_err();
        assert_eq!(e.message, "no objects found");
    }

    #[test]
    fn catalog_version_overrides_header() {
        let d = doc(
            b"%PDF-1.4\n1 0 obj <</Type/Catalog/Version/1.7>> endobj\ntrailer <</Root 1 0 R>>\n",
        );
        assert_eq!(d.version(), Version(17));
        assert_eq!(d.header_version(), Version(14));
    }

    #[test]
    fn repair_finds_catalog_without_trailer() {
        let d = doc(b"%PDF-1.4\n1 0 obj <</Type/Catalog/Pages 2 0 R>> endobj\n2 0 obj <</Count 0>> endobj\n");
        assert_eq!(
            d.trailer().get(b"Root"),
            Some(&Object::Ref(ObjRef::new(1, 0)))
        );
    }

    #[test]
    fn page_count_sanity_checks() {
        let d = doc(b"%PDF-1.4\n1 0 obj <</Type/Catalog/Pages 2 0 R>> endobj\n2 0 obj <</Count 5>> endobj\ntrailer <</Root 1 0 R>>\n");
        // Five pages cannot fit in a three-slot xref.
        assert!(d.page_count().is_err());
    }

    #[test]
    fn concurrent_resolution_agrees() {
        let d = Arc::new(doc(SIMPLE));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let d = Arc::clone(&d);
                std::thread::spawn(move || d.object(2))
            })
            .collect();
        let first = d.object(2);
        for h in handles {
            assert_eq!(h.join().unwrap(), first);
        }
    }

    #[test]
    fn document_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Document>();
    }
}
