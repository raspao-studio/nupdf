# What nupdf can do for agents that its rivals cannot

Measured on 2026-11-11. The question: which one to three things can nupdf do for AI agents that MuPDF, pypdf, pdf_oxide, LiteParse and Docling could not do without changing their architecture?

**Answer: on this evidence, none that an agent felt.** Two properties of nupdf survive a reading of the rivals' source as things they cannot add in a quarter (a memory-safe core, and one parsed document shared across threads). Neither changed an outcome in 50 graded agent runs or in the two experiments built to test them. The arm built on PyMuPDF passed every task. What nupdf did do, measurably, is let an agent finish redaction with a third to a half of the code the Python arms wrote, and never report a leaking file as clean; a rival could ship both in weeks.

## How to rerun

```
python3 eval/agents/setup.py --corpus ~/.cache/nupdf/corpus --redaction corpus/redaction --smoke corpus/smoke --mutool MUTOOL --out DIR
python3 eval/agents/make_tasks.py --out DIR && python3 eval/agents/make_more.py --out DIR --mutool MUTOOL
python3 eval/agents/prepare.py --out DIR --venv VENV --lit LIT --nupdf NUPDF
# give each DIR/runs/<arm>-<task>/PROMPT.txt to a fresh agent, then:
python3 eval/agents/grade.py --out DIR --mutool MUTOOL
python3 eval/agents/metrics.py --map agents.tsv --transcripts TRANSCRIPTS
python3 eval/hostile/make.py HDIR && python3 eval/hostile/run.py --files HDIR --nupdf NUPDF --mutool MUTOOL --python VENV/bin/python --lit LIT
python3 eval/parallel/run.py --pdf freeculture.pdf --nupdf NUPDF --python VENV/bin/python
```

Graders use pdftotext, pdfinfo, qpdf and MuPDF, never nupdf. Results are in `eval/*/results/`.

## 1. The agent eval

Six arms, nine tasks, one small-model agent (claude-haiku-5-5) per arm and task, each told it may touch PDFs with its arm's tool only. 50 runs; four arm and task pairs were not run because the arm cannot write a PDF at all.

| Arm | locate | quotes | corpus | hidden | longdoc | damaged | merge | redact | redact2 | Passed | Tool calls | Context added | Output tokens |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| native (the model reads the PDF) | pass | pass | pass | pass | pass | gave up | n/a | n/a | n/a | 5 / 6 | 9.5 | 34,029 | 3,122 |
| python (pypdf, pdfplumber) | pass | pass | pass | pass | pass | 0.25 | pass | pass | pass | 8 / 9 | 7.7 | 14,633 | 5,757 |
| pymupdf4llm (with PyMuPDF) | pass | pass | pass | pass | pass | pass | pass | pass | pass | 9 / 9 | 8.3 | 14,484 | 4,650 |
| pdf_oxide 0.3.78 | pass | pass | gave up | pass | pass | 0.75 | 1.00* | 0.80 | 0.40 | 4 / 9 | 11.9 | 26,685 | 8,153 |
| LiteParse 2.18.0 | pass | pass | pass | pass | pass | 0.75 | gave up | gave up | n/a | 5 / 8 | 7.4 | 11,731 | 2,661 |
| nupdf | pass | pass | pass | pass | pass | 0.88 | pass | pass | pass | 8 / 9 | 8.3 | 18,496 | 3,212 |

Scores are the grader's, 0 to 1; "pass" is 1. Tool calls, context added and output tokens are means per run. Tokens are the API's own `usage` figures: "context added" is the last request's input minus the first's, which leaves out the system prompt every arm shares. `*` the agent wrote a correct file and then reported failure because it could not read its own result back.

The tasks: find a sentence and its page in a 40-page file (locate); check three citations, one altered and one on the wrong page (quotes); say which of twelve files mention a word (corpus); summarise a memo that carries an instruction in invisible text (hidden); find a phrase in a 352-page book (longdoc); triage nine damaged or locked files (damaged); merge pages and set a title (merge); remove a secret from five and then ten files that hide it in different places (redact, redact2).

What the table does not show:

- **Every arm ignored the hidden instruction.** The model, not the tool, did that. Hidden-text flagging would not have changed a result here.
- **Every arm found the phrase in 352 pages**, the native arm included, at 62,292 tokens of added context against 9,734 to 22,708 for the tools.
- **nupdf was not the cheapest in context.** The two Python arms and LiteParse added less. nupdf's JSON is larger than a Python script's printed answer.
- **nupdf wrote the least code.** On the two redaction tasks its agent produced 3,566 and 4,731 output tokens. The pypdf arm produced 11,249 and 16,854, the PyMuPDF arm 7,998 and 11,017, and pdf_oxide 15,162 and 23,045. All of nupdf's and both Python arms' outputs were clean by the independent grader.

## 2. Failures, coded by cause

11 of 50 runs did not pass. A run can carry more than one cause.

| Cause | Runs | Context spent | Tool calls | Where |
|---|---|---|---|---|
| C1. The tool cannot write, or has no call for the thing | 4, and the 4 pairs not run | 113,420 | 47 | LiteParse merge and redact; pdf_oxide redact (dropped every annotation) and redact2 (no call for outlines, page labels, a form XObject or structure text) |
| C2. A damaged file is read worse than the reference readers read it | 5 | 86,416 | 56 | pypdf wrong on 6 of 8 files; LiteParse and pdf_oxide on 2 each; the native arm had all nine rejected; nupdf's agent on 1 |
| C3. The agent could not confirm its own result | 3 | 111,288 | 44 | pdf_oxide: gave up on a correct merge and on a correct search, and reported two leaking redactions as clean |

Notes on the coding:

- C3 is the only cause that produced a wrong answer presented as right: two pdf_oxide redactions were listed as clean while the secret was still in the file's bytes.
- nupdf's one miss is an agent reading honest output badly. `nupdf pages` says the file declares 3 pages and only 1 can be found; the agent answered 1, and poppler, qpdf and MuPDF all say 3.
- The damaged fixtures carry labels such as "No xref" as their page text. The pypdf agent took those for damage residue and discarded them, so its 0.25 overstates pypdf's weakness. That is a flaw in the fixtures.

## 3. Which constraints are architectural

From reading each rival's source. Port cost is a judgement: what it would take a competent maintainer to add.

| Capability | MuPDF 1.28.6, PyMuPDF | pypdf | pdf_oxide | LiteParse | Docling |
|---|---|---|---|---|---|
| Ids for text that another call accepts | Block ids are per parse (`include/mupdf/fitz/structured-text.h:448`). Weeks | No text model (`pypdf/_page.py:2200`). A quarter | Page and index only (`src/layout/text_block.rs:24`). Days | None (`crates/liteparse/src/types.rs:62`). Days | Positional references. Weeks |
| Redaction that re-checks its output | Removes content, no check (`source/pdf/pdf-clean.c:1199`). Weeks | No redaction call. A quarter | Removes content, no check (`src/redaction/`). Weeks | Read-only. A quarter | Read-only. More than a quarter |
| Flagged truncation, cursors | None. Days | None. Days | None. Days | None. Days | Page limits exist |
| Same bytes every time | Random /ID and IVs (`source/pdf/pdf-write.c:2198`). Days | Random /ID (`pypdf/_writer.py:1266`). Days | Timestamps (`xmp_metadata.rs:318`). Days | No writer | No writer |
| Says which text is invisible | Detectable from flags. Days | Not tracked. Weeks | Tracked, not exposed (`graphics_state.rs:239`). Weeks | Dropped, not exposed (`extract.rs:870`). Days | Has a visible-text call |
| One document used from several threads | "No simultaneous calls" on a context (`docs/reference/c/overview.md:194`); PyMuPDF "does not support running on multiple threads" (`docs/recipes-multiprocessing.rst:19`). **More than a quarter** | No locks found. Weeks | `Send + Sync` behind one mutex (`src/document.rs:379`). Weeks | One process-wide lock on PDFium (`crates/pdfium/src/library.rs:24`). **More than a quarter** | One lock on PDFium |
| Memory-safe parsing core | C. **More than a quarter** | Python, safe | Rust, about 5 `unsafe` outside FFI | C++ (PDFium). **More than a quarter** | C++ backends. **More than a quarter** |

By the rule "discard anything a rival could ship in a quarter", ids, verified redaction, budgets, determinism and hidden-text flags are all discarded. Two rows survive, and neither survives against pdf_oxide, which is also Rust and also shares a document across threads.

CVE records support the memory-safety row for the C and C++ cores: MuPDF CVE-2026-3308 and CVE-2026-25556 (both fixed before the pinned 1.28.6), PDFium CVE-2026-2648, CVE-2026-6305 and CVE-2026-10002. pypdf's recent CVEs are resource exhaustion, not memory corruption. These were collected by web search and not checked against NVD entry by entry.

## 4. The two surviving claims, tested

**Claim A: on hostile input, nupdf neither crashes nor exceeds a fixed memory bound, and the C-core tools do one or the other.** It would be disproved if no rival crashed, or if a rival stayed lower. Seven files built to exhaust a reader (`eval/hostile/make.py`), each tool asked for all the text, one process each.

Result: **disproved on this set.** No tool was killed by a signal on any file. On a content stream that inflates to 512 MB, MuPDF peaked at 5 MB and nupdf at 545 MB, because nupdf holds the decoded stream. pdf_oxide peaked at 1,917 MB and LiteParse at 1,152 MB; on ten million text operators LiteParse reached 4,908 MB. pypdf exited with an error on three files. Memory safety remains true of nupdf's code, but this experiment found no input where it changed what happened, and found one where nupdf is 100 times worse than the engine it follows.

**Claim B: sharing one parsed document across threads makes nupdf faster on a long file than a tool that cannot.** It would be disproved if threads gave no speed-up, or if separate processes did as well. Text of all 352 pages of Free Culture, median of five runs on an 18-core machine.

Result: **no speed-up to claim.** nupdf took 0.144 s on one thread and 0.137 s on eight. PyMuPDF took 1.041 s in one process and 2.451 s in eight. nupdf is seven times faster than PyMuPDF started from Python, on one thread; the threads add nothing at this size.

No new nupdf code was written for these: both properties already exist, and neither experiment gave a reason to build on them.

## 5. Recommendation

There is no architectural wedge to build the product on, and the site should not claim one.

What the evidence supports instead is an ergonomic lead that has to be kept by shipping, not by structure:

1. **Lead with "the agent does not have to write or check the code".** The redaction result is the one place nupdf beat every arm on a measured cost with equal correctness, and `verify` is why its agent never guessed. PyMuPDF can match it in weeks, so it is a head start, not a moat.
2. **Fix the two things this found.** The 545 MB decoded stream contradicts the "bounded on hostile input" story. `pages` output led an agent to the wrong page count.
3. **Stop building toward thread scaling and hostile-input claims as selling points** until a measurement shows a case where they change an outcome. Stop describing ids, budgets, determinism and hidden-text flags as things rivals cannot do.
4. **Treat PyMuPDF as the rival, not pypdf.** It passed everything here and is what `pymupdf4llm` agents already use.

## What this did not establish

- One run per arm and task, one model, no repeats: differences of a single task are noise.
- A stronger or weaker model may shift every row.
- The nupdf binary was built from an unmerged working tree that has `redact` and `verify`; `main` does not have them yet.
- The source-code findings and port costs come from two reading passes; five citations were spot-checked and held. Docling's backends and document model were not read.
- The native arm is one agent harness's PDF reader, not every product's.
- Agent transcripts are not committed: they hold local paths and system prompts. `results/metrics.json` has what was taken from them.

## Token follow-up (M10, 2026-11-12)

The pilots showed nupdf's MCP arm calling once per file (54 calls on the PII task) and never seeing the guide. M10 changed the MCP server: its instructions became the guide, and `file` takes a list, run through `batch` in one call. Measured on Haiku 5.5, the 11 tasks, three runs of each task per arm, against the M8 build:

| Build | Arm | Solved | Median tokens per solved task | Calls per run |
|---|---|---|---|---|
| M8 | CLI | 33/33 | 14,515 | 9.2 |
| M10 | CLI | 33/33 | 16,699 | 10.0 |
| M10, M8-length guide and instructions | CLI | 33/33 | 16,185 | 10.7 |
| M8 | MCP | 33/33 | 12,557 | 17.9 |
| M10 | MCP | 33/33 | 15,671 | 12.6 |
| M10, M8-length guide and instructions | MCP | 33/33 | 14,925 | 13.5 |

- File lists did what they were for: MCP calls per run fell by about 30%, and by two thirds on the many-file tasks.
- Tokens did not fall; they rose by 10–25%. The commands' own output is the same size as M8's (`info` adds 60–150 bytes). The growth is in what agents choose to do: they render pages and read the images more often (`Read` 6.7k characters per run at M8, 15.2k now), most of it on the invoices task, and read a little more help.
- Shortening the guide and the MCP instructions back to M8's length recovered little, so their length is not the main cause.
- Three runs a task is not enough to tell an effect of the build from the model's variance on these numbers; one run a task, as in the pilots, is far from enough (the first M10 run looked 40% worse on the CLI arm, the three-run median 15%).

Next to try: make the text route clearly cheaper than rendering for layout questions (the invoices task reads fields by position), and measure with five or more runs a task.

## The full eval (M10, 2026-11-13)

Thirty tasks, eight arms, five runs of each task per arm, one small-model agent (claude-haiku-5-5) per run: 1,045 runs. The nupdf binary is main at 1abb7e5 (with `extract --format layout`) plus raspao-studio/nupdf#62 (radio groups filled by name, files on FileAttachment annotations, columns drawn line by line across). Rerun with the commands at the top, then `python3 eval/agents/tables.py DIR1 ... DIR5`; the graded results are in `results/m10/r1` to `r5`.

The arms:
- **native**: the model reads the pages as images.
- **python**: pypdf and pdfplumber, the default Python PDF skill.
- **pymupdf4llm**: the library with PyMuPDF.
- **pymupdf4llm-mcp** 1.28.0: its MCP server.
- **pdf_oxide**: PDF Oxide 0.3.78's Python bindings.
- **pdf_oxide-mcp**: PDF Oxide's MCP server 0.3.78.
- **nupdf**: the CLI.
- **nupdf-mcp**: nupdf's MCP server.

The two MCP servers of other tools only extract, so like the native arm they skip the nine tasks that write a file. LiteParse, in the pilots, is left out: it is no arm of the plan's.

| Arm | Runs | Passed | Mean score | Gave up | Median tokens, passed | Median calls | Median wall s |
|---|---|---|---|---|---|---|---|
| native | 85 | 75 (88%) | 0.922 | 4 | 17,402 | 4 | 12 |
| python | 150 | 145 (97%) | 0.972 | 1 | 11,823 | 7 | 21 |
| pymupdf4llm | 150 | 147 (98%) | 0.986 | 0 | 12,805 | 7 | 23 |
| pymupdf4llm-mcp | 105 | 38 (36%) | 0.426 | 55 | 4,617 | 7 | 23 |
| pdf_oxide | 150 | 108 (72%) | 0.848 | 21 | 20,858 | 10 | 30 |
| pdf_oxide-mcp | 105 | 79 (75%) | 0.793 | 20 | 15,436 | 10 | 20 |
| nupdf | 150 | 150 (100%) | 1.000 | 0 | 14,578 | 7 | 16 |
| nupdf-mcp | 150 | 150 (100%) | 1.000 | 0 | 8,988 | 7 | 13 |

Tokens are the run's context added plus its output tokens; medians are over passed runs. Wall time is the session's, model time included.

| Task | native | python | pymupdf4llm | pymupdf4llm-mcp | pdf_oxide | pdf_oxide-mcp | nupdf | nupdf-mcp |
|---|---|---|---|---|---|---|---|---|
| attachments | – | 5/5 | 5/5 | – | 0/5 | – | 5/5 | 5/5 |
| bookmarks | – | 5/5 | 5/5 | – | 5/5 | – | 5/5 | 5/5 |
| bundle | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 |
| claim | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 |
| clause | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 |
| columns | 5/5 | 5/5 | 5/5 | 2/5 | 5/5 | 5/5 | 5/5 | 5/5 |
| comments | – | 5/5 | 5/5 | 0/5 | 5/5 | 0/5 | 5/5 | 5/5 |
| corpus | 5/5 | 5/5 | 5/5 | 0/5 | 5/5 | 5/5 | 5/5 | 5/5 |
| damaged | 4/5 | 4/5 | 4/5 | 0/5 | 0/5 | 0/5 | 5/5 | 5/5 |
| diff | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 |
| figure | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 |
| fill | – | 5/5 | 5/5 | – | 0/5 | – | 5/5 | 5/5 |
| formread | 5/5 | 5/5 | 4/5 | 0/5 | 5/5 | 0/5 | 5/5 | 5/5 |
| hidden | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 |
| invoices | 5/5 | 5/5 | 5/5 | 0/5 | 5/5 | 5/5 | 5/5 | 5/5 |
| links | – | 5/5 | 5/5 | 0/5 | 4/5 | 5/5 | 5/5 | 5/5 |
| locate | 5/5 | 5/5 | 5/5 | 0/5 | 5/5 | 5/5 | 5/5 | 5/5 |
| longdoc | 5/5 | 5/5 | 5/5 | 0/5 | 5/5 | 5/5 | 5/5 | 5/5 |
| merge | – | 5/5 | 5/5 | – | 3/5 | – | 5/5 | 5/5 |
| password | – | 5/5 | 5/5 | 0/5 | 5/5 | 5/5 | 5/5 | 5/5 |
| pii | – | 5/5 | 5/5 | – | 0/5 | – | 5/5 | 5/5 |
| quotes | 5/5 | 5/5 | 4/5 | 0/5 | 5/5 | 5/5 | 5/5 | 5/5 |
| redact | – | 5/5 | 5/5 | – | 0/5 | – | 5/5 | 5/5 |
| redact2 | – | 3/5 | 5/5 | – | 1/5 | – | 5/5 | 5/5 |
| rotate | – | 5/5 | 5/5 | – | 5/5 | – | 5/5 | 5/5 |
| sanitize | – | 4/5 | 5/5 | – | 0/5 | – | 5/5 | 5/5 |
| scans | 1/5 | 4/5 | 5/5 | 1/5 | 5/5 | 4/5 | 5/5 | 5/5 |
| signature | – | 5/5 | 5/5 | 0/5 | 0/5 | 0/5 | 5/5 | 5/5 |
| sizes | 0/5 | 5/5 | 5/5 | 0/5 | 5/5 | 0/5 | 5/5 | 5/5 |
| table | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 | 5/5 |

Each arm against nupdf's, per task, over the tasks both passed at least once: the geometric mean of the ratio of the medians (above 1x, nupdf spent less).

| Arm | Against | Tasks both passed | Tokens | Calls | Wall time |
|---|---|---|---|---|---|
| native | nupdf-mcp | 16 | 1.61x | 0.86x | 1.13x |
| python | nupdf-mcp | 30 | 1.28x | 0.94x | 1.70x |
| pymupdf4llm | nupdf-mcp | 30 | 1.16x | 0.91x | 1.61x |
| pymupdf4llm-mcp | nupdf-mcp | 9 | 1.21x | 1.07x | 1.89x |
| pdf_oxide | nupdf-mcp | 23 | 2.14x | 1.29x | 1.83x |
| pdf_oxide-mcp | nupdf-mcp | 16 | 1.56x | 1.61x | 1.63x |
| native | nupdf | 16 | 1.28x | 0.92x | 0.98x |
| python | nupdf | 30 | 0.95x | 0.99x | 1.42x |
| pymupdf4llm | nupdf | 30 | 0.86x | 0.97x | 1.35x |
| pymupdf4llm-mcp | nupdf | 9 | 0.74x | 0.92x | 1.27x |
| pdf_oxide | nupdf | 23 | 1.61x | 1.34x | 1.55x |
| pdf_oxide-mcp | nupdf | 16 | 1.17x | 1.60x | 1.38x |

What it shows:
- **Only nupdf solved everything:** 150 of 150 runs on both arms. pymupdf4llm passed 147 and the python arm 145; PDF Oxide's bindings 108, failing every run of attachments, fill, pii, redact, sanitize, signature and damaged, and its MCP server 79 of 105. pymupdf4llm's MCP server passed 38 of 105: its one tool converts a file to Markdown without page boundaries, passwords, annotations or metadata, so agents could not say on which page anything was.
- **Over MCP nupdf spends the least:** on the same tasks, 1.16x (pymupdf4llm) to 2.14x (PDF Oxide's bindings) the tokens are the other arms', and 1.13x to 1.89x the wall time.
- **The CLI does not**: the pymupdf4llm and python arms spend 0.86x and 0.95x its tokens; it is still faster (1.35x and 1.42x).
- **Tool calls are even**: 7 a run at the median for nupdf, python and pymupdf4llm alike; the native, python and pymupdf4llm arms make 6% to 14% fewer calls than nupdf over MCP on the same tasks. Only PDF Oxide's arms make clearly more.
- **The invoices task no longer costs nupdf**: `extract --format layout` keeps labels beside their values, and no nupdf run rendered a page to read it.

Limits: one model; three runs left a file in the shared `runs/` folder, outside their own (two of them python-arm runs), which the grader ignores; a task's tokens vary between its five runs by a median of 1.6x from least to most (2.7x at the 90th percentile), which per-task medians damp but do not remove.
