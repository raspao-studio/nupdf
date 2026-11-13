// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from zlib 1.3.2 compress.c, deflate.c, deflate.h, trees.c and
// adler32.c, as bundled with MuPDF 1.28.6. This is an altered version,
// translated into Rust; NOTICE.md says what changed. zlib's notice:
//
//   Copyright (C) 1995-2026 Jean-loup Gailly and Mark Adler
//
//   This software is provided 'as-is', without any express or implied
//   warranty.  In no event will the authors be held liable for any damages
//   arising from the use of this software.
//
//   Permission is granted to anyone to use this software for any purpose,
//   including commercial applications, and to alter it and redistribute it
//   freely, subject to the following restrictions:
//
//   1. The origin of this software must not be misrepresented; you must not
//      claim that you wrote the original software. If you use this software
//      in a product, an acknowledgment in the product documentation would be
//      appreciated but is not required.
//   2. Altered source versions must be plainly marked as such, and must not be
//      misrepresented as being the original software.
//   3. This notice may not be removed or altered from any source distribution.
//
//   Jean-loup Gailly        Mark Adler
//   jloup@gzip.org          madler@alumni.caltech.edu

//! zlib's `compress2`, the way MuPDF calls it to deflate streams on save
//! (`deflatebuf`): the whole input at once, `Z_FINISH`, an output buffer of
//! `compressBound` bytes, a 32K window, memory level 8 and the default
//! strategy. Only that path is ported, and its output is zlib's byte for
//! byte. General-purpose encoders, zlib-rs included, follow zlib-ng's
//! match finder and produce different bytes for the same level.

const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const W_SIZE: usize = 1 << 15;
const W_MASK: usize = W_SIZE - 1;
const WINDOW_SIZE: usize = 2 * W_SIZE;
const HASH_SIZE: usize = 1 << 15;
const HASH_MASK: usize = HASH_SIZE - 1;
const HASH_SHIFT: usize = 5;
const LIT_BUFSIZE: usize = 1 << 14;
const PENDING_BUF_SIZE: usize = LIT_BUFSIZE * 4;
const SYM_END: usize = (LIT_BUFSIZE - 1) * 3;
const MIN_LOOKAHEAD: usize = MAX_MATCH + MIN_MATCH + 1;
const MAX_DIST: usize = W_SIZE - MIN_LOOKAHEAD;
const WIN_INIT: usize = MAX_MATCH;
const TOO_FAR: usize = 4096;
const MAX_STORED: usize = 65535;

const LENGTH_CODES: usize = 29;
const LITERALS: usize = 256;
const L_CODES: usize = LITERALS + 1 + LENGTH_CODES;
const D_CODES: usize = 30;
const BL_CODES: usize = 19;
const HEAP_SIZE: usize = 2 * L_CODES + 1;
const MAX_BITS: usize = 15;
const MAX_BL_BITS: usize = 7;
const END_BLOCK: usize = 256;
const REP_3_6: usize = 16;
const REPZ_3_10: usize = 17;
const REPZ_11_138: usize = 18;

const EXTRA_LBITS: [u8; LENGTH_CODES] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const EXTRA_DBITS: [u8; D_CODES] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const EXTRA_BLBITS: [u8; BL_CODES] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 3, 7];
const BL_ORDER: [u8; BL_CODES] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// `ct_data`: a tree node. As in zlib, a frequency and a code share `fc`,
/// a parent and a length share `dl`, and the tree builders rely on it.
#[derive(Debug, Clone, Copy, Default)]
struct Ct {
    fc: u16,
    dl: u16,
}

/// `configuration_table`: good length, lazy, nice length, chain.
const CONFIG: [(usize, usize, usize, usize); 10] = [
    (0, 0, 0, 0),
    (4, 4, 8, 4),
    (4, 5, 16, 8),
    (4, 6, 32, 32),
    (4, 4, 16, 16),
    (8, 16, 32, 32),
    (8, 16, 128, 128),
    (8, 32, 128, 256),
    (32, 128, 258, 1024),
    (32, 258, 258, 4096),
];

/// The tables `tr_static_init` computes (zlib ships them as `trees.h`).
struct Tables {
    static_ltree: [Ct; L_CODES + 2],
    static_dtree: [Ct; D_CODES],
    dist_code: [u8; 512],
    length_code: [u8; MAX_MATCH - MIN_MATCH + 1],
    base_length: [u16; LENGTH_CODES],
    base_dist: [u16; D_CODES],
}

static TABLES: Tables = tables();

const fn bi_reverse(mut code: u32, mut len: u32) -> u32 {
    let mut res = 0;
    loop {
        res |= code & 1;
        code >>= 1;
        res <<= 1;
        len -= 1;
        if len == 0 {
            return res >> 1;
        }
    }
}

const fn tables() -> Tables {
    let mut t = Tables {
        static_ltree: [Ct { fc: 0, dl: 0 }; L_CODES + 2],
        static_dtree: [Ct { fc: 0, dl: 0 }; D_CODES],
        dist_code: [0; 512],
        length_code: [0; MAX_MATCH - MIN_MATCH + 1],
        base_length: [0; LENGTH_CODES],
        base_dist: [0; D_CODES],
    };
    let mut length = 0;
    let mut code = 0;
    while code < LENGTH_CODES - 1 {
        t.base_length[code] = length as u16;
        let mut n = 0;
        while n < 1 << EXTRA_LBITS[code] {
            t.length_code[length] = code as u8;
            length += 1;
            n += 1;
        }
        code += 1;
    }
    // Length 255 (a match of 258) has two encodings; the shorter wins.
    t.length_code[length - 1] = code as u8;

    let mut dist = 0;
    code = 0;
    while code < 16 {
        t.base_dist[code] = dist as u16;
        let mut n = 0;
        while n < 1 << EXTRA_DBITS[code] {
            t.dist_code[dist] = code as u8;
            dist += 1;
            n += 1;
        }
        code += 1;
    }
    dist >>= 7;
    while code < D_CODES {
        t.base_dist[code] = (dist << 7) as u16;
        let mut n = 0;
        while n < 1 << (EXTRA_DBITS[code] - 7) {
            t.dist_code[256 + dist] = code as u8;
            dist += 1;
            n += 1;
        }
        code += 1;
    }

    let mut bl_count = [0u16; MAX_BITS + 1];
    let mut n = 0;
    while n < L_CODES + 2 {
        let len = match n {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
        t.static_ltree[n].dl = len;
        bl_count[len as usize] += 1;
        n += 1;
    }
    // gen_codes over all 288 codes, so the tree is canonical.
    let mut next_code = [0u16; MAX_BITS + 1];
    let mut c: u32 = 0;
    let mut bits = 1;
    while bits <= MAX_BITS {
        c = (c + bl_count[bits - 1] as u32) << 1;
        next_code[bits] = c as u16;
        bits += 1;
    }
    n = 0;
    while n < L_CODES + 2 {
        let len = t.static_ltree[n].dl as usize;
        t.static_ltree[n].fc = bi_reverse(next_code[len] as u32, len as u32) as u16;
        next_code[len] += 1;
        n += 1;
    }
    n = 0;
    while n < D_CODES {
        t.static_dtree[n].dl = 5;
        t.static_dtree[n].fc = bi_reverse(n as u32, 5) as u16;
        n += 1;
    }
    t
}

fn d_code(dist: usize) -> usize {
    let t = &TABLES;
    if dist < 256 {
        t.dist_code[dist] as usize
    } else {
        t.dist_code[256 + (dist >> 7)] as usize
    }
}

/// `static_tree_desc`.
struct StaticDesc {
    tree: Option<&'static [Ct]>,
    extra: &'static [u8],
    extra_base: usize,
    elems: usize,
    max_length: usize,
}

static L_DESC: StaticDesc = StaticDesc {
    tree: Some(&TABLES.static_ltree),
    extra: &EXTRA_LBITS,
    extra_base: LITERALS + 1,
    elems: L_CODES,
    max_length: MAX_BITS,
};
static D_DESC: StaticDesc = StaticDesc {
    tree: Some(&TABLES.static_dtree),
    extra: &EXTRA_DBITS,
    extra_base: 0,
    elems: D_CODES,
    max_length: MAX_BITS,
};
static BL_DESC: StaticDesc = StaticDesc {
    tree: None,
    extra: &EXTRA_BLBITS,
    extra_base: 0,
    elems: BL_CODES,
    max_length: MAX_BL_BITS,
};

/// `zlib`'s `compress2(dest, &compressBound(n), data, n, level)`, with
/// `level` -1 for `Z_DEFAULT_COMPRESSION`. `None` where zlib returns an
/// error: a level out of range, or output that outgrows the buffer (which
/// `compressBound` rules out).
pub fn compress(data: &[u8], level: i32) -> Option<Vec<u8>> {
    let level = match level {
        -1 => 6,
        0..=9 => level as usize,
        _ => return None,
    };
    let mut s = Deflate::new(data, level);
    // compress2: the output buffer is handed over once, the input too, and
    // deflate runs with Z_FINISH until it reports the end of the stream.
    let mut left = compress_bound(data.len());
    loop {
        if s.avail_out == 0 {
            s.avail_out = left;
            left = 0;
        }
        match s.deflate() {
            Ret::Ok => {}
            Ret::StreamEnd => return Some(s.out),
            Ret::BufError => return None,
        }
    }
}

/// `compressBound`.
pub fn compress_bound(n: usize) -> usize {
    n + (n >> 12) + (n >> 14) + (n >> 25) + 13
}

/// How many bytes `a` and `b` (as long as each other) have in common from
/// the start.
fn common_run(a: &[u8], b: &[u8]) -> usize {
    let mut n = 0;
    for (x, y) in a.as_chunks::<8>().0.iter().zip(b.as_chunks::<8>().0) {
        let diff = u64::from_le_bytes(*x) ^ u64::from_le_bytes(*y);
        if diff != 0 {
            return n + (diff.trailing_zeros() / 8) as usize;
        }
        n += 8;
    }
    n + a[n..]
        .iter()
        .zip(&b[n..])
        .take_while(|(x, y)| x == y)
        .count()
}

fn adler32(adler: u32, buf: &[u8]) -> u32 {
    const BASE: u32 = 65521;
    let mut a = adler & 0xffff;
    let mut b = adler >> 16;
    // 5552 is the most bytes before b can overflow 32 bits (zlib's NMAX).
    for chunk in buf.chunks(5552) {
        // A chunk at once: b gains a for each byte and each byte weighted
        // by the steps from it to the end, the sums byte by byte makes.
        let n = chunk.len() as u32;
        let (mut sum, mut weighted) = (0u32, 0u32);
        for (i, &c) in chunk.iter().enumerate() {
            sum += u32::from(c);
            weighted = weighted.wrapping_add((n - i as u32) * u32::from(c));
        }
        // b stays below 2^32 byte by byte (zlib's NMAX), so the sum of its
        // parts does too, and the wrapping is never taken.
        b = b.wrapping_add(n.wrapping_mul(a)).wrapping_add(weighted);
        a += sum;
        a %= BASE;
        b %= BASE;
    }
    (b << 16) | a
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ret {
    Ok,
    StreamEnd,
    BufError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    NeedMore,
    FinishStarted,
    FinishDone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Init,
    Busy,
    Finish,
}

/// The pending buffer and bit buffer (`pending_buf`, `bi_buf`, `bi_valid`).
struct Pending {
    buf: Vec<u8>,
    bi_buf: u16,
    bi_valid: i32,
}

impl Pending {
    fn put_byte(&mut self, c: u8) {
        self.buf.push(c);
    }

    fn put_short(&mut self, w: u16) {
        self.buf.push(w as u8);
        self.buf.push((w >> 8) as u8);
    }

    fn put_short_msb(&mut self, b: u32) {
        self.buf.push((b >> 8) as u8);
        self.buf.push(b as u8);
    }

    fn send_bits(&mut self, value: u32, length: i32) {
        if self.bi_valid > 16 - length {
            self.bi_buf |= (value << self.bi_valid) as u16;
            self.put_short(self.bi_buf);
            self.bi_buf = (value >> (16 - self.bi_valid)) as u16;
            self.bi_valid += length - 16;
        } else {
            self.bi_buf |= (value << self.bi_valid) as u16;
            self.bi_valid += length;
        }
    }

    fn send_code(&mut self, c: usize, tree: &[Ct]) {
        self.send_bits(u32::from(tree[c].fc), i32::from(tree[c].dl));
    }

    fn bi_flush(&mut self) {
        if self.bi_valid == 16 {
            self.put_short(self.bi_buf);
            self.bi_buf = 0;
            self.bi_valid = 0;
        } else if self.bi_valid >= 8 {
            self.put_byte(self.bi_buf as u8);
            self.bi_buf >>= 8;
            self.bi_valid -= 8;
        }
    }

    fn bi_windup(&mut self) {
        if self.bi_valid > 8 {
            self.put_short(self.bi_buf);
        } else if self.bi_valid > 0 {
            self.put_byte(self.bi_buf as u8);
        }
        self.bi_buf = 0;
        self.bi_valid = 0;
    }

    /// `_tr_stored_block`.
    fn stored_block(&mut self, data: &[u8], last: bool) {
        self.send_bits(u32::from(last), 3);
        self.bi_windup();
        self.put_short(data.len() as u16);
        self.put_short(!(data.len() as u16));
        self.buf.extend_from_slice(data);
    }
}

/// What `build_tree` and `gen_bitlen` share across trees.
struct Huff {
    heap: [usize; 2 * L_CODES + 1],
    heap_len: usize,
    heap_max: usize,
    depth: [u8; 2 * L_CODES + 1],
    bl_count: [u16; MAX_BITS + 1],
    /// `ulg` sums; `build_tree` can take them below zero for a moment.
    opt_len: u64,
    static_len: u64,
}

impl Huff {
    fn smaller(&self, tree: &[Ct], n: usize, m: usize) -> bool {
        tree[n].fc < tree[m].fc || (tree[n].fc == tree[m].fc && self.depth[n] <= self.depth[m])
    }

    fn pqdownheap(&mut self, tree: &[Ct], mut k: usize) {
        let v = self.heap[k];
        let mut j = k << 1;
        while j <= self.heap_len {
            if j < self.heap_len && self.smaller(tree, self.heap[j + 1], self.heap[j]) {
                j += 1;
            }
            if self.smaller(tree, v, self.heap[j]) {
                break;
            }
            self.heap[k] = self.heap[j];
            k = j;
            j <<= 1;
        }
        self.heap[k] = v;
    }

    fn gen_bitlen(&mut self, tree: &mut [Ct], max_code: usize, desc: &StaticDesc) {
        let max_length = desc.max_length;
        self.bl_count = [0; MAX_BITS + 1];
        let mut overflow = 0i32;
        tree[self.heap[self.heap_max]].dl = 0;
        let mut h = self.heap_max + 1;
        while h < HEAP_SIZE {
            let n = self.heap[h];
            let mut bits = tree[tree[n].dl as usize].dl as usize + 1;
            if bits > max_length {
                bits = max_length;
                overflow += 1;
            }
            tree[n].dl = bits as u16;
            h += 1;
            if n > max_code {
                continue;
            }
            self.bl_count[bits] += 1;
            let xbits = if n >= desc.extra_base {
                desc.extra[n - desc.extra_base] as usize
            } else {
                0
            };
            let f = u64::from(tree[n].fc);
            self.opt_len = self.opt_len.wrapping_add(f * (bits + xbits) as u64);
            if let Some(stree) = desc.tree {
                self.static_len = self
                    .static_len
                    .wrapping_add(f * (stree[n].dl as usize + xbits) as u64);
            }
        }
        if overflow == 0 {
            return;
        }
        loop {
            let mut bits = max_length - 1;
            while self.bl_count[bits] == 0 {
                bits -= 1;
            }
            self.bl_count[bits] -= 1;
            self.bl_count[bits + 1] += 2;
            self.bl_count[max_length] -= 1;
            overflow -= 2;
            if overflow <= 0 {
                break;
            }
        }
        let mut bits = max_length;
        while bits != 0 {
            let mut n = self.bl_count[bits];
            while n != 0 {
                h -= 1;
                let m = self.heap[h];
                if m > max_code {
                    continue;
                }
                if tree[m].dl as usize != bits {
                    self.opt_len = self.opt_len.wrapping_add(
                        (bits as u64)
                            .wrapping_sub(u64::from(tree[m].dl))
                            .wrapping_mul(u64::from(tree[m].fc)),
                    );
                    tree[m].dl = bits as u16;
                }
                n -= 1;
            }
            bits -= 1;
        }
    }

    /// `build_tree`, returning the tree's `max_code`.
    fn build_tree(&mut self, tree: &mut [Ct], desc: &StaticDesc) -> usize {
        let elems = desc.elems;
        let mut max_code: isize = -1;
        self.heap_len = 0;
        self.heap_max = HEAP_SIZE;
        for (n, t) in tree.iter_mut().enumerate().take(elems) {
            if t.fc != 0 {
                self.heap_len += 1;
                self.heap[self.heap_len] = n;
                max_code = n as isize;
                self.depth[n] = 0;
            } else {
                t.dl = 0;
            }
        }
        // At least two codes, so that at least one bit is sent.
        while self.heap_len < 2 {
            let node = if max_code < 2 {
                max_code += 1;
                max_code as usize
            } else {
                0
            };
            self.heap_len += 1;
            self.heap[self.heap_len] = node;
            tree[node].fc = 1;
            self.depth[node] = 0;
            self.opt_len = self.opt_len.wrapping_sub(1);
            if let Some(stree) = desc.tree {
                self.static_len = self.static_len.wrapping_sub(u64::from(stree[node].dl));
            }
        }
        let max_code = max_code as usize;
        for n in (1..=self.heap_len / 2).rev() {
            self.pqdownheap(tree, n);
        }
        let mut node = elems;
        loop {
            // pqremove
            let n = self.heap[1];
            self.heap[1] = self.heap[self.heap_len];
            self.heap_len -= 1;
            self.pqdownheap(tree, 1);
            let m = self.heap[1];
            self.heap_max -= 1;
            self.heap[self.heap_max] = n;
            self.heap_max -= 1;
            self.heap[self.heap_max] = m;
            tree[node].fc = tree[n].fc.wrapping_add(tree[m].fc);
            self.depth[node] = self.depth[n].max(self.depth[m]) + 1;
            tree[n].dl = node as u16;
            tree[m].dl = node as u16;
            self.heap[1] = node;
            node += 1;
            self.pqdownheap(tree, 1);
            if self.heap_len < 2 {
                break;
            }
        }
        self.heap_max -= 1;
        self.heap[self.heap_max] = self.heap[1];
        self.gen_bitlen(tree, max_code, desc);
        gen_codes(tree, max_code, &self.bl_count);
        max_code
    }
}

fn gen_codes(tree: &mut [Ct], max_code: usize, bl_count: &[u16; MAX_BITS + 1]) {
    let mut next_code = [0u16; MAX_BITS + 1];
    let mut code: u32 = 0;
    for bits in 1..=MAX_BITS {
        code = (code + u32::from(bl_count[bits - 1])) << 1;
        next_code[bits] = code as u16;
    }
    for t in tree.iter_mut().take(max_code + 1) {
        let len = t.dl as usize;
        if len == 0 {
            continue;
        }
        t.fc = bi_reverse(u32::from(next_code[len]), len as u32) as u16;
        next_code[len] = next_code[len].wrapping_add(1);
    }
}

/// The repeat-count states `scan_tree` and `send_tree` share.
fn repeat_limits(curlen: u16, nextlen: u16) -> (usize, usize) {
    if nextlen == 0 {
        (138, 3)
    } else if curlen == nextlen {
        (6, 3)
    } else {
        (7, 4)
    }
}

fn scan_tree(bl_tree: &mut [Ct], tree: &mut [Ct], max_code: usize) {
    let mut prevlen: i32 = -1;
    let mut nextlen = tree[0].dl;
    let mut count = 0;
    let (mut max_count, mut min_count) = if nextlen == 0 { (138, 3) } else { (7, 4) };
    tree[max_code + 1].dl = 0xffff;
    for n in 0..=max_code {
        let curlen = nextlen;
        nextlen = tree[n + 1].dl;
        count += 1;
        if count < max_count && curlen == nextlen {
            continue;
        } else if count < min_count {
            bl_tree[curlen as usize].fc = bl_tree[curlen as usize].fc.wrapping_add(count as u16);
        } else if curlen != 0 {
            if i32::from(curlen) != prevlen {
                bl_tree[curlen as usize].fc = bl_tree[curlen as usize].fc.wrapping_add(1);
            }
            bl_tree[REP_3_6].fc = bl_tree[REP_3_6].fc.wrapping_add(1);
        } else if count <= 10 {
            bl_tree[REPZ_3_10].fc = bl_tree[REPZ_3_10].fc.wrapping_add(1);
        } else {
            bl_tree[REPZ_11_138].fc = bl_tree[REPZ_11_138].fc.wrapping_add(1);
        }
        count = 0;
        prevlen = i32::from(curlen);
        (max_count, min_count) = repeat_limits(curlen, nextlen);
    }
}

fn send_tree(p: &mut Pending, bl_tree: &[Ct], tree: &[Ct], max_code: usize) {
    let mut prevlen: i32 = -1;
    let mut nextlen = tree[0].dl;
    let mut count = 0usize;
    let (mut max_count, mut min_count) = if nextlen == 0 { (138, 3) } else { (7, 4) };
    for n in 0..=max_code {
        let curlen = nextlen;
        nextlen = tree[n + 1].dl;
        count += 1;
        if count < max_count && curlen == nextlen {
            continue;
        } else if count < min_count {
            while count != 0 {
                p.send_code(curlen as usize, bl_tree);
                count -= 1;
            }
        } else if curlen != 0 {
            if i32::from(curlen) != prevlen {
                p.send_code(curlen as usize, bl_tree);
                count -= 1;
            }
            p.send_code(REP_3_6, bl_tree);
            p.send_bits(count as u32 - 3, 2);
        } else if count <= 10 {
            p.send_code(REPZ_3_10, bl_tree);
            p.send_bits(count as u32 - 3, 3);
        } else {
            p.send_code(REPZ_11_138, bl_tree);
            p.send_bits(count as u32 - 11, 7);
        }
        count = 0;
        prevlen = i32::from(curlen);
        (max_count, min_count) = repeat_limits(curlen, nextlen);
    }
}

fn compress_block(p: &mut Pending, sym_buf: &[u8], ltree: &[Ct], dtree: &[Ct]) {
    let t = &TABLES;
    for sym in sym_buf.as_chunks::<3>().0 {
        let mut dist = usize::from(sym[0]) | usize::from(sym[1]) << 8;
        let mut lc = usize::from(sym[2]);
        if dist == 0 {
            p.send_code(lc, ltree);
        } else {
            let code = t.length_code[lc] as usize;
            p.send_code(code + LITERALS + 1, ltree);
            let extra = EXTRA_LBITS[code];
            if extra != 0 {
                lc -= t.base_length[code] as usize;
                p.send_bits(lc as u32, i32::from(extra));
            }
            dist -= 1;
            let code = d_code(dist);
            p.send_code(code, dtree);
            let extra = EXTRA_DBITS[code];
            if extra != 0 {
                dist -= t.base_dist[code] as usize;
                p.send_bits(dist as u32, i32::from(extra));
            }
        }
    }
    p.send_code(END_BLOCK, ltree);
}

/// `deflate_state` together with the `z_stream` fields it touches.
struct Deflate<'a> {
    input: &'a [u8],
    next_in: usize,
    out: Vec<u8>,
    avail_out: usize,
    adler: u32,
    status: Status,
    wrap: i32,
    level: usize,

    window: Vec<u8>,
    prev: Vec<u16>,
    head: Vec<u16>,
    ins_h: usize,
    block_start: i64,
    match_length: usize,
    prev_match: usize,
    match_available: bool,
    strstart: usize,
    match_start: usize,
    lookahead: usize,
    prev_length: usize,
    max_chain_length: usize,
    max_lazy_match: usize,
    good_match: usize,
    nice_match: usize,
    insert: usize,
    high_water: usize,

    dyn_ltree: Vec<Ct>,
    dyn_dtree: Vec<Ct>,
    bl_tree: Vec<Ct>,
    huff: Huff,
    sym_buf: Vec<u8>,
    p: Pending,
}

impl<'a> Deflate<'a> {
    /// `deflateInit` and `deflateReset`.
    fn new(input: &'a [u8], level: usize) -> Deflate<'a> {
        let (good, lazy, nice, chain) = CONFIG[level];
        Deflate {
            input,
            next_in: 0,
            out: Vec::with_capacity(compress_bound(input.len())),
            avail_out: 0,
            adler: 1,
            status: Status::Init,
            wrap: 1,
            level,
            window: vec![0; WINDOW_SIZE],
            prev: vec![0; W_SIZE],
            head: vec![0; HASH_SIZE],
            ins_h: 0,
            block_start: 0,
            match_length: MIN_MATCH - 1,
            prev_match: 0,
            match_available: false,
            strstart: 0,
            match_start: 0,
            lookahead: 0,
            prev_length: MIN_MATCH - 1,
            max_chain_length: chain,
            max_lazy_match: lazy,
            good_match: good,
            nice_match: nice,
            insert: 0,
            high_water: 0,
            dyn_ltree: Self::fresh_tree(HEAP_SIZE, L_CODES),
            dyn_dtree: Self::fresh_tree(2 * D_CODES + 1, D_CODES),
            bl_tree: vec![Ct::default(); 2 * BL_CODES + 1],
            huff: Huff {
                heap: [0; 2 * L_CODES + 1],
                heap_len: 0,
                heap_max: 0,
                depth: [0; 2 * L_CODES + 1],
                bl_count: [0; MAX_BITS + 1],
                opt_len: 0,
                static_len: 0,
            },
            sym_buf: Vec::with_capacity(SYM_END),
            p: Pending {
                buf: Vec::with_capacity(PENDING_BUF_SIZE),
                bi_buf: 0,
                bi_valid: 0,
            },
        }
    }

    fn fresh_tree(size: usize, leaves: usize) -> Vec<Ct> {
        let mut t = vec![Ct::default(); size];
        if leaves == L_CODES {
            t[END_BLOCK].fc = 1;
        }
        t
    }

    fn avail_in(&self) -> usize {
        self.input.len() - self.next_in
    }

    /// `init_block`.
    fn init_block(&mut self) {
        for t in &mut self.dyn_ltree[..L_CODES] {
            t.fc = 0;
        }
        for t in &mut self.dyn_dtree[..D_CODES] {
            t.fc = 0;
        }
        for t in &mut self.bl_tree[..BL_CODES] {
            t.fc = 0;
        }
        self.dyn_ltree[END_BLOCK].fc = 1;
        self.huff.opt_len = 0;
        self.huff.static_len = 0;
        self.sym_buf.clear();
    }

    /// `flush_pending`.
    fn flush_pending(&mut self) {
        self.p.bi_flush();
        let len = self.p.buf.len().min(self.avail_out);
        if len == 0 {
            return;
        }
        self.out.extend_from_slice(&self.p.buf[..len]);
        self.p.buf.drain(..len);
        self.avail_out -= len;
    }

    /// `read_buf` into the window at `at`.
    fn read_to_window(&mut self, at: usize, size: usize) -> usize {
        let len = self.avail_in().min(size);
        if len == 0 {
            return 0;
        }
        let src = &self.input[self.next_in..self.next_in + len];
        self.adler = adler32(self.adler, src);
        self.window[at..at + len].copy_from_slice(src);
        self.next_in += len;
        len
    }

    /// `read_buf` straight to the output.
    fn read_to_out(&mut self, len: usize) {
        let src = &self.input[self.next_in..self.next_in + len];
        self.adler = adler32(self.adler, src);
        self.out.extend_from_slice(src);
        self.next_in += len;
        self.avail_out -= len;
    }

    fn update_hash(&mut self, c: u8) {
        self.ins_h = ((self.ins_h << HASH_SHIFT) ^ usize::from(c)) & HASH_MASK;
    }

    /// `INSERT_STRING`, returning the previous head of the chain.
    fn insert_string(&mut self, at: usize) -> usize {
        self.update_hash(self.window[at + MIN_MATCH - 1]);
        let head = self.head[self.ins_h];
        self.prev[at & W_MASK] = head;
        self.head[self.ins_h] = at as u16;
        usize::from(head)
    }

    fn slide_hash(&mut self) {
        // Each table on its own, as zlib's loops: chained, they did not
        // vectorize, and this runs every 32 KB of input.
        for p in &mut self.head {
            *p = p.saturating_sub(W_SIZE as u16);
        }
        for p in &mut self.prev {
            *p = p.saturating_sub(W_SIZE as u16);
        }
    }

    fn fill_window(&mut self) {
        loop {
            let mut more = WINDOW_SIZE - self.lookahead - self.strstart;
            if self.strstart >= W_SIZE + MAX_DIST {
                let keep = W_SIZE - more;
                self.window.copy_within(W_SIZE..W_SIZE + keep, 0);
                self.match_start = self.match_start.wrapping_sub(W_SIZE);
                self.strstart -= W_SIZE;
                self.block_start -= W_SIZE as i64;
                if self.insert > self.strstart {
                    self.insert = self.strstart;
                }
                self.slide_hash();
                more += W_SIZE;
            }
            if self.avail_in() == 0 {
                break;
            }
            let n = self.read_to_window(self.strstart + self.lookahead, more);
            self.lookahead += n;
            if self.lookahead + self.insert >= MIN_MATCH {
                let mut at = self.strstart - self.insert;
                self.ins_h = usize::from(self.window[at]);
                self.update_hash(self.window[at + 1]);
                while self.insert != 0 {
                    self.update_hash(self.window[at + MIN_MATCH - 1]);
                    self.prev[at & W_MASK] = self.head[self.ins_h];
                    self.head[self.ins_h] = at as u16;
                    at += 1;
                    self.insert -= 1;
                    if self.lookahead + self.insert < MIN_MATCH {
                        break;
                    }
                }
            }
            if self.lookahead >= MIN_LOOKAHEAD || self.avail_in() == 0 {
                break;
            }
        }
        // The window past the data is zeroed up to WIN_INIT bytes; what lies
        // beyond can be stale, and the match finder reads it.
        if self.high_water < WINDOW_SIZE {
            let curr = self.strstart + self.lookahead;
            if self.high_water < curr {
                let init = (WINDOW_SIZE - curr).min(WIN_INIT);
                self.window[curr..curr + init].fill(0);
                self.high_water = curr + init;
            } else if self.high_water < curr + WIN_INIT {
                let init = (curr + WIN_INIT - self.high_water).min(WINDOW_SIZE - self.high_water);
                self.window[self.high_water..self.high_water + init].fill(0);
                self.high_water += init;
            }
        }
    }

    fn longest_match(&mut self, mut cur_match: usize) -> usize {
        let mut chain_length = self.max_chain_length;
        let start = self.strstart;
        let mut best_len = self.prev_length;
        let mut nice_match = self.nice_match;
        let limit = self.strstart.saturating_sub(MAX_DIST);
        let strend = self.strstart + MAX_MATCH;
        let w = &self.window;
        let mut scan_end1 = w[start + best_len - 1];
        let mut scan_end = w[start + best_len];
        if self.prev_length >= self.good_match {
            chain_length >>= 2;
        }
        if nice_match > self.lookahead {
            nice_match = self.lookahead;
        }
        loop {
            let m = cur_match;
            if w[m + best_len] == scan_end
                && w[m + best_len - 1] == scan_end1
                && w[m] == w[start]
                && w[m + 1] == w[start + 1]
            {
                // scan[2] equals match[2] when the hashes do; zlib compares
                // from scan[3], eight bytes between bounds checks, and stops
                // at the first difference or at strend, read or not. That
                // is the length of the common run from 3 up to MAX_MATCH,
                // found here eight bytes at a time.
                let len = common_run(&w[start + 3..strend], &w[m + 3..m + MAX_MATCH]) + 3;
                if len > best_len {
                    self.match_start = cur_match;
                    best_len = len;
                    if len >= nice_match {
                        break;
                    }
                    scan_end1 = w[start + best_len - 1];
                    scan_end = w[start + best_len];
                }
            }
            cur_match = usize::from(self.prev[cur_match & W_MASK]);
            if cur_match <= limit {
                break;
            }
            chain_length -= 1;
            if chain_length == 0 {
                break;
            }
        }
        best_len.min(self.lookahead)
    }

    /// `_tr_tally_lit`; true when the block is full.
    fn tally_lit(&mut self, c: u8) -> bool {
        self.sym_buf.extend_from_slice(&[0, 0, c]);
        self.dyn_ltree[usize::from(c)].fc += 1;
        self.sym_buf.len() == SYM_END
    }

    /// `_tr_tally_dist`; true when the block is full.
    fn tally_dist(&mut self, dist: usize, len: usize) -> bool {
        let t = &TABLES;
        self.sym_buf
            .extend_from_slice(&[dist as u8, (dist >> 8) as u8, len as u8]);
        self.dyn_ltree[t.length_code[len] as usize + LITERALS + 1].fc += 1;
        self.dyn_dtree[d_code(dist - 1)].fc += 1;
        self.sym_buf.len() == SYM_END
    }

    /// `build_bl_tree`, returning the index in `BL_ORDER` of the last
    /// length code to send.
    fn build_bl_tree(&mut self, l_max: usize, d_max: usize) -> usize {
        scan_tree(&mut self.bl_tree, &mut self.dyn_ltree, l_max);
        scan_tree(&mut self.bl_tree, &mut self.dyn_dtree, d_max);
        self.huff.build_tree(&mut self.bl_tree, &BL_DESC);
        let mut max_blindex = BL_CODES - 1;
        while max_blindex >= 3 {
            if self.bl_tree[BL_ORDER[max_blindex] as usize].dl != 0 {
                break;
            }
            max_blindex -= 1;
        }
        self.huff.opt_len = self
            .huff
            .opt_len
            .wrapping_add(3 * (max_blindex as u64 + 1) + 5 + 5 + 4);
        max_blindex
    }

    /// `_tr_flush_block`, over the window from `block_start` when it is
    /// still there.
    fn flush_block(&mut self, buf: Option<usize>, stored_len: usize, last: bool) {
        self.huff.opt_len = 0;
        self.huff.static_len = 0;
        let l_max = self.huff.build_tree(&mut self.dyn_ltree, &L_DESC);
        let d_max = self.huff.build_tree(&mut self.dyn_dtree, &D_DESC);
        let max_blindex = self.build_bl_tree(l_max, d_max);
        let mut opt_lenb = self.huff.opt_len.wrapping_add(3 + 7) >> 3;
        let static_lenb = self.huff.static_len.wrapping_add(3 + 7) >> 3;
        if static_lenb <= opt_lenb {
            opt_lenb = static_lenb;
        }
        if stored_len as u64 + 4 <= opt_lenb
            && let Some(at) = buf
        {
            self.p.stored_block(&self.window[at..at + stored_len], last);
        } else if static_lenb == opt_lenb {
            self.p.send_bits((1 << 1) + u32::from(last), 3);
            compress_block(
                &mut self.p,
                &self.sym_buf,
                &TABLES.static_ltree,
                &TABLES.static_dtree,
            );
        } else {
            self.p.send_bits((2 << 1) + u32::from(last), 3);
            self.send_all_trees(l_max + 1, d_max + 1, max_blindex + 1);
            compress_block(&mut self.p, &self.sym_buf, &self.dyn_ltree, &self.dyn_dtree);
        }
        self.init_block();
        if last {
            self.p.bi_windup();
        }
    }

    fn send_all_trees(&mut self, lcodes: usize, dcodes: usize, blcodes: usize) {
        let p = &mut self.p;
        p.send_bits(lcodes as u32 - 257, 5);
        p.send_bits(dcodes as u32 - 1, 5);
        p.send_bits(blcodes as u32 - 4, 4);
        for &rank in &BL_ORDER[..blcodes] {
            p.send_bits(u32::from(self.bl_tree[rank as usize].dl), 3);
        }
        send_tree(p, &self.bl_tree, &self.dyn_ltree, lcodes - 1);
        send_tree(p, &self.bl_tree, &self.dyn_dtree, dcodes - 1);
    }

    /// `FLUSH_BLOCK_ONLY`.
    fn flush_block_only(&mut self, last: bool) {
        let buf = usize::try_from(self.block_start).ok();
        let stored_len = (self.strstart as i64 - self.block_start) as usize;
        self.flush_block(buf, stored_len, last);
        self.block_start = self.strstart as i64;
        self.flush_pending();
    }

    /// `FLUSH_BLOCK`: false when the output is full and deflate must return.
    fn flush_block_checked(&mut self, last: bool) -> bool {
        self.flush_block_only(last);
        self.avail_out != 0
    }

    /// `deflate(strm, Z_FINISH)`.
    fn deflate(&mut self) -> Ret {
        if self.avail_out == 0 {
            return Ret::BufError;
        }
        if !self.p.buf.is_empty() {
            self.flush_pending();
            if self.avail_out == 0 {
                return Ret::Ok;
            }
        }
        if self.status == Status::Finish && self.avail_in() != 0 {
            return Ret::BufError;
        }
        if self.status == Status::Init {
            let mut header: u32 = (8 + ((15 - 8) << 4)) << 8;
            let level_flags = match self.level {
                0 | 1 => 0,
                2..=5 => 1,
                6 => 2,
                _ => 3,
            };
            header |= level_flags << 6;
            header += 31 - (header % 31);
            self.p.put_short_msb(header);
            self.adler = 1;
            self.status = Status::Busy;
            self.flush_pending();
            if !self.p.buf.is_empty() {
                return Ret::Ok;
            }
        }
        if self.avail_in() != 0 || self.lookahead != 0 || self.status != Status::Finish {
            let bstate = match self.level {
                0 => self.deflate_stored(),
                1..=3 => self.deflate_fast(),
                _ => self.deflate_slow(),
            };
            if matches!(bstate, Block::FinishStarted | Block::FinishDone) {
                self.status = Status::Finish;
            }
            if matches!(bstate, Block::NeedMore | Block::FinishStarted) {
                return Ret::Ok;
            }
        }
        if self.wrap <= 0 {
            return Ret::StreamEnd;
        }
        self.p.put_short_msb(self.adler >> 16);
        self.p.put_short_msb(self.adler & 0xffff);
        self.flush_pending();
        self.wrap = -self.wrap;
        if self.p.buf.is_empty() {
            Ret::StreamEnd
        } else {
            Ret::Ok
        }
    }

    /// `deflate_stored` with `Z_FINISH`.
    fn deflate_stored(&mut self) -> Block {
        let mut min_block = (PENDING_BUF_SIZE - 5).min(W_SIZE);
        let mut last = false;
        let used = self.avail_in();
        loop {
            let mut len = MAX_STORED;
            let header = (self.p.bi_valid as usize + 42) >> 3;
            if self.avail_out < header {
                break;
            }
            let have = self.avail_out - header;
            let mut left = (self.strstart as i64 - self.block_start) as usize;
            let avail_in = self.avail_in();
            if len > left + avail_in {
                len = left + avail_in;
            }
            if len > have {
                len = have;
            }
            if len < min_block && len != left + avail_in {
                break;
            }
            last = len == left + avail_in;
            self.p.stored_block(&[], last);
            let n = self.p.buf.len();
            self.p.buf[n - 4] = len as u8;
            self.p.buf[n - 3] = (len >> 8) as u8;
            self.p.buf[n - 2] = !len as u8;
            self.p.buf[n - 1] = (!len >> 8) as u8;
            self.flush_pending();
            if left != 0 {
                left = left.min(len);
                let at = self.block_start as usize;
                self.out.extend_from_slice(&self.window[at..at + left]);
                self.avail_out -= left;
                self.block_start += left as i64;
                len -= left;
            }
            if len != 0 {
                self.read_to_out(len);
            }
            if last {
                break;
            }
        }

        let used = used - self.avail_in();
        if used != 0 {
            if used >= W_SIZE {
                let from = self.next_in - W_SIZE;
                self.window[..W_SIZE].copy_from_slice(&self.input[from..self.next_in]);
                self.strstart = W_SIZE;
                self.insert = self.strstart;
            } else {
                if WINDOW_SIZE - self.strstart <= used {
                    self.strstart -= W_SIZE;
                    self.window.copy_within(W_SIZE..W_SIZE + self.strstart, 0);
                    if self.insert > self.strstart {
                        self.insert = self.strstart;
                    }
                }
                let from = self.next_in - used;
                self.window[self.strstart..self.strstart + used]
                    .copy_from_slice(&self.input[from..self.next_in]);
                self.strstart += used;
                self.insert += used.min(W_SIZE - self.insert);
            }
            self.block_start = self.strstart as i64;
        }
        if self.high_water < self.strstart {
            self.high_water = self.strstart;
        }
        if last {
            return Block::FinishDone;
        }

        let mut have = WINDOW_SIZE - self.strstart;
        if self.avail_in() > have && self.block_start >= W_SIZE as i64 {
            self.block_start -= W_SIZE as i64;
            self.strstart -= W_SIZE;
            self.window.copy_within(W_SIZE..W_SIZE + self.strstart, 0);
            have += W_SIZE;
            if self.insert > self.strstart {
                self.insert = self.strstart;
            }
        }
        have = have.min(self.avail_in());
        if have != 0 {
            self.read_to_window(self.strstart, have);
            self.strstart += have;
            self.insert += have.min(W_SIZE - self.insert);
        }
        if self.high_water < self.strstart {
            self.high_water = self.strstart;
        }

        let header = (self.p.bi_valid as usize + 42) >> 3;
        let have = (PENDING_BUF_SIZE - header).min(MAX_STORED);
        min_block = have.min(W_SIZE);
        let left = (self.strstart as i64 - self.block_start) as usize;
        if left >= min_block || (self.avail_in() == 0 && left <= have) {
            let len = left.min(have);
            last = self.avail_in() == 0 && len == left;
            let at = self.block_start as usize;
            self.p.stored_block(&self.window[at..at + len], last);
            self.block_start += len as i64;
            self.flush_pending();
        }
        if last {
            Block::FinishStarted
        } else {
            Block::NeedMore
        }
    }

    /// `deflate_fast` with `Z_FINISH`: levels 1 to 3, no lazy matching.
    fn deflate_fast(&mut self) -> Block {
        loop {
            if self.lookahead < MIN_LOOKAHEAD {
                self.fill_window();
                if self.lookahead == 0 {
                    break;
                }
            }
            let mut hash_head = 0;
            if self.lookahead >= MIN_MATCH {
                hash_head = self.insert_string(self.strstart);
            }
            if hash_head != 0 && self.strstart.wrapping_sub(hash_head) <= MAX_DIST {
                self.match_length = self.longest_match(hash_head);
            }
            let bflush;
            if self.match_length >= MIN_MATCH {
                bflush = self.tally_dist(
                    self.strstart.wrapping_sub(self.match_start),
                    self.match_length - MIN_MATCH,
                );
                self.lookahead -= self.match_length;
                // max_insert_length is max_lazy_match.
                if self.match_length <= self.max_lazy_match && self.lookahead >= MIN_MATCH {
                    self.match_length -= 1;
                    loop {
                        self.strstart += 1;
                        self.insert_string(self.strstart);
                        self.match_length -= 1;
                        if self.match_length == 0 {
                            break;
                        }
                    }
                    self.strstart += 1;
                } else {
                    self.strstart += self.match_length;
                    self.match_length = 0;
                    self.ins_h = usize::from(self.window[self.strstart]);
                    self.update_hash(self.window[self.strstart + 1]);
                }
            } else {
                bflush = self.tally_lit(self.window[self.strstart]);
                self.lookahead -= 1;
                self.strstart += 1;
            }
            if bflush && !self.flush_block_checked(false) {
                return Block::NeedMore;
            }
        }
        self.insert = self.strstart.min(MIN_MATCH - 1);
        if !self.flush_block_checked(true) {
            return Block::FinishStarted;
        }
        Block::FinishDone
    }

    /// `deflate_slow` with `Z_FINISH`: levels 4 to 9, lazy matching.
    fn deflate_slow(&mut self) -> Block {
        loop {
            if self.lookahead < MIN_LOOKAHEAD {
                self.fill_window();
                if self.lookahead == 0 {
                    break;
                }
            }
            let mut hash_head = 0;
            if self.lookahead >= MIN_MATCH {
                hash_head = self.insert_string(self.strstart);
            }
            self.prev_length = self.match_length;
            self.prev_match = self.match_start;
            self.match_length = MIN_MATCH - 1;
            if hash_head != 0
                && self.prev_length < self.max_lazy_match
                && self.strstart.wrapping_sub(hash_head) <= MAX_DIST
            {
                self.match_length = self.longest_match(hash_head);
                if self.match_length <= 5
                    && self.match_length == MIN_MATCH
                    && self.strstart.wrapping_sub(self.match_start) > TOO_FAR
                {
                    self.match_length = MIN_MATCH - 1;
                }
            }
            if self.prev_length >= MIN_MATCH && self.match_length <= self.prev_length {
                let max_insert = self.strstart + self.lookahead - MIN_MATCH;
                let bflush = self.tally_dist(
                    self.strstart.wrapping_sub(1).wrapping_sub(self.prev_match),
                    self.prev_length - MIN_MATCH,
                );
                self.lookahead -= self.prev_length - 1;
                self.prev_length -= 2;
                loop {
                    self.strstart += 1;
                    if self.strstart <= max_insert {
                        self.insert_string(self.strstart);
                    }
                    self.prev_length -= 1;
                    if self.prev_length == 0 {
                        break;
                    }
                }
                self.match_available = false;
                self.match_length = MIN_MATCH - 1;
                self.strstart += 1;
                if bflush && !self.flush_block_checked(false) {
                    return Block::NeedMore;
                }
            } else if self.match_available {
                let bflush = self.tally_lit(self.window[self.strstart - 1]);
                if bflush {
                    self.flush_block_only(false);
                }
                self.strstart += 1;
                self.lookahead -= 1;
                if self.avail_out == 0 {
                    return Block::NeedMore;
                }
            } else {
                self.match_available = true;
                self.strstart += 1;
                self.lookahead -= 1;
            }
        }
        if self.match_available {
            self.tally_lit(self.window[self.strstart - 1]);
            self.match_available = false;
        }
        self.insert = self.strstart.min(MIN_MATCH - 1);
        if !self.flush_block_checked(true) {
            return Block::FinishStarted;
        }
        Block::FinishDone
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn inflate(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(data)
            .read_to_end(&mut out)
            .unwrap_or_default();
        out
    }

    /// Text-like data with long and short repeats, from a fixed generator.
    fn sample(n: usize, seed: u32) -> Vec<u8> {
        let words: [&[u8]; 8] = [
            b"BT ",
            b"/F1 12 Tf ",
            b"0 0 1 rg ",
            b"(Hello) Tj ",
            b"ET\n",
            b"q ",
            b"Q\n",
            b"72 712 Td ",
        ];
        let mut x = seed;
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let r = (x >> 16) as usize;
            if r.is_multiple_of(7) {
                out.push((r >> 3) as u8);
            } else {
                out.extend_from_slice(words[r % 8]);
            }
        }
        out.truncate(n);
        out
    }

    fn crc(data: &[u8]) -> u32 {
        let mut c = flate2::Crc::new();
        c.update(data);
        c.sum()
    }

    #[test]
    fn output_inflates_back_at_every_level() {
        for n in [0, 1, 2, 3, 100, 5_000, 70_000, 200_000] {
            let data = sample(n, 7);
            for level in -1..=9 {
                let out = compress(&data, level).unwrap_or_default();
                assert!(out.len() <= compress_bound(n), "n {n} level {level}");
                assert_eq!(inflate(&out), data, "n {n} level {level}");
            }
        }
        assert_eq!(compress(b"x", 10), None);
    }

    /// Lengths and CRC-32s of MuPDF's zlib 1.3.2 `compress2` output for the
    /// same inputs.
    #[test]
    fn output_matches_zlib() {
        let cases: &[(usize, u32, i32, usize, u32)] = &[
            (0, 1, -1, 8, 0xc36c_0b3c),
            (0, 1, 0, 11, 0x8e78_21f1),
            (0, 1, 1, 8, 0x4d73_1cc0),
            (0, 1, 3, 8, 0x98ee_859e),
            (0, 1, 4, 8, 0x98ee_859e),
            (0, 1, 9, 8, 0xe4b7_4b7f),
            (1, 1, -1, 9, 0xa696_0837),
            (1, 1, 0, 12, 0x5f0f_57ed),
            (1, 1, 1, 9, 0x1213_a917),
            (1, 1, 3, 9, 0x9e15_487d),
            (1, 1, 4, 9, 0x9e15_487d),
            (1, 1, 9, 9, 0x4964_c35d),
            (5000, 3, -1, 1017, 0x972a_31db),
            (5000, 3, 0, 5011, 0x043f_f313),
            (5000, 3, 1, 1183, 0xe046_a7de),
            (5000, 3, 3, 1087, 0xca79_4c86),
            (5000, 3, 4, 1069, 0x6eac_fb65),
            (5000, 3, 9, 1015, 0x8961_8642),
            (70_000, 7, -1, 11_823, 0x2411_9dc1),
            (70_000, 7, 0, 70_016, 0xce82_9df7),
            (70_000, 7, 1, 14_985, 0x32ad_1ee6),
            (70_000, 7, 3, 13_125, 0x67ef_1a81),
            (70_000, 7, 4, 12_920, 0x9387_5819),
            (70_000, 7, 9, 11_347, 0x8dcb_dc89),
            (300_000, 11, -1, 49_534, 0xf039_6769),
            (300_000, 11, 0, 300_031, 0x3d70_4e40),
            (300_000, 11, 1, 63_859, 0x30d9_4b66),
            (300_000, 11, 3, 55_201, 0x798e_b559),
            (300_000, 11, 4, 54_443, 0xf56f_8fbe),
            (300_000, 11, 9, 46_950, 0xa3eb_bfed),
        ];
        for &(n, seed, level, len, sum) in cases {
            let out = compress(&sample(n, seed), level).unwrap_or_default();
            assert_eq!(
                (out.len(), crc(&out)),
                (len, sum),
                "n {n} seed {seed} level {level}"
            );
        }
    }

    fn pseudo_random(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (x >> 16) as u8
            })
            .collect()
    }

    /// Inputs that steer zlib down its other paths: maximal matches,
    /// incompressible data (stored blocks), far and near repeats, a mix
    /// that changes block types, and input too short for dynamic trees.
    fn inputs() -> Vec<(&'static str, Vec<u8>)> {
        let random = pseudo_random(50_000, 9);
        let mut far = pseudo_random(20_000, 4);
        far.extend_from_within(..15_000);
        far.extend(pseudo_random(40_000, 5));
        far.extend_from_within(..5_000);
        let mut near = Vec::new();
        for i in 0..4000u32 {
            near.extend_from_slice(&i.to_le_bytes()[..3]);
            near.extend_from_slice(b"abc");
        }
        let mut mixed = sample(60_000, 5);
        mixed.extend(pseudo_random(30_000, 6));
        mixed.extend(vec![b'z'; 40_000]);
        mixed.extend(sample(30_000, 8));
        vec![
            ("sample", sample(5000, 3)),
            ("text", sample(70_000, 7)),
            ("runs", vec![b'a'; 100_000]),
            ("random", random),
            ("far", far),
            ("near", near),
            ("mixed", mixed),
            ("short", b"BT /F1 12 Tf (Hi) Tj ET".to_vec()),
        ]
    }

    /// MuPDF 1.28.6's deflate of each input, at each level: the length
    /// and CRC-32, from tests/data/deflate/zlib_oracle.py.
    #[rustfmt::skip]
    const MUPDF_DEFLATE: &[(&str, i32, usize, u32)] = &[
        ("sample", -1, 1017, 0x972a31db),
        ("sample", 0, 5011, 0x043ff313),
        ("sample", 1, 1183, 0xe046a7de),
        ("sample", 2, 1137, 0xc8168608),
        ("sample", 3, 1087, 0xca794c86),
        ("sample", 4, 1069, 0x6eacfb65),
        ("sample", 5, 1043, 0x36a29b08),
        ("sample", 6, 1017, 0x972a31db),
        ("sample", 7, 1015, 0x89618642),
        ("sample", 8, 1015, 0x89618642),
        ("sample", 9, 1015, 0x89618642),
        ("text", -1, 11823, 0x24119dc1),
        ("text", 0, 70016, 0xce829df7),
        ("text", 1, 14985, 0x32ad1ee6),
        ("text", 2, 14061, 0xcc9b4625),
        ("text", 3, 13125, 0x67ef1a81),
        ("text", 4, 12920, 0x93875819),
        ("text", 5, 12425, 0x981a179e),
        ("text", 6, 11823, 0x24119dc1),
        ("text", 7, 11603, 0x9e73e246),
        ("text", 8, 11347, 0x8dcbdc89),
        ("text", 9, 11347, 0x8dcbdc89),
        ("runs", -1, 121, 0x1993b6b2),
        ("runs", 0, 100016, 0x589f299a),
        ("runs", 1, 461, 0x4467d0ce),
        ("runs", 2, 461, 0x5b108768),
        ("runs", 3, 461, 0x5b108768),
        ("runs", 4, 121, 0x22a650e5),
        ("runs", 5, 121, 0x22a650e5),
        ("runs", 6, 121, 0x1993b6b2),
        ("runs", 7, 121, 0x3e4fca05),
        ("runs", 8, 121, 0x3e4fca05),
        ("runs", 9, 121, 0x3e4fca05),
        ("random", -1, 50026, 0x75742cd9),
        ("random", 0, 50011, 0x06152910),
        ("random", 1, 50026, 0xf6fbc331),
        ("random", 2, 50026, 0x24c9cb4f),
        ("random", 3, 50026, 0x24c9cb4f),
        ("random", 4, 50026, 0x80e9592d),
        ("random", 5, 50026, 0x80e9592d),
        ("random", 6, 50026, 0x75742cd9),
        ("random", 7, 50026, 0xc400fab7),
        ("random", 8, 50026, 0xc400fab7),
        ("random", 9, 50026, 0xc400fab7),
        ("far", -1, 65239, 0xb42ff722),
        ("far", 0, 80016, 0x9052104f),
        ("far", 1, 65246, 0x10e9e84a),
        ("far", 2, 65246, 0xc3fdd987),
        ("far", 3, 65246, 0xc3fdd987),
        ("far", 4, 65239, 0x9f98a427),
        ("far", 5, 65239, 0x9f98a427),
        ("far", 6, 65239, 0xb42ff722),
        ("far", 7, 65239, 0xb1b3cc6b),
        ("far", 8, 65239, 0xb1b3cc6b),
        ("far", 9, 65239, 0xb1b3cc6b),
        ("near", -1, 6081, 0x4fbb89a6),
        ("near", 0, 24011, 0x95e966b8),
        ("near", 1, 6067, 0x953a8b45),
        ("near", 2, 6132, 0x8199c6ec),
        ("near", 3, 6132, 0x8199c6ec),
        ("near", 4, 6081, 0xd90a73c8),
        ("near", 5, 6081, 0xd90a73c8),
        ("near", 6, 6081, 0x4fbb89a6),
        ("near", 7, 6061, 0x693e8b04),
        ("near", 8, 6059, 0x52b9a245),
        ("near", 9, 6059, 0x52b9a245),
        ("mixed", -1, 46858, 0xbd746a98),
        ("mixed", 0, 160021, 0xaf38be34),
        ("mixed", 1, 51766, 0xddf736e9),
        ("mixed", 2, 50189, 0x85be67ba),
        ("mixed", 3, 48734, 0xb299e173),
        ("mixed", 4, 48509, 0xe5234319),
        ("mixed", 5, 47721, 0x7ddb225c),
        ("mixed", 6, 46858, 0xbd746a98),
        ("mixed", 7, 46552, 0x3557e7ce),
        ("mixed", 8, 46260, 0x54ea5008),
        ("mixed", 9, 46258, 0x29515eeb),
        ("short", -1, 31, 0x7a222c0f),
        ("short", 0, 34, 0x49872a15),
        ("short", 1, 31, 0xced88db6),
        ("short", 2, 31, 0x2ba1f0c4),
        ("short", 3, 31, 0x2ba1f0c4),
        ("short", 4, 31, 0x2ba1f0c4),
        ("short", 5, 31, 0x2ba1f0c4),
        ("short", 6, 31, 0x7a222c0f),
        ("short", 7, 31, 0xeba19076),
        ("short", 8, 31, 0xeba19076),
        ("short", 9, 31, 0xeba19076),
    ];

    #[test]
    fn every_level_deflates_as_mupdf_saves() {
        let dump = std::env::var_os("NUPDF_DUMP_DEFLATE_CASES");
        let mut wrong = Vec::new();
        for (name, data) in inputs() {
            if let Some(dir) = &dump {
                let path = std::path::Path::new(dir).join(format!("{name}.bin"));
                std::fs::write(path, &data).unwrap_or_else(|e| panic!("{e}"));
            }
            for &(_, level, len, sum) in MUPDF_DEFLATE.iter().filter(|c| c.0 == name) {
                let out = compress(&data, level).unwrap_or_default();
                if (out.len(), crc(&out)) != (len, sum) {
                    wrong.push(format!(
                        "{name} level {level}: {} {:08x}",
                        out.len(),
                        crc(&out)
                    ));
                }
                assert_eq!(inflate(&out), data, "{name} level {level}");
            }
        }
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    #[test]
    fn the_static_tables_are_zlibs() {
        // Evaluated at run time too: the tables `TABLES` holds, by the
        // values zlib's trees.h lists.
        let t = tables();
        let ct = |c: Ct| (c.fc, c.dl);
        assert_eq!(ct(t.static_ltree[0]), (12, 8));
        assert_eq!(ct(t.static_ltree[143]), (253, 8));
        assert_eq!(ct(t.static_ltree[144]), (19, 9));
        assert_eq!(ct(t.static_ltree[255]), (511, 9));
        assert_eq!(ct(t.static_ltree[256]), (0, 7));
        assert_eq!(ct(t.static_ltree[280]), (3, 8));
        assert_eq!(ct(t.static_ltree[287]), (227, 8));
        assert_eq!(ct(t.static_dtree[1]), (16, 5));
        assert_eq!(ct(t.static_dtree[29]), (23, 5));
        assert_eq!(&t.dist_code[..8], [0, 1, 2, 3, 4, 4, 5, 5]);
        assert_eq!(&t.dist_code[256..264], [0, 0, 16, 17, 18, 18, 19, 19]);
        assert_eq!(t.dist_code[511], 29);
        assert_eq!(
            (
                t.length_code[0],
                t.length_code[8],
                t.length_code[254],
                t.length_code[255]
            ),
            (0, 8, 27, 28)
        );
        assert_eq!((t.base_length[8], t.base_length[28]), (8, 0));
        assert_eq!((t.base_dist[4], t.base_dist[29]), (4, 24576));
    }

    #[test]
    fn adler32_matches_zlib() {
        assert_eq!(adler32(1, b""), 1);
        assert_eq!(adler32(1, b"Wikipedia"), 0x11e6_0398);
    }
}
