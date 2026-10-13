#!/usr/bin/env python3
"""Summary of a long-session run (results/long/<scenario>) for the showcase:
per lane, the solved rate with a 95% Wilson interval, mean score, cost, time,
steps and tokens per task; what the history (the week of work before the
tasks) cost, Engram's distillation included; and per task, how many runs
each lane solved.

  python3 scripts/showcase_stats.py results/long/kestrel --out results/long/kestrel/summary.json
"""

import argparse
import json
import math
import os
import statistics
from collections import defaultdict

ORDER = ["fresh", "fresh+engram", "compact", "compact+engram", "compact+engram+graph", "full"]


def wilson(k, n, z=1.96):
    if not n:
        return (0.0, 0.0)
    p = k / n
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return (max(0.0, c - h), min(1.0, c + h))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--out")
    args = ap.parse_args()
    rows = [json.loads(l) for l in open(os.path.join(args.dir, "runs.jsonl"))]
    by = defaultdict(list)
    for r in rows:
        by[r["lane"]].append(r)
    lanes = [l for l in ORDER if l in by] + sorted(l for l in by if l not in ORDER)
    out = {"lanes": {}, "tasks": {}}
    for lane in lanes:
        rs = by[lane]
        n, k = len(rs), sum(r["solved"] for r in rs)
        conv = [r for r in rs if r.get("kind") == "conversation"]
        hpath = os.path.join(args.dir, f"history_{lane.replace('+', '_')}.json")
        h = json.load(open(hpath)) if os.path.exists(hpath) else {}
        d = h.get("distill") or {}
        ms = [m for r in rs for m in ((r.get("engram") or {}).get("recall_ms") or [])]
        out["lanes"][lane] = {
            "runs": n, "solved": k, "solved_rate": k / n, "solved_ci95": wilson(k, n),
            "conversation_solved_rate": sum(r["solved"] for r in conv) / len(conv) if conv else None,
            "mean_score": statistics.mean(r["score"] for r in rs),
            "cost_per_task_usd": statistics.mean(r["cost_usd"] or 0 for r in rs),
            "cost_per_solved_task_usd": sum(r["cost_usd"] or 0 for r in rs) / k if k else None,
            "wall_s_mean": statistics.mean(r["wall_s"] for r in rs),
            "wall_s_median": statistics.median(r["wall_s"] for r in rs),
            "steps_mean": statistics.mean(r["steps"] or 0 for r in rs),
            "input_tokens_mean": statistics.mean(r["input_tokens"] for r in rs),
            "errors": sum(bool(r.get("error")) for r in rs),
            "memory_tool_calls_mean": statistics.mean(r.get("memory_tool_calls") or 0 for r in rs),
            "history_cost_usd": h.get("cost_usd"),
            "history_distill_cost_usd": d.get("cost_usd"),
            "history_wall_s": sum(t.get("wall_s") or 0 for t in h.get("turns", [])),
            "history_context_tokens": h.get("context_tokens"),
            "recall_ms_median": statistics.median(ms) if ms else None,
            "recall_ms_p95": sorted(ms)[int(0.95 * (len(ms) - 1))] if ms else None,
        }
    for r in rows:
        t = out["tasks"].setdefault(r["task"], {"kind": r.get("kind"), "lanes": {}})
        s = t["lanes"].setdefault(r["lane"], {"solved": 0, "runs": 0, "score": 0.0})
        s["solved"] += r["solved"]
        s["runs"] += 1
        s["score"] += r["score"]
    for t in out["tasks"].values():
        for s in t["lanes"].values():
            s["score"] /= s["runs"]
    hdr = f"{'lane':<22}{'solved':>12}{'95% CI':>14}{'conv':>7}{'score':>7}{'$/task':>8}{'$/solved':>9}{'wall s':>8}{'steps':>7}{'in tok':>9}{'hist $':>8}{'distill $':>10}"
    print(hdr)
    for lane in lanes:
        s = out["lanes"][lane]
        lo, hi = s["solved_ci95"]
        print(f"{lane:<22}{s['solved']:>5}/{s['runs']:<3} {s['solved_rate']:>4.0%} {lo:>5.0%}-{hi:<5.0%}{(s['conversation_solved_rate'] or 0):>7.0%}{s['mean_score']:>7.2f}"
              f"{s['cost_per_task_usd']:>8.3f}{(s['cost_per_solved_task_usd'] or 0):>9.3f}{s['wall_s_mean']:>8.0f}{s['steps_mean']:>7.1f}{s['input_tokens_mean']:>9.0f}"
              f"{(s['history_cost_usd'] or 0):>8.2f}{(s['history_distill_cost_usd'] or 0):>10.3f}")
    print()
    print(f"{'task':<22}{'kind':<14}" + "".join(f"{l:>22}" for l in lanes))
    for tid, t in out["tasks"].items():
        print(f"{tid:<22}{t['kind'] or '':<14}" + "".join(f"{t['lanes'].get(l, {}).get('solved', 0):>18}/{t["lanes"].get(l, {}).get("runs", 0):<3}" for l in lanes))
    if args.out:
        json.dump(out, open(args.out, "w"), indent=1)


if __name__ == "__main__":
    main()
