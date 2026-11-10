// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF include/mupdf/fitz/stream.h, source/fitz/stream-read.c,
// source/fitz/stream-open.c and source/fitz/filter-basic.c (the endstream
// filter), Copyright (C) 2004-2026 Artifex Software, Inc.

//! Streams as MuPDF reads them.
//!
//! A stream is a chain of stages; each stage produces a buffer at a time
//! from the stage below (`next`), and readers take bytes from the top. The
//! details decide what a damaged stream yields, so they follow MuPDF's:
//!
//! - A stage that fails while producing a buffer loses that buffer, and the
//!   reader above sees the end of the data after a reported error and a
//!   "read error" warning (`fz_available`).
//! - A stage can mark itself finished while handing over its last buffer.
//!   `available` then returns nothing even if bytes are still buffered,
//!   while `read_byte` still returns them.
//! - Each stage's buffer size bounds what it produces per call, and the
//!   whole-stream reader (`read_best`) asks for at most what is left in its
//!   own growing buffer.

use crate::diag::Diagnostics;

/// What a stage's `next` produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Bytes are in the buffer (an empty buffer means the end).
    Data,
    /// Bytes are in the buffer, and the stage is finished: MuPDF sets
    /// `stm->eof` while returning them.
    Finished,
    /// No more data.
    Eof,
}

/// One stage of a stream (MuPDF's `next` function and its state). `next`
/// replaces `out` with the next bytes; `max` is the reader's hint, which
/// some stages honor and others ignore, as in MuPDF. Errors are report
/// lines (`format error: ...`).
pub trait Source {
    fn next(&mut self, max: usize, out: &mut Vec<u8>, diag: &Diagnostics) -> Result<Next, String>;

    /// Whether this is the run-length decoder (`fz_open_rld` refuses to
    /// stack another on top).
    fn is_rld(&self) -> bool {
        false
    }
}

/// `fz_stream`.
pub struct Stream<'a> {
    source: Box<dyn Source + 'a>,
    buf: Vec<u8>,
    rp: usize,
    eof: bool,
    error: bool,
    /// Bit reader state (`stm->bits`, `stm->avail`); `bits` is -1 at EOF.
    bits: i32,
    avail: u32,
    diag: &'a Diagnostics,
}

impl std::fmt::Debug for Stream<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream")
            .field("buffered", &(self.buf.len() - self.rp))
            .field("eof", &self.eof)
            .field("error", &self.error)
            .finish()
    }
}

/// Smallest size above which `read_best` suspects a compression bomb.
const MIN_BOMB: usize = 100 << 20;

/// Why a whole-stream read failed. Damaged data never fails a read; it
/// ends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// `fz_read_best`'s "compression bomb detected".
    Bomb,
    /// More than `Limits::max_decoded_bytes`.
    Limit,
}

impl<'a> Stream<'a> {
    pub fn new(source: impl Source + 'a, diag: &'a Diagnostics) -> Stream<'a> {
        Stream {
            source: Box::new(source),
            buf: Vec::new(),
            rp: 0,
            eof: false,
            error: false,
            bits: 0,
            avail: 0,
            diag,
        }
    }

    /// `fz_open_memory`.
    pub fn memory(data: &'a [u8], diag: &'a Diagnostics) -> Stream<'a> {
        Stream::new(Memory(Some(data)), diag)
    }

    pub fn diag(&self) -> &'a Diagnostics {
        self.diag
    }

    /// Whether a stage failed and was treated as the end (`stm->error`).
    pub fn had_error(&self) -> bool {
        self.error
    }

    pub fn is_rld(&self) -> bool {
        self.source.is_rld()
    }

    /// Marks the stream finished, as `fz_open_rld` does to a stacked one.
    pub fn finish(&mut self) {
        self.eof = true;
    }

    /// Calls `next`. A failure is reported and becomes the end of the data.
    fn fill(&mut self, max: usize) -> Next {
        self.buf.clear();
        self.rp = 0;
        match self.source.next(max, &mut self.buf, self.diag) {
            Ok(next) => {
                if next == Next::Eof || self.buf.is_empty() {
                    self.buf.clear();
                    self.eof = true;
                    Next::Eof
                } else {
                    if next == Next::Finished {
                        self.eof = true;
                    }
                    next
                }
            }
            Err(e) => {
                self.buf.clear();
                for line in e.lines().filter(|l| !l.is_empty()) {
                    self.diag.warn("error.reported", line);
                }
                self.diag
                    .warn("stream.read_error", "read error; treating as end of file");
                self.error = true;
                self.eof = true;
                Next::Eof
            }
        }
    }

    /// `fz_available`: how many bytes are buffered, reading more if none
    /// are. A finished stream has none, buffered or not.
    pub fn available(&mut self, max: usize) -> usize {
        if self.eof {
            return 0;
        }
        let len = self.buf.len() - self.rp;
        if len > 0 {
            return len;
        }
        match self.fill(max) {
            Next::Eof => 0,
            _ => self.buf.len(),
        }
    }

    /// The buffered bytes (after `available`).
    pub fn data(&self) -> &[u8] {
        &self.buf[self.rp..]
    }

    pub fn consume(&mut self, n: usize) {
        self.rp = (self.rp + n).min(self.buf.len());
    }

    /// `fz_read`.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let mut count = 0;
        while count < out.len() {
            let n = self.available(out.len() - count).min(out.len() - count);
            if n == 0 {
                break;
            }
            out[count..count + n].copy_from_slice(&self.buf[self.rp..self.rp + n]);
            self.rp += n;
            count += n;
        }
        count
    }

    /// `fz_read_byte`: buffered bytes come out even after the stream is
    /// finished.
    pub fn read_byte(&mut self) -> Option<u8> {
        if self.rp < self.buf.len() {
            self.rp += 1;
            return Some(self.buf[self.rp - 1]);
        }
        if self.eof {
            return None;
        }
        match self.fill(1) {
            Next::Eof => None,
            _ => {
                self.rp = 1;
                Some(self.buf[0])
            }
        }
    }

    /// `fz_peek_byte`.
    pub fn peek_byte(&mut self) -> Option<u8> {
        if self.rp < self.buf.len() {
            return Some(self.buf[self.rp]);
        }
        if self.eof {
            return None;
        }
        match self.fill(1) {
            Next::Eof => None,
            _ => Some(self.buf[0]),
        }
    }

    /// `fz_is_eof`.
    pub fn is_eof(&mut self) -> bool {
        if self.rp == self.buf.len() {
            if self.eof {
                return true;
            }
            return self.peek_byte().is_none();
        }
        false
    }

    fn read_byte_or_eof(&mut self) -> i32 {
        self.read_byte().map_or(-1, i32::from)
    }

    /// `fz_read_bits`: the next `n` bits, most significant first. At the
    /// end of the data the missing bits read as ones, as in MuPDF.
    pub fn read_bits(&mut self, mut n: u32) -> u32 {
        let mask = |k: u32| if k >= 32 { u32::MAX } else { (1u32 << k) - 1 };
        if n <= self.avail {
            self.avail -= n;
            return ((self.bits >> self.avail) as u32) & mask(n);
        }
        let mut x = (self.bits as u32) & mask(self.avail);
        n -= self.avail;
        self.avail = 0;
        while n > 8 {
            x = (x << 8) | (self.read_byte_or_eof() as u32);
            n -= 8;
        }
        if n > 0 {
            self.bits = self.read_byte_or_eof();
            self.avail = 8 - n;
            x = (x << n) | ((self.bits >> self.avail) as u32);
        }
        x
    }

    /// `fz_is_eof_bits`.
    pub fn is_eof_bits(&mut self) -> bool {
        self.is_eof() && (self.avail == 0 || self.bits == -1)
    }

    /// `fz_sync_bits`.
    pub fn sync_bits(&mut self) {
        self.avail = 0;
    }

    /// `fz_read_best` (`fz_read_all` when `worst_case` is 0): reads to the
    /// end into a buffer of `initial` bytes (at least 1024) plus one, grown
    /// by half when full, asking for at most what is left of it each time.
    /// Fails only on a suspected compression bomb or the decoded-size
    /// limit; damaged data ends the read instead.
    pub fn read_best(
        &mut self,
        initial: usize,
        worst_case: usize,
        limit: usize,
    ) -> Result<Vec<u8>, ReadError> {
        let check_bomb = initial > 0;
        let worst = if worst_case == 0 {
            initial.saturating_mul(200)
        } else {
            worst_case
        }
        .max(MIN_BOMB);
        let mut cap = initial.max(1024).saturating_add(1);
        let mut out: Vec<u8> = Vec::with_capacity(cap.min(1 << 20));
        loop {
            if out.len() == cap {
                cap = cap.saturating_add(cap >> 1);
            }
            if check_bomb && out.len() > worst {
                return Err(ReadError::Bomb);
            }
            if out.len() > limit {
                return Err(ReadError::Limit);
            }
            // fz_read into what is left of the buffer.
            let mut space = cap - out.len();
            let mut count = 0;
            while space > 0 {
                let n = self.available(space).min(space);
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&self.buf[self.rp..self.rp + n]);
                self.rp += n;
                space -= n;
                count += n;
            }
            if count == 0 {
                return Ok(out);
            }
        }
    }

    /// Reads to the end one byte at a time, as MuPDF's lexer and the xref
    /// stream reader do (`fz_read_byte`), so bytes a finished stage still
    /// holds are read.
    pub fn read_bytewise(&mut self, limit: usize) -> Result<Vec<u8>, ReadError> {
        let mut out = Vec::new();
        loop {
            if self.rp < self.buf.len() {
                out.extend_from_slice(&self.buf[self.rp..]);
                self.rp = self.buf.len();
                if out.len() > limit {
                    return Err(ReadError::Limit);
                }
            }
            if self.eof || self.fill(1) == Next::Eof {
                return Ok(out);
            }
        }
    }
}

/// `fz_open_memory`: all of the data, then the end.
struct Memory<'a>(Option<&'a [u8]>);

impl Source for Memory<'_> {
    fn next(&mut self, _max: usize, out: &mut Vec<u8>, _: &Diagnostics) -> Result<Next, String> {
        match self.0.take() {
            Some(data) => {
                out.extend_from_slice(data);
                Ok(Next::Data)
            }
            None => Ok(Next::Eof),
        }
    }
}

/// MuPDF's file stream hands out at most this much after each seek.
const FILE_CHUNK: usize = 4096;

/// `fz_open_endstream_filter`: /Length bytes of the file from `offset`,
/// then, since /Length may be wrong, more until `endstream`. The search
/// works in windows of 32, 64, ... 4096 bytes and holds back 11 bytes each
/// time in case the marker straddles two windows.
#[derive(Debug)]
pub struct Endstream<'a> {
    file: &'a [u8],
    remain: usize,
    offset: usize,
    window: Vec<u8>,
    size: usize,
    warned: bool,
}

impl<'a> Endstream<'a> {
    pub fn new(file: &'a [u8], length: usize, offset: usize) -> Endstream<'a> {
        Endstream {
            file,
            remain: length,
            offset,
            window: Vec::new(),
            // Doubled before first use.
            size: 16,
            warned: false,
        }
    }
}

impl Source for Endstream<'_> {
    fn next(&mut self, _max: usize, out: &mut Vec<u8>, diag: &Diagnostics) -> Result<Next, String> {
        let left = self.file.len().saturating_sub(self.offset);
        if self.remain > 0 {
            let n = left.min(FILE_CHUNK).min(self.remain);
            if n == 0 {
                return Ok(Next::Eof);
            }
            out.extend_from_slice(&self.file[self.offset..self.offset + n]);
            self.remain -= n;
            self.offset += n;
            return Ok(Next::Data);
        }

        self.size = (self.size * 2).min(4096);
        let read = self.size.saturating_sub(self.window.len()).min(left);
        self.window
            .extend_from_slice(&self.file[self.offset..self.offset + read]);
        self.offset += read;

        let in_buffer = self.window.len();
        let (n, found) = match memchr::memmem::find(&self.window, b"endstream") {
            Some(mut at) => {
                // The end-of-line before `endstream` is not data.
                if at > 0 && self.window[at - 1] == b'\n' {
                    at -= 1;
                }
                if at > 0 && self.window[at - 1] == b'\r' {
                    at -= 1;
                }
                (at, true)
            }
            None if in_buffer > 11 => (in_buffer - 11, false),
            None => (in_buffer, false),
        };
        out.extend_from_slice(&self.window[..n]);
        self.window.drain(..n);
        if n == 0 {
            return Ok(Next::Eof);
        }
        if !self.warned {
            self.warned = true;
            diag.warn("stream.length_incorrect", "PDF stream Length incorrect");
        }
        Ok(if found { Next::Finished } else { Next::Data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(file: &[u8], start: usize, length: usize) -> Vec<u8> {
        let diag = Diagnostics::new();
        let mut s = Stream::new(Endstream::new(file, length, start), &diag);
        s.read_best(length, 0, usize::MAX).unwrap_or_default()
    }

    #[test]
    fn endstream_trusts_length_then_scans() {
        let data = b"stream\nABCDE\nendstream";
        assert_eq!(read_all(data, 7, 5), b"ABCDE");
        assert_eq!(read_all(data, 7, 3), b"ABCDE");
        assert_eq!(read_all(data, 7, 0), b"ABCDE");
        assert_eq!(read_all(b"stream\r\nAB\r\nendstream", 8, 0), b"AB");
        // No endstream: everything to the end of the file.
        assert_eq!(read_all(b"stream\nABC", 7, 1), b"ABC");
        let diag = Diagnostics::new();
        let mut s = Stream::new(Endstream::new(data, 3, 7), &diag);
        let _ = s.read_best(3, 0, usize::MAX);
        assert!(diag.has("stream.length_incorrect"));
    }

    #[test]
    fn a_found_endstream_drops_what_does_not_fit_the_reader() {
        // Measured with MuPDF 1.28.6: a /Length that overshoots into
        // "endstream", and the next marker `dist` bytes later.
        for (dist, expect) in [
            (500, 521),
            (1500, 1025),
            (2200, 2221),
            (3000, 2305),
            (5000, 5021),
        ] {
            let mut file = b"<</Length 3>>stream\r\n\r\nendstream\nendobj\n% ".to_vec();
            file.extend(std::iter::repeat_n(b'x', dist));
            file.extend(b"\nendstream\n");
            assert_eq!(read_all(&file, 21, 3).len(), expect, "{dist}");
        }
    }

    #[test]
    fn bytewise_readers_still_get_what_a_finished_stage_holds() {
        let mut file = b"<</Length 3>>stream\r\n\r\nendstream\nendobj\n% ".to_vec();
        file.extend(std::iter::repeat_n(b'x', 3000));
        file.extend(b"\nendstream\n");
        let diag = Diagnostics::new();
        let mut s = Stream::new(Endstream::new(&file, 3, 21), &diag);
        assert_eq!(s.read_bytewise(usize::MAX).map(|d| d.len()), Ok(3021));
    }

    #[test]
    fn bits_read_as_ones_past_the_end() {
        let diag = Diagnostics::new();
        let mut s = Stream::memory(&[0b1010_1100, 0x0f], &diag);
        assert_eq!(s.read_bits(3), 0b101);
        assert_eq!(s.read_bits(9), 0b0_1100_0000);
        assert!(!s.is_eof_bits());
        assert_eq!(s.read_bits(4), 0xf);
        assert!(s.is_eof_bits());
        // Unmasked, as in MuPDF: every bit past the end is set.
        assert_eq!(s.read_bits(12), u32::MAX);
    }

    #[test]
    fn whole_reads_detect_compression_bombs() {
        struct Zeros;
        impl Source for Zeros {
            fn next(
                &mut self,
                _: usize,
                out: &mut Vec<u8>,
                _: &Diagnostics,
            ) -> Result<Next, String> {
                out.resize(4096, 0);
                Ok(Next::Data)
            }
        }
        let diag = Diagnostics::new();
        let mut s = Stream::new(Zeros, &diag);
        assert_eq!(s.read_best(10, 0, usize::MAX), Err(ReadError::Bomb));
        let mut s = Stream::new(Zeros, &diag);
        assert_eq!(s.read_best(10, 0, 1 << 20), Err(ReadError::Limit));
    }
}
