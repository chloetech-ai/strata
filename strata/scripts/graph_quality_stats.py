#!/usr/bin/env python3
"""Significance of the reference-finding study (strata-graph-bench quality).

The held-out papers are split by id: even ids choose each method's number
of vector seeds (S) by nDCG@20, odd ids report it, so the reported numbers
are not tuned on themselves. Methods are compared on the report half with
paired tests over the same queries: the mean difference, a 95% bootstrap
interval, and a two-sided sign-flip permutation test.

  python3 scripts/graph_quality_stats.py results/graph/quality_arxiv.rows.jsonl.gz --out results/graph/quality_stats.json
"""

import argparse
import gzip
import json
import random
import re
from collections import defaultdict


def mean(xs):
    return sum(xs) / len(xs) if xs else 0.0


def paired(a, b, iters, rng):
    """a, b: per-query values on the same queries. Returns mean(a - b), its
    95% bootstrap interval and the two-sided sign-flip p-value."""
    d = [x - y for x, y in zip(a, b)]
    obs = mean(d)
    boots = sorted(mean([rng.choice(d) for _ in d]) for _ in range(iters))
    lo, hi = boots[int(0.025 * iters)], boots[int(0.975 * iters) - 1]
    hits = 0
    for _ in range(iters):
        s = mean([x if rng.random() < 0.5 else -x for x in d])
        hits += abs(s) >= abs(obs) - 1e-12
    return obs, (lo, hi), (hits + 1) / (iters + 1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("rows")
    ap.add_argument("--iters", type=int, default=10000)
    ap.add_argument("--out")
    args = ap.parse_args()
    rng = random.Random(1)
    by = defaultdict(dict)  # method -> query -> row
    for line in (gzip.open(args.rows, "rt") if args.rows.endswith(".gz") else open(args.rows)):
        r = json.loads(line)
        by[r["method"]][r["query"]] = r
    families = defaultdict(dict)  # family -> S -> method
    for m in by:
        mm = re.match(r"(.*) \(S=(\d+)\)$", m)
        if mm:
            families[mm.group(1)][int(mm.group(2))] = m
        else:
            families[m][None] = m
    queries = sorted(set.intersection(*(set(v) for v in by.values())))
    dev = [q for q in queries if q % 2 == 0]
    test = [q for q in queries if q % 2 == 1]
    chosen = {}
    sweep = {}
    for fam, ms in families.items():
        sweep[fam] = {str(s): {"dev_ndcg": mean([by[m][q]["ndcg"] for q in dev]), "test_ndcg": mean([by[m][q]["ndcg"] for q in test]),
                               "dev_recall": mean([by[m][q]["recall"] for q in dev]), "test_recall": mean([by[m][q]["recall"] for q in test])}
                      for s, m in ms.items()}
        best = max(ms, key=lambda s: mean([by[ms[s]][q]["ndcg"] for q in dev]))
        chosen[fam] = (best, ms[best])
    out = {"queries": {"dev": len(dev), "test": len(test)}, "methods": {}, "sweep": sweep, "comparisons": {}}
    for fam, (s, m) in chosen.items():
        out["methods"][fam] = {"S": s, "recall": mean([by[m][q]["recall"] for q in test]), "ndcg": mean([by[m][q]["ndcg"] for q in test]),
                               "ms": mean([by[m][q]["ms"] for q in test])}
    order = sorted(out["methods"], key=lambda f: -out["methods"][f]["ndcg"])
    print(f"{'method':<52}{'S':>4}{'recall@20':>11}{'nDCG@20':>9}{'ms':>8}")
    for f in order:
        r = out["methods"][f]
        print(f"{f:<52}{str(r['S'] or ''):>4}{r['recall']:>11.3f}{r['ndcg']:>9.3f}{r['ms']:>8.1f}")
    for base in ("vector", "bm25", "hybrid"):
        for fam in order:
            if fam == base:
                continue
            for metric in ("ndcg", "recall"):
                a = [by[chosen[fam][1]][q][metric] for q in test]
                b = [by[chosen[base][1]][q][metric] for q in test]
                d, ci, p = paired(a, b, args.iters, rng)
                out["comparisons"][f"{fam} vs {base} ({metric})"] = {"diff": d, "ci95": ci, "p": p, "relative": d / max(mean(b), 1e-12)}
    for k, v in out["comparisons"].items():
        if k.endswith("(ndcg)"):
            print(f"{k:<80} {v['diff']:+.3f} [{v['ci95'][0]:+.3f}, {v['ci95'][1]:+.3f}] ({v['relative']:+.0%}) p={v['p']:.4f}")
    if args.out:
        json.dump(out, open(args.out, "w"), indent=1)


if __name__ == "__main__":
    main()
