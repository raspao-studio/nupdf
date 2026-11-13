"""Reads each run's agent transcript (Claude Code JSONL) and prints what it cost, as the API counted it.

  python3 eval/agents/metrics.py --map agents.tsv --transcripts DIR

Per run: tool calls, tool calls that returned an error, output tokens, and the tokens the task added to
the model's context (the last request's input minus the first request's, so the fixed system prompt,
which is the same for every arm, is left out). Token counts are the API's own `usage`, not an estimate.
"""
import argparse, json, os

ap = argparse.ArgumentParser()
ap.add_argument("--map", required=True)
ap.add_argument("--transcripts", required=True)
args = ap.parse_args()
rows = []
for line in open(args.map):
    run, agent = line.split()
    path = os.path.join(args.transcripts, agent + ".output")
    seen, ctx, out, calls, errors, biggest, tools = set(), [], 0, 0, 0, 0, {}
    for raw in open(path, errors="replace"):
        try:
            j = json.loads(raw)
        except Exception:
            continue
        m = j.get("message") if isinstance(j.get("message"), dict) else {}
        if j.get("type") == "assistant":
            for b in m.get("content") or []:
                if isinstance(b, dict) and b.get("type") == "tool_use" and b.get("id") not in seen:
                    seen.add(b.get("id")); calls += 1; tools[b.get("name")] = tools.get(b.get("name"), 0) + 1
            u, mid = m.get("usage"), m.get("id")
            if u and mid not in seen:
                seen.add(mid)
                ctx.append(u.get("input_tokens", 0) + u.get("cache_creation_input_tokens", 0) + u.get("cache_read_input_tokens", 0))
                out += u.get("output_tokens", 0)
        elif j.get("type") == "user":
            for b in m.get("content") or [] if isinstance(m.get("content"), list) else []:
                if isinstance(b, dict) and b.get("type") == "tool_result":
                    errors += bool(b.get("is_error"))
                    biggest = max(biggest, len(json.dumps(b.get("content"))))
    arm, task = run.rsplit("-", 1)
    rows.append({"arm": arm, "task": task, "tool_calls": calls, "tool_errors": errors, "output_tokens": out,
                 "context_added": (ctx[-1] - ctx[0]) if ctx else None, "requests": len(ctx), "largest_result_chars": biggest, "tools": tools})
print(json.dumps(rows, indent=1))
