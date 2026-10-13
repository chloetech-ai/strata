#!/usr/bin/env python3
"""Significance of an A/B result from scripts/ab_eval.py.

For each scenario (and pooled over the scenarios given):
- solved counts per condition, with Fisher's exact test (two-sided and
  one-sided, Engram better);
- a permutation test stratified by task: condition labels are shuffled
  within each task, so a difference between tasks cannot pass for an effect;
  the statistic is the difference in mean score (and, separately, in solved
  runs);
- the mean score difference with a 95% two-stage bootstrap interval
  (tasks, then runs within each task and condition);
- the same per kind of task (conversation, repo, control), so harm on the
  controls shows.

  python3 scripts/ab_stats.py results/ab/stockroom/runs.jsonl results/ab/slotwise/runs.jsonl [--out stats.json]
  python3 scripts/ab_stats.py tally-dev=results/ab/runs_v2.jsonl:engram,results/ab/runs.jsonl:none \
      stockroom=results/ab/stockroom/runs.jsonl slotwise=results/ab/slotwise/runs.jsonl --pool stockroom,slotwise

An argument is a runs file (named by its directory) or NAME=SPEC, where SPEC
lists files, each optionally limited to one condition (FILE:engram). The
scenario whose task kinds apply is NAME up to the first '-'.

Standard library only.
"""

import argparse
import json
import math
import random
from collections import defaultdict

import ab_eval


def lchoose(n, k):
    return math.lgamma(n + 1) - math.lgamma(k + 1) - math.lgamma(n - k + 1)


def fisher(a, b, c, d):
    """Fisher's exact test on [[a, b], [c, d]] (rows: Engram, none; columns:
    solved, not solved). Returns (two-sided p, one-sided p for a larger a)."""
    r1, r2, c1 = a + b, c + d, a + c
    n = r1 + r2
    lo, hi = max(0, c1 - r2), min(r1, c1)

    def p(x):
        return math.exp(lchoose(r1, x) + lchoose(r2, c1 - x) - lchoose(n, c1))

    pa = p(a)
    two = sum(p(x) for x in range(lo, hi + 1) if p(x) <= pa * (1 + 1e-9))
    one = sum(p(x) for x in range(a, hi + 1))
    return min(1.0, two), min(1.0, one)


def strata(rows):
    """task -> {"engram": [rows], "none": [rows]}"""
    by = defaultdict(lambda: {"engram": [], "none": []})
    for r in rows:
        by[(r.get("scenario", ""), r["task"])][r["condition"]].append(r)
    return {k: v for k, v in by.items() if v["engram"] and v["none"]}


def diff(st, key):
    e = [r[key] for v in st.values() for r in v["engram"]]
    n = [r[key] for v in st.values() for r in v["none"]]
    return sum(map(float, e)) / len(e) - sum(map(float, n)) / len(n)


def permutation(st, key, iters, rng):
    """One- and two-sided p of the observed difference under shuffling the
    condition labels within each task."""
    obs = diff(st, key)
    pools = [([float(r[key]) for r in v["engram"] + v["none"]], len(v["engram"])) for v in st.values()]
    ne = sum(k for _, k in pools)
    nn = sum(len(p) - k for p, k in pools)
    total = sum(sum(p) for p, _ in pools)
    ge = ab = 0
    for _ in range(iters):
        se = 0.0
        for p, k in pools:
            se += sum(rng.sample(p, k))
        d = se / ne - (total - se) / nn
        ge += d >= obs - 1e-12
        ab += abs(d) >= abs(obs) - 1e-12
    return obs, (ab + 1) / (iters + 1), (ge + 1) / (iters + 1)


def bootstrap(st, key, iters, rng):
    """Two-stage: tasks are resampled, then runs within each resampled task
    and condition, so the interval covers the variation between tasks."""
    cells = list(st.values())
    ds = []
    for _ in range(iters):
        e, n = [], []
        for _ in cells:
            v = rng.choice(cells)
            e += [float(rng.choice(v["engram"])[key]) for _ in v["engram"]]
            n += [float(rng.choice(v["none"])[key]) for _ in v["none"]]
        ds.append(sum(e) / len(e) - sum(n) / len(n))
    ds.sort()
    return ds[int(0.025 * iters)], ds[int(0.975 * iters) - 1]


def analyse(rows, iters, rng):
    st = strata(rows)
    if not st:
        return None
    runs = [r for v in st.values() for c in ("engram", "none") for r in v[c]]
    es = [r for r in runs if r["condition"] == "engram"]
    ns = [r for r in runs if r["condition"] == "none"]
    a, c = sum(r["solved"] for r in es), sum(r["solved"] for r in ns)
    two, one = fisher(a, len(es) - a, c, len(ns) - c)
    obs, p2, p1 = permutation(st, "score", iters, rng)
    sobs, sp2, sp1 = permutation(st, "solved", iters, rng)
    lo, hi = bootstrap(st, "score", min(iters, 5000), rng)
    mean = lambda xs: sum(xs) / len(xs)  # noqa: E731
    return {
        "tasks": len(st), "runs": {"engram": len(es), "none": len(ns)},
        "solved": {"engram": a, "none": c},
        "solved_rate": {"engram": a / len(es), "none": c / len(ns)},
        "score": {"engram": mean([r["score"] for r in es]), "none": mean([r["score"] for r in ns])},
        "fisher_solved": {"two_sided": two, "one_sided": one},
        "perm_score": {"diff": obs, "two_sided": p2, "one_sided": p1},
        "perm_solved": {"diff": sobs, "two_sided": sp2, "one_sided": sp1},
        "score_diff_ci95": [lo, hi],
        "cost_usd": {"engram": mean([r.get("cost_usd") or 0 for r in es]), "none": mean([r.get("cost_usd") or 0 for r in ns])},
        "turns": {"engram": mean([r.get("turns") or 0 for r in es]), "none": mean([r.get("turns") or 0 for r in ns])},
        "wall_s": {"engram": mean([r.get("wall_s") or 0 for r in es]), "none": mean([r.get("wall_s") or 0 for r in ns])},
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("runs", nargs="+", help="runs.jsonl files (named by directory) or NAME=FILE[:condition],...")
    ap.add_argument("--pool", default=None, help="comma-separated names to pool (default: all, if more than one)")
    ap.add_argument("--iters", type=int, default=20000)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--out", default=None)
    args = ap.parse_args()
    rng = random.Random(args.seed)
    rows = []
    names = []
    for arg in args.runs:
        if "=" in arg:
            name, spec = arg.split("=", 1)
        else:
            name = arg.rstrip("/").split("/")[-2]
            name, spec = ("tally" if name == "ab" else name), arg
        names.append(name)
        kinds = {t["id"]: t.get("kind") for t in ab_eval.load_scenario(name.split("-")[0])["tasks"]}
        for part in spec.split(","):
            path, _, cond = part.partition(":")
            for line in open(path):
                r = json.loads(line)
                if cond and r["condition"] != cond:
                    continue
                r["scenario"] = name
                r["kind"] = r.get("kind") or kinds.get(r["task"])
                rows.append(r)
    out = {}
    pool = [p for p in args.pool.split(",") if p] if args.pool is not None else (names if len(names) > 1 else [])
    for g in names + (["pooled"] if pool else []):
        rs = [r for r in rows if r["scenario"] in pool] if g == "pooled" else [r for r in rows if r["scenario"] == g]
        out[g] = {"all": analyse(rs, args.iters, rng)}
        for kind in sorted({r.get("kind") or "?" for r in rs}):
            out[g][kind] = analyse([r for r in rs if (r.get("kind") or "?") == kind], args.iters, rng)
    for g, d in out.items():
        for k, a in d.items():
            if not a:
                continue
            print(f"{g:<10} {k:<13} solved E {a['solved']['engram']:>2}/{a['runs']['engram']:<3} N {a['solved']['none']:>2}/{a['runs']['none']:<3} "
                  f"score E {a['score']['engram']:.2f} N {a['score']['none']:.2f} Δ {a['perm_score']['diff']:+.2f} "
                  f"[{a['score_diff_ci95'][0]:+.2f}, {a['score_diff_ci95'][1]:+.2f}]  "
                  f"p(Fisher, 2s) {a['fisher_solved']['two_sided']:.4f}  p(perm score, 2s) {a['perm_score']['two_sided']:.4f}  "
                  f"p(perm solved, 2s) {a['perm_solved']['two_sided']:.4f}")
    if args.out:
        json.dump(out, open(args.out, "w"), indent=1)


if __name__ == "__main__":
    main()
