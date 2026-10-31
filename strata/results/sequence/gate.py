"""The recommender gate: next-item recommendation on MovieLens-1M by every
candidate source Strata has and two baselines, in one harness, checked
against the numbers committed in strata/tests/fixtures/recsys_gate.json.

The protocol is eval.py's, the one published SASRec numbers use:
leave-one-out (each user's last rating is the test; everything before it
is what the sources learn from and what the user has seen), ranked against
every item but the user's own, over all 6,040 users. Each source answers
each user's top 100 and the gate scores Recall@10, @50 and @100 (HR@k, one
test item), NDCG@10, and coverage: the share of the 3,706 items in some
user's top 10.

The sources:

- `popularity`: items by how many users rated them (a baseline, in Python).
- `item_knn`: items by the sum of their cosine similarities, over who rated
  what, to the user's items, each item's 100 nearest kept (a baseline, in
  Python: what any recommender has to beat).
- `encode`: `similarity(emb, encode('ml1m', history, now => :t))`, the
  sequence model of strata/tests/fixtures/recsys/ml1m, trained by
  `strata.train.sequence` as eval.py trains it (with times; `--train`),
  its item vectors (the model's items.safetensors) in `items.emb`.
- `maxsim`: `maxsim(emb, :recent)`, the user's last 20 items' vectors as
  the query vectors.
- `next_items`: counted transitions (`strata.next_items`, the Next items
  recipe): what came after the user's last two items, backing off to the
  last one, from a counts table of every training event, the rest of the
  top 100 filled by popularity.
- `ppr`: personalized PageRank over an edge table of what came after what
  (1/g from each of the 5 items before an event to it, the query
  language's recipe), seeded by the user's last 5 items, `dir => 'out'`,
  `eps => 1e-6` (Next items' recipe: at the default 1e-4 the walk hardly
  leaves the seeds of a graph this dense, and Recall@10 is 0.005).

All but the baselines run as Strata queries on a database the gate builds.

    python gate.py --ratings ~/data/ml-1m/ratings.dat              # check
    STRATA_RECSYS_GATE=update python gate.py --ratings ...           # rewrite
    python gate.py --ratings ... --train 30                          # retrain the model (PyTorch), 30 min

CI reads ratings.csv from a mirror of MovieLens-1M on Hugging Face, pinned
by revision and SHA-256 (.github/workflows/ci.yml).

A metric that moves by more than its tolerance fails the gate (rounding
and SIMD differences move none by 0.001 at this size), and so does a
Strata source that no longer beats both baselines' Recall@10 where the
fixture says it did. A change that moves them commits the new numbers,
which the gate writes with STRATA_RECSYS_GATE=update, and says why in its
pull request.
"""
import argparse
import csv
import datetime
import json
import math
import os
import shutil
import sys
import tempfile
import time

import numpy as np

import strata

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.normpath(os.path.join(HERE, "..", ".."))
FIXTURE = os.path.join(ROOT, "tests", "fixtures", "recsys_gate.json")
MODELS = os.path.join(ROOT, "tests", "fixtures", "recsys")
MODEL = "ml1m"
TOLERANCE = {"recall@10": 0.002, "recall@50": 0.002, "recall@100": 0.002, "ndcg@10": 0.002, "coverage@10": 0.005}
K = 100


def ratings(path):
    """Each user's ratings as (time, file order, item), from ratings.dat
    (`user::movie::rating::time`) or a CSV of the same columns."""
    seqs = {}
    if path.endswith(".dat"):
        rows = (line.strip().split("::") for line in open(path, encoding="latin-1"))
    else:
        f = open(path, newline="")
        rows = csv.reader(f)
        next(rows)
    for order, (u, m, _r, t) in enumerate(rows):
        seqs.setdefault(f"u{u}", []).append((int(t.strip()), order, f"m{m}"))
    return {u: sorted(evs) for u, evs in seqs.items()}


def scores(top, target, cut=(10, 50, 100)):
    out = {f"recall@{k}": float(target in top[:k]) for k in cut}
    out["ndcg@10"] = 1 / math.log2(top.index(target) + 2) if target in top[:10] else 0.0
    return out


class Tally:
    def __init__(self, n_items):
        self.sums, self.n, self.seen, self.n_items, self.ms = {}, 0, set(), n_items, []

    def add(self, top, target, ms=None):
        for k, v in scores(top, target).items():
            self.sums[k] = self.sums.get(k, 0.0) + v
        self.n += 1
        self.seen.update(top[:10])
        if ms is not None:
            self.ms.append(ms)

    def result(self):
        out = {k: round(v / self.n, 4) for k, v in sorted(self.sums.items())}
        out["coverage@10"] = round(len(self.seen) / self.n_items, 4)
        if self.ms:
            ms = sorted(self.ms)
            out["query_ms_p50"] = round(ms[len(ms) // 2], 2)
        return out


def baselines(train, items, users):
    """Popularity and item-kNN over the training histories."""
    index = {k: i for i, k in enumerate(items)}
    R = np.zeros((len(users), len(items)), dtype=np.float32)
    for ui, u in enumerate(users):
        for _, _, it in train[u]:
            R[ui, index[it]] = 1.0
    pop = R.sum(axis=0)
    norms = np.sqrt(pop) + 1e-9
    S = (R.T @ R) / norms[:, None] / norms[None, :]
    np.fill_diagonal(S, 0.0)
    # Each item's 100 nearest.
    keep = np.argpartition(-S, 100, axis=1)[:, :100]
    S2 = np.zeros_like(S)
    rows = np.arange(len(items))[:, None]
    S2[rows, keep] = S[rows, keep]
    return R, pop, S2


def top_of(score, seen, k=K):
    score = score.copy()
    score[seen] = -np.inf
    idx = np.argpartition(-score, k)[:k]
    return idx[np.argsort(-score[idx], kind="stable")]


def build(work, train, items, vectors):
    """A database of the items (their vectors), every training event, the
    edge table of what came after what, and the counts of what came right
    after the last one and two items."""
    from strata import ngrams

    path = os.path.join(work, "db")
    shutil.rmtree(path, ignore_errors=True)
    dim = len(next(iter(vectors.values())))
    db = strata.Database.create(path, {"table": "items", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "emb", "type": "vector", "dim": dim, "index": "flat"}]}, models=MODELS)
    db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, seq INT NOT NULL, item KEYWORD NOT NULL, ts TIMESTAMP NOT NULL) OPTIONS(cluster_by = 'usr')")
    db.sql(ngrams.create_table("next"))
    db.sql("CREATE TABLE after (id KEYWORD PRIMARY KEY, src KEYWORD NOT NULL REFERENCES items, dst KEYWORD NOT NULL REFERENCES items, w FLOAT DEFAULT 0) OPTIONS(edges = 'src, dst, w')")
    db.insert([{"id": k, "emb": vectors[k]} for k in items], table="items")
    iso = lambda t: datetime.datetime.fromtimestamp(t, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")  # noqa: E731
    rows, eid, edges, adds = [], 0, {}, []
    for u, evs in train.items():
        for i, (t, _, it) in enumerate(evs):
            rows.append({"id": eid, "usr": u, "seq": i, "item": it, "ts": iso(t)})
            eid += 1
            if i:
                adds += ngrams.ops([e[2] for e in evs[max(0, i - 2) : i]], it, table="next")
            if len(adds) >= 20_000:
                db.write(adds)
                adds = []
            for g in range(1, 6):
                if i - g >= 0:
                    key = (evs[i - g][2], it)
                    edges[key] = edges.get(key, 0.0) + 1.0 / g
            if len(rows) == 50_000:
                db.insert(rows, table="events")
                rows = []
    db.insert(rows, table="events")
    db.write(adds)
    out = [{"id": f"{s}>{d}", "src": s, "dst": d, "w": w} for (s, d), w in edges.items()]
    for i in range(0, len(out), 50_000):
        db.insert(out[i : i + 50_000], table="after")
    db.optimize()
    return db, eid, len(edges)


def load_vectors():
    """The item vectors the model was trained with (its items.safetensors)."""
    from strata import _sequence as S

    vectors = S.read_items(os.path.join(MODELS, MODEL))
    if not vectors:
        raise SystemExit(f"no item vectors beside the model in {MODELS}/{MODEL}: train it with --train")
    return vectors


def train_model(train, items, minutes, seed=0):
    """Trains the gate's model as eval.py does (with times, item vectors
    learned from a random start, every item a negative, the last training
    event of each user held out to stop on), for at most `minutes`."""
    import torch

    import strata.train

    torch.set_num_threads(os.cpu_count())
    g = torch.Generator().manual_seed(seed)
    start = torch.nn.functional.normalize(torch.randn(len(items), 64, generator=g), dim=-1)
    vectors = {k: start[i].tolist() for i, k in enumerate(items)}
    seqs = {u: [(it, None, float(t)) for t, _, it in evs] for u, evs in train.items()}
    r = strata.train.sequence(
        None, MODEL, vectors=vectors, sequences=seqs, models=MODELS, width=64, layers=2, heads=1, max_len=200,
        learn_items=True, negatives="all", temperature=0.05, epochs=200, batch=128, lr=1e-3, dropout=0.2,
        holdout=True, patience=10, max_seconds=minutes * 60, seed=seed, log=lambda row: print(json.dumps(row), flush=True),
    )
    best = max(r["history"], key=lambda h: h["ndcg@10"])
    print(f"trained {len(r['history'])} epochs, best {best}", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ratings", required=True, help="ratings.dat of MovieLens-1M, or a CSV of the same columns")
    ap.add_argument("--work", default=None)
    ap.add_argument("--fixture", default=FIXTURE)
    ap.add_argument("--users", type=int, default=None, help="score this many users only (a quick run; not a check)")
    ap.add_argument("--train", type=float, default=None, metavar="MINUTES", help="first retrain the model, for at most this long")
    a = ap.parse_args()
    update = os.environ.get("STRATA_RECSYS_GATE") == "update"

    seqs = ratings(os.path.expanduser(a.ratings))
    users = sorted(seqs)
    items = sorted({e[2] for s in seqs.values() for e in s})
    n_events = sum(len(s) for s in seqs.values())
    print(f"{len(users)} users, {len(items)} items, {n_events} ratings", flush=True)
    train = {u: s[:-1] for u, s in seqs.items()}
    test = {u: s[-1] for u, s in seqs.items()}
    if a.users:
        users = users[: a.users]
    if a.train is not None:
        train_model(train, items, a.train)

    t0 = time.time()
    R, pop, S = baselines(train, items, sorted(seqs))
    index = {k: i for i, k in enumerate(items)}
    row_of = {u: i for i, u in enumerate(sorted(seqs))}
    tally = {s: Tally(len(items)) for s in ["popularity", "item_knn", "encode", "maxsim", "next_items", "ppr"]}
    popular = {}
    for u in users:
        seen = R[row_of[u]] > 0
        target = test[u][2]
        popular[u] = [items[i] for i in top_of(pop, seen)]
        tally["popularity"].add(popular[u], target)
        tally["item_knn"].add([items[i] for i in top_of(R[row_of[u]] @ S, seen)], target)
    print(f"baselines in {time.time() - t0:.0f} s", flush=True)

    vectors = load_vectors()
    work = a.work or tempfile.mkdtemp(prefix="recsys-gate-")
    t0 = time.time()
    db, n_ev, n_edges = build(work, train, items, vectors)
    print(f"loaded {len(items)} items, {n_ev} events, {n_edges} edges in {time.time() - t0:.0f} s", flush=True)

    not_seen = "NOT id IN (SELECT item FROM events WHERE usr = :u)"
    last5 = "(SELECT item FROM events WHERE usr = :u ORDER BY seq DESC LIMIT 5)"
    sql = {
        "encode": f"SELECT id FROM items WHERE {not_seen} ORDER BY similarity(emb, encode('{MODEL}', (SELECT item, ts FROM events WHERE usr = :u ORDER BY seq DESC LIMIT 200), now => :t)) DESC LIMIT {K}",
        "maxsim": f"SELECT id FROM items WHERE {not_seen} ORDER BY maxsim(emb, :recent) DESC LIMIT {K}",
        "ppr": f"SELECT id FROM items WHERE {not_seen} ORDER BY ppr(after, {last5}, dir => 'out', eps => 1e-6) DESC LIMIT {K}",
    }
    for u in users[:20]:
        db.query(sql["encode"], {"u": u, "t": test[u][0] * 1_000_000})
    for u in users:
        target = test[u][2]
        last = [e[2] for e in train[u][-20:]][::-1]
        params = {
            "encode": {"u": u, "t": test[u][0] * 1_000_000},
            "maxsim": {"u": u, "recent": [vectors[k] for k in last]},
            "ppr": {"u": u},
        }
        for s, q in sql.items():
            t = time.perf_counter()
            top = [r[0] for r in db.query(q, params[s]).rows]
            tally[s].add(top, target, (time.perf_counter() - t) * 1e3)
        # Counts know nothing of what the user has seen: ask for as many
        # more as they have seen, and drop those.
        seen = {e[2] for e in train[u]}
        t = time.perf_counter()
        nxt = [x for x, _ in strata.next_items(db, [e[2] for e in train[u][-2:]], K + len(seen), table="next") if x not in seen][:K]
        ms = (time.perf_counter() - t) * 1e3
        tally["next_items"].add(nxt + [x for x in popular[u] if x not in nxt][: K - len(nxt)], target, ms)
    db.close()
    if not a.work:
        shutil.rmtree(work, ignore_errors=True)

    got = {s: t.result() for s, t in tally.items()}
    for s, r in got.items():
        print(f"{s:>11} " + "  ".join(f"{k} {v}" for k, v in r.items()), flush=True)
    if a.users:
        print("a run over some users only: not checked")
        return 0
    baseline = max(got["popularity"]["recall@10"], got["item_knn"]["recall@10"])
    beats = sorted(s for s in ("encode", "maxsim", "next_items", "ppr") if got[s]["recall@10"] > baseline)
    if update:
        out = {
            "data": {"name": "MovieLens-1M", "users": len(users), "items": len(items), "ratings": n_events},
            "protocol": "leave-one-out, full ranking without the user's own items, all users",
            "model": MODEL,
            "tolerance": TOLERANCE,
            "beat_both_baselines": beats,
            "sources": {s: {k: v for k, v in r.items() if not k.startswith("query_ms")} for s, r in got.items()},
        }
        with open(a.fixture, "w") as f:
            json.dump(out, f, indent=1)
            f.write("\n")
        print(f"wrote {a.fixture}")
        return 0
    want = json.load(open(a.fixture))
    failed = []
    for s, metrics in want["sources"].items():
        for k, v in metrics.items():
            d = got[s][k] - v
            if abs(d) > want["tolerance"][k]:
                failed.append(f"{s} {k}: {got[s][k]} against {v} ({d:+.4f})")
    for s in want["beat_both_baselines"]:
        if s not in beats:
            failed.append(f"{s} no longer beats both baselines' Recall@10 ({got[s]['recall@10']} against {baseline})")
    if failed:
        print("the recommender gate fails:\n  " + "\n  ".join(failed) + "\nIf the change is meant to move them, rerun with STRATA_RECSYS_GATE=update, commit the fixture, and say why in the pull request.")
        return 1
    print("the recommender gate holds")
    return 0


if __name__ == "__main__":
    sys.exit(main())
