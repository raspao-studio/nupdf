# nupdf and PDF Oxide, library to library, on `long` (30 files, median of 5 runs)

nupdf from this tree; pdf_oxide 0.3.78 (features `rendering`, `logging` and its defaults); both built with the root workspace's release profile (`bench/engines`).

| Operation | Files | PDF Oxide / nupdf | Of totals | Files in them | nupdf total ms | PDF Oxide total ms | Failed: nupdf | PDF Oxide | Disagree | Units: nupdf / PDF Oxide |
|---|---|---|---|---|---|---|---|---|---|---|
| open | 30 | 5.85x | 7.35x | 30 | 2.6 | 19.4 | 0 | 0 | 0 | 3414 / 3414 |
| pages | 30 | 11.45x | 9.94x | 23 | 8.5 | 84.4 | 0 | 7 | 0 | 2669 / 2669 |
| text | 30 | 1.82x | 2.57x | 30 | 1622.7 | 4167.3 | 0 | 0 | 0 | 2033184 / 1987292 |
| text-fast | 30 | 1.55x | 2.01x | 30 | 1611.0 | 3233.5 | 0 | 0 | 0 | 2033184 / 1985906 |
| render | 30 | 3.08x | 7.36x | 15 | 2580.0 | 18992.3 | 1 | 5 | 9 | 2551 / 2536 |
| tables | 30 | 1.55x | 1.34x | 30 | 2047.3 | 2744.1 | 0 | 0 | - | 147 / 71 |
| annots | 30 | 22.03x | 8.10x | 30 | 33.4 | 270.7 | 0 | 0 | - | 0 / 0 |

In-process library time, the median of the timed runs, from opening the document in memory to the last page: PDF Oxide's over nupdf's, the geometric mean and the ratio of the totals, over the files both did and, where checked, agree on. Above 1x, nupdf is faster.
