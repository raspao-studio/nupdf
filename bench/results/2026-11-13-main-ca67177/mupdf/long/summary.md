# nupdf and MuPDF, command by command, on `long`

## Bench `info` on `long` (median of 3 runs)

open, read the effective version, count pages and xref slots (JS overhead included on the MuPDF side)

Files: 30. Geometric mean speedup: 0.26x. Total time: MuPDF 152 ms, nupdf 984 ms (0.15x).

## Bench `clean` on `long` (median of 3 runs)

rewrite the document with `clean`'s defaults; nupdf also reads its output back to verify it

Files: 30. Geometric mean speedup: 1.02x. Total time: MuPDF 426 ms, nupdf 520 ms (0.82x).

## Bench `clean-gggg` on `long` (median of 3 runs)

rewrite the document with every garbage collection level; nupdf also reads its output back to verify it

Files: 30. Geometric mean speedup: 0.94x. Total time: MuPDF 1176 ms, nupdf 1139 ms (1.03x).

## Bench `clean-z` on `long` (median of 3 runs)

rewrite the document deflating every stream that has no filter; nupdf also reads its output back to verify it

Files: 30. Geometric mean speedup: 0.35x. Total time: MuPDF 588 ms, nupdf 3897 ms (0.15x).

## Bench `merge` on `long` (median of 3 runs)

merge the document with itself into a new one: pages, links and outline; nupdf also reads its output back to verify it

Files: 30. Geometric mean speedup: 0.85x. Total time: MuPDF 2949 ms, nupdf 3651 ms (0.81x).

## Bench `split` on `long` (median of 3 runs)

write each page to a file of its own from a fresh copy of the input; MuPDF has no split command, so a script does it (JS overhead included); nupdf also reads each part back

Files: 30. Geometric mean speedup: 1.18x. Total time: MuPDF 38147 ms, nupdf 38890 ms (0.98x).

## Bench `tables` on `long` (median of 3 runs)

every page's tables found by table hunting, as rows of cells (JS overhead included on the MuPDF side)

Files: 30. Geometric mean speedup: 3.77x. Total time: MuPDF 18364 ms, nupdf 3499 ms (5.25x).

## Bench `annots` on `long` (median of 3 runs)

every page's annotations with what MuPDF's getters read of each (JS overhead included on the MuPDF side)

Files: 30. Geometric mean speedup: 1.51x. Total time: MuPDF 260 ms, nupdf 171 ms (1.52x).

## Bench `forms` on `long` (median of 3 runs)

every page's form widgets with what the field getters read of each (JS overhead included on the MuPDF side)

Files: 30. Geometric mean speedup: 1.45x. Total time: MuPDF 228 ms, nupdf 161 ms (1.42x).

## Bench `signatures` on `long` (median of 3 runs)

every signature field checked: byte ranges, signatory, the digest (JS overhead included on the MuPDF side)

Files: 30. Geometric mean speedup: 1.44x. Total time: MuPDF 260 ms, nupdf 187 ms (1.39x).

## Bench `flatten` on `long` (median of 3 runs)

annotations and fields baked into the pages, then the document written; nupdf also reads its output back to verify it

Files: 30. Geometric mean speedup: 1.43x. Total time: MuPDF 653 ms, nupdf 557 ms (1.17x).

## Bench `stext` on `long` (median of 3 runs)

structured text of every page as JSON, on one thread: `mutool draw -F stext.json` against `nupdf stext`

Files: 30. Geometric mean speedup: 3.44x. Total time: MuPDF 13448 ms, nupdf 1945 ms (6.91x).

## Bench `stext-threads` on `long` (median of 3 runs)

structured text of every page as JSON, MuPDF on one thread against nupdf on 8

Files: 30. Geometric mean speedup: 6.71x. Total time: MuPDF 15014 ms, nupdf 862 ms (17.43x).

## Bench `render` on `long` (median of 3 runs)

every page at 72 dpi as PPM, color management off, on one thread: `mutool draw -N` (which also draws annotations) against `nupdf render`

Files: 30. Geometric mean speedup: 1.97x. Total time: MuPDF 315027 ms, nupdf 10524 ms (29.93x).

## Bench `render-threads` on `long` (median of 3 runs)

every page at 72 dpi as PPM, MuPDF on one thread (the oracle is built without threads) against nupdf on 8

Files: 30. Geometric mean speedup: 5.20x. Total time: MuPDF 313951 ms, nupdf 3835 ms (81.87x).

## Bench `stext-scaling` on `long` (median of 3 runs)

nupdf's structured text on 1 thread against 8, for documents of 50 pages or more that take 100 ms or more on one

Files: 6. Geometric mean speedup: 3.13x. Total time: baseline 1020 ms, nupdf 324 ms (3.15x).

Left out: 24 file(s) on which baseline takes less than 100 ms.

## Bench `extract-scaling` on `long` (median of 3 runs)

nupdf's `extract` of every page as JSON on 1 thread against 8, for documents of 50 pages or more that take 100 ms or more on one

Files: 6. Geometric mean speedup: 2.97x. Total time: baseline 841 ms, nupdf 283 ms (2.97x).

Left out: 24 file(s) on which baseline takes less than 100 ms.
