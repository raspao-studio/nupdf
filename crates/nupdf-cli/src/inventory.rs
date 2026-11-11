// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/tools/pdfinfo.c and source/tools/pdfextract.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! The `fonts` and `images` commands: what the pages use, as `mutool info`
//! lists it, and the files `mutool extract` writes.

use std::path::Path;

use nupdf::fontdesc::Loader;
use nupdf::inventory::{self, Inventory};
use nupdf::{Document, Object};
use serde_json::{Map, Value, json};

use crate::trace::script_string;

fn name(doc: &Document, v: &Object) -> Value {
    match doc.resolve(v).as_name() {
        Some(n) => Value::String(script_string(n)),
        None => Value::Null,
    }
}

fn number(v: &Object) -> i32 {
    v.as_ref().map_or(0, |r| r.num)
}

fn page_errors(inv: &Inventory) -> Value {
    Value::Array(
        inv.page_errors
            .iter()
            .map(|(page, e)| json!({ "page": page, "error": nupdf::diag::report::strip(e) }))
            .collect(),
    )
}

fn write(dir: &Path, file: &str, data: &[u8]) -> Value {
    match std::fs::write(dir.join(file), data) {
        Ok(()) => json!({ "name": file, "file": file }),
        Err(e) => json!({ "name": file, "error": format!("cannot write {file}: {e}") }),
    }
}

/// The fonts the pages use, with what nupdf loads for each (`load`),
/// and with `extract`, every embedded font program written as
/// `font-NNNN.ext` (the descriptor's object number) into that directory.
pub fn fonts(doc: &Document, extract: Option<&Path>) -> Value {
    let inv = inventory::inventory(doc);
    // The count before fonts load: repair can add objects.
    let count = doc.xref_len();
    let loader = Loader { doc };
    let fonts: Vec<Value> = inv
        .fonts
        .iter()
        .map(|f| {
            let load = match loader.load(&f.dict, None) {
                Ok(fd) => {
                    let font = &fd.font;
                    let program = match (&font.face, &font.t3) {
                        (Some(face), _) => Value::from(face.format_name()),
                        (None, Some(_)) => Value::from("Type 3"),
                        _ => Value::Null,
                    };
                    json!({
                        "name": script_string(&font.name),
                        "program": program,
                        "glyphs": font.face.as_ref().map(|f| f.num_glyphs),
                        "embedded": fd.is_embedded,
                        "substitute": font.ft_substitute,
                        "to_unicode": fd.to_unicode.is_some(),
                        "vertical": fd.wmode != 0,
                    })
                }
                Err(e) => json!({ "error": nupdf::diag::report::strip(&e) }),
            };
            json!({
                "page": f.page,
                "page_object": f.page_object,
                "object": number(&f.dict),
                "subtype": name(doc, &f.subtype),
                "name": name(doc, &f.name),
                "encoding": name(doc, &f.encoding),
                "load": load,
            })
        })
        .collect();
    let mut out = Map::new();
    out.insert("page_count".into(), json!(inv.page_count));
    out.insert("fonts".into(), Value::Array(fonts));
    out.insert("page_errors".into(), page_errors(&inv));
    if let Some(dir) = extract {
        let mut files = Map::new();
        for num in 1..count as i32 {
            let obj = Object::Ref(nupdf::ObjRef::new(num, 0));
            if !doc.dict_get(&obj, b"Type").is_name(b"FontDescriptor") {
                continue;
            }
            let entry = match inventory::font_file(doc, &obj) {
                Ok(Some(ff)) => write(dir, &format!("font-{num:04}.{}", ff.ext), &ff.data),
                Ok(None) => continue,
                Err(e) => json!({ "error": nupdf::diag::report::strip(&e) }),
            };
            files.insert(num.to_string(), entry);
        }
        out.insert("object_count".into(), json!(count));
        out.insert("extracted".into(), Value::Object(files));
    }
    Value::Object(out)
}

/// How `images --extract` writes each image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ImageFormat {
    /// As `mutool extract`: JPEG data as it is, gray and RGB as PNG, the
    /// rest as PAM.
    Auto,
    /// Every image decoded, as PAM.
    Pam,
}

/// The images the pages use; with `digests`, every image XObject's pixmap
/// as MuPDF decodes it (size, components, colorspace, MD5 of the
/// samples); with `extract`, every image XObject written as
/// `image-NNNN.ext` into that directory.
pub fn images(doc: &Document, digests: bool, extract: Option<&Path>, format: ImageFormat) -> Value {
    let inv = inventory::inventory(doc);
    let images: Vec<Value> = inv
        .images
        .iter()
        .map(|i| {
            let filters: Vec<Value> = match doc.resolve(&i.filter) {
                Object::Null => Vec::new(),
                Object::Array(fs) => fs.iter().map(|f| name(doc, f)).collect(),
                f => vec![name(doc, &f)],
            };
            let int = |v: &Object| doc.resolve(v).to_int();
            json!({
                "page": i.page,
                "page_object": i.page_object,
                "object": number(&i.dict),
                "width": int(&i.width),
                "height": int(&i.height),
                "bpc": (!matches!(i.bpc, Object::Null)).then(|| int(&i.bpc)),
                "filters": filters,
                "colorspace": name(doc, &i.colorspace),
                "alternate": name(doc, &i.alternate),
            })
        })
        .collect();
    let mut out = Map::new();
    out.insert("page_count".into(), json!(inv.page_count));
    out.insert("images".into(), Value::Array(images));
    out.insert("page_errors".into(), page_errors(&inv));
    if digests || extract.is_some() {
        let count = doc.xref_len();
        let mut pixmaps = Map::new();
        let mut files = Map::new();
        for num in 1..count as i32 {
            let obj = Object::Ref(nupdf::ObjRef::new(num, 0));
            if !doc.dict_get(&obj, b"Subtype").is_name(b"Image") {
                continue;
            }
            let image = nupdf::image::load(doc, &obj);
            if digests {
                let entry = match image
                    .as_ref()
                    .map_err(String::clone)
                    .and_then(|img| nupdf::pixmap::from_image(doc, img))
                {
                    Ok(pix) => json!({
                        "w": pix.w,
                        "h": pix.h,
                        "n": pix.n,
                        "alpha": pix.alpha,
                        // The script glue has no object for MuPDF's device
                        // Lab space.
                        "colorspace": pix
                            .colorspace
                            .as_ref()
                            .filter(|c| !matches!(c, nupdf::colorspace::ColorSpace::Lab))
                            .map(|c| json!([c.type_name(), c.n()])),
                        "md5": crate::md5::hex(&pix.samples),
                    }),
                    Err(e) => json!({ "error": nupdf::diag::report::strip(&e) }),
                };
                pixmaps.insert(num.to_string(), entry);
            }
            if let Some(dir) = extract {
                files.insert(num.to_string(), extract_image(doc, dir, num, image, format));
            }
        }
        out.insert("object_count".into(), json!(count));
        if digests {
            out.insert("pixmaps".into(), Value::Object(pixmaps));
        }
        if extract.is_some() {
            out.insert("extracted".into(), Value::Object(files));
        }
    }
    Value::Object(out)
}

/// `saveimage`.
fn extract_image(
    doc: &Document,
    dir: &Path,
    num: i32,
    image: Result<nupdf::image::Image, String>,
    format: ImageFormat,
) -> Value {
    let fail = |e: String| json!({ "error": nupdf::diag::report::strip(&e) });
    let image = match image {
        Ok(i) => i,
        Err(e) => return fail(e),
    };
    let stem = format!("image-{num:04}");
    if format == ImageFormat::Auto && image.colorkey.is_none() && image.decode.is_none() {
        match nupdf::image::jpeg_data(doc, &image) {
            Ok(Some(data)) => return write(dir, &format!("{stem}.jpg"), &data),
            Ok(None) => {}
            Err(e) => return fail(e),
        }
    }
    let pix = match nupdf::pixmap::from_image(doc, &image) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let png = format == ImageFormat::Auto
        && pix.colorspace.as_ref().is_none_or(|c| {
            matches!(
                c.family(),
                Some(nupdf::colorspace::Family::Gray | nupdf::colorspace::Family::Rgb)
            )
        });
    let (ext, data) = if png {
        ("png", pix.png())
    } else {
        ("pam", pix.pam())
    };
    match data {
        Ok(data) => write(dir, &format!("{stem}.{ext}"), &data),
        Err(e) => fail(e),
    }
}
