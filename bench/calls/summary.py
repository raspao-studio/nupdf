"""Summarises bench/calls: per tool, how many calls gave the right text, and what they cost.

  python3 bench/calls/summary.py DIR --mutool PATH

Speed is over the files every tool completed, so no tool is timed on a different set. Text is judged
apart from speed: a file is gradable when MuPDF's and PDFium's words for it agree (token F1 of 0.9 or
more, through their Python bindings), which keeps out files whose text is in dispute, and a call is
right when its words match PDFium's (F1 of 0.8 or more). PDFium is the judge because nupdf follows
MuPDF. Pages come from `mutool info`. Each call's wall time is the median of its runs; `fastest_*` uses
the fastest run instead, and `cpu_*` the processor time (user plus system), neither of which a busy
machine stretches as it stretches the median. Prints JSON.
"""
import argparse, collections, json, os, re, statistics, subprocess

ap = argparse.ArgumentParser()
ap.add_argument("dir")
ap.add_argument("--mutool", required=True)
ap.add_argument("--corpus", default=os.path.expanduser("~/.cache/nupdf/corpus"))
args = ap.parse_args()
rows = [json.loads(l) for l in open(os.path.join(args.dir, "calls.jsonl"))]
tools = list(dict.fromkeys(r["tool"] for r in rows))
files = list(dict.fromkeys(r["file"] for r in rows))
by = {(r["file"], r["tool"]): r for r in rows}


def words(f, tool):
    p = os.path.join(args.dir, "text", f"{f.replace('/', '__')}.{tool}.txt")
    if not os.path.exists(p):
        return None
    return collections.Counter(re.findall(r"\w+", open(p, "rb").read().decode("utf-8", "replace").lower()))


def f1(a, b):
    if a is None or b is None:
        return 0.0
    if not a and not b:
        return 1.0
    same = sum((a & b).values())
    return 2 * same / (sum(a.values()) + sum(b.values())) if same else 0.0


def pages(f):
    o = subprocess.run([args.mutool, "info", os.path.join(args.corpus, f)], capture_output=True).stdout.decode("utf-8", "replace")
    m = re.search(r"Pages:\s+(\d+)", o)
    return int(m.group(1)) if m else 0


gradable, right = [], collections.defaultdict(set)
for f in files:
    judge = words(f, "pypdfium2")
    if f1(judge, words(f, "pymupdf")) < 0.9:
        continue
    gradable.append(f)
    for t in tools:
        if by[(f, t)]["ok"] and f1(judge, words(f, t)) >= 0.8:
            right[t].add(f)
common = [f for f in files if all(by[(f, t)]["ok"] for t in tools)]
npages = {f: pages(f) for f in common}
out = {"files": len(files), "gradable": len(gradable), "common": len(common), "common_pages": sum(npages.values()), "tools": []}
for t in tools:
    secs = [by[(f, t)]["seconds"] for f in common]
    fast = [by[(f, t)]["fastest"] for f in common]
    cpu = [by[(f, t)]["cpu_seconds"] for f in common]
    out["tools"].append({"tool": t, "completed": sum(by[(f, t)]["ok"] for f in files), "right": len(right[t]),
                         "median_ms": round(statistics.median(secs) * 1000, 1), "mean_ms": round(statistics.mean(secs) * 1000, 1), "total_s": round(sum(secs), 2),
                         "pages_per_s": round(sum(npages.values()) / sum(secs)) if sum(secs) else None,
                         "fastest_median_ms": round(statistics.median(fast) * 1000, 1), "fastest_total_s": round(sum(fast), 2), "fastest_pages_per_s": round(sum(npages.values()) / sum(fast)) if sum(fast) else None,
                         "cpu_median_ms": round(statistics.median(cpu) * 1000, 1), "cpu_total_s": round(sum(cpu), 2),
                         "median_peak_mb": round(statistics.median(by[(f, t)]["peak_mb"] for f in common), 1), "max_peak_mb": round(max(by[(f, t)]["peak_mb"] for f in common), 1)})
out["tools"].sort(key=lambda r: r["total_s"])
print(json.dumps(out, indent=1))
