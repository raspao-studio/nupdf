# nupdf, MuPDF and pdf_oxide on `standard` (200 files, median of 3 runs)

pdf_oxide and Python: 0.3.78 3.12.11. MuPDF: mutool version 1.28.6.

| Operation | Files | MuPDF / nupdf | Files in it | pdf_oxide / nupdf | In process | Of totals | Files in them | Failed: MuPDF | nupdf | pdf_oxide | Disagree | Units: nupdf | pdf_oxide |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| info | 200 | 1.60x | 199 | 12.48x | 0.18x | 0.12x | 193 | 1 | 1 | 4 | 2 | 357 | 360 |
| pages | 200 | 1.99x | 199 | 15.32x | 0.43x | 0.43x | 193 | 1 | 1 | 5 | 1 | 357 | 359 |
| text | 200 | 1.72x | 190 | 13.00x | 0.58x | 0.85x | 189 | 10 | 1 | 4 | 6 | 138162 | 132270 |
| text-fast | 200 | 1.75x | 190 | 13.41x | 0.69x | 0.90x | 189 | 10 | 1 | 4 | 6 | 138162 | 132261 |
| render | 200 | 1.31x | 190 | 11.05x | 4.36x | 3.81x | 178 | 10 | 1 | 7 | 15 | 329 | 339 |
| tables | 200 | 2.07x | 199 | 12.91x | 0.49x | 0.79x | 195 | 1 | 1 | 4 | - | 40 | 9 |
| annots | 200 | 1.99x | 199 | 15.50x | 0.32x | 0.30x | 194 | 1 | 1 | 5 | - | 40 | 93 |
| forms | 200 | 2.06x | 199 | 15.91x | 0.54x | 0.52x | 195 | 1 | 1 | 4 | - | 41 | 33 |
| signatures | 200 | 2.03x | 199 | 15.98x | 0.62x | 0.61x | 195 | 1 | 1 | 4 | - | 0 | 0 |

Ratios are each side's time over nupdf's, geometric means: above 1x, nupdf is faster. MuPDF's is over the files MuPDF and nupdf did (`mutool` exits with 1 when any page has an error, which counts as failing), pdf_oxide's over the files nupdf and pdf_oxide did and, where checked, agree on. In process, pdf_oxide's time runs from opening the document to its last byte of output, and nupdf's is its process time less 1.7 ms, the least it takes to start and exit with nothing to do (`nupdf --version`, fastest of 21 runs), and at least 0.5 ms; "of totals" is the ratio of the summed in-process times. Units are totals over the files both nupdf and pdf_oxide did.

pdf_oxide's interpreter start and import, the median of its process time less its time in process: info 35 ms, pages 37 ms, text 36 ms, text-fast 34 ms, render 36 ms, tables 38 ms, annots 37 ms, forms 37 ms, signatures 35 ms.
