# Changelog

The crates `nupdf` (the library) and `nupdf-cli` (the `nupdf` command) share a version. Report schemas are versioned on their own ([SCHEMA-CHANGELOG](docs/SCHEMA-CHANGELOG.md)). Until 1.0, only the latest release gets fixes ([SECURITY](SECURITY.md)).

## Unreleased

- **A predefined CMap is decoded when a font first asks for it**, not all 71 at the first font that uses one: about 6 ms off every call that reads text or draws a page of a document with such a font, most of what a call on a small file took.
- **`merge` writes a page's links onto it once**: a page of thousands of links cost the square of them. Merging `long-008.pdf` with itself goes from 2.95 s to 0.49 s.
- **`batch` writes each file's line in one write**, where the output got a write for every token of its JSON: on 400 small files, 0.91 s to 0.53 s.
- **Long documents run on threads without `--threads`**: `extract`, `stext`, `search` and `tables` on 32 pages or more, and `render` on any number, take up to eight threads; a budget (`--max-chars`, `--max-items`) still reads one page after another. Text, ids and pixels are unchanged; a long document's diagnostics now come sorted and once each, as with `--threads` ([D53](docs/DECISIONS.md)). On the 30 long documents `extract` goes from 1.42 s to 0.66 s and `render --format ppm` from 9.2 s to 5.5 s.
- **`render --threads` no longer changes pixels from run to run** on a document whose pages depend on the pages before them (a Type 3 font that borrows its resources from the page that first uses it): such a document's pages are drawn in order.
- **`render` writes its PNGs at zlib level 3**: the same pixels, written in about a third of the time into files a seventh larger ([D54](docs/DECISIONS.md)). With the threads above, `render` on the 30 long documents goes from 16.8 s to 6.4 s. `Pixmap::png_at` takes the level; `Pixmap::png` and `images --extract` keep MuPDF's.
- **`info` stops reading a page at its first shown string**, as its scan for scanned pages was meant to: a page that is one long text object was read whole. On a page of ten million strings `info` goes from 991 MB to 75 MB. Its copy of the document shares the file's bytes and keeps the limits given.
- **`search --max-items` on a long document runs growing windows of pages on threads**, so a needle whose first hit is far in is found as fast as an uncapped one.
- **`clean` with a stream option checks its output without decoding the streams it kept**: `clean -z` on the 30 long documents goes from 4.2 s to 1.05 s.

## 0.1.1

No change to the engine or to any report: this release adds ways to get the `nupdf` command without building it.

- **Prebuilt binaries** for Linux (x86-64 and ARM, for glibc and as static musl builds), macOS (Apple silicon and Intel) and Windows (x86-64), attached to each GitHub release with SHA-256 checksums. `cargo binstall nupdf-cli` fetches them.
- **PyPI**: `pip install nupdf-cli`, or `uvx --from nupdf-cli nupdf`.
- **npm**: `npx nupdf`, or `npm install -g nupdf`.
- The docs moved to https://nupdf.dev/docs/, behind a landing page at https://nupdf.dev/.

## 0.1.0

The first public release: a PDF engine for agents in Rust, with no `unsafe` code, a command-line tool, an MCP server and a library, checked against MuPDF 1.28.6. What it is measured to do, and where it falls short, is in [RESULTS](docs/RESULTS.md).

- **Reading**: syntax, cross-reference tables and streams, repair of damaged files, the page tree, every standard filter, encryption (RC4, AES-128, AES-256), content streams, fonts (Type 1, TrueType, CFF, Type 3, CID), images, colour spaces with ICC colour management.
- **Text**: structured text as MuPDF's device builds it, search, tables, Markdown, a layout format that keeps each row of a page on one line, an opt-in reading order, and OCR through Tesseract for scanned pages.
- **Rendering**: pages with their annotations and form fields, as `mutool draw` draws them, single-threaded or a band per thread.
- **Writing**: `clean`, `merge`, `split`, rearranging pages, metadata, incremental saves, encryption, flattening, sanitizing (JavaScript, actions, XFA, attachments), annotations and form filling.
- **Redaction** with a separate `verify` that re-reads the output and fails a file that still holds a target.
- **Signatures**: byte ranges and digests checked, RSA signatures verified.
- **For agents**: every command returns a versioned JSON report with a document id and stable diagnostic codes; `guide` (a short usage text), `schema`, `batch` over many files, `diff`, a Claude Code skill, and `nupdf mcp`, an MCP server whose tools take one file or many.
