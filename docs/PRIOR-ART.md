# Prior art

Surveyed on 2026-11-09. Marks: **V** verified on a live repository, crates.io or docs.rs page that day; **C** the project's own claim, not tested; **I** our inference. Star counts and dates come from the GitHub API on that day and will drift.

## Has anyone rewritten MuPDF (or PDFium, poppler, qpdf) in Rust?

Not in a usable form.

| Project | What it is | Status |
|---|---|---|
| [mupdf-rust-wip](https://github.com/PoignardAzur/mupdf-rust-wip) | "Experimental port of MuPDF to Rust", AGPL-3.0 | V: an empty stub, one push in 2022 |
| [micropdf](https://docs.rs/crate/micropdf/latest) | Claims to be pure Rust with full API compatibility with MuPDF's C headers, MIT/Apache | V: 0.17.0 (2026-04), docs.rs build failing, ~670 downloads. I: a permissively licensed MuPDF API clone raises provenance questions |
| [rpdfium](https://docs.rs/crate/rpdfium/latest) | "Faithful Rust port of PDFium", 10 sub-crates, Apache-2.0 | V: releases only between 2026-02 and 2026-03; the GitHub repository now returns 404 |
| [pdfspine](https://github.com/VoldemortGin/pdfspine) | Clean-room reimplementation of the PyMuPDF API in Rust | V: 7 stars, active. C: 703 of 769 PyMuPDF symbols |
| qpdf, poppler | Only bindings exist (`qpdf`, `poppler` crates), as with `mupdf-rs` and `pdfium-render` | V |

We found no C-to-Rust translation research aimed specifically at a PDF library.

## Pure-Rust PDF engines

| Crate | License | Parse | Repair | Decrypt | Write | Text | Redact | Render |
|---|---|---|---|---|---|---|---|---|
| [hayro](https://github.com/LaurenzV/hayro) | MIT/Apache | yes | best effort (V) | RC4, AES (V, marked unaudited) | no (read-only) | no API | no | yes, via vello_cpu; C: 1,600-file visual regression suite |
| [pdf](https://github.com/pdf-rs/pdf) | MIT | yes | undocumented | undocumented | experimental | example only | no | separate crate |
| [lopdf](https://github.com/J-F-Liu/lopdf) | MIT | yes | undocumented | partial | yes, the most-used editor | basic | no | no |
| [pdf-extract](https://github.com/jrmuizel/pdf-extract) | MIT | through lopdf | no | no | no | yes; I: mediocre | no | no |
| [oxidize-pdf](https://github.com/bzsanti/oxidizePdf) | MIT | C: 99.3% of 9k files | C: lenient | C: R2–R6 | generate, merge, split | yes | not mentioned | no |
| [pdf_oxide](https://github.com/yfedoseev/pdf_oxide) | MIT/Apache | yes | unclear | yes | yes, with incremental save | C: 99.5% parity with PyMuPDF | yes, refuses Type0 fonts | partial; optional pdfium and lcms2 |
| [krilla](https://github.com/LaurenzV/krilla), [pdf-writer](https://github.com/typst/pdf-writer) | MIT/Apache | no | no | no | creation only | no | no | no |
| [printpdf](https://github.com/fschutt/printpdf) | MIT | experimental | no | no | generation | experimental | no | to SVG, experimental |

Younger 2026 projects (claims unverified): zpdf (repair, decryption, redaction, rendering; opened 426 of 618 files in a malformed corpus), PDFPurr, pdfplumber-rs, stet.

hayro is the quality leader among pure-Rust readers. Typst depends on it, which accounts for roughly 5M downloads (V).

## Agent-oriented PDF tools

| Tool | Engine | Interface | Redaction |
|---|---|---|---|
| [pymupdf4llm-mcp](https://artifex.com/blog/pymupdf4llm-mcp-enhancing-llm-agents-with-high-quality-pdf-context) (Artifex) | MuPDF | MCP, Markdown output | none |
| [liteparse](https://github.com/run-llama/liteparse) | PDFium | CLI with JSON, bboxes, screenshots, an agent skill | none |
| [pdf-inspector](https://github.com/firecrawl/pdf-inspector) | lopdf | library; detects scanned vs text PDFs | none |
| [xberg](https://github.com/xberg-io/xberg) | PDFium (I) | CLI, REST, MCP | masks PII in extracted text only (V) |
| pdf_oxide | its own | CLI with `--json`, MCP | [real removal](https://pdf.oxide.fyi/docs/editing/redaction) with a report, but no independent re-check |
| [ferrules](https://github.com/AmineDiro/ferrules) | PDFium plus a layout model | CLI, HTTP | none |
| [omitly-mcp](https://github.com/omitly/omitly-mcp) | proprietary, on qpdf | MCP | `check_redaction` scans for leaks; closed source and paid |
| [redact-pdf-mcp](https://github.com/dambuchs/redact-pdf-mcp) | rasterizes pages | MCP | drops the text layer by rasterizing |
| [pdf-redact](https://docs.rs/crate/pdf-redact/1.0.0-beta.3) | lopdf fork (I) | library | real removal, beta, no verification |

The best external yardstick is [pdf-redaction-benchmarks](https://github.com/RedactionTools/pdf-redaction-benchmarks) (AGPL-3.0), which scores leaks across content streams, annotations, hidden layers, previous revisions, metadata, images and OCR. It is the `redaction` corpus tier.

We found no open-source tool that removes content and then independently proves it is gone. pdfspine claims "verified content removal", which we did not examine.

## Building blocks we plan to reuse

All permissive and AGPL-compatible. See ARCHITECTURE's dependency table: zlib-rs, hayro-jpeg2000, hayro-jbig2, skrifa/read-fonts, subsetter, moxcms, vello_cpu or tiny-skia, and RustCrypto. ocrs (Latin script only, "early preview") is the optional pure-Rust OCR. ttf-parser and qcms are effectively unmaintained; fontations and moxcms replace them.

## Corpora

The PDF Association keeps a catalog at [pdf-corpora](https://github.com/pdf-association/pdf-corpora). The sources nupdf uses, with sizes and licenses, are in `corpus/manifest.toml`. The [Arlington PDF Model](https://github.com/pdf-association/arlington-pdf-model) (Apache-2.0) is a machine-readable grammar of every PDF object, and it is how M3 validates the writer's output.

## Implications

**What is new about nupdf:**

1. No maintained, MuPDF-faithful Rust engine exists. The permissively licensed "ports" have unclear provenance and look dormant. Being openly AGPL and derived is what lets nupdf read MuPDF's source legitimately.
2. No mature pure-Rust engine combines interpretation, editing and writing. hayro does not write, lopdf does not interpret content, and pdf_oxide and zpdf are young.
3. No open-source tool offers redaction with machine-checkable proof that the content is gone.
4. We found no tool that gives text spans stable ids. This is a negative search result, not proof that none exists.

**What to watch:**

- [PDFluent SDK](https://github.com/pdfluent/pdfluent-sdk): Rust, AGPL-3.0-only or commercial, created 2026-07, built on hayro and vello_cpu. It covers rendering, extraction, forms, PDF/A, signatures and redaction. It is the closest match to nupdf's scope and licensing.
- pdf_oxide: the strongest agent-side competitor (JSON CLI, MCP, real redaction), permissively licensed.
- hayro: the reference for robustness and architecture in pure Rust, and nupdf's second renderer for triage.
- pymupdf4llm-mcp: the incumbent that agents use today.
