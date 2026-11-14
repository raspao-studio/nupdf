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
        .map(|(n, o)| crate::search::count_page(page, n, *o).unwrap_or(0))
        .collect()
}

/// Runs every check.
pub fn verify(doc: &Document, t: &Targets) -> Verification {
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
    let mut hits_after = vec![0usize; t.needles.len()];
    for i in 0..count {
        // One page's text at a time: each is dropped before the next.
        let Some(with_hidden) =
            page_text(doc, i, "clip=no,all-layers").map(|all| count_hits(&all, &t.needles))
        else {
            continue;
        };
        if with_hidden.iter().all(|&n| n == 0) {
            continue;
        }
        let visible = page_text(doc, i, "clip=no")
            .map(|p| count_hits(&p, &t.needles))
            .unwrap_or_else(|| vec![0; t.needles.len()]);
        for (k, (needle, _)) in t.needles.iter().enumerate() {
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
    if !t.matcher.is_empty() {
        objects(doc, &t.matcher, &mut c);
    }
    for (index, quads) in &t.areas {
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
fn objects(doc: &Document, m: &Matcher, c: &mut Collector) {
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
    m: &'a Matcher,
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
    m: &Matcher,
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

    fn classes(doc: &Document) -> Vec<Class> {
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
}
