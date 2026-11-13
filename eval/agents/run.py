"""Runs each prepared run folder as a fresh, headless Claude Code session, and sums up what each cost.

  python3 eval/agents/run.py --out DIR --nupdf NUPDF --model claude-opus-5-5 --effort high [--jobs 5] [--only RUN,...]
  python3 eval/agents/run.py --out DIR --summarize

Each session starts in its run folder with the built-in tools only (no skills, no plugins, none of the
user's settings or MCP servers): Bash, Read, Write, Edit, Glob and Grep, plus the arm's own MCP server if
it has one. Its full transcript (stream-json) goes to DIR/transcripts/<run>.jsonl. --summarize reads
them: tool calls, tool errors, output tokens, the context the task added (the last request's input
minus the first's), turns, wall time (the session's own, or the runner's clock when the session
ended without reporting it, as on a timeout), and the model and effort the session reported.
"""
import argparse, concurrent.futures, json, os, subprocess, time

ap = argparse.ArgumentParser()
ap.add_argument("--out", required=True)
ap.add_argument("--nupdf")
ap.add_argument("--pymupdf4llm-mcp", default="pymupdf4llm-mcp", help="the pymupdf4llm-mcp program")
ap.add_argument("--pdf-oxide-mcp", default="pdf-oxide-mcp", help="PDF Oxide's MCP server program")
ap.add_argument("--model", default="claude-opus-5-5")
ap.add_argument("--effort", default="high")
ap.add_argument("--jobs", type=int, default=5)
ap.add_argument("--only", default="")
ap.add_argument("--timeout", type=int, default=1800)
ap.add_argument("--summarize", action="store_true")
args = ap.parse_args()
here = os.path.dirname(os.path.abspath(__file__))
out = os.path.abspath(args.out)
arms = json.load(open(os.path.join(here, "arms.json")))
transcripts = os.path.join(out, "transcripts")
os.makedirs(transcripts, exist_ok=True)
TOOLS = ["Bash", "Read", "Write", "Edit", "Glob", "Grep"]


def arm_of(run):
    # The longest arm name the run's folder starts with (nupdf-mcp before nupdf).
    return max((a for a in arms if run.startswith(a + "-")), key=len)


def mcp_config(arm):
    servers = arms[arm].get("mcp")
    if not servers:
        return None
    path = os.path.join(out, "mcp", arm + ".json")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    text = json.dumps({"mcpServers": servers}).replace("{nupdf}", args.nupdf or "")
    text = text.replace("{pymupdf4llm_mcp}", args.pymupdf4llm_mcp).replace("{pdf_oxide_mcp}", args.pdf_oxide_mcp)
    open(path, "w").write(text)
    return path


def one(run):
    d = os.path.join(out, "runs", run)
    arm = arm_of(run)
    cmd = ["claude", "-p", open(os.path.join(d, "PROMPT.txt")).read(), "--model", args.model, "--effort", args.effort,
           "--output-format", "stream-json", "--verbose", "--tools", ",".join(TOOLS),
           "--allowedTools", " ".join(TOOLS + [f"mcp__{s}" for s in arms[arm].get("mcp", {})]),
           "--setting-sources", "project", "--disable-slash-commands", "--strict-mcp-config", "--no-session-persistence"]
    config = mcp_config(arm)
    if config:
        cmd += ["--mcp-config", config]
    start = time.time()
    with open(os.path.join(transcripts, run + ".jsonl"), "w") as log:
        try:
            r = subprocess.run(cmd, cwd=d, stdout=log, stderr=subprocess.PIPE, timeout=args.timeout)
            code = r.returncode
        except subprocess.TimeoutExpired:
            code = "timeout"
    wall = round(time.time() - start, 1)
    # The session reports its own duration only if it finishes; keep the runner's clock for every run.
    json.dump({"exit": code, "wall_s": wall}, open(os.path.join(transcripts, run + ".wall.json"), "w"))
    return run, code, wall


def summarize():
    rows = []
    for name in sorted(os.listdir(transcripts)):
        if not name.endswith(".jsonl"):
            continue
        run = name[:-6]
        seen, ctx, out_tokens, calls, errors, tools, init, result = set(), [], 0, 0, 0, {}, {}, {}
        for raw in open(os.path.join(transcripts, name), errors="replace"):
            try:
                j = json.loads(raw)
            except Exception:
                continue
            if j.get("type") == "system" and j.get("subtype") == "init":
                init = j
            if j.get("type") == "result":
                result = j
            m = j.get("message") if isinstance(j.get("message"), dict) else {}
            if j.get("type") == "assistant":
                for b in m.get("content") or []:
                    if isinstance(b, dict) and b.get("type") == "tool_use" and b.get("id") not in seen:
                        seen.add(b.get("id")); calls += 1; tools[b.get("name")] = tools.get(b.get("name"), 0) + 1
                u, mid = m.get("usage"), m.get("id")
                if u and mid not in seen:
                    seen.add(mid)
                    ctx.append(u.get("input_tokens", 0) + u.get("cache_creation_input_tokens", 0) + u.get("cache_read_input_tokens", 0))
                    out_tokens += u.get("output_tokens", 0)
            elif j.get("type") == "user":
                for b in m.get("content") or [] if isinstance(m.get("content"), list) else []:
                    if isinstance(b, dict) and b.get("type") == "tool_result":
                        errors += bool(b.get("is_error"))
        arm = arm_of(run)
        clock = os.path.join(transcripts, run + ".wall.json")
        clock = json.load(open(clock)) if os.path.exists(clock) else {}
        rows.append({"run": run, "arm": arm, "task": run[len(arm) + 1:], "tool_calls": calls, "tool_errors": errors,
                     "output_tokens": out_tokens, "context_added": (ctx[-1] - ctx[0]) if ctx else None, "requests": len(ctx),
                     "turns": result.get("num_turns"),
                     "wall_s": round(result["duration_ms"] / 1000, 1) if result.get("duration_ms") else clock.get("wall_s"),
                     "exit": clock.get("exit"),
                     "model": init.get("model"), "effort": init.get("effort") or init.get("effortLevel"),
                     "mcp": [s.get("name") + ":" + s.get("status", "") for s in init.get("mcp_servers") or []],
                     "is_error": result.get("is_error"), "tools": tools,
                     # The session's own totals: thinking included, what an API key would have paid.
                     "total_output_tokens": (result.get("usage") or {}).get("output_tokens"),
                     "cost_usd": result.get("total_cost_usd")})
    print(json.dumps(rows, indent=1))


if args.summarize:
    summarize()
else:
    runs = sorted(os.listdir(os.path.join(out, "runs")))
    if args.only:
        runs = [r for r in runs if r in args.only.split(",")]
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        for run, code, wall in pool.map(one, runs):
            print(json.dumps({"run": run, "exit": code, "wall_s": wall}), flush=True)
