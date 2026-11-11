// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `cargo xtask tables`: the data MuPDF compiles in, taken from the pinned
//! oracle's sources and written where nupdf includes it.
//!
//! - `crates/nupdf/src/tables.rs`: the Adobe Glyph List as MuPDF keeps it
//!   (source/fitz/glyphlist.h) and the encodings fonts use
//!   (source/fitz/encodings.h), and each code point's bidi class
//!   (source/fitz/ucdn_db.h).
//! - `crates/nupdf/data/cmaps.bin`: the predefined CMaps, as MuPDF dumped
//!   them into source/pdf/cmaps/*.h, in a compact binary form.
//! - `crates/nupdf/src/font/names.rs`: FreeType's PostScript name tables
//!   (the standard strings, the Macintosh glyph names) and CFF's standard
//!   encodings and charsets, from the FreeType bundled with the oracle.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};

const TABLES: &str = "crates/nupdf/src/tables.rs";
const CMAPS: &str = "crates/nupdf/data/cmaps.bin";
const NAMES: &str = "crates/nupdf/src/font/names.rs";

pub fn run(root: &Path, check: bool) -> Result<()> {
    let source = crate::oracle::source_dir(root)?.join("source");
    let tables = tables(&source)?;
    let cmaps = cmaps(&source.join("pdf/cmaps"))?;
    let names = ps_names(&source.join("../thirdparty/freetype/src"))?;
    let outputs: [(&str, Vec<u8>); 3] = [
        (TABLES, tables.into_bytes()),
        (CMAPS, cmaps),
        (NAMES, names.into_bytes()),
    ];
    for (path, data) in outputs {
        let path = root.join(path);
        if check {
            let have =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            if have != data {
                bail!("{} is out of date: run cargo xtask tables", path.display());
            }
        } else {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
            println!("wrote {}", path.display());
        }
    }
    Ok(())
}

/// The body of a C array initializer `name[...] = { ... };`.
fn array<'a>(text: &'a str, name: &str) -> Result<&'a str> {
    let at = text
        .find(&format!("{name}["))
        .with_context(|| format!("no array {name}"))?;
    let open = at + text[at..].find('{').context("no array body")?;
    // The matching brace, outside string and character literals.
    let bytes = text.as_bytes();
    let (mut depth, mut i, mut quote) = (0usize, open, None::<u8>);
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == b'\\' {
                    i += 1;
                } else if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(&text[open + 1..i]);
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    bail!("unterminated array {name}")
}

/// The items of an initializer: string literals, numbers and `_notdef`
/// (None), comments dropped.
fn items(body: &str) -> Result<Vec<Option<String>>> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ',');
        if rest.is_empty() {
            break;
        }
        if let Some(r) = rest.strip_prefix("/*") {
            let end = r.find("*/").context("unterminated comment")?;
            rest = &r[end + 2..];
        } else if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"').context("unterminated string")?;
            out.push(Some(r[..end].to_string()));
            rest = &r[end + 1..];
        } else {
            let end = rest
                .find(|c: char| c == ',' || c.is_whitespace())
                .unwrap_or(rest.len());
            let word = &rest[..end];
            out.push(match word {
                "_notdef" | "NULL" => None,
                w => Some(w.to_string()),
            });
            rest = &rest[end..];
        }
    }
    Ok(out)
}

fn number(s: &str) -> Result<u32> {
    let s = s.trim();
    let v = match s.strip_prefix("0x") {
        Some(h) => u32::from_str_radix(h, 16),
        None => s.parse(),
    };
    v.with_context(|| format!("not a number: {s}"))
}

fn numbers(body: &str) -> Result<Vec<u32>> {
    items(body)?
        .into_iter()
        .map(|i| number(&i.unwrap_or_default()))
        .collect()
}

fn rust_str(s: &str) -> String {
    format!("{s:?}")
}

fn tables(source: &Path) -> Result<String> {
    let glyphs = std::fs::read_to_string(source.join("fitz/glyphlist.h"))?;
    let encodings = std::fs::read_to_string(source.join("fitz/encodings.h"))?;
    let mut out = String::from(
        "// SPDX-License-Identifier: AGPL-3.0-or-later\n\
         // Copyright (C) 2026 hum\n\
         // Derived from MuPDF source/fitz/glyphlist.h, source/fitz/encodings.h\n\
         // and source/fitz/ucdn_db.h, Copyright (C) 2004-2026 Artifex Software, Inc.\n\
         // Generated by `cargo xtask tables`. Do not edit.\n\n\
         //! The Adobe Glyph List, the font encodings and the bidi classes MuPDF\n\
         //! compiles in.\n\n",
    );
    let names = items(array(&glyphs, "single_name_list")?)?;
    let codes = numbers(array(&glyphs, "single_code_list")?)?;
    if names.len() != codes.len() {
        bail!("glyph list names and codes differ in length");
    }
    let _ = writeln!(
        out,
        "/// Glyph names, sorted, with `AGL_CODES` their code points."
    );
    let _ = writeln!(out, "pub static AGL_NAMES: [&str; {}] = [", names.len());
    for n in &names {
        let _ = writeln!(out, "    {},", rust_str(n.as_deref().unwrap_or("")));
    }
    let _ = writeln!(
        out,
        "];\n\npub static AGL_CODES: [u16; {}] = [",
        codes.len()
    );
    for chunk in codes.chunks(12) {
        let line: Vec<String> = chunk.iter().map(|c| format!("0x{c:04x}")).collect();
        let _ = writeln!(out, "    {},", line.join(", "));
    }
    let _ = writeln!(out, "];\n");
    let dup_offsets = numbers(array(&glyphs, "agl_dup_offsets")?)?;
    let _ = writeln!(
        out,
        "/// Pairs of a code point and the offset of its names in `AGL_DUP_NAMES`."
    );
    let _ = writeln!(
        out,
        "pub static AGL_DUP_OFFSETS: [u16; {}] = [",
        dup_offsets.len()
    );
    for chunk in dup_offsets.chunks(12) {
        let line: Vec<String> = chunk.iter().map(|c| format!("{c}")).collect();
        let _ = writeln!(out, "    {},", line.join(", "));
    }
    let _ = writeln!(out, "];\n");
    let dup_names = items(array(&glyphs, "agl_dup_names")?)?;
    let _ = writeln!(
        out,
        "/// Glyph names that share a code point, each list ended by `\"\"`."
    );
    let _ = writeln!(
        out,
        "pub static AGL_DUP_NAMES: [&str; {}] = [",
        dup_names.len()
    );
    for n in &dup_names {
        let n = n.as_deref().filter(|n| *n != "0").unwrap_or("");
        let _ = writeln!(out, "    {},", rust_str(n));
    }
    let _ = writeln!(out, "];\n");
    for (rust, c) in [
        ("STANDARD", "fz_glyph_name_from_adobe_standard"),
        ("MAC_ROMAN", "fz_glyph_name_from_mac_roman"),
        ("MAC_EXPERT", "fz_glyph_name_from_mac_expert"),
        ("WIN_ANSI", "fz_glyph_name_from_win_ansi"),
    ] {
        let names = items(array(&encodings, c)?)?;
        if names.len() != 256 {
            bail!("{c} has {} entries", names.len());
        }
        let _ = writeln!(out, "/// `{c}`.");
        let _ = writeln!(out, "pub static {rust}: [Option<&str>; 256] = [");
        for n in &names {
            match n {
                Some(n) => {
                    let _ = writeln!(out, "    Some({}),", rust_str(n));
                }
                None => {
                    let _ = writeln!(out, "    None,");
                }
            }
        }
        let _ = writeln!(out, "];\n");
    }
    for (rust, c) in [
        ("DOS_437", "fz_unicode_from_dos_437"),
        (
            "TRUETYPE_GLYPH_INDEX",
            "fz_unicode_from_true_type_glyph_index",
        ),
    ] {
        let codes = numbers(array(&encodings, c)?)?;
        let _ = writeln!(out, "/// `{c}`.");
        let _ = writeln!(out, "pub static {rust}: [u16; {}] = [", codes.len());
        for chunk in codes.chunks(12) {
            let line: Vec<String> = chunk.iter().map(|c| format!("{c}")).collect();
            let _ = writeln!(out, "    {},", line.join(", "));
        }
        let _ = writeln!(out, "];\n");
    }
    bidi_classes(
        &std::fs::read_to_string(source.join("fitz/ucdn_db.h"))?,
        &mut out,
    )?;
    Ok(out.trim_end().to_string() + "\n")
}

/// `ucdn_get_bidi_class` for every code point, as runs: each entry is the
/// first code point of a run and the class of the run.
fn bidi_classes(db: &str, out: &mut String) -> Result<()> {
    let records: Vec<u32> = array(db, "ucd_records")?
        .split(['{', '}', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(number)
        .collect::<Result<_>>()?;
    if !records.len().is_multiple_of(6) {
        bail!("ucd_records is not a list of six-field records");
    }
    let bidi: Vec<u32> = records.chunks(6).map(|r| r[2]).collect();
    let index0 = numbers(array(db, "index0")?)?;
    let index1 = numbers(array(db, "index1")?)?;
    let index2 = numbers(array(db, "index2")?)?;
    let (shift1, shift2) = (5u32, 3u32);
    for (name, want) in [("SHIFT1", shift1), ("SHIFT2", shift2)] {
        if !db.contains(&format!("#define {name} {want}\n")) {
            bail!("ucdn_db.h changed {name}");
        }
    }
    let class = |code: u32| -> Result<u32> {
        let get = |v: &[u32], i: u32| {
            v.get(i as usize)
                .copied()
                .context("ucdn index out of range")
        };
        let index = get(&index0, code >> (shift1 + shift2))? << shift1;
        let offset = (code >> shift2) & ((1 << shift1) - 1);
        let index = get(&index1, index + offset)? << shift2;
        let offset = code & ((1 << shift2) - 1);
        get(&bidi, get(&index2, index + offset)?)
    };
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for code in 0..0x110000u32 {
        let c = class(code)?;
        if runs.last().is_none_or(|&(_, last)| last != c) {
            runs.push((code, c));
        }
    }
    let _ = writeln!(
        out,
        "/// `ucdn_get_bidi_class` (Unicode {}) as runs of code points: the first\n\
         /// code point of each run and its class.",
        db.split("UNIDATA_VERSION \"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .unwrap_or("?")
    );
    let _ = writeln!(
        out,
        "pub static BIDI_CLASS: [(u32, u8); {}] = [",
        runs.len()
    );
    for chunk in runs.chunks(6) {
        let line: Vec<String> = chunk
            .iter()
            .map(|(s, c)| format!("(0x{s:x}, {c})"))
            .collect();
        let _ = writeln!(out, "    {},", line.join(", "));
    }
    let _ = writeln!(out, "];\n");
    Ok(())
}

/// The NUL-separated names of `ft_standard_glyph_names`, at their offsets.
fn glyph_name_blob(text: &str) -> Result<Vec<u8>> {
    let body = array(text, "ft_standard_glyph_names")?;
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find('\'') {
        let r = &rest[at + 1..];
        let end = r.find('\'').context("unterminated char")?;
        let ch = &r[..end];
        out.push(match ch {
            "\\\\" => b'\\',
            c => *c.as_bytes().first().context("empty char")?,
        });
        rest = &r[end + 1..];
        let rest_trim = rest.trim_start_matches(',').trim_start();
        // A bare 0 ends each name.
        if let Some(r2) = rest_trim.strip_prefix('0')
            && !r2.starts_with('x')
        {
            out.push(0);
            rest = r2;
        }
    }
    Ok(out)
}

fn name_at(blob: &[u8], offset: usize) -> Result<String> {
    let tail = blob.get(offset..).context("name offset out of range")?;
    let end = tail
        .iter()
        .position(|&b| b == 0)
        .context("unterminated name")?;
    Ok(String::from_utf8_lossy(&tail[..end]).into_owned())
}

fn ps_names(freetype: &Path) -> Result<String> {
    let pstables = std::fs::read_to_string(freetype.join("psnames/pstables.h"))?;
    let cffload = std::fs::read_to_string(freetype.join("cff/cffload.c"))?;
    let blob = glyph_name_blob(&pstables)?;
    let mut out = String::from(
        "// SPDX-License-Identifier: AGPL-3.0-or-later\n\
         // Copyright (C) 2026 hum\n\
         // Derived from FreeType 2 src/psnames/pstables.h and src/cff/cffload.c,\n\
         // Copyright (C) 1996-2025 by David Turner, Robert Wilhelm, and Werner\n\
         // Lemberg, used under the FreeType License (NOTICE.md).\n\
         // Generated by `cargo xtask tables`. Do not edit.\n\n\
         //! The PostScript names FreeType knows by number, and CFF's\n\
         //! predefined encodings and charsets.\n\n",
    );
    for (rust, c, doc) in [
        (
            "SID_NAMES",
            "ft_sid_names",
            "The CFF standard strings, by SID.",
        ),
        (
            "MAC_NAMES",
            "ft_mac_names",
            "The Macintosh standard glyph names a `post` table indexes.",
        ),
    ] {
        let offsets = numbers(array(&pstables, c)?)?;
        let _ = writeln!(out, "/// {doc}");
        let _ = writeln!(out, "pub static {rust}: [&str; {}] = [", offsets.len());
        for o in offsets {
            let _ = writeln!(out, "    {},", rust_str(&name_at(&blob, o as usize)?));
        }
        let _ = writeln!(out, "];\n");
    }
    for (rust, c) in [
        ("STANDARD_ENCODING", "cff_standard_encoding"),
        ("EXPERT_ENCODING", "cff_expert_encoding"),
        ("EXPERT_CHARSET", "cff_expert_charset"),
        ("EXPERTSUBSET_CHARSET", "cff_expertsubset_charset"),
    ] {
        let sids = numbers(array(&cffload, c)?)?;
        let _ = writeln!(out, "/// `{c}`: SIDs.");
        let _ = writeln!(out, "pub static {rust}: [u16; {}] = [", sids.len());
        for chunk in sids.chunks(12) {
            let line: Vec<String> = chunk.iter().map(|c| format!("{c}")).collect();
            let _ = writeln!(out, "    {},", line.join(", "));
        }
        let _ = writeln!(out, "];\n");
    }
    Ok(out.trim_end().to_string() + "\n")
}

fn varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn short(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let len = u8::try_from(s.len()).context("name too long")?;
    out.push(len);
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// One dumped CMap: its name, use-CMap, writing mode, code spaces, then
/// its ranges (16-bit and wider ones together), one-to-many ranges and
/// their table, each as varints.
fn cmap(text: &str, out: &mut Vec<u8>) -> Result<()> {
    let field = |label: &str| -> Result<&str> {
        let at = text
            .find(&format!("/* {label} */"))
            .with_context(|| format!("no {label}"))?;
        Ok(&text[at + label.len() + 6..])
    };
    let quoted = |s: &str| -> Result<String> {
        let s = s.trim_start();
        let s = s.strip_prefix('"').context("expected a string")?;
        Ok(s[..s.find('"').context("unterminated")?].to_string())
    };
    let name = quoted(field("cmapname")?)?;
    let usecmap = quoted(field("usecmap")?)?;
    let wmode = number(field("wmode")?.split(',').next().unwrap_or(""))?;
    let cs = field("codespaces")?;
    let count = number(cs.split(',').next().unwrap_or(""))?;
    short(out, &name)?;
    short(out, &usecmap)?;
    out.push(u8::try_from(wmode)?);
    out.push(u8::try_from(count)?);
    let mut rest = &cs[cs.find('{').context("no codespaces")? + 1..];
    for _ in 0..count {
        let open = rest.find('{').context("codespace")?;
        let close = rest.find('}').context("codespace")?;
        let v = numbers(&rest[open + 1..close])?;
        if v.len() != 3 {
            bail!("codespace of {name}");
        }
        out.push(u8::try_from(v[0])?);
        varint(out, v[1]);
        varint(out, v[2]);
        rest = &rest[close + 1..];
    }
    let id = name.replace('-', "_");
    let list = |kind: &str| -> Result<Vec<u32>> {
        let array_name = format!("cmap_{id}_{kind}");
        match text.find(&format!("{array_name}[]")) {
            Some(_) => {
                let body = array(text, &array_name)?;
                numbers(&body.replace(['{', '}'], " "))
            }
            None => Ok(Vec::new()),
        }
    };
    let mut ranges: Vec<[u32; 3]> = list("ranges")?
        .chunks(3)
        .map(|c| [c[0], c[1], c[2]])
        .collect();
    ranges.extend(list("xranges")?.chunks(3).map(|c| [c[0], c[1], c[2]]));
    ranges.sort_by_key(|r| r[0]);
    varint(out, u32::try_from(ranges.len())?);
    let mut prev = 0;
    for [low, high, o] in ranges {
        varint(out, low - prev);
        varint(out, high - low);
        varint(out, o);
        prev = high;
    }
    let mranges = list("mranges")?;
    varint(out, u32::try_from(mranges.len() / 2)?);
    for c in mranges.chunks(2) {
        varint(out, c[0]);
        varint(out, c[1]);
    }
    let table = list("table")?;
    varint(out, u32::try_from(table.len())?);
    for v in table {
        varint(out, v);
    }
    Ok(())
}

fn cmaps(dir: &Path) -> Result<Vec<u8>> {
    let mut names: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "h"))
        .collect();
    names.sort();
    let mut out = b"NCM1".to_vec();
    varint(&mut out, u32::try_from(names.len())?);
    for path in names {
        let text = std::fs::read_to_string(&path)?;
        cmap(&text, &mut out).with_context(|| format!("in {}", path.display()))?;
    }
    Ok(out)
}
