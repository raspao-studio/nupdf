# nupdf

A memory-safe, parallel PDF engine written in Rust from MuPDF's design, with an interface built for AI agents.

- **Faithful.** MuPDF is the reference. Every subsystem is checked file by file against a pinned MuPDF build, and parity is ratcheted in CI.
- **Safe.** No `unsafe`, no panics on input, explicit limits, continuous fuzzing.
- **Parallel.** A parsed document is shared across threads, so pages are processed concurrently.
- **Agent-first.** Versioned JSON reports, stable ids for every span of text, diagnostics as data, and redaction that proves its own result.

**Status: milestone M0 (foundation).** Only `nupdf info` (header facts) exists. See [the plan](docs/PLAN.md).

## Try it

```bash
cargo run -p nupdf-cli -- info corpus/smoke/garbage-prefix.pdf --pretty
```

Compare against MuPDF:

```bash
cargo xtask oracle build     # builds the pinned MuPDF 1.28.6 into target/oracle
cargo xtask harness          # differential checks on the smoke corpus
cargo xtask bench            # paired timings and peak memory
```

## Docs

| | |
|---|---|
| [RESULTS](docs/RESULTS.md) | What nupdf is measured to do: parity, safety, speed, agents, and where it falls short |
| [PLAN](docs/PLAN.md) | Milestones M0–M10, work items, exit criteria |
| [ARCHITECTURE](docs/ARCHITECTURE.md) | Layers, module-to-MuPDF map, data model, concurrency, safety |
| [AGENT-INTERFACE](docs/AGENT-INTERFACE.md) | The JSON contract, addressing, exit codes, the redaction contract |
| [TESTING](docs/TESTING.md) | Oracle, corpora, checks, ratchet, benchmarks, fuzzing, agent eval |
| [BENCHMARKS](docs/BENCHMARKS.md) | Speed against MuPDF and pdf_oxide, with the machine and versions |
| [DECISIONS](docs/DECISIONS.md) | Why things are the way they are |
| [PRIOR-ART](docs/PRIOR-ART.md) | What else exists, and what is new here |

## License

AGPL-3.0-or-later. nupdf is derived from MuPDF by Artifex Software, Inc.; see [NOTICE](NOTICE.md).
