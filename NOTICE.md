# Notice

nupdf is a rewrite in Rust of the PDF engine in MuPDF.

MuPDF is Copyright (C) 2004-2025 Artifex Software, Inc., and is licensed under the GNU Affero General Public License, version 3 or later. nupdf follows MuPDF's design, algorithms and behavior closely enough to be a derivative work, so it is distributed under the same license (see [LICENSE](LICENSE)).

Source files written by following a particular MuPDF file name that file in their header and carry Artifex's copyright line next to nupdf's.

nupdf builds on years of careful work by MuPDF's authors. Wherever it improves on MuPDF, it does so because MuPDF showed what correct behavior looks like.

nupdf is an independent project. It is not affiliated with, sponsored by or endorsed by Artifex Software, Inc. MuPDF is a trademark of Artifex Software, Inc.

## The Independent JPEG Group's software

The JPEG decoder in [crates/nupdf/src/jpeg](crates/nupdf/src/jpeg) is a translation into Rust of the decompression part of the Independent JPEG Group's libjpeg, version 10 (the copy bundled with MuPDF 1.28.6). This software is based in part on the work of the Independent JPEG Group. libjpeg is copyright (C) 1991-2026, Thomas G. Lane, Guido Vollbeding; its README, with its copyright and no-warranty notice, is included unaltered as [README.ijg](crates/nupdf/src/jpeg/README.ijg).

Changes from the original files: the C code was rewritten in Rust and reorganized into fewer modules (`marker`, `huff`, `arith`, `idct`, and the controllers in `mod.rs`). Only what MuPDF uses is kept: decompression at full scale with the default parameters, one scanline at a time, from a data source that never suspends. Color quantization, merged upsampling, raw data output, buffered-image mode, scaled output, the fast and floating-point IDCTs, and the IDCT sizes that only "SmartScale" block sizes reach were left out. libjpeg's warnings are not reported, as MuPDF discards them. Memory for whole-image coefficient buffers is capped.

## hayro-jbig2

`crates/nupdf/src/hayro_jbig2` is hayro-jbig2 0.4.0 by Laurenz Stampfl (https://github.com/LaurenzV/hayro), under MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE` there), with the changes `NUPDF.md` there describes.

## zlib

The deflate encoder in [crates/nupdf/src/deflate.rs](crates/nupdf/src/deflate.rs) is a translation into Rust of part of zlib 1.3.2 (the copy bundled with MuPDF 1.28.6), so that streams `clean` compresses are the same bytes MuPDF writes. zlib is Copyright (C) 1995-2026 Jean-loup Gailly and Mark Adler; its license notice is kept, unaltered, at the top of that file.

Changes from the original files: the C code was rewritten in Rust and reduced to what `compress2` reaches when MuPDF deflates a stream: one call with all of the input, `Z_FINISH`, an output buffer of `compressBound` bytes, a 32K window, memory level 8 and the default strategy. Streaming input, other flush modes, preset dictionaries, the gzip wrapper, `deflateParams`, the Huffman-only, RLE, filtered and fixed strategies, and the debugging code were left out. The symbol buffer is kept apart from the pending output instead of sharing its memory. This is not the original zlib and should not be taken for it.

## Arlington PDF Model

The files in [xtask/data/arlington](xtask/data/arlington) are definitions from the PDF Association's Arlington PDF Model (<https://github.com/pdf-association/arlington-pdf-model>), unmodified, used by the test harness only. They are licensed under the Apache License 2.0, whose text is in [xtask/data/arlington/LICENSE](xtask/data/arlington/LICENSE); the commit they come from is in that directory's README.

## FreeType

The font readers in [crates/nupdf/src/font](crates/nupdf/src/font) follow FreeType 2 (the copy bundled with MuPDF 1.28.6), so that fonts open, fail, map characters, report widths and bound their glyphs as they do in MuPDF. Portions of this software are copyright (C) 1996-2025 The FreeType Project (https://freetype.org). All rights reserved. FreeType is used under the FreeType License, whose text is included unaltered as [FTL.TXT](crates/nupdf/src/font/FTL.TXT). The PostScript name tables and CFF's standard encodings in [names.rs](crates/nupdf/src/font/names.rs) are generated from FreeType's sources by `cargo xtask tables`.

Changes from the original files: the C code was rewritten in Rust and reduced to what MuPDF asks of FreeType when it loads a PDF's fonts and draws their glyphs: opening a face (the TrueType, Type 1 and CFF drivers, in MuPDF's order), its charmaps with their validation, glyph names and advance widths, a glyph's unhinted outline (the TrueType glyph loader and the Adobe charstring engine) with its control box, and the smooth rasterizer that renders it ([draw/grays.rs](crates/nupdf/src/draw/grays.rs), from `src/smooth/ftgrays.c`, `ftsmooth.c` and parts of `src/base`). Hinting, the monochrome rasterizer, bitmap and color fonts, and the other drivers were left out. This is not FreeType and should not be taken for it.

## ICC profiles

The color profiles in [crates/nupdf/icc](crates/nupdf/icc) (gray, sRGB, SWOP CMYK and Lab, and the ps_gray, ps_rgb and ps_cmyk profiles MuPDF draws soft masks with) are MuPDF's own (resources/icc in MuPDF 1.28.6; the ps_ ones taken byte for byte from source/fitz/icc/ps_*.icc.h), unmodified, Copyright Artifex Software, and part of MuPDF under the GNU AGPL. nupdf's color management ([cms.rs](crates/nupdf/src/cms.rs)) uses them as MuPDF does with lcms2, through moxcms, and computes black point compensation as lcms2 does.

## URW base fonts

The fonts in [crates/nupdf/fonts/urw](crates/nupdf/fonts/urw) are the URW++ base fonts MuPDF uses for the standard 14 fonts and their substitutes (resources/fonts/urw in MuPDF 1.28.6), unmodified. They are Copyright 2016 by (URW)++ Design & Development and licensed under the SIL Open Font License 1.1, whose text is in [OFL.txt](crates/nupdf/fonts/urw/OFL.txt).

## Glyphless font

[crates/nupdf/fonts/glyphless/pdf.ttf](crates/nupdf/fonts/glyphless/pdf.ttf) is Tesseract's glyphless font (tessdata/pdf.ttf in Tesseract 5.5.4), unmodified, which `ocr` embeds for its invisible text. It is Copyright Google Inc. and the Tesseract authors, licensed under the Apache License 2.0, whose text is in [LICENSE-APACHE](crates/nupdf/fonts/glyphless/LICENSE-APACHE).

## UCDN

The bidi classes in [crates/nupdf/src/tables.rs](crates/nupdf/src/tables.rs) are generated from MuPDF's copy of UCDN's Unicode database (source/fitz/ucdn_db.h, Unicode 16.0.0). UCDN is Copyright (C) 2012 Grigori Goronzy and is used under the ISC license: permission to use, copy, modify, and/or distribute this software for any purpose with or without fee is hereby granted, provided that the above copyright notice and this permission notice appear in all copies. The data comes from the Unicode Character Database, Copyright (C) 1991-2024 Unicode, Inc., under the Unicode License.
