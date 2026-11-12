// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! An invisible text layer from OCR: words Tesseract found on a page's
//! image (its TSV output), put on the page in render mode 3 (neither filled
//! nor stroked) so the page looks the same and its text can be selected,
//! searched and extracted (DECISIONS D50).
//!
//! The text is set in a font with no glyphs, as Tesseract's own PDF output
//! does: a Type 0 font whose codes are the characters' Unicode values, all
//! shown with the one empty glyph of Tesseract's glyphless font (embedded,
//! half an em wide), with a ToUnicode map from each code to its character.
//! Each word is scaled to the box Tesseract gave it.

use std::sync::Arc;

use crate::document::Document;
use crate::geometry::{Matrix, Rect};
use crate::object::{Dict, ObjRef, Object};

/// One word Tesseract read, its box in the image's pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub bbox: Rect,
    /// Tesseract's confidence, 0 to 100.
    pub confidence: f32,
}

/// The words in Tesseract's TSV output (`tesseract image - tsv`): the
/// rows of level 5, with text.
pub fn parse_tsv(tsv: &str) -> Vec<Word> {
    let mut lines = tsv.lines();
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let cols: Vec<&str> = header.split('\t').collect();
    let at = |name: &str| cols.iter().position(|c| *c == name);
    let (Some(level), Some(left), Some(top), Some(width), Some(height), Some(conf), Some(text)) = (
        at("level"),
        at("left"),
        at("top"),
        at("width"),
        at("height"),
        at("conf"),
        at("text"),
    ) else {
        return Vec::new();
    };
    lines
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let num = |i: usize| f.get(i).and_then(|v| v.trim().parse::<f32>().ok());
            if num(level)? != 5.0 {
                return None;
            }
            let text = f.get(text)?.trim();
            if text.is_empty() {
                return None;
            }
            let (x, y, w, h) = (num(left)?, num(top)?, num(width)?, num(height)?);
            if w <= 0.0 || h <= 0.0 {
                return None;
            }
            Some(Word {
                text: text.to_string(),
                bbox: Rect::new(x, y, x + w, y + h),
                confidence: num(conf).unwrap_or(-1.0),
            })
        })
        .collect()
}

fn reference(num: i32) -> Object {
    Object::Ref(ObjRef::new(num, 0))
}

fn name(n: &[u8]) -> Object {
    Object::name(n)
}

/// The ToUnicode CMap: every two-byte code is its own UTF-16 unit.
fn to_unicode() -> Vec<u8> {
    let mut s = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    // A range may not cross a change in the first byte, so 256 of them,
    // in blocks of at most 100.
    for block in (0..256).collect::<Vec<u32>>().chunks(100) {
        s += &format!("{} beginbfrange\n", block.len());
        for hi in block {
            s += &format!("<{hi:02X}00> <{hi:02X}FF> <{hi:02X}00>\n");
        }
        s += "endbfrange\n";
    }
    s += "endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n";
    s.into_bytes()
}

/// The glyphless font, as new objects: its number.
pub fn add_font(doc: &Document) -> i32 {
    let cmap = to_unicode();
    let mut d = Dict::new();
    d.put(b"Length", Object::Int(cmap.len() as i64));
    let cmap = doc.add_stream(d, cmap);
    let mut fd = Dict::new();
    fd.put(b"Type", name(b"FontDescriptor"));
    fd.put(b"FontName", name(b"GlyphLessFont"));
    fd.put(b"Flags", Object::Int(5));
    fd.put(
        b"FontBBox",
        Object::Array(Arc::new(
            [0, 0, 500, 1000].iter().map(|&v| Object::Int(v)).collect(),
        )),
    );
    for (k, v) in [
        (&b"ItalicAngle"[..], 0),
        (b"Ascent", 1000),
        (b"Descent", 0),
        (b"CapHeight", 1000),
        (b"StemV", 80),
    ] {
        fd.put(k, Object::Int(v));
    }
    // Tesseract's glyphless TrueType font: one empty glyph, half an em wide.
    let ttf: &[u8] = include_bytes!("../fonts/glyphless/pdf.ttf");
    let mut ff = Dict::new();
    ff.put(b"Length1", Object::Int(ttf.len() as i64));
    ff.put(b"Length", Object::Int(ttf.len() as i64));
    let ff = doc.add_stream(ff, ttf.to_vec());
    fd.put(b"FontFile2", reference(ff));
    let fd = doc.add_object(Object::Dict(Arc::new(fd)));
    // Every code shows glyph 1, the font's one glyph.
    let map: Vec<u8> = std::iter::repeat_n([0u8, 1u8], 65536).flatten().collect();
    let gidmap = match crate::deflate::compress(&map, 9) {
        Some(z) => {
            let mut d = Dict::new();
            d.put(b"Filter", name(b"FlateDecode"));
            d.put(b"Length", Object::Int(z.len() as i64));
            doc.add_stream(d, z)
        }
        None => {
            let mut d = Dict::new();
            d.put(b"Length", Object::Int(map.len() as i64));
            doc.add_stream(d, map)
        }
    };
    let mut info = Dict::new();
    info.put(b"Registry", Object::String(Arc::from(&b"Adobe"[..])));
    info.put(b"Ordering", Object::String(Arc::from(&b"Identity"[..])));
    info.put(b"Supplement", Object::Int(0));
    let mut cid = Dict::new();
    cid.put(b"Type", name(b"Font"));
    cid.put(b"Subtype", name(b"CIDFontType2"));
    cid.put(b"BaseFont", name(b"GlyphLessFont"));
    cid.put(b"CIDSystemInfo", Object::Dict(Arc::new(info)));
    cid.put(b"FontDescriptor", reference(fd));
    cid.put(b"DW", Object::Int(500));
    cid.put(b"CIDToGIDMap", reference(gidmap));
    let cid = doc.add_object(Object::Dict(Arc::new(cid)));
    let mut font = Dict::new();
    font.put(b"Type", name(b"Font"));
    font.put(b"Subtype", name(b"Type0"));
    font.put(b"BaseFont", name(b"GlyphLessFont"));
    font.put(b"Encoding", name(b"Identity-H"));
    font.put(
        b"DescendantFonts",
        Object::Array(Arc::new(vec![reference(cid)])),
    );
    font.put(b"ToUnicode", reference(cmap));
    doc.add_object(Object::Dict(Arc::new(font)))
}

fn g(out: &mut String, f: f32) {
    let mut b = Vec::new();
    crate::print::fmtfloat(f, &mut b);
    out.push_str(&String::from_utf8_lossy(&b));
}

/// The content stream that sets `words` invisibly over the page. The words'
/// boxes are in pixels of the page rendered at `dpi`; `page_ctm` is the
/// page's transform (`Document::page_transform`).
pub fn text_layer(words: &[Word], dpi: f32, page_ctm: Matrix, font: &[u8]) -> Vec<u8> {
    let to_user = page_ctm.invert();
    let scale = 72.0 / dpi;
    let mut out = String::from("q\nBT\n3 Tr\n");
    out += &format!("/{} 1 Tf\n", String::from_utf8_lossy(font));
    for w in words {
        // Page space: points from the top left, y down.
        let b = Rect::new(
            w.bbox.x0 * scale,
            w.bbox.y0 * scale,
            w.bbox.x1 * scale,
            w.bbox.y1 * scale,
        );
        let units: Vec<u16> = w
            .text
            .chars()
            .map(|c| u16::try_from(u32::from(c)).unwrap_or(0xfffd))
            .collect();
        if units.is_empty() {
            continue;
        }
        let sx = (b.x1 - b.x0) / (units.len() as f32 * 0.5);
        let sy = b.y1 - b.y0;
        // Text space to page space (y flipped, baseline at the box's
        // bottom), then to user space.
        let glyphs = Matrix {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: -sy,
            e: b.x0,
            f: b.y1,
        };
        let m = glyphs.concat(to_user);
        for v in [m.a, m.b, m.c, m.d, m.e, m.f] {
            g(&mut out, v);
            out.push(' ');
        }
        out += "Tm\n<";
        for u in units {
            out += &format!("{u:04X}");
        }
        out += "> Tj\n";
    }
    out += "ET\nQ\n";
    out.into_bytes()
}

/// Puts `layer` on page `index`: the font in the page's resources as
/// `name`, the layer after the page's contents, with enough `q` and `Q`
/// around the old contents that it starts from the page's initial state.
pub fn add_layer(
    doc: &Document,
    index: i32,
    font: i32,
    font_name: &[u8],
    layer: Vec<u8>,
) -> Result<(), String> {
    let page = doc.page(index).map_err(|e| e.0)?;
    let res = doc.inherited(&page, b"Resources").map_err(|e| e.0)?;
    let mut res = res
        .map(|r| doc.resolve(&r))
        .and_then(|r| r.as_dict().cloned())
        .unwrap_or_default();
    let mut fonts = doc
        .resolve(res.get(b"Font").unwrap_or(&Object::Null))
        .as_dict()
        .cloned()
        .unwrap_or_default();
    fonts.put(font_name, reference(font));
    res.put(b"Font", Object::Dict(Arc::new(fonts)));
    let res_obj = Object::Dict(Arc::new(res));
    let contents = doc.dict_get(&page, b"Contents");
    let stored = doc
        .resolve(&page)
        .as_dict()
        .and_then(|d| d.get(b"Contents").cloned())
        .unwrap_or(Object::Null);
    let (prepend, append) = crate::bake::q_balance(doc, &res_obj, &stored);
    let stream = |data: Vec<u8>| {
        let mut d = Dict::new();
        d.put(b"Length", Object::Int(data.len() as i64));
        reference(doc.add_stream(d, data))
    };
    let mut items = Vec::new();
    if prepend > 0 {
        items.push(stream(b"q\n".repeat(prepend as usize)));
    }
    match doc.resolve(&contents) {
        Object::Array(a) => items.extend(a.iter().cloned()),
        Object::Null => {}
        _ => items.push(stored),
    }
    let mut tail = b"Q\n".repeat(append.max(0) as usize);
    tail.extend(layer);
    items.push(stream(tail));
    crate::edit::edit_page(doc, index, &mut |d| {
        d.put(b"Resources", res_obj.clone());
        d.put(b"Contents", Object::Array(Arc::new(items.clone())));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_come_out_of_tesseract_tsv() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   1\t1\t0\t0\t0\t0\t0\t0\t612\t792\t-1\t\n\
                   5\t1\t1\t1\t1\t1\t100\t200\t50\t20\t96.5\tHello\n\
                   5\t1\t1\t1\t1\t2\t160\t200\t60\t20\t91\twörld\n\
                   5\t1\t1\t1\t1\t3\t230\t200\t0\t20\t10\t \n";
        let words = parse_tsv(tsv);
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].text, "Hello");
        assert_eq!(words[0].bbox, Rect::new(100.0, 200.0, 150.0, 220.0));
        assert_eq!(words[1].confidence, 91.0);
        assert!(parse_tsv("").is_empty());
        assert!(parse_tsv("a\tb\n1\t2\n").is_empty());
    }

    #[test]
    fn each_word_fills_its_box() {
        let w = Word {
            text: "ab".into(),
            bbox: Rect::new(0.0, 0.0, 100.0, 50.0),
            confidence: 90.0,
        };
        // At 144 dpi a pixel is half a point; on an unrotated 792-point
        // page, page space flips y.
        let ctm = Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: -1.0,
            e: 0.0,
            f: 792.0,
        };
        let layer = String::from_utf8(text_layer(&[w], 144.0, ctm, b"FOCR")).unwrap();
        assert!(
            layer.contains("3 Tr\n/FOCR 1 Tf\n50 0 0 25 0 767 Tm\n<00610062> Tj\n"),
            "{layer}"
        );
    }
}
