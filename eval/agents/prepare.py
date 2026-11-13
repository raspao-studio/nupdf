"""Makes one folder per (arm, task) with its inputs and the prompt an agent is given.

  python3 eval/agents/prepare.py --out DIR --venv VENV --lit LIT --nupdf NUPDF
"""
import argparse, json, os, shutil

ap = argparse.ArgumentParser()
for a in ("out", "venv", "lit", "nupdf"):
    ap.add_argument("--" + a, required=True)
ap.add_argument("--only", default="", help="comma-separated task names; other tasks' folders are left alone")
ap.add_argument("--arms", default="", help="comma-separated arm names; other arms are left out")
args = ap.parse_args()
here = os.path.dirname(os.path.abspath(__file__))
tasks, arms = (json.load(open(os.path.join(here, f))) for f in ("tasks.json", "arms.json"))
out = os.path.abspath(args.out)
for arm, a in arms.items():
    if args.arms and arm not in args.arms.split(","):
        continue
    for name, t in tasks.items():
        if name in a.get("skip", []) or (args.only and name not in args.only.split(",")):
            continue
        d = os.path.join(out, "runs", f"{arm}-{name}")
        shutil.rmtree(d, ignore_errors=True)
        os.makedirs(d)
        for i in t["inputs"]:
            src = os.path.join(out, "fixtures", i)
            (shutil.copytree if os.path.isdir(src) else shutil.copy)(src, os.path.join(d, i))
        rule = a["rule"].format(venv=args.venv, lit=args.lit, nupdf=args.nupdf)
        open(os.path.join(d, "PROMPT.txt"), "w").write(f"""You are doing one task on PDF files. Work only inside this folder: {d}

Rule: {rule} Other tools for PDFs are off limits: no pdftotext, qpdf, mutool, strings or grep on the PDF bytes, no other libraries, and do not install anything. Ordinary shell commands and your own scripts on non-PDF files are fine.

Task: {t["task"]}

When you are done, write {d}/answer.json in exactly this shape:
{t["answer"]}

If the allowed tool cannot do the task, or you cannot be sure of the result, do not guess: write {{"failed": "<why, in a sentence>"}} to answer.json instead. Do not ask questions; finish on your own. End with one line saying what you wrote.
""")
print(sorted(os.listdir(os.path.join(out, "runs"))))
