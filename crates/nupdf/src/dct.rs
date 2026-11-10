// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/filter-dct.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! `DCTDecode`: MuPDF's driver around libjpeg (`next_dctd`), over the
//! libjpeg port in `jpeg`. Stray whitespace before the data is skipped,
//! /ColorTransform and the Adobe marker decide whether color is
//! transformed, CMYK is left as stored, and output comes a scanline at a
//! time into chunks of at most 4096 bytes, so a decoding error loses the
//! chunk it happens in, as in MuPDF.

use crate::diag::Diagnostics;
use crate::jpeg::{ColorSpace, Decoder, Error};
use crate::stream::{Next, Source, Stream};

/// `fz_open_dctd` with the PDF's /ColorTransform (-1 when absent).
pub fn open(chain: Stream<'_>, color_transform: i32) -> Stream<'_> {
    let diag = chain.diag();
    Stream::new(
        Dct {
            chain: Some(chain),
            decoder: None,
            color_transform,
            scanline: Vec::new(),
            rp: 0,
        },
        diag,
    )
}

struct Dct<'a> {
    chain: Option<Stream<'a>>,
    decoder: Option<Decoder<'a>>,
    color_transform: i32,
    /// A scanline that did not fit in the last chunk, and how much of it
    /// has gone out.
    scanline: Vec<u8>,
    rp: usize,
}

fn report(e: Error) -> String {
    match e {
        Error::Jpeg(m) => format!("library error: jpeg error: {m}"),
        Error::Unsupported(m) => format!("unsupported error: {m}"),
        Error::Limit(m) => format!("limit error: {m}"),
    }
}

impl<'a> Dct<'a> {
    /// The first call's setup: `jpeg_read_header`, MuPDF's color transform
    /// choice, `jpeg_start_decompress`.
    fn init(&mut self) -> Result<(), Error> {
        let Some(mut chain) = self.chain.take() else {
            return Ok(());
        };
        while matches!(chain.peek_byte(), Some(b'\n' | b'\r' | b' ')) {
            chain.read_byte();
        }
        let d = self.decoder.insert(Decoder::new(chain));
        d.read_header()?;
        let n = d.num_components();
        // Default 1 with three components and 0 otherwise; the Adobe
        // marker overrides /ColorTransform.
        let mut ct = self.color_transform;
        if ct < 0 {
            ct = i32::from(n == 3);
        }
        if let Some(t) = d.adobe_transform() {
            ct = i32::from(t);
        }
        if ct == 0 {
            match n {
                3 => d.set_jpeg_color_space(ColorSpace::Rgb),
                4 => d.set_jpeg_color_space(ColorSpace::Cmyk),
                _ => {}
            }
        }
        d.start()
    }
}

impl Source for Dct<'_> {
    fn next(&mut self, max: usize, out: &mut Vec<u8>, _: &Diagnostics) -> Result<Next, String> {
        if self.chain.is_some() {
            self.init().map_err(report)?;
        }
        let Some(d) = self.decoder.as_mut() else {
            return Ok(Next::Eof);
        };
        let cap = max.min(4096);
        let stride = d.output_width() * d.output_components();
        let pending = (self.scanline.len() - self.rp).min(cap);
        out.extend_from_slice(&self.scanline[self.rp..self.rp + pending]);
        self.rp += pending;
        while out.len() < cap && d.output_scanline() < d.output_height() {
            let at = out.len();
            if at + stride <= cap {
                out.resize(at + stride, 0);
                d.read_scanline(&mut out[at..]).map_err(report)?;
            } else {
                self.scanline.resize(stride, 0);
                d.read_scanline(&mut self.scanline).map_err(report)?;
                let n = cap - at;
                out.extend_from_slice(&self.scanline[..n]);
                self.rp = n;
            }
        }
        Ok(if out.is_empty() {
            Next::Eof
        } else {
            Next::Data
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(data: &[u8], color_transform: i32) -> (Vec<u8>, Vec<String>) {
        let diag = Diagnostics::new();
        let mut s = open(Stream::memory(data, &diag), color_transform);
        let out = s.read_bytewise(usize::MAX).unwrap_or_default();
        let warnings = diag.snapshot().into_iter().map(|d| d.message).collect();
        (out, warnings)
    }

    #[test]
    fn errors_before_any_output_end_the_stream_empty() {
        let (out, warnings) = decode(b"  \nnot a jpeg", -1);
        assert!(out.is_empty());
        assert_eq!(
            warnings,
            [
                "library error: jpeg error: Not a JPEG file: starts with 0x6e 0x6f",
                "read error; treating as end of file",
            ]
        );
    }
}
