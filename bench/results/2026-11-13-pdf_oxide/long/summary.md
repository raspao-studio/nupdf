# nupdf, MuPDF and pdf_oxide on `long` (30 files, median of 3 runs)

pdf_oxide and Python: 0.3.78 3.12.11. MuPDF: mutool version 1.28.6.

| Operation | Files | MuPDF / nupdf | Files in it | pdf_oxide / nupdf | In process | Of totals | Files in them | Failed: MuPDF | nupdf | pdf_oxide | Disagree | Units: nupdf | pdf_oxide |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| info | 30 | 0.16x | 30 | 1.31x | 0.03x | 0.01x | 30 | 0 | 0 | 0 | 0 | 3414 | 3414 |
| pages | 30 | 1.54x | 30 | 13.06x | 10.55x | 8.11x | 30 | 0 | 0 | 0 | 0 | 3414 | 3414 |
| text | 30 | 1.43x | 20 | 3.67x | 1.61x | 1.54x | 30 | 10 | 0 | 0 | 0 | 2033184 | 1987292 |
| text-fast | 30 | 1.48x | 20 | 3.43x | 1.45x | 1.33x | 30 | 10 | 0 | 0 | 0 | 2033184 | 1985906 |
| render | 30 | 1.46x | 20 | 2.94x | 1.37x | 0.26x | 15 | 10 | 0 | 5 | 10 | 2671 | 2657 |
| tables | 30 | 2.93x | 30 | 2.72x | 1.23x | 0.33x | 30 | 0 | 0 | 0 | - | 147 | 71 |
| annots | 30 | 1.42x | 30 | 9.81x | 3.02x | 2.59x | 30 | 0 | 0 | 0 | - | 0 | 33187 |
| forms | 30 | 1.44x | 30 | 8.53x | 0.46x | 0.26x | 30 | 0 | 0 | 0 | - | 0 | 0 |
| signatures | 30 | 1.40x | 30 | 8.37x | 0.58x | 0.35x | 30 | 0 | 0 | 0 | - | 0 | 0 |

Ratios are each side's time over nupdf's, geometric means: above 1x, nupdf is faster. MuPDF's is over the files MuPDF and nupdf did (`mutool` exits with 1 when any page has an error, which counts as failing), pdf_oxide's over the files nupdf and pdf_oxide did and, where checked, agree on. In process, pdf_oxide's time runs from opening the document to its last byte of output, and nupdf's is its process time less 2.4 ms, the least it takes to start and exit with nothing to do (`nupdf --version`, fastest of 21 runs), and at least 0.5 ms; "of totals" is the ratio of the summed in-process times. Units are totals over the files both nupdf and pdf_oxide did.

pdf_oxide's interpreter start and import, the median of its process time less its time in process: info 39 ms, pages 39 ms, text 49 ms, text-fast 41 ms, render 144 ms, tables 37 ms, annots 35 ms, forms 41 ms, signatures 36 ms.
