# Changelog

The crates `nupdf` (the library) and `nupdf-cli` (the `nupdf` command) share a version. Report schemas are versioned on their own ([SCHEMA-CHANGELOG](docs/SCHEMA-CHANGELOG.md)). Until 1.0, only the latest release gets fixes ([SECURITY](SECURITY.md)).

## 0.1.0

The first public release: MuPDF 1.28.6's PDF engine ported to Rust, with no `unsafe` code, and a command-line and MCP interface built for agents. What it is measured to do, and where it falls short, is in [RESULTS](docs/RESULTS.md).

- **Reading**: syntax, cross-reference tables and streams, repair of damaged files, the page tree, every standard filter, encryption (RC4, AES-128, AES-256), content streams, fonts (Type 1, TrueType, CFF, Type 3, CID), images, colour spaces with ICC colour management.
- **Text**: structured text as MuPDF's device builds it, search, tables, Markdown, a layout format that keeps each row of a page on one line, an opt-in reading order, and OCR through Tesseract for scanned pages.
- **Rendering**: pages with their annotations and form fields, as `mutool draw` draws them, single-threaded or a band per thread.
- **Writing**: `clean`, `merge`, `split`, rearranging pages, metadata, incremental saves, encryption, flattening, sanitizing (JavaScript, actions, XFA, attachments), annotations and form filling.
- **Redaction** with a separate `verify` that re-reads the output and fails a file that still holds a target.
- **Signatures**: byte ranges and digests checked, RSA signatures verified.
- **For agents**: every command returns a versioned JSON report with a document id and stable diagnostic codes; `guide` (a short usage text), `schema`, `batch` over many files, `diff`, a Claude Code skill, and `nupdf mcp`, an MCP server whose tools take one file or many.
