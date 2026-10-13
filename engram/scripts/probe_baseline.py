"""Is each probe's answer in the model's context at an evaluation point?
Context = the compaction summary at the window line + the decoded content of
every later line before the ask line (prompts, assistant text, tool inputs
and results; thinking excluded). Merges with engram-bench probes output.
Usage: python3 scripts/probe_baseline.py TRANSCRIPT PROBES ROWS_JSON OUT_JSON"""
import json
import re
import sys

transcript, probes_path, rows_path, out_path = sys.argv[1:5]
lines = open(transcript).read().split("\n")
probes = json.load(open(probes_path))
rows = json.load(open(rows_path))


def decode(d):
    out = []
    m = d.get("message") or {}
    c = m.get("content")
    if isinstance(c, str):
        out.append(c)
    elif isinstance(c, list):
        for b in c:
            t = b.get("type")
            if t == "text":
                out.append(b.get("text", ""))
            elif t == "tool_use":
                out.append(json.dumps(b.get("input"), ensure_ascii=False))
            elif t == "tool_result":
                cc = b.get("content")
                out.append(cc if isinstance(cc, str) else json.dumps(cc, ensure_ascii=False))
    return "\n".join(out)


def context(w, a):
    parts = []
    for i in range(w - 1, min(a - 1, len(lines))):
        if lines[i]:
            parts.append(decode(json.loads(lines[i])))
    return "\n".join(parts)


points = sorted({r["point"] for r in rows})
ctx = {p: context(*map(int, p.split(":"))) for p in points}
summary_only = {p: decode(json.loads(lines[int(p.split(":")[0]) - 1])) for p in points}
for r in rows:
    pats = [re.compile(a) for a in probes[r["probe"]]["a"]]
    hit = lambda t: any(x.search(t) for x in pats)
    r["in_context"] = False if r["fresh"] else hit(ctx[r["point"]])
    r["in_summary"] = False if r["fresh"] else hit(summary_only[r["point"]])
json.dump(rows, open(out_path, "w"), indent=1)
for p in points:
    for fresh in (False, True):
        sel = [r for r in rows if r["point"] == p and r["fresh"] == fresh and r["stored"]]
        n = max(len(sel), 1)
        c = lambda f: sum(1 for r in sel if f(r)) / n
        print(f"{p} fresh={fresh} n={len(sel)}: in context {c(lambda r: r['in_context']):.2f} (summary {c(lambda r: r['in_summary']):.2f}); "
              f"engram injected {c(lambda r: r['inject_hit']):.2f} (gated {c(lambda r: r.get('auto_hit', False)):.2f}); context or injected {c(lambda r: r['in_context'] or r['inject_hit']):.2f}; "
              f"context or pull@8 {c(lambda r: r['in_context'] or r['pull8_hit']):.2f}; lost (not in context) {c(lambda r: not r['in_context']):.2f}, "
              f"of which recovered by injection {sum(1 for r in sel if not r['in_context'] and r['inject_hit'])} (gated {sum(1 for r in sel if not r['in_context'] and r.get('auto_hit'))})/{sum(1 for r in sel if not r['in_context'])}, by pull {sum(1 for r in sel if not r['in_context'] and r['pull8_hit'])}")
