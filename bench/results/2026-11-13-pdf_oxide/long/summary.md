# nupdf, MuPDF and pdf_oxide on `long` (30 files, median of 3 runs)

pdf_oxide CLI: pdf-oxide 0.3.78. pdf_oxide and Python: 0.3.78 3.12.11. MuPDF: mutool version 1.28.6.

| Operation | Files | MuPDF / nupdf | Files in it | CLI / nupdf | Of totals | Files in it | Python / nupdf | In process | Of totals | Files in them | Failed: MuPDF | nupdf | CLI | Python | Disagree: CLI | Python | Units, nupdf / CLI | nupdf / Python |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| info | 30 | 0.17x | 30 | 0.18x | 0.05x | 30 | 1.32x | 0.02x | 0.01x | 30 | 0 | 0 | 0 | 0 | 0 | 0 | 3414 / 3414 | 3414 / 3414 |
| pages | 30 | 1.54x | 30 | - | - | - | 12.76x | 8.05x | 7.38x | 30 | 0 | 0 | - | 0 | - | 0 | - | 3414 / 3414 |
| text | 30 | 1.52x | 20 | 1.57x | 1.37x | 30 | 3.63x | 1.67x | 1.41x | 30 | 10 | 0 | 0 | 0 | 0 | 0 | 2033184 / 1987292 | 2033184 / 1987292 |
| text-fast | 30 | 1.50x | 20 | - | - | - | 3.54x | 1.43x | 1.35x | 30 | 10 | 0 | - | 0 | - | 0 | - | 2033184 / 1985906 |
| render | 30 | 1.49x | 20 | - | - | - | 3.06x | 1.45x | 0.27x | 15 | 10 | 0 | - | 5 | - | 10 | - | 2671 / 2657 |
| render-png | 30 | 0.84x | 20 | 0.29x | 0.31x | 15 | 0.37x | 0.28x | 0.39x | 15 | 10 | 0 | 5 | 5 | 10 | 10 | 2671 / 2657 | 2671 / 2657 |
| tables | 30 | 2.94x | 30 | - | - | - | 2.80x | 1.13x | 0.35x | 30 | 0 | 0 | - | 0 | - | - | - | 147 / 71 |
| annots | 30 | 1.39x | 30 | - | - | - | 9.20x | 1.44x | 2.08x | 30 | 0 | 0 | - | 0 | - | - | - | 0 / 33187 |
| forms | 30 | 1.40x | 30 | 0.95x | 0.71x | 30 | 7.97x | 0.33x | 0.23x | 30 | 0 | 0 | 0 | 0 | - | - | 0 / 0 | 0 / 0 |
| signatures | 30 | 1.37x | 30 | - | - | - | 8.35x | 0.43x | 0.34x | 30 | 0 | 0 | - | 0 | - | - | - | 0 / 0 |

Ratios are each side's time over nupdf's, geometric means: above 1x, nupdf is faster. Each is over the files both of its tools did (`mutool` exits with 1 when any page has an error, which counts as failing) and, where checked, nupdf and pdf_oxide agree on. "CLI" is pdf_oxide's native `pdf-oxide`, "Python" its bindings. In process, Python's time runs from opening the document to its last byte of output, and nupdf's is its process time less 2.1 ms, the least it takes to start and exit with nothing to do (`nupdf --version`, fastest of 21 runs), and at least 0.5 ms; "of totals" is the ratio of the summed times. Units are totals over the files both tools did.

pdf_oxide's interpreter start and import, the median of its process time less its time in process: info 40 ms, pages 38 ms, text 49 ms, text-fast 47 ms, render 148 ms, render-png 42 ms, tables 44 ms, annots 49 ms, forms 41 ms, signatures 40 ms.

The `text` row is from a rerun of that operation alone, after the CLI's page separators were left out of its count (`f7903f9`).
