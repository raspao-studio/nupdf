// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! nupdf's own check that redacted content is gone: an independent pass
//! over a document, one check per leak class of the redaction contract
//! (AGENT-INTERFACE). It reads the document as it is, with no knowledge of
//! how it was redacted: page text with clipping off and hidden layers
//! run, every string of every object, the text strings shown by
//! annotation appearances, marked-content properties in content streams,
//! the pixels of images under the redacted areas, and the file's history.

use std::collections::HashSet;

use crate::content::{self, Op, ProcState, Processor};
use crate::device::{ColorParams, Device};
use crate::document::Document;
use crate::geometry::{Matrix, Quad, Rect};
use crate::image::Image;
use crate::object::Object;
use crate::scrub::Matcher;
use crate::stext::Options;

/// The leak classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Class {
    /// Target text in the pages' text.
    ContentText,
    /// In structure elements and marked-content properties.
    Structure,
    /// In annotations, their appearances, and form fields.
    Annotations,
    /// In the information dictionary and XMP.
    Metadata,
    /// In outline titles, named destinations and page labels.
    Outline,
    /// In embedded files' names, descriptions and contents.
    EmbeddedFiles,
    /// In content shown only with hidden optional content, or layer names.
    HiddenContent,
    /// Pixels under a redacted area not blanked; thumbnails of redacted
    /// pages.
    Images,
    /// More than one revision, or targets in objects nothing reaches.
    History,
    /// In JavaScript.
    JavaScript,
}

impl Class {
    pub const ALL: [Class; 10] = [
        Class::ContentText,
        Class::Structure,
        Class::Annotations,
        Class::Metadata,
        Class::Outline,
        Class::EmbeddedFiles,
        Class::HiddenContent,
        Class::Images,
        Class::History,
        Class::JavaScript,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Class::ContentText => "content_text",
            Class::Structure => "structure",
            Class::Annotations => "annotations",
            Class::Metadata => "metadata",
            Class::Outline => "outline",
            Class::EmbeddedFiles => "embedded_files",
            Class::HiddenContent => "hidden_content",
            Class::Images => "images",
            Class::History => "history",
            Class::JavaScript => "javascript",
        }
    }
}

/// One leak.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub class: Class,
    /// 1-based.
    pub page: Option<i32>,
    pub object: Option<i32>,
    pub detail: String,
}

/// What to look for.
#[derive(Debug)]
pub struct Targets {
    /// Needles as `search` takes them (text with its options).
    pub needles: Vec<(String, u32)>,
    /// The same targets as patterns, for strings outside the page text.
    pub matcher: Matcher,
    /// Redacted areas by page (0-based), in page space.
    pub areas: Vec<(i32, Vec<Quad>)>,
}

/// The verdict.
#[derive(Debug, Default)]
pub struct Verification {
    pub findings: Vec<Finding>,
    /// Per needle, its hits in the pages' text.
    pub hits_after: Vec<usize>,
    pub revisions: usize,
}

impl Verification {
    pub fn verified(&self) -> bool {
        self.findings.is_empty()
    }
}

/// At most this many findings are kept per class.
const MAX_FINDINGS: usize = 50;

struct Collector {
    findings: Vec<Finding>,
}

impl Collector {
    fn push(&mut self, f: Finding) {
        let n = self.findings.iter().filter(|g| g.class == f.class).count();
        if n < MAX_FINDINGS {
            self.findings.push(f);
        }
    }
}

fn page_text(doc: &Document, index: i32, opts: &str) -> Option<crate::stext::StextPage> {
    let opts = Options::parse(opts).ok()?;
    crate::parallel::sequential(doc, &[index], opts)
        .next()?
        .ok()
}

fn count_hits(page: &crate::stext::StextPage, needles: &[(String, u32)]) -> Vec<usize> {
    needles
        .iter()
        .map(|(n, o)| crate::search::search_page(page, n, *o).map_or(0, |h| h.len()))
        .collect()
}

#[cfg(test)]
thread_local! {
    /// How many times this thread's prefiltered checks ran a full matcher.
    static FULL_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One run of a full matcher by a prefiltered check, counted in tests.
fn full_call() {
    #[cfg(test)]
    FULL_CALLS.with(|n| n.set(n.get() + 1));
}

#[cfg(test)]
pub(crate) fn full_calls() -> usize {
    FULL_CALLS.with(std::cell::Cell::get)
}

const NO_NODE: u32 = u32::MAX;

/// Many byte strings looked for in one pass over a haystack
/// (Aho-Corasick): which of them occur in it, in time that grows with the
/// haystack and the strings found, not with how many strings there are.
pub(crate) struct Many {
    /// Each node's edges, sorted by byte; node 0 is the root.
    edges: Vec<Vec<(u8, u32)>>,
    fail: Vec<u32>,
    /// The ids of the strings that end at each node.
    ends: Vec<Vec<usize>>,
    /// The nearest node down the failure chain, not the node itself, at
    /// which a string ends.
    dict: Vec<u32>,
    /// Per node, the scan that last reported it, so each scan reports a
    /// node once.
    stamps: std::cell::RefCell<(u32, Vec<u32>)>,
}

impl Many {
    /// An empty string is never found.
    pub(crate) fn new<'a>(strings: impl IntoIterator<Item = (usize, &'a [u8])>) -> Many {
        let mut edges: Vec<Vec<(u8, u32)>> = vec![Vec::new()];
        let mut ends: Vec<Vec<usize>> = vec![Vec::new()];
        for (id, s) in strings.into_iter().filter(|(_, s)| !s.is_empty()) {
            let mut node = 0usize;
            for &b in s {
                node = match edges[node].binary_search_by_key(&b, |e| e.0) {
                    Ok(i) => edges[node][i].1 as usize,
                    Err(i) => {
                        let next = edges.len();
                        edges[node].insert(i, (b, next as u32));
                        edges.push(Vec::new());
                        ends.push(Vec::new());
                        next
                    }
                };
            }
            ends[node].push(id);
        }
        let n = edges.len();
        let mut many = Many {
            edges,
            fail: vec![0; n],
            ends,
            dict: vec![NO_NODE; n],
            stamps: std::cell::RefCell::new((0, vec![0; n])),
        };
        // Breadth first, so a node's failure, which is shallower, is done.
        let mut queue = std::collections::VecDeque::from([0u32]);
        while let Some(u) = queue.pop_front() {
            for k in 0..many.edges[u as usize].len() {
                let (b, v) = many.edges[u as usize][k];
                let f = if u == 0 {
                    0
                } else {
                    many.step(many.fail[u as usize], b)
                };
                many.fail[v as usize] = f;
                many.dict[v as usize] = if many.ends[f as usize].is_empty() {
                    many.dict[f as usize]
                } else {
                    f
                };
                queue.push_back(v);
            }
        }
        many
    }

    /// The node after `node` reads `b`. Each failure is shallower, so this
    /// ends within the node's depth.
    fn step(&self, mut node: u32, b: u8) -> u32 {
        loop {
            let edges = &self.edges[node as usize];
            if let Ok(i) = edges.binary_search_by_key(&b, |e| e.0) {
                return edges[i].1;
            }
            if node == 0 {
                return 0;
            }
            node = self.fail[node as usize];
        }
    }

    /// Adds to `out` the id of every string that occurs in `hay`, each once.
    pub(crate) fn found(&self, hay: &[u8], out: &mut Vec<usize>) {
        let mut local;
        let mut shared = self.stamps.try_borrow_mut().ok();
        let (scan, stamps) = match shared.as_deref_mut() {
            Some((scan, stamps)) => (scan, stamps),
            None => {
                local = (0u32, vec![0u32; self.edges.len()]);
                (&mut local.0, &mut local.1)
            }
        };
        if *scan == u32::MAX {
            stamps.iter_mut().for_each(|s| *s = 0);
            *scan = 0;
        }
        *scan += 1;
        let mut node = 0u32;
        for &b in hay {
            node = self.step(node, b);
            let mut t = if self.ends[node as usize].is_empty() {
                self.dict[node as usize]
            } else {
                node
            };
            // A node reported in this scan had its whole chain reported.
            while t != NO_NODE && stamps[t as usize] != *scan {
                stamps[t as usize] = *scan;
                out.extend_from_slice(&self.ends[t as usize]);
                t = self.dict[t as usize];
            }
        }
    }
}

/// Whether a byte string's runes, as [`crate::stext::chartorune`] reads
/// them, are at least two.
fn at_least_two_runes(s: &[u8]) -> bool {
    let mut i = 0;
    for _ in 0..2 {
        if i >= s.len() {
            return false;
        }
        i += crate::stext::chartorune(&s[i..]).1.max(1);
    }
    true
}

/// Needles found in pages' text as [`crate::search::search_page`] finds
/// them, each page's text read once for all of them: a needle is searched
/// for only on a page whose text, as the search transforms it, holds the
/// needle as the search transforms it, since that is what a hit is. A
/// regular expression, and a needle that transforms to fewer than two
/// characters, is searched for on every page.
pub(crate) struct PageNeedles {
    needles: Vec<(String, u32)>,
    /// One per set of options.
    groups: Vec<(u32, Many)>,
    always: Vec<usize>,
}

impl PageNeedles {
    pub(crate) fn new(needles: &[(String, u32)]) -> PageNeedles {
        let mut spun: std::collections::BTreeMap<u32, Vec<(usize, Vec<u8>)>> = Default::default();
        let mut always = Vec::new();
        for (k, (needle, opts)) in needles.iter().enumerate() {
            match crate::search::spun_needle(needle, *opts) {
                Some(s) if at_least_two_runes(&s) => spun.entry(*opts).or_default().push((k, s)),
                _ => always.push(k),
            }
        }
        let groups = spun
            .into_iter()
            .map(|(opts, list)| {
                (
                    opts,
                    Many::new(list.iter().map(|(k, s)| (*k, s.as_slice()))),
                )
            })
            .collect();
        PageNeedles {
            needles: needles.to_vec(),
            groups,
            always,
        }
    }

    /// The needles that may have hits on `page`, in order: every one that
    /// has is among them.
    pub(crate) fn candidates(&self, page: &crate::stext::StextPage) -> Vec<usize> {
        let mut out = self.always.clone();
        for (opts, many) in &self.groups {
            many.found(&crate::search::spun_page(page, *opts), &mut out);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The hits of needle `k` on `page`, by the full search.
    pub(crate) fn hits(&self, page: &crate::stext::StextPage, k: usize) -> usize {
        full_call();
        self.needles.get(k).map_or(0, |(n, o)| {
            crate::search::search_page(page, n, *o).map_or(0, |h| h.len())
        })
    }

    /// What [`count_hits`] gives.
    fn count(&self, page: &crate::stext::StextPage) -> Vec<usize> {
        let mut counts = vec![0; self.needles.len()];
        for k in self.candidates(page) {
            counts[k] = self.hits(page, k);
        }
        counts
    }
}

/// A string as the literal targets' patterns read it: its runes as
/// [`crate::stext::chartorune`] decodes them, without the white space
/// (space, tab, CR, LF) a pattern's white space matches, each rune written
/// the same way whatever bytes it came from.
fn literal_canon(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let (c, len) = crate::stext::chartorune(&s[i..]);
        i += len.max(1);
        if matches!(c, 0x20 | 0x09 | 0x0d | 0x0a) {
            continue;
        }
        match u32::try_from(c).ok().and_then(char::from_u32) {
            Some(ch) => out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes()),
            // No UTF-8 starts with 0xff.
            None => {
                out.push(0xff);
                out.extend_from_slice(&c.to_le_bytes());
            }
        }
    }
    out
}

/// Literal targets matched in strings outside the page text as
/// [`Matcher::new`]`(texts, &[], false)` matches them, every string read
/// once for all of them. A target's pattern matches its characters as they
/// are and its white space as runs of space, tab, CR and LF (the pattern
/// ending at a NUL), so a string it matches holds the target's characters
/// up to any NUL, white space dropped, once the string's own space, tab, CR
/// and LF are dropped. Only the targets whose characters a string holds so
/// are matched in it by their own pattern; a target of fewer than two such
/// characters is matched in every string.
pub(crate) struct Literals {
    each: Vec<Matcher>,
    many: Many,
    always: Vec<usize>,
}

impl Literals {
    pub(crate) fn new(texts: &[String]) -> Result<Literals, String> {
        let mut each = Vec::with_capacity(texts.len());
        let mut canons = Vec::new();
        let mut always = Vec::new();
        for (k, t) in texts.iter().enumerate() {
            each.push(Matcher::new(std::slice::from_ref(t), &[], false)?);
            let kept: String = t
                .split('\0')
                .next()
                .unwrap_or_default()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            let canon = literal_canon(kept.as_bytes());
            if at_least_two_runes(&canon) {
                canons.push((k, canon));
            } else if !each[k].is_empty() {
                always.push(k);
            }
        }
        let many = Many::new(canons.iter().map(|(k, c)| (*k, c.as_slice())));
        Ok(Literals { each, many, always })
    }

    fn is_empty(&self) -> bool {
        self.each.iter().all(Matcher::is_empty)
    }

    fn matches(&self, s: &[u8]) -> bool {
        let mut ids = self.always.clone();
        self.many.found(&literal_canon(s), &mut ids);
        ids.sort_unstable();
        ids.dedup();
        ids.into_iter().any(|k| {
            full_call();
            self.each.get(k).is_some_and(|m| m.matches(s))
        })
    }

    fn matches_text(&self, s: &[u8]) -> bool {
        self.matches(&crate::text_string::utf8_from_pdf_string(s))
    }
}

/// What strings outside the page text are matched with.
#[derive(Clone, Copy)]
enum Strs<'a> {
    All(&'a Matcher),
    Literals(&'a Literals),
}

impl Strs<'_> {
    fn is_empty(self) -> bool {
        match self {
            Strs::All(m) => m.is_empty(),
            Strs::Literals(l) => l.is_empty(),
        }
    }

    fn matches(self, s: &[u8]) -> bool {
        match self {
            Strs::All(m) => m.matches(s),
            Strs::Literals(l) => l.matches(s),
        }
    }

    fn matches_text(self, s: &[u8]) -> bool {
        match self {
            Strs::All(m) => m.matches_text(s),
            Strs::Literals(l) => l.matches_text(s),
        }
    }
}

/// [`verify`] with literal `texts` as the targets (each a needle with no
/// options, and all of them the matcher, case kept), with no areas:
/// the same findings, each page's text and each string read once for all
/// the targets rather than once for each.
pub fn verify_literals(doc: &Document, texts: &[String]) -> Result<Verification, String> {
    let needles: Vec<(String, u32)> = texts.iter().map(|s| (s.clone(), 0)).collect();
    let pages = PageNeedles::new(&needles);
    let literals = Literals::new(texts)?;
    Ok(run(
        doc,
        &needles,
        Some(&pages),
        Strs::Literals(&literals),
        &[],
    ))
}

/// Runs every check.
pub fn verify(doc: &Document, t: &Targets) -> Verification {
    run(doc, &t.needles, None, Strs::All(&t.matcher), &t.areas)
}

fn run(
    doc: &Document,
    needles: &[(String, u32)],
    fast: Option<&PageNeedles>,
    strs: Strs<'_>,
    areas: &[(i32, Vec<Quad>)],
) -> Verification {
    let count_hits = |page: &crate::stext::StextPage| match fast {
        Some(f) => f.count(page),
        None => count_hits(page, needles),
    };
    let mut c = Collector {
        findings: Vec::new(),
    };
    let revisions = doc.revisions();
    if revisions > 1 {
        c.push(Finding {
            class: Class::History,
            page: None,
            object: None,
            detail: format!("{revisions} revisions; earlier ones keep what was redacted"),
        });
    }
    let count = doc.page_count().unwrap_or(0);
    let mut hits_after = vec![0usize; needles.len()];
    for i in 0..count {
        let Some(all) = page_text(doc, i, "clip=no,all-layers") else {
            continue;
        };
        let with_hidden = count_hits(&all);
        if with_hidden.iter().all(|&n| n == 0) {
            continue;
        }
        let visible = page_text(doc, i, "clip=no")
            .map(|p| count_hits(&p))
            .unwrap_or_else(|| vec![0; needles.len()]);
        for (k, (needle, _)) in needles.iter().enumerate() {
            hits_after[k] += with_hidden[k];
            if visible[k] > 0 {
                c.push(Finding {
                    class: Class::ContentText,
                    page: Some(i + 1),
                    object: None,
                    detail: format!("{} hits of {needle:?} in the page's text", visible[k]),
                });
            }
            if with_hidden[k] > visible[k] {
                c.push(Finding {
                    class: Class::HiddenContent,
                    page: Some(i + 1),
                    object: None,
                    detail: format!(
                        "{} hits of {needle:?} in hidden optional content",
                        with_hidden[k] - visible[k]
                    ),
                });
            }
        }
    }
    if !strs.is_empty() {
        objects(doc, strs, &mut c);
    }
    for (index, quads) in areas {
        images(doc, *index, quads, &mut c);
    }
    Verification {
        findings: c.findings,
        hits_after,
        revisions,
    }
}

/// Where a string sits, by the keys that lead to it: its class.
fn classify(path: &[&[u8]]) -> Class {
    let has = |k: &[u8]| path.contains(&k);
    if has(b"JS") || has(b"JavaScript") {
        Class::JavaScript
    } else if has(b"EmbeddedFiles") || has(b"EF") || has(b"FileAttachment") {
        Class::EmbeddedFiles
    } else if has(b"Info") || has(b"Metadata") {
        Class::Metadata
    } else if has(b"Outlines") || has(b"Dests") || has(b"PageLabels") {
        Class::Outline
    } else if has(b"OCProperties") || has(b"OC") {
        Class::HiddenContent
    } else if has(b"StructTreeRoot") || has(b"Alt") || has(b"ActualText") || has(b"E") {
        Class::Structure
    } else if has(b"Annots") || has(b"AcroForm") || has(b"AP") || has(b"Fields") {
        Class::Annotations
    } else {
        Class::ContentText
    }
}

/// Every object reachable from the trailer, then every one that is not:
/// strings matched, by where they sit; streams' data matched where it is
/// text (XMP, JavaScript, embedded files); appearance streams' shown
/// strings and content streams' marked-content properties matched too.
fn objects(doc: &Document, m: Strs<'_>, c: &mut Collector) {
    let mut seen: HashSet<i32> = HashSet::new();
    let trailer = Object::Dict(std::sync::Arc::new(doc.trailer()));
    let mut walker = Walker {
        doc,
        m,
        seen: &mut seen,
        found: Vec::new(),
        streams: Vec::new(),
    };
    walker.walk(&trailer, &mut Vec::new(), None, 0);
    let Walker { found, streams, .. } = walker;
    for f in found {
        c.push(f);
    }
    for (num, class, kind) in streams {
        content_strings(doc, m, num, class, kind, c);
    }
    // Objects nothing reaches.
    for num in 1..doc.xref_len() as i32 {
        if seen.contains(&num) {
            continue;
        }
        let Some(obj) = doc.object(num) else { continue };
        let mut hit = false;
        let mut stack = vec![(obj, 0usize)];
        while let Some((o, depth)) = stack.pop() {
            if depth > 32 || hit {
                break;
            }
            match o {
                Object::String(s) => hit |= m.matches_text(&s),
                Object::Array(a) => stack.extend(a.iter().cloned().map(|v| (v, depth + 1))),
                Object::Dict(d) => stack.extend(d.iter().map(|(_, v)| (v.clone(), depth + 1))),
                Object::Stream(s) => {
                    stack.extend(s.dict.iter().map(|(_, v)| (v.clone(), depth + 1)));
                    if let Ok(data) = doc.stream_data(&s) {
                        hit |= !is_binary_stream(&s.dict) && m.matches(&data);
                    }
                }
                _ => {}
            }
        }
        if hit {
            c.push(Finding {
                class: Class::History,
                page: None,
                object: Some(num),
                detail: "a target in an object nothing reaches".into(),
            });
        }
    }
}

/// Image, font and content data, which are not text to match.
fn is_binary_stream(d: &crate::object::Dict) -> bool {
    let name = |k: &[u8]| d.get(k).and_then(|v| v.as_name().map(<[u8]>::to_vec));
    matches!(name(b"Subtype").as_deref(), Some(b"Image") | Some(b"Form"))
        || d.get(b"Length1").is_some()
        || d.get(b"Length2").is_some()
        || d.get(b"Length3").is_some()
        || matches!(
            name(b"Subtype").as_deref(),
            Some(b"Type1C") | Some(b"CIDFontType0C") | Some(b"OpenType")
        )
}

/// What a stream is, for what to match in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    /// Its data is text: XMP, JavaScript, an embedded file.
    Text,
    /// A content stream: its marked-content properties are matched,
    /// and, for an appearance, the strings it shows.
    Content {
        shows: bool,
    },
    Other,
}

struct Walker<'a> {
    doc: &'a Document,
    m: Strs<'a>,
    seen: &'a mut HashSet<i32>,
    found: Vec<Finding>,
    streams: Vec<(i32, Class, StreamKind)>,
}

impl Walker<'_> {
    fn walk(&mut self, obj: &Object, path: &mut Vec<Vec<u8>>, holder: Option<i32>, depth: usize) {
        if depth > 256 {
            return;
        }
        let (value, num) = match obj {
            Object::Ref(r) => {
                if !self.seen.insert(r.num) {
                    return;
                }
                match self.doc.object(r.num) {
                    Some(v) => (v, Some(r.num)),
                    None => return,
                }
            }
            v => (v.clone(), holder),
        };
        let owned = path.clone();
        let keys: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        match &value {
            Object::String(s) => {
                if self.m.matches_text(s) {
                    self.found.push(Finding {
                        class: classify(&keys),
                        page: None,
                        object: num,
                        detail: format!(
                            "a target in /{}",
                            String::from_utf8_lossy(path.last().map_or(&b""[..], |k| k))
                        ),
                    });
                }
            }
            Object::Name(n)
                if keys.last() == Some(&&b"Dests"[..]) || keys.last() == Some(&&b"Names"[..]) =>
            {
                if self.m.matches(n) {
                    self.found.push(Finding {
                        class: Class::Outline,
                        page: None,
                        object: num,
                        detail: "a target in a destination name".into(),
                    });
                }
            }
            Object::Array(items) => {
                for item in items.iter() {
                    self.walk(item, path, num, depth + 1);
                }
            }
            Object::Dict(d) => {
                for (k, v) in d.iter() {
                    if k == b"Parent" || k == b"P" && !matches!(v, Object::String(_)) {
                        continue;
                    }
                    path.push(k.to_vec());
                    self.walk(v, path, num, depth + 1);
                    path.pop();
                }
            }
            Object::Stream(s) => {
                for (k, v) in s.dict.iter() {
                    if k == b"Parent" {
                        continue;
                    }
                    path.push(k.to_vec());
                    self.walk(v, path, num, depth + 1);
                    path.pop();
                }
                let class = classify(&keys);
                let kind = if keys.iter().any(|k| *k == b"Contents")
                    || s.dict.get(b"Subtype").is_some_and(|t| t.is_name(b"Form"))
                    || s.dict.get(b"PatternType").is_some()
                {
                    StreamKind::Content {
                        shows: keys.contains(&&b"AP"[..]),
                    }
                } else if keys
                    .iter()
                    .any(|k| matches!(*k, b"Metadata" | b"JS" | b"EF" | b"F" | b"UF"))
                    || s.dict
                        .get(b"Type")
                        .is_some_and(|t| t.is_name(b"EmbeddedFile") || t.is_name(b"Metadata"))
                {
                    StreamKind::Text
                } else {
                    StreamKind::Other
                };
                if let Some(n) = num {
                    self.streams.push((n, class, kind));
                }
            }
            _ => {}
        }
    }
}

/// A stream's text matched, or its marked-content properties and shown
/// strings.
fn content_strings(
    doc: &Document,
    m: Strs<'_>,
    num: i32,
    class: Class,
    kind: StreamKind,
    c: &mut Collector,
) {
    let Some(Object::Stream(s)) = doc.object(num) else {
        return;
    };
    match kind {
        StreamKind::Text => {
            if let Ok(data) = doc.stream_data(&s)
                && m.matches(&data)
            {
                c.push(Finding {
                    class,
                    page: None,
                    object: Some(num),
                    detail: "a target in the stream's data".into(),
                });
            }
        }
        StreamKind::Content { shows } => {
            let mut p = Strings {
                state: ProcState::default(),
                props: Vec::new(),
                shown: Vec::new(),
            };
            let obj = Object::Ref(crate::object::ObjRef::new(num, 0));
            let _ = content::process_raw_contents(doc, &obj, &mut p);
            for props in &p.props {
                if let Object::Dict(d) = props {
                    for (k, v) in d.iter() {
                        if let Object::String(t) = v
                            && m.matches_text(t)
                        {
                            c.push(Finding {
                                class: Class::Structure,
                                page: None,
                                object: Some(num),
                                detail: format!(
                                    "a target in marked-content /{}",
                                    String::from_utf8_lossy(k)
                                ),
                            });
                        }
                    }
                }
            }
            if shows && p.shown.iter().any(|t| m.matches(t)) {
                c.push(Finding {
                    class: Class::Annotations,
                    page: None,
                    object: Some(num),
                    detail: "a target shown by an appearance stream".into(),
                });
            }
        }
        StreamKind::Other => {}
    }
}

/// Collects marked-content properties and shown strings, nothing else.
struct Strings {
    state: ProcState,
    props: Vec<Object>,
    shown: Vec<Vec<u8>>,
}

impl Processor for Strings {
    fn op(&mut self, op: Op<'_>) -> Result<(), String> {
        match op {
            Op::BeginMarkedProperties { raw: Some(r), .. }
            | Op::MarkedPointProperties { raw: Some(r), .. } => self.props.push(r.clone()),
            Op::Show { text, .. } | Op::NextLineShow { text, .. } => self.shown.push(text.to_vec()),
            Op::NextLineShowSpaced { text, .. } => self.shown.push(text.to_vec()),
            Op::ShowArray(Some(Object::Array(a))) => {
                let mut s = Vec::new();
                for item in a.iter() {
                    if let Object::String(t) = item {
                        s.extend_from_slice(t);
                    }
                }
                self.shown.push(s);
            }
            _ => {}
        }
        Ok(())
    }

    fn state(&mut self) -> &mut ProcState {
        &mut self.state
    }

    fn wants_soft_mask(&self) -> bool {
        false
    }
}

/// The images drawn on a page, with where.
struct ImageSpy {
    images: Vec<(Image, Matrix)>,
}

impl Device for ImageSpy {
    fn fill_image(&mut self, image: &Image, ctm: Matrix, _: f32, _: ColorParams) {
        self.images.push((image.clone(), ctm));
    }

    fn fill_image_mask(
        &mut self,
        image: &Image,
        ctm: Matrix,
        _: &crate::colorspace::ColorSpace,
        _: &[f32],
        _: f32,
        _: ColorParams,
    ) {
        self.images.push((image.clone(), ctm));
    }
}

/// Every image drawn under an area has white pixels there (an image mask
/// paints none there); a redacted page has no thumbnail.
fn images(doc: &Document, index: i32, quads: &[Quad], c: &mut Collector) {
    let Ok(page) = doc.page(index) else { return };
    if !matches!(doc.dict_get(&page, b"Thumb"), Object::Null) {
        c.push(Finding {
            class: Class::Images,
            page: Some(index + 1),
            object: None,
            detail: "a redacted page keeps its thumbnail".into(),
        });
    }
    let mut spy = ImageSpy { images: Vec::new() };
    let _ = crate::run::run_page_contents_for(doc, &page, &mut spy, Default::default(), None);
    for (image, ctm) in spy.images {
        let area = Rect::new(0.0, 0.0, 1.0, 1.0).transform(ctm);
        let under: Vec<&Quad> = quads
            .iter()
            .filter(|q| !Rect::from_quad(**q).intersect(area).is_empty())
            .collect();
        if under.is_empty() || !image.image_mask && image.colorspace.is_none() {
            continue;
        }
        let Ok(pix) = crate::pixmap::from_image(doc, &image) else {
            c.push(Finding {
                class: Class::Images,
                page: Some(index + 1),
                object: image
                    .stream
                    .as_ref()
                    .and_then(|s| s.as_ref())
                    .map(|r| r.num),
                detail: "an image under a redacted area cannot be checked".into(),
            });
            continue;
        };
        // An image mask's samples are what it paints.
        let white = if image.image_mask
            || matches!(
                pix.colorspace,
                Some(crate::colorspace::ColorSpace::Cmyk)
                    | Some(crate::colorspace::ColorSpace::Separation { .. })
            ) {
            0
        } else {
            255
        };
        let n = if image.image_mask {
            pix.n
        } else {
            pix.n - usize::from(pix.alpha)
        };
        let mut leaked = 0usize;
        for q in under {
            // The pixels pdf_redact_image_imp blanks: the area's box in
            // image space, rounded out. A device's image matrix already
            // puts the first row at the top, so no rows are flipped.
            let inv = ctm.invert().post_scale(pix.w as f32, pix.h as f32);
            let r = Rect::from_quad(*q).transform(inv);
            let clamp = |v: f32, hi: usize| (v.max(0.0) as usize).min(hi);
            let x0 = clamp((r.x0 + 0.001).floor(), pix.w);
            let x1 = clamp((r.x1 - 0.001).ceil(), pix.w);
            let y0 = clamp((r.y0 + 0.001).floor(), pix.h);
            let y1 = clamp((r.y1 - 0.001).ceil(), pix.h);
            for y in y0..y1 {
                for x in x0..x1 {
                    let at = (y * pix.w + x) * pix.n;
                    if pix
                        .samples
                        .get(at..at + n)
                        .is_some_and(|px| px.iter().any(|&v| v != white))
                    {
                        leaked += 1;
                    }
                }
            }
        }
        if leaked > 0 {
            c.push(Finding {
                class: Class::Images,
                page: Some(index + 1),
                object: image
                    .stream
                    .as_ref()
                    .and_then(|s| s.as_ref())
                    .map(|r| r.num),
                detail: format!("{leaked} pixels under a redacted area are not blanked"),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_clean::tests::page_doc;
    use crate::object::{Dict, ObjRef};
    use std::sync::Arc;

    const SECRET: &str = "SECRET-4711";

    fn targets() -> Targets {
        Targets {
            needles: vec![(SECRET.to_string(), 0)],
            matcher: Matcher::new(&[SECRET.to_string()], &[], false)
                .unwrap_or_else(|e| panic!("{e}")),
            areas: Vec::new(),
        }
    }

    /// The literal targets: SECRET among decoys, some its prefixes,
    /// suffixes and near misses.
    fn many_texts() -> Vec<String> {
        let mut texts: Vec<String> = (0..300).map(|i| format!("decoy-{i:04}-x")).collect();
        texts.extend(
            [
                "SECRET-471",
                "ECRET-4711",
                "SECRET-47110",
                "SECRET 4711",
                "secret-4711",
                "S",
                "11",
                " ",
                "",
                "\0SECRET",
                "SECRET-4711\0tail",
            ]
            .map(String::from),
        );
        texts.insert(150, SECRET.to_string());
        texts
    }

    /// `verify_literals` gives what `verify` gives, for SECRET alone and
    /// among many targets.
    fn agree(doc: &Document) {
        for texts in [vec![SECRET.to_string()], many_texts()] {
            let t = Targets {
                needles: texts.iter().map(|s| (s.clone(), 0)).collect(),
                matcher: Matcher::new(&texts, &[], false).unwrap_or_else(|e| panic!("{e}")),
                areas: Vec::new(),
            };
            let slow = verify(doc, &t);
            let fast = verify_literals(doc, &texts).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(fast.findings, slow.findings);
            assert_eq!(fast.hits_after, slow.hits_after);
            assert_eq!(fast.revisions, slow.revisions);
        }
    }

    fn classes(doc: &Document) -> Vec<Class> {
        agree(doc);
        let mut c: Vec<Class> = verify(doc, &targets())
            .findings
            .iter()
            .map(|f| f.class)
            .collect();
        c.dedup();
        c
    }

    fn s(text: &str) -> Object {
        Object::String(Arc::from(text.as_bytes()))
    }

    #[test]
    fn a_clean_page_verifies() {
        let doc = page_doc(b"BT /F1 12 Tf 72 700 Td (nothing here) Tj ET");
        agree(&doc);
        assert!(verify(&doc, &targets()).verified());
    }

    #[test]
    fn classes_have_the_report_names() {
        let names: Vec<&str> = Class::ALL.iter().map(|c| c.name()).collect();
        assert_eq!(
            names,
            [
                "content_text",
                "structure",
                "annotations",
                "metadata",
                "outline",
                "embedded_files",
                "hidden_content",
                "images",
                "history",
                "javascript",
            ]
        );
    }

    #[test]
    fn page_text_and_hidden_text_leak() {
        let doc = page_doc(format!("BT 3 Tr /F1 12 Tf 72 700 Td ({SECRET}) Tj ET").as_bytes());
        assert_eq!(classes(&doc), [Class::ContentText]);
        assert_eq!(verify(&doc, &targets()).hits_after, [1]);
    }

    #[test]
    fn strings_leak_by_where_they_sit() {
        let doc = page_doc(b"BT /F1 12 Tf 72 700 Td (nothing) Tj ET");
        // Metadata.
        let mut info = Dict::new();
        info.put(b"Title", s(SECRET));
        let num = doc.add_object(Object::Dict(Arc::new(info)));
        let mut trailer = doc.trailer();
        trailer.put(b"Info", Object::Ref(ObjRef::new(num, 0)));
        doc.set_trailer(trailer);
        assert_eq!(classes(&doc), [Class::Metadata]);
        // An annotation's contents.
        let mut annot = Dict::new();
        annot.put(b"Subtype", Object::name(b"Text"));
        annot.put(b"Contents", s(&format!("note {SECRET}")));
        let a = doc.add_object(Object::Dict(Arc::new(annot)));
        crate::edit::edit_page(&doc, 0, &mut |d| {
            d.put(
                b"Annots",
                Object::Array(Arc::new(vec![Object::Ref(ObjRef::new(a, 0))])),
            )
        })
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(classes(&doc), [Class::Annotations, Class::Metadata]);
    }

    #[test]
    fn objects_nothing_reaches_are_history() {
        let doc = page_doc(b"BT /F1 12 Tf 72 700 Td (nothing) Tj ET");
        doc.add_object(s(SECRET));
        assert_eq!(classes(&doc), [Class::History]);
    }

    #[test]
    fn marked_content_properties_leak() {
        let doc = page_doc(
            format!("/Span <</ActualText ({SECRET})>> BDC BT /F1 12 Tf 72 700 Td (XXXX) Tj ET EMC")
                .as_bytes(),
        );
        let found = classes(&doc);
        assert!(found.contains(&Class::Structure), "{found:?}");
    }

    /// A one-page document: the catalog with `catalog` added, the page with
    /// `page` added drawing `content` with Helvetica as /F1, and `extra`
    /// as objects 6 onwards.
    fn doc_with(catalog: &str, page: &str, content: &[u8], extra: &[Vec<u8>]) -> Document {
        let mut objs = vec![
            format!("<</Type/Catalog/Pages 2 0 R{catalog}>>").into_bytes(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R{page}>>"
            )
            .into_bytes(),
            crate::testutil::stream("", content),
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_vec(),
        ];
        objs.extend(extra.iter().cloned());
        crate::testutil::doc_bytes(&objs)
    }

    /// The classes found, in order, each once.
    fn sorted(doc: &Document) -> Vec<Class> {
        let mut c = classes(doc);
        c.sort();
        c.dedup();
        c
    }

    #[test]
    fn each_place_a_string_sits_has_its_class() {
        let t = SECRET;
        let cases: [(&str, Class); 9] = [
            (
                &format!("/Names<</JavaScript<</Names[(a)<</S/JavaScript/JS({t})>>]>>>>"),
                Class::JavaScript,
            ),
            (
                &format!("/Names<</EmbeddedFiles<</Names[(f)<</Type/Filespec/F({t})>>]>>>>"),
                Class::EmbeddedFiles,
            ),
            (&format!("/Metadata<</X({t})>>"), Class::Metadata),
            (&format!("/Outlines<</Title({t})>>"), Class::Outline),
            (
                &format!("/PageLabels<</Nums[0<</P({t})>>]>>"),
                Class::Outline,
            ),
            // A destination named by a name in a name tree.
            (
                &format!("/Names<</Dests<</Names[/{t}[3 0 R/Fit]]>>>>"),
                Class::Outline,
            ),
            (
                &format!("/OCProperties<</D<</Name({t})>>>>"),
                Class::HiddenContent,
            ),
            (
                &format!("/StructTreeRoot<</K<</S/P/T({t})>>>>"),
                Class::Structure,
            ),
            (
                &format!("/AcroForm<</Fields[<</T({t})>>]>>"),
                Class::Annotations,
            ),
        ];
        for (catalog, class) in cases {
            let doc = doc_with(catalog, "", b"", &[]);
            assert_eq!(sorted(&doc), [class], "{catalog}");
        }
        // Anywhere else counts as the page's text.
        let doc = doc_with(&format!("/Foo({t})"), "", b"", &[]);
        assert_eq!(sorted(&doc), [Class::ContentText]);
        // A name elsewhere is no destination.
        let doc = doc_with(&format!("/Foo/{t}"), "", b"", &[]);
        agree(&doc);
        assert!(verify(&doc, &targets()).verified());
    }

    #[test]
    fn streams_are_matched_by_what_they_hold() {
        let t = SECRET;
        // 6: XMP; 7: a script; 8: an embedded file; 9: an appearance that
        // shows the target with each text operator; 10: a form whose
        // marked point names it; 11: a stream naming a parent, to be
        // skipped; 12: an image, whose data is not text.
        let doc = doc_with(
            "/Metadata 6 0 R/Names<</JavaScript<</Names[(a)<</S/JavaScript/JS 7 0 R>>]>>\
             /EmbeddedFiles<</Names[(f)<</Type/Filespec/F(f)/EF<</F 8 0 R>>>>]>>>>",
            "/Annots[<</Subtype/Text/Rect[0 0 1 1]/AP<</N 9 0 R>>>>]/Resources<</XObject<</X 10 0 R/I 12 0 R>>>>/Foo 11 0 R",
            b"",
            &[
                crate::testutil::stream("/Type/Metadata/Subtype/XML", t.as_bytes()),
                crate::testutil::stream("", format!("app.alert('{t}')").as_bytes()),
                crate::testutil::stream("/Type/EmbeddedFile", t.as_bytes()),
                crate::testutil::stream(
                    "/Subtype/Form/BBox[0 0 1 1]",
                    format!("BT ({t}) Tj ({t}) ' 1 2 ({t}) \" [(S) ({t})] TJ ET").as_bytes(),
                ),
                crate::testutil::stream(
                    "/Subtype/Form/BBox[0 0 1 1]",
                    format!("/P <</Alt({t})>> DP").as_bytes(),
                ),
                crate::testutil::stream("/Parent 3 0 R", b"plain"),
                crate::testutil::stream(
                    "/Subtype/Image/Width 11/Height 1/BitsPerComponent 8/ColorSpace/DeviceGray",
                    t.as_bytes(),
                ),
            ],
        );
        agree(&doc);
        let v = verify(&doc, &targets());
        let found: Vec<(Class, Option<i32>)> =
            v.findings.iter().map(|f| (f.class, f.object)).collect();
        assert!(found.contains(&(Class::Metadata, Some(6))), "{found:?}");
        assert!(found.contains(&(Class::JavaScript, Some(7))), "{found:?}");
        assert!(
            found.contains(&(Class::EmbeddedFiles, Some(8))),
            "{found:?}"
        );
        assert!(found.contains(&(Class::Annotations, Some(9))), "{found:?}");
        assert!(found.contains(&(Class::Structure, Some(10))), "{found:?}");
        assert!(!found.iter().any(|(_, o)| *o == Some(12)), "{found:?}");
    }

    #[test]
    fn unreachable_streams_are_history_unless_binary_or_too_deep() {
        let t = SECRET;
        let deep = format!("{}({t}){}", "[".repeat(40), "]".repeat(40));
        let doc = doc_with(
            "",
            "",
            b"",
            &[
                crate::testutil::stream("", t.as_bytes()),
                crate::testutil::stream("/Length1 11", t.as_bytes()),
                crate::testutil::stream("/Subtype/Type1C", t.as_bytes()),
                deep.into_bytes(),
                format!("<</Ref 99 0 R/X[({t})]>>").into_bytes(),
            ],
        );
        agree(&doc);
        let objects: Vec<Option<i32>> = verify(&doc, &targets())
            .findings
            .iter()
            .map(|f| f.object)
            .collect();
        assert_eq!(objects, [Some(6), Some(10)]);
    }

    #[test]
    fn earlier_revisions_are_history() {
        let path = format!(
            "{}/../../corpus/smoke/incremental-update.pdf",
            env!("CARGO_MANIFEST_DIR")
        );
        let doc = Document::open(std::fs::read(path).unwrap_or_else(|e| panic!("{e}")))
            .unwrap_or_else(|e| panic!("{e:?}"));
        agree(&doc);
        let v = verify(&doc, &targets());
        assert_eq!(v.revisions, 2);
        assert_eq!(sorted(&doc), [Class::History]);
    }

    #[test]
    fn text_only_hidden_layers_show_is_hidden_content() {
        let doc = doc_with(
            "/OCProperties<</OCGs[6 0 R]/D<</OFF[6 0 R]>>>>",
            "/Resources<</Font<</F1 5 0 R>>/Properties<</oc 6 0 R>>>>",
            format!("/OC /oc BDC BT /F1 12 Tf 72 700 Td ({SECRET}) Tj ET EMC").as_bytes(),
            &[b"<</Type/OCG/Name(layer)>>".to_vec()],
        );
        agree(&doc);
        let v = verify(&doc, &targets());
        assert_eq!(sorted(&doc), [Class::HiddenContent]);
        assert_eq!(v.hits_after, [1]);
    }

    #[test]
    fn pixels_under_an_area_must_be_blank_after_redaction() {
        // A grey image at (100, 600) and an image mask at (300, 600), each
        // 2x2 and 100 wide, and a thumbnail.
        let content = b"q 100 0 0 100 100 600 cm /Im Do Q q 0 g 100 0 0 100 300 600 cm /M Do Q";
        let doc = doc_with(
            "",
            "/Resources<</XObject<</Im 6 0 R/M 7 0 R>>>>/Thumb 8 0 R",
            content,
            &[
                crate::testutil::stream(
                    "/Subtype/Image/Width 2/Height 2/BitsPerComponent 8/ColorSpace/DeviceGray",
                    &[1, 2, 3, 4],
                ),
                crate::testutil::stream("/Subtype/Image/Width 2/Height 2/ImageMask true", &[0, 0]),
                crate::testutil::stream("", b"thumb"),
            ],
        );
        // The left column of each, in page space.
        let page_quad = |x0: f32, y0: f32, x1: f32, y1: f32| Quad {
            ul: crate::geometry::Point { x: x0, y: y0 },
            ur: crate::geometry::Point { x: x1, y: y0 },
            ll: crate::geometry::Point { x: x0, y: y1 },
            lr: crate::geometry::Point { x: x1, y: y1 },
        };
        let areas = vec![
            page_quad(90.0, 82.0, 150.0, 202.0),
            page_quad(290.0, 82.0, 350.0, 202.0),
        ];
        let t = Targets {
            areas: vec![(0, areas.clone())],
            ..targets()
        };
        let before = verify(&doc, &t);
        let details: Vec<&str> = before.findings.iter().map(|f| f.detail.as_str()).collect();
        assert_eq!(
            details,
            [
                "a redacted page keeps its thumbnail",
                "2 pixels under a redacted area are not blanked",
                "2 pixels under a redacted area are not blanked",
            ]
        );
        let page = doc.page(0).unwrap_or_else(|e| panic!("{e}"));
        let (_, ctm) = doc.page_transform(&page).unwrap_or_else(|e| panic!("{e}"));
        let pdf: Vec<Quad> = areas
            .iter()
            .map(|q| crate::redact::quad_to_pdf(*q, ctm))
            .collect();
        let redacted =
            crate::redact::redact_page(&doc, 0, &pdf, crate::redact::RedactOptions::default());
        assert_eq!(redacted, Ok(true));
        let m = Matcher::new(&[], &[], false).unwrap_or_else(|e| panic!("{e}"));
        crate::scrub::scrub(&doc, &m, &[0]);
        assert!(verify(&doc, &t).verified());
    }

    /// A deterministic xorshift, for varied inputs without a dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len())]
        }
    }

    #[test]
    fn many_finds_what_a_scan_for_each_finds() {
        for seed in 1..=200u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let alphabet = [b'a', b'b', b'c', 0, 0xc3, 0xa9, 0xff];
            let word = |rng: &mut Rng, max: usize| -> Vec<u8> {
                (0..rng.below(max)).map(|_| *rng.pick(&alphabet)).collect()
            };
            let hay = word(&mut rng, 60);
            let mut needles: Vec<Vec<u8>> = (0..30).map(|_| word(&mut rng, 6)).collect();
            // Pieces of the haystack, and their prefixes and suffixes.
            for _ in 0..10 {
                let a = rng.below(hay.len() + 1);
                let b = a + rng.below(hay.len() - a + 1);
                let piece = hay[a..b].to_vec();
                needles.push(piece[..rng.below(piece.len() + 1)].to_vec());
                needles.push(piece[rng.below(piece.len() + 1)..].to_vec());
                needles.push(piece);
            }
            let many = Many::new(needles.iter().enumerate().map(|(k, n)| (k, n.as_slice())));
            for _ in 0..2 {
                let mut found = Vec::new();
                many.found(&hay, &mut found);
                let mut sorted = found.clone();
                sorted.sort_unstable();
                sorted.dedup();
                assert_eq!(sorted.len(), found.len(), "each once");
                let want: Vec<usize> = (0..needles.len())
                    .filter(|&k| {
                        !needles[k].is_empty()
                            && hay.windows(needles[k].len()).any(|w| w == needles[k])
                    })
                    .collect();
                assert_eq!(sorted, want, "seed {seed}");
            }
        }
    }

    /// A page of `lines`, each a list of (font, string) runs: F1 is
    /// Helvetica in WinAnsi, L Helvetica with fi and fl at codes 1 and 2,
    /// and M Helvetica whose ToUnicode makes a an e with a combining
    /// acute, b a fullwidth A, c a lone acute and d a soft hyphen.
    fn runs_doc(blocks: &[Vec<Vec<(&str, String)>>]) -> Document {
        let cmap = "/CIDInit/ProcSet findresource begin 12 dict begin begincmap /CMapName/M def \
            1 begincodespacerange <00><FF> endcodespacerange 4 beginbfchar <61><00650301> \
            <62><FF21> <63><0301> <64><00AD> endbfchar endcmap CMapName currentdict /CMap \
            defineresource pop end end";
        let mut content = String::new();
        for (b, lines) in blocks.iter().enumerate() {
            content.push_str(&format!("BT 20 {} Td\n", 760 - 120 * b as i32));
            for line in lines {
                for (font, text) in line {
                    content.push_str(&format!("/{font} 10 Tf ({text}) Tj "));
                }
                content.push_str("0 -12 Td\n");
            }
            content.push_str("ET\n");
        }
        crate::testutil::doc_bytes(&[
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<<\
              /F1<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>\
              /L<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding<</Differences[1/fi/fl]>>>>\
              /M<</Type/Font/Subtype/Type1/BaseFont/Helvetica/ToUnicode 5 0 R>>>>>>>>"
                .to_vec(),
            crate::testutil::stream("", content.as_bytes()),
            crate::testutil::stream("", cmap.as_bytes()),
        ])
    }

    /// A run: its font, its string as the content stream writes it, and
    /// its text.
    const RUNS: &[(&str, &str, &str)] = &[
        ("F1", "Alice", "Alice"),
        ("F1", "alice", "alice"),
        ("F1", "VANCE", "VANCE"),
        ("F1", "Vance", "Vance"),
        ("F1", "415-555", "415-555"),
        ("F1", "0199", "0199"),
        ("F1", "Caf\\351", "Caf\u{e9}"),
        ("F1", "CAF\\311", "CAF\u{c9}"),
        ("F1", "cafe", "cafe"),
        ("F1", " ", " "),
        ("F1", "   ", "   "),
        ("F1", "-", "-"),
        ("F1", "\\255", "\u{ad}"),
        ("F1", "co-signer", "co-signer"),
        ("F1", "\\(x\\)", "(x)"),
        ("L", "o\\001ce", "o\u{fb01}ce"),
        ("L", "\\002ow", "\u{fb02}ow"),
        ("M", "a", "e\u{301}"),
        ("M", "b", "\u{ff21}"),
        ("M", "c", "\u{301}"),
        ("M", "d", "\u{ad}"),
        ("M", "ab", "e\u{301}\u{ff21}"),
    ];

    /// A needle made from `text`: a piece of it, perhaps cased
    /// differently, its spaces doubled or dropped, a hyphen put in or a
    /// ligature or composed form used.
    fn needle_from(rng: &mut Rng, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let a = rng.below(chars.len() + 1);
        let b = (a + 1 + rng.below(12)).min(chars.len());
        let mut n: String = chars[a.min(b)..b].iter().collect();
        match rng.below(9) {
            0 => n = n.to_uppercase(),
            1 => n = n.to_lowercase(),
            2 => n = n.replace(' ', "   "),
            3 => n = n.replace(' ', ""),
            4 => n = n.replace("fi", "\u{fb01}").replace("e\u{301}", "\u{e9}"),
            5 => n = n.replace('\u{e9}', "e\u{301}").replace('\u{fb01}', "fi"),
            6 => {
                let at = n
                    .char_indices()
                    .nth(rng.below(n.chars().count() + 1))
                    .map_or(n.len(), |(i, _)| i);
                n.insert(at, *rng.pick(&['-', '\u{ad}', ' ', '\u{2010}']));
            }
            7 => n = n.replace('\u{ad}', "").replace('-', "\u{2011}"),
            _ => {}
        }
        n
    }

    #[test]
    fn page_needles_find_every_page_the_search_finds() {
        let opts_list = [
            0,
            crate::search::options::IGNORE_CASE,
            crate::search::options::IGNORE_DIACRITICS,
            crate::search::options::IGNORE_CASE | crate::search::options::IGNORE_DIACRITICS,
            crate::search::options::KEEP_HYPHENS,
            crate::search::options::KEEP_LINES,
            crate::search::options::KEEP_PARAGRAPHS,
            crate::search::options::REGEXP,
        ];
        let (mut hits, mut candidates, mut pairs) = (0usize, 0usize, 0usize);
        for seed in 1..=60u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut text = String::new();
            let blocks: Vec<Vec<Vec<(&str, String)>>> = (0..1 + rng.below(3))
                .map(|_| {
                    (0..1 + rng.below(4))
                        .map(|_| {
                            let line: Vec<(&str, String)> = (0..1 + rng.below(6))
                                .map(|_| {
                                    let (font, raw, plain) = *rng.pick(RUNS);
                                    text.push_str(plain);
                                    (font, raw.to_string())
                                })
                                .collect();
                            text.push(' ');
                            line
                        })
                        .collect()
                })
                .collect();
            let doc = runs_doc(&blocks);
            let mut needles: Vec<(String, u32)> = (0..40)
                .map(|_| (needle_from(&mut rng, &text), *rng.pick(&opts_list)))
                .collect();
            // Needles that are pieces of one another.
            for _ in 0..10 {
                let (n, o) = needles[rng.below(needles.len())].clone();
                let cut = n
                    .char_indices()
                    .nth(1 + rng.below(4))
                    .map_or(n.len(), |(i, _)| i);
                needles.push((n[..cut].to_string(), o));
                needles.push((n[cut..].to_string(), o));
                needles.push((format!("{n}{n}"), o));
            }
            let fast = PageNeedles::new(&needles);
            let obj = doc.page(0).unwrap_or_else(|e| panic!("{e}"));
            for opts in [
                "",
                "dehyphenate",
                "preserve-ligatures",
                "preserve-whitespace",
            ] {
                let o = Options::parse(opts).unwrap_or_else(|e| panic!("{e}"));
                let page = crate::stext::extract_page(&doc, &obj, o, Default::default())
                    .0
                    .unwrap_or_else(|e| panic!("{e}"));
                let found = fast.candidates(&page);
                let slow = count_hits(&page, &needles);
                for (k, &n) in slow.iter().enumerate() {
                    pairs += 1;
                    if n > 0 {
                        hits += 1;
                        assert!(
                            found.contains(&k),
                            "seed {seed} {opts:?}: {:?} has {n} hits but is no candidate in {text:?}",
                            needles[k]
                        );
                    }
                }
                candidates += found.len();
                assert_eq!(fast.count(&page), slow, "seed {seed} {opts:?}");
            }
        }
        // The pages had hits to miss.
        assert!(
            hits > 1000,
            "{hits} hits of {pairs}, {candidates} candidates"
        );
    }

    /// A string to match outside the page text: pieces of the targets with
    /// their white space changed, and other bytes.
    fn varied_string(rng: &mut Rng, texts: &[String]) -> Vec<u8> {
        let pieces: &[&[u8]] = &[
            b" ",
            b"  ",
            b"\t",
            b"\r\n",
            b"\n",
            "\u{a0}".as_bytes(),
            b"\0",
            b"x",
            b"-",
            b".",
            b"\xc3",
            b"\xff",
            b"\xc0\x80",
            "\u{e9}".as_bytes(),
            "\u{2003}".as_bytes(),
        ];
        let mut out = Vec::new();
        for _ in 0..rng.below(12) {
            if rng.below(2) == 0 {
                let t = rng.pick(texts);
                let ws: &[u8] = rng.pick(&[&b" "[..], b"\t\t", b"\r\n ", b"", "\u{a0}".as_bytes()]);
                let mut piece = Vec::new();
                for c in t.chars() {
                    if c.is_whitespace() {
                        piece.extend_from_slice(ws);
                    } else {
                        piece.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                    }
                }
                let cut = rng.below(piece.len() + 1);
                match rng.below(3) {
                    0 => out.extend_from_slice(&piece[..cut]),
                    1 => out.extend_from_slice(&piece[cut..]),
                    _ => out.extend_from_slice(&piece),
                }
            } else {
                out.extend_from_slice(rng.pick(pieces));
            }
        }
        out
    }

    #[test]
    fn literals_match_every_string_the_patterns_match() {
        let base = [
            "Alice Vance",
            "Alice  Vance",
            " Vance",
            "Vance\t",
            "415 555 0199",
            "4155550199",
            "a.b*c",
            "(x)",
            "[y]",
            "caf\u{e9}",
            "x\u{a0}y",
            "x\u{2003}y",
            "e\0f",
            "\0ef",
            "  \0e",
            "e",
            " e ",
            "\u{e9}",
            "\u{fffd}x",
            "--",
            "",
            "  ",
            "\\d",
        ];
        let (mut hits, mut pairs) = (0usize, 0usize);
        for seed in 1..=60u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut texts: Vec<String> = base.iter().map(|s| s.to_string()).collect();
            for _ in 0..10 {
                let t = rng.pick(&base).to_string();
                let cut = t
                    .char_indices()
                    .nth(rng.below(t.chars().count() + 1))
                    .map_or(t.len(), |(i, _)| i);
                texts.push(t[..cut].to_string());
                texts.push(t[cut..].to_string());
            }
            let each: Vec<Matcher> = texts
                .iter()
                .map(|t| {
                    Matcher::new(std::slice::from_ref(t), &[], false)
                        .unwrap_or_else(|e| panic!("{e}"))
                })
                .collect();
            let all = Matcher::new(&texts, &[], false).unwrap_or_else(|e| panic!("{e}"));
            let fast = Literals::new(&texts).unwrap_or_else(|e| panic!("{e}"));
            for _ in 0..100 {
                let s = varied_string(&mut rng, &texts);
                let mut found = fast.always.clone();
                fast.many.found(&literal_canon(&s), &mut found);
                for (k, m) in each.iter().enumerate() {
                    pairs += 1;
                    if m.matches(&s) {
                        hits += 1;
                        assert!(found.contains(&k), "{:?} matches {s:?}", texts[k]);
                    }
                }
                assert_eq!(fast.matches(&s), all.matches(&s), "{s:?}");
                // As a PDF text string, in UTF-16 too.
                let mut utf16 = vec![0xfe, 0xff];
                for u in String::from_utf8_lossy(&s).encode_utf16() {
                    utf16.extend(u.to_be_bytes());
                }
                for pdf in [&s, &utf16] {
                    assert_eq!(fast.matches_text(pdf), all.matches_text(pdf), "{pdf:?}");
                }
            }
        }
        assert!(hits > 1000, "{hits} hits of {pairs}");
    }

    #[test]
    fn a_clean_document_runs_no_full_matcher() {
        // 30 pages of text that holds none of 2,000 targets.
        let pages: Vec<String> = (0..30)
            .map(|p| {
                format!("BT /F1 9 Tf 40 700 Td (Page {p}: 415-555-01{p:02} and nothing else) Tj ET")
            })
            .collect();
        let mut objs = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            {
                let kids: Vec<String> = (0..pages.len())
                    .map(|p| format!("{} 0 R", 4 + 2 * p))
                    .collect();
                format!(
                    "<</Type/Pages/Kids[{}]/Count {}>>",
                    kids.join(" "),
                    pages.len()
                )
                .into_bytes()
            },
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_vec(),
        ];
        for (p, content) in pages.iter().enumerate() {
            objs.push(
                format!(
                    "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 3 0 R>>>>\
                     /Contents {} 0 R/Annots[<</Subtype/Text/Rect[0 0 1 1]/Contents(note {p})>>]>>",
                    5 + 2 * p
                )
                .into_bytes(),
            );
            objs.push(crate::testutil::stream("", content.as_bytes()));
        }
        let doc = crate::testutil::doc_bytes(&objs);
        let texts: Vec<String> = (0..2000).map(|i| format!("415-555-{i:04}x")).collect();
        let before = full_calls();
        let v = verify_literals(&doc, &texts).unwrap_or_else(|e| panic!("{e}"));
        assert!(v.verified());
        assert_eq!(full_calls() - before, 0);
        // One leak costs its own searches, not every target's.
        let mut leaky = texts.clone();
        leaky.push("nothing else".into());
        let before = full_calls();
        let v = verify_literals(&doc, &leaky).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(v.findings.len(), 30);
        // Each page's text twice (with and without hidden layers).
        assert_eq!(full_calls() - before, 60);
    }
}
