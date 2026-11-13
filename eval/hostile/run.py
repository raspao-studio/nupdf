"""Opens each hostile file with each tool, as its own process, and records how the process ended.

  python3 eval/hostile/run.py --files DIR --nupdf PATH --mutool PATH --python VENV/bin/python --lit PATH [--timeout 60]

Each tool is asked for the same thing: the text of every page. Prints one JSON row per (file, tool) with
the exit status, a signal name if it was killed by one, wall seconds, peak resident memory in MB (as
`/usr/bin/time -l` reports it on macOS) and whether it timed out.
"""
import argparse, json, os, re, signal, subprocess, time

ap = argparse.ArgumentParser()
for a in ("files", "nupdf", "mutool", "python", "lit"):
    ap.add_argument("--" + a, required=True)
ap.add_argument("--timeout", type=int, default=60)
args = ap.parse_args()
PY = {
    "pymupdf": "import sys,pymupdf\nd=pymupdf.open(sys.argv[1])\nn=sum(len(p.get_text()) for p in d)\nprint(len(d),n)",
    "pypdf": "import sys,pypdf\nr=pypdf.PdfReader(sys.argv[1])\nn=sum(len(p.extract_text()) for p in r.pages)\nprint(len(r.pages),n)",
    "pdf_oxide": "import sys,pdf_oxide\nd=pdf_oxide.PdfDocument(sys.argv[1])\nn=sum(len(d.extract_text(i)) for i in range(d.page_count()))\nprint(d.page_count(),n)",
}
tools = {
    "nupdf": lambda f: [args.nupdf, "extract", f, "--format", "text"],
    "mupdf": lambda f: [args.mutool, "draw", "-q", "-F", "txt", f],
    "pymupdf": lambda f: [args.python, "-c", PY["pymupdf"], f],
    "pypdf": lambda f: [args.python, "-c", PY["pypdf"], f],
    "pdf_oxide": lambda f: [args.python, "-c", PY["pdf_oxide"], f],
    "liteparse": lambda f: [args.lit, "parse", f, "--format", "text", "--no-ocr"],
}
rows = []
for name in sorted(os.listdir(args.files)):
    f = os.path.join(args.files, name)
    for tool, cmd in tools.items():
        t = time.time()
        p = subprocess.Popen(["/usr/bin/time", "-l"] + cmd(f), stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, start_new_session=True)
        try:
            _, err = p.communicate(timeout=args.timeout)
            timed_out = False
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL); _, err = p.communicate(); timed_out = True
        err = err.decode("utf-8", "replace")
        rss = re.search(r"(\d+)\s+maximum resident set size", err)
        said = [l for l in err.split("        ")[0].strip().splitlines() if l.strip() and "real" not in l]
        rows.append({"file": name[:-4], "tool": tool, "exit": p.returncode, "signal": signal.Signals(-p.returncode).name if p.returncode < 0 and not timed_out else None,
                     "seconds": round(time.time() - t, 2), "timed_out": timed_out, "peak_mb": round(int(rss.group(1)) / 1e6) if rss else None, "stderr": said[-1][:110] if said else ""})
        print(json.dumps(rows[-1]), flush=True)
