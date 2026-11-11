// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-interpret.c, source/pdf/pdf-image.c
// (pdf_load_image_imp for inline images), source/pdf/pdf-stream.c
// (pdf_open_contents_stream, pdf_open_object_array,
// pdf_load_compressed_inline_image), source/fitz/filter-basic.c
// (fz_open_concat), source/fitz/filter-leech.c and source/fitz/image.c
// (fz_decomp_image_from_stream), Copyright (C) 2004-2026 Artifex Software,
// Inc.

//! Content streams, as MuPDF's interpreter reads them.
//!
//! A page's contents are read as one stream (the parts of a contents array
//! joined by spaces), lexed, and each operator is handed to a [`Processor`]
//! with its operands, the way `pdf_process_stream` and
//! `pdf_process_keyword` do it:
//!
//! - Operands live where MuPDF keeps them: up to 32 numbers, one name, one
//!   short string (256 bytes) and one object (an array, a dictionary, a
//!   second name or a longer string). An operator takes the numbers it
//!   needs from the bottom of the stack, missing ones read as 0, and the
//!   stack is cleared after it.
//! - Inside `BT`/`ET`, an array is a `TJ` array: numbers and strings only,
//!   with `Tw` and `Tc` allowed inside it after a number.
//! - A syntax error is reported and lexing carries on after it; after 100
//!   the rest of the page is ignored. Other errors end the page.
//! - An inline image is decoded to find where its data ends: the decoders
//!   take from the stream exactly what they read, through a leech, and
//!   `EI` is searched for after that.
//! - Unbalanced `q` are closed at the end.
//!
//! Resource lookups that fail are errors, as in MuPDF, and so are
//! colorspaces and shadings that do not load: they are loaded here, as
//! `pdf_process_keyword` loads them. Fonts, patterns, images and soft
//! masks are handed over as objects for the interpreter to load, and
//! optional content is not evaluated yet, so a `Do` of a hidden XObject is
//! still handed over.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::colorspace::{self, ColorSpace};
use crate::diag::{Diagnostics, report};
use crate::document::Document;
use crate::lex::{Lexer, Token};
use crate::object::{Dict, Object};
use crate::parse::{parse_array, parse_dict};
use crate::shade::{self, Shade};
use crate::stream::{Next, Source, Stream};

/// MuPDF's MAX_SYNTAX_ERRORS.
pub const MAX_SYNTAX_ERRORS: usize = 100;
/// `pdf_csi`'s operand stack.
const STACK_SIZE: usize = 32;
/// `pdf_csi`'s name and string buffers.
const NAME_SIZE: usize = 256;
const STRING_SIZE: usize = 256;
/// `parse_inline_image`'s colorspace name buffer.
const CSNAME_SIZE: usize = 40;

/// An inline image, as `pdf_load_inline_image` made it.
#[derive(Debug)]
pub struct InlineImage<'a> {
    pub dict: Dict,
    pub width: i32,
    pub height: i32,
    pub bpc: i32,
    pub image_mask: bool,
    pub colorspace: Option<ColorSpace>,
    /// Components per pixel: the colorspace's, or 1.
    pub n: usize,
    /// The (still encoded) bytes the decoders consumed.
    pub data: &'a [u8],
}

/// One operator, as `pdf_process_keyword` hands it to a processor. Names
/// are the bytes MuPDF keeps (up to a NUL); objects are as written,
/// unresolved.
#[derive(Debug)]
pub enum Op<'a> {
    LineWidth(f32),
    LineJoin(i32),
    LineCap(i32),
    MiterLimit(f32),
    Dash {
        array: Option<&'a Object>,
        phase: f32,
    },
    Intent(&'a [u8]),
    Flatness(f32),
    /// `gs`: the entries of the ExtGState follow, then `GsEnd`.
    GsBegin {
        name: &'a [u8],
        extgstate: &'a Object,
    },
    GsEnd,
    GsBlendMode(&'a [u8]),
    GsStrokeAlpha(f32),
    GsFillAlpha(f32),
    /// `/SMask`: the mask dictionary, or None for `/None`.
    GsSoftMask(Option<&'a Object>),
    GsOverprintStroke(bool),
    GsOverprintFill(bool),
    GsOverprintMode(i32),
    GsBlackPointCompensation(&'a [u8]),
    Save,
    Restore,
    Transform([f32; 6]),
    MoveTo(f32, f32),
    LineTo(f32, f32),
    CurveTo([f32; 6]),
    CurveToV([f32; 4]),
    CurveToY([f32; 4]),
    ClosePath,
    Rect([f32; 4]),
    Stroke,
    CloseStroke,
    FillCompat,
    Fill,
    FillEvenOdd,
    FillStroke,
    FillStrokeEvenOdd,
    CloseFillStroke,
    CloseFillStrokeEvenOdd,
    EndPath,
    Clip,
    ClipEvenOdd,
    BeginText,
    EndText,
    CharSpacing(f32),
    WordSpacing(f32),
    HorizontalScaling(f32),
    Leading(f32),
    /// `Tf`, or an ExtGState's /Font (named `ExtGState`); `font` is the
    /// font dictionary, or None where MuPDF falls back to its last-resort
    /// font.
    Font {
        name: &'a [u8],
        font: Option<&'a Object>,
        size: f32,
    },
    RenderMode(i32),
    Rise(f32),
    TextMove(f32, f32),
    TextMoveSetLeading(f32, f32),
    TextMatrix([f32; 6]),
    NextLine,
    ShowArray(Option<&'a Object>),
    /// `Tj`. `c_string` is the operand as code reading it as a C string
    /// sees it: a short string is kept in a 256-byte buffer that is never
    /// cleared, so read up to a NUL it runs on into what earlier, longer
    /// strings left there.
    Show {
        text: &'a [u8],
        c_string: &'a [u8],
    },
    NextLineShow {
        text: &'a [u8],
        c_string: &'a [u8],
    },
    NextLineShowSpaced {
        word_spacing: f32,
        char_spacing: f32,
        text: &'a [u8],
        c_string: &'a [u8],
    },
    Type3Width(f32, f32),
    Type3Bbox([f32; 6]),
    /// `CS`/`cs`: None for a pattern space.
    ColorSpace {
        stroke: bool,
        name: &'a [u8],
        colorspace: Option<ColorSpace>,
    },
    ColorPattern {
        stroke: bool,
        name: &'a [u8],
        pattern: &'a Object,
        color: &'a [f32],
    },
    ColorShade {
        stroke: bool,
        name: &'a [u8],
        shading: &'a Object,
        shade: &'a Shade,
    },
    Color {
        stroke: bool,
        color: &'a [f32],
    },
    Gray {
        stroke: bool,
        gray: f32,
    },
    Rgb {
        stroke: bool,
        rgb: [f32; 3],
    },
    Cmyk {
        stroke: bool,
        cmyk: [f32; 4],
    },
    /// `BI`, with the /CS name when it is a direct name.
    InlineImage {
        image: &'a InlineImage<'a>,
        colorspace_name: Option<&'a [u8]>,
    },
    Shade {
        name: &'a [u8],
        shading: &'a Object,
        shade: &'a Shade,
    },
    DrawImage {
        name: &'a [u8],
        image: &'a Object,
    },
    DrawForm {
        name: &'a [u8],
        form: &'a Object,
    },
    MarkedPoint(&'a [u8]),
    MarkedPointProperties {
        tag: &'a [u8],
        raw: Option<&'a Object>,
    },
    BeginMarked(&'a [u8]),
    BeginMarkedProperties {
        tag: &'a [u8],
        raw: Option<&'a Object>,
    },
    EndMarked,
    BeginCompatibility,
    EndCompatibility,
    /// The end of the data, before unbalanced `q` are closed.
    EndOfData,
    /// The end of processing.
    End,
}

/// What operators go to (`pdf_processor`). An error is a report line;
/// syntax and format errors are counted and lexing goes on, others end
/// the page.
pub trait Processor {
    fn op(&mut self, op: Op<'_>) -> Result<(), String>;

    /// Whether the processor takes `/SMask` from an ExtGState. MuPDF looks
    /// at the mask only for processors that do.
    fn wants_soft_mask(&self) -> bool {
        true
    }
}

/// Up to the first NUL, as MuPDF's C strings hold names and keywords.
fn cstr(bytes: &[u8]) -> &[u8] {
    bytes.split(|&b| b == 0).next().unwrap_or_default()
}

fn recoverable(line: &str) -> bool {
    line.starts_with("syntax error: ") || line.starts_with("format error: ")
}

/// `pdf_process_contents` for a page or form: `contents` is its /Contents
/// as written (None when there is none), `resources` its resource
/// dictionary. Errors that end the page are returned as report lines.
pub fn process_contents(
    doc: &Document,
    resources: Option<&Object>,
    contents: Option<&Object>,
    proc: &mut dyn Processor,
) -> Result<(), String> {
    let Some(contents) = contents else {
        return Ok(());
    };
    let data = contents_data(doc, contents)?;
    let diag = doc.diagnostics();
    let mut csi = Csi::new(doc, proc);
    if let Some(res) = resources {
        csi.rstack.push(res.clone());
    }
    let mut lx = Lexer::small(&data, 0, diag);
    csi.process_stream(&mut lx)?;
    csi.process_end()
}

/// `pdf_open_contents_stream`, read whole.
fn contents_data(doc: &Document, contents: &Object) -> Result<Vec<u8>, String> {
    let diag = doc.diagnostics();
    match doc.resolve(contents) {
        // pdf_open_object_array: each part by number, joined with spaces.
        // A part that does not open is skipped; one whose reading fails
        // ends the stream there.
        Object::Array(parts) => {
            let mut out = Vec::new();
            for (i, part) in parts.iter().enumerate() {
                let num = part.as_ref().map_or(0, |r| r.num);
                let opened = match doc.object(num) {
                    Some(Object::Stream(s)) => doc.content_stream_data(&s).map_err(|e| e.report()),
                    _ => Err(report::format("object is not a stream")),
                };
                match opened {
                    Ok((data, failed)) => {
                        out.extend_from_slice(&data);
                        if failed {
                            return Ok(out);
                        }
                        out.push(b' ');
                    }
                    Err(line) => {
                        diag.warn("error.reported", line);
                        diag.warn(
                            "content.part_unloadable",
                            format!("cannot load content stream part {}/{}", i + 1, parts.len()),
                        );
                    }
                }
            }
            Ok(out)
        }
        Object::Stream(s) => doc
            .content_stream_data(&s)
            .map(|(data, _)| data)
            .map_err(|e| e.report()),
        _ => {
            let num = contents.as_ref().map_or(0, |r| r.num);
            diag.warn(
                "content.not_stream",
                format!("content stream is not a stream ({num} 0 R)"),
            );
            Ok(Vec::new())
        }
    }
}

/// `pdf_csi`: the interpreter's state while it lexes one stream.
struct Csi<'d, 'p> {
    doc: &'d Document,
    proc: &'p mut dyn Processor,
    /// Resource dictionaries, innermost last (`pdf_resource_stack`).
    rstack: Vec<Object>,
    /// Shadings loaded so far, by object number, as MuPDF's store keeps
    /// them: a function-based one is sampled 66,049 times.
    shades: HashMap<i32, Result<Arc<Shade>, String>>,
    gstate: i32,
    xbalance: i32,
    in_text: bool,
    obj: Option<Object>,
    name: Vec<u8>,
    string: [u8; STRING_SIZE],
    string_len: usize,
    top: usize,
    stack: [f32; STACK_SIZE],
}

impl<'d, 'p> Csi<'d, 'p> {
    fn new(doc: &'d Document, proc: &'p mut dyn Processor) -> Self {
        Csi {
            doc,
            proc,
            rstack: Vec::new(),
            shades: HashMap::new(),
            gstate: 0,
            xbalance: 0,
            in_text: false,
            obj: None,
            name: Vec::new(),
            string: [0; STRING_SIZE],
            string_len: 0,
            top: 0,
            stack: [0.0; STACK_SIZE],
        }
    }

    fn diag(&self) -> &'d Diagnostics {
        self.doc.diagnostics()
    }

    fn clear_stack(&mut self) {
        self.obj = None;
        self.name.clear();
        // The bytes stay in the buffer.
        self.string_len = 0;
        self.stack = [0.0; STACK_SIZE];
        self.top = 0;
    }

    /// `pdf_lookup_resource`: the innermost dictionary that has it.
    fn lookup(&self, kind: &[u8], name: &[u8]) -> Option<Object> {
        self.rstack.iter().rev().find_map(|res| {
            let sub = self.doc.dict_get(res, kind);
            let found = sub.as_dict()?.get(name)?;
            (!matches!(found, Object::Null)).then(|| found.clone())
        })
    }

    fn s(&self, i: usize) -> f32 {
        self.stack[i]
    }

    fn s6(&self) -> [f32; 6] {
        [
            self.s(0),
            self.s(1),
            self.s(2),
            self.s(3),
            self.s(4),
            self.s(5),
        ]
    }

    fn s4(&self) -> [f32; 4] {
        [self.s(0), self.s(1), self.s(2), self.s(3)]
    }

    fn emit(&mut self, op: Op<'_>) -> Result<(), String> {
        self.proc.op(op)
    }

    /// `pdf_process_stream`.
    fn process_stream(&mut self, lx: &mut Lexer) -> Result<(), String> {
        self.clear_stack();
        let mut syntax_errors = 0;
        let mut in_text_array = false;
        loop {
            let result = self.lex_ops(lx, &mut in_text_array);
            self.clear_stack();
            match result {
                Ok(()) => break,
                Err(line) if recoverable(&line) => {
                    self.diag().warn("error.reported", line);
                    syntax_errors += 1;
                    if syntax_errors >= MAX_SYNTAX_ERRORS {
                        self.diag().warn(
                            "content.too_many_errors",
                            "too many syntax errors; ignoring rest of page",
                        );
                        break;
                    }
                    in_text_array = false;
                }
                Err(line) => return Err(line),
            }
        }
        if syntax_errors > 0 {
            self.diag().warn(
                "content.syntax_errors",
                "encountered syntax errors; page may not be correct",
            );
        }
        Ok(())
    }

    /// The inner loop of `pdf_process_stream`, until the end of the data
    /// (Ok) or an error.
    fn lex_ops(&mut self, lx: &mut Lexer, in_text_array: &mut bool) -> Result<(), String> {
        loop {
            let tok = lx.next_token();
            if *in_text_array {
                match tok {
                    Token::CloseArray => *in_text_array = false,
                    Token::Real(f) => self.push_text_item(Object::Real(f)),
                    Token::Int(i) => self.push_text_item(Object::Int(i)),
                    Token::String(s) => self.push_text_item(Object::String(Arc::from(s))),
                    Token::Eof => return Ok(()),
                    Token::Keyword(w) if matches!(cstr(&w), b"Tw" | b"Tc") => {
                        // A spacing operator inside a TJ array takes the
                        // number before it.
                        let last = match &self.obj {
                            Some(Object::Array(a)) => a.last().filter(|o| o.is_number()).cloned(),
                            _ => None,
                        };
                        let Some(last) = last else {
                            return Err(report::syntax("syntax error in array"));
                        };
                        self.stack[0] = last.to_real();
                        if let Some(Object::Array(a)) = &mut self.obj {
                            Arc::make_mut(a).pop();
                        }
                        self.keyword(lx, cstr(&w))?;
                    }
                    _ => return Err(report::syntax("syntax error in array")),
                }
                continue;
            }
            match tok {
                Token::EndStream | Token::Eof => return Ok(()),
                Token::OpenArray => {
                    self.obj = None;
                    if self.in_text {
                        *in_text_array = true;
                        self.obj = Some(Object::Array(Arc::new(Vec::new())));
                    } else {
                        let a = parse_array(lx, 0).map_err(|e| report::syntax(&e.0))?;
                        self.obj = Some(Object::Array(Arc::new(a)));
                    }
                }
                Token::OpenDict => {
                    self.obj = None;
                    let d = parse_dict(lx, 0).map_err(|e| report::syntax(&e.0))?;
                    self.obj = Some(Object::Dict(Arc::new(d)));
                }
                Token::Name(n) => {
                    let n = cstr(&n);
                    if self.name.is_empty() {
                        self.name = n[..n.len().min(NAME_SIZE - 1)].to_vec();
                    } else {
                        self.obj = Some(Object::name(n));
                    }
                }
                Token::Int(i) => self.push_number(i as f32)?,
                Token::Real(f) => self.push_number(f)?,
                Token::String(s) => {
                    if s.len() <= STRING_SIZE {
                        self.string[..s.len()].copy_from_slice(&s);
                        self.string_len = s.len();
                    } else {
                        self.obj = Some(Object::String(Arc::from(s)));
                    }
                }
                Token::Keyword(w) => {
                    self.keyword(lx, cstr(&w))?;
                    self.clear_stack();
                }
                _ => return Err(report::syntax("syntax error in content stream")),
            }
        }
    }

    fn push_text_item(&mut self, item: Object) {
        if let Some(Object::Array(a)) = &mut self.obj {
            Arc::make_mut(a).push(item);
        }
    }

    fn push_number(&mut self, v: f32) -> Result<(), String> {
        if self.top >= STACK_SIZE {
            return Err(report::syntax("stack overflow"));
        }
        self.stack[self.top] = v;
        self.top += 1;
        Ok(())
    }

    /// The string operand of `Tj`, `'` and `"`: the short string, or else
    /// the string in the object slot; and the same read as a C string.
    fn text(&self) -> (Vec<u8>, Vec<u8>) {
        if self.string_len > 0 {
            let end = self
                .string
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(STRING_SIZE);
            return (
                self.string[..self.string_len].to_vec(),
                self.string[..end].to_vec(),
            );
        }
        let text = match &self.obj {
            Some(Object::String(s)) => s.to_vec(),
            _ => Vec::new(),
        };
        (text.clone(), text)
    }

    /// `pdf_process_keyword`.
    fn keyword(&mut self, lx: &mut Lexer, word: &[u8]) -> Result<(), String> {
        let name = self.name.clone();
        match word {
            b"w" => self.emit(Op::LineWidth(self.s(0))),
            b"j" => self.emit(Op::LineJoin(clampi(self.s(0), 0, 2))),
            b"J" => self.emit(Op::LineCap(clampi(self.s(0), 0, 2))),
            b"M" => self.emit(Op::MiterLimit(self.s(0))),
            b"d" => {
                let obj = self.obj.clone();
                self.emit(Op::Dash {
                    array: obj.as_ref(),
                    phase: self.s(0),
                })
            }
            b"ri" => self.emit(Op::Intent(&name)),
            b"i" => self.emit(Op::Flatness(self.s(0))),
            b"gs" => {
                let Some(gs) = self.lookup(b"ExtGState", &name) else {
                    return Err(report::syntax(&format!(
                        "cannot find ExtGState resource '{}'",
                        String::from_utf8_lossy(&name)
                    )));
                };
                self.emit(Op::GsBegin {
                    name: &name,
                    extgstate: &gs,
                })?;
                self.process_extgstate(&gs)?;
                self.emit(Op::GsEnd)
            }
            b"q" => {
                self.gstate += 1;
                self.emit(Op::Save)
            }
            b"Q" => {
                self.gstate -= 1;
                self.emit(Op::Restore)
            }
            b"cm" => self.emit(Op::Transform(self.s6())),
            b"m" => self.emit(Op::MoveTo(self.s(0), self.s(1))),
            b"l" => self.emit(Op::LineTo(self.s(0), self.s(1))),
            b"c" => self.emit(Op::CurveTo(self.s6())),
            b"v" => self.emit(Op::CurveToV(self.s4())),
            b"y" => self.emit(Op::CurveToY(self.s4())),
            b"h" => self.emit(Op::ClosePath),
            b"re" => self.emit(Op::Rect(self.s4())),
            b"S" => self.emit(Op::Stroke),
            b"s" => self.emit(Op::CloseStroke),
            b"F" => self.emit(Op::FillCompat),
            b"f" => self.emit(Op::Fill),
            b"f*" => self.emit(Op::FillEvenOdd),
            b"B" => self.emit(Op::FillStroke),
            b"B*" => self.emit(Op::FillStrokeEvenOdd),
            b"b" => self.emit(Op::CloseFillStroke),
            b"b*" => self.emit(Op::CloseFillStrokeEvenOdd),
            b"n" => self.emit(Op::EndPath),
            b"W" => self.emit(Op::Clip),
            b"W*" => self.emit(Op::ClipEvenOdd),
            b"BT" => {
                self.in_text = true;
                self.emit(Op::BeginText)
            }
            b"ET" => {
                self.in_text = false;
                self.emit(Op::EndText)
            }
            b"Tc" => self.emit(Op::CharSpacing(self.s(0))),
            b"Tw" => self.emit(Op::WordSpacing(self.s(0))),
            b"Tz" => self.emit(Op::HorizontalScaling(self.s(0))),
            b"TL" => self.emit(Op::Leading(self.s(0))),
            b"Tr" => self.emit(Op::RenderMode(self.s(0) as i32)),
            b"Ts" => self.emit(Op::Rise(self.s(0))),
            b"Tf" => {
                // pdf_is_dict: otherwise MuPDF's last-resort font.
                let font = self
                    .lookup(b"Font", &name)
                    .filter(|f| self.doc.resolve(f).as_dict().is_some());
                self.emit(Op::Font {
                    name: &name,
                    font: font.as_ref(),
                    size: self.s(0),
                })
            }
            b"Td" => self.emit(Op::TextMove(self.s(0), self.s(1))),
            b"TD" => self.emit(Op::TextMoveSetLeading(self.s(0), self.s(1))),
            b"Tm" => self.emit(Op::TextMatrix(self.s6())),
            b"T*" => self.emit(Op::NextLine),
            b"TJ" => {
                let obj = self.obj.clone();
                self.emit(Op::ShowArray(obj.as_ref()))
            }
            b"Tj" => {
                let (text, c_string) = self.text();
                self.emit(Op::Show {
                    text: &text,
                    c_string: &c_string,
                })
            }
            b"'" => {
                let (text, c_string) = self.text();
                self.emit(Op::NextLineShow {
                    text: &text,
                    c_string: &c_string,
                })
            }
            b"\"" => {
                let (text, c_string) = self.text();
                self.emit(Op::NextLineShowSpaced {
                    word_spacing: self.s(0),
                    char_spacing: self.s(1),
                    text: &text,
                    c_string: &c_string,
                })
            }
            b"d0" => self.emit(Op::Type3Width(self.s(0), self.s(1))),
            b"d1" => self.emit(Op::Type3Bbox(self.s6())),
            b"CS" => self.process_cs(true, &name),
            b"cs" => self.process_cs(false, &name),
            b"SC" | b"SCN" => self.process_sc(true, &name),
            b"sc" | b"scn" => self.process_sc(false, &name),
            b"G" => self.emit(Op::Gray {
                stroke: true,
                gray: self.s(0),
            }),
            b"g" => self.emit(Op::Gray {
                stroke: false,
                gray: self.s(0),
            }),
            b"RG" => self.emit(Op::Rgb {
                stroke: true,
                rgb: [self.s(0), self.s(1), self.s(2)],
            }),
            b"rg" => self.emit(Op::Rgb {
                stroke: false,
                rgb: [self.s(0), self.s(1), self.s(2)],
            }),
            b"K" => self.emit(Op::Cmyk {
                stroke: true,
                cmyk: self.s4(),
            }),
            b"k" => self.emit(Op::Cmyk {
                stroke: false,
                cmyk: self.s4(),
            }),
            b"BI" => {
                let (dict, csname) = self.parse_inline_dict(lx)?;
                let image = self.load_inline_image(lx, dict)?;
                find_ei(lx)?;
                self.emit(Op::InlineImage {
                    image: &image,
                    colorspace_name: (!csname.is_empty()).then_some(&csname[..]),
                })
            }
            b"sh" => {
                let Some(shading) = self.lookup(b"Shading", &name) else {
                    return Err(report::syntax(&format!(
                        "cannot find Shading resource '{}'",
                        String::from_utf8_lossy(&name)
                    )));
                };
                let shade = self.load_shade(&shading)?;
                self.emit(Op::Shade {
                    name: &name,
                    shading: &shading,
                    shade: &shade,
                })
            }
            b"Do" => self.process_do(&name),
            b"MP" => self.emit(Op::MarkedPoint(&name)),
            b"DP" => {
                let obj = self.obj.clone();
                self.emit(Op::MarkedPointProperties {
                    tag: &name,
                    raw: obj.as_ref(),
                })
            }
            b"BMC" => self.emit(Op::BeginMarked(&name)),
            b"BDC" => {
                let obj = self.obj.clone();
                self.emit(Op::BeginMarkedProperties {
                    tag: &name,
                    raw: obj.as_ref(),
                })
            }
            b"EMC" => self.emit(Op::EndMarked),
            b"BX" => {
                self.xbalance += 1;
                self.emit(Op::BeginCompatibility)
            }
            b"EX" => {
                self.xbalance -= 1;
                self.emit(Op::EndCompatibility)
            }
            _ => {
                if self.xbalance == 0 {
                    let message = format!("unknown keyword: '{}'", String::from_utf8_lossy(word));
                    if matches!(word, b"Infinity" | b"NaN" | b"inf" | b"nan") {
                        self.diag().warn("content.unknown_keyword", message);
                    } else {
                        return Err(report::syntax(&message));
                    }
                }
                Ok(())
            }
        }
    }

    /// `pdf_process_extgstate`, for the entries that need no loading.
    fn process_extgstate(&mut self, gs: &Object) -> Result<(), String> {
        let doc = self.doc;
        let get = |key: &[u8]| doc.dict_get(gs, key);
        let lw = get(b"LW");
        if lw.is_number() {
            self.emit(Op::LineWidth(lw.to_real()))?;
        }
        if let Object::Int(_) = get(b"LC") {
            self.emit(Op::LineCap(get(b"LC").to_int().clamp(0, 2)))?;
        }
        if let Object::Int(_) = get(b"LJ") {
            self.emit(Op::LineJoin(get(b"LJ").to_int().clamp(0, 2)))?;
        }
        let ml = get(b"ML");
        if ml.is_number() {
            self.emit(Op::MiterLimit(ml.to_real()))?;
        }
        if let Object::Array(d) = get(b"D") {
            let array = d.first().cloned();
            let phase = d.get(1).map_or(0.0, |p| doc.resolve(p).to_real());
            self.emit(Op::Dash {
                array: array.as_ref(),
                phase,
            })?;
        }
        if let Object::Name(ri) = get(b"RI") {
            self.emit(Op::Intent(&ri))?;
        }
        let fl = get(b"FL");
        if fl.is_number() {
            self.emit(Op::Flatness(fl.to_real()))?;
        }
        if let Object::Array(font) = get(b"Font") {
            let font_ref = font.first().cloned().unwrap_or(Object::Null);
            let size = font.get(1).map_or(0.0, |s| doc.resolve(s).to_real());
            let font_obj = doc
                .resolve(&font_ref)
                .as_dict()
                .is_some()
                .then_some(&font_ref);
            self.emit(Op::Font {
                name: b"ExtGState",
                font: font_obj,
                size,
            })?;
        }
        if let Object::Bool(b) = get(b"OP") {
            self.emit(Op::GsOverprintStroke(b))?;
        }
        if let Object::Bool(b) = get(b"op") {
            self.emit(Op::GsOverprintFill(b))?;
        }
        if let Object::Int(_) = get(b"OPM") {
            self.emit(Op::GsOverprintMode(get(b"OPM").to_int()))?;
        }
        if let Object::Name(n) = get(b"UseBlackPtComp") {
            self.emit(Op::GsBlackPointCompensation(&n))?;
        }
        let tr2 = get(b"TR2");
        if let Object::Name(n) = &tr2
            && !matches!(n.as_ref(), b"Identity" | b"Default")
        {
            self.diag()
                .warn("content.transfer_function", "ignoring transfer function");
        }
        // /TR counts only without /TR2.
        let has_tr2 = doc
            .resolve(gs)
            .as_dict()
            .and_then(|d| d.get(b"TR2"))
            .is_some_and(|v| !matches!(v, Object::Null));
        if !has_tr2
            && let Object::Name(n) = get(b"TR")
            && n.as_ref() != b"Identity"
        {
            self.diag()
                .warn("content.transfer_function", "ignoring transfer function");
        }
        let ca = get(b"CA");
        if ca.is_number() {
            self.emit(Op::GsStrokeAlpha(ca.to_real()))?;
        }
        let ca = get(b"ca");
        if ca.is_number() {
            self.emit(Op::GsFillAlpha(ca.to_real()))?;
        }
        let mut bm = get(b"BM");
        if let Object::Array(a) = &bm {
            bm = a.first().map_or(Object::Null, |b| doc.resolve(b));
        }
        if let Object::Name(n) = &bm {
            self.emit(Op::GsBlendMode(n))?;
        }
        if self.proc.wants_soft_mask() {
            match get(b"SMask") {
                Object::Name(n) if n.as_ref() == b"None" => self.emit(Op::GsSoftMask(None))?,
                mask if mask.as_dict().is_some() => self.emit(Op::GsSoftMask(Some(&mask)))?,
                _ => {}
            }
        }
        Ok(())
    }

    /// `pdf_process_CS`.
    fn process_cs(&mut self, stroke: bool, name: &[u8]) -> Result<(), String> {
        let pattern = |name: &'static [u8]| Op::ColorSpace {
            stroke,
            name,
            colorspace: None,
        };
        let cs = match name {
            b"Pattern" => return self.emit(pattern(b"Pattern")),
            b"DeviceGray" => ColorSpace::Gray,
            b"DeviceRGB" => ColorSpace::Rgb,
            b"DeviceCMYK" => ColorSpace::Cmyk,
            _ => {
                let Some(obj) = self.lookup(b"ColorSpace", name) else {
                    return Err(report::syntax(&format!(
                        "cannot find ColorSpace resource '{}'",
                        String::from_utf8_lossy(name)
                    )));
                };
                if let Object::Array(a) = self.doc.resolve(&obj)
                    && a.len() == 1
                    && self.doc.resolve(&a[0]).is_name(b"Pattern")
                {
                    return self.emit(pattern(b"Pattern"));
                }
                colorspace::load(self.doc, &obj)?
            }
        };
        self.emit(Op::ColorSpace {
            stroke,
            name,
            colorspace: Some(cs),
        })
    }

    /// `pdf_process_SC`.
    fn process_sc(&mut self, stroke: bool, name: &[u8]) -> Result<(), String> {
        let color: Vec<f32> = self.stack[..self.top].to_vec();
        if name.is_empty() {
            return self.emit(Op::Color {
                stroke,
                color: &color,
            });
        }
        let Some(pattern) = self.lookup(b"Pattern", name) else {
            return Err(report::syntax(&format!(
                "cannot find Pattern resource '{}'",
                String::from_utf8_lossy(name)
            )));
        };
        match self.doc.dict_get(&pattern, b"PatternType").to_int() {
            1 => self.emit(Op::ColorPattern {
                stroke,
                name,
                pattern: &pattern,
                color: &color,
            }),
            2 => {
                let shade = self.load_shade(&pattern)?;
                self.emit(Op::ColorShade {
                    stroke,
                    name,
                    shading: &pattern,
                    shade: &shade,
                })
            }
            other => Err(report::syntax(&format!("unknown pattern type: {other}"))),
        }
    }

    /// `pdf_load_shading`, once per object.
    fn load_shade(&mut self, obj: &Object) -> Result<Arc<Shade>, String> {
        let Some(r) = obj.as_ref() else {
            return shade::load(self.doc, obj).map(Arc::new);
        };
        if let Some(done) = self.shades.get(&r.num) {
            return done.clone();
        }
        let loaded = shade::load(self.doc, obj).map(Arc::new);
        self.shades.insert(r.num, loaded.clone());
        loaded
    }

    /// `pdf_process_Do`.
    fn process_do(&mut self, name: &[u8]) -> Result<(), String> {
        let Some(xobj) = self.lookup(b"XObject", name) else {
            return Err(report::syntax(&format!(
                "cannot find XObject resource '{}'",
                String::from_utf8_lossy(name)
            )));
        };
        let mut subtype = self.doc.dict_get(&xobj, b"Subtype");
        if subtype.is_name(b"Form") {
            let st = self.doc.dict_get(&xobj, b"Subtype2");
            if !matches!(st, Object::Null) {
                subtype = st;
            }
        }
        let Some(subtype) = subtype.as_name() else {
            return Err(report::syntax("no XObject subtype specified"));
        };
        match subtype {
            b"Form" => self.emit(Op::DrawForm { name, form: &xobj }),
            b"Image" => self.emit(Op::DrawImage { name, image: &xobj }),
            b"PS" => {
                self.diag()
                    .warn("content.xobject_ps", "ignoring XObject with subtype PS");
                Ok(())
            }
            other => {
                self.diag().warn(
                    "content.xobject_subtype",
                    format!(
                        "ignoring XObject with unknown subtype: '{}'",
                        String::from_utf8_lossy(other)
                    ),
                );
                Ok(())
            }
        }
    }

    /// The first half of `parse_inline_image`: the dictionary up to `ID`,
    /// the /CS name, and the white space after `ID`.
    fn parse_inline_dict(&mut self, lx: &mut Lexer) -> Result<(Dict, Vec<u8>), String> {
        // The dictionary is read with the document's (large) lex buffer.
        let mut big = Lexer::new(lx.data(), lx.pos(), self.diag());
        let parsed = parse_dict(&mut big, 0);
        lx.seek(big.pos());
        let dict = parsed.map_err(|e| report::syntax(&e.0))?;
        let csname = match dict.get(b"CS") {
            Some(Object::Name(n)) => {
                let n = cstr(n);
                n[..n.len().min(CSNAME_SIZE - 1)].to_vec()
            }
            _ => Vec::new(),
        };
        if lx.read_byte() == Some(b'\r') && lx.peek_byte() == Some(b'\n') {
            lx.read_byte();
        }
        Ok((dict, csname))
    }

    /// `pdf_load_image_imp` for an inline image whose data starts at the
    /// lexer's position, and `pdf_load_compressed_inline_image`.
    fn load_inline_image<'a>(
        &mut self,
        lx: &mut Lexer<'a>,
        dict: Dict,
    ) -> Result<InlineImage<'a>, String> {
        let doc = self.doc;
        let get = |key: &[u8]| match dict.get(key) {
            Some(Object::Null) | None => None,
            Some(v) => Some(v.clone()),
        };
        // pdf_dict_geta: the abbreviation wins.
        let geta = |key: &[u8], abbrev: &[u8]| {
            get(abbrev)
                .or_else(|| get(key))
                .map_or(Object::Null, |v| doc.resolve(&v))
        };
        let is_jpx = match get(b"Filter").map(|f| doc.resolve(&f)) {
            Some(Object::Name(n)) => n.as_ref() == b"JPXDecode",
            Some(Object::Array(a)) => a.iter().any(|f| doc.resolve(f).is_name(b"JPXDecode")),
            _ => false,
        };
        if is_jpx {
            return Err(report::syntax("inline JPXDecode image"));
        }
        let width = geta(b"Width", b"W").to_int();
        let height = geta(b"Height", b"H").to_int();
        let mut bpc = geta(b"BitsPerComponent", b"BPC").to_int();
        if bpc == 0 {
            bpc = 8;
        }
        let image_mask = matches!(geta(b"ImageMask", b"IM"), Object::Bool(true));
        if image_mask {
            bpc = 1;
        }
        if width <= 0 {
            return Err(report::syntax("image width is zero (or less)"));
        }
        if height <= 0 {
            return Err(report::syntax("image height is zero (or less)"));
        }
        if bpc <= 0 {
            return Err(report::syntax("image depth is zero (or less)"));
        }
        if bpc > 16 {
            return Err(report::syntax(&format!("image depth is too large: {bpc}")));
        }
        let mut colorspace = None;
        let mut n = 1;
        let cs_obj = get(b"CS").or_else(|| get(b"ColorSpace"));
        if let Some(mut obj) = cs_obj
            && !image_mask
        {
            // Inline images name colorspaces in the resources.
            if let Object::Name(name) = doc.resolve(&obj)
                && let Some(res) = self.lookup(b"ColorSpace", cstr(&name))
            {
                obj = res;
            }
            let cs = colorspace::load(doc, &obj)?;
            n = cs.n();
            colorspace = Some(cs);
        }
        let area = (bpc as u128).div_ceil(8) * width as u128 * height as u128 * n as u128;
        if area > u64::MAX as u128 {
            return Err(report::syntax("image is too large"));
        }
        let diag = self.diag();
        let resolved = |key: &[u8]| get(key).map_or(Object::Null, |v| doc.resolve(&v));
        let mut mask = resolved(b"SMask");
        if mask.as_dict().is_none() {
            mask = resolved(b"Mask");
        }
        if mask.as_dict().is_some() {
            diag.warn(
                "image.inline_soft_mask",
                "Ignoring invalid inline image soft mask",
            );
        } else if let Object::Array(key) = &mask {
            for i in 0..n * 2 {
                if !matches!(key.get(i).map(|v| doc.resolve(v)), Some(Object::Int(_))) {
                    diag.warn("image.color_key", "invalid value in color key mask");
                }
            }
        }
        let overflow = || report::syntax("integer overflow in inline image size");
        let stride = width
            .checked_mul(n as i32)
            .and_then(|s| s.checked_mul(bpc))
            .ok_or_else(overflow)?;
        let stride = (stride as i64 + 7) >> 3;
        let size = i32::try_from(stride)
            .ok()
            .and_then(|s| s.checked_mul(height))
            .ok_or_else(overflow)?;

        let start = lx.pos();
        let consumed = decode_inline(doc, &dict, &lx.data()[start..], size as usize)?;
        lx.seek(start + consumed);
        Ok(InlineImage {
            dict,
            width,
            height,
            bpc,
            image_mask,
            colorspace,
            n,
            data: &lx.data()[start..start + consumed],
        })
    }

    /// `pdf_process_end`.
    fn process_end(&mut self) -> Result<(), String> {
        self.emit(Op::EndOfData)?;
        while self.gstate > 0 {
            self.gstate -= 1;
            self.emit(Op::Restore)?;
        }
        self.emit(Op::End)
    }
}

/// `fz_clampi` of a float operand, which C converts to int first.
fn clampi(v: f32, lo: i32, hi: i32) -> i32 {
    (v as i32).clamp(lo, hi)
}

/// `fz_open_leecher` over the content: hands the decoders what they ask
/// for, and counts what it handed out.
struct Leech<'a> {
    data: &'a [u8],
    pos: Rc<Cell<usize>>,
}

impl Source for Leech<'_> {
    fn next(&mut self, max: usize, out: &mut Vec<u8>, _: &Diagnostics) -> Result<Next, String> {
        let at = self.pos.get();
        if at >= self.data.len() {
            return Ok(Next::Eof);
        }
        let n = max.max(1).min(self.data.len() - at);
        out.extend_from_slice(&self.data[at..at + n]);
        self.pos.set(at + n);
        Ok(Next::Data)
    }
}

/// Decodes an inline image's `size` bytes from `data`, as
/// `fz_decomp_image_from_stream` reads them, and returns how many bytes of
/// `data` the decoders consumed.
fn decode_inline(doc: &Document, dict: &Dict, data: &[u8], size: usize) -> Result<usize, String> {
    let diag = doc.diagnostics();
    let pos = Rc::new(Cell::new(0));
    // Without a filter this reads exactly the image's bytes, as MuPDF's
    // null filter does.
    let leech = Stream::new(
        Leech {
            data,
            pos: Rc::clone(&pos),
        },
        diag,
    );
    let resolve = |o: &Object| doc.resolve(o);
    let load = |o: &Object| match doc.resolve(o) {
        Object::Stream(s) => Some(doc.stream_data(&s)),
        _ => None,
    };
    let mut stm = crate::filter::open_filters(leech, dict, &resolve, Some(&load), None, true)
        .map_err(|e| e.report())?;
    let mut read = 0;
    let mut scratch = vec![0u8; 1 << 16];
    while read < size {
        let want = (size - read).min(scratch.len());
        let got = stm.read(&mut scratch[..want]);
        read += got;
        if got < want {
            break;
        }
    }
    if read < size {
        diag.warn("image.truncated", "padding truncated image");
    }
    drop(stm);
    Ok(pos.get())
}

/// The end of `parse_inline_image`: `EI` followed by white space, a `<`,
/// a `/` or the end.
fn find_ei(lx: &mut Lexer) -> Result<(), String> {
    let mut ch = lx.read_byte();
    loop {
        while !matches!(ch, Some(b'E') | None) {
            ch = lx.read_byte();
        }
        if ch == Some(b'E') {
            ch = lx.read_byte();
            if ch == Some(b'I') {
                // fz_peek_byte's EOF is -1, which is <= 32.
                match lx.peek_byte() {
                    None => return Ok(()),
                    Some(c) if c <= 32 || c == b'<' || c == b'/' => return Ok(()),
                    Some(c) => ch = Some(c),
                }
            }
        }
        if ch.is_none() {
            return Err(report::syntax("syntax error after inline image"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records each operator as text, for tests.
    #[derive(Default)]
    struct Log(Vec<String>);

    impl Processor for Log {
        fn op(&mut self, op: Op<'_>) -> Result<(), String> {
            let line = match op {
                Op::InlineImage { image, .. } => {
                    format!("BI {}x{} {:?}", image.width, image.height, image.data)
                }
                Op::Show { text, .. } => format!("Tj {}", String::from_utf8_lossy(text)),
                Op::ShowArray(a) => format!("TJ {a:?}"),
                Op::EndOfData | Op::End => return Ok(()),
                other => format!("{other:?}"),
            };
            self.0.push(line);
            Ok(())
        }
    }

    fn run(content: &[u8]) -> (Vec<String>, Vec<String>) {
        let mut file = b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n2 0 obj<</Length ".to_vec();
        file.extend_from_slice(format!("{}>>stream\n", content.len()).as_bytes());
        file.extend_from_slice(content);
        file.extend_from_slice(b"\nendstream endobj\ntrailer<</Root 1 0 R/Size 3>>\n");
        let doc = Document::open(file).unwrap_or_else(|e| panic!("{e:?}"));
        let before = doc.diagnostics().snapshot().len();
        let mut log = Log::default();
        let contents = Object::Ref(crate::ObjRef::new(2, 0));
        let res = Object::Dict(Arc::new(Dict::new()));
        process_contents(&doc, Some(&res), Some(&contents), &mut log)
            .unwrap_or_else(|e| panic!("{e}"));
        let diags = doc.diagnostics().snapshot()[before..]
            .iter()
            .map(|d| d.message.clone())
            .collect();
        (log.0, diags)
    }

    #[test]
    fn operators_take_their_operands_from_the_bottom_of_the_stack() {
        let (ops, diags) = run(b"1 2 3 m 4 l 0.5 w 7 j q 1 0 0 1 5 6 cm Q");
        assert_eq!(
            ops,
            [
                "MoveTo(1.0, 2.0)",
                "LineTo(4.0, 0.0)",
                "LineWidth(0.5)",
                "LineJoin(2)",
                "Save",
                "Transform([1.0, 0.0, 0.0, 1.0, 5.0, 6.0])",
                "Restore",
            ]
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn a_tj_array_holds_spacing_operators_and_text() {
        let (ops, _) = run(b"BT [(a) 5 Tw -20 (b)] TJ (c) Tj ET");
        assert_eq!(
            ops,
            [
                "BeginText",
                "WordSpacing(5.0)",
                "TJ Some(Array([String([97]), Int(-20), String([98])]))",
                "Tj c",
                "EndText",
            ]
        );
    }

    #[test]
    fn short_strings_share_a_buffer_that_is_never_cleared() {
        #[derive(Default)]
        struct Texts(Vec<(Vec<u8>, Vec<u8>)>);
        impl Processor for Texts {
            fn op(&mut self, op: Op<'_>) -> Result<(), String> {
                if let Op::Show { text, c_string } = op {
                    self.0.push((text.to_vec(), c_string.to_vec()));
                }
                Ok(())
            }
        }
        let doc = Document::open(
            &b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n2 0 obj<</Length 31>>stream\n\
(quoted) Tj (AB) Tj (x) w () Tj\nendstream endobj\ntrailer<</Root 1 0 R/Size 3>>\n"[..],
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        let mut t = Texts::default();
        let contents = Object::Ref(crate::ObjRef::new(2, 0));
        process_contents(&doc, None, Some(&contents), &mut t).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            t.0,
            [
                (b"quoted".to_vec(), b"quoted".to_vec()),
                (b"AB".to_vec(), b"ABoted".to_vec()),
                (Vec::new(), Vec::new()),
            ]
        );
    }

    #[test]
    fn syntax_errors_are_reported_and_lexing_goes_on() {
        let (ops, diags) = run(b"1 w foo 2 w BX bar EX true 3 w nan");
        assert_eq!(
            ops,
            [
                "LineWidth(1.0)",
                "LineWidth(2.0)",
                "BeginCompatibility",
                "EndCompatibility",
                "LineWidth(3.0)",
            ]
        );
        assert_eq!(
            diags,
            [
                "syntax error: unknown keyword: 'foo'",
                "syntax error: syntax error in content stream",
                "unknown keyword: 'nan'",
                "encountered syntax errors; page may not be correct",
            ]
        );
    }

    #[test]
    fn missing_resources_are_errors_and_unbalanced_q_are_closed() {
        let (ops, diags) = run(b"q q /F1 12 Tf /X Do /GS gs /CS0 cs /DeviceRGB cs");
        assert_eq!(
            ops,
            [
                "Save",
                "Save",
                "Font { name: [70, 49], font: None, size: 12.0 }",
                "ColorSpace { stroke: false, name: [68, 101, 118, 105, 99, 101, 82, 71, 66], colorspace: Some(Rgb) }",
                "Restore",
                "Restore",
            ]
        );
        assert_eq!(
            diags[..3],
            [
                "syntax error: cannot find XObject resource 'X'",
                "syntax error: cannot find ExtGState resource 'GS'",
                "syntax error: cannot find ColorSpace resource 'CS0'",
            ]
        );
    }

    #[test]
    fn inline_image_data_is_skipped_by_its_size_then_ei() {
        // Two bytes of data that read as "EI", then the real EI.
        let (ops, diags) = run(b"BI /W 2 /H 1 /CS /G ID EI EI 1 w");
        assert_eq!(ops, ["BI 2x1 [69, 73]", "LineWidth(1.0)"]);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn filtered_inline_images_consume_what_the_decoder_reads() {
        // ASCIIHex stops once it has the two bytes; what follows up to EI
        // is skipped.
        let (ops, diags) = run(b"BI /W 2 /H 1 /CS /G /F /AHx ID 4142> junk EI 2 w");
        assert_eq!(ops[0], "BI 2x1 [52, 49, 52, 50]");
        assert_eq!(ops[1], "LineWidth(2.0)");
        assert!(diags.is_empty(), "{diags:?}");
        // Too little data is padded, with a warning.
        let (_, diags) = run(b"BI /W 4 /H 1 /CS /G /F /AHx ID 41> EI");
        assert_eq!(diags, ["padding truncated image"]);
        // No EI at all.
        let (ops, diags) = run(b"BI /W 1 /H 1 ID x");
        assert!(ops.is_empty());
        assert_eq!(diags[0], "syntax error: syntax error after inline image");
    }
}
