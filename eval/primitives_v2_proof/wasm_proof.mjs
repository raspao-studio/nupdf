#!/usr/bin/env node
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Live V8 WebAssembly zero-import proof & microbenchmark for `nupdf_wasm_edge.wasm`.
// Instantiates the compiled `wasm32-unknown-unknown` binary with `{}` (strictly
// ZERO host imports — no WASI, no libc, no JS glue) and executes all 6 v2
// capabilities + v1 primitives inside linear memory.

import { readFileSync } from "node:fs";
import { performance } from "node:perf_hooks";

const [wasmPath, chartPdfPath, flatFormPdfPath, reflowPdfPath, historyPdfPath, cloakPdfPath] =
  process.argv.slice(2);

if (!wasmPath || !chartPdfPath) {
  console.error(
    "Usage: node wasm_proof.mjs <wasm> <chart.pdf> <flat_form.pdf> <reflow.pdf> <history.pdf> <cloak.pdf>"
  );
  process.exit(2);
}

const wasmBytes = readFileSync(wasmPath);
const tCompile0 = performance.now();
const mod = new WebAssembly.Module(wasmBytes);
const compileMs = performance.now() - tCompile0;

const imports = WebAssembly.Module.imports(mod);
const exportsList = WebAssembly.Module.exports(mod).map((e) => `${e.kind}:${e.name}`);

if (imports.length !== 0) {
  throw new Error(`Expected 0 host imports, got ${imports.length}: ${JSON.stringify(imports)}`);
}

// Instantiate with an empty `{}` import object: proves zero WASI / OS / JS dependencies.
const tInst0 = performance.now();
const instance = new WebAssembly.Instance(mod, {});
const instantiateMs = performance.now() - tInst0;

const { memory, wasm_alloc, wasm_dealloc, wasm_invoke, wasm_result_ptr, wasm_result_len } =
  instance.exports;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

function callEdge(pdfBuf, cmdObj) {
  const cmdBytes = encoder.encode(JSON.stringify(cmdObj));
  const pdfPtr = wasm_alloc(pdfBuf.length);
  const cmdPtr = wasm_alloc(cmdBytes.length);
  try {
    new Uint8Array(memory.buffer, pdfPtr, pdfBuf.length).set(pdfBuf);
    new Uint8Array(memory.buffer, cmdPtr, cmdBytes.length).set(cmdBytes);
    const rc = wasm_invoke(pdfPtr, pdfBuf.length, cmdPtr, cmdBytes.length);
    if (rc !== 0) {
      throw new Error(`wasm_invoke returned ${rc}`);
    }
    const resPtr = wasm_result_ptr();
    const resLen = wasm_result_len();
    const jsonText = decoder.decode(new Uint8Array(memory.buffer, resPtr, resLen));
    return JSON.parse(jsonText);
  } finally {
    wasm_dealloc(pdfPtr, pdfBuf.length);
    wasm_dealloc(cmdPtr, cmdBytes.length);
  }
}

function benchCall(pdfBuf, cmdObj, iters = 7) {
  const times = [];
  let last = null;
  for (let i = 0; i < iters; i++) {
    const t0 = performance.now();
    last = callEdge(pdfBuf, cmdObj);
    times.push(performance.now() - t0);
    if (!last.ok) {
      throw new Error(`WASM edge error on ${cmdObj.op}: ${last.error}`);
    }
  }
  times.sort((a, b) => a - b);
  return {
    median_ms: Number(times[Math.floor(times.length / 2)].toFixed(3)),
    result: last.result,
  };
}

const chartBuf = readFileSync(chartPdfPath);
const flatBuf = readFileSync(flatFormPdfPath);
const reflowBuf = readFileSync(reflowPdfPath);
const historyBuf = readFileSync(historyPdfPath);
const cloakBuf = readFileSync(cloakPdfPath);

const chartRun = benchCall(chartBuf, { op: "chart", pages: null });
const flatRun = benchCall(flatBuf, { op: "flat_forms", pages: null });
const reflowRun = benchCall(reflowBuf, {
  op: "patch",
  target_id: "p1.b2",
  new_text:
    "2. Indemnification. Provider shall defend, indemnify, and hold harmless Customer and its affiliates from all third-party claims, damages, liabilities, regulatory fines, and reasonable attorneys fees arising out of intellectual property infringement, security breaches, or gross negligence under this Agreement.",
  reflow: true,
});
const cloakRun = benchCall(cloakBuf, { op: "cloak", presets: ["pii"] });
const historyRun = benchCall(historyBuf, { op: "history", run_attest: true });
const pipelineRun = benchCall(reflowBuf, {
  op: "pipeline",
  spec: {
    compact: true,
    steps: [
      { op: "info", id: "i1" },
      { op: "query", id: "q1", right_of: "Agreement" },
      {
        op: "patch",
        id: "p1",
        reflow: true,
        replacements: [
          {
            id: "p1.b2",
            text: "2. Indemnification. Expanded edge-WASM reflowed paragraph verified inside V8 linear memory with zero host syscalls and zero layout collisions.",
          },
        ],
      },
      {
        op: "audit",
        id: "a1",
        claims: [{ claim_id: "c1", page: 1, quote: "Expanded edge-WASM reflowed paragraph" }],
      },
    ],
  },
});

const summary = {
  wasm_size_bytes: wasmBytes.length,
  wasm_compile_ms: Number(compileMs.toFixed(3)),
  wasm_instantiate_ms: Number(instantiateMs.toFixed(3)),
  host_imports_count: imports.length,
  host_imports: imports,
  exports: exportsList,
  linear_memory_mb_after_all_calls: Number((memory.buffer.byteLength / (1024 * 1024)).toFixed(2)),
  benchmarks: {
    chart: {
      median_ms: chartRun.median_ms,
      charts_found: chartRun.result.charts.length,
      diagrams_found: chartRun.result.diagrams.length,
    },
    flat_forms: {
      median_ms: flatRun.median_ms,
      fields_found: flatRun.result.fields.length,
    },
    reflow_patch: {
      median_ms: reflowRun.median_ms,
      verified: reflowRun.result.report.verification?.verified ?? false,
      patched_bytes_len: reflowRun.result.patched_bytes_len,
    },
    cloak: {
      median_ms: cloakRun.median_ms,
      verified_zero_leaks: cloakRun.result.report.verified_zero_leaks,
      entities_cloaked: cloakRun.result.report.entities_cloaked,
      replacements: cloakRun.result.report.replacements,
    },
    history: {
      median_ms: historyRun.median_ms,
      revision_count: historyRun.result.revision_count,
      tampered: historyRun.result.tampered,
      classifications: historyRun.result.revisions.map((r) => r.classification),
    },
    pipeline: {
      median_ms: pipelineRun.median_ms,
      mutated: pipelineRun.result.mutated,
      steps_completed: pipelineRun.result.steps.length,
      audit_verified: pipelineRun.result.steps[3]?.data?.all_verified ?? false,
    },
  },
};

console.log(JSON.stringify(summary, null, 2));
