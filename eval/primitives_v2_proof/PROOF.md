# Empirical Proof: Next-Generation Agent-Native PDF Capabilities (v2)

Every number in this report comes from the reproducible benchmark and multi-engine verification harness in `eval/primitives_v2_proof/run_proof.py`, `eval/primitives_v2_proof/wasm_proof.mjs`, and `bench/engines/src/bin/primitives_v2_vs_oxide.rs`, with raw machine-readable output recorded in `eval/primitives_v2_proof/results.json`.

## Reproduction Commands

```bash
# 1. Build release binaries & zero-import wasm32 edge module
cargo build --release -p nupdf-cli
cargo build --release --manifest-path bench/engines/Cargo.toml --bin primitives_v2_vs_oxide
cargo build --release --target wasm32-unknown-unknown --manifest-path bench/wasm-edge/Cargo.toml
cargo install --locked --root target/oxide-cli pdf_oxide_cli@0.3.78   # the pdf-oxide CLI
cargo xtask oracle build                                               # mutool 1.28.6

# 2. A Python with the rival engines
python3 -m venv VENV && VENV/bin/pip install pdfplumber pymupdf pypdf pypdfium2

# 3. Run all 6 multi-engine + native pdf_oxide + V8 WebAssembly proofs
VENV/bin/python eval/primitives_v2_proof/run_proof.py \
  --nupdf target/release/nupdf \
  --mutool target/oracle/mupdf-1.28.6/build/release/mutool \
  --pdf-oxide target/oxide-cli/bin/pdf-oxide \
  --engines-bin bench/engines/target/release/primitives_v2_vs_oxide \
  --node node \
  --wasm bench/wasm-edge/target/wasm32-unknown-unknown/release/nupdf_wasm_edge.wasm \
  --out-json eval/primitives_v2_proof/results.json
```

The numbers below are from one run of these commands on the branch's tip after the review fixes (Python 3.14, Node 26). The script runs local binaries and libraries only; it starts no model session. Timings are single-machine medians and move by tens of percent between runs; the counts, classifications and exit codes do not.

---

## Executive Summary Across Engines

| Capability | `nupdf` (Native Rust) | `nupdf` (`wasm32`, 0 imports) | `pdf_oxide 0.3.78` (Native Rust) | `PyMuPDF` | `pypdf` | `pdfplumber` | `pypdfium2` | `mutool 1.28.6` |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **1. Vector Chart & Flowchart Decompiler (`0` vision tokens)** | **2 charts (`0.00000` err, `R²=1.0`) + 4-node/3-edge DAG in `0.301 ms`** | **2 charts + 1 DAG in `0.828 ms`** | `0` charts, `0` values (`0.308 ms` text only) | `0` charts | `0` charts | `0` charts (`0` tables) | `0` charts | `0` charts |
| **2. Flat-Form Synthesis, Comb Guard & `/AcroForm` Promotion** | **6/6 flat fields (`0.201 ms`), fill (`1.800 ms`), promote (`0.266 ms`)** | **6/6 flat fields in `0.432 ms`** | `0` before promote → **sees all `6` after `nupdf --promote`** | `0` before → **`6` after** | `0` before → **`6` after** | N/A | N/A | N/A |
| **3. Block Paragraph Reflow & Vertical Y-Shift (`--reflow`)** | **Wrapped in column (`x1=539.1 pt`), shifted page `+56.0 pt`, `0` overlaps (`6.209 ms`)** | **Verified reflow in `7.025 ms`** | Overflowed 612pt page right edge (`x1=612.45 pt`) | Truncated text in original rect | No content edit | Read-only | No content edit | No reflow |
| **4. Temporal `%%EOF` History & Post-Sig Tamper Forensics** | **3 revisions mounted (`6.144 ms`), RSA-2048 verified, caught content + whiteout tamper** | **3 revisions classified in `7.514 ms`** | Blindly extracted tampered `$90,000.00` (no rev/sig API) | Blindly extracted `$90,000.00` | Blindly extracted `$90,000.00` | Read-only | Read-only | Latest rev only |
| **5. Format-Preserving Reversible PII Cloak Firewall** | **6 entities / 12 replacements, `0` leaks (`2.035 ms`), exact PDF+JSON uncloak (`1.413 ms`)** | **Verified `0` leaks in `2.683 ms`** | Verified `0` leaks in `nupdf` output + 100% uncloak | Irreversible redact only | No redaction | Read-only | No redaction | Irreversible |
| **6. Single-Call Compound Pipeline & Zero-Import Edge WASM** | **5-step pipeline in `4.201 ms` (`2.92x` CLI speedup, `2.70x` smaller JSON)** | **`0` host imports, `3.34 ms` V8 compile, `5.21 ms` pipeline** | None | None | None | None | None | None |

---

## Detailed Proof Breakdown

### 1. Deterministic Vector Chart & Diagram Decompiler (`nupdf chart`)
- **Fixture:** 3-page showcase PDF where **none** of the numeric series values appear as text strings anywhere in the PDF:
  - Page 1: Grouped 2-series vertical bar chart (*"Quarterly Cloud Spend vs Revenue"*, `Revenue = [42.37, 68.91, 81.25, 94.60]`, `CloudSpend = [19.45, 27.80, 31.15, 35.90]`) with color legend swatches.
  - Page 2: Calibrated line chart (*"Net Retention Index"*, `[31.5, 54.0, 62.5, 88.0]`).
  - Page 3: 4-node directed vector flowchart (*Ingest PDF → Attest Pixels → Cloak PII → Audit Claims*) with triangle arrowheads and edge labels (`valid`, `clean`).
- **Result:**
  - `nupdf::chart::decompile_charts` ran in **`0.301 ms` in-process** (`0.828 ms` in V8 WASM, `5.27 ms` CLI) with **`0` vision tokens**, recovering all 8 grouped bar floats (`max_bar_abs_error = 0.0`), both legend series names (`Revenue`, `CloudSpend`), all 4 line vertices (`max_line_abs_error = 0.0`), `axis_r_squared = 1.0`, and all 4 nodes + 3 directed edges.
  - `pdf_oxide 0.3.78` (`0.308 ms`), `PyMuPDF`, `pypdf`, `pdfplumber`, `pypdfium2`, and `mutool` all recovered `0` chart values and `0` directed edges.

### 2. Flat-Form & Vector Checkbox Synthesis, Filling & `/AcroForm` Promotion (`nupdf flat-forms`)
- **Fixture:** Flat print-master intake PDF with **zero** `/AcroForm` dictionary entries — only 2 stroked underlines (*Applicant Name*, *Department*), 1 9-cell SSN comb grid, and 3 `10x10 pt` vector checkboxes (1 pre-checked with a vector `X` stroke, 2 unchecked).
- **Result:**
  - `pdf_oxide 0.3.78`, `PyMuPDF`, and `pypdf` all reported **`0` form fields**.
  - `nupdf flat-forms` synthesized all **`6` flat fields** in **`0.201 ms`** in-process (`5.15 ms` CLI), accurately distinguishing `existing_patient = true` from `us_citizen = false` and `hipaa_consent = false`.
  - Filling (`--set`, `--check`, `--uncheck`) succeeded with `nupdf::verify` confirmation in **`1.800 ms`**, while a 10-digit comb overflow (`1234567890` into 9 cells) was deterministically rejected with exit code **`4`**.
  - `nupdf flat-forms --promote` upgraded the flat geometry into real interactive `/AcroForm` `/Widget`s in **`0.266 ms`**, after which **native `pdf_oxide 0.3.78`, `PyMuPDF`, and `pypdf` all saw and interacted with all `6` widgets**.

### 3. Block-Scoped Paragraph Reflow & Vertical Y-Shift (`nupdf patch --reflow`)
- **Fixture:** Multi-clause contract where 2-line Clause 2 (*Indemnification*) sits above Clause 3 (*Governing Law*), a vector horizontal divider rule, and a signature block.
- **Result:**
  - Replacing Clause 2 with a 6-line legal indemnification paragraph via `pdf_oxide 0.3.78` (`DocumentEditor::replace_text`) overflowed off the right edge of the `612 pt` Letter page (`max_line_x1 = 612.452 pt`). `PyMuPDF`'s `insert_textbox` inside the original Clause 2 rectangle truncated the text.
  - `nupdf patch --reflow` word-wrapped the paragraph using exact font metrics (`max_line_x1 = 539.093 pt` in native Rust / `537.85 pt` word bound in `PyMuPDF`) and shifted Clause 3, the vector divider line, and the signature block down by **`56.0 pt`** in **`6.209 ms`** with **`0` pairwise block overlaps**, while rejecting an 80-sentence vertical page overflow with exit code **`4`** (`verify.page_overflow`).

### 4. Temporal Xref Time-Travel & Post-Signature Tamper Forensics (`nupdf history`)
- **Fixture:** Genuine 3-revision RSA-2048 SHA-256 PKCS#7 signed PDF (`cn=Elena Rostova, o=Helios Treasury`):
  - **Revision 1 (`7,326` bytes):** Signed wire authorization for `$10,000.00` + *"Dual Board Sign-Off Required Above \$25,000"* + unsigned `notes` field (`Pending`).
  - **Revision 2 (`7,928` bytes):** Countersigner fill via `nupdf forms --set notes="Approved by CFO" --incremental`.
  - **Revision 3 (`8,352` bytes):** Incremental post-signature attack painting an opaque white rectangle over the board sign-off restriction and `$10,000.00`, overlaying `Approved Wire Total: $90,000.00`.
- **Result:**
  - `pdf_oxide 0.3.78`, `PyMuPDF`, and `pypdf` all returned `$90,000.00` with no revision diffing or occlusion forensics.
  - `nupdf history` mounted all 3 revisions in **`6.144 ms`** in-process (`12.70 ms` CLI), verified the RSA-2048 signature on Revision 1 (`initial`) and Revision 2 (`clean_incremental_fill`), flagged Revision 3 as `tampered_after_signature` with both `tamper.post_signature_content_edit` and `tamper.post_signature_visual_occlusion`, exited **`4`** under `--strict`, and extracted clean Revision 1 (`--extract-revision 1`) containing only `$10,000.00`.
  - Revision 2 stays `clean_incremental_fill` because it only fills an existing field. Since the per-page fingerprint change on this branch, an annotation (other than a field fill or a new signature) added after a signature, or a change to a page's images, fonts or vectors, is `tampered_after_signature` too, and a page too large to fingerprint is `tamper.post_signature_unchecked`; this fixture exercises neither.

### 5. Reversible Format-Preserving PII Cloak Firewall (`nupdf cloak` / `nupdf uncloak`)
- **Fixture:** Mutual NDA & Escrow PDF containing repeated party names (*Evelyn Vance*, *Marcus Sterling*), two SSNs, email, phone number, an AcroForm widget holding PII, and `/Info` Title/Author metadata.
- **Result:**
  - `nupdf cloak` replaced **`6` entities (`12` occurrences)** in **`2.035 ms`** in-process (`7.91 ms` CLI) with coreference-preserving tokens (`[PERSON_01]`, `[PERSON_02]`, `000-00-0001`, `000-00-0002`, `user1@example.invalid`), scrubbed unreachable objects (`garbage: 4`), and verified **`0` leaks** across `pdf_oxide 0.3.78`, `PyMuPDF` (text + widgets + metadata), `pypdf` (text + metadata), and ` cloak` stdout.
  - `nupdf uncloak` deterministically restored both a simulated cloud-agent JSON summary (`--text-mode`, `4.85 ms` CLI) and the PDF itself (`1.413 ms` in-process, `6.31 ms` CLI) with 100% fidelity verified by `pdf_oxide 0.3.78` and `PyMuPDF`.

### 6. Single-Call Compound Agent Pipeline (`nupdf pipeline`) & Zero-Import `wasm32` Edge Engine
- **Compound Pipeline:** Executing a 5-step workflow (`info → attest gate → query right_of "Contract Value:" → ${q_val.first_id} patch "$49,500.00" → audit`) in one `nupdf pipeline` call took **`4.201 ms` in-process** (`9.95 ms` CLI vs `29.08 ms` across 5 sequential CLI calls = **`2.92x` wall-time speedup**) and reduced stdout JSON context bytes from `5,063` to `1,873` (**`2.70x` reduction**), while halting with exit **`4`** before any write when an `attest` gate tripped on hidden prompt injection.
- **Zero-Import `wasm32-unknown-unknown` Edge Engine (`bench/wasm-edge`):**
  - Compiled `nupdf` + all 11 agent primitives to `wasm32-unknown-unknown` (`7,098,097` bytes).
  - Verified live in Node.js V8 (`eval/primitives_v2_proof/wasm_proof.mjs`): **`WebAssembly.Module.imports(mod).length === 0`** (zero WASI, zero filesystem, zero network, zero clock/env imports — pure `[memory, wasm_cmd_buffer, wasm_invoke, wasm_pdf_buffer, wasm_result_len, wasm_result_ptr]`), `3.336 ms` V8 compile, `0.249 ms` instantiate, `7.94 MB` linear memory after running `chart` (`0.828 ms`), `flat_forms` (`0.432 ms`), `reflow_patch` (`7.025 ms`), `cloak` (`2.683 ms`), `history` (`7.514 ms`), and `pipeline` (`5.210 ms`).
  - The module owns its buffers: the host asks for one of a length (`wasm_pdf_buffer`, `wasm_cmd_buffer`), writes at the address returned, calls `wasm_invoke()`, and reads the result by `wasm_result_ptr`/`wasm_result_len`. The module builds no slice from a host pointer; its only `unsafe` is the export attribute.
