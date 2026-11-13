# First commands

Every command has the form `nupdf <command> <file> [options]` and prints one JSON report: its `schema` (`nupdf.info/v1`), the file's `doc_id` (a hash of its bytes), the command's fields, and `diagnostics`, the problems nupdf found in the file and worked around. Errors are JSON too, on stderr. The exit code says what happened: 0 ok, 2 usage, 3 bad input, 4 verification failed, 5 a limit was hit. `--pretty` indents the JSON for people; agents do not need it. The [agent interface](docs/AGENT-INTERFACE.md) has the whole contract.

The examples use `invoice.pdf`; any PDF will do.

## The guide

```bash
nupdf guide
```

A page of plain text written for models: the conventions above, which command to reach for at each step of a task, and every command by name. An agent reads it once at the start of a task. `nupdf guide extract` gives one command's options in full, as does `nupdf extract --help`.

## Triage

```bash
nupdf info invoice.pdf --pretty
```

The facts an agent decides with before reading: page count, PDF version, encryption and permissions, whether the file was repaired, its metadata, and which pages have text and which look scanned (`text.scanned_pages`, the ones that need `ocr`).

## Read

```bash
nupdf extract invoice.pdf --format markdown
nupdf extract invoice.pdf --format layout
```

`markdown` (or `text`) is the text in reading order. `layout` keeps each line where it sits on the page, a row per line, so a value stays beside its label: the format for invoices, forms and tables, where position carries meaning. Text formats go to stdout and the report to stderr.

`--format json` gives every block, line and span with its id (`p3.b2.l4`), box and font. For long files, `--max-chars 8000` stops at a budget and the report's `next_cursor` continues from there (`--cursor <next_cursor>`); `--pages 3-5` reads only some pages.

## Find

```bash
nupdf search invoice.pdf "total due" --options ignore-case
```

Each hit has its page, the text matched, the quads it covers in page space (points, origin top left) and the ids of the spans it falls in, which other commands take. `--options regexp` searches by regular expression.

## Look

```bash
nupdf render invoice.pdf -o pages --pages 1 --dpi 72
```

Writes `pages/page-0001.png` and reports where each image went and its size. An image costs a model far more than text does, so render only what text cannot answer: a chart, a signature, a layout to check.

## Redact, then verify

```bash
nupdf redact invoice.pdf -o redacted.pdf --preset pii
nupdf verify redacted.pdf --text "jane@example.com"
```

`redact` removes what it is given (`--text`, `--regex`, `--preset email|phone|card|ssn|iban|pii`, `--region`, or `--id` from a search) everywhere it appears: page text, images and line art under it, annotations, form fields, metadata, bookmarks, attachments and earlier revisions. Then it reads its output back and searches it again. Its report counts each target's hits before and after; if anything is left it exits 4. The input is never changed.

`verify` checks any file, redacted by nupdf or not, for a text (`--text`, `--regex`) across all of those places, and exits 4 if it finds it. `verify --against` takes a `redact` report and checks its targets and areas. The [redaction contract](docs/AGENT-INTERFACE.md#redaction-contract) says exactly what is checked.

## Everything else

`nupdf guide` lists all the commands: splitting, merging and rearranging pages, metadata, forms and annotations, outlines and links, tables, OCR for scanned pages, `sanitize` for untrusted files, `diff` between versions, and `batch` to run any command on many files at once.
