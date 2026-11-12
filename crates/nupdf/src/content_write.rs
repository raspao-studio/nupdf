// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-op-buffer.c (pdf_new_output_processor)
// and source/pdf/pdf-stream.c (build_compression_params),
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! Content stream operators written back out, as MuPDF's output processor
//! writes them: numbers as `%g` (shortest digits), names escaped, strings
//! literal or hex, a space between operators (or a newline each, with
//! `newlines`), and the `q` a stream leaves open closed at the end. The
//! entries an ExtGState sets are not written again after its `gs`.

use crate::colorspace::ColorSpace;
use crate::content::{InlineImage, Op};
use crate::diag::Diagnostics;
use crate::document::Document;
use crate::object::{Dict, Object};
use crate::print::{fmtfloat, print_sep};

/// What an inline image's data is still compressed with
/// (`fz_compression_params`).
#[derive(Debug, Clone, PartialEq)]
pub enum Compression {
    Raw,
    Jpeg {
        color_transform: i32,
        invert_cmyk: bool,
    },
    Fax {
        k: i32,
        end_of_line: bool,
        encoded_byte_align: bool,
        columns: i32,
        rows: i32,
        end_of_block: bool,
        black_is_1: bool,
    },
    Flate {
        predictor: i32,
        columns: i32,
        colors: i32,
        bpc: i32,
    },
    Lzw {
        predictor: i32,
        columns: i32,
        colors: i32,
        bpc: i32,
        early_change: i32,
    },
    Rld,
    Brotli {
        predictor: i32,
        columns: i32,
        colors: i32,
        bpc: i32,
    },
    Jbig2,
    Jpx,
}

/// The image `BI` writes: what `fz_compressed_image_buffer` gives of an
/// inline image, or a pixmap's samples (8 bits, raw).
#[derive(Debug, Clone)]
pub struct BiImage {
    pub width: i32,
    pub height: i32,
    pub bpc: i32,
    pub image_mask: bool,
    pub colorspace: Option<ColorSpace>,
    /// Whether the colorspace is MuPDF's device gray, RGB or CMYK itself,
    /// which `BI` writes by its abbreviation; a calibrated or ICC space of
    /// the same family is written by its resource name.
    pub device: bool,
    pub interpolate: bool,
    /// Two values per component.
    pub decode: Vec<f32>,
    pub compression: Compression,
    pub data: Vec<u8>,
}

/// `build_compression_params` for filter `f` with parameters `p`.
fn compression(doc: &Document, f: &Object, p: &Object) -> Compression {
    let name = match doc.resolve(f) {
        Object::Name(n) => n,
        _ => return Compression::Raw,
    };
    let int = |key: &[u8], default: i32| match doc.dict_get(p, key) {
        Object::Int(i) => i as i32,
        Object::Real(r) => r as i32,
        _ => default,
    };
    let boolean = |key: &[u8], default: bool| match doc.dict_get(p, key) {
        Object::Bool(b) => b,
        _ => default,
    };
    match name.as_ref() {
        b"CCITTFaxDecode" | b"CCF" => Compression::Fax {
            k: int(b"K", 0),
            end_of_line: boolean(b"EndOfLine", false),
            encoded_byte_align: boolean(b"EncodedByteAlign", false),
            columns: int(b"Columns", 1728),
            rows: int(b"Rows", 0),
            end_of_block: boolean(b"EndOfBlock", true),
            black_is_1: boolean(b"BlackIs1", false),
        },
        b"DCTDecode" | b"DCT" => Compression::Jpeg {
            color_transform: int(b"ColorTransform", -1),
            invert_cmyk: false,
        },
        b"RunLengthDecode" | b"RL" => Compression::Rld,
        b"FlateDecode" | b"Fl" => Compression::Flate {
            predictor: int(b"Predictor", 1),
            columns: int(b"Columns", 1),
            colors: int(b"Colors", 1),
            bpc: int(b"BitsPerComponent", 8),
        },
        b"BrotliDecode" | b"Br" => Compression::Brotli {
            predictor: int(b"Predictor", 1),
            columns: int(b"Columns", 1),
            colors: int(b"Colors", 1),
            bpc: int(b"BitsPerComponent", 8),
        },
        b"LZWDecode" | b"LZW" => Compression::Lzw {
            predictor: int(b"Predictor", 1),
            columns: int(b"Columns", 1),
            colors: int(b"Colors", 1),
            bpc: int(b"BitsPerComponent", 8),
            early_change: int(b"EarlyChange", 1),
        },
        b"JBIG2Decode" => Compression::Jbig2,
        b"JPXDecode" => Compression::Jpx,
        _ => Compression::Raw,
    }
}

/// An inline image as `pdf_load_compressed_inline_image` keeps it: the
/// last filter, when it is an image filter, is recorded and left
/// undecoded; the filters before it are applied. Without an image filter
/// the samples are decoded.
/// `lookup` finds a colorspace resource by name.
pub fn bi_image(
    doc: &Document,
    image: &InlineImage,
    lookup: &dyn Fn(&[u8]) -> Option<Object>,
) -> Result<BiImage, String> {
    let get = |key: &[u8], abbrev: &[u8]| {
        let v = match image.dict.get(abbrev) {
            Some(Object::Null) | None => image.dict.get(key),
            v => v,
        };
        v.filter(|v| !matches!(v, Object::Null))
            .map_or(Object::Null, |v| doc.resolve(v))
    };
    let filters = get(b"Filter", b"F");
    let params = get(b"DecodeParms", b"DP");
    let (list, plist): (Vec<Object>, Vec<Object>) = match &filters {
        Object::Name(_) => (vec![filters.clone()], vec![params.clone()]),
        Object::Array(a) if !a.is_empty() => {
            let ps = params.as_array().unwrap_or_default();
            (
                a.to_vec(),
                (0..a.len())
                    .map(|i| ps.get(i).cloned().unwrap_or(Object::Null))
                    .collect(),
            )
        }
        _ => (Vec::new(), Vec::new()),
    };
    let last = match (list.last(), plist.last()) {
        (Some(f), Some(p)) => compression(doc, f, p),
        _ => Compression::Raw,
    };
    let full = image::image_full_decode(doc, image);
    let data = if list.is_empty() {
        image.data.to_vec()
    } else {
        let keep = if last == Compression::Raw {
            list.len()
        } else {
            list.len() - 1
        };
        if keep == 0 {
            image.data.to_vec()
        } else {
            let mut dict = Dict::new();
            dict.put(
                b"Filter",
                Object::Array(std::sync::Arc::new(list[..keep].to_vec())),
            );
            dict.put(
                b"DecodeParms",
                Object::Array(std::sync::Arc::new(plist[..keep].to_vec())),
            );
            let resolve = |o: &Object| doc.resolve(o);
            let mut out = crate::filter::decode(
                &dict,
                image.data,
                &resolve,
                doc.diagnostics(),
                &doc.limits(),
            )
            .map_err(|e| e.report())?;
            if last == Compression::Raw {
                let size = image::inline_size(image);
                out.truncate(size);
            }
            out
        }
    };
    let interpolate = matches!(get(b"Interpolate", b"I"), Object::Bool(true));
    let mut cs = get(b"ColorSpace", b"CS");
    if let Object::Name(n) = &cs
        && let Some(res) = lookup(n)
    {
        cs = doc.resolve(&res);
    }
    let device = is_device_colorspace(doc, &cs);
    Ok(BiImage {
        width: image.width,
        height: image.height,
        bpc: image.bpc,
        image_mask: image.image_mask,
        colorspace: image.colorspace.clone(),
        device,
        interpolate,
        decode: full,
        compression: last,
        data,
    })
}

/// Whether `pdf_load_colorspace` gives one of the device spaces itself
/// for `obj`: a device name, or an array that starts with one.
pub(crate) fn is_device_colorspace(doc: &Document, obj: &Object) -> bool {
    let name = match obj {
        Object::Name(n) => n.clone(),
        Object::Array(a) => match a.first().map(|v| doc.resolve(v)) {
            Some(Object::Name(n)) => n,
            _ => return false,
        },
        _ => return false,
    };
    let top = matches!(obj, Object::Name(_));
    match name.as_ref() {
        b"G" | b"RGB" | b"CMYK" | b"DeviceGray" | b"DeviceRGB" | b"DeviceCMYK" => true,
        b"CalCMYK" => !top,
        _ => false,
    }
}

mod image {
    use crate::content::InlineImage;

    /// `img->decode`: the image's decode array, all of it.
    pub fn image_full_decode(doc: &crate::document::Document, image: &InlineImage) -> Vec<f32> {
        let n = image.n.min(crate::colorspace::MAX_COLORS);
        let indexed = image
            .colorspace
            .as_ref()
            .is_some_and(crate::colorspace::ColorSpace::is_indexed);
        let maxval = if indexed {
            ((1i64 << image.bpc.clamp(0, 31)) - 1) as f32
        } else {
            1.0
        };
        let given = crate::image::from_inline(doc, image).decode;
        match given {
            Some(d) => d,
            None => (0..n * 2)
                .map(|i| if i & 1 == 1 { maxval } else { 0.0 })
                .collect(),
        }
    }

    /// The bytes of the image's samples.
    pub fn inline_size(image: &InlineImage) -> usize {
        let stride = (image.width as i64 * image.n as i64 * image.bpc as i64 + 7) >> 3;
        usize::try_from(stride * image.height as i64).unwrap_or(0)
    }
}

fn report_argument(message: &str) -> String {
    format!("argument error: {message}")
}

/// `pdf_output_processor`.
#[derive(Debug)]
pub struct Writer<'d> {
    pub out: Vec<u8>,
    diag: &'d Diagnostics,
    ahx: bool,
    newlines: bool,
    extgstate: bool,
    balance: i32,
    sep: bool,
}

fn put_float(out: &mut Vec<u8>, f: f32) {
    fmtfloat(f, out);
}

/// `fmtname`, as `%n` writes a name.
fn put_name(out: &mut Vec<u8>, name: &[u8]) {
    out.push(b'/');
    for &c in name.iter().take_while(|&&c| c != 0) {
        // C's char is signed: bytes from 128 count as below 32.
        if c <= 32 || c >= 128 || c == b'/' || c == b'#' {
            out.push(b'#');
            out.push(b"0123456789ABCDEF"[usize::from(c >> 4)]);
            out.push(b"0123456789ABCDEF"[usize::from(c & 15)]);
        } else {
            out.push(c);
        }
    }
}

/// `fz_write_pdf_string`: literal when every byte is printable ASCII, else
/// lower-case hex.
fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    if s.iter().any(|&c| !(32..127).contains(&c)) {
        out.push(b'<');
        for &c in s {
            out.push(b"0123456789abcdef"[usize::from(c >> 4)]);
            out.push(b"0123456789abcdef"[usize::from(c & 15)]);
        }
        out.push(b'>');
    } else {
        out.push(b'(');
        for &c in s {
            if matches!(c, b'(' | b')' | b'\\') {
                out.push(b'\\');
            }
            out.push(c);
        }
        out.push(b')');
    }
}

impl<'d> Writer<'d> {
    /// `pdf_new_buffer_processor`.
    pub fn new(diag: &'d Diagnostics, ahx: bool, newlines: bool) -> Writer<'d> {
        Writer {
            out: Vec::new(),
            diag,
            ahx,
            newlines,
            extgstate: false,
            balance: 0,
            sep: false,
        }
    }

    fn post_op(&mut self) {
        if self.newlines {
            self.out.push(b'\n');
            self.sep = false;
        } else {
            self.sep = true;
        }
    }

    fn separate(&mut self) {
        if self.sep {
            self.out.push(b' ');
        }
    }

    /// An operator with no operands.
    fn word(&mut self, op: &[u8]) {
        self.separate();
        self.out.extend_from_slice(op);
        self.post_op();
    }

    /// Numbers, then the operator.
    fn floats(&mut self, v: &[f32], op: &[u8]) {
        self.separate();
        for &f in v {
            put_float(&mut self.out, f);
            self.out.push(b' ');
        }
        self.out.extend_from_slice(op);
        self.post_op();
    }

    fn int(&mut self, v: i32, op: &[u8]) {
        self.separate();
        self.out.extend_from_slice(v.to_string().as_bytes());
        self.out.push(b' ');
        self.out.extend_from_slice(op);
        self.post_op();
    }

    fn named(&mut self, name: &[u8], op: &[u8]) {
        self.separate();
        put_name(&mut self.out, name);
        self.out.push(b' ');
        self.out.extend_from_slice(op);
        self.post_op();
    }

    fn object(&mut self, obj: Option<&Object>) {
        let obj = obj.unwrap_or(&Object::Null);
        self.sep = print_sep(obj, self.ahx, &mut self.out);
    }

    fn colors(&mut self, color: &[f32], name: Option<&[u8]>, op: &[u8]) {
        self.separate();
        for &c in color {
            put_float(&mut self.out, c);
            self.out.push(b' ');
        }
        if let Some(name) = name {
            put_name(&mut self.out, name);
            self.out.push(b' ');
        }
        self.out.extend_from_slice(op);
        self.post_op();
    }

    /// Writes one operator. `InlineImage` goes through `inline_image`.
    pub fn op(&mut self, op: &Op<'_>) {
        let gs_only = self.extgstate;
        match *op {
            Op::LineWidth(w) if !gs_only => self.floats(&[w], b"w"),
            Op::LineJoin(j) if !gs_only => self.int(j, b"j"),
            Op::LineCap(c) if !gs_only => self.int(c, b"J"),
            Op::MiterLimit(m) if !gs_only => self.floats(&[m], b"M"),
            Op::Dash { array, phase } if !gs_only => {
                self.object(array);
                self.floats(&[phase], b"d");
            }
            Op::Intent(i) if !gs_only => self.named(i, b"ri"),
            Op::Flatness(f) if !gs_only => self.floats(&[f], b"i"),
            Op::GsBegin { name, .. } => {
                self.extgstate = true;
                put_name(&mut self.out, name);
                self.out.extend_from_slice(b" gs");
                self.post_op();
            }
            Op::GsEnd => self.extgstate = false,
            Op::Save => {
                self.balance += 1;
                self.word(b"q");
            }
            Op::Restore => {
                self.balance -= 1;
                if self.balance < 0 {
                    self.diag.warn(
                        "content.gstate_underflow",
                        "gstate underflow (too many Q operators)",
                    );
                }
                self.word(b"Q");
            }
            Op::Transform(m) => self.floats(&m, b"cm"),
            Op::MoveTo(x, y) => self.floats(&[x, y], b"m"),
            Op::LineTo(x, y) => self.floats(&[x, y], b"l"),
            Op::CurveTo(c) => self.floats(&c, b"c"),
            Op::CurveToV(c) => self.floats(&c, b"v"),
            Op::CurveToY(c) => self.floats(&c, b"y"),
            Op::ClosePath => self.word(b"h"),
            Op::Rect(r) => self.floats(&r, b"re"),
            Op::Stroke => self.word(b"S"),
            Op::CloseStroke => self.word(b"s"),
            Op::FillCompat => self.word(b"F"),
            Op::Fill => self.word(b"f"),
            Op::FillEvenOdd => self.word(b"f*"),
            Op::FillStroke => self.word(b"B"),
            Op::FillStrokeEvenOdd => self.word(b"B*"),
            Op::CloseFillStroke => self.word(b"b"),
            Op::CloseFillStrokeEvenOdd => self.word(b"b*"),
            Op::EndPath => self.word(b"n"),
            Op::Clip => self.word(b"W"),
            Op::ClipEvenOdd => self.word(b"W*"),
            Op::BeginText => self.word(b"BT"),
            Op::EndText => self.word(b"ET"),
            Op::CharSpacing(v) => self.floats(&[v], b"Tc"),
            Op::WordSpacing(v) => self.floats(&[v], b"Tw"),
            Op::HorizontalScaling(v) => self.floats(&[v], b"Tz"),
            Op::Leading(v) => self.floats(&[v], b"TL"),
            Op::Font { name, size, .. } if !gs_only => {
                // MuPDF writes no separator before Tf.
                put_name(&mut self.out, name);
                self.out.push(b' ');
                put_float(&mut self.out, size);
                self.out.extend_from_slice(b" Tf");
                self.post_op();
            }
            Op::RenderMode(r) => self.int(r, b"Tr"),
            Op::Rise(v) => self.floats(&[v], b"Ts"),
            Op::TextMove(x, y) => self.floats(&[x, y], b"Td"),
            Op::TextMoveSetLeading(x, y) => self.floats(&[x, y], b"TD"),
            Op::TextMatrix(m) => self.floats(&m, b"Tm"),
            Op::NextLine => self.word(b"T*"),
            Op::ShowArray(array) => {
                self.object(array);
                self.word(b"TJ");
            }
            Op::Show { text, .. } => self.show(text, b"Tj"),
            Op::NextLineShow { text, .. } => self.show(text, b"'"),
            Op::NextLineShowSpaced {
                word_spacing,
                char_spacing,
                text,
                ..
            } => {
                self.separate();
                put_float(&mut self.out, word_spacing);
                self.out.push(b' ');
                put_float(&mut self.out, char_spacing);
                self.out.push(b' ');
                put_string(&mut self.out, text);
                self.out.push(b'"');
                self.post_op();
            }
            Op::Type3Width(x, y) => self.floats(&[x, y], b"d0"),
            Op::Type3Bbox(b) => self.floats(&b, b"d1"),
            Op::ColorSpace { stroke, name, .. } => {
                self.named(name, if stroke { b"CS" } else { b"cs" })
            }
            Op::ColorPattern {
                stroke,
                name,
                color,
                ..
            } => self.colors(color, Some(name), if stroke { b"SCN" } else { b"scn" }),
            Op::ColorShade { stroke, name, .. } => {
                self.named(name, if stroke { b"SCN" } else { b"scn" })
            }
            Op::Color { stroke, color } => {
                self.colors(color, None, if stroke { b"SCN" } else { b"scn" })
            }
            Op::Gray { stroke, gray } => self.floats(&[gray], if stroke { b"G" } else { b"g" }),
            Op::Rgb { stroke, rgb } => self.floats(&rgb, if stroke { b"RG" } else { b"rg" }),
            Op::Cmyk { stroke, cmyk } => self.floats(&cmyk, if stroke { b"K" } else { b"k" }),
            Op::Shade { name, .. } => self.named(name, b"sh"),
            Op::DrawImage { name, .. } | Op::DrawForm { name, .. } => self.named(name, b"Do"),
            Op::MarkedPoint(tag) => self.named(tag, b"MP"),
            Op::MarkedPointProperties { tag, raw, .. } => self.marked(tag, raw, b"DP"),
            Op::BeginMarked(tag) => self.named(tag, b"BMC"),
            Op::BeginMarkedProperties { tag, raw, .. } => self.marked(tag, raw, b"BDC"),
            Op::EndMarked => self.word(b"EMC"),
            Op::BeginCompatibility => self.word(b"BX"),
            Op::EndCompatibility => self.word(b"EX"),
            _ => {}
        }
    }

    fn show(&mut self, text: &[u8], op: &[u8]) {
        self.separate();
        put_string(&mut self.out, text);
        self.out.extend_from_slice(op);
        self.post_op();
    }

    fn marked(&mut self, tag: &[u8], raw: Option<&Object>, op: &[u8]) {
        self.separate();
        put_name(&mut self.out, tag);
        self.sep = true;
        self.object(raw);
        self.word(op);
    }

    /// `pdf_out_BI`. An image that is no mask and has no device or named
    /// colorspace is an error, written up to its size.
    pub fn inline_image(&mut self, img: &BiImage, csname: &[u8]) -> Result<(), String> {
        let ahx = self.ahx;
        self.separate();
        let out = &mut self.out;
        out.extend_from_slice(b"BI ");
        out.extend_from_slice(
            format!("/W {}/H {}/BPC {}", img.width, img.height, img.bpc).as_bytes(),
        );
        if img.image_mask {
            out.extend_from_slice(b"/IM true");
        } else {
            match &img.colorspace {
                Some(ColorSpace::Gray) if img.device => out.extend_from_slice(b"/CS/G"),
                Some(ColorSpace::Rgb) if img.device => out.extend_from_slice(b"/CS/RGB"),
                Some(ColorSpace::Cmyk) if img.device => out.extend_from_slice(b"/CS/CMYK"),
                Some(_) => {
                    out.extend_from_slice(b"/CS");
                    put_name(out, csname);
                }
                None => {
                    return Err(report_argument(
                        "BI operator can only show ImageMask, Gray, RGB, or CMYK images",
                    ));
                }
            }
        }
        if img.interpolate {
            out.extend_from_slice(b"/I true");
        }
        out.extend_from_slice(b"/D[");
        for (i, &d) in img.decode.iter().enumerate() {
            if i > 0 {
                out.push(b' ');
            }
            put_float(out, d);
        }
        out.push(b']');
        let mut sep = false;
        let n = img.colorspace.as_ref().map_or(1, ColorSpace::n);
        let pick = |plain: &'static [u8], hex: &'static [u8]| if ahx { hex } else { plain };
        match &img.compression {
            Compression::Jpeg {
                color_transform,
                invert_cmyk,
            } => {
                out.extend_from_slice(pick(b"/F/DCT", b"/F[/AHx/DCT]"));
                sep = !ahx;
                if *color_transform >= 0 {
                    out.extend_from_slice(
                        format!("/DP<</ColorTransform {color_transform}>>").as_bytes(),
                    );
                    sep = false;
                }
                if *invert_cmyk && n == 4 {
                    out.extend_from_slice(b"/D[1 0 1 0 1 0 1 0]");
                    sep = false;
                }
            }
            Compression::Fax {
                k,
                end_of_line,
                encoded_byte_align,
                columns,
                rows,
                end_of_block,
                black_is_1,
            } => {
                out.extend_from_slice(pick(b"/F/CCF/DP<<", b"/F[/AHx/CCF]/DP[null<<"));
                out.extend_from_slice(format!("/K {k}").as_bytes());
                if *columns != 1728 {
                    out.extend_from_slice(format!("/Columns {columns}").as_bytes());
                }
                if *rows > 0 {
                    out.extend_from_slice(format!("/Rows {rows}").as_bytes());
                }
                if *end_of_line {
                    out.extend_from_slice(b"/EndOfLine true");
                }
                if *encoded_byte_align {
                    out.extend_from_slice(b"/EncodedByteAlign true");
                }
                if !*end_of_block {
                    out.extend_from_slice(b"/EndOfBlock false");
                }
                if *black_is_1 {
                    out.extend_from_slice(b"/BlackIs1 true");
                }
                out.extend_from_slice(pick(b">>", b">>]"));
            }
            Compression::Raw => {
                if ahx {
                    out.extend_from_slice(b"/F/AHx");
                    sep = true;
                }
            }
            Compression::Rld => {
                out.extend_from_slice(pick(b"/F/RL", b"/F[/AHx/RL]"));
                sep = !ahx;
            }
            Compression::Flate {
                predictor,
                columns,
                colors,
                bpc,
            } => {
                out.extend_from_slice(pick(b"/F/Fl", b"/F[/AHx/Fl]"));
                sep = !ahx;
                if *predictor > 1 {
                    predictor_parms(out, ahx, *predictor, *columns, *colors, *bpc, None);
                    sep = false;
                }
            }
            Compression::Lzw {
                predictor,
                columns,
                colors,
                bpc,
                early_change,
            } => {
                out.extend_from_slice(pick(b"/F/LZW", b"/F[/AHx/LZW]"));
                sep = !ahx;
                if *predictor > 1 {
                    predictor_parms(
                        out,
                        ahx,
                        *predictor,
                        *columns,
                        *colors,
                        *bpc,
                        Some(*early_change),
                    );
                    sep = false;
                }
            }
            Compression::Brotli {
                predictor,
                columns,
                colors,
                bpc,
            } => {
                out.extend_from_slice(pick(b"/F/Br\n", b"/F[/AHx/Br]\n"));
                if *predictor > 1 {
                    out.extend_from_slice(pick(b"/DP<<\n", b"/DP[null<<\n"));
                    out.extend_from_slice(format!("/Predictor {predictor}\n").as_bytes());
                    if *columns != 1 {
                        out.extend_from_slice(format!("/Columns {columns}\n").as_bytes());
                    }
                    if *colors != 1 {
                        out.extend_from_slice(format!("/Colors {colors}\n").as_bytes());
                    }
                    if *bpc != 8 {
                        out.extend_from_slice(format!("/BitsPerComponent {bpc}\n").as_bytes());
                    }
                    out.extend_from_slice(pick(b">>\n", b">>]\n"));
                }
            }
            Compression::Jbig2 | Compression::Jpx => {
                return Err(report_argument("unknown compressed buffer type"));
            }
        }
        if sep {
            out.push(b' ');
        }
        out.extend_from_slice(b"ID ");
        if ahx {
            for (z, &c) in img.data.iter().enumerate() {
                out.push(b"0123456789abcdef"[usize::from(c >> 4)]);
                out.push(b"0123456789abcdef"[usize::from(c & 15)]);
                if z & 31 == 31 {
                    out.push(b'\n');
                }
            }
            out.push(b'>');
        } else {
            out.extend_from_slice(&img.data);
        }
        out.extend_from_slice(b" EI");
        self.sep = true;
        Ok(())
    }

    /// `pdf_close_output_processor`: the `Q` still owed.
    pub fn close(&mut self) {
        while self.balance > 0 {
            self.balance -= 1;
            self.separate();
            self.out.push(b'Q');
            self.post_op();
        }
    }

    /// `pdf_reset_output_processor`: the output only.
    pub fn reset(&mut self) {
        self.out.clear();
    }
}

fn predictor_parms(
    out: &mut Vec<u8>,
    ahx: bool,
    predictor: i32,
    columns: i32,
    colors: i32,
    bpc: i32,
    early_change: Option<i32>,
) {
    out.extend_from_slice(if ahx { b"/DP[null<<" } else { b"/DP<<" });
    out.extend_from_slice(format!("/Predictor {predictor}").as_bytes());
    if columns != 1 {
        out.extend_from_slice(format!("/Columns {columns}").as_bytes());
    }
    if colors != 1 {
        out.extend_from_slice(format!("/Colors {colors}").as_bytes());
    }
    if bpc != 8 {
        out.extend_from_slice(format!("/BitsPerComponent {bpc}").as_bytes());
    }
    if let Some(e) = early_change
        && e != 1
    {
        out.extend_from_slice(format!("/EarlyChange {e}").as_bytes());
    }
    out.extend_from_slice(if ahx { b">>]" } else { b">>" });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn written(ops: &[Op<'_>], newlines: bool) -> String {
        let diag = Diagnostics::default();
        let mut w = Writer::new(&diag, false, newlines);
        for op in ops {
            w.op(op);
        }
        w.close();
        String::from_utf8_lossy(&w.out).into_owned()
    }

    #[test]
    fn operators_are_separated_by_spaces_or_newlines() {
        let ops = [
            Op::Save,
            Op::Transform([1.0, 0.0, 0.0, 1.0, 72.5, 0.25]),
            Op::Rgb {
                stroke: false,
                rgb: [1.0, 0.5, 0.0],
            },
            Op::Rect([0.0, 0.0, 10.0, 10.0]),
            Op::Fill,
        ];
        assert_eq!(
            written(&ops, false),
            "q 1 0 0 1 72.5 .25 cm 1 .5 0 rg 0 0 10 10 re f Q"
        );
        assert_eq!(
            written(&ops, true),
            "q\n1 0 0 1 72.5 .25 cm\n1 .5 0 rg\n0 0 10 10 re\nf\nQ\n"
        );
    }

    #[test]
    fn text_operators_follow_mupdf_spacing() {
        let font = Object::Null;
        let arr = Object::Array(Arc::new(vec![
            Object::String(Arc::from(&b"Hi"[..])),
            Object::Int(-250),
            Object::String(Arc::from(&b"(x)"[..])),
        ]));
        let ops = [
            Op::BeginText,
            Op::Font {
                name: b"F1",
                font: Some(&font),
                size: 12.0,
            },
            Op::Show {
                text: b"a\\b",
                c_string: b"a\\b",
            },
            Op::Show {
                text: b"\x01\xff",
                c_string: b"",
            },
            Op::ShowArray(Some(&arr)),
            Op::EndText,
        ];
        // No separator before Tf or an array, nor between a string and its
        // operator.
        assert_eq!(
            written(&ops, false),
            "BT/F1 12 Tf (a\\\\b)Tj <01ff>Tj[(Hi)-250(\\(x\\))]TJ ET"
        );
    }

    #[test]
    fn extgstate_entries_are_not_written_again() {
        let gs = Object::Null;
        let ops = [
            Op::GsBegin {
                name: b"GS 1",
                extgstate: &gs,
            },
            Op::LineWidth(2.0),
            Op::GsEnd,
            Op::LineWidth(3.0),
            Op::Restore,
        ];
        let diag = Diagnostics::default();
        let mut w = Writer::new(&diag, false, false);
        for op in &ops {
            w.op(op);
        }
        assert_eq!(String::from_utf8_lossy(&w.out), "/GS#201 gs 3 w Q");
        assert!(diag.has("content.gstate_underflow"));
    }

    #[test]
    fn marked_content_writes_its_properties() {
        let props = Object::Dict(Arc::new({
            let mut d = Dict::new();
            d.put(b"MCID", Object::Int(3));
            d
        }));
        let name = Object::name(b"P0");
        let ops = [
            Op::BeginMarkedProperties {
                tag: b"Span",
                raw: Some(&props),
                cooked: None,
            },
            Op::EndMarked,
            Op::BeginMarkedProperties {
                tag: b"OC",
                raw: Some(&name),
                cooked: None,
            },
            Op::EndMarked,
        ];
        assert_eq!(
            written(&ops, false),
            "/Span<</MCID 3>>BDC EMC /OC/P0 BDC EMC"
        );
    }

    #[test]
    fn inline_images_write_their_parameters() {
        let diag = Diagnostics::default();
        let mut w = Writer::new(&diag, false, false);
        let img = BiImage {
            width: 2,
            height: 1,
            bpc: 8,
            image_mask: false,
            colorspace: Some(ColorSpace::Gray),
            device: true,
            interpolate: false,
            decode: vec![0.0, 1.0],
            compression: Compression::Flate {
                predictor: 1,
                columns: 1,
                colors: 1,
                bpc: 8,
            },
            data: b"zz".to_vec(),
        };
        w.op(&Op::Save);
        w.inline_image(&img, b"").unwrap();
        w.op(&Op::Restore);
        assert_eq!(
            String::from_utf8_lossy(&w.out),
            "q BI /W 2/H 1/BPC 8/CS/G/D[0 1]/F/Fl ID zz EI Q"
        );
    }
}
