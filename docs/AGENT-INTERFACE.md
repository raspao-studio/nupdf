# Agent interface

The contract that agents program against. The CLI and the MCP server (M8) expose the same commands with the same JSON. Library functions return the typed reports these documents describe. Schemas are versioned: they stay at `v0`, and may change, until M8 freezes `v1`. After that, breaking changes bump the version in `schema`.

## Rules every command follows

1. **JSON on stdout.** One JSON object per invocation (`--format jsonl` for per-page streams). `--pretty` indents it. Human formats are opt-in: `--format text` or `--format markdown` where they make sense.
2. **Self-describing.** Every report has `schema` (`"nupdf.<command>/v<N>"`) and the input's `doc_id`.
3. **Diagnostics are data.** Every report has a `diagnostics` array of `{code, severity, message, page?, object?, offset?}`. A degraded result says so here instead of failing silently.
4. **Errors are JSON on stderr.** `{"schema": "nupdf.error/v0", "error": {"code", "message", "hint"?}}`, and nothing on stdout.
5. **Exit codes mean one thing each.** See the table below.
6. **Inputs are never modified.** Mutating commands need `-o <path>` (or an explicit `--in-place`), write atomically, and report the output's `doc_id`, size and verification block.
7. **Every mutation has `--dry-run`.** It returns the plan, writes nothing, and uses the same schema with `"applied": false`.
8. **Budgets are explicit.** `--max-chars`, `--max-items`, `--fields <list>`, and pagination through `next_cursor`. Large outputs are truncated deliberately, with `"truncated": true` and a cursor, never cut off mid-object.
9. **Limits are explicit.** `--timeout`, `--max-memory` and the decompression limits have safe defaults. Hitting one is an error with a `limit.*` code, not a hang.
10. **Deterministic.** Same file, same arguments, same nupdf version: byte-identical output.
11. **Discoverable.** `nupdf guide` prints compact usage written for models. `nupdf schema <command>` prints the JSON Schema of a report.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success. Warnings, if any, are in `diagnostics`. |
| 1 | Internal error: a bug in nupdf. Please report it. |
| 2 | Usage error: bad arguments. |
| 3 | Input error: unreadable, not a PDF, wrong password, damaged beyond repair. |
| 4 | Verification failed: a mutation's self-check found a problem, such as redacted text still present. |
| 5 | Limit exceeded: time, memory, decompression or size. |

## Addressing content

- **Pages** are 1-based. Selections use MuPDF's syntax: `1-3,7`, `N` for the last page, `5-N`, `N-1` for reverse order.
- **Coordinates** are in points, in MuPDF's page space: origin at the top left, y pointing down, page rotation applied. This matches what `render` draws, so a bbox read from `extract` can be overlaid on a rendered image without conversion.
- **Content ids** are stable for a given `doc_id` and schema version: `p3` (page), `p3.b2` (block), `p3.b2.l4` (line), `p3.b2.l4.s1` (span), and `p3.b2.l4.s1:5-12` (a character range within a span). Search hits refer to these ids, and redaction accepts them. An id used with a different `doc_id` is rejected, so agents find out the file changed.
- **Regions** are `p3:[x0,y0,x1,y1]` in the coordinate space above.

## Commands by milestone

| Command | What it does | Milestone |
|---|---|---|
| `info` | Triage: version, pages, encryption, repairs, revisions, text-layer presence per page, scanned-page detection, fonts at risk (no ToUnicode), forms, JavaScript, attachments, signatures, plus suggested next commands | M0 (header), extended every milestone |
| `objects`, `show <ref>` | Canonical JSON dump of objects | M1 |
| `pages` | Page objects with inherited MediaBox, CropBox, Rotate and Resources | M1 |
| `streams` | List streams, decode one | M2 |
| `decrypt`, `encrypt` | Remove or add encryption | M2, M3 |
| `merge`, `split`, `pages`, `meta`, `clean` | Structural editing | M3 |
| `trace`, `fonts`, `images` | Device-call trace, font inventory, image list and extraction | M4 |
| `extract` | Text as JSON (ids, bboxes, fonts), plain, Markdown or JSONL | M5 |
| `search` | Literal or regex hits with quads and ids | M5 |
| `redact`, `verify` | Remove content and prove it is gone | M6 |
| `render` | PNG of a page or region, with a pixel budget | M7 |
| `mcp`, `batch`, `diff`, `guide`, `schema` | Agent surface | M8 |
| `forms`, `annots`, `ocr`, `tables`, `attachments`, `outline`, `sanitize`, `signatures` | Coverage | M9 |

## Examples

`nupdf info` today (M0):

```json
{"schema":"nupdf.info/v0","doc_id":"b3:0b7651d98660496320e5c86a9bab07fc","file_size":619,"kind":"pdf","pdf_version":"1.6",
 "header_offset":64,"diagnostics":[{"code":"header.garbage_prefix","severity":"warning","message":"64 bytes before the %PDF- marker"}]}
```

`nupdf extract --pages 3` (M5 target), abbreviated:

```json
{"schema":"nupdf.extract/v1","doc_id":"b3:...","pages":[{"page":3,"size":[612,792],"blocks":[
  {"id":"p3.b0","kind":"text","bbox":[72,60,540,88],"lines":[
    {"id":"p3.b0.l0","bbox":[72,60,540,88],"spans":[
      {"id":"p3.b0.l0.s0","text":"Invoice 2026-114","bbox":[72,60,301,88],"font":"Helvetica-Bold","size":24,"color":"#000000"}]}]}]}],
 "diagnostics":[],"truncated":false}
```

`nupdf search --regex '\d{3}-\d{2}-\d{4}'` (M5 target) returns hits shaped like `{"id":"h0","page":2,"text":"123-45-6789","quads":[[...]],"spans":["p2.b4.l1.s0:10-21"]}`.

## Redaction contract

Redaction is the command where a silent failure does the most damage, so its contract is stricter than the rest.

**Inputs.** `--text`, `--regex`, `--preset pii|email|phone|card|ssn|iban`, `--region`, `--id`, or `--hits hits.json` (output of `search`). Text matching normalizes ligatures and whitespace and crosses line breaks and hyphenation.

**Apply.** Removes the glyphs, image pixels and vector content inside each target's quads. Removes or flattens annotations, widgets and links in those areas. Draws overlay boxes only if asked. Then scrubs document-wide copies of the targets, and always does a full rewrite, so no previous revision survives.

**Verify** is a separate pass over the output, which `redact` always runs and which can be run alone (`nupdf verify --against targets.json`). It checks these leak classes:

| Leak class | Check |
|---|---|
| Content-stream text | Target text absent from extraction, including invisible text (render mode 3), text under images, white-on-white and tiny text |
| ActualText, Alt, structure tree | Absent from every structure element and marked-content property |
| Annotations and form fields | Absent from contents, appearance streams and field values |
| Metadata | Absent from Info and XMP |
| Outline, named destinations, page labels | Absent from titles and names |
| Embedded files | Absent from names and descriptions, and attachments removed when they contain targets |
| Hidden optional content | Absent from content hidden by OCGs |
| Images | Pixels in redacted regions blanked |
| History | The output has exactly one revision, and no orphan objects contain targets |

The report lists, for each target, the hits before, the hits after (which must be 0) and the result of each class, plus `"verified": true|false`. If verification fails, `redact` exits 4 and does not leave the output at `-o`.
