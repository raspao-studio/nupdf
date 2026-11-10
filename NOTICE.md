# Notice

nupdf is a rewrite in Rust of the PDF engine in MuPDF.

MuPDF is Copyright (C) 2004-2025 Artifex Software, Inc., and is licensed under the GNU Affero General Public License, version 3 or later. nupdf follows MuPDF's design, algorithms and behavior closely enough to be a derivative work, so it is distributed under the same license (see [LICENSE](LICENSE)).

Source files written by following a particular MuPDF file name that file in their header and carry Artifex's copyright line next to nupdf's.

nupdf builds on years of careful work by MuPDF's authors. Wherever it improves on MuPDF, it does so because MuPDF showed what correct behavior looks like.

nupdf is an independent project. It is not affiliated with, sponsored by or endorsed by Artifex Software, Inc. MuPDF is a trademark of Artifex Software, Inc.

## The Independent JPEG Group's software

The JPEG decoder in [crates/nupdf/src/jpeg](crates/nupdf/src/jpeg) is a translation into Rust of the decompression part of the Independent JPEG Group's libjpeg, version 10 (the copy bundled with MuPDF 1.28.6). This software is based in part on the work of the Independent JPEG Group. libjpeg is copyright (C) 1991-2026, Thomas G. Lane, Guido Vollbeding; its README, with its copyright and no-warranty notice, is included unaltered as [README.ijg](crates/nupdf/src/jpeg/README.ijg).

Changes from the original files: the C code was rewritten in Rust and reorganized into fewer modules (`marker`, `huff`, `arith`, `idct`, and the controllers in `mod.rs`). Only what MuPDF uses is kept: decompression at full scale with the default parameters, one scanline at a time, from a data source that never suspends. Color quantization, merged upsampling, raw data output, buffered-image mode, scaled output, the fast and floating-point IDCTs, and the IDCT sizes that only "SmartScale" block sizes reach were left out. libjpeg's warnings are not reported, as MuPDF discards them. Memory for whole-image coefficient buffers is capped.
