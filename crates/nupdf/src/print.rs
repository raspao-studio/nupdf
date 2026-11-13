// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-object.c (pdf_print_obj, fmt_obj),
// source/fitz/printf.c (fmtfloat) and source/fitz/ftoa.c (fz_grisu),
// Copyright (C) 2004-2026 Artifex Software, Inc. fz_grisu is in turn
// derived from Florian Loitsch's Grisu2, Copyright (c) 2009 Florian Loitsch,
// MIT license.

//! PDF syntax for objects, as MuPDF writes it (`pdf_print_obj`). "Tight"
//! output has no optional whitespace and writes a dictionary's /Type and
//! /Subtype first; otherwise dictionaries get a line per entry. Strings are
//! literal or hex, whichever MuPDF would choose, and hex whenever they are
//! encrypted. Reals use MuPDF's shortest round-trip digits.

use crate::object::{Dict, Object};

/// How to print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// No optional whitespace (`do_tight`, the default when writing).
    pub tight: bool,
    /// Hex for strings with bytes outside printable ASCII.
    pub ascii: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            tight: true,
            ascii: false,
        }
    }
}

/// `struct fmt`.
struct Fmt<'a> {
    out: &'a mut Vec<u8>,
    indent: usize,
    tight: bool,
    ascii: bool,
    col: usize,
    sep: bool,
    last: u8,
    /// Encrypts a string (the object's key is already chosen).
    encrypt: Option<Encrypt<'a>>,
}

/// Encrypts a string or stream of the object being written.
pub type Encrypt<'a> = &'a dyn Fn(&[u8]) -> Vec<u8>;

fn is_white(c: u8) -> bool {
    matches!(c, 0 | 9 | 10 | 12 | 13 | 32)
}

fn is_delim(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn hex_digit(c: u8) -> u8 {
    if c < 10 { b'0' + c } else { b'A' + c - 10 }
}

impl Fmt<'_> {
    /// `fmt_putc`: a pending separator becomes a space only between two
    /// regular characters.
    fn putc(&mut self, c: u8) {
        if self.sep && !is_delim(self.last) && !is_white(self.last) && !is_delim(c) && !is_white(c)
        {
            self.sep = false;
            self.putc(b' ');
        }
        self.sep = false;
        self.out.push(c);
        if c == b'\n' {
            self.col = 0;
        } else {
            self.col += 1;
        }
        self.last = c;
    }

    fn puts(&mut self, s: &[u8]) {
        let Some((&first, rest)) = s.split_first() else {
            return;
        };
        self.putc(first);
        self.more(rest);
    }

    /// The rest of a token [`Fmt::putc`] began: no separator is pending
    /// after a character, so each is `putc`'s plain append.
    fn more(&mut self, s: &[u8]) {
        let Some(&last) = s.last() else {
            return;
        };
        self.out.extend_from_slice(s);
        self.col = match s.iter().rposition(|&c| c == b'\n') {
            Some(i) => s.len() - 1 - i,
            None => self.col + s.len(),
        };
        self.last = last;
    }

    /// An integer, as `%d` writes it.
    fn int(&mut self, v: i64) {
        let mut buf = [0u8; 20];
        let mut at = buf.len();
        let mut n = v.unsigned_abs();
        loop {
            at -= 1;
            buf[at] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        if v < 0 {
            self.putc(b'-');
            self.more(&buf[at..]);
        } else {
            self.puts(&buf[at..]);
        }
    }

    fn put_indent(&mut self) {
        for _ in 0..self.indent {
            self.puts(b"  ");
        }
    }

    /// A string's bytes, after its `(`.
    fn str_out(&mut self, s: &[u8]) {
        let mut out = Vec::with_capacity(s.len());
        for &c in s {
            match c {
                b'\n' => out.extend_from_slice(b"\\n"),
                b'\r' => out.extend_from_slice(b"\\r"),
                b'\t' => out.extend_from_slice(b"\\t"),
                8 => out.extend_from_slice(b"\\b"),
                12 => out.extend_from_slice(b"\\f"),
                b'(' => out.extend_from_slice(b"\\("),
                b')' => out.extend_from_slice(b"\\)"),
                b'\\' => out.extend_from_slice(b"\\\\"),
                c if !(32..127).contains(&c) => out.extend_from_slice(&[
                    b'\\',
                    b'0' + ((c / 64) & 7),
                    b'0' + ((c / 8) & 7),
                    b'0' + (c & 7),
                ]),
                c => out.push(c),
            }
        }
        self.more(&out);
    }

    /// A string's bytes in hex, after its `<`.
    fn hex_out(&mut self, s: &[u8]) {
        let out: Vec<u8> = s
            .iter()
            .flat_map(|&b| [hex_digit(b >> 4), hex_digit(b & 15)])
            .collect();
        self.more(&out);
    }

    fn string(&mut self, s: &[u8]) {
        let encrypted = self.encrypt.map(|e| e(s));
        let hex = self.encrypt.is_some()
            || (self.ascii && is_binary_string(s))
            || s.starts_with(&[0xff, 0xfe])
            || s.starts_with(&[0xfe, 0xff])
            || is_longer_than_hex(s);
        let data = encrypted.as_deref().unwrap_or(s);
        if hex {
            self.putc(b'<');
            self.hex_out(data);
            self.putc(b'>');
        } else {
            self.putc(b'(');
            self.str_out(data);
            self.putc(b')');
        }
    }

    fn name(&mut self, n: &[u8]) {
        self.putc(b'/');
        // MuPDF's names are C strings.
        let n = n.split(|&c| c == 0).next().unwrap_or_default();
        if n.iter()
            .all(|&c| !(is_delim(c) || is_white(c) || c == b'#' || !(32..127).contains(&c)))
        {
            self.more(n);
        } else {
            let mut out = Vec::with_capacity(n.len() + 8);
            for &c in n {
                if is_delim(c) || is_white(c) || c == b'#' || !(32..127).contains(&c) {
                    out.extend_from_slice(&[b'#', hex_digit(c >> 4), hex_digit(c & 15)]);
                } else {
                    out.push(c);
                }
            }
            self.more(&out);
        }
        self.sep = true;
    }

    fn array(&mut self, items: &[Object]) {
        if self.tight {
            self.putc(b'[');
            for item in items {
                self.obj(item);
            }
            self.putc(b']');
        } else {
            self.putc(b'[');
            self.indent += 1;
            for item in items {
                if self.col > 60 {
                    self.putc(b'\n');
                    self.put_indent();
                } else {
                    self.putc(b' ');
                }
                self.obj(item);
            }
            self.indent -= 1;
            self.putc(b' ');
            self.putc(b']');
            self.sep = true;
        }
    }

    /// A value, without encryption for a signature's /Contents.
    fn value(&mut self, dict: &Dict, key: &[u8], val: &Object) {
        if key == b"Contents" && is_signature(dict) {
            let saved = self.encrypt.take();
            self.obj(val);
            self.encrypt = saved;
        } else {
            self.obj(val);
        }
    }

    fn dict(&mut self, dict: &Dict) {
        if self.tight {
            self.puts(b"<<");
            let mut skip_type = false;
            let mut skip_subtype = false;
            if let Some(ty) = get(dict, b"Type") {
                self.name(b"Type");
                self.obj(ty);
                if let Some(sub) = get(dict, b"Subtype") {
                    self.name(b"Subtype");
                    self.obj(sub);
                    skip_subtype = true;
                }
                skip_type = true;
            }
            for (key, val) in dict.iter() {
                if (skip_type && key == b"Type") || (skip_subtype && key == b"Subtype") {
                    continue;
                }
                self.name(key);
                self.value(dict, key, val);
            }
            self.puts(b">>");
        } else {
            self.puts(b"<<\n");
            self.indent += 1;
            for (key, val) in dict.iter() {
                self.put_indent();
                self.name(key);
                self.putc(b' ');
                let array = matches!(val, Object::Array(_));
                if array {
                    self.indent += 1;
                }
                self.value(dict, key, val);
                self.putc(b'\n');
                if array {
                    self.indent -= 1;
                }
            }
            self.indent -= 1;
            self.put_indent();
            self.puts(b">>");
        }
    }

    /// `fmt_obj`.
    fn obj(&mut self, obj: &Object) {
        match obj {
            Object::Null => {
                self.puts(b"null");
                self.sep = true;
            }
            Object::Bool(b) => {
                self.puts(if *b { b"true" } else { b"false" });
                self.sep = true;
            }
            Object::Ref(r) => {
                self.int(i64::from(r.num));
                self.putc(b' ');
                self.int(i64::from(r.generation));
                self.puts(b" R");
                self.sep = true;
            }
            Object::Int(i) => {
                self.int(*i);
                self.sep = true;
            }
            Object::Real(f) => {
                let mut s = Vec::new();
                format_real(*f, &mut s);
                self.puts(&s);
                self.sep = true;
            }
            Object::String(s) => self.string(s),
            Object::Name(n) => self.name(n),
            Object::Array(items) => self.array(items),
            Object::Dict(d) => self.dict(d),
            Object::Stream(s) => self.dict(&s.dict),
        }
    }
}

/// `pdf_dict_get`, where a null value is absent: MuPDF's null object is
/// the null pointer, so its `if (obj)` checks skip it.
fn get<'a>(d: &'a Dict, key: &[u8]) -> Option<&'a Object> {
    d.get(key).filter(|v| !matches!(v, Object::Null))
}

/// A signature dictionary, whose /Contents is never encrypted.
fn is_signature(d: &Dict) -> bool {
    get(d, b"Type").is_some_and(|t| t.is_name(b"Sig"))
        && get(d, b"Contents").is_some()
        && get(d, b"ByteRange").is_some()
        && get(d, b"Filter").is_some()
}

fn is_binary_string(s: &[u8]) -> bool {
    s.iter()
        .any(|&c| c > 126 || (c < 32 && !matches!(c, b'\t' | b'\n' | b'\r')))
}

/// Whether escaping would take more than hex's two characters a byte.
fn is_longer_than_hex(s: &[u8]) -> bool {
    let m: usize = s
        .iter()
        .map(|&c| match c {
            c if c > 126 => 4,
            0 => 4,
            b'\n' | b'\r' | b'\t' | 8 | 12 | b'(' | b')' | b'\\' => 2,
            c if c < 32 => 4,
            _ => 1,
        })
        .sum();
    m > s.len() * 2
}

/// `pdf_print_obj`, appending to `out`.
pub fn print(obj: &Object, options: Options, out: &mut Vec<u8>) {
    print_encrypted(obj, options, None, out);
}

/// `pdf_print_encrypted_obj`: strings go through `encrypt`.
pub fn print_encrypted(
    obj: &Object,
    options: Options,
    encrypt: Option<Encrypt>,
    out: &mut Vec<u8>,
) {
    let mut fmt = Fmt {
        out,
        indent: 0,
        tight: options.tight,
        ascii: options.ascii,
        col: 0,
        sep: false,
        last: 0,
        encrypt,
    };
    fmt.obj(obj);
}

/// `pdf_print_encrypted_obj` with a separator, tight and unencrypted, as
/// the content stream writer calls it: returns whether a separator is
/// pending after the object. A pending separator going in never shows,
/// since nothing has been written before it.
pub fn print_sep(obj: &Object, ascii: bool, out: &mut Vec<u8>) -> bool {
    let mut fmt = Fmt {
        out,
        indent: 0,
        tight: true,
        ascii,
        col: 0,
        sep: false,
        last: 0,
        encrypt: None,
    };
    fmt.obj(obj);
    fmt.sep
}

/// The object as a string, for tests and diagnostics.
pub fn to_string(obj: &Object, options: Options) -> String {
    let mut out = Vec::new();
    print(obj, options, &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// How `fmt_obj` writes a real: as an integer when `(int)f` converts back
/// to it, otherwise as `fmtfloat` does. C leaves the conversion undefined
/// outside the int range. x86 gives INT_MIN there, which is what nupdf
/// does; ARM saturates, so MuPDF built for ARM writes 2^31 as 2147483647.
fn format_real(f: f32, out: &mut Vec<u8>) {
    let i = if (-2147483648.0..2147483648.0).contains(&f) {
        f as i32
    } else {
        i32::MIN
    };
    if f == i as f32 {
        out.extend_from_slice(i.to_string().as_bytes());
    } else {
        fmtfloat(f, out);
    }
}

/// `fmtfloat`: shortest digits, never an exponent, no leading zero.
pub(crate) fn fmtfloat(f: f32, out: &mut Vec<u8>) {
    let f = if f.is_nan() {
        0.0
    } else if f.is_infinite() {
        if f < 0.0 { -f32::MAX } else { f32::MAX }
    } else {
        f
    };
    if f.is_sign_negative() {
        out.push(b'-');
    }
    if f == 0.0 {
        out.push(b'0');
        return;
    }
    let (digits, exp) = grisu(f.abs());
    let mut point = exp + digits.len() as i32;
    if point <= 0 {
        out.push(b'.');
        while point < 0 {
            out.push(b'0');
            point += 1;
        }
        out.extend_from_slice(&digits);
    } else {
        let mut left = digits.len();
        for &d in &digits {
            out.push(d);
            left -= 1;
            point -= 1;
            if point == 0 && left > 0 {
                out.push(b'.');
            }
        }
        while point > 0 {
            out.push(b'0');
            point -= 1;
        }
    }
}

// Grisu2 for single precision (fz_grisu).

#[derive(Clone, Copy)]
struct DiyFp {
    f: u64,
    e: i32,
}

const SP_SIGNIFICAND_SIZE: i32 = 23;
const SP_EXPONENT_BIAS: i32 = 127 + SP_SIGNIFICAND_SIZE;
const SP_MIN_EXPONENT: i32 = -SP_EXPONENT_BIAS;
const SP_HIDDEN_BIT: u64 = 0x80_0000;

fn multiply(x: DiyFp, y: DiyFp) -> DiyFp {
    let mask = (1u64 << 32) - 1;
    let (a, b) = (x.f >> 32, x.f & mask);
    let (c, d) = (y.f >> 32, y.f & mask);
    let (ac, bc, ad, bd) = (a * c, b * c, a * d, b * d);
    let mut tmp = (bd >> 32) + (ad & mask) + (bc & mask);
    tmp += 1 << 31;
    DiyFp {
        f: ac + (ad >> 32) + (bc >> 32) + (tmp >> 32),
        e: x.e + y.e + 64,
    }
}

fn float_to_diy_fp(v: f32) -> DiyFp {
    let bits = v.to_bits();
    let biased_e = ((bits & 0x7f80_0000) >> SP_SIGNIFICAND_SIZE) as i32;
    let significand = u64::from(bits & 0x7f_ffff);
    if biased_e != 0 {
        DiyFp {
            f: significand + SP_HIDDEN_BIT,
            e: biased_e - SP_EXPONENT_BIAS,
        }
    } else {
        DiyFp {
            f: significand,
            e: SP_MIN_EXPONENT + 1,
        }
    }
}

fn normalize_boundary(mut v: DiyFp) -> DiyFp {
    while v.f & (SP_HIDDEN_BIT << 1) == 0 {
        v.f <<= 1;
        v.e -= 1;
    }
    let shift = 64 - SP_SIGNIFICAND_SIZE - 2;
    v.f <<= shift;
    v.e -= shift;
    v
}

fn normalized_boundaries(f: f32) -> (DiyFp, DiyFp) {
    let v = float_to_diy_fp(f);
    let significand_is_zero = v.f == SP_HIDDEN_BIT;
    let mut upper = normalize_boundary(DiyFp {
        f: (v.f << 1) + 1,
        e: v.e - 1,
    });
    let mut lower = if significand_is_zero {
        DiyFp {
            f: (v.f << 2) - 1,
            e: v.e - 2,
        }
    } else {
        DiyFp {
            f: (v.f << 1) - 1,
            e: v.e - 1,
        }
    };
    lower.f <<= lower.e - upper.e;
    lower.e = upper.e;
    // Double boundaries, so `(float) strtod` reads the digits back too.
    upper.f -= 1 << 10;
    lower.f += 1 << 10;
    (lower, upper)
}

fn k_comp(n: i32) -> i32 {
    let tmp = n + 64 - 1;
    let k = (tmp * 1233) / (1 << 12);
    if tmp > 0 { k + 1 } else { k }
}

/// Cached powers of ten 10^-37 to 10^46: significands and exponents.
const POWERS_TEN: [u64; 84] = [
    0x881cea14545c7575,
    0xaa242499697392d3,
    0xd4ad2dbfc3d07788,
    0x84ec3c97da624ab5,
    0xa6274bbdd0fadd62,
    0xcfb11ead453994ba,
    0x81ceb32c4b43fcf5,
    0xa2425ff75e14fc32,
    0xcad2f7f5359a3b3e,
    0xfd87b5f28300ca0e,
    0x9e74d1b791e07e48,
    0xc612062576589ddb,
    0xf79687aed3eec551,
    0x9abe14cd44753b53,
    0xc16d9a0095928a27,
    0xf1c90080baf72cb1,
    0x971da05074da7bef,
    0xbce5086492111aeb,
    0xec1e4a7db69561a5,
    0x9392ee8e921d5d07,
    0xb877aa3236a4b449,
    0xe69594bec44de15b,
    0x901d7cf73ab0acd9,
    0xb424dc35095cd80f,
    0xe12e13424bb40e13,
    0x8cbccc096f5088cc,
    0xafebff0bcb24aaff,
    0xdbe6fecebdedd5bf,
    0x89705f4136b4a597,
    0xabcc77118461cefd,
    0xd6bf94d5e57a42bc,
    0x8637bd05af6c69b6,
    0xa7c5ac471b478423,
    0xd1b71758e219652c,
    0x83126e978d4fdf3b,
    0xa3d70a3d70a3d70a,
    0xcccccccccccccccd,
    0x8000000000000000,
    0xa000000000000000,
    0xc800000000000000,
    0xfa00000000000000,
    0x9c40000000000000,
    0xc350000000000000,
    0xf424000000000000,
    0x9896800000000000,
    0xbebc200000000000,
    0xee6b280000000000,
    0x9502f90000000000,
    0xba43b74000000000,
    0xe8d4a51000000000,
    0x9184e72a00000000,
    0xb5e620f480000000,
    0xe35fa931a0000000,
    0x8e1bc9bf04000000,
    0xb1a2bc2ec5000000,
    0xde0b6b3a76400000,
    0x8ac7230489e80000,
    0xad78ebc5ac620000,
    0xd8d726b7177a8000,
    0x878678326eac9000,
    0xa968163f0a57b400,
    0xd3c21bcecceda100,
    0x84595161401484a0,
    0xa56fa5b99019a5c8,
    0xcecb8f27f4200f3a,
    0x813f3978f8940984,
    0xa18f07d736b90be5,
    0xc9f2c9cd04674edf,
    0xfc6f7c4045812296,
    0x9dc5ada82b70b59e,
    0xc5371912364ce305,
    0xf684df56c3e01bc7,
    0x9a130b963a6c115c,
    0xc097ce7bc90715b3,
    0xf0bdc21abb48db20,
    0x96769950b50d88f4,
    0xbc143fa4e250eb31,
    0xeb194f8e1ae525fd,
    0x92efd1b8d0cf37be,
    0xb7abc627050305ae,
    0xe596b7b0c643c719,
    0x8f7e32ce7bea5c70,
    0xb35dbf821ae4f38c,
    0xe0352f62a19e306f,
];
const POWERS_TEN_E: [i32; 84] = [
    -186, -183, -180, -176, -173, -170, -166, -163, -160, -157, -153, -150, -147, -143, -140, -137,
    -133, -130, -127, -123, -120, -117, -113, -110, -107, -103, -100, -97, -93, -90, -87, -83, -80,
    -77, -73, -70, -67, -63, -60, -57, -54, -50, -47, -44, -40, -37, -34, -30, -27, -24, -20, -17,
    -14, -10, -7, -4, 0, 3, 6, 10, 13, 16, 20, 23, 26, 30, 33, 36, 39, 43, 46, 49, 53, 56, 59, 63,
    66, 69, 73, 76, 79, 83, 86, 89,
];

fn cached_power(i: i32) -> DiyFp {
    let idx = (i + 37).clamp(0, 83) as usize;
    DiyFp {
        f: POWERS_TEN[idx],
        e: POWERS_TEN_E[idx],
    }
}

/// `digit_gen_mix_grisu2`.
fn digit_gen(upper: DiyFp, mut delta: DiyFp, k: &mut i32) -> Vec<u8> {
    let one_e = upper.e;
    let one_f = 1u64 << (-one_e);
    // MuPDF keeps the integral part in an unsigned char.
    let mut p1 = (upper.f >> (-one_e)) as u8;
    let mut p2 = upper.f & (one_f - 1);
    let mut div: u8 = 10;
    let mask = one_f - 1;
    let mut out = Vec::with_capacity(12);
    let mut kappa = 2;
    while kappa > 0 {
        let digit = p1 / div;
        if digit != 0 || !out.is_empty() {
            out.push(b'0' + digit);
        }
        p1 %= div;
        div /= 10;
        if (u64::from(p1) << (-one_e)) + p2 <= delta.f {
            *k += kappa - 1;
            return out;
        }
        kappa -= 1;
    }
    loop {
        p2 = p2.wrapping_mul(10);
        out.push(b'0' + (p2 >> (-one_e)) as u8);
        p2 &= mask;
        kappa -= 1;
        delta.f = delta.f.wrapping_mul(10);
        if p2 <= delta.f {
            break;
        }
    }
    *k += kappa;
    out
}

/// `fz_grisu`: digits `m` and exponent `e` with `v = m * 10^e`, as short as
/// Grisu2 finds them. `v` is finite and positive.
fn grisu(v: f32) -> (Vec<u8>, i32) {
    let alpha = -64 + 4;
    let (w_lower, w_upper) = normalized_boundaries(v);
    let mk = k_comp(alpha - w_upper.e - 64);
    let c_mk = cached_power(mk);
    let mut d_upper = multiply(w_upper, c_mk);
    let mut d_lower = multiply(w_lower, c_mk);
    d_upper.f -= 1;
    d_lower.f += 1;
    let delta = DiyFp {
        f: d_upper.f - d_lower.f,
        e: d_upper.e,
    };
    let mut k = -mk;
    let digits = digit_gen(d_upper, delta, &mut k);
    (digits, k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Dict, ObjRef};
    use std::sync::Arc;

    fn real(f: f32) -> String {
        let mut out = Vec::new();
        format_real(f, &mut out);
        String::from_utf8(out).unwrap_or_default()
    }

    /// Tokens of every kind side by side, tight and spaced, as the
    /// writer and the comparisons print them.
    fn sampler() -> Object {
        let d = |entries: Vec<(&[u8], Object)>| {
            let mut d = Dict::new();
            for (k, v) in entries {
                d.put(k, v);
            }
            Object::Dict(Arc::new(d))
        };
        let s = |b: &[u8]| Object::String(Arc::from(b));
        let n = |b: &[u8]| Object::Name(Arc::from(b));
        let mut long = Vec::new();
        for i in 0..40 {
            long.push(Object::Int(i * 1_234_567 - 9_000_000));
            long.push(n(b"N"));
            long.push(s(b"line\nbreak (x) \\ \t\x08\x0c\x01\xff"));
        }
        d(vec![
            (b"Type", n(b"Page")),
            (b"A", Object::Array(Arc::new(long))),
            (b"Odd#Key", n(b"a b/c#d\x7f\x00tail")),
            (b"Min", Object::Int(i64::MIN)),
            (b"Max", Object::Int(i64::MAX)),
            (b"Zero", Object::Int(0)),
            (b"R", Object::Ref(ObjRef::new(12, 3))),
            (b"Hex", s(b"\xfe\xff\x00A")),
            (b"Bin", s(&[0, 1, 2, 3, 200, 201])),
            (b"Empty", s(b"")),
            (b"EmptyName", n(b"")),
            (
                b"Nested",
                d(vec![(
                    b"K",
                    Object::Array(Arc::new(vec![
                        Object::Null,
                        Object::Bool(true),
                        Object::Real(-0.5),
                        n(b"X"),
                        Object::Int(-7),
                    ])),
                )]),
            ),
        ])
    }

    #[test]
    fn tokens_print_byte_for_byte_as_mupdf_writes_them() {
        let obj = sampler();
        for (tight, ascii) in [(true, false), (false, false), (true, true), (false, true)] {
            let mut out = Vec::new();
            print(&obj, Options { tight, ascii }, &mut out);
            let mut crc = flate2::Crc::new();
            crc.update(&out);
            // From the printer before it wrote a token's characters at once.
            let want = [
                (2398, 0x984b_5cf6),
                (2836, 0x9696_fa6b),
                (2064, 0xee0b_0e6e),
                (2424, 0x9aae_19dd),
            ][usize::from(tight) * 2 + usize::from(ascii)];
            assert_eq!((out.len(), crc.sum()), want, "tight {tight} ascii {ascii}");
        }
    }

    #[test]
    fn reals_print_like_mupdf() {
        assert_eq!(real(0.0), "0");
        assert_eq!(real(-0.0), "0");
        assert_eq!(real(1.0), "1");
        assert_eq!(real(0.5), ".5");
        assert_eq!(real(-0.25), "-.25");
        assert_eq!(real(0.7), ".7");
        assert_eq!(real(12.75), "12.75");
        assert_eq!(real(0.001), ".001");
        assert_eq!(real(1e-7), ".0000001");
        assert_eq!(real(3e9), "3000000000");
        // Outside the int range, as x86 converts.
        assert_eq!(real(2147483648.0), "2147483700");
        assert_eq!(real(-2147483648.0), "-2147483648");
        assert_eq!(real(-3e9), "-3000000000");
        assert_eq!(real(f32::NAN), "0");
        assert_eq!(real(1.1), "1.1");
        assert_eq!(real(612.123), "612.123");
    }

    fn s(bytes: &[u8]) -> Object {
        Object::String(Arc::from(bytes))
    }

    #[test]
    fn tight_output_spaces_only_between_regular_tokens() {
        let mut d = Dict::new();
        d.put(
            b"Kids",
            Object::Array(Arc::new(vec![
                Object::Ref(ObjRef::new(3, 0)),
                Object::Ref(ObjRef::new(4, 0)),
            ])),
        );
        d.put(b"Count", Object::Int(2));
        d.put(b"Type", Object::name(b"Pages"));
        d.put(b"Flag", Object::Bool(true));
        d.put(b"Name", Object::name(b"A B#"));
        let o = Object::Dict(Arc::new(d));
        assert_eq!(
            to_string(&o, Options::default()),
            "<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2/Flag true/Name/A#20B#23>>"
        );
    }

    #[test]
    fn pretty_output_puts_entries_on_lines() {
        let mut d = Dict::new();
        d.put(b"A", Object::Int(1));
        d.put(
            b"B",
            Object::Array(Arc::new(vec![Object::Int(1), Object::Int(2)])),
        );
        let o = Object::Dict(Arc::new(d));
        let pretty = Options {
            tight: false,
            ascii: false,
        };
        assert_eq!(to_string(&o, pretty), "<<\n  /A 1\n  /B [ 1 2 ]\n>>");
    }

    #[test]
    fn strings_choose_literal_or_hex() {
        let t = |o: &Object| to_string(o, Options::default());
        assert_eq!(t(&s(b"Hello (world)\n")), "(Hello \\(world\\)\\n)");
        assert_eq!(t(&s(b"\xfe\xff\x00A")), "<FEFF0041>");
        assert_eq!(t(&s(b"\x01\x02\x03")), "<010203>");
        assert_eq!(t(&s(b"caf\xe9")), "(caf\\351)");
        let ascii = Options {
            tight: true,
            ascii: true,
        };
        assert_eq!(to_string(&s(b"caf\xe9"), ascii), "<636166E9>");
    }

    #[test]
    fn encrypted_strings_are_hex() {
        let mut out = Vec::new();
        let rot = |d: &[u8]| d.iter().map(|b| b.wrapping_add(1)).collect::<Vec<u8>>();
        print_encrypted(&s(b"AB"), Options::default(), Some(&rot), &mut out);
        assert_eq!(out, b"<4243>");
    }
}
