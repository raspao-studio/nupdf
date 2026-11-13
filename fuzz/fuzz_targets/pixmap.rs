// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
#![no_main]
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use nupdf::{Dict, Document, ObjRef, Object};

// The first two bytes pick an image's shape: size, depth, colorspace,
// a decode array, a color key, a /Matte soft mask, an image mask, and
// whether the rest is JPEG data. The rest is the samples. The image is
// loaded, decoded to a pixmap as MuPDF decodes it, and written as PAM and
// PNG.
fuzz_target!(|data: &[u8]| {
    let [a, b, body @ ..] = data else {
        return;
    };
    let Ok(doc) = Document::create() else {
        return;
    };
    let ints = |v: &[i64]| Object::Array(Arc::new(v.iter().map(|&i| Object::Int(i)).collect()));
    let mut dict = Dict::new();
    dict.put(b"Type", Object::name(b"XObject"));
    dict.put(b"Subtype", Object::name(b"Image"));
    dict.put(b"Width", Object::Int(i64::from(a & 15) + 1));
    dict.put(b"Height", Object::Int(i64::from(a >> 4) + 1));
    let bpc = [1, 2, 4, 8, 16, 8, 8, 8][usize::from(b & 7)];
    dict.put(b"BitsPerComponent", Object::Int(bpc));
    let n = match (b >> 3) & 3 {
        0 => {
            dict.put(b"ColorSpace", Object::name(b"DeviceGray"));
            1
        }
        1 => {
            dict.put(b"ColorSpace", Object::name(b"DeviceRGB"));
            3
        }
        2 => {
            dict.put(b"ColorSpace", Object::name(b"DeviceCMYK"));
            4
        }
        _ => {
            let lookup = Object::String(Arc::from(&b"\x00\x10\x20\x30\x40\x50\x60\x70\x80"[..]));
            dict.put(
                b"ColorSpace",
                Object::Array(Arc::new(vec![
                    Object::name(b"Indexed"),
                    Object::name(b"DeviceRGB"),
                    Object::Int(2),
                    lookup,
                ])),
            );
            1
        }
    };
    if b & 0x20 != 0 {
        dict.put(b"Decode", ints(&[1, 0].repeat(n)));
    }
    if b & 0x40 != 0 {
        dict.put(b"Mask", ints(&[0, 3].repeat(n)));
    } else if b & 0x80 != 0 {
        let mut m = Dict::new();
        m.put(b"Type", Object::name(b"XObject"));
        m.put(b"Subtype", Object::name(b"Image"));
        m.put(b"Width", Object::Int(i64::from(a & 15) + 1));
        m.put(b"Height", Object::Int(i64::from(a >> 4) + 1));
        m.put(b"BitsPerComponent", Object::Int(8));
        m.put(b"ColorSpace", Object::name(b"DeviceGray"));
        m.put(b"Matte", ints(&vec![0; n]));
        let mask = doc.add_stream(m, body.iter().rev().copied().collect());
        dict.put(b"SMask", Object::Ref(ObjRef::new(mask, 0)));
    }
    if b & 7 == 5 {
        dict.put(b"ImageMask", Object::Bool(true));
    }
    if b & 7 == 6 {
        dict.put(b"Filter", Object::name(b"DCTDecode"));
    }
    let num = doc.add_stream(dict, body.to_vec());
    let Ok(image) = nupdf::image::load(&doc, &Object::Ref(ObjRef::new(num, 0))) else {
        return;
    };
    if let Ok(pix) = nupdf::pixmap::from_image(&doc, &image) {
        let _ = pix.pam();
        let _ = pix.png();
    }
    // Decoded smaller, as for drawing: JPEG data through libjpeg's
    // reduced IDCTs, the rest averaged in blocks.
    let _ = nupdf::pixmap::from_image_subsampled(&doc, &image, u32::from(a ^ b) % 7);
    let _ = nupdf::image::jpeg_data(&doc, &image);
});
