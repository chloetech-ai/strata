#!/usr/bin/env python3
"""Pools the four projects' runs into pooled.json: the tail lane against the
lane without it, with ab_stats' tests (tasks as strata). Run from engram/:

  python3 results/memory_bench_tail/pool.py
"""

import json
import random
import sys

sys.path.insert(0, "scripts")
import ab_stats  # noqa: E402

DIR = "results/memory_bench_tail"
rows = []
for sc in ["bakehouse", "kestrel", "slotwise", "stockroom"]:
    for line in open(f"{DIR}/{sc}/runs.jsonl"):
        r = json.loads(line)
        # ab_stats compares conditions "engram" and "none".
        r["condition"] = "engram" if r["lane"] == "engram+tail" else "none"
        r["solved_f"] = float(r["solved"])
        rows.append(r)
st = ab_stats.strata(rows)
a = [r for v in st.values() for r in v["engram"]]
b = [r for v in st.values() for r in v["none"]]
sa, sb = sum(r["solved"] for r in a), sum(r["solved"] for r in b)
p, _ = ab_stats.fisher(sa, len(a) - sa, sb, len(b) - sb)
rng = random.Random(1)
ds, ps, _ = ab_stats.permutation(st, "score", 10000, rng)
dv, pv, _ = ab_stats.permutation(st, "solved_f", 10000, rng)
inf = [r for r in rows if r.get("kind") == "inference"]


def mean(lane, f):
    xs = [f(r) for r in rows if r["lane"] == lane]
    return sum(xs) / len(xs)


def per_lane(f):
    return {lane: mean(lane, f) for lane in ["engram+tail", "engram"]}


out = {
    "tasks": len(st),
    "runs": [len(a), len(b)],
    "solved": {"engram+tail": sa, "engram": sb},
    "fisher_solved_p": p,
    "score_diff": ds,
    "perm_score_p": ps,
    "solved_diff": dv,
    "perm_solved_p": pv,
    "inference": {
        "engram+tail": sum(r["solved"] for r in inf if r["lane"] == "engram+tail"),
        "engram": sum(r["solved"] for r in inf if r["lane"] == "engram"),
        "runs_each": sum(1 for r in inf if r["lane"] == "engram"),
    },
    "cost_usd_per_run": per_lane(lambda r: r["cost_usd"] or 0),
    "memory_cost_usd_per_run": per_lane(lambda r: r["memory_cost_usd"] or 0),
    "prompt_hook_chars_per_run": per_lane(lambda r: (r.get("hook_context_chars") or {}).get("UserPromptSubmit", 0)),
}
json.dump(out, open(f"{DIR}/pooled.json", "w"), indent=1)
print(json.dumps(out, indent=1))
