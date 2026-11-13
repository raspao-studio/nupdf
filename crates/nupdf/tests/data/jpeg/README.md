# JPEG fixtures

Small JPEGs for the decoder's unit tests (`src/jpeg/mod.rs`). `make.py`
makes every one of them from generated pictures with libjpeg-turbo 3.2's
`cjpeg` (and Pillow for `cmyk.jpg`); rerunning it reproduces these bytes.
The expected outputs in the tests are MuPDF 1.28.6's, from `../oracle.py`.

The 37x29 picture is RGB gradients with a hard-edged square; `cjpeg
-quality 75` unless noted.

| File | How it was made |
|---|---|
| `base420.jpg` | baseline, 2x2 chroma (cjpeg's default) |
| `base422.jpg`, `base440.jpg`, `base411.jpg`, `base32.jpg`, `base13.jpg` | `-sample 2x1`, `1x2`, `4x1`, `3x2`, `1x3` |
| `base444.jpg` | `-quality 90 -sample 1x1` |
| `mixed.jpg` | `-sample 1x1,1x2,2x1`: chroma larger than luma |
| `sep44.jpg` | `-sample 4x4`, one scan per component |
| `gray.jpg` | `-grayscale` of the picture's gray |
| `rgb.jpg` | `-rgb`: RGB component IDs and an Adobe marker with transform 0 |
| `opt.jpg` | `-optimize`: Huffman tables of the image's own |
| `restart.jpg`, `restart1b.jpg` | `-restart 1` (each MCU row); `-sample 1x1 -restart 1B` (each MCU) |
| `multiscan.jpg` | two sequential scans, luma then chroma |
| `prog.jpg`, `prog_gray.jpg` | `-progressive` |
| `prog_restart.jpg` | `-progressive -restart 2B` |
| `prog_sepdc.jpg` | progressive, each component's DC in its own scan |
| `arith.jpg`, `arith_prog.jpg` | `-arithmetic`, and with `-progressive` |
| `arith_restart.jpg`, `arith_prog_restart.jpg` | `-arithmetic -restart 2B`; `-arithmetic -progressive -restart 3B` |
| `p12.jpg` | `-precision 12`, which MuPDF's libjpeg does not decode |
| `cmyk.jpg` | Pillow's CMYK save: an Adobe marker with transform 0 |
| `noise_base.jpg`, `noise_prog.jpg`, `noise_arith_prog.jpg` | 24x16 gray noise with bright dots, `-quality 95`: baseline, progressive, arithmetic progressive |
| `stripes_prog.jpg`, `stripes_arith_prog.jpg` | 16x16 gray, the (4, 4) DCT basis in each block (an AC coefficient after a 39-zero run), successive approximation scans; Huffman and arithmetic |

The tests derive damaged and relabelled variants from these in code
(`variants()`). To check them against MuPDF again:

```sh
mkdir /tmp/v && NUPDF_DUMP_JPEG_VARIANTS=/tmp/v cargo test -p nupdf --lib jpeg::tests::damaged
python3 ../oracle.py "/Filter /DCTDecode" /tmp/v/*.ct-1.jpg
python3 ../oracle.py "/Filter /DCTDecode /DecodeParms << /ColorTransform 0 >>" /tmp/v/*.ct0.jpg
python3 ../oracle.py "/Filter /DCTDecode /DecodeParms << /ColorTransform 1 >>" /tmp/v/*.ct1.jpg
```
