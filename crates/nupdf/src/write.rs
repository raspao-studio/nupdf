// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-write.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Writing a whole document (`pdf_save_document`), as MuPDF does: every
//! object in number order, streams copied raw unless the stream options
//! decode or encode them ([`crate::deflate`], [`crate::fax_encode`]),
//! object streams and xref streams unpacked into plain objects and a
//! classic xref table, the security handler kept, and optionally garbage
//! collected first ([`crate::garbage`]). The output is the same byte for
//! byte as MuPDF's in reproducible mode, apart from the AES initialization
//! vectors (DECISIONS D18) and the comment naming the writer.

use std::collections::HashMap;
use std::sync::Arc;

use crate::crypt::{Crypt, NewMethod};
use crate::document::Document;
use crate::garbage::{self, Slot};
use crate::object::{Dict, ObjRef, Object, Stream};
use crate::print::{self, Options};
use crate::xref::Kind;

/// What happens to the security handler (`do_encrypt`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Encrypt {
    /// Kept as it is (`PDF_ENCRYPT_KEEP`).
    #[default]
    Keep,
    /// Removed, and everything written in the clear (`PDF_ENCRYPT_NONE`,
    /// `clean -D`).
    Remove,
    /// Replaced by a new handler (`clean -E`, `-O`, `-U`, `-P`).
    New {
        method: NewMethod,
        owner: String,
        user: String,
        permissions: i32,
    },
}

/// What `pdf_write_options` nupdf supports so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteOptions {
    /// Dictionaries a line per entry (`do_pretty`).
    pub pretty: bool,
    /// Hex strings for anything outside printable ASCII (`do_ascii`).
    pub ascii: bool,
    /// Garbage collection (`do_garbage`, `mutool clean -g` to `-gggg`): 1
    /// drops what the trailer does not reach, 2 also renumbers, 3 also
    /// merges duplicate objects, 4 also duplicate streams. Higher counts as 4.
    pub garbage: u8,
    /// Streams written decoded (`do_decompress`, `clean -d`), apart from
    /// XML metadata and JPEG 2000 images.
    pub decompress: bool,
    /// Streams with no filter deflated (`do_compress`, `clean -z`), or for
    /// a 1-bit image, CCITT G4 encoded.
    pub compress: bool,
    /// The same for images, which also keeps them from being decoded
    /// (`do_compress_images`, `clean -i`).
    pub compress_images: bool,
    /// The same for fonts (`do_compress_fonts`, `clean -f`).
    pub compress_fonts: bool,
    /// `compression_effort` (`clean -e`): 0 for zlib's default level,
    /// else 1 to 100 for levels 0 to 9. Values outside count as the nearest.
    pub effort: i32,
    /// Objects packed into object streams, and an xref stream instead of
    /// the table (`do_use_objstms`, `clean -Z`).
    pub objstms: bool,
    /// The security handler to write with.
    pub encrypt: Encrypt,
    /// Only the changes, appended to the file as a new revision
    /// (`do_incremental`).
    pub incremental: bool,
}

impl WriteOptions {
    /// Whether a stream's data can be written differently from the input's.
    pub fn changes_streams(&self) -> bool {
        self.decompress
            || self.compress
            || self.compress_images
            || self.compress_fonts
            || self.ascii
    }
}

/// Why the document could not be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteError {
    /// Report lines, as MuPDF prints the error that stops its save.
    pub message: String,
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WriteError {}

/// The comment MuPDF writes after the header (in reproducible mode,
/// without its version), naming nupdf instead.
pub const WRITER_COMMENT: &[u8] = b"% Written by nupdf\n\n";

/// An xref entry as the writer sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Type {
    Unset,
    Free,
    InUse,
    /// In the object stream numbered `stream`, at `index`.
    Packed {
        stream: usize,
        index: usize,
    },
}

/// The parts of `pdf_write_state` the default save uses.
struct State {
    out: Vec<u8>,
    tight: bool,
    ascii: bool,
    use_list: Vec<bool>,
    ofs_list: Vec<i64>,
    gen_list: Vec<i32>,
    crypt_object_number: i32,
    /// The handler objects are encrypted with, if any.
    crypt: Option<Arc<Crypt>>,
}

/// `pdf_save_document` with `options`, returning the bytes.
pub fn save(doc: &Document, options: &WriteOptions) -> Result<Vec<u8>, WriteError> {
    if options.incremental {
        return save_incremental(doc, options);
    }
    check_document(doc);
    let mut trailer = doc.trailer();
    let crypt = match &options.encrypt {
        Encrypt::Keep => doc.crypt(),
        Encrypt::Remove => {
            trailer.remove(b"Encrypt");
            None
        }
        Encrypt::New {
            method,
            owner,
            user,
            permissions,
        } => Some(Arc::new(new_encryption(
            doc,
            &mut trailer,
            *method,
            owner,
            user,
            *permissions,
        ))),
    };
    // MuPDF's null object is the null pointer: a null value counts as
    // absent wherever it checks for an object.
    let crypt_obj = trailer
        .get(b"Encrypt")
        .filter(|v| !matches!(v, Object::Null))
        .cloned();

    // preloadobjstms: objects in object streams become plain objects (or
    // free, if they cannot be loaded).
    let mut xref_len = doc.xref_len();
    let load = !doc.repair_attempted();
    let mut types = Vec::with_capacity(xref_len);
    let mut gens = Vec::with_capacity(xref_len);
    for num in 0..xref_len as i32 {
        let e = doc.xref_entry(num);
        let (t, g) = match e.kind {
            Kind::None => (Type::Unset, e.generation),
            Kind::Free => (Type::Free, e.generation),
            Kind::InFile => (Type::InUse, e.generation),
            // pdf_load_obj_stm marks a member that parses as null free,
            // keeping its generation (its index in the stream).
            Kind::InStream => match doc.load_object(num) {
                Ok(Some(Object::Null)) => (Type::Free, e.generation),
                Err(cause) if load => {
                    report(doc, &cause);
                    (Type::Free, 0)
                }
                _ => (Type::InUse, 0),
            },
        };
        types.push(t);
        gens.push(g);
    }

    // Object streams need PDF 1.5: MuPDF raises the version, and writes
    // it into the catalog when the catalog names one.
    let mut changed = HashMap::new();
    let mut version = doc.version().0;
    if options.objstms && version < 15 {
        version = 15;
        raise_catalog_version(doc, &mut trailer, &mut changed);
    }

    let garbage = options.garbage.min(4);
    let input_gens = gens.clone();
    let mut collected = None;
    if garbage >= 1 {
        let slots = (0..xref_len)
            .map(|num| match (changed.get(&num), types[num]) {
                (Some(o), _) => Slot::Loaded(o.clone(), None),
                (None, Type::InUse) => match doc.load_object(num as i32) {
                    Ok(Some(o)) => {
                        let data = match &o {
                            Object::Stream(s) => Some(Arc::clone(s)),
                            _ => None,
                        };
                        Slot::Loaded(o, data)
                    }
                    Ok(None) => Slot::Empty,
                    Err(_) => Slot::Failed(num as i32),
                },
                (None, _) => Slot::Empty,
            })
            .collect();
        let c = garbage::collect(doc, slots, &mut trailer, garbage)?;
        if garbage >= 2 {
            // Renumbered: entry 0 stays, and every other is in use.
            types.truncate(1);
            gens.truncate(1);
            types.resize(c.slots.len(), Type::InUse);
            gens.resize(c.slots.len(), 0);
        }
        xref_len = c.slots.len();
        changed.clear();
        collected = Some(c);
    }
    // The generation in each xref entry, which renumbering moves with the
    // entry: what decides whether an object can go in an object stream.
    let entry_gens: Vec<i32> = match &collected {
        Some(c) if garbage >= 2 => c
            .origin
            .iter()
            .map(|&o| input_gens.get(o as usize).copied().unwrap_or(0))
            .collect(),
        _ => input_gens,
    };

    // An object stream that failed to load has a free entry from then on,
    // although garbage collection still reaches its dictionary.
    for (num, t) in types.iter_mut().enumerate() {
        let input = match &collected {
            Some(c) => c.origin.get(num).copied().unwrap_or(0),
            None => num as i32,
        };
        if *t == Type::InUse && input > 0 && doc.is_demoted(input) {
            *t = Type::Free;
        }
    }
    let (slots, use_list) = match collected {
        Some(c) => (Some(c.slots), c.use_list),
        None => (None, vec![true; xref_len]),
    };
    let objects = Objects {
        doc,
        slots,
        changed,
        buffers: HashMap::new(),
    };
    let plan = Plan {
        trailer,
        crypt_obj,
        types,
        gens,
        entry_gens,
        use_list,
        xref_len,
        version,
        crypt,
    };
    finish(doc, options, objects, plan)
}

/// What `save` has worked out before writing.
struct Plan {
    trailer: Dict,
    crypt_obj: Option<Object>,
    types: Vec<Type>,
    gens: Vec<i32>,
    entry_gens: Vec<i32>,
    use_list: Vec<bool>,
    xref_len: usize,
    version: i32,
    /// The handler to write with.
    crypt: Option<Arc<Crypt>>,
}

/// The rest of `pdf_save_document`: gathering into object streams, then
/// writing the objects, the free list and the xref.
fn finish(
    doc: &Document,
    options: &WriteOptions,
    mut objects: Objects,
    plan: Plan,
) -> Result<Vec<u8>, WriteError> {
    let Plan {
        trailer,
        crypt_obj,
        mut types,
        mut gens,
        entry_gens,
        mut use_list,
        mut xref_len,
        version,
        crypt,
    } = plan;
    let garbage = options.garbage.min(4);
    let mut crypt_object_number = 0;
    if crypt.is_some()
        && let Some(Object::Ref(r)) = trailer.get(b"Encrypt")
    {
        crypt_object_number = r.num;
    }

    if options.objstms {
        let root = trailer
            .get(b"Root")
            .and_then(Object::as_ref)
            .map_or(0, |r| r.num);
        let info = trailer
            .get(b"Info")
            .and_then(Object::as_ref)
            .map_or(0, |r| r.num);
        let skip = [root, info, crypt_object_number];
        gather(&mut objects, &mut types, &entry_gens, skip, xref_len)?;
        // The object streams are new objects at the end, in use.
        let len = types.len();
        gens.resize(len, 0);
        use_list.resize(len, true);
        xref_len = len;
    }

    let n = xref_len + 3;
    use_list.resize(n, false);
    let mut st = State {
        // Usually about the input's size.
        out: Vec::with_capacity(doc.data().len().saturating_add(4096)),
        tight: !options.pretty,
        ascii: options.ascii,
        use_list,
        ofs_list: vec![0; n],
        gen_list: vec![0; n],
        crypt_object_number,
        crypt,
    };
    // flush_gathered gives a packed object its index as offset and
    // generation; an unused one, which is not written, keeps both.
    for (num, t) in types.iter().enumerate() {
        if let Type::Packed { index, .. } = *t {
            st.ofs_list[num] = index as i64;
            st.gen_list[num] = index as i32;
        }
    }
    // Truncate the xref after compacting and renumbering.
    if garbage >= 2 {
        while xref_len > 0 && !st.use_list[xref_len - 1] {
            xref_len -= 1;
        }
    }

    // writeobjects
    st.out
        .extend_from_slice(format!("%PDF-{}.{}\n", version / 10, version % 10).as_bytes());
    st.out.extend_from_slice(b"%\xC2\xB5\xC2\xB6\n");
    st.out.extend_from_slice(WRITER_COMMENT);
    for num in 0..xref_len {
        // dowriteobject
        let mut generation = match types[num] {
            Type::Unset => st.gen_list[num],
            Type::Free | Type::InUse | Type::Packed { .. } => gens[num],
        };
        // Renumbering resets every generation, which re-encrypting allows.
        if garbage >= 2 {
            generation = if num == 0 { 65535 } else { 0 };
        }
        // An object in an object stream has its index as its generation.
        if let Type::Packed { index, .. } = types[num] {
            generation = index as i32;
        }
        st.gen_list[num] = generation;
        if garbage >= 1 && !st.use_list[num] {
            continue;
        }
        match types[num] {
            Type::Packed { stream, .. } => st.ofs_list[num] = stream as i64,
            Type::InUse => {
                st.ofs_list[num] = st.out.len() as i64;
                let unenc = num as i32 == st.crypt_object_number;
                let obj = objects.get(num).map_err(|message| WriteError { message })?;
                write_object(
                    &mut objects,
                    &mut st,
                    options,
                    num,
                    generation,
                    unenc,
                    obj,
                    true,
                )?;
            }
            Type::Free | Type::Unset => st.use_list[num] = false,
        }
    }

    // Chain the free entries.
    let mut lastfree = 0;
    for num in 0..xref_len {
        if !st.use_list[num] {
            st.gen_list[num] += 1;
            st.ofs_list[lastfree] = num as i64;
            lastfree = num;
        }
    }
    st.gen_list[0] = 0xffff;

    let startxref = st.out.len();
    if options.objstms {
        write_xref_stream(
            &mut objects,
            &mut st,
            options,
            &types,
            &trailer,
            crypt_obj.as_ref(),
            xref_len,
            startxref,
        )?;
    } else {
        write_xref(&mut st, &trailer, crypt_obj.as_ref(), xref_len, startxref);
    }
    Ok(st.out)
}

/// `pdf_save_document` with `do_incremental`: the file as it was, then the
/// objects MuPDF's incremental section holds, and an xref for them, in the
/// style of the last xref read, whose trailer points back with /Prev. The
/// /ID is kept, as the rest of nupdf's writes keep it.
fn save_incremental(doc: &Document, options: &WriteOptions) -> Result<Vec<u8>, WriteError> {
    let refuse = |message: &str| {
        Err(WriteError {
            message: message.into(),
        })
    };
    if doc.is_new() {
        return refuse("Can't do incremental writes on a new document");
    }
    if doc.repair_attempted() {
        return refuse("Can't do incremental writes on a repaired file");
    }
    if options.garbage > 0 {
        return refuse("Can't do incremental writes with garbage collection");
    }
    if !matches!(options.encrypt, Encrypt::Keep) {
        return refuse("Can't do incremental writes when changing encryption");
    }
    if options.objstms {
        return refuse("incremental writes with object streams are not supported");
    }
    let data = doc.data();
    let mut out = data[..doc.file_size().min(data.len())].to_vec();
    let entries = doc.incremental_entries();
    // pdf_has_unsaved_changes: nothing more to write.
    if entries.is_empty() {
        return Ok(out);
    }
    out.push(b'\n');
    // writeobjects names the writer here too.
    out.extend_from_slice(WRITER_COMMENT);

    let trailer = doc.trailer();
    let crypt = doc.crypt();
    let crypt_object_number = match (&crypt, trailer.get(b"Encrypt")) {
        (Some(_), Some(Object::Ref(r))) => r.num,
        _ => 0,
    };
    let xref_len = doc.xref_len();
    let n = xref_len + 2;
    let mut st = State {
        out,
        tight: !options.pretty,
        ascii: options.ascii,
        use_list: vec![true; n],
        ofs_list: vec![0; n],
        gen_list: vec![0; n],
        crypt_object_number,
        crypt,
    };
    let mut objects = Objects {
        doc,
        slots: None,
        changed: HashMap::new(),
        buffers: HashMap::new(),
    };
    let incremental: Vec<bool> = {
        let mut v = vec![false; n];
        for &(num, _, _) in &entries {
            if let Some(slot) = usize::try_from(num).ok().filter(|&i| i < n) {
                v[slot] = true;
            }
        }
        v
    };
    for &(num, filled, generation) in &entries {
        let Some(i) = usize::try_from(num).ok().filter(|&i| i < xref_len) else {
            continue;
        };
        st.gen_list[i] = generation;
        if filled {
            st.ofs_list[i] = st.out.len() as i64;
            let unenc = num == st.crypt_object_number;
            let obj = objects.get(i).map_err(|message| WriteError { message })?;
            write_object(
                &mut objects,
                &mut st,
                options,
                i,
                generation,
                unenc,
                obj,
                true,
            )?;
        } else {
            st.use_list[i] = false;
        }
    }
    // Free entries in the increment cannot be reused.
    for (i, &inc) in incremental.iter().enumerate().take(xref_len) {
        if inc && !st.use_list[i] {
            st.gen_list[i] = 65535;
            st.ofs_list[i] = 0;
        }
    }

    let bias = doc.bias() as i64;
    let startxref = st.out.len();
    let runs = |len: usize, inc: &dyn Fn(usize) -> bool| -> Vec<(usize, usize)> {
        let mut runs = Vec::new();
        let mut from = 0;
        while from < len {
            while from < len && !inc(from) {
                from += 1;
            }
            let mut to = from;
            while to < len && inc(to) {
                to += 1;
            }
            if from < to {
                runs.push((from, to));
            }
            from = to;
        }
        runs
    };
    let prev = Object::Int(doc.startxref());
    if doc.last_xref_was_table() {
        st.out.extend_from_slice(b"xref\n");
        for (from, to) in runs(xref_len, &|i| incremental[i]) {
            st.out
                .extend_from_slice(format!("{from} {}\n", to - from).as_bytes());
            for i in from..to {
                let kind = if st.use_list[i] { 'n' } else { 'f' };
                st.out.extend_from_slice(
                    format!(
                        "{:010} {:05} {kind} \n",
                        st.ofs_list[i] - bias,
                        st.gen_list[i]
                    )
                    .as_bytes(),
                );
            }
        }
        st.out.push(b'\n');
        let mut t = trailer;
        t.put(b"Size", Object::Int(xref_len as i64));
        t.put(b"Prev", prev);
        t.remove(b"XRefStm");
        st.out.extend_from_slice(b"trailer\n");
        print::print(&Object::Dict(Arc::new(t)), options_of(&st), &mut st.out);
        st.out.push(b'\n');
    } else {
        // writexrefstream: a new object, in the increment too.
        let num = xref_len;
        st.use_list[num] = true;
        st.ofs_list[num] = startxref as i64;
        st.gen_list[num] = 0;
        let size = xref_len + 1;
        let mut dict = Dict::new();
        for key in [&b"Info"[..], b"Root", b"ID"] {
            if let Some(v) = present(trailer.get(key)) {
                dict.put(key, v.clone());
            }
        }
        match present(trailer.get(b"Encrypt")) {
            Some(Object::Ref(_)) => dict.put(
                b"Encrypt",
                Object::Ref(ObjRef::new(st.crypt_object_number, 0)),
            ),
            Some(o) => dict.put(b"Encrypt", o.clone()),
            None => {}
        }
        dict.put(b"Size", Object::Int(size as i64));
        dict.put(b"Prev", prev);
        dict.put(b"Type", Object::name(b"XRef"));
        let ints = |v: &[i64]| Object::Array(Arc::new(v.iter().map(|&i| Object::Int(i)).collect()));
        dict.put(b"W", ints(&[1, 4, 1]));
        let mut index = Vec::new();
        let mut rows = Vec::new();
        for (from, to) in runs(size, &|i| i == num || incremental[i]) {
            index.extend([from as i64, (to - from) as i64]);
            for i in from..to {
                let (kind, ofs): (u8, i64) = if st.use_list[i] {
                    (1, st.ofs_list[i] - bias)
                } else {
                    (0, st.ofs_list[i])
                };
                rows.push(kind);
                rows.extend_from_slice(&(ofs as i32).to_be_bytes());
                rows.push(st.gen_list[i] as u8);
            }
        }
        dict.put(b"Index", ints(&index));
        dict.put(b"Length", Object::Int(rows.len() as i64));
        objects.add_stream(num, dict, rows);
        let obj = objects.get(num).map_err(|message| WriteError { message })?;
        write_object(&mut objects, &mut st, options, num, 0, true, obj, false)?;
    }
    st.out
        .extend_from_slice(format!("startxref\n{}\n%%EOF\n", startxref as i64 - bias).as_bytes());
    Ok(st.out)
}

/// The new handler and /Encrypt for `clean -E`: the dictionary goes into
/// the trailer directly, after a new /ID. `mutool clean` keeps the /ID it
/// reads (`dont_regenerate_id`), and so never hands it to the encryption
/// step, which makes a new one, at random; nupdf derives it from the input.
fn new_encryption(
    doc: &Document,
    trailer: &mut Dict,
    method: NewMethod,
    owner: &str,
    user: &str,
    permissions: i32,
) -> Crypt {
    let seed = *blake3::hash(doc.data()).as_bytes();
    let mut h = blake3::Hasher::new_derive_key("nupdf 2026 file identifier");
    h.update(&seed);
    let bytes = *h.finalize().as_bytes();
    trailer.put(
        b"ID",
        Object::Array(Arc::new(vec![
            Object::String(Arc::from(&bytes[..16])),
            Object::String(Arc::from(&bytes[16..])),
        ])),
    );
    let crypt = Crypt::for_writing(
        method,
        owner,
        user,
        Some(&bytes[..16]),
        permissions,
        &seed,
        doc.diagnostics(),
    );
    trailer.put(b"Encrypt", Object::Dict(Arc::new(crypt.encryption_dict())));
    crypt
}

/// The version check before writing object streams: the catalog's
/// /Version, when it has one, becomes 1.5, where MuPDF puts it.
fn raise_catalog_version(doc: &Document, trailer: &mut Dict, changed: &mut HashMap<usize, Object>) {
    let one_five = Object::name(b"1.5");
    match trailer.get(b"Root").cloned() {
        Some(Object::Dict(root)) => {
            if present(root.get(b"Version")).is_some() {
                let mut root = (*root).clone();
                root.put(b"Version", one_five);
                trailer.put(b"Root", Object::Dict(Arc::new(root)));
            }
        }
        Some(Object::Ref(r)) => {
            let mut num = r.num;
            for _ in 0..10 {
                match doc.load_object(num) {
                    Ok(Some(Object::Ref(next))) => num = next.num,
                    Ok(Some(Object::Dict(root))) => {
                        if present(root.get(b"Version")).is_some() {
                            let mut root = (*root).clone();
                            root.put(b"Version", one_five);
                            if let Ok(num) = usize::try_from(num) {
                                changed.insert(num, Object::Dict(Arc::new(root)));
                            }
                        }
                        return;
                    }
                    _ => return,
                }
            }
        }
        _ => {}
    }
}

/// Most objects in one object stream, and the data that ends one early.
const OBJSTM_MAXOBJS: usize = 256;
const OBJSTM_MAXLEN: usize = 1 << 24;

/// `gather_to_objstms`: every object that can be packed goes into an
/// object stream, in number order, 256 at most to a stream. Streams,
/// objects whose xref entry has a generation other than 0, the trailer's
/// /Root and /Info, the encryption dictionary, bare integers and bare
/// references stay out. Unused objects are packed too: MuPDF does not
/// check, though it does not list them as in use.
fn gather(
    objects: &mut Objects,
    types: &mut Vec<Type>,
    entry_gens: &[i32],
    skip: [i32; 3],
    xref_len: usize,
) -> Result<(), WriteError> {
    let mut members: Vec<usize> = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();
    let mut content: Vec<u8> = Vec::new();
    let tight = Options {
        tight: true,
        ascii: false,
    };
    for num in 1..xref_len {
        if skip.contains(&(num as i32)) {
            continue;
        }
        // pdf_cache_object comes first: an object that does not load fails
        // the save, and so does an entry the xref never defined.
        let obj = match types[num] {
            Type::InUse => objects
                .load_member(num)
                .map_err(|message| WriteError { message })?,
            Type::Unset if objects.doc.has_unset_entry(num as i32) => {
                return Err(WriteError {
                    message: format!("format error: cannot find object in xref ({num} 0 R)"),
                });
            }
            Type::Unset => continue,
            Type::Free | Type::Packed { .. } => continue,
        };
        if entry_gens.get(num).copied().unwrap_or(0) != 0 {
            continue;
        }
        let obj = obj.unwrap_or(Object::Null);
        if matches!(obj, Object::Stream(_) | Object::Int(_) | Object::Ref(_)) {
            continue;
        }
        offsets.push(content.len());
        print::print(&obj, tight, &mut content);
        members.push(num);
        if members.len() == OBJSTM_MAXOBJS || content.len() > OBJSTM_MAXLEN {
            flush_gathered(objects, types, &mut members, &mut offsets, &mut content);
        }
    }
    flush_gathered(objects, types, &mut members, &mut offsets, &mut content);
    Ok(())
}

/// `flush_gathered`: the object stream for the gathered objects, as a new
/// object at the end.
fn flush_gathered(
    objects: &mut Objects,
    types: &mut Vec<Type>,
    members: &mut Vec<usize>,
    offsets: &mut Vec<usize>,
    content: &mut Vec<u8>,
) {
    if members.is_empty() {
        return;
    }
    let mut data = Vec::with_capacity(content.len() + members.len() * 12);
    for (num, at) in members.iter().zip(offsets.iter()) {
        data.extend_from_slice(format!("{num} {at} ").as_bytes());
    }
    let first = data.len();
    data.append(content);
    let mut dict = Dict::new();
    dict.put(b"First", Object::Int(first as i64));
    dict.put(b"N", Object::Int(members.len() as i64));
    dict.put(b"Type", Object::name(b"ObjStm"));
    dict.put(b"Length", Object::Int(data.len() as i64));
    let stream = types.len();
    objects.add_stream(stream, dict, data);
    types.push(Type::InUse);
    for (index, &num) in members.iter().enumerate() {
        types[num] = Type::Packed { stream, index };
    }
    members.clear();
    offsets.clear();
}

/// `writexrefstream` for a full save: a new object after the others, with
/// the trailer's entries and an entry per object of 1, 4 and 1 bytes. The
/// entries stop at `xref_len`, after truncation, while the stream takes the
/// number after every object: when renumbering leaves nothing, the stream
/// is not among its own entries.
#[allow(clippy::too_many_arguments)]
fn write_xref_stream(
    objects: &mut Objects,
    st: &mut State,
    options: &WriteOptions,
    types: &[Type],
    trailer: &Dict,
    crypt_obj: Option<&Object>,
    xref_len: usize,
    startxref: usize,
) -> Result<(), WriteError> {
    let num = types.len();
    let size = xref_len + 1;
    let need = size.max(num + 1);
    if st.use_list.len() < need {
        st.use_list.resize(need, false);
        st.ofs_list.resize(need, 0);
        st.gen_list.resize(need, 0);
    }
    st.use_list[num] = true;
    st.ofs_list[num] = startxref as i64;
    st.gen_list[num] = 0;

    let mut dict = Dict::new();
    for key in [&b"Info"[..], b"Root", b"ID"] {
        if let Some(v) = present(trailer.get(key)) {
            dict.put(key, v.clone());
        }
    }
    match crypt_obj {
        Some(Object::Ref(_)) => dict.put(
            b"Encrypt",
            Object::Ref(ObjRef::new(st.crypt_object_number, 0)),
        ),
        Some(o) => dict.put(b"Encrypt", o.clone()),
        None => {}
    }
    dict.put(b"Size", Object::Int(size as i64));
    dict.put(b"Type", Object::name(b"XRef"));
    let ints = |v: &[i64]| Object::Array(Arc::new(v.iter().map(|&i| Object::Int(i)).collect()));
    dict.put(b"W", ints(&[1, 4, 1]));
    dict.put(b"Index", ints(&[0, size as i64]));

    let mut data = Vec::with_capacity(6 * size);
    for n in 0..size {
        let kind: u8 = if !st.use_list[n] {
            0
        } else if matches!(types.get(n), Some(Type::Packed { .. })) {
            2
        } else {
            1
        };
        data.push(kind);
        data.extend_from_slice(&(st.ofs_list[n] as i32).to_be_bytes());
        data.push(st.gen_list[n] as u8);
    }
    dict.put(b"Length", Object::Int(data.len() as i64));
    objects.add_stream(num, dict, data);
    let obj = objects.get(num).map_err(|message| WriteError { message })?;
    write_object(objects, st, options, num, 0, true, obj, false)?;
    st.out
        .extend_from_slice(format!("startxref\n{startxref}\n%%EOF\n").as_bytes());
    Ok(())
}

/// What reading a written document back found: `clean`'s self-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    /// The output opened at all.
    pub reopened: bool,
    /// Opening it, or loading its objects, needed a repair.
    pub repaired: bool,
    /// Objects it has in use.
    pub objects: usize,
    /// Those that read back differently from the original, in order.
    pub mismatched: Vec<i32>,
}

impl Verification {
    pub fn verified(&self) -> bool {
        self.reopened && !self.repaired && self.mismatched.is_empty()
    }
}

/// Opens `written` (with `password`, as the original was) and compares it
/// with the original: the syntax of each object, apart from the stream
/// /Length the writer recomputes, and each stream's raw data. Without
/// garbage collection every object the output has in use is compared with
/// the original's of the same number. With it, objects are paired by where
/// the trailer's /Root, /Info, /ID and /Encrypt lead in each, since they
/// can be renumbered or merged; a reference that reads as null may have
/// become null. `mismatched` then names the original's objects, and 0 the
/// trailer.
pub fn verify(
    original: &Document,
    written: &[u8],
    password: Option<&str>,
    options: &WriteOptions,
) -> Verification {
    let Ok(again) = Document::open_with(written.to_vec(), original.limits()) else {
        return Verification {
            reopened: false,
            repaired: false,
            objects: 0,
            mismatched: Vec::new(),
        };
    };
    // The output opens with the input's password, unless the handler changed.
    let password = match &options.encrypt {
        Encrypt::Keep => password,
        Encrypt::Remove => None,
        Encrypt::New { user, .. } => Some(user.as_str()),
    };
    if let Some(password) = password {
        let _ = again.authenticate(password);
    }
    // -Z raises the catalog's /Version to 1.5, on purpose.
    let root = original
        .trailer()
        .get(b"Root")
        .and_then(Object::as_ref)
        .filter(|_| options.objstms)
        .map(|r| r.num);
    let (objects, mismatched) = if options.garbage >= 1 {
        let mut walk = Walk::new(original, &again, options.changes_streams());
        walk.root = root;
        walk.new_handler = options.encrypt != Encrypt::Keep;
        walk.run()
    } else {
        let mut objects = 0;
        let mut mismatched = Vec::new();
        let decoded = options.changes_streams();
        // Object streams and the xref stream the writer adds come after
        // the input's objects.
        let len = (again.xref_len()).min(original.xref_len());
        for num in 1..len as i32 {
            if matches!(again.xref_entry(num).kind, Kind::InFile | Kind::InStream) {
                objects += 1;
                let catalog = root == Some(num);
                let same = match (original.load_object(num), again.load_object(num)) {
                    (Ok(Some(Object::Stream(a))), Ok(Some(Object::Stream(b))))
                        if decoded && kept_as_written(original, &a, &again, &b) =>
                    {
                        let show = |d: &Dict| {
                            let dict = Object::Dict(Arc::new(stream_dict(d, decoded)));
                            print::to_string(&dict, Options::default())
                        };
                        show(&a.dict) == show(&b.dict)
                    }
                    _ => same_read(
                        as_read(original, num, decoded, catalog),
                        as_read(&again, num, decoded, catalog),
                        decoded,
                    ),
                };
                if !same {
                    mismatched.push(num);
                }
            }
        }
        (objects, mismatched)
    };
    Verification {
        reopened: true,
        repaired: again.repaired(),
        objects,
        mismatched,
    }
}

/// The original and the written document side by side, from the trailer.
struct Walk<'a> {
    original: &'a Document,
    again: &'a Document,
    /// Original object number to written object number.
    pairs: HashMap<i32, i32>,
    queue: Vec<(i32, i32)>,
    /// Streams compared decoded, as the options change their encoding.
    decoded: bool,
    /// The input's catalog, when its /Version may have been raised.
    root: Option<i32>,
    /// The trailer's /Encrypt and /ID were replaced or removed.
    new_handler: bool,
}

impl<'a> Walk<'a> {
    fn new(original: &'a Document, again: &'a Document, decoded: bool) -> Self {
        Walk {
            original,
            again,
            pairs: HashMap::new(),
            queue: Vec::new(),
            decoded,
            root: None,
            new_handler: false,
        }
    }

    fn run(mut self) -> (usize, Vec<i32>) {
        let (before, after) = (self.original.trailer(), self.again.trailer());
        let mut mismatched = Vec::new();
        for key in [&b"Root"[..], b"Info", b"ID", b"Encrypt"] {
            if self.new_handler && (key == b"ID" || key == b"Encrypt") {
                continue;
            }
            let a = before.get(key).cloned().unwrap_or(Object::Null);
            let b = after.get(key).cloned().unwrap_or(Object::Null);
            if !self.same(&a, &b) {
                mismatched.push(0);
            }
        }
        let mut objects = 0;
        while let Some((o, w)) = self.queue.pop() {
            objects += 1;
            if !self.same_object(o, w) {
                mismatched.push(o);
            }
        }
        mismatched.sort_unstable();
        mismatched.dedup();
        (objects, mismatched)
    }

    /// Pairs `o` with `w`; an object already paired must pair the same way.
    fn pair(&mut self, o: i32, w: i32) -> bool {
        match self.pairs.get(&o) {
            Some(&before) => before == w,
            None => {
                self.pairs.insert(o, w);
                self.queue.push((o, w));
                true
            }
        }
    }

    /// Whether reference `num` in the original leads to nothing, as
    /// garbage collection decides where it makes references null.
    fn reads_null(&self, mut num: i32) -> bool {
        for _ in 0..10 {
            if num <= 0 || num as usize >= self.original.xref_len() {
                return true;
            }
            match self.original.load_object(num) {
                Ok(Some(Object::Ref(r))) => num = r.num,
                Ok(Some(Object::Null) | None) | Err(_) => return true,
                Ok(Some(_)) => return false,
            }
        }
        true
    }

    fn same_object(&mut self, o: i32, w: i32) -> bool {
        let a = self
            .original
            .load_object(o)
            .ok()
            .flatten()
            .unwrap_or(Object::Null);
        let Ok(b) = self.again.load_object(w) else {
            return false;
        };
        let b = b.unwrap_or(Object::Null);
        if self.root == Some(o)
            && let (Object::Dict(x), Object::Dict(y)) = (&a, &b)
        {
            return self.same_dict(&x.without(b"Version"), &y.without(b"Version"));
        }
        match (&a, &b) {
            (Object::Stream(sa), Object::Stream(sb)) => {
                let same_data = (self.decoded
                    && kept_as_written(self.original, sa, self.again, sb))
                    || match (
                        content(self.original, sa, self.decoded),
                        content(self.again, sb, self.decoded),
                    ) {
                        (Ok(x), Ok(y)) => same_content(&x, &y, &sb.dict, self.decoded),
                        _ => false,
                    };
                same_data
                    && self.same_dict(
                        &stream_dict(&sa.dict, self.decoded),
                        &stream_dict(&sb.dict, self.decoded),
                    )
            }
            (Object::Stream(_), _) | (_, Object::Stream(_)) => false,
            _ => self.same(&a, &b),
        }
    }

    fn same(&mut self, a: &Object, b: &Object) -> bool {
        match (a, b) {
            (Object::Ref(x), Object::Ref(y)) => self.pair(x.num, y.num),
            (Object::Ref(x), Object::Null) => self.reads_null(x.num),
            (Object::Ref(_), _) | (_, Object::Ref(_)) => false,
            (Object::Array(x), Object::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| self.same(p, q))
            }
            (Object::Dict(x), Object::Dict(y)) => self.same_dict(x, y),
            _ => {
                let show = |o: &Object| print::to_string(o, Options::default());
                show(a) == show(b)
            }
        }
    }

    /// Dictionaries as a reader sees them: the order of keys aside.
    fn same_dict(&mut self, x: &Dict, y: &Dict) -> bool {
        if x.len() != y.len() {
            return false;
        }
        let sorted = |d: &Dict| -> Vec<(Arc<[u8]>, Object)> {
            let mut e: Vec<_> = d
                .iter()
                .map(|(k, v)| (Arc::<[u8]>::from(k), v.clone()))
                .collect();
            e.sort_by(|p, q| p.0.cmp(&q.0));
            e
        };
        let (ex, ey) = (sorted(x), sorted(y));
        ex.iter()
            .zip(ey.iter())
            .all(|((ka, va), (kb, vb))| ka == kb && self.same(va, vb))
    }
}

/// An object as a reader sees it: its syntax, and for a stream its data
/// (decoded when `decoded`) and dictionary.
type Read = Result<(String, Option<(Vec<u8>, Dict)>), String>;

fn as_read(doc: &Document, num: i32, decoded: bool, catalog: bool) -> Read {
    let show = |o: &Object| print::to_string(o, Options::default());
    match doc.load_object(num)? {
        Some(Object::Dict(d)) if catalog => {
            Ok((show(&Object::Dict(Arc::new(d.without(b"Version")))), None))
        }
        Some(Object::Stream(s)) => {
            let data = content(doc, &s, decoded)?;
            let dict = Object::Dict(Arc::new(stream_dict(&s.dict, decoded)));
            Ok((show(&dict), Some((data, s.dict.clone()))))
        }
        Some(o) => Ok((show(&o), None)),
        None => Ok((String::new(), None)),
    }
}

fn same_read(original: Read, again: Read, decoded: bool) -> bool {
    match (original, again) {
        (Ok((ta, da)), Ok((tb, db))) => {
            ta == tb
                && match (da, db) {
                    (None, None) => true,
                    (Some((a, _)), Some((b, written))) => same_content(&a, &b, &written, decoded),
                    _ => false,
                }
        }
        (Err(a), Err(b)) => a == b,
        _ => false,
    }
}

/// A stream's data for comparison: raw, or decoded as `pdf_load_stream`
/// reads it.
fn content(doc: &Document, s: &Stream, decoded: bool) -> Result<Vec<u8>, String> {
    if decoded {
        doc.stream_data(s).map_err(|e| e.report())
    } else {
        doc.raw_stream_data(s).map(std::borrow::Cow::into_owned)
    }
}

/// Whether the writer left a stream as it was: the same filters and
/// parameters, and the same data under them, which then decodes to the
/// same thing without being decoded. Most of a document's bytes are such
/// streams (its images, above all), and decoding each twice was most of
/// what a save with a stream option took.
fn kept_as_written(original: &Document, a: &Stream, again: &Document, b: &Stream) -> bool {
    same_encoding(&a.dict, &b.dict)
        && match (original.raw_stream_data(a), again.raw_stream_data(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
}

/// Whether two stream dictionaries give the same filters with the same
/// parameters, written out in the dictionaries themselves. An array of
/// one filter is the same as the filter alone, which is how the writer
/// leaves it.
fn same_encoding(a: &Dict, b: &Dict) -> bool {
    match (encoding(a), encoding(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// A stream's filters in order, each with its parameters as written (""
/// for none), or None where they cannot be told from the dictionary
/// alone: a reference among them can lead to different objects in two
/// documents (the writer edits a filter array it reaches through one), a
/// /Crypt filter decrypts by the object's number, and where /Filter and
/// /DecodeParms do not go together a reader decides what applies.
fn encoding(d: &Dict) -> Option<Vec<(Vec<u8>, String)>> {
    fn outright(o: &Object, depth: usize) -> bool {
        match o {
            _ if depth > 8 => false,
            Object::Ref(_) | Object::Stream(_) => false,
            Object::Array(items) => items.iter().all(|i| outright(i, depth + 1)),
            Object::Dict(d) => d.iter().all(|(_, v)| outright(v, depth + 1)),
            _ => true,
        }
    }
    let parms = |o: Option<&Object>| match o {
        None | Some(Object::Null) => Some(String::new()),
        Some(o @ Object::Dict(_)) if outright(o, 0) => {
            Some(print::to_string(o, Options::default()))
        }
        Some(_) => None,
    };
    let name = |o: &Object| match o {
        Object::Name(n) if &n[..] != b"Crypt" => Some(n.to_vec()),
        _ => None,
    };
    match (d.get(b"Filter"), d.get(b"DecodeParms")) {
        (None, None) => Some(Vec::new()),
        (None, Some(_)) => None,
        (Some(f @ Object::Name(_)), p) if !matches!(p, Some(Object::Array(_))) => {
            Some(vec![(name(f)?, parms(p)?)])
        }
        (Some(Object::Array(filters)), None | Some(Object::Null)) => filters
            .iter()
            .map(|f| Some((name(f)?, String::new())))
            .collect(),
        (Some(Object::Array(filters)), Some(Object::Array(each)))
            if filters.len() == each.len() =>
        {
            let pairs = filters.iter().zip(each.iter());
            pairs
                .map(|(f, p)| Some((name(f)?, parms(Some(p))?)))
                .collect()
        }
        _ => None,
    }
}

/// A stream dictionary for comparison: without /Length, which the writer
/// recomputes, nor, when the data is compared decoded, the filters.
fn stream_dict(d: &Dict, decoded: bool) -> Dict {
    let d = d.without(b"Length");
    if decoded {
        d.without(b"Filter").without(b"DecodeParms")
    } else {
        d
    }
}

/// Whether two streams' data agree, `written` being the written stream's
/// dictionary. A fax encoder reads only the pixels of each row, so the bits
/// that pad a row to a whole byte come back white.
fn same_content(original: &[u8], again: &[u8], written: &Dict, decoded: bool) -> bool {
    if !decoded
        || !written
            .get(b"Filter")
            .is_some_and(|f| f.is_name(b"CCITTFaxDecode"))
    {
        return original == again;
    }
    rows_padded(original.to_vec(), written) == rows_padded(again.to_vec(), written)
}

/// 1-bit rows of `written`'s /Columns with their padding bits set.
fn rows_padded(mut data: Vec<u8>, written: &Dict) -> Vec<u8> {
    let columns = written
        .get(b"DecodeParms")
        .and_then(Object::as_dict)
        .and_then(|p| p.get(b"Columns"))
        .map_or(0, Object::to_int);
    let (Ok(columns), true) = (usize::try_from(columns), columns % 8 != 0) else {
        return data;
    };
    let stride = columns.div_ceil(8);
    let pad = 0xffu8 >> (columns % 8);
    for row in data.chunks_mut(stride) {
        if row.len() == stride
            && let Some(last) = row.last_mut()
        {
            *last |= pad;
        }
    }
    data
}

/// `pdf_check_document`: loads every object, so repairs happen before
/// anything is written, reporting what fails. Edits come after it, as a
/// repair would lose them.
pub fn check_document(doc: &Document) {
    let mut num = 1;
    while num < doc.xref_len() as i32 {
        if matches!(doc.xref_entry(num).kind, Kind::InFile | Kind::InStream)
            && let Err(cause) = doc.load_object(num)
        {
            report(doc, &cause);
        }
        num += 1;
    }
}

/// A caught error, reported as MuPDF reports it.
fn report(doc: &Document, cause: &str) {
    for line in cause.lines().filter(|l| !l.is_empty()) {
        doc.diagnostics().warn("error.reported", line);
    }
}

fn options_of(st: &State) -> Options {
    Options {
        tight: st.tight,
        ascii: st.ascii,
    }
}

/// The objects as the writer sees them: garbage collection's working copy
/// when it ran, else the document's, with what writing has changed on top.
/// MuPDF changes a filter array it reaches through a reference in place,
/// so the change shows wherever that object is used or written later.
struct Objects<'a> {
    doc: &'a Document,
    slots: Option<Vec<Slot>>,
    changed: HashMap<usize, Object>,
    /// The data of the streams the writer makes: object streams and the
    /// xref stream (`stm_buf`).
    buffers: HashMap<usize, Vec<u8>>,
}

impl Objects<'_> {
    /// `pdf_load_object` for writing object `num`.
    fn get(&self, num: usize) -> Result<Option<Object>, String> {
        if let Some(o) = self.changed.get(&num) {
            return Ok(Some(o.clone()));
        }
        match &self.slots {
            Some(slots) => Ok(slots.get(num).and_then(Slot::object).cloned()),
            None => self.doc.load_object(num as i32),
        }
    }

    /// `pdf_cache_object` for gathering object `num`, which fails where
    /// loading it fails.
    fn load_member(&self, num: usize) -> Result<Option<Object>, String> {
        if let Some(o) = self.changed.get(&num) {
            return Ok(Some(o.clone()));
        }
        match &self.slots {
            Some(slots) => match slots.get(num) {
                Some(Slot::Loaded(o, _)) => Ok(Some(o.clone())),
                Some(Slot::Failed(original)) => self.doc.load_object(*original),
                Some(Slot::Empty) | None => Ok(None),
            },
            None => self.doc.load_object(num as i32),
        }
    }

    /// A stream the writer makes, with its data in memory.
    fn add_stream(&mut self, num: usize, dict: Dict, data: Vec<u8>) {
        let stream = Stream {
            dict,
            data_start: 0,
            num: num as i32,
        };
        self.changed.insert(num, Object::Stream(Arc::new(stream)));
        self.buffers.insert(num, data);
    }

    /// The stream as loaded, which locates the data in the input, when
    /// garbage collection has changed the dictionary.
    fn data(&self, num: usize) -> Option<&Stream> {
        self.slots.as_ref()?.get(num)?.data()
    }

    /// `pdf_resolve_indirect` of one reference.
    fn load(&self, num: i32) -> Object {
        if let Some(o) = usize::try_from(num).ok().and_then(|n| self.changed.get(&n)) {
            return o.clone();
        }
        match &self.slots {
            None => self.doc.object(num),
            Some(slots) => match usize::try_from(num).ok().and_then(|n| slots.get(n)) {
                Some(Slot::Loaded(o, _)) => Some(o.clone()),
                Some(Slot::Failed(original)) => self.doc.object(*original),
                Some(Slot::Empty) | None => None,
            },
        }
        .unwrap_or(Object::Null)
    }

    /// RESOLVE: the object `obj` leads to, and the number of the last
    /// reference on the way.
    fn resolve_num(&self, obj: &Object) -> (Object, Option<i32>) {
        let mut obj = obj.clone();
        let mut num = None;
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
                return (Object::Null, None);
            }
            num = Some(r.num);
            obj = self.load(r.num);
        }
        (obj, num)
    }

    fn resolve(&self, obj: &Object) -> Object {
        self.resolve_num(obj).0
    }

    /// `pdf_name_eq` against an interned name: resolving.
    fn name_is(&self, obj: Option<&Object>, name: &[u8]) -> bool {
        obj.is_some_and(|o| self.resolve(o).is_name(name))
    }

    /// Changes the array `value` holds or leads to, as MuPDF changes it in
    /// place: in the object it references, or in `value` itself.
    fn edit_array(&mut self, value: &mut Object, f: impl FnOnce(&mut Vec<Object>)) {
        let (resolved, num) = self.resolve_num(value);
        let Object::Array(mut items) = resolved else {
            return;
        };
        f(Arc::make_mut(&mut items));
        match num {
            Some(num) => {
                if let Ok(num) = usize::try_from(num) {
                    self.changed.insert(num, Object::Array(items));
                }
            }
            None => *value = Object::Array(items),
        }
    }
}

/// A value MuPDF holds as a pointer other than null.
fn present(obj: Option<&Object>) -> Option<&Object> {
    obj.filter(|o| !matches!(o, Object::Null))
}

/// A direct name, which is what MuPDF's pointer comparisons with an
/// interned name match.
fn is_direct(obj: Option<&Object>, name: &[u8]) -> bool {
    obj.is_some_and(|o| o.is_name(name))
}

fn is_image_filter(obj: &Object) -> bool {
    const IMAGE_FILTERS: [&[u8]; 8] = [
        b"CCITTFaxDecode",
        b"CCF",
        b"DCTDecode",
        b"DCT",
        b"RunLengthDecode",
        b"RL",
        b"JBIG2Decode",
        b"JPXDecode",
    ];
    IMAGE_FILTERS.iter().any(|n| obj.is_name(n))
}

/// Whether the filter, or a filter in the array, satisfies `test`; array
/// items are not resolved.
fn any_filter(objects: &Objects, filter: Option<&Object>, test: fn(&Object) -> bool) -> bool {
    let Some(f) = filter else { return false };
    if let Object::Name(_) = f {
        return test(f);
    }
    match objects.resolve(f) {
        Object::Array(items) => items.iter().any(test),
        _ => false,
    }
}

/// `pdf_is_image_stream`.
fn is_image_stream(objects: &Objects, d: &Dict) -> bool {
    (objects.name_is(d.get(b"Type"), b"XObject") && objects.name_is(d.get(b"Subtype"), b"Image"))
        || any_filter(objects, d.get(b"Filter"), is_image_filter)
        || (present(d.get(b"Width")).is_some() && present(d.get(b"Height")).is_some())
}

/// `is_font_stream`.
fn is_font_stream(objects: &Objects, d: &Dict) -> bool {
    objects.name_is(d.get(b"Type"), b"Font")
        || objects.name_is(d.get(b"Type"), b"FontDescriptor")
        || [&b"Length1"[..], b"Length2", b"Length3"]
            .iter()
            .any(|k| present(d.get(k)).is_some())
        || objects.name_is(d.get(b"Subtype"), b"Type1C")
        || objects.name_is(d.get(b"Subtype"), b"CIDFontType0C")
}

/// `is_jpx_stream`.
fn is_jpx_stream(objects: &Objects, d: &Dict) -> bool {
    any_filter(objects, d.get(b"Filter"), |o| o.is_name(b"JPXDecode"))
}

/// `is_xml_metadata`.
fn is_xml_metadata(objects: &Objects, d: &Dict) -> bool {
    objects.name_is(d.get(b"Type"), b"Metadata") && objects.name_is(d.get(b"Subtype"), b"XML")
}

/// `is_bitmap_stream`: a 1-bit gray image or an image mask whose data is
/// exactly its rows, as width and height.
fn bitmap_size(objects: &Objects, d: &Dict, len: usize) -> Option<(i32, i32)> {
    if !is_direct(d.get(b"Subtype"), b"Image") {
        return None;
    }
    let int = |key: &[u8]| d.get(key).map_or(0, |o| objects.resolve(o).to_int());
    let (w, h) = (int(b"Width"), int(b"Height"));
    // (size_t)stride * h, as C converts the ints.
    let stride = w.wrapping_add(7) >> 3;
    if (stride as i64 as u64).wrapping_mul(h as i64 as u64) != len as u64 {
        return None;
    }
    let resolved = |key: &[u8]| d.get(key).map_or(Object::Null, |o| objects.resolve(o));
    if matches!(resolved(b"ImageMask"), Object::Bool(true)) {
        return Some((w, h));
    }
    let one_bit = matches!(resolved(b"BitsPerComponent"), Object::Int(1));
    (one_bit && resolved(b"ColorSpace").is_name(b"DeviceGray")).then_some((w, h))
}

/// `isbinarystream`.
fn is_binary(data: &[u8]) -> bool {
    data.iter()
        .any(|&c| !matches!(c, b'\n' | b'\r' | b'\t') && !(32..=127).contains(&c))
}

/// `hexbuf`: two digits a byte, a newline every 32 bytes, and `>`.
fn hex(data: &[u8]) -> Vec<u8> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(data.len() * 2 + data.len() / 32 + 1);
    for (i, &c) in data.iter().enumerate() {
        out.push(DIGITS[usize::from(c >> 4)]);
        out.push(DIGITS[usize::from(c & 15)]);
        if i % 32 == 31 {
            out.push(b'\n');
        }
    }
    out.push(b'>');
    out
}

/// `addhexfilter`: ASCIIHexDecode in front of the filters.
fn add_hex_filter(objects: &mut Objects, d: &mut Dict) {
    let mut f = present(d.get(b"Filter")).cloned();
    let mut dp = present(d.get(b"DecodeParms")).cloned();
    let hex = Object::name(b"ASCIIHexDecode");
    let resolved = f.as_ref().map(|o| objects.resolve(o));
    match (&mut f, resolved) {
        (Some(value), Some(Object::Name(_))) => {
            *value = Object::Array(Arc::new(vec![hex, value.clone()]));
            if let Some(p) = &mut dp
                && matches!(objects.resolve(p), Object::Dict(_))
            {
                *p = Object::Array(Arc::new(vec![Object::Null, p.clone()]));
            }
        }
        (Some(value), Some(Object::Array(_))) => {
            objects.edit_array(value, |items| items.insert(0, hex));
            if let Some(p) = &mut dp {
                objects.edit_array(p, |items| items.insert(0, Object::Null));
            }
        }
        _ => f = Some(hex),
    }
    if let Some(f) = f {
        d.put(b"Filter", f);
    }
    if let Some(dp) = dp {
        d.put(b"DecodeParms", dp);
    }
}

/// `striphexfilter`: takes an ASCIIHexDecode off the front of the filters,
/// returning whether there was one. An array left with one filter becomes
/// that filter, with the first of /DecodeParms, or null when /DecodeParms
/// is not an array.
fn strip_hex_filter(objects: &mut Objects, d: &mut Dict) -> Result<bool, WriteError> {
    let Some(f) = present(d.get(b"Filter")).cloned() else {
        return Ok(false);
    };
    let dp = present(d.get(b"DecodeParms")).cloned();
    if f.is_name(b"ASCIIHexDecode") {
        d.remove(b"Filter");
        d.remove(b"DecodeParms");
        return Ok(true);
    }
    let Object::Array(items) = objects.resolve(&f) else {
        return Ok(false);
    };
    // The arrays as edited: in place, so the dictionary holds them too.
    let (mut f, mut dp) = (f, dp);
    let is_hex = is_direct(items.first(), b"ASCIIHexDecode");
    if is_hex {
        objects.edit_array(&mut f, |items| {
            items.remove(0);
        });
        if let Some(v) = d.get_mut(b"Filter") {
            *v = f.clone();
        }
        if let Some(p) = &mut dp
            && let Object::Array(parms) = objects.resolve(p)
        {
            if parms.is_empty() {
                return Err(WriteError {
                    message: "argument error: index out of bounds".into(),
                });
            }
            objects.edit_array(p, |items| {
                items.remove(0);
            });
            if let Some(v) = d.get_mut(b"DecodeParms") {
                *v = p.clone();
            }
        }
    }
    let Object::Array(items) = objects.resolve(&f) else {
        return Ok(is_hex);
    };
    if items.len() == 1 {
        d.put(b"Filter", items[0].clone());
        if let Some(p) = &dp {
            let first = match objects.resolve(p) {
                Object::Array(parms) => parms.first().cloned(),
                _ => None,
            };
            d.put(b"DecodeParms", first.unwrap_or(Object::Null));
        }
    } else if items.is_empty() {
        d.remove(b"Filter");
        d.remove(b"DecodeParms");
    }
    Ok(is_hex)
}

/// The deflate or CCITT G4 step of `copystream` and `expandstream`:
/// `data` encoded, and `d` saying how.
fn encode(
    objects: &Objects,
    d: &mut Dict,
    data: Vec<u8>,
    effort: i32,
    drop_parms: bool,
) -> Result<Vec<u8>, WriteError> {
    let limit = objects.doc.limits().max_decoded_bytes;
    if let Some((w, h)) = bitmap_size(objects, d, data.len()) {
        let stride = usize::try_from(w.wrapping_add(7) >> 3).unwrap_or(0);
        let out = crate::fax_encode::compress_g4(&data, w, h, stride, limit).ok_or_else(|| {
            WriteError {
                message: "limit error: fax data exceeds the size limit".into(),
            }
        })?;
        d.put(b"Filter", Object::name(b"CCITTFaxDecode"));
        let mut dp = Dict::new();
        dp.put(b"K", Object::Int(-1));
        dp.put(b"Columns", Object::Int(i64::from(w)));
        d.put(b"DecodeParms", Object::Dict(Arc::new(dp)));
        return Ok(out);
    }
    let effort = effort.clamp(0, 100);
    let level = if effort == 0 { -1 } else { effort * 9 / 100 };
    let out = crate::deflate::compress(&data, level).ok_or_else(|| WriteError {
        message: "library error: cannot deflate buffer".into(),
    })?;
    d.put(b"Filter", Object::name(b"FlateDecode"));
    if drop_parms {
        d.remove(b"DecodeParms");
    }
    Ok(out)
}

/// `writeobject`, for `obj` as it is to be written. Object streams are
/// left out unless the writer packs objects itself, and xref streams when
/// `skip_xrefs`.
#[allow(clippy::too_many_arguments)]
fn write_object(
    objects: &mut Objects,
    st: &mut State,
    options: &WriteOptions,
    num: usize,
    generation: i32,
    unenc: bool,
    obj: Option<Object>,
    skip_xrefs: bool,
) -> Result<(), WriteError> {
    let doc = objects.doc;
    let dict = match &obj {
        Some(Object::Dict(d)) => Some(d.as_ref()),
        Some(Object::Stream(s)) => Some(&s.dict),
        _ => None,
    };
    if let Some(d) = dict {
        let ty = d.get(b"Type");
        if ty.is_some_and(|t| {
            (t.is_name(b"ObjStm") && !options.objstms) || (skip_xrefs && t.is_name(b"XRef"))
        }) {
            st.use_list[num] = false;
            return Ok(());
        }
    }
    let crypt = if unenc { None } else { st.crypt.clone() };
    let num = num as i32;
    let encrypt = |data: &[u8]| -> Vec<u8> {
        crypt
            .as_ref()
            .map_or_else(|| data.to_vec(), |c| c.encrypt_data(num, generation, data))
    };
    let encrypt_ref: Option<print::Encrypt> = crypt.as_ref().map(|_| &encrypt as _);
    match &obj {
        Some(Object::Stream(s)) => {
            let mut deflate = options.compress;
            let mut expand = options.decompress;
            if options.compress_images && is_image_stream(objects, &s.dict) {
                (deflate, expand) = (true, false);
            }
            if options.compress_fonts && is_font_stream(objects, &s.dict) {
                (deflate, expand) = (true, false);
            }
            if is_xml_metadata(objects, &s.dict) || is_jpx_stream(objects, &s.dict) {
                (deflate, expand) = (false, false);
            }
            // pdf_copy_dict: the same entries, and the same values.
            let mut dict: Dict = s.dict.clone();
            let source = objects.data(num as usize).unwrap_or(s);
            let buffer = objects.buffers.get(&(num as usize)).cloned();
            let mut data = if expand {
                // expandstream: decoded as pdf_load_stream_number reads it.
                // MuPDF decodes the dictionary after garbage collection;
                // the input's, which leads to the same objects, is used.
                // A stream the writer made has no filter, so its data is
                // used as it is.
                dict.remove(b"Filter");
                dict.remove(b"DecodeParms");
                let data = match buffer {
                    Some(data) => data,
                    None => doc.stream_data(source).map_err(|e| WriteError {
                        message: e.report(),
                    })?,
                };
                if deflate {
                    encode(objects, &mut dict, data, options.effort, false)?
                } else {
                    data
                }
            } else {
                // copystream: the raw data.
                let mut data = match buffer {
                    Some(data) => data,
                    None => doc
                        .raw_stream_data(source)
                        .map_err(|message| WriteError { message })?
                        .into_owned(),
                };
                if deflate && strip_hex_filter(objects, &mut dict)? {
                    let mut hex = Dict::new();
                    hex.put(b"Filter", Object::name(b"ASCIIHexDecode"));
                    data = crate::filter::decode(
                        &hex,
                        &data,
                        &|o| o.clone(),
                        doc.diagnostics(),
                        &doc.limits(),
                    )
                    .map_err(|e| WriteError {
                        message: e.report(),
                    })?;
                }
                if deflate && present(dict.get(b"Filter")).is_none() {
                    data = encode(objects, &mut dict, data, options.effort, true)?;
                }
                data
            };
            if options.ascii && is_binary(&data) {
                data = hex(&data);
                add_hex_filter(objects, &mut dict);
            }
            let len = crypt
                .as_ref()
                .map_or(data.len(), |c| c.encrypted_len(data.len()));
            dict.put(b"Length", Object::Int(len as i64));
            st.out
                .extend_from_slice(format!("{num} {generation} obj\n").as_bytes());
            print::print_encrypted(
                &Object::Dict(Arc::new(dict)),
                options_of(st),
                encrypt_ref,
                &mut st.out,
            );
            st.out.extend_from_slice(b"\nstream\n");
            match &crypt {
                Some(c) => st
                    .out
                    .extend_from_slice(&c.encrypt_data(num, generation, &data)),
                None => st.out.extend_from_slice(&data),
            }
            st.out.extend_from_slice(b"\nendstream\nendobj\n\n");
        }
        Some(o) => {
            st.out
                .extend_from_slice(format!("{num} {generation} obj\n").as_bytes());
            print::print_encrypted(o, options_of(st), encrypt_ref, &mut st.out);
            st.out.extend_from_slice(b"\nendobj\n\n");
        }
        None => {
            // pdf_load_object's NULL, which is MuPDF's null object.
            st.out
                .extend_from_slice(format!("{num} {generation} obj\nnull\nendobj\n\n").as_bytes());
        }
    }
    Ok(())
}

/// `writexref` for a full save.
fn write_xref(
    st: &mut State,
    old: &Dict,
    crypt_obj: Option<&Object>,
    xref_len: usize,
    startxref: usize,
) {
    st.out.extend_from_slice(b"xref\n");
    st.out
        .extend_from_slice(format!("0 {xref_len}\n").as_bytes());
    for num in 0..xref_len {
        let kind = if st.use_list[num] { 'n' } else { 'f' };
        st.out.extend_from_slice(
            format!("{:010} {:05} {kind} \n", st.ofs_list[num], st.gen_list[num]).as_bytes(),
        );
    }
    st.out.push(b'\n');

    let mut trailer = Dict::new();
    trailer.put(b"Size", Object::Int(xref_len as i64));
    for key in [&b"Info"[..], b"Root", b"ID"] {
        if let Some(v) = old.get(key).filter(|v| !matches!(v, Object::Null)) {
            trailer.put(key, v.clone());
        }
    }
    match crypt_obj {
        Some(Object::Ref(_)) => trailer.put(
            b"Encrypt",
            Object::Ref(ObjRef::new(st.crypt_object_number, 0)),
        ),
        Some(o) => trailer.put(b"Encrypt", o.clone()),
        None => {}
    }
    st.out.extend_from_slice(b"trailer\n");
    print::print(
        &Object::Dict(Arc::new(trailer)),
        options_of(st),
        &mut st.out,
    );
    st.out.push(b'\n');
    st.out
        .extend_from_slice(format!("startxref\n{startxref}\n%%EOF\n").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(bytes: &[u8]) -> Document {
        Document::open(bytes.to_vec()).unwrap_or_else(|e| panic!("{e:?}"))
    }

    const MINIMAL: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>\nendobj\n\
4 0 obj\n<< /Length 5 >>\nstream\nBT ET\nendstream\nendobj\n\
xref\n0 5\n0000000000 65535 f \n0000000009 00000 n \n0000000058 00000 n \n\
0000000115 00000 n \n0000000207 00000 n \n\
trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n258\n%%EOF\n";

    fn incremental() -> WriteOptions {
        WriteOptions {
            incremental: true,
            ..Default::default()
        }
    }

    const VALID: &[u8] = include_bytes!("../../../corpus/smoke/minimal.pdf");

    #[test]
    fn an_incremental_save_appends_the_changes() {
        let doc = open(VALID);
        crate::edit::set_metadata(&doc, "Title", "inc");
        let out = save(&doc, &incremental()).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(&out[..VALID.len()], VALID);
        // What MuPDF's incremental save appends, with nupdf named.
        assert_eq!(
            String::from_utf8_lossy(&out[VALID.len()..]),
            "\n% Written by nupdf\n\n6 0 obj\n<</Title(inc)>>\nendobj\n\n\
             xref\n6 1\n0000000578 00000 n \n\n\
             trailer\n<</Root 1 0 R/Size 7/Info 6 0 R/Prev 377>>\nstartxref\n610\n%%EOF\n"
        );
        let again = open(&out);
        assert_eq!(again.revisions(), 2);
        assert_eq!(again.metadata("Title").as_deref(), Some("inc"));
    }

    #[test]
    fn an_unchanged_document_saves_as_it_was() {
        let out = save(&open(VALID), &incremental()).unwrap_or_default();
        assert_eq!(out, VALID);
    }

    #[test]
    fn an_xref_stream_file_gets_an_xref_stream_increment() {
        let doc = open(include_bytes!("../../../corpus/smoke/xref-stream.pdf"));
        crate::edit::set_metadata(&doc, "Title", "inc");
        let out = save(&doc, &incremental()).unwrap_or_else(|e| panic!("{e:?}"));
        let tail = String::from_utf8_lossy(&out);
        assert!(tail.contains(
            "<</Type/XRef/Info 8 0 R/Root 1 0 R/Size 10/Prev 422/W[1 4 1]/Index[8 2]/Length 12>>"
        ));
        assert_eq!(open(&out).metadata("Title").as_deref(), Some("inc"));
    }

    #[test]
    fn incremental_saves_refuse_what_mupdf_refuses() {
        let fail =
            |doc: &Document, options: WriteOptions| save(doc, &options).err().map(|e| e.message);
        let new = Document::create().unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            fail(&new, incremental()).as_deref(),
            Some("Can't do incremental writes on a new document")
        );
        let repaired = open(b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\ntrailer<</Root 1 0 R>>\n");
        assert_eq!(
            fail(&repaired, incremental()).as_deref(),
            Some("Can't do incremental writes on a repaired file")
        );
        let doc = open(VALID);
        let gc = WriteOptions {
            garbage: 1,
            ..incremental()
        };
        assert_eq!(
            fail(&doc, gc).as_deref(),
            Some("Can't do incremental writes with garbage collection")
        );
        let decrypt = WriteOptions {
            encrypt: Encrypt::Remove,
            ..incremental()
        };
        assert_eq!(
            fail(&doc, decrypt).as_deref(),
            Some("Can't do incremental writes when changing encryption")
        );
    }

    #[test]
    fn writes_objects_xref_and_trailer_like_mupdf() {
        let out = save(&open(MINIMAL), &WriteOptions::default()).unwrap_or_default();
        let text = String::from_utf8_lossy(&out);
        let expected_start = "%PDF-1.4\n%\u{b5}\u{b6}\n% Written by nupdf\n\n\
1 0 obj\n<</Type/Catalog/Pages 2 0 R>>\nendobj\n\n\
2 0 obj\n<</Type/Pages/Kids[3 0 R]/Count 1>>\nendobj\n\n\
3 0 obj\n<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R>>\nendobj\n\n\
4 0 obj\n<</Length 5>>\nstream\nBT ET\nendstream\nendobj\n\n\
xref\n0 5\n0000000000 65535 f \n";
        assert!(text.starts_with(expected_start), "{text}");
        assert!(text.contains("trailer\n<</Size 5/Root 1 0 R>>\nstartxref\n"));
        assert!(text.ends_with("%%EOF\n"));
    }

    #[test]
    fn the_output_reopens_with_the_same_objects() {
        let doc = open(MINIMAL);
        let out = save(&doc, &WriteOptions::default()).unwrap_or_default();
        let again = open(&out);
        assert!(!again.repaired());
        let show = |d: &Document, num| {
            d.object(num)
                .map(|o| print::to_string(&o, Options::default()))
                .unwrap_or_default()
        };
        for num in 1..5 {
            assert_eq!(show(&again, num), show(&doc, num), "object {num}");
        }
        let out2 = save(&again, &WriteOptions::default()).unwrap_or_default();
        assert_eq!(out, out2, "writing is a fixed point");
    }

    fn xref_section(out: &[u8]) -> String {
        let text = String::from_utf8_lossy(out);
        let start = text.rfind("\nxref\n").map_or(0, |i| i + 1);
        let end = text.rfind("\ntrailer").unwrap_or(text.len());
        text[start..end].to_string()
    }

    /// Every object, and every stream's raw data, as the reader sees them.
    fn contents(doc: &Document) -> Vec<(String, Vec<u8>)> {
        (1..doc.xref_len() as i32)
            .map(|num| {
                let obj = doc.object(num);
                let raw = match &obj {
                    Some(Object::Stream(s)) => doc
                        .raw_stream_data(s)
                        .map(|d| d.into_owned())
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                let mut shown = obj
                    .map(|o| print::to_string(&o, Options::default()))
                    .unwrap_or_default();
                // The written /Length is the encrypted length.
                if let Some(at) = shown.find("/Length ") {
                    shown.truncate(at);
                }
                (shown, raw)
            })
            .collect()
    }

    #[test]
    fn verification_reads_every_written_object_back() {
        let doc = open(include_bytes!(
            "../../../corpus/smoke/encrypted-aes-256.pdf"
        ));
        let out = save(&doc, &WriteOptions::default()).unwrap_or_default();
        let v = verify(&doc, &out, None, &WriteOptions::default());
        assert!(v.verified(), "{v:?}");
        assert!(v.objects > 0);

        // A stream whose data no longer matches is caught.
        let doc = open(MINIMAL);
        let mut out = save(&doc, &WriteOptions::default()).unwrap_or_default();
        let at = out.windows(5).position(|w| w == b"BT ET").unwrap_or(0);
        out[at..at + 5].copy_from_slice(b"BT EX");
        let v = verify(&doc, &out, None, &WriteOptions::default());
        assert_eq!(v.mismatched, vec![4]);
        assert!(!v.verified());

        let v = verify(&doc, b"not a pdf", None, &WriteOptions::default());
        assert!(!v.reopened && !v.verified());
    }

    #[test]
    fn encrypted_output_decrypts_to_the_same_content() {
        let files: [&[u8]; 6] = [
            include_bytes!("../../../corpus/smoke/encrypted-rc4-40.pdf"),
            include_bytes!("../../../corpus/smoke/encrypted-rc4-128.pdf"),
            include_bytes!("../../../corpus/smoke/encrypted-rc4-crypt-filter.pdf"),
            include_bytes!("../../../corpus/smoke/encrypted-aes-128.pdf"),
            include_bytes!("../../../corpus/smoke/encrypted-aes-256-r5.pdf"),
            include_bytes!("../../../corpus/smoke/encrypted-aes-256.pdf"),
        ];
        for (i, file) in files.into_iter().enumerate() {
            let doc = open(file);
            let out = save(&doc, &WriteOptions::default()).unwrap_or_default();
            let again = open(&out);
            assert!(!again.repaired(), "file {i}");
            assert_eq!(again.encryption(), doc.encryption(), "file {i}");
            assert_eq!(contents(&again), contents(&doc), "file {i}");
            let twice = save(&doc, &WriteOptions::default()).unwrap_or_default();
            assert_eq!(out, twice, "file {i}: AES IVs are derived, not random");
        }
    }

    #[test]
    fn a_null_object_stream_member_is_written_free() {
        let doc = open(include_bytes!(
            "../../../corpus/smoke/write-objstm-null.pdf"
        ));
        let out = save(&doc, &WriteOptions::default()).unwrap_or_default();
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("\n8 0 obj"), "{text}");
        assert!(!text.contains("/ObjStm"), "object streams are not copied");
        // Free entries chain in order; the member's generation was its index
        // in the object stream, plus one when it was freed.
        assert!(
            xref_section(&out)
                .ends_with("0000000007 00001 f \n0000000008 00001 f \n0000000000 00004 f \n"),
            "{}",
            xref_section(&out)
        );
    }

    #[test]
    fn free_generations_count_up_like_mupdf() {
        let doc = open(include_bytes!(
            "../../../corpus/smoke/write-free-generations.pdf"
        ));
        let out = save(&doc, &WriteOptions::default()).unwrap_or_default();
        // 7 becomes 8; 65535 is written as 65536, as MuPDF's int list does;
        // 65536 was read as a 16-bit 0, so it becomes 1.
        assert!(
            xref_section(&out)
                .ends_with("0000000007 00008 f \n0000000008 65536 f \n0000000000 00001 f \n"),
            "{}",
            xref_section(&out)
        );
    }

    #[test]
    fn null_trailer_entries_are_left_out() {
        let doc = open(include_bytes!(
            "../../../corpus/smoke/write-null-trailer.pdf"
        ));
        let out = save(&doc, &WriteOptions::default()).unwrap_or_default();
        let text = String::from_utf8_lossy(&out);
        let trailer = &text[text.rfind("trailer").unwrap_or(0)..];
        assert!(trailer.starts_with("trailer\n<</Size "), "{trailer}");
        assert!(
            !trailer.contains("/ID") && !trailer.contains("/Info"),
            "{trailer}"
        );
    }

    fn garbage(level: u8) -> WriteOptions {
        WriteOptions {
            garbage: level,
            ..Default::default()
        }
    }

    /// The `num 0 obj` lines of `out` with the line after each.
    fn objects_of(out: &[u8]) -> Vec<String> {
        let text = String::from_utf8_lossy(out);
        let lines: Vec<&str> = text.lines().collect();
        lines
            .windows(2)
            .filter(|w| w[0].ends_with(" 0 obj"))
            .map(|w| format!("{} {}", w[0], w[1]))
            .collect()
    }

    const REACH: &[u8] = include_bytes!("../../../corpus/smoke/gc-reach.pdf");

    #[test]
    fn garbage_collection_keeps_what_the_trailer_reaches() {
        let doc = open(REACH);
        let out = save(&doc, &garbage(1)).unwrap_or_default();
        let objects = objects_of(&out);
        let text = String::from_utf8_lossy(&out);
        // Unreachable objects, and the /Length object once baked, are gone;
        // the rest keep their numbers.
        assert_eq!(
            objects
                .iter()
                .map(|o| o.split(' ').next().unwrap_or(""))
                .collect::<Vec<_>>(),
            ["1", "2", "3", "4", "5"],
            "{text}"
        );
        assert!(text.contains("<</Length 40>>"), "{text}");
        // A reference to a missing, free or null object, or to a reference
        // to nothing, becomes null wherever it appears.
        assert!(
            text.contains("/Missing null/Free null/Null null/Chain null>>"),
            "{text}"
        );
        assert!(text.contains("/Later null>>"), "{text}");
        assert_eq!(xref_section(&out).lines().count(), 2 + 14, "numbers kept");
        assert!(verify(&doc, &out, None, &garbage(1)).verified());
    }

    #[test]
    fn garbage_collection_renumbers_from_one() {
        let doc = open(REACH);
        let out = save(&doc, &garbage(2)).unwrap_or_default();
        assert!(xref_section(&out).starts_with("xref\n0 6\n0000000000 65535 f \n"));
        assert!(!xref_section(&out).contains(" 00001 "), "generation 0");
        let v = verify(&doc, &out, None, &garbage(2));
        assert!(v.verified(), "{v:?}");
        assert_eq!(v.objects, 5);
    }

    #[test]
    fn duplicates_merge_into_the_first_but_pages_never_do() {
        let doc = open(include_bytes!("../../../corpus/smoke/gc-duplicates.pdf"));
        let at3 = objects_of(&save(&doc, &garbage(3)).unwrap_or_default());
        // The fonts merge, then the resources that differed only by font;
        // the streams and the reordered dictionaries do not.
        assert_eq!(at3.len(), 10, "{at3:#?}");
        assert!(at3.contains(&"4 0 obj <</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources 7 0 R/Contents 6 0 R>>".to_string()), "{at3:#?}");
        let at4 = objects_of(&save(&doc, &garbage(4)).unwrap_or_default());
        // -gggg merges the streams too, which leaves the pages alike.
        assert_eq!(at4.len(), 9, "{at4:#?}");
        assert_eq!(at4[2][2..], at4[3][2..], "pages alike, kept apart");
        for level in [3, 4] {
            let out = save(&doc, &garbage(level)).unwrap_or_default();
            assert!(verify(&doc, &out, None, &garbage(level)).verified());
        }
    }

    #[test]
    fn renumbering_quirks_are_written_and_refused_on_reading_back() {
        let doc = open(include_bytes!("../../../corpus/smoke/gc-renumber.pdf"));
        let out = save(&doc, &garbage(2)).unwrap_or_default();
        let text = String::from_utf8_lossy(&out);
        // The 101-entry dictionary is sorted by the put.
        assert!(text.contains("<</A 9 0 R/K000 0/K001 1"), "{text}");
        // The interned last key gains a duplicate, the old number first.
        assert!(text.contains("/K100 100/ca 11 0 R/ca 10 0 R>>"), "{text}");
        // The reference the sort moved last is renumbered twice.
        assert!(text.contains("/K099 99/Z 10 0 R>>"), "{text}");
        let v = verify(&doc, &out, None, &garbage(2));
        assert_eq!(v.mismatched, [8, 12], "{v:?}");
        // Without renumbering the output reads back the same.
        let out = save(&doc, &garbage(1)).unwrap_or_default();
        assert!(verify(&doc, &out, None, &garbage(1)).verified());
    }

    #[test]
    fn renumbered_objects_are_encrypted_under_their_new_numbers() {
        let doc = open(include_bytes!("../../../corpus/smoke/gc-encrypted.pdf"));
        let out = save(&doc, &garbage(2)).unwrap_or_default();
        let again = open(&out);
        assert!(!again.repaired());
        assert_eq!(again.metadata("Title").as_deref(), Some("Renumbered"));
        assert!(verify(&doc, &out, None, &garbage(2)).verified());
        assert_eq!(save(&doc, &garbage(2)).ok(), Some(out), "deterministic");
    }

    #[test]
    fn a_document_without_a_root_collects_to_nothing() {
        let doc = open(
            b"%PDF-1.4\n1 0 obj\n<</A 1>>\nendobj\n\
xref\n0 2\n0000000000 65535 f \n0000000009 00000 n \n\
trailer\n<</Size 2>>\nstartxref\n34\n%%EOF\n",
        );
        let out = save(&doc, &garbage(1)).unwrap_or_default();
        assert!(objects_of(&out).is_empty());
        assert!(xref_section(&out).starts_with("xref\n0 2\n"));
        // Renumbering truncates the xref to nothing, which does not open.
        let out = save(&doc, &garbage(2)).unwrap_or_default();
        assert!(xref_section(&out).starts_with("xref\n0 0\n"));
        assert!(!verify(&doc, &out, None, &garbage(2)).reopened);
    }

    fn streams(options: WriteOptions) -> WriteOptions {
        WriteOptions {
            pretty: false,
            ..options
        }
    }

    /// Object `num` of `out` as read back, and its stream data decoded.
    fn stream_of(out: &[u8], num: i32) -> (String, Vec<u8>) {
        let again = open(out);
        let Some(Object::Stream(s)) = again.load_object(num).ok().flatten() else {
            return (String::new(), Vec::new());
        };
        let dict = print::to_string(&Object::Dict(Arc::new(s.dict.clone())), Options::default());
        (dict, again.stream_data(&s).unwrap_or_default())
    }

    const RAW: &[u8] = include_bytes!("../../../corpus/smoke/streams-raw.pdf");
    const HEX: &[u8] = include_bytes!("../../../corpus/smoke/streams-hex.pdf");

    #[test]
    fn compress_deflates_streams_without_a_filter() {
        let options = streams(WriteOptions {
            compress: true,
            ..Default::default()
        });
        let out = save(&open(MINIMAL), &options).unwrap_or_default();
        let (dict, data) = stream_of(&out, 4);
        assert_eq!(dict, "<</Length 13/Filter/FlateDecode>>");
        assert_eq!(data, b"BT ET");
        assert!(verify(&open(MINIMAL), &out, None, &options).verified());
    }

    #[test]
    fn one_bit_images_are_fax_encoded_and_others_deflated() {
        let doc = open(RAW);
        let options = streams(WriteOptions {
            compress: true,
            ..Default::default()
        });
        let out = save(&doc, &options).unwrap_or_default();
        let filter = |num| {
            let (dict, _) = stream_of(&out, num);
            let at = dict.find("/Filter").unwrap_or(dict.len());
            dict[at..].to_string()
        };
        assert_eq!(
            filter(6),
            "/Filter/CCITTFaxDecode/DecodeParms<</K -1/Columns 77>>>>"
        );
        assert_eq!(
            filter(7),
            "/Filter/CCITTFaxDecode/DecodeParms<</K -1/Columns 3000>>>>"
        );
        assert_eq!(filter(8), "/Filter/FlateDecode>>");
        // A byte short, and /Subtype by reference: deflated.
        assert_eq!(filter(9), "/Filter/FlateDecode>>");
        assert_eq!(filter(13), "/Filter/FlateDecode>>");
        // /BitsPerComponent by reference still counts.
        assert!(filter(16).starts_with("/Filter/CCITTFaxDecode"));
        // XML metadata stays as it is.
        assert_eq!(filter(10), "");
        // The rows decode back, but for the black bits that pad them.
        assert!(verify(&doc, &out, None, &options).verified());
        let (_, rows) = stream_of(&out, 6);
        assert_eq!(rows.len(), 10 * 40);
        assert_eq!(rows[9] & 0x07, 0x07);
    }

    /// A document with an unfiltered content stream (4), a deflated
    /// stream holding `payload` (5) and one whose filters are in an array
    /// of their own (6, 7).
    fn kept_streams(payload: &[u8]) -> Vec<u8> {
        let deflated = crate::deflate::compress(payload, -1).unwrap_or_default();
        crate::testutil::pdf_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R/Extra[5 0 R 6 0 R]>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 10 10]/Contents 4 0 R>>".to_vec(),
            crate::testutil::stream("", b"0 0 m 5 5 l S"),
            crate::testutil::stream("/Filter/FlateDecode", &deflated),
            crate::testutil::stream("/Filter 7 0 R", &deflated),
            b"[/FlateDecode]".to_vec(),
        ])
    }

    #[test]
    fn streams_kept_as_written_are_compared_without_decoding() {
        // Stream 5 decodes to more than the limit, so it cannot be compared
        // decoded; it is written as it was, which its bytes show.
        let limits = crate::Limits {
            max_decoded_bytes: 4096,
            ..Default::default()
        };
        let deflated = crate::deflate::compress(&[b'x'; 100_000], -1).unwrap_or_default();
        let bytes = crate::testutil::pdf_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R/Extra 5 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 10 10]/Contents 4 0 R>>".to_vec(),
            crate::testutil::stream("", b"0 0 m 5 5 l S"),
            crate::testutil::stream("/Filter/FlateDecode", &deflated),
        ]);
        let doc = Document::open_with(bytes, limits).unwrap_or_else(|e| panic!("{e:?}"));
        assert!(doc.stream_data(&stream_at(&doc, 5)).is_err());
        for level in [0, 1, 2, 4] {
            let options = streams(WriteOptions {
                compress: true,
                garbage: level,
                ..Default::default()
            });
            let out = save(&doc, &options).unwrap_or_default();
            if level < 2 {
                // The content stream was deflated, the other left alone.
                assert!(stream_of(&out, 4).0.contains("/Filter/FlateDecode"));
            }
            let v = verify(&doc, &out, None, &options);
            assert!(v.verified(), "-g {level}: {v:?}");
            assert_eq!(v.objects, 5);
        }
    }

    fn stream_at(doc: &Document, num: i32) -> Arc<Stream> {
        match doc.load_object(num) {
            Ok(Some(Object::Stream(s))) => s,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_stream_changed_under_the_same_filter_is_found() {
        let doc = open(&kept_streams(b"what was there"));
        let other = kept_streams(b"what is not");
        for level in [0, 1] {
            let options = streams(WriteOptions {
                compress: true,
                garbage: level,
                ..Default::default()
            });
            let v = verify(&doc, &other, None, &options);
            assert_eq!(v.mismatched, [5, 6], "-g {level}");
            let same = verify(&doc, &kept_streams(b"what was there"), None, &options);
            assert!(same.verified(), "-g {level}: {same:?}");
        }
    }

    #[test]
    fn only_filters_written_in_the_dictionary_say_a_stream_was_kept() {
        let dict = |text: &str| {
            let doc = crate::testutil::doc_of(&["<</Type/Catalog>>", text]);
            match doc.load_object(2) {
                Ok(Some(Object::Dict(d))) => (*d).clone(),
                other => panic!("{other:?}"),
            }
        };
        let same = |a: &str, b: &str| same_encoding(&dict(a), &dict(b));
        assert!(same("<</Length 3>>", "<</Length 9/Type/XObject>>"));
        assert!(same("<</Filter/DCTDecode>>", "<</Filter/DCTDecode>>"));
        assert!(same(
            "<</Filter[/ASCII85Decode/FlateDecode]/DecodeParms[null<</Predictor 12/Columns 4>>]>>",
            "<</DecodeParms[null<</Predictor 12/Columns 4>>]/Filter[/ASCII85Decode/FlateDecode]>>"
        ));
        // The writer unpacks an array of one filter.
        assert!(same(
            "<</Filter[/RunLengthDecode]>>",
            "<</Filter/RunLengthDecode>>"
        ));
        assert!(same(
            "<</Filter[/FlateDecode]/DecodeParms[<</Predictor 12>>]>>",
            "<</Filter/FlateDecode/DecodeParms<</Predictor 12>>>>"
        ));
        assert!(same(
            "<</Filter[/LZWDecode]/DecodeParms[null]>>",
            "<</Filter/LZWDecode>>"
        ));
        assert!(!same("<</Filter/DCTDecode>>", "<</Filter/FlateDecode>>"));
        assert!(!same("<</Filter/FlateDecode>>", "<<>>"));
        assert!(!same(
            "<</Filter[/A85/FlateDecode]>>",
            "<</Filter[/FlateDecode/A85]>>"
        ));
        assert!(!same(
            "<</Filter/FlateDecode/DecodeParms<</Predictor 12>>>>",
            "<</Filter/FlateDecode/DecodeParms<</Predictor 2>>>>"
        ));
        assert!(!same(
            "<</Filter/FlateDecode/DecodeParms<</Predictor 12>>>>",
            "<</Filter/FlateDecode>>"
        ));
        // A reference may lead elsewhere in the other document, and a
        // /Crypt filter decrypts by the object's number.
        assert!(!same("<</Filter 3 0 R>>", "<</Filter 3 0 R>>"));
        assert!(!same("<</Filter[3 0 R]>>", "<</Filter[3 0 R]>>"));
        assert!(!same(
            "<</Filter/JBIG2Decode/DecodeParms<</JBIG2Globals 3 0 R>>>>",
            "<</Filter/JBIG2Decode/DecodeParms<</JBIG2Globals 3 0 R>>>>"
        ));
        assert!(!same(
            "<</Filter[/Crypt/FlateDecode]>>",
            "<</Filter[/Crypt/FlateDecode]>>"
        ));
        // Where the two do not go together, a reader decides what applies:
        // MuPDF drops the parameters of the first of these.
        let odd = "<</Filter/FlateDecode/DecodeParms[<</Predictor 12>>]>>";
        assert!(!same(odd, odd));
        assert!(!same(
            "<</Filter[/FlateDecode]/DecodeParms<</Predictor 12>>>>",
            "<</Filter[/FlateDecode]/DecodeParms<</Predictor 12>>>>"
        ));
        assert!(!same(
            "<</DecodeParms<</Predictor 12>>>>",
            "<</DecodeParms<</Predictor 12>>>>"
        ));
    }

    #[test]
    fn fonts_and_images_compress_alone_and_stay_encoded() {
        let doc = open(RAW);
        let options = streams(WriteOptions {
            decompress: true,
            compress_fonts: true,
            ..Default::default()
        });
        let out = save(&doc, &options).unwrap_or_default();
        assert!(stream_of(&out, 12).0.contains("/Filter/FlateDecode"));
        assert!(!stream_of(&out, 8).0.contains("/Filter"));
        assert!(verify(&doc, &out, None, &options).verified());
    }

    #[test]
    fn decompress_writes_streams_decoded() {
        let doc = open(include_bytes!("../../../corpus/smoke/flate-content.pdf"));
        let options = streams(WriteOptions {
            decompress: true,
            ..Default::default()
        });
        let out = save(&doc, &options).unwrap_or_default();
        let (dict, data) = stream_of(&out, 4);
        assert!(!dict.contains("/Filter"), "{dict}");
        assert!(String::from_utf8_lossy(&out).contains("(Compressed) Tj"));
        assert!(!data.is_empty());
        assert!(verify(&doc, &out, None, &options).verified());
    }

    #[test]
    fn ascii_writes_binary_streams_in_hex() {
        assert_eq!(hex(&[0xab; 33]).len(), 33 * 2 + 1 + 1);
        assert_eq!(&hex(&[0xab; 33])[62..], b"ab\nab>");
        assert!(!is_binary(b"text\r\n\t~\x7f"));
        assert!(is_binary(b"\x80"));
        let doc = open(HEX);
        let options = streams(WriteOptions {
            ascii: true,
            ..Default::default()
        });
        let out = save(&doc, &options).unwrap_or_default();
        let text = String::from_utf8_lossy(&out);
        // A filter name becomes an array; an array gets ASCIIHexDecode in
        // front, with null in front of its parameters.
        assert!(
            stream_of(&out, 16)
                .0
                .contains("/Filter[/ASCIIHexDecode/FlateDecode]/DecodeParms[null<</Predictor 1>>]")
        );
        // Streams 11 and 12 share array 22, which is edited for each.
        assert!(text.contains("22 0 obj\n[/ASCIIHexDecode/ASCIIHexDecode/FlateDecode]"));
        assert!(!verify(&doc, &out, None, &options).verified());
    }

    #[test]
    fn compress_takes_hex_off_first() {
        let doc = open(HEX);
        let options = streams(WriteOptions {
            compress: true,
            ..Default::default()
        });
        let out = save(&doc, &options).unwrap_or_default();
        assert!(stream_of(&out, 6).0.contains("/Filter/FlateDecode"));
        assert!(
            stream_of(&out, 7)
                .0
                .starts_with("<</Filter/FlateDecode/DecodeParms<</Predictor 1>>/Length")
        );
        // An array of one filter is unpacked, and /DecodeParms, a dictionary,
        // becomes null.
        assert!(
            stream_of(&out, 8)
                .0
                .starts_with("<</Filter/FlateDecode/DecodeParms null/Length")
        );
        // The abbreviation stays.
        assert!(stream_of(&out, 13).0.contains("/Filter/AHx"));
        // Array 20 lost its ASCIIHexDecode to stream 9, so stream 10, whose
        // data is still hex, is now read as flate.
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("20 0 obj\n[/FlateDecode]"));
        assert!(text.contains("21 0 obj\n[null]"));
        assert!(!verify(&doc, &out, None, &options).verified());
    }

    #[test]
    fn hex_off_an_empty_parameter_array_fails_the_save() {
        let doc = open(include_bytes!(
            "../../../corpus/smoke/streams-hex-empty-parms.pdf"
        ));
        let options = WriteOptions {
            compress: true,
            ..Default::default()
        };
        assert_eq!(
            save(&doc, &options).err().map(|e| e.message),
            Some("argument error: index out of bounds".to_string())
        );
    }

    #[test]
    fn a_filter_the_lookup_misses_is_written_twice() {
        let doc = open(include_bytes!(
            "../../../corpus/smoke/streams-sorted-filter.pdf"
        ));
        let options = WriteOptions {
            compress: true,
            ..Default::default()
        };
        let out = save(&doc, &options).unwrap_or_default();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("/A100 100/Filter/FlateDecode/Filter/FlateDecode/Length"));
    }

    #[test]
    fn effort_picks_the_zlib_level() {
        let doc = open(RAW);
        let at = |effort| {
            let options = WriteOptions {
                compress: true,
                effort,
                ..Default::default()
            };
            save(&doc, &options).unwrap_or_default().len()
        };
        // 1 to 11 is level 0, stored; 100 is level 9; 0 is level 6.
        assert!(at(1) > at(0));
        assert!(at(100) <= at(0));
        assert_eq!(at(-5), at(0));
        assert_eq!(at(1000), at(100));
    }

    fn objstms(garbage: u8) -> WriteOptions {
        WriteOptions {
            objstms: true,
            garbage,
            ..Default::default()
        }
    }

    const PACK: &[u8] = include_bytes!("../../../corpus/smoke/objstm-pack.pdf");

    #[test]
    fn objstms_pack_what_can_be_packed() {
        let doc = open(PACK);
        let out = save(&doc, &objstms(0)).unwrap_or_default();
        let again = open(&out);
        let kind = |num| again.xref_entry(num).kind;
        // Left out: the catalog, /Info, an integer, a bare reference, an
        // object of generation 1, and the content stream.
        for num in [1, 6, 7, 8, 14, 4] {
            assert_eq!(kind(num), Kind::InFile, "{num}");
        }
        for num in [2, 3, 5, 9, 10, 11, 12, 13, 15, 314, 315] {
            assert_eq!(kind(num), Kind::InStream, "{num}");
        }
        // 256 to a stream: two new ones after the input's 316 objects, then
        // the xref stream.
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("316 0 obj\n<</Type/ObjStm/First"));
        assert!(text.contains("/N 256/Length"));
        assert!(text.contains("317 0 obj\n<</Type/ObjStm/First 413/N 53/Length"));
        assert!(text.contains(
            "318 0 obj\n<</Type/XRef/Info 6 0 R/Root 1 0 R/Size 319/W[1 4 1]/Index[0 319]"
        ));
        assert!(!text.contains("\ntrailer\n"));
        // The catalog's /Version is raised with the header.
        assert!(text.starts_with("%PDF-1.5\n"));
        assert!(text.contains("/Version/1.5"));
        // Packed with nothing between them; each reads back from its own
        // range of the stream.
        assert!(text.contains("truenull.5/Name"));
        assert!(verify(&doc, &out, None, &objstms(0)).verified());
        assert_eq!(save(&doc, &objstms(0)).ok(), Some(out), "deterministic");
    }

    #[test]
    fn objstm_output_reads_back() {
        for fixture in [
            &include_bytes!("../../../corpus/smoke/multipage-inherit.pdf")[..],
            include_bytes!("../../../corpus/smoke/encrypted-rc4-128.pdf"),
            include_bytes!("../../../corpus/smoke/xref-stream.pdf"),
        ] {
            let doc = open(fixture);
            for garbage in [0, 1, 2, 4] {
                let out = save(&doc, &objstms(garbage)).unwrap_or_default();
                let v = verify(&doc, &out, None, &objstms(garbage));
                assert!(v.verified(), "garbage {garbage}: {v:?}");
            }
        }
    }

    #[test]
    fn a_gap_in_the_only_section_fails_the_save() {
        let doc = open(include_bytes!("../../../corpus/smoke/objstm-gap.pdf"));
        assert_eq!(
            save(&doc, &objstms(0)).err().map(|e| e.message),
            Some("format error: cannot find object in xref (6 0 R)".into())
        );
        // Past the oldest section's size, the lookup finds nothing.
        let doc = open(include_bytes!(
            "../../../corpus/smoke/objstm-gap-update.pdf"
        ));
        assert!(save(&doc, &objstms(0)).is_ok());
    }

    #[test]
    fn an_empty_collection_leaves_the_xref_stream_unlisted() {
        let doc = open(
            b"%PDF-1.5\n1 0 obj\n<</A 1>>\nendobj\n\
xref\n0 2\n0000000000 65535 f \n0000000009 00000 n \n\
trailer\n<</Size 2>>\nstartxref\n34\n%%EOF\n",
        );
        let out = save(&doc, &objstms(2)).unwrap_or_default();
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("1 0 obj\n<</Type/XRef/Size 1/W[1 4 1]/Index[0 1]"),
            "{text}"
        );
    }

    fn encrypted(method: NewMethod) -> WriteOptions {
        WriteOptions {
            encrypt: Encrypt::New {
                method,
                owner: "own".into(),
                user: "usr".into(),
                permissions: -1,
            },
            ..Default::default()
        }
    }

    const ALL_METHODS: [NewMethod; 4] = [
        NewMethod::Rc4_40,
        NewMethod::Rc4_128,
        NewMethod::Aes128,
        NewMethod::Aes256,
    ];

    #[test]
    fn decrypting_writes_everything_in_the_clear() {
        let doc = open(include_bytes!(
            "../../../corpus/smoke/encrypted-rc4-128.pdf"
        ));
        let options = WriteOptions {
            encrypt: Encrypt::Remove,
            ..Default::default()
        };
        let out = save(&doc, &options).unwrap_or_default();
        let again = open(&out);
        assert!(!again.is_encrypted());
        assert!(!String::from_utf8_lossy(&out).contains("/Encrypt"));
        assert!(verify(&doc, &out, None, &options).verified());
    }

    #[test]
    fn new_encryption_opens_with_either_password() {
        let doc = open(include_bytes!(
            "../../../corpus/smoke/multipage-inherit.pdf"
        ));
        for method in ALL_METHODS {
            let options = encrypted(method);
            let out = save(&doc, &options).unwrap_or_default();
            let again = open(&out);
            assert!(again.needs_password(), "{method:?}");
            assert_eq!(again.authenticate("nope").ok(), Some(0), "{method:?}");
            assert_eq!(again.authenticate("usr").ok(), Some(2), "{method:?}");
            assert_eq!(
                again.authenticate("own").ok().map(|a| a & 4),
                Some(4),
                "{method:?}"
            );
            assert!(verify(&doc, &out, None, &options).verified(), "{method:?}");
            assert_eq!(
                save(&doc, &options).ok(),
                Some(out),
                "{method:?} deterministic"
            );
        }
    }

    #[test]
    fn new_encryption_writes_a_new_id_and_a_direct_dictionary() {
        let doc = open(include_bytes!(
            "../../../corpus/smoke/encrypted-rc4-128.pdf"
        ));
        let out = save(&doc, &encrypted(NewMethod::Aes256)).unwrap_or_default();
        let text = String::from_utf8_lossy(&out);
        let trailer = &text[text.rfind("trailer\n").unwrap_or(0)..];
        assert!(
            trailer.contains("/Encrypt<</Filter/Standard/R 6/V 5/Length 256/P -4"),
            "{trailer}"
        );
        assert_ne!(open(&out).trailer().get(b"ID"), doc.trailer().get(b"ID"));
        // Permissions keep only the bits MuPDF keeps.
        let options = WriteOptions {
            encrypt: Encrypt::New {
                method: NewMethod::Rc4_128,
                owner: "own".into(),
                user: String::new(),
                permissions: 0,
            },
            ..Default::default()
        };
        let out = save(&doc, &options).unwrap_or_default();
        assert!(String::from_utf8_lossy(&out).contains("/P -3904"));
        assert!(!open(&out).needs_password());
    }

    #[test]
    fn an_empty_stream_under_aes_does_not_read_back() {
        // MuPDF writes no data, but the /Length of an IV and a block.
        let doc = open(include_bytes!(
            "../../../corpus/smoke/write-empty-stream.pdf"
        ));
        let rc4 = encrypted(NewMethod::Rc4_128);
        let out = save(&doc, &rc4).unwrap_or_default();
        assert!(verify(&doc, &out, None, &rc4).verified());
        let aes = encrypted(NewMethod::Aes256);
        let out = save(&doc, &aes).unwrap_or_default();
        assert!(String::from_utf8_lossy(&out).contains("<</Length 32>>\nstream\n\nendstream"));
        assert!(!verify(&doc, &out, None, &aes).verified());
    }
}
