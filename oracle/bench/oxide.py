# One operation on one file with pdf_oxide, as its Python docs do it: what
# `cargo xtask bench --against pdf_oxide` times against nupdf and MuPDF.
# Usage: oxide.py <op> <file> <out>
#
# <out> is a file for text, tables, annots, forms and signatures, and a
# directory for render (one PAM file per page). The last line of stdout is
# a JSON object: `work_ms`, the time from opening the document to the last
# byte written (the interpreter and `import pdf_oxide` left out, and so are
# the checks below); `import_ms`; `pages`; `failed_pages`, pages on which
# pdf_oxide raised; and `units`, what the output holds, so that a fast
# failure is not counted as a win:
#
#   info        the page count
#   text        non-whitespace characters, `extract_text` with its defaults
#   text-fast   the same with `extract_tables=False`, its speed-first mode
#   render      pages with any non-white pixel, `render_pixmap` at 72 dpi
#   tables      tables found by `extract_tables`
#   annots      annotations `get_annotations` lists
#   forms       fields `get_form_fields` lists
#   signatures  signatures `signatures` lists, each checked with
#               `verify_detached` (digest and signer attributes)

import json
import operator
import sys
import time

start = time.perf_counter()
import pdf_oxide  # noqa: E402

imported = time.perf_counter()


class Clock:
    """Time spent inside `with clock:` blocks, in ms."""

    def __init__(self):
        self.ms = 0.0

    def __enter__(self):
        self.t = time.perf_counter()

    def __exit__(self, *exc):
        self.ms += (time.perf_counter() - self.t) * 1000.0


def plain(v):
    """A value json can write; pdf_oxide's objects as their repr."""
    if isinstance(v, (str, int, float, bool)) or v is None:
        return v
    if isinstance(v, (list, tuple)):
        return [plain(x) for x in v]
    if isinstance(v, dict):
        return {str(k): plain(x) for k, x in v.items()}
    if isinstance(v, bytes):
        return v.hex()
    return repr(v)


def per_page(doc, n, clock, f, work):
    """`work(i)` for every page, inside the clock; failures counted."""
    failed = 0
    results = []
    for i in range(n):
        try:
            with clock:
                r = work(i)
                if f is not None:
                    f.write(json.dumps(plain(r)))
                    f.write("\n")
            results.append(r)
        except Exception:
            failed += 1
    return results, failed


def main(op, path, out):
    clock = Clock()
    with clock:
        doc = pdf_oxide.PdfDocument(path)
        n = operator.index(doc.page_count)
    failed = 0
    units = 0
    if op == "info":
        with clock:
            doc.version()
        units = n
    elif op in ("text", "text-fast"):
        tables = op == "text"
        with open(out, "w", encoding="utf-8", errors="replace") as f:
            def page(i):
                t = doc.extract_text(i, extract_tables=tables)
                f.write(t)
                f.write("\f\n")
                return t

            texts, failed = per_page(doc, n, clock, None, page)
        units = sum(len("".join(t.split())) for t in texts)
    elif op == "render":
        def page(i):
            pm = doc.render_pixmap(i, dpi=72)
            with open(f"{out}/p{i + 1}.pam", "wb") as f:
                f.write(
                    b"P7\nWIDTH %d\nHEIGHT %d\nDEPTH 4\nMAXVAL 255\n"
                    b"TUPLTYPE RGB_ALPHA\nENDHDR\n" % (pm.width, pm.height)
                )
                f.write(pm.data)
            return pm

        units = 0
        for i in range(n):
            try:
                pm = None
                with clock:
                    pm = page(i)
            except Exception:
                failed += 1
            if pm is not None and len(pm.data) and pm.data.count(255) != len(pm.data):
                units += 1
            del pm
    elif op in ("tables", "annots"):
        get = doc.extract_tables if op == "tables" else doc.get_annotations
        with open(out, "w", encoding="utf-8") as f:
            found, failed = per_page(doc, n, clock, f, get)
        units = sum(len(r or []) for r in found)
    elif op == "forms":
        with clock:
            fields = [
                {
                    "name": x.name,
                    "type": x.field_type,
                    "value": plain(x.value),
                    "bounds": x.bounds,
                    "flags": x.flags,
                    "read_only": x.is_readonly,
                }
                for x in doc.get_form_fields()
            ]
            with open(out, "w", encoding="utf-8") as f:
                json.dump(fields, f)
        units = len(fields)
    elif op == "signatures":
        with clock:
            with open(path, "rb") as f:
                data = f.read()
            sigs = []
            for s in doc.signatures():
                try:
                    ok = s.verify_detached(data)
                except Exception as e:
                    ok = str(e)
                sigs.append({"signer": s.signer_name, "whole": s.covers_whole_document, "ok": ok})
            with open(out, "w", encoding="utf-8") as f:
                json.dump(sigs, f)
        units = len(sigs)
    else:
        raise SystemExit(f"unknown op {op}")
    print(
        json.dumps(
            {
                "work_ms": clock.ms,
                "import_ms": (imported - start) * 1000.0,
                "pages": n,
                "failed_pages": failed,
                "units": units,
            }
        )
    )


if __name__ == "__main__":
    main(*sys.argv[1:4])
