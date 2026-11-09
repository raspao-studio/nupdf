// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-stream.c (build_filter),
// source/fitz/filter-flate.c and source/fitz/filter-predict.c,
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! Stream filters. M1 needs only what cross-reference streams and object
//! streams use in practice: FlateDecode with PNG and TIFF predictors. The
//! remaining filters arrive in M2 and report themselves as unsupported.

use flate2::{Decompress, FlushDecompress, Status};

use crate::diag::Diagnostics;
use crate::object::{Dict, Object};

use crate::limits::Limits;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The filter exists in PDF but nupdf does not decode it yet.
    Unsupported(String),
    /// The data is broken in a way MuPDF also refuses.
    Failed(String),
    Limit,
}

impl DecodeError {
    /// The line MuPDF prints when this error is caught.
    pub fn report(&self) -> String {
        match self {
            DecodeError::Failed(m) if m.starts_with("zlib error") => format!("library error: {m}"),
            DecodeError::Limit => format!("limit error: {self}"),
            other => format!("format error: {other}"),
        }
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Unsupported(name) => write!(f, "unsupported filter {name}"),
            DecodeError::Failed(msg) => f.write_str(msg),
            DecodeError::Limit => f.write_str("decoded stream exceeds the size limit"),
        }
    }
}

/// Decodes `raw` according to the stream dictionary, for non-image use.
/// `resolve` turns indirect references into objects.
pub fn decode(
    dict: &Dict,
    raw: &[u8],
    resolve: &dyn Fn(&Object) -> Object,
    diag: &Diagnostics,
    limits: &Limits,
) -> Result<Vec<u8>, DecodeError> {
    // pdf_dict_geta: the abbreviation wins when both are present.
    let filters = resolve(
        dict.get(b"F")
            .or_else(|| dict.get(b"Filter"))
            .unwrap_or(&Object::Null),
    );
    let params = resolve(
        dict.get(b"DP")
            .or_else(|| dict.get(b"DecodeParms"))
            .unwrap_or(&Object::Null),
    );

    let chain: Vec<(Object, Object)> = match &filters {
        Object::Name(_) => vec![(filters.clone(), params)],
        Object::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let p = params
                    .as_array()
                    .and_then(|ps| ps.get(i))
                    .map_or(Object::Null, resolve);
                (resolve(f), p)
            })
            .collect(),
        _ => Vec::new(),
    };

    let mut data = raw.to_vec();
    for (filter, params) in chain {
        data = apply(
            &filter,
            &params,
            &data,
            resolve,
            diag,
            limits.max_decoded_bytes,
        )?;
    }
    Ok(data)
}

fn apply(
    filter: &Object,
    params: &Object,
    input: &[u8],
    resolve: &dyn Fn(&Object) -> Object,
    diag: &Diagnostics,
    max: usize,
) -> Result<Vec<u8>, DecodeError> {
    let name = filter.as_name().unwrap_or_default();
    let int = |key: &[u8], default: i32| {
        params
            .as_dict()
            .and_then(|d| d.get(key))
            .map_or(default, |v| resolve(v).to_int_or(default))
    };
    match name {
        b"FlateDecode" | b"Fl" => {
            let data = inflate(input, diag, max)?;
            let predictor = int(b"Predictor", 1);
            if predictor > 1 {
                predict(
                    &data,
                    predictor,
                    int(b"Columns", 1),
                    int(b"Colors", 1),
                    int(b"BitsPerComponent", 8),
                    diag,
                )
            } else {
                Ok(data)
            }
        }
        b"CCITTFaxDecode" | b"CCF" | b"DCTDecode" | b"DCT" | b"JBIG2Decode" | b"JPXDecode" => {
            diag.warn(
                "filter.image_only",
                "Can't open image only stream for non-image purposes",
            );
            Ok(Vec::new())
        }
        b"ASCIIHexDecode" | b"AHx" => ascii_hex(input),
        b"ASCII85Decode" | b"A85" => ascii85(input, diag),
        b"RunLengthDecode" | b"RL" => run_length(input, max),
        b"LZWDecode" | b"LZW" | b"BrotliDecode" | b"Br" => Err(DecodeError::Unsupported(
            String::from_utf8_lossy(name).into_owned(),
        )),
        b"Crypt" => {
            diag.warn(
                "filter.crypt_unencrypted",
                "crypt filter in unencrypted document",
            );
            Ok(input.to_vec())
        }
        other => {
            diag.warn(
                "filter.unknown",
                format!("unknown filter name ({})", String::from_utf8_lossy(other)),
            );
            Ok(input.to_vec())
        }
    }
}

/// MuPDF's `ahxd`: hex pairs until '>', whitespace ignored.
pub fn ascii_hex(input: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::with_capacity(input.len() / 2);
    let mut high: Option<u8> = None;
    for &c in input {
        if c.is_ascii_hexdigit() {
            let v = (c as char).to_digit(16).unwrap_or(0) as u8;
            match high.take() {
                Some(h) => out.push((h << 4) | v),
                None => high = Some(v),
            }
        } else if c == b'>' {
            if let Some(h) = high {
                out.push(h << 4);
            }
            return Ok(out);
        } else if !crate::lex::is_white(c) {
            return Err(DecodeError::Failed(format!(
                "bad data in ahxd: '{}'",
                c as char
            )));
        }
    }
    Ok(out)
}

/// MuPDF's `a85d`, including its handling of short final groups.
pub fn ascii85(input: &[u8], diag: &Diagnostics) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::with_capacity(input.len() * 4 / 5);
    let mut word: u32 = 0;
    let mut count = 0;
    let mut i = 0;
    while let Some(&c) = input.get(i) {
        i += 1;
        if (b'!'..=b'u').contains(&c) {
            word = word.wrapping_mul(85).wrapping_add(u32::from(c - b'!'));
            if count == 4 {
                out.extend_from_slice(&word.to_be_bytes());
                word = 0;
                count = 0;
            } else {
                count += 1;
            }
        } else if c == b'z' && count == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else if c == b'~' {
            if input.get(i) != Some(&b'>') {
                diag.warn("filter.a85_bad_eod", "bad eod marker in a85d");
            }
            let bytes = word_tail(word, count, diag);
            out.extend_from_slice(&bytes);
            return Ok(out);
        } else if !crate::lex::is_white(c) {
            return Err(DecodeError::Failed(format!(
                "bad data in a85d: '{}'",
                c as char
            )));
        }
    }
    Ok(out)
}

fn word_tail(word: u32, count: usize, diag: &Diagnostics) -> Vec<u8> {
    let full = |w: u32, n: usize| w.to_be_bytes()[..n].to_vec();
    match count {
        1 => {
            diag.warn("filter.a85_partial", "partial final byte in a85d");
            Vec::new()
        }
        2 => full(word.wrapping_mul(85 * 85 * 85).wrapping_add(0xff_ffff), 1),
        3 => full(word.wrapping_mul(85 * 85).wrapping_add(0xffff), 2),
        4 => full(word.wrapping_mul(85).wrapping_add(0xff), 3),
        _ => Vec::new(),
    }
}

/// MuPDF's `rld`: PackBits runs, 128 ends the data.
pub fn run_length(input: &[u8], max: usize) -> Result<Vec<u8>, DecodeError> {
    let premature = || DecodeError::Failed("premature end of data in run length decode".into());
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while let Some(&run) = input.get(i) {
        i += 1;
        match run {
            128 => break,
            0..=127 => {
                let n = usize::from(run) + 1;
                let chunk = input.get(i..i + n).ok_or_else(premature)?;
                out.extend_from_slice(chunk);
                i += n;
            }
            _ => {
                let &c = input.get(i).ok_or_else(premature)?;
                i += 1;
                out.extend(std::iter::repeat_n(c, 257 - usize::from(run)));
            }
        }
        if out.len() > max {
            return Err(DecodeError::Limit);
        }
    }
    Ok(out)
}

/// zlib inflate with MuPDF's tolerance: truncated data and errors at the
/// very end of the input keep what was decoded; other errors fail.
pub fn inflate(input: &[u8], diag: &Diagnostics, max: usize) -> Result<Vec<u8>, DecodeError> {
    let mut z = Decompress::new(true);
    // Never hold more than one byte past the limit, so exceeding it is
    // detected without first allocating the whole expansion.
    let cap = |n: usize| n.min(max.saturating_add(1));
    let mut out: Vec<u8> =
        Vec::with_capacity(cap(input.len().saturating_mul(4).clamp(64, 1 << 20)));
    loop {
        if out.len() > max {
            return Err(DecodeError::Limit);
        }
        if out.len() == out.capacity() {
            let grow = cap(out.len() + out.len().clamp(64 * 1024, 16 << 20)) - out.len();
            out.reserve(grow.max(1));
        }
        let consumed = usize::try_from(z.total_in())
            .unwrap_or(input.len())
            .min(input.len());
        let rest = &input[consumed..];
        let before = (z.total_in(), z.total_out());
        match z.decompress_vec(rest, &mut out, FlushDecompress::Sync) {
            Ok(Status::StreamEnd) if out.len() > max => return Err(DecodeError::Limit),
            Ok(Status::StreamEnd) => return Ok(out),
            Ok(Status::BufError) => {
                diag.warn(
                    "filter.flate_truncated",
                    "premature end of data in flate filter",
                );
                return Ok(out);
            }
            Ok(Status::Ok) => {
                // No progress with input left means zlib wants more than we have.
                if (z.total_in(), z.total_out()) == before && out.len() < out.capacity() {
                    diag.warn(
                        "filter.flate_truncated",
                        "premature end of data in flate filter",
                    );
                    return Ok(out);
                }
            }
            Err(e) => {
                let msg = e.message().unwrap_or("unknown error").to_string();
                let consumed = usize::try_from(z.total_in()).unwrap_or(0);
                if consumed >= input.len() || msg == "incorrect data check" {
                    diag.warn("filter.flate_error", format!("ignoring zlib error: {msg}"));
                    return Ok(out);
                }
                return Err(DecodeError::Failed(format!("zlib error: {msg}")));
            }
        }
    }
}

/// MuPDF's predictor filter, applied to a whole decoded buffer.
pub fn predict(
    input: &[u8],
    predictor: i32,
    columns: i32,
    colors: i32,
    bpc: i32,
    diag: &Diagnostics,
) -> Result<Vec<u8>, DecodeError> {
    const MAX_COLORS: i32 = 32;
    let mut predictor = predictor.max(1);
    let columns = columns.max(1);
    let colors = colors.max(1);
    let bpc = if bpc < 1 { 8 } else { bpc };
    if ![1, 2, 4, 8, 16].contains(&bpc) {
        return Err(DecodeError::Failed(format!(
            "invalid number of bits per component: {bpc}"
        )));
    }
    if colors > MAX_COLORS {
        return Err(DecodeError::Failed(format!(
            "too many color components ({colors} > {MAX_COLORS})"
        )));
    }
    if columns >= i32::MAX / (bpc * colors) {
        return Err(DecodeError::Failed(format!(
            "too many columns lead to an integer overflow ({columns})"
        )));
    }
    if ![1, 2, 10, 11, 12, 13, 14, 15].contains(&predictor) {
        diag.warn(
            "filter.bad_predictor",
            format!("invalid predictor: {predictor}"),
        );
        predictor = 1;
    }
    let (colors, bpc) = (colors as usize, bpc as usize);
    let stride = (bpc * colors * columns as usize).div_ceil(8);
    let bpp = (bpc * colors).div_ceil(8);
    let png = predictor >= 10;
    let row_in = stride + usize::from(png);

    let mut out = Vec::with_capacity(input.len());
    let mut row = vec![0u8; stride];
    let mut reference = vec![0u8; stride];
    let mut padded = vec![0u8; stride];
    for chunk in input.chunks(row_in) {
        let n = chunk.len();
        match predictor {
            1 => out.extend_from_slice(chunk),
            2 => {
                padded[..n].copy_from_slice(chunk);
                tiff_row(&mut row, &padded, columns as usize, colors, bpc);
                out.extend_from_slice(&row[..n]);
            }
            _ => {
                let len = n - 1;
                png_row(
                    &mut row[..len],
                    &chunk[1..],
                    &reference,
                    bpp,
                    chunk[0],
                    diag,
                );
                reference.copy_from_slice(&row);
                out.extend_from_slice(&row[..len]);
            }
        }
    }
    Ok(out)
}

fn png_row(
    out: &mut [u8],
    input: &[u8],
    reference: &[u8],
    bpp: usize,
    kind: u8,
    diag: &Diagnostics,
) {
    let len = out.len();
    let bpp = bpp.min(len);
    match kind {
        1 => {
            for i in 0..len {
                out[i] = if i < bpp {
                    input[i]
                } else {
                    input[i].wrapping_add(out[i - bpp])
                };
            }
        }
        2 => {
            for i in 0..len {
                out[i] = input[i].wrapping_add(reference[i]);
            }
        }
        3 => {
            for i in 0..len {
                let avg = if i < bpp {
                    u16::from(reference[i]) / 2
                } else {
                    (u16::from(out[i - bpp]) + u16::from(reference[i])) / 2
                };
                out[i] = input[i].wrapping_add(avg as u8);
            }
        }
        4 => {
            for i in 0..len {
                let p = if i < bpp {
                    paeth(0, i32::from(reference[i]), 0)
                } else {
                    paeth(
                        i32::from(out[i - bpp]),
                        i32::from(reference[i]),
                        i32::from(reference[i - bpp]),
                    )
                };
                out[i] = input[i].wrapping_add(p as u8);
            }
        }
        other => {
            if other != 0 {
                diag.warn(
                    "filter.bad_png_predictor",
                    format!("unknown png predictor {other}, treating as none"),
                );
            }
            out.copy_from_slice(&input[..len]);
        }
    }
}

fn paeth(a: i32, b: i32, c: i32) -> i32 {
    // MuPDF's definitions of ac and bc are deliberate, not a typo.
    let ac = b - c;
    let bc = a - c;
    let abcc = ac + bc;
    let (pa, pb, pc) = (ac.abs(), bc.abs(), abcc.abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn tiff_row(out: &mut [u8], input: &[u8], columns: usize, colors: usize, bpc: usize) {
    let mut left = vec![0u32; colors];
    let cells = (0..columns).flat_map(|i| (0..colors).map(move |k| (i * colors + k, k)));
    if bpc == 8 {
        for (x, k) in cells {
            let v = (u32::from(input[x]) + left[k]) & 0xff;
            out[x] = v as u8;
            left[k] = v;
        }
        return;
    }
    let mask = (1u32 << bpc) - 1;
    if bpc < 8 {
        out.fill(0);
    }
    for (x, k) in cells {
        let c = (get_component(input, x, bpc) + left[k]) & mask;
        put_component(out, x, bpc, c);
        left[k] = c;
    }
}

fn get_component(line: &[u8], x: usize, bpc: usize) -> u32 {
    let byte = |i: usize| u32::from(line[i]);
    match bpc {
        1 => (byte(x >> 3) >> (7 - (x & 7))) & 1,
        2 => (byte(x >> 2) >> ((3 - (x & 3)) << 1)) & 3,
        4 => (byte(x >> 1) >> ((1 - (x & 1)) << 2)) & 15,
        16 => (byte(x << 1) << 8) + byte((x << 1) + 1),
        _ => 0,
    }
}

fn put_component(buf: &mut [u8], x: usize, bpc: usize, value: u32) {
    match bpc {
        1 => buf[x >> 3] |= (value << (7 - (x & 7))) as u8,
        2 => buf[x >> 2] |= (value << ((3 - (x & 3)) << 1)) as u8,
        4 => buf[x >> 1] |= (value << ((1 - (x & 1)) << 2)) as u8,
        16 => {
            buf[x << 1] = (value >> 8) as u8;
            buf[(x << 1) + 1] = value as u8;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn inflates_and_tolerates_truncation() {
        let diag = Diagnostics::new();
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let packed = zlib(&data);
        assert_eq!(inflate(&packed, &diag, usize::MAX).unwrap(), data);
        assert!(diag.snapshot().is_empty());

        let cut = &packed[..packed.len() / 2];
        let partial = inflate(cut, &diag, usize::MAX).unwrap();
        assert!(!partial.is_empty() && data.starts_with(&partial));
        assert!(diag.has("filter.flate_truncated"));
    }

    #[test]
    fn bad_checksum_keeps_the_data() {
        let diag = Diagnostics::new();
        let mut packed = zlib(b"hello hello hello");
        let n = packed.len();
        packed[n - 1] ^= 0xff;
        assert_eq!(
            inflate(&packed, &diag, usize::MAX).unwrap(),
            b"hello hello hello"
        );
        assert!(diag.has("filter.flate_error"));
    }

    #[test]
    fn bad_header_fails_like_mupdf() {
        let diag = Diagnostics::new();
        assert!(matches!(
            inflate(b"not zlib at all", &diag, usize::MAX),
            Err(DecodeError::Failed(_))
        ));
    }

    #[test]
    fn png_up_predictor() {
        let diag = Diagnostics::new();
        // Two rows of 3 bytes, filter type 2 (Up).
        let input = [2, 1, 2, 3, 2, 1, 1, 1];
        assert_eq!(
            predict(&input, 12, 3, 1, 8, &diag).unwrap(),
            vec![1, 2, 3, 2, 3, 4]
        );
    }

    #[test]
    fn png_sub_average_paeth() {
        let diag = Diagnostics::new();
        assert_eq!(
            predict(&[1, 5, 1, 1], 11, 3, 1, 8, &diag).unwrap(),
            vec![5, 6, 7]
        );
        assert_eq!(
            predict(&[0, 10, 20, 3, 2, 2], 10, 2, 1, 8, &diag).unwrap(),
            vec![10, 20, 7, 15]
        );
        assert_eq!(
            predict(&[0, 10, 20, 4, 1, 1], 15, 2, 1, 8, &diag).unwrap(),
            vec![10, 20, 11, 21]
        );
    }

    #[test]
    fn partial_last_row_is_kept() {
        let diag = Diagnostics::new();
        assert_eq!(
            predict(&[0, 1, 2, 3, 2, 1], 12, 3, 1, 8, &diag).unwrap(),
            vec![1, 2, 3, 2]
        );
    }

    #[test]
    fn tiff_predictor() {
        let diag = Diagnostics::new();
        assert_eq!(
            predict(&[1, 1, 1, 5, 5, 5], 2, 3, 1, 8, &diag).unwrap(),
            vec![1, 2, 3, 5, 10, 15]
        );
        // 4 bits per component: 0x11 = [1,1] -> [1,2] = 0x12
        assert_eq!(predict(&[0x11], 2, 2, 1, 4, &diag).unwrap(), vec![0x12]);
    }

    #[test]
    fn predictor_parameter_checks() {
        let diag = Diagnostics::new();
        assert!(predict(&[0], 12, 1, 1, 3, &diag).is_err());
        assert!(predict(&[0], 12, 1, 33, 8, &diag).is_err());
        assert_eq!(predict(&[7, 8], 99, 1, 1, 8, &diag).unwrap(), vec![7, 8]);
        assert!(diag.has("filter.bad_predictor"));
    }

    #[test]
    fn decode_limit_stops_expansion() {
        let diag = Diagnostics::new();
        let packed = zlib(&vec![0u8; 1 << 20]);
        assert_eq!(inflate(&packed, &diag, 1000), Err(DecodeError::Limit));
        assert_eq!(run_length(&[129, 7, 129, 7], 100), Err(DecodeError::Limit));
    }

    #[test]
    fn ascii_hex_decoding() {
        assert_eq!(ascii_hex(b"48 65\n6C6c6F>ignored").unwrap(), b"Hello");
        assert_eq!(ascii_hex(b"4>").unwrap(), vec![0x40]);
        assert_eq!(ascii_hex(b"414").unwrap(), vec![0x41]);
        assert!(ascii_hex(b"4G>").is_err());
    }

    #[test]
    fn ascii85_decoding() {
        let diag = Diagnostics::new();
        assert_eq!(
            ascii85(b"87cURD]i,\"Ebo80~>", &diag).unwrap(),
            b"Hello World!"
        );
        assert_eq!(ascii85(b"z~>", &diag).unwrap(), vec![0, 0, 0, 0]);
        assert_eq!(ascii85(b"9jqo^~>", &diag).unwrap(), b"Man ");
        assert_eq!(ascii85(b"9jqo~>", &diag).unwrap(), b"Man");
        assert!(ascii85(b"9j{", &diag).is_err());
        ascii85(b"9~>", &diag).unwrap();
        assert!(diag.has("filter.a85_partial"));
    }

    #[test]
    fn run_length_decoding() {
        assert_eq!(
            run_length(&[2, b'a', b'b', b'c', 254, b'z', 128, 9], usize::MAX).unwrap(),
            b"abczzz"
        );
        assert!(run_length(&[5, b'a'], usize::MAX).is_err());
        assert!(run_length(&[200], usize::MAX).is_err());
    }

    #[test]
    fn decode_chain_with_abbreviations_and_unknown_filters() {
        let diag = Diagnostics::new();
        let identity = |o: &Object| o.clone();
        let mut dict = Dict::new();
        dict.put(b"Filter", Object::name(b"FlateDecode"));
        let packed = zlib(b"abc");
        assert_eq!(
            decode(&dict, &packed, &identity, &diag, &Limits::default()).unwrap(),
            b"abc"
        );

        let mut dict = Dict::new();
        dict.put(b"Filter", Object::name(b"Bogus"));
        assert_eq!(
            decode(&dict, b"raw", &identity, &diag, &Limits::default()).unwrap(),
            b"raw"
        );
        assert!(diag.has("filter.unknown"));

        // /F wins over /Filter; a non-name /F (a file spec) means no filter.
        let mut dict = Dict::new();
        dict.put(b"Filter", Object::name(b"FlateDecode"));
        dict.put(b"F", Object::String(std::sync::Arc::from(&b"file.bin"[..])));
        assert_eq!(
            decode(&dict, b"raw", &identity, &diag, &Limits::default()).unwrap(),
            b"raw"
        );
    }
}
