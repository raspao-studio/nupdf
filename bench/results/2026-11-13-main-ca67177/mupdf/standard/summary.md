# nupdf and MuPDF, command by command, on `standard`

## Bench `info` on `standard` (median of 3 runs)

open, read the effective version, count pages and xref slots (JS overhead included on the MuPDF side)

Files: 200. Geometric mean speedup: 1.57x. Total time: MuPDF 848 ms, nupdf 574 ms (1.48x).

## Bench `clean` on `standard` (median of 3 runs)

rewrite the document with `clean`'s defaults; nupdf also reads its output back to verify it

Files: 200. Geometric mean speedup: 1.38x. Total time: MuPDF 694 ms, nupdf 492 ms (1.41x).

## Bench `clean-gggg` on `standard` (median of 3 runs)

rewrite the document with every garbage collection level; nupdf also reads its output back to verify it

Files: 200. Geometric mean speedup: 1.34x. Total time: MuPDF 759 ms, nupdf 560 ms (1.36x).

## Bench `clean-z` on `standard` (median of 3 runs)

rewrite the document deflating every stream that has no filter; nupdf also reads its output back to verify it

Files: 200. Geometric mean speedup: 1.24x. Total time: MuPDF 809 ms, nupdf 698 ms (1.16x).

## Bench `merge` on `standard` (median of 3 runs)

merge the document with itself into a new one: pages, links and outline; nupdf also reads its output back to verify it

Files: 200. Geometric mean speedup: 1.37x. Total time: MuPDF 776 ms, nupdf 563 ms (1.38x).

## Bench `split` on `standard` (median of 3 runs)

write each page to a file of its own from a fresh copy of the input; MuPDF has no split command, so a script does it (JS overhead included); nupdf also reads each part back

Files: 200. Geometric mean speedup: 1.72x. Total time: MuPDF 1179 ms, nupdf 713 ms (1.65x).

## Bench `tables` on `standard` (median of 3 runs)

every page's tables found by table hunting, as rows of cells (JS overhead included on the MuPDF side)

Files: 200. Geometric mean speedup: 2.02x. Total time: MuPDF 1545 ms, nupdf 681 ms (2.27x).

## Bench `annots` on `standard` (median of 3 runs)

every page's annotations with what MuPDF's getters read of each (JS overhead included on the MuPDF side)

Files: 200. Geometric mean speedup: 1.98x. Total time: MuPDF 916 ms, nupdf 460 ms (1.99x).

## Bench `forms` on `standard` (median of 3 runs)

every page's form widgets with what the field getters read of each (JS overhead included on the MuPDF side)

Files: 200. Geometric mean speedup: 1.99x. Total time: MuPDF 964 ms, nupdf 483 ms (2.00x).

## Bench `signatures` on `standard` (median of 3 runs)

every signature field checked: byte ranges, signatory, the digest (JS overhead included on the MuPDF side)

Files: 200. Geometric mean speedup: 1.92x. Total time: MuPDF 931 ms, nupdf 483 ms (1.93x).

## Bench `flatten` on `standard` (median of 3 runs)

annotations and fields baked into the pages, then the document written; nupdf also reads its output back to verify it

Files: 200. Geometric mean speedup: 1.89x. Total time: MuPDF 988 ms, nupdf 518 ms (1.91x).

## Bench `stext` on `standard` (median of 3 runs)

structured text of every page as JSON, on one thread: `mutool draw -F stext.json` against `nupdf stext`

Files: 200. Geometric mean speedup: 1.77x. Total time: MuPDF 1532 ms, nupdf 682 ms (2.25x).

## Bench `stext-threads` on `standard` (median of 3 runs)

structured text of every page as JSON, MuPDF on one thread against nupdf on 8

Files: 200. Geometric mean speedup: 1.77x. Total time: MuPDF 1682 ms, nupdf 717 ms (2.35x).

## Bench `render` on `standard` (median of 3 runs)

every page at 72 dpi as PPM, color management off, on one thread: `mutool draw -N` (which also draws annotations) against `nupdf render`

Files: 200. Geometric mean speedup: 1.28x. Total time: MuPDF 1609 ms, nupdf 1388 ms (1.16x).

## Bench `render-threads` on `standard` (median of 3 runs)

every page at 72 dpi as PPM, MuPDF on one thread (the oracle is built without threads) against nupdf on 8

Files: 200. Geometric mean speedup: 1.32x. Total time: MuPDF 1625 ms, nupdf 1234 ms (1.32x).

## Bench `stext-scaling` on `standard` (median of 3 runs)

nupdf's structured text on 1 thread against 8, for documents of 50 pages or more that take 100 ms or more on one

Files: 1. Geometric mean speedup: 3.21x. Total time: baseline 203 ms, nupdf 63 ms (3.21x).

Left out: 6 file(s) on which baseline takes less than 100 ms.

## Bench `extract-scaling` on `standard` (median of 3 runs)

nupdf's `extract` of every page as JSON on 1 thread against 8, for documents of 50 pages or more that take 100 ms or more on one

Files: 1. Geometric mean speedup: 3.08x. Total time: baseline 114 ms, nupdf 37 ms (3.08x).

Left out: 6 file(s) on which baseline takes less than 100 ms.
