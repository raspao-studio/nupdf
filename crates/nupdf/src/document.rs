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
//! parsed on first use into a sharded cache, so threads rarely contend. A
//! repair builds a new `State` and swaps it in; threads holding the old one
//! keep a consistent snapshot. In an encrypted document, strings are
//! decrypted as objects load and streams as their data is read.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::crypt::Crypt;
use crate::diag::{Diagnostics, report};
use crate::filter::{self, DecodeError};
use crate::header::{self, Kind as HeaderKind, Version};
use crate::lex::{Lexer, Token, fz_atof};
use crate::limits::Limits;
use crate::object::{Dict, MAX_OBJECT_NUMBER, ObjRef, Object, Stream};
use crate::parse::{IndirectError, IndirectObject, parse_indirect, parse_stm_obj};
use crate::repair;
use crate::slots::Slots;
use crate::stream;
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

/// Loaded objects, keyed by number, in chunks of slots made when an
/// object in their range is first cached: memory follows what was loaded
/// rather than how many slots the file declares. Each slot is set once,
/// so readers on many threads take no lock and write nothing shared.
#[derive(Debug)]
struct Cache {
    chunks: Box<[OnceLock<Chunk>]>,
}

/// [`Cache::CHUNK`] slots.
type Chunk = Box<[OnceLock<Loaded>]>;

impl Cache {
    const CHUNK: usize = 256;

    /// A cache with slots for objects numbered below `len`, rounded up to
    /// a whole chunk.
    fn new(len: usize) -> Self {
        Cache {
            chunks: (0..len.div_ceil(Self::CHUNK))
                .map(|_| OnceLock::new())
                .collect(),
        }
    }

    fn slot(&self, num: i32, make: bool) -> Option<&OnceLock<Loaded>> {
        let n = usize::try_from(num).ok()?;
        let chunk = self.chunks.get(n / Self::CHUNK)?;
        let slots = if make {
            chunk.get_or_init(|| (0..Self::CHUNK).map(|_| OnceLock::new()).collect())
        } else {
            chunk.get()?
        };
        slots.get(n % Self::CHUNK)
    }

    fn get(&self, num: i32) -> Option<Loaded> {
        self.slot(num, false)?.get().cloned()
    }

    fn clear(&mut self) {
        for chunk in &mut self.chunks {
            *chunk = OnceLock::new();
        }
    }

    /// Stores `value` unless another thread got there first; returns the
    /// value that is now cached.
    fn insert_if_absent(&self, num: i32, value: Loaded) -> Loaded {
        match self.slot(num, true) {
            Some(slot) => slot.get_or_init(|| value).clone(),
            None => value,
        }
    }

    fn all(&self) -> Vec<(i32, Loaded)> {
        let mut out = Vec::new();
        for (c, chunk) in self.chunks.iter().enumerate() {
            let Some(slots) = chunk.get() else { continue };
            for (i, slot) in slots.iter().enumerate() {
                if let (Some(v), Ok(num)) = (slot.get(), i32::try_from(c * Self::CHUNK + i)) {
                    out.push((num, v.clone()));
                }
            }
        }
        out
    }
}

#[derive(Debug)]
pub(crate) struct State {
    bias: usize,
    entries: Slots,
    cache: Cache,
    objstms: Mutex<HashMap<i32, Arc<ObjStm>>>,
    /// Object streams that failed to load. MuPDF marks their entry free, so
    /// from then on the object still resolves but is no longer a stream.
    demoted: Mutex<std::collections::HashSet<i32>>,
    /// Whether `demoted` has anything in it, so that looking up a stream
    /// does not take its lock on every thread.
    any_demoted: AtomicBool,
    trailer: Dict,
    revisions: usize,
    repaired: bool,
    /// MuPDF's page map, built on first lookup (see `page`).
    pub(crate) page_map: OnceLock<Option<crate::page::PageMap>>,
    /// Set when building the page map found fewer pages than /Count claims;
    /// MuPDF rewrites /Count to match.
    pub(crate) page_count_override: OnceLock<i32>,
    /// Parent links re-derived from /Kids after an inheritance cycle, as
    /// MuPDF's `pdf_repair_page_tree_parents` rewrites them.
    pub(crate) repaired_parents: OnceLock<HashMap<i32, Object>>,
    /// Xref section offsets relative to the header, newest first; empty
    /// after a repair.
    section_offsets: Vec<i64>,
    /// The oldest xref section's size (`Loaded::oldest_size`); 0 when
    /// there is one section, as after a repair.
    oldest_size: i32,
    /// `doc->last_xref_was_old_style`.
    last_xref_was_old_style: bool,
    /// The xref stream whose dictionary the trailer is (`Loaded::trailer_owner`).
    trailer_owner: Option<i32>,
}

impl State {
    fn new(bias: usize, entries: Slots, trailer: Dict, revisions: usize, repaired: bool) -> Self {
        State {
            bias,
            cache: Cache::new(entries.len()),
            entries,
            objstms: Mutex::new(HashMap::new()),
            demoted: Mutex::new(std::collections::HashSet::new()),
            any_demoted: AtomicBool::new(false),
            trailer,
            revisions,
            repaired,
            page_map: OnceLock::new(),
            page_count_override: OnceLock::new(),
            repaired_parents: OnceLock::new(),
            section_offsets: Vec::new(),
            oldest_size: 0,
            last_xref_was_old_style: false,
            trailer_owner: None,
        }
    }

    fn empty() -> Self {
        State::new(0, Slots::filled(0, Entry::NONE), Dict::new(), 0, true)
    }

    fn precache(&self, num: i32, obj: Object) {
        if num > 0 && (num as usize) < self.xref_len() {
            self.cache.insert_if_absent(num, Loaded::Obj(obj));
        }
    }

    fn xref_len(&self) -> usize {
        self.entries.len()
    }

    fn is_demoted(&self, num: i32) -> bool {
        self.any_demoted.load(Ordering::Acquire)
            && self.demoted.lock().is_ok_and(|d| d.contains(&num))
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

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// A document's state as one thread keeps it.
#[expect(
    clippy::redundant_allocation,
    reason = "the Rc is counted without atomics, which is why threads keep it"
)]
struct Kept {
    id: u64,
    generation: u64,
    state: Rc<Arc<State>>,
}

thread_local! {
    static CURRENT: RefCell<Option<Kept>> = const { RefCell::new(None) };
}

impl Drop for Document {
    /// Lets go of the state this thread keeps, so a document dropped where
    /// it was used does not stay in memory. Other threads let go of theirs
    /// when they read another document or end.
    fn drop(&mut self) {
        let _ = CURRENT.try_with(|kept| {
            if let Ok(mut kept) = kept.try_borrow_mut()
                && kept.as_ref().is_some_and(|k| k.id == self.id)
            {
                *kept = None;
            }
        });
    }
}

#[derive(Debug)]
pub struct Document {
    data: Arc<[u8]>,
    /// MuPDF's `doc->file_size`: only set when the xref loader ran, so it is
    /// 0 for files without a header, and every stream /Length is ignored.
    file_size: usize,
    header_version: Version,
    is_fdf: bool,
    /// The first object is a linearization dictionary (`pdf_check_linear`).
    linearized: bool,
    limits: Limits,
    diag: Diagnostics,
    state: RwLock<Arc<State>>,
    /// Counts the states installed, so that a thread can keep the current
    /// one ([`Document::current`]) and see when it is replaced.
    generation: AtomicU64,
    /// Tells documents apart in each thread's kept state.
    id: u64,
    /// MuPDF repairs at most once per document.
    repair_attempted: AtomicBool,
    /// Serializes repairs so other threads wait for the new xref.
    repair_lock: Mutex<()>,
    /// The security handler, created once while opening (MuPDF's
    /// `doc->crypt`). Authenticating replaces it with a new key.
    crypt: RwLock<Option<Arc<Crypt>>>,
    /// Whether the empty password failed (`pdf_needs_password`).
    needs_password: AtomicBool,
    /// Objects changed or added since opening (MuPDF changes its in-memory
    /// objects in place); see `edit`.
    edits: RwLock<Edits>,
    /// Whether `edits` holds anything, checked before every load.
    edited: AtomicBool,
    /// Made by `create`, not read from a file.
    created: bool,
    /// MuPDF's local xref: streams made only to be drawn (appearances an
    /// annotation is given on loading its page), by their negative
    /// numbers, which no object of the document has.
    local: RwLock<HashMap<i32, Vec<u8>>>,
    local_next: std::sync::atomic::AtomicI32,
}

/// The copy-on-write layer over the parsed document.
#[derive(Debug, Default)]
pub(crate) struct Edits {
    /// Replaced and new objects by number.
    pub(crate) objects: HashMap<i32, Object>,
    /// The trailer, once changed.
    pub(crate) trailer: Option<Dict>,
    /// The xref length with new objects (`pdf_xref_len`).
    pub(crate) len: usize,
    /// The data of streams the edits made, by object number.
    pub(crate) data: HashMap<i32, Vec<u8>>,
    /// The generation of each edited object's entry in MuPDF's incremental
    /// section: kept when an object is changed in place, 0 when it is
    /// replaced or added.
    pub(crate) gens: HashMap<i32, i32>,
    /// The page map, built again after each edit as MuPDF drops it.
    pub(crate) page_map: Option<Option<crate::page::PageMap>>,
}

/// A stream of the document's local xref ([`Document::add_local_stream`]).
#[derive(Debug)]
pub struct LocalStream<'a> {
    doc: &'a Document,
    obj: Object,
    num: i32,
}

impl LocalStream<'_> {
    /// The stream, to be run as a form.
    pub fn object(&self) -> &Object {
        &self.obj
    }
}

impl Drop for LocalStream<'_> {
    fn drop(&mut self) {
        if let Ok(mut l) = self.doc.local.write() {
            l.remove(&self.num);
        }
    }
}

/// Marks a repair error that came from the security handler.
const CRYPT_PREFIX: &str = "crypt: ";

/// Page counting failed the way MuPDF's `pdf_count_pages` fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCountError(pub String);

impl Document {
    /// `pdf_create_document`: a new document whose page tree has no pages.
    /// As in MuPDF, its /Info is in the catalog, naming the producer.
    pub fn create() -> Result<Document, OpenError> {
        let objects = [
            "<</Type/Catalog/Pages 2 0 R/Info<</Producer(nupdf)>>>>",
            "<</Type/Pages/Count 0/Kids[]>>",
        ];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(b"xref\n0 3\n0000000000 65535 f \n");
        for ofs in offsets {
            out.extend_from_slice(format!("{ofs:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<</Size 3/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        let mut doc = Self::open(out)?;
        doc.created = true;
        Ok(doc)
    }

    /// `pdf_init_document`: read the header and xref, repairing if needed.
    pub fn open(data: impl Into<Arc<[u8]>>) -> Result<Document, OpenError> {
        Self::open_with(data, Limits::default())
    }

    /// `open` with explicit resource limits.
    pub fn open_with(data: impl Into<Arc<[u8]>>, limits: Limits) -> Result<Document, OpenError> {
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

        let file_size = if broken.is_none() && !is_fdf {
            data.len()
        } else {
            0
        };
        // Checked before the xref loads, so an indirect /Linearized reads 0.
        // MuPDF reports, then swallows, a parse error here.
        let linearized = broken.is_none() && !is_fdf && {
            let mut lx = Lexer::new(&data, bias, &diag);
            match parse_indirect(&mut lx) {
                Ok(ind) => ind
                    .obj
                    .as_dict()
                    .and_then(|d| d.get(b"Linearized"))
                    .is_some_and(|v| v.to_int() == 1),
                Err(e) => {
                    diag.warn("error.reported", report::syntax(&e.message));
                    false
                }
            }
        };
        let doc = Document {
            data,
            file_size,
            header_version: version,
            is_fdf,
            linearized,
            limits,
            diag,
            state: RwLock::new(Arc::new(State::empty())),
            generation: AtomicU64::new(0),
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            repair_attempted: AtomicBool::new(false),
            repair_lock: Mutex::new(()),
            crypt: RwLock::new(None),
            needs_password: AtomicBool::new(false),
            edits: RwLock::new(Edits::default()),
            edited: AtomicBool::new(false),
            created: false,
            local: RwLock::new(HashMap::new()),
            local_next: std::sync::atomic::AtomicI32::new(-1),
        };

        let loaded = if broken.is_none() && !is_fdf {
            match xref::load(&doc.data, bias, &doc.diag, &doc.limits) {
                Ok(loaded) => Some(loaded),
                Err(e) if e.starts_with(xref::LIMIT_PREFIX) => return Err(limit_error(&e)),
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
                let mut state = State::new(bias, l.entries, l.trailer, l.revisions, false);
                state.section_offsets = l.section_offsets;
                state.last_xref_was_old_style = l.last_xref_was_old_style;
                state.trailer_owner = l.trailer_owner;
                if l.revisions > 1 {
                    state.oldest_size = l.oldest_size;
                }
                for (num, obj) in l.cached {
                    state.precache(num, obj);
                }
                let trailer = state.trailer.clone();
                doc.install(state);
                doc.id_and_password(&trailer, &|o| doc.resolve(o))
                    .map_err(|e| crypt_error(&e))?;
            }
            None => {
                if let Some(e) = &broken {
                    doc.diag.warn("xref.broken", report::line(e));
                    doc.diag.warn("xref.repair", "trying to repair broken xref");
                }
                doc.repair_attempted.store(true, Ordering::SeqCst);
                match doc.repair(true) {
                    Ok(state) => doc.install(state),
                    Err(e) if e.starts_with(xref::LIMIT_PREFIX) => return Err(limit_error(&e)),
                    Err(e) if e.starts_with(CRYPT_PREFIX) => {
                        return Err(crypt_error(&e[CRYPT_PREFIX.len()..]));
                    }
                    Err(e) => {
                        return Err(OpenError {
                            code: "input.unrepairable",
                            message: report::strip(&e).to_string(),
                        });
                    }
                }
            }
        }
        Ok(doc)
    }

    /// Branches a clean, unedited view of this document in `O(1)` by sharing
    /// the immutable source bytes, parsed cross-reference [`State`], sharded
    /// object cache, and security handler, while starting with an empty edit
    /// overlay. Speculative or multi-agent edit branches can mutate their
    /// fork independently without reparsing the file or affecting readers.
    pub fn fork_clean(&self) -> Document {
        let diag = Diagnostics::new();
        diag.replay(self.diag.snapshot());
        let state = self
            .state
            .read()
            .map(|s| Arc::clone(&s))
            .unwrap_or_else(|_| Arc::new(State::empty()));
        let crypt = self.crypt.read().ok().and_then(|c| c.clone());
        Document {
            data: Arc::clone(&self.data),
            file_size: self.file_size,
            header_version: self.header_version,
            is_fdf: self.is_fdf,
            linearized: self.linearized,
            limits: self.limits,
            diag,
            state: RwLock::new(state),
            generation: AtomicU64::new(self.generation.load(Ordering::Acquire)),
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            repair_attempted: AtomicBool::new(self.repair_attempted.load(Ordering::Acquire)),
            repair_lock: Mutex::new(()),
            crypt: RwLock::new(crypt),
            needs_password: AtomicBool::new(self.needs_password.load(Ordering::Acquire)),
            edits: RwLock::new(Edits::default()),
            edited: AtomicBool::new(false),
            created: self.created,
            local: RwLock::new(HashMap::new()),
            local_next: std::sync::atomic::AtomicI32::new(-1),
        }
    }

    fn install(&self, state: State) {
        if let Ok(mut s) = self.state.write() {
            *s = Arc::new(state);
        }
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// The state objects are read from. Every object lookup asks for it, so
    /// each thread keeps it rather than have every thread lock and count
    /// the one shared copy, which on many threads cost more than the work.
    #[expect(
        clippy::redundant_allocation,
        reason = "the Rc is counted without atomics, which is why threads keep it"
    )]
    pub(crate) fn current(&self) -> Rc<Arc<State>> {
        // Read before the state, so a state installed meanwhile is seen as
        // newer next time.
        let generation = self.generation.load(Ordering::Acquire);
        CURRENT.with(|kept| {
            let mut kept = kept.borrow_mut();
            if let Some(k) = kept.as_ref()
                && k.id == self.id
                && k.generation == generation
            {
                return Rc::clone(&k.state);
            }
            let state = Rc::new(
                self.state
                    .read()
                    .map(|s| Arc::clone(&s))
                    .unwrap_or_else(|_| Arc::new(State::empty())),
            );
            *kept = Some(Kept {
                id: self.id,
                generation,
                state: Rc::clone(&state),
            });
            state
        })
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The raw (still encoded) bytes of a stream as MuPDF reads them
    /// (`pdf_load_raw_stream`): decrypted, unless a /Crypt filter in the
    /// chain does that, and read whole, so a damaged stream ends where
    /// MuPDF's read ends.
    pub fn raw_stream_data(&self, stream: &Stream) -> Result<Cow<'_, [u8]>, String> {
        if let Some(data) = self.edited_data(stream) {
            return Ok(Cow::Owned(data));
        }
        let st = self.current();
        let length = self.stream_length(&st, stream, true);
        let mut raw = self.raw_stream_in(&st, stream, length)?;
        raw.read_best(length, 0, self.limits.max_decoded_bytes)
            .map(Cow::Owned)
            .map_err(|e| DecodeError::from(e).report())
    }

    /// `pdf_stream_length`: /Length, or 0 when it is negative or beyond the
    /// file. `may_repair` allows the lookup to repair the document.
    fn stream_length(&self, st: &State, stream: &Stream, may_repair: bool) -> usize {
        let length = stream.dict.get(b"Length").unwrap_or(&Object::Null);
        let length = if may_repair {
            self.resolve(length)
        } else {
            self.resolve_in(st, length)
        }
        .to_int64();
        usize::try_from(length)
            .ok()
            .filter(|&l| l <= self.file_size)
            .unwrap_or(0)
    }

    /// `pdf_open_raw_filter`: the bytes between `stream` and `endstream`
    /// (trusting /Length, then looking for `endstream`), decrypted in an
    /// encrypted document. Fails only where MuPDF cannot open the
    /// decryption filter.
    fn raw_stream_in(
        &self,
        st: &State,
        stream: &Stream,
        length: usize,
    ) -> Result<stream::Stream<'_>, String> {
        let base = stream::Stream::new(
            stream::Endstream::new(&self.data, length, stream.data_start),
            &self.diag,
        );
        match self.crypt() {
            Some(crypt) if !self.has_crypt_filter(st, &stream.dict) => {
                let cf = crypt.stream_filter();
                let cipher = crypt.stream_cipher(&cf, stream.num, entry_generation(st, stream.num));
                filter::open_cipher(base, cipher)
            }
            _ => Ok(base),
        }
    }

    /// The decoded stream, opened as `pdf_open_stream_number` does (images
    /// decoded too).
    fn open_decoded(
        &self,
        st: &State,
        stream: &Stream,
        length: usize,
        might_be_image: bool,
    ) -> Result<stream::Stream<'_>, DecodeError> {
        let raw = self
            .raw_stream_in(st, stream, length)
            .map_err(DecodeError::Failed)?;
        let crypt = self.crypt();
        // pdf_load_jbig2_globals: the globals stream, decoded; a stream that
        // needs itself is a cycle.
        let load = |o: &Object| -> Option<Result<Vec<u8>, DecodeError>> {
            let Object::Stream(s) = self.resolve_in(st, o) else {
                return None;
            };
            let Some(_guard) = LoadGuard::enter(st, -1 - s.num) else {
                return Some(Err(DecodeError::Failed(report::format(
                    "cyclic reference when loading JBIG2 globals",
                ))));
            };
            Some(self.stream_data_in(st, &s, true))
        };
        filter::open_filters(
            raw,
            &stream.dict,
            &|o| self.resolve_in(st, o),
            Some(&load),
            crypt.as_deref().map(|crypt| filter::CryptContext {
                crypt,
                num: stream.num,
                generation: entry_generation(st, stream.num),
            }),
            might_be_image,
        )
    }

    /// `pdf_stream_has_crypt`: /Crypt among the filters.
    fn has_crypt_filter(&self, st: &State, dict: &Dict) -> bool {
        let filters = dict
            .get(b"F")
            .or_else(|| dict.get(b"Filter"))
            .map_or(Object::Null, |f| self.resolve_in(st, f));
        match &filters {
            Object::Name(_) => filters.is_name(b"Crypt"),
            Object::Array(items) => items
                .iter()
                .any(|f| self.resolve_in(st, f).is_name(b"Crypt")),
            _ => false,
        }
    }

    // ---- encryption ----------------------------------------------------

    pub(crate) fn crypt(&self) -> Option<Arc<Crypt>> {
        self.crypt.read().ok().and_then(|c| c.clone())
    }

    /// MuPDF's `id_and_password`: creates the security handler from the
    /// trailer's /Encrypt and /ID, which are loaded before it exists and so
    /// stay unencrypted, then tries the empty password.
    fn id_and_password(
        &self,
        trailer: &Dict,
        resolve: &dyn Fn(&Object) -> Object,
    ) -> Result<(), String> {
        let encrypt = trailer.get(b"Encrypt").map_or(Object::Null, resolve);
        let id = trailer.get(b"ID").map_or(Object::Null, resolve);
        let Some(dict) = encrypt.as_dict() else {
            return Ok(());
        };
        let mut crypt = Crypt::new(dict, &id, resolve, &self.diag)?;
        let access = crypt.authenticate("")?;
        self.needs_password.store(access == 0, Ordering::SeqCst);
        if let Ok(mut c) = self.crypt.write() {
            *c = Some(Arc::new(crypt));
        }
        Ok(())
    }

    /// Whether the document needs a password: it is encrypted and the empty
    /// password opens it neither as user nor as owner.
    pub fn needs_password(&self) -> bool {
        self.needs_password.load(Ordering::SeqCst)
    }

    /// `pdf_authenticate_password`: returns the access granted (0 none,
    /// 2 user, 4 owner, 6 both) and keeps the key the attempt computed,
    /// right or wrong, as MuPDF does. Objects already loaded stay as they
    /// were decrypted. Unencrypted documents need no password and return 1.
    pub fn authenticate(&self, password: &str) -> Result<u8, String> {
        let Some(current) = self.crypt() else {
            return Ok(1);
        };
        let mut crypt = (*current).clone();
        let access = crypt.authenticate(password)?;
        if let Ok(mut c) = self.crypt.write() {
            *c = Some(Arc::new(crypt));
        }
        Ok(access)
    }

    /// `pdf_has_permission`: everything is allowed without a security
    /// handler or with the owner password; otherwise /P decides. The
    /// permissions are MuPDF's names: print, edit, copy, annotate, form,
    /// accessibility, assemble, print-hq.
    pub fn has_permission(&self, permission: &str) -> bool {
        let Some(crypt) = self.crypt() else {
            return true;
        };
        if crypt.access() & 4 != 0 {
            return true;
        }
        let bit = match permission {
            "print" => 1 << 2,
            "edit" => 1 << 3,
            "copy" => 1 << 4,
            "annotate" => 1 << 5,
            "form" => 1 << 8,
            "accessibility" => 1 << 9,
            "assemble" => 1 << 10,
            "print-hq" => 1 << 11,
            _ => return true,
        };
        crypt.permissions() & bit != 0
    }

    /// MuPDF's encryption description, such as "Standard V4 R4 128-bit
    /// AES", or "None".
    pub fn encryption(&self) -> String {
        self.crypt()
            .map_or_else(|| "None".to_string(), |c| c.description())
    }

    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diag
    }

    /// The same file opened again, as it was before any repair or load, with
    /// this document's security handler (so an accepted password carries
    /// over). `None` for documents made or changed in memory, which have
    /// no file to go back to.
    pub fn reopen(&self) -> Option<Document> {
        if self.created || self.edited.load(Ordering::SeqCst) {
            return None;
        }
        let doc = Self::open_with(self.data.clone(), self.limits).ok()?;
        if let (Ok(mut c), Some(current)) = (doc.crypt.write(), self.crypt()) {
            *c = Some(current);
        }
        Some(doc)
    }

    pub fn trailer(&self) -> Dict {
        if let Some(t) = self.with_edits(|e| e.trailer.clone()).flatten() {
            return t;
        }
        self.current().trailer.clone()
    }

    /// Number of slots in the xref (MuPDF's `pdf_xref_len`).
    pub fn xref_len(&self) -> usize {
        let len = self.current().xref_len();
        self.with_edits(|e| e.len.max(len)).unwrap_or(len)
    }

    /// `f` on the edits, when there are any.
    pub(crate) fn with_edits<T>(&self, f: impl FnOnce(&Edits) -> T) -> Option<T> {
        if !self.edited.load(Ordering::Acquire) {
            return None;
        }
        self.edits.read().ok().map(|e| f(&e))
    }

    fn edit_mut<T>(&self, f: impl FnOnce(&mut Edits) -> T) -> Option<T> {
        let first = !self.edited.load(Ordering::Acquire);
        // MuPDF rewrote /Count in the object when the page tree had fewer
        // pages; from now on objects say so themselves.
        let count = if first {
            self.current()
                .page_count_override
                .get()
                .copied()
                .and_then(|n| self.pages_with_count(n))
        } else {
            None
        };
        let mut e = self.edits.write().ok()?;
        if e.len == 0 {
            e.len = self.current().xref_len();
        }
        if let Some((num, pages, generation)) = count {
            e.objects.insert(num, pages);
            e.gens.entry(num).or_insert(generation);
        }
        // MuPDF drops its page map when an object changes.
        e.page_map = None;
        let out = f(&mut e);
        self.edited.store(true, Ordering::Release);
        Some(out)
    }

    /// `pdf_update_object`: object `num` is `obj` from now on, in a new
    /// xref entry of generation 0 (unless an earlier change in place moved
    /// the entry already).
    pub fn update_object(&self, num: i32, obj: Object) {
        self.edit_mut(|e| {
            e.objects.insert(num, obj);
            e.gens.entry(num).or_insert(0);
            if let Ok(n) = usize::try_from(num) {
                e.len = e.len.max(n + 1);
            }
        });
    }

    /// Object `num` changed in place, as `pdf_dict_put` and its kin change
    /// it: its xref entry keeps its generation, but for an object from an
    /// object stream, whose generation becomes 0.
    pub fn alter_object(&self, num: i32, obj: Object) {
        let entry = self.xref_entry(num);
        let generation = match entry.kind {
            Kind::InStream => 0,
            _ => entry.generation,
        };
        self.edit_mut(|e| {
            e.objects.insert(num, obj);
            e.gens.entry(num).or_insert(generation);
            if let Ok(n) = usize::try_from(num) {
                e.len = e.len.max(n + 1);
            }
        });
    }

    /// `pdf_add_object`: `obj` as a new object after the others, returning
    /// its number.
    pub fn add_object(&self, obj: Object) -> i32 {
        self.edit_mut(|e| {
            let num = e.len as i32;
            e.objects.insert(num, obj);
            e.gens.insert(num, 0);
            e.len += 1;
            num
        })
        .unwrap_or(0)
    }

    /// A new stream object with `dict` and `data`, returning its number.
    pub fn add_stream(&self, dict: Dict, data: Vec<u8>) -> i32 {
        self.edit_mut(|e| {
            let num = e.len as i32;
            let stream = Stream {
                dict,
                data_start: 0,
                num,
            };
            e.objects.insert(num, Object::Stream(Arc::new(stream)));
            e.gens.insert(num, 0);
            e.data.insert(num, data);
            e.len += 1;
            num
        })
        .unwrap_or(0)
    }

    /// `pdf_create_object`: a new object number, its entry free until
    /// `update_object` fills it.
    pub fn create_object(&self) -> i32 {
        self.edit_mut(|e| {
            let num = e.len as i32;
            e.gens.insert(num, 0);
            e.len += 1;
            num
        })
        .unwrap_or(0)
    }

    /// `pdf_delete_object`: object `num` is free from now on. Its entry in
    /// MuPDF's incremental section is free with the next generation when
    /// the file had the object; when the file had it free, or never had
    /// it, the entry is unset again, so the file's own entry shows.
    pub fn delete_object(&self, num: i32) {
        if num <= 0 || num as usize >= self.xref_len() {
            self.diag.warn(
                "object.out_of_range",
                format!(
                    "object out of range ({num} 0 R); xref size {}",
                    self.xref_len()
                ),
            );
            return;
        }
        let st = self.current();
        let in_file = (num as usize) < st.xref_len()
            && !matches!(st.entries.get(num).kind, Kind::None | Kind::Free);
        self.edit_mut(|e| {
            e.objects.remove(&num);
            e.data.remove(&num);
            if in_file {
                let generation = e.gens.get(&num).copied().unwrap_or(0);
                e.gens.insert(num, generation + 1);
            } else {
                e.gens.remove(&num);
            }
        });
    }

    /// `pdf_update_stream` with data already encoded: object `num`, a
    /// dictionary or a stream, becomes a stream with `data` and a /Length
    /// to match. Its filters stay as they are.
    pub fn update_stream(&self, num: i32, data: Vec<u8>) {
        let Some(obj) = self.edited_object(num).or_else(|| self.object(num)) else {
            return;
        };
        let mut dict = match obj {
            Object::Dict(d) => (*d).clone(),
            Object::Stream(s) => s.dict.clone(),
            _ => return,
        };
        dict.put(b"Length", Object::Int(data.len() as i64));
        self.edit_mut(|e| {
            let stream = Stream {
                dict,
                data_start: 0,
                num,
            };
            e.objects.insert(num, Object::Stream(Arc::new(stream)));
            e.gens.entry(num).or_insert(0);
            e.data.insert(num, data);
        });
    }

    /// Object `num` becomes a stream with `dict` and `data` (already as it
    /// is to be stored), changed in place as `alter_object` changes it.
    pub fn replace_stream(&self, num: i32, dict: Dict, data: Vec<u8>) {
        let entry = self.xref_entry(num);
        let generation = match entry.kind {
            Kind::InStream => 0,
            _ => entry.generation,
        };
        self.edit_mut(|e| {
            let stream = Stream {
                dict,
                data_start: 0,
                num,
            };
            e.objects.insert(num, Object::Stream(Arc::new(stream)));
            e.gens.entry(num).or_insert(generation);
            e.data.insert(num, data);
            if let Ok(n) = usize::try_from(num) {
                e.len = e.len.max(n + 1);
            }
        });
    }

    /// A stream of the local xref (`pdf_new_local_xref`'s objects): made
    /// to be drawn and never part of the document, nor of what it writes.
    /// It is a direct stream object with a negative number; it goes when
    /// the [`LocalStream`] is dropped.
    pub fn add_local_stream(&self, dict: Dict, data: Vec<u8>) -> LocalStream<'_> {
        let num = self.local_next.fetch_sub(1, Ordering::Relaxed);
        if let Ok(mut l) = self.local.write() {
            l.insert(num, data);
        }
        let stream = Stream {
            dict,
            data_start: 0,
            num,
        };
        LocalStream {
            doc: self,
            obj: Object::Stream(Arc::new(stream)),
            num,
        }
    }

    /// Replaces the trailer.
    pub fn set_trailer(&self, trailer: Dict) {
        self.edit_mut(|e| e.trailer = Some(trailer));
    }

    /// The object holding /Root/Pages, what it is with /Count `n`, and its
    /// generation.
    fn pages_with_count(&self, n: i32) -> Option<(i32, Object, i32)> {
        let root = self.trailer().get(b"Root").cloned()?;
        let Some(Object::Ref(r)) = self.dict_get_raw(&root, b"Pages") else {
            return None;
        };
        let Object::Dict(d) = self.resolve(&Object::Ref(r)) else {
            return None;
        };
        let mut d = (*d).clone();
        d.put(b"Count", Object::Int(i64::from(n)));
        let entry = self.xref_entry(r.num);
        let generation = if entry.kind == Kind::InStream {
            0
        } else {
            entry.generation
        };
        Some((r.num, Object::Dict(Arc::new(d)), generation))
    }

    /// The value of `key` in the dictionary `obj` leads to, not resolved.
    fn dict_get_raw(&self, obj: &Object, key: &[u8]) -> Option<Object> {
        match self.resolve(obj) {
            Object::Dict(d) => d.get(key).cloned(),
            Object::Stream(s) => s.dict.get(key).cloned(),
            _ => None,
        }
    }

    /// MuPDF's /Count rewrite while editing: the object changes.
    pub(crate) fn rewrite_page_count(&self, n: i32) {
        if let Some((num, pages, _)) = self.pages_with_count(n) {
            self.alter_object(num, pages);
        }
    }

    /// What loading a page does to a page tree that claims more pages than
    /// it has, in MuPDF: /Count is rewritten in the document, edited or not
    /// (`pdf_load_page_tree_internal`). Elsewhere nupdf keeps the count to
    /// itself until the first edit, as MuPDF's commands that load no page
    /// see the object unchanged.
    pub fn settle_page_count(&self) {
        if let Some(&n) = self.current().page_count_override.get() {
            self.rewrite_page_count(n);
        }
    }

    /// Keeps the page map until the next edit.
    pub(crate) fn cache_page_map(&self, map: Option<crate::page::PageMap>) {
        if let Ok(mut e) = self.edits.write() {
            e.page_map = Some(map);
        }
    }

    /// Whether anything was changed since opening.
    pub fn is_edited(&self) -> bool {
        self.edited.load(Ordering::Acquire)
    }

    /// The replaced or new object `num`, if the edits have one.
    fn edited_object(&self, num: i32) -> Option<Object> {
        self.with_edits(|e| e.objects.get(&num).cloned()).flatten()
    }

    /// The data of a stream the edits made, or of a local one.
    fn edited_data(&self, stream: &Stream) -> Option<Vec<u8>> {
        // A local stream that is gone reads as empty, never as the file.
        if stream.num < 0 {
            return Some(
                self.local
                    .read()
                    .ok()
                    .and_then(|l| l.get(&stream.num).cloned())
                    .unwrap_or_default(),
            );
        }
        self.with_edits(|e| e.data.get(&stream.num).cloned())
            .flatten()
    }

    /// Whether the xref had to be rebuilt by scanning the file.
    pub fn repaired(&self) -> bool {
        self.current().repaired
    }

    /// Saved versions of the document, as `pdf_count_versions` counts
    /// them: xref sections (one after a repair), less the extra section of
    /// a linearized file.
    pub fn revisions(&self) -> usize {
        let s = self.current();
        let sections = if s.repaired { 1 } else { s.revisions };
        sections.saturating_sub(usize::from(self.linearized))
    }

    /// Whether `num` is an object stream that failed to load, whose entry
    /// MuPDF has marked free.
    pub(crate) fn is_demoted(&self, num: i32) -> bool {
        self.current().is_demoted(num)
    }

    /// Whether MuPDF's `pdf_get_xref_entry` finds an entry for `num`, which
    /// no section defines, once the save has made the newest section
    /// solid: always with one section, and with several only below the
    /// oldest section's size. Loading such an entry fails.
    pub(crate) fn has_unset_entry(&self, num: i32) -> bool {
        let s = self.current();
        let sections = if s.repaired { 1 } else { s.revisions };
        sections <= 1 || num < s.oldest_size
    }

    /// Absolute file offsets of the xref sections, newest first: one per
    /// saved revision, plus the first-page section of a linearized file.
    /// Empty when the xref was rebuilt by repair, since revisions are then
    /// unknown. Redaction uses these to drop every earlier revision.
    pub fn revision_offsets(&self) -> Vec<u64> {
        let state = self.current();
        state
            .section_offsets
            .iter()
            .filter_map(|&o| u64::try_from(o.saturating_add(state.bias as i64)).ok())
            .collect()
    }

    /// Exact byte end offsets (`1..=data.len()`) of each saved revision in
    /// chronological order (`rev 1` = oldest, ..., `rev N` = `self.data.len()`).
    ///
    /// Falls back gracefully to `[self.data.len()]` when the document has at
    /// most one revision or its cross-reference table was rebuilt by repair.
    pub fn revision_byte_ends(&self) -> Vec<usize> {
        let total_len = self.data.len();
        if total_len == 0 {
            return Vec::new();
        }
        let state = self.current();
        if state.repaired || self.revisions() <= 1 || state.section_offsets.len() <= 1 {
            return vec![total_len];
        }

        let mut abs_offsets: Vec<usize> = state
            .section_offsets
            .iter()
            .rev()
            .filter_map(|&o| usize::try_from(o.saturating_add(state.bias as i64)).ok())
            .filter(|&o| o < total_len)
            .collect();

        // A linearized file's first-page xref sits near the top of the file;
        // drop that smallest offset so revision 1 ends after the main xref.
        if self.linearized
            && abs_offsets.len() > 1
            && let Some((min_idx, _)) = abs_offsets.iter().enumerate().min_by_key(|&(_, &o)| o)
        {
            abs_offsets.remove(min_idx);
        }

        if abs_offsets.len() <= 1 {
            return vec![total_len];
        }

        let mut ends: Vec<usize> = Vec::with_capacity(abs_offsets.len());
        for i in 0..abs_offsets.len().saturating_sub(1) {
            let abs_ofs = abs_offsets[i];
            let next_ofs = abs_offsets[i + 1];
            let upper = if next_ofs > abs_ofs {
                next_ofs.min(total_len)
            } else {
                total_len
            };
            if let Some(end) = find_revision_eof_end(&self.data, abs_ofs, upper)
                && end > 0
                && end < total_len
                && ends.last().is_none_or(|&prev| end > prev)
            {
                ends.push(end);
            }
        }
        ends.push(total_len);
        ends
    }

    /// Returns the raw file byte prefix `&self.data[..end]` for 1-based
    /// historical revision `rev_1based` (`1..=revision_byte_ends().len()`).
    pub fn revision_bytes(&self, rev_1based: usize) -> Result<&[u8], OpenError> {
        let ends = self.revision_byte_ends();
        if rev_1based == 0 || rev_1based > ends.len() {
            return Err(OpenError {
                code: "usage.revision_out_of_range",
                message: format!("revision {rev_1based} out of range (1..={})", ends.len()),
            });
        }
        let end = ends[rev_1based - 1];
        self.data.get(..end).ok_or_else(|| OpenError {
            code: "input.truncated",
            message: format!("revision {rev_1based} byte end {end} exceeds file length"),
        })
    }

    /// Opens a standalone [`Document`] at 1-based historical revision `rev_1based`.
    ///
    /// For the final revision of an unedited document, branches an `O(1)` view via
    /// [`Document::fork_clean`]; for historical revisions `1..N`, slices `&self.data[..end]`
    /// and opens it with this document's [`Limits`].
    pub fn open_revision(&self, rev_1based: usize) -> Result<Document, OpenError> {
        let ends = self.revision_byte_ends();
        if rev_1based == 0 || rev_1based > ends.len() {
            return Err(OpenError {
                code: "usage.revision_out_of_range",
                message: format!("revision {rev_1based} out of range (1..={})", ends.len()),
            });
        }
        if rev_1based == ends.len() && !self.is_edited() {
            return Ok(self.fork_clean());
        }
        let end = ends[rev_1based - 1];
        let slice = self.data.get(..end).ok_or_else(|| OpenError {
            code: "input.truncated",
            message: format!("revision {rev_1based} byte end {end} exceeds file length"),
        })?;
        let rev_doc = Self::open_with(Arc::<[u8]>::from(slice), self.limits)?;
        if rev_doc.needs_password()
            && !self.needs_password()
            && let (Ok(mut c), Some(current)) = (rev_doc.crypt.write(), self.crypt())
        {
            *c = Some(current);
            rev_doc.needs_password.store(false, Ordering::SeqCst);
        }
        Ok(rev_doc)
    }

    /// MuPDF's `doc->startxref`: where the newest xref section starts, as
    /// the file's `startxref` gives it; 0 after a repair.
    pub(crate) fn startxref(&self) -> i64 {
        self.current().section_offsets.first().copied().unwrap_or(0)
    }

    /// The object a first edit to the trailer moves into MuPDF's
    /// incremental section: the xref stream whose dictionary the trailer
    /// is, until edits begin (MuPDF's trailer is a copy from then on).
    pub(crate) fn trailer_owner(&self) -> Option<i32> {
        if self.is_edited() {
            return None;
        }
        self.current().trailer_owner
    }

    /// `doc->last_xref_was_old_style`.
    pub(crate) fn last_xref_was_table(&self) -> bool {
        self.current().last_xref_was_old_style
    }

    /// Bytes before the header, which offsets in the file do not count.
    pub(crate) fn bias(&self) -> usize {
        self.current().bias
    }

    /// MuPDF's `doc->file_size`: the bytes an incremental save copies.
    pub(crate) fn file_size(&self) -> usize {
        self.file_size
    }

    /// Whether `create` made it, so there is no file to add to.
    pub(crate) fn is_new(&self) -> bool {
        self.created
    }

    /// The entries of MuPDF's incremental section, in number order: each
    /// object number, whether it holds an object (or is free), and its
    /// generation.
    pub(crate) fn incremental_entries(&self) -> Vec<(i32, bool, i32)> {
        let mut out: Vec<(i32, bool, i32)> = self
            .with_edits(|e| {
                e.gens
                    .iter()
                    .map(|(&num, &g)| (num, e.objects.contains_key(&num), g))
                    .collect()
            })
            .unwrap_or_default();
        out.sort_unstable();
        out
    }

    /// Whether the file starts with a linearization dictionary.
    pub fn linearized(&self) -> bool {
        self.linearized
    }

    /// An Info dictionary entry as text, like `fz_lookup_metadata` with an
    /// `info:` key: `None` when missing, not a string, or empty.
    pub fn metadata(&self, key: &str) -> Option<String> {
        let info = self.trailer().get(b"Info").cloned()?;
        let value = self.dict_get(&info, key.as_bytes());
        let Object::String(s) = value else {
            return None;
        };
        let text = crate::text_string::text_string(&s);
        (!text.is_empty()).then_some(text)
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
                Loaded::Obj(o) => return Some(demote_if_failed(&state, num, o)),
                Loaded::Missing => return None,
                Loaded::Failed(cause) => {
                    self.report(&cause);
                    self.diag.warn(
                        "object.unloadable",
                        format!("cannot load object ({num} 0 R) into cache"),
                    );
                    return None;
                }
                Loaded::NeedsRepair(cause) => {
                    self.report(&cause);
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

    /// `pdf_load_unencrypted_object`: object `num` parsed from the file as
    /// it is there, not decrypted and not cached; None for an object that
    /// is not in the file itself (in an object stream, free, missing).
    pub fn load_unencrypted(&self, num: i32) -> Option<Object> {
        let st = self.current();
        if num <= 0 || num as usize >= st.xref_len() || st.entries.get(num).kind != Kind::InFile {
            return None;
        }
        match self.load_slot(&st, num, false) {
            Loaded::Obj(o) => Some(o),
            _ => None,
        }
    }

    /// `pdf_load_object` for the writer: `Err` where MuPDF throws (the
    /// cause), `Ok(None)` where it loads nothing without an error.
    pub(crate) fn load_object(&self, num: i32) -> Result<Option<Object>, String> {
        loop {
            let state = self.current();
            match self.load_in(&state, num, true) {
                Loaded::Obj(o) => return Ok(Some(demote_if_failed(&state, num, o))),
                Loaded::Missing => return Ok(None),
                Loaded::Failed(cause) => return Err(cause),
                Loaded::NeedsRepair(cause) => {
                    self.report(&cause);
                    if !self.repair_on_demand(&state) {
                        return Err(report::format(&format!("cannot load object ({num} 0 R)")));
                    }
                }
            }
        }
    }

    /// Entry `num` of the current xref.
    pub(crate) fn xref_entry(&self, num: i32) -> xref::Entry {
        // An edited object's entry is in MuPDF's incremental section; one
        // created but not filled yet is free there.
        let edited = self
            .with_edits(|e| e.gens.get(&num).map(|g| (*g, e.objects.contains_key(&num))))
            .flatten();
        if let Some((generation, filled)) = edited {
            return xref::Entry {
                kind: if filled { Kind::InFile } else { Kind::Free },
                generation,
                ofs: 0,
            };
        }
        let st = self.current();
        if num >= 0 && (num as usize) < st.xref_len() {
            st.entries.get(num)
        } else {
            xref::Entry::NONE
        }
    }

    /// `doc->repair_attempted`.
    pub(crate) fn repair_attempted(&self) -> bool {
        self.attempted()
    }

    /// The limits it was opened with.
    pub fn limits(&self) -> Limits {
        self.limits
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
        let count = match self.current().page_count_override.get() {
            // Edits carry the rewritten /Count in the object.
            Some(&n) if !self.is_edited() => n,
            _ => {
                let root = self.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
                let pages = self.dict_get(&root, b"Pages");
                self.dict_get(&pages, b"Count").to_int()
            }
        };
        if count < 0 || i64::from(count) >= self.xref_len() as i64 {
            return Err(PageCountError("Invalid number of pages".into()));
        }
        Ok(count)
    }

    /// Decoded data of a stream object, for non-image use.
    pub fn stream_data(&self, stream: &Stream) -> Result<Vec<u8>, DecodeError> {
        // The /Length lookup may repair the document, as in MuPDF.
        self.stream_data_in(&self.current(), stream, true)
    }

    /// An image's samples as `fz_decomp_image_from_stream` reads them: the
    /// stream through all its filters, read up to the `len` bytes the image
    /// needs (`fz_read`). MuPDF keeps the image filter for that read, so no
    /// compression-bomb guard applies to it.
    pub fn image_samples(&self, stream: &Stream, len: usize) -> Result<Vec<u8>, DecodeError> {
        let st = self.current();
        if let Some(data) = self.edited_data(stream) {
            let resolve = |o: &Object| self.resolve_in(&st, o);
            let mut s = filter::open_filters(
                stream::Stream::memory(&data, &self.diag),
                &stream.dict,
                &resolve,
                None,
                None,
                true,
            )?;
            let mut out = vec![0; len];
            let n = s.read(&mut out);
            out.truncate(n);
            return Ok(out);
        }
        let length = self.stream_length(&st, stream, true);
        let mut s = self.open_decoded(&st, stream, length, true)?;
        let mut out = vec![0; len];
        let n = s.read(&mut out);
        out.truncate(n);
        Ok(out)
    }

    /// A stream read as `pdf_open_stream` opens it and the lexer or a bit
    /// reader takes it, a byte at a time: for functions and the like.
    pub fn stream_bytes(&self, stream: &Stream) -> Result<Vec<u8>, DecodeError> {
        let st = self.current();
        if let Some(data) = self.edited_data(stream) {
            let resolve = |o: &Object| self.resolve_in(&st, o);
            return filter::decode(&stream.dict, &data, &resolve, &self.diag, &self.limits);
        }
        let length = self.stream_length(&st, stream, true);
        let mut s = self.open_decoded(&st, stream, length, true)?;
        Ok(s.read_bytewise(self.limits.max_decoded_bytes)?)
    }

    /// A content stream as MuPDF's interpreter reads it: through
    /// `pdf_open_image_stream` with image filters refused, a byte at a time
    /// (`fz_read_byte`). Also whether the read ended in an error, which
    /// stops a contents array there (`fz_open_concat`).
    pub fn content_stream_data(&self, stream: &Stream) -> Result<(Vec<u8>, bool), DecodeError> {
        let st = self.current();
        if let Some(data) = self.edited_data(stream) {
            let resolve = |o: &Object| self.resolve_in(&st, o);
            return filter::decode(&stream.dict, &data, &resolve, &self.diag, &self.limits)
                .map(|d| (d, false));
        }
        let length = self.stream_length(&st, stream, true);
        let mut s = self.open_decoded(&st, stream, length, false)?;
        let data = s.read_bytewise(self.limits.max_decoded_bytes)?;
        Ok((data, s.had_error()))
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
            Loaded::Failed(cause) | Loaded::NeedsRepair(cause) => {
                self.report(&cause);
                self.diag.warn(
                    "object.unloadable",
                    format!("cannot load object ({} 0 R) into cache", r.num),
                );
                Object::Null
            }
        }
    }

    /// Records the error lines MuPDF prints when it catches a failure.
    fn report(&self, cause: &str) {
        for line in cause.lines().filter(|l| !l.is_empty()) {
            self.diag.warn("error.reported", line);
        }
    }

    /// `pdf_load_stream`: decoded and read whole (`fz_read_best`), with
    /// MuPDF's compression bomb check.
    fn stream_data_in(
        &self,
        st: &State,
        stream: &Stream,
        may_repair: bool,
    ) -> Result<Vec<u8>, DecodeError> {
        // A stream the edits made: its data in memory, through its filters.
        if let Some(data) = self.edited_data(stream) {
            let resolve = |o: &Object| self.resolve_in(st, o);
            return filter::decode(&stream.dict, &data, &resolve, &self.diag, &self.limits);
        }
        let length = self.stream_length(st, stream, may_repair);
        let mut s = self.open_decoded(st, stream, length, true)?;
        let resolve = |o: &Object| self.resolve_in(st, o);
        let initial = filter::guess_length(length, &stream.dict, &resolve);
        let worst = filter::worst_case(&stream.dict, initial, &resolve);
        Ok(s.read_best(initial, worst, self.limits.max_decoded_bytes)?)
    }

    /// The decoded stream read a byte at a time, as MuPDF's lexer reads
    /// object streams.
    fn stream_bytes_in(&self, st: &State, stream: &Stream) -> Result<Vec<u8>, DecodeError> {
        let length = self.stream_length(st, stream, false);
        let mut s = self.open_decoded(st, stream, length, true)?;
        Ok(s.read_bytewise(self.limits.max_decoded_bytes)?)
    }

    /// `pdf_cache_object` against one state.
    fn load_in(&self, st: &State, num: i32, allow_repair: bool) -> Loaded {
        if num > 0 {
            // An entry the edits made free (created and not filled yet, or
            // deleted) loads as a free entry does.
            let edited = self
                .with_edits(|e| (e.objects.get(&num).cloned(), e.gens.contains_key(&num)))
                .unwrap_or((None, false));
            match edited {
                (Some(obj), _) => return Loaded::Obj(obj),
                (None, true) => return Loaded::Obj(Object::Null),
                (None, false) => {}
            }
        }
        if num <= 0 || num as usize >= st.xref_len() {
            return Loaded::Missing;
        }
        // Free and unset slots depend only on the entry; caching them would
        // cost memory per declared slot, which a hostile file sets freely.
        if matches!(st.entries.get(num).kind, Kind::Free | Kind::None) {
            return self.load_slot(st, num, allow_repair);
        }
        if let Some(v) = st.cache.get(num) {
            return v;
        }
        let Some(_guard) = LoadGuard::enter(st, num) else {
            // MuPDF marks a member whose object stream is being read, and
            // a lookup of it meanwhile finds no usable entry.
            let message = if st.entries.get(num).kind == Kind::InStream {
                format!("cannot find object in xref ({num} 0 R)")
            } else {
                format!("recursive object lookup ({num} 0 R)")
            };
            return Loaded::Failed(report::format(&message));
        };
        let loaded = match (self.load_slot(st, num, allow_repair), self.crypt()) {
            // pdf_cache_object decrypts what it parsed from the file. If AES
            // fails partway, the object stays cached as far as it got and
            // this lookup reports the error.
            (Loaded::Obj(obj), Some(crypt)) if st.entries.get(num).kind == Kind::InFile => {
                let (obj, error) =
                    crypt.decrypt_object(obj, num, st.entries.get(num).generation, &self.diag);
                if let Some(e) = error {
                    st.cache.insert_if_absent(num, Loaded::Obj(obj));
                    return Loaded::Failed(e);
                }
                Loaded::Obj(obj)
            }
            (loaded, _) => loaded,
        };
        // MuPDF keeps no object for a member of a failed object stream, so
        // every lookup goes back to the stream (and reports it again).
        if matches!(loaded, Loaded::Failed(_)) && st.entries.get(num).kind == Kind::InStream {
            return loaded;
        }
        st.cache.insert_if_absent(num, loaded)
    }

    /// Loads an in-file object without decrypting it, as MuPDF does with
    /// /Encrypt and /ID after a repair, and caches it.
    fn precache_unencrypted(&self, st: &State, obj: Option<&Object>) {
        let Some(Object::Ref(r)) = obj else {
            return;
        };
        let entry = st.entries.get(r.num);
        if entry.kind != Kind::InFile || st.cache.get(r.num).is_some() {
            return;
        }
        if let Loaded::Obj(o) = self.load_slot(st, r.num, false) {
            st.precache(r.num, o);
        }
    }

    fn parse_at(&self, st: &State, ofs: i64) -> Result<IndirectObject, IndirectError> {
        let at = usize::try_from(ofs)
            .unwrap_or(usize::MAX)
            .saturating_add(st.bias);
        let mut lx = Lexer::new(&self.data, at, &self.diag);
        parse_indirect(&mut lx)
    }

    fn load_slot(&self, st: &State, num: i32, allow_repair: bool) -> Loaded {
        let entry = st.entries.get(num);
        let can_repair = allow_repair && !self.attempted();
        match entry.kind {
            Kind::Free => Loaded::Obj(Object::Null),
            Kind::None => Loaded::Failed(report::format(&format!(
                "cannot find object in xref ({num} 0 R)"
            ))),
            Kind::InFile => match self.parse_at(st, entry.ofs) {
                Ok(ind) if ind.num == num => Loaded::Obj(into_object(ind)),
                Ok(_) => {
                    if can_repair {
                        // MuPDF repairs silently when the number is wrong.
                        Loaded::NeedsRepair(String::new())
                    } else {
                        Loaded::Missing
                    }
                }
                Err(e) if e.try_repair => {
                    if can_repair {
                        Loaded::NeedsRepair(report::syntax(&e.message))
                    } else {
                        Loaded::Failed(
                            [
                                report::syntax(&e.message),
                                report::format("Repair failed already - not trying again"),
                                report::format(&format!("cannot parse object ({num} 0 R)")),
                            ]
                            .join("\n"),
                        )
                    }
                }
                Err(e) => Loaded::Failed(report::syntax(&e.message)),
            },
            Kind::InStream => {
                let objstm = self.objstm(st, entry.ofs as i32);
                if let Some((_, obj)) = objstm.objects.iter().find(|(n, _)| *n == num) {
                    return Loaded::Obj(obj.clone());
                }
                if let Some(e) = &objstm.error {
                    return Loaded::Failed(e.clone());
                }
                // pdf_load_obj_stm found no such member: MuPDF throws, with
                // no repair, at every lookup.
                Loaded::Failed(report::format(&format!(
                    "cannot load object stream containing object ({num} 0 R)"
                )))
            }
        }
    }

    /// `pdf_load_obj_stm`, cached per state.
    fn objstm(&self, st: &State, stm: i32) -> Arc<ObjStm> {
        if let Some(found) = st.objstms.lock().ok().and_then(|m| m.get(&stm).cloned()) {
            // After a failure MuPDF marks the stream's entry free, so later
            // lookups give up before reading it, with a shorter message.
            // The members parsed before the failure stay cached in their
            // entries.
            if found.error.is_some() {
                return Arc::new(ObjStm {
                    objects: found.objects.clone(),
                    error: Some(report::format(&format!("corrupt object stream {stm}"))),
                });
            }
            return found;
        }
        let computed = Arc::new(self.load_objstm(st, stm));
        if let Some(error) = &computed.error {
            if let Ok(mut demoted) = st.demoted.lock() {
                demoted.insert(stm);
                st.any_demoted.store(true, Ordering::Release);
            }
            // The lookup that read the stream fails, even for a member
            // parsed before the error (pdf_load_obj_stm throws); the
            // members it parsed are kept for the lookups after it.
            let failed = Arc::new(ObjStm {
                objects: Vec::new(),
                error: Some(error.clone()),
            });
            if let Ok(mut map) = st.objstms.lock() {
                map.entry(stm).or_insert(computed);
            }
            return failed;
        }
        if let Ok(mut map) = st.objstms.lock() {
            return Arc::clone(map.entry(stm).or_insert(computed));
        }
        computed
    }

    fn load_objstm(&self, st: &State, stm: i32) -> ObjStm {
        let fail = |e: String| ObjStm {
            objects: Vec::new(),
            error: Some(report::line(&e)),
        };
        let Some(_guard) = LoadGuard::enter(st, -1 - stm) else {
            return fail("recursive object stream lookup".into());
        };
        let xref_len = st.xref_len() as i64;
        if stm <= 0 || i64::from(stm) >= xref_len {
            return fail(format!("corrupt object stream {stm}"));
        }
        if st.entries.get(stm).kind == Kind::Free {
            return fail(format!("corrupt object stream {stm}"));
        }
        let stream = match self.load_in(st, stm, true) {
            Loaded::Obj(Object::Stream(s)) => s,
            Loaded::Obj(_) => return fail("object is not a stream".into()),
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
        if count as usize > self.limits.max_objstm_objects {
            self.diag.warn(
                "limit.objstm_objects",
                format!("object stream {stm} declares {count} objects"),
            );
            return fail(format!("object stream {stm} exceeds the object limit"));
        }
        let data = match self.stream_bytes_in(st, &stream) {
            Ok(d) => d,
            Err(e) => return fail(e.report()),
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
                        error: Some(report::syntax(&e.0)),
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
        match self.repair(false) {
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
    fn repair(&self, at_open: bool) -> Result<State, String> {
        if !self.is_fdf {
            self.diag.warn("xref.repairing", "repairing PDF document");
        }
        let scan = repair::scan(&self.data, &self.diag)?;
        let encrypted = scan.encrypt.is_some();

        let len = scan.maxnum as usize + 1;
        if len > self.limits.max_xref_entries {
            return Err(format!(
                "{}repaired xref has {len} entries",
                xref::LIMIT_PREFIX
            ));
        }
        // Every slot below maxnum starts free; found objects then fill theirs.
        // MuPDF then chains the free entries, which takes each generation
        // from 0 to 1: filled that way from the start, a file that names a
        // huge object number keeps a sparse table instead of one entry per
        // number.
        let mut entries = Slots::filled(len, Entry::free(1));
        let mut fixed: HashMap<i32, Object> = HashMap::new();
        // The scan's last stream offset per number picks object streams.
        let mut stream_at: HashMap<i32, i64> = HashMap::new();
        let parse_state = State::empty();
        for item in &scan.list {
            entries.set(
                item.num,
                Entry {
                    kind: Kind::InFile,
                    ofs: item.ofs,
                    generation: item.generation,
                },
            );
            stream_at.insert(item.num, item.stm_ofs);
            // MuPDF reads stream data from the entry's offset, which tracks the
            // latest definition, even when it keeps an older cached object.
            if let Some(obj) = fixed.remove(&item.num) {
                fixed.insert(item.num, with_data_start(obj, item.num, item.stm_ofs));
            }
            // Correct stream lengths measured by the scan (unencrypted only).
            // MuPDF patches the object it has cached for this number, even if
            // that came from an earlier definition.
            if !encrypted && item.stm_len >= 0 {
                let obj = match fixed.remove(&item.num) {
                    Some(o) => o,
                    None => match self.parse_at(&parse_state, item.ofs) {
                        Ok(ind) if ind.num == item.num => into_object(ind),
                        Ok(_) => return Err(format!("not a dict ({} 0 R)", item.num)),
                        Err(_) => return Err(format!("cannot parse object ({} 0 R)", item.num)),
                    },
                };
                let obj = with_length(obj, item.stm_len).ok_or_else(|| "not a dict".to_string())?;
                fixed.insert(item.num, obj);
            }
        }
        entries.set(0, Entry::free(65535));

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
        // While opening, the security handler is created here, after the
        // scan and before object streams are read (pdf_repair_xref_aux).
        if at_open {
            self.id_and_password(&trailer, &|o| self.resolve_in(&first, o))
                .map_err(|e| format!("{CRYPT_PREFIX}{e}"))?;
        }
        let initial_len = entries.len() as i32;
        let mut candidates: Vec<i32> = stream_at
            .iter()
            .filter(|&(_, &ofs)| ofs != 0)
            .map(|(&n, _)| n)
            .collect();
        candidates.sort_unstable();
        let mut changed: HashSet<i32> = HashSet::new();
        for i in candidates {
            // An object stream processed earlier may have claimed this one.
            if entries.get(i).kind != Kind::InFile {
                continue;
            }
            let obj = match self.load_in(&first, i, false) {
                Loaded::Obj(o) => o,
                Loaded::Missing => continue,
                Loaded::Failed(e) | Loaded::NeedsRepair(e) => return Err(e),
            };
            let is_objstm = obj
                .as_dict()
                .and_then(|d| d.get(b"Type"))
                .is_some_and(|t| self.resolve_in(&first, t).is_name(b"ObjStm"));
            if is_objstm
                && let Err(e) = self.repair_objstm(&first, i, &obj, &mut entries, &mut changed)
            {
                self.report(&e);
                self.diag.warn(
                    "repair.broken_objstm",
                    format!("ignoring broken object stream ({i} 0 R)"),
                );
            }
        }
        let mut explicit = entries.explicit();
        explicit.sort_unstable_by_key(|&(n, _)| n);
        for (i, e) in explicit {
            if i < initial_len
                && e.kind == Kind::InStream
                && entries.get(e.ofs as i32).kind != Kind::InFile
            {
                self.diag.warn(
                    "repair.bad_objstm_ref",
                    format!(
                        "invalid reference to non-object-stream: {}, assuming {i} 0 R is a freed object",
                        e.ofs as i32
                    ),
                );
                entries.set(
                    i,
                    Entry {
                        kind: Kind::Free,
                        ..e
                    },
                );
                changed.insert(i);
            }
        }

        let mut second = State::new(0, entries, trailer, 1, true);
        for (num, value) in first.cache.all() {
            if !changed.contains(&num) && (num as usize) < second.xref_len() {
                second.cache.insert_if_absent(num, value);
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
        for i in second.entries.in_use_descending() {
            if i <= 0 || (has_root && has_info) {
                break;
            }
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
                Loaded::Failed(cause) | Loaded::NeedsRepair(cause) => {
                    self.report(&cause);
                    self.diag.warn(
                        "repair.broken_object",
                        format!("ignoring broken object ({i} 0 R)"),
                    );
                }
            }
        }

        second.trailer = trailer;
        // Objects loaded during the repair may have been decrypted with a
        // half-built xref: MuPDF drops them, then caches /Encrypt and /ID
        // without decryption.
        if self.crypt().is_some() {
            second.cache.clear();
            let trailer = second.trailer.clone();
            self.precache_unencrypted(&second, trailer.get(b"Encrypt"));
            self.precache_unencrypted(&second, trailer.get(b"ID"));
        }
        Ok(second)
    }

    /// `pdf_repair_obj_stm`.
    fn repair_objstm(
        &self,
        st: &State,
        stm: i32,
        obj: &Object,
        entries: &mut Slots,
        changed: &mut HashSet<i32>,
    ) -> Result<(), String> {
        let Object::Stream(stream) = obj else {
            return Err(report::format("object is not a stream"));
        };
        let count = self
            .resolve_in(st, stream.dict.get(b"N").unwrap_or(&Object::Null))
            .to_int();
        let data = self.stream_bytes_in(st, stream).map_err(|e| e.report())?;
        let mut lx = Lexer::small(&data, 0, &self.diag);
        let mut i = 0;
        while i < count {
            let Token::Int(n) = lx.next_token() else {
                return Err(report::format(&format!(
                    "corrupt object stream ({stm} 0 R)"
                )));
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
            let mut replace = true;
            let existing = entries.get(n);
            if existing.kind != Kind::None && existing.kind != Kind::Free {
                let here = entry_offset(entries, n);
                if here >= 0 && here > entry_offset(entries, stm) {
                    replace = false;
                }
            }
            if replace {
                entries.set(
                    n,
                    Entry {
                        kind: Kind::InStream,
                        ofs: i64::from(stm),
                        generation: i,
                    },
                );
                changed.insert(n);
            }
            let Token::Int(_) = lx.next_token() else {
                return Err(report::format(&format!(
                    "corrupt object stream ({stm} 0 R)"
                )));
            };
            i += 1;
        }
        Ok(())
    }
}

fn demote_if_failed(st: &State, num: i32, obj: Object) -> Object {
    match obj {
        Object::Stream(s) if st.is_demoted(num) => Object::Dict(Arc::new(s.dict.clone())),
        other => other,
    }
}

fn limit_error(message: &str) -> OpenError {
    OpenError {
        code: "limit.xref_entries",
        message: message
            .strip_prefix(xref::LIMIT_PREFIX)
            .unwrap_or(message)
            .to_string(),
    }
}

/// Offset used to decide whether an object stream may claim a number.
fn entry_offset(entries: &Slots, num: i32) -> i64 {
    let e = entries.get(num);
    match e.kind {
        Kind::None | Kind::Free => 0,
        Kind::InFile => e.ofs,
        Kind::InStream => match i32::try_from(e.ofs).map(|o| entries.get(o)) {
            Ok(s) if s.kind == Kind::InFile => s.ofs,
            _ => -1,
        },
    }
}

fn into_object(ind: IndirectObject) -> Object {
    match (ind.stream_start, ind.obj) {
        (Some(data_start), Object::Dict(dict)) => Object::Stream(Arc::new(Stream {
            dict: Arc::unwrap_or_clone(dict),
            data_start,
            num: ind.num,
        })),
        (_, obj) => obj,
    }
}

/// The generation that seeds a stream's key: its xref entry's, or 0 when
/// the number is outside the xref (`pdf_open_raw_filter`).
fn entry_generation(st: &State, num: i32) -> i32 {
    if num > 0 && (num as usize) < st.xref_len() {
        st.entries.get(num).generation
    } else {
        0
    }
}

fn crypt_error(e: &str) -> OpenError {
    OpenError {
        code: "input.encryption",
        message: report::strip(e).to_string(),
    }
}

/// Scans `data[abs_ofs..upper]` for the `%%EOF` marker terminating the xref
/// section at `abs_ofs`, preferring a `startxref <digits> %%EOF` sequence and
/// including at most one trailing line ending (`\r\n`, `\n`, or `\r`).
fn find_revision_eof_end(data: &[u8], abs_ofs: usize, upper: usize) -> Option<usize> {
    if abs_ofs >= upper || upper > data.len() {
        return None;
    }
    let slice = data.get(abs_ofs..upper)?;
    let is_pdf_ws = |b: u8| matches!(b, 0 | 9 | 10 | 12 | 13 | 32);

    let mut first_eof = None;
    let mut matched_eof = None;
    for eof_rel in memchr::memmem::find_iter(slice, b"%%EOF") {
        if first_eof.is_none() {
            first_eof = Some(eof_rel);
        }
        // Check whether `slice[..eof_rel]` ends with `startxref <ws> <digits> <ws>`.
        let mut p = eof_rel;
        let ws_after_end = p;
        while p > 0 && slice.get(p - 1).is_some_and(|&b| is_pdf_ws(b)) {
            p -= 1;
        }
        if p == ws_after_end {
            continue;
        }
        let digits_end = p;
        while p > 0 && slice.get(p - 1).is_some_and(|b| b.is_ascii_digit()) {
            p -= 1;
        }
        if p == digits_end {
            continue;
        }
        let ws_before_end = p;
        while p > 0 && slice.get(p - 1).is_some_and(|&b| is_pdf_ws(b)) {
            p -= 1;
        }
        if p < ws_before_end
            && slice
                .get(..p)
                .is_some_and(|head| head.ends_with(b"startxref"))
        {
            matched_eof = Some(eof_rel);
            break;
        }
    }

    let eof_rel = matched_eof.or(first_eof)?;
    let after_marker = abs_ofs.saturating_add(eof_rel).saturating_add(5);
    let tail = data.get(after_marker..)?;
    let h_ws = tail
        .iter()
        .take(16)
        .take_while(|&&b| b == b' ' || b == b'\t')
        .count();
    let eol_tail = tail.get(h_ws..).unwrap_or(&[]);
    let extra = if eol_tail.starts_with(b"\r\n") {
        h_ws + 2
    } else if eol_tail.first().is_some_and(|&b| b == b'\n' || b == b'\r') {
        h_ws + 1
    } else {
        0
    };
    Some(after_marker.saturating_add(extra).min(data.len()))
}

/// Re-point a cached object at the entry's stream data. In MuPDF an object
/// is a stream when its entry has stream data, whatever was cached, so a
/// cached dictionary becomes a stream again when a later definition of the
/// number has data, and a stream becomes a dictionary when it has none.
fn with_data_start(obj: Object, num: i32, stm_ofs: i64) -> Object {
    let start = usize::try_from(stm_ofs).ok().filter(|&s| s > 0);
    match (obj, start) {
        (Object::Stream(s), Some(start)) => {
            let mut s = Arc::unwrap_or_clone(s);
            s.data_start = start;
            Object::Stream(Arc::new(s))
        }
        (Object::Stream(s), None) => Object::Dict(Arc::new(Arc::unwrap_or_clone(s).dict)),
        (Object::Dict(d), Some(start)) => Object::Stream(Arc::new(Stream {
            dict: Arc::unwrap_or_clone(d),
            data_start: start,
            num,
        })),
        (other, _) => other,
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
    fn the_cache_keeps_the_first_value_and_only_numbers_it_has_slots_for() {
        let mut cache = Cache::new(2 * Cache::CHUNK);
        assert!(cache.get(5).is_none() && cache.get(-1).is_none());
        assert!(matches!(
            cache.insert_if_absent(5, Loaded::Obj(Object::Int(1))),
            Loaded::Obj(Object::Int(1))
        ));
        // A second thread's value for the same object loses to the first.
        assert!(matches!(
            cache.insert_if_absent(5, Loaded::Obj(Object::Int(2))),
            Loaded::Obj(Object::Int(1))
        ));
        cache.insert_if_absent(511, Loaded::Missing);
        // Past the slots, or negative: handed back, not kept.
        assert!(matches!(
            cache.insert_if_absent(512, Loaded::Obj(Object::Int(3))),
            Loaded::Obj(Object::Int(3))
        ));
        cache.insert_if_absent(-4, Loaded::Missing);
        assert!(cache.get(512).is_none() && cache.get(-4).is_none());
        let mut all: Vec<i32> = cache.all().into_iter().map(|(n, _)| n).collect();
        all.sort_unstable();
        assert_eq!(all, [5, 511]);
        cache.clear();
        assert!(cache.get(5).is_none() && cache.all().is_empty());
    }

    #[test]
    fn threads_loading_the_same_objects_see_one_value() {
        let d = doc(SIMPLE);
        let objects: Vec<Vec<String>> = std::thread::scope(|s| {
            let workers: Vec<_> = (0..8)
                .map(|_| s.spawn(|| (1..3).map(|n| format!("{:?}", d.object(n))).collect()))
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        assert!(objects.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn repair_keeps_generations_as_mupdf_does() {
        // No xref, so the file is repaired: object 2 is missing and object 3
        // has a generation past 16 bits.
        let d = doc(b"%PDF-1.4\n1 0 obj <</Type/Catalog>> endobj\n\
3 65537 obj (x) endobj\ntrailer <</Root 1 0 R>>\n");
        assert!(d.repair_attempted());
        let e = |num| d.xref_entry(num);
        assert_eq!((e(0).kind, e(0).generation), (Kind::Free, 65535));
        // Free entries are chained, which bumps their generation.
        assert_eq!((e(2).kind, e(2).generation), (Kind::Free, 1));
        // The scanner clamps generations (an xref table wraps them).
        assert_eq!((e(3).kind, e(3).generation), (Kind::InFile, 65535));
    }

    #[test]
    fn a_huge_object_number_repairs_without_an_entry_per_number() {
        let d = doc(b"%PDF-1.4\n1 0 obj <</Type/Catalog>> endobj\n\
8000000 0 obj <<>> endobj\ntrailer <</Root 1 0 R>>\n");
        assert!(d.repair_attempted());
        assert_eq!(d.xref_len(), 8_000_001);
        let e = |num| d.xref_entry(num);
        assert_eq!((e(0).kind, e(0).generation), (Kind::Free, 65535));
        assert_eq!(
            (e(4_000_000).kind, e(4_000_000).generation),
            (Kind::Free, 1)
        );
        assert_eq!(e(8_000_000).kind, Kind::InFile);
    }

    #[test]
    fn missing_xref_is_repaired() {
        let d = doc(SIMPLE);
        assert!(d.repaired());
        assert!(d.diagnostics().has("xref.repairing"));
        assert_eq!(d.page_count(), Ok(0));
        assert_eq!(d.version(), Version(14));
    }

    #[test]
    fn passwords_grant_access_like_mupdf() {
        let d = doc(include_bytes!(
            "../../../corpus/smoke/encrypted-user-password.pdf"
        ));
        assert!(d.needs_password());
        assert_eq!(d.encryption(), "Standard V5 R6 256-bit AES");
        assert_eq!(d.authenticate("wrong"), Ok(0));
        assert_eq!(d.authenticate("user"), Ok(2));
        assert_eq!(d.authenticate("owner"), Ok(4));
        // Loaded after a successful attempt, strings decrypt.
        assert_eq!(
            d.metadata("Title").as_deref(),
            Some("Encrypted: encrypted-user-password.pdf")
        );

        let plain = doc(SIMPLE);
        assert!(!plain.needs_password());
        assert_eq!(plain.encryption(), "None");
        assert_eq!(plain.authenticate("anything"), Ok(1));
    }

    #[test]
    fn unknown_security_handlers_fail_to_open() {
        let file = b"%PDF-1.4\n1 0 obj <</Type/Catalog/Pages 2 0 R>> endobj\n2 0 obj <</Type/Pages/Kids[]/Count 0>> endobj\ntrailer <</Root 1 0 R/Encrypt<</Filter/Adobe.PubSec/V 4>>>>\n";
        let e = Document::open(file.to_vec()).unwrap_err();
        assert_eq!(e.code, "input.encryption");
        assert_eq!(e.message, "unknown encryption handler: 'Adobe.PubSec'");
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
    fn xref_limit_refuses_large_tables() {
        let limits = Limits {
            max_xref_entries: 2,
            ..Limits::default()
        };
        let e = Document::open_with(SIMPLE.to_vec(), limits).unwrap_err();
        assert_eq!(e.code, "limit.xref_entries");
    }

    #[test]
    fn revision_offsets_point_at_xref_sections() {
        let head = "%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n";
        let at = head.len();
        let data = format!(
            "{head}xref\n0 2\n0000000000 65535 f \n0000000009 00000 n \n\
             trailer<</Root 1 0 R/Size 2>>\nstartxref\n{at}\n%%EOF\n"
        );
        let d = doc(data.as_bytes());
        assert!(!d.repaired());
        assert_eq!(d.revision_offsets(), vec![at as u64]);
        assert!(doc(SIMPLE).revision_offsets().is_empty());
    }

    #[test]
    fn document_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Document>();
    }

    #[test]
    fn edits_are_seen_by_every_lookup() {
        let doc =
            Document::open(&include_bytes!("../../../corpus/smoke/multipage-inherit.pdf")[..])
                .unwrap_or_else(|e| panic!("{e:?}"));
        let len = doc.xref_len();
        assert!(!doc.is_edited());
        // A new object goes after the others, with generation 0.
        let num = doc.add_object(Object::Int(7));
        assert_eq!(num as usize, len);
        assert_eq!(doc.xref_len(), len + 1);
        assert_eq!(doc.object(num), Some(Object::Int(7)));
        assert_eq!(doc.xref_entry(num).generation, 0);
        // A replaced object is resolved through references too.
        doc.update_object(1, Object::Int(42));
        assert_eq!(
            doc.resolve(&Object::Ref(ObjRef::new(1, 0))),
            Object::Int(42)
        );
        // The trailer can change.
        let mut t = doc.trailer();
        t.put(b"Extra", Object::Bool(true));
        doc.set_trailer(t);
        assert_eq!(doc.trailer().get(b"Extra"), Some(&Object::Bool(true)));
        // A new stream's data is in memory.
        let s = doc.add_stream(Dict::new(), b"data".to_vec());
        let Some(Object::Stream(stream)) = doc.object(s) else {
            panic!("not a stream");
        };
        assert_eq!(
            doc.raw_stream_data(&stream).ok().as_deref(),
            Some(&b"data"[..])
        );
        assert_eq!(doc.stream_data(&stream).ok(), Some(b"data".to_vec()));
        assert!(doc.is_edited());
    }

    #[test]
    fn local_streams_are_read_while_kept_and_never_edit_the_document() {
        let doc = doc(SIMPLE);
        let len = doc.xref_len();
        let a = doc.add_local_stream(Dict::new(), b"one".to_vec());
        let b = doc.add_local_stream(Dict::new(), b"two".to_vec());
        let data = |l: &LocalStream| match l.object() {
            Object::Stream(s) => doc.stream_data(s).ok(),
            _ => None,
        };
        assert_eq!(data(&a), Some(b"one".to_vec()));
        assert_eq!(data(&b), Some(b"two".to_vec()));
        assert!(!doc.is_edited());
        assert_eq!(doc.xref_len(), len);
        let Object::Stream(gone) = a.object().clone() else {
            panic!("not a stream");
        };
        drop(a);
        assert_eq!(doc.stream_data(&gone).ok(), Some(Vec::new()));
        assert_eq!(data(&b), Some(b"two".to_vec()));
    }

    #[test]
    fn replacing_resets_the_generation_and_changing_in_place_keeps_it() {
        let data = b"%PDF-1.4\n1 3 obj\n<</Type/Catalog/Pages 2 0 R>>\nendobj\n\
2 5 obj\n<</Type/Pages/Kids[]/Count 0>>\nendobj\n\
xref\n0 3\n0000000000 65535 f \n0000000009 00003 n \n0000000056 00005 n \n\
trailer\n<</Size 3/Root 1 3 R>>\nstartxref\n104\n%%EOF\n";
        let doc = Document::open(&data[..]).unwrap_or_else(|e| panic!("{e:?}"));
        doc.update_object(1, Object::Null);
        doc.alter_object(2, Object::Null);
        assert_eq!(doc.xref_entry(1).generation, 0);
        assert_eq!(doc.xref_entry(2).generation, 5);
        // Once moved, an entry keeps what it got.
        doc.update_object(2, Object::Int(1));
        assert_eq!(doc.xref_entry(2).generation, 5);
    }

    #[test]
    fn created_objects_are_free_until_filled_and_deleting_frees_them() {
        let doc = Document::create().unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(doc.page_count(), Ok(0));
        assert_eq!(doc.xref_len(), 3);
        let num = doc.create_object();
        assert_eq!(num, 3);
        assert_eq!(doc.xref_entry(3).kind, Kind::Free);
        doc.update_object(3, Object::Dict(Arc::new(Dict::new())));
        assert_eq!(doc.xref_entry(3).kind, Kind::InFile);
        doc.update_stream(3, b"abc".to_vec());
        match doc.object(3) {
            Some(Object::Stream(s)) => {
                assert_eq!(s.dict.get(b"Length"), Some(&Object::Int(3)));
                assert_eq!(doc.raw_stream_data(&s).ok().as_deref(), Some(&b"abc"[..]));
            }
            other => panic!("{other:?}"),
        }
        // An object the file never had goes back to nothing.
        doc.delete_object(3);
        assert_eq!(doc.xref_entry(3).kind, Kind::None);
        assert_eq!(doc.object(3), None);
        // One it had is free, of the next generation in the new section.
        doc.delete_object(2);
        let e = doc.xref_entry(2);
        assert_eq!((e.kind, e.generation), (Kind::Free, 1));
        assert_eq!(doc.object(2), Some(Object::Null));
    }
}

#[cfg(test)]
mod kept_state {
    use super::*;

    /// A file whose xref sends object 3 to object 2's offset, so loading 3
    /// repairs the document.
    fn wrong_offset() -> Vec<u8> {
        let objs = [
            "1 0 obj <</Type/Catalog/Pages 2 0 R>> endobj\n",
            "2 0 obj <</Type/Pages/Kids[]/Count 0>> endobj\n",
            "3 0 obj (three) endobj\n",
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for o in objs {
            offsets.push(out.len());
            out.extend_from_slice(o.as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
        for o in [offsets[0], offsets[1], offsets[1]] {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer <</Size 4/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        out
    }

    #[test]
    fn a_thread_that_kept_the_state_sees_a_repair_made_on_another() {
        let doc = Document::open(wrong_offset()).unwrap();
        let before = doc.current();
        assert!(!doc.repair_attempted());
        std::thread::scope(|s| {
            s.spawn(|| {
                let three = doc.resolve(&Object::Ref(ObjRef {
                    num: 3,
                    generation: 0,
                }));
                assert!(matches!(three, Object::String(s) if &*s == b"three"));
            });
        });
        assert!(doc.repair_attempted());
        assert!(!Arc::ptr_eq(&before, &doc.current()));
        assert!(Arc::ptr_eq(&doc.current(), &doc.current()));
    }

    #[test]
    fn dropping_a_document_lets_go_of_the_kept_state() {
        let doc = Document::open(wrong_offset()).unwrap();
        let state = Arc::downgrade(&doc.current());
        assert!(state.upgrade().is_some());
        drop(doc);
        assert!(state.upgrade().is_none());
    }
}

#[cfg(test)]
pub(crate) mod object_streams {
    use super::*;

    fn doc(bytes: &[u8]) -> Document {
        Document::open(bytes.to_vec()).unwrap()
    }

    /// A file indexed by an xref stream: `objects` (number, body) in the
    /// file, in order, and `packed` (number, object stream, index) in
    /// object streams. The xref stream comes last, numbered after the
    /// others, with object 1 as /Root.
    pub(crate) fn xref_stream_file(
        objects: &[(i32, Vec<u8>)],
        packed: &[(i32, i32, i32)],
    ) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = HashMap::new();
        for (n, body) in objects {
            offsets.insert(*n, out.len());
            out.extend_from_slice(format!("{n} 0 obj\n").as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let max = objects
            .iter()
            .map(|o| o.0)
            .chain(packed.iter().map(|p| p.0))
            .max()
            .unwrap_or(0);
        let xnum = max + 1;
        let xref = out.len();
        offsets.insert(xnum, xref);
        let mut data = Vec::new();
        for i in 0..=xnum {
            let (kind, a, b): (u8, u32, u16) = if i == 0 {
                (0, 0, 0xffff)
            } else if let Some(&o) = offsets.get(&i) {
                (1, o as u32, 0)
            } else if let Some(&(_, stm, idx)) = packed.iter().find(|p| p.0 == i) {
                (2, stm as u32, idx as u16)
            } else {
                (0, 0, 0)
            };
            data.push(kind);
            data.extend_from_slice(&a.to_be_bytes());
            data.extend_from_slice(&b.to_be_bytes());
        }
        let dict = format!("/Type/XRef/Size {}/W[1 4 2]/Root 1 0 R", xnum + 1);
        out.extend_from_slice(format!("{xnum} 0 obj\n").as_bytes());
        out.extend_from_slice(&crate::testutil::stream(&dict, &data));
        out.extend_from_slice(format!("\nendobj\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        out
    }

    /// An object stream's body holding `members` (number, object text).
    pub(crate) fn objstm(members: &[(i32, &str)]) -> Vec<u8> {
        let mut header = String::new();
        let mut body = String::new();
        for (n, text) in members {
            header.push_str(&format!("{n} {} ", body.len()));
            body.push_str(text);
            body.push(' ');
        }
        let dict = format!("/Type/ObjStm/N {}/First {}", members.len(), header.len());
        crate::testutil::stream(&dict, format!("{header}{body}").as_bytes())
    }

    fn messages(doc: &Document) -> Vec<String> {
        doc.diagnostics()
            .snapshot()
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    /// One good object stream and one broken in each way MuPDF tells
    /// apart; with `bad_refs`, members said to be in streams that are
    /// free or past the end of the xref too.
    fn broken_objstms(bad_refs: bool) -> Vec<u8> {
        let s = crate::testutil::stream;
        let mut packed = vec![
            (2, 10, 0),
            (3, 11, 0),
            (4, 12, 0),
            (5, 13, 0),
            (6, 14, 1),
            (7, 15, 0),
            (8, 15, 1),
            (9, 16, 0),
            (17, 1, 0),
            (21, 22, 0),
        ];
        if bad_refs {
            packed.extend([(18, 19, 0), (20, 999, 0)]);
        }
        xref_stream_file(
            &[
                (1, b"<</Type/Catalog/Pages 2 0 R>>".to_vec()),
                (10, objstm(&[(2, "<</Type/Pages/Kids[]/Count 0>>")])),
                (11, s("/Type/ObjStm/N -1/First 0", b"")),
                (12, s("/Type/ObjStm/N 1/First 4", b"x y (a)")),
                (13, s("/Type/ObjStm/N 1/First 4", b"5 x (a)")),
                (14, s("/Type/ObjStm/N 2/First 9", b"99 0 6 0 (six)")),
                (15, s("/Type/ObjStm/N 2/First 8", b"7 3 8 0 (e)(s)")),
                (16, s("/Type/ObjStm/N 1/First 4", b"9 0 )")),
                (
                    22,
                    s("/Type/ObjStm/N 1/First 4/Filter/FlateDecode", b"junk"),
                ),
            ],
            &packed,
        )
    }

    /// Each object looked up in turn, with what the lookup reported.
    fn lookups(d: &Document, nums: &[i32]) -> Vec<(i32, Option<Object>, Vec<String>)> {
        nums.iter()
            .map(|&n| {
                let before = messages(d).len();
                let obj = d.object(n);
                (n, obj, messages(d)[before..].to_vec())
            })
            .collect()
    }

    fn string(s: &str) -> Option<Object> {
        Some(Object::String(Arc::from(s.as_bytes())))
    }

    fn said(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn broken_object_streams_fail_as_mupdf_reports_them() {
        // What `mutool show -b FILE 2 3 3 4 5 6 7 7 8 9 17 21` prints, a
        // value and the errors and warnings before it, for each lookup.
        let d = doc(&broken_objstms(false));
        assert_eq!(d.page_count(), Ok(0));
        let cannot = |n: i32| format!("cannot load object ({n} 0 R) into cache");
        let expected = vec![
            (
                3,
                None,
                said(&[
                    "format error: number of objects in object stream out of range",
                    &cannot(3),
                ]),
            ),
            (
                3,
                None,
                said(&["format error: corrupt object stream 11", &cannot(3)]),
            ),
            (
                4,
                None,
                said(&["format error: corrupt object stream (12 0 R)", &cannot(4)]),
            ),
            (
                5,
                None,
                said(&["format error: corrupt object stream (13 0 R)", &cannot(5)]),
            ),
            (
                6,
                string("six"),
                said(&["object stream object out of range, skipping"]),
            ),
            (
                7,
                None,
                said(&[
                    "cannot seek backwards",
                    "syntax error: unknown token in object stream",
                    &cannot(7),
                ]),
            ),
            // Parsed before the error, and kept.
            (7, string("s"), vec![]),
            (
                8,
                None,
                said(&["format error: corrupt object stream 15", &cannot(8)]),
            ),
            (
                9,
                None,
                said(&["syntax error: unknown token in object stream", &cannot(9)]),
            ),
            (
                17,
                None,
                said(&["format error: object is not a stream", &cannot(17)]),
            ),
            (
                21,
                None,
                said(&[
                    "library error: zlib error: incorrect header check",
                    "read error; treating as end of file",
                    "format error: corrupt object stream (22 0 R)",
                    &cannot(21),
                ]),
            ),
        ];
        let nums: Vec<i32> = expected.iter().map(|e| e.0).collect();
        assert_eq!(lookups(&d, &nums), expected);
        assert!(matches!(d.object(2), Some(Object::Dict(_))));
        assert!(!d.repair_attempted());
    }

    #[test]
    fn repair_reads_object_streams_and_reports_what_it_skips() {
        // A member said to be in a free object (19) makes MuPDF repair the
        // file as it opens. Its warnings, in order, from `mutool show`.
        let d = doc(&broken_objstms(true));
        assert!(d.repair_attempted());
        assert_eq!(
            messages(&d),
            said(&[
                "format error: invalid reference to an objstm that does not exist: 19 (18 0 R)",
                "trying to repair broken xref",
                "repairing PDF document",
                "format error: corrupt object stream (12 0 R)",
                "ignoring broken object stream (12 0 R)",
                "format error: corrupt object stream (13 0 R)",
                "ignoring broken object stream (13 0 R)",
                "library error: zlib error: incorrect header check",
                "read error; treating as end of file",
                "format error: corrupt object stream (22 0 R)",
                "ignoring broken object stream (22 0 R)",
                "syntax error: unknown token in object stream",
                "ignoring broken object (99 0 R)",
                "syntax error: unknown token in object stream",
                "ignoring broken object (9 0 R)",
                "cannot seek backwards",
                "syntax error: unknown token in object stream",
                "ignoring broken object (8 0 R)",
                "format error: corrupt object stream 14",
                "ignoring broken object (6 0 R)",
                "format error: corrupt object stream (13 0 R)",
                "ignoring broken object (5 0 R)",
            ])
        );
        let values: Vec<Option<Object>> = [2, 3, 4, 7, 17, 18, 20, 21]
            .iter()
            .map(|&n| d.object(n))
            .collect();
        assert!(matches!(values[0], Some(Object::Dict(_))));
        assert_eq!(
            values[1..].to_vec(),
            [
                Some(Object::Null),
                Some(Object::Null),
                string("s"),
                Some(Object::Null),
                Some(Object::Null),
                Some(Object::Null),
                Some(Object::Null)
            ]
        );
    }

    #[test]
    fn an_object_stream_past_the_limit_is_refused() {
        let file = xref_stream_file(
            &[
                (1, b"<</Type/Catalog/Pages 2 0 R>>".to_vec()),
                (
                    3,
                    objstm(&[(2, "<</Type/Pages/Kids[]/Count 0>>"), (4, "1")]),
                ),
            ],
            &[(2, 3, 0), (4, 3, 1)],
        );
        let limits = Limits {
            max_objstm_objects: 1,
            ..Limits::default()
        };
        let d = Document::open_with(file, limits).unwrap();
        assert_eq!(d.object(4), None);
        assert!(d.diagnostics().has("limit.objstm_objects"));
        assert!(
            messages(&d)
                .contains(&"format error: object stream 3 exceeds the object limit".to_string())
        );
    }
}

#[cfg(test)]
mod lookups_and_edits {
    use super::object_streams::{objstm, xref_stream_file};
    use super::*;

    fn doc(bytes: &[u8]) -> Document {
        Document::open(bytes.to_vec()).unwrap()
    }

    fn messages(doc: &Document) -> Vec<String> {
        doc.diagnostics()
            .snapshot()
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    fn packed() -> Document {
        doc(&xref_stream_file(
            &[
                (1, b"<</Type/Catalog/Pages 2 0 R>>".to_vec()),
                (
                    3,
                    objstm(&[(5, "5"), (2, "<</Type/Pages/Kids[]/Count 0>>")]),
                ),
                (4, crate::testutil::stream("", b"data")),
                (6, b"42".to_vec()),
            ],
            &[(5, 3, 0), (2, 3, 1)],
        ))
    }

    fn pages_count(d: &Document) -> Object {
        let root = d.trailer().get(b"Root").cloned().unwrap();
        d.dict_get(&d.dict_get(&root, b"Pages"), b"Count")
    }

    #[test]
    fn a_failed_open_says_why() {
        let e = Document::open(b"not a pdf".to_vec()).err().unwrap();
        assert_eq!(e.to_string(), "no objects found");
        assert_eq!(e.code, "input.unrepairable");
    }

    #[test]
    fn members_of_object_streams_changed_in_place_get_generation_0() {
        let d = packed();
        assert_eq!(d.xref_entry(2).kind, Kind::InStream);
        assert_eq!(d.xref_entry(2).generation, 1);
        d.alter_object(2, Object::Int(7));
        assert_eq!(d.xref_entry(2).generation, 0);
        d.replace_stream(5, Dict::new(), b"x".to_vec());
        assert_eq!(d.xref_entry(5).generation, 0);
        assert!(matches!(d.object(5), Some(Object::Stream(_))));
    }

    #[test]
    fn objects_out_of_range_are_not_deleted() {
        let d = packed();
        d.delete_object(0);
        d.delete_object(99);
        assert_eq!(
            messages(&d),
            [
                "object out of range (0 0 R); xref size 8",
                "object out of range (99 0 R); xref size 8"
            ]
        );
        assert!(!d.is_edited());
    }

    #[test]
    fn update_stream_needs_a_dictionary_to_hold_the_data() {
        let d = packed();
        d.update_stream(99, b"nothing".to_vec());
        d.update_stream(6, b"not a dict".to_vec());
        assert!(!d.is_edited());
        d.update_stream(4, b"longer data".to_vec());
        let Some(Object::Stream(s)) = d.object(4) else {
            panic!("not a stream");
        };
        assert_eq!(s.dict.get(b"Length"), Some(&Object::Int(11)));
        assert_eq!(d.stream_data(&s).unwrap(), b"longer data");
    }

    #[test]
    fn edited_streams_are_read_through_their_filters() {
        let d = packed();
        let mut dict = Dict::new();
        dict.put(b"Filter", Object::name(b"ASCIIHexDecode"));
        d.replace_stream(4, dict, b"414243>".to_vec());
        let Some(Object::Stream(s)) = d.object(4) else {
            panic!("not a stream");
        };
        assert_eq!(d.stream_bytes(&s).unwrap(), b"ABC");
        assert_eq!(d.content_stream_data(&s).unwrap(), (b"ABC".to_vec(), false));
        assert_eq!(d.image_samples(&s, 2).unwrap(), b"AB");
    }

    #[test]
    fn a_count_too_high_is_written_into_the_tree_at_the_first_edit() {
        let d = doc(include_bytes!(
            "../../../corpus/smoke/pages-count-too-high.pdf"
        ));
        assert!(d.page(0).is_ok());
        // Loading the page tree found one page; nupdf keeps it to itself.
        assert_eq!(d.page_count(), Ok(1));
        assert_eq!(pages_count(&d), Object::Int(3));
        d.add_object(Object::Null);
        assert_eq!(pages_count(&d), Object::Int(1));
        assert_eq!(d.page_count(), Ok(1));
    }

    #[test]
    fn settling_the_page_count_rewrites_it_unedited() {
        let d = doc(include_bytes!(
            "../../../corpus/smoke/pages-count-too-high.pdf"
        ));
        d.settle_page_count();
        assert!(!d.is_edited());
        assert!(d.page(0).is_ok());
        d.settle_page_count();
        assert!(d.is_edited());
        assert_eq!(pages_count(&d), Object::Int(1));
    }

    #[test]
    fn permissions_follow_p_unless_the_owner_opened_it() {
        let d = doc(include_bytes!(
            "../../../corpus/smoke/encrypted-restricted.pdf"
        ));
        let allowed = |d: &Document| {
            [
                "print",
                "edit",
                "copy",
                "annotate",
                "form",
                "accessibility",
                "assemble",
                "print-hq",
                "something-else",
            ]
            .map(|p| d.has_permission(p))
        };
        assert_eq!(
            allowed(&d),
            [true, false, false, false, false, true, false, false, true]
        );
        assert_eq!(d.authenticate("owner"), Ok(4));
        assert_eq!(allowed(&d), [true; 9]);
        // The password carries over to the file opened again.
        let again = d.reopen().unwrap();
        assert_eq!(allowed(&again), [true; 9]);
        assert_eq!(again.encryption(), d.encryption());
    }

    #[test]
    fn documents_made_in_memory_cannot_be_reopened() {
        let d = Document::create().unwrap();
        assert!(d.reopen().is_none());
        let e = packed();
        assert!(e.reopen().is_some());
        e.add_object(Object::Null);
        assert!(e.reopen().is_none());
    }

    #[test]
    fn unencrypted_loads_are_only_for_objects_in_the_file() {
        let d = packed();
        assert_eq!(d.load_unencrypted(6), Some(Object::Int(42)));
        assert_eq!(d.load_unencrypted(2), None);
        assert_eq!(d.load_unencrypted(0), None);
        assert_eq!(d.load_unencrypted(99), None);
        assert_eq!(d.load_object(99), Ok(None));
    }

    #[test]
    fn an_object_stream_whose_count_is_inside_itself_is_recursive() {
        // /N is object 2, which is in the stream itself.
        let d = doc(&xref_stream_file(
            &[
                (1, b"<</Type/Catalog/Pages 4 0 R>>".to_vec()),
                (
                    3,
                    crate::testutil::stream("/Type/ObjStm/N 2 0 R/First 4", b"2 0 1"),
                ),
                (4, b"<</Type/Pages/Kids[]/Count 0>>".to_vec()),
            ],
            &[(2, 3, 0)],
        ));

        // As `mutool show FILE 2` reports it: the member is marked while
        // its stream is read, so the lookup of /N finds nothing.
        assert_eq!(d.object(2), None);
        assert_eq!(
            messages(&d),
            [
                "format error: cannot find object in xref (2 0 R)",
                "cannot load object (2 0 R) into cache",
                "format error: cannot load object stream containing object (2 0 R)",
                "cannot load object (2 0 R) into cache"
            ]
        );
        assert!(!d.repair_attempted());
    }

    #[test]
    fn jbig2_globals_that_need_themselves_are_a_cycle() {
        let d = crate::testutil::doc_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[]/Count 0>>".to_vec(),
            crate::testutil::stream(
                "/Filter/JBIG2Decode/DecodeParms<</JBIG2Globals 3 0 R>>",
                b"\0\0\0\0",
            ),
            crate::testutil::stream(
                "/Filter/JBIG2Decode/DecodeParms<</JBIG2Globals 5 0 R>>",
                b"\0\0\0\0",
            ),
            b"<</NotAStream true>>".to_vec(),
        ]);
        let Some(Object::Stream(s)) = d.object(3) else {
            panic!("not a stream");
        };
        assert_eq!(
            d.stream_data(&s).err().map(|e| e.report()),
            Some("format error: cyclic reference when loading JBIG2 globals".to_string())
        );
        let Some(Object::Stream(s)) = d.object(4) else {
            panic!("not a stream");
        };
        let _ = d.stream_data(&s);
    }

    #[test]
    fn an_unset_entry_cannot_be_found() {
        // Subsections for 0-1 and 3-3: 2 is inside the table but unset.
        let mut out = b"%PDF-1.7\n".to_vec();
        let one = out.len();
        out.extend_from_slice(b"1 0 obj<</Type/Catalog/Pages 3 0 R>>endobj\n");
        let three = out.len();
        out.extend_from_slice(b"3 0 obj<</Type/Pages/Kids[]/Count 0>>endobj\n");
        let xref = out.len();
        out.extend_from_slice(
            format!(
                "xref\n0 2\n0000000000 65535 f \n{one:010} 00000 n \n3 1\n{three:010} 00000 n \n\
                 trailer<</Size 4/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        let d = doc(&out);
        assert_eq!(d.page_count(), Ok(0));
        let before = messages(&d).len();
        assert_eq!(d.object(2), None);
        assert_eq!(
            messages(&d)[before..],
            [
                "format error: cannot find object in xref (2 0 R)",
                "cannot load object (2 0 R) into cache"
            ]
        );
    }

    #[test]
    fn a_member_missing_from_its_stream_fails_at_each_lookup() {
        // The xref puts 7 in object stream 3, which holds only 5 and 2.
        // As `mutool show FILE 7 5 7` reports it.
        let d = doc(&xref_stream_file(
            &[
                (1, b"<</Type/Catalog/Pages 2 0 R>>".to_vec()),
                (
                    3,
                    objstm(&[(5, "5"), (2, "<</Type/Pages/Kids[]/Count 0>>")]),
                ),
            ],
            &[(5, 3, 0), (2, 3, 1), (7, 3, 2)],
        ));
        let failure = [
            "format error: cannot load object stream containing object (7 0 R)",
            "cannot load object (7 0 R) into cache",
        ];
        assert_eq!(d.object(7), None);
        assert_eq!(messages(&d), failure);
        assert_eq!(d.object(5), Some(Object::Int(5)));
        assert_eq!(d.object(7), None);
        assert_eq!(messages(&d), [failure, failure].concat());
        assert!(!d.repair_attempted());
    }

    #[test]
    fn open_revision_time_travels_across_incremental_updates() {
        let bytes = include_bytes!("../../../corpus/smoke/incremental-update.pdf");
        let d = doc(bytes);
        assert_eq!(d.revisions(), 2);
        let ends = d.revision_byte_ends();
        assert_eq!(ends, vec![555, bytes.len()]);

        assert!(d.revision_bytes(0).is_err());
        assert!(d.revision_bytes(3).is_err());
        assert!(d.open_revision(0).is_err());
        assert!(d.open_revision(3).is_err());

        let rev1_bytes = d.revision_bytes(1).unwrap();
        assert!(rev1_bytes.ends_with(b"%%EOF\n"));

        let rev1 = d.open_revision(1).unwrap();
        assert_eq!(rev1.revisions(), 1);
        assert_eq!(rev1.metadata("Title"), None);

        let rev2 = d.open_revision(2).unwrap();
        assert_eq!(rev2.revisions(), 2);
        assert_eq!(rev2.metadata("Title").as_deref(), Some("Revision 2"));

        // Single-revision and repaired files fall back cleanly to `[data.len()]`.
        let minimal = include_bytes!("../../../corpus/smoke/minimal.pdf");
        let m = doc(minimal);
        assert_eq!(m.revision_byte_ends(), vec![minimal.len()]);

        let repaired = doc(include_bytes!("../../../corpus/smoke/no-xref.pdf"));
        assert!(repaired.repaired());
        assert_eq!(repaired.revision_byte_ends(), vec![repaired.data().len()]);
    }
}
