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
