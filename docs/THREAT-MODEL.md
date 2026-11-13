# Threat model

What nupdf defends against, how, and where the defence stops.

## Who uses it, on what

nupdf is a command-line tool, an MCP server and a Rust library that agents and their tools run on PDF files they did not make: mail attachments, web downloads, scans, forms, contracts. The person or agent running it trusts nupdf; it does not trust the file. The file's author may be hostile, and may know nupdf.

## What a hostile file may try

| Attack | Defence | Checked by |
|---|---|---|
| Memory corruption through a parser bug | No `unsafe` in the product crates (`unsafe_code = "forbid"`); bounds-checked slices; dependencies pure Rust where they parse (zlib-rs, hayro-jpeg2000, moxcms; the JBIG2 decoder is carried in the crate) | `cargo deny`, review of new dependencies (AGENTS.md) |
| A panic that kills the agent's tool | No `unwrap`, `expect` or `panic!` on paths that touch input; the CLI and MCP server catch what escapes (`internal.panic`, exit 1) and the MCP server keeps serving | 19 fuzz targets, each run on every change in CI (`fuzz/`), the wild-tier crash sweep (`cargo xtask crashes`) |
| A decompression bomb, or a huge allocation from a small number in the file | `Limits`: decoded bytes per stream, xref entries, object-stream members; image and pixmap sizes checked before allocating; `--max-pixels` for rendering | fuzz targets with RSS limits; smoke fixtures for each bomb kind |
| A hang: cycles in page trees, outlines, name trees, structure trees, field parents; endless content loops | Every walk over input is bounded (depths, visited sets, step counts) where MuPDF would loop or recurse without end; `--timeout` (`nupdf::interrupt`) stops anything after a deadline, checked per operator and per page | fuzz timeouts in CI; regression files for each one found (e.g. a structure tree whose parents form a cycle) |
| A stack overflow from deep nesting | Recursion over input is bounded; parsing of nested arrays and dictionaries is iterative or depth-limited | fuzz targets on `parse_object`, `lex`, `content` |
| Hiding text from `redact` (invisible text, text in forms, in annotations, in fonts' ToUnicode, in metadata, in incremental updates) | `redact` runs with clipping off and all layers on, removes text, images, line art, annotations and metadata by policy, writes a full rewrite (old revisions gone), and **reads its output back and searches it again**: anything left is exit 4 | the redaction contract (docs/AGENT-INTERFACE.md); the redaction tier's leak scoring |
| Running code in the viewer the cleaned file goes to (JavaScript, Launch, SubmitForm, GoToR, XFA) | `sanitize` removes them wherever they hang and scans the output again before writing it (exit 4 if anything is left) | CLI tests; `sanitize` on the wild tier in the crash sweep |
| Faking a valid signature | `signatures` hashes the byte ranges itself, checks the gaps between them hold only the signature, compares the signed digest and verifies RSA signatures; "OK" needs all of it | the `signatures` harness check against MuPDF built with OpenSSL; tampered-file tests |
| Making nupdf touch other files | Commands read the files they are given and write only to `-o` (atomically) or into a directory given; names from the file (attachments, fonts) are made safe before use and never leave that directory | CLI tests (`attachment_names_cannot_leave_the_directory`) |
| Making nupdf run programs | Only `ocr` runs one: the Tesseract named by `--tesseract` (default `tesseract` on the PATH), on images nupdf drew, with a time limit | `ocr`'s tests |
| Prompt injection through the file's text | Out of scope for nupdf itself: it reports what a file says, as data, in JSON. The agent decides what to do with it. The agent eval includes a hostile fixture to see how agents fare | `eval/hostile` |

## What nupdf does not defend against

- **A hostile environment.** Whoever can change nupdf's binary, its PATH (for `ocr`) or the files while it runs is trusted.
- **Denial of service by size alone.** A 10 GB file takes time to read. The limits bound the work per byte, not the bytes.
- **Rendering differences as an attack.** A file can look different in nupdf (and MuPDF) than in Acrobat. nupdf follows MuPDF; it does not try to match every viewer.
- **Certificate trust.** A signature can be valid and its certificate untrusted; nupdf does not check trust.
- **Side channels.** Nothing in nupdf handles secrets an attacker could time; the RSA code verifies public signatures only.

## Keeping it true

- Every new parser surface gets a fuzz target in the same change, and CI runs each target on every pull request.
- Each crash, hang or leak found becomes a regression test with the file that showed it.
- The wild tier (100,000 files from Common Crawl, not sanitized) is swept for crashes before a release: `cargo xtask crashes --tier wild`.
- New dependencies must be pure Rust on the `deny.toml` licence allow-list, and pass `cargo deny`'s advisories.
