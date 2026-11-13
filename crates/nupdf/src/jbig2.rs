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

/// The shortest symbol dictionary jbig2dec reads: its flags, adaptive
/// template pixels and symbol counts (7.4.2.1).
fn symbol_dictionary_min(body: &[u8]) -> usize {
    let [a, b, ..] = *body else {
        return 10;
    };
    let flags = u16::from_be_bytes([a, b]);
    let huffman = flags & 1 != 0;
    let refinement = flags >> 1 & 1 != 0;
    let template = flags >> 10 & 3;
    let refinement_template = flags >> 12 & 1;
    let at = if huffman {
        0
    } else if template == 0 {
        8
    } else {
        2
    };
    let rat = if refinement && refinement_template == 0 {
        4
    } else {
        0
    };
    (2 + at + rat + 8).max(10)
}

/// What jbig2dec says about a segment it finds too short or does not fully
/// implement, and whether it fails on it.
fn segment_check(kind: u8, body: &[u8]) -> (Vec<(Severity, String)>, bool) {
    use Severity::{Fatal, Warning};
    let one = |severity, m: &str, fails| (vec![(severity, m.to_string())], fails);
    let min = match kind {
        0 => symbol_dictionary_min(body),
        16 => 7,
        4 | 6 | 7 => 17,
        22 | 23 => 18,
        38 | 39 => 18,
        // Template 0 refinement carries two adaptive pixels more.
        40 | 42 | 43 if body.get(17).is_some_and(|f| f & 1 == 0) => 22,
        40 | 42 | 43 => 18,
        48 => 19,
        50 | 62 => 4,
        _ => 0,
    };
    if body.len() < min {
        return one(Fatal, "segment too short", true);
    }
    match kind {
        0 => {
            let flags = u16::from_be_bytes([body[0], body[1]]);
            if flags & 0x0100 != 0 {
                one(
                    Warning,
                    "segment marks bitmap coding context as used (NYI)",
                    true,
                )
            } else if flags & 0x0200 != 0 {
                let m = "segment marks bitmap coding context as retained (NYI)";
                one(Warning, m, false)
            } else {
                (Vec::new(), false)
            }
        }
        36 => one(
            Fatal,
            "unhandled segment type 'intermediate generic region' (NYI)",
            true,
        ),
        48 if body[16] & 0x80 != 0 => one(
            Fatal,
            "page segment indicates use of color segments (NYI)",
            true,
        ),
        38 | 39 => {
            let flags = body[17];
            let at = match (flags & 1, flags & 6) {
                (1, _) => 0,
                (_, 0) => 8,
                _ => 2,
            };
            if 18 + at > body.len() {
                one(Fatal, "segment too short", true)
            } else if flags >> 5 & 1 != 0 {
                one(
                    Fatal,
                    "segment uses 12 adaptive template pixels (NYI)",
                    true,
                )
            } else {
                (Vec::new(), false)
            }
        }
        4 | 6 | 7 if body[16] & 8 != 0 => one(
            Fatal,
            "region segment flags indicate use of colored bitmap (NYI)",
            true,
        ),
        // jbig2dec returns its warning's -1, so the segment fails.
        54 => one(
            Warning,
            "unhandled segment type 'color palette' (NYI)",
            true,
        ),
        // `jbig2_parse_extension_segment`: comments are ignored, other
        // types skipped unless marked necessary.
        62 => {
            let ty = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
            let reserved = ty & 0x2000_0000 != 0;
            let necessary = ty & 0x8000_0000 != 0;
            let mut said = Vec::new();
            if necessary && !reserved {
                said.push((
                    Warning,
                    "extension segment is marked 'necessary' but not 'reserved' contrary to spec"
                        .to_string(),
                ));
            }
            if matches!(ty, 0x2000_0000 | 0x2000_0002) {
                return (said, false);
            }
            if necessary {
                said.push((
                    Fatal,
                    format!("unhandled necessary extension segment type 0x{ty:08x}"),
                ));
                return (said, true);
            }
            said.push((
                Warning,
                "unhandled non-necessary extension segment, skipping".to_string(),
            ));
            (said, false)
        }
        _ => (Vec::new(), false),
    }
}

/// Segment types jbig2dec knows (`jbig2_parse_segment`).
const KNOWN_SEGMENTS: [u8; 22] = [
    0, 4, 6, 7, 16, 20, 22, 23, 36, 38, 39, 40, 42, 43, 48, 49, 50, 51, 52, 53, 54, 62,
];

/// What walking the segments as jbig2dec would found.
struct Walk {
    /// jbig2dec fails on a segment.
    fails: bool,
    /// The data hayro-jbig2 should decode for the same result, where it
    /// differs: segment lengths jbig2dec works out, flags it ignores.
    patched: Option<Vec<u8>>,
    /// A page information segment was read: without one jbig2dec cannot
    /// complete the page.
    page: bool,
}

/// Walks the segments as jbig2dec would, reporting what it says about the
/// parts it does not implement or finds damaged.
fn jbig2dec_walk(data: &[u8], diag: &Diagnostics) -> Walk {
    let mut p = 0;
    let mut random = false;
    let mut edits: Vec<(usize, [u8; 4], usize)> = Vec::new();
    let page = std::cell::Cell::new(false);
    let walk = |fails: bool, edits: Vec<(usize, [u8; 4], usize)>| {
        let patched = (!edits.is_empty()).then(|| {
            let mut d = data.to_vec();
            for (at, bytes, n) in edits {
                d[at..at + n].copy_from_slice(&bytes[..n]);
            }
            d
        });
        Walk {
            fails,
            patched,
            page: page.get(),
        }
    };
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
            return walk(true, edits);
        }
        p = if flags & 2 == 0 { 13 } else { 9 };
        random = flags & 1 == 0;
    }
    // One segment's body at `at`: true when jbig2dec stops there.
    let body = |number: u32, kind: u8, at: usize, body: &[u8], edits: &mut Vec<_>| -> bool {
        page.set(page.get() || kind == 48);
        if !KNOWN_SEGMENTS.contains(&kind) {
            let m = format!("unknown segment type {kind}");
            jbig2dec_message(diag, Severity::Warning, number, &m);
            return false;
        }
        let (said, fails) = segment_check(kind, body);
        for (severity, message) in &said {
            jbig2dec_message(diag, *severity, number, message);
        }
        if fails {
            jbig2dec_message(
                diag,
                Severity::Warning,
                number,
                "failed to decode; treating as end of file",
            );
        } else if matches!(kind, 38 | 39) && body[17] & 0x17 == 0x10 {
            // jbig2dec reads template 0's four adaptive pixels whatever
            // the extended-template bit says; hayro-jbig2 would read 12.
            edits.push((at + 17, [body[17] & !0x10, 0, 0, 0], 1));
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
        let mut length = length as usize;
        if length == u32::MAX as usize {
            // An immediate generic region of unknown length ends at its
            // end-of-data marker and row count.
            let marker: &[u8] = match data.get(p + 17) {
                Some(f) if f & 1 != 0 => &[0, 0],
                _ => &[0xff, 0xac],
            };
            let end = data
                .get(p + 18..)
                .filter(|_| kind == 38)
                .and_then(|d| d.windows(2).position(|w| w == marker))
                .map(|at| p + 18 + at + 2 + 4)
                .filter(|&end| end <= data.len());
            match end {
                Some(end) => length = end - p,
                None => {
                    // jbig2dec waits for more data, then takes what there
                    // is (`jbig2_complete_page`).
                    jbig2dec_message(
                        diag,
                        Severity::Warning,
                        number,
                        "file has an invalid segment data length; trying to decode using the available data",
                    );
                    let rest = data.len() - p;
                    edits.push((p - 4, (rest as u32).to_be_bytes(), 4));
                    let fails = body(number, kind, p, &data[p..], &mut edits);
                    return walk(fails, edits);
                }
            }
        }
        let Some(d) = data.get(p..p.saturating_add(length)) else {
            return walk(false, edits);
        };
        if body(number, kind, p, d, &mut edits) {
            return walk(true, edits);
        }
        if kind == 51 {
            return walk(false, edits);
        }
        p += length;
    }
    for (number, kind, length) in headers {
        let Some(d) = data.get(p..p.saturating_add(length as usize)) else {
            return walk(false, edits);
        };
        if body(number, kind, p, d, &mut edits) {
            return walk(true, edits);
        }
        p += length as usize;
    }
    walk(false, edits)
}

/// `fz_load_jbig2_globals`: decodes a /JBIG2Globals stream's data (`None`
/// when it is empty, as MuPDF has no globals then).
pub fn load_globals(
    data: &[u8],
    diag: &Diagnostics,
) -> Result<Option<crate::hayro_jbig2::Globals>, String> {
    if data.is_empty() {
        return Ok(None);
    }
    let fail = || "library error: cannot decode jbig2 globals".to_string();
    let walk = jbig2dec_walk(data, diag);
    if walk.fails {
        return Err(fail());
    }
    let data = walk.patched.as_deref().unwrap_or(data);
    crate::hayro_jbig2::Globals::new(data, &crate::hayro_jbig2::DecodeSettings::default())
        .map(Some)
        .map_err(|_| fail())
}

/// `fz_open_jbig2d` for an embedded stream.
pub fn open(chain: Stream<'_>, globals: Option<crate::hayro_jbig2::Globals>) -> Stream<'_> {
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
    globals: Option<crate::hayro_jbig2::Globals>,
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

impl crate::hayro_jbig2::Decoder for Rows {
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
        let walk = jbig2dec_walk(&data, self.chain.diag());
        if walk.fails {
            return Err(fail());
        }
        if !walk.page {
            jbig2dec_message(
                self.chain.diag(),
                Severity::Fatal,
                UNKNOWN_SEGMENT,
                "page has no image, cannot be completed",
            );
            return Err("library error: cannot complete jbig2 image".to_string());
        }
        if let Some(patched) = walk.patched {
            data = patched;
        }
        let header = data.starts_with(&FILE_ID);
        let settings = crate::hayro_jbig2::DecodeSettings::default();
        let file;
        let page = if header {
            file = crate::hayro_jbig2::File::new(&data, &settings).map_err(|_| fail())?;
            file.page(0).ok_or_else(fail)?
        } else {
            crate::hayro_jbig2::Page::new_embedded(&data, self.globals.as_ref(), &settings)
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
        let fails = jbig2dec_walk(data, &diag).fails;
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
        // A symbol dictionary as long as jbig2dec needs: flags, four
        // adaptive pixels, two counts.
        let mut dict = vec![0x02, 0x00];
        dict.resize(18, 0);
        let mut data = segment(0, 0, &dict);
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
    fn endless_empty_export_runs_end_the_decode() {
        // A fuzzer find: a symbol dictionary whose export runs decode as
        // zeros forever. MuPDF fails it with jbig2dec's guard; so does the
        // patched hayro-jbig2.
        let data = include_bytes!("../testdata/jbig2-endless-export-runs.jb2");
        let diag = Diagnostics::new();
        let mut s = open(Stream::memory(data, &diag), None);
        assert_eq!(s.read_best(0, 0, 1 << 20), Ok(Vec::new()));
    }

    /// A segment header in full: number, type with page association size,
    /// referred-to segments, page, data length.
    fn segment_with(number: u32, kind: u8, refs: &[u32], page: u32, data: &[u8]) -> Vec<u8> {
        let wide_page = page > 255;
        let mut s = number.to_be_bytes().to_vec();
        s.push(kind | if wide_page { 0x40 } else { 0 });
        if refs.len() <= 4 {
            s.push((refs.len() as u8) << 5);
        } else {
            // The long form, with the retention bytes jbig2dec reckons:
            // (count + 1) / 8, rounded down.
            s.extend((0xe000_0000u32 | refs.len() as u32).to_be_bytes());
            s.extend(vec![0; (refs.len() + 1) / 8]);
        }
        for &r in refs {
            match number {
                0..=256 => s.push(r as u8),
                257..=65536 => s.extend((r as u16).to_be_bytes()),
                _ => s.extend(r.to_be_bytes()),
            }
        }
        if wide_page {
            s.extend(page.to_be_bytes());
        } else {
            s.push(page as u8);
        }
        s.extend((data.len() as u32).to_be_bytes());
        s.extend_from_slice(data);
        s
    }

    /// A page information segment's data.
    fn page_info(w: u32, h: u32, flags: u8) -> Vec<u8> {
        let mut d = w.to_be_bytes().to_vec();
        d.extend(h.to_be_bytes());
        d.extend([0; 8]);
        d.push(flags);
        d.extend([0, 0]);
        d
    }

    /// An immediate lossless generic region's data: region information,
    /// generic flags, adaptive template pixels unless MMR, coded data.
    fn region(w: u32, h: u32, flags: u8, coded: &[u8]) -> Vec<u8> {
        let mut d = w.to_be_bytes().to_vec();
        d.extend(h.to_be_bytes());
        d.extend([0; 8]);
        d.push(0);
        d.push(flags);
        if flags & 1 == 0 {
            // The nominal adaptive pixels of each template.
            if flags >> 1 & 3 == 0 {
                d.extend([3, 0xff, 0xfd, 0xff, 2, 0xfe, 0xfe, 0xfe]);
            } else {
                d.extend([3, 0xff]);
            }
        }
        d.extend_from_slice(coded);
        d
    }

    /// A 16x5 bitmap (0 for black, as fax_encode takes it), MMR coded.
    fn mmr() -> (Vec<u8>, Vec<u8>) {
        let rows: Vec<u8> = vec![0xff, 0xff, 0x0f, 0xf0, 0x00, 0x00, 0xaa, 0x55, 0xff, 0x7e];
        let coded = crate::fax_encode::compress_g4(&rows, 16, 5, 2, 1 << 20).unwrap_or_default();
        (rows, coded)
    }

    /// Deterministic bytes for arithmetic-coded data.
    fn noise(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (x >> 16) as u8
            })
            .collect()
    }

    /// A case: its name, the stream data and the globals stream data.
    type Case = (&'static str, Vec<u8>, Option<Vec<u8>>);

    /// Embedded streams and their globals, decoded with each.
    fn cases() -> Vec<Case> {
        let (_, coded) = mmr();
        let page = segment_with(0, 48, &[], 1, &page_info(16, 5, 0));
        let end = segment_with(9, 49, &[], 1, &[]);
        let page_with = |segs: &[Vec<u8>]| [&page[..], &segs.concat(), &end].concat();
        let generic =
            |flags: u8, coded: &[u8]| segment_with(1, 39, &[], 1, &region(16, 5, flags, coded));
        let header = |flags: u8, segs: &[u8]| {
            let mut f = FILE_ID.to_vec();
            f.push(flags);
            if flags & 2 == 0 {
                f.extend(1u32.to_be_bytes());
            }
            f.extend_from_slice(segs);
            f
        };
        // Random-access organisation: every header, then every body.
        let random = {
            let segs = [
                segment_with(0, 48, &[], 1, &page_info(16, 5, 0)),
                generic(1, &coded),
                segment_with(2, 51, &[], 0, &[]),
            ];
            let mut heads = Vec::new();
            let mut bodies = Vec::new();
            for s in &segs {
                let (_, _, len, size) = segment_header(s).unwrap_or_default();
                heads.extend_from_slice(&s[..size]);
                bodies.extend_from_slice(&s[size..size + len as usize]);
            }
            header(0, &[heads, bodies].concat())
        };
        let mut unknown_length = segment_with(1, 38, &[], 1, &region(16, 5, 1, &coded));
        let at = unknown_length.len() - region(16, 5, 1, &coded).len() - 4;
        unknown_length[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        vec![
            ("mmr", page_with(&[generic(1, &coded)]), None),
            ("template_0", page_with(&[generic(0, &noise(40, 1))]), None),
            ("template_1", page_with(&[generic(2, &noise(40, 2))]), None),
            ("template_2", page_with(&[generic(4, &noise(40, 3))]), None),
            ("template_3", page_with(&[generic(6, &noise(40, 4))]), None),
            (
                "typical_prediction",
                page_with(&[generic(8, &noise(40, 5))]),
                None,
            ),
            (
                "file_sequential",
                header(1, &page_with(&[generic(1, &coded)])),
                None,
            ),
            (
                "file_unknown_pages",
                header(3, &page_with(&[generic(1, &coded)])),
                None,
            ),
            ("file_random_access", random, None),
            ("file_12_pixels", header(4, &[]), None),
            ("file_colored", header(8, &[]), None),
            (
                "unknown_length",
                [&page[..], &unknown_length].concat(),
                None,
            ),
            (
                "wide_numbers",
                [
                    segment_with(300, 48, &[], 1, &page_info(16, 5, 0)),
                    segment_with(70000, 39, &[300], 1, &region(16, 5, 1, &coded)),
                    segment_with(70001, 49, &[300, 70000], 1, &[]),
                ]
                .concat(),
                None,
            ),
            (
                "wide_page",
                [
                    segment_with(0, 48, &[], 300, &page_info(16, 5, 0)),
                    segment_with(1, 39, &[], 300, &region(16, 5, 1, &coded)),
                ]
                .concat(),
                None,
            ),
            (
                "many_references",
                page_with(&[segment_with(2, 62, &[0, 1, 0, 1, 0, 1], 1, &[0; 4])]),
                None,
            ),
            (
                "colored_page",
                page_with(&[])
                    .iter()
                    .enumerate()
                    .map(|(i, &b)| if i == 11 + 16 { 0x80 } else { b })
                    .collect(),
                None,
            ),
            (
                "twelve_pixels",
                page_with(&[generic(0x20, &noise(40, 6))]),
                None,
            ),
            (
                "ext_template",
                page_with(&[generic(0x10, &noise(40, 7))]),
                None,
            ),
            (
                "colored_text_region",
                page_with(&[segment_with(
                    1,
                    6,
                    &[],
                    1,
                    &[[0; 16].as_slice(), &[8, 0, 0]].concat(),
                )]),
                None,
            ),
            (
                "palette",
                page_with(&[segment_with(1, 54, &[], 1, &[0; 4])]),
                None,
            ),
            (
                "used_context",
                page_with(&[segment_with(1, 0, &[], 1, &[0x01, 0x00, 0, 0])]),
                None,
            ),
            (
                "unknown_type",
                page_with(&[segment_with(1, 33, &[], 1, &[1, 2, 3]), generic(1, &coded)]),
                None,
            ),
            (
                "truncated",
                page_with(&[generic(1, &coded)])[..60].to_vec(),
                None,
            ),
            (
                "huge_page",
                [
                    segment_with(0, 48, &[], 1, &page_info(30_000, 30_000, 0)),
                    segment_with(9, 49, &[], 1, &[]),
                ]
                .concat(),
                None,
            ),
            // Globals: harmless, unknown, failing.
            (
                "globals_unknown_segment",
                page_with(&[generic(1, &coded)]),
                Some(segment_with(0, 33, &[], 0, &[1, 2])),
            ),
            (
                "globals_failing",
                page_with(&[generic(1, &coded)]),
                Some(segment_with(0, 36, &[], 0, &[0; 18])),
            ),
            (
                "globals_tables",
                page_with(&[generic(1, &coded)]),
                Some(segment_with(
                    0,
                    53,
                    &[],
                    0,
                    &[0x02, 0, 0, 0, 0, 0, 0, 0, 1, 0x88],
                )),
            ),
            // Symbol dictionaries too short for their flags: Huffman,
            // template 1, refinement with template 0; and one long
            // enough to reach the bitmap context check.
            (
                "dict_huffman_short",
                page_with(&[segment_with(
                    1,
                    0,
                    &[],
                    1,
                    &[0x00, 0x01, 0, 0, 0, 0, 0, 0, 0],
                )]),
                None,
            ),
            (
                "dict_template_1_short",
                page_with(&[segment_with(
                    1,
                    0,
                    &[],
                    1,
                    &[0x04, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                )]),
                None,
            ),
            (
                "dict_refinement_short",
                page_with(&[segment_with(
                    1,
                    0,
                    &[],
                    1,
                    &[
                        0x00, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    ],
                )]),
                None,
            ),
            (
                "dict_used_context",
                page_with(&[segment_with(
                    1,
                    0,
                    &[],
                    1,
                    &[[0x01, 0x00].as_slice(), &[0; 16]].concat(),
                )]),
                None,
            ),
            (
                "dict_tiny",
                page_with(&[segment_with(1, 0, &[], 1, &[0x00])]),
                None,
            ),
            // Other segments shorter than jbig2dec reads.
            (
                "pattern_dict_short",
                page_with(&[segment_with(1, 16, &[], 1, &[0; 6])]),
                None,
            ),
            (
                "halftone_short",
                page_with(&[segment_with(1, 23, &[], 1, &[0; 17])]),
                None,
            ),
            (
                "refinement_short",
                page_with(&[segment_with(
                    1,
                    42,
                    &[],
                    1,
                    &[[0; 17].as_slice(), &[0, 0, 0, 0]].concat(),
                )]),
                None,
            ),
            (
                "refinement_template_1_short",
                page_with(&[segment_with(1, 42, &[], 1, &[0; 17])]),
                None,
            ),
            (
                "end_of_stripe_short",
                page_with(&[segment_with(1, 50, &[], 1, &[0; 3])]),
                None,
            ),
            (
                "generic_short",
                page_with(&[segment_with(1, 39, &[], 1, &[0; 17])]),
                None,
            ),
            (
                "generic_short_at",
                page_with(&[segment_with(
                    1,
                    39,
                    &[],
                    1,
                    &[[0; 17].as_slice(), &[0, 1, 2]].concat(),
                )]),
                None,
            ),
            (
                "page_short",
                [segment_with(0, 48, &[], 1, &[0; 18]), end.clone()].concat(),
                None,
            ),
            // Extension segments: a comment, necessary ones with and
            // without the reserved bit, one too short.
            (
                "extension_comment",
                page_with(&[
                    segment_with(1, 62, &[], 1, &[0x20, 0, 0, 0, b'h', b'i']),
                    generic(1, &coded),
                ]),
                None,
            ),
            (
                "extension_necessary",
                page_with(&[segment_with(1, 62, &[], 1, &[0xa0, 0, 0, 7])]),
                None,
            ),
            (
                "extension_unreserved",
                page_with(&[segment_with(1, 62, &[], 1, &[0x80, 0, 0, 7])]),
                None,
            ),
            (
                "extension_short",
                page_with(&[segment_with(1, 62, &[], 1, &[0x20, 0])]),
                None,
            ),
            // Unknown lengths ended by their markers: MMR and arithmetic.
            (
                "unknown_length_mmr_marker",
                {
                    let mut seg = segment_with(
                        1,
                        38,
                        &[],
                        1,
                        &[region(16, 5, 1, &coded), vec![0, 0, 0, 0, 0, 5]].concat(),
                    );
                    let at = seg.len() - region(16, 5, 1, &coded).len() - 6 - 4;
                    seg[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
                    [&page[..], &seg, &end].concat()
                },
                None,
            ),
            (
                "unknown_length_arith_marker",
                {
                    let body = [
                        region(16, 5, 0, &noise(30, 9)),
                        vec![0xff, 0xac, 0, 0, 0, 5],
                    ]
                    .concat();
                    let mut seg = segment_with(1, 38, &[], 1, &body);
                    let at = seg.len() - body.len() - 4;
                    seg[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
                    [&page[..], &seg, &end].concat()
                },
                None,
            ),
            (
                "random_access_truncated",
                {
                    let mut f = FILE_ID.to_vec();
                    f.push(0);
                    f.extend(1u32.to_be_bytes());
                    f.extend(&segment_with(0, 48, &[], 1, &page_info(16, 5, 0))[..11]);
                    f.extend(&segment_with(1, 51, &[], 0, &[])[..]);
                    f.extend(&page_info(16, 5, 0)[..10]);
                    f
                },
                None,
            ),
            (
                "random_access_failing",
                {
                    let mut f = FILE_ID.to_vec();
                    f.push(0);
                    f.extend(1u32.to_be_bytes());
                    let segs = [
                        segment_with(0, 48, &[], 1, &page_info(16, 5, 0x80)),
                        segment_with(1, 51, &[], 0, &[]),
                    ];
                    for s in &segs {
                        f.extend(&s[..11]);
                    }
                    f.extend(page_info(16, 5, 0x80));
                    f
                },
                None,
            ),
        ]
    }

    /// MuPDF 1.28.6's result for each case, from tests/data/oracle.py.
    #[rustfmt::skip]
    const MUPDF_JBIG2: &[(&str, usize, &str, &[&str])] = &[
        ("colored_page", 0, "e3b0c44298fc1c14", &["jbig2dec error: page segment indicates use of color segments (NYI) (segment 0)", "jbig2dec warning: failed to decode; treating as end of file (segment 0)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("colored_text_region", 0, "e3b0c44298fc1c14", &["jbig2dec error: region segment flags indicate use of colored bitmap (NYI) (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("dict_huffman_short", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("dict_refinement_short", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("dict_template_1_short", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("dict_tiny", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("dict_used_context", 0, "e3b0c44298fc1c14", &["jbig2dec warning: segment marks bitmap coding context as used (NYI) (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("end_of_stripe_short", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("ext_template", 10, "c20daaea40952998", &[]),
        ("extension_comment", 10, "61c14e4ba7370ae4", &[]),
        ("extension_necessary", 0, "e3b0c44298fc1c14", &["jbig2dec error: unhandled necessary extension segment type 0xa0000007 (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("extension_short", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("extension_unreserved", 0, "e3b0c44298fc1c14", &["jbig2dec warning: extension segment is marked 'necessary' but not 'reserved' contrary to spec (segment 1)", "jbig2dec error: unhandled necessary extension segment type 0x80000007 (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("file_12_pixels", 0, "e3b0c44298fc1c14", &["jbig2dec warning: JBIG2 file header ignored in supposedly embedded stream (segment 4294967295)", "jbig2dec error: file header indicates use of 12 adaptive template pixels (NYI) (segment 4294967295)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("file_colored", 0, "e3b0c44298fc1c14", &["jbig2dec warning: JBIG2 file header ignored in supposedly embedded stream (segment 4294967295)", "jbig2dec error: file header indicates use of colored region segments (NYI) (segment 4294967295)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("file_random_access", 10, "61c14e4ba7370ae4", &["jbig2dec warning: JBIG2 file header ignored in supposedly embedded stream (segment 4294967295)"]),
        ("file_sequential", 10, "61c14e4ba7370ae4", &["jbig2dec warning: JBIG2 file header ignored in supposedly embedded stream (segment 4294967295)"]),
        ("file_unknown_pages", 10, "61c14e4ba7370ae4", &["jbig2dec warning: JBIG2 file header ignored in supposedly embedded stream (segment 4294967295)"]),
        ("generic_short", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("generic_short_at", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("globals_failing", 0, "e3b0c44298fc1c14", &["jbig2dec error: unhandled segment type 'intermediate generic region' (NYI) (segment 0)", "jbig2dec warning: failed to decode; treating as end of file (segment 0)", "error cannot decode jbig2 globals"]),
        ("globals_tables", 10, "61c14e4ba7370ae4", &[]),
        ("globals_unknown_segment", 10, "61c14e4ba7370ae4", &["jbig2dec warning: unknown segment type 33 (segment 0)"]),
        ("halftone_short", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("huge_page", 0, "e3b0c44298fc1c14", &["error compression bomb detected"]),
        ("many_references", 10, "0083af118d18a63c", &["jbig2dec warning: unhandled non-necessary extension segment, skipping (segment 2)"]),
        ("mmr", 10, "61c14e4ba7370ae4", &[]),
        ("page_short", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 0)", "jbig2dec warning: failed to decode; treating as end of file (segment 0)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("palette", 0, "e3b0c44298fc1c14", &["jbig2dec warning: unhandled segment type 'color palette' (NYI) (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("pattern_dict_short", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("random_access_failing", 0, "e3b0c44298fc1c14", &["jbig2dec warning: JBIG2 file header ignored in supposedly embedded stream (segment 4294967295)", "jbig2dec error: page segment indicates use of color segments (NYI) (segment 0)", "jbig2dec warning: failed to decode; treating as end of file (segment 0)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("random_access_truncated", 0, "e3b0c44298fc1c14", &["jbig2dec warning: JBIG2 file header ignored in supposedly embedded stream (segment 4294967295)", "jbig2dec error: page has no image, cannot be completed (segment 4294967295)", "library error: cannot complete jbig2 image", "read error; treating as end of file"]),
        ("refinement_short", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("refinement_template_1_short", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("template_0", 10, "2a4c2bea95a01761", &[]),
        ("template_1", 10, "c4b65c2349ff59f4", &[]),
        ("template_2", 10, "ee3894230c67a748", &[]),
        ("template_3", 10, "043f4a73277a49c6", &[]),
        ("truncated", 10, "0083af118d18a63c", &[]),
        ("twelve_pixels", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment uses 12 adaptive template pixels (NYI) (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("typical_prediction", 10, "4ddd6d453b2ced95", &[]),
        ("unknown_length", 10, "61c14e4ba7370ae4", &["jbig2dec warning: file has an invalid segment data length; trying to decode using the available data (segment 1)"]),
        ("unknown_length_arith_marker", 10, "f2dfd88c384bf2e8", &[]),
        ("unknown_length_mmr_marker", 10, "61c14e4ba7370ae4", &[]),
        ("unknown_type", 10, "61c14e4ba7370ae4", &["jbig2dec warning: unknown segment type 33 (segment 1)"]),
        ("used_context", 0, "e3b0c44298fc1c14", &["jbig2dec error: segment too short (segment 1)", "jbig2dec warning: failed to decode; treating as end of file (segment 1)", "library error: cannot decode jbig2 image", "read error; treating as end of file"]),
        ("wide_numbers", 10, "61c14e4ba7370ae4", &[]),
        ("wide_page", 10, "61c14e4ba7370ae4", &[]),
    ];

    const EXTRAS: &str = "/Filter /JBIG2Decode /DecodeParms << /JBIG2Globals $obj4 >>";

    #[test]
    fn segments_decode_as_in_mupdf() {
        let dump = std::env::var_os("NUPDF_DUMP_JBIG2_CASES");
        let mut wrong = Vec::new();
        for (name, data, globals) in cases() {
            if let Some(dir) = &dump {
                let path = std::path::Path::new(dir).join(format!("{name}.jb2"));
                std::fs::write(&path, &data).unwrap_or_else(|e| panic!("{e}"));
                if let Some(g) = &globals {
                    let mut p = path.into_os_string();
                    p.push(".obj4");
                    std::fs::write(p, g).unwrap_or_else(|e| panic!("{e}"));
                }
            }
            let got = crate::filter::decode_as_mupdf(EXTRAS, &data, globals.as_deref());
            match MUPDF_JBIG2.iter().find(|c| c.0 == name) {
                Some(&(_, len, hash, warnings))
                    if len == got.0 && hash == got.1 && got.2 == warnings => {}
                Some(want) => wrong.push(format!("{name}:\n  got   {got:?}\n  MuPDF {want:?}")),
                None => wrong.push(format!("{name}: no MuPDF result (got {got:?})")),
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn mmr_regions_decode_to_their_bitmap() {
        let (rows, coded) = mmr();
        let mut data = segment_with(0, 48, &[], 1, &page_info(16, 5, 0));
        data.extend(segment_with(1, 39, &[], 1, &region(16, 5, 1, &coded)));
        let diag = Diagnostics::new();
        let mut s = open(Stream::memory(&data, &diag), None);
        assert_eq!(s.read_best(0, 0, 1 << 20), Ok(rows));
        // Empty globals are none at all.
        assert!(matches!(load_globals(&[], &diag), Ok(None)));
    }

    #[test]
    fn undecodable_pages_end_the_stream_empty() {
        let diag = Diagnostics::new();
        let mut s = open(Stream::memory(b"not jbig2", &diag), None);
        assert_eq!(s.read_best(0, 0, 1 << 20), Ok(Vec::new()));
        assert!(diag.has("stream.read_error"));
    }
}
