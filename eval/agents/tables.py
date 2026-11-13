"""Tables for REPORT.md from replicated runs: each arm's passes, tokens, calls and wall time, each
task's passes per arm, and per-task ratios of every arm to nupdf's two over the tasks both passed.

  python3 eval/agents/tables.py DIR1 DIR2 ...   # each holding grades.json and metrics.json
"""
import json, math, statistics as st, sys

ARMS = ["native", "python", "pymupdf4llm", "pymupdf4llm-mcp", "pdf_oxide", "pdf_oxide-mcp", "nupdf", "nupdf-mcp"]
G, M = {}, {}
for k, d in enumerate(sys.argv[1:]):
    for g in json.load(open(f"{d}/grades.json")):
        G[(k, g["arm"], g["task"])] = g
    for m in json.load(open(f"{d}/metrics.json")):
        M[(k, m["arm"], m["task"])] = m
arms = [a for a in ARMS if any(key[1] == a for key in G)]
tasks = sorted({key[2] for key in G})
tok = lambda m: m["context_added"] + m["total_output_tokens"]
calls = lambda m: m["tool_calls"]
wall = lambda m: m["wall_s"] or 0.1

print("| Arm | Runs | Passed | Mean score | Gave up | Median tokens, passed | Median calls | Median wall s |")
print("|---|---|---|---|---|---|---|---|")
for a in arms:
    keys = [key for key in G if key[1] == a]
    p = [key for key in keys if G[key]["pass"]]
    m = [M[key] for key in keys if key in M]
    print(f"| {a} | {len(keys)} | {len(p)} ({100 * len(p) / len(keys):.0f}%) | {st.mean(G[key]['score'] for key in keys):.3f} "
          f"| {sum(1 for key in keys if G[key].get('gave_up'))} | {st.median(tok(M[key]) for key in p if key in M):,.0f} "
          f"| {st.median(calls(x) for x in m):.0f} | {st.median(wall(x) for x in m):.0f} |")
print("\n| Task | " + " | ".join(arms) + " |\n|---|" + "---|" * len(arms))
for t in tasks:
    cells = []
    for a in arms:
        ks = [key for key in G if key[1] == a and key[2] == t]
        cells.append(f"{sum(1 for key in ks if G[key]['pass'])}/{len(ks)}" if ks else "–")
    print(f"| {t} | " + " | ".join(cells) + " |")


def med(arm, task, f):
    v = [f(M[key]) for key in G if key[1] == arm and key[2] == task and G[key]["pass"] and key in M]
    return st.median(v) if v else None


print("\n| Arm | Against | Tasks both passed | Tokens | Calls | Wall time |\n|---|---|---|---|---|---|")
for ours in ["nupdf-mcp", "nupdf"]:
    for o in [a for a in arms if not a.startswith("nupdf")]:
        rows = []
        for t in tasks:
            a, b = [med(o, t, f) for f in (tok, calls, wall)], [med(ours, t, f) for f in (tok, calls, wall)]
            if None not in a and None not in b:
                rows.append([x / y for x, y in zip(a, b)])
        if rows:
            g = lambda i: math.exp(st.mean(math.log(r[i]) for r in rows))
            print(f"| {o} | {ours} | {len(rows)} | {g(0):.2f}x | {g(1):.2f}x | {g(2):.2f}x |")
