#!/usr/bin/env python3
"""Decodes files as MuPDF's PDF stream filters do, for pinning in tests.

Usage: python3 oracle.py "<stream dictionary entries>" file...
  e.g. python3 oracle.py "/Filter /DCTDecode" jpeg/*.jpg

Each file becomes the only stream of a small PDF, with the given entries in
its dictionary; `mutool run` reads it back decoded (readStream). One line is
printed per file, in the form the tests' tables use:

    ("name", length, "first 16 hex digits of the SHA-256", &["warning", ...]),

with MuPDF's distinct stderr lines ("warning: " removed, repeats dropped).

A file `<name>.obj4` beside an input becomes stream object 4 of its PDF,
and `$obj4` in the entries becomes a reference to it (or null without
one): "/Filter /JBIG2Decode /DecodeParms << /JBIG2Globals $obj4 >>". A
file `<name>.extras` gives that input its own entries instead.
The mutool used is $NUPDF_MUTOOL or the one `cargo xtask oracle build`
makes.
"""
import hashlib
import os
import subprocess
import sys
import tempfile

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "../../../.."))
MUTOOL = os.environ.get("NUPDF_MUTOOL") or os.path.join(
    ROOT, "target/oracle/mupdf-1.28.6/build/release/mutool"
)
DUMP_JS = """
var doc = Document.openDocument(scriptArgs[0]);
try { doc.newIndirect(1, 0).readStream().save(scriptArgs[1]); }
catch (e) { print("error " + String(e.message || e)); }
"""


def pdf(data, extras, obj4=None):
    objs = [
        b"<< /Length %d %s >>\nstream\n" % (len(data), extras.encode()) + data + b"\nendstream",
        b"<< /Type /Catalog /Pages 3 0 R >>",
        b"<< /Type /Pages /Kids [] /Count 0 >>",
    ]
    if obj4 is not None:
        objs.append(b"<< /Length %d >>\nstream\n" % len(obj4) + obj4 + b"\nendstream")
    out = bytearray(b"%PDF-1.7\n")
    offsets = []
    for i, o in enumerate(objs):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % (i + 1) + o + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1)
    for o in offsets:
        out += b"%010d 00000 n \n" % o
    out += b"trailer\n<< /Size %d /Root 2 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, xref)
    return bytes(out)


def main():
    extras, files = sys.argv[1], sys.argv[2:]
    with tempfile.TemporaryDirectory() as d:
        js = os.path.join(d, "dump.js")
        with open(js, "w") as f:
            f.write(DUMP_JS)
        for path in files:
            src = os.path.join(d, "in.pdf")
            out = os.path.join(d, "out")
            if os.path.exists(out):
                os.remove(out)
            with open(path, "rb") as f:
                data = f.read()
            obj4 = None
            if os.path.exists(path + ".obj4"):
                with open(path + ".obj4", "rb") as f:
                    obj4 = f.read()
            entries = extras
            if os.path.exists(path + ".extras"):
                with open(path + ".extras") as f:
                    entries = f.read()
            entries = entries.replace("$obj4", "4 0 R" if obj4 is not None else "null")
            with open(src, "wb") as f:
                f.write(pdf(data, entries, obj4))
            r = subprocess.run([MUTOOL, "run", js, src, out], capture_output=True, text=True)
            decoded = b""
            if os.path.exists(out):
                with open(out, "rb") as f:
                    decoded = f.read()
            warnings = []
            for line in (r.stderr + r.stdout).splitlines():
                line = line.strip()
                if not line or "repeated" in line:
                    continue
                if line.startswith("warning: "):
                    line = line[len("warning: "):]
                if line not in warnings:
                    warnings.append(line)
            name = os.path.basename(path).split(".")[0]
            h = hashlib.sha256(decoded).hexdigest()[:16]
            quoted = ", ".join('"%s"' % w.replace("\\", "\\\\").replace('"', '\\"') for w in warnings)
            print(f'("{name}", {len(decoded)}, "{h}", &[{quoted}]),')


if __name__ == "__main__":
    main()
