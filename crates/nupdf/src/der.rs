// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Just enough DER (and BER, as OpenSSL reads it) to read a CMS signature:
//! tag, length and contents of each element. Indefinite lengths, which BER
//! allows on constructed elements, are read to their end-of-contents
//! marker, nested at most `MAX_DEPTH` deep.

/// Indefinite-length elements nested deeper than this are refused.
const MAX_DEPTH: usize = 64;

/// One element: its tag byte (class, constructed bit and number, for tag
/// numbers below 31) and its contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tlv<'a> {
    pub tag: u8,
    pub value: &'a [u8],
    /// The whole element, header included.
    pub raw: &'a [u8],
}

pub const INTEGER: u8 = 0x02;
pub const OCTET_STRING: u8 = 0x04;
pub const OID: u8 = 0x06;
pub const UTF8_STRING: u8 = 0x0c;
pub const PRINTABLE_STRING: u8 = 0x13;
pub const T61_STRING: u8 = 0x14;
pub const IA5_STRING: u8 = 0x16;
pub const UTC_TIME: u8 = 0x17;
pub const GENERALIZED_TIME: u8 = 0x18;
pub const BMP_STRING: u8 = 0x1e;
pub const SEQUENCE: u8 = 0x30;
pub const SET: u8 = 0x31;

/// `[n]`, context-specific and constructed.
pub const fn context(n: u8) -> u8 {
    0xa0 | n
}

/// The element at the start of `data` and what follows it. The contents
/// of an indefinite-length element run up to its end-of-contents marker.
pub fn read(data: &[u8]) -> Result<(Tlv<'_>, &[u8]), String> {
    read_at(data, 0)
}

fn read_at(data: &[u8], depth: usize) -> Result<(Tlv<'_>, &[u8]), String> {
    let (&tag, rest) = data.split_first().ok_or("DER: unexpected end")?;
    if tag & 0x1f == 0x1f {
        return Err("DER: high tag numbers are not supported".into());
    }
    let (&first, mut rest) = rest.split_first().ok_or("DER: unexpected end")?;
    if first == 0x80 {
        if tag & 0x20 == 0 {
            return Err("DER: indefinite length on a primitive element".into());
        }
        if depth >= MAX_DEPTH {
            return Err("DER: nested too deep".into());
        }
        let start = data.len() - rest.len();
        let mut inner = rest;
        loop {
            if inner.starts_with(&[0, 0]) {
                let len = rest.len() - inner.len();
                let end = start + len + 2;
                return Ok((
                    Tlv {
                        tag,
                        value: &rest[..len],
                        raw: &data[..end],
                    },
                    &inner[2..],
                ));
            }
            inner = read_at(inner, depth + 1)?.1;
        }
    }
    let len = if first < 0x80 {
        usize::from(first)
    } else {
        let n = usize::from(first & 0x7f);
        if n > 4 || rest.len() < n {
            return Err("DER: bad length".into());
        }
        let len = rest[..n]
            .iter()
            .fold(0usize, |l, &b| (l << 8) | usize::from(b));
        rest = &rest[n..];
        len
    };
    if rest.len() < len {
        return Err("DER: element runs past its end".into());
    }
    let header = data.len() - rest.len();
    Ok((
        Tlv {
            tag,
            value: &rest[..len],
            raw: &data[..header + len],
        },
        &rest[len..],
    ))
}

/// The element at the start of `data`, which must have tag `tag`.
pub fn expect(data: &[u8], tag: u8) -> Result<(Tlv<'_>, &[u8]), String> {
    let (t, rest) = read(data)?;
    if t.tag != tag {
        return Err(format!(
            "DER: expected tag {tag:#04x}, found {:#04x}",
            t.tag
        ));
    }
    Ok((t, rest))
}

/// The elements of a constructed element's contents, at most `limit`.
pub fn children(mut data: &[u8], limit: usize) -> Result<Vec<Tlv<'_>>, String> {
    let mut out = Vec::new();
    while !data.is_empty() {
        if out.len() >= limit {
            return Err("DER: too many elements".into());
        }
        let (t, rest) = read(data)?;
        out.push(t);
        data = rest;
    }
    Ok(out)
}

/// An object identifier in dotted form.
pub fn oid(value: &[u8]) -> String {
    let mut parts: Vec<u64> = Vec::new();
    let mut n: u64 = 0;
    for &b in value {
        n = (n << 7) | u64::from(b & 0x7f);
        if b & 0x80 == 0 {
            if parts.is_empty() {
                let (a, b) = if n < 80 {
                    (n / 40, n % 40)
                } else {
                    (2, n - 80)
                };
                parts.push(a);
                parts.push(b);
            } else {
                parts.push(n);
            }
            n = 0;
        }
        if n > u64::MAX >> 7 {
            break;
        }
    }
    parts
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

/// A string element's text: UTF-8, BMP (UTF-16BE), or one byte a character.
pub fn text(t: &Tlv) -> String {
    match t.tag {
        UTF8_STRING => String::from_utf8_lossy(t.value).into_owned(),
        BMP_STRING => {
            let units: Vec<u16> = t
                .value
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| u16::from_be_bytes(c))
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => t.value.iter().map(|&b| char::from(b)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_short_and_long_lengths() {
        let (t, rest) = read(&[0x04, 0x02, 0xaa, 0xbb, 0x05]).unwrap();
        assert_eq!(
            (t.tag, t.value, rest),
            (OCTET_STRING, &[0xaa, 0xbb][..], &[0x05][..])
        );
        let mut long = vec![0x04, 0x81, 0x80];
        long.extend([7u8; 0x80]);
        assert_eq!(read(&long).unwrap().0.value.len(), 0x80);
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        assert!(read(&[]).is_err());
        assert!(read(&[0x30, 0x80]).is_err());
        assert!(read(&[0x04, 0x80, 0, 0]).is_err());
        assert!(read(&[0x04, 0x05, 1]).is_err());
        assert!(read(&[0x1f, 0x01, 0x00]).is_err());
        assert!(read(&[0x04, 0x85, 1, 1, 1, 1, 1]).is_err());
    }

    #[test]
    fn reads_indefinite_lengths_to_their_end() {
        let ber = [
            0x30, 0x80, 0x04, 0x01, 0xaa, 0x30, 0x80, 0x00, 0x00, 0x00, 0x00, 0x05,
        ];
        let (t, rest) = read(&ber).unwrap();
        assert_eq!(t.value, &ber[2..9]);
        assert_eq!(t.raw.len(), 11);
        assert_eq!(rest, &[0x05]);
        assert_eq!(children(t.value, 10).unwrap().len(), 2);
        let deep: Vec<u8> = std::iter::repeat_n([0x30u8, 0x80], 100).flatten().collect();
        assert!(read(&deep).is_err());
    }

    #[test]
    fn object_identifiers_read_dotted() {
        assert_eq!(
            oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02]),
            "1.2.840.113549.1.7.2"
        );
        assert_eq!(
            oid(&[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01]),
            "2.16.840.1.101.3.4.2.1"
        );
    }
}
