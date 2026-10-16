#!/usr/bin/env python3
"""Paired comparison of memory systems graded on the same questions.

Reads the graded answers scripts/reader_eval.py (verdicts_<judge>.jsonl:
config, qid, correct) and scripts/bench_eval.py (reader_grades.jsonl: config,
qid, score) write, keeps the questions every compared config answered, and
reports for each config: its score with a 95% bootstrap interval, its score
by question type, and its difference from --base on the same questions, with
a 95% bootstrap interval and a two-sided p-value (exact McNemar test for
yes/no grades, a paired sign-flip permutation test for graded scores).

  python3 scripts/compare_systems.py --data longmemeval_s.jsonl --grades results/v3/competitors/longmemeval_s/verdicts_small.jsonl \\
      --base engram --config engram --config mem0 --config hindsight [--out comparison.json]

--config NAME or NAME=CONFIG (CONFIG as named in the grades file). Standard library only.
"""

import argparse
import gzip
import json
import math
import random
from collections import defaultdict


def opener(path):
    return gzip.open(path, "rt") if path.endswith(".gz") else open(path)


def load_grades(paths):
    g = defaultdict(dict)
    for path in paths:
        with opener(path) as f:
            for line in f:
                r = json.loads(line)
                v = r.get("correct", r.get("score"))
                if v is not None:
                    g[r["config"]][r["qid"]] = float(v)
    return g


def load_types(path):
    t = {}
    with opener(path) as f:
        for line in f:
            u = json.loads(line)
            for q in u["questions"]:
                t[q["qid"]] = "abstention" if q.get("abstention") else q.get("question_type", "all")
    return t


def bootstrap(xs, iters, rng):
    n = len(xs)
    means = sorted(sum(xs[rng.randrange(n)] for _ in range(n)) / n for _ in range(iters))
    return means[int(0.025 * iters)], means[int(0.975 * iters) - 1]


def mcnemar(a, b):
    """Exact two-sided McNemar on yes/no grades: binomial test on the discordant pairs."""
    up = sum(1 for x, y in zip(a, b) if x > y)
    down = sum(1 for x, y in zip(a, b) if x < y)
    n = up + down
    if n == 0:
        return 1.0, up, down
    k = min(up, down)
    p = sum(math.comb(n, i) for i in range(k + 1)) / 2**n * 2
    return min(1.0, p), up, down


def signflip(d, iters, rng):
    obs = abs(sum(d))
    hits = sum(1 for _ in range(iters) if abs(sum(x if rng.random() < 0.5 else -x for x in d)) >= obs - 1e-12)
    return (hits + 1) / (iters + 1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True, help="the benchmark's .jsonl (question types)")
    ap.add_argument("--grades", action="append", required=True)
    ap.add_argument("--config", action="append", required=True, help="NAME or NAME=CONFIG")
    ap.add_argument("--base", required=True, help="the NAME the others are compared with")
    ap.add_argument("--iters", type=int, default=10000)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--out")
    args = ap.parse_args()

    rng = random.Random(args.seed)
    grades = load_grades(args.grades)
    types = load_types(args.data)
    cfgs = [(c.split("=", 1) + [c])[:2] if "=" in c else [c, c] for c in args.config]
    for name, src in cfgs:
        if src not in grades:
            raise SystemExit(f"no grades for {src}; have {sorted(grades)}")
    qids = sorted(set.intersection(*(set(grades[src]) for _, src in cfgs)))
    binary = all(v in (0.0, 1.0) for _, src in cfgs for v in (grades[src][q] for q in qids))
    base = dict(cfgs)[args.base]
    out = {"questions": len(qids), "binary": binary, "base": args.base, "configs": {}}
    print(f"{len(qids)} questions every config answered ({'yes/no' if binary else 'graded'})")
    for name, src in cfgs:
        xs = [grades[src][q] for q in qids]
        lo, hi = bootstrap(xs, args.iters, rng)
        by = defaultdict(list)
        for q, x in zip(qids, xs):
            by[types.get(q, "all")].append(x)
        row = {"config": src, "score": sum(xs) / len(xs), "ci95": [lo, hi],
               "by_type": {k: {"n": len(v), "score": sum(v) / len(v)} for k, v in sorted(by.items())}}
        if name != args.base:
            bs = [grades[base][q] for q in qids]
            d = [x - y for x, y in zip(xs, bs)]
            dlo, dhi = bootstrap(d, args.iters, rng)
            row["vs_base"] = {"diff": sum(d) / len(d), "ci95": [dlo, dhi]}
            if binary:
                p, up, down = mcnemar(xs, bs)
                row["vs_base"].update({"p": p, "better": up, "worse": down})
            else:
                row["vs_base"]["p"] = signflip(d, args.iters, rng)
        out["configs"][name] = row
        vs = row.get("vs_base")
        tail = f"  vs {args.base} {vs['diff'] * 100:+.1f} [{vs['ci95'][0] * 100:+.1f}, {vs['ci95'][1] * 100:+.1f}] p={vs['p']:.3g}" if vs else ""
        print(f"{name:<20} {row['score'] * 100:5.1f} [{lo * 100:.1f}, {hi * 100:.1f}]{tail}")
    if args.out:
        with open(args.out, "w") as f:
            json.dump(out, f, indent=1)


if __name__ == "__main__":
    main()
