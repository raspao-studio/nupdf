#!/usr/bin/env python3
"""Makes the JPEG fixtures in this directory (see README.md).

Usage: python3 make.py [outdir]   (default: this directory)
Needs libjpeg-turbo 3's cjpeg (brew install jpeg-turbo) and Pillow.
"""
import math
import os
import subprocess
import sys
import tempfile

from PIL import Image

OUT = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(__file__))


def picture(tmp):
    """37x29 RGB gradients with a hard-edged square, and its gray."""
    w, h = 37, 29
    rgb = bytearray()
    for y in range(h):
        for x in range(w):
            r, g, b = x * 255 // (w - 1), y * 255 // (h - 1), (x * y * 7) & 255
            if 10 <= x < 20 and 8 <= y < 18:
                r, g, b = 255 - r, 40, 200
            rgb += bytes((r, g, b))
    gray = bytes(
        rgb[i] * 77 + rgb[i + 1] * 150 + rgb[i + 2] * 29 >> 8 for i in range(0, len(rgb), 3)
    )
    write(tmp, "src.ppm", b"P6 %d %d 255\n" % (w, h) + rgb)
    write(tmp, "src.pgm", b"P5 %d %d 255\n" % (w, h) + gray)


def noise(tmp):
    """24x16 gray noise from a 32-bit LCG, with sparse bright dots."""
    w, h, x = 24, 16, 12345
    px = bytearray()
    for _ in range(w * h):
        x = (x * 1103515245 + 12345) & 0xFFFFFFFF
        px.append(250 if (x >> 8) % 23 == 0 else 128 + ((x >> 16) % 9) - 4)
    write(tmp, "noise.pgm", b"P5 %d %d 255\n" % (w, h) + px)


def basis(tmp):
    """16x16 gray: the (4, 4) DCT basis function in every block, so each
    block's one AC coefficient follows a zero run longer than 16."""
    px = bytearray()
    for y in range(16):
        for x in range(16):
            c = math.cos((2 * (x % 8) + 1) * math.pi / 4) * math.cos((2 * (y % 8) + 1) * math.pi / 4)
            px.append(round(128 + 60 * c))
    write(tmp, "stripes.pgm", b"P5 16 16 255\n" + px)


def write(d, name, data):
    with open(os.path.join(d, name), "wb") as f:
        f.write(data)


SCANS = {
    # Two sequential scans: luma, then both chroma components.
    "seq": "0;\n1 2;\n",
    # One sequential scan per component.
    "sep": "0;\n1;\n2;\n",
    # Successive approximation: DC and AC first at Al=1, then refinements.
    "approx": "0: 0 0 0 1;\n0: 1 63 0 1;\n0: 0 0 1 0;\n0: 1 63 1 0;\n",
    # Progressive with each component's DC in its own scan.
    "sepdc": "0: 0 0 0 0;\n1: 0 0 0 0;\n2: 0 0 0 0;\n0: 1 63 0 0;\n1: 1 63 0 0;\n2: 1 63 0 0;\n",
}

CJPEG = [
    ("base420", "src.ppm", []),
    ("base422", "src.ppm", ["-sample", "2x1"]),
    ("base440", "src.ppm", ["-sample", "1x2"]),
    ("base444", "src.ppm", ["-quality", "90", "-sample", "1x1"]),
    ("base411", "src.ppm", ["-sample", "4x1"]),
    ("base32", "src.ppm", ["-sample", "3x2"]),
    ("base13", "src.ppm", ["-sample", "1x3"]),
    ("mixed", "src.ppm", ["-sample", "1x1,1x2,2x1"]),
    ("sep44", "src.ppm", ["-sample", "4x4", "-scans", "sep"]),
    ("gray", "src.pgm", ["-grayscale"]),
    ("rgb", "src.ppm", ["-rgb"]),
    ("opt", "src.ppm", ["-optimize"]),
    ("restart", "src.ppm", ["-restart", "1"]),
    ("restart1b", "src.ppm", ["-sample", "1x1", "-restart", "1B"]),
    ("multiscan", "src.ppm", ["-scans", "seq"]),
    ("prog", "src.ppm", ["-progressive"]),
    ("prog_gray", "src.pgm", ["-progressive", "-grayscale"]),
    ("prog_restart", "src.ppm", ["-progressive", "-restart", "2B"]),
    ("prog_sepdc", "src.ppm", ["-scans", "sepdc"]),
    ("arith", "src.ppm", ["-arithmetic"]),
    ("arith_prog", "src.ppm", ["-arithmetic", "-progressive"]),
    ("arith_restart", "src.ppm", ["-arithmetic", "-restart", "2B"]),
    ("arith_prog_restart", "src.ppm", ["-arithmetic", "-progressive", "-restart", "3B"]),
    ("p12", "src.ppm", ["-precision", "12"]),
    ("noise_base", "noise.pgm", ["-quality", "95"]),
    ("noise_prog", "noise.pgm", ["-quality", "95", "-progressive"]),
    ("noise_arith_prog", "noise.pgm", ["-quality", "95", "-arithmetic", "-progressive"]),
    ("stripes_prog", "stripes.pgm", ["-scans", "approx"]),
    ("stripes_arith_prog", "stripes.pgm", ["-arithmetic", "-scans", "approx"]),
]


def main():
    with tempfile.TemporaryDirectory() as tmp:
        picture(tmp)
        noise(tmp)
        basis(tmp)
        for name, text in SCANS.items():
            write(tmp, name, text.encode())
        for name, src, opts in CJPEG:
            opts = [os.path.join(tmp, o) if o in SCANS else o for o in opts]
            if "-quality" not in opts:
                opts = ["-quality", "75"] + opts
            out = os.path.join(OUT, name + ".jpg")
            subprocess.run(["cjpeg", *opts, "-outfile", out, os.path.join(tmp, src)], check=True)
        # CMYK with an Adobe marker (transform 0), as Pillow writes it.
        Image.open(os.path.join(tmp, "src.ppm")).convert("CMYK").save(
            os.path.join(OUT, "cmyk.jpg"), quality=75
        )


if __name__ == "__main__":
    main()
