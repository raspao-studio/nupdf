# Report schema changelog

Every report names its schema, `nupdf.<command>/v<N>`, and `nupdf schema <command>` prints that schema as JSON Schema. This file records each version.

## What a version promises

Within a version, a report keeps every field it has, with the same type and meaning. These changes keep the version:

- A new field, in a report or in anything it contains. Readers ignore fields they do not know; the schemas allow them.
- A new value of a code (`diagnostics[].code`, `error.code`), or a new warning. Codes are stable once given: one is never reused for something else.
- A field that was always present becoming present in more cases, or one that could be null becoming null less often.
- Different text in `message` fields, which are for people.

These change the version:

- A field removed or renamed, or given another type (including becoming nullable).
- A value given another meaning: units, coordinate space, numbering (pages are 1-based; content ids are `p3.b2.l4.s1`).
- An exit code given another meaning.

The version of each report changes on its own: a breaking change to `extract` makes `nupdf.extract/v2` and leaves `nupdf.info/v1` as it is.

## v1 (2026-11-12)

The first frozen version: 27 schemas, read off what the commands print (DECISIONS D45). Every report from v0 keeps its shape; only the version in `schema` changed.

`batch`, `batch.item`, `clean`, `decrypt`, `diff`, `encrypt`, `error`, `extract`, `extract.page`, `fonts`, `images`, `info`, `merge`, `meta`, `objects`, `operators`, `pages`, `redact`, `render`, `schema`, `search`, `show`, `split`, `stext`, `streams`, `trace`, `verify`.

## Additions to v1 (2026-11-12)

New reports, at v1: `annots`, `annots-edit`, `attachments`, `fill`, `flatten`, `forms`, `labels`, `links`, `ocr`, `outline`, `sanitize`, `signatures`, `tables`. Added fields: `info` has `text` (pages with text, scanned pages); a written document's `output` has `signatures` when the input was signed.

## Additions to v1 (2026-11-14)

New reports, at v1: `apply`, `attest`, `audit`, `patch`, `query` (DECISIONS D59), and `chart`, `cloak`, `flat_forms`, `history`, `pipeline`, `uncloak` (DECISIONS D60).

## v0

Reports before the schemas were frozen, while their shapes could still change.
