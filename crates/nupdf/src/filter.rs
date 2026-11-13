// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-stream.c (build_filter,
// build_compression_params, pdf_open_filter, pdf_guess_filter_length),
// source/pdf/pdf-colorspace.c (pdf_guess_colorspace_components),
// source/fitz/compressed-buffer.c (fz_open_image_decomp_stream),
// source/fitz/filter-basic.c (ASCIIHex, ASCII85, RunLength, RC4 and AES),
// source/fitz/filter-flate.c, source/fitz/filter-predict.c,
// source/fitz/filter-lzw.c and source/fitz/filter-brotli.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Stream filters, as stages of a `Stream`. Each keeps MuPDF's buffer size
//! and its way of asking for input, since both decide what a damaged
//! stream yields: a filter that fails loses the buffer it was filling.

use brotli_decompressor::{BrotliDecompressStream, BrotliResult, BrotliState, StandardAlloc};
use flate2::{Decompress, FlushDecompress, Status};

use crate::crypt::{Aes, Arc4, Cipher, Crypt};
use crate::diag::{Diagnostics, report};
use crate::limits::Limits;
use crate::object::{Dict, Object};
use crate::stream::{Next, ReadError, Source, Stream};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The filter exists in PDF but nupdf does not decode it yet.
    Unsupported(String),
    /// Opening or reading failed where MuPDF also fails; the report line.
    Failed(String),
    Limit,
}

impl DecodeError {
    /// The line MuPDF prints when this error is caught.
    pub fn report(&self) -> String {
        match self {
            DecodeError::Failed(m) => report::line(m),
            DecodeError::Limit => format!("limit error: {self}"),
            other => format!("format error: {other}"),
        }
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Unsupported(name) => write!(f, "unsupported filter {name}"),
            DecodeError::Failed(msg) => f.write_str(report::strip(msg)),
            DecodeError::Limit => f.write_str("decoded stream exceeds the size limit"),
        }
    }
}

impl From<ReadError> for DecodeError {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::Bomb => DecodeError::Failed(report::format("compression bomb detected")),
            ReadError::Limit => DecodeError::Limit,
        }
    }
}

/// What a `/Crypt` filter needs: the document's security handler and the
/// object the stream belongs to.
#[derive(Debug, Clone, Copy)]
pub struct CryptContext<'a> {
    pub crypt: &'a Crypt,
    pub num: i32,
    pub generation: i32,
}

/// `pdf_open_crypt_imp`: decrypts `chain` with `cipher`, or leaves it.
pub fn open_cipher<'a>(chain: Stream<'a>, cipher: Option<Cipher>) -> Result<Stream<'a>, String> {
    let diag = chain.diag();
    match cipher {
        None => Ok(chain),
        Some(Cipher::Rc4(key)) => Ok(Stream::new(
            Rc4Filter {
                chain,
                arc4: Arc4::new(&key),
            },
            diag,
        )),
        Some(Cipher::Aes(key)) => {
            let bits = key.len() as i32 * 8;
            let aes = Aes::new(&key, bits)
                .ok_or_else(|| format!("argument error: aes invalid key size ({bits})"))?;
            Ok(Stream::new(
                AesFilter {
                    chain,
                    aes,
                    iv: [0; 16],
                    ivcount: 0,
                    block: [0; 16],
                    rp: 0,
                    wp: 0,
                },
                diag,
            ))
        }
    }
}

const IMAGE_FILTERS: [&[u8]; 6] = [
    b"CCITTFaxDecode",
    b"CCF",
    b"DCTDecode",
    b"DCT",
    b"JBIG2Decode",
    b"JPXDecode",
];

/// `pdf_load_stream` for an object a filter's parameters refer to
/// (/JBIG2Globals): its decoded data, or `None` when it is not a stream.
pub type LoadStream<'r> = &'r dyn Fn(&Object) -> Option<Result<Vec<u8>, DecodeError>>;

/// `pdf_open_filter`: `raw` through the stream's filter chain. With
/// `might_be_image` false, image filters yield nothing, as in MuPDF.
pub fn open_filters<'a>(
    raw: Stream<'a>,
    dict: &Dict,
    resolve: &dyn Fn(&Object) -> Object,
    load: Option<LoadStream<'_>>,
    crypt: Option<CryptContext<'_>>,
    might_be_image: bool,
) -> Result<Stream<'a>, DecodeError> {
    // pdf_dict_geta: the abbreviation wins when both are present.
    let filters = dict
        .get(b"F")
        .or_else(|| dict.get(b"Filter"))
        .map_or(Object::Null, resolve);
    let params = dict.get(b"DP").or_else(|| dict.get(b"DecodeParms"));
    match &filters {
        Object::Name(_) => {
            let p = params.map_or(Object::Null, resolve);
            build_filter(raw, &filters, &p, resolve, load, crypt, might_be_image)
        }
        Object::Array(items) if !items.is_empty() => {
            let params = params.map_or(Object::Null, resolve);
            let mut chain = raw;
            for (i, f) in items.iter().enumerate() {
                let p = params
                    .as_array()
                    .and_then(|ps| ps.get(i))
                    .map_or(Object::Null, resolve);
                chain = build_filter(chain, &resolve(f), &p, resolve, load, crypt, might_be_image)?;
            }
            Ok(chain)
        }
        _ => Ok(raw),
    }
}

/// `build_filter` with `build_compression_params` and
/// `fz_open_image_decomp_stream`.
fn build_filter<'a>(
    chain: Stream<'a>,
    filter: &Object,
    params: &Object,
    resolve: &dyn Fn(&Object) -> Object,
    load: Option<LoadStream<'_>>,
    crypt: Option<CryptContext<'_>>,
    might_be_image: bool,
) -> Result<Stream<'a>, DecodeError> {
    let diag = chain.diag();
    let name = filter.as_name().unwrap_or_default();
    if !might_be_image && IMAGE_FILTERS.contains(&name) {
        diag.warn(
            "filter.image_only",
            "Can't open image only stream for non-image purposes",
        );
        return Ok(Stream::memory(b"", diag));
    }
    let int = |key: &[u8], default: i32| {
        params
            .as_dict()
            .and_then(|d| d.get(key))
            .map_or(default, |v| resolve(v).to_int_or(default))
    };
    let predictor = |head: Stream<'a>| -> Result<Stream<'a>, DecodeError> {
        let p = int(b"Predictor", 1);
        if p > 1 {
            open_predict(
                head,
                p,
                int(b"Columns", 1),
                int(b"Colors", 1),
                int(b"BitsPerComponent", 8),
            )
        } else {
            Ok(head)
        }
    };
    match name {
        b"CCITTFaxDecode" | b"CCF" => {
            let flag = |key: &[u8], default: bool| match params
                .as_dict()
                .and_then(|d| d.get(key))
                .map(resolve)
            {
                Some(Object::Bool(b)) => b,
                _ => default,
            };
            let p = crate::fax::FaxParams {
                k: int(b"K", 0),
                end_of_line: flag(b"EndOfLine", false),
                encoded_byte_align: flag(b"EncodedByteAlign", false),
                columns: int(b"Columns", 1728),
                rows: int(b"Rows", 0),
                end_of_block: flag(b"EndOfBlock", true),
                black_is_1: flag(b"BlackIs1", false),
            };
            crate::fax::open(chain, p).map_err(DecodeError::Failed)
        }
        b"DCTDecode" | b"DCT" => Ok(crate::dct::open(chain, int(b"ColorTransform", -1))),
        b"JBIG2Decode" => {
            let mut globals = None;
            let g = params.as_dict().and_then(|d| d.get(b"JBIG2Globals"));
            if let Some(g) = g.filter(|g| !matches!(g, Object::Null)) {
                match load.and_then(|load| load(g)) {
                    Some(data) => {
                        globals = crate::jbig2::load_globals(&data?, diag)
                            .map_err(DecodeError::Failed)?;
                    }
                    None => diag.warn(
                        "filter.jbig2_globals_not_stream",
                        "jbig2 globals is not a stream, skipping globals",
                    ),
                }
            }
            Ok(crate::jbig2::open(chain, globals))
        }
        b"RunLengthDecode" | b"RL" => Ok(open_rld(chain)),
        b"FlateDecode" | b"Fl" => predictor(Stream::new(
            Flate {
                chain,
                z: Some(inflater()),
                bad: None,
                done: false,
            },
            diag,
        )),
        b"BrotliDecode" | b"Br" => predictor(Stream::new(
            Brotli {
                chain,
                state: Box::new(BrotliState::new(
                    StandardAlloc::default(),
                    StandardAlloc::default(),
                    StandardAlloc::default(),
                )),
                done: false,
            },
            diag,
        )),
        b"LZWDecode" | b"LZW" => {
            predictor(Stream::new(Lzw::new(chain, int(b"EarlyChange", 1)), diag))
        }
        // Decoded by the image code; for everything else, the raw data.
        b"JPXDecode" => Ok(chain),
        b"ASCIIHexDecode" | b"AHx" => Ok(Stream::new(AsciiHex { chain, eod: false }, diag)),
        b"ASCII85Decode" | b"A85" => Ok(Stream::new(Ascii85 { chain, eod: false }, diag)),
        b"Crypt" => {
            let Some(cx) = crypt else {
                diag.warn(
                    "filter.crypt_unencrypted",
                    "crypt filter in unencrypted document",
                );
                return Ok(chain);
            };
            let named = params
                .as_dict()
                .and_then(|d| d.get(b"Name"))
                .map(resolve)
                .and_then(|n| n.as_name().map(<[u8]>::to_vec));
            match named {
                // pdf_open_crypt_with_filter
                Some(n) if n != b"Identity" => {
                    let cf = cx
                        .crypt
                        .parse_filter(&n, diag)
                        .map_err(DecodeError::Failed)?;
                    open_cipher(chain, cx.crypt.stream_cipher(&cf, cx.num, cx.generation))
                        .map_err(DecodeError::Failed)
                }
                _ => Ok(chain),
            }
        }
        other => {
            diag.warn(
                "filter.unknown",
                format!("unknown filter name ({})", String::from_utf8_lossy(other)),
            );
            Ok(chain)
        }
    }
}

/// Whether the filter chain (/F, else /Filter) names an image filter.
pub fn has_image_filter(dict: &Dict, resolve: &dyn Fn(&Object) -> Object) -> bool {
    let is_image = |o: &Object| o.as_name().is_some_and(|n| IMAGE_FILTERS.contains(&n));
    match dict.get(b"F").or_else(|| dict.get(b"Filter")).map(resolve) {
        Some(Object::Array(items)) => items.iter().any(|f| is_image(&resolve(f))),
        Some(f) => is_image(&f),
        None => false,
    }
}

/// `pdf_guess_filter_length` over /Filter (the full key only, as MuPDF).
pub fn guess_length(len: usize, dict: &Dict, resolve: &dyn Fn(&Object) -> Object) -> usize {
    let guess = |len: usize, name: &[u8]| -> usize {
        match name {
            b"ASCIIHexDecode" => return len / 2,
            b"ASCII85Decode" => return len * 4 / 5,
            _ => {}
        }
        let n = match name {
            b"FlateDecode" | b"RunLengthDecode" => len.saturating_mul(3),
            b"BrotliDecode" => len.saturating_mul(4),
            b"LZWDecode" => len.saturating_mul(2),
            _ => len,
        };
        n.max(len)
    };
    match dict.get(b"Filter").map(resolve) {
        Some(Object::Name(n)) => guess(len, &n),
        Some(Object::Array(items)) => items.iter().fold(guess(len, b""), |l, f| {
            guess(l, resolve(f).as_name().unwrap_or_default())
        }),
        _ => guess(len, b""),
    }
}

/// The image-size bound `pdf_load_image_stream` uses as the compression
/// bomb limit for /Subtype /Image streams, or 0.
pub fn worst_case(dict: &Dict, len: usize, resolve: &dyn Fn(&Object) -> Object) -> usize {
    let get = |k: &[u8]| dict.get(k).map_or(Object::Null, resolve);
    if !get(b"Subtype").is_name(b"Image") {
        return 0;
    }
    let w = get(b"Width").to_int64();
    let h = get(b"Height").to_int64();
    let bpc = i64::from(get(b"BitsPerComponent").to_int_or(8));
    let nc = i64::from(colorspace_components(
        dict.get(b"ColorSpace").unwrap_or(&Object::Null),
        resolve,
        0,
    ));
    if w > 0 && h > 0 && bpc > 0 && nc > 0 {
        let row = (w.saturating_mul(nc).saturating_mul(bpc).saturating_add(7)) >> 3;
        let size = usize::try_from(row.saturating_mul(h)).unwrap_or(usize::MAX);
        size.max(len)
    } else {
        0
    }
}

/// `pdf_guess_colorspace_components`. MuPDF compares names by identity,
/// so only direct names count.
fn colorspace_components(cs: &Object, resolve: &dyn Fn(&Object) -> Object, depth: u8) -> i32 {
    let name_count = |n: &[u8]| match n {
        b"G" | b"DeviceGray" | b"I" | b"Indexed" | b"CalGray" | b"Separation" => Some(1),
        b"RGB" | b"DeviceRGB" | b"CalRGB" | b"Lab" => Some(3),
        b"CMYK" | b"DeviceCMYK" | b"CalCMYK" => Some(4),
        _ => None,
    };
    match cs {
        Object::Name(n) => match n.as_ref() {
            b"Pattern" | b"G" | b"DeviceGray" => 1,
            b"RGB" | b"DeviceRGB" => 3,
            b"CMYK" | b"DeviceCMYK" => 4,
            _ => 0,
        },
        _ => {
            let Object::Array(items) = resolve(cs) else {
                return 0;
            };
            let Some(Object::Name(first)) = items.first() else {
                return 0;
            };
            if let Some(n) = name_count(first) {
                return n;
            }
            let second = items.get(1).map_or(Object::Null, resolve);
            match first.as_ref() {
                b"ICCBased" => second
                    .as_dict()
                    .and_then(|d| d.get(b"N"))
                    .map_or(0, |n| resolve(n).to_int())
                    .clamp(0, 4),
                b"DeviceN" => second.as_array().map_or(0, |a| a.len() as i32).clamp(0, 32),
                b"Pattern" if depth < 8 => match items.get(1) {
                    Some(inner) => {
                        let resolved = resolve(inner);
                        if resolved
                            .as_array()
                            .and_then(|a| a.first())
                            .is_some_and(|f| f.is_name(b"Pattern"))
                        {
                            0
                        } else {
                            colorspace_components(inner, resolve, depth + 1)
                        }
                    }
                    None => 0,
                },
                _ => 0,
            }
        }
    }
}

/// Decodes `raw` through the filters of `dict`, read whole as MuPDF's
/// `pdf_load_stream` does (`fz_read_best`).
pub fn decode(
    dict: &Dict,
    raw: &[u8],
    resolve: &dyn Fn(&Object) -> Object,
    diag: &Diagnostics,
    limits: &Limits,
) -> Result<Vec<u8>, DecodeError> {
    let mut s = open_filters(Stream::memory(raw, diag), dict, resolve, None, None, true)?;
    Ok(s.read_best(0, 0, limits.max_decoded_bytes)?)
}

// ---- RC4 and AES ---------------------------------------------------------

struct Rc4Filter<'a> {
    chain: Stream<'a>,
    arc4: Arc4,
}

impl Source for Rc4Filter<'_> {
    fn next(&mut self, max: usize, out: &mut Vec<u8>, _: &Diagnostics) -> Result<Next, String> {
        let n = self.chain.available(max).min(256);
        if n == 0 {
            return Ok(Next::Eof);
        }
        out.extend_from_slice(&self.chain.data()[..n]);
        self.arc4.apply(out);
        self.chain.consume(n);
        Ok(Next::Data)
    }
}

/// `fz_open_aesd`: a 16-byte IV, then whole blocks; padding is stripped from
/// the block that ends the input. It produces at most what the reader asks
/// for, so a reader that takes one byte at a time sees every block before a
/// failing one.
struct AesFilter<'a> {
    chain: Stream<'a>,
    aes: Aes,
    iv: [u8; 16],
    ivcount: usize,
    block: [u8; 16],
    rp: usize,
    wp: usize,
}

impl Source for AesFilter<'_> {
    fn next(&mut self, max: usize, out: &mut Vec<u8>, _: &Diagnostics) -> Result<Next, String> {
        let max = max.clamp(1, 256);
        while self.ivcount < 16 {
            let Some(c) = self.chain.read_byte() else {
                return Err(report::format("premature end in aes filter"));
            };
            self.iv[self.ivcount] = c;
            self.ivcount += 1;
        }
        let take = |f: &mut Self, out: &mut Vec<u8>| {
            let n = (f.wp - f.rp).min(max - out.len());
            out.extend_from_slice(&f.block[f.rp..f.rp + n]);
            f.rp += n;
        };
        take(self, out);
        while out.len() < max {
            let mut block = [0u8; 16];
            let n = self.chain.read(&mut block);
            if n == 0 {
                break;
            }
            if n < 16 {
                return Err(report::format("partial block in aes filter"));
            }
            self.aes.cbc_decrypt(&mut self.iv, &mut block);
            self.block = block;
            self.rp = 0;
            self.wp = 16;
            if self.chain.is_eof() {
                let pad = block[15];
                if !(1..=16).contains(&pad) {
                    return Err(report::format(&format!("aes padding out of range: {pad}")));
                }
                self.wp -= usize::from(pad);
            }
            take(self, out);
        }
        Ok(if out.is_empty() {
            Next::Eof
        } else {
            Next::Data
        })
    }
}

// ---- Flate ---------------------------------------------------------------

thread_local! {
    /// Inflaters this thread has finished with, reset for the next stream.
    /// Making one allocates zlib's state and window, which on many threads
    /// at once waits on the allocator.
    static INFLATERS: std::cell::RefCell<Vec<Decompress>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// How many finished inflaters a thread keeps.
const KEPT_INFLATERS: usize = 4;

/// A zlib inflater as `inflateInit` leaves it.
fn inflater() -> Decompress {
    INFLATERS
        .with(|kept| kept.try_borrow_mut().ok().and_then(|mut k| k.pop()))
        .unwrap_or_else(|| Decompress::new(true))
}

/// `fz_open_flated`: fills a 4096-byte buffer per call, feeding zlib
/// whatever the stage below has buffered.
struct Flate<'a> {
    chain: Stream<'a>,
    /// Always there until the filter is dropped, when it is kept for the
    /// next stream.
    z: Option<Decompress>,
    /// zlib's message after an error it was told to ignore: zlib keeps
    /// failing the same way afterwards.
    bad: Option<String>,
    done: bool,
}

impl Drop for Flate<'_> {
    fn drop(&mut self) {
        if let Some(mut z) = self.z.take() {
            z.reset(true);
            INFLATERS.with(|kept| {
                if let Ok(mut kept) = kept.try_borrow_mut()
                    && kept.len() < KEPT_INFLATERS
                {
                    kept.push(z);
                }
            });
        }
    }
}

impl Source for Flate<'_> {
    fn next(&mut self, _: usize, out: &mut Vec<u8>, diag: &Diagnostics) -> Result<Next, String> {
        let Some(z) = self.z.as_mut().filter(|_| !self.done) else {
            return Ok(Next::Eof);
        };
        out.resize(4096, 0);
        let mut produced = 0;
        while produced < out.len() {
            let avail = self.chain.available(1);
            let (result, consumed, made) = match &self.bad {
                Some(msg) => (Err(Some(msg.clone())), 0, 0),
                None => {
                    let (in0, out0) = (z.total_in(), z.total_out());
                    let r = z.decompress(
                        &self.chain.data()[..avail],
                        &mut out[produced..],
                        FlushDecompress::Sync,
                    );
                    let consumed = (z.total_in() - in0) as usize;
                    let made = (z.total_out() - out0) as usize;
                    let r = r.map_err(|e| {
                        if e.needs_dictionary().is_some() {
                            None
                        } else {
                            Some(e.message().unwrap_or("(null)").to_string())
                        }
                    });
                    (r, consumed, made)
                }
            };
            self.chain.consume(consumed);
            produced += made;
            let left = avail - consumed.min(avail);
            match result {
                Ok(Status::StreamEnd) => {
                    self.done = true;
                    break;
                }
                Ok(Status::BufError) => {
                    diag.warn(
                        "filter.flate_truncated",
                        "premature end of data in flate filter",
                    );
                    break;
                }
                Ok(Status::Ok) => {}
                Err(Some(msg)) if left == 0 => {
                    diag.warn("filter.flate_error", format!("ignoring zlib error: {msg}"));
                    self.bad = Some(msg);
                    break;
                }
                Err(Some(msg)) if msg == "incorrect data check" => {
                    diag.warn("filter.flate_error", format!("ignoring zlib error: {msg}"));
                    self.chain.consume(left);
                    self.bad = Some(msg);
                    break;
                }
                Err(msg) => {
                    return Err(format!(
                        "library error: zlib error: {}",
                        msg.as_deref().unwrap_or("(null)")
                    ));
                }
            }
        }
        out.truncate(produced);
        Ok(if out.is_empty() {
            Next::Eof
        } else {
            Next::Data
        })
    }
}

// ---- Brotli --------------------------------------------------------------

/// `fz_open_brotlid`.
struct Brotli<'a> {
    chain: Stream<'a>,
    state: Box<BrotliState<StandardAlloc, StandardAlloc, StandardAlloc>>,
    done: bool,
}

impl Source for Brotli<'_> {
    fn next(&mut self, _: usize, out: &mut Vec<u8>, _: &Diagnostics) -> Result<Next, String> {
        if self.done {
            return Ok(Next::Eof);
        }
        out.resize(4096, 0);
        let mut produced = 0;
        while produced < out.len() {
            let avail = self.chain.available(1);
            let mut available_in = avail;
            let mut input_offset = 0;
            let mut available_out = out.len() - produced;
            let mut total_out = 0;
            let result = BrotliDecompressStream(
                &mut available_in,
                &mut input_offset,
                &self.chain.data()[..avail],
                &mut available_out,
                &mut produced,
                out,
                &mut total_out,
                &mut self.state,
            );
            self.chain.consume(input_offset);
            match result {
                BrotliResult::ResultSuccess => {
                    self.done = true;
                    break;
                }
                BrotliResult::ResultFailure => {
                    return Err("library error: brotli decompression error".into());
                }
                // MuPDF loops forever here on a truncated stream; nupdf
                // treats it as the end.
                BrotliResult::NeedsMoreInput if avail == 0 => break,
                _ => {}
            }
        }
        out.truncate(produced);
        Ok(if out.is_empty() {
            Next::Eof
        } else {
            Next::Data
        })
    }
}

// ---- LZW -----------------------------------------------------------------

const LZW_MAX_BITS: u32 = 12;
const LZW_NUM_CODES: i32 = 1 << LZW_MAX_BITS;

#[derive(Clone, Copy, Default)]
struct LzwCode {
    prev: i32,
    length: u16,
    value: u8,
    first_char: u8,
}

/// `fz_open_lzwd` with 9-bit minimum codes, as PDF uses.
struct Lzw<'a> {
    chain: Stream<'a>,
    eod: bool,
    early_change: i32,
    min_bits: u32,
    code_bits: u32,
    old_code: i32,
    next_code: i32,
    table: Vec<LzwCode>,
    string: Vec<u8>,
    rp: usize,
}

impl<'a> Lzw<'a> {
    fn new(chain: Stream<'a>, early_change: i32) -> Lzw<'a> {
        let min_bits = 9;
        let clear = 1i32 << (min_bits - 1);
        let mut table = vec![
            LzwCode {
                prev: -1,
                ..LzwCode::default()
            };
            LZW_NUM_CODES as usize
        ];
        for (i, code) in table.iter_mut().enumerate().take(clear as usize) {
            *code = LzwCode {
                prev: -1,
                length: 1,
                value: i as u8,
                first_char: i as u8,
            };
        }
        Lzw {
            chain,
            eod: false,
            early_change,
            min_bits,
            code_bits: min_bits,
            old_code: -1,
            next_code: clear + 2,
            table,
            string: Vec::new(),
            rp: 0,
        }
    }

    fn clear(&self) -> i32 {
        1 << (self.min_bits - 1)
    }
}

impl Source for Lzw<'_> {
    fn next(&mut self, max: usize, out: &mut Vec<u8>, diag: &Diagnostics) -> Result<Next, String> {
        // At most the reader's hint, up to the 4 KiB buffer, as next_lzwd:
        // read a byte at a time, a failing code loses only that byte.
        let cap = max.clamp(1, 4096);
        let out_of_range = || report::format("out of range code encountered in lzw decode");
        let (clear, eod_code, first) = (self.clear(), self.clear() + 1, self.clear() + 2);
        let copy = |f: &mut Self, out: &mut Vec<u8>| {
            let n = (f.string.len() - f.rp).min(cap - out.len());
            out.extend_from_slice(&f.string[f.rp..f.rp + n]);
            f.rp += n;
        };
        copy(self, out);
        while out.len() < cap {
            if self.eod {
                return Ok(Next::Eof);
            }
            if self.chain.is_eof_bits() {
                diag.warn("filter.lzw_premature", "premature end in lzw decode");
                self.eod = true;
                break;
            }
            let mut code = self.chain.read_bits(self.code_bits) as i32;
            if code == eod_code {
                self.eod = true;
                break;
            }
            if self.next_code > LZW_NUM_CODES && code != clear {
                diag.warn(
                    "filter.lzw_missing_clear",
                    "missing clear code in lzw decode",
                );
                code = clear;
            }
            if code == clear {
                self.code_bits = self.min_bits;
                self.next_code = first;
                self.old_code = -1;
                continue;
            }
            if self.old_code == -1 {
                // Right after a clear, a code must already be in the table.
                if code >= first {
                    return Err(out_of_range());
                }
                self.old_code = code;
            } else if self.next_code == LZW_NUM_CODES {
                diag.warn(
                    "filter.lzw_out_of_range",
                    "tolerating a single out of range code in lzw decode",
                );
                self.next_code += 1;
            } else if code > self.next_code || self.next_code >= LZW_NUM_CODES {
                return Err(out_of_range());
            } else {
                let next = self.next_code as usize;
                let old = self.table[self.old_code as usize];
                // A code cut short by the end of the data reads as -1; MuPDF
                // then reads table[-1], the fields just before the table,
                // whose first_char is the top byte of next_code: 0.
                let value = if code < 0 {
                    0
                } else if code < self.next_code {
                    self.table[code as usize].first_char
                } else {
                    old.first_char
                };
                self.table[next] = LzwCode {
                    prev: self.old_code,
                    length: old.length.saturating_add(1),
                    value,
                    first_char: old.first_char,
                };
                self.next_code += 1;
                if self.next_code > (1 << self.code_bits) - self.early_change - 1 {
                    self.code_bits = (self.code_bits + 1).min(LZW_MAX_BITS);
                }
                self.old_code = code;
            }

            // The code's string, written back to front.
            self.string.clear();
            self.rp = 0;
            if code >= clear {
                let len = usize::from(self.table[code as usize].length);
                self.string.resize(len, 0);
                let mut c = code;
                let mut i = len;
                while i > 0 {
                    i -= 1;
                    let entry = self.table[c as usize];
                    self.string[i] = entry.value;
                    c = entry.prev;
                    if c < 0 {
                        break;
                    }
                }
            } else {
                self.string.push(code as u8);
            }
            copy(self, out);
        }
        Ok(if out.is_empty() {
            Next::Eof
        } else {
            Next::Data
        })
    }
}

// ---- ASCIIHex, ASCII85, RunLength ---------------------------------------

/// "bad data in <filter>: '%c'" as MuPDF prints it: a NUL ends the line
/// there, and a byte above 127 is not text, so it reads back as U+FFFD.
fn bad_data(filter: &str, c: u8) -> String {
    match c {
        0 => format!("bad data in {filter}: '"),
        0x80.. => format!("bad data in {filter}: '\u{FFFD}'"),
        _ => format!("bad data in {filter}: '{}'", c as char),
    }
}

/// MuPDF's whitespace for these filters.
fn is_white(c: u8) -> bool {
    matches!(c, b'\n' | b'\r' | b'\t' | b' ' | 0 | 0x0c | 0x08 | 0x7f)
}

/// `fz_open_ahxd`.
struct AsciiHex<'a> {
    chain: Stream<'a>,
    eod: bool,
}

impl Source for AsciiHex<'_> {
    fn next(&mut self, max: usize, out: &mut Vec<u8>, _: &Diagnostics) -> Result<Next, String> {
        let max = max.clamp(1, 256);
        let mut high: Option<u8> = None;
        while out.len() < max {
            if self.eod {
                break;
            }
            let Some(c) = self.chain.read_byte() else {
                break;
            };
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
                self.eod = true;
                break;
            } else if !is_white(c) {
                return Err(report::format(&bad_data("ahxd", c)));
            }
        }
        Ok(if out.is_empty() {
            Next::Eof
        } else {
            Next::Data
        })
    }
}

/// `fz_open_a85d`.
struct Ascii85<'a> {
    chain: Stream<'a>,
    eod: bool,
}

impl Source for Ascii85<'_> {
    fn next(&mut self, max: usize, out: &mut Vec<u8>, diag: &Diagnostics) -> Result<Next, String> {
        if self.eod {
            return Ok(Next::Eof);
        }
        let max = max.clamp(1, 256);
        let mut word: u32 = 0;
        let mut count = 0;
        while out.len() < max {
            let Some(c) = self.chain.read_byte() else {
                break;
            };
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
                out.extend_from_slice(&[0; 4]);
            } else if c == b'~' {
                if self.chain.read_byte() != Some(b'>') {
                    diag.warn("filter.a85_bad_eod", "bad eod marker in a85d");
                }
                let tail = |w: u32, n: usize| w.to_be_bytes()[..n].to_vec();
                match count {
                    1 => diag.warn("filter.a85_partial", "partial final byte in a85d"),
                    2 => out.extend(tail(
                        word.wrapping_mul(85 * 85 * 85).wrapping_add(0xff_ffff),
                        1,
                    )),
                    3 => out.extend(tail(word.wrapping_mul(85 * 85).wrapping_add(0xffff), 2)),
                    4 => out.extend(tail(word.wrapping_mul(85).wrapping_add(0xff), 3)),
                    _ => {}
                }
                self.eod = true;
                break;
            } else if !is_white(c) {
                return Err(report::format(&bad_data("a85d", c)));
            }
        }
        Ok(if out.is_empty() {
            Next::Eof
        } else {
            Next::Data
        })
    }
}

/// `fz_open_rld`, which refuses to stack on another run-length decoder.
fn open_rld(chain: Stream<'_>) -> Stream<'_> {
    let diag = chain.diag();
    let stacked = chain.is_rld();
    let mut s = Stream::new(
        RunLength {
            chain,
            run: 0,
            n: 0,
            c: 0,
        },
        diag,
    );
    if stacked {
        diag.warn("filter.rle_bomb", "RLE bomb defused");
        s.finish();
    }
    s
}

struct RunLength<'a> {
    chain: Stream<'a>,
    run: i32,
    n: i32,
    c: u8,
}

impl Source for RunLength<'_> {
    fn next(&mut self, max: usize, out: &mut Vec<u8>, _: &Diagnostics) -> Result<Next, String> {
        if self.run == 128 {
            return Ok(Next::Eof);
        }
        let premature = || report::format("premature end of data in run length decode");
        let max = max.clamp(1, 256);
        while out.len() < max {
            if self.run == 128 {
                break;
            }
            if self.n == 0 {
                let Some(run) = self.chain.read_byte() else {
                    self.run = 128;
                    break;
                };
                self.run = i32::from(run);
                if self.run < 128 {
                    self.n = self.run + 1;
                }
                if self.run > 128 {
                    self.n = 257 - self.run;
                    self.c = self.chain.read_byte().ok_or_else(premature)?;
                }
            }
            if self.run < 128 {
                while out.len() < max && self.n > 0 {
                    out.push(self.chain.read_byte().ok_or_else(premature)?);
                    self.n -= 1;
                }
            }
            if self.run > 128 {
                while out.len() < max && self.n > 0 {
                    out.push(self.c);
                    self.n -= 1;
                }
            }
        }
        Ok(if out.is_empty() {
            Next::Eof
        } else {
            Next::Data
        })
    }

    fn is_rld(&self) -> bool {
        true
    }
}

// ---- Predictors ------------------------------------------------------------

/// `fz_open_predict`.
pub fn open_predict(
    chain: Stream<'_>,
    predictor: i32,
    columns: i32,
    colors: i32,
    bpc: i32,
) -> Result<Stream<'_>, DecodeError> {
    const MAX_COLORS: i32 = 32;
    let diag = chain.diag();
    let mut predictor = predictor.max(1);
    let columns = columns.max(1);
    let colors = colors.max(1);
    let bpc = if bpc < 1 { 8 } else { bpc };
    if ![1, 2, 4, 8, 16].contains(&bpc) {
        return Err(DecodeError::Failed(format!(
            "argument error: invalid number of bits per component: {bpc}"
        )));
    }
    if colors > MAX_COLORS {
        return Err(DecodeError::Failed(format!(
            "argument error: too many color components ({colors} > {MAX_COLORS})"
        )));
    }
    if columns >= i32::MAX / (bpc * colors) {
        return Err(DecodeError::Failed(format!(
            "limit error: too many columns lead to an integer overflow ({columns})"
        )));
    }
    if ![1, 2, 10, 11, 12, 13, 14, 15].contains(&predictor) {
        diag.warn(
            "filter.bad_predictor",
            format!("invalid predictor: {predictor}"),
        );
        predictor = 1;
    }
    let stride = ((bpc * colors * columns + 7) / 8) as usize;
    let bpp = ((bpc * colors + 7) / 8) as usize;
    Ok(Stream::new(
        Predict {
            chain,
            predictor,
            columns: columns as usize,
            colors: colors as usize,
            bpc: bpc as usize,
            stride,
            bpp,
            input: vec![0; stride + 1],
            output: vec![0; stride],
            reference: vec![0; stride],
            rp: 0,
            wp: 0,
        },
        diag,
    ))
}

struct Predict<'a> {
    chain: Stream<'a>,
    predictor: i32,
    columns: usize,
    colors: usize,
    bpc: usize,
    stride: usize,
    bpp: usize,
    input: Vec<u8>,
    output: Vec<u8>,
    reference: Vec<u8>,
    rp: usize,
    wp: usize,
}

impl Source for Predict<'_> {
    fn next(&mut self, max: usize, out: &mut Vec<u8>, diag: &Diagnostics) -> Result<Next, String> {
        let max = max.clamp(1, 4096);
        let png = usize::from(self.predictor >= 10);
        let copy = |f: &mut Self, out: &mut Vec<u8>| {
            let n = (f.wp - f.rp).min(max - out.len());
            out.extend_from_slice(&f.output[f.rp..f.rp + n]);
            f.rp += n;
        };
        copy(self, out);
        while out.len() < max {
            let want = self.stride + png;
            let n = self.chain.read(&mut self.input[..want]);
            if n == 0 {
                break;
            }
            match self.predictor {
                1 => self.output[..n].copy_from_slice(&self.input[..n]),
                2 => tiff_row(
                    &mut self.output,
                    &self.input,
                    self.columns,
                    self.colors,
                    self.bpc,
                ),
                _ => {
                    png_row(
                        &mut self.output,
                        &self.input[1..n],
                        &self.reference,
                        self.bpp,
                        self.input[0],
                        diag,
                    );
                    self.reference.copy_from_slice(&self.output);
                }
            }
            self.rp = 0;
            self.wp = n - png;
            copy(self, out);
        }
        Ok(if out.is_empty() {
            Next::Eof
        } else {
            Next::Data
        })
    }
}

fn png_row(
    out: &mut [u8],
    input: &[u8],
    reference: &[u8],
    bpp: usize,
    kind: u8,
    diag: &Diagnostics,
) {
    let len = input.len();
    let bpp = bpp.min(len);
    match kind {
        0 => out[..len].copy_from_slice(input),
        1 => {
            out[..bpp].copy_from_slice(&input[..bpp]);
            for i in bpp..len {
                out[i] = input[i].wrapping_add(out[i - bpp]);
            }
        }
        2 => {
            for i in 0..len {
                out[i] = input[i].wrapping_add(reference[i]);
            }
        }
        3 => {
            for i in 0..bpp {
                out[i] = input[i].wrapping_add(reference[i] / 2);
            }
            for i in bpp..len {
                let avg = (u32::from(out[i - bpp]) + u32::from(reference[i])) / 2;
                out[i] = input[i].wrapping_add(avg as u8);
            }
        }
        4 => {
            for i in 0..bpp {
                out[i] = input[i].wrapping_add(paeth(0, i32::from(reference[i]), 0) as u8);
            }
            for i in bpp..len {
                let p = paeth(
                    i32::from(out[i - bpp]),
                    i32::from(reference[i]),
                    i32::from(reference[i - bpp]),
                );
                out[i] = input[i].wrapping_add(p as u8);
            }
        }
        other => {
            diag.warn(
                "filter.png_predictor",
                format!("unknown png predictor {other}, treating as none"),
            );
            out[..len].copy_from_slice(input);
        }
    }
}

fn paeth(a: i32, b: i32, c: i32) -> i32 {
    // ac and bc are as in MuPDF; not a typo.
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
    let mut left = [0u32; 32];
    let left = &mut left[..colors];
    if bpc == 8 {
        for i in 0..columns {
            for (k, l) in left.iter_mut().enumerate() {
                let x = i * colors + k;
                *l = (u32::from(input[x]) + *l) & 0xff;
                out[x] = *l as u8;
            }
        }
        return;
    }
    let mask = (1u32 << bpc) - 1;
    if bpc < 8 {
        out.fill(0);
    }
    for i in 0..columns {
        for (k, l) in left.iter_mut().enumerate() {
            let x = i * colors + k;
            let c = (get_component(input, x, bpc) + *l) & mask;
            put_component(out, x, bpc, c);
            *l = c;
        }
    }
}

fn get_component(line: &[u8], x: usize, bpc: usize) -> u32 {
    let at = |i: usize| u32::from(line.get(i).copied().unwrap_or(0));
    match bpc {
        1 => (at(x >> 3) >> (7 - (x & 7))) & 1,
        2 => (at(x >> 2) >> ((3 - (x & 3)) << 1)) & 3,
        4 => (at(x >> 1) >> ((1 - (x & 1)) << 2)) & 15,
        8 => at(x),
        16 => (at(x << 1) << 8) + at((x << 1) + 1),
        _ => 0,
    }
}

fn put_component(buf: &mut [u8], x: usize, bpc: usize, value: u32) {
    let mut or = |i: usize, v: u32| {
        if let Some(b) = buf.get_mut(i) {
            *b |= v as u8;
        }
    };
    match bpc {
        1 => or(x >> 3, value << (7 - (x & 7))),
        2 => or(x >> 2, value << ((3 - (x & 3)) << 1)),
        4 => or(x >> 1, value << ((1 - (x & 1)) << 2)),
        8 => {
            if let Some(b) = buf.get_mut(x) {
                *b = value as u8;
            }
        }
        16 => {
            if let Some(b) = buf.get_mut(x << 1) {
                *b = (value >> 8) as u8;
            }
            if let Some(b) = buf.get_mut((x << 1) + 1) {
                *b = value as u8;
            }
        }
        _ => {}
    }
}

/// For tests: `data` decoded as MuPDF's `readStream` decodes the stream
/// `tests/data/oracle.py` makes of it (the same file: the stream is object
/// 1 with `extras` in its dictionary, `obj4` an optional second stream
/// that `$obj4` in `extras` refers to). Returns the decoded length, the
/// first 16 hex digits of its SHA-256 and the distinct warnings, in the
/// form of oracle.py's lines.
#[cfg(test)]
pub(crate) fn decode_as_mupdf(
    extras: &str,
    data: &[u8],
    obj4: Option<&[u8]>,
) -> (usize, String, Vec<String>) {
    use sha2::Digest;
    let extras = extras.replace("$obj4", if obj4.is_some() { "4 0 R" } else { "null" });
    let mut objs = vec![
        [
            format!("<< /Length {} {extras} >>\nstream\n", data.len()).as_bytes(),
            data,
            b"\nendstream",
        ]
        .concat(),
        b"<< /Type /Catalog /Pages 3 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
    ];
    if let Some(o) = obj4 {
        objs.push(
            [
                format!("<< /Length {} >>\nstream\n", o.len()).as_bytes(),
                o,
                b"\nendstream",
            ]
            .concat(),
        );
    }
    let mut file = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(file.len());
        file.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        file.extend_from_slice(o);
        file.extend_from_slice(b"\nendobj\n");
    }
    let xref = file.len();
    file.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        file.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    file.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 2 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    let doc = crate::Document::open(file).unwrap_or_else(|e| panic!("{e:?}"));
    // A failed load is what the oracle's script prints for the exception.
    let (out, error) = match doc.object(1) {
        Some(Object::Stream(s)) => match doc.stream_data(&s) {
            Ok(out) => (out, None),
            Err(e) => (
                Vec::new(),
                Some(format!("error {}", report::strip(&e.report()))),
            ),
        },
        _ => (Vec::new(), None),
    };
    let hash: String = sha2::Sha256::digest(&out)
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut warnings: Vec<String> = Vec::new();
    for m in doc
        .diagnostics()
        .snapshot()
        .into_iter()
        .map(|d| d.message)
        .chain(error)
    {
        if !warnings.contains(&m) {
            warnings.push(m);
        }
    }
    (out.len(), hash, warnings)
}

/// A stream case for [`check_against_mupdf`]: name, dictionary entries,
/// data, and the data of object 4 if any.
#[cfg(test)]
pub(crate) type StreamCase = (&'static str, String, Vec<u8>, Option<Vec<u8>>);

/// For tests: decodes every case with [`decode_as_mupdf`] and compares
/// it with `table`, MuPDF's results as `tests/data/oracle.py` prints them.
/// With the environment variable `env` naming a directory, the cases are
/// written there first (`<name>.bin`, with `.extras` and `.obj4`), for
/// oracle.py to read.
#[cfg(test)]
pub(crate) fn check_against_mupdf(
    env: &str,
    cases: &[StreamCase],
    table: &[(&str, usize, &str, &[&str])],
) {
    let dump = std::env::var_os(env);
    let mut wrong = Vec::new();
    for (name, extras, data, obj4) in cases {
        if let Some(dir) = &dump {
            let path = std::path::Path::new(dir).join(format!("{name}.bin"));
            let with = |ext: &str| {
                let mut p = path.clone().into_os_string();
                p.push(ext);
                p
            };
            std::fs::write(&path, data).unwrap_or_else(|e| panic!("{e}"));
            std::fs::write(with(".extras"), extras).unwrap_or_else(|e| panic!("{e}"));
            if let Some(o) = obj4 {
                std::fs::write(with(".obj4"), o).unwrap_or_else(|e| panic!("{e}"));
            }
        }
        let got = decode_as_mupdf(extras, data, obj4.as_deref());
        match table.iter().find(|c| c.0 == *name) {
            Some(&(_, len, hash, warnings))
                if len == got.0 && hash == got.1 && got.2 == warnings => {}
            Some(want) => wrong.push(format!("{name}:\n  got   {got:?}\n  MuPDF {want:?}")),
            None => wrong.push(format!("{name}: no MuPDF result (got {got:?})")),
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[cfg(test)]
mod tests {
    #[test]
    fn bad_data_reads_as_mupdf_prints_it() {
        assert_eq!(bad_data("a85d", b'x'), "bad data in a85d: 'x'");
        assert_eq!(bad_data("a85d", 0xb1), "bad data in a85d: '\u{FFFD}'");
        assert_eq!(bad_data("ahxd", 0), "bad data in ahxd: '");
    }

    use super::*;
    use std::io::Write;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// Decodes `input` with one filter and its parameters, read whole.
    fn run(
        filter: &str,
        params: Option<Dict>,
        input: &[u8],
        diag: &Diagnostics,
    ) -> Result<Vec<u8>, DecodeError> {
        let mut dict = Dict::new();
        dict.put(b"Filter", Object::name(filter.as_bytes()));
        if let Some(p) = params {
            dict.put(b"DecodeParms", Object::Dict(std::sync::Arc::new(p)));
        }
        decode(&dict, input, &|o| o.clone(), diag, &Limits::default())
    }

    fn predictor(predictor: i32, columns: i32, colors: i32, bpc: i32) -> Dict {
        let mut p = Dict::new();
        for (k, v) in [
            (&b"Predictor"[..], predictor),
            (b"Columns", columns),
            (b"Colors", colors),
            (b"BitsPerComponent", bpc),
        ] {
            p.put(k, Object::Int(i64::from(v)));
        }
        p
    }

    #[test]
    fn flate_inflates_and_tolerates_truncation() {
        let diag = Diagnostics::new();
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let packed = zlib(&data);
        assert_eq!(run("FlateDecode", None, &packed, &diag).unwrap(), data);
        assert!(diag.snapshot().is_empty());

        let cut = &packed[..packed.len() / 2];
        let partial = run("Fl", None, cut, &diag).unwrap();
        assert!(!partial.is_empty() && data.starts_with(&partial));
        assert!(diag.has("filter.flate_truncated"));
    }

    #[test]
    fn kept_inflaters_decode_the_next_stream_afresh() {
        // Streams on one thread reuse the inflaters of those before them,
        // whatever state those ended in: whole, cut short or broken.
        let data: Vec<u8> = (0..20000u32).map(|i| (i * 7 % 253) as u8).collect();
        let packed = zlib(&data);
        let other = zlib(b"another stream, read after the rest");
        let diag = Diagnostics::new();
        for _ in 0..3 {
            let _ = run("FlateDecode", None, &packed[..packed.len() / 3], &diag);
            let _ = run("FlateDecode", None, b"not zlib at all", &diag);
            let mut bad = packed.clone();
            bad[packed.len() / 2] ^= 0x55;
            let _ = run("FlateDecode", None, &bad, &diag);
            let fresh = Diagnostics::new();
            assert_eq!(run("FlateDecode", None, &packed, &fresh).unwrap(), data);
            assert_eq!(
                run("FlateDecode", None, &other, &fresh).unwrap(),
                b"another stream, read after the rest"
            );
            assert!(fresh.snapshot().is_empty());
        }
        assert!(INFLATERS.with(|k| k.borrow().len()) <= KEPT_INFLATERS);
    }

    #[test]
    fn flate_errors_end_the_data_like_mupdf() {
        // A bad checksum at the very end keeps everything.
        let diag = Diagnostics::new();
        let mut packed = zlib(b"hello hello hello");
        let n = packed.len();
        packed[n - 1] ^= 0xff;
        assert_eq!(
            run("FlateDecode", None, &packed, &diag).unwrap(),
            b"hello hello hello"
        );
        assert!(diag.has("filter.flate_error"));

        // A bad header with data left is an error, so the read ends there.
        let diag = Diagnostics::new();
        assert_eq!(
            run("FlateDecode", None, b"not zlib at all", &diag).unwrap(),
            b""
        );
        assert!(diag.has("stream.read_error"));
        assert!(
            diag.snapshot()
                .iter()
                .any(|d| d.message == "library error: zlib error: incorrect header check")
        );
    }

    #[test]
    fn flate_errors_lose_the_buffer_being_filled() {
        // 10000 bytes decode in 4096-byte buffers; corrupting the stream
        // late loses the buffer being filled, so whole buffers survive.
        let data: Vec<u8> = (0..10000u32).map(|i| (i * 7 % 256) as u8).collect();
        let mut packed = zlib(&data);
        let at = packed.len() - 40;
        packed[at] ^= 0x55;
        packed.extend_from_slice(b"trailing");
        let diag = Diagnostics::new();
        let out = run("FlateDecode", None, &packed, &diag).unwrap();
        assert_eq!(out.len() % 4096, 0, "{}", out.len());
        assert!(data.starts_with(&out));
    }

    #[test]
    fn png_and_tiff_predictors() {
        let diag = Diagnostics::new();
        let unpack =
            |input: &[u8], p: Dict| run("FlateDecode", Some(p), &zlib(input), &diag).unwrap();
        assert_eq!(
            unpack(&[2, 1, 2, 3, 2, 1, 1, 1], predictor(12, 3, 1, 8)),
            [1, 2, 3, 2, 3, 4]
        );
        assert_eq!(unpack(&[1, 5, 1, 1], predictor(11, 3, 1, 8)), [5, 6, 7]);
        assert_eq!(
            unpack(&[0, 10, 20, 3, 2, 2], predictor(10, 2, 1, 8)),
            [10, 20, 7, 15]
        );
        assert_eq!(
            unpack(&[0, 10, 20, 4, 1, 1], predictor(15, 2, 1, 8)),
            [10, 20, 11, 21]
        );
        // A partial last row is kept.
        assert_eq!(
            unpack(&[0, 1, 2, 3, 2, 1], predictor(12, 3, 1, 8)),
            [1, 2, 3, 2]
        );
        assert_eq!(
            unpack(&[1, 1, 1, 5, 5, 5], predictor(2, 3, 1, 8)),
            [1, 2, 3, 5, 10, 15]
        );
        assert_eq!(unpack(&[0x11], predictor(2, 2, 1, 4)), [0x12]);
    }

    #[test]
    fn predictor_parameters_are_checked_when_opening() {
        let diag = Diagnostics::new();
        let packed = zlib(&[0]);
        assert!(run("FlateDecode", Some(predictor(12, 1, 1, 3)), &packed, &diag).is_err());
        assert!(run("FlateDecode", Some(predictor(12, 1, 33, 8)), &packed, &diag).is_err());
        let out = run(
            "FlateDecode",
            Some(predictor(99, 1, 1, 8)),
            &zlib(&[7, 8]),
            &diag,
        );
        assert_eq!(out.unwrap(), [7, 8]);
        assert!(diag.has("filter.bad_predictor"));
    }

    #[test]
    fn lzw_read_a_byte_at_a_time_loses_only_the_byte_that_fails() {
        // Nine-bit codes: clear, 'a', 'b', 'c', then one past the table.
        let mut bits = 0u64;
        for code in [256u64, 97, 98, 99, 500] {
            bits = (bits << 9) | code;
        }
        let bits = bits << (64 - 45);
        let encoded = &bits.to_be_bytes()[..6];
        let diag = Diagnostics::new();
        let mut dict = Dict::new();
        dict.put(b"Filter", Object::name(b"LZWDecode"));
        let mut s = open_filters(
            Stream::memory(encoded, &diag),
            &dict,
            &|o| o.clone(),
            None,
            None,
            true,
        )
        .unwrap();
        // As the content lexer reads, as next_lzwd honors the size asked for.
        assert_eq!(s.read_bytewise(usize::MAX).unwrap(), b"abc");
        assert!(s.had_error());
        // Read whole, the failing buffer holds all three.
        let diag = Diagnostics::new();
        assert!(run("LZWDecode", None, encoded, &diag).unwrap().is_empty());
    }

    #[test]
    fn lzw_decodes_the_spec_example() {
        // PDF 32000-1, 7.4.4.2: "-----A---B", with EarlyChange 1.
        let diag = Diagnostics::new();
        let encoded = [0x80, 0x0b, 0x60, 0x50, 0x22, 0x0c, 0x0c, 0x85, 0x01];
        assert_eq!(
            run("LZWDecode", None, &encoded, &diag).unwrap(),
            b"-----A---B"
        );
        assert!(diag.snapshot().is_empty());
        // Cut short: what was decoded, and a warning.
        let diag = Diagnostics::new();
        // The last, partial code reads as all ones, which MuPDF writes out
        // as a single byte before noticing the end.
        let partial = run("LZW", None, &encoded[..6], &diag).unwrap();
        assert_eq!(partial, b"-----A\xff");
        assert!(diag.has("filter.lzw_premature"));
    }

    #[test]
    fn ascii_filters_end_the_data_at_bad_bytes() {
        let diag = Diagnostics::new();
        assert_eq!(
            run("ASCIIHexDecode", None, b"48 65\n6C6c6F>ignored", &diag).unwrap(),
            b"Hello"
        );
        assert_eq!(run("AHx", None, b"4>", &diag).unwrap(), [0x40]);
        assert_eq!(run("AHx", None, b"414", &diag).unwrap(), [0x41]);
        // A bad byte loses this call's buffer: here, everything.
        assert_eq!(run("AHx", None, b"4142G>", &diag).unwrap(), b"");
        assert!(diag.has("stream.read_error"));

        let diag = Diagnostics::new();
        assert_eq!(
            run("ASCII85Decode", None, b"87cURD]i,\"Ebo80~>", &diag).unwrap(),
            b"Hello World!"
        );
        assert_eq!(run("A85", None, b"z~>", &diag).unwrap(), [0, 0, 0, 0]);
        assert_eq!(run("A85", None, b"9jqo~>", &diag).unwrap(), b"Man");
        run("A85", None, b"9~>", &diag).unwrap();
        assert!(diag.has("filter.a85_partial"));
    }

    #[test]
    fn run_length_and_its_bomb_guard() {
        let diag = Diagnostics::new();
        assert_eq!(
            run(
                "RunLengthDecode",
                None,
                &[2, b'a', b'b', b'c', 254, b'z', 128, 9],
                &diag
            )
            .unwrap(),
            b"abczzz"
        );
        // Truncated runs end the data.
        assert_eq!(run("RL", None, &[5, b'a'], &diag).unwrap(), b"");
        let mut dict = Dict::new();
        dict.put(
            b"Filter",
            Object::Array(std::sync::Arc::new(vec![
                Object::name(b"RL"),
                Object::name(b"RL"),
            ])),
        );
        let out = decode(
            &dict,
            &[1, 1, 7, 128],
            &|o| o.clone(),
            &diag,
            &Limits::default(),
        );
        assert_eq!(out.unwrap(), b"");
        assert!(diag.has("filter.rle_bomb"));
    }

    #[test]
    fn unknown_filters_pass_data_through() {
        let diag = Diagnostics::new();
        assert_eq!(run("Bogus", None, b"raw", &diag).unwrap(), b"raw");
        assert!(diag.has("filter.unknown"));
        // /F wins over /Filter; a non-name /F (a file spec) means no filter.
        let mut dict = Dict::new();
        dict.put(b"Filter", Object::name(b"FlateDecode"));
        dict.put(b"F", Object::String(std::sync::Arc::from(&b"file.bin"[..])));
        let out = decode(&dict, b"raw", &|o| o.clone(), &diag, &Limits::default());
        assert_eq!(out.unwrap(), b"raw");
    }

    #[test]
    fn decode_limit_stops_expansion() {
        let diag = Diagnostics::new();
        let mut dict = Dict::new();
        dict.put(b"Filter", Object::name(b"FlateDecode"));
        let limits = Limits {
            max_decoded_bytes: 1000,
            ..Limits::default()
        };
        let out = decode(
            &dict,
            &zlib(&vec![0u8; 1 << 20]),
            &|o| o.clone(),
            &diag,
            &limits,
        );
        assert_eq!(out, Err(DecodeError::Limit));
    }

    #[test]
    fn aes_stops_before_a_bad_block() {
        let key = [7u8; 16];
        let aes = Aes::new(&key, 128).unwrap();
        let encrypt = |plain: &[u8]| {
            let mut data = plain.to_vec();
            aes.cbc_encrypt(&mut [9u8; 16], &mut data);
            [&[9u8; 16][..], &data].concat()
        };
        let read = |data: &[u8], bytewise: bool| {
            let diag = Diagnostics::new();
            let base = Stream::memory(data, &diag);
            let mut s = open_cipher(base, Some(Cipher::Aes(key.to_vec()))).unwrap();
            let out = if bytewise {
                s.read_bytewise(usize::MAX)
            } else {
                s.read_best(data.len(), 0, usize::MAX)
            };
            (out.unwrap(), diag.has("stream.read_error"))
        };
        let mut padded = b"hello".to_vec();
        padded.extend([11u8; 11]);
        assert_eq!(read(&encrypt(&padded), false), (b"hello".to_vec(), false));
        assert_eq!(read(&[9u8; 16], false), (Vec::new(), false));
        assert_eq!(read(&[0u8; 10], false), (Vec::new(), true));
        // Bad padding: a whole read loses its last chunk (here, all of it),
        // a byte-at-a-time reader keeps the blocks before the bad one.
        let bad = encrypt(&[[1u8; 16], [0u8; 16]].concat());
        assert_eq!(read(&bad, false), (Vec::new(), true));
        assert_eq!(read(&bad, true), (vec![1u8; 16], true));
        let diag = Diagnostics::new();
        assert!(open_cipher(Stream::memory(&bad, &diag), Some(Cipher::Aes(vec![1; 5]))).is_err());
    }

    /// A PDF LZW encoder (codes MSB first, 9 to 12 bits), clearing the
    /// table when full unless `clear_when_full` is false. Code widths
    /// follow the decoder's table, as LZWDecode reads them.
    fn lzw(data: &[u8], early: u32, clear_when_full: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let (mut acc, mut nbits) = (0u64, 0u32);
        let mut put = |code: u32, bits: u32, out: &mut Vec<u8>| {
            acc = acc << bits | u64::from(code);
            nbits += bits;
            while nbits >= 8 {
                nbits -= 8;
                out.push((acc >> nbits) as u8);
            }
        };
        let mut dict: std::collections::HashMap<Vec<u8>, u32> = Default::default();
        let (mut next, mut dec_next, mut bits, mut emitted) = (258u32, 258u32, 9u32, 0u32);
        let mut emit =
            |code: u32, out: &mut Vec<u8>, put: &mut dyn FnMut(u32, u32, &mut Vec<u8>)| {
                put(code, bits, out);
                if code == 256 {
                    (bits, dec_next, emitted) = (9, 258, 0);
                    return;
                }
                if emitted > 0 && dec_next < 4096 {
                    dec_next += 1;
                    if dec_next > (1 << bits) - early - 1 {
                        bits = (bits + 1).min(12);
                    }
                }
                emitted += 1;
            };
        emit(256, &mut out, &mut put);
        let code_of = |w: &[u8], dict: &std::collections::HashMap<Vec<u8>, u32>| {
            if w.len() == 1 {
                u32::from(w[0])
            } else {
                dict[w]
            }
        };
        let mut w: Vec<u8> = Vec::new();
        for &c in data {
            let mut wc = w.clone();
            wc.push(c);
            if w.is_empty() || dict.contains_key(&wc) {
                w = wc;
                continue;
            }
            emit(code_of(&w, &dict), &mut out, &mut put);
            if next < 4094 {
                dict.insert(wc, next);
                next += 1;
            } else if clear_when_full {
                emit(256, &mut out, &mut put);
                dict.clear();
                next = 258;
            }
            w = vec![c];
        }
        if !w.is_empty() {
            emit(code_of(&w, &dict), &mut out, &mut put);
        }
        emit(257, &mut out, &mut put);
        put(0, 7, &mut out);
        out
    }

    /// Raw codes at fixed widths, MSB first.
    fn codes(codes: &[(u32, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        let (mut acc, mut nbits) = (0u64, 0u32);
        for &(code, bits) in codes.iter().chain(&[(0, 7)]) {
            acc = acc << bits | u64::from(code);
            nbits += bits;
            while nbits >= 8 {
                nbits -= 8;
                out.push((acc >> nbits) as u8);
            }
        }
        out
    }

    /// Rows of samples with a PNG predictor tag before each.
    fn png_rows(rows: &[(u8, &[u8])]) -> Vec<u8> {
        rows.iter()
            .flat_map(|(tag, row)| std::iter::once(*tag).chain(row.iter().copied()))
            .collect()
    }

    fn pseudo_random(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (x >> 16) as u8
            })
            .collect()
    }

    fn filter_cases() -> Vec<StreamCase> {
        let text: Vec<u8> = b"TOBEORNOTTOBEORTOBEORNOT#".repeat(20);
        let noisy = pseudo_random(9000, 7);
        let flate = |extras: &str| format!("/Filter /FlateDecode {extras}");
        let parms = |p: &str| flate(&format!("/DecodeParms << {p} >>"));
        let packed = zlib(&text);
        let mut bad_check = packed.clone();
        let n = bad_check.len();
        bad_check[n - 1] ^= 0xff;
        let mut corrupt = zlib(&noisy);
        corrupt[200] ^= 0xff;
        corrupt[201] ^= 0xff;
        let rows = png_rows(&[
            (0, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
            (1, &[1, 1, 1, 2, 2, 2, 3, 3, 3, 250, 250, 250]),
            (2, &[5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5]),
            (3, &[9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 255, 254]),
            (4, &[200, 3, 9, 1, 100, 7, 33, 66, 99, 12, 1, 0]),
            (4, &[0, 255, 0, 255, 0, 255, 128, 128, 128, 1, 2, 3]),
            (7, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
        ]);
        let random_rows: Vec<u8> = pseudo_random(13 * 12, 3)
            .chunks(13)
            .flat_map(|r| std::iter::once(r[0] % 5).chain(r[1..].iter().copied()))
            .collect();
        let s = |v: &str| v.to_string();
        vec![
            // Brotli (PDF 2.0).
            ("brotli", s("/Filter /BrotliDecode"), BROTLI.to_vec(), None),
            (
                "brotli_bad",
                s("/Filter /Br"),
                b"\xffnot brotli at all".to_vec(),
                None,
            ),
            // LZW.
            ("lzw", s("/Filter /LZWDecode"), lzw(&text, 1, true), None),
            (
                "lzw_early_0",
                s("/Filter /LZW /DecodeParms << /EarlyChange 0 >>"),
                lzw(&text, 0, true),
                None,
            ),
            (
                "lzw_full_table",
                s("/Filter /LZWDecode"),
                lzw(&noisy, 1, true),
                None,
            ),
            (
                "lzw_no_clear",
                s("/Filter /LZWDecode"),
                lzw(&noisy, 1, false),
                None,
            ),
            (
                "lzw_unknown_after_clear",
                s("/Filter /LZWDecode"),
                codes(&[(256, 9), (65, 9), (300, 9), (257, 9)]),
                None,
            ),
            (
                "lzw_code_after_clear",
                s("/Filter /LZWDecode"),
                codes(&[(256, 9), (300, 9), (257, 9)]),
                None,
            ),
            (
                "lzw_premature",
                s("/Filter /LZWDecode"),
                codes(&[(256, 9), (65, 9)]),
                None,
            ),
            (
                "lzw_with_png_predictor",
                s("/Filter /LZWDecode /DecodeParms << /Predictor 15 /Columns 4 /Colors 3 >>"),
                lzw(&rows, 1, true),
                None,
            ),
            // ASCII85 and ASCIIHex.
            (
                "a85",
                s("/Filter /ASCII85Decode"),
                b"87cURD]i,\"Ebo80~>".to_vec(),
                None,
            ),
            ("a85_z", s("/Filter /A85"), b"z 87cUR z~>".to_vec(), None),
            ("a85_tail_1", s("/Filter /A85"), b"87cURD~>".to_vec(), None),
            ("a85_tail_2", s("/Filter /A85"), b"87cURD]~>".to_vec(), None),
            (
                "a85_tail_3",
                s("/Filter /A85"),
                b"87cURD]i~>".to_vec(),
                None,
            ),
            (
                "a85_tail_4",
                s("/Filter /A85"),
                b"87cURD]i,~>".to_vec(),
                None,
            ),
            (
                "a85_bad_eod",
                s("/Filter /A85"),
                b"87cURD]i~x".to_vec(),
                None,
            ),
            (
                "a85_bad_byte",
                s("/Filter /A85"),
                b"87cUR{D]i~>".to_vec(),
                None,
            ),
            (
                "a85_no_eod",
                s("/Filter /A85"),
                b"87cURD]i,\"Ebo80".to_vec(),
                None,
            ),
            (
                "ahx",
                s("/Filter /ASCIIHexDecode"),
                b"48 65 6C 6c\n6f>".to_vec(),
                None,
            ),
            ("ahx_odd", s("/Filter /AHx"), b"48656>".to_vec(), None),
            ("ahx_bad", s("/Filter /AHx"), b"4865zz>".to_vec(), None),
            ("ahx_no_eod", s("/Filter /AHx"), b"486566".to_vec(), None),
            // RunLength.
            (
                "rld",
                s("/Filter /RunLengthDecode"),
                vec![2, b'a', b'b', b'c', 254, b'x', 0, b'!', 128, b'z'],
                None,
            ),
            (
                "rld_no_eod",
                s("/Filter /RL"),
                vec![1, b'a', b'b', 255, b'q'],
                None,
            ),
            ("rld_short_literal", s("/Filter /RL"), vec![5, b'a'], None),
            ("rld_short_run", s("/Filter /RL"), vec![250], None),
            (
                "rld_stacked",
                s("/Filter [/RL /RL]"),
                vec![0, 3, 253, b'x', 128],
                None,
            ),
            // Flate.
            ("flate", flate(""), packed.clone(), None),
            ("flate_bad_check", flate(""), bad_check, None),
            ("flate_corrupt", flate(""), corrupt, None),
            (
                "flate_trailing",
                flate(""),
                [&packed[..], b"trailing junk"].concat(),
                None,
            ),
            (
                "flate_not_zlib",
                flate(""),
                b"\x00\x01\x02 not zlib".to_vec(),
                None,
            ),
            // Predictors.
            (
                "png_rgb",
                parms("/Predictor 15 /Columns 4 /Colors 3"),
                zlib(&rows),
                None,
            ),
            (
                "png_random",
                parms("/Predictor 12 /Columns 4 /Colors 3"),
                zlib(&random_rows),
                None,
            ),
            (
                "png_16bit",
                parms("/Predictor 11 /Columns 3 /Colors 2 /BitsPerComponent 16"),
                zlib(&random_rows),
                None,
            ),
            (
                "png_4bit",
                parms("/Predictor 10 /Columns 7 /Colors 3 /BitsPerComponent 4"),
                zlib(&random_rows),
                None,
            ),
            (
                "png_short_row",
                parms("/Predictor 15 /Columns 5 /Colors 3"),
                zlib(&rows[..20]),
                None,
            ),
            (
                "tiff_8bit",
                parms("/Predictor 2 /Columns 4 /Colors 3"),
                zlib(&noisy[..120]),
                None,
            ),
            (
                "tiff_16bit",
                parms("/Predictor 2 /Columns 3 /Colors 2 /BitsPerComponent 16"),
                zlib(&noisy[..120]),
                None,
            ),
            (
                "tiff_1bit",
                parms("/Predictor 2 /Columns 21 /Colors 1 /BitsPerComponent 1"),
                zlib(&noisy[..60]),
                None,
            ),
            (
                "tiff_2bit",
                parms("/Predictor 2 /Columns 9 /Colors 3 /BitsPerComponent 2"),
                zlib(&noisy[..70]),
                None,
            ),
            (
                "tiff_4bit",
                parms("/Predictor 2 /Columns 5 /Colors 3 /BitsPerComponent 4"),
                zlib(&noisy[..80]),
                None,
            ),
            (
                "tiff_short_row",
                parms("/Predictor 2 /Columns 4 /Colors 3"),
                zlib(&noisy[..17]),
                None,
            ),
            (
                "columns_overflow",
                parms("/Predictor 12 /Columns 2147483647 /Colors 32 /BitsPerComponent 16"),
                zlib(&text),
                None,
            ),
            // Filter chains, parameters and the filters without data.
            (
                "chain",
                s("/Filter [/AHx /Fl] /DecodeParms [null << /Predictor 12 /Columns 4 >>]"),
                {
                    let z = zlib(&png_rows(&[(2, &[1, 2, 3, 4]), (1, &[5, 6, 7, 8])]));
                    let mut hex: Vec<u8> = z
                        .iter()
                        .flat_map(|b| format!("{b:02x}").into_bytes())
                        .collect();
                    hex.push(b'>');
                    hex
                },
                None,
            ),
            (
                "crypt_unencrypted",
                s("/Filter /Crypt"),
                b"plain".to_vec(),
                None,
            ),
            (
                "crypt_identity",
                s("/Filter /Crypt /DecodeParms << /Name /Identity >>"),
                b"plain".to_vec(),
                None,
            ),
            (
                "jbig2_globals_not_stream",
                s("/Filter /JBIG2Decode /DecodeParms << /JBIG2Globals 5 >>"),
                b"junk".to_vec(),
                None,
            ),
            (
                "unknown_filter",
                s("/Filter /Foo"),
                b"passes".to_vec(),
                None,
            ),
            (
                "filter_not_a_name",
                s("/Filter 5"),
                b"passes".to_vec(),
                None,
            ),
            (
                "filters_with_null",
                s("/Filter [null /AHx]"),
                b"6869>".to_vec(),
                None,
            ),
        ]
    }

    /// `printf 'Brotli in PDF 2.0: hello hello hello hello.' | brotli -c`
    const BROTLI: [u8; 48] = [
        0x21, 0xa8, 0x00, 0x04, 0x42, 0x72, 0x6f, 0x74, 0x6c, 0x69, 0x20, 0x69, 0x6e, 0x20, 0x50,
        0x44, 0x46, 0x20, 0x32, 0x2e, 0x30, 0x3a, 0x20, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x68,
        0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x68, 0x65, 0x6c, 0x6c,
        0x6f, 0x2e, 0x03,
    ];

    /// MuPDF 1.28.6's result for each case, from tests/data/oracle.py.
    #[rustfmt::skip]
    const MUPDF_FILTERS: &[(&str, usize, &str, &[&str])] = &[
        ("a85", 12, "7f83b1657ff1fc53", &[]),
        ("a85_bad_byte", 0, "e3b0c44298fc1c14", &["format error: bad data in a85d: '{'", "read error; treating as end of file"]),
        ("a85_bad_eod", 6, "334d016f755cd6dc", &["bad eod marker in a85d"]),
        ("a85_no_eod", 12, "7f83b1657ff1fc53", &[]),
        ("a85_tail_1", 4, "8d586f8b60df1ad7", &["partial final byte in a85d"]),
        ("a85_tail_2", 5, "2113cd4a7aebc3a4", &[]),
        ("a85_tail_3", 6, "334d016f755cd6dc", &[]),
        ("a85_tail_4", 7, "c1b51085d0841788", &[]),
        ("a85_z", 12, "f1ff162f7ec232d4", &[]),
        ("ahx", 5, "185f8db32271fe25", &[]),
        ("ahx_bad", 0, "e3b0c44298fc1c14", &["format error: bad data in ahxd: 'z'", "read error; treating as end of file"]),
        ("ahx_no_eod", 3, "21c02284e00ce613", &[]),
        ("ahx_odd", 3, "c778c131f931ed7c", &[]),
        ("brotli", 43, "dc68344173b5695f", &[]),
        ("brotli_bad", 0, "e3b0c44298fc1c14", &["library error: brotli decompression error", "read error; treating as end of file"]),
        ("chain", 8, "f16fda117f2c10af", &[]),
        ("columns_overflow", 0, "e3b0c44298fc1c14", &["error too many columns lead to an integer overflow (2147483647)"]),
        ("crypt_identity", 5, "a116c9ed46d62077", &["crypt filter in unencrypted document"]),
        ("crypt_unencrypted", 5, "a116c9ed46d62077", &["crypt filter in unencrypted document"]),
        ("filter_not_a_name", 6, "2c28ec094b830e09", &[]),
        ("filters_with_null", 2, "8f434346648f6b96", &["unknown filter name ()"]),
        ("flate", 500, "d93b21de421e06aa", &[]),
        ("flate_bad_check", 500, "d93b21de421e06aa", &["ignoring zlib error: incorrect data check"]),
        ("flate_corrupt", 9000, "b0768b422b53c852", &["ignoring zlib error: incorrect data check"]),
        ("flate_not_zlib", 0, "e3b0c44298fc1c14", &["library error: zlib error: incorrect header check", "read error; treating as end of file"]),
        ("flate_trailing", 500, "d93b21de421e06aa", &[]),
        ("jbig2_globals_not_stream", 0, "e3b0c44298fc1c14", &["jbig2 globals is not a stream, skipping globals", "jbig2dec error: page has no image, cannot be completed (segment 4294967295)", "library error: cannot complete jbig2 image", "read error; treating as end of file"]),
        ("lzw", 500, "d93b21de421e06aa", &[]),
        ("lzw_code_after_clear", 0, "e3b0c44298fc1c14", &["format error: out of range code encountered in lzw decode", "read error; treating as end of file"]),
        ("lzw_early_0", 500, "d93b21de421e06aa", &[]),
        ("lzw_full_table", 9000, "2eb2268b750a45e7", &[]),
        ("lzw_no_clear", 0, "e3b0c44298fc1c14", &["tolerating a single out of range code in lzw decode", "missing clear code in lzw decode", "format error: out of range code encountered in lzw decode", "read error; treating as end of file"]),
        ("lzw_premature", 2, "f3c43500fa3e97e6", &["premature end in lzw decode"]),
        ("lzw_unknown_after_clear", 0, "e3b0c44298fc1c14", &["format error: out of range code encountered in lzw decode", "read error; treating as end of file"]),
        ("lzw_with_png_predictor", 84, "8b882b7881ed63c0", &["unknown png predictor 7, treating as none"]),
        ("png_16bit", 144, "837cb0f94d496a06", &[]),
        ("png_4bit", 143, "b4312ed1cd82be8d", &["unknown png predictor 61, treating as none", "unknown png predictor 116, treating as none", "unknown png predictor 147, treating as none", "unknown png predictor 255, treating as none", "unknown png predictor 238, treating as none", "unknown png predictor 38, treating as none", "unknown png predictor 134, treating as none", "unknown png predictor 68, treating as none", "unknown png predictor 250, treating as none", "unknown png predictor 98, treating as none", "unknown png predictor 219, treating as none", "unknown png predictor 178, treating as none"]),
        ("png_random", 144, "c01a769efabc1046", &[]),
        ("png_rgb", 84, "8b882b7881ed63c0", &["unknown png predictor 7, treating as none"]),
        ("png_short_row", 18, "18cd8d6999a5ad45", &[]),
        ("rld", 7, "cb4fd602bde9140c", &[]),
        ("rld_no_eod", 4, "c94b72b9893c2b29", &[]),
        ("rld_short_literal", 0, "e3b0c44298fc1c14", &["format error: premature end of data in run length decode", "read error; treating as end of file"]),
        ("rld_short_run", 0, "e3b0c44298fc1c14", &["format error: premature end of data in run length decode", "read error; treating as end of file"]),
        ("rld_stacked", 0, "e3b0c44298fc1c14", &["RLE bomb defused"]),
        ("tiff_16bit", 120, "38172a95581a9fb1", &[]),
        ("tiff_1bit", 60, "4efce29d2ea78361", &[]),
        ("tiff_2bit", 70, "c76b78392fba7720", &[]),
        ("tiff_4bit", 80, "fe3c3b1a436f4642", &[]),
        ("tiff_8bit", 120, "ae4a8cf8b8dd2544", &[]),
        ("tiff_short_row", 17, "169d2f0432928901", &[]),
        ("unknown_filter", 6, "2c28ec094b830e09", &["unknown filter name (Foo)"]),
    ];

    #[test]
    fn filters_decode_as_in_mupdf() {
        check_against_mupdf("NUPDF_DUMP_FILTER_CASES", &filter_cases(), MUPDF_FILTERS);
    }

    #[test]
    fn truncated_brotli_gives_what_was_decoded() {
        // MuPDF 1.28.6 never returns from this stream (its brotli filter
        // loops once the input runs out mid-block), so there is no result
        // to pin; nupdf ends the data where the input does, quietly.
        let (len, _, warnings) = decode_as_mupdf("/Filter /BrotliDecode", &BROTLI[..20], None);
        assert_eq!(len, 16);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn the_lzw_encoder_round_trips() {
        // The test encoder against nupdf's decoder, where both agree with
        // MuPDF above: the fixtures mean what they say.
        let diag = Diagnostics::new();
        let text = b"TOBEORNOTTOBEORTOBEORNOT#".repeat(20);
        assert_eq!(
            run("LZWDecode", None, &lzw(&text, 1, true), &diag).ok(),
            Some(text)
        );
        let noisy = pseudo_random(9000, 7);
        assert_eq!(
            run("LZWDecode", None, &lzw(&noisy, 1, true), &diag).ok(),
            Some(noisy)
        );
    }
}
