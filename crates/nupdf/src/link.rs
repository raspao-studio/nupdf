// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-link.c, source/pdf/pdf-annot.c
// (pdf_create_link), source/pdf/pdf-nametree.c (pdf_lookup_dest),
// source/pdf/pdf-page.c (pdf_lookup_page_number), source/fitz/link.c
// (fz_is_external_link) and source/fitz/string.c (the URI helpers),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Links as MuPDF reads and writes them. A link's target is a URI:
//! `#page=3&zoom=100,72,720` within the document, `#nameddest=intro` for a
//! named destination, `file:other.pdf#page=2` for another file, or any
//! other URI. Reading turns destinations and actions into URIs; writing
//! turns URIs back into actions and destinations, through the page's
//! transform, so a link survives the trip as MuPDF's does.

use std::sync::Arc;

use crate::document::Document;
use crate::edit::{dict_mut, text_string};
use crate::geometry::{Matrix, Rect};
use crate::object::{Dict, ObjRef, Object};

/// Name trees and destination chains deeper than this are not followed.
const MAX_DEPTH: usize = 64;

/// `fz_link_dest_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestKind {
    Fit,
    FitB,
    FitH,
    FitBH,
    FitV,
    FitBV,
    FitR,
    Xyz,
}

/// `fz_link_dest`: where a link leads, in MuPDF's page space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkDest {
    pub chapter: i32,
    pub page: i32,
    pub kind: DestKind,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub zoom: f32,
}

impl LinkDest {
    /// `fz_make_link_dest_none`.
    pub fn none() -> LinkDest {
        LinkDest {
            chapter: -1,
            page: -1,
            kind: DestKind::Xyz,
            x: f32::NAN,
            y: f32::NAN,
            w: f32::NAN,
            h: f32::NAN,
            zoom: f32::NAN,
        }
    }
}

/// A link on a page: its area in MuPDF's page space and its URI.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub rect: Rect,
    pub uri: String,
}

/// `fz_is_external_link`: a scheme of three or more characters, a letter
/// first, then a colon.
pub fn is_external_link(uri: &str) -> bool {
    let b = uri.as_bytes();
    if !b.first().is_some_and(u8::is_ascii_alphabetic) {
        return false;
    }
    let rest = b[1..]
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
        .count();
    b.get(1 + rest) == Some(&b':') && rest > 1
}

/// `%g` in MuPDF's printf: `fmtfloat`, the shortest digits that read back.
fn g(f: f32) -> String {
    let mut out = Vec::new();
    crate::print::fmtfloat(f, &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

const UNESCAPED: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.!~*'()";

fn encode(s: &str, extra: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if UNESCAPED.contains(&c) || extra.contains(&c) {
            out.push(char::from(c));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(c >> 4)]));
            out.push(char::from(HEX[usize::from(c & 15)]));
        }
    }
    out
}

/// `fz_encode_uri_component`.
pub fn encode_uri_component(s: &str) -> String {
    encode(s, b"")
}

/// `fz_encode_uri_pathname`: `/` kept.
pub fn encode_uri_pathname(s: &str) -> String {
    encode(s, b"/")
}

/// `fz_decode_uri_component`: `%XX` escapes decoded, bytes that are not
/// UTF-8 replaced.
pub fn decode_uri_component(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let (Some(&h), Some(&l)) = (b.get(i + 1), b.get(i + 2))
            && let (Some(h), Some(l)) = (hex(h), hex(l))
        {
            out.push((h << 4 | l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    // A C string ends at its first NUL.
    let end = out.iter().position(|&c| c == 0).unwrap_or(out.len());
    String::from_utf8_lossy(&out[..end]).into_owned()
}

/// `fz_cleanname`: `.` and empty elements dropped, `..` taken back where
/// it can be; the empty path is `.`.
pub fn cleanname(name: &str) -> String {
    let rooted = name.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    // How many leading `..` cannot be taken back.
    let mut fixed = 0;
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.len() > fixed {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                    fixed = parts.len();
                }
            }
            p => parts.push(p),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    }
}

fn kind_name(kind: DestKind) -> &'static str {
    match kind {
        DestKind::Fit => "Fit",
        DestKind::FitB => "FitB",
        DestKind::FitH => "FitH",
        DestKind::FitBH => "FitBH",
        DestKind::FitV => "FitV",
        DestKind::FitBV => "FitBV",
        DestKind::FitR => "FitR",
        DestKind::Xyz => "XYZ",
    }
}

/// `format_explicit_dest_link_uri`.
fn format_explicit_dest(schema: &str, uri: &str, dest: &LinkDest) -> String {
    let page = if dest.page < 0 { 1 } else { dest.page + 1 };
    let sep = if uri.contains('#') { '&' } else { '#' };
    let head = format!("{schema}{uri}{sep}page={page}");
    let nan_or_zero = |f: f32| f.is_nan() || f == 0.0;
    let or_nan = |f: f32| if f.is_nan() { "nan".to_string() } else { g(f) };
    match dest.kind {
        DestKind::Fit | DestKind::FitB => format!("{head}&view={}", kind_name(dest.kind)),
        DestKind::FitH | DestKind::FitBH if dest.y.is_nan() => {
            format!("{head}&view={}", kind_name(dest.kind))
        }
        DestKind::FitH | DestKind::FitBH => {
            format!("{head}&view={},{}", kind_name(dest.kind), g(dest.y))
        }
        DestKind::FitV | DestKind::FitBV if dest.x.is_nan() => {
            format!("{head}&view={}", kind_name(dest.kind))
        }
        DestKind::FitV | DestKind::FitBV => {
            format!("{head}&view={},{}", kind_name(dest.kind), g(dest.x))
        }
        DestKind::Xyz if nan_or_zero(dest.zoom) && dest.x.is_nan() && dest.y.is_nan() => head,
        DestKind::Xyz => {
            let zoom = if nan_or_zero(dest.zoom) {
                "nan".to_string()
            } else {
                g(dest.zoom)
            };
            format!("{head}&zoom={zoom},{},{}", or_nan(dest.x), or_nan(dest.y))
        }
        DestKind::FitR => format!(
            "{head}&viewrect={},{},{},{}",
            g(dest.x),
            g(dest.y),
            g(dest.w),
            g(dest.h)
        ),
    }
}

/// `pdf_format_remote_link_uri_from_name`.
fn named_dest_uri(name: &str) -> String {
    format!("#nameddest={}", encode_uri_component(name))
}

/// `pdf_append_named_dest_to_uri`.
fn append_named_dest(uri: &str, name: &str) -> String {
    let sep = if uri.contains('#') { '&' } else { '#' };
    format!("{uri}{sep}nameddest={}", encode_uri_component(name))
}

/// The schema and encoded path `pdf_new_uri_from_path_and_*_dest` start
/// from: `file://` for an absolute path, `file:` for a relative one.
fn file_uri_base(path: &str) -> (&'static str, String) {
    if path.is_empty() {
        return ("", String::new());
    }
    let schema = if path.starts_with('/') {
        "file://"
    } else {
        "file:"
    };
    (schema, cleanname(&encode_uri_pathname(path)))
}

/// `pdf_to_text_string` of a string; nothing else has text.
fn text_of(doc: &Document, v: &Object) -> String {
    match doc.resolve(v) {
        Object::String(s) => crate::text_string::text_string(&s),
        _ => String::new(),
    }
}

/// `pdf_to_name`: a name's bytes up to a NUL, as text.
fn name_of(n: &[u8]) -> String {
    let end = n.iter().position(|&c| c == 0).unwrap_or(n.len());
    String::from_utf8_lossy(&n[..end]).into_owned()
}

/// `pdf_dict_get` on what `container` resolves to: a null value is none.
fn get(doc: &Document, container: &Object, key: &[u8]) -> Option<Object> {
    match doc.resolve(container) {
        Object::Dict(d) => d.get(key).cloned(),
        Object::Stream(s) => s.dict.get(key).cloned(),
        _ => None,
    }
    .filter(|v| !matches!(v, Object::Null))
}

/// `pdf_array_get`: a null item is none.
fn array_get(doc: &Document, array: &Object, i: usize) -> Option<Object> {
    match doc.resolve(array) {
        Object::Array(a) => a.get(i).cloned(),
        _ => None,
    }
    .filter(|v| !matches!(v, Object::Null))
}

/// `pdf_name_eq` against a name, through references.
fn is_name(doc: &Document, v: Option<&Object>, name: &[u8]) -> bool {
    matches!(v.map(|v| doc.resolve(v)), Some(Object::Name(n)) if n.as_ref() == name)
}

/// The interned name constants MuPDF compares by pointer: only a direct
/// name is equal.
fn is_direct_name(v: Option<&Object>, name: &[u8]) -> bool {
    matches!(v, Some(Object::Name(n)) if n.as_ref() == name)
}

/// `pdf_lookup_name_imp`: binary search by /Limits and /Names, then, as
/// Acrobat copes with unsorted trees, a plain search.
fn lookup_name(
    doc: &Document,
    node: &Object,
    needle: &str,
    ancestors: &mut Vec<i32>,
) -> Option<Object> {
    let num = node.as_ref().map_or(0, |r| r.num);
    if (num > 0 && ancestors.contains(&num)) || ancestors.len() > MAX_DEPTH {
        return None;
    }
    ancestors.push(num);
    let found = lookup_name_in(doc, node, needle, ancestors);
    ancestors.pop();
    found
}

fn lookup_name_in(
    doc: &Document,
    node: &Object,
    needle: &str,
    ancestors: &mut Vec<i32>,
) -> Option<Object> {
    let key = |names: &Object, i: usize| {
        array_get(doc, names, i).map_or(String::new(), |k| text_of(doc, &k))
    };
    if let Some(kids) = get(doc, node, b"Kids")
        && let Object::Array(items) = doc.resolve(&kids)
    {
        let (mut l, mut r) = (0i64, items.len() as i64 - 1);
        while l <= r {
            let m = (l + r) >> 1;
            let kid = items[m as usize].clone();
            let limits = get(doc, &kid, b"Limits").unwrap_or(Object::Null);
            let (first, last) = (key(&limits, 0), key(&limits, 1));
            if !matches!(kid, Object::Ref(_)) {
                doc.diagnostics().warn(
                    "names.direct_node",
                    "non-indirect internal node found in name tree",
                );
                break;
            }
            if needle < first.as_str() {
                r = m - 1;
            } else if needle > last.as_str() {
                l = m + 1;
            } else {
                match lookup_name(doc, &kid, needle, ancestors) {
                    Some(obj) => return Some(obj),
                    None => break,
                }
            }
        }
        for kid in items.iter() {
            if !matches!(kid, Object::Ref(_)) {
                doc.diagnostics().warn(
                    "names.direct_node",
                    "non-indirect internal node found in name tree",
                );
                continue;
            }
            if let Some(obj) = lookup_name(doc, kid, needle, ancestors) {
                return Some(obj);
            }
        }
    }
    if let Some(names) = get(doc, node, b"Names")
        && let Object::Array(items) = doc.resolve(&names)
    {
        let pairs = items.len() / 2;
        let (mut l, mut r) = (0i64, pairs as i64 - 1);
        while l <= r {
            let m = (l + r) >> 1;
            let k = key(&names, m as usize * 2);
            match needle.cmp(k.as_str()) {
                std::cmp::Ordering::Less => r = m - 1,
                std::cmp::Ordering::Greater => l = m + 1,
                std::cmp::Ordering::Equal => return array_get(doc, &names, m as usize * 2 + 1),
            }
        }
        for i in 0..pairs {
            if needle == key(&names, i * 2) {
                return array_get(doc, &names, i * 2 + 1);
            }
        }
    }
    None
}

/// `pdf_lookup_dest`: a named destination, from the catalog's /Dests
/// dictionary (PDF 1.1) or the /Dests name tree.
pub fn lookup_dest(doc: &Document, needle: &Object) -> Option<Object> {
    let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
    if let Some(dests) = get(doc, &root, b"Dests") {
        let key = match doc.resolve(needle) {
            Object::Name(n) => n.to_vec(),
            // pdf_to_str_buf, as a C string.
            Object::String(s) => {
                let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
                s[..end].to_vec()
            }
            _ => Vec::new(),
        };
        return get(doc, &dests, &key);
    }
    let names = get(doc, &root, b"Names")?;
    let tree = get(doc, &names, b"Dests").unwrap_or(Object::Null);
    lookup_name(doc, &tree, &text_of(doc, needle), &mut Vec::new())
}

/// `resolve_dest`: through names, strings and /D dictionaries to an
/// explicit destination.
fn resolve_dest(doc: &Document, dest: Option<Object>, depth: usize) -> Option<Object> {
    if depth > 10 {
        return None;
    }
    let dest = dest?;
    match doc.resolve(&dest) {
        Object::Name(_) | Object::String(_) => {
            resolve_dest(doc, lookup_dest(doc, &dest), depth + 1)
        }
        Object::Array(_) => Some(dest),
        Object::Dict(_) | Object::Stream(_) => resolve_dest(doc, get(doc, &dest, b"D"), depth + 1),
        _ => matches!(dest, Object::Ref(_)).then_some(dest),
    }
}

/// `pdf_lookup_page_number`: through the page map when it builds, else by
/// counting the pages before each ancestor.
pub fn lookup_page_number(doc: &Document, page: &Object) -> Result<i32, String> {
    let num = page.as_ref().map_or(0, |r| r.num);
    if let Some(index) = doc.page_index_by_number(num) {
        return Ok(index);
    }
    if doc.has_page_map() {
        return Ok(-1);
    }
    if !is_name(doc, get(doc, page, b"Type").as_ref(), b"Page") {
        doc.diagnostics()
            .warn("page.invalid", "invalid page object");
        return Ok(-1);
    }
    let mut total: i32 = 0;
    let mut needle = num;
    let mut parent = get(doc, page, b"Parent");
    let mut seen: Vec<i32> = Vec::new();
    while let Some(p) = parent.filter(|p| matches!(doc.resolve(p), Object::Dict(_))) {
        let pnum = p.as_ref().map_or(0, |r| r.num);
        if pnum > 0 && seen.contains(&pnum) {
            return Err("cycle in page tree (parents)".into());
        }
        seen.push(pnum);
        let n = pages_before_kid(doc, &p, needle)?;
        if n < 0 {
            return Ok(-1);
        }
        if i32::MAX - total <= n {
            return Err("illegal or missing count in pages tree".into());
        }
        total += n;
        needle = pnum;
        parent = get(doc, &p, b"Parent");
    }
    Ok(total)
}

/// `pdf_count_pages_before_kid`.
fn pages_before_kid(doc: &Document, parent: &Object, kid_num: i32) -> Result<i32, String> {
    let kids = get(doc, parent, b"Kids").map(|k| doc.resolve(&k));
    let mut total: i32 = 0;
    for kid in kids.as_ref().and_then(Object::as_array).unwrap_or_default() {
        if kid.as_ref().map_or(0, |r| r.num) == kid_num {
            return Ok(total);
        }
        if is_name(doc, get(doc, kid, b"Type").as_ref(), b"Pages") {
            let count = get(doc, kid, b"Count").map(|c| doc.resolve(&c));
            let n = count.as_ref().map_or(0, Object::to_int);
            if !matches!(count, Some(Object::Int(_))) || n < 0 || i32::MAX - total <= n {
                return Err("illegal or missing count in pages tree".into());
            }
            total += n;
        } else {
            total += 1;
        }
    }
    Ok(-1)
}

/// `populate_destination`: an explicit destination array as a
/// `LinkDest`, through the target page's transform unless it is in
/// another file.
fn populate_destination(doc: &Document, dest: &Object, remote: bool) -> Result<LinkDest, String> {
    let arg = |i| array_get(doc, dest, i);
    let (arg1, arg2, arg3, arg4) = (arg(2), arg(3), arg(4), arg(5));
    let real = |a: &Option<Object>| a.as_ref().map_or(0.0, |v| doc.resolve(v).to_real());
    let (v1, v2, v3, v4) = (real(&arg1), real(&arg2), real(&arg3), real(&arg4));
    let mut out = LinkDest::none();
    let mut ctm = Matrix::IDENTITY;
    let pageno = if remote {
        arg(0).map_or(0, |v| doc.resolve(&v).to_int())
    } else {
        let page = arg(0);
        let (pageno, page) = match page.as_ref().map(|p| doc.resolve(p)) {
            Some(Object::Int(_)) => {
                let n = page.as_ref().map_or(0, |p| doc.resolve(p).to_int());
                (n, Some(doc.page(n).map_err(|e| e.0)?))
            }
            _ => {
                let n = lookup_page_number(doc, page.as_ref().unwrap_or(&Object::Null))?;
                (n, page)
            }
        };
        let count = doc.page_count().map_err(|e| e.0)?;
        // fz_clampi
        let hi = count - 1;
        let pageno = if pageno > 0 {
            if pageno < hi { pageno } else { hi }
        } else {
            0
        };
        if let Some(p) = &page
            && matches!(doc.resolve(p), Object::Dict(_))
        {
            ctm = doc.page_transform(p).map_err(|e| e.0)?.1;
        }
        pageno
    };
    out.page = pageno;
    let kind = arg(1);
    out.kind = [
        (&b"XYZ"[..], DestKind::Xyz),
        (b"Fit", DestKind::Fit),
        (b"FitH", DestKind::FitH),
        (b"FitV", DestKind::FitV),
        (b"FitR", DestKind::FitR),
        (b"FitB", DestKind::FitB),
        (b"FitBH", DestKind::FitBH),
        (b"FitBV", DestKind::FitBV),
    ]
    .into_iter()
    .find(|(n, _)| is_direct_name(kind.as_ref(), n))
    .map_or(DestKind::Xyz, |(_, k)| k);
    match out.kind {
        DestKind::Fit | DestKind::FitB => {}
        DestKind::FitH | DestKind::FitBH => {
            let p = ctm.transform_xy(0.0, v1);
            out.y = if arg1.is_some() { p.y } else { f32::NAN };
        }
        DestKind::FitV | DestKind::FitBV => {
            let p = ctm.transform_xy(v1, 0.0);
            out.x = if arg1.is_some() { p.x } else { f32::NAN };
        }
        DestKind::Xyz => {
            let p = ctm.transform_xy(v1, v2);
            out.x = if arg1.is_some() { p.x } else { f32::NAN };
            out.y = if arg2.is_some() { p.y } else { f32::NAN };
            out.zoom = match &arg3 {
                Some(_) if v3 > 0.0 => v3 * 100.0,
                _ => f32::NAN,
            };
        }
        DestKind::FitR => {
            let r = Rect::new(v1, v2, v3, v4).transform(ctm);
            out.x = r.x0.min(r.x1);
            out.y = r.y0.min(r.y1);
            out.w = (r.x1 - r.x0).abs();
            out.h = (r.y1 - r.y0).abs();
        }
    }
    Ok(out)
}

/// `pdf_parse_link_dest`: a destination as a URI.
pub fn parse_link_dest(doc: &Document, dest: Option<&Object>) -> Result<Option<String>, String> {
    let resolved = dest.map(|d| doc.resolve(d));
    Ok(match (dest, resolved) {
        (Some(d), Some(Object::Array(a))) if !a.is_empty() => Some(format_explicit_dest(
            "",
            "",
            &populate_destination(doc, d, false)?,
        )),
        (_, Some(Object::Name(n))) => Some(named_dest_uri(&name_of(&n))),
        (Some(d), Some(Object::String(_))) => Some(named_dest_uri(&text_of(doc, d))),
        _ => {
            doc.diagnostics()
                .warn("link.invalid_dest", "invalid link destination");
            None
        }
    })
}

/// `get_file_stream_and_name`, for the name only: /UF, /F, /Unix, /DOS
/// or /Mac, the first there is, unless an embedded file names another.
fn file_spec_name(doc: &Document, fs: &Object) -> Option<Object> {
    let ef = get(doc, fs, b"EF").unwrap_or(Object::Null);
    let mut name = get(doc, fs, b"UF");
    let mut file = get(doc, &ef, b"UF");
    let mut any = name.clone();
    for key in [&b"F"[..], b"Unix", b"DOS", b"Mac"] {
        if name.is_none() && file.is_none() {
            name = get(doc, fs, key);
            if any.is_none() {
                any = name.clone();
            }
            file = get(doc, &ef, key);
        }
    }
    if any.is_some() && file.is_none() {
        name = any;
    }
    name
}

/// `convert_file_spec_to_URI`.
fn file_spec_uri(
    doc: &Document,
    fs: Option<&Object>,
    dest: Option<&Object>,
    remote: bool,
) -> Result<Option<String>, String> {
    let fs = fs.cloned().unwrap_or(Object::Null);
    let name = match doc.resolve(&fs) {
        Object::String(_) => Some(fs.clone()),
        Object::Dict(_) => file_spec_name(doc, &fs),
        _ => None,
    };
    let Some(name) = name.filter(|n| matches!(doc.resolve(n), Object::String(_))) else {
        doc.diagnostics()
            .warn("link.bad_filespec", "cannot parse file specification");
        return Ok(None);
    };
    let text = text_of(doc, &name);
    let dest_kind = dest.map(|d| doc.resolve(d));
    let invalid = || {
        doc.diagnostics()
            .warn("link.invalid_dest", "invalid link destination");
        Ok(None)
    };
    if is_direct_name(get(doc, &fs, b"FS").as_ref(), b"URL") {
        // pdf_parse_link_dest_to_file_with_uri
        return match (dest, dest_kind) {
            (Some(d), Some(Object::Array(a))) if !a.is_empty() => Ok(Some(format_explicit_dest(
                "",
                &text,
                &populate_destination(doc, d, true)?,
            ))),
            (_, Some(Object::Name(n))) => Ok(Some(append_named_dest(&text, &name_of(&n)))),
            (Some(d), Some(Object::String(_))) => {
                Ok(Some(append_named_dest(&text, &text_of(doc, d))))
            }
            _ => invalid(),
        };
    }
    // pdf_parse_link_dest_to_file_with_path
    let (schema, path) = file_uri_base(&text);
    match (dest, dest_kind) {
        (Some(d), Some(Object::Array(a))) if !a.is_empty() => {
            let d = if remote {
                Some(d.clone())
            } else {
                resolve_dest(doc, Some(d.clone()), 0)
            };
            let parsed = match d {
                Some(d) => populate_destination(doc, &d, remote)?,
                None => LinkDest::none(),
            };
            Ok(Some(format_explicit_dest(schema, &path, &parsed)))
        }
        (_, Some(Object::Name(n))) => Ok(Some(format!(
            "{schema}{path}#nameddest={}",
            encode_uri_component(&name_of(&n))
        ))),
        (Some(d), Some(Object::String(_))) => Ok(Some(format!(
            "{schema}{path}#nameddest={}",
            encode_uri_component(&text_of(doc, d))
        ))),
        _ => Ok(Some(format_explicit_dest(schema, &path, &LinkDest::none()))),
    }
}

/// `pdf_parse_link_action`: an action as a URI. `pagenum` is the page the
/// action is on, for the /Named actions that move from it (-1 when none).
pub fn parse_link_action(
    doc: &Document,
    action: Option<&Object>,
    pagenum: i32,
) -> Result<Option<String>, String> {
    let Some(action) = action else {
        return Ok(None);
    };
    let s = get(doc, action, b"S");
    let s = s.as_ref();
    if is_name(doc, s, b"GoTo") {
        return parse_link_dest(doc, get(doc, action, b"D").as_ref());
    }
    if is_name(doc, s, b"URI") {
        let uri = get(doc, action, b"URI").map_or(String::new(), |u| text_of(doc, &u));
        if !is_external_link(&uri) {
            let root = doc.trailer().get(b"Root").cloned().unwrap_or(Object::Null);
            let base = get(doc, &root, b"URI")
                .and_then(|u| get(doc, &u, b"Base"))
                .map_or("file://".to_string(), |b| text_of(doc, &b));
            return Ok(Some(base + &uri));
        }
        return Ok(Some(uri));
    }
    if is_name(doc, s, b"Launch") {
        return file_spec_uri(doc, get(doc, action, b"F").as_ref(), None, false);
    }
    if is_name(doc, s, b"GoToR") {
        let dest = get(doc, action, b"D");
        return file_spec_uri(doc, get(doc, action, b"F").as_ref(), dest.as_ref(), true);
    }
    if is_name(doc, s, b"Named") {
        let n = get(doc, action, b"N");
        let n = n.as_ref();
        let mut page = pagenum;
        if is_name(doc, n, b"FirstPage") {
            page = 0;
        } else if is_name(doc, n, b"LastPage") {
            page = doc.page_count().map_err(|e| e.0)? - 1;
        } else if is_name(doc, n, b"PrevPage") && page >= 0 {
            if page > 0 {
                page -= 1;
            }
        } else if is_name(doc, n, b"NextPage") && page >= 0 {
            if page < doc.page_count().map_err(|e| e.0)? - 1 {
                page += 1;
            }
        } else {
            return Ok(None);
        }
        return Ok(Some(format!("#page={}", page + 1)));
    }
    Ok(None)
}

/// Whether a page has /Annots, which `pdf_load_page` loads links from.
pub fn has_annots(doc: &Document, page: &Object) -> bool {
    get(doc, page, b"Annots").is_some()
}

/// `pdf_load_link_annots`: the links among a page's annotations, in order,
/// their rectangles through `ctm`. A link that fails to load is reported
/// and left out.
pub fn load_links(doc: &Document, page: &Object, pagenum: i32, ctm: Matrix) -> Vec<Link> {
    let annots = get(doc, page, b"Annots").unwrap_or(Object::Null);
    let n = match doc.resolve(&annots) {
        Object::Array(a) => a.len(),
        _ => 0,
    };
    let mut out = Vec::new();
    for i in 0..n {
        let annot = array_get(doc, &annots, i).unwrap_or(Object::Null);
        match load_link(doc, &annot, pagenum, ctm) {
            Ok(Some(link)) => out.push(link),
            Ok(None) => {}
            Err(e) => doc.diagnostics().warn("link.unloadable", e),
        }
    }
    out
}

/// `pdf_load_link`.
fn load_link(
    doc: &Document,
    annot: &Object,
    pagenum: i32,
    ctm: Matrix,
) -> Result<Option<Link>, String> {
    if !is_name(doc, get(doc, annot, b"Subtype").as_ref(), b"Link") {
        return Ok(None);
    }
    let Some(rect) = get(doc, annot, b"Rect") else {
        return Ok(None);
    };
    let rect = doc.to_rect(Some(&rect)).transform(ctm);
    let uri = match get(doc, annot, b"Dest") {
        Some(dest) => parse_link_dest(doc, Some(&dest))?,
        None => {
            // The additional actions' mouse up, then down, otherwise.
            let action = get(doc, annot, b"A").or_else(|| {
                let aa = get(doc, annot, b"AA").unwrap_or(Object::Null);
                get(doc, &aa, b"U").or_else(|| get(doc, &aa, b"D"))
            });
            parse_link_action(doc, action.as_ref(), pagenum)?
        }
    };
    Ok(uri.map(|uri| Link { rect, uri }))
}

fn has_explicit_dest(uri: &str) -> bool {
    uri.find('#').is_some_and(|i| uri[i..].contains("page="))
}

fn has_named_dest(uri: &str) -> bool {
    uri.find('#')
        .is_some_and(|i| uri[i..].contains("nameddest=") || !has_explicit_dest(uri))
}

/// `parse_uri_named_dest`.
fn named_dest_of(uri: &str) -> String {
    if let Some(i) = uri.find("nameddest=") {
        let rest = &uri[i + 10..];
        let end = rest.find('&').unwrap_or(rest.len());
        return decode_uri_component(&rest[..end]);
    }
    uri.find('#')
        .map_or(String::new(), |i| decode_uri_component(&uri[i + 1..]))
}

/// `parse_file_uri_path`: the path of a `file:` URI, decoded and cleaned.
fn file_uri_path(uri: &str) -> String {
    let rest = uri.get(5..).unwrap_or("");
    let rest = rest.split('#').next().unwrap_or("");
    cleanname(&decode_uri_component(rest))
}

/// C's `atoi`: leading space, a sign, digits.
fn atoi(s: &str) -> i32 {
    let s = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut n: i32 = 0;
    for c in s.bytes().take_while(u8::is_ascii_digit) {
        n = n.wrapping_mul(10).wrapping_add(i32::from(c - b'0'));
    }
    if neg { n.wrapping_neg() } else { n }
}

/// `next_float`: past an optional comma, the float there and the rest.
fn next_float(s: &str, eat_comma: bool) -> (f32, &str) {
    let s = if eat_comma {
        s.strip_prefix(',').unwrap_or(s)
    } else {
        s
    };
    let (v, n) = crate::strtof::fz_strtof(s.as_bytes());
    (v, s.get(n..).unwrap_or(""))
}

/// `pdf_new_explicit_dest_from_uri`.
fn explicit_dest_from_uri(uri: &str) -> LinkDest {
    let frag = uri.find('#').map(|i| &uri[i..]);
    let find = |key: &str| frag.and_then(|f| f.find(key).map(|i| &f[i + key.len()..]));
    let mut val = LinkDest::none();
    val.chapter = 0;
    val.page = find("page=").map_or(0, |p| (atoi(p) - 1).max(0));
    if let Some(rect) = find("viewrect=") {
        val.kind = DestKind::FitR;
        let (x, r) = next_float(rect, false);
        let (y, r) = next_float(r, true);
        let (w, r) = next_float(r, true);
        let (h, _) = next_float(r, true);
        (val.x, val.y, val.w, val.h) = (x, y, w, h);
    } else if let Some(zoom) = find("zoom=") {
        val.kind = DestKind::Xyz;
        let (z, r) = next_float(zoom, false);
        let (x, r) = next_float(r, true);
        let (y, _) = next_float(r, true);
        (val.zoom, val.x, val.y) = (z, x, y);
        if val.zoom <= 0.0 || val.zoom.is_infinite() {
            val.zoom = 100.0;
        }
    } else if let Some(view) = find("view=") {
        let starts = |p: &str| view.len() >= p.len() && view[..p.len()].eq_ignore_ascii_case(p);
        let after_comma = |rest: &str| {
            if rest.contains(',') {
                next_float(rest, true).0
            } else {
                f32::NAN
            }
        };
        if starts("FitH") {
            val.kind = DestKind::FitH;
            val.y = after_comma(&view[4..]);
        } else if starts("FitBH") {
            val.kind = DestKind::FitBH;
            val.y = after_comma(&view[5..]);
        } else if starts("FitV") {
            val.kind = DestKind::FitV;
            val.x = after_comma(&view[4..]);
        } else if starts("FitBV") {
            val.kind = DestKind::FitBV;
            val.x = after_comma(&view[5..]);
        } else if starts("FitB") {
            val.kind = DestKind::FitB;
        } else if starts("Fit") {
            val.kind = DestKind::Fit;
        }
    }
    val
}

/// `pdf_new_dest_from_link`: a URI as a destination: a name, or an array
/// whose page is an object (or, in another file, a number) and whose
/// coordinates are back in PDF space.
pub fn new_dest_from_link(doc: &Document, uri: &str, remote: bool) -> Result<Object, String> {
    if has_named_dest(uri) {
        return Ok(text_string(&named_dest_of(uri)));
    }
    let val = explicit_dest_from_uri(uri);
    let mut dest = Vec::with_capacity(6);
    let inv = if remote {
        dest.push(Object::Int(i64::from(val.page)));
        Matrix::IDENTITY
    } else {
        let page = doc.page(val.page).map_err(|e| e.0)?;
        let ctm = doc.page_transform(&page).map_err(|e| e.0)?.1;
        dest.push(page);
        ctm.invert()
    };
    let real = |f: f32| {
        if f.is_nan() {
            Object::Null
        } else {
            Object::Real(f)
        }
    };
    match val.kind {
        DestKind::Fit => dest.push(Object::name(b"Fit")),
        DestKind::FitB => dest.push(Object::name(b"Fit")),
        DestKind::FitH | DestKind::FitBH => {
            let p = inv.transform_xy(0.0, val.y);
            dest.push(Object::name(kind_name(val.kind).as_bytes()));
            dest.push(real(p.y));
        }
        DestKind::FitV | DestKind::FitBV => {
            let p = inv.transform_xy(val.x, 0.0);
            dest.push(Object::name(kind_name(val.kind).as_bytes()));
            dest.push(real(p.x));
        }
        DestKind::Xyz => {
            let zero = |f: f32| if f.is_nan() { 0.0 } else { f };
            let p = if inv.a == 0.0 && inv.d == 0.0 {
                // Turned 90 or 270 degrees: x and y trade places.
                let mut p = inv.transform_xy(zero(val.x), zero(val.y));
                if val.x.is_nan() {
                    p.y = val.x;
                }
                if val.y.is_nan() {
                    p.x = val.y;
                }
                p
            } else if inv.b == 0.0 && inv.c == 0.0 {
                let mut p = inv.transform_xy(zero(val.x), zero(val.y));
                if val.x.is_nan() {
                    p.x = val.x;
                }
                if val.y.is_nan() {
                    p.y = val.y;
                }
                p
            } else {
                inv.transform_xy(val.x, val.y)
            };
            dest.push(Object::name(b"XYZ"));
            dest.push(real(p.x));
            dest.push(real(p.y));
            dest.push(if val.zoom.is_nan() {
                Object::Null
            } else {
                Object::Real(val.zoom / 100.0)
            });
        }
        DestKind::FitR => {
            let r = Rect::new(val.x, val.y, val.x + val.w, val.y + val.h).transform(inv);
            dest.push(Object::name(b"FitR"));
            for v in [r.x0, r.y0, r.x1, r.y1] {
                dest.push(Object::Real(v));
            }
        }
    }
    Ok(Object::Array(Arc::new(dest)))
}

/// `pdf_add_filespec` with no embedded file: a new file specification
/// object, its /F the name with anything outside printable ASCII as `_`.
fn add_filespec(doc: &Document, filename: &str) -> Object {
    let ascii: String = filename
        .chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { '_' })
        .collect();
    let mut d = Dict::new();
    d.put(b"Type", Object::name(b"Filespec"));
    d.put(b"F", text_string(&ascii));
    d.put(b"UF", text_string(filename));
    new_object(doc, d)
}

/// `pdf_add_new_dict`: a reference to a new object.
fn new_object(doc: &Document, d: Dict) -> Object {
    let num = doc.create_object();
    doc.update_object(num, Object::Dict(Arc::new(d)));
    Object::Ref(ObjRef::new(num, 0))
}

/// `pdf_new_action_from_link`: `#...` as a GoTo, `file:...` as a GoToR
/// with a file specification, any other external URI as a URI action.
pub fn new_action_from_link(doc: &Document, uri: &str) -> Result<Object, String> {
    let mut action = Dict::new();
    if uri.starts_with('#') {
        action.put(b"S", Object::name(b"GoTo"));
        action.put(b"D", new_dest_from_link(doc, uri, false)?);
    } else if uri.starts_with("file:") {
        action.put(b"S", Object::name(b"GoToR"));
        action.put(b"D", new_dest_from_link(doc, uri, true)?);
        action.put(b"F", add_filespec(doc, &file_uri_path(uri)));
    } else if is_external_link(uri) {
        action.put(b"S", Object::name(b"URI"));
        action.put(b"URI", text_string(uri));
    } else {
        return Err("unsupported link URI type".into());
    }
    Ok(Object::Dict(Arc::new(action)))
}

/// `pdf_resolve_link_dest`: where a URI leads in this document; a page of
/// -1 when nowhere.
pub fn resolve_link_dest(doc: &Document, uri: &str) -> Result<LinkDest, String> {
    resolve_link_dest_at(doc, uri, 0)
}

fn resolve_link_dest_at(doc: &Document, uri: &str, depth: usize) -> Result<LinkDest, String> {
    let mut dest = LinkDest::none();
    if depth > MAX_DEPTH {
        return Ok(dest);
    }
    if has_explicit_dest(uri) {
        dest = explicit_dest_from_uri(uri);
        if !dest.x.is_nan() || !dest.y.is_nan() || !dest.w.is_nan() || !dest.h.is_nan() {
            let page = doc.page(dest.page).map_err(|e| e.0)?;
            let (mediabox, ctm) = doc.page_transform(&page).map_err(|e| e.0)?;
            let mediabox = mediabox.transform(ctm);
            // fz_clamp: NaN ends up at the low end.
            let clamp = |f: f32, lo: f32, hi: f32| {
                if f > lo {
                    if f < hi { f } else { hi }
                } else {
                    lo
                }
            };
            dest.x = clamp(dest.x, 0.0, mediabox.x1 - mediabox.x0);
            dest.y = clamp(dest.y, 0.0, mediabox.y1 - mediabox.y0);
            dest.w = clamp(dest.w, 0.0, mediabox.x1 - dest.x);
            dest.h = clamp(dest.h, 0.0, mediabox.y1 - dest.y);
        }
    } else if has_named_dest(uri) {
        let needle = text_string(&named_dest_of(uri));
        if let Some(target) = resolve_dest(doc, Some(needle), 0) {
            let inner = parse_link_dest(doc, Some(&target))?;
            let inner = resolve_link_dest_at(doc, inner.as_deref().unwrap_or(""), depth + 1)?;
            if dest.kind == DestKind::Xyz
                && dest.x.is_nan()
                && dest.y.is_nan()
                && dest.zoom.is_nan()
            {
                dest = inner;
            } else {
                (dest.chapter, dest.page) = (inner.chapter, inner.page);
            }
        }
    } else {
        dest.page = atoi(uri) - 1;
    }
    Ok(if dest.page >= 0 {
        dest
    } else {
        LinkDest::none()
    })
}

/// `fz_page_number_from_location` of `fz_resolve_link`, for a PDF's one
/// chapter.
pub fn resolve_link_page(doc: &Document, uri: &str) -> Result<i32, String> {
    let dest = resolve_link_dest(doc, uri)?;
    Ok(if dest.chapter == 0 { dest.page } else { -1 })
}

/// `pdf_create_link`: a new Link annotation on page object `page` (a
/// reference), over `bbox` in MuPDF's page space, leading to `uri`. An
/// /Annots array of its own is copied into the page first.
pub fn create_link(doc: &Document, page: &Object, bbox: Rect, uri: &str) -> Result<(), String> {
    let Object::Ref(page_ref) = page else {
        return Err("cannot add a link to a page that is not an object".into());
    };
    let ctm = doc.page_transform(page).map_err(|e| e.0)?.1;
    let rect = bbox.transform(ctm.invert());

    let mut page_obj = doc.object(page_ref.num).ok_or("cannot find page object")?;
    let annots = match get(doc, &page_obj, b"Annots") {
        Some(a) => match doc.resolve(&a) {
            Object::Array(items) => Some((*items).clone()),
            _ => None,
        },
        None => None,
    };
    let mut annots = annots.unwrap_or_default();

    let mut annot = Dict::new();
    annot.put(b"Type", Object::name(b"Annot"));
    annot.put(b"Subtype", Object::name(b"Link"));
    annot.put(
        b"Rect",
        Object::Array(Arc::new(
            [rect.x0, rect.y0, rect.x1, rect.y1]
                .map(Object::Real)
                .to_vec(),
        )),
    );
    let mut bs = Dict::new();
    bs.put(b"S", Object::name(b"S"));
    bs.put(b"Type", Object::name(b"Border"));
    bs.put(b"W", Object::Int(0));
    annot.put(b"BS", Object::Dict(Arc::new(bs)));
    annot.put(b"A", new_action_from_link(doc, uri)?);
    let annot = new_object(doc, annot);
    annots.push(annot);

    // The action may have made objects, but not changed the page.
    let page_dict = dict_mut(&mut page_obj)?;
    page_dict.put(b"Annots", Object::Array(Arc::new(annots)));
    doc.alter_object(page_ref.num, page_obj);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Document {
        Document::open(&include_bytes!("../../../corpus/smoke/merge-links.pdf")[..])
            .unwrap_or_else(|e| panic!("{e:?}"))
    }

    #[test]
    fn links_read_as_mupdf_reads_them() {
        // What MuPDF's getLinks gives for page 1, rotated and cropped.
        let doc = fixture();
        let page = doc.page(0).unwrap_or(Object::Null);
        let ctm = doc
            .page_transform(&page)
            .map(|(_, m)| m)
            .unwrap_or(Matrix::IDENTITY);
        let links = load_links(&doc, &page, 0, ctm);
        let expected: [(Rect, &str); 7] = [
            (
                Rect::new(0.0, 0.0, 19.75, 90.25),
                "http://example.com/a?b=c",
            ),
            (
                Rect::new(29.75, 89.5, 279.75, 189.5),
                "https://example.com/docs/rel/page.html",
            ),
            (
                Rect::new(-18.25, -9.5, -16.25, -7.5),
                "file:other.pdf#page=1",
            ),
            (
                Rect::new(-14.25, -5.5, -12.25, -3.5),
                "file:doc%20dir/b.pdf#page=3&view=FitH,500",
            ),
            (
                Rect::new(-10.25, -1.5, -8.25, 0.5),
                "file:///abs/c.pdf#nameddest=named",
            ),
            (Rect::new(-6.25, 2.5, -4.25, 4.5), "#page=2&view=Fit"),
            (Rect::new(-2.25, 6.5, -0.25, 8.5), "mailto:a@example.com"),
        ];
        let got: Vec<(Rect, &str)> = links.iter().map(|l| (l.rect, l.uri.as_str())).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn uris_resolve_to_pages() {
        let doc = fixture();
        let page = |uri| resolve_link_page(&doc, uri).unwrap_or(-2);
        assert_eq!(page("#page=3&view=FitH,500"), 2);
        assert_eq!(page("#nameddest=dest2"), 1);
        assert_eq!(page("#intro"), 0);
        assert_eq!(page("https://example.com"), -1);
        assert_eq!(page(""), -1);
    }

    #[test]
    fn schemes_need_three_characters() {
        for (uri, ext) in [
            ("http://x", true),
            ("mailto:a@b", true),
            ("file:a.pdf", true),
            ("c:/dir", false),
            ("ab:x", false),
            ("#page=2", false),
            ("1http:x", false),
        ] {
            assert_eq!(is_external_link(uri), ext, "{uri}");
        }
    }

    #[test]
    fn names_encode_and_paths_clean_as_mupdf_does() {
        assert_eq!(encode_uri_component("a b/é"), "a%20b%2F%C3%A9");
        assert_eq!(encode_uri_pathname("/a b/c"), "/a%20b/c");
        assert_eq!(decode_uri_component("a%20b%zz"), "a b%zz");
        assert_eq!(cleanname("//a/./b/../c/"), "/a/c");
        assert_eq!(cleanname("../a/../../b"), "../../b");
        assert_eq!(cleanname(""), ".");
        assert_eq!(cleanname("/.."), "/");
    }

    #[test]
    fn explicit_destinations_round_trip_through_uris() {
        let d = LinkDest {
            chapter: 0,
            page: 2,
            kind: DestKind::Xyz,
            x: 72.5,
            y: f32::NAN,
            w: f32::NAN,
            h: f32::NAN,
            zoom: 150.0,
        };
        let uri = format_explicit_dest("", "", &d);
        assert_eq!(uri, "#page=3&zoom=150,72.5,nan");
        let back = explicit_dest_from_uri(&uri);
        assert_eq!((back.page, back.zoom, back.x), (2, 150.0, 72.5));
        assert!(back.y.is_nan());
        let fit = format_explicit_dest(
            "file:",
            "b.pdf#x",
            &LinkDest {
                kind: DestKind::FitH,
                y: 10.0,
                ..d
            },
        );
        assert_eq!(fit, "file:b.pdf#x&page=3&view=FitH,10");
        assert_eq!(
            explicit_dest_from_uri("#page=0&view=fitbh").kind,
            DestKind::FitBH
        );
    }

    #[test]
    fn named_destinations_are_text() {
        assert!(has_named_dest("#nameddest=a%20b"));
        assert!(has_named_dest("#chapter"));
        assert!(!has_named_dest("#page=1"));
        assert_eq!(named_dest_of("x#nameddest=a%20b&page=2"), "a b");
        assert_eq!(named_dest_of("#chapter%201"), "chapter 1");
        assert_eq!(file_uri_path("file://tmp/a%20b.pdf#page=2"), "/tmp/a b.pdf");
    }
}
