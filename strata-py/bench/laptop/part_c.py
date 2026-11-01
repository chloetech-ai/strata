"""Part C of the laptop benchmark: the Shop app's queries, unchanged, from a
file in the app to a sharded cluster. part_c.sh drives the stages; this
script builds the file and runs the queries against one stage.

    python part_c.py build --shop SHOP_DIR --out shop.strata      # the file, and shop.schema.json beside it
    python part_c.py run --db shop.strata --label app --out DIR    # stage 1: in the app's process
    python part_c.py run --url http://127.0.0.1:7700 --label server --out DIR
    python part_c.py run --url ... --label moving --loop 600 --out DIR   # while slots move: errors and latency only
    python part_c.py compare --out DIR                              # every stage's answers against stage 1's

Twelve queries an app makes, from parts A1 to A3: a product by id, a user's
reviews, a page of a category, ratings per brand, reviews per month, the
products with most reviews (a join), a user's products (a join), a search,
its facets, a hybrid search, similar items and the home feed. Each runs for
50 probes, the same at every stage (a seeded choice from the file's rows,
kept in DIR/probes.json). A stage writes DIR/c_<label>.json: per query the
p50 and p99 of a request, its errors, and a SHA-256 of the answers (rows as
JSON, floats to 6 digits); `compare` says, per stage and query, whether the
answers are those of stage 1 row by row.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import sys
import time
from pathlib import Path
from typing import Any

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from common import percentile, write_report  # noqa: E402

DIM = 384
PROBES = 50

PRODUCTS = {
    "table": "products",
    "key": "id",
    "fields": [
        {"name": "id", "type": "keyword"},
        {"name": "title", "type": "text", "indexed": False, "positions": False},
        {"name": "description", "type": "text", "indexed": False, "positions": False},
        {"name": "body", "type": "text", "from": ["title", "description"]},
        {"name": "brand", "type": "keyword"},
        {"name": "platform", "type": "keyword"},
        {"name": "category", "type": "keyword"},
        {"name": "price", "type": "float"},
        {"name": "rating", "type": "float"},
        {"name": "rating_count", "type": "int"},
        {"name": "stock", "type": "int"},
        {"name": "year", "type": "int"},
        {"name": "rated", "type": "keyword"},
        {"name": "attrs", "type": "json"},
        {"name": "emb", "type": "vector", "dim": DIM},
    ],
}
REVIEWS = (
    "CREATE TABLE reviews (id KEYWORD PRIMARY KEY, user_id KEYWORD NOT NULL, product_id KEYWORD NOT NULL, rating INT, "
    "title TEXT OPTIONS(indexed = false), text TEXT OPTIONS(indexed = false), ts INT, month INT, helpful INT, verified INT)"
)
COOC = "CREATE TABLE cooc (id KEYWORD PRIMARY KEY, src KEYWORD NOT NULL, dst KEYWORD NOT NULL, w FLOAT DEFAULT 0) OPTIONS(edges = 'src, dst, w')"

QUERIES = {
    "get": "SELECT id, title, brand, price, rating, stock FROM products WHERE id = :id",
    "user_reviews": "SELECT id, product_id, rating, ts FROM reviews WHERE user_id = :u ORDER BY ts DESC LIMIT 50",
    "page": "SELECT id, title, price, rating FROM products WHERE category = :c AND price BETWEEN :lo AND :hi ORDER BY rating DESC, id LIMIT 20 OFFSET 100",
    "agg_brand": "SELECT brand, AVG(rating) AS avg_rating, COUNT(*) AS n FROM products WHERE brand IS NOT NULL GROUP BY brand HAVING COUNT(*) >= 20 ORDER BY n DESC, brand LIMIT 20",
    "agg_month": "SELECT month, COUNT(*) AS n, AVG(rating) AS avg_rating FROM reviews GROUP BY month ORDER BY month",
    "join_top": "SELECT p.id, p.title, COUNT(*) AS n FROM reviews r JOIN products p ON r.product_id = p.id GROUP BY p.id, p.title ORDER BY n DESC, p.id LIMIT 10",
    "join_user": "SELECT p.id, p.title, r.rating, r.ts FROM reviews r JOIN products p ON p.id = r.product_id WHERE r.user_id = :u ORDER BY r.ts DESC, p.id LIMIT 50",
    "search": "SELECT id, score() FROM products ORDER BY bm25(body, :q, syntax => 'plain') DESC LIMIT 10",
    "facet": "SELECT category, COUNT(*) AS n FROM products WHERE MATCH(body, :q, syntax => 'plain') GROUP BY category ORDER BY n DESC, category LIMIT 5",
    "hybrid": "SELECT id FROM products ORDER BY rrf(bm25(body, :q, syntax => 'plain'), similarity(emb, :v), depth => 100) DESC LIMIT 10",
    "similar": (
        "SELECT id FROM products WHERE stock > 0 AND price BETWEEN :lo AND :hi "
        "AND NOT id IN (SELECT product_id FROM reviews WHERE user_id = :u) ORDER BY similarity(emb, :v) DESC LIMIT 10"
    ),
    "feed": (
        "SELECT id FROM products WHERE stock > 0 AND price <= :cap "
        "AND NOT id IN (SELECT product_id FROM reviews WHERE user_id = :u) "
        "ORDER BY rrf(similarity(emb, :fv), ppr(cooc, (SELECT product_id FROM reviews WHERE user_id = :u ORDER BY ts DESC LIMIT 3)), depth => 100) DESC "
        "MAX 2 PER brand LIMIT 20"
    ),
}
PARAMS = {
    "get": ["id"],
    "user_reviews": ["u"],
    "page": ["c", "lo", "hi"],
    "agg_brand": [],
    "agg_month": [],
    "join_top": [],
    "join_user": ["u"],
    "search": ["q"],
    "facet": ["q"],
    "hybrid": ["q", "v"],
    "similar": ["lo", "hi", "u", "v"],
    "feed": ["cap", "u", "fv"],
}


def month_of(ts_ms: int) -> int:
    t = time.gmtime(ts_ms / 1000)
    return t.tm_year * 100 + t.tm_mon


def build(shop: Path, out: Path, limit_reviews: int | None) -> None:
    import strata

    from a1_app import product_row, review_row  # the rows A1 loads

    meta = json.loads((shop / "meta.json").read_text())
    n = meta["products"]
    vecs = np.fromfile(shop / "product_vecs.f32", dtype=np.float32, count=n * DIM).reshape(n, DIM)
    t0 = time.perf_counter()
    db = strata.open(str(out), schema=PRODUCTS)
    rows, i = [], 0
    for line in open(shop / "products.jsonl"):
        p = json.loads(line)
        r = product_row(p)
        r["emb"] = vecs[i]
        i += 1
        rows.append(r)
        if len(rows) == 10_000:
            db.insert(rows)
            rows = []
    if rows:
        db.insert(rows)
    db.sql(REVIEWS)
    hist: dict[str, list[tuple[int, str]]] = {}
    rows = []
    for j, line in enumerate(open(shop / "reviews.jsonl")):
        if limit_reviews and j >= limit_reviews:
            break
        r = review_row(json.loads(line))
        hist.setdefault(r["user_id"], []).append((r["ts"], r["product_id"]))
        rows.append(r)
        if len(rows) == 50_000:
            db.insert(rows, table="reviews")
            rows = []
    if rows:
        db.insert(rows, table="reviews")
    w: dict[tuple[str, str], float] = {}
    for h in hist.values():
        items = [p for _, p in sorted(h)]
        for k in range(1, len(items)):
            for g in range(1, min(5, k) + 1):
                if items[k - g] != items[k]:
                    w[(items[k - g], items[k])] = w.get((items[k - g], items[k]), 0.0) + 1.0 / g
    db.sql(COOC)
    edges = [{"id": f"{a}>{b}", "src": a, "dst": b, "w": x} for (a, b), x in w.items()]
    for k in range(0, len(edges), 50_000):
        db.insert(edges[k : k + 50_000], table="cooc")
    db.close()
    Path(str(out).removesuffix(".strata") + ".schema.json").write_text(json.dumps(PRODUCTS, indent=1))
    print(f"built {out}: {n} products, {sum(len(h) for h in hist.values())} reviews, {len(edges)} edges in {time.perf_counter() - t0:.0f} s", file=sys.stderr)


def make_probes(db, seed: int = 11) -> dict[str, list[dict]]:
    """Probes drawn from the database itself, the same at every stage."""
    rng = random.Random(seed)
    prods = db.query("SELECT id, title, category, price FROM products WHERE price IS NOT NULL LIMIT 1000000").rows
    users = [r[0] for r in db.query("SELECT user_id, COUNT(*) AS n FROM reviews GROUP BY user_id HAVING COUNT(*) >= 3 ORDER BY n DESC, user_id LIMIT 5000").rows]
    cats = [r[0] for r in db.query("SELECT category, COUNT(*) AS n FROM products WHERE category IS NOT NULL GROUP BY category ORDER BY n DESC, category LIMIT 10").rows]
    out: dict[str, list[dict]] = {}
    pick = rng.sample(prods, PROBES)
    us = rng.sample(users, PROBES)

    for name in QUERIES:
        ps = []
        for i in range(PROBES if PARAMS[name] else 1):
            pid, title, cat, price = pick[i]
            words = [w for w in title.split() if w.isalpha()][:3] or ["game"]
            ps.append(
                {
                    "id": pid,
                    "u": us[i],
                    "c": cats[i % len(cats)],
                    "lo": 10.0,
                    "hi": 60.0,
                    "q": " ".join(words),
                    "item": pid,
                    "price": price,
                    "cap": round(1.5 * price, 2),
                }
            )
        out[name] = ps
    return out


def canon(rows: list) -> list:
    return [[round(v, 6) if isinstance(v, float) else v for v in r] for r in rows]


class Target:
    def __init__(self, db_path: str | None, url: str | None) -> None:
        import strata

        self.db = strata.Database.open(db_path) if db_path else strata.Client(url, timeout=120)
        self.local = db_path is not None

    def query(self, sql: str, params: dict) -> list:
        return self.db.query(sql, params).rows



def params_of(name: str, p: dict) -> dict:
    out = {k: p[k] for k in PARAMS[name] if k in p}
    if name in ("similar",):
        out.update(lo=round(p["price"] * 0.5, 2), hi=round(p["price"] * 1.5, 2), v={"vector_of": p["item"]})
    if name == "hybrid":
        out["v"] = {"vector_of": p["item"]}
    if name == "feed":
        out["fv"] = {"vector_of": p["item"]}
    return out


def run(t: Target, probes: dict[str, list[dict]], repeats: int) -> dict:
    rep: dict[str, Any] = {}
    for name, sql in QUERIES.items():
        answers, ms, errors = [], [], []
        for p in probes[name]:
            prm = params_of(name, p)
            got = None
            for r in range(repeats):
                t0 = time.perf_counter()
                try:
                    got = t.query(sql, prm)
                except Exception as e:  # noqa: BLE001
                    errors.append(repr(e)[:300])
                    got = None
                ms.append((time.perf_counter() - t0) * 1e3)
            answers.append(canon(got) if got is not None else None)
        digest = hashlib.sha256(json.dumps(answers, sort_keys=True).encode()).hexdigest()
        rep[name] = {"p50_ms": percentile(ms, 50), "p99_ms": percentile(ms, 99), "requests": len(ms), "errors": len(errors), "first_error": errors[0] if errors else None, "sha256": digest, "answers": answers}
        print(f"  {name:13} p50 {rep[name]['p50_ms']:8.2f} ms  errors {len(errors)}  {digest[:12]}", file=sys.stderr)
    return rep


def loop(t: Target, probes: dict[str, list[dict]], seconds: float) -> dict:
    """Queries round-robin for `seconds`, as a client during a change: latency and errors per second."""
    names = list(QUERIES)
    t_end = time.perf_counter() + seconds
    per_s: dict[int, dict[str, Any]] = {}
    t_start = time.perf_counter()
    i = 0
    while time.perf_counter() < t_end:
        name = names[i % len(names)]
        p = probes[name][(i // len(names)) % len(probes[name])]
        t0 = time.perf_counter()
        err = None
        try:
            t.query(QUERIES[name], params_of(name, p))
        except Exception as e:  # noqa: BLE001
            err = repr(e)[:200]
        dt = (time.perf_counter() - t0) * 1e3
        sec = int(t0 - t_start)
        b = per_s.setdefault(sec, {"n": 0, "errors": 0, "ms": [], "first_error": None})
        b["n"] += 1
        b["ms"].append(dt)
        if err:
            b["errors"] += 1
            b["first_error"] = b["first_error"] or err
        i += 1
    seconds_out = [{"second": s, "requests": b["n"], "errors": b["errors"], "p50_ms": percentile(b["ms"], 50), "p99_ms": percentile(b["ms"], 99), "first_error": b["first_error"]} for s, b in sorted(per_s.items())]
    all_ms = [x for b in per_s.values() for x in b["ms"]]
    return {"seconds": seconds_out, "requests": len(all_ms), "errors": sum(b["errors"] for b in per_s.values()), "p50_ms": percentile(all_ms, 50), "p99_ms": percentile(all_ms, 99)}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build")
    b.add_argument("--shop", required=True)
    b.add_argument("--out", required=True)
    b.add_argument("--limit-reviews", type=int, default=None)
    r = sub.add_parser("run")
    r.add_argument("--db")
    r.add_argument("--url")
    r.add_argument("--label", required=True)
    r.add_argument("--out", required=True)
    r.add_argument("--repeats", type=int, default=3)
    r.add_argument("--loop", type=float, default=0, help="query round-robin this many seconds instead (during a change)")
    c = sub.add_parser("compare")
    c.add_argument("--out", required=True)
    a = ap.parse_args()
    if a.cmd == "build":
        build(Path(a.shop), Path(a.out), a.limit_reviews)
        return
    out = Path(a.out)
    if a.cmd == "run":
        t = Target(a.db, a.url)
        pf = out / "probes.json"
        if pf.exists():
            probes = json.loads(pf.read_text())
        else:
            probes = make_probes(t.db)
            out.mkdir(parents=True, exist_ok=True)
            pf.write_text(json.dumps(probes))
        print(f"stage {a.label}:", file=sys.stderr)
        if a.loop:
            rep = {"stage": a.label, "loop": loop(t, probes, a.loop)}
            write_report(out / f"c_{a.label}-loop.json", rep)
        else:
            t0 = time.perf_counter()
            rep = {"stage": a.label, "target": a.db or a.url, "queries": run(t, probes, a.repeats), "seconds": time.perf_counter() - t0}
            write_report(out / f"c_{a.label}.json", rep)
        if t.local:
            t.db.close()
        return
    # compare
    stages = sorted(out.glob("c_*.json"))
    stages = [p for p in stages if not p.name.endswith("-loop.json")]
    reps = {json.loads(p.read_text())["stage"]: json.loads(p.read_text()) for p in stages}
    first = reps.get("app") or next(iter(reps.values()))
    table = {}
    for stage, r in reps.items():
        row = {}
        for q, v in r["queries"].items():
            same = v["sha256"] == first["queries"][q]["sha256"]
            if not same:
                a_, b_ = first["queries"][q]["answers"], v["answers"]
                differ = sum(1 for x, y in zip(a_, b_) if x != y)
                row[q] = f"differs on {differ} of {len(a_)} probes"
            else:
                row[q] = "same"
        table[stage] = row
    print(json.dumps(table, indent=1))
    write_report(out / "c_compare.json", {"against": first["stage"], "table": table})


if __name__ == "__main__":
    main()
