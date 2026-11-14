"""Writes small PDFs built to exhaust a reader: each is valid enough to open and asks for far more than it holds.

  python3 eval/hostile/make.py DIR
"""
import os, sys, zlib

out = sys.argv[1]
os.makedirs(out, exist_ok=True)


def pdf(objs, trailer_extra=b"", size=None):
    data, offs = b"%PDF-1.7\n", []
    for i, o in enumerate(objs, 1):
        offs.append(len(data))
        data += b"%d 0 obj\n" % i + o + b"\nendobj\n"
    xref = len(data)
    n = len(objs) + 1
    data += b"xref\n0 %d\n0000000000 65535 f \n" % n + b"".join(b"%010d 00000 n \n" % o for o in offs)
    data += b"trailer\n<< /Size %d /Root 1 0 R %s>>\nstartxref\n%d\n%%%%EOF\n" % (size or n, trailer_extra, xref)
    return data


def stream(d, extra=b""):
    return b"<< /Length %d %s>>\nstream\n" % (len(d), extra) + d + b"\nendstream"


page = lambda contents, res=b"": b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents %s /Resources << /Font << /F1 5 0 R >> %s>> >>" % (contents, res)
font = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"
hello = b"BT /F1 12 Tf 72 720 Td (hello) Tj ET"
base = lambda content_obj, extra_objs=(), res=b"": [b"<< /Type /Catalog /Pages 2 0 R >>", b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>", page(b"4 0 R", res), content_obj, font, *extra_objs]

files = {}
# 1. A content stream that inflates to 512 MB of spaces (about 500 KB on disk).
z = zlib.compressobj(9)
bomb = b"".join(z.compress(b" " * (1 << 20)) for _ in range(512)) + z.flush()
files["flate-bomb"] = pdf(base(stream(hello + b"\n" + b"", b"") if False else stream(bomb, b"/Filter /FlateDecode ")))
# 2. A trailer that declares eight million objects in a file that has five.
files["xref-size"] = pdf(base(stream(hello)), size=8388607)
# 3. One array nested 200,000 deep.
files["deep-array"] = pdf(base(stream(hello), [b"[" * 200000 + b"]" * 200000]))
# 4. A page tree that says it has two billion pages.
o = base(stream(hello)); o[1] = b"<< /Type /Pages /Kids [3 0 R] /Count 2147483647 >>"
files["page-count"] = pdf(o)
# 5. A form that draws itself.
form = stream(b"/X Do", b"/Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << /XObject << /X 6 0 R >> >> ")
files["self-form"] = pdf(base(stream(b"/X Do"), [form], b"/XObject << /X 6 0 R >> "))
# 6. A stream filtered through FlateDecode a hundred times (each layer deflates the one inside).
d = hello
for _ in range(100):
    d = zlib.compress(d)
files["filter-chain"] = pdf(base(stream(d, b"/Filter [" + b"/FlateDecode " * 100 + b"] ")))
# 7. A page whose content is ten million one-glyph text operators (60 MB of work, 40 KB on disk).
files["many-ops"] = pdf(base(stream(zlib.compress(b"BT /F1 1 Tf " + b"(a) Tj " * 10_000_000 + b"ET", 9), b"/Filter /FlateDecode ")))
# 8. A page of ten million stroked paths: nothing to read, and a display list entry each for whoever keeps one.
files["many-paths"] = pdf(base(stream(zlib.compress(b"0 0 m 9 9 l S " * 10_000_000, 9), b"/Filter /FlateDecode ")))
# 9. Ten million glyphs that stay on the page, each a line of its own: text nothing clips away.
files["many-glyphs"] = pdf(base(stream(zlib.compress(b"BT /F1 9 Tf " + b"1 0 0 1 50 50 Tm (a) Tj 1 0 0 1 50 60 Tm (b) Tj " * 5_000_000 + b"ET", 9), b"/Filter /FlateDecode ")))
# 10. One path of ten million segments.
files["one-path"] = pdf(base(stream(zlib.compress(b"0 0 m " + b"9 9 l 3 5 l " * 5_000_000 + b"S", 9), b"/Filter /FlateDecode ")))
for name, data in files.items():
    open(os.path.join(out, name + ".pdf"), "wb").write(data)
    print(name, len(data))
