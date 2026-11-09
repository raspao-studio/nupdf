// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! The smoke corpus: small handcrafted PDFs, one syntax feature or failure
//! mode each. Generated deterministically so the bytes are reviewable and
//! license-free. `cargo xtask fixtures` writes them; `--check` verifies them.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};

pub struct Fixture {
    pub name: &'static str,
    pub about: &'static str,
    /// MuPDF is expected to warn and rebuild the xref for this file.
    pub needs_repair: bool,
    pub bytes: Vec<u8>,
}

pub fn all() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "minimal.pdf",
            about: "One page, classic xref table, Helvetica text",
            needs_repair: false,
            bytes: hello(Pdf::new("1.7"), "Hello, nupdf", false).finish_with_xref(""),
        },
        Fixture {
            name: "version-2.0.pdf",
            about: "PDF 2.0 header",
            needs_repair: false,
            bytes: hello(Pdf::new("2.0"), "PDF 2.0", false).finish_with_xref(""),
        },
        Fixture {
            name: "catalog-version.pdf",
            about: "Header says 1.4, /Root/Version overrides it to 1.7",
            needs_repair: false,
            bytes: catalog_version(),
        },
        Fixture {
            name: "multipage-inherit.pdf",
            about: "Three pages inheriting MediaBox and Resources; one rotated, one with its own box",
            needs_repair: false,
            bytes: multipage_inherit(),
        },
        Fixture {
            name: "flate-content.pdf",
            about: "Content stream compressed with FlateDecode",
            needs_repair: false,
            bytes: hello(Pdf::new("1.5"), "Compressed", true).finish_with_xref(""),
        },
        Fixture {
            name: "xref-stream.pdf",
            about: "Catalog, page tree and page inside an object stream, indexed by an xref stream",
            needs_repair: false,
            bytes: xref_stream(),
        },
        Fixture {
            name: "incremental-update.pdf",
            about: "Two revisions: the update replaces the page content and adds /Info via /Prev",
            needs_repair: false,
            bytes: incremental_update(),
        },
        Fixture {
            name: "cr-line-endings.pdf",
            about: "Every line ends in a bare CR",
            needs_repair: false,
            bytes: hello(Pdf::with_eol("1.4", "\r"), "CR only", false).finish_with_xref(""),
        },
        Fixture {
            name: "crlf-line-endings.pdf",
            about: "Every line ends in CRLF",
            needs_repair: false,
            bytes: hello(Pdf::with_eol("1.4", "\r\n"), "CRLF", false).finish_with_xref(""),
        },
        Fixture {
            name: "garbage-prefix.pdf",
            about: "64 bytes of junk before %PDF-; offsets are relative to the header",
            needs_repair: false,
            bytes: garbage_prefix(),
        },
        Fixture {
            name: "broken-xref-offsets.pdf",
            about: "Every xref offset is 7 bytes too far",
            needs_repair: true,
            bytes: hello(Pdf::new("1.4"), "Skewed", false)
                .skew_offsets(7)
                .finish_with_xref(""),
        },
        Fixture {
            name: "no-xref.pdf",
            about: "Objects and trailer, but no xref table and no startxref",
            needs_repair: true,
            bytes: no_xref(),
        },
        Fixture {
            name: "truncated.pdf",
            about: "minimal.pdf cut off inside the page object",
            needs_repair: true,
            bytes: truncated(),
        },
    ]
}

pub fn run(root: &Path, check: bool) -> Result<()> {
    let dir = root.join("corpus/smoke");
    let fixtures = all();
    let mut expected: BTreeMap<String, Vec<u8>> = fixtures
        .iter()
        .map(|f| (f.name.to_string(), f.bytes.clone()))
        .collect();
    expected.insert("README.md".into(), readme(&fixtures).into_bytes());

    if check {
        let mut problems = Vec::new();
        for (name, bytes) in &expected {
            match std::fs::read(dir.join(name)) {
                Ok(found) if &found == bytes => {}
                Ok(_) => problems.push(format!("{name} differs from its generator")),
                Err(_) => problems.push(format!("{name} is missing")),
            }
        }
        for entry in std::fs::read_dir(&dir).context("reading corpus/smoke")? {
            let name = entry?.file_name().to_string_lossy().to_string();
            if !expected.contains_key(&name) {
                problems.push(format!("{name} has no generator"));
            }
        }
        if !problems.is_empty() {
            bail!(
                "corpus/smoke is stale; run `cargo xtask fixtures`:\n  {}",
                problems.join("\n  ")
            );
        }
        println!("corpus/smoke is up to date ({} fixtures)", fixtures.len());
        return Ok(());
    }

    std::fs::create_dir_all(&dir)?;
    for (name, bytes) in &expected {
        std::fs::write(dir.join(name), bytes)?;
    }
    println!("wrote {} fixtures to {}", fixtures.len(), dir.display());
    Ok(())
}

fn readme(fixtures: &[Fixture]) -> String {
    let mut out = String::from(
        "# Smoke corpus\n\n\
         Generated by `cargo xtask fixtures` (xtask/src/fixtures.rs). Do not edit by hand.\n\n\
         | File | Exercises | MuPDF repairs it |\n|---|---|---|\n",
    );
    for f in fixtures {
        let repair = if f.needs_repair { "yes" } else { "no" };
        let _ = writeln!(out, "| `{}` | {} | {repair} |", f.name, f.about);
    }
    out
}

/// Minimal PDF writer that tracks object offsets so xref tables are exact.
struct Pdf {
    out: Vec<u8>,
    eol: &'static str,
    offsets: BTreeMap<u32, usize>,
    skew: usize,
}

impl Pdf {
    fn new(version: &str) -> Self {
        Self::with_eol(version, "\n")
    }

    fn with_eol(version: &str, eol: &'static str) -> Self {
        let mut pdf = Pdf {
            out: Vec::new(),
            eol,
            offsets: BTreeMap::new(),
            skew: 0,
        };
        pdf.line(&format!("%PDF-{version}"));
        // Binary comment so tools treat the file as binary, as real writers do.
        pdf.out.extend_from_slice(b"%\xe2\xe3\xcf\xd3");
        pdf.out.extend_from_slice(eol.as_bytes());
        pdf
    }

    fn line(&mut self, text: &str) {
        self.out.extend_from_slice(text.as_bytes());
        self.out.extend_from_slice(self.eol.as_bytes());
    }

    fn obj(&mut self, num: u32, body: &str) {
        self.offsets.insert(num, self.out.len());
        self.line(&format!("{num} 0 obj"));
        self.line(body);
        self.line("endobj");
    }

    fn stream(&mut self, num: u32, dict: &str, data: &[u8]) {
        self.offsets.insert(num, self.out.len());
        self.line(&format!("{num} 0 obj"));
        self.line(&format!("<<{dict}/Length {}>>", data.len()));
        self.line("stream");
        self.out.extend_from_slice(data);
        self.out.extend_from_slice(self.eol.as_bytes());
        self.line("endstream");
        self.line("endobj");
    }

    fn skew_offsets(mut self, by: usize) -> Self {
        self.skew = by;
        self
    }

    /// Writes an xref section covering `nums` (plus entry 0 when `with_free_head`).
    fn xref(&mut self, nums: &[u32], with_free_head: bool) -> usize {
        let start = self.out.len();
        self.line("xref");
        let mut entries: Vec<u32> = nums.to_vec();
        if with_free_head {
            entries.insert(0, 0);
        }
        entries.sort_unstable();
        // Group into contiguous subsections.
        let mut i = 0;
        while i < entries.len() {
            let mut j = i + 1;
            while j < entries.len() && entries[j] == entries[j - 1] + 1 {
                j += 1;
            }
            self.line(&format!("{} {}", entries[i], j - i));
            for &num in &entries[i..j] {
                // Entries are exactly 20 bytes: the EOL is padded to 2 bytes.
                let eol = if self.eol.len() == 2 {
                    self.eol.to_string()
                } else {
                    format!(" {}", self.eol)
                };
                let entry = match self.offsets.get(&num) {
                    Some(&offset) if num != 0 => format!("{:010} 00000 n{eol}", offset + self.skew),
                    _ => format!("0000000000 65535 f{eol}"),
                };
                self.out.extend_from_slice(entry.as_bytes());
            }
            i = j;
        }
        start
    }

    fn size(&self) -> u32 {
        self.offsets.keys().max().map_or(1, |n| n + 1)
    }

    fn finish_with_xref(mut self, extra_trailer: &str) -> Vec<u8> {
        let nums: Vec<u32> = (1..self.size()).collect();
        let start = self.xref(&nums, true);
        self.trailer(&format!("/Root 1 0 R{extra_trailer}"), start);
        self.out
    }

    fn trailer(&mut self, entries: &str, xref_start: usize) {
        let size = self.size();
        self.line("trailer");
        self.line(&format!("<<{entries}/Size {size}>>"));
        self.line("startxref");
        self.line(&xref_start.to_string());
        self.line("%%EOF");
    }
}

const FONT: &str = "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>";

fn text_content(text: &str) -> Vec<u8> {
    format!("BT /F1 24 Tf 72 720 Td ({text}) Tj ET").into_bytes()
}

/// Objects 1-5: catalog, pages, one page, its content, its font.
fn hello(mut pdf: Pdf, text: &str, compress: bool) -> Pdf {
    pdf.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    pdf.obj(
        3,
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>",
    );
    let content = text_content(text);
    if compress {
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&content, 6);
        pdf.stream(4, "/Filter/FlateDecode", &packed);
    } else {
        pdf.stream(4, "", &content);
    }
    pdf.obj(5, FONT);
    pdf
}

fn catalog_version() -> Vec<u8> {
    let mut pdf = Pdf::new("1.4");
    pdf.obj(1, "<</Type/Catalog/Pages 2 0 R/Version/1.7>>");
    pdf.obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    pdf.obj(
        3,
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>",
    );
    pdf.stream(4, "", &text_content("Catalog says 1.7"));
    pdf.obj(5, FONT);
    pdf.finish_with_xref("")
}

fn multipage_inherit() -> Vec<u8> {
    let mut pdf = Pdf::new("1.4");
    pdf.obj(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.obj(
        2,
        "<</Type/Pages/Kids[3 0 R 4 0 R 5 0 R]/Count 3/MediaBox[0 0 595 842]/Resources<</Font<</F1 7 0 R>>>>>>",
    );
    pdf.obj(3, "<</Type/Page/Parent 2 0 R/Contents 6 0 R>>");
    pdf.obj(4, "<</Type/Page/Parent 2 0 R/Contents 6 0 R/Rotate 90>>");
    pdf.obj(
        5,
        "<</Type/Page/Parent 2 0 R/Contents 6 0 R/MediaBox[0 0 300 300]>>",
    );
    pdf.stream(6, "", &text_content("Inherited"));
    pdf.obj(7, FONT);
    pdf.finish_with_xref("")
}

fn xref_stream() -> Vec<u8> {
    let mut pdf = Pdf::new("1.5");
    let content = text_content("Object streams");
    pdf.stream(4, "", &content);
    pdf.obj(5, FONT);

    // Object stream 6 holds objects 1-3.
    let objects = [
        (1, "<</Type/Catalog/Pages 2 0 R>>"),
        (2, "<</Type/Pages/Kids[3 0 R]/Count 1>>"),
        (
            3,
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>",
        ),
    ];
    let mut index = String::new();
    let mut body = String::new();
    for (num, text) in objects {
        let _ = write!(index, "{num} {} ", body.len());
        body.push_str(text);
        body.push('\n');
    }
    let data = format!("{index}{body}");
    pdf.stream(
        6,
        &format!("/Type/ObjStm/N {}/First {}", objects.len(), index.len()),
        data.as_bytes(),
    );

    // Xref stream 7, uncompressed, W [1 4 2]: type, offset or objstm number, gen or index.
    let xref_offset = pdf.out.len();
    let mut rows = Vec::new();
    let mut row = |kind: u8, field2: u32, field3: u16| {
        rows.push(kind);
        rows.extend_from_slice(&field2.to_be_bytes());
        rows.extend_from_slice(&field3.to_be_bytes());
    };
    row(0, 0, 65535);
    for (i, _) in objects.iter().enumerate() {
        row(2, 6, u16::try_from(i).unwrap_or(0));
    }
    for num in [4, 5, 6] {
        row(1, u32::try_from(pdf.offsets[&num]).unwrap_or(0), 0);
    }
    row(1, u32::try_from(xref_offset).unwrap_or(0), 0);
    pdf.stream(7, "/Type/XRef/Size 8/W[1 4 2]/Root 1 0 R", &rows);
    pdf.line("startxref");
    pdf.line(&xref_offset.to_string());
    pdf.line("%%EOF");
    pdf.out
}

fn incremental_update() -> Vec<u8> {
    let mut pdf = hello(Pdf::new("1.4"), "Revision 1", false);
    let nums: Vec<u32> = (1..pdf.size()).collect();
    let first = pdf.xref(&nums, true);
    pdf.trailer("/Root 1 0 R", first);

    pdf.stream(4, "", &text_content("Revision 2"));
    pdf.obj(6, "<</Title(Revision 2)/Producer(nupdf fixtures)>>");
    let second = pdf.xref(&[4, 6], true);
    pdf.trailer(&format!("/Root 1 0 R/Info 6 0 R/Prev {first}"), second);
    pdf.out
}

fn garbage_prefix() -> Vec<u8> {
    let body = hello(Pdf::new("1.6"), "Junk first", false).finish_with_xref("");
    let mut out = b"Content-Type: application/pdf\r\nX-Junk: ".to_vec();
    out.resize(64, b'x');
    out.extend_from_slice(&body);
    out
}

fn no_xref() -> Vec<u8> {
    let mut pdf = hello(Pdf::new("1.4"), "No xref", false);
    pdf.line("trailer");
    pdf.line("<</Root 1 0 R/Size 6>>");
    pdf.line("%%EOF");
    pdf.out
}

fn truncated() -> Vec<u8> {
    let pdf = hello(Pdf::new("1.7"), "Truncated", false);
    let cut = pdf.offsets[&3] + 20;
    pdf.out[..cut].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_fixtures_match_generators() {
        let root = crate::repo_root();
        run(&root, true).unwrap();
    }

    #[test]
    #[ignore = "needs the oracle: cargo xtask oracle build"]
    fn repair_expectations_match_mupdf() {
        let root = crate::repo_root();
        let mutool = crate::oracle::mutool(&root).unwrap();
        for f in all() {
            let path = root.join("corpus/smoke").join(f.name);
            let run = crate::proc::run(
                &mutool,
                &[std::ffi::OsStr::new("info"), path.as_os_str()],
                std::time::Duration::from_secs(30),
            )
            .unwrap();
            let stderr = String::from_utf8_lossy(&run.stderr);
            let repaired = stderr.contains("repairing PDF document");
            assert_eq!(repaired, f.needs_repair, "{}: {stderr}", f.name);
        }
    }

    #[test]
    fn names_are_unique_pdfs() {
        let fixtures = all();
        let mut names: Vec<_> = fixtures.iter().map(|f| f.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), fixtures.len());
        assert!(names.iter().all(|n| n.ends_with(".pdf")));
    }

    #[test]
    fn recorded_offsets_point_at_objects() {
        let pdf = hello(Pdf::with_eol("1.4", "\r\n"), "x", true);
        for (num, &offset) in &pdf.offsets {
            assert!(pdf.out[offset..].starts_with(format!("{num} 0 obj").as_bytes()));
        }
    }

    #[test]
    fn xref_entries_are_twenty_bytes_for_every_eol() {
        for eol in ["\n", "\r", "\r\n"] {
            let mut pdf = hello(Pdf::with_eol("1.4", eol), "x", false);
            let start = pdf.xref(&[1, 2, 3, 4, 5], true);
            let table = &pdf.out[start..];
            let header_len = format!("xref{eol}0 6{eol}").len();
            assert_eq!(table.len(), header_len + 6 * 20, "eol {eol:?}");
        }
    }

    #[test]
    fn xref_stream_rows_resolve_to_objects() {
        let bytes = xref_stream();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("/Type/XRef"));
        assert!(text.contains("/Type/ObjStm/N 3"));
    }

    #[test]
    fn incremental_update_links_revisions() {
        let text = String::from_utf8_lossy(&incremental_update()).to_string();
        assert_eq!(text.matches("%%EOF").count(), 2);
        assert!(text.contains("/Prev "));
    }
}
