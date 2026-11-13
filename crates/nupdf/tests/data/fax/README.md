# CCITT fax fixtures

One 45x12 bilevel picture (stripes that widen down the page, and a
diagonal), encoded by libtiff through Pillow 13 and cut out of the TIFF's
one strip. `make.py` remakes them.

| File | TIFF compression, T4Options |
|---|---|
| `g3_1d.fax` | group3, 0: 1-D, EOL before each row |
| `g3_1d_aligned.fax` | group3, 4: 1-D, EOLs byte aligned |
| `g3_2d.fax` | group3, 1: 2-D, a 1-D/2-D tag bit after each EOL |
| `g3_2d_aligned.fax` | group3, 5: 2-D, EOLs byte aligned |
| `g4.fax` | group4 |

The tests in `src/fax.rs` decode them, and damaged variants, with
`/CCITTFaxDecode` parameters and compare with MuPDF through
`../oracle.py` (see its usage; `NUPDF_DUMP_FAX_CASES=<dir>` writes the
cases out).
