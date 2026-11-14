"""Builds the hero demo's data from a real file: one page of Free Culture, as nupdf reads it.

  python3 site/demo/build.py NUPDF MUTOOL freeculture.pdf

Writes page.png (the page as MuPDF draws it), page.json (the page as `nupdf extract --fields ...,chars`
gives it, cut down to ids, boxes, text and each character's left edge) and searches.json (what
`nupdf search` returns for a few needles, which the site's tests hold the in-page search to).
"""
import json, os, subprocess, sys

nupdf, mutool, pdf = sys.argv[1:4]
PAGE, here = 15, os.path.dirname(os.path.abspath(__file__))
run = lambda *a: subprocess.run(a, check=True, capture_output=True).stdout

subprocess.run([mutool, "draw", "-q", "-r", "132", "-o", os.path.join(here, "page.png"), pdf, str(PAGE)], check=True)
doc = json.loads(run(nupdf, "extract", pdf, "--pages", str(PAGE), "--fields", "page,bbox,blocks,lines,spans,font,size,text,chars"))
page = doc["pages"][0]
r = lambda v: round(v, 1)
out = {"schema": doc["schema"], "doc_id": doc["doc_id"], "file": "freeculture.pdf", "page": PAGE, "size": [r(v) for v in page["bbox"][2:]], "lines": []}
for block in page["blocks"]:
    for line in block.get("lines", []):
        spans = []
        for s in line["spans"]:
            quads = [c[-1] if isinstance(c[-1], list) else c["quad"] for c in s["chars"]] if s.get("chars") else []
            xs = [r(q[0]) for q in quads] + ([r(quads[-1][2])] if quads else [])
            spans.append({"id": s["id"], "bbox": [r(v) for v in s["bbox"]], "text": s["text"], "font": s["font"], "size": s["size"], "x": xs})
        out["lines"].append({"id": line["id"], "block": block["id"], "bbox": [r(v) for v in line["bbox"]], "spans": spans})
json.dump(out, open(os.path.join(here, "page.json"), "w"), separators=(",", ":"), ensure_ascii=False)

found = {}
for needle in ["property", "free culture", "Anarchy", "the"]:
    rep = json.loads(run(nupdf, "search", pdf, needle, "--pages", str(PAGE), "--options", "ignore-case"))
    found[needle] = [h.get("id") for h in rep["hits"]]
json.dump(found, open(os.path.join(here, "searches.json"), "w"), indent=1)
print(len(out["lines"]), "lines;", {k: len(v) for k, v in found.items()})
