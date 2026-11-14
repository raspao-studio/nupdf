# The agent eval

Can an agent with only one PDF tool finish real PDF work, and what does it cost it? Each run gives a small model one task and one tool, and a grader that never uses nupdf checks the result. Measured on 2026-11-13.

Thirty tasks, eight arms, five runs of each task per arm, one small-model agent (claude-haiku-5-5) per run: 1,045 runs. The nupdf binary is main at 1abb7e5 (with `extract --format layout`) plus raspao-studio/nupdf#62 (radio groups filled by name, files on FileAttachment annotations, columns drawn line by line across). The graded results are in `results/m10/r1` to `r5`; [how to rerun](#how-to-rerun) is below.

The arms:
- **native**: the model reads the pages as images.
- **python**: pypdf and pdfplumber, the default Python PDF skill.
- **pymupdf4llm**: the library with PyMuPDF.
- **pymupdf4llm-mcp** 1.28.0: its MCP server.
- **pdf_oxide**: PDF Oxide 0.3.78's Python bindings.
- **pdf_oxide-mcp**: PDF Oxide's MCP server 0.3.78.
- **nupdf**: the CLI.
- **nupdf-mcp**: nupdf's MCP server.

The two MCP servers of other tools only extract, so like the native arm they skip the nine tasks that write a file. LiteParse, an arm of the pilot, is not one here.

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

## How to rerun

The runs start paid model sessions.

```
python3 eval/agents/setup.py --corpus ~/.cache/nupdf/corpus --redaction corpus/redaction --smoke corpus/smoke --mutool MUTOOL --out DIR
python3 eval/agents/make_tasks.py --out DIR && python3 eval/agents/make_more.py --out DIR --mutool MUTOOL
python3 eval/agents/prepare.py --out DIR --venv VENV --lit LIT --nupdf NUPDF
# give each DIR/runs/<arm>-<task>/PROMPT.txt to a fresh agent, then:
python3 eval/agents/grade.py --out DIR --mutool MUTOOL
python3 eval/agents/metrics.py --map agents.tsv --transcripts TRANSCRIPTS
python3 eval/agents/tables.py DIR1 DIR2 DIR3 DIR4 DIR5
```

Graders use pdftotext, pdfinfo, qpdf and MuPDF, never nupdf. Agent transcripts are not committed: they hold local paths and system prompts. `results/` has what was taken from them.

## Earlier runs

Two smaller rounds came before this one, and their graded results are kept under `agents/results` beside the full eval's: a pilot on 2026-11-11 (nine tasks, six arms, one run each) and a round on 2026-11-12 (eleven tasks, three runs each, on nupdf's two arms). Neither had enough runs a task to compare arms, which is why this one has five.

## Two measurements beside the eval

Not agent runs; each has its own script and results folder.

- **Files built to exhaust a reader** (`python3 eval/hostile/make.py DIR`, then `eval/hostile/run.py`; `hostile/results`). Seven files, each tool asked for all the text. No tool crashed on any. nupdf holds a decoded stream in memory, so on a content stream that inflates to 512 MB it peaked at 545 MB, where MuPDF peaked at 5 MB; PDF Oxide peaked at 1,917 MB and LiteParse at 1,152 MB.
- **One long file, one thread or eight** (`eval/parallel/run.py`; `parallel/results`). The text of all 352 pages of Free Culture, median of five runs on an 18-core machine: nupdf 0.144 s on one thread and 0.137 s on eight, PyMuPDF 1.041 s in one process. At this size threads add nothing.
