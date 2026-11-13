# nupdf and PDF Oxide, library to library, on `standard` (200 files, median of 5 runs)

nupdf from this tree; pdf_oxide 0.3.78 (features `rendering`, `logging` and its defaults); both built with the root workspace's release profile (`bench/engines`).

| Operation | Files | PDF Oxide / nupdf | Of totals | Files in them | nupdf total ms | PDF Oxide total ms | Failed: nupdf | PDF Oxide | Disagree | Units: nupdf / PDF Oxide |
|---|---|---|---|---|---|---|---|---|---|---|
| open | 200 | 2.14x | 1.03x | 193 | 5.1 | 5.3 | 1 | 4 | 2 | 357 / 360 |
| pages | 200 | 2.22x | 1.33x | 192 | 6.0 | 8.0 | 1 | 6 | 1 | 356 / 358 |
| text | 200 | 1.30x | 1.98x | 189 | 86.0 | 170.6 | 1 | 4 | 6 | 138162 / 132270 |
| text-fast | 200 | 1.15x | 1.61x | 189 | 84.5 | 136.4 | 1 | 4 | 6 | 138162 / 132261 |
| render | 200 | 1.57x | 3.37x | 190 | 530.2 | 1786.3 | 1 | 6 | 4 | 329 / 328 |
| tables | 200 | 1.25x | 1.68x | 195 | 129.5 | 217.5 | 1 | 4 | - | 40 / 9 |
| annots | 200 | 2.19x | 1.38x | 194 | 7.0 | 9.7 | 1 | 5 | - | 40 / 39 |

In-process library time, the median of the timed runs, from opening the document in memory to the last page: PDF Oxide's over nupdf's, the geometric mean and the ratio of the totals, over the files both did and, where checked, agree on. Above 1x, nupdf is faster.
