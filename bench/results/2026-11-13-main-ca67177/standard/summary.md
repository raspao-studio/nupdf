# nupdf, MuPDF and pdf_oxide on `standard` (200 files, median of 3 runs)

pdf_oxide CLI: pdf-oxide 0.3.78. pdf_oxide and Python: 0.3.78 3.12.11. MuPDF: mutool version 1.28.6.

| Operation | Files | MuPDF / nupdf | Files in it | CLI / nupdf | Of totals | Files in it | Python / nupdf | In process | Of totals | Files in them | Failed: MuPDF | nupdf | CLI | Python | Disagree: CLI | Python | Units, nupdf / CLI | nupdf / Python |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| info | 200 | 1.64x | 199 | 1.11x | 1.01x | 193 | 12.72x | 0.23x | 0.15x | 193 | 1 | 1 | 4 | 4 | 2 | 2 | 357 / 360 | 357 / 360 |
| pages | 200 | 1.99x | 199 | - | - | - | 15.63x | 0.53x | 0.57x | 193 | 1 | 1 | - | 5 | - | 1 | - | 357 / 359 |
| text | 200 | 1.68x | 190 | 1.26x | 1.31x | 191 | 12.89x | 0.71x | 1.04x | 191 | 10 | 1 | 4 | 4 | 4 | 4 | 138336 / 132270 | 138336 / 132270 |
| text-fast | 200 | 1.69x | 190 | - | - | - | 12.86x | 0.67x | 0.90x | 191 | 10 | 1 | - | 4 | - | 4 | - | 138336 / 132261 |
| render | 200 | 1.28x | 190 | - | - | - | 11.38x | 5.26x | 4.13x | 185 | 10 | 1 | - | 7 | - | 8 | - | 344 / 339 |
| render-png | 200 | 1.28x | 190 | 2.16x | 1.93x | 185 | 7.79x | 2.91x | 2.07x | 185 | 10 | 1 | 7 | 7 | 8 | 8 | 344 / 339 | 344 / 339 |
| tables | 200 | 2.10x | 199 | - | - | - | 12.94x | 0.65x | 0.94x | 195 | 1 | 1 | - | 4 | - | - | - | 40 / 9 |
| annots | 200 | 2.03x | 199 | - | - | - | 15.68x | 0.42x | 0.43x | 194 | 1 | 1 | - | 5 | - | - | - | 40 / 93 |
| forms | 200 | 2.04x | 199 | 1.33x | 1.33x | 195 | 15.58x | 0.61x | 0.60x | 195 | 1 | 1 | 4 | 4 | - | - | 41 / 33 | 41 / 33 |
| signatures | 200 | 2.00x | 199 | - | - | - | 15.69x | 0.64x | 0.64x | 195 | 1 | 1 | - | 4 | - | - | - | 0 / 0 |

Ratios are each side's time over nupdf's, geometric means: above 1x, nupdf is faster. Each is over the files both of its tools did (`mutool` exits with 1 when any page has an error, which counts as failing) and, where checked, nupdf and pdf_oxide agree on. "CLI" is pdf_oxide's native `pdf-oxide`, "Python" its bindings. In process, Python's time runs from opening the document to its last byte of output, and nupdf's is its process time less 1.8 ms, the least it takes to start and exit with nothing to do (`nupdf --version`, fastest of 21 runs), and at least 0.5 ms; "of totals" is the ratio of the summed times. Units are totals over the files both tools did.

pdf_oxide's interpreter start and import, the median of its process time less its time in process: info 33 ms, pages 33 ms, text 34 ms, text-fast 34 ms, render 35 ms, render-png 34 ms, tables 34 ms, annots 34 ms, forms 33 ms, signatures 33 ms.
