// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/filter-jbig2.c,
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! `JBIG2Decode`: MuPDF's driver around jbig2dec (`next_jbig2d`), over
//! hayro-jbig2. The whole input is read, the embedded page decoded, and its
//! rows come out inverted (JBIG2 has 1 for black, PDF images 0).
//!
//! jbig2dec leaves some of the standard unimplemented and fails on it;
//! those segments are looked for first, so such streams fail here too, with
//! jbig2dec's messages.

use crate::diag::Diagnostics;
use crate::stream::{Next, Source, Stream};

/// The JBIG2 file header's ID string (D.4.1). jbig2dec accepts a file
/// header even in a stream that should have none.
const FILE_ID: [u8; 8] = [0x97, 0x4a, 0x42, 0x32, 0x0d, 0x0a, 0x1a, 0x0a];

/// `JBIG2_UNKNOWN_SEGMENT_NUMBER`.
const UNKNOWN_SEGMENT: u32 = u32::MAX;

fn be32(d: &[u8], at: usize) -> Option<u32> {
    let b = d.get(at..at + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// A segment header (7.2): its number, type and data length, and the
/// header's own length.
fn segment_header(d: &[u8]) -> Option<(u32, u8, u32, usize)> {
    if d.len() < 11 {
        return None;
    }
    let number = be32(d, 0)?;
    let flags = d[4];
    let (count, mut offset) = if d[5] & 0xe0 == 0xe0 {
        let count = (be32(d, 5)? & 0x1fff_ffff) as usize;
        (count, 9 + (count + 1) / 8)
    } else {
        (usize::from(d[5] >> 5), 6)
    };
    let ref_size = if number <= 256 {
        1
    } else if number <= 65536 {
        2
    } else {
        4
    };
    offset += count.checked_mul(ref_size)?;
    offset += if flags & 0x40 != 0 { 4 } else { 1 };
    let length = be32(d, offset)?;
    Some((number, flags & 63, length, offset + 4))
}

/// jbig2dec's message severities, as MuPDF's callback prints them.
#[derive(Clone, Copy)]
enum Severity {
    Fatal,
    Warning,
}

fn jbig2dec_message(diag: &Diagnostics, severity: Severity, number: u32, message: &str) {
    let kind = match severity {
        Severity::Fatal => "error",
        Severity::Warning => "warning",
    };
    diag.warn(
        "filter.jbig2dec",
        format!("jbig2dec {kind}: {message} (segment {number})"),
    );
}

/// What jbig2dec says about a segment it does not fully implement, and
/// whether it fails on it.
fn segment_check(kind: u8, body: &[u8]) -> Option<(Severity, &'static str, bool)> {
    use Severity::{Fatal, Warning};
    match kind {
        0 if body.len() >= 2 => {
            let flags = u16::from_be_bytes([body[0], body[1]]);
            if flags & 0x0100 != 0 {
                Some((
                    Warning,
                    "segment marks bitmap coding context as used (NYI)",
                    true,
                ))
            } else if flags & 0x0200 != 0 {
                let m = "segment marks bitmap coding context as retained (NYI)";
                Some((Warning, m, false))
            } else {
                None
            }
        }
        36 => Some((
            Fatal,
            "unhandled segment type 'intermediate generic region' (NYI)",
            true,
        )),
        48 if body.len() >= 19 && body[16] & 0x80 != 0 => Some((
            Fatal,
            "page segment indicates use of color segments (NYI)",
            true,
        )),
        38 | 39 if body.len() >= 18 && body[17] >> 5 & 1 != 0 => Some((
            Fatal,
            "segment uses 12 adaptive template pixels (NYI)",
            true,
        )),
        4 | 6 | 7 if body.len() >= 17 && body[16] & 8 != 0 => Some((
            Fatal,
            "region segment flags indicate use of colored bitmap (NYI)",
            true,
        )),
        // jbig2dec returns its warning's -1, so the segment fails.
        54 => Some((
            Warning,
            "unhandled segment type 'color palette' (NYI)",
            true,
        )),
        _ => None,
    }
}

/// Segment types jbig2dec knows (`jbig2_parse_segment`).
const KNOWN_SEGMENTS: [u8; 22] = [
    0, 4, 6, 7, 16, 20, 22, 23, 36, 38, 39, 40, 42, 43, 48, 49, 50, 51, 52, 53, 54, 62,
];

/// Walks the segments as jbig2dec would, reporting what it says about the
/// parts it does not implement. Returns whether it fails on one.
fn jbig2dec_fails(data: &[u8], diag: &Diagnostics) -> bool {
    let mut p = 0;
    let mut random = false;
    if data.len() >= 9 && data[..8] == FILE_ID {
        jbig2dec_message(
            diag,
            Severity::Warning,
            UNKNOWN_SEGMENT,
            "JBIG2 file header ignored in supposedly embedded stream",
        );
        let flags = data[8];
        let header_nyi = if flags & 4 != 0 {
            Some("file header indicates use of 12 adaptive template pixels (NYI)")
        } else if flags & 8 != 0 {
            Some("file header indicates use of colored region segments (NYI)")
        } else {
            None
        };
        if let Some(m) = header_nyi {
            jbig2dec_message(diag, Severity::Fatal, UNKNOWN_SEGMENT, m);
            return true;
        }
        p = if flags & 2 == 0 { 13 } else { 9 };
        random = flags & 1 == 0;
    }
    // One segment's body: true when jbig2dec stops there.
    let body = |number: u32, kind: u8, body: &[u8]| -> bool {
        if !KNOWN_SEGMENTS.contains(&kind) {
            let m = format!("unknown segment type {kind}");
            jbig2dec_message(diag, Severity::Warning, number, &m);
            return false;
        }
        let Some((severity, message, fails)) = segment_check(kind, body) else {
            return false;
        };
        jbig2dec_message(diag, severity, number, message);
        if fails {
            jbig2dec_message(
                diag,
                Severity::Warning,
                number,
                "failed to decode; treating as end of file",
            );
        }
        fails
    };
    // Random access: all headers, then the bodies in the same order.
    let mut headers = Vec::new();
    while let Some((number, kind, length, size)) = data.get(p..).and_then(segment_header) {
        p += size;
        if random {
            headers.push((number, kind, length));
            if kind == 51 {
                break;
            }
            continue;
        }
        if length == u32::MAX {
            // An immediate generic region of unknown length: its flags are
            // in reach, the next segment is not.
            return data.get(p..).is_some_and(|d| body(number, kind, d));
        }
        let Some(d) = data.get(p..p.saturating_add(length as usize)) else {
            return false;
        };
        if body(number, kind, d) {
            return true;
        }
        if kind == 51 {
            return false;
        }
        p += length as usize;
    }
    for (number, kind, length) in headers {
        let Some(d) = data.get(p..p.saturating_add(length as usize)) else {
            return false;
        };
        if body(number, kind, d) {
            return true;
        }
        p += length as usize;
    }
    false
}

/// `fz_load_jbig2_globals`: decodes a /JBIG2Globals stream's data (`None`
/// when it is empty, as MuPDF has no globals then).
pub fn load_globals(
    data: &[u8],
    diag: &Diagnostics,
) -> Result<Option<hayro_jbig2::Globals>, String> {
    if data.is_empty() {
        return Ok(None);
    }
    let fail = || "library error: cannot decode jbig2 globals".to_string();
    if jbig2dec_fails(data, diag) {
        return Err(fail());
    }
    hayro_jbig2::Globals::new(data, &hayro_jbig2::DecodeSettings::default())
        .map(Some)
        .map_err(|_| fail())
}

/// `fz_open_jbig2d` for an embedded stream.
pub fn open(chain: Stream<'_>, globals: Option<hayro_jbig2::Globals>) -> Stream<'_> {
    let diag = chain.diag();
    Stream::new(
        Jbig2 {
            chain,
            globals,
            page: None,
            idx: 0,
        },
        diag,
    )
}

struct Jbig2<'a> {
    chain: Stream<'a>,
    globals: Option<hayro_jbig2::Globals>,
    page: Option<Vec<u8>>,
    idx: usize,
}

/// Packs decoded pixels into rows of bytes, already inverted.
struct Rows {
    data: Vec<u8>,
    stride: usize,
    row: usize,
    bit: usize,
}

impl hayro_jbig2::Decoder for Rows {
    fn push_pixel(&mut self, black: bool) {
        if black && let Some(b) = self.data.get_mut(self.row * self.stride + self.bit / 8) {
            *b &= !(0x80 >> (self.bit % 8));
        }
        self.bit += 1;
    }

    fn push_pixel_chunk(&mut self, black: bool, chunk_count: u32) {
        let at = self.row * self.stride + self.bit / 8;
        let n = chunk_count as usize;
        if black && let Some(bytes) = self.data.get_mut(at..at + n) {
            bytes.fill(0);
        }
        self.bit += 8 * n;
    }

    fn next_line(&mut self) {
        self.row += 1;
        self.bit = 0;
    }
}

impl Jbig2<'_> {
    fn decode(&mut self) -> Result<Vec<u8>, String> {
        let mut data = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = self.chain.read(&mut tmp);
            if n == 0 {
                break;
            }
            data.extend_from_slice(&tmp[..n]);
        }
        let fail = || "library error: cannot decode jbig2 image".to_string();
        if jbig2dec_fails(&data, self.chain.diag()) {
            return Err(fail());
        }
        let header = data.starts_with(&FILE_ID);
        let settings = hayro_jbig2::DecodeSettings::default();
        let file;
        let page = if header {
            file = hayro_jbig2::File::new(&data, &settings).map_err(|_| fail())?;
            file.page(0).ok_or_else(fail)?
        } else {
            hayro_jbig2::Page::new_embedded(&data, self.globals.as_ref(), &settings)
                .map_err(|_| fail())?
        };
        let (w, h) = (page.width() as usize, page.height() as usize);
        let stride = w.div_ceil(8);
        let size = stride
            .checked_mul(h)
            .filter(|&n| n <= 1 << 30)
            .ok_or_else(|| "limit error: jbig2 image too large".to_string())?;
        let mut rows = Rows {
            data: vec![0xff; size],
            stride,
            row: 0,
            bit: 0,
        };
        page.decode(&mut rows).map_err(|_| fail())?;
        Ok(rows.data)
    }
}

impl Source for Jbig2<'_> {
    fn next(&mut self, max: usize, out: &mut Vec<u8>, _: &Diagnostics) -> Result<Next, String> {
        if self.page.is_none() {
            self.page = Some(self.decode()?);
        }
        let page = self.page.as_deref().unwrap_or_default();
        let n = (page.len() - self.idx).min(max.min(4096));
        out.extend_from_slice(&page[self.idx..self.idx + n]);
        self.idx += n;
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

    fn segment(number: u32, kind: u8, data: &[u8]) -> Vec<u8> {
        let mut s = number.to_be_bytes().to_vec();
        s.extend([kind, 0, 1]);
        s.extend((data.len() as u32).to_be_bytes());
        s.extend_from_slice(data);
        s
    }

    fn messages(data: &[u8]) -> (bool, Vec<String>) {
        let diag = Diagnostics::new();
        let fails = jbig2dec_fails(data, &diag);
        (
            fails,
            diag.snapshot().into_iter().map(|d| d.message).collect(),
        )
    }

    #[test]
    fn intermediate_generic_regions_fail_as_in_jbig2dec() {
        let mut data = segment(0, 48, &[0; 19]);
        data.extend(segment(1, 36, &[0; 18]));
        let (fails, msgs) = messages(&data);
        assert!(fails);
        assert_eq!(
            msgs,
            [
                "jbig2dec error: unhandled segment type 'intermediate generic region' (NYI) (segment 1)",
                "jbig2dec warning: failed to decode; treating as end of file (segment 1)",
            ]
        );
    }

    #[test]
    fn retained_contexts_and_unknown_segments_only_warn() {
        let mut data = segment(0, 0, &[0x02, 0x00]);
        data.extend(segment(1, 33, &[]));
        let (fails, msgs) = messages(&data);
        assert!(!fails);
        assert_eq!(
            msgs,
            [
                "jbig2dec warning: segment marks bitmap coding context as retained (NYI) (segment 0)",
                "jbig2dec warning: unknown segment type 33 (segment 1)",
            ]
        );
    }

    #[test]
    fn file_headers_are_noted_and_their_flags_checked() {
        let mut data = FILE_ID.to_vec();
        data.push(0x04);
        let (fails, msgs) = messages(&data);
        assert!(fails);
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].contains("file header ignored"));
        assert!(msgs[1].contains("12 adaptive template pixels"));
    }

    #[test]
    fn undecodable_pages_end_the_stream_empty() {
        let diag = Diagnostics::new();
        let mut s = open(Stream::memory(b"not jbig2", &diag), None);
        assert_eq!(s.read_best(0, 0, 1 << 20), Ok(Vec::new()));
        assert!(diag.has("stream.read_error"));
    }
}
