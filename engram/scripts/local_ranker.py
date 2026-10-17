#!/usr/bin/env python3
"""Fits the query engine's local ranker: a logistic regression over the
features `engram-bench deep --features` writes for each candidate, with the
label "this candidate holds evidence for the question".

  python3 scripts/local_ranker.py FEATURES.jsonl [MORE.jsonl ...] [--test TEST.jsonl ...]

Questions are split into five folds; each fold is scored by weights fitted on
the other four, so every number below is out of sample. --test files are
scored only with the weights fitted on all the training files (a dataset the
weights never saw). Prints what the picks hold against the same pool taken
in its fused order, and the weights as Rust constants for src/deep.rs.
"""

import json
import sys

import numpy as np

FEATURES = ["fused", "rank", "cos", "cover_idf", "cover", "note", "prompt", "answer", "tool", "len", "both", "newer"]


def load(paths):
    qs = []
    for p in paths:
        for line in open(p):
            q = json.loads(line)
            if q["abstention"] or q["n_evidence"] == 0 or not q["cands"]:
                continue
            X = np.array([c["f"] for c in q["cands"]], dtype=float)
            y = np.array([c["y"] for c in q["cands"]], dtype=float)
            qs.append({"qid": q["qid"], "qtype": q["qtype"], "n_ev": q["n_evidence"], "X": X, "y": y, "src": p})
    return qs


def fit(qs, l2=1.0, iters=50):
    """Logistic regression by Newton's method, L2 on the weights (not the intercept)."""
    X = np.vstack([q["X"] for q in qs])
    y = np.concatenate([q["y"] for q in qs])
    X = np.hstack([X, np.ones((len(X), 1))])
    w = np.zeros(X.shape[1])
    reg = np.full(X.shape[1], l2)
    reg[-1] = 0.0
    for _ in range(iters):
        p = 1 / (1 + np.exp(-X @ w))
        g = X.T @ (p - y) + reg * w
        H = (X * (p * (1 - p))[:, None]).T @ X + np.diag(reg) + 1e-6 * np.eye(len(w))
        step = np.linalg.solve(H, g)
        w -= step
        if np.abs(step).max() < 1e-8:
            break
    return w


def probs(q, w):
    return 1 / (1 + np.exp(-(q["X"] @ w[:-1] + w[-1])))


def held(q, idx):
    """(any, all) evidence among the candidates idx. A question's evidence can
    be missing from the pool; `all` counts against all of its evidence."""
    got = int(q["y"][idx].sum()) if len(idx) else 0
    return float(got > 0), float(got >= q["n_ev"])


def evaluate(qs, scorer, k=None, thr=None, cap=8):
    any_, all_, n = 0.0, 0.0, 0
    for q in qs:
        s = scorer(q)
        order = np.argsort(-s, kind="stable")
        if thr is not None:
            order = [i for i in order if s[i] >= thr][:cap]
        else:
            order = list(order[:k])
        a, b = held(q, np.array(order, dtype=int))
        any_ += a
        all_ += b
        n += len(order)
    m = max(len(qs), 1)
    return any_ / m, all_ / m, n / m


def pool_bound(qs):
    return sum(held(q, np.arange(len(q["y"])))[1] for q in qs) / max(len(qs), 1)


def report(name, qs, scorer_ltr):
    fused = lambda q: q["X"][:, 0]  # noqa: E731
    print(f"\n{name}: {len(qs)} questions with evidence; pool holds all evidence for {pool_bound(qs):.3f}")
    print(f"  {'':34} any    all    picks")
    for k in (1, 3, 5, 10):
        a, b, n = evaluate(qs, fused, k=k)
        print(f"  fused order, top {k:<2}               {a:.3f}  {b:.3f}  {n:.1f}")
    for k in (1, 3, 5):
        a, b, n = evaluate(qs, scorer_ltr, k=k)
        print(f"  local ranker, top {k:<2}              {a:.3f}  {b:.3f}  {n:.1f}")
    for t in (0.3, 0.4, 0.5, 0.6):
        a, b, n = evaluate(qs, scorer_ltr, thr=t)
        print(f"  local ranker, p >= {t:<3} (max 8)      {a:.3f}  {b:.3f}  {n:.1f}")


def main():
    args = sys.argv[1:]
    test_paths = []
    if "--test" in args:
        i = args.index("--test")
        args, test_paths = args[:i], args[i + 1:]
    train = load(args)
    rng = np.random.default_rng(1)
    folds = rng.integers(0, 5, len(train))
    cv_w = [fit([q for q, f in zip(train, folds) if f != k]) for k in range(5)]
    by_q = {id(q): cv_w[f] for q, f in zip(train, folds)}
    report("cross-validated (5 folds)", train, lambda q: probs(q, by_q[id(q)]))
    w = fit(train)
    if test_paths:
        test = load(test_paths)
        report("held-out dataset", test, lambda q: probs(q, w))
    print("\nweights (fitted on every training question):")
    for name, v in zip(FEATURES + ["intercept"], w):
        print(f"  {name:10} {v:+.3f}")
    print("\npub const LOCAL_WEIGHTS: [f64; 13] = [" + ", ".join(f"{v:.3f}" for v in w) + "];")


if __name__ == "__main__":
    main()
