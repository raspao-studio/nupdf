# nupdf, MuPDF and pdf_oxide on `standard` (200 files, median of 3 runs)

pdf_oxide CLI: pdf-oxide 0.3.78. pdf_oxide and Python: 0.3.78 3.12.11. MuPDF: mutool version 1.28.6.

| Operation | Files | MuPDF / nupdf | Files in it | CLI / nupdf | Of totals | Files in it | Python / nupdf | In process | Of totals | Files in them | Failed: MuPDF | nupdf | CLI | Python | Disagree: CLI | Python | Units, nupdf / CLI | nupdf / Python |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| info | 200 | 1.54x | 199 | 1.15x | 1.03x | 193 | 12.57x | 0.17x | 0.12x | 193 | 1 | 1 | 4 | 4 | 2 | 2 | 357 / 360 | 357 / 360 |
| pages | 200 | 1.94x | 199 | - | - | - | 15.13x | 0.45x | 0.42x | 193 | 1 | 1 | - | 5 | - | 1 | - | 357 / 359 |
| text | 200 | 1.63x | 190 | 1.31x | 1.35x | 189 | 11.93x | 0.48x | 0.74x | 189 | 10 | 1 | 4 | 4 | 6 | 6 | 138162 / 132270 | 138162 / 132270 |
| text-fast | 200 | 1.76x | 190 | - | - | - | 13.15x | 0.74x | 0.93x | 189 | 10 | 1 | - | 4 | - | 6 | - | 138162 / 132261 |
| render | 200 | 1.34x | 190 | - | - | - | 11.23x | 4.94x | 3.93x | 178 | 10 | 1 | - | 7 | - | 15 | - | 329 / 339 |
| render-png | 200 | 1.07x | 190 | 1.88x | 1.61x | 178 | 6.32x | 2.05x | 1.58x | 178 | 10 | 1 | 7 | 7 | 15 | 15 | 329 / 339 | 329 / 339 |
| tables | 200 | 2.08x | 199 | - | - | - | 12.61x | 0.60x | 0.90x | 195 | 1 | 1 | - | 4 | - | - | - | 40 / 9 |
| annots | 200 | 2.07x | 199 | - | - | - | 15.92x | 0.44x | 0.45x | 194 | 1 | 1 | - | 5 | - | - | - | 40 / 93 |
| forms | 200 | 2.07x | 199 | 1.34x | 1.33x | 195 | 15.70x | 0.61x | 0.61x | 195 | 1 | 1 | 4 | 4 | - | - | 41 / 33 | 41 / 33 |
| signatures | 200 | 2.02x | 199 | - | - | - | 15.70x | 0.67x | 0.63x | 195 | 1 | 1 | - | 4 | - | - | - | 0 / 0 |

Ratios are each side's time over nupdf's, geometric means: above 1x, nupdf is faster. Each is over the files both of its tools did (`mutool` exits with 1 when any page has an error, which counts as failing) and, where checked, nupdf and pdf_oxide agree on. "CLI" is pdf_oxide's native `pdf-oxide`, "Python" its bindings. In process, Python's time runs from opening the document to its last byte of output, and nupdf's is its process time less 2.2 ms, the least it takes to start and exit with nothing to do (`nupdf --version`, fastest of 21 runs), and at least 0.5 ms; "of totals" is the ratio of the summed times. Units are totals over the files both tools did.

pdf_oxide's interpreter start and import, the median of its process time less its time in process: info 47 ms, pages 45 ms, text 48 ms, text-fast 40 ms, render 43 ms, render-png 41 ms, tables 39 ms, annots 34 ms, forms 33 ms, signatures 36 ms.

The `text` row is from a rerun of that operation alone, after the CLI's page separators were left out of its count (`f7903f9`).
