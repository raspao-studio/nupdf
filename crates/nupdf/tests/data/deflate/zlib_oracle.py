#!/usr/bin/env python3
"""MuPDF's deflate (its bundled zlib's compress2, as `mutool clean -z`
calls it) of each input file, for pinning in src/deflate.rs.

Usage: python3 zlib_oracle.py file.level... e.g. runs.bin.9
Each argument names an input file and, after its last dot, a zlib level
(-1 for the default). The file goes into a PDF as an uncompressed stream,
`mutool clean -z -e <effort>` deflates it at that level, and one line is
printed: ("name", level, compressed length, CRC-32 of the compressed data).
"""
import os
import subprocess
import sys
import tempfile
import zlib

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "../../../../.."))
MUTOOL = os.environ.get("NUPDF_MUTOOL") or os.path.join(
    ROOT, "target/oracle/mupdf-1.28.6/build/release/mutool"
)
RAW_JS = 'Document.openDocument(scriptArgs[0]).newIndirect(1, 0).readRawStream().save(scriptArgs[1]);'
# pdf-write.c: level = effort * 9 / 100, 0 for zlib's default.
EFFORT = {-1: 0, 0: 1, 1: 12, 2: 23, 3: 34, 4: 45, 5: 56, 6: 67, 7: 78, 8: 89, 9: 100}


def pdf(data):
    objs = [
        b"<< /Length %d >>\nstream\n" % len(data) + data + b"\nendstream",
        b"<< /Type /Catalog /Pages 3 0 R >>",
        b"<< /Type /Pages /Kids [] /Count 0 >>",
    ]
    out = bytearray(b"%PDF-1.7\n")
    offsets = []
    for i, o in enumerate(objs):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % (i + 1) + o + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 4\n0000000000 65535 f \n"
    for o in offsets:
        out += b"%010d 00000 n \n" % o
    out += b"trailer\n<< /Size 4 /Root 2 0 R >>\nstartxref\n%d\n%%%%EOF\n" % xref
    return bytes(out)


def main():
    with tempfile.TemporaryDirectory() as d:
        js = os.path.join(d, "raw.js")
        with open(js, "w") as f:
            f.write(RAW_JS)
        for arg in sys.argv[1:]:
            path, level = arg.rsplit(".", 1)
            level = int(level)
            with open(path, "rb") as f:
                data = f.read()
            src, dst, raw = (os.path.join(d, n) for n in ("in.pdf", "out.pdf", "raw"))
            with open(src, "wb") as f:
                f.write(pdf(data))
            subprocess.run([MUTOOL, "clean", "-z", "-e", str(EFFORT[level]), src, dst], check=True,
                           capture_output=True)
            subprocess.run([MUTOOL, "run", js, dst, raw], check=True, capture_output=True)
            with open(raw, "rb") as f:
                packed = f.read()
            name = os.path.basename(path).split(".")[0]
            print(f'("{name}", {level}, {len(packed)}, 0x{zlib.crc32(packed):08x}),')


if __name__ == "__main__":
    main()
