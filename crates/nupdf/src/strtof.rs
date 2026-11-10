// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/strtof.c and source/fitz/string.c
// (fz_atof), Copyright (C) 2004-2025 Artifex Software, Inc.

//! MuPDF's float parser, bit for bit.
//!
//! It is not correctly rounded: it keeps 9 significant digits and uses
//! Coonen's Algorithm D with a 32-bit significand, so exponents beyond ±13
//! can be off by up to (1/2 + 1/256) ulp. Every real in a PDF goes through
//! it, so parity requires the same algorithm, not a better one.

#[derive(Clone, Copy)]
struct Fp {
    f: u32,
    e: i32,
}

fn multiply(x: Fp, y: Fp) -> Fp {
    let mut e = x.e + y.e + 32;
    let mut tmp = u64::from(x.f) * u64::from(y.f);
    if tmp < 1u64 << 63 {
        tmp <<= 1;
        e -= 1;
    }
    let mut f = (tmp >> 32) as u32;
    // Round to odd: mark inexact results in the last bit.
    if tmp & 0xffff_ffff != 0 {
        f |= 1;
    }
    Fp { f, e }
}

fn divide(x: Fp, y: Fp) -> Fp {
    let mut e = x.e - y.e - 32;
    let product = u64::from(x.f) << 32;
    let quotient = product / u64::from(y.f);
    let remainder = product % u64::from(y.f);
    let mut f = if quotient <= 0xffff_ffff {
        quotient as u32
    } else {
        e += 1;
        (quotient >> 1) as u32
    };
    if remainder != 0 {
        f |= 1;
    }
    Fp { f, e }
}

const POWERS_TEN: [u32; 55] = [
    0x80000000, 0xa0000000, 0xc8000000, 0xfa000000, 0x9c400000, 0xc3500000, 0xf4240000, 0x98968000,
    0xbebc2000, 0xee6b2800, 0x9502f900, 0xba43b740, 0xe8d4a510, 0x9184e72a, 0xb5e620f4, 0xe35fa932,
    0x8e1bc9bf, 0xb1a2bc2f, 0xde0b6b3a, 0x8ac72305, 0xad78ebc6, 0xd8d726b7, 0x87867832, 0xa968163f,
    0xd3c21bcf, 0x84595161, 0xa56fa5ba, 0xcecb8f28, 0x813f3979, 0xa18f07d7, 0xc9f2c9cd, 0xfc6f7c40,
    0x9dc5ada8, 0xc5371912, 0xf684df57, 0x9a130b96, 0xc097ce7c, 0xf0bdc21b, 0x96769951, 0xbc143fa5,
    0xeb194f8e, 0x92efd1b9, 0xb7abc627, 0xe596b7b1, 0x8f7e32ce, 0xb35dbf82, 0xe0352f63, 0x8c213d9e,
    0xaf298d05, 0xdaf3f046, 0x88d8762c, 0xab0e93b7, 0xd5d238a5, 0x85a36367, 0xa70c3c41,
];

const POWERS_TEN_E: [i32; 55] = [
    -31, -28, -25, -22, -18, -15, -12, -8, -5, -2, 2, 5, 8, 12, 15, 18, 22, 25, 28, 32, 35, 38, 42,
    45, 48, 52, 55, 58, 62, 65, 68, 71, 75, 78, 81, 85, 88, 91, 95, 98, 101, 105, 108, 111, 115,
    118, 121, 125, 128, 131, 135, 138, 141, 145, 148,
];

fn cached_power(i: i32) -> Fp {
    let i = i.clamp(0, 54) as usize;
    Fp {
        f: POWERS_TEN[i],
        e: POWERS_TEN_E[i],
    }
}

fn normalize(x: u32) -> Fp {
    let shift = x.leading_zeros();
    Fp {
        f: x << shift,
        e: -(shift as i32),
    }
}

/// Result plus MuPDF's `errno == ERANGE` flag.
struct Parsed {
    value: f32,
    range_error: bool,
}

fn diy_to_float(x: Fp, negative: bool) -> Parsed {
    const SIGNIFICAND: u32 = 23;
    const MASK: u32 = 0x7f_ffff;
    let mut range_error = false;
    let mut result: u32 = if x.e > 96 || (x.e == 96 && x.f >= 0xffff_ff80) {
        range_error = true;
        0xff << SIGNIFICAND
    } else if x.e > -158 {
        let mut r = ((x.e + 158) as u32) << SIGNIFICAND;
        r |= (x.f >> 8) & MASK;
        if x.f & 0x80 != 0 && (x.f & 0x7f != 0 || x.f & 0x100 != 0) {
            r += 1;
        }
        r
    } else if x.e == -158 && x.f >= 0xffff_ff00 {
        1 << SIGNIFICAND
    } else if x.e > -181 {
        let shift = (-149 - x.e) as u32;
        let mut r = x.f >> shift;
        if x.f & (1 << (shift - 1)) != 0
            && (x.f & ((1 << (shift - 1)) - 1) != 0 || x.f & (1 << shift) != 0)
        {
            r += 1;
        }
        r
    } else if x.e == -181 && x.f > 0x8000_0000 {
        1
    } else {
        range_error = true;
        0
    };
    if negative {
        result |= 0x8000_0000;
    }
    Parsed {
        value: f32::from_bits(result),
        range_error,
    }
}

fn scale(mut m: u32, mut n: i32, negative: bool) -> Parsed {
    let signed = |v: f32| if negative { -v } else { v };
    if m == 0 {
        return Parsed {
            value: signed(0.0),
            range_error: false,
        };
    }
    if n > 38 {
        return Parsed {
            value: signed(f32::INFINITY),
            range_error: true,
        };
    }
    if n < -54 {
        return Parsed {
            value: signed(0.0),
            range_error: true,
        };
    }
    // Exponents within ±13 are exact; try to scale into that range.
    while n > 13 && m <= u32::MAX / 10 {
        m *= 10;
        n -= 1;
    }
    while n < -13 && m.is_multiple_of(10) {
        m /= 10;
        n += 1;
    }
    let x = normalize(m);
    let result = if n >= 0 {
        multiply(x, cached_power(n))
    } else {
        divide(x, cached_power(-n))
    };
    diy_to_float(result, negative)
}

fn starts_with_ignore_case(s: &[u8], word: &[u8]) -> bool {
    s.len() >= word.len() && s.iter().zip(word).all(|(&a, &b)| a == b || a == b + 32)
}

fn strtof(text: &[u8]) -> Parsed {
    strtof_end(text).0
}

/// `fz_strtof` with its tail pointer: the value, and how many bytes it
/// read (none when there is no number).
pub(crate) fn fz_strtof(text: &[u8]) -> (f32, usize) {
    let (parsed, end) = strtof_end(text);
    (parsed.value, end)
}

fn strtof_end(text: &[u8]) -> (Parsed, usize) {
    let at = |i: usize| text.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while matches!(at(i), b' ' | b'\x0c' | b'\n' | b'\r' | b'\t' | b'\x0b') {
        i += 1;
    }
    if at(i) == b'+' {
        i += 1;
    }
    let mut negative = false;
    if at(i) == b'-' {
        negative = true;
        i += 1;
    }
    let number_start = i;
    let (mut m, mut n, mut digits) = (0u32, 0i32, 0);
    while at(i).is_ascii_digit() {
        let d = u32::from(at(i) - b'0');
        if digits > 0 {
            if digits < 9 {
                digits += 1;
                m = m * 10 + d;
            } else if n < 1000 {
                n += 1;
            }
        } else if d > 0 {
            m = d;
            digits += 1;
        }
        i += 1;
    }
    if at(i) == b'.' {
        i += 1;
    }
    while at(i).is_ascii_digit() {
        let d = u32::from(at(i) - b'0');
        if digits < 9 {
            if digits > 0 || d > 0 {
                digits += 1;
                m = m * 10 + d;
            }
            n -= 1;
        }
        i += 1;
    }
    if i == number_start || (i == number_start + 1 && at(number_start) == b'.') {
        let rest = &text[number_start.min(text.len())..];
        if starts_with_ignore_case(rest, b"INF") {
            let len = if starts_with_ignore_case(rest, b"INFINITY") {
                8
            } else {
                3
            };
            let value = if negative {
                f32::NEG_INFINITY
            } else {
                f32::INFINITY
            };
            return (
                Parsed {
                    value,
                    range_error: true,
                },
                number_start + len,
            );
        }
        if starts_with_ignore_case(rest, b"NAN") {
            return (
                Parsed {
                    value: f32::NAN,
                    range_error: false,
                },
                number_start + 3,
            );
        }
        return (
            Parsed {
                value: 0.0,
                range_error: false,
            },
            0,
        );
    }
    if matches!(at(i), b'e' | b'E') {
        let mut j = i + 1;
        let mut exp_negative = false;
        if at(j) == b'+' {
            j += 1;
        } else if at(j) == b'-' {
            exp_negative = true;
            j += 1;
        }
        let digits_start = j;
        let mut exp = 0i32;
        while at(j).is_ascii_digit() {
            if exp < 100 {
                exp = exp * 10 + i32::from(at(j) - b'0');
            }
            j += 1;
        }
        if j != digits_start {
            n += if exp_negative { -exp } else { exp };
            i = j;
        }
    }
    (scale(m, n, negative), i)
}

/// MuPDF's `fz_atof`: `fz_strtof`, except that underflow and NaN become 1
/// ("a small known value that won't cause a divide by 0") and infinities
/// are clamped to the largest finite float.
pub fn fz_atof(text: &[u8]) -> f32 {
    let parsed = strtof(text);
    if (parsed.range_error && parsed.value == 0.0) || parsed.value.is_nan() {
        return 1.0;
    }
    parsed.value.clamp(-f32::MAX, f32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_is_where_the_number_ends() {
        assert_eq!(fz_strtof(b"72.5,720"), (72.5, 4));
        assert_eq!(fz_strtof(b"1e3x"), (1000.0, 3));
        assert_eq!(fz_strtof(b"2ex"), (2.0, 1));
        let (v, n) = fz_strtof(b"nan,1");
        assert!(v.is_nan() && n == 3);
        assert_eq!(fz_strtof(b",1"), (0.0, 0));
    }

    #[test]
    fn exact_for_small_exponents() {
        for s in [
            "1",
            "0.5",
            "1.7",
            "2.0",
            "612",
            "-3.25",
            "0.001",
            "123456.789",
        ] {
            let expected: f32 = s.parse().unwrap();
            assert_eq!(fz_atof(s.as_bytes()), expected, "{s}");
        }
    }

    #[test]
    fn keeps_nine_significant_digits() {
        // The tenth digit is dropped, not rounded.
        assert_eq!(fz_atof(b"1.0000000009"), 1.0);
        assert_eq!(
            fz_atof(b"0.0000000001234567891"),
            fz_atof(b"0.000000000123456789")
        );
    }

    #[test]
    fn underflow_and_nan_become_one() {
        assert_eq!(fz_atof(b"1e-60"), 1.0);
        assert_eq!(fz_atof(b"nan"), 1.0);
        assert_eq!(fz_atof(b"0"), 0.0);
        assert_eq!(fz_atof(b"1e60"), f32::MAX);
        assert_eq!(fz_atof(b"-inf"), -f32::MAX);
    }

    #[test]
    fn junk_is_zero_and_exponents_need_digits() {
        assert_eq!(fz_atof(b""), 0.0);
        assert_eq!(fz_atof(b"."), 0.0);
        assert_eq!(fz_atof(b"abc"), 0.0);
        assert_eq!(fz_atof(b"1.5e"), 1.5);
        assert_eq!(fz_atof(b"1.5e2"), 150.0);
        assert_eq!(fz_atof(b"  +-2"), -2.0);
    }

    #[test]
    fn large_exponents_follow_algorithm_d() {
        // Values where MuPDF differs from correct rounding come out of the
        // same arithmetic; spot-check that results are finite and close.
        let v = fz_atof(b"3.4e38");
        assert!((v - 3.4e38).abs() / 3.4e38 < 1e-6);
        let tiny = fz_atof(b"1.17549435e-38");
        assert!(tiny > 0.0);
    }
}
