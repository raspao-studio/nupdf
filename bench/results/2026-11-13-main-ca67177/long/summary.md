# nupdf, MuPDF and pdf_oxide on `long` (30 files, median of 3 runs)

pdf_oxide CLI: pdf-oxide 0.3.78. pdf_oxide and Python: 0.3.78 3.12.11. MuPDF: mutool version 1.28.6.

| Operation | Files | MuPDF / nupdf | Files in it | CLI / nupdf | Of totals | Files in it | Python / nupdf | In process | Of totals | Files in them | Failed: MuPDF | nupdf | CLI | Python | Disagree: CLI | Python | Units, nupdf / CLI | nupdf / Python |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| info | 30 | 0.24x | 30 | 0.24x | 0.16x | 30 | 1.96x | 0.04x | 0.03x | 30 | 0 | 0 | 0 | 0 | 0 | 0 | 3414 / 3414 | 3414 / 3414 |
| pages | 30 | 1.59x | 30 | - | - | - | 13.52x | 10.31x | 8.28x | 30 | 0 | 0 | - | 0 | - | 0 | - | 3414 / 3414 |
| text | 30 | 1.96x | 20 | 1.86x | 3.68x | 30 | 4.31x | 1.94x | 3.78x | 30 | 10 | 0 | 0 | 0 | 0 | 0 | 2032465 / 1987292 | 2032465 / 1987292 |
| text-fast | 30 | 1.92x | 20 | - | - | - | 3.94x | 1.69x | 3.42x | 30 | 10 | 0 | - | 0 | - | 0 | - | 2032465 / 1985906 |
| render | 30 | 1.79x | 20 | - | - | - | 5.82x | 2.54x | 5.86x | 15 | 10 | 0 | - | 5 | - | 10 | - | 2671 / 2657 |
| render-png | 30 | 1.48x | 20 | 0.73x | 3.06x | 15 | 0.90x | 0.70x | 2.45x | 15 | 10 | 0 | 5 | 5 | 10 | 10 | 2671 / 2657 | 2671 / 2657 |
| tables | 30 | 3.98x | 30 | - | - | - | 3.59x | 1.60x | 2.23x | 30 | 0 | 0 | - | 0 | - | - | - | 147 / 71 |
| annots | 30 | 1.46x | 30 | - | - | - | 10.16x | 2.86x | 2.77x | 30 | 0 | 0 | - | 0 | - | - | - | 0 / 33187 |
| forms | 30 | 1.48x | 30 | 0.91x | 0.65x | 30 | 8.76x | 0.46x | 0.29x | 30 | 0 | 0 | 0 | 0 | - | - | 0 / 0 | 0 / 0 |
| signatures | 30 | 1.44x | 30 | - | - | - | 8.82x | 0.55x | 0.36x | 30 | 0 | 0 | - | 0 | - | - | - | 0 / 0 |

Ratios are each side's time over nupdf's, geometric means: above 1x, nupdf is faster. Each is over the files both of its tools did (`mutool` exits with 1 when any page has an error, which counts as failing) and, where checked, nupdf and pdf_oxide agree on. "CLI" is pdf_oxide's native `pdf-oxide`, "Python" its bindings. In process, Python's time runs from opening the document to its last byte of output, and nupdf's is its process time less 1.8 ms, the least it takes to start and exit with nothing to do (`nupdf --version`, fastest of 21 runs), and at least 0.5 ms; "of totals" is the ratio of the summed times. Units are totals over the files both tools did.

pdf_oxide's interpreter start and import, the median of its process time less its time in process: info 34 ms, pages 34 ms, text 35 ms, text-fast 35 ms, render 142 ms, render-png 35 ms, tables 33 ms, annots 32 ms, forms 32 ms, signatures 32 ms.
