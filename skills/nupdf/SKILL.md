---
name: nupdf
description: Read, search, extract, render, compare, edit and redact PDF files with the nupdf CLI. Use whenever a task involves a PDF: getting its text or tables, finding or removing information, checking what changed between versions, splitting or merging, or rendering pages.
---

# nupdf

A PDF toolkit for agents. `nupdf <command> <file> [options]` prints one JSON report on stdout: `schema` (`nupdf.<command>/v1`), `doc_id`, the command's fields, `diagnostics` (warnings as data: `code`, `severity`, `message`). Errors are JSON on stderr: `{"schema":"nupdf.error/v1","error":{"code","message"}}`. Exit codes: 0 ok, 1 bug, 2 usage, 3 bad input (unreadable, not a PDF, wrong password), 4 verification failed, 5 limit (`limit.timeout`, `limit.*`). `nupdf schema <command>` prints a report's JSON Schema; `nupdf guide <command>` a command's options.

## Start here

- `info <file>`: version, page count, encryption, repairs, revisions. Do this first.
- `extract <file>`: the text with content ids. `--format markdown` or `--format text` for reading; `--format json` (default) for ids, boxes, fonts. Budget it: `--max-chars 8000` stops at a whole block with `"truncated": true` and `next_cursor`; pass that to `--cursor` to go on. `--pages 3-5`, `--fields page,text` for less.
- `search <file> <needle>`: hits with page, quads, text and content ids. `--options ignore-case,ignore-diacritics,regexp`.

## Addressing

- Pages are 1-based. Ranges: `1-3,7`, `N` (last), `5-N`, `N-1` (reversed).
- Content ids: `p3` page, `p3.b2` block, `p3.b2.l4` line, `p3.b2.l4.s1` span, `p3.b2.l4.s1:5-12` characters. Stable for a `doc_id`; a cursor or id from another document is refused.
- Coordinates are points, origin top left, y down, rotation applied: what `render` draws.

## Changing documents

Writers never touch the input: `-o <out>` writes atomically, `--dry-run` reports the plan and writes nothing. Output is read back and checked before it is kept.

- `redact <file> -o <out>` with `--text`, `--regex`, `--preset pii|email|phone|card|ssn|iban`, `--region p3:[x0,y0,x1,y1]`, `--id`, or `--hits` (a `search` report). It removes the content, scrubs metadata, annotations, outline and the rest, rewrites the file whole, then searches it again: a target still found exits 4 and writes nothing. `--dry-run --preview DIR` renders what would go. `verify <file> --text ...` checks a file by itself.
- `clean`, `merge a.pdf b.pdf 1-3 -o out`, `split -o dir`, `pages --select|--delete|--rotate`, `meta --set Title=...`, `encrypt`, `decrypt`.

## More

- `render <file> -o dir [--dpi 72] [--pages]`: page-NNNN.png files.
- `diff <a> <b> [--by text,structure,pixels]`: what changed, lines with ids on both sides.
- `batch <files|dirs|globs> -- <command> [args]`: a JSON line per file; `-o run.jsonl --resume` takes a stopped run up again; `{stem}`, `{name}`, `{index}` name outputs.
- `fonts`, `images` (`--extract dir`), `stext` (MuPDF's structured text), `trace`, `operators`, `objects`, `show <ref>`, `streams`: lower level.
- `--threads N` on `stext`, `extract`, `search`, `render`; `--timeout SECONDS` on anything; `--password` for encrypted files.
- `nupdf mcp` serves every command as an MCP tool, arguments named as the options (`max_chars`).

## Commands
- `info <file>`: Identify a file and report what nupdf can tell about it.
- `objects <file>`: Dump every object in the cross-reference table as canonical JSON.
- `pages <file>`: List pages with their inherited MediaBox, CropBox, Rotate and Resources; or, with --select, --delete or --rotate, write the document with its pages changed. Options: --select, --delete, --rotate, --rotate-pages, --output, --dry-run, --no-verify, --incremental.
- `meta <file>`: Show the document information dictionary; or, with --set or --strip, write the document with it changed. Options: --set, --strip, --output, --dry-run, --no-verify, --incremental.
- `split <file>`: Write each range of pages to its own file in a directory, as `clean --pages` writes it: one page per file by default. Options: --output, --pages-per-file, --ranges, --dry-run, --no-verify.
- `merge <inputs>`: Merge the pages of several documents into a new one, with their external links and outline items, as `mutool merge` does. Options: --output, --dry-run, --no-verify, --incremental.
- `encrypt <file>`: Encrypt with a new security handler, as `clean -E`. Options: --method, --user-password, --owner-password, --permissions, --output, --dry-run, --no-verify, --incremental.
- `decrypt <file>`: Write without encryption, as `clean -D`. Options: --output, --dry-run, --no-verify, --incremental.
- `streams <file>`: Decode every stream: length and BLAKE3 of the data. Options: --images.
- `operators <file>`: List each page's content-stream operators with their operands, as MuPDF's interpreter hands them to a processor. Options: --no-resources.
- `trace <file>`: List each page's device calls, as MuPDF's run processor makes them for a page's contents.
- `stext <file>`: Each page's structured text, as MuPDF's stext device gathers it from the page's contents: blocks, lines and characters with their quads, origins, fonts and sizes. Options: --options, --threads.
- `extract <file>`: Extract the pages' text with content ids (p3, p3.b2, p3.b2.l4, p3.b2.l4.s1): as JSON with blocks, lines and spans and their boxes and fonts, as JSON lines (a page per line), as plain text (as `mutool draw -F txt` writes it) or as Markdown. Options: --format, --pages, --options, --max-chars, --max-items, --cursor, --fields, --threads.
- `search <file> <needle>`: Find a needle in the pages' text, as MuPDF searches: each hit with its quads, its text and the content ids of its characters. Options: --options, --stext-options, --pages, --max-items, --threads.
- `redact <file>`: Remove text, images and line art under targets, and prove it is gone: the targets' hits are found on each page, the pages are redacted as MuPDF's pdf_redact_page does, the document is rewritten whole, and the output is searched again before it is kept. Options: --text, --regex, --preset, --region, --id, --hits, --ignore-case, --stext-options, --pages, --boxes, --images, --line-art, --output, --dry-run, --preview, --preview-dpi, --no-verify.
- `verify <file>`: Check a document for the targets, class by class (content text, structure, annotations and forms, metadata, outline and names, embedded files, hidden layers, images, history, JavaScript). Options: --against, --text, --regex, --preset, --ignore-case.
- `render <file>`: Render pages as `mutool draw` paints them (page contents, without annotations), into page-NNNN.png (or .ppm) files. Options: --output, --pages, --dpi, --format, --max-pixels, --region, --threads, --no-icc.
- `fonts <file>`: List the fonts the pages use, as `mutool info` finds them, with what each loads as; --extract writes the embedded font programs as `mutool extract` does. Options: --extract.
- `images <file>`: List the images the pages use, as `mutool info` finds them; --extract writes every image XObject as `mutool extract` does. Options: --digests, --extract, --format.
- `clean <file>`: Rewrite the whole document as `mutool clean` does: every object in a plain xref table, object and xref streams unpacked, encryption kept. Options: --output, -g, -d, -z, -f, -i, -a, -e, -t, -Z, -c, -s, -D, -E, -O, -U, -P, --pages, --dry-run, --no-verify.
- `show <file> <target>`: Show one object (`12`, `12 0 R`) or the trailer (`trailer`). Options: --data, --max-bytes.
- `diff <a> <b>`: Compare two PDFs: their text (pages lined up, then lines, with content ids), their structure (pages, sizes, metadata, outline, fonts, annotations) and, on request, their pixels. Options: --by, --options, --dpi, --max-items.
- `batch <inputs> <command>`: Run a command on many files: each as `nupdf <command> <file> ...`, several at once, a JSON line per file in the order given, then the summary. Options: --jobs, --output, --resume, --item-timeout.
- `schema <name>`: The JSON Schema of a report: a command's (`extract`), or one of its parts (`extract.page`, `batch.item`, `error`); without a name, the names there are.
- `guide <command>`: How to use nupdf, written for models: conventions, where to start, and every command with its options. Options: --skill.
- `mcp`: Serve the commands as tools of a Model Context Protocol server, over stdin and stdout. Options: --call-timeout, --max-output-bytes.
