// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use nupdf::font::Face;
use nupdf::fontdesc::Loader;
use nupdf::{Dict, Document, ObjRef, Object};

// The first byte picks how the rest is read: as a font file opened on its
// own, or embedded in a simple or composite PDF font of each kind and
// loaded as MuPDF loads it. Either way each glyph's name, width, code
// mapping and outline box is asked for.
fuzz_target!(|data: &[u8]| {
    let pick = data.first().copied().unwrap_or(0);
    let body = data.get(1..).unwrap_or_default();
    if pick & 1 == 0 {
        let Ok(mut face) = Face::open(Arc::from(body)) else {
            return;
        };
        for gid in 0..face.num_glyphs.min(512) {
            let _ = face.advance(gid);
            let _ = face.glyph_cbox(gid);
            if let Some(name) = face.glyph_name(gid) {
                let _ = face.name_index(&name);
            }
        }
        for code in 0..512 {
            let _ = face.char_index(code);
        }
        face.select_unicode();
        let _ = face.char_index(0x41);
        return;
    }
    let Ok(doc) = Document::create() else {
        return;
    };
    let kinds: [(&[u8], &[u8]); 5] = [
        (b"Type1", b"FontFile"),
        (b"TrueType", b"FontFile2"),
        (b"Type1", b"FontFile3"),
        (b"CIDFontType0", b"FontFile3"),
        (b"CIDFontType2", b"FontFile2"),
    ];
    let (subtype, file) = kinds[usize::from(pick >> 1) % kinds.len()];
    let stream = doc.add_stream(Dict::new(), body.to_vec());
    let mut desc = Dict::new();
    desc.put(b"Flags", Object::Int(i64::from(pick >> 4)));
    desc.put(file, Object::Ref(ObjRef::new(stream, 0)));
    let desc = Object::Dict(Arc::new(desc));
    let mut font = Dict::new();
    font.put(b"BaseFont", Object::name(b"ABCDEF+Fuzz"));
    font.put(b"FontDescriptor", desc);
    let font = if subtype.starts_with(b"CID") {
        font.put(b"Subtype", Object::name(subtype));
        let mut info = Dict::new();
        info.put(b"Registry", Object::String(Arc::from(&b"Adobe"[..])));
        info.put(b"Ordering", Object::String(Arc::from(&b"Identity"[..])));
        font.put(b"CIDSystemInfo", Object::Dict(Arc::new(info)));
        let mut top = Dict::new();
        top.put(b"Subtype", Object::name(b"Type0"));
        top.put(b"Encoding", Object::name(b"Identity-H"));
        top.put(
            b"DescendantFonts",
            Object::Array(Arc::new(vec![Object::Dict(Arc::new(font))])),
        );
        top
    } else {
        font.put(b"Subtype", Object::name(subtype));
        font.put(b"Encoding", Object::name(b"WinAnsiEncoding"));
        font
    };
    let font = Object::Dict(Arc::new(font));
    let Ok(fd) = (Loader { doc: &doc }).load(&font, None) else {
        return;
    };
    for cid in 0..512 {
        let gid = fd.cid_to_gid(cid);
        let _ = nupdf::fontdesc::bound_glyph(&fd.font, gid, nupdf::geometry::Matrix::IDENTITY);
        let _ = fd.lookup_hmtx(cid);
        let _ = fd.lookup_vmtx(cid);
    }
});
