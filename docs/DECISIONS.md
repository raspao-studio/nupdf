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

*2026-11-09. Settles the plan's open question 1.*

MuPDF stores reals as `float`, and its `fz_strtof` is not correctly rounded: it keeps 9 significant digits and uses Coonen's Algorithm D with a 32-bit significand. Parsing with Rust's correctly rounded `f32::from_str` made dozens of standard-tier files differ by one ulp. nupdf ports `strtof.c` exactly (`crates/nupdf/src/strtof.rs`), including `fz_atof`'s rule that an underflow returns 1.0. Geometry code may still compute in f64 internally, as long as what it reports matches MuPDF.

## D11. Dictionaries keep insertion order, with replace-in-place

*2026-11-09. Settles the plan's open question 2 for now.*

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

The fuzzer found two loops in hayro-jbig2's symbol dictionary decoding that never end on damaged data, because its arithmetic decoder keeps returning values past the end: endless empty export runs, and endless empty height classes. jbig2dec stops the first after 1000 empty runs. nupdf carries hayro-jbig2 0.4.0 with that guard and the same one for height classes, until upstream has an equivalent: first as a vendored crate, and since 0.1.0 as a module of the nupdf crate (`crates/nupdf/src/hayro_jbig2/NUPDF.md`), because crates.io does not apply a workspace's patches. Decoding time also follows the symbol sizes a dictionary declares, and a 600-byte stream can declare tens of thousands of large symbols: MuPDF took more than five minutes on one the fuzzer found. The carried copy caps a dictionary at 2^28 pixels of symbols, far more than real dictionaries hold.

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
- **Outlines are not read yet** (until D34). A glyph's box is taken to be its font's box. Boxes only shape what is drawn around text (a soft mask's or a knockout group's bounds, the area a pattern covers), and no standard-tier trace differs because of it. Outlines come with rendering (M7), and with them the width table MuPDF builds for stretching substitute glyphs.
- **The standard 14 fonts and the substitutes** are the URW fonts MuPDF bundles, vendored unmodified (NOTICE). There are no system fonts, as in the oracle.
- **No CJK fonts** (until D34, which carries their metrics). MuPDF's default build bundles Source Han Serif (a 25 MB collection). nupdf does not, and behaves as MuPDF built with `NO_CJK`: a CJK substitute fails ("cannot find builtin CJK font") and the text takes the last-resort font. This is what remains of the trace gap. Carrying the font, or loading it when asked, is left for later.
- **Data tables are generated** from the pinned oracle by `cargo xtask tables`: the Adobe Glyph List and the named encodings, the predefined CMaps (as a compact binary, linked through their `/UseCMap` chains on first use), FreeType's PostScript name tables and CFF's standard encodings, and UCDN's bidi classes.
- **MuPDF's quirks are kept.** A span's language comes only from the innermost marked content's own /Lang: MuPDF means to fall back to the structure element and to outer marked content, but `pdf_dict_get_string` never returns NULL. Span fields are compared and kept as MuPDF's bit-fields hold them. `cid_to_ucs` is MuPDF's `unsigned short`. Type 3 glyphs are run once when their font loads, for the device flags their operators leave; a glyph that inherits the graphics state, or a colored glyph filled with a pattern, is drawn through the device, and a font that reaches itself while loading or drawing fails ("recursive type3 font"). A Type 3 glyph's box is its `d1` box, or the font's when that is not to be trusted, where MuPDF would measure the glyph with a bounding-box device (D34 measures it).
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

## D34. Structured text is MuPDF's stext device, glyph boxes come from outlines, and CJK substitutes carry their metrics

*2026-11-11*

The first M5 item ports the stext device (`stext`) and adds `nupdf stext`. The `stext` check compares each page's blocks, lines and characters with `Page.toStructuredText` walked by oracle/js/stext.js; `stext-preserve` and `stext-raw` do the same with MuPDF's other options. A file matches when every page has the same blocks, lines and characters, each character's font, flags and bidi level equal, its origin and size close, and its box overlapping MuPDF's by an IoU of at least 0.98. The harness also reports the M5 measures: the pages whose text is MuPDF's character for character, and the characters whose box overlaps MuPDF's by 0.98 or more.

- **The device is `stext-device.c`**: lines and blocks from the pen's motion (spacing, baseline offset, paragraph indents, bullets), synthetic spaces, fake bold, ligature and presentation-form decomposition, whitespace folding, ActualText (prefix and suffix matching, text flushed at the pen, or over the bounds of what it covers, with Helvetica borrowed when no font has been seen), clipping by the device's scissor and the media box, image blocks, and the closing pass (dehyphenation flags, boxes, right-to-left runs reversed). The container stack and its scissors are `device.c`'s, kept inside the device. An unbalanced call disables the device as MuPDF's does: it stops taking text and images and is not closed. Only the page's contents run, as `fz_new_stext_page_from_page` runs them; annotations are not part of it.
- **Options are MuPDF's option strings**, parsed into MuPDF's flags: `preserve-ligatures`, `preserve-whitespace`, `preserve-images`, `preserve-spans`, `inhibit-spaces`, `dehyphenate`, `clip` (and `mediabox-clip`), `clip-rect`, `use-cid-for-unknown-unicode`, `use-gid-for-unknown-unicode`, `accurate-bboxes`, `accurate-side-bearings`, `ignore-actualtext`. Structure, vectors, styles, segmentation, tables and accurate ascenders are refused until the items that implement them. The defaults are MuPDF's: clipping only.
- **Colors are not compared.** A character's color is converted to RGB as MuPDF converts it, which goes through its color management for CMYK and ICC spaces; the device converts gray, RGB and CMYK naively. The check leaves color out.
- **Glyph boxes come from outlines**, as `fz_bound_ft_glyph` measures them: the control box of the glyph's unhinted outline at one font unit per 26.6 unit, every point counted. `font::outline` ports FreeType's TrueType loader (simple glyphs, composites with their transforms, offsets and point matching, the recursion and loop checks) and its Adobe charstring engine for CFF and Type 1 (stems and hint masks for their byte counts, flex, `seac`, the other subroutines, the arithmetic operators, the stack and nesting limits), as far as which points reach the outline; a charstring's coordinates come out floored to whole units, as the engine's shift does. A glyph that fails to load is bounded by a point, as in MuPDF. A substitute font's glyphs are stretched to the PDF's widths through the width table MuPDF builds (`pdf_make_width_table`). Boxes are measured once per glyph and kept with the font, in blocks of 256 as `bbox_table` is. Fake bold and fake italic substitutes, which the bundled fonts never need, and CFF inside an sfnt (only system fonts have it) are not measured; the latter keeps the font's box.
- **A Type 3 glyph's box is measured** as `fz_bound_t3_glyph` measures it: the bbox device run over the glyph's display list under the font matrix, with the list's culling of empty content and of what an empty clip encloses.
- **CJK substitutes carry Source Han Serif's metrics** and nothing else: a font with its units per em, box, ascender and descender (its four faces share them), every character mapping to glyph 0, keeping no glyph boxes, as MuPDF keeps none for CJK fonts. Text extraction reads nothing else of the font, so non-embedded CJK text now extracts as MuPDF extracts it. The trace still differs there: its glyph ids are the real font's.
- **A CID-keyed CFF font's dictionaries keep their own units**: a glyph is scaled from its font dictionary's units per em to the top dictionary's, as `cff_slot_load` scales it. Skewed font matrices are not applied.
- **Exit measures on the standard tier**: 100% of pages have MuPDF's text and 100% of characters have a box with IoU ≥ 0.98 (19,792 pages, 2,887,542 characters), with default options and with `stext-preserve`'s. `stext-raw`, whose boxes come from glyph outlines (`accurate-bboxes`), matches 99.8% of files: what is left is a Type 3 glyph that fails while its font loads (MuPDF keeps an infinite box), and a few fonts whose outlines nupdf does not yet measure as FreeType does. The speed measures come with the bench pairs item.

## D35. `extract` gives text with content ids, in four formats and within budgets

*2026-11-11*

The output item adds `nupdf extract`. Its structure is the stext device's (D34) with the options given; what it adds is nupdf's own, except the plain text, which is MuPDF's and is checked against it (`text`, `text-dehyphenate`, oracle/js/text.js).

- **Content ids are positions.** `p3` is page 3, `p3.b2` its second block (text and image blocks counted together), `p3.b2.l4` the block's fourth line, `p3.b2.l4.s1` the line's first span, all counted from 1; `p3.b2.l4.s1:5-12` names characters 5 to 12 of a span, from 0, the end excluded, for search hits and redaction. Ids depend on the document and the stext options, as block and line boundaries do.
- **Spans are runs of one style**: font name, size, the character's bold flag, the font's italic style and color. A space the device adds takes its neighbours' style, so it never starts a span. Colors are `#rrggbb`, with the alpha after when it is not opaque, converted as D34 says, so CMYK and ICC colors are approximate.
- **`--fields` picks levels and keys.** Levels nest: `blocks`, `lines`, `spans`, then `chars` (each character and its quad); a level needs the one above. Text goes at the deepest level kept, so `--fields text` gives each page's text alone and the default (everything but `chars`) gives each span's. Ids are always given.
- **Budgets stop between blocks.** `--max-chars` counts the characters of the blocks given and stops before the block that would pass it; `--max-items` counts blocks, or pages when blocks are not asked for; the first block is always given, so every call makes progress. A stopped report has `"truncated": true` and `next_cursor`, `p3.b5@<doc_id>`, which `--cursor` resumes from; a cursor for another document is a usage error. With `--pages`, a cursor resumes within those pages.
- **Plain text is MuPDF's** `fz_print_stext_page_as_text`, with `mutool draw -F txt`'s form feed and newline after each page: a line per line, an empty line after each block, and a line joined by `dehyphenate` written without its hyphen or break. Like `Page.toStructuredText` and unlike `mutool draw`, only the page's contents are run, not its annotations.
- **Markdown is a reading of the blocks**, not MuPDF's. A block whose largest characters are 1.2, 1.4 or 1.8 times the page's body size (the size most characters have, to half a point) is a heading of level 3, 2 or 1, and neighbouring heading blocks of one size are one heading; a line that starts with a bullet starts a list item, which the lines indented to its text continue; other lines join into a paragraph with spaces, or without one after a joined hyphen. Bold and italic runs that hold a letter or digit are marked. Markdown's syntax characters are escaped. Each page starts with `<!-- p3 -->`, and image blocks are left out.
- **JSON lines** give a `nupdf.extract.page/v1` object per page, each with `doc_id`, then the report without pages. Text and Markdown go to stdout and the report to stderr, so a pipe gets only the text.

## D36. Search is MuPDF's, from its text transforms to its regular expressions

*2026-11-11*

The search item adds `nupdf search`, a port of `fz_match_stext_page_cb`. The checks `search`, `search-icase` and `search-regexp` compare each page's hits, quad by quad, with `StructuredText.search` (oracle/js/search.js).

- **The haystack is the page flattened as MuPDF flattens it**: lines joined by a space, blocks by a space, a soft hyphen or a line `dehyphenate` joined dropped with its line break (unless `keep-hyphens`), line and paragraph breaks kept as newlines with `keep-lines` and `keep-paragraphs`; each character knows where it came from, and added spaces come from nowhere.
- **Haystack and needle are transformed alike**: compatibility decomposition, then the composition of marks after their sorting by combining class, full-width ASCII made ASCII, hyphens made `-`, spaces folded into one, and with the options upper case (`fz_toupper`) and non-spacing marks dropped. So `é` finds `é` and `fi` finds the ligature. The UCDN data (categories, combining classes, decompositions, compositions) and MuPDF's case tables are generated from the oracle by `cargo xtask tables` into `unicode`.
- **Regular expressions are MuPDF's** (`regexp.c`, from MuJS): JavaScript's syntax with captures, back-references, lookahead, classes and counted repeats, compiled per needle and run by backtracking with MuPDF's limits (4096 levels, 2^20 steps for a search, 32K instructions); a pattern that does not compile fails each page with MuPDF's message (`regcomp failure: ...`). Case-insensitive classes give MuPDF's answers but find them from the case tables instead of trying every rune of each range, which MuPDF does and which can take minutes on `[\W]`.
- **A hit's quads** are its characters' quads, a character merged into the quad before it where it continues it (within half a size along the line and a tenth across, as MuPDF measures it). Hits made only of added spaces are skipped, as in MuPDF.
- **Hits carry content ids** (D35): `id` for a hit within one span (`p3.b2.l4.s1:5-12`, from 0, the end excluded), otherwise `begin` and `end`, and their text.
- **An empty needle finds nothing.** MuPDF returns no hits for an empty needle; one that becomes empty when transformed (only marks, with `ignore-diacritics`) would match everywhere without moving on in MuPDF, and finds nothing here.

## D37. Pages run in parallel on request, with a report that does not depend on the thread count

*2026-11-11*

`stext`, `extract` and `search` take `--threads N` (`parallel`). Without it pages run one after another as MuPDF runs them, sharing one resource store; with it they run on N threads and the output is the same at every N, 1 included. The two can differ only in diagnostics.

- **Each thread keeps its own store** (fonts, images, shadings, patterns) for the pages it takes, as a run without threads keeps one for all of them. The store holds layer state that is not `Send`, so it cannot be shared. What a store has cached does not change a page's text: across the standard tier the pages from `--threads` equal the pages without it, for `stext` and for `search`.
- **Fonts are loaded once for all the threads.** A font one thread has loaded is in a map the others look in before loading it themselves (`Store::sharing_fonts`); a thread takes its own copy of it, because every character holds its font, and eight threads counting the holders of one font cost more than the copy. Type 3 fonts are not shared: what their glyphs load depends on the page that first uses them. Two threads that load a font at once each use their own; the two are the same.
- **Nothing a page does on one thread waits on the others.** Loaded objects are kept in slots set once (`Cache`), so reading one takes no lock; a thread keeps its finished pages until it is done; zlib inflaters are kept per thread and reset, since making one waits on the allocator's lock when eight threads make them at once.
- **The work of turning a page into output runs on the threads too** (`parallel::map_pages`): `stext` builds each page's JSON there and `search` its hits.
- **The page tree is walked first, on the calling thread**, so its warnings come in order. Then the threads take pages in turn and their results are put back in page order.
- **Each page's diagnostics are kept apart while it runs** (`diag::capture`) and reported once each, sorted by code and message. The document is shared, so a warning from loading an object goes to whichever page loads it first, and that changes with the thread count; sorted and deduplicated, the set does not. Repeats of a warning across pages are therefore reported once.
- **A repair during the run starts the run again.** A page that makes the document repair itself changes what the pages before it would have read, and which pages ran before it depends on timing. So the file is opened afresh (`Document::reopen`, keeping an accepted password) and every page is done again, one after another, on the new document. A document made or changed in memory has no file to reopen; its pages stay as they ran.
- **`extract` with a budget works on 64 pages at a time**, the same at every thread count, so `--max-chars` and `--max-items` stop it early without extracting the rest of a long document; without one it works on all the pages at once. Its threads are kept from one window to the next (`parallel::with_workers`), each with its store, and each page's JSON is made on the thread that extracted it. If a window repairs the document, every store starts afresh for the next. `stext` works on all the pages asked for, and so does `search` with threads; without them it stops running pages once `--max-items` is reached, as before.
- **Workers run on 1 GB stacks**, as the CLI's main thread does: content streams nest about a thousand forms deep.

*2026-11-12.* With these, on the standard tier's seven documents of 50 pages or more, 8 threads against 1 (`stext-scaling`, `extract-scaling`; 6 performance and 12 efficiency cores, with other work on the machine): `stext` 1.97× in geometric mean, `extract` 1.80×, from 1.92× and 1.63×; on `freeculture.pdf` (352 pages) 3.6× and 3.4×. The threads now run almost the instructions one thread runs (1% to 9% more, from up to 24%); what is left is that each instruction takes longer on eight cores than on one. Two of the seven documents take 4 to 6 ms in all, mostly starting the process, and one has 10,000 pages of a few objects each, whose shared objects the threads count at once.

## D38. Reading order is an opt-in stext option of nupdf's own, scored against labeled pages

*2026-11-11*

By default, blocks come in the order MuPDF gives them: the order the content stream draws them in, grouped by its stext device. Many files draw out of reading order (forms filled in field by field, sheets of notes drawn row by row), and an agent reading such text gets it scrambled. The `reading-order` stext option (`order`) reorders each list of blocks, the page's and each structure's, by a recursive XY-cut: the widest empty band across or down the area cuts it in two, read top before bottom or left before right, until no band separates the blocks, which then keep MuPDF's order.

- **It is an option, not the default** (D5): the default stays MuPDF's, and the harness keeps comparing it.
- **It is scored against people, not MuPDF.** `corpus/reading-order/truth.json` holds hand-labeled pages: for each, distinctive fragments of its text in the order a person reads them. `cargo xtask reading-order` scores both orders by the share of fragment pairs that come in the labeled order. The pages include ones MuPDF's order already gets right (two-column papers, a one-column book page), so the option is shown not to make them worse, and ones it gets wrong (a sheet of notes in four columns, two forms). At introduction: MuPDF's order 0.880, `reading-order` 0.995.
- **MuPDF does not know the option name** and would reject it; nupdf accepts it alongside MuPDF's options.
- **Columns drawn line by line across** (2026-11-13). When the content stream draws a line of the left column, then one of the right, MuPDF's device puts both columns into one block, and no reordering of blocks can separate them. The option now splits such a block at its gutter first, if the lines on each side are at least as wide as the gutter (columns of text, where a form's short labels and far-off values stay together). Bands are weighed in what they would hold: a band across in lines of text, a band down in characters, taken as half a line, so paragraph breaks no longer outweigh a gutter. A heading of up to four blocks is taken off first when it reaches into the gutter and what remains divides into columns. The labeled pages still score 0.995; the agent eval's two-column procedure page now reads column by column.

## D39. Arithmetic is fused as MuPDF's build fuses it

*2026-11-11*

The oracle is MuPDF built by clang at `-O2`, whose default `-ffp-contract=on` turns `a * b + c * d` into `fma(a, b, c * d)` and `a * b - c * d` into `fma(a, b, -(c * d))` within an expression. Content streams written back (`clean -s`, redaction) print matrices and text moves at the shortest round-trip digits, so a last-bit difference changes the bytes. `geometry` (matrix concatenation, translation, point and vector transforms), the text matrix updates in `run` and the sanitize filter's text moves use `mul_add` in the same places, which is what disassembling MuPDF's code shows clang does. A MuPDF built without contraction (gcc on x86-64 without `-mfma`) would round differently; the harness's oracle is the build in `target/oracle`.

## D40. Redaction is MuPDF's pdf_redact_page, given quads, then nupdf's own scrub and check

*2026-11-11*

`redact` finds its targets as quads on each page (text and patterns with MuPDF's search over the stext with clipping off and hidden layers run, regions, content ids, `search` reports), converts them to PDF space as `pdf_set_annot_quad_points` would, and redacts each page as `pdf_redact_page` does with those quads in place of Redact annotations: the sanitize filter (`content_filter`) drops glyphs whose box, shrunk by a tenth, meets an area, splits `TJ` arrays around them, gives each use of a form its own copy, removes line art an area covers (MuPDF's default keeps it; the contract says vector content goes), and blanks image pixels; links and FreeText annotations over the areas go. The contract asks for more than MuPDF does, so after it:

- **Every annotation and widget over an area goes**, whatever its type, with its popup, and a widget leaves the form's fields.
- **Marked-content properties lose an /ActualText, /Alt or /E that holds a target**, besides MuPDF's edits to the structure elements its MCIDs name.
- **The document is scrubbed** (`scrub`): metadata entries and XMP packets that hold a target are removed, as are structure /Alt, /ActualText and /E, JavaScript, named destinations and attachments; outline titles, annotation and field text, URIs, page labels and layer names have the matches cut out; redacted pages lose their thumbnails. The output is a full rewrite with unreferenced objects collected, so one revision remains.
- **Pixel redaction writes 8-bit samples** as `pdf_add_image` does for a pixmap (a bit each when they are black and white), deflated by the writer; a JPEG image comes back as Flate, not DCT. Image masks and inline images are blanked as MuPDF blanks them, but an inline image mask is written a bit a sample (MuPDF writes a byte each, which reads back as another mask), and an inline image whose data cannot be written inline again (JBIG2, JPX) or whose colorspace `BI` names by resource is removed whole.
- **`verify` is independent of how the output was made** (`verify`): page text with clipping off and hidden layers run, every string of every object (unreachable ones counting as history), text streams, marked-content properties and the strings appearance streams show, the pixels of images under the redacted areas, thumbnails, and the revision count. `redact` runs it on the output before writing and exits 4 without writing when anything is found.

Black boxes are drawn only with `--boxes` (MuPDF's default draws them). Parity with MuPDF is checked on what the redaction leaves (`redact-text`: every run of digits redacted, each page's text compared as MuPDF reads both outputs); the adversarial corpus (`corpus/redaction`, `cargo xtask redaction`) and the cases pdf-redaction-benchmarks generates (`--benchmarks`, its generator run in isolation at a pinned time) check that nothing leaks, judged by `nupdf verify`, MuPDF and pdftotext. `--no-verify` writes the output anyway, for the parity check: MuPDF's redaction leaves what the check catches.

## D41. The rasterizer is a port of MuPDF's draw code; color management is moxcms made to act like lcms2

*2026-11-11*

The plan left the rasterizer open: vello_cpu, tiny-skia, or a port of `draw-edge`/`draw-paint`, to be chosen by measured pixel parity and speed. The exit asks for 98% of pages within a pixel threshold of `mutool draw`, and anti-aliasing alone puts a general-purpose rasterizer outside that on most text: MuPDF covers paths with a 17×15 subsample grid, draws glyphs with FreeType's exact-area rasterizer at quarter-pixel offsets, scales images with its own filter, and paints with its own 8-bit rounding. hayro, on vello_cpu, shows the gap: on pdf.js's transparency and knockout tests it is 20% to 60% of pixels away from MuPDF where the port matches it exactly (`cargo xtask triage`). So `draw` ports MuPDF: the edge list and scan conversion, path flattening and stroking, the span, glyph and image painters with their arithmetic, FreeType's smooth rasterizer for glyphs (`draw::grays`), subsampling, the scaler and the affine painter, shadings as triangles, groups, soft masks, blend modes and tiles. The draw device keeps MuPDF's state stack.

- **The threshold**: a pixel differs when a channel is off by more than 8 (of 255); a page is within the threshold with at most 0.5% of its pixels differing. Sizes and per-page errors must agree. The `render-72`, `-150` and their `-icc` checks render page contents without annotations (`Page.runPageContents`, `render --no-annots`); `render-annots-*` render whole pages as `mutool draw` does (D52).
- **Color management is on by default**, as in `mutool draw` (D5). MuPDF's own profiles (gray, sRGB, SWOP CMYK, Lab) are vendored and used through moxcms, a pure-Rust CMS. moxcms has no black point compensation, which lcms2 applies for MuPDF's default intent; `cms` computes it as lcms2 does (`cmsDetectBlackPoint`: a perceptual round trip of black for an output CMYK profile, the darkest colorant otherwise; `ComputeBlackPointCompensation` in XYZ) around moxcms's transforms into a linear sRGB. On SWOP CMYK swatches the result is within 1 of MuPDF's. `--no-icc` is MuPDF's `-N`: the device formulas.
- **Transparency is MuPDF's**: groups isolated or not (a non-isolated group paints over a copy of its backdrop and keeps its group alpha), knockout groups (each object over a copy of the group's backdrop, with a shape, knocked back), soft masks by luminosity or alpha with transfer functions, the separable and non-separable blend modes with MuPDF's byte arithmetic, including its non-isolated "uncompositing".
- **Known differences**: stroked text (render modes 1 and 2) is drawn by stroking the outline as a path where MuPDF uses FreeType's stroker; colored Type 3 glyphs paint as masks; large glyphs (over 256 pixels) are filled unhinted where MuPDF lets FreeType hint them at 1024 ppem; tiles drawn inside non-isolated or knockout groups do not carry group alpha or shape; JPEG 2000 decodes through hayro-jpeg2000 rather than OpenJPEG; JBIG2 files on which jbig2dec gives up decode in full; images are decoded whole where MuPDF may decode a subarea, and JPEG is not decoded at reduced scale. Overprint is not simulated: MuPDF simulates it only for subtractive destinations, and `render` draws RGB.
- **Parallelism**: pages render on `--threads` threads; a page of a megapixel or more with threads to spare is recorded once as a display list (`draw::list`, `fz_display_list`) and played into horizontal bands on separate threads. Every band gives the same pixels as the page drawn whole, which a test checks.

At introduction, on the standard tier: with color management on, 98.98% of 19,790 pages within the threshold at 72 dpi and 98.40% of 9,790 at 150 dpi; with it off on both sides, 99.30% and 99.03%. Against `mutool draw -N` at 72 dpi on a 200-file sample: one thread, geometric mean 1.25× MuPDF's speed; eight threads, 1.28× (the oracle is built without threads, so MuPDF runs on one).

## D42. The MCP server runs the CLI's commands in-process, one thread per call

*2026-11-12*

`nupdf mcp` serves the commands as tools of a Model Context Protocol server, JSON-RPC 2.0 over stdin and stdout, one message a line (protocol revisions 2025-06-18, 2025-03-26 and 2024-11-05).

- **The tools are the CLI's commands, read from its definition.** Each command but `mcp` is a tool whose input schema is built from the command's clap arguments: a flag is a boolean, a repeatable flag (`clean -g`) a count, an option with fixed values an enum, a list an array, anything else a string or a number, with the CLI's help as the description and its default. The global options come with every tool except `--pretty`. A call is turned back into a command line, options in the order the command defines them and positional values after `--`, and parsed by the CLI's own parser, so a tool takes exactly what the command takes and stays in step with it.
- **A call's report is the CLI's, byte for byte.** The call runs `run`, the CLI's dispatcher, on a thread of its own (on a 1 GB stack, as the CLI's), with what the command writes to stdout and stderr kept for that thread. The result's text is stdout, then stderr when there is any (the report of `extract --format text`); `structuredContent` is the report when the report is all the call gave; text and Markdown output has none, since clients show a model the structured content in place of the text (the agent eval's pilot found agents seeing only `extract --format text`'s report). A failure is a tool error carrying nupdf's error report. Calls run at once, each on its thread.
- **Files are read and hashed once; documents are parsed for each call.** The server keeps each file's bytes and `doc_id` (up to 1 GB, the least recently used going first), keyed by its path, size and modification time, so a changed file is read again. It does not keep parsed documents: a document reports a warning the first time an object loads, so a document reused from an earlier call would leave out of a report warnings the CLI gives. Parsing the xref again is a small part of a call; reading and hashing the file is what it saves.
- **A call stops at its time or when cancelled, where its work next checks.** Each call has an `Interrupt` (`nupdf::interrupt`), a flag and a deadline (`--call-timeout`, 300 s by default, or the call's `timeout` if shorter), which the threads it starts carry too. Content streams check it at every operator and page threads before every page, so a long call stops within a few operators of the deadline or of `notifications/cancelled`. Interrupted work breaks off as a damaged content stream would, so nothing it made is kept: its output goes nowhere, files it would write are not written (`clean` and the other writers check before the output is renamed into place), and a call out of time fails with exit 5's `limit.timeout`; a cancelled call gets no answer, as the protocol asks. The CLI takes the same `--timeout`. When stdin closes, the calls still running finish, each within its time, and are answered.
- **Output has a ceiling.** A report larger than `--max-output-bytes` (8 MB by default) is a `limit.output` error that points to the budgets (`pages`, `fields`, `max_chars`, `max_items`), rather than a message the client may not take.

## D43. `batch` runs a command on each file in-process, writing a line per file in the order given

*2026-11-12*

`nupdf batch <inputs>... -- <command> [args]` runs `nupdf <command> <file> [args]` for each file, as the MCP server runs a call (D42): in-process, on a thread of its own, with what it writes kept.

- **Inputs** are files, directories (their `*.pdf`, any case, at any depth) and glob patterns, each file once, in name order within an input and the inputs in the order given. A pattern that matches nothing is a usage error. A command whose first argument is not a file (`merge`, `batch`, `mcp`) is refused.
- **Files run several at once** (`--jobs`, one per core by default), each command with its own threads if it takes `--threads`. Their lines go out in the order the files were given, each as soon as the ones before it are out, so the output is the same at every `--jobs`.
- **A line per file**: `nupdf.batch.item/v1` with the file, its size and modification time, the exit code, and the command's report (`report`; for text output, `text` and the report from stderr) or its error report (`error`). The summary, `nupdf.batch/v1`, counts the files, those that succeeded, failed, were skipped and were not run. A file that fails does not fail the batch: its line says why, and `batch` exits 0.
- **Runs can be taken up again.** With `-o FILE`, the lines go to the file, each written as soon as it is in order, and the summary to stdout. `--resume` reads the lines already there and skips every file they report with the same size and modification time; a file changed since runs again, and its new line is added after the old one, so the last line for a file is the one that counts. A line cut off by a killed run is ended before adding.
- **Arguments can name each file's output**: `{stem}`, `{name}` and `{index}` (the file's number, zero-padded) in the command's arguments, as in `-- clean -g -o out/{stem}.pdf`.
- **`--item-timeout`** bounds each file as a call's `timeout` does; the global `--timeout` bounds the batch, which then starts no more files.
- `batch` is not an MCP tool: an agent calls tools at once itself, and a batch's lines are a stream, not one report.

## D44. `diff` lines pages up by their text, then lines within the pages that changed

*2026-11-12*

`nupdf diff <a> <b>` compares two documents by `--by text,structure` (the default), and `pixels` on request; the report has a part for each, null when not asked for, and `identical` when every part asked for is the same.

- **Text: pages first, then lines.** Each page's lines are its stext lines with text, with the content ids `extract` gives them (`p3.b2.l1`). The pages of the two documents are lined up by the shortest edit script over their whole text (Myers), so a page taken out or put in is one page deleted or inserted, not every page after it changed. A run of pages gone next to a run of pages new is taken as pages changed, paired in order, and what is left over is pages deleted or inserted. Each pair of changed pages gets the edit script of its lines: `delete` with the id in `a`, `insert` with the id in `b`, and the line's text. Past 2,000 edits between two pages, the page is `replaced` instead; past 2,000 between the documents' pages, pages are paired by position.
- **Structure: named parts, each compared whole.** The page count; each page's size (its bounds after rotation, to 0.01 pt), rotation, annotation count and image count; each Info entry (`info.Title`); the outline, as titles indented by level; the form's field count; and the fonts the text is drawn in (from the text, not from `fonts`, which follows `mutool info` and misses fonts it does not look for). A difference names the part and gives `a` and `b`, or for the fonts and the outline what is only in each.
- **Pixels: each page drawn at `--dpi` (36 by default) on both sides**, in the same pixels as `render`, compared exactly: the pixels that differ, their share of the page, the largest difference in a channel, and their bounding box in points. Pages are compared by position, as far as the shorter document goes.
- **`--max-items`** (500) bounds each kind's list, with `truncated` on the text.
- Diagnostics from both documents are reported, each with `document`: `a` or `b`. The report's `doc_id` is `a`'s.

## D45. The guide and the skill are made from the CLI's definition; the schemas are read off its reports

*2026-11-12*

- **`nupdf guide`** is plain text a model reads at the start of a task, so it is short (1.6 KB, a few hundred tokens): the report envelope and exit codes, the commands for each step of a task with the options that matter (`--options reading-order` for text that comes out of order, budgets and cursors for long files), and every command by name, listed from the CLI's own definition so the list cannot fall behind. It was 8 KB at first; the agent eval's pilot showed agents reading it on every task, at 2,000 tokens each time, and turning to rendered pages where the guide did not say how to get text in reading order. `nupdf guide <command>` is that command's full help. `nupdf guide --skill` is the same text as a Claude Code skill, with the name and the description Claude Code reads to decide when to use it; `skills/nupdf/SKILL.md` is that output, and a test fails when the two differ.
- **`nupdf schema <name>`** prints the JSON Schema (draft 2020-12) of a report: a command's (`extract`), or one of its parts (`extract.page`, `batch.item`, `error`); the report's own id (`nupdf.extract/v1`) works too, and `nupdf schema` lists them.
- **The schemas are read off what the commands print**, not written by hand: `cargo xtask schemas` runs each command, in the variants that change its report's shape, on every smoke fixture and 400 files of the standard tier, groups the reports by `schema`, and writes each group's shape: the types each field has had, the fields every report had as required, arrays by what their items held. Keys that nest under themselves (stext's `blocks`) share one definition in `$defs`, so a block deep in a structure has what blocks have anywhere; an object with more than 24 keys is a map; an array only ever seen empty takes anything. What a shape cannot tell is written in the generator: a PDF object as `objects` and `show` encode it (a recursive definition), the Info dictionary as a map of strings, `batch.item`'s `report` and `error` as any report, `diff`'s values as anything, and `page_count` as possibly null wherever it appears. A schema allows fields it does not list, since adding a field does not break a reader.
- **They are checked against more than they were read from.** `cargo xtask schemas --validate standard --sample N` runs the same commands on other files of a tier and lists every place a report breaks its schema; at introduction, 150 other standard-tier files broke none. `--check`, which needs only the smoke tier, checks that every smoke report has a schema and fits it, and the CLI's tests check a set of reports the same way.

## D46. Outlines, links, page labels and embedded files are read as MuPDF's script API gives them

*2026-11-12*

`outline`, `links`, `labels` and `attachments` report what MuPDF's functions return, through the calls `mutool run` scripts make, and each is checked against `oracle/js/navigation.js` on every file of a tier.

- **The outline** is `fz_load_outline`: the outline iterator, its repair of /Parent, /Prev and /Last included, read item by item as `load_outline_sub` reads it, each entry's page from `fz_resolve_link`. A tree the iterator refuses (a cycle, or more than 256 levels) fails with `input.outline`.
- **Links** are `pdf_load_link_annots` on the page's /Annots, boxes through the page's transform, as `Page.getLinks` gives them; the page an internal link leads to is `fz_resolve_link`'s.
- **Page labels** are `pdf_page_label`: the last /Nums entry at or before the page in the /PageLabels tree, its style (`D`, `R`, `r`, `A`, `a`, or none), prefix and start, cut as MuPDF's 100-byte buffer cuts it; a page outside every range gets its number. MuPDF follows /Kids without a bound; nupdf stops at 64 levels and at a node it has seen.
- **Embedded files** are the /EmbeddedFiles name tree walked as the script API's `getEmbeddedFiles` walks it, names in the order a script's record of them has (names that are array indices first, by number), and each file specification as `pdf_get_filespec_params`, `pdf_is_embedded_file` (which wants /Type /Filespec) and `pdf_verify_embedded_file_checksum` read it. Dates are `pdf_parse_date`'s, its warnings included (`date.invalid`).
- **Pages load as `pdf_load_page` would** before their links or label are read: the page object must be a dictionary; a page with /Annots must have a transform (its inherited boxes), as MuPDF computes it only then, for the links; and its inherited /Resources must be readable. A page MuPDF cannot load is an error in place, with MuPDF's message, and one it can is not.
- **The script API's own failures are not taken as MuPDF's.** `getEmbeddedFiles` throws on a document with no /EmbeddedFiles tree; the oracle reads that as no files, which is what MuPDF's functions see.


## D47. Signatures are checked as MuPDF's OpenSSL backend checks them, RSA signers in nupdf's own code

*2026-11-12*

`nupdf signatures` reports each signature field as `PDFWidget` gives it in a MuPDF built with OpenSSL, and `oracle/js/signatures.js` holds it to that on every file of a tier.

- **What a field says**: signed or not (`pdf_signature_is_signed`), its name, the signature dictionary's /Filter, /SubFilter, /Reason, /Location, /Name and /M, the /ByteRange as given, the length of /Contents (read from the file without decrypting it, as `pdf_load_unencrypted_object` reads it), and whether the file goes on past the last signed byte (`pdf_signature_incremental_change_since_signing`). A range outside the file, and a gap between ranges that holds anything but the hex signature (`validate_certificate_data`), are errors in MuPDF's words.
- **The digest check is `PKCS7_signatureVerify`'s**: the bytes the ranges cover hashed with the signer's digest; with signed attributes, their messageDigest compared with that hash ("Signature invalidated by change to document." when they differ), then the signer's signature over the attributes; without, the signature over the hash itself. The result is in MuPDF's words (`pdf_signature_error_description`): "OK", "No signatures.", "Unknown error.". A subfilter of `adbe.pkcs7.sha1`, whose signed content is a hash of the bytes, fails, as it does in MuPDF.
- **nupdf verifies RSA signers itself** (PKCS #1 v1.5, `rsa.rs`: the public-key operation, on public data, so not in constant time) instead of depending on the `rsa` crate, whose advisory (RUSTSEC-2023-0071, about private keys) would fail `cargo deny`. A signer with another kind of key (ECDSA, RSA-PSS) is "Unknown error." here, where OpenSSL would check it. Signatures are read with a small DER reader that also takes BER's indefinite lengths, as OpenSSL does (`der.rs`, fuzzed with the forms target).
- **The signatory** is the subject of the certificate the first signer names by issuer and serial number (names compared as `X509_NAME_cmp` compares them: entries in order, values trimmed, white space collapsed, ASCII lowercased), as `cn=..., o=..., ou=..., email=..., c=...`, else "Signature information missing.".
- **Not checked**: the certificate's trust (MuPDF's answer depends on the machine's trust store), and `pdf_validate_signature`'s comparison of the fields a signature locks across the file's versions.
- **Saving a signed file**: every command that writes reports, for a signed input, each signature's check before and after (`output.signatures`), and warns (`signature.invalidated`) when a signature that held no longer does. Only `--incremental` keeps the signed bytes, so that is what the warning says.

## D48. `sanitize` removes what a document can do besides show itself, and proves it by scanning the output

*2026-11-12*

MuPDF has no command for this; `sanitize` is nupdf's own.

- **Every object is walked**, so an action is found wherever it hangs: /A, /OpenAction, /PA, /Next chains (one action or an array of them), each trigger of /AA (page, annotation, field, document), outline items, and the /Names /JavaScript tree. An action that is an object of its own is emptied and the keys pointing to it dropped; a direct one goes with its key. /AA dictionaries left empty go.
- **Removed by default**: JavaScript (JavaScript actions, Rendition actions with /JS, the document's scripts, the form's calculation order /CO), Launch, SubmitForm and ImportData (`data`), GoToR and GoToE (`remote`), and XFA (the AcroForm's /XFA, the catalog's /NeedsRendering), since an XFA form carries scripts of its own. `--keep javascript,launch,data,remote,xfa` keeps any of them.
- **By choice**: web links (`--uri keep`, the default; `remove`; or `allow:HOST,...`, which keeps links to those hosts and their subdomains), and embedded files (`--attachments`: the /EmbeddedFiles tree and FileAttachment annotations' files).
- **Each removal is reported** with the object, the key path inside it (`/AA/K`, `/Names/JavaScript`), its kind and what it said (the script's first characters, the URI, the program), with counts by kind.
- **The output is checked before it is written**: saved, read back and scanned with the same policy; anything left is exit 4 (`verify.failed`), as for `redact`.

## D49. Annotations, forms and flattening follow MuPDF to the byte

*2026-11-12*

- **`annots`** lists each page's annotations as `pdf_sync_annots` does (dictionaries in /Annots, links, popups and widgets aside), with what the getters read: type, flags, contents, colours, opacity, dates, and, for the types that have them, rect, author, subject, interior colour, icon, open, border, line, vertices, quad points and ink list, in page space. An annotation without an appearance is read as loading the page leaves it, with the appearance MuPDF makes for it (its /Rect and /RD can change). `oracle/js/annots.js` checks it on every file.
- **Appearance synthesis** (`appearance/`) is `pdf-appearance.c`, writer by writer: the shapes, text markup, icons, stamps, free text, line captions and widgets (text, choice, push buttons, check boxes, radio buttons). The `annot-synthesis` and `widget-synthesis` checks make every appearance again on both sides (`requestResynthesis`, then `update`) and compare the streams byte for byte, with their /Rect, /RD, BBox, Matrix and Resources. Arithmetic is C's, on `f32`, in C's order, with `a * b + c` as a fused multiply-add where clang makes one (the left product of `a*b + c*d`). Not covered: text MuPDF lays out with its HTML engine (free text and fields with /RC, /RV or /DS, or characters no base-14 or CJK font has), which is an error here (`annot.appearance`), and unsigned signature fields, which MuPDF draws through a display list.
- **`forms`** lists each page's widgets with what the field getters read: full name (the /Parent chain mended as `pdf_repair_page_tree_parents` mends it, which also makes each widget's parent its page), label, type, flags, value, options, maximum length, read-only and signed; with `need_appearances` and `xfa` for the document. `sanitize` strips XFA.
- **`flatten`** is `pdf_bake_document`: on each page, what has no appearance gets one, written into the document; then each annotation (`--annots-only`) or field (`--widgets-only`), or both, is drawn into the page as a form XObject (`AnnotN`) with the transform that maps its box onto its /Rect, after enough `q` and `Q` to start from the page's state, and removed from /Annots; links stay; with fields, /AcroForm goes. MuPDF's quirks are kept: a direct /Resources inherited by a page is shared with the node it comes from and every page that took it, as one object is in MuPDF; an annotation stored inside /Annots gets its form made but the page never shows it; a Stamp with a name MuPDF does not draw keeps the appearance made on loading the page. The `flatten` and `flatten-annots` checks compare the written files' BLAKE3 with MuPDF's.

- **Editing annotations**: `annots --add JSON` creates one as `pdf_create_annot` does, with each type's defaults (flags, rect, colour, a Text note's popup, FreeText's DA, a Stamp's "Draft"), then applies the fields given through MuPDF's setters, in page space, in a fixed order the oracle uses too; the fields are the listing's, so a listed annotation can be passed back. `--delete ID` is `pdf_delete_annot` (its popup goes with it), ids from the listing before any change; `--highlight TEXT` adds a Highlight on each `search` hit. The changed annotations get appearances on the page update, in the order MuPDF updates its open pages (newest first). MuPDF's quirks are kept: a direct page is shared through /P; a direct annotation without an appearance cannot be deleted (MuPDF deletes the copy it made on loading), which is reported as `not_deleted`.
- **Filling fields**: `forms --set NAME=VALUE` and `--toggle NAME` are `setTextValue`, `setChoiceValue` and `toggle` with JavaScript off (`scripts_run: false`): /V on the field, /AS on its widgets, radio groups turned together, every widget of the field made again, on every page. MuPDF sets read-only fields and choice values outside the options; so does nupdf, and the report notes it.
- **Writing appearances**: the fonts the writers need are added as objects of their own, before the form that uses them, one per font and encoding for the whole document, as MuPDF's font cache keeps them (`appearance::commit`).

## D50. OCR runs Tesseract and lays its words over the page as invisible text

*2026-11-12*

- **`ocr`** draws each page at `--dpi` (300), runs `tesseract <image> stdout -l <lang> --dpi <dpi> tsv` as a subprocess (with a time limit a page), and puts the words it reads on the page in render mode 3, each scaled to the box Tesseract gave it, after the page's contents and outside their graphics state. The page looks the same; its text can be searched, selected and extracted.
- **The font has no glyphs**, as Tesseract's own PDFs have it: a Type 0 font (Identity-H) whose codes are the characters' UTF-16 units, every glyph half an em wide, with a ToUnicode map from each code to itself. Characters outside the Basic Multilingual Plane become U+FFFD.
- **Only scanned pages, unless asked**: a page with no text and images over at least half of it. `--pages` and `--all` choose otherwise; `--min-confidence` leaves out words Tesseract is unsure of.
- **`info` says which pages look scanned** (`text.scanned_pages`, and `text.next` names the command), from a copy of the document of its own so that drawing the pages adds nothing to what `info` reports. Documents of more than 2,000 pages are not looked at page by page.
- **Tesseract is not bundled.** Without it, `ocr` fails with `ocr.unavailable` and how to install it; `--tesseract PATH` names another program, which is how the tests run it.

## D51. `extract --format layout` puts each row of a page on one line

*2026-11-13*

- **Why**: in MuPDF's text order a label and its value are often far apart. Generated forms and invoices draw values in their own blocks, sometimes before the labels, so `Total` and `393.25` come out lines apart or reversed. In the agent eval, agents that met that text rendered the pages and read the images, which cost several times the tokens of the text (eval/REPORT.md, token follow-up).
- **What it does**: horizontal lines whose baselines are within half a line of each other share a row; rows go down the page, lines across it, each at the column its left edge gives (one column per the page's median character width, counted from the page's leftmost text), at least a space apart. A gap between rows of more than two lines' height becomes one empty line. Lines that are not horizontal follow, a line each. Rows are cut off at column 400, so text placed far off the page cannot make a row of millions of spaces. A form feed ends each page, as in `text`.
- **Its own format, not MuPDF's**: MuPDF has no such output, so `text` stays `fz_print_stext_page_as_text` (D5) and `layout` is opt-in. `--options` apply as for the other formats.
- **Budgets count whole pages**: rows cross blocks, so `--max-chars` and `--max-items` stop before a page, and `next_cursor` names a page.

## D52. Pages are drawn whole, annotations and widgets included, as `mutool draw` draws them

*2026-11-13*

- **Why**: `render` drew the page contents alone (`page.runPageContents`), but `mutool draw` runs `fz_run_page`: the contents, then the annotations, then the widgets. A filled form, a stamp or a comment was missing from nupdf's pixels, and `extract --format text`, which says it is what `mutool draw -F txt` writes, was missing their text. D41 left this for M9, which made the appearances.
- **What is drawn** (`page_run`, from `pdf_run_page_with_usage`, `pdf_run_annot_with_usage` and `pdf_process_annot`): the page's annotations as `pdf_sync_annots` lists them (never links or popups), then its widgets, each in a run processor of its own with the page's resources and default color spaces. Not drawn: a widget without an inheritable /FT and /T; Invisible or Hidden ones; NoView ones for "View" (only Print ones for "Print"); those whose /OC is off; those without an appearance. The appearance is `pdf_annot_ap`'s (/AP /N when it is a stream object, else the state /AS names), drawn as `q`, the `cm` of `pdf_annot_transform` (its /BBox through its /Matrix onto the display rectangle: /Rect, resized to the box as stored for NoZoom, turned with the page for NoRotate), `Do`, `Q`. Annotations are not clipped to the CropBox and not inside the page's transparency group, as in MuPDF. An error ends the page's run where it happens, as it leaves `fz_run_page`: after an error in the contents no annotation is drawn.
- **Appearances made on loading stay out of the document**: what `pdf_update_appearance` makes when the page loads (for an annotation without an appearance, for every widget when the form has /NeedAppearances, and for an unsigned signature, whatever it has) MuPDF keeps in a local xref, with the annotation's new /Rect. nupdf makes the same with `appearance::synthesize` and keeps the form as a local stream of the document (`Document::add_local_stream`: a negative number no object has, its data dropped with it), with the changed annotation as a copy. Rendering leaves the document unedited: `annots`, `flatten` and a write after it see what they would have seen. An unsigned signature shows MuPDF's sign-here arrow (`pdf_signature_appearance_unsigned`, "SIGN" fitted by `pdf_layout_fit_text`); MuPDF makes that form through its PDF-writing device, nupdf writes the same drawing directly, so only the pixels are MuPDF's, and only `render` and text use it.
- **Each command follows the MuPDF function it names**: `render`, `redact --preview` and `diff --by pixels` draw the whole page (`render --no-annots` for the contents alone). `extract --format text` is `mutool draw -F txt`: `fz_run_page` into a display list, played into the stext device as `fz_run_display_list` plays it (`list_run`: each call's rectangle as the list device computes it, a clip's narrowed at its pop to the union of what was drawn inside, calls with nothing to show left out). A text device reads its clips, so the list matters: text whose quad lies mostly outside the box of what its clip holds is not gathered, as `mutool draw -F txt` leaves it out and `mutool draw -D` keeps it. `--no-annots` gives the contents alone, run directly as before. `stext`, `search`, `tables`, `trace` and `extract`'s JSON, Markdown and layout stay with the contents, as `Page.toStructuredText`, `fz_search_page` and `fz_new_stext_page_from_page` run them, so content ids mean the same block everywhere and `redact --id` finds what `extract` named. `ocr` draws the contents alone: its text layer is for the scan, and an annotation's text is its own. Redaction is unchanged: its targets come from the contents' text, every annotation and widget over an area goes whatever it shows, and `verify` reads annotation contents, field values and the strings appearance streams show from the objects themselves.
- **Checks**: `render-annots-72` and `render-annots-72-icc` compare whole pages with MuPDF's `page.run` into a draw device; `text-annots` compares `extract --format text` with each page's `toDisplayList(true).toStructuredText().asText()` and a form feed, which is what `mutool draw -F txt` writes; the contents-only `render-*` checks run `render --no-annots`. Measured on 2026-11-13, before (nupdf's contents against MuPDF's whole page) and after:

  | Check | Standard tier (5,142 files) | Wild sample (1,000 files) |
  |---|---|---|
  | `render-annots-72` | 94.1% → 98.9% of files; 98.35% → 99.74% of 19,790 pages | 94.7% → 95.6% of files; 98.43% → 98.68% of 13,041 pages |
  | `render-annots-72-icc` | 97.8% of files; 99.36% of pages | 89.4% of files; 95.52% of pages |
  | `text-annots` | 95.3% → 99.4% | 70.8% → 99.3% |

  Whole pages match exactly as often as the contents alone do (`render-72`: 5,070 standard files, 954 wild ones), but for the two rich-text annotations below; the colour-managed check's remaining differences are those of `render-72-icc`, CMYK under an output intent's profile, which the annotations of two Isartor files draw in. Of `text-annots`' 29 standard mismatches, 28 are rich text and 1 is contents' text that comes out of the list differently; the wild sample's 7 are in the contents' text.
- **Known differences**: a FreeText or text field whose text needs rich layout (an /RC or /DS, or characters neither the base 14 fonts nor MuPDF's CJK fonts have) gets its appearance from MuPDF's HTML story engine, which nupdf does not have: nupdf warns `annot.appearance` and draws nothing for it.

## D59. Agent-native primitives: `attest`, `patch`, `apply`, `query` and `audit`

*2026-11-14*

MuPDF parity (`D5`) stays intact on every existing command; five opt-in primitives (`attest`, `patch`, `apply`, `query`, `audit`) build on nupdf's rasterizer, content-stream filter, `Arc`-shared `Document` overlay and structured-text geometry for autonomous agent workflows.

- **`attest` (perceptual-vs-stream truth)**: agents that read extracted text alone miss z-order occlusions (faux white/black rectangles or images painted over text), zero-contrast prompt injections, vector-drawn strikethroughs (lines stroked across glyphs with no annotation or font flag), `/ActualText` spoofs where extracted text differs from rendered glyphs, and broken ToUnicode maps (`U+FFFD`). `attest` renders each page twice — full `page_run` and a `NoTextDevice` pass that drops only text calls — comparing per-span pixel deltas for visibility and contrast while intersecting vector strokes/fills with glyph boxes, comparing `/ActualText` against underlying glyph runes, and optionally writing cropped PNGs only for blind-spot regions.
- **`patch` (verified surgical text replacement)**: replaces spans or lines (`--replace ID=TEXT`, ids as `extract` numbers them) by erasing only the target's glyphs from the content stream and appending a text layer at the target's baseline origin and direction, size and fill colour. The replacement is set in the Base-14 font that matches the original's family, weight and posture (or a CJK font for CJK text), added as a new font resource; it does not re-encode through the original font. `fallback_font` is false only when the page already used that exact Base-14 font and the text is WinAnsi; `--strict-font` makes any other case a `verify.font_fallback_disallowed` failure. The output is saved, read back as every writer's is, and re-extracted before anything is written: the replacement must be found near the target (`verify.text_missing`), the old text gone (`verify.old_text_remained`), the glyphs inside the page (`verify.page_overflow`), and they must not overlap an untouched span more than the original did (`verify.collision`; the overlap single-spaced lines' boxes already have is no collision). A failure exits 4 and writes nothing.
- **`apply` and `Document::fork_clean` (multi-agent transactional plans)**: `Document::fork_clean` clones `Arc<[u8]>` source bytes and `Arc<State>` (parsed xref and 64-shard object cache) in `O(1)` with an isolated fresh `Edits` overlay. `apply` checks each plan's `doc_id` against the base, then resolves every target on the immutable base before any edit: content ids (numbered as `extract` numbers them), patch targets with their old text and boxes, annotation ids by the base's listing, text and regex hits, regions and pages. An op that repeats, field for field, one another plan staged is applied once and listed in `duplicates`; a repeat within one plan is that plan's own. Pairwise conflicts (patches of one span with different text, patch or highlight over a redaction, different values for one field or metadata key, different page selections, an op on a page another drops) exit 4 with nothing written. The edits then run on a fork in this order: form fills; annotation deletes and highlights; redactions, then the redacted pages' thumbnails and every string the patterns match scrubbed (forcing a full rewrite with garbage collection); patches at the targets found on the base (`plan.target_destroyed` if another op took a target's glyphs); metadata; sanitizing; and last, page rotations and selection. The patches' and redactions' checks (the patch checks above; `verify` on the redactions' needles, patterns and areas, as `redact` runs it) read a save made before the page ops, whose coordinates and page numbers are the base's; with no page ops they read the output itself. The output goes through the writers' shared save and read-back check; `verified` is null under `--no-verify`, never true for edits not checked.
- **`query` (spatial/relational queries and corpus aggregation)**: resolves anchors, as whole words (`Total` does not anchor on `Subtotal`) or as content ids, and scores directional candidates (`right-of`, `left-of`, `below`, `above`, `inside`, `near-widget`) with style filters, accounting/currency number parsing (`($1,250.50)` → `-1250.5`), and parallel directory/glob aggregation (`--summary-only`) so multi-PDF quantitative questions need one deterministic call instead of per-file agent turns.
- **`audit` (deterministic citation and epistemic-hazard verification)**: checks structured claims (`doc_id`, page, content id, quote) without LLM calls, relocating citations that appear on a different page (`wrong_location`) and checking verified spans for five epistemic hazards: vector/flag/annotation strikethroughs, adjacent superscript footnote markers (`*`, `†`, digits) omitted from the quote (recovering matching bottom-of-page footnote text), `/ActualText` mismatches, invisible/occluded text, and post-signature incremental revisions (`--strict` exits 4 on any unverified claim or hazard). A quote matches on word boundaries and never stops inside a number (`Total 12` does not verify against `Total 125`, nor `rate 12` against `rate 12.5`); a quote that ends in `.` or `,` before a digit verifies only when that digit is a superscript footnote marker (smaller and raised), so `rate 12.` does not verify against `rate 12.5` nor `1,000,` against `1,000,000`, while `capped at $1,000.` does before a superscript `1`.

## D60. Next-generation agent primitives: `chart`, `flat-forms`, `patch --reflow`, `history`, `cloak`/`uncloak`, `pipeline` and zero-import `wasm32`

*2026-11-14*

Six capabilities extend D59 so agents can operate on vector charts, flat print-master forms, multi-line contract paragraphs, signed revision histories, sensitive PII documents, compound workflows, and sandboxed WebAssembly edge runtimes without falling back to lossy raster vision or host syscalls.

- **`chart` (deterministic vector chart and diagram decompiler)**: runs a bounded `Device` path collector (`MAX_COLLECTED = 50_000`) alongside `stext::extract_page`, fits least-squares linear calibration (`R² >= 0.995`) over monotonic numeric tick columns/rows, recovers exact floating-point data series from bar/horizontal-bar rectangles and stroked polylines/markers (matching series names to legend swatches by fill/stroke RGB), and extracts directed node-edge flowcharts by pairing text-enclosing boxes with perimeter connectors and filled triangle or V-chevron arrowheads.
- **`flat-forms` (flat vector field synthesis, verified filling, and `/AcroForm` promotion)**: detects flat `"underline"` rules (or `'_'` runs), multi-cell `"comb"` grids (`|_|_|_|`), and `"checkbox"` squares (`5..18 pt`, with z-ordered check-state detection) on pages lacking `/AcroForm` widgets, associating nearby caption spans as deterministic `snake_case` names. `--set`, `--check` and `--uncheck` fill fields with auto-shrunk text, cell-centered comb glyphs (failing with exit 4 `verify.comb_overflow` when input exceeds `cell_count`), and crisp vector checkmarks verified on read-back; `--promote` upgrades flat geometry into interactive `/AcroForm` `/Widget` annotations (`/FT /Tx` with `Comb` `1<<24` and `/MaxLen`, `/FT /Btn` checkboxes). Fills and promotions are made on a fork, saved, and read back with `write::verify` as every writer's output is (`verify.failed`; the library functions do it too) before the fills or widgets are checked on the output; the report's diagnostics are the fork's, so warnings raised while editing reach it.
- **`patch --reflow` (block-scoped paragraph reflow and vertical Y-shift)**: when replacing a 2-segment block id (`p1.b2`) or passing `--reflow`, `reflow` word-wraps replacement paragraphs within the block's column width using exact font advance metrics, computes `delta_y`, shifts single-column downstream content-stream text matrices, subset glyphs, vector rules and `/Annots` `/Rect`/`/QuadPoints` vertically, and rejects multi-column collisions (`verify.collision`) or page-bottom overflows (`verify.page_overflow`) with exit 4 before writing. A plan (`apply`) does not take a block id (`usage.block_target`): it checks its patches as spans that stay where they were found, which a reflowed block does not.
- **`history` and `Document::open_revision` (temporal xref time-travel and post-signature tamper forensics)**: `Document::revision_byte_ends` locates chronological `%%EOF` boundaries so `Document::open_revision(1..=N)` mounts any historical revision in-process without mutating the outer document. `history` verifies PKCS#7 RSA signatures at each revision, diffs structured lines, form fields, annotations and perceptual span visibility, and distinguishes legitimate incremental fills (`clean_incremental_fill`, where prior `/ByteRange` digests remain `"OK"`) from post-signature content edits (`tamper.post_signature_content_edit`: changed text lines, or any change to what a page's content streams, resources and boxes reach, compared by hash so an image, font or vector swap counts), post-signature annotation edits (`tamper.post_signature_annotation_edit`: any non-widget annotation added, removed or changed, or a form field added or removed; filling existing fields and adding signatures stay clean), post-signature white-box visual occlusions (`tamper.post_signature_visual_occlusion`), and broken digests (`tamper.signature_broken`); `--strict` exits 4 on any post-signature tamper. A page's fingerprint follows at most 200,000 objects and 64 levels of nesting; a page cut short by either, in a revision after a valid signature or the one before it, could not be compared and is a hazard too (`tamper.post_signature_unchecked`, its numbers in the delta's `unchecked_pages`), so a check that could not compare never reports clean. Only hashes that do not depend on where an object was reached from (none cut short, none cut at a cycle) are reused across a revision's pages.
- **`cloak` and `uncloak` (reversible format-preserving PII firewall)**: replaces PII presets (`ssn`, `email`, `phone`, `card`, `iban`, `pii`), custom literals (`--entity CATEGORY=SECRET`), and regexes across page content streams, AcroForm field values and appearances, annotations, `/Info`, outlines, structure trees, and XMP metadata with deterministic coreference-preserving synthetic tokens (`000-00-0001`, `[PERSON_01]`), rewrites with full orphan stream GC (`garbage: 4`), and enforces a mandatory `verify` zero-leak check (exit 4 on any residual secret) while writing the secret mapping only to `--vault`. `uncloak` deterministically restores synthetic tokens in both agent-edited PDFs and UTF-8 text/JSON (`--text-mode`). The leak check reads a locked output with the document's password and fails with `verify.unreadable` when it cannot read it; its error names the category and page of what leaked, never the secret. The check reads each page's text and each string once for all the secrets (`verify::verify_literals`): a many-string scan (Aho-Corasick) over the text as the search transforms it, and over strings with the space, tab, CR and LF a literal's pattern skips taken out, picks the secrets that can be there, and only those go to the search or pattern that decides, so it reports what searching for each secret on its own did (tested against that check on generated pages and strings, and on a leak of each class). On an 80-page directory of 4,000 phone numbers it takes 36 ms where the per-secret check took 14.0 s, and 146 ms where it took 261 s at 320 pages and 16,000 numbers (`cargo test --release -p nupdf --lib cloak_leak_check_timings -- --ignored --nocapture`). A category's tokens grow a digit when its format runs out (`555-010-10000`). Both save through `write::save_verified`, the read-back every writer runs, before the leak check (`verify.failed`), and report the fork's diagnostics. An incremental cloak is refused (`usage.incremental`) instead of quietly written in full: it would keep the secrets in the bytes it appends to, and `garbage: 4` cannot be incremental.
- **`pipeline` and zero-import `wasm32-unknown-unknown` edge execution**: `pipeline` executes multi-step workflows (`info`, `attest` hazard gate, `query` with `${step.var}` variable binding, `patch`/`apply_plan`, `chart`, `flat_forms`, `history` tamper gate, and `audit`) in a single in-memory pass without intermediate disk serialization or repeated CLI/MCP round-trips. Because `nupdf`'s entire parser, font engine, rasterizer, writer and crypto stack are pure safe Rust with deterministic seeding and no OS/C dependencies, `nupdf` + all 11 agent primitives compile to `wasm32-unknown-unknown` with **zero host imports** (`WebAssembly.Module.imports(mod).length === 0`, instantiable with `{}` in bare V8/Edge isolates). An id a variable bound (`${q1.first_id}`, `${q1.first_span_id}`, `${p1.first_new_id}`) is tracked by that variable, not by the id string, so another step binding the same string to other text later does not change what it names; after a step that changed the document it is found again only as the span with the same text at its own position (within 2 pt), or at that position moved by the `delta_y` a reflow step reported for what lies below its block. Text gone from there, or two spans with it there, fails the step with `pipeline.stale_id`, never a nearby twin. Ids typed in the spec are used as given. A gate that aborts writes nothing. Its mutated output is read back before it is written (it must reopen, with the password, without a repair and load every object and page: `verify.failed`), and a step's incremental save the file cannot take is `usage.incremental`. Its report lists the warnings raised on each step's working copy of the document, each once.
