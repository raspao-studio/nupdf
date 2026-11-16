---
name: nupdf
description: Read, search, extract, render, compare, edit and redact PDF files with the nupdf CLI. Use whenever a task involves a PDF: getting its text or tables, finding or removing information, checking what changed between versions, splitting or merging, or rendering pages.
---

# nupdf

PDF toolkit for agents. `nupdf <command> <file> [options]` prints one JSON report (`schema`, `doc_id`, fields, `diagnostics`); errors are JSON on stderr. Exit: 0 ok, 2 usage, 3 bad input, 4 verification failed, 5 limit. `nupdf <command> --help` lists options.

- Triage & Forensics: `info <file>`; `attest <file> [--crops-dir dir]` checks pixel contrast vs stream text (occlusion, strikethrough, `ActualText` spoofs, blind spots); `history <file> [--strict]` time-travels `%%EOF` revisions & catches post-signature tampering.
- Read, Charts & Query: `extract <file> --format markdown` (or `text`; `--options reading-order`; `--format layout` for rows; `--format json` for ids `p3.b2.l4`, boxes, fonts; `--max-chars 8000` + `--cursor`); `chart <file>` decompiles vector charts & diagrams; `query <path> --right-of "Total" --nearest-only [--summary-only]`.
- Find & Audit: `search <file> <needle> [--options ignore-case,regexp]`; `tables <file> [--format csv]`; `audit <file> --claims claims.json [--evidence-dir dir]` verifies quotes, ids & hazards (0 tokens).
- Look & Navigate: `render <file> -o dir --pages 2 --dpi 72`, only when text cannot answer; `outline`, `links`, `labels`, `attachments`, `annots`, `forms`, `flat-forms`, `signatures`.
- Write (`-o out` or `--dry-run`): `patch <file> --replace p1.b2.l1.s1=NEW [--reflow] -o out` (exit 4 on collision/overflow); `apply <file> --plan a.json -o out`; `cloak`/`uncloak` reversible PII firewall; `redact`; `verify`; `merge`; `split`; `pages`; `meta`; `clean`, `encrypt`, `decrypt`.
- Untrusted & Pipelines: `ocr <file> -o out` (Tesseract); `sanitize <file> -o out`; `pipeline <file> --spec flow.json` runs multi-step workflows in 1 call.
- Annotate & Fill (`-o out`): `annots --add|--highlight|--delete`; `forms --set|--toggle`; `flat-forms --set|--check|--promote`; `flatten`.
- Scale: `diff a.pdf b.pdf`; `batch <inputs> -- <command> [args]`. 1-based pages (`N` last); points from top-left; `--password`, `--timeout S`.

## Commands
`info`, `objects`, `pages`, `meta`, `sanitize`, `flatten`, `ocr`, `split`, `merge`, `encrypt`, `decrypt`, `streams`, `operators`, `trace`, `stext`, `extract`, `search`, `redact`, `verify`, `render`, `tables`, `annots`, `forms`, `signatures`, `outline`, `links`, `labels`, `attachments`, `fonts`, `images`, `clean`, `show`, `diff`, `attest`, `patch`, `chart`, `flat-forms`, `history`, `cloak`, `uncloak`, `pipeline`, `apply`, `query`, `audit`, `batch`, `schema`, `guide`, `mcp`.
