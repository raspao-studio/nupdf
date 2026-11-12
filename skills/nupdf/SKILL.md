---
name: nupdf
description: Read, search, extract, render, compare, edit and redact PDF files with the nupdf CLI. Use whenever a task involves a PDF: getting its text or tables, finding or removing information, checking what changed between versions, splitting or merging, or rendering pages.
---

# nupdf

PDF toolkit for agents. `nupdf <command> <file> [options]` prints one JSON report (`schema`, `doc_id`, fields, `diagnostics`); errors are JSON on stderr. Exit: 0 ok, 2 usage, 3 bad input, 4 verification failed, 5 limit. `nupdf <command> --help` lists a command's options.

- Triage: `info <file>`.
- Read: `extract <file> --format markdown` (or `text`); add `--options reading-order` when text comes out of order (columns, forms). `--format json` gives ids (`p3.b2.l4`), boxes and fonts. Long files: `--max-chars 8000`, then `--cursor <next_cursor>`; `--pages 3-5`.
- Find: `search <file> <needle> [--options ignore-case,regexp]`: page, quads, text, ids. Tables: `tables <file> [--format csv]`.
- Look: `render <file> -o dir --pages 2 --dpi 72`. Find your way: `outline` (bookmarks, with pages), `links`, `labels` (page labels), `attachments [--extract dir]`. Annotations and forms: `annots`, `forms`, `signatures` (digest checked).
- Write (input never changed; `-o out` or `--dry-run`): `redact <file> -o out --text|--regex|--preset pii|--region|--id` removes it everywhere and re-checks, exit 4 if anything is left; `verify <file> --text`; `merge a.pdf b.pdf 1-3 -o out`; `split <file> -o dir --ranges "1-3;4-N"`; `pages --select|--delete|--rotate`; `meta --set Title=...`; `clean`, `encrypt`, `decrypt`.
- Scanned pages (`info` lists `text.scanned_pages`): `ocr <file> -o out` (needs Tesseract). Untrusted files: `sanitize <file> -o out` removes JavaScript, launch and submit actions, XFA.
- Annotate and fill (`-o out`): `annots <file> --add '{"page":1,"type":"Highlight","quad_points":[[...]]}'`, `--highlight TEXT`, `--delete p1.a2`; `forms <file> --set name=value --toggle box`; `flatten` draws them into the page.
- Compare: `diff a.pdf b.pdf`. Many files: `batch <files|dirs|globs> -- <command> [args]`.
- Pages are 1-based (`N` is the last); coordinates are points, origin top left. `--password` for encrypted files, `--timeout S` on anything.

## Commands
`info`, `objects`, `pages`, `meta`, `sanitize`, `flatten`, `ocr`, `split`, `merge`, `encrypt`, `decrypt`, `streams`, `operators`, `trace`, `stext`, `extract`, `search`, `redact`, `verify`, `render`, `tables`, `annots`, `forms`, `signatures`, `outline`, `links`, `labels`, `attachments`, `fonts`, `images`, `clean`, `show`, `diff`, `batch`, `schema`, `guide`, `mcp`.
