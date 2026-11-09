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

A `Vec` of key/value pairs with linear lookup, where re-inserting a key replaces its value in place (`pdf_dict_put`'s behavior, so the last duplicate wins). Real dictionaries are small. MuPDF sorts dictionaries over 100 entries; nupdf will add an index if profiles show large dictionaries matter.

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

