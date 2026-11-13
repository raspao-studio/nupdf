# Image fixtures

JPEG 2000 files for the image loader's tests (`src/image.rs`), made with
Pillow 13 (OpenJPEG 2.5): `Image.new(mode, size, color).save(name)`.

| File | Mode, size |
|---|---|
| `rgb.jp2`, `rgb.j2k` | RGB 5x3, as a JP2 file and a bare codestream |
| `gray.jp2` | L 4x2 |
| `la.jp2` | LA 2x2: two components, a gray colour box |
| `rgba.jp2` | RGBA 3x3 |
| `cmyk.jp2` | CMYK 2x2 |

`image.js` prints MuPDF's summary of object 2 of a file loaded as an image,
in the form the tests compare against. The tests build their cases' files
in code; to check them against MuPDF again:

```sh
mkdir /tmp/i && NUPDF_DUMP_IMAGE_CASES=/tmp/i cargo test -p nupdf --lib image::tests::images_load
for f in /tmp/i/*.pdf; do echo "$f: $(mutool run image.js $f)"; done
```
