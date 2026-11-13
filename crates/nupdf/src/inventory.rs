// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/tools/pdfinfo.c (gatherresourceinfo,
// gatherfonts, gatherimages) and source/tools/pdfextract.c (savefont),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! The fonts and images a document's pages use, found as `mutool info`
//! walks each page's resources, and the font files `mutool extract`
//! writes.

use std::collections::{HashMap, HashSet};

use crate::document::Document;
use crate::garbage;
use crate::object::{ObjRef, Object};

/// A font dictionary, where `mutool info` first finds it.
#[derive(Debug, Clone, PartialEq)]
pub struct FontUse {
    /// 1-based.
    pub page: i32,
    pub page_object: i32,
    /// The dictionary as the resources hold it (usually a reference).
    pub dict: Object,
    pub subtype: Object,
    /// /BaseFont, or /Name when there is none.
    pub name: Object,
    /// /Encoding, or a dictionary's /BaseEncoding.
    pub encoding: Object,
}

/// An image XObject, where `mutool info` first finds it.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageUse {
    pub page: i32,
    pub page_object: i32,
    pub dict: Object,
    pub width: Object,
    pub height: Object,
    pub bpc: Object,
    pub filter: Object,
    /// The colorspace, or the family name of an array.
    pub colorspace: Object,
    /// A Separation's or DeviceN's alternate space.
    pub alternate: Object,
}

/// What the walk found, and the pages it could not walk.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inventory {
    /// The page count before the walk (finding pages can repair the tree),
    /// None when the page tree gives none.
    pub page_count: Option<i32>,
    pub fonts: Vec<FontUse>,
    pub images: Vec<ImageUse>,
    /// 1-based page numbers with their error.
    pub page_errors: Vec<(i32, String)>,
}

/// The fonts and images every page's resources reach, through fonts,
/// XObjects and patterns, as `mutool info` gathers them: each page's own
/// /Resources (not inherited ones), objects deduplicated with
/// `pdf_objcmp`, each object visited once per page.
pub fn inventory(doc: &Document) -> Inventory {
    let mut inv = Inventory {
        page_count: doc.page_count().ok(),
        ..Inventory::default()
    };
    let count = inv.page_count.unwrap_or(0);
    let limits = doc.limits();
    let (mut fonts, mut images) = (Seen::default(), Seen::default());
    // Resources pages share, by reference, with the walk of the first
    // page that walked them cleanly.
    let mut shared: HashMap<ObjRef, Walked> = HashMap::new();
    for index in 0..count {
        let page = index + 1;
        let pageref = match doc.page(index) {
            Ok(p) => p,
            Err(e) => {
                inv.page_errors.push((page, e.0));
                continue;
            }
        };
        let page_object = pageref.as_ref().map_or(0, |r| r.num);
        let key = stored(doc, &pageref, b"Resources").as_ref();
        let repaired = doc.repair_attempted();
        if let Some(w) = key.and_then(|k| shared.get(&k))
            && w.repaired == repaired
            && !w.marks.contains(&page_object)
        {
            // This page's walk would go as that one did, finding nothing
            // new and saying nothing.
            continue;
        }
        let mut walk = Walk {
            doc,
            page,
            page_object,
            marks: HashSet::new(),
            budget: limits.max_xref_entries.max(1 << 16),
            hit_page: false,
            fonts: &mut fonts,
            images: &mut images,
        };
        let ((), said) = crate::diag::capture(|| walk.resources(&pageref, &mut inv));
        let clean = said.is_empty() && !walk.hit_page && doc.repair_attempted() == repaired;
        doc.diagnostics().replay(said);
        if let Some(k) = key
            && clean
        {
            let marks = std::mem::take(&mut walk.marks);
            shared.entry(k).or_insert(Walked { marks, repaired });
        }
    }
    inv
}

/// A page's walk that another page sharing its /Resources would repeat:
/// it gave no warnings and did not come back to its page. The other walk
/// goes the same way unless it meets its own page, which it marks first,
/// among the objects this one marked, or the document has been repaired
/// since; the fonts and images it would find are found already. MuPDF
/// walks them again, which a document whose thousands of pages share one
/// dictionary of thousands of forms makes the whole run.
struct Walked {
    marks: HashSet<i32>,
    repaired: bool,
}

/// The dictionaries already found, to tell a new one from them as
/// `pdf_objcmp` against each would: a reference is the same only as the
/// same reference, so those are looked up; a direct dictionary is
/// compared with the direct ones found.
#[derive(Default)]
struct Seen {
    refs: HashSet<ObjRef>,
    direct: Vec<Object>,
}

impl Seen {
    /// Whether `dict` is one already found; if not, it is now.
    fn found(&mut self, dict: &Object) -> bool {
        match dict {
            Object::Ref(r) => !self.refs.insert(*r),
            _ if self.direct.iter().any(|d| !garbage::differ(d, dict)) => true,
            _ => {
                self.direct.push(dict.clone());
                false
            }
        }
    }
}

struct Walk<'a> {
    doc: &'a Document,
    page: i32,
    page_object: i32,
    /// `pdf_mark_list`: object numbers visited on this page. MuPDF's is a
    /// list it searches whole at each push, which a page of tens of
    /// thousands of resources makes the walk's whole time.
    marks: HashSet<i32>,
    /// Objects the walk may still visit; MuPDF's walk is bounded only by
    /// the mark list, which direct objects never enter.
    budget: usize,
    /// Whether the walk came back to its page's object.
    hit_page: bool,
    fonts: &'a mut Seen,
    images: &'a mut Seen,
}

impl Walk<'_> {
    /// `pdf_mark_list_push`: true when the object was already visited.
    fn mark(&mut self, obj: &Object) -> bool {
        let num = obj.as_ref().map_or(0, |r| r.num);
        // Numbers of 0 and below (direct objects) are never found.
        let seen = num > 0 && !self.marks.insert(num);
        if seen && num == self.page_object {
            self.hit_page = true;
        }
        seen
    }

    fn values(&self, dict: &Object) -> Vec<Object> {
        self.doc
            .resolve(dict)
            .as_dict()
            .map(|d| d.iter().map(|(_, v)| v.clone()).collect())
            .unwrap_or_default()
    }

    /// `gatherresourceinfo` with everything shown.
    fn resources(&mut self, obj: &Object, inv: &mut Inventory) {
        if self.budget == 0 || self.mark(obj) {
            return;
        }
        self.budget -= 1;
        let doc = self.doc;
        let rsrc = doc.dict_get(obj, b"Resources");
        let stored = |key: &[u8]| {
            doc.resolve(&rsrc)
                .as_dict()
                .and_then(|d| d.get(key).cloned())
                .filter(|o| !matches!(o, Object::Null))
        };
        if let Some(font) = stored(b"Font")
            && !self.mark(&font)
        {
            self.fonts(&font, inv);
            for f in self.values(&font) {
                self.resources(&f, inv);
            }
        }
        if let Some(xobj) = stored(b"XObject")
            && !self.mark(&xobj)
        {
            self.images(&xobj, inv);
            for x in self.values(&xobj) {
                self.resources(&x, inv);
            }
        }
        if let Some(shade) = stored(b"Shading") {
            self.mark(&shade);
        }
        if let Some(pattern) = stored(b"Pattern")
            && !self.mark(&pattern)
        {
            for p in self.values(&pattern) {
                self.resources(&p, inv);
            }
        }
    }

    /// `gatherfonts`.
    fn fonts(&mut self, dict: &Object, inv: &mut Inventory) {
        let doc = self.doc;
        for fontdict in self.values(dict) {
            if doc.resolve(&fontdict).as_dict().is_none() {
                doc.diagnostics().warn(
                    "inventory.not_a_font",
                    format!(
                        "not a font dict ({} 0 R)",
                        fontdict.as_ref().map_or(0, |r| r.num)
                    ),
                );
                continue;
            }
            let get = |key: &[u8]| stored(doc, &fontdict, key);
            let subtype = get(b"Subtype");
            let basefont = get(b"BaseFont");
            let name = if matches!(doc.resolve(&basefont), Object::Null) {
                get(b"Name")
            } else {
                basefont
            };
            let mut encoding = get(b"Encoding");
            if doc.resolve(&encoding).as_dict().is_some() {
                encoding = stored(doc, &encoding, b"BaseEncoding");
            }
            if self.fonts.found(&fontdict) {
                continue;
            }
            inv.fonts.push(FontUse {
                page: self.page,
                page_object: self.page_object,
                dict: fontdict,
                subtype,
                name,
                encoding,
            });
        }
    }

    /// `gatherimages`.
    fn images(&mut self, dict: &Object, inv: &mut Inventory) {
        let doc = self.doc;
        for imagedict in self.values(dict) {
            if doc.resolve(&imagedict).as_dict().is_none() {
                doc.diagnostics().warn(
                    "inventory.not_an_image",
                    format!(
                        "not an image dict ({} 0 R)",
                        imagedict.as_ref().map_or(0, |r| r.num)
                    ),
                );
                continue;
            }
            let get = |key: &[u8]| stored(doc, &imagedict, key);
            if !doc.resolve(&get(b"Subtype")).is_name(b"Image") {
                continue;
            }
            let filter = get(b"Filter");
            let mut alternate = Object::Null;
            let mut colorspace = get(b"ColorSpace");
            if let Object::Array(cses) = doc.resolve(&colorspace) {
                colorspace = cses.first().cloned().unwrap_or(Object::Null);
                let family = doc.resolve(&colorspace);
                if family.is_name(b"DeviceN") || family.is_name(b"Separation") {
                    alternate = cses.get(2).cloned().unwrap_or(Object::Null);
                    if let Object::Array(alt) = doc.resolve(&alternate) {
                        alternate = alt.first().cloned().unwrap_or(Object::Null);
                    }
                }
            }
            if self.images.found(&imagedict) {
                continue;
            }
            inv.images.push(ImageUse {
                page: self.page,
                page_object: self.page_object,
                width: get(b"Width"),
                height: get(b"Height"),
                bpc: get(b"BitsPerComponent"),
                filter,
                colorspace,
                alternate,
                dict: imagedict,
            });
        }
    }
}

/// `pdf_dict_get`: the stored value, unresolved.
fn stored(doc: &Document, dict: &Object, key: &[u8]) -> Object {
    doc.resolve(dict)
        .as_dict()
        .and_then(|d| d.get(key).cloned())
        .unwrap_or(Object::Null)
}

/// A font program `mutool extract` writes: its descriptor's object
/// number, the extension it names the file with, and the data.
#[derive(Debug, Clone, PartialEq)]
pub struct FontFile {
    pub ext: &'static str,
    pub data: Vec<u8>,
}

/// `savefont`: the program a font descriptor embeds, None when there is
/// none.
pub fn font_file(doc: &Document, descriptor: &Object) -> Result<Option<FontFile>, String> {
    let get =
        |key: &[u8]| Some(stored(doc, descriptor, key)).filter(|o| !matches!(o, Object::Null));
    let mut found = None;
    if let Some(f) = get(b"FontFile") {
        found = Some((f, "pfa"));
    }
    if let Some(f) = get(b"FontFile2") {
        found = Some((f, "ttf"));
    }
    if let Some(f) = get(b"FontFile3") {
        let subtype = stored(doc, &f, b"Subtype");
        let sub = doc.resolve(&subtype);
        if !matches!(subtype, Object::Null) && sub.as_name().is_none() {
            return Err("format error: invalid font descriptor subtype".into());
        }
        let ext = match sub.as_name() {
            Some(b"Type1C") => "cff",
            Some(b"CIDFontType0C") => "cid",
            Some(b"OpenType") => "otf",
            other => {
                return Err(format!(
                    "unsupported error: unhandled font type '{}'",
                    String::from_utf8_lossy(other.unwrap_or_default())
                ));
            }
        };
        found = Some((f, ext));
    }
    let Some((stream, ext)) = found else {
        return Ok(None);
    };
    let data = match doc.resolve(&stream) {
        Object::Stream(s) => doc.stream_data(&s).map_err(|e| e.report())?,
        _ => return Err("format error: object is not a stream".into()),
    };
    Ok(Some(FontFile { ext, data }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fonts_and_images_are_found_through_forms_once() {
        let pdf = b"%PDF-1.7\n\
            1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2>>endobj\n\
            3 0 obj<</Type/Page/Parent 2 0 R/Resources<</Font<</F1 5 0 R>>/XObject<</X 6 0 R>>>>>>endobj\n\
            4 0 obj<</Type/Page/Parent 2 0 R/Resources<</Font<</F1 5 0 R/F2 9 0 R>>>>>>endobj\n\
            5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding<</BaseEncoding/WinAnsiEncoding>>>>endobj\n\
            6 0 obj<</Type/XObject/Subtype/Form/Resources<</XObject<</I 7 0 R>>>>/Length 0>>stream\nendstream endobj\n\
            7 0 obj<</Type/XObject/Subtype/Image/Width 2/Height 3/ColorSpace[/Separation/Spot[/ICCBased 8 0 R]null]/BitsPerComponent 8/Filter/FlateDecode/Length 0>>stream\nendstream endobj\n\
            9 0 obj 12 endobj\n\
            trailer<</Root 1 0 R>>\n";
        let doc = Document::open(pdf.to_vec()).unwrap_or_else(|e| panic!("{e:?}"));
        let inv = inventory(&doc);
        assert_eq!(inv.fonts.len(), 1);
        let f = &inv.fonts[0];
        assert_eq!((f.page, f.page_object), (1, 3));
        assert!(f.name.is_name(b"Helvetica"));
        assert!(f.encoding.is_name(b"WinAnsiEncoding"));
        assert_eq!(inv.images.len(), 1);
        let i = &inv.images[0];
        assert!(i.colorspace.is_name(b"Separation"));
        assert!(i.alternate.is_name(b"ICCBased"));
        assert_eq!(doc.resolve(&i.width).to_int(), 2);
        assert!(doc.diagnostics().has("inventory.not_a_font"));
    }

    /// A document of `pages` pages sharing resources 3 (`resources`),
    /// with `extra` as objects from 4 + pages on.
    fn shared(pages: usize, resources: &str, extra: &[&str]) -> Document {
        let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", 4 + i)).collect();
        let mut pdf = format!(
            "%PDF-1.7\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
             2 0 obj<</Type/Pages/Kids[{}]/Count {pages}>>endobj\n3 0 obj{resources}endobj\n",
            kids.join(" ")
        );
        for i in 0..pages {
            pdf += &format!(
                "{} 0 obj<</Type/Page/Parent 2 0 R/Resources 3 0 R>>endobj\n",
                4 + i
            );
        }
        for (i, e) in extra.iter().enumerate() {
            pdf += &format!("{} 0 obj{e}endobj\n", 4 + pages + i);
        }
        pdf += "trailer<</Root 1 0 R>>\n";
        Document::open(pdf.into_bytes()).unwrap_or_else(|e| panic!("{e:?}"))
    }

    #[test]
    fn pages_sharing_resources_find_and_warn_as_each_walked_them() {
        // Found on the first page, through a form, once.
        let doc = shared(
            50,
            "<</Font<</F1 54 0 R>>/XObject<</X 55 0 R>>>>",
            &[
                "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
                "<</Type/XObject/Subtype/Form/Resources<</XObject<</I 56 0 R>>>>/Length 0>>stream\nendstream",
                "<</Type/XObject/Subtype/Image/Width 1/Height 1/Length 0>>stream\nendstream",
            ],
        );
        let inv = inventory(&doc);
        assert_eq!(inv.fonts.len(), 1);
        assert_eq!((inv.images.len(), inv.images[0].page), (1, 1));
        // A walk that warns warns again on every page.
        let doc = shared(3, "<</Font<</F1 7 0 R>>>>", &["12"]);
        inventory(&doc);
        let warned = doc.diagnostics().snapshot();
        assert_eq!(
            warned
                .iter()
                .filter(|d| d.code == "inventory.not_a_font")
                .count(),
            3
        );
        // Resources that lead to the second page's object: its walk comes
        // back to its page, and is walked again.
        let doc = shared(
            2,
            "<</Font<</F1 6 0 R/F2 5 0 R>>>>",
            &["<</Type/Font/Subtype/Type1/BaseFont/Courier>>"],
        );
        let inv = inventory(&doc);
        let pages: Vec<i32> = inv.fonts.iter().map(|f| f.page).collect();
        assert_eq!(pages, [1, 1]);
    }

    #[test]
    fn fonts_are_told_apart_as_pdf_objcmp_tells_them() {
        // Page 1: a direct Helvetica and a reference to an equal one; page
        // 2: the same reference, an equal direct one, and a direct Times.
        let pdf = b"%PDF-1.7\n\
            1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
            2 0 obj<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2>>endobj\n\
            3 0 obj<</Type/Page/Parent 2 0 R/Resources<</Font<</F1<</Type/Font/BaseFont/Helvetica>>/F2 5 0 R>>>>>>endobj\n\
            4 0 obj<</Type/Page/Parent 2 0 R/Resources<</Font<</F2 5 0 R/F3<</Type/Font/BaseFont/Helvetica>>/F4<</Type/Font/BaseFont/Times-Roman>>>>>>>>endobj\n\
            5 0 obj<</Type/Font/BaseFont/Helvetica>>endobj\n\
            trailer<</Root 1 0 R>>\n";
        let doc = Document::open(pdf.to_vec()).unwrap_or_else(|e| panic!("{e:?}"));
        let found: Vec<(i32, bool)> = inventory(&doc)
            .fonts
            .iter()
            .map(|f| (f.page, f.dict.as_ref().is_some()))
            .collect();
        assert_eq!(found, [(1, false), (1, true), (2, false)]);
    }
}
