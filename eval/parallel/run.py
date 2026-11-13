"""Text of every page of one long PDF: one worker against eight, for nupdf's threads and for PyMuPDF's processes.

  python3 eval/parallel/run.py --pdf FILE --nupdf PATH --python VENV/bin/python [--runs 5]

nupdf shares one parsed document across threads (`--threads`). MuPDF documents cannot be shared across
threads, so the PyMuPDF side does what its documentation recommends: one process per slice of the page
range, each opening the file again. Prints the median wall time of each, in seconds.
"""
import argparse, json, statistics, subprocess, time

ap = argparse.ArgumentParser()
for a in ("pdf", "nupdf", "python"):
    ap.add_argument("--" + a, required=True)
ap.add_argument("--runs", type=int, default=5)
args = ap.parse_args()
MP = """
import sys, pymupdf
from multiprocessing import Pool
def work(a):
    path, lo, hi = a
    d = pymupdf.open(path)
    return sum(len(d[i].get_text()) for i in range(lo, hi))
if __name__ == "__main__":
    path, n = sys.argv[1], int(sys.argv[2])
    pages = len(pymupdf.open(path))
    step = -(-pages // n)
    parts = [(path, i, min(i + step, pages)) for i in range(0, pages, step)]
    print(sum(Pool(n).map(work, parts)) if n > 1 else work(parts[0]))
"""
open("/tmp/_mp_text.py", "w").write(MP)


def med(cmd):
    ts = []
    for _ in range(args.runs):
        t = time.time(); subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True); ts.append(time.time() - t)
    return round(statistics.median(ts), 3)


out = {}
for n in (1, 8):
    out[f"nupdf_threads_{n}"] = med([args.nupdf, "extract", args.pdf, "--format", "text", "--threads", str(n)])
    out[f"pymupdf_processes_{n}"] = med([args.python, "/tmp/_mp_text.py", args.pdf, str(n)])
print(json.dumps(out))
