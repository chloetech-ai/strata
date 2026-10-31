"""Next items from counted transitions (`strata.next_items`) on Amazon
Beauty, leave-one-out with full ranking, as P5 and TIGER report SASRec.

    curl -O http://snap.stanford.edu/data/amazon/productGraph/categoryFiles/reviews_Beauty_5.json.gz
    python ngrams_beauty.py reviews_Beauty_5.json.gz

Each user's reviews in time order are a sequence (22,363 users, 12,101
items, 198,502 reviews; reviews of one day stay in the file's order, by
product, as the usual preprocessing leaves them). The transitions before
each user's last item, or only those before the last two (as SASRec
trains), go into an n-gram table (`strata.ngrams`, orders 1 to 3,
in-process); each user's last item is predicted from the items before it.
Fewer than 10 scored items are filled with the most frequent next items.
The first `--check` users' answers are compared with scoring every item.
Writes ngrams-beauty.json beside this file.
"""

import argparse
import gzip
import json
import math
import os
import platform
import tempfile
import time
from collections import Counter

import strata
from strata import ngrams


def sequences(path):
    """Each user's items in time order (one day's in file order), and how
    many users' last two items share a day."""
    by_user = {}
    with gzip.open(path, "rt") as f:
        for i, line in enumerate(f):
            r = json.loads(line)
            by_user.setdefault(r["reviewerID"], []).append((r["unixReviewTime"], i, r["asin"]))
    seqs = [sorted(v) for v in by_user.values()]
    return [[a for _, _, a in v] for v in seqs], sum(v[-1][0] == v[-2][0] for v in seqs)


def reference(counts, history, k, order, backoff):
    """Stupid backoff scored for every item."""
    scores = {}
    for i, m in enumerate(range(min(order, len(history)), 0, -1)):
        c = counts.get(tuple(history[-m:]))
        if not c:
            continue
        total = sum(c.values())
        for w, n in c.items():
            scores.setdefault(w, backoff**i * n / total)
    return [w for w, _ in sorted(scores.items(), key=lambda kv: (-kv[1], kv[0]))[:k]]


def cpu():
    try:
        with open("/proc/cpuinfo") as f:
            return next(l.split(":", 1)[1].strip() for l in f if l.startswith("model name"))
    except (OSError, StopIteration):
        return platform.processor() or platform.machine()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("reviews")
    ap.add_argument("--check", type=int, default=2000, help="users whose answers are compared with scoring every item")
    ap.add_argument("--out", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "ngrams-beauty.json"))
    a = ap.parse_args()
    seqs, ties = sequences(a.reviews)
    items = {x for s in seqs for x in s}
    print(f"{len(seqs)} users, {len(items)} items, {sum(map(len, seqs))} reviews; last two on one day: {ties / len(seqs):.3f}")
    runs = []
    loads = []
    # Counts from every transition before each user's last item, or only
    # from those before the last two, as SASRec trains (the second last is
    # its validation item).
    for held in (1, 2):
        counts = {}
        popular = Counter()
        ops = []
        for s in seqs:
            train = s[:-held]
            for i in range(1, len(train)):
                ops += ngrams.ops(train[:i], train[i], order=3)
                popular[train[i]] += 1
                for m in range(1, min(3, i) + 1):
                    counts.setdefault(tuple(train[i - m : i]), Counter())[train[i]] += 1
        fill = [w for w, _ in sorted(popular.items(), key=lambda kv: (-kv[1], kv[0]))]
        with tempfile.TemporaryDirectory() as d:
            db = strata.open(os.path.join(d, "next.strata"), ngrams.schema())
            t = time.perf_counter()
            for j in range(0, len(ops), 10_000):
                db.write(ops[j : j + 10_000])
            db.optimize()
            load_s = time.perf_counter() - t
            rows = len(db)
            loads.append({"counts_before_last": held, "adds": len(ops), "rows": rows, "load_s": load_s})
            print(f"counts before the last {held}: {len(ops)} adds in {load_s:.1f} s, {rows} rows")
            for order in (1, 2, 3):
                backoff = 0.4
                hits = ndcg = 0.0
                ms = []
                for u, s in enumerate(seqs):
                    history, want = s[:-1], s[-1]
                    t = time.perf_counter()
                    got = [x for x, _ in strata.next_items(db, history, 10, order=order, backoff=backoff)]
                    ms.append((time.perf_counter() - t) * 1e3)
                    if u < a.check:
                        assert got == reference(counts, history, 10, order, backoff), (u, history)
                    top = got + [x for x in fill if x not in got][: 10 - len(got)]
                    if want in top:
                        hits += 1
                        ndcg += 1 / math.log2(top.index(want) + 2)
                ms.sort()
                run = {
                    "counts_before_last": held,
                    "order": order,
                    "backoff": backoff,
                    "recall_at_10": hits / len(seqs),
                    "ndcg_at_10": ndcg / len(seqs),
                    "ms": {"p50": ms[len(ms) // 2], "p99": ms[int(len(ms) * 0.99)], "mean": sum(ms) / len(ms)},
                }
                runs.append(run)
                print(f"  order {order}: Recall@10 {run['recall_at_10']:.4f}, NDCG@10 {run['ndcg_at_10']:.4f}, p50 {run['ms']['p50']:.2f} ms, p99 {run['ms']['p99']:.2f} ms")
            db.close()
    report = {
        "data": {"name": "amazon-beauty-5core", "users": len(seqs), "items": len(items), "reviews": sum(map(len, seqs)), "last_two_same_day": ties / len(seqs)},
        "protocol": "leave-one-out (each user's last item, from the items before it), full ranking; fewer than 10 scored items filled by frequency; reviews of one day in file order (by product), as the usual preprocessing leaves them",
        "checked_users": min(a.check, len(seqs)),
        "params": {"k": 10, "threads_query": 1},
        "loads": loads,
        "published": {"sasrec": {"recall_at_10": 0.0605, "ndcg_at_10": 0.0318, "source": "TIGER (arXiv 2305.05065) Table 1, from P5"}},
        "machine": {"cpu": cpu(), "cores": os.cpu_count()},
        "runs": runs,
    }
    with open(a.out, "w") as f:
        json.dump(report, f, indent=2)
    print(f"wrote {a.out}")


if __name__ == "__main__":
    main()
