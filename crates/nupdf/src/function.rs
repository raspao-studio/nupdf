// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-function.c and source/fitz/device.c
// (fz_eval_function), Copyright (C) 2004-2026 Artifex Software, Inc.

//! PDF functions (types 0, 2, 3 and 4), loaded and evaluated as MuPDF
//! loads and evaluates them.
//!
//! The arithmetic is single precision, with MuPDF's guards: a PostScript
//! stack of 100 entries that ignores overflow and underflow, NaN pushed as
//! 1, division by zero giving the largest float of the right sign, and
//! inputs and outputs clamped to the domain and range. Integer arithmetic
//! in calculator functions wraps.

use crate::diag::report;
use crate::document::Document;
use crate::lex::{Lexer, Token};
use crate::object::Object;
use crate::stream::Stream;

/// MuPDF's MAX_M and MAX_N (FZ_MAX_COLORS).
pub const MAX_M: usize = 32;
pub const MAX_N: usize = 32;
const MAX_STITCHING: usize = 256;
const MAX_SAMPLE_FUNCTION_SIZE: i64 = 100 << 20;
/// Nesting of stitching functions; MuPDF has only its cycle check.
const MAX_DEPTH: usize = 64;
/// `if`/`ifelse` nesting in a calculator function.
const MAX_PS_DEPTH: usize = 100;
const PS_STACK: usize = 100;

const FLT_EPSILON: f32 = f32::EPSILON;
const FZ_RADIAN: f32 = 57.295_78;

/// A loaded function: `m` inputs, `n` outputs.
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub m: usize,
    pub n: usize,
    domain: [[f32; 2]; MAX_M],
    range: [[f32; 2]; MAX_N],
    has_range: bool,
    kind: Kind,
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Sample(Box<Sampled>),
    Exponential(Box<Exponential>),
    Stitching {
        funcs: Vec<Function>,
        bounds: Vec<f32>,
        encode: Vec<f32>,
    },
    PostScript(Vec<Ps>),
}

#[derive(Debug, Clone, PartialEq)]
struct Sampled {
    size: [i32; MAX_M],
    encode: [[f32; 2]; MAX_M],
    decode: [[f32; 2]; MAX_N],
    samples: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
struct Exponential {
    n: f32,
    c0: [f32; MAX_N],
    c1: [f32; MAX_N],
}

/// A calculator function's code (`psobj`).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ps {
    Bool(bool),
    Int(i32),
    Real(f32),
    Op(Op),
    /// An `if`/`ifelse` block pointer.
    Block(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Abs,
    Add,
    And,
    Atan,
    Bitshift,
    Ceiling,
    Copy,
    Cos,
    Cvi,
    Cvr,
    Div,
    Dup,
    Eq,
    Exch,
    Exp,
    False,
    Floor,
    Ge,
    Gt,
    Idiv,
    If,
    Ifelse,
    Index,
    Le,
    Ln,
    Log,
    Lt,
    Mod,
    Mul,
    Ne,
    Neg,
    Not,
    Or,
    Pop,
    Return,
    Roll,
    Round,
    Sin,
    Sqrt,
    Sub,
    True,
    Truncate,
    Xor,
}

const PS_OPS: [(&[u8], Op); 43] = [
    (b"abs", Op::Abs),
    (b"add", Op::Add),
    (b"and", Op::And),
    (b"atan", Op::Atan),
    (b"bitshift", Op::Bitshift),
    (b"ceiling", Op::Ceiling),
    (b"copy", Op::Copy),
    (b"cos", Op::Cos),
    (b"cvi", Op::Cvi),
    (b"cvr", Op::Cvr),
    (b"div", Op::Div),
    (b"dup", Op::Dup),
    (b"eq", Op::Eq),
    (b"exch", Op::Exch),
    (b"exp", Op::Exp),
    (b"false", Op::False),
    (b"floor", Op::Floor),
    (b"ge", Op::Ge),
    (b"gt", Op::Gt),
    (b"idiv", Op::Idiv),
    (b"if", Op::If),
    (b"ifelse", Op::Ifelse),
    (b"index", Op::Index),
    (b"le", Op::Le),
    (b"ln", Op::Ln),
    (b"log", Op::Log),
    (b"lt", Op::Lt),
    (b"mod", Op::Mod),
    (b"mul", Op::Mul),
    (b"ne", Op::Ne),
    (b"neg", Op::Neg),
    (b"not", Op::Not),
    (b"or", Op::Or),
    (b"pop", Op::Pop),
    (b"return", Op::Return),
    (b"roll", Op::Roll),
    (b"round", Op::Round),
    (b"sin", Op::Sin),
    (b"sqrt", Op::Sqrt),
    (b"sub", Op::Sub),
    (b"true", Op::True),
    (b"truncate", Op::Truncate),
    (b"xor", Op::Xor),
];

/// `fz_clamp`: NaN passes through.
fn clamp(x: f32, min: f32, max: f32) -> f32 {
    if x < min {
        min
    } else if x > max {
        max
    } else {
        x
    }
}

fn lerp(x: f32, xmin: f32, xmax: f32, ymin: f32, ymax: f32) -> f32 {
    if xmin == xmax || ymin == ymax {
        return ymin;
    }
    ymin + (x - xmin) * (ymax - ymin) / (xmax - xmin)
}

/// `pdf_load_function`: errors are MuPDF's report lines.
pub fn load(
    doc: &Document,
    obj: &Object,
    inputs: usize,
    outputs: usize,
) -> Result<Function, String> {
    load_imp(doc, obj, inputs, outputs, &mut Vec::new())
}

fn load_imp(
    doc: &Document,
    obj: &Object,
    inputs: usize,
    outputs: usize,
    cycle: &mut Vec<i32>,
) -> Result<Function, String> {
    let num = obj.as_ref().map_or(0, |r| r.num);
    if (num != 0 && cycle.contains(&num)) || cycle.len() >= MAX_DEPTH {
        return Err(report::syntax("recursive function"));
    }
    cycle.push(num);
    let out = load_resolved(doc, obj, inputs, outputs, cycle);
    cycle.pop();
    out
}

fn real(doc: &Document, a: &[Object], i: usize) -> f32 {
    a.get(i).map_or(0.0, |v| doc.resolve(v).to_real())
}

fn load_resolved(
    doc: &Document,
    obj: &Object,
    inputs: usize,
    outputs: usize,
    cycle: &mut Vec<i32>,
) -> Result<Function, String> {
    let diag = doc.diagnostics();
    let get = |key: &[u8]| doc.dict_get(obj, key);
    let ty = get(b"FunctionType").to_int();
    if !matches!(ty, 0 | 2 | 3 | 4) {
        let num = obj.as_ref().map_or(0, |r| r.num);
        return Err(report::syntax(&format!(
            "unknown function type ({num} 0 R)"
        )));
    }
    let mut f = Function {
        m: 1,
        n: outputs,
        domain: [[0.0; 2]; MAX_M],
        range: [[0.0; 2]; MAX_N],
        has_range: false,
        kind: Kind::PostScript(Vec::new()),
    };
    // Required for all.
    let domain = get(b"Domain");
    let d = domain.as_array().unwrap_or_default();
    f.m = (d.len() / 2).clamp(1, MAX_M);
    for i in 0..f.m {
        f.domain[i] = [real(doc, d, i * 2), real(doc, d, i * 2 + 1)];
    }
    // Required for types 0 and 4, optional otherwise.
    if let Object::Array(r) = get(b"Range") {
        f.has_range = true;
        f.n = (r.len() / 2).clamp(1, MAX_N);
        for i in 0..f.n {
            f.range[i] = [real(doc, &r, i * 2), real(doc, &r, i * 2 + 1)];
        }
    } else {
        f.n = outputs.min(MAX_N);
    }
    if f.m != inputs {
        diag.warn("function.inputs", "wrong number of function inputs");
    }
    if f.n != outputs {
        diag.warn("function.outputs", "wrong number of function outputs");
    }
    f.kind = match ty {
        0 => load_sample(doc, &mut f, obj)?,
        2 => load_exponential(doc, &mut f, obj),
        3 => load_stitching(doc, &mut f, obj, cycle)?,
        _ => Kind::PostScript(load_postscript(doc, obj)?),
    };
    Ok(f)
}

/// `pdf_open_stream`, read whole.
fn stream_of(doc: &Document, obj: &Object) -> Result<Vec<u8>, String> {
    match doc.resolve(obj) {
        Object::Stream(s) => doc.stream_bytes(&s).map_err(|e| e.report()),
        _ => Err(report::format("object is not a stream")),
    }
}

fn load_sample(doc: &Document, f: &mut Function, obj: &Object) -> Result<Kind, String> {
    let diag = doc.diagnostics();
    let get = |key: &[u8]| doc.dict_get(obj, key);
    let mut size = [0i32; MAX_M];
    let sizes = get(b"Size");
    let s = sizes.as_array().unwrap_or_default();
    if s.len() < f.m {
        return Err(report::syntax("too few sample function dimension sizes"));
    }
    if s.len() > f.m {
        diag.warn(
            "function.sample_sizes",
            "too many sample function dimension sizes",
        );
    }
    for i in 0..f.m {
        size[i] = doc.resolve(&s[i]).to_int();
        if size[i] <= 0 {
            diag.warn(
                "function.sample_size",
                "non-positive sample function dimension size",
            );
            size[i] = 1;
        }
    }
    let bps = get(b"BitsPerSample").to_int();
    let mut encode = [[0.0f32; 2]; MAX_M];
    for i in 0..f.m {
        encode[i] = [0.0, (size[i] - 1) as f32];
    }
    if let Object::Array(e) = get(b"Encode") {
        let ranges = f.m.min(e.len() / 2);
        if ranges != f.m {
            diag.warn(
                "function.sample_encode",
                "wrong number of sample function input mappings",
            );
        }
        for (i, pair) in encode.iter_mut().enumerate().take(ranges) {
            *pair = [real(doc, &e, i * 2), real(doc, &e, i * 2 + 1)];
        }
    }
    let mut decode = [[0.0f32; 2]; MAX_N];
    decode[..f.n].copy_from_slice(&f.range[..f.n]);
    if let Object::Array(dd) = get(b"Decode") {
        let ranges = f.n.min(dd.len() / 2);
        if ranges != f.n {
            diag.warn(
                "function.sample_decode",
                "wrong number of sample function output mappings",
            );
        }
        for (i, pair) in decode.iter_mut().enumerate().take(ranges) {
            *pair = [real(doc, &dd, i * 2), real(doc, &dd, i * 2 + 1)];
        }
    }
    let mut count = f.n as i64;
    for &sz in &size[..f.m] {
        if count > MAX_SAMPLE_FUNCTION_SIZE / i64::from(sz) {
            return Err(report::syntax("sample function too large"));
        }
        count *= i64::from(sz);
    }
    if count > MAX_SAMPLE_FUNCTION_SIZE {
        return Err(report::syntax("sample function too large"));
    }
    let data = stream_of(doc, obj)?;
    let mut stm = Stream::memory(&data, diag);
    let mut samples = Vec::new();
    for _ in 0..count {
        if stm.is_eof_bits() {
            return Err(report::syntax("truncated sample function stream"));
        }
        let byte = |stm: &mut Stream| stm.read_byte().map(u32::from);
        let be = |stm: &mut Stream, n: usize, what: &str| -> Result<u32, String> {
            let mut v = 0u32;
            for _ in 0..n {
                let Some(b) = byte(stm) else {
                    return Err(report::format(&format!("premature end of file in {what}")));
                };
                v = (v << 8) | b;
            }
            Ok(v)
        };
        let s = match bps {
            1 => stm.read_bits(1) as f32,
            2 => stm.read_bits(2) as f32 / 3.0,
            4 => stm.read_bits(4) as f32 / 15.0,
            8 => byte(&mut stm).map_or(-1.0, |b| b as f32) / 255.0,
            12 => stm.read_bits(12) as f32 / 4095.0,
            16 => be(&mut stm, 2, "int16")? as f32 / 65535.0,
            24 => be(&mut stm, 3, "int24")? as f32 / 16_777_215.0,
            32 => be(&mut stm, 4, "int32")? as f32 / 4_294_967_295.0,
            other => {
                return Err(report::syntax(&format!(
                    "sample stream bit depth {other} unsupported"
                )));
            }
        };
        samples.push(s);
    }
    Ok(Kind::Sample(Box::new(Sampled {
        size,
        encode,
        decode,
        samples,
    })))
}

fn load_exponential(doc: &Document, f: &mut Function, obj: &Object) -> Kind {
    let diag = doc.diagnostics();
    let get = |key: &[u8]| doc.dict_get(obj, key);
    if f.m > 1 {
        diag.warn(
            "function.exponential_inputs",
            "exponential functions have at most one input",
        );
    }
    f.m = 1;
    let n = get(b"N").to_real();
    if n != (n as i32) as f32 {
        if f.domain[0][0] < 0.0 || f.domain[0][1] < 0.0 {
            diag.warn(
                "function.exponential_domain",
                "exponential function input domain includes illegal negative input values",
            );
        }
    } else if n < 0.0 {
        let [d0, d1] = f.domain[0];
        if d0 == 0.0 || d1 == 0.0 || (d0 < 0.0 && d1 > 0.0) {
            diag.warn(
                "function.exponential_domain",
                "exponential function input domain includes illegal input value zero",
            );
        }
    }
    let mut c0 = [0.0f32; MAX_N];
    let mut c1 = [0.0f32; MAX_N];
    for c in c1.iter_mut().take(f.n) {
        *c = 1.0;
    }
    for (key, target, message) in [
        (
            &b"C0"[..],
            &mut c0,
            "wrong number of C0 constants for exponential function",
        ),
        (
            b"C1",
            &mut c1,
            "wrong number of C1 constants for exponential function",
        ),
    ] {
        if let Object::Array(a) = get(key) {
            let ranges = f.n.min(a.len());
            if ranges != f.n {
                diag.warn("function.exponential_constants", message);
            }
            for (i, t) in target.iter_mut().enumerate().take(ranges) {
                *t = real(doc, &a, i);
            }
        }
    }
    Kind::Exponential(Box::new(Exponential { n, c0, c1 }))
}

fn load_stitching(
    doc: &Document,
    f: &mut Function,
    obj: &Object,
    cycle: &mut Vec<i32>,
) -> Result<Kind, String> {
    let diag = doc.diagnostics();
    let get = |key: &[u8]| doc.dict_get(obj, key);
    if f.m > 1 {
        diag.warn(
            "function.stitching_inputs",
            "stitching functions have at most one input",
        );
    }
    f.m = 1;
    let Object::Array(subs) = get(b"Functions") else {
        return Err(report::syntax("stitching function has no input functions"));
    };
    let k = subs.len();
    if k < 1 {
        return Err(report::syntax("no sub-functions in stitching function"));
    }
    if k > MAX_STITCHING {
        return Err(report::syntax(
            "too many sub-functions in stitching function",
        ));
    }
    let mut funcs = Vec::with_capacity(k);
    for (i, sub) in subs.iter().enumerate() {
        let g = load_imp(doc, sub, 1, f.n, cycle)?;
        if g.m != f.m {
            diag.warn(
                "function.stitching_sub",
                format!("wrong number of inputs for sub function {i}"),
            );
        }
        if g.n != f.n {
            diag.warn(
                "function.stitching_sub",
                format!("wrong number of outputs for sub function {i}"),
            );
        }
        funcs.push(g);
    }
    let Object::Array(b) = get(b"Bounds") else {
        return Err(report::syntax("stitching function has no bounds"));
    };
    if b.len() < k - 1 {
        return Err(report::syntax("too few subfunction boundaries"));
    }
    if b.len() > k {
        diag.warn(
            "function.stitching_bounds",
            "too many subfunction boundaries",
        );
    }
    let mut bounds = Vec::with_capacity(k - 1);
    for i in 0..k - 1 {
        bounds.push(real(doc, &b, i));
        if i > 0 && bounds[i - 1] > bounds[i] {
            return Err(report::syntax(&format!(
                "subfunction {i} boundary out of range"
            )));
        }
    }
    if k > 1 && (f.domain[0][0] > bounds[0] || f.domain[0][1] < bounds[k - 2]) {
        diag.warn(
            "function.stitching_bounds",
            "subfunction boundaries outside of input mapping",
        );
    }
    let mut encode = vec![0.0f32; k * 2];
    if let Object::Array(e) = get(b"Encode") {
        let ranges = k.min(e.len() / 2);
        if ranges != k {
            diag.warn(
                "function.stitching_encode",
                "wrong number of stitching function input mappings",
            );
        }
        for i in 0..ranges {
            encode[i * 2] = real(doc, &e, i * 2);
            encode[i * 2 + 1] = real(doc, &e, i * 2 + 1);
        }
    }
    Ok(Kind::Stitching {
        funcs,
        bounds,
        encode,
    })
}

/// `load_postscript_func` and `parse_code`.
fn load_postscript(doc: &Document, obj: &Object) -> Result<Vec<Ps>, String> {
    let data = stream_of(doc, obj)?;
    let mut lx = Lexer::small(&data, 0, doc.diagnostics());
    if lx.next_token() != Token::OpenBrace {
        return Err(report::syntax("stream is not a calculator function"));
    }
    let mut code = Vec::new();
    parse_code(&mut lx, &mut code, 0)?;
    Ok(code)
}

fn parse_code(lx: &mut Lexer, code: &mut Vec<Ps>, depth: usize) -> Result<(), String> {
    if depth > MAX_PS_DEPTH {
        return Err(report::syntax("too much nesting in calculator function"));
    }
    loop {
        match lx.next_token() {
            Token::Eof => return Err(report::syntax("truncated calculator function")),
            Token::Int(i) => code.push(Ps::Int(i as i32)),
            Token::True => code.push(Ps::Bool(true)),
            Token::False => code.push(Ps::Bool(false)),
            Token::Real(f) => code.push(Ps::Real(f)),
            Token::OpenBrace => {
                // The operator, the else block, the if block and the end,
                // then the blocks.
                let op = code.len();
                code.extend([Ps::Block(0); 4]);
                let if_block = code.len();
                parse_code(lx, code, depth + 1)?;
                let mut tok = lx.next_token();
                let mut else_block = None;
                if tok == Token::OpenBrace {
                    else_block = Some(code.len());
                    parse_code(lx, code, depth + 1)?;
                    tok = lx.next_token();
                }
                let Token::Keyword(word) = tok else {
                    return Err(report::syntax("missing keyword in 'if-else' context"));
                };
                match (&word[..], else_block) {
                    (b"if", None) => {
                        code[op] = Ps::Op(Op::If);
                        code[op + 2] = Ps::Block(if_block);
                        code[op + 3] = Ps::Block(code.len());
                    }
                    (b"if", Some(_)) => {
                        return Err(report::syntax("too many branches for 'if'"));
                    }
                    (b"ifelse", Some(e)) => {
                        code[op] = Ps::Op(Op::Ifelse);
                        code[op + 1] = Ps::Block(e);
                        code[op + 2] = Ps::Block(if_block);
                        code[op + 3] = Ps::Block(code.len());
                    }
                    (b"ifelse", None) => {
                        return Err(report::syntax("not enough branches for 'ifelse'"));
                    }
                    (other, _) => {
                        return Err(report::syntax(&format!(
                            "unknown keyword in 'if-else' context: '{}'",
                            String::from_utf8_lossy(other)
                        )));
                    }
                }
            }
            Token::CloseBrace => {
                code.push(Ps::Op(Op::Return));
                return Ok(());
            }
            Token::Keyword(word) => {
                let Some(&(_, op)) = PS_OPS.iter().find(|(n, _)| *n == &word[..]) else {
                    return Err(report::syntax(&format!(
                        "unknown operator: '{}'",
                        String::from_utf8_lossy(&word)
                    )));
                };
                match op {
                    Op::Ifelse => {
                        return Err(report::syntax(
                            "illegally positioned ifelse operator in function",
                        ));
                    }
                    Op::If => {
                        return Err(report::syntax(
                            "illegally positioned if operator in function",
                        ));
                    }
                    _ => code.push(Ps::Op(op)),
                }
            }
            _ => return Err(report::syntax("calculator function syntax error")),
        }
    }
}

impl Function {
    /// `pdf_eval_function` (`fz_eval_function`): missing inputs read as 0,
    /// outputs past the function's are 0.
    pub fn eval(&self, doc: &Document, input: &[f32], out: &mut [f32]) {
        let mut fake_in = [0.0f32; MAX_M];
        let n = input.len().min(MAX_M);
        fake_in[..n].copy_from_slice(&input[..n]);
        let mut fake_out = [0.0f32; MAX_N];
        self.eval_raw(doc, &fake_in, &mut fake_out);
        let k = out.len().min(self.n);
        out[..k].copy_from_slice(&fake_out[..k]);
        for o in out.iter_mut().skip(self.n) {
            *o = 0.0;
        }
    }

    fn eval_raw(&self, doc: &Document, input: &[f32; MAX_M], out: &mut [f32; MAX_N]) {
        match &self.kind {
            Kind::Sample(sa) => {
                self.eval_sample(input, out, &sa.size, &sa.encode, &sa.decode, &sa.samples)
            }
            Kind::Exponential(e) => {
                let (n, c0, c1) = (&e.n, &e.c0, &e.c1);
                let x = clamp(input[0], self.domain[0][0], self.domain[0][1]);
                if (*n != (*n as i32) as f32 && x < 0.0) || (*n < 0.0 && x == 0.0) {
                    out[..self.n].fill(0.0);
                    return;
                }
                let t = x.powf(*n);
                for i in 0..self.n {
                    out[i] = c0[i] + t * (c1[i] - c0[i]);
                    if self.has_range {
                        out[i] = clamp(out[i], self.range[i][0], self.range[i][1]);
                    }
                }
            }
            Kind::Stitching {
                funcs,
                bounds,
                encode,
            } => {
                let k = funcs.len();
                let x = clamp(input[0], self.domain[0][0], self.domain[0][1]);
                // The first interval whose bound is above x; NaN finds
                // none.
                let mut i = 0;
                while i < k - 1 && x.partial_cmp(&bounds[i]) != Some(std::cmp::Ordering::Less) {
                    i += 1;
                }
                let (low, high) = if i == 0 && k == 1 {
                    (self.domain[0][0], self.domain[0][1])
                } else if i == 0 {
                    (self.domain[0][0], bounds[0])
                } else if i == k - 1 {
                    (bounds[k - 2], self.domain[0][1])
                } else {
                    (bounds[i - 1], bounds[i])
                };
                let x = lerp(x, low, high, encode[i * 2], encode[i * 2 + 1]);
                funcs[i].eval(doc, &[x], &mut out[..self.n]);
            }
            Kind::PostScript(code) => {
                let mut st = PsStack::default();
                for (x, d) in input.iter().zip(&self.domain).take(self.m) {
                    st.push_real(clamp(*x, d[0], d[1]));
                }
                ps_run(doc, code, &mut st, 0);
                for i in (0..self.n).rev() {
                    let x = st.pop_real();
                    out[i] = clamp(x, self.range[i][0], self.range[i][1]);
                }
            }
        }
    }

    fn eval_sample(
        &self,
        input: &[f32; MAX_M],
        out: &mut [f32; MAX_N],
        size: &[i32; MAX_M],
        encode: &[[f32; 2]; MAX_M],
        decode: &[[f32; 2]; MAX_N],
        samples: &[f32],
    ) {
        let (m, n) = (self.m, self.n);
        let mut e0 = [0usize; MAX_M];
        let mut e1 = [0usize; MAX_M];
        let mut efrac = [0.0f32; MAX_M];
        for i in 0..m {
            let x = clamp(input[i], self.domain[i][0], self.domain[i][1]);
            let x = lerp(
                x,
                self.domain[i][0],
                self.domain[i][1],
                encode[i][0],
                encode[i][1],
            );
            let x = clamp(x, 0.0, (size[i] - 1) as f32);
            // C converts NaN to 0 here, as `as` does.
            let lo = x.floor() as i32;
            e0[i] = lo.max(0) as usize;
            e1[i] = (x.ceil() as i32).max(0) as usize;
            efrac[i] = x - lo as f32;
        }
        let at = |idx: usize| samples.get(idx).copied().unwrap_or(0.0);
        let mut scale = [0usize; MAX_M];
        scale[0] = n;
        for i in 1..m {
            scale[i] = scale[i - 1] * size[i - 1] as usize;
        }
        for i in 0..n {
            let v = if m == 1 {
                let a = at(e0[0] * n + i);
                let b = at(e1[0] * n + i);
                a + (b - a) * efrac[0]
            } else if m == 2 {
                let s0 = n;
                let s1 = s0 * size[0] as usize;
                let a = at(e0[0] * s0 + e0[1] * s1 + i);
                let b = at(e1[0] * s0 + e0[1] * s1 + i);
                let c = at(e0[0] * s0 + e1[1] * s1 + i);
                let d = at(e1[0] * s0 + e1[1] * s1 + i);
                let ab = a + (b - a) * efrac[0];
                let cd = c + (d - c) * efrac[0];
                ab + (cd - ab) * efrac[1]
            } else {
                interpolate(&at, &scale, &e0, &e1, &efrac, m - 1, i)
            };
            out[i] = lerp(v, 0.0, 1.0, decode[i][0], decode[i][1]);
            out[i] = clamp(out[i], self.range[i][0], self.range[i][1]);
        }
    }
}

fn interpolate(
    at: &dyn Fn(usize) -> f32,
    scale: &[usize; MAX_M],
    e0: &[usize; MAX_M],
    e1: &[usize; MAX_M],
    efrac: &[f32; MAX_M],
    dim: usize,
    idx: usize,
) -> f32 {
    let idx0 = e0[dim] * scale[dim] + idx;
    let idx1 = e1[dim] * scale[dim] + idx;
    let (a, b) = if dim == 0 {
        (at(idx0), at(idx1))
    } else {
        (
            interpolate(at, scale, e0, e1, efrac, dim - 1, idx0),
            interpolate(at, scale, e0, e1, efrac, dim - 1, idx1),
        )
    };
    a + (b - a) * efrac[dim]
}

/// `ps_stack`: overflow and underflow are ignored.
struct PsStack {
    stack: [Ps; PS_STACK],
    sp: usize,
}

impl Default for PsStack {
    /// Zeroed, as `ps_init_stack` leaves it: false booleans.
    fn default() -> Self {
        PsStack {
            stack: [Ps::Bool(false); PS_STACK],
            sp: 0,
        }
    }
}

impl PsStack {
    fn overflow(&self, n: i32) -> bool {
        n < 0 || self.sp as i64 + i64::from(n) >= PS_STACK as i64
    }

    fn underflow(&self, n: i32) -> bool {
        n < 0 || n as usize > self.sp
    }

    fn top_is(&self, f: fn(&Ps) -> bool) -> bool {
        !self.underflow(1) && f(&self.stack[self.sp - 1])
    }

    fn top2_are(&self, f: fn(&Ps) -> bool) -> bool {
        !self.underflow(2) && f(&self.stack[self.sp - 1]) && f(&self.stack[self.sp - 2])
    }

    fn push(&mut self, v: Ps) {
        if !self.overflow(1) {
            self.stack[self.sp] = v;
            self.sp += 1;
        }
    }

    fn push_bool(&mut self, b: bool) {
        self.push(Ps::Bool(b));
    }

    fn push_int(&mut self, i: i32) {
        self.push(Ps::Int(i));
    }

    fn push_real(&mut self, f: f32) {
        // NaN becomes 1, a small value that cannot cause a division by 0.
        let f = if f.is_nan() { 1.0 } else { f };
        self.push(Ps::Real(clamp(f, -f32::MAX, f32::MAX)));
    }

    fn pop_bool(&mut self) -> bool {
        if let Some(Ps::Bool(b)) = self.peek() {
            self.sp -= 1;
            return b;
        }
        false
    }

    fn pop_int(&mut self) -> i32 {
        match self.peek() {
            Some(Ps::Int(i)) => {
                self.sp -= 1;
                i
            }
            Some(Ps::Real(f)) => {
                self.sp -= 1;
                f as i32
            }
            _ => 0,
        }
    }

    fn pop_real(&mut self) -> f32 {
        match self.peek() {
            Some(Ps::Int(i)) => {
                self.sp -= 1;
                i as f32
            }
            Some(Ps::Real(f)) => {
                self.sp -= 1;
                f
            }
            _ => 0.0,
        }
    }

    fn peek(&self) -> Option<Ps> {
        (self.sp > 0).then(|| self.stack[self.sp - 1])
    }

    fn copy(&mut self, n: i32) {
        if !self.underflow(n) && !self.overflow(n) {
            let n = n as usize;
            for i in 0..n {
                self.stack[self.sp + i] = self.stack[self.sp - n + i];
            }
            self.sp += n;
        }
    }

    fn roll(&mut self, n: i32, j: i32) {
        if self.underflow(n) || j == 0 || n == 0 {
            return;
        }
        // C's arithmetic, INT_MIN included: a shift past n moves nothing.
        let j = if j >= 0 {
            j % n
        } else {
            let j = j.wrapping_neg() % n;
            if j != 0 { n - j } else { 0 }
        };
        if j < 0 || j > n {
            return;
        }
        let (n, j) = (n as usize, j as usize);
        let window = &mut self.stack[self.sp - n..self.sp];
        if j * 2 > n {
            window.rotate_left(n - j);
        } else {
            window.rotate_right(j);
        }
    }

    fn index(&mut self, n: i32) {
        if !self.overflow(1) && !self.underflow(n.wrapping_add(1)) {
            // n may be -1, which copies the slot above the top.
            let from = self.sp as i64 - i64::from(n) - 1;
            self.stack[self.sp] = self.stack[from as usize];
            self.sp += 1;
        }
    }
}

fn is_int(p: &Ps) -> bool {
    matches!(p, Ps::Int(_))
}

fn is_bool(p: &Ps) -> bool {
    matches!(p, Ps::Bool(_))
}

/// `DIV_BY_ZERO`.
fn div_by_zero<T>(a_neg: bool, b_neg: bool, min: T, max: T) -> T {
    if a_neg ^ b_neg { min } else { max }
}

/// `ps_run`.
fn ps_run(doc: &Document, code: &[Ps], st: &mut PsStack, mut pc: usize) {
    let diag = doc.diagnostics();
    loop {
        let Some(&item) = code.get(pc) else {
            return;
        };
        pc += 1;
        let op = match item {
            Ps::Int(i) => {
                st.push_int(i);
                continue;
            }
            Ps::Real(f) => {
                st.push_real(f);
                continue;
            }
            Ps::Op(op) => op,
            Ps::Bool(_) | Ps::Block(_) => {
                diag.warn(
                    "function.ps_foreign",
                    "foreign object in calculator function",
                );
                return;
            }
        };
        let block = |at: usize| match code.get(at) {
            Some(Ps::Block(b)) => *b,
            _ => usize::MAX,
        };
        match op {
            Op::Abs => {
                if st.top_is(is_int) {
                    let i = st.pop_int();
                    st.push_int(if i < 0 { i.wrapping_neg() } else { i });
                } else {
                    let r = st.pop_real();
                    st.push_real(if r < 0.0 { -r } else { r });
                }
            }
            Op::Add | Op::Sub | Op::Mul => {
                if st.top2_are(is_int) {
                    let i2 = st.pop_int();
                    let i1 = st.pop_int();
                    st.push_int(match op {
                        Op::Add => i1.wrapping_add(i2),
                        Op::Sub => i1.wrapping_sub(i2),
                        _ => i1.wrapping_mul(i2),
                    });
                } else {
                    let r2 = st.pop_real();
                    let r1 = st.pop_real();
                    st.push_real(match op {
                        Op::Add => r1 + r2,
                        Op::Sub => r1 - r2,
                        _ => r1 * r2,
                    });
                }
            }
            Op::And | Op::Or | Op::Xor => {
                let ints = if op == Op::And {
                    st.top2_are(is_int)
                } else {
                    !st.top2_are(is_bool)
                };
                if ints {
                    let i2 = st.pop_int();
                    let i1 = st.pop_int();
                    st.push_int(match op {
                        Op::And => i1 & i2,
                        Op::Or => i1 | i2,
                        _ => i1 ^ i2,
                    });
                } else {
                    let b2 = st.pop_bool();
                    let b1 = st.pop_bool();
                    st.push_bool(match op {
                        Op::And => b1 && b2,
                        Op::Or => b1 || b2,
                        _ => b1 ^ b2,
                    });
                }
            }
            Op::Atan => {
                let r2 = st.pop_real();
                let r1 = st.pop_real();
                let mut a = r1.atan2(r2) * FZ_RADIAN;
                if a < 0.0 {
                    a += 360.0;
                }
                st.push_real(a);
            }
            Op::Bitshift => {
                let i2 = st.pop_int();
                let i1 = st.pop_int();
                if i2 > 0 && i2 < 32 {
                    st.push_int(((i1 as u32) << i2) as i32);
                } else if i2 < 0 && i2 > -32 {
                    st.push_int(((i1 as u32) >> -i2) as i32);
                } else {
                    st.push_int(i1);
                }
            }
            Op::Ceiling => {
                let r = st.pop_real();
                st.push_real(r.ceil());
            }
            Op::Copy => {
                let n = st.pop_int();
                st.copy(n);
            }
            Op::Cos => {
                let r = st.pop_real();
                st.push_real((r / FZ_RADIAN).cos());
            }
            Op::Cvi => {
                let i = st.pop_int();
                st.push_int(i);
            }
            Op::Cvr => {
                let r = st.pop_real();
                st.push_real(r);
            }
            Op::Div => {
                let r2 = st.pop_real();
                let r1 = st.pop_real();
                if r2.abs() >= FLT_EPSILON {
                    st.push_real(r1 / r2);
                } else {
                    st.push_real(div_by_zero(r1 < 0.0, r2 < 0.0, -f32::MAX, f32::MAX));
                }
            }
            Op::Dup => st.copy(1),
            Op::Eq | Op::Ne => {
                let eq = if st.top2_are(is_bool) {
                    let b2 = st.pop_bool();
                    st.pop_bool() == b2
                } else if st.top2_are(is_int) {
                    let i2 = st.pop_int();
                    st.pop_int() == i2
                } else {
                    let r2 = st.pop_real();
                    st.pop_real() == r2
                };
                st.push_bool(if op == Op::Eq { eq } else { !eq });
            }
            Op::Exch => st.roll(2, 1),
            Op::Exp => {
                let r2 = st.pop_real();
                let r1 = st.pop_real();
                st.push_real(r1.powf(r2));
            }
            Op::False => st.push_bool(false),
            Op::Floor => {
                let r = st.pop_real();
                st.push_real(r.floor());
            }
            Op::Ge | Op::Gt | Op::Le | Op::Lt => {
                let (a, b) = if st.top2_are(is_int) {
                    let i2 = st.pop_int();
                    let i1 = st.pop_int();
                    (f64::from(i1), f64::from(i2))
                } else {
                    let r2 = st.pop_real();
                    let r1 = st.pop_real();
                    (f64::from(r1), f64::from(r2))
                };
                st.push_bool(match op {
                    Op::Ge => a >= b,
                    Op::Gt => a > b,
                    Op::Le => a <= b,
                    _ => a < b,
                });
            }
            Op::Idiv => {
                let i2 = st.pop_int();
                let i1 = st.pop_int();
                st.push_int(if i2 == 0 {
                    div_by_zero(i1 < 0, i2 < 0, i32::MIN, i32::MAX)
                } else if i1 == i32::MIN && i2 == -1 {
                    i32::MAX
                } else {
                    i1 / i2
                });
            }
            Op::Index => {
                let n = st.pop_int();
                st.index(n);
            }
            Op::Ln => {
                let r = st.pop_real();
                st.push_real(r.ln());
            }
            Op::Log => {
                let r = st.pop_real();
                st.push_real(r.log10());
            }
            Op::Mod => {
                let i2 = st.pop_int();
                let i1 = st.pop_int();
                st.push_int(if i2 == 0 {
                    div_by_zero(i1 < 0, i2 < 0, i32::MIN, i32::MAX)
                } else if i1 == i32::MIN && i2 == -1 {
                    0
                } else {
                    i1 % i2
                });
            }
            Op::Neg => {
                if st.top_is(is_int) {
                    let i = st.pop_int();
                    st.push_int(i.wrapping_neg());
                } else {
                    let r = st.pop_real();
                    st.push_real(-r);
                }
            }
            Op::Not => {
                if st.top_is(is_bool) {
                    let b = st.pop_bool();
                    st.push_bool(!b);
                } else {
                    let i = st.pop_int();
                    st.push_int(!i);
                }
            }
            Op::Pop => {
                if !st.underflow(1) {
                    st.sp -= 1;
                }
            }
            Op::Roll => {
                let i2 = st.pop_int();
                let i1 = st.pop_int();
                st.roll(i1, i2);
            }
            Op::Round => {
                if !st.top_is(is_int) {
                    let r = st.pop_real();
                    st.push_real(if r >= 0.0 {
                        (r + 0.5).floor()
                    } else {
                        (r - 0.5).ceil()
                    });
                }
            }
            Op::Sin => {
                let r = st.pop_real();
                st.push_real((r / FZ_RADIAN).sin());
            }
            Op::Sqrt => {
                let r = st.pop_real();
                st.push_real(r.sqrt());
            }
            Op::True => st.push_bool(true),
            Op::Truncate => {
                if !st.top_is(is_int) {
                    let r = st.pop_real();
                    st.push_real(if r >= 0.0 { r.floor() } else { r.ceil() });
                }
            }
            Op::If => {
                if st.pop_bool() {
                    ps_run(doc, code, st, block(pc + 1));
                }
                pc = block(pc + 2);
            }
            Op::Ifelse => {
                if st.pop_bool() {
                    ps_run(doc, code, st, block(pc + 1));
                } else {
                    ps_run(doc, code, st, block(pc));
                }
                pc = block(pc + 2);
            }
            Op::Return => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A document holding `body` as object 2.
    fn doc_with(body: &[u8]) -> Document {
        let mut file = b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n2 0 obj".to_vec();
        file.extend_from_slice(body);
        file.extend_from_slice(b"endobj\ntrailer<</Root 1 0 R/Size 3>>\n");
        Document::open(file).unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn postscript(code: &str, domain: &str, range: &str) -> (Document, Object) {
        let body = format!(
            "<</FunctionType 4/Domain[{domain}]/Range[{range}]/Length {}>>stream\n{code}\nendstream",
            code.len()
        );
        (
            doc_with(body.as_bytes()),
            Object::Ref(crate::ObjRef::new(2, 0)),
        )
    }

    fn eval1(doc: &Document, f: &Function, x: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0; f.n];
        f.eval(doc, x, &mut out);
        out
    }

    #[test]
    fn calculator_functions_run_with_mupdfs_guards() {
        let (doc, obj) = postscript("{ 2 mul 1 add }", "0 1", "0 10");
        let f = load(&doc, &obj, 1, 1).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(eval1(&doc, &f, &[0.25]), [1.5]);
        // Inputs are clamped to the domain, outputs to the range.
        assert_eq!(eval1(&doc, &f, &[9.0]), [3.0]);
        // Division by zero gives the largest float of the sign, clamped to
        // the range.
        let (doc, obj) = postscript("{ 0 div }", "-1 1", "-1000 1000");
        let f = load(&doc, &obj, 1, 1).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(eval1(&doc, &f, &[-0.5]), [-1000.0]);
        let (doc, obj) = postscript(
            "{ dup 0.5 gt { pop 1 } { 2 exch sub } ifelse }",
            "0 1",
            "0 2",
        );
        let f = load(&doc, &obj, 1, 1).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(eval1(&doc, &f, &[0.75]), [1.0]);
        assert_eq!(eval1(&doc, &f, &[0.25]), [1.75]);
        // Integer arithmetic wraps; three outputs from one input.
        let (doc, obj) = postscript(
            "{ pop 2147483647 1 add 7 3 mod -7 2 idiv }",
            "0 1",
            "-3000000000 3000000000 -9 9 -9 9",
        );
        let f = load(&doc, &obj, 1, 3).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(eval1(&doc, &f, &[0.0]), [-2_147_483_648.0, 1.0, -3.0]);
    }

    #[test]
    fn calculator_syntax_errors_are_mupdfs() {
        let err = |code: &str| {
            let (doc, obj) = postscript(code, "0 1", "0 1");
            load(&doc, &obj, 1, 1).err()
        };
        assert_eq!(
            err("1 add }").as_deref(),
            Some("syntax error: stream is not a calculator function")
        );
        assert_eq!(
            err("{ 1 frob }").as_deref(),
            Some("syntax error: unknown operator: 'frob'")
        );
        assert_eq!(
            err("{ { 1 } { 2 } if }").as_deref(),
            Some("syntax error: too many branches for 'if'")
        );
        assert_eq!(
            err("{ 1 if }").as_deref(),
            Some("syntax error: illegally positioned if operator in function")
        );
        assert_eq!(
            err("{ 1 add").as_deref(),
            Some("syntax error: truncated calculator function")
        );
    }

    #[test]
    fn sampled_exponential_and_stitching_functions() {
        // Two samples, 8 bits: 0 and 255, so a ramp.
        let doc = doc_with(
            b"<</FunctionType 0/Domain[0 1]/Range[0 1]/Size[2]/BitsPerSample 8/Length 2>>stream\n\x00\xff\nendstream",
        );
        let f = load(&doc, &Object::Ref(crate::ObjRef::new(2, 0)), 1, 1)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(eval1(&doc, &f, &[0.5]), [0.5]);
        let doc = doc_with(b"<</FunctionType 2/Domain[0 1]/C0[0 1]/C1[1 0]/N 2>>");
        let f = load(&doc, &Object::Ref(crate::ObjRef::new(2, 0)), 1, 2)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(eval1(&doc, &f, &[0.5]), [0.25, 0.75]);
        let doc = doc_with(
            b"<</FunctionType 3/Domain[0 2]/Functions[<</FunctionType 2/Domain[0 1]/N 1>> <</FunctionType 2/Domain[0 1]/C0[1]/C1[0]/N 1>>]/Bounds[1]/Encode[0 1 0 1]>>",
        );
        let f = load(&doc, &Object::Ref(crate::ObjRef::new(2, 0)), 1, 1)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(eval1(&doc, &f, &[0.5]), [0.5]);
        assert_eq!(eval1(&doc, &f, &[1.5]), [0.5]);
        assert_eq!(eval1(&doc, &f, &[1.75]), [0.25]);
        // Failures.
        let doc = doc_with(b"<</FunctionType 3/Domain[0 1]/Functions[2 0 R]/Bounds[]>>");
        assert_eq!(
            load(&doc, &Object::Ref(crate::ObjRef::new(2, 0)), 1, 1)
                .err()
                .as_deref(),
            Some("syntax error: recursive function")
        );
        let doc = doc_with(b"<</FunctionType 5>>");
        assert_eq!(
            load(&doc, &Object::Ref(crate::ObjRef::new(2, 0)), 1, 1)
                .err()
                .as_deref(),
            Some("syntax error: unknown function type (2 0 R)")
        );
        let doc = doc_with(b"<</FunctionType 0/Domain[0 1]/Range[0 1]/Size[4]/BitsPerSample 8/Length 2>>stream\n\x00\xff\nendstream");
        assert_eq!(
            load(&doc, &Object::Ref(crate::ObjRef::new(2, 0)), 1, 1)
                .err()
                .as_deref(),
            Some("syntax error: truncated sample function stream")
        );
    }
}
