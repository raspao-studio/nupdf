// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/fitz/regexp.c (from MuJS),
// Copyright (C) 2014-2026 Artifex Software, Inc.

//! MuPDF's regular expressions: JavaScript's syntax, compiled to a small
//! program run by backtracking over UTF-8, with MuPDF's limits.

use crate::stext::chartorune;
use crate::unicode;

pub const ICASE: u32 = 1;
pub const NEWLINE: u32 = 2;
pub const NOTBOL: u32 = 4;
pub const RUNAWAY: u32 = 8;

const REPINF: u32 = 255;
const MAXPROG: usize = 32 << 10;
const MAXREC: usize = 4096;
const MAXSPAN: usize = 64;
const MAXCLASS: usize = 128;
const MAXRUN: u32 = 1 << 20;
const MAXSUB: usize = 16;
const EOF: i32 = -1;

/// The runes whose upper case (`fz_toupper`) is `c`, other than `c`.
fn upper_sources(c: i32) -> Vec<i32> {
    static INVERSE: std::sync::OnceLock<Vec<(i32, i32)>> = std::sync::OnceLock::new();
    let inverse = INVERSE.get_or_init(|| {
        let mut v = Vec::new();
        for t in crate::tables::UCD_TOUPPER2.as_chunks::<3>().0 {
            for r in t[0]..=t[1] {
                v.push((r + t[2], r));
            }
        }
        for t in crate::tables::UCD_TOUPPER1.as_chunks::<2>().0 {
            v.push((t[0] + t[1], t[0]));
        }
        v.retain(|&(u, r)| u != r && unicode::to_upper(r) == u);
        v.sort_unstable();
        v
    });
    let from = inverse.partition_point(|&(u, _)| u < c);
    inverse[from..]
        .iter()
        .take_while(|&&(u, _)| u == c)
        .map(|&(_, r)| r)
        .collect()
}

/// `canon`: upper case, unless that would make a non-ASCII letter ASCII.
fn canon(c: i32) -> i32 {
    let u = unicode::to_upper(c);
    if c >= 128 && u < 128 { c } else { u }
}

// Tokens past single characters.
const L_CHAR: i32 = 256;
const L_CCLASS: i32 = 257;
const L_NCCLASS: i32 = 258;
const L_NC: i32 = 259;
const L_PLA: i32 = 260;
const L_NLA: i32 = 261;
const L_WORD: i32 = 262;
const L_NWORD: i32 = 263;
const L_REF: i32 = 264;
const L_COUNT: i32 = 265;

const ESCAPES: &[u8] = b"BbDdSsWw^$\\.*+?()[]{}|-0123456789";

/// `strchr(set, c)`: C truncates the rune to a byte, and finds the NUL.
fn in_set(set: &[u8], c: i32) -> bool {
    let b = c as u8;
    b == 0 || set.contains(&b)
}

fn is_unicode_letter(c: i32) -> bool {
    (b'a' as i32..=b'z' as i32).contains(&c)
        || (b'A' as i32..=b'Z' as i32).contains(&c)
        || unicode::is_alpha(c)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Cat,
    Alt,
    Rep,
    Bol,
    Eol,
    Word,
    NWord,
    Par,
    Pla,
    Nla,
    Any,
    Char,
    Cclass,
    Ncclass,
    Ref,
}

#[derive(Debug, Clone)]
struct Node {
    kind: NodeKind,
    ng: bool,
    m: u32,
    n: u32,
    c: i32,
    cc: usize,
    x: Option<usize>,
    y: Option<usize>,
}

struct Compiler<'a> {
    source: &'a [u8],
    at: usize,
    nodes: Vec<Node>,
    classes: Vec<Vec<(i32, i32)>>,
    nsub: usize,
    sub: [Option<usize>; MAXSUB],
    lookahead: i32,
    yychar: i32,
    yycc: usize,
    yymin: u32,
    yymax: u32,
}

type Fail = String;

fn die<T>(message: &str) -> Result<T, Fail> {
    Err(format!("regcomp failure: {message}"))
}

impl Compiler<'_> {
    fn byte(&self, i: usize) -> u8 {
        self.source.get(i).copied().unwrap_or(0)
    }

    fn take(&mut self) -> u8 {
        let b = self.byte(self.at);
        self.at += 1;
        b
    }

    fn rune(&mut self) -> i32 {
        let (c, len) = chartorune(self.source.get(self.at..).unwrap_or_default());
        self.at += len;
        c
    }

    fn hex(c: u8) -> Result<i32, Fail> {
        match c {
            b'0'..=b'9' => Ok(i32::from(c - b'0')),
            b'a'..=b'f' => Ok(i32::from(c - b'a') + 10),
            b'A'..=b'F' => Ok(i32::from(c - b'A') + 10),
            _ => die("invalid escape sequence"),
        }
    }

    fn dec(c: u8) -> Result<u32, Fail> {
        if c.is_ascii_digit() {
            Ok(u32::from(c - b'0'))
        } else {
            die("invalid quantifier")
        }
    }

    /// `nextrune`: whether the rune read was escaped.
    fn next_rune(&mut self) -> Result<bool, Fail> {
        if self.byte(self.at) == 0 {
            self.yychar = EOF;
            return Ok(false);
        }
        self.yychar = self.rune();
        if self.yychar != '\\' as i32 {
            return Ok(false);
        }
        if self.byte(self.at) == 0 {
            return die("unterminated escape sequence");
        }
        self.yychar = self.rune();
        match u8::try_from(self.yychar).unwrap_or(0xff) {
            b'f' => self.yychar = 0x0c,
            b'n' => self.yychar = '\n' as i32,
            b'r' => self.yychar = '\r' as i32,
            b't' => self.yychar = '\t' as i32,
            b'v' => self.yychar = 0x0b,
            b'c' => {
                if self.byte(self.at) == 0 {
                    return die("unterminated escape sequence");
                }
                self.yychar = i32::from(self.take() & 31);
            }
            b'x' => {
                if self.byte(self.at) == 0 || self.byte(self.at + 1) == 0 {
                    return die("unterminated escape sequence");
                }
                let hi = Self::hex(self.take())?;
                let lo = Self::hex(self.take())?;
                self.yychar = (hi << 4) + lo;
                if self.yychar == 0 {
                    self.yychar = '0' as i32;
                }
                return Ok(true);
            }
            b'u' => {
                if (0..4).any(|i| self.byte(self.at + i) == 0) {
                    return die("unterminated escape sequence");
                }
                let mut v = 0;
                for _ in 0..4 {
                    v = (v << 4) + Self::hex(self.take())?;
                }
                self.yychar = if v == 0 { '0' as i32 } else { v };
                return Ok(true);
            }
            0 if self.yychar == 0 => {
                self.yychar = '0' as i32;
                return Ok(true);
            }
            _ => {
                if in_set(ESCAPES, self.yychar) {
                    return Ok(true);
                }
                if is_unicode_letter(self.yychar) || self.yychar == '_' as i32 {
                    return die("invalid escape character");
                }
            }
        }
        Ok(false)
    }

    /// `lexcount`.
    fn lex_count(&mut self) -> Result<i32, Fail> {
        let mut c = self.take();
        self.yymin = Self::dec(c)?;
        c = self.take();
        while c != b',' && c != b'}' {
            self.yymin = self.yymin * 10 + Self::dec(c)?;
            c = self.take();
            if self.yymin >= REPINF {
                return die("numeric overflow");
            }
        }
        if c == b',' {
            c = self.take();
            if c == b'}' {
                self.yymax = REPINF;
            } else {
                self.yymax = Self::dec(c)?;
                c = self.take();
                while c != b'}' {
                    self.yymax = self.yymax * 10 + Self::dec(c)?;
                    c = self.take();
                    if self.yymax >= REPINF {
                        return die("numeric overflow");
                    }
                }
            }
        } else {
            self.yymax = self.yymin;
        }
        Ok(L_COUNT)
    }

    fn new_class(&mut self) -> Result<(), Fail> {
        if self.classes.len() >= MAXCLASS {
            return die("too many character classes");
        }
        self.classes.push(Vec::new());
        self.yycc = self.classes.len() - 1;
        Ok(())
    }

    /// `addrange`.
    fn add_range(&mut self, a: i32, b: i32) -> Result<(), Fail> {
        if a > b {
            return die("invalid character class range");
        }
        let cc = &mut self.classes[self.yycc];
        for p in cc.iter_mut() {
            if a >= p.0 && b <= p.1 {
                return Ok(());
            }
            if a < p.0 && b >= p.1 {
                *p = (a, b);
                return Ok(());
            }
            if b >= p.0 - 1 && b <= p.1 && a < p.0 {
                p.0 = a;
                return Ok(());
            }
            if a >= p.0 && a <= p.1 + 1 && b > p.1 {
                p.1 = b;
                return Ok(());
            }
        }
        if cc.len() * 2 + 2 >= MAXSPAN {
            return die("too many character class ranges");
        }
        cc.push((a, b));
        Ok(())
    }

    fn add_class(&mut self, which: u8) -> Result<(), Fail> {
        let ranges: &[(i32, i32)] = match which {
            b'd' => &[(0x30, 0x39)],
            b'D' => &[(0, 0x2f), (0x3a, 0xffff)],
            b's' => &[
                (0x9, 0xd),
                (0x20, 0x20),
                (0xa0, 0xa0),
                (0x2028, 0x2029),
                (0xfeff, 0xfeff),
            ],
            b'S' => &[
                (0, 0x8),
                (0xe, 0x1f),
                (0x21, 0x9f),
                (0xa1, 0x2027),
                (0x202a, 0xfefe),
                (0xff00, 0xffff),
            ],
            b'w' => &[(0x30, 0x39), (0x41, 0x5a), (0x5f, 0x5f), (0x61, 0x7a)],
            b'W' => &[
                (0, 0x2f),
                (0x3a, 0x40),
                (0x5b, 0x5e),
                (0x60, 0x60),
                (0x7b, 0xffff),
            ],
            _ => &[],
        };
        for &(a, b) in ranges {
            self.add_range(a, b)?;
        }
        Ok(())
    }

    /// `lexclass`.
    fn lex_class(&mut self) -> Result<i32, Fail> {
        let mut kind = L_CCLASS;
        self.new_class()?;
        let mut quoted = self.next_rune()?;
        if !quoted && self.yychar == '^' as i32 {
            kind = L_NCCLASS;
            quoted = self.next_rune()?;
        }
        let (mut save, mut havesave, mut havedash) = (0i32, false, false);
        loop {
            if self.yychar == EOF {
                return die("unterminated character class");
            }
            if !quoted && self.yychar == ']' as i32 {
                break;
            }
            if !quoted && self.yychar == '-' as i32 {
                if havesave {
                    if havedash {
                        self.add_range(save, '-' as i32)?;
                        havesave = false;
                        havedash = false;
                    } else {
                        havedash = true;
                    }
                } else {
                    save = '-' as i32;
                    havesave = true;
                }
            } else if quoted && in_set(b"DSWdsw", self.yychar) {
                if havesave {
                    self.add_range(save, save)?;
                    if havedash {
                        self.add_range('-' as i32, '-' as i32)?;
                    }
                }
                self.add_class(self.yychar as u8)?;
                havesave = false;
                havedash = false;
            } else {
                if quoted {
                    if self.yychar == 'b' as i32 {
                        self.yychar = 8;
                    } else if self.yychar == '0' as i32 {
                        self.yychar = 0;
                    }
                }
                if havesave {
                    if havedash {
                        self.add_range(save, self.yychar)?;
                        havesave = false;
                        havedash = false;
                    } else {
                        self.add_range(save, save)?;
                        save = self.yychar;
                    }
                } else {
                    save = self.yychar;
                    havesave = true;
                }
            }
            quoted = self.next_rune()?;
        }
        if havesave {
            self.add_range(save, save)?;
            if havedash {
                self.add_range('-' as i32, '-' as i32)?;
            }
        }
        Ok(kind)
    }

    /// `lex`.
    fn lex(&mut self) -> Result<i32, Fail> {
        let quoted = self.next_rune()?;
        if quoted {
            let c = self.yychar;
            let class = |me: &mut Self, w: u8, t: i32| -> Result<i32, Fail> {
                me.new_class()?;
                me.add_class(w)?;
                Ok(t)
            };
            match u8::try_from(c).unwrap_or(0xff) {
                b'b' if c == 'b' as i32 => return Ok(L_WORD),
                b'B' if c == 'B' as i32 => return Ok(L_NWORD),
                b'd' if c == 'd' as i32 => return class(self, b'd', L_CCLASS),
                b's' if c == 's' as i32 => return class(self, b's', L_CCLASS),
                b'w' if c == 'w' as i32 => return class(self, b'w', L_CCLASS),
                b'D' if c == 'D' as i32 => return class(self, b'd', L_NCCLASS),
                b'S' if c == 'S' as i32 => return class(self, b's', L_NCCLASS),
                b'W' if c == 'W' as i32 => return class(self, b'w', L_NCCLASS),
                b'0' if c == '0' as i32 => {
                    self.yychar = 0;
                    return Ok(L_CHAR);
                }
                _ => {}
            }
            if (b'0' as i32..=b'9' as i32).contains(&c) {
                self.yychar = c - '0' as i32;
                let next = self.byte(self.at);
                if next.is_ascii_digit() {
                    self.yychar = self.yychar * 10 + i32::from(next - b'0');
                    self.at += 1;
                }
                return Ok(L_REF);
            }
            return Ok(L_CHAR);
        }
        let c = self.yychar;
        if c == EOF || b"$)*+.?^|".iter().any(|&b| c == i32::from(b)) {
            return Ok(c);
        }
        if c == '{' as i32 {
            return self.lex_count();
        }
        if c == '[' as i32 {
            return self.lex_class();
        }
        if c == '(' as i32 {
            if self.byte(self.at) == b'?' {
                let t = match self.byte(self.at + 1) {
                    b':' => Some(L_NC),
                    b'=' => Some(L_PLA),
                    b'!' => Some(L_NLA),
                    _ => None,
                };
                if let Some(t) = t {
                    self.at += 2;
                    return Ok(t);
                }
            }
            return Ok('(' as i32);
        }
        Ok(L_CHAR)
    }

    fn next(&mut self) -> Result<(), Fail> {
        self.lookahead = self.lex()?;
        Ok(())
    }

    fn accept(&mut self, t: i32) -> Result<bool, Fail> {
        if self.lookahead == t {
            self.next()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn node(&mut self, kind: NodeKind) -> usize {
        self.nodes.push(Node {
            kind,
            ng: false,
            m: 0,
            n: 0,
            c: 0,
            cc: 0,
            x: None,
            y: None,
        });
        self.nodes.len() - 1
    }

    /// `empty`.
    fn empty(&self, node: Option<usize>) -> bool {
        let Some(i) = node else { return true };
        let n = &self.nodes[i];
        match n.kind {
            NodeKind::Cat => self.empty(n.x) && self.empty(n.y),
            NodeKind::Alt => self.empty(n.x) || self.empty(n.y),
            NodeKind::Rep => self.empty(n.x) || n.m == 0,
            NodeKind::Par | NodeKind::Ref => self.empty(n.x),
            NodeKind::Any | NodeKind::Char | NodeKind::Cclass | NodeKind::Ncclass => false,
            _ => true,
        }
    }

    fn rep(&mut self, atom: Option<usize>, ng: bool, min: u32, max: u32) -> Result<usize, Fail> {
        if max == REPINF && self.empty(atom) {
            return die("infinite loop matching the empty string");
        }
        let r = self.node(NodeKind::Rep);
        let n = &mut self.nodes[r];
        n.ng = ng;
        n.m = min;
        n.n = max;
        n.x = atom;
        Ok(r)
    }

    /// `parseatom`: None for a group of nothing.
    fn atom(&mut self) -> Result<Option<usize>, Fail> {
        match self.lookahead {
            L_CHAR => {
                let a = self.node(NodeKind::Char);
                self.nodes[a].c = self.yychar;
                self.next()?;
                return Ok(Some(a));
            }
            L_CCLASS | L_NCCLASS => {
                let kind = if self.lookahead == L_CCLASS {
                    NodeKind::Cclass
                } else {
                    NodeKind::Ncclass
                };
                let a = self.node(kind);
                self.nodes[a].cc = self.yycc;
                self.next()?;
                return Ok(Some(a));
            }
            L_REF => {
                let r = usize::try_from(self.yychar).unwrap_or(0);
                if r == 0 || r >= self.nsub || self.sub[r].is_none() {
                    return die("invalid back-reference");
                }
                let a = self.node(NodeKind::Ref);
                self.nodes[a].n = r as u32;
                self.nodes[a].x = self.sub[r];
                self.next()?;
                return Ok(Some(a));
            }
            _ => {}
        }
        if self.accept('.' as i32)? {
            return Ok(Some(self.node(NodeKind::Any)));
        }
        if self.accept('(' as i32)? {
            let a = self.node(NodeKind::Par);
            if self.nsub == MAXSUB {
                return die("too many captures");
            }
            self.nodes[a].n = self.nsub as u32;
            self.nsub += 1;
            let x = self.alt()?;
            self.nodes[a].x = x;
            let n = self.nodes[a].n as usize;
            self.sub[n] = Some(a);
            if !self.accept(')' as i32)? {
                return die("unmatched '('");
            }
            return Ok(Some(a));
        }
        if self.accept(L_NC)? {
            let a = self.alt()?;
            if !self.accept(')' as i32)? {
                return die("unmatched '('");
            }
            return Ok(a);
        }
        for (t, kind) in [(L_PLA, NodeKind::Pla), (L_NLA, NodeKind::Nla)] {
            if self.accept(t)? {
                let a = self.node(kind);
                let x = self.alt()?;
                self.nodes[a].x = x;
                if !self.accept(')' as i32)? {
                    return die("unmatched '('");
                }
                return Ok(Some(a));
            }
        }
        die("syntax error")
    }

    fn repeat(&mut self) -> Result<Option<usize>, Fail> {
        for (t, kind) in [
            ('^' as i32, NodeKind::Bol),
            ('$' as i32, NodeKind::Eol),
            (L_WORD, NodeKind::Word),
            (L_NWORD, NodeKind::NWord),
        ] {
            if self.accept(t)? {
                return Ok(Some(self.node(kind)));
            }
        }
        let atom = self.atom()?;
        if self.lookahead == L_COUNT {
            let (min, max) = (self.yymin, self.yymax);
            self.next()?;
            if max < min {
                return die("invalid quantifier");
            }
            let ng = self.accept('?' as i32)?;
            return self.rep(atom, ng, min, max).map(Some);
        }
        for (t, min, max) in [('*', 0, REPINF), ('+', 1, REPINF), ('?', 0, 1)] {
            if self.accept(t as i32)? {
                let ng = self.accept('?' as i32)?;
                return self.rep(atom, ng, min, max).map(Some);
            }
        }
        Ok(atom)
    }

    fn cat(&mut self) -> Result<Option<usize>, Fail> {
        let stop = |t: i32| t == EOF || t == '|' as i32 || t == ')' as i32;
        if stop(self.lookahead) {
            return Ok(None);
        }
        let head = self.repeat()?;
        // A right-leaning tree, new cats spliced in at the tail.
        let mut root = head;
        let mut tail: Option<usize> = None;
        while !stop(self.lookahead) {
            let c = self.node(NodeKind::Cat);
            let right = self.repeat()?;
            match tail {
                None => {
                    self.nodes[c].x = root;
                    root = Some(c);
                }
                Some(t) => {
                    let old = self.nodes[t].y;
                    self.nodes[c].x = old;
                    self.nodes[t].y = Some(c);
                }
            }
            self.nodes[c].y = right;
            tail = Some(c);
        }
        Ok(root)
    }

    fn alt(&mut self) -> Result<Option<usize>, Fail> {
        let mut alt = self.cat()?;
        while self.accept('|' as i32)? {
            let a = self.node(NodeKind::Alt);
            let y = self.cat()?;
            self.nodes[a].x = alt;
            self.nodes[a].y = y;
            alt = Some(a);
        }
        Ok(alt)
    }

    /// `count`: instructions a node compiles to.
    fn count(&self, node: Option<usize>, depth: usize) -> Result<usize, Fail> {
        let Some(i) = node else { return Ok(0) };
        let depth = depth + 1;
        if depth > MAXREC {
            return die("stack overflow");
        }
        let n = &self.nodes[i];
        Ok(match n.kind {
            NodeKind::Cat => self.count(n.x, depth)? + self.count(n.y, depth)?,
            NodeKind::Alt => self.count(n.x, depth)? + self.count(n.y, depth)? + 2,
            NodeKind::Rep => {
                let (min, max) = (n.m as usize, n.n as usize);
                let x = self.count(n.x, depth)?;
                let total = if min == max {
                    x.checked_mul(min)
                } else if max < REPINF as usize {
                    x.checked_mul(max).map(|v| v + (max - min))
                } else {
                    x.checked_mul(min + 1).map(|v| v + 2)
                };
                match total {
                    Some(t) if t <= MAXPROG => t,
                    _ => return die("program too large"),
                }
            }
            NodeKind::Par | NodeKind::Pla | NodeKind::Nla => self.count(n.x, depth)? + 2,
            _ => 1,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    End,
    Jump,
    Split,
    Pla,
    Nla,
    AnyNl,
    Any,
    Char,
    Cclass,
    Ncclass,
    Ref,
    Bol,
    Eol,
    Word,
    NWord,
    Lpar,
    Rpar,
}

#[derive(Debug, Clone)]
struct Inst {
    op: Op,
    n: usize,
    c: i32,
    cc: usize,
    x: usize,
    y: usize,
}

/// A compiled expression (`fz_regex`).
#[derive(Debug, Clone)]
pub struct Regex {
    prog: Vec<Inst>,
    classes: Vec<Vec<(i32, i32)>>,
    flags: u32,
}

impl Regex {
    fn emit(&mut self, op: Op) -> usize {
        self.prog.push(Inst {
            op,
            n: 0,
            c: 0,
            cc: 0,
            x: 0,
            y: 0,
        });
        self.prog.len() - 1
    }

    /// `compile`.
    fn compile_node(&mut self, nodes: &[Node], node: Option<usize>) {
        let mut node = node;
        while let Some(i) = node {
            let n = &nodes[i];
            match n.kind {
                NodeKind::Cat => {
                    self.compile_node(nodes, n.x);
                    node = n.y;
                    continue;
                }
                NodeKind::Alt => {
                    let split = self.emit(Op::Split);
                    self.compile_node(nodes, n.x);
                    let jump = self.emit(Op::Jump);
                    self.compile_node(nodes, n.y);
                    self.prog[split].x = split + 1;
                    self.prog[split].y = jump + 1;
                    self.prog[jump].x = self.prog.len();
                }
                NodeKind::Rep => {
                    let mut inst = self.prog.len();
                    for _ in 0..n.m {
                        inst = self.prog.len();
                        self.compile_node(nodes, n.x);
                    }
                    if n.m == n.n {
                    } else if n.n < REPINF {
                        for _ in n.m..n.n {
                            let split = self.emit(Op::Split);
                            self.compile_node(nodes, n.x);
                            let end = self.prog.len();
                            let s = &mut self.prog[split];
                            if n.ng {
                                s.y = split + 1;
                                s.x = end;
                            } else {
                                s.x = split + 1;
                                s.y = end;
                            }
                        }
                    } else if n.m == 0 {
                        let split = self.emit(Op::Split);
                        self.compile_node(nodes, n.x);
                        let jump = self.emit(Op::Jump);
                        let end = self.prog.len();
                        let s = &mut self.prog[split];
                        if n.ng {
                            s.y = split + 1;
                            s.x = end;
                        } else {
                            s.x = split + 1;
                            s.y = end;
                        }
                        self.prog[jump].x = split;
                    } else {
                        let split = self.emit(Op::Split);
                        let end = self.prog.len();
                        let s = &mut self.prog[split];
                        if n.ng {
                            s.y = inst;
                            s.x = end;
                        } else {
                            s.x = inst;
                            s.y = end;
                        }
                    }
                }
                NodeKind::Bol => {
                    self.emit(Op::Bol);
                }
                NodeKind::Eol => {
                    self.emit(Op::Eol);
                }
                NodeKind::Word => {
                    self.emit(Op::Word);
                }
                NodeKind::NWord => {
                    self.emit(Op::NWord);
                }
                NodeKind::Par => {
                    let l = self.emit(Op::Lpar);
                    self.prog[l].n = n.n as usize;
                    self.compile_node(nodes, n.x);
                    let r = self.emit(Op::Rpar);
                    self.prog[r].n = n.n as usize;
                }
                NodeKind::Pla | NodeKind::Nla => {
                    let split = self.emit(if n.kind == NodeKind::Pla {
                        Op::Pla
                    } else {
                        Op::Nla
                    });
                    self.compile_node(nodes, n.x);
                    self.emit(Op::End);
                    self.prog[split].x = split + 1;
                    self.prog[split].y = self.prog.len();
                }
                NodeKind::Any => {
                    self.emit(Op::Any);
                }
                NodeKind::Char => {
                    let c = if self.flags & ICASE != 0 {
                        canon(n.c)
                    } else {
                        n.c
                    };
                    let i = self.emit(Op::Char);
                    self.prog[i].c = c;
                }
                NodeKind::Cclass | NodeKind::Ncclass => {
                    let i = self.emit(if n.kind == NodeKind::Cclass {
                        Op::Cclass
                    } else {
                        Op::Ncclass
                    });
                    self.prog[i].cc = n.cc;
                }
                NodeKind::Ref => {
                    let i = self.emit(Op::Ref);
                    self.prog[i].n = n.n as usize;
                }
            }
            break;
        }
    }

    /// `fz_regcomp`.
    pub fn compile(pattern: &[u8], flags: u32) -> Result<Regex, String> {
        let len = pattern
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(pattern.len());
        if len * 2 > MAXPROG {
            return die("program too large");
        }
        let mut c = Compiler {
            source: &pattern[..len],
            at: 0,
            nodes: Vec::new(),
            classes: Vec::new(),
            nsub: 1,
            sub: [None; MAXSUB],
            lookahead: 0,
            yychar: 0,
            yycc: 0,
            yymin: 0,
            yymax: 0,
        };
        c.next()?;
        let node = c.alt()?;
        if c.lookahead == ')' as i32 {
            return die("unmatched ')'");
        }
        if c.lookahead != EOF {
            return die("syntax error");
        }
        let n = 6 + c.count(node, 0)?;
        if n > MAXPROG {
            return die("program too large");
        }
        let mut re = Regex {
            prog: Vec::with_capacity(n),
            classes: std::mem::take(&mut c.classes),
            flags,
        };
        let split = re.emit(Op::Split);
        re.prog[split].x = split + 3;
        re.prog[split].y = split + 1;
        re.emit(Op::AnyNl);
        let jump = re.emit(Op::Jump);
        re.prog[jump].x = split;
        re.emit(Op::Lpar);
        re.compile_node(&c.nodes, node);
        re.emit(Op::Rpar);
        re.emit(Op::End);
        Ok(re)
    }

    fn in_class(&self, cc: usize, c: i32) -> bool {
        self.classes[cc].iter().any(|&(a, b)| a <= c && c <= b)
    }

    /// `incclasscanon`: whether a rune of the class canonicalizes to `c`.
    /// MuPDF tries every rune of each range; the runes that can are `c`
    /// itself and those whose upper case is `c`, found from the case
    /// tables, which gives the same answer without the walk.
    fn in_class_canon(&self, cc: usize, c: i32) -> bool {
        let inside = |r: i32| self.classes[cc].iter().any(|&(a, b)| a <= r && r <= b);
        (canon(c) == c && inside(c)) || upper_sources(c).iter().any(|&r| canon(r) == c && inside(r))
    }

    /// `fz_regexec` from byte `sp` of `s` (a C string: NUL past its end):
    /// the whole match's start and end, or None.
    pub fn exec(&self, s: &[u8], sp: usize, eflags: u32) -> Result<Option<(usize, usize)>, String> {
        let mut sub = [(None, None); MAXSUB];
        let mut runaway = 0u32;
        let m = Matcher {
            re: self,
            s,
            bol: sp,
            flags: self.flags | eflags,
        };
        match m.run(0, sp, &mut sub, 0, &mut runaway) {
            -1 => Err("regexec failure".into()),
            0 => Ok(match sub[0] {
                (Some(a), Some(b)) => Some((a, b)),
                _ => None,
            }),
            _ => Ok(None),
        }
    }
}

type Subs = [(Option<usize>, Option<usize>); MAXSUB];

struct Matcher<'a> {
    re: &'a Regex,
    s: &'a [u8],
    bol: usize,
    flags: u32,
}

fn is_newline(b: u8) -> bool {
    b == 0xa || b == 0xd
}

fn is_word_char(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

impl Matcher<'_> {
    fn byte(&self, i: usize) -> u8 {
        self.s.get(i).copied().unwrap_or(0)
    }

    fn rune(&self, sp: &mut usize) -> i32 {
        let (c, len) = chartorune(self.s.get(*sp..).unwrap_or_default());
        *sp += len;
        c
    }

    /// `match`: 0 for a match, 1 for none, -1 for too deep or too long.
    fn run(&self, pc: usize, sp: usize, out: &mut Subs, depth: usize, runaway: &mut u32) -> i32 {
        if depth > MAXREC {
            return -1;
        }
        let (mut pc, mut sp) = (pc, sp);
        let icase = self.flags & ICASE != 0;
        loop {
            if self.flags & RUNAWAY != 0 {
                *runaway += 1;
                if *runaway >= MAXRUN {
                    return -1;
                }
            }
            let Some(inst) = self.re.prog.get(pc) else {
                return 1;
            };
            match inst.op {
                Op::End => return 0,
                Op::Jump => pc = inst.x,
                Op::Split => {
                    let mut scratch = *out;
                    match self.run(inst.x, sp, &mut scratch, depth + 1, runaway) {
                        -1 => return -1,
                        0 => {
                            *out = scratch;
                            return 0;
                        }
                        _ => pc = inst.y,
                    }
                }
                Op::Pla => match self.run(inst.x, sp, out, depth + 1, runaway) {
                    -1 => return -1,
                    1 => return 1,
                    _ => pc = inst.y,
                },
                Op::Nla => {
                    let mut scratch = *out;
                    match self.run(inst.x, sp, &mut scratch, depth + 1, runaway) {
                        -1 => return -1,
                        0 => return 1,
                        _ => pc = inst.y,
                    }
                }
                Op::AnyNl => {
                    if self.byte(sp) == 0 {
                        return 1;
                    }
                    self.rune(&mut sp);
                    pc += 1;
                }
                Op::Any => {
                    if self.byte(sp) == 0 {
                        return 1;
                    }
                    let c = self.rune(&mut sp);
                    if matches!(c, 0xa | 0xd | 0x2028 | 0x2029) {
                        return 1;
                    }
                    pc += 1;
                }
                Op::Char => {
                    if self.byte(sp) == 0 {
                        return 1;
                    }
                    let mut c = self.rune(&mut sp);
                    if icase {
                        c = canon(c);
                    }
                    if c != inst.c {
                        return 1;
                    }
                    pc += 1;
                }
                Op::Cclass | Op::Ncclass => {
                    if self.byte(sp) == 0 {
                        return 1;
                    }
                    let c = self.rune(&mut sp);
                    let inside = if icase {
                        self.re.in_class_canon(inst.cc, canon(c))
                    } else {
                        self.re.in_class(inst.cc, c)
                    };
                    if inside != (inst.op == Op::Cclass) {
                        return 1;
                    }
                    pc += 1;
                }
                Op::Ref => {
                    let (a, b) = out[inst.n];
                    let (start, len) = match (a, b) {
                        (Some(a), Some(b)) if b >= a => (a, b - a),
                        _ => (0, 0),
                    };
                    let differs = if icase {
                        self.cmp_canon(sp, start, len)
                    } else {
                        self.strncmp(sp, start, len)
                    };
                    if differs {
                        return 1;
                    }
                    sp += len;
                    pc += 1;
                }
                Op::Bol => {
                    if sp == self.bol && self.flags & NOTBOL == 0 {
                        pc += 1;
                        continue;
                    }
                    if self.flags & NEWLINE != 0 && sp > self.bol && is_newline(self.byte(sp - 1)) {
                        pc += 1;
                        continue;
                    }
                    return 1;
                }
                Op::Eol => {
                    let b = self.byte(sp);
                    if b == 0 || (self.flags & NEWLINE != 0 && is_newline(b)) {
                        pc += 1;
                        continue;
                    }
                    return 1;
                }
                Op::Word | Op::NWord => {
                    let before = sp > self.bol && is_word_char(self.byte(sp - 1));
                    let edge = before ^ is_word_char(self.byte(sp));
                    if edge != (inst.op == Op::Word) {
                        return 1;
                    }
                    pc += 1;
                }
                Op::Lpar => {
                    out[inst.n].0 = Some(sp);
                    pc += 1;
                }
                Op::Rpar => {
                    out[inst.n].1 = Some(sp);
                    pc += 1;
                }
            }
        }
    }

    /// `strncmp`: whether `n` bytes differ, as C strings.
    fn strncmp(&self, a: usize, b: usize, n: usize) -> bool {
        for k in 0..n {
            let (x, y) = (self.byte(a + k), self.byte(b + k));
            if x != y {
                return true;
            }
            if x == 0 {
                return false;
            }
        }
        false
    }

    /// `strncmpcanon`: whether `n` runes differ.
    fn cmp_canon(&self, a: usize, b: usize, n: usize) -> bool {
        let (mut a, mut b) = (a, b);
        for _ in 0..n {
            if self.byte(a) == 0 || self.byte(b) == 0 {
                return true;
            }
            let ra = self.rune(&mut a);
            let rb = self.rune(&mut b);
            if canon(ra) != canon(rb) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(pattern: &str, text: &str, flags: u32) -> Option<(usize, usize)> {
        Regex::compile(pattern.as_bytes(), flags)
            .unwrap_or_else(|e| panic!("{e}"))
            .exec(text.as_bytes(), 0, 0)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn javascript_syntax_matches_leftmost() {
        assert_eq!(find("b+", "abbbc", 0), Some((1, 4)));
        assert_eq!(find("b+?", "abbbc", 0), Some((1, 2)));
        assert_eq!(find("[0-9]{2,3}", "x12345", 0), Some((1, 4)));
        assert_eq!(find("(a|b)c\\1", "xbcb", 0), Some((1, 4)));
        assert_eq!(find("\\bword\\b", "a word.", 0), Some((2, 6)));
        assert_eq!(find("^b", "a\nb", NEWLINE), Some((2, 3)));
        assert_eq!(find("x(?=y)", "xzxy", 0), Some((2, 3)));
        assert_eq!(find("ABC", "xabc", ICASE), Some((1, 4)));
        assert_eq!(find("[a-c]+", "xABCd", ICASE), Some((1, 4)));
        assert_eq!(find("[^a-c]", "ABCd", ICASE), Some((3, 4)));
        assert_eq!(find("[\\W]", "\u{e9}", ICASE), Some((0, 2)));
        assert_eq!(find("q", "abc", 0), None);
    }

    #[test]
    fn bad_patterns_fail_as_mupdf_says() {
        let err = |p: &str| Regex::compile(p.as_bytes(), 0).err().unwrap_or_default();
        assert_eq!(err("(a"), "regcomp failure: unmatched '('");
        assert_eq!(err("a)"), "regcomp failure: unmatched ')'");
        assert_eq!(err("\\q"), "regcomp failure: invalid escape character");
        assert_eq!(
            err("(?:)*"),
            "regcomp failure: infinite loop matching the empty string"
        );
    }
}
