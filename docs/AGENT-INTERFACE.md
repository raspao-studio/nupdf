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

## Encrypted documents

Every command takes `--password`, tried as both the user and the owner password. Without it, nupdf tries the empty password, as MuPDF does, and opens the file either way: `info` reports `"needs_password": true` and leaves `page_count` null when the empty password does not work, and objects then decrypt to the same unreadable bytes MuPDF shows. A password that opens neither way fails with exit 3 and `input.wrong_password`. A command that writes refuses a document the empty password does not open, unless `--password` is given, with exit 3 and `input.needs_password`, as `mutool clean` refuses it. `info.permissions` says what the document allows (`print`, `edit`, `copy`, `annotate`, `form`, `accessibility`, `assemble`, `print-hq`): everything, unless it is encrypted and was not opened with the owner password. `info.encryption` is MuPDF's description, such as `"Standard V5 R6 256-bit AES"`, or `"None"`. A security handler nupdf does not support (anything but `/Standard`) fails to open with `input.encryption`.

## Writing documents

`clean <file> -o <out>` rewrites the whole document as `mutool clean` does: every object in number order in a classic xref table, object streams and xref streams unpacked, streams copied raw, the encryption and `/ID` kept. The bytes are MuPDF's, except for the comment line naming the writer and the AES initialization vectors (DECISIONS D18). `--dry-run` does everything but write; `-o` or `--dry-run` is required.

`-g` collects garbage as `mutool clean -g` does, and repeating it goes further (DECISIONS D19). `-g` writes only what the trailer reaches and turns references to nothing into null. `-gg` also renumbers the objects from 1, `-ggg` also merges duplicate objects, and `-gggg` also merges streams with the same data. The report's `garbage` gives the level, 0 to 4.

The stream options are `mutool clean`'s (DECISIONS D20). `-d` writes streams decoded, apart from XML metadata and JPEG 2000 images. `-z` compresses streams that have no filter: with zlib's deflate, or for a 1-bit image with CCITT G4, after taking off any ASCIIHexDecode. `-f` and `-i` do the same for fonts and images only, and keep them from being decoded by `-d`. `-a` hex-encodes binary streams, and writes strings outside printable ASCII in hex. `-e N` sets the compression effort: 0 for zlib's default level, or 1 to 100 for levels 0 to 9. `-zz` (Brotli) is refused with exit 2 and `usage.unsupported`. Objects are written compact, or with `-a` or `-d` and no `-z`, a dictionary entry per line; `-t` forces compact and `-tt` a line per entry. The report's `streams` gives the options and `syntax` gives `compact` or `pretty`.

`-Z` packs objects into object streams and writes an xref stream instead of the table, as `mutool clean -Z` does, raising the version to 1.5 (DECISIONS D21). As in MuPDF, it fails with `input.unloadable_object` on a document with an object that does not load or, in most files, an object number the xref skips, even where `clean` without `-Z` writes it. The report's `objstms` says whether it was given.

`--pages 1-3,5` keeps only those pages, in that order, as `mutool clean`'s page arguments do: `N` is the last page, a range can run backwards (`5-1`), and a page can repeat (DECISIONS D23). The catalog is rebuilt with only the page tree, outlines, optional content and the named destinations that still lead somewhere, and links, form fields and outline items that lead to dropped pages are removed. The report's `pages` lists the pages kept, from 1. A page tree that cannot be walked fails with `input.page_tree`.

`-D` writes the document without encryption. `-E rc4-40|rc4-128|aes-128|aes-256` encrypts it with a new security handler, with `-O` and `-U` for the owner and user passwords (empty by default; with no user password the output opens without one) and `-P` for the permission flags (-1, everything, by default), as `mutool clean` does (DECISIONS D22). As in MuPDF, `-E` gives the document a new `/ID`; nupdf derives it, and every other value MuPDF draws at random, from the input, so the output is reproducible. `-D` and `-E` cannot be combined. The report's `encryption` is `"keep"`, `"none"`, or the new method and permissions. The read-back check opens the output with the new user password. It refuses output where MuPDF's encryption loses a stream: an empty stream under AES, or a stream with a crypt filter of its own.

Before writing, nupdf opens the output (with `--password`, if given) and compares it with the input: the syntax of each object, apart from the stream `/Length` it recomputes, and each stream's raw data. With a stream option, streams are compared decoded instead, and their `/Filter` and `/DecodeParms` are left out. With `-Z`, objects in object streams are compared too, the object streams and xref stream the writer adds are not, and the catalog's `/Version` is left out. Without `-g`, every object the output has in use is compared with the input's of the same number. With `-g`, objects are paired by following references from the trailer, since they may be renumbered or merged, and `objects` counts the pairs. If the output does not open, needs a repair, or reads back differently, `clean` writes nothing and fails with exit 4 and `verify.failed`, naming the input's objects. MuPDF's writer is lossy in a few cases, such as an empty key name before a number, a stream under an Identity crypt filter, a reference renumbered in a dictionary of more than 100 entries, with `-gg` a document whose trailer has no `/Root`, or, with `-z` or `-a`, a filter array two streams share. `--no-verify` skips the check and writes MuPDF's output anyway.

```json
{"schema":"nupdf.clean/v0","doc_id":"b3:...","applied":true,"garbage":0,
 "streams":{"decompress":false,"compress":false,"compress_fonts":false,
  "compress_images":false,"ascii":false,"effort":0},"syntax":"compact","objstms":false,"pages":null,"encryption":"keep",
 "output":{"path":"out.pdf","doc_id":"b3:...","len":1574,"blake3":"..."},
 "verification":{"verified":true,"objects":7},"diagnostics":[]}
```

`verification` is null with `--no-verify`. An object that cannot be loaded while writing fails with `input.unloadable_object`; an output path that cannot be written fails with exit 3 and `output.unwritable`. The output is written to a temporary file beside it and renamed into place.

## Editing documents

Each editing command makes one edit, then writes the document as `clean` writes it with no options: `-o` or `--dry-run` is required, `--no-verify` skips the read-back check, and the report carries `applied`, `output` and `verification` as `clean`'s does (DECISIONS D24). With `--incremental`, `meta` and `pages` append the edit to the file as a new revision instead, leaving its bytes as they were, as MuPDF's incremental save does (DECISIONS D27); a repaired file, and anything `merge` makes, cannot be appended to, which fails with exit 2 and `usage.incremental`.

`meta <file>` reports the document information dictionary as text, and `xmp`, whether the catalog has XMP `/Metadata`. `--set KEY=VALUE` (repeatable) sets an entry as MuPDF's `pdf_set_metadata` does: `info:Title` and `Title` are the same key, and the value is written as a text string. `--strip` removes `/Info` and the catalog's `/Metadata` first. The report shows the entries as written and `stripped`.

```json
{"schema":"nupdf.meta/v0","doc_id":"b3:...","applied":true,"info":{"Title":"Été"},"xmp":false,"stripped":false,
 "output":{...},"verification":{"verified":true,"objects":7},"diagnostics":[]}
```

`pages <file>` lists the pages; with an edit it writes them changed. `--select 3,1-2` keeps those pages in that order and `--delete 2` drops those, both as `clean --pages`; `--rotate 90` turns pages clockwise by a multiple of 90, all of them or those `--rotate-pages` names, counted after the selection. The report's `kept` lists the pages kept, from 1, `rotated` the degrees and `page_count` the count after.

`split <file> -o <dir>` writes one file per page, or per `--pages-per-file N` pages, or per page list in `--ranges "1-3;4-N"`, each as `clean --pages` would. The parts are `<stem>-1.pdf` onwards, zero-padded to the same width. The report's `parts` give each part's `pages`, `output` and `verification`; `--dry-run` writes nothing, not even the directory.

`merge <inputs>... -o <out>` copies pages into a new document, as `mutool merge` does (DECISIONS D25). Each input file may be followed by its pages, in the same syntax (`a.pdf b.pdf 3-1 c.pdf N`); without them, all of its pages are copied. Objects two pages of one input share are copied once. The report's `doc_id` is the first input's that opened, and `inputs` gives each input's `path`, `doc_id`, the `pages` merged from 1, `error`, and the `diagnostics` from reading it. An input that fails partway, such as one whose page refers past its xref, keeps the pages merged before it, as in MuPDF; its `error` says why, and a `merge.input_failed` warning is in the report's `diagnostics`. A locked input fails that way without `--password`. Only when no input opens does the command fail. A page's links to other files and the web come with it, recreated as new Link annotations; links within the document do not, as in MuPDF. Outline items that lead to a merged page are appended to the outline, with their ancestors, renumbered to the merged pages (DECISIONS D26).

```json
{"schema":"nupdf.merge/v0","doc_id":"b3:...","applied":true,
 "inputs":[{"path":"a.pdf","doc_id":"b3:...","pages":[1,2],"error":null,"diagnostics":[]}],
 "page_count":2,"output":{...},"verification":{"verified":true,"objects":9},"diagnostics":[]}
```

`encrypt <file>` is `clean -E`: `--method` (`aes-256` by default), `--owner-password`, `--user-password` and `--permissions`. `decrypt <file>` is `clean -D`. Their reports (`nupdf.encrypt/v0`, `nupdf.decrypt/v0`) carry `encryption` as `clean`'s does.

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
| `streams` | Every stream's decoded length and BLAKE3 hash (image filters listed, not decoded; `--images` decodes only those); `show <ref> --data` decodes one | M2 |
| `decrypt`, `encrypt` | Remove or add encryption | M2, M3 |
| `merge`, `split`, `pages`, `meta`, `clean` | Structural editing | M3 |
| `operators` | Each page's content-stream operators with their operands, as MuPDF's interpreter dispatches them; `--no-resources` processes the content streams alone | M4 |
| `trace` | Each page's device calls, as MuPDF's run processor makes them for the page's contents: paths with their matrices, colors and stroke states, clips, images, shadings, text (spans with their font, matrix, writing mode and language, and each glyph's id, Unicode and position), groups, masks, layers, structure and metatext | M4 |
| `fonts` | The fonts the pages use, as `mutool info` finds them (page, object, subtype, name, encoding), with what each loads as (`load`: program, glyph count, embedded or substituted, ToUnicode, vertical); `--extract DIR` writes each embedded program as `mutool extract` does (`font-NNNN.ext`) | M4 |
| `images` | The images the pages use, as `mutool info` finds them (page, object, size, depth, filters, colorspace); `--digests` decodes every image XObject as MuPDF does and gives its pixmap's size, components, colorspace and MD5; `--extract DIR` writes each as `mutool extract` does (`image-NNNN.jpg`, `.png` or `.pam`; `--format pam` for all PAM) | M4 |
| `stext` | Each page's structured text as MuPDF's stext device gathers it from the page's contents: blocks (text; images with `preserve-images`; with `vectors`, the boxes of filled and stroked paths as `{"type": "vector", "bbox", "flags"}`, flags stroked 1, rectangle 2, continues 4), lines with their box, writing mode and direction, and characters as `[text, origin, font, size, quad, flags, bidi]` (quads as `[ul, ur, ll, lr]`, flags as MuPDF's `FZ_STEXT_*` character flags); `--options` takes MuPDF's stext option string (`preserve-ligatures`, `dehyphenate`, `clip=no`, `clip-rect=x0:y0:x1:y1`, ...); an unknown option fails with exit 2 and `usage.bad_option`. `stext`, `extract` and `search` take `--threads N`: pages run on N threads with output identical at every N (D37) | M5 |
| `extract` | The pages' text with content ids: `--format json` (pages, blocks, lines and spans, with boxes, fonts, sizes, bold, italic and colors; `--fields` picks levels and keys, text going at the deepest level kept, `chars` adding each character's quad), `jsonl` (a `nupdf.extract.page/v0` object per page, then the report), `text` (as `mutool draw -F txt` writes it: MuPDF's `fz_print_stext_page_as_text`, a form feed after each page) or `markdown` (headings, list items, bold and italic, a `<!-- p3 -->` comment per page); for text and Markdown the report goes to stderr. `--pages` selects pages, `--options` takes the stext options, `--max-chars` and `--max-items` stop at a whole block with `"truncated": true` and a `next_cursor` (`p3.b5@<doc_id>`) that `--cursor` resumes from; a cursor for another document fails with exit 2 | M5 |
| `search` | Hits of a needle as MuPDF's search finds them (`fz_match_stext_page_cb`): `--options` takes MuPDF's search options (`ignore-case`, `ignore-diacritics`, `regexp`, `keep-lines`, `keep-paragraphs`, `keep-hyphens`), `--stext-options` the stext options. Each hit has its page, its quads, its text and the content ids of its characters: `id` (`p3.b2.l4.s1:5-12`) when it lies in one span, otherwise `begin` and `end` (`p3.b2.l4.s1:5`). A pattern that does not compile is a page error, as in MuPDF. `--pages` and `--max-items` as for `extract` | M5 |
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
