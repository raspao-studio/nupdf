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
