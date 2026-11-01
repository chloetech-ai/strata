"""Next-item recommendation with `encode(model, rows)`: MovieLens-1M and
Amazon Beauty (5-core), leave-one-out, full ranking, as published SASRec
numbers are measured.

For each user the last event is the test, the one before it validates
(training stops when NDCG@10 on those stops rising), the rest train. The
model is trained with `strata.train.sequence`; then a Strata database holds
the items (their vectors) and every event, and each test user's top 10 come
from one query:

    SELECT id FROM items
    WHERE NOT id IN (SELECT item FROM events WHERE usr = :u AND seq < :n)
    ORDER BY similarity(emb, encode('m', (SELECT item, ts FROM events
        WHERE usr = :u AND seq < :n ORDER BY seq DESC LIMIT 200), now => :t)) DESC
    LIMIT 10

The items of the history are left out, as full-ranking evaluations do.
Recall@10 and NDCG@10 are computed from those answers, and again from
PyTorch's own vectors of the same model, which they should equal.

With `--no-time` the model reads no times, as SASRec reads none: the
history is its items only, without `ts` and `now =>`. Without it, the time
from each event to the next (and from the newest to the target's time, the
request's `now`) is a feature SASRec does not have.

    python eval.py --data ml-1m --dir ~/data/seq/ml-1m --out . --max-minutes 95
    python eval.py --data beauty --dir ~/data/seq/beauty --out . --max-len 50
    python eval.py --data beauty --dir ~/data/seq/beauty --out . --max-len 50 --no-time --tag=-no-time
"""
import argparse
import datetime
import gzip
import json
import math
import os
import platform
import random
import shutil
import statistics
import subprocess
import tempfile
import time

import strata
import strata.train
import torch
from strata import _sequence as S


def ml1m(d):
    seqs, order = {}, 0
    for line in open(os.path.join(d, "ratings.dat"), encoding="latin-1"):
        u, m, r, t = line.strip().split("::")
        seqs.setdefault(f"u{u}", []).append((int(t), order, f"m{m}", r))
        order += 1
    return seqs


def beauty(d):
    seqs, order = {}, 0
    for line in gzip.open(os.path.join(d, "reviews_Beauty_5.json.gz"), "rt"):
        j = json.loads(line)
        seqs.setdefault(j["reviewerID"], []).append((int(j["unixReviewTime"]), order, j["asin"], str(int(j["overall"]))))
        order += 1
    return seqs


def machine():
    if platform.system() == "Darwin":
        q = lambda k: subprocess.run(["sysctl", "-n", k], capture_output=True, text=True).stdout.strip()  # noqa: E731
        cpu, mem = f"{q('machdep.cpu.brand_string')} ({q('hw.model')})", int(q("hw.memsize") or 0) / 1e9
    else:
        cpu = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo") if l.startswith("model name")), platform.processor())
        mem = next((int(l.split()[1]) / 1e6 for l in open("/proc/meminfo") if l.startswith("MemTotal")), None)
    return {"cpu": cpu, "cores": os.cpu_count(), "memory_gb": round(mem, 1) if mem else None, "os": f"{platform.system()} {platform.release()}", "torch": torch.__version__}


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", choices=["ml-1m", "beauty"], required=True)
    ap.add_argument("--dir", required=True)
    ap.add_argument("--out", default=".")
    ap.add_argument("--work", default=None, help="where the database and model go (a temporary directory by default)")
    ap.add_argument("--max-len", type=int, default=200)
    ap.add_argument("--width", type=int, default=64)
    ap.add_argument("--layers", type=int, default=2)
    ap.add_argument("--heads", type=int, default=1)
    ap.add_argument("--dim", type=int, default=64, help="the item vectors' dimension")
    ap.add_argument("--negatives", default="all")
    ap.add_argument("--temperature", type=float, default=0.05)
    ap.add_argument("--epochs", type=int, default=100)
    ap.add_argument("--patience", type=int, default=10)
    ap.add_argument("--max-minutes", type=float, default=None, help="stop training after the first epoch that ends past this many minutes")
    ap.add_argument("--batch", type=int, default=128)
    ap.add_argument("--lr", type=float, default=1e-3)
    ap.add_argument("--dropout", type=float, default=0.2)
    ap.add_argument("--actions", choices=["none", "rating"], default="none")
    ap.add_argument("--no-time", action="store_true", help="give the model no times (as SASRec has none): the history is items (and actions) only")
    ap.add_argument("--users", type=int, default=None, help="evaluate this many users (all by default)")
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--tag", default="")
    a = ap.parse_args()
    torch.set_num_threads(os.cpu_count())
    raw = (ml1m if a.data == "ml-1m" else beauty)(os.path.expanduser(a.dir))
    seqs = {u: [(item, r if a.actions == "rating" else None, None if a.no_time else float(t)) for t, _, item, r in sorted(evs)] for u, evs in raw.items()}
    times = {u: [float(t) for t, _, _, _ in sorted(evs)] for u, evs in raw.items()}
    users = sorted(seqs)
    items = sorted({e[0] for s in seqs.values() for e in s})
    n_events = sum(len(s) for s in seqs.values())
    print(f"{a.data}: {len(users)} users, {len(items)} items, {n_events} events", flush=True)

    # Learned item vectors, from a random start (SASRec's item embeddings,
    # kept in the table's VECTOR field).
    g = torch.Generator().manual_seed(a.seed)
    start = torch.nn.functional.normalize(torch.randn(len(items), a.dim, generator=g), dim=-1)
    vectors = {k: start[i].tolist() for i, k in enumerate(items)}
    work = a.work or tempfile.mkdtemp(prefix="seq-eval-")
    models = os.path.join(work, "models")
    name = f"{a.data}{a.tag}".replace("-", "")
    t0 = time.time()
    r = strata.train.sequence(
        None, name, vectors=vectors, sequences={u: s[:-1] for u, s in seqs.items()}, models=models, width=a.width, layers=a.layers, heads=a.heads,
        max_len=a.max_len, learn_items=True, negatives=a.negatives, temperature=a.temperature, epochs=a.epochs, batch=a.batch, lr=a.lr,
        dropout=a.dropout, holdout=True, patience=a.patience, max_seconds=None if a.max_minutes is None else a.max_minutes * 60, seed=a.seed, log=lambda row: print(json.dumps(row), flush=True),
    )
    train_s = time.time() - t0
    learned = r["vectors"]
    best = max(r["history"], key=lambda h: h["ndcg@10"])
    print(f"trained in {train_s:.0f} s, best epoch {best['epoch']}: valid {best}", flush=True)

    # The database: items with the learned vectors, every event.
    path = os.path.join(work, "db")
    shutil.rmtree(path, ignore_errors=True)
    db = strata.Database.create(path, {"table": "items", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "emb", "type": "vector", "dim": a.dim, "index": "flat"}]}, models=models)
    db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, seq INT NOT NULL, item KEYWORD NOT NULL, action KEYWORD, ts TIMESTAMP NOT NULL) OPTIONS(cluster_by = 'usr')")
    db.insert([{"id": k, "emb": v} for k, v in learned.items()], table="items")
    iso = lambda t: datetime.datetime.fromtimestamp(int(t), datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")  # noqa: E731
    rows, eid = [], 0
    for u in users:
        for i, ((item, act, _), t) in enumerate(zip(seqs[u], times[u])):
            rows.append({"id": eid, "usr": u, "seq": i, "item": item, "action": act, "ts": iso(t)})
            eid += 1
            if len(rows) == 50_000:
                db.insert(rows, table="events")
                rows = []
    db.insert(rows, table="events")
    db.optimize()
    print(f"loaded {len(learned)} items, {eid} events", flush=True)

    # PyTorch's reference over the same test cases.
    model, cfg = S.load(os.path.join(models, name))
    index = {k: i for i, k in enumerate(learned)}
    E = torch.tensor([learned[k] for k in learned])
    acts = {x: i for i, x in enumerate(cfg["actions"])}
    rng = random.Random(a.seed)
    test_users = users if a.users is None else rng.sample(users, a.users)
    cases = [(seqs[u][:-1], seqs[u][-1]) for u in test_users]
    ref = S.evaluate(model, cases, index, acts, cfg, E)

    hist_cols = ("item, action" if a.actions == "rating" else "item") + ("" if a.no_time else ", ts")
    history = f"(SELECT {hist_cols} FROM events WHERE usr = :u AND seq < :n ORDER BY seq DESC LIMIT {a.max_len})"
    now = "" if a.no_time else ", now => :t"
    sql = f"SELECT id FROM items WHERE NOT id IN (SELECT item FROM events WHERE usr = :u AND seq < :n) ORDER BY similarity(emb, encode('{name}', {history}{now})) DESC LIMIT 10"
    plain = "SELECT id FROM items WHERE NOT id IN (SELECT item FROM events WHERE usr = :u AND seq < :n) ORDER BY similarity(emb, :v) DESC LIMIT 10"
    vecs = S.encode_batch(model, [c[0] for c in cases[:300]], index, acts, cfg, E, [c[1][2] for c in cases[:300]])
    # Warm up, then time each user's query, and the same query given the
    # vector, alternating.
    for u, (h, target) in zip(test_users[:50], cases[:50]):
        db.query(sql, {"u": u, "n": len(h), "t": int(times[u][-1]) * 1_000_000})
    hits, ndcg, took, took_plain, same = 0, 0.0, [], [], 0
    for j, (u, (h, target)) in enumerate(zip(test_users, cases)):
        p = {"u": u, "n": len(h), "t": int(times[u][-1]) * 1_000_000}
        t = time.perf_counter()
        res = db.query(sql, p)
        took.append((time.perf_counter() - t) * 1e3)
        top = [row[0] for row in res.rows]
        if j < 300:
            t = time.perf_counter()
            res2 = db.query(plain, {"u": u, "n": len(h), "v": vecs[j].tolist()})
            took_plain.append((time.perf_counter() - t) * 1e3)
            same += top == [row[0] for row in res2.rows]
        if target[0] in top:
            hits += 1
            ndcg += 1 / math.log2(top.index(target[0]) + 2)
    n = len(cases)
    out = {
        "data": {"name": a.data, "users": len(users), "items": len(items), "events": n_events, "evaluated_users": n},
        "params": {k: getattr(a, k) for k in ["max_len", "width", "layers", "heads", "dim", "negatives", "temperature", "epochs", "patience", "max_minutes", "batch", "lr", "dropout", "actions", "no_time", "seed"]},
        "machine": machine(),
        "train": {"seconds": round(train_s, 1), "epochs": len(r["history"]), "best_epoch": best["epoch"], "valid": {k: best[k] for k in ("recall@10", "ndcg@10")}, "history": r["history"]},
        "strata": {"recall@10": hits / n, "ndcg@10": ndcg / n},
        "pytorch": ref,
        "same_top10_as_given_vector": f"{same}/{min(n, 300)}",
        "latency_ms": {
            "query_p50": pct(took, 50), "query_p99": pct(took, 99), "query_mean": statistics.mean(took),
            "given_vector_p50": pct(took_plain, 50), "given_vector_mean": statistics.mean(took_plain),
        },
    }
    print(json.dumps({k: out[k] for k in ("strata", "pytorch", "same_top10_as_given_vector", "latency_ms")}, indent=1), flush=True)
    os.makedirs(a.out, exist_ok=True)
    with open(os.path.join(a.out, f"{a.data}{a.tag}.json"), "w") as f:
        json.dump(out, f, indent=1)


if __name__ == "__main__":
    main()
