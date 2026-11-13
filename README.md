# nupdf

A PDF engine for AI agents, in Rust. One binary reads, searches, renders, edits and redacts PDFs, and answers every command with a versioned JSON report a model can act on. It runs as a command-line tool, as an MCP server, or as a library.

- **Correct.** Checked file by file against MuPDF on 5,142 test files: 91 of 97 checks agree on at least 99% of them, the median check on 99.88%.
- **Safe.** No `unsafe` code, no panics on input, limits on every loop over a file's contents, fuzzed. 1.5 million runs over 100,000 PDFs from the open web, malware included, ended in no crash.
- **Fast.** Faster than MuPDF on most commands, and than PDF Oxide's library on every operation compared. A parsed document is shared across threads, so pages are worked on at once.
- **Built for agents.** Stable ids for every span of text, diagnostics as data, budgets and cursors for long files, files in batches, and a redaction that reads its own output back to prove nothing is left. In a 30-task agent eval, agents using nupdf solved every task in every run, and over MCP spent the fewest tokens of any tool.

The numbers, how to reproduce them, and where nupdf falls short: [RESULTS](docs/RESULTS.md).

## Install

```bash
cargo install --locked nupdf-cli
```

This installs the `nupdf` binary. It needs Rust 1.92 or newer. `ocr` also needs [Tesseract](https://github.com/tesseract-ocr/tesseract) on `PATH`; nothing else does.

## Use it

```bash
nupdf guide                                   # what every command does, on one page
nupdf info report.pdf                         # triage: pages, text layer, scans, forms, signatures, what to run next
nupdf extract report.pdf --format markdown    # the text, as Markdown
nupdf extract invoice.pdf --format layout     # each row of the page on one line: labels beside their values
nupdf search contract.pdf "terminat" --options ignore-case
nupdf render report.pdf -o pages --pages 1-3 --dpi 72
nupdf redact statement.pdf -o clean.pdf --preset email --preset phone
nupdf verify clean.pdf --preset email --preset phone
```

Every command prints one JSON report (`--pretty` to read it yourself) with a `schema`, a `doc_id` for the file and its `diagnostics`, and exits with a code that says what happened ([agent interface](docs/AGENT-INTERFACE.md)). There are commands for tables, fonts, images, annotations, forms, signatures, outlines, links, attachments, merging, splitting, rotating, cleaning, flattening, sanitizing, filling forms, OCR and comparing two files; `nupdf --help` lists them.

### With Claude Code or another agent

As an MCP server, one tool per command, each taking one file or many:

```bash
claude mcp add nupdf -- nupdf mcp
```

Or as a Claude Code skill, which runs the CLI from the agent's shell:

```bash
mkdir -p ~/.claude/skills/nupdf && nupdf guide --skill > ~/.claude/skills/nupdf/SKILL.md
```

### As a library

```toml
[dependencies]
nupdf = "0.1"
```

```rust,ignore
let doc = nupdf::Document::open(std::fs::read("report.pdf")?)?;
let pages: Vec<i32> = (0..doc.page_count().map_err(|e| e.0)?).collect();
for page in nupdf::parallel::extract(&doc, &pages, Default::default(), Some(4)) {
    print!("{}", page?.text());
}
```

The library's API may change in minor releases until 1.0; the CLI's JSON reports are the stable interface, each versioned on its own ([schema changelog](docs/SCHEMA-CHANGELOG.md)).

## Docs

The [docs site](https://raspao-studio.github.io/nupdf/) has a getting-started guide and everything below, built from `book/`.

| | |
|---|---|
| [RESULTS](docs/RESULTS.md) | What nupdf is measured to do: parity, safety, speed, agents, and where it falls short |
| [AGENT-INTERFACE](docs/AGENT-INTERFACE.md) | The JSON contract, addressing, exit codes, the redaction contract |
| [ARCHITECTURE](docs/ARCHITECTURE.md) | Layers, modules, data model, concurrency, safety |
| [TESTING](docs/TESTING.md) | The reference build, corpora, checks, benchmarks, fuzzing, the agent eval |
| [BENCHMARKS](docs/BENCHMARKS.md) | Speed against MuPDF and PDF Oxide, with the machine and versions |
| [DECISIONS](docs/DECISIONS.md) | Why things are the way they are |
| [PLAN](docs/PLAN.md) | Milestones, work items, exit criteria |
| [PRIOR-ART](docs/PRIOR-ART.md) | What else exists, and what is new here |
| [SECURITY](SECURITY.md) | Reporting a vulnerability; the [threat model](docs/THREAT-MODEL.md) |
| [CHANGELOG](CHANGELOG.md) | What each release changed |

Contributing, by hand or with an agent: [AGENTS.md](AGENTS.md) has the commands and the rules.

## License

AGPL-3.0-or-later. nupdf is derived from MuPDF by Artifex Software, Inc., and is an independent project, not affiliated with or endorsed by Artifex; see [NOTICE](NOTICE.md) for what it derives from and the third-party code it includes.
