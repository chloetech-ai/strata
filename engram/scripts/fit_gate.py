"""Fits the injection gate on benchmark rows written by engram-bench.

A row is positive when the packed injection (4,000 characters) contains
evidence; negatives are foreign questions (asked against another
conversation), abstention questions and misses. Features: max cosine, max
IDF-weighted term coverage, past-reference cue, BM25/vector agreement.
Logistic regression gives the linear score G = a cos + b cov + c cue + d agree;
the thresholds are set for a target precision on the training set.
Usage: python3 scripts/fit_gate.py TRAIN.rows.jsonl [TEST.rows.jsonl ...] [--out gate.json]"""
import json
import sys

import numpy as np

args = [a for a in sys.argv[1:] if not a.startswith("--")]
out = sys.argv[sys.argv.index("--out") + 1] if "--out" in sys.argv else None
if out in args:
    args.remove(out)


def policy(path, w, hi, lo):
    """The gated policy on a rows file: full injection at G >= hi, brief at
    lo <= G < hi. Coverage over answerable questions, precision over all
    injections, and characters injected per prompt."""
    tot = ans = cov = inj = good = 0
    chars = 0.0
    always_cov = 0
    for l in open(path):
        r = json.loads(l)
        e = r["evidence"]
        x = np.array([e["max_cos"], e["max_cov"], float(e["cue"]), float(e["agree"]), e.get("cos_gap", 0.0), e.get("bm_gap", 0.0)])
        g = x @ w
        answerable = not (r["foreign"] or r["abstention"] or r["n_evidence"] == 0)
        m = r["configs"].get("hybrid_0.5_priors", {})
        tot += 1
        ans += answerable
        always_cov += answerable and m.get("pack4k", 0) > 0
        if g >= hi:
            inj += 1
            chars += m.get("pack4k_chars", 3000) if answerable else 3000
            hit = answerable and m.get("pack4k", 0) > 0
        elif g >= lo:
            inj += 1
            chars += 900
            hit = answerable and m.get("pack_brief", 0) > 0
        else:
            hit = False
        cov += hit
        good += hit
    return {"coverage": cov / max(ans, 1), "precision": good / max(inj, 1), "inject_rate": inj / max(tot, 1),
            "chars_per_prompt": chars / max(tot, 1), "always_coverage": always_cov / max(ans, 1), "always_precision": always_cov / max(tot, 1)}


def load(path):
    X, y = [], []
    for l in open(path):
        r = json.loads(l)
        e = r["evidence"]
        x = [e["max_cos"], e["max_cov"], float(e["cue"]), float(e["agree"]), e.get("cos_gap", 0.0), e.get("bm_gap", 0.0)]
        if r["foreign"] or r["abstention"] or r["n_evidence"] == 0:
            label = 0
        else:
            label = int(r["configs"]["hybrid_0.5_priors"]["pack4k"] > 0)
        X.append(x)
        y.append(label)
    return np.array(X), np.array(y)


def fit(X, y, l2=1e-3, iters=4000, lr=0.5):
    w = np.zeros(X.shape[1])
    b = 0.0
    for _ in range(iters):
        z = X @ w + b
        p = 1 / (1 + np.exp(-z))
        g = p - y
        w -= lr * (X.T @ g / len(y) + l2 * w)
        b -= lr * g.mean()
    return w, b


def curve(score, y):
    order = np.argsort(-score)
    s, yy = score[order], y[order]
    tp = np.cumsum(yy)
    k = np.arange(1, len(yy) + 1)
    return s, tp / k, tp / max(yy.sum(), 1)


def at(score, y, thr):
    sel = score >= thr
    prec = y[sel].mean() if sel.any() else 1.0
    rec = (y[sel].sum() / max(y.sum(), 1))
    return prec, rec, sel.mean()


parts = [load(p) for p in args[0].split("+")]
Xtr, ytr = np.vstack([x for x, _ in parts]), np.concatenate([y for _, y in parts])
w, b = fit(Xtr, ytr)
G = Xtr @ w
s, prec, rec = curve(G, ytr)


def thr_for(p):
    ok = np.where(prec >= p)[0]
    return s[ok[-1]] if len(ok) else s[0]


hi, lo = thr_for(0.90), thr_for(0.70)
res = {"train": args[0], "weights": {"a_cos": w[0], "b_cov": w[1], "c_cue": w[2], "d_agree": w[3], "e_cos_gap": w[4], "f_bm_gap": w[5]}, "bias": b, "hi": hi, "lo": lo,
       "train_positive_rate": float(ytr.mean())}
print(f"train {args[0]}: n={len(ytr)} positive {ytr.mean():.3f}")
print(f"  G = {w[0]:.3f} cos + {w[1]:.3f} cov + {w[2]:.3f} cue + {w[3]:.3f} agree + {w[4]:.3f} cos_gap + {w[5]:.3f} bm_gap;  hi {hi:.3f} (precision 0.9), lo {lo:.3f} (precision 0.7)")
for name, (X, y) in [("train", (Xtr, ytr))] + [(p, load(p)) for p in args[1:] if "+" not in p]:
    g = X @ w
    p_hi, r_hi, f_hi = at(g, y, hi)
    p_lo, r_lo, f_lo = at(g, y, lo)
    # single-feature baselines at the same injection rate as hi
    k = int(round(f_hi * len(y)))
    base = {}
    for j, fn in enumerate(["cos", "cov"]):
        o = np.argsort(-X[:, j])[:k]
        base[fn] = float(y[o].mean()) if k else 1.0
    print(f"  {name}: n={len(y)} pos {y.mean():.3f} | inject (G>=hi): precision {p_hi:.3f} recall {r_hi:.3f} rate {f_hi:.3f} | pointer-or-inject (G>=lo): precision {p_lo:.3f} recall {r_lo:.3f} | same-rate precision cos-only {base['cos']:.3f} cov-only {base['cov']:.3f}")
    res[name] = {"n": int(len(y)), "positive": float(y.mean()), "hi": [p_hi, r_hi, f_hi], "lo": [p_lo, r_lo, f_lo], "single_feature_precision": base}
print("gated policy (full at hi, brief at lo):")
for path in [q for a in args for q in a.split("+")]:
    p = policy(path, w, hi, lo)
    res.setdefault("policy", {})[path] = p
    print(f"  {path}: coverage {p['coverage']:.3f} (always inject {p['always_coverage']:.3f}), precision {p['precision']:.3f} (always {p['always_precision']:.3f}), "
          f"inject rate {p['inject_rate']:.3f}, {p['chars_per_prompt']:.0f} chars per prompt (always about 3,000)")
if out:
    json.dump(res, open(out, "w"), indent=1, default=float)
