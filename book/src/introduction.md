# nupdf

nupdf is a PDF engine for AI agents: a port to Rust of the PDF engine in [MuPDF](https://mupdf.com), with a command-line tool, an MCP server and a library on top, all built to be called by a model rather than read by a person.

- **Faithful.** MuPDF is the reference. Each subsystem is checked file by file against a pinned MuPDF build, and parity is ratcheted in CI, so a file reads in nupdf as it reads in MuPDF.
- **Safe.** No `unsafe` code, no panics on input, explicit limits on every loop over a file's contents, and continuous fuzzing. A hostile file can fail to open; it cannot crash or hang the agent that opened it.
- **Parallel.** A parsed document is shared across threads, so its pages are processed at once.
- **Agent-first.** Every command prints one versioned JSON report with stable ids for each span of text, diagnostics as data instead of log lines, budgets and cursors for long files, and a redaction that reads its own output back to prove nothing is left.

## Who it is for

- **Agents and the people who build them.** An agent that gets a PDF (an invoice, a contract, a scanned form, a bundle of statements) can triage it, read it in reading order or by position, find text and get its boxes, render a page only when text cannot answer, fill forms, compare versions, and redact with a check that the redaction worked. [Claude Code and MCP](claude-code.md) shows how to wire it in.
- **Scripts and pipelines** that want MuPDF's reading of a file as JSON, with exit codes that mean something (see the [agent interface](docs/AGENT-INTERFACE.md)).
- **Rust programs** that want a memory-safe PDF reader and writer: see [as a library](library.md).

## Where to go next

- [Install](install.md), then [first commands](first-commands.md).
- [Results](docs/RESULTS.md): what nupdf is measured to do, and where it falls short.
- [Agent interface](docs/AGENT-INTERFACE.md): the JSON contract every command follows.
- [Architecture](docs/ARCHITECTURE.md) and [decisions](docs/DECISIONS.md): how it is built and why.
- [Security](SECURITY.md): how to report a vulnerability, and the [threat model](docs/THREAT-MODEL.md).

## License

nupdf is licensed under the GNU Affero General Public License, version 3 or later (AGPL-3.0-or-later). It follows MuPDF's design, algorithms and behavior closely enough to be a derivative work of MuPDF, by Artifex Software, Inc., and is distributed under MuPDF's license. It is an independent project, not affiliated with or endorsed by Artifex. The [NOTICE](https://github.com/raspao-studio/nupdf/blob/main/NOTICE.md) lists what nupdf derives from and the third-party code and data it includes, with their licenses.
