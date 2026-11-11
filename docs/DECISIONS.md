# Decisions

Each decision records what was chosen and why. Newer decisions can supersede older ones; when that happens the old entry stays and is marked as superseded.

## D1. License: AGPL-3.0-or-later

*2026-11-09*

nupdf follows MuPDF's design, algorithms and edge-case behavior closely, and its authors read MuPDF's source while writing it. That makes it a derivative work, and MuPDF is AGPL-3.0-or-later, so nupdf is too. Calling it a clean-room implementation would be false, and choosing AGPL lets us read MuPDF openly.

What this means:

- Anyone can use, modify and redistribute nupdf under the AGPL. Running it as a network service obliges you to offer the source to its users.
- Programs that call the `nupdf` CLI or MCP server as a separate process are generally not bound by the AGPL. Programs that link the `nupdf` crate are.
- A permissive or commercial relicense would need Artifex's agreement, because they hold the copyright on what we derive from. We would have to ask them, and that is fine.
- Files that follow a MuPDF file name it and carry Artifex's copyright line (see NOTICE).

## D2. A rewrite, not a port and not a wrapper

*2026-11-09. Supersedes the earlier idea of shipping an agent CLI over MuPDF's C library first.*

Memory safety and real parallelism are architectural properties. A thin layer over MuPDF's C would keep MuPDF's memory model and its per-thread contexts. A line-by-line translation (c2rust or similar) would reproduce `fz_try`/`fz_catch`, manual reference counting and context threading in unsafe Rust. So nupdf is written as idiomatic Rust that uses MuPDF as the behavioral reference and as the source of algorithms, and contains no C.

## D3. The oracle is a pinned MuPDF, used out of process

*2026-11-09*

MuPDF 1.28.6 is built from source by `cargo xtask oracle build` and driven through `mutool run` (its JavaScript API), `mutool trace` and `mutool draw`.

- Out of process means MuPDF crashes and hangs become classified outcomes, nothing C is linked into the workspace, and the same binary serves both the harness and the benchmarks.
- Pinned, built from source, and version-checked on every use means results are reproducible and baselines are meaningful.
- The JavaScript API reaches almost everything the C API does, so new checks are short scripts in `oracle/js/`.

## D4. One library crate until a split pays for itself

*2026-11-09*

Starting with many crates would freeze boundaries before we know them. `nupdf` stays a single crate with modules. A split happens when it buys something measurable, such as compile time or keeping heavy optional dependencies (rendering, OCR) out of the core. This is revisited at M4 and M7.

## D5. Default behavior matches MuPDF; improvements are opt-in modes

*2026-11-09*

The harness is only meaningful if the default output is supposed to equal MuPDF's. Where nupdf can do better (reading order, stricter validation, recovering files MuPDF rejects), the better behavior ships as a named mode, measured against ground truth rather than against MuPDF. Bugs in MuPDF that we choose not to reproduce are listed, with the file that shows them, in the check's documentation.

## D6. A JSON contract from day one

*2026-11-09*

Every command returns a versioned report (`schema`), with `doc_id`, diagnostics and stable exit codes, from its first version. Schemas are `v0` and may change until M8 freezes `v1`. The contract is in AGENT-INTERFACE.md.

## D7. Pure-Rust dependencies, licenses enforced by cargo-deny

*2026-11-09*

Dependencies must be pure Rust (they may contain audited `unsafe` internally) with AGPL-compatible licenses. External tools such as Tesseract run as subprocesses, never linked. For crypto, nupdf uses RustCrypto rather than hayro's unaudited implementation, because it must also write encrypted files.

## D8. BLAKE3 for document identity

*2026-11-09*

`doc_id` is the first 128 bits of the file's BLAKE3 hash, written `b3:<32 hex>`. BLAKE3 is fast enough to hash large files on every call, and the prefix makes the algorithm explicit if it ever changes.

## D9. Name: nupdf

*2026-11-09*

ν (nu) is the Greek letter after μ (mu). The name was free on crates.io and GitHub. It does not use the MuPDF trademark.

## D10. Reals are f32, parsed by MuPDF's own algorithm

*2026-11-09. Settles PLAN open question 1.*

MuPDF stores reals as `float`, and its `fz_strtof` is not correctly rounded: it keeps 9 significant digits and uses Coonen's Algorithm D with a 32-bit significand. Parsing with Rust's correctly rounded `f32::from_str` made dozens of standard-tier files differ by one ulp. nupdf ports `strtof.c` exactly (`crates/nupdf/src/strtof.rs`), including `fz_atof`'s rule that an underflow returns 1.0. Geometry code may still compute in f64 internally, as long as what it reports matches MuPDF.

## D11. Dictionaries keep insertion order, with replace-in-place

*2026-11-09. Settles PLAN open question 2 for now.*

A `Vec` of key/value pairs with linear lookup, where re-inserting a key replaces its value in place (`pdf_dict_put`'s behavior, so the last duplicate wins). Real dictionaries are small. MuPDF sorts dictionaries over 100 entries; nupdf will add an index if profiles show large dictionaries matter. Revised by D18: the order is what gets written, so nupdf sorts them too.

## D12. Small filters move from M2 into M1

*2026-11-09*

Cross-reference streams and object streams in the standard corpus use ASCIIHex as well as Flate, and the parity work for M1 could not be measured without decoding them. ASCIIHex, ASCII85, RunLength, Flate and the predictors are therefore part of M1. LZW, Brotli, the image decoders and the security handler stay in M2.

## D13. How the harness compares numbers

*2026-11-09*

Integers compare exactly. Non-integers compare at f32 precision, because MuPDF's values are floats and mujs prints only 16 significant digits. Integers beyond 2^53 compare to within 1e-15 relative, for the same reason. Oracle scripts avoid lossy MuPDF JavaScript accessors (`asNumber` squeezes integers through a 32-bit int and a float) and print names in MuPDF's `#xx`-escaped form, so the JSON stays valid when names contain arbitrary bytes.

## D14. Xref storage follows the file, not the declared size

*2026-11-09*

MuPDF allocates a 40-byte entry for every slot a section declares, in every section. A few hundred bytes can declare 8M objects (a large object number, or an xref stream with zero-width fields), and each `/Prev` can declare them again. That is a memory amplification any hostile file can trigger, so nupdf does not copy it.

Each section stores what it read as runs: listed entries one by one, and long stretches of identical entries once. The effective table is an array up to 1M slots, or when listed entries are at least a quarter of it. Above that it is a set of uniform ranges plus a map. Free and unset slots are computed, not cached, and `objects` writes its report while it loads, so its memory does not grow with the report.

Behavior stays MuPDF's (D5). The object count, the entries and the errors are the same. MuPDF checks entries in the order of its in-memory subsection list, which its merging rules shape: a subsection inside another is ignored, one that starts inside or right after another extends it, one that touches two makes the section a single subsection, and so does an xref stream whose own number lies outside its `/Index`. That order decides which error a broken file reports, so nupdf models the list on number ranges instead of allocating it. The rules are tested against MuPDF's output on constructed files.

## D15. Encryption moves from M2 into M1

*2026-11-09*

Encrypted files were almost all of what kept M1 from its exit bar: 68 of the 69 `info` mismatches on the standard tier, most of `objects` and a third of `warnings`. Agents also meet them constantly, usually with only an owner password set, which every reader opens silently. So the standard security handler (revisions 2 to 6, RC4 and AES, crypt filters) is part of M1. LZW, Brotli and the image decoders stay in M2.

nupdf copies MuPDF's handling, including two behaviors that look like accidents. The file key is state that each authentication attempt overwrites, so a document opened without its password is decrypted with the key from the last failed attempt, and reports the same unreadable strings MuPDF does. And MuPDF decrypts AES as a stream: a bad final block ends the data where a reader reaches it, so Flate usually fails first on a wrong key, and a whole-stream read keeps the 256-byte chunks it got before the error. Both decide what agents see in a file they cannot open, and both are tested against MuPDF.

Passwords are given with `--password`. A wrong one is an input error (exit 3, `input.wrong_password`); no password is not an error, since MuPDF opens such files too, and `info` reports `needs_password`.

## D16. Streams are read through a port of MuPDF's stream model

*2026-11-10*

What a damaged stream yields in MuPDF is decided by its stream machinery rather than by any one filter. Each stage fills a buffer of a fixed size (4096 bytes for Flate, LZW, Brotli and predictors, 256 for ASCIIHex, ASCII85, RunLength, RC4 and AES). A stage that fails loses the buffer it was filling, and the reader above treats the failure as the end of the data after reporting it. The whole-stream reader asks for at most what is left in its own growing buffer. A stage can also mark itself finished while still holding bytes, which `fz_available` then drops and `fz_read_byte` does not. M1 approximated these rules case by case (the AES chunks, the endstream window). M2 ports the model itself (`stream`) with each filter as a stage, so they hold for every filter and every combination.

LZW is a port of MuPDF's decoder rather than a library: its tolerance (a missing clear code, a single out-of-range code) and its reading past the end of a truncated stream are part of what it returns. Brotli uses brotli-decompressor, a port of the same reference decoder MuPDF links. One difference is deliberate: on a truncated Brotli stream MuPDF loops forever waiting for input, and nupdf ends the data instead.


## D17. JPEG is a port of libjpeg; JBIG2 uses hayro-jbig2 with jbig2dec's refusals

*2026-11-10*

The plan named zune-jpeg for DCTDecode. Measured against MuPDF it was close but not exact: 95.8% of files matched within the old tolerance, and 115 JPEGs differed by up to a dozen levels. jpeg-decoder did no better (95.9%). The cause is libjpeg itself. Since version 7 it upsamples subsampled chroma inside the inverse DCT (a 2x2-subsampled block goes through a 16x16 transform), where other decoders upsample afterwards; its integer IDCT also clamps the DC term and rounds in its own way. So nupdf ports libjpeg 10's decompressor, the copy MuPDF bundles (`jpeg`, NOTICE). The port is limited to how MuPDF drives libjpeg: default parameters, full scale, one scanline at a time, a source that never suspends. That still covers baseline, extended and progressive Huffman and arithmetic coding, restart resynchronization, block smoothing of incomplete progressive images, and libjpeg's handling of truncated data. Every DCT stream in the standard tier now matches MuPDF byte for byte, so the `images` check compares exactly rather than within a level.

CCITTFax is a port of MuPDF's `filter-fax.c`, which also matches exactly.

JBIG2 uses hayro-jbig2. Porting jbig2dec (17,000 lines) was not needed: on valid streams the two decoders agree. Where they do not, jbig2dec is usually the one refusing. It leaves parts of the standard unimplemented (intermediate generic regions, 12 adaptive template pixels, color extensions, reused coding contexts) and fails the whole stream on them, so MuPDF yields no image. nupdf scans the segment headers for those parts first, then fails the same way and reports jbig2dec's messages. It also accepts a JBIG2 file header in a PDF stream, as jbig2dec does. 18 files still differ, all of them test files for the corner cases:

- 14 pdf.js refinement tests, where jbig2dec mis-decodes Huffman-coded text regions.
- 3 where jbig2dec's XNOR composition sets the padding bits past the end of each row.
- `bomb_giant.pdf`, whose 100,000 x 100,000 page MuPDF allocates before its compression bomb check stops it, and nupdf refuses.

JPX passes through undecoded in `pdf_load_stream`, as in MuPDF; decoding it belongs to image loading (M4).

The fuzzer found two loops in hayro-jbig2's symbol dictionary decoding that never end on damaged data, because its arithmetic decoder keeps returning values past the end: endless empty export runs, and endless empty height classes. jbig2dec stops the first after 1000 empty runs. nupdf vendors hayro-jbig2 0.4.0 with that guard and the same one for height classes (`vendor/hayro-jbig2/NUPDF.md`), until upstream has an equivalent. Decoding time also follows the symbol sizes a dictionary declares, and a 600-byte stream can declare tens of thousands of large symbols: MuPDF took more than five minutes on one the fuzzer found. The vendored copy caps a dictionary at 2^28 pixels of symbols, far more than real dictionaries hold.

## D18. The writer matches `mutool clean` byte for byte

*2026-11-10*

`nupdf clean` writes what `pdf_save_document` writes with default options, and the `clean` check compares the two files' BLAKE3 hashes. Matching bytes needs MuPDF's printer exactly: `/Type` and `/Subtype` first in tight mode, a space only between two regular characters, its choice between literal and hex strings, `#XX` names cut at a NUL, and reals through `fmtfloat` with its port of Grisu2 (`print`). It also needs these behaviors of MuPDF's model:

- **Dictionary order.** A put into a dictionary of more than 100 entries sorts it by key, and later keys go into their sorted place (`pdf_dict_get_put`); a delete moves the last entry into the gap and unsorts it. This revises D11, since the order is what gets written. Lookups take one of two paths. A name MuPDF interns as a constant (608 of them, `names`) goes through `pdf_dict_find`, which in a sorted dictionary reports the last key as absent when it is the key sought. Any other name goes through `pdf_dict_finds`, which does not. So a sorted dictionary whose last key is `/Type` does not get `/Type` moved to the front.
- **Null is absent.** MuPDF's null object is the null pointer, so a null `/Type`, `/ID` or `/Info` counts as missing wherever it checks for an object.
- **Reals that are integers.** MuPDF writes a real as an integer when `(int)f` converts back to it, a conversion C leaves undefined outside the int range. x86 gives INT_MIN there and ARM saturates, so MuPDF writes 2147483648.0 as `2147483700` on x86 and as `2147483647` on ARM. nupdf follows x86, which loses nothing, and the fixtures avoid that one value.
- **Generations are 16 bits.** Xref generations are `unsigned short`, so 65536 reads as 0, and the free list then counts up from what was read.
- **Objects that cannot be loaded.** Object stream members that parse as null, or fail to load, are written as free entries, with MuPDF's generations.

MuPDF uses random AES initialization vectors. nupdf derives each one from the key, the object and the data (the first 16 bytes of SHA-256 over them), so its output is deterministic (AGENT-INTERFACE rule 10). AES-encrypted files therefore differ from MuPDF's in those 16 bytes per string and stream, and the `clean` check counts them as mismatches. The comment after the header names nupdf instead of MuPDF (same length; the harness swaps it before hashing).

The `roundtrip` check covers what bytes cannot. Each side writes its file, MuPDF reads each one back, and the harness compares the object graph, the raw stream data, whether a repair was needed, and MuPDF's warnings. Its remaining mismatches also come from random IVs, where MuPDF's own output does not read back exactly: AES streams of a document opened without its password, which MuPDF writes empty with a 32-byte `/Length`, and streams under an Identity crypt filter, which it encrypts anyway. On `issue20453.pdf`, MuPDF's save fails after a repair because it parses an object stream's number as a file offset; nupdf writes the file.

`clean` then reads its own output back and refuses to write one that differs from the input (exit 4), as AGENT-INTERFACE requires of mutations. That rejects some of MuPDF's lossy cases (an empty key name before a number, Identity crypt filters, documents opened without their password: 27 standard files). `--no-verify` writes MuPDF's output regardless, and the harness uses it.


## D19. Garbage collection keeps MuPDF's quirks, and its read-back follows references

*2026-11-10*

`clean -g` through `-gggg` run MuPDF's `do_garbage` on the writer's copy of the objects (`garbage`), and the `clean-g` to `clean-gggg` checks compare the output's BLAKE3 with MuPDF's at each level. Matching it means doing what MuPDF does, including where that loses data:

- **Marking.** Only what the trailer reaches is written. A reference to a missing, free or null object becomes null where it appears, and so does a reference to a reference to nothing, because `pdf_is_null` follows the chain. An indirect stream `/Length` becomes its number first, so the object that held it is dropped.
- **Duplicates** (`-ggg`) are found by zlib's CRC-32 over each object as MuPDF holds it in memory: null, true, false and interned names hash as their small pointer values, integers as 64 bits, reals as 32. Objects with the same hash are then compared with `pdf_objcmp`. Two dictionaries with the same entries in another order hash differently, so they are not merged, although the comparison would call them equal. Pages are never merged, and streams only when `-gggg` compares their data. Merging repeats until nothing changes, since renumbering can make more objects equal.
- **Renumbering** (`-gg`) gives every object a new number from 1 and generation 0, and encrypts it again under the new number. Each changed reference goes back into its dictionary with `pdf_dict_put`, by key. A dictionary of 101 entries is sorted by that put, so its key order changes, and an entry the sort moves past the loop's position is renumbered a second time, ending up pointing at another object. In a sorted dictionary whose last key is an interned name, the put appends a duplicate key with the new number and leaves the old one in place (D18's lookup quirk).
- **Truncation.** When nothing is reachable, because the trailer has no `/Root`, `-gg` writes an empty xref that neither tool can open.

On the standard tier all four levels match MuPDF on every file except the 33 AES files `clean` differs on (random IVs, D18). Collecting also lets both tools write 37 files that fail without it, because the objects that cannot be loaded are ones nothing reaches. `roundtrip-gggg` covers the AES files, as `roundtrip` does for `clean`.

With garbage collection, objects can be renumbered or merged, so `clean`'s read-back check cannot compare them by number. It walks both documents from the trailer's `/Root`, `/Info`, `/ID` and `/Encrypt`, pairing objects where the same reference leads, and fails if an object pairs two ways or reads back differently. Merged objects may share a partner, and a reference that reads as null in the input may be null in the output. The check refuses the renumbering quirks above and the empty outputs: 20 standard files at `-g` (all also refused without `-g`) and 53 at `-gggg`, 33 of them empty.

## D20. Stream options follow `copystream` and `expandstream`, with zlib's own deflate

*2026-11-10*

`clean -d`, `-z`, `-f`, `-i`, `-a` and `-e` decide each stream as MuPDF's `writeobject` does, and the `clean-d`, `clean-z`, `clean-ze`, `clean-a`, `clean-dfi` and `clean-dzgg` checks compare the output's BLAKE3 with MuPDF's. Byte-identical output needs MuPDF's encoders as well as its decisions:

- **Deflate is a port of zlib 1.3.2** (`deflate`), the copy MuPDF bundles, reduced to the one `compress2` call MuPDF makes. zlib-rs, which nupdf decodes with, is a port of zlib-ng, whose match finder differs: its output differed from MuPDF's at most levels. The port matched MuPDF's zlib on 52 inputs (empty to several megabytes of text, binary, random and PDF data) at every level before any PDF was written with it. `-e` maps effort 1 to 100 onto levels 0 to 9 as MuPDF does, so efforts 1 to 11 store the data uncompressed.
- **1-bit images are CCITT G4 encoded** with a port of MuPDF's encoder (`fax_encode`), when the data is exactly the image's rows. MuPDF compares `/Subtype` with `/Image` by pointer, so an image whose `/Subtype` is a reference is deflated instead; the port keeps that.
- **Filter arrays are edited in place.** Taking `ASCIIHexDecode` off for `-z`, or putting it on for `-a`, changes the filter array, and an array reached through a reference is shared: every later stream that uses it, and the array object when it is written, sees the change. MuPDF writes streams whose filters no longer match their data this way, and so does nupdf; the read-back check then refuses the output. An array left with one filter becomes that filter, and a `/DecodeParms` that is not an array becomes null. An empty `/DecodeParms` array fails the whole save, in both tools.
- **Lookups miss what MuPDF's miss.** A sorted dictionary whose last key is `/Filter` reads as unfiltered (D18's lookup quirk), so `-z` compresses its data again and appends a second `/Filter`.
- **Brotli (`-zz`) is refused.** MuPDF marks it experimental, and matching it would need a port of Google's encoder.

`mutool clean` turns on a line per dictionary entry for `-a` and `-d` without `-z`; the write options MuPDF's script interface takes do not, so the checks pass `pretty` to the oracle explicitly.

With `-d` after `-gg`, MuPDF decodes each stream through its renumbered dictionary; nupdf decodes it through the input's, which leads to the same objects except where renumbering's own quirks (D19) point a reference elsewhere.

On the standard tier the checks match MuPDF on every file except AES ones (random IVs, D18) and, where images are decoded, the files whose image decoding already differs in the `images` check (JBIG2 refinement and halftone regions, mostly). On `bomb_giant.pdf`, jbig2dec decodes a 100000 by 100000 page until MuPDF's compression-bomb check fails the save; nupdf's JBIG2 decoder refuses the page and writes the stream empty.

The read-back check compares decoded data when any stream option is given, and leaves `/Filter` and `/DecodeParms` out of the dictionary comparison, since changing them is the point. Data that went through the fax encoder is compared without the bits that pad each row to a byte: the encoder never reads them, so they come back white.

## D21. Object streams are gathered as `gather_to_objstms` gathers them

*2026-11-10*

`clean -Z` packs objects into object streams and ends the file with an xref stream instead of a table, and the `clean-Z`, `clean-Zg` and `clean-Zggz` checks compare the output's BLAKE3 with MuPDF's. What gets packed, and where the save fails, follows MuPDF's loop, not the PDF specification:

- **What is packed.** Every object whose xref entry is in use with generation 0, in number order, 256 at most to a stream, printed compact with nothing between them. Left out are streams, the trailer's `/Root` and `/Info`, the encryption dictionary, bare integers (they may be a `/Length`) and bare references. The test is the xref entry's generation, which renumbering moves with the object, so after `-gg` an object that had generation 1 is still left out although it is written with 0.
- **Unused objects are packed too.** At `-g`, an object nothing reaches still goes into an object stream; it is just listed as free, with its index as offset and generation.
- **Loading comes first.** Gathering loads each entry before looking at it. An object that does not load fails the save, so `-Z` fails on files `clean` writes. So does a number no xref section defines, when MuPDF's entry lookup falls back to the newest section: always for a file with one section, and below the oldest section's size for a file with several. 22 standard files fail this way with `-Z`, in both tools.
- **Object streams from the input are written** as ordinary streams, since only `-Z` keeps them. One that failed to load has a free entry from then on, so it is neither written nor packed.
- **The version becomes 1.5** when it is lower, and the catalog's `/Version`, if it has one, is rewritten.
- **The xref stream** is a new object after every other one. Its entries stop at the xref's length after truncation, so when renumbering leaves nothing, the stream is not among its own entries.

An encrypted document can have its crypt filter dictionary packed into an object stream that needs it to be decrypted; MuPDF writes that, and the read-back check refuses it. Otherwise `-Z` output reads back: the check also compares the objects in object streams, leaves out the object streams and xref stream the writer adds, and ignores the catalog's `/Version`.

On the standard tier the three checks match MuPDF on every file except AES ones (random IVs, D18) and `issue20453.pdf`, where MuPDF's repair during the save differs from nupdf's (D18).

## D22. New encryption follows `pdf_new_encrypt`, with derived secrets instead of random ones

*2026-11-10*

`clean -D` removes the security handler and `clean -E rc4-40|rc4-128|aes-128|aes-256` replaces it, with `-O`, `-U` and `-P` for the passwords and permissions, as `mutool clean` does. The new handler is a port of `pdf_new_encrypt` and `create_encryption_dictionary`: the same keys from the same passwords, the permissions masked to the bits MuPDF keeps, and an `/Encrypt` dictionary written directly into the trailer with its entries in MuPDF's order.

- **`-D` is compared byte for byte** (`clean-D`), and matches MuPDF on every standard file both tools write, the AES ones included, since nothing is encrypted.
- **`-E` cannot be.** `mutool clean` keeps the `/ID` it reads, and because of that never hands it to the encryption step, which then makes a new `/ID` at random. The RC4 and AES-128 keys come from that `/ID`. AES-256 also draws its file key, its salts and four bytes of `/Perms` at random, and AES output needs random initialization vectors. nupdf replaces the `/ID` too, but derives every one of these values from the input (D18 does the same for IVs), so the same input and passwords always write the same bytes. The AES-256 file key is derived from the passwords as well as the content. The `roundtrip-E40`, `roundtrip-E128`, `roundtrip-Eaes128` and `roundtrip-Eaes256` checks instead have MuPDF read each side's output back, with an owner password only, and compare the decrypted objects.
- **An empty stream under AES** gets MuPDF's `/Length` of an IV and a block but no data (D18). Reading it back runs into the bytes after it, so the stream does not survive. 278 standard files have one, and nupdf's read-back check refuses them with `-E aes-128` or `aes-256`; `--no-verify` writes MuPDF's output.
- **Streams under a crypt filter of their own** (`/Filter /Crypt`, as embedded files and some metadata use) are copied with their data still encrypted under the old key, then encrypted again under the new one, as in MuPDF. They no longer decrypt; the check refuses them (10 standard files).

Passwords are cut at 127 bytes, as MuPDF's write options store them, and converted to PDFDocEncoding for revisions 2 to 4.

## D23. Edits are an overlay on the document, and page selection follows `pdf_rearrange_pages`

*2026-11-10*

MuPDF edits a document by changing its objects in memory. nupdf's `Document` keeps the parsed file as it was and records edits beside it: objects replaced, changed or added, a new trailer, the data of new streams. Every lookup consults the edits first, so page lookup, inheritance, name trees, the writer and the read-back check all see the edited document without knowing about edits. The page map is built again after each edit, as MuPDF drops its own. The /Count rewrite MuPDF makes when a page tree is short becomes an edit too.

MuPDF moves an edited object's xref entry into an incremental section, and the writer uses that entry. Two cases differ, and both show in the output. An object replaced with `pdf_update_object` gets a fresh entry, so it is written with generation 0. One changed in place keeps its generation, except one from an object stream, whose generation becomes 0. The overlay records which happened (`update_object`, `alter_object`).

`clean --pages` is `mutool clean`'s page arguments: `pdf_check_document` first, so that a repair cannot lose the edits, then `pdf_rearrange_pages` with the structure tree dropped, then the save. Matching its output needs its in-place changes, quirks included:

- The catalog is replaced by one with only /Type, /Pages, /Outlines, /OCProperties and the named destinations that lead to a kept page; /AcroForm, /Metadata, /Lang and the rest are gone. A direct /Pages or /Outlines is shared between the old catalog and the new one, so changes to it show in both.
- Name tree keys may be strings or names. A link or outline item whose /Dest is a name, not a string or an array, counts as leading nowhere and is removed.
- Form fields are found from widgets whose /Subtype is a direct /Widget, then trimmed from the top of their /Parent chain. In a malformed file whose widget has the page tree as /Parent, that empties /Kids, in both tools.
- A page or /Pages that is not a dictionary fails the command, as `pdf_dict_put` does.

The harness's `clean-pages-reverse`, `clean-pages-first` and `clean-pages-rest` checks compare the output's BLAKE3 with MuPDF's for `N-1`, `1` and `2-N`. MuPDF's script interface has no `pdf_check_document`, so the oracle loads every object itself before rearranging, as `mutool clean` does. On the standard tier the checks match on every file but 25 AES ones. Both tools fail on 66 to 71 more files than `clean` does, whose page trees cannot be counted or walked.

## D24. Editing commands write as `clean` does

*2026-11-10*

`meta`, `pages` with an edit, `split`, `encrypt` and `decrypt` have no `mutool` command of their own to match; `mutool clean` comes closest, and its page arguments already do what `pages --select` and `split` need. So each command is `clean` with MuPDF's defaults and one edit before the save: `pdf_check_document` first, so that a repair cannot lose the edit, then the edit on the overlay (D23), then the save and the read-back check. The output is what `mutool clean` would write had the same edit been made through MuPDF's API.

- `meta --set KEY=VALUE` is `pdf_set_metadata`: an `info:` prefix is dropped, an /Info that is not a dictionary is replaced by a new object, and the value becomes a text string as `pdf_new_text_string` makes it: ASCII as is, anything else UTF-16BE after a byte order mark. Dates are stored as the text given, which is where MuPDF's parsed date ends up too, since it writes the text over it. `--strip` removes the trailer's /Info and the catalog's /Metadata, before any `--set`.
- `pages --select` and `--delete` are `clean --pages`, with `--delete` keeping the pages it does not name, in order. `--rotate` adds a multiple of 90 to each page's inherited /Rotate, read as `pdf_to_int` reads it, and puts the result, from 0 to 359, on the page itself, after any selection. A page that is a direct dictionary is changed where it sits, in its parent's /Kids, as MuPDF changes it: nupdf finds it as its page lookup did, through the page map's walk or the slow descent.
- `split` writes each part from a fresh copy of the input, as `clean --pages` would. Its parts are named after the input, numbered from 1 and zero-padded to the same width.
- `encrypt` and `decrypt` are `clean -E` and `clean -D`.

`pdf_clean_file` refuses a document that the empty password does not open, unless `-p` gives one. MuPDF still decrypts such a document with whatever key the failed attempt left, which can even be the right one: an empty owner password authenticates, then is discarded because the user password is not empty. Every command that writes refuses it too, with exit 3 and `input.needs_password`, and the harness's `clean.js` oracle refuses it as `mutool clean` does. That is 45 standard files and two smoke fixtures, which move from matching to both tools failing in every write check. They are the documents opened without their password that D18's read-back check refused as lossy output; they now fail before the save.

The `meta-set`, `meta-strip`, `pages-rotate` and `pages-select-rotate` checks make the same edit through MuPDF's script interface (`setMetaData`, deleting the entries, `getInheritable` and `put`) after loading every object, then save as `clean` does, and compare the output's BLAKE3. A trailer that is an xref stream's dictionary belongs to that object, so MuPDF's first edit to it moves the object into a new incremental section, whose trailer is copied before the edit; the copy is what it writes. A lone `delete("Info")` is lost that way, on 11% of the standard tier. nupdf's `--strip` removes /Info either way, and the oracle deletes twice, the second time from the copy. On the standard tier each check matches on every file but the 20 AES ones `clean` differs on.

## D25. Grafting follows `pdf_graft_mapped_object`, and a new document is `pdf_create_document`'s

*2026-11-10*

`mutool merge` makes a new document with `pdf_create_document`, then, for each input, a graft map of its own and `pdf_graft_mapped_page` for each page of its range. nupdf does the same, and what MuPDF does along the way shows in the output:

- **A new document** is two objects: a catalog, and a page tree with no pages. As in MuPDF, its /Info is a dictionary inside the catalog, not in the trailer, naming the producer: nupdf where MuPDF names itself. There is no /ID. nupdf builds it as a small file opened like any other, so the overlay (D23) holds everything added later.
- **Copying an object** gives an indirect object its new number before what it holds is copied, so numbers follow the order references are first met, depth first. A stream's data is copied raw (decrypted, still encoded) and its /Length set to match, which leaves an indirect /Length behind as an object nothing uses. A map copies each source object once, so pages from one input share what they shared; a second input, or the same file again, gets a new map and new copies.
- **Null is no object.** MuPDF's null object is the null pointer, so an object that cannot be loaded, or that is free, resolves to the same thing as one that resolves to null, and `pdf_update_object` given it deletes the new object: the output has a free entry where the reference points, of generation 1. The same identity means `pdf_dict_get` never returns a stored null, so page lookups, inheritance and the page edits treat a null value as absent and look further up. `pdf_delete_object` is in the overlay too: the entry becomes free with the next generation when the file had the object, and goes back to the file's own entry when the file had it free or never had it.
- **A failure partway** keeps what was copied. A reference past the source's xref fails the page, and `mutool merge` reports the input and moves on to the next, so the pages merged before it stay, and so do the objects the failed page had copied, some of them taken but never filled, which are free. nupdf reports the input's error in its entry and a `merge.input_failed` warning, and fails the command only when no input opens, where MuPDF would write an empty document. A locked input is refused, as every write refuses one (D24).
- **Inserting a page** is `pdf_insert_page`: into its parent's /Kids where the slow lookup (`pdf_lookup_page_loc`) finds the neighbouring page, with /Parent set and /Count raised on the parent and every node above it (`pdf_walk_parent`, whose cycle check lets a few counts change before it notices). Inserting at page 0 of a document with more than one page starts page labels, as MuPDF does, flattening the label tree into one /Nums array first; later labels shift either way.

The recursion in `pdf_graft_mapped_object` would follow a chain of references as deep as the file makes it; nupdf's copy keeps its own stack, in the same order, so a long chain cannot exhaust the thread's.

The `merge`, `merge-twice` and `merge-reverse` checks compare the output's BLAKE3 with MuPDF's for each file merged whole, twice, and in reverse. The oracle grafts through MuPDF's script interface as `mutool merge` does, saving as `clean` saves, with nupdf as the producer; it matches real `mutool merge` output, apart from the comment line and the producer, on every smoke fixture without links or outlines.

## D26. Links and outlines go through MuPDF's URIs

*2026-11-10*

`mutool merge` copies two things besides pages, and both pass through the URIs MuPDF's link layer speaks: `#page=3&zoom=150,72,720` for a place in the document, `#nameddest=intro` for a named destination, `file:other.pdf#page=2` for another file, anything else for the web. nupdf ports that layer (`link`, from pdf-link.c) rather than copying the objects, because what MuPDF writes is what the URIs say, not what the source held:

- **Links.** A page's Link annotations are read as `pdf_load_link_annots` reads them: the rectangle through the page's transform (`geometry`, `pdf_page_obj_transform`), the target from /Dest, /A, or the /AA mouse-up or mouse-down action. Only external ones are copied, `fz_is_external_link` being a scheme of three or more characters; a relative URI action becomes external against the catalog's /URI /Base, or `file://`. Each becomes a new annotation from `pdf_create_link`: the rectangle back through the destination page's inverse transform, a solid border of width 0, and an action from `pdf_new_action_from_link`, which turns `file:` URIs into GoToR actions with a new file specification object. Links to pages within the document are not copied, as in MuPDF.
- **Outlines.** The source's outline is walked with MuPDF's iterator (`outline`, from pdf-outline.c), which first repairs /Parent, /Prev and /Last pointers that disagree with the /First and /Next chains. An item whose target resolves to a merged page is inserted with its ancestors, `#page=` URIs renumbered to the merged document, and a backward range counted from its low end, as `position_in_range` counts it. The quirks show in the output and are kept: every item copied in one run gets the URI of the last, a new node's open state is ignored and inserting below it closes it (its /Count goes from 0 to -1), and the root's /Count goes negative the same way.
- **Floats.** A coordinate passes through `%g`, which in MuPDF's printf is the shortest form that reads back exactly, so the trip through a URI loses nothing; the transforms are single precision, as MuPDF's are.

The `merge` checks' oracle mirrors pdfmerge.c through MuPDF's script interface, links and outline included. On the smoke fixtures with links and outlines it matches real `mutool merge` output apart from the comment line and the producer. `merge-links.pdf` covers URI, relative, Launch and GoToR links on a rotated and cropped page, and outline items by explicit and named destination, action and none.

## D27. Incremental saves are MuPDF's, touches included

*2026-11-10*

`--incremental` on `meta` and `pages` appends the edit to the file as a new revision, as `pdf_save_document` with `incremental` does: the file's bytes as they were, then a blank line and the comment naming the writer, the objects in MuPDF's incremental section in number order, an xref for those entries alone, and a trailer pointing back with /Prev. MuPDF writes the xref in the style of the last one it read while loading, which for a file of several revisions is the oldest: a table, with the trailer copied from the newest and its /Size and /Prev set and /XRefStm removed, or an xref stream, a new object numbered after the others whose /Index lists the runs of entries. A free entry in the increment gets generation 65535, so it is never reused. Nothing changed leaves the file as it was. The /ID is kept, as in every nupdf write.

What goes into the increment is whatever MuPDF's edits touch, not only what they change. `pdf_dict_del` moves its object into the incremental section even when the key is not there, so `meta --strip` writes the catalog either way. And a trailer that is an xref stream's dictionary belongs to that stream, so the first edit to it moves the stream object into the section, where the writer skips it, leaving a free entry in place of the old xref stream (D24's lost delete is the same effect). nupdf's edits touch the same objects; a full save, which writes everything, never shows the difference.

As in MuPDF, a new document, a repaired file, garbage collection and a change of encryption are refused, with exit 2 and `usage.incremental`; nupdf also refuses object streams. The `meta-set-incremental`, `meta-strip-incremental` and `pages-rotate-incremental` checks compare the output's BLAKE3 with MuPDF's incremental save of the same edit, the comment line in the increment swapped first. On the standard tier they match on every file but AES ones, whose new strings get MuPDF's random IVs (D18); both tools refuse the 211 repaired files and the 45 locked ones, and fail on the 19 that do not open.

## D28. Output is validated against MuPDF's, not against perfection

*2026-11-10*

`qpdf --check` and the Arlington PDF Model judge a file on its own. Most of what they find in nupdf's output was in the input, and MuPDF's output, which nupdf matches byte for byte almost everywhere, has the same findings. So the validation checks are differential like the rest: each side writes the document, the validator reads each output, and nupdf's output passes when the validator says the same of it as of MuPDF's. An absolute pass would fail nupdf on every damaged input MuPDF also writes back damaged.

- **qpdf.** The `qpdf-clean`, `qpdf-clean-Z`, `qpdf-merge` and `qpdf-meta-incremental` checks compare qpdf's exit status (0 clean, 2 errors, 3 warnings) and its messages, with the file's path taken out. qpdf is a test dependency, installed in CI, never linked.
- **Arlington.** nupdf vendors the definitions for the dictionaries it writes or edits: trailer and xref stream, catalog, page tree and pages, link annotations with their actions, border styles and file specifications, outline and outline items, and the document information. It applies the rules that need no predicates: required keys (inheritable ones looked up the page tree), value types with `fn:` version wrappers looked through, fixed name lists, and the direct and indirect requirements. A key whose value is null, or a reference to nothing, counts as absent, as ISO 32000 7.3.7 says. Keys the model does not list are not reported, since MuPDF writes some (a new document's /Info sits in its catalog, D25). The `arlington-clean`, `arlington-merge` and `arlington-meta-incremental` checks compare the violations. `cargo xtask arlington <files>` reports them for any file.

## D29. Content streams are checked operator by operator, through MuPDF's processor

*2026-11-10*

M4's exit is measured on `mutool trace`, which needs the whole interpreter: fonts, colorspaces, functions, shadings and images loaded, then run against a device. The content parser comes first and is checked on its own, against what MuPDF's interpreter hands a processor (`PDFPage.process` in the script interface): each operator with its operands, after the lexing, the operand stack, the text-array rules, error recovery and inline image skipping.

- **The parse alone.** The `operators` and `operators-warnings` checks take each page's resources and annotations away first (`--no-resources`, and the oracle's `bare`). Every resource lookup then fails the same way on both sides, so the result depends on the content streams alone, not on loaders that come with later items. With the resources kept, `nupdf operators` already looks them up, but hands fonts, patterns, shadings and images over unloaded, and does not evaluate optional content yet.
- **State as MuPDF keeps it.** The operand stack holds 32 numbers, one name (255 bytes), one short string in a 256-byte buffer, and one object. Missing operands read as 0, extras are ignored, and the buffer is never cleared, which matters to whatever reads a short string as a C string (below).
- **Inline images.** MuPDF decodes the image to find where its data ends, then looks for `EI`. nupdf runs the image's filters over the content through a leech that counts what it hands out, as `pdf_load_compressed_inline_image` does. The decoders take what they read: LZW, ASCIIHex, ASCII85 and RunLength stop at the bytes asked for, Flate at the end of its buffer or its stream, and DCT and CCITT where their decoders stop reading. The colorspace decides how many bytes the image has, so colorspace definitions load here (`colorspace`), without tint transforms or ICC profile parsing yet.
- **Contents arrays.** The parts are joined with a space after each, a part that does not open is skipped with a warning, and one whose decoding fails ends the contents there, as `fz_open_concat` does. Parts are decoded a byte at a time, as the lexer reads them, so a decoder that fails loses only the byte it was asked for. That made LZW honor the reader's size hint, as `next_lzwd` does.
- **The oracle's quirks.** The script glue has bugs that the comparison works around rather than hides. It passes `"` the wrong arguments, so `"` is left out on both sides. For an inline image without a `/CS` name it pushes a null string, which crashes the script engine, so `BI` is not reported on both sides; what follows the image shows where it ended. It hides the ExtGState's line, font and flatness entries inside a `gs`, but not its dash. And it pushes short ASCII text as a C string, which runs on into what longer strings left in MuPDF's buffer and is read by mujs as UTF-8. nupdf's library reports the exact operand and also its C-string view, and only the recorder that writes the oracle's format reproduces the rest.
- **Warnings.** `operators-warnings` compares the distinct warnings and reported errors, the content's syntax errors among them, which made diagnostics keep every distinct message past their cap of 1,000 entries (up to 10,000), and made decoders print a bad byte as MuPDF's `%c` does.

## D30. Resources load where MuPDF's interpreter loads them

*2026-11-11*

`pdf_process_keyword` does not only dispatch operators: it loads the colorspace `CS` names, the shading `sh` names and a shading pattern's shading, and throws when one does not load, which counts as a syntax error and drops the operator. So the loaders come before the interpreter, and are checked through the same processor oracle with the page's resources kept (`operators-resources`).

- **Functions** are a port of `pdf-function.c`, evaluation included: shadings sample their functions when they load, 256 times along an axis and on a 257 by 257 grid for a function-based shading. Arithmetic is single precision with MuPDF's guards, and integer arithmetic in calculator functions wraps as C's does in practice. Function evaluation shows only in rendering, so it is checked fully in M7.
- **Colorspaces** load their tint transforms. An ICC profile is not parsed: its header names the component count, and MuPDF's checks with lcms on a broken profile (which warn and fall back to /Alternate or /N) are not reproduced. (D31 reproduces the header checks.)
- **Shadings** are kept per object number for one run of a page, as MuPDF's store keeps them, so that a function-based shading used many times is sampled once.
- **What waits.** Fonts load with the font item, so `Tf` still hands over the font dictionary, and the warnings of a run with resources are not compared yet: font loading warns, and so do the transparency scans `pdf_load_page` makes of the resources. Optional content waits for the interpreter: MuPDF's processor for scripts has no usage, so it never evaluates it.

## D31. The run processor is checked against MuPDF's script device, text left out until fonts

*2026-11-10*

The interpreter item is `pdf-op-run.c` ported whole, and its check runs each page's contents into a device on both sides: MuPDF's script device (`PDFPage.runPageContents`, oracle/js/trace.js) and nupdf's trace device (`nupdf trace`). Each device call is printed as an array, and the `trace` check compares them page by page.

- **What a call shows.** Paths as their walked segments, matrices, colors with their colorspace's type and component count, stroke states, color parameters, images by shape, colorspace, decode array, color key and mask, shadings by their bounds under the call's matrix, groups, masks, layers, structure and metatext. A colorspace is not named: an ICC space's name comes from its profile's description tag.
- **The glue's quirks are followed.** The glue has no script object for MuPDF's device Lab space, so the script sees undefined for it (null in the trace). Its `beginTile` returns from inside its exception frame, which breaks the script engine after a few tiles, so the oracle leaves the tile callbacks out; MuPDF then draws a tile's content once, and neither side prints tiles.
- **Text is left out on both sides** (`without_text`; superseded by D32, which compares text): the calls that show text, the clips text opens (their content stays), and the groups and masks that hold nothing once text is gone. Without fonts nupdf draws no glyphs, but it tracks what text changes elsewhere: shown text flushes pending layers and structure where MuPDF flushes it, glyphs shown in a clipping mode make a text clip at `ET` that ends with its gstate, and a `cm` with such a clip pending fails the page as MuPDF's does. What remains needs fonts: Type 3 glyphs MuPDF draws through the device, and text filled with a pattern or a shading (its clip, and the pattern's content or the shading, are drawn for it).
- **Numbers agree within 1e-4 of their size.** The oracle build fuses multiply-adds, so a matrix or a box can differ in its last bits.
- **What is emulated rather than kept on the document.** MuPDF writes the parents its structure tree check repairs into the document; nupdf keeps them with the store, per document. MuPDF memoizes the transparency scan of a resource dictionary; nupdf computes it afresh, which differs only for cycles through resources.
- **Limits.** The graphics state stack stops where MuPDF's does (2048 levels, "too many nested graphics states"), which bounds how deep forms and patterns nest; the CLI runs on a 1 GiB stack (touched only as used) so that this depth fits. MuPDF can fail earlier, at about 60 nested forms, when its 256-frame exception stack overflows; that depth counts frames of its own and of the script glue, and is not reproduced. A pattern fill that would draw more than 2^20 cells fails ("too many pattern cells"), where MuPDF would draw them all.
- **ICC profiles and JPEG 2000 headers** are judged as lcms and openjpeg read them: lcms's header checks (signature, version, device class, tag directory) and channel counts, with MuPDF's family checks against /Alternate; and openjpeg's header read (the SIZ segment, the JP2 colour specification box and embedded profile), with MuPDF's choice of colorspace from it and the dictionary's, and 8 bits per component.

## D32. Fonts load as MuPDF loads them through FreeType, and text is in the trace

*2026-11-11*

The fonts item is `pdf-font.c`, `pdf-type3.c`, `pdf-unicode.c` and `pdf-metrics.c` ported on top of a port of the parts of FreeType MuPDF uses. The `trace` check now compares text: each text call carries its spans as the script glue walks them (the font's name, the matrix without its translation, writing mode, bidi level, markup direction and language) and each glyph's id, Unicode and position. The harness's text filter is gone.

- **Font programs open as FreeType opens them.** TrueType and OpenType (`font::sfnt`: the table directory, `head`, `maxp`, `hhea`, `hmtx`, `OS/2`, `post` names, and each `cmap` subtable validated at FreeType's default level), CFF (`font::cff`: charsets, encodings, CID-keyed fonts and their FDSelect, widths from charstrings) and Type 1 (`font::type1`: PFA and PFB, eexec, the private dictionary and charstrings, `.notdef` moved to glyph 0). The drivers are tried in MuPDF's order, and a failure carries FreeType's error, so a font that FreeType refuses ("FT_New_Memory_Face(%s): broken table") falls back as it does in MuPDF. Charmaps, the default Unicode charmap, a Unicode charmap synthesized from glyph names, and MuPDF's `ft_char_index` and `ft_name_index` fallbacks follow FreeType and fitz.
- **Outlines are not read yet.** A glyph's box is taken to be its font's box. Boxes only shape what is drawn around text (a soft mask's or a knockout group's bounds, the area a pattern covers), and no standard-tier trace differs because of it. Outlines come with rendering (M7), and with them the width table MuPDF builds for stretching substitute glyphs.
- **The standard 14 fonts and the substitutes** are the URW fonts MuPDF bundles, vendored unmodified (NOTICE). There are no system fonts, as in the oracle.
- **No CJK fonts.** MuPDF's default build bundles Source Han Serif (a 25 MB collection). nupdf does not, and behaves as MuPDF built with `NO_CJK`: a CJK substitute fails ("cannot find builtin CJK font") and the text takes the last-resort font. This is what remains of the trace gap. Carrying the font, or loading it when asked, is left for later.
- **Data tables are generated** from the pinned oracle by `cargo xtask tables`: the Adobe Glyph List and the named encodings, the predefined CMaps (as a compact binary, linked through their `/UseCMap` chains on first use), FreeType's PostScript name tables and CFF's standard encodings, and UCDN's bidi classes.
- **MuPDF's quirks are kept.** A span's language comes only from the innermost marked content's own /Lang: MuPDF means to fall back to the structure element and to outer marked content, but `pdf_dict_get_string` never returns NULL. Span fields are compared and kept as MuPDF's bit-fields hold them. `cid_to_ucs` is MuPDF's `unsigned short`. Type 3 glyphs are run once when their font loads, for the device flags their operators leave; a glyph that inherits the graphics state, or a colored glyph filled with a pattern, is drawn through the device, and a font that reaches itself while loading or drawing fails ("recursive type3 font"). A Type 3 glyph's box is its `d1` box, or the font's when that is not to be trusted, where MuPDF would measure the glyph with a bounding-box device.
- **Limits.** Remapping a ToUnicode CMap through a composite font's encoding stops after 2^24 codes ("ToUnicode remap too large"); MuPDF has no bound, and a range over all four-byte codes would keep it busy for minutes.

## D33. Fonts and images are listed as `mutool info` lists them, and extracted as `mutool extract` writes them

*2026-11-11*

The commands item adds `fonts` and `images`. Their lists, the files they write and the pixmaps `images --digests` decodes are each checked against MuPDF (`fonts`, `font-files`, `image-list`, `image-pixmaps`, oracle/js/inventory.js).

- **The lists follow `mutool info`'s walk** (`gatherresourceinfo` with everything shown): each page's own /Resources, not inherited ones, through fonts, XObjects and patterns; each object visited once per page; each font or image dictionary listed once, at the first page it is found on, deduplicated with `pdf_objcmp`. A page that cannot be found is reported and the walk goes on, where `mutool info` would stop. The oracle is a script, so it walks with MuPDF's objects but not with MuPDF's own walk; the walk is short enough to read against pdfinfo.c line by line.
- **`fonts` also says what each font loads as** (`load`), from the font loader of D32, with its Type 3 fonts' resources left to their own. MuPDF's scripts cannot see this, so the check leaves it out.
- **Files are written as `mutool extract` writes them**: every font descriptor's program by its key (`FontFile` as `.pfa`, `FontFile2` as `.ttf`, `FontFile3` by its /Subtype), every image XObject as JPEG data where its last filter is DCTDecode and it has no decode array or color key, otherwise decoded, as PNG for gray and RGB and PAM for the rest. The PNG has no iCCP chunk: nupdf does not keep an ICC space's profile. The checks compare written files by hash, on both sides.
- **Pixmaps are decoded as `fz_get_pixmap_from_image` decodes them** at full size: the samples read up to the rows the image needs (so no compression-bomb guard applies, as in MuPDF's image read), JPEG frame headers patched to the image's height, samples unpacked to 8 bits, image masks inverted, color keys, decode arrays, indexed images in their base space, and /Matte colors unblended with the soft mask. `--digests` gives the MD5 of the samples, as MuPDF's `computeMD5` does; the script glue shows MuPDF's device Lab space as null, and so does the report.
- **JPEG 2000 images are not decoded yet**: they are listed and fail to decode with an unsupported error. Decoding them as openjpeg does is left for rendering (M7).
- **Both sides list before they decode.** Finding pages can repair a document, which changes what later objects load as, so the oracle walks the pages before it extracts or decodes, as the commands do, and object counts are taken before anything loads.

