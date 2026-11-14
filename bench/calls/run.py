"""Times one call per file, as an agent makes it: a new process each time, start-up and imports included.

  python3 bench/calls/run.py --set typical|long --corpus ~/.cache/nupdf/corpus --nupdf PATH --mutool PATH --oxide PATH --python VENV/bin/python --out DIR [--runs 3]

Every tool is asked for the text of every page on stdout. `typical` is an evenly spaced 200 of the
standard tier's 5,142 files; `long` is the 30 long documents. For each file and tool it records the
median and fastest wall time, the processor time it used (user plus system, which a busy machine
does not stretch as it stretches wall time), the peak resident memory (`/usr/bin/time -l`, macOS), whether the call succeeded, and
keeps the text so summary.py can check it. Rows already in the output are skipped, so a run can resume.
"""
import argparse, glob, json, os, re, statistics, subprocess, time

ap = argparse.ArgumentParser()
for a in ("set", "corpus", "nupdf", "mutool", "python", "out"):
    ap.add_argument("--" + a, required=True)
ap.add_argument("--oxide", help="the pdf-oxide CLI (cargo install pdf_oxide_cli); left out when not given")
ap.add_argument("--runs", type=int, default=3)
ap.add_argument("--limit", type=int, default=120)
args = ap.parse_args()
SOURCES = ["pdf-differences", "pdf20examples", "pdfium", "pdfjs", "qpdf", "sample-files", "verapdf"]
if args.set == "typical":
    files = sorted(f for s in SOURCES for f in glob.glob(os.path.join(args.corpus, s, "**", "*.[pP][dD][fF]"), recursive=True))
    files = files[:: len(files) // 200][:200]
else:
    files = sorted(glob.glob(os.path.join(args.corpus, "long", "*.pdf")))
py = lambda code: lambda f: [args.python, "-c", "import sys\n" + code, f]
TOOLS = {
    "nupdf": lambda f: [args.nupdf, "extract", f, "--format", "text"],
    "nupdf-8": lambda f: [args.nupdf, "extract", f, "--format", "text", "--threads", "8"],
    "mutool": lambda f: [args.mutool, "draw", "-q", "-F", "txt", f],
    "pdftotext": lambda f: ["pdftotext", f, "-"],
    "pdf-oxide": lambda f: [args.oxide, "text", f],
    "pymupdf": py("import pymupdf\nd=pymupdf.open(sys.argv[1])\nsys.stdout.write(''.join(p.get_text() for p in d))"),
    "pypdfium2": py("import pypdfium2\nd=pypdfium2.PdfDocument(sys.argv[1])\nsys.stdout.write(''.join(d[i].get_textpage().get_text_range() for i in range(len(d))))"),
    "pdf_oxide": py("import pdf_oxide\nd=pdf_oxide.PdfDocument(sys.argv[1])\nsys.stdout.write(''.join(d.extract_text(i) for i in range(d.page_count())))"),
    "pypdf": py("import pypdf\nsys.stdout.write(''.join(p.extract_text() for p in pypdf.PdfReader(sys.argv[1]).pages))"),
    "pdfplumber": py("import pdfplumber\nd=pdfplumber.open(sys.argv[1])\nsys.stdout.write(''.join(p.extract_text() or '' for p in d.pages))"),
    "pdfminer": py("from pdfminer.high_level import extract_text\nsys.stdout.write(extract_text(sys.argv[1]))"),
}
if args.set == "typical":
    TOOLS.pop("nupdf-8")
if not args.oxide:
    TOOLS.pop("pdf-oxide")
os.makedirs(os.path.join(args.out, "text"), exist_ok=True)
log = os.path.join(args.out, "calls.jsonl")
done = {(r["file"], r["tool"]) for r in map(json.loads, open(log))} if os.path.exists(log) else set()
with open(log, "a") as out:
    for f in files:
        name = os.path.relpath(f, args.corpus)
        for tool, cmd in TOOLS.items():
            if (name, tool) in done:
                continue
            times, cpu, rss, ok = [], [], [], True
            kept = os.path.join(args.out, "text", f"{name.replace('/', '__')}.{tool}.txt")
            # The child writes straight to files: holding its output here would grow this process, and
            # every later spawn would pay for copying a large parent (seen as 180 to 280 ms a call).
            for _ in range(args.runs):
                with open(kept, "wb") as so, open(os.path.join(args.out, "stderr.tmp"), "wb") as se:
                    t = time.perf_counter()
                    try:
                        rc = subprocess.run(["/usr/bin/time", "-l"] + cmd(f), stdout=so, stderr=se, timeout=args.limit).returncode
                    except subprocess.TimeoutExpired:
                        ok = False; break
                    times.append(time.perf_counter() - t)
                with open(os.path.join(args.out, "stderr.tmp"), "rb") as se:
                    se.seek(max(0, os.path.getsize(se.name) - 4096)); err = se.read()
                c = re.search(rb"([\d.]+) real\s+([\d.]+) user\s+([\d.]+) sys", err)
                cpu.append(float(c.group(2)) + float(c.group(3)) if c else 0.0)
                m = re.search(rb"(\d+)\s+maximum resident set size", err)
                rss.append(int(m.group(1)) if m else 0)
                # mutool exits 1 when any page had an error and still writes the text of the rest
                if rc != 0 and not (tool == "mutool" and rc == 1 and os.path.getsize(kept)):
                    ok = False; break
            if not ok and os.path.exists(kept):
                os.remove(kept)
            out.write(json.dumps({"file": name, "tool": tool, "ok": ok, "seconds": statistics.median(times) if ok else None, "fastest": min(times) if ok else None, "cpu_seconds": statistics.median(cpu) if ok else None, "peak_mb": round(max(rss) / 1e6, 1) if ok else None}) + "\n"); out.flush()
print("done", len(files))
