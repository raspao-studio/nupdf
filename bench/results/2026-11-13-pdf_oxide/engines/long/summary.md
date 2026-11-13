# nupdf and PDF Oxide, library to library, on `long` (30 files, median of 5 runs)

nupdf from this tree; pdf_oxide 0.3.78 (features `rendering`, `logging` and its defaults); both built with the root workspace's release profile (`bench/engines`).

| Operation | Files | PDF Oxide / nupdf | Of totals | Files in them | nupdf total ms | PDF Oxide total ms | Failed: nupdf | PDF Oxide | Disagree | Units: nupdf / PDF Oxide |
|---|---|---|---|---|---|---|---|---|---|---|
| open | 30 | 4.76x | 6.86x | 30 | 1.8 | 12.1 | 0 | 0 | 0 | 3414 / 3414 |
| pages | 30 | 7.77x | 6.00x | 23 | 8.9 | 53.6 | 0 | 7 | 0 | 2669 / 2669 |
| text | 30 | 1.42x | 0.73x | 30 | 3804.5 | 2794.3 | 0 | 0 | 0 | 2033184 / 1987292 |
| text-fast | 30 | 1.22x | 0.62x | 30 | 3936.0 | 2435.9 | 0 | 0 | 0 | 2033184 / 1985906 |
| render | 30 | 1.61x | 0.19x | 15 | 82242.5 | 15772.2 | 1 | 5 | 9 | 2551 / 2536 |
| tables | 30 | 1.10x | 0.18x | 30 | 15303.0 | 2802.7 | 0 | 0 | - | 147 / 71 |
| annots | 30 | 15.44x | 5.56x | 30 | 47.5 | 264.1 | 0 | 0 | - | 0 / 0 |

In-process library time, the median of the timed runs, from opening the document in memory to the last page: PDF Oxide's over nupdf's, the geometric mean and the ratio of the totals, over the files both did and, where checked, agree on. Above 1x, nupdf is faster.
