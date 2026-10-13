#!/usr/bin/env python3
"""Does graph recall get more of the evidence into the context?

Reads an engram-structure rows file and compares packs at one presentation
and budget. The graph variant is chosen on one half of the questions (md5
of the question id, even) and reported on the other (odd), against the
layouts without the graph, with a paired bootstrap and a sign-flip test.

  python3 scripts/graph_recall_stats.py results/graph_recall/longmemeval_s_graph.rows.jsonl \
      --pres snip600 --budget 4000 --out results/graph_recall/stats_longmemeval_s.json

The metric is `all`: every evidence session is shown (for a question with
one evidence session, that some evidence is shown).
"""

import argparse
import gzip
import hashlib
import json
import random
from collections import defaultdict


def half(qid):
    return int(hashlib.md5(qid.encode()).hexdigest(), 16) % 2


def load(path):
    op = gzip.open if path.endswith(".gz") else open
    with op(path, "rt") as f:
        return [json.loads(l) for l in f if l.strip()]


def metric(row, key, name):
    m = row["pack"].get(key)
    if m is None:
        return None
    if name == "all":
        return m["all"]
    if name == "ans":
        return m.get("ans")
    return m[name]


def paired(a, b, iters=10000, seed=1):
    d = [x - y for x, y in zip(a, b)]
    n = len(d)
    mean = sum(d) / n
    rng = random.Random(seed)
    boots = sorted(sum(d[rng.randrange(n)] for _ in range(n)) / n for _ in range(iters))
    flips = sum(1 for _ in range(iters) if abs(sum(x if rng.random() < 0.5 else -x for x in d) / n) >= abs(mean) - 1e-12)
    return {"diff": mean, "ci95": [boots[int(0.025 * iters)], boots[int(0.975 * iters)]], "p": (flips + 1) / (iters + 1), "n": n}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("rows")
    ap.add_argument("--pres", default="snip600")
    ap.add_argument("--budget", type=int, default=4000)
    ap.add_argument("--graph-layout", default="turn+facts+graph")
    ap.add_argument("--baselines", default="turn,turn+facts")
    ap.add_argument("--out")
    args = ap.parse_args()
    rows = load(args.rows)
    suffix = f"|{args.pres}|{args.budget}"
    names = sorted({k[: -len(suffix)] for r in rows for k in r["pack"] if k.endswith(suffix)})
    variants = [n for n in names if n.startswith(args.graph_layout + ":")]
    bases = [b for b in args.baselines.split(",") if b in names] + [args.graph_layout]
    dev = [r for r in rows if half(r["qid"]) == 0]
    test = [r for r in rows if half(r["qid"]) == 1]

    def mean(rs, name, m):
        v = [metric(r, name + suffix, m) for r in rs]
        v = [x for x in v if x is not None]
        return sum(v) / len(v) if v else None

    dev_scores = {v: mean(dev, v, "all") for v in variants}
    best = max(variants, key=lambda v: dev_scores[v]) if variants else None
    out = {"pres": args.pres, "budget": args.budget, "questions": {"dev": len(dev), "test": len(test)}, "dev_all": dev_scores, "chosen": best, "test": {}, "by_type": {}, "comparisons": {}}
    multi = lambda rs: [r for r in rs if r["n_ev_sessions"] > 1]  # noqa: E731
    for name in bases + variants:
        out["test"][name] = {
            "ev": mean(test, name, "ev"), "all": mean(test, name, "all"), "all_multi": mean(multi(test), name, "all"),
            "ans": mean(test, name, "ans"), "chars": mean(test, name, "chars"),
        }
    by = defaultdict(list)
    for r in test:
        by[r["qtype"]].append(r)
    for ty, rs in sorted(by.items()):
        out["by_type"][ty] = {name: {"n": len(rs), "all": mean(rs, name, "all"), "ev": mean(rs, name, "ev")} for name in bases + ([best] if best else [])}
    if best:
        for base in bases:
            for m, sel in (("all", test), ("ev", test), ("all_multi", multi(test))):
                key = "all" if m == "all_multi" else m
                a = [metric(r, best + suffix, key) for r in sel]
                b = [metric(r, base + suffix, key) for r in sel]
                pairs = [(x, y) for x, y in zip(a, b) if x is not None and y is not None]
                if pairs:
                    out["comparisons"][f"{best} vs {base} ({m})"] = paired([p[0] for p in pairs], [p[1] for p in pairs])
    print(f"chosen on dev: {best}")
    print(f"{'ranking':<34}{'ev':>8}{'all':>8}{'all multi':>11}{'ans':>8}{'chars':>8}")
    for name in bases + variants:
        t = out["test"][name]
        f = lambda x: "—" if x is None else f"{x:.3f}"  # noqa: E731
        print(f"{name:<34}{f(t['ev']):>8}{f(t['all']):>8}{f(t['all_multi']):>11}{f(t['ans']):>8}{t['chars'] or 0:>8.0f}")
    for k, c in out["comparisons"].items():
        print(f"{k:<60} {c['diff']:+.3f} [{c['ci95'][0]:+.3f}, {c['ci95'][1]:+.3f}] p={c['p']:.4f} n={c['n']}")
    if args.out:
        json.dump(out, open(args.out, "w"), indent=1)


if __name__ == "__main__":
    main()
