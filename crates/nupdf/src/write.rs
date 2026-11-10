// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-write.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Writing a whole document (`pdf_save_document`), as MuPDF does with its
//! default options: every object in number order, streams copied raw,
//! object streams and xref streams unpacked into plain objects and a
//! classic xref table, the security handler kept. The output is the same
//! byte for byte as MuPDF's in reproducible mode, apart from the AES
//! initialization vectors (DECISIONS D18) and the comment naming the
//! writer.

use std::sync::Arc;

use crate::document::Document;
use crate::object::{Dict, ObjRef, Object};
use crate::print::{self, Options};
use crate::xref::Kind;

/// What `pdf_write_options` nupdf supports so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteOptions {
    /// Dictionaries a line per entry (`do_pretty`).
    pub pretty: bool,
    /// Hex strings for anything outside printable ASCII (`do_ascii`).
    pub ascii: bool,
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
}

/// `pdf_save_document` with `options`, returning the bytes.
pub fn save(doc: &Document, options: &WriteOptions) -> Result<Vec<u8>, WriteError> {
    check_document(doc);
    let crypt = doc.crypt();
    let trailer = doc.trailer();
    // MuPDF's null object is the null pointer: a null value counts as
    // absent wherever it checks for an object.
    let crypt_obj = trailer
        .get(b"Encrypt")
        .filter(|v| !matches!(v, Object::Null))
        .cloned();

    // preloadobjstms: objects in object streams become plain objects (or
    // free, if they cannot be loaded).
    let xref_len = doc.xref_len();
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

    let n = xref_len + 3;
    let mut st = State {
        // Usually about the input's size.
        out: Vec::with_capacity(doc.data().len().saturating_add(4096)),
        tight: !options.pretty,
        ascii: options.ascii,
        use_list: vec![true; n],
        ofs_list: vec![0; n],
        gen_list: vec![0; n],
        crypt_object_number: 0,
    };
    if crypt.is_some()
        && let Some(Object::Ref(r)) = &crypt_obj
    {
        st.crypt_object_number = r.num;
    }

    // writeobjects
    let version = doc.version().0;
    st.out
        .extend_from_slice(format!("%PDF-{}.{}\n", version / 10, version % 10).as_bytes());
    st.out.extend_from_slice(b"%\xC2\xB5\xC2\xB6\n");
    st.out.extend_from_slice(WRITER_COMMENT);
    for num in 0..xref_len {
        // dowriteobject
        let gen_ = gens[num];
        st.gen_list[num] = match types[num] {
            Type::Unset => st.gen_list[num],
            Type::Free | Type::InUse => gen_,
        };
        if types[num] == Type::InUse {
            st.ofs_list[num] = st.out.len() as i64;
            let unenc = num as i32 == st.crypt_object_number;
            let generation = st.gen_list[num];
            write_object(doc, &mut st, num as i32, generation, unenc)?;
        } else {
            st.use_list[num] = false;
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
    write_xref(&mut st, &trailer, crypt_obj.as_ref(), xref_len, startxref);
    Ok(st.out)
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

/// Opens `written` (with `password`, as the original was) and compares each
/// object it has in use with the original's: its syntax, apart from the
/// stream /Length the writer recomputes, and a stream's raw data.
pub fn verify(original: &Document, written: &[u8], password: Option<&str>) -> Verification {
    let Ok(again) = Document::open_with(written.to_vec(), original.limits()) else {
        return Verification {
            reopened: false,
            repaired: false,
            objects: 0,
            mismatched: Vec::new(),
        };
    };
    if let Some(password) = password {
        let _ = again.authenticate(password);
    }
    let mut objects = 0;
    let mut mismatched = Vec::new();
    let mut num = 1;
    while num < again.xref_len() as i32 {
        if again.xref_entry(num).kind == Kind::InFile {
            objects += 1;
            if as_read(original, num) != as_read(&again, num) {
                mismatched.push(num);
            }
        }
        num += 1;
    }
    Verification {
        reopened: true,
        repaired: again.repaired(),
        objects,
        mismatched,
    }
}

/// An object as a reader sees it.
fn as_read(doc: &Document, num: i32) -> Result<(String, Option<Vec<u8>>), String> {
    let show = |o: &Object| print::to_string(o, Options::default());
    match doc.load_object(num)? {
        Some(Object::Stream(s)) => {
            let raw = doc.raw_stream_data(&s)?.into_owned();
            let dict = Object::Dict(Arc::new(s.dict.without(b"Length")));
            Ok((show(&dict), Some(raw)))
        }
        Some(o) => Ok((show(&o), None)),
        None => Ok((String::new(), None)),
    }
}

/// `pdf_check_document`: loads every object, so repairs happen before
/// anything is written, reporting what fails.
fn check_document(doc: &Document) {
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

fn options(st: &State) -> Options {
    Options {
        tight: st.tight,
        ascii: st.ascii,
    }
}

/// `writeobject`.
fn write_object(
    doc: &Document,
    st: &mut State,
    num: i32,
    generation: i32,
    unenc: bool,
) -> Result<(), WriteError> {
    let obj = doc
        .load_object(num)
        .map_err(|message| WriteError { message })?;
    let dict = match &obj {
        Some(Object::Dict(d)) => Some(d.as_ref()),
        Some(Object::Stream(s)) => Some(&s.dict),
        _ => None,
    };
    if let Some(d) = dict {
        let ty = d.get(b"Type");
        if ty.is_some_and(|t| t.is_name(b"ObjStm") || t.is_name(b"XRef")) {
            st.use_list[num as usize] = false;
            return Ok(());
        }
    }
    let crypt = if unenc { None } else { doc.crypt() };
    let encrypt = |data: &[u8]| -> Vec<u8> {
        crypt
            .as_ref()
            .map_or_else(|| data.to_vec(), |c| c.encrypt_data(num, generation, data))
    };
    let encrypt_ref: Option<print::Encrypt> = crypt.as_ref().map(|_| &encrypt as _);
    match &obj {
        Some(Object::Stream(s)) => {
            // copystream
            let raw = doc
                .raw_stream_data(s)
                .map_err(|message| WriteError { message })?;
            let mut dict: Dict = s.dict.clone();
            let len = crypt
                .as_ref()
                .map_or(raw.len(), |c| c.encrypted_len(raw.len()));
            dict.put(b"Length", Object::Int(len as i64));
            st.out
                .extend_from_slice(format!("{num} {generation} obj\n").as_bytes());
            print::print_encrypted(
                &Object::Dict(Arc::new(dict)),
                options(st),
                encrypt_ref,
                &mut st.out,
            );
            st.out.extend_from_slice(b"\nstream\n");
            match &crypt {
                Some(c) => st
                    .out
                    .extend_from_slice(&c.encrypt_data(num, generation, &raw)),
                None => st.out.extend_from_slice(&raw),
            }
            st.out.extend_from_slice(b"\nendstream\nendobj\n\n");
        }
        Some(o) => {
            st.out
                .extend_from_slice(format!("{num} {generation} obj\n").as_bytes());
            print::print_encrypted(o, options(st), encrypt_ref, &mut st.out);
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
    print::print(&Object::Dict(Arc::new(trailer)), options(st), &mut st.out);
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
        let v = verify(&doc, &out, None);
        assert!(v.verified(), "{v:?}");
        assert!(v.objects > 0);

        // A stream whose data no longer matches is caught.
        let doc = open(MINIMAL);
        let mut out = save(&doc, &WriteOptions::default()).unwrap_or_default();
        let at = out.windows(5).position(|w| w == b"BT ET").unwrap_or(0);
        out[at..at + 5].copy_from_slice(b"BT EX");
        let v = verify(&doc, &out, None);
        assert_eq!(v.mismatched, vec![4]);
        assert!(!v.verified());

        let v = verify(&doc, b"not a pdf", None);
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
}
