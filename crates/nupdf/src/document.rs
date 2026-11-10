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
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Loaded objects, keyed by number. Sharded so concurrent readers rarely
/// contend, and sparse so memory follows what was actually loaded rather
/// than how many slots the file declares.
#[derive(Debug)]
struct Cache {
    shards: Vec<RwLock<HashMap<i32, Loaded>>>,
}

impl Cache {
    const SHARDS: usize = 64;

    fn new() -> Self {
        Cache {
            shards: (0..Self::SHARDS)
                .map(|_| RwLock::new(HashMap::new()))
                .collect(),
        }
    }

    fn shard(&self, num: i32) -> &RwLock<HashMap<i32, Loaded>> {
        &self.shards[num.unsigned_abs() as usize % Self::SHARDS]
    }

    fn get(&self, num: i32) -> Option<Loaded> {
        self.shard(num).read().ok()?.get(&num).cloned()
    }

    fn clear(&self) {
        for shard in &self.shards {
            if let Ok(mut s) = shard.write() {
                s.clear();
            }
        }
    }

    /// Stores `value` unless another thread got there first; returns the
    /// value that is now cached.
    fn insert_if_absent(&self, num: i32, value: Loaded) -> Loaded {
        match self.shard(num).write() {
            Ok(mut shard) => shard.entry(num).or_insert(value).clone(),
            Err(_) => value,
        }
    }

    fn all(&self) -> Vec<(i32, Loaded)> {
        self.shards
            .iter()
            .filter_map(|s| {
                s.read()
                    .ok()
                    .map(|m| m.iter().map(|(&n, v)| (n, v.clone())).collect::<Vec<_>>())
            })
            .flatten()
            .collect()
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
}

impl State {
    fn new(bias: usize, entries: Slots, trailer: Dict, revisions: usize, repaired: bool) -> Self {
        State {
            bias,
            entries,
            cache: Cache::new(),
            objstms: Mutex::new(HashMap::new()),
            demoted: Mutex::new(std::collections::HashSet::new()),
            trailer,
            revisions,
            repaired,
            page_map: OnceLock::new(),
            page_count_override: OnceLock::new(),
            repaired_parents: OnceLock::new(),
            section_offsets: Vec::new(),
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
    /// MuPDF repairs at most once per document.
    repair_attempted: AtomicBool,
    /// Serializes repairs so other threads wait for the new xref.
    repair_lock: Mutex<()>,
    /// The security handler, created once while opening (MuPDF's
    /// `doc->crypt`). Authenticating replaces it with a new key.
    crypt: RwLock<Option<Arc<Crypt>>>,
    /// Whether the empty password failed (`pdf_needs_password`).
    needs_password: AtomicBool,
}

/// Marks a repair error that came from the security handler.
const CRYPT_PREFIX: &str = "crypt: ";

/// Page counting failed the way MuPDF's `pdf_count_pages` fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCountError(pub String);

impl Document {
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
            repair_attempted: AtomicBool::new(false),
            repair_lock: Mutex::new(()),
            crypt: RwLock::new(None),
            needs_password: AtomicBool::new(false),
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

    fn install(&self, state: State) {
        if let Ok(mut s) = self.state.write() {
            *s = Arc::new(state);
        }
    }

    pub(crate) fn current(&self) -> Arc<State> {
        self.state
            .read()
            .map(|s| Arc::clone(&s))
            .unwrap_or_else(|_| Arc::new(State::empty()))
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The raw (still encoded) bytes of a stream as MuPDF reads them:
    /// decrypted, unless a /Crypt filter in the chain does that. A broken
    /// AES stream yields what MuPDF reads before the error.
    pub fn raw_stream_data(&self, stream: &Stream) -> Result<Cow<'_, [u8]>, String> {
        let st = self.current();
        let (raw, decrypted) = self.raw_stream_in(&st, stream, true)?;
        Ok(match decrypted {
            None => Cow::Borrowed(&raw[..self.read_all_len(&st, stream, raw.len())]),
            Some(stage) => Cow::Owned(self.read_all(&st, stream, stage)),
        })
    }

    /// The bytes `fz_read_best` gets from an unencrypted stream it reads
    /// directly (see `xref::read_all_len`).
    fn read_all_len(&self, st: &State, stream: &Stream, raw_len: usize) -> usize {
        let length = self
            .resolve_in(st, stream.dict.get(b"Length").unwrap_or(&Object::Null))
            .to_int64();
        let initial = usize::try_from(length)
            .ok()
            .filter(|&l| l <= self.file_size)
            .unwrap_or(0);
        xref::read_all_len(
            &self.data,
            stream.data_start,
            length,
            self.file_size,
            initial,
        )
        .min(raw_len)
    }

    /// Whether the stream names any filter.
    fn has_filters(&self, st: &State, dict: &Dict) -> bool {
        let filters = dict
            .get(b"F")
            .or_else(|| dict.get(b"Filter"))
            .map_or(Object::Null, |f| self.resolve_in(st, f));
        match filters {
            Object::Name(_) => true,
            Object::Array(items) => !items.is_empty(),
            _ => false,
        }
    }

    /// `pdf_open_raw_filter`: the bytes between `stream` and `endstream`,
    /// and, in an encrypted document, those bytes decrypted. `may_repair`
    /// allows the /Length lookup to repair the document. Fails only where
    /// MuPDF cannot open the decryption filter.
    fn raw_stream_in(
        &self,
        st: &State,
        stream: &Stream,
        may_repair: bool,
    ) -> Result<(&[u8], Option<filter::Stage>), String> {
        let length = stream.dict.get(b"Length").unwrap_or(&Object::Null);
        let length = if may_repair {
            self.resolve(length)
        } else {
            self.resolve_in(st, length)
        }
        .to_int64();
        let raw = xref::raw_stream(
            &self.data,
            stream.data_start,
            length,
            self.file_size,
            &self.diag,
        );
        match self.crypt() {
            Some(crypt) if !self.has_crypt_filter(st, &stream.dict) => {
                let decrypted =
                    crypt.decrypt_stream(raw, stream.num, entry_generation(st, stream.num))?;
                Ok((raw, Some(decrypted.into())))
            }
            _ => Ok((raw, None)),
        }
    }

    /// `fz_read_best` over a stage that may stop with an error: it reads in
    /// chunks of at most 256 bytes into a buffer of the stream's length (at
    /// least 1024) plus one, grown by half when full, and loses the chunk
    /// during which the error happens.
    fn read_all(&self, st: &State, stream: &Stream, stage: filter::Stage) -> Vec<u8> {
        let Some(error) = &stage.error else {
            return stage.data;
        };
        filter::report_read_error(error, &self.diag);
        let length = self
            .resolve_in(st, stream.dict.get(b"Length").unwrap_or(&Object::Null))
            .to_int64();
        // pdf_stream_length: negative or beyond the file means 0.
        let initial = usize::try_from(length)
            .ok()
            .filter(|&l| l <= self.file_size)
            .unwrap_or(0);
        let mut data = stage.data;
        data.truncate(kept_before_error(data.len(), initial));
        data
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

    /// MuPDF's encryption description, such as "Standard V4 R4 128-bit
    /// AES", or "None".
    pub fn encryption(&self) -> String {
        self.crypt()
            .map_or_else(|| "None".to_string(), |c| c.description())
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

    /// Saved versions of the document, as `pdf_count_versions` counts
    /// them: xref sections (one after a repair), less the extra section of
    /// a linearized file.
    pub fn revisions(&self) -> usize {
        let s = self.current();
        let sections = if s.repaired { 1 } else { s.revisions };
        sections.saturating_sub(usize::from(self.linearized))
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
            Some(&n) => n,
            None => {
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

    fn stream_data_in(&self, st: &State, stream: &Stream) -> Result<Vec<u8>, DecodeError> {
        let (raw, decrypted) = self
            .raw_stream_in(st, stream, false)
            .map_err(DecodeError::Failed)?;
        let input = decrypted.unwrap_or_else(|| {
            // Read directly, without filters, the reader is fz_read_best.
            let keep = if self.has_filters(st, &stream.dict) {
                raw.len()
            } else {
                self.read_all_len(st, stream, raw.len())
            };
            filter::Stage::complete(raw[..keep].to_vec())
        });
        let crypt = self.crypt();
        let out = filter::decode_stage(
            &stream.dict,
            input,
            &|o| self.resolve_in(st, o),
            &self.diag,
            &self.limits,
            crypt.as_deref().map(|crypt| filter::CryptContext {
                crypt,
                num: stream.num,
                generation: entry_generation(st, stream.num),
            }),
        )?;
        Ok(self.read_all(st, stream, out))
    }

    /// `pdf_cache_object` against one state.
    fn load_in(&self, st: &State, num: i32, allow_repair: bool) -> Loaded {
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
            return Loaded::Failed(report::format(&format!(
                "recursive object lookup ({num} 0 R)"
            )));
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
                if can_repair {
                    Loaded::NeedsRepair(String::new())
                } else {
                    Loaded::Failed(report::format(&format!(
                        "object ({num} 0 R) was not found in its object stream"
                    )))
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
        if computed.error.is_some()
            && let Ok(mut demoted) = st.demoted.lock()
        {
            demoted.insert(stm);
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
        if count as usize > self.limits.max_objstm_objects {
            self.diag.warn(
                "limit.objstm_objects",
                format!("object stream {stm} declares {count} objects"),
            );
            return fail(format!("object stream {stm} exceeds the object limit"));
        }
        let data = match self.stream_data_in(st, &stream) {
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
        let mut entries = Slots::filled(len, Entry::free(0));
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
                fixed.insert(item.num, with_data_start(obj, item.stm_ofs));
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
                && self
                    .repair_objstm(&first, i, &obj, &mut entries, &mut changed)
                    .is_err()
            {
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
                Loaded::Failed(_) | Loaded::NeedsRepair(_) => {
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
                return Err(format!("corrupt object stream ({stm} 0 R)"));
            };
            i += 1;
        }
        Ok(())
    }
}

fn demote_if_failed(st: &State, num: i32, obj: Object) -> Object {
    match obj {
        Object::Stream(s) if st.demoted.lock().is_ok_and(|d| d.contains(&num)) => {
            Object::Dict(Arc::new(s.dict.clone()))
        }
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

/// How many bytes `fz_read_best` keeps when the stream fails while
/// producing byte `fail_at` (see `Document::read_all`).
fn kept_before_error(fail_at: usize, initial: usize) -> usize {
    let mut cap = initial.max(1024) + 1;
    let mut pos = 0;
    loop {
        if pos == cap {
            cap += cap >> 1;
        }
        let end = pos + (cap - pos).min(256);
        if fail_at < end {
            return pos;
        }
        pos = end;
    }
}

fn crypt_error(e: &str) -> OpenError {
    OpenError {
        code: "input.encryption",
        message: report::strip(e).to_string(),
    }
}

/// Re-point a cached stream at new data, or demote it to a plain dictionary
/// when the entry no longer has stream data.
fn with_data_start(obj: Object, stm_ofs: i64) -> Object {
    match obj {
        Object::Stream(s) => match usize::try_from(stm_ofs) {
            Ok(start) if start > 0 => {
                let mut s = Arc::unwrap_or_clone(s);
                s.data_start = start;
                Object::Stream(Arc::new(s))
            }
            _ => Object::Dict(Arc::new(Arc::unwrap_or_clone(s).dict)),
        },
        other => other,
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
}
