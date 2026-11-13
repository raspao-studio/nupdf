# nupdf and PDF Oxide, library to library, on `standard` (200 files, median of 5 runs)

nupdf from this tree; pdf_oxide 0.3.78 (features `rendering`, `logging` and its defaults); both built with the root workspace's release profile (`bench/engines`).

| Operation | Files | PDF Oxide / nupdf | Of totals | Files in them | nupdf total ms | PDF Oxide total ms | Failed: nupdf | PDF Oxide | Disagree | Units: nupdf / PDF Oxide |
|---|---|---|---|---|---|---|---|---|---|---|
| open | 200 | 2.21x | 1.03x | 193 | 9.2 | 9.5 | 1 | 4 | 2 | 357 / 360 |
| pages | 200 | 2.49x | 1.53x | 192 | 9.7 | 14.9 | 1 | 6 | 1 | 356 / 358 |
| text | 200 | 1.40x | 2.22x | 189 | 189.1 | 419.4 | 1 | 4 | 6 | 138162 / 132270 |
| text-fast | 200 | 1.24x | 1.68x | 189 | 154.7 | 259.8 | 1 | 4 | 6 | 138162 / 132261 |
| render | 200 | 1.58x | 3.20x | 190 | 1001.9 | 3204.1 | 1 | 6 | 4 | 329 / 328 |
| tables | 200 | 1.27x | 1.70x | 195 | 195.4 | 332.0 | 1 | 4 | - | 40 / 9 |
| annots | 200 | 2.41x | 1.44x | 194 | 9.6 | 13.9 | 1 | 5 | - | 40 / 39 |

In-process library time, the median of the timed runs, from opening the document in memory to the last page: PDF Oxide's over nupdf's, the geometric mean and the ratio of the totals, over the files both did and, where checked, agree on. Above 1x, nupdf is faster.
