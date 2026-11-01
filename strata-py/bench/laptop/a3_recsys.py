"""Part A3 of the laptop benchmark: recommendations in one file, on the Shop
data (SHOP_DIR from shop_prep.py).

    python a3_recsys.py --shop SHOP_DIR --work /tmp/a3 --out RESULTS_DIR [--systems strata,lancedb,sqlite-vec,faiss-sqlite]

Similar items: the 10 products nearest a product the user reviewed last, in
stock, within half to one and a half times its price, and not one the user
has reviewed. Strata answers with one query (the history is a subquery of
its reviews table); LanceDB with a prefiltered vector search after reading
the history from its reviews table; SQLite with sqlite-vec scans the rows
the filter keeps (exact); FAISS (HNSW) plus SQLite, the common two-part
stack, asks FAISS for 100 neighbours and SQLite which of them pass, asking
for 4 times more until 10 pass. Measured: build time and size, p50/p99 at 1
and 4 threads, recall@10 against an exact search of the same filter.

Home feed: 20 products for a user from two sources, the products nearest
the mean vector of the user's last 5, and a walk (personalized PageRank)
over co-review edges from the user's last 3, fused by RRF, then rules:
in stock, at most 1.5 times the dearest of the last 5, not reviewed, at
most 2 of a brand. Strata does it in one SQL query; with LanceDB or
sqlite-vec the app does the walk (over the edge table it keeps in memory),
the fusion and the rules in Python, as it would have to. Measured: p50/p99
per request, and how many of Strata's 20 each returns.

Next item (encode() and walks over Amazon 2014 Beauty, Sports and Toys) is
strata/results/generative/tiger.py --no-decoder, run on the same machine.

RESULTS_DIR/a3_<system>.json per system, each in its own process.
"""

from __future__ import annotations

import argparse
import heapq
import json
import random
import shutil
import subprocess
import sys
import threading
import time
from collections import defaultdict
from pathlib import Path
from typing import Any

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from common import files_bytes, timed_passes, write_report  # noqa: E402

DIM = 384
ALPHA, EPS = 0.15, 1e-4  # ppr's defaults in Strata
DEPTH = 100


class Shop:
    def __init__(self, shop: Path, limit_reviews: int | None) -> None:
        self.products = []
        for line in open(shop / "products.jsonl"):
            p = json.loads(line)
            self.products.append({"id": p["id"], "brand": p["brand"], "price": p["price"], "stock": p["stock"]})
        n = len(self.products)
        self.vecs = np.fromfile(shop / "product_vecs.f32", dtype=np.float32, count=n * DIM).reshape(n, DIM)
        self.index = {p["id"]: i for i, p in enumerate(self.products)}
        hist: dict[str, list[tuple[int, str]]] = defaultdict(list)
        self.reviews = []
        for i, line in enumerate(open(shop / "reviews.jsonl")):
            if limit_reviews and i >= limit_reviews:
                break
            r = json.loads(line)
            hist[r["user_id"]].append((r["ts"], r["product_id"]))
            self.reviews.append({"id": r["id"], "user_id": r["user_id"], "product_id": r["product_id"], "ts": r["ts"]})
        self.hist = {u: [p for _, p in sorted(h)] for u, h in hist.items()}
        # Co-review edges, as Strata's next-items pages build them: 1/g from
        # the item g steps back (g up to 5) to each item of a user's history.
        w: dict[tuple[str, str], float] = defaultdict(float)
        for items in self.hist.values():
            for j in range(1, len(items)):
                for g in range(1, min(5, j) + 1):
                    if items[j - g] != items[j]:
                        w[(items[j - g], items[j])] += 1.0 / g
        self.edges = [{"id": f"{a}>{b}", "src": a, "dst": b, "w": x} for (a, b), x in w.items()]


def probes(shop: Shop, n: int, seed: int = 5) -> list[dict]:
    rng = random.Random(seed)
    users = [u for u, h in shop.hist.items() if len(h) >= 3 and shop.products[shop.index[h[-1]]]["price"]]
    out = []
    for u in rng.sample(users, min(n, len(users))):
        h = shop.hist[u]
        item = h[-1]
        price = shop.products[shop.index[item]]["price"]
        last5 = h[-5:]
        v = shop.vecs[[shop.index[x] for x in last5]].mean(axis=0)
        prices = [shop.products[shop.index[x]]["price"] for x in last5 if shop.products[shop.index[x]]["price"]]
        out.append(
            {
                "user": u,
                "item": item,
                "v": shop.vecs[shop.index[item]],
                "lo": round(price * 0.5, 2),
                "hi": round(price * 1.5, 2),
                "seen": set(h),
                "feed_v": v / np.linalg.norm(v),
                "cap": round(1.5 * max(prices), 2) if prices else 60.0,
                "last3": h[-3:][::-1],
            }
        )
    return out


def exact_similar(shop: Shop, q: dict) -> list[str]:
    price = np.array([p["price"] if p["price"] is not None else -1.0 for p in shop.products])
    stock = np.array([p["stock"] for p in shop.products])
    m = (stock > 0) & (price >= q["lo"]) & (price <= q["hi"])
    for k in q["seen"]:
        m[shop.index[k]] = False
    s = np.where(m, shop.vecs @ q["v"], -np.inf)
    top = np.argsort(-s)[:10]
    return [shop.products[i]["id"] for i in top if np.isfinite(s[i])]


# ---------------------------------------------------------------- app code


class Graph:
    """The co-review edges in memory, read once from the system's edge table,
    and a personalized PageRank by pushes (Andersen, Chung and Lang), as
    Strata's ppr does, over the edges both ways."""

    def __init__(self, edges: list[tuple[str, str, float]]) -> None:
        adj: dict[str, dict[str, float]] = defaultdict(dict)
        for a, b, w in edges:
            adj[a][b] = adj[a].get(b, 0.0) + w
            adj[b][a] = adj[b].get(a, 0.0) + w
        self.adj = adj
        self.deg = {k: sum(v.values()) for k, v in adj.items()}

    def ppr(self, seeds: list[str], alpha: float = ALPHA, eps: float = EPS) -> dict[str, float]:
        seeds = [s for s in seeds if s in self.adj]
        if not seeds:
            return {}
        p: dict[str, float] = defaultdict(float)
        r: dict[str, float] = defaultdict(float)
        for s in seeds:
            r[s] += 1.0 / len(seeds)
        queue = [s for s in set(seeds) if r[s] >= eps * self.deg[s]]
        while queue:
            u = queue.pop()
            ru = r[u]
            if ru < eps * self.deg[u]:
                continue
            p[u] += alpha * ru
            r[u] = 0.0
            share = (1 - alpha) * ru / self.deg[u]
            for v, w in self.adj[u].items():
                before = r[v]
                r[v] = before + share * w
                if before < eps * self.deg[v] <= r[v]:
                    queue.append(v)
        return p


def rrf_keys(*lists: list[str], k: int = 60) -> list[str]:
    s: dict[str, float] = defaultdict(float)
    for l in lists:
        for i, key in enumerate(l):
            s[key] += 1.0 / (k + i + 1)
    return [key for key, _ in sorted(s.items(), key=lambda kv: (-kv[1], kv[0]))]


def max_per(keys: list[str], brand: dict[str, Any], k: int, limit: int) -> list[str]:
    seen: dict[Any, int] = defaultdict(int)
    out = []
    for key in keys:
        b = brand.get(key)
        if seen[b] < k:
            seen[b] += 1
            out.append(key)
            if len(out) == limit:
                break
    return out


# ---------------------------------------------------------------- the systems


class StrataR:
    name = "strata"

    def __init__(self, work: Path) -> None:
        import strata

        self.strata, self.path = strata, work / "shop.strata"

    def files(self):
        return [self.path, Path(str(self.path) + "-wal")]

    def build(self, shop: Shop) -> None:
        db = self.strata.open(
            str(self.path),
            schema={
                "table": "products",
                "key": "id",
                "fields": [
                    {"name": "id", "type": "keyword"},
                    {"name": "brand", "type": "keyword"},
                    {"name": "price", "type": "float"},
                    {"name": "stock", "type": "int"},
                    {"name": "emb", "type": "vector", "dim": DIM},
                ],
            },
        )
        for i in range(0, len(shop.products), 10_000):
            db.insert([{**p, "emb": shop.vecs[i + j]} for j, p in enumerate(shop.products[i : i + 10_000])])
        db.sql("CREATE TABLE reviews (id KEYWORD PRIMARY KEY, user_id KEYWORD NOT NULL, product_id KEYWORD NOT NULL, ts INT) OPTIONS(cluster_by = 'user_id')")
        for i in range(0, len(shop.reviews), 50_000):
            db.insert(shop.reviews[i : i + 50_000], table="reviews")
        db.sql("CREATE TABLE cooc (id KEYWORD PRIMARY KEY, src KEYWORD NOT NULL, dst KEYWORD NOT NULL, w FLOAT DEFAULT 0) OPTIONS(edges = 'src, dst, w')")
        for i in range(0, len(shop.edges), 50_000):
            db.insert(shop.edges[i : i + 50_000], table="cooc")
        db.optimize()
        self.db = db

    SIMILAR = (
        "SELECT id FROM products WHERE stock > 0 AND price BETWEEN :lo AND :hi "
        "AND NOT id IN (SELECT product_id FROM reviews WHERE user_id = :u) "
        "ORDER BY similarity(emb, :v) DESC LIMIT 10"
    )
    FEED = (
        "SELECT id FROM products WHERE stock > 0 AND price <= :cap "
        "AND NOT id IN (SELECT product_id FROM reviews WHERE user_id = :u) "
        "ORDER BY rrf(similarity(emb, :v), ppr(cooc, (SELECT product_id FROM reviews WHERE user_id = :u ORDER BY ts DESC LIMIT 3)), depth => 100) DESC "
        "MAX 2 PER brand LIMIT 20"
    )

    def similar(self, q: dict) -> list[str]:
        return list(self.db.query(self.SIMILAR, {"lo": q["lo"], "hi": q["hi"], "u": q["user"], "v": q["v"]}).keys)

    def feed(self, q: dict) -> list[str]:
        return list(self.db.query(self.FEED, {"cap": q["cap"], "u": q["user"], "v": q["feed_v"]}).keys)


class SqliteVecR:
    """SQLite with sqlite-vec: an exact scan of the rows the filter keeps."""

    name = "sqlite-vec"

    def __init__(self, work: Path) -> None:
        import sqlite3

        import sqlite_vec

        self.sqlite3, self.sqlite_vec, self.path = sqlite3, sqlite_vec, work / "shop.sqlite"
        self.local = threading.local()

    def files(self):
        return [self.path, Path(str(self.path) + "-wal"), Path(str(self.path) + "-shm")]

    def connect(self):
        c = self.sqlite3.connect(str(self.path), isolation_level=None, check_same_thread=False)
        c.enable_load_extension(True)
        self.sqlite_vec.load(c)
        c.enable_load_extension(False)
        c.execute("PRAGMA journal_mode = WAL")
        c.execute("PRAGMA cache_size = -262144")
        c.execute("PRAGMA mmap_size = 1073741824")
        return c

    @property
    def con(self):
        c = getattr(self.local, "con", None)
        if c is None:
            c = self.local.con = self.connect()
        return c

    def build(self, shop: Shop) -> None:
        c = self.con
        c.executescript(
            f"""
            CREATE TABLE products (rowid INTEGER PRIMARY KEY, id TEXT UNIQUE, brand TEXT, price REAL, stock INT);
            CREATE VIRTUAL TABLE products_vec USING vec0(emb float[{DIM}] distance_metric=cosine);
            CREATE TABLE reviews (id TEXT PRIMARY KEY, user_id TEXT, product_id TEXT, ts INT);
            CREATE TABLE cooc (src TEXT, dst TEXT, w REAL);
            """
        )
        c.execute("BEGIN")
        c.executemany("INSERT INTO products VALUES (?, ?, ?, ?, ?)", [(i + 1, p["id"], p["brand"], p["price"], p["stock"]) for i, p in enumerate(shop.products)])
        c.executemany("INSERT INTO products_vec (rowid, emb) VALUES (?, ?)", [(i + 1, shop.vecs[i].tobytes()) for i in range(len(shop.products))])
        c.executemany("INSERT INTO reviews VALUES (?, ?, ?, ?)", [(r["id"], r["user_id"], r["product_id"], r["ts"]) for r in shop.reviews])
        c.executemany("INSERT INTO cooc VALUES (?, ?, ?)", [(e["src"], e["dst"], e["w"]) for e in shop.edges])
        c.execute("COMMIT")
        c.executescript("CREATE INDEX reviews_user_ts ON reviews(user_id, ts); CREATE INDEX products_price ON products(price); ANALYZE; PRAGMA wal_checkpoint(TRUNCATE);")
        self.graph = Graph(c.execute("SELECT src, dst, w FROM cooc").fetchall())

    def similar(self, q: dict) -> list[str]:
        sql = (
            "SELECT p.id, vec_distance_cosine(v.emb, ?) AS d FROM products p JOIN products_vec v ON v.rowid = p.rowid "
            "WHERE p.stock > 0 AND p.price BETWEEN ? AND ? AND p.id NOT IN (SELECT product_id FROM reviews WHERE user_id = ?) ORDER BY d LIMIT ?"
        )
        return [r[0] for r in self.con.execute(sql, (q["v"].tobytes(), q["lo"], q["hi"], q["user"], 10))]

    def feed(self, q: dict) -> list[str]:
        c = self.con
        hist = [r[0] for r in c.execute("SELECT product_id FROM reviews WHERE user_id = ? ORDER BY ts DESC", (q["user"],))]
        seen = set(hist)
        sql = (
            "SELECT p.id, vec_distance_cosine(v.emb, ?) AS d FROM products p JOIN products_vec v ON v.rowid = p.rowid "
            "WHERE p.stock > 0 AND p.price <= ? AND p.id NOT IN (SELECT product_id FROM reviews WHERE user_id = ?) ORDER BY d LIMIT ?"
        )
        near = [r[0] for r in c.execute(sql, (q["feed_v"].tobytes(), q["cap"], q["user"], DEPTH))]
        walk = self.graph.ppr(hist[:3])
        cand = [k for k, _ in heapq.nlargest(len(walk), walk.items(), key=lambda kv: (kv[1], kv[0])) if k not in seen]
        rows = {}
        for i in range(0, len(cand), 900):
            part = cand[i : i + 900]
            rows.update({r[0]: r[1:] for r in c.execute(f"SELECT id, brand, price, stock FROM products WHERE id IN ({', '.join('?' * len(part))})", part)})
        walked = [k for k in cand if k in rows and rows[k][2] > 0 and rows[k][1] is not None and rows[k][1] <= q["cap"]][:DEPTH]
        fused = rrf_keys(near, walked)
        need = [k for k in fused if k not in rows]
        for i in range(0, len(need), 900):
            part = need[i : i + 900]
            rows.update({r[0]: r[1:] for r in c.execute(f"SELECT id, brand, price, stock FROM products WHERE id IN ({', '.join('?' * len(part))})", part)})
        return max_per(fused, {k: rows[k][0] for k in fused}, 2, 20)


class FaissSqliteR(SqliteVecR):
    """FAISS HNSW for the vectors, SQLite for everything else."""

    name = "faiss-sqlite"

    def build(self, shop: Shop) -> None:
        import faiss

        super().build(shop)
        self.con.execute("DROP TABLE products_vec")
        self.con.execute("VACUUM")
        idx = faiss.IndexHNSWFlat(DIM, 16, faiss.METRIC_INNER_PRODUCT)
        idx.hnsw.efConstruction = 200
        idx.add(np.ascontiguousarray(shop.vecs))
        idx.hnsw.efSearch = 128
        faiss.write_index(idx, str(self.path) + ".faiss")
        self.idx, self.n = idx, len(shop.products)

    def files(self):
        return super().files() + [Path(str(self.path) + ".faiss")]

    def similar(self, q: dict) -> list[str]:
        import faiss

        k = 100
        while True:
            params = faiss.SearchParametersHNSW(efSearch=max(128, k))
            _, ids = self.idx.search(q["v"][None, :], k, params=params)
            rowids = [int(i) + 1 for i in ids[0] if i >= 0]
            sql = (
                f"SELECT rowid, id FROM products WHERE rowid IN ({', '.join('?' * len(rowids))}) AND stock > 0 AND price BETWEEN ? AND ? "
                "AND id NOT IN (SELECT product_id FROM reviews WHERE user_id = ?)"
            )
            ok = dict(self.con.execute(sql, (*rowids, q["lo"], q["hi"], q["user"])).fetchall())
            got = [ok[r] for r in rowids if r in ok]
            if len(got) >= 10 or k >= self.n:
                return got[:10]
            k = min(self.n, k * 4)

    def feed(self, q: dict) -> list[str]:
        raise NotImplementedError


class LanceR:
    name = "lancedb"

    def __init__(self, work: Path) -> None:
        import os

        os.environ.setdefault("LANCE_LOG", "error")
        import lancedb

        self.lancedb, self.path = lancedb, work / "shop.lance"

    def files(self):
        return [self.path]

    def build(self, shop: Shop) -> None:
        import pyarrow as pa

        db = self.lancedb.connect(str(self.path))
        t = pa.table(
            {
                "id": [p["id"] for p in shop.products],
                "brand": [p["brand"] for p in shop.products],
                "price": pa.array([p["price"] for p in shop.products], pa.float64()),
                "stock": [p["stock"] for p in shop.products],
                "vector": pa.FixedSizeListArray.from_arrays(pa.array(shop.vecs.reshape(-1)), DIM),
            }
        )
        self.p = db.create_table("products", t, mode="overwrite")
        self.p.create_index(metric="cosine", index_type="IVF_HNSW_SQ", vector_column_name="vector")
        for col in ("id", "price", "stock"):
            self.p.create_scalar_index(col)
        self.r = db.create_table("reviews", pa.Table.from_pylist(shop.reviews), mode="overwrite")
        self.r.create_scalar_index("user_id")
        self.e = db.create_table("cooc", pa.Table.from_pylist(shop.edges), mode="overwrite")
        e = self.e.to_arrow()
        self.graph = Graph(list(zip(e["src"].to_pylist(), e["dst"].to_pylist(), e["w"].to_pylist())))

    @staticmethod
    def _in(keys) -> str:
        return ", ".join("'" + k.replace("'", "''") + "'" for k in keys)

    def history(self, u: str) -> list[str]:
        r = self.r.search().where(f"user_id = '{u}'").select(["product_id", "ts"]).limit(100_000).to_arrow()
        return r.sort_by([("ts", "descending")])["product_id"].to_pylist()

    def similar(self, q: dict) -> list[str]:
        seen = self.history(q["user"])
        w = f"stock > 0 AND price BETWEEN {q['lo']} AND {q['hi']}" + (f" AND id NOT IN ({self._in(seen)})" if seen else "")
        s = self.p.search(q["v"].tolist(), vector_column_name="vector").distance_type("cosine").where(w, prefilter=True).limit(10).select(["id"])
        return s.to_arrow()["id"].to_pylist()

    def feed(self, q: dict) -> list[str]:
        hist = self.history(q["user"])
        seen = set(hist)
        w = f"stock > 0 AND price <= {q['cap']}" + (f" AND id NOT IN ({self._in(hist)})" if hist else "")
        near = self.p.search(q["feed_v"].tolist(), vector_column_name="vector").distance_type("cosine").where(w, prefilter=True).limit(DEPTH).select(["id", "brand"]).to_arrow()
        rows = dict(zip(near["id"].to_pylist(), near["brand"].to_pylist()))
        walk = self.graph.ppr(hist[:3])
        cand = [k for k, _ in heapq.nlargest(len(walk), walk.items(), key=lambda kv: (kv[1], kv[0])) if k not in seen]
        ok = {}
        for i in range(0, len(cand), 1000):
            part = cand[i : i + 1000]
            got = self.p.search().where(f"id IN ({self._in(part)}) AND stock > 0 AND price <= {q['cap']}").select(["id", "brand"]).limit(len(part)).to_arrow()
            ok.update(zip(got["id"].to_pylist(), got["brand"].to_pylist()))
        walked = [k for k in cand if k in ok][:DEPTH]
        rows.update(ok)
        fused = rrf_keys(near["id"].to_pylist(), walked)
        return max_per(fused, rows, 2, 20)


SYSTEMS = {"strata": StrataR, "lancedb": LanceR, "sqlite-vec": SqliteVecR, "faiss-sqlite": FaissSqliteR}


def run(name: str, shop_dir: Path, work: Path, seconds: float, users: int, limit_reviews: int | None) -> dict:
    shop = Shop(shop_dir, limit_reviews)
    qs = probes(shop, users)
    s = SYSTEMS[name](work)
    t0 = time.perf_counter()
    s.build(shop)
    rep: dict[str, Any] = {"system": name, "products": len(shop.products), "reviews": len(shop.reviews), "edges": len(shop.edges), "probes": len(qs), "build": {"seconds": time.perf_counter() - t0, "bytes": files_bytes(s.files())}}
    print(f"{name}: built in {rep['build']['seconds']:.1f} s, {rep['build']['bytes'] / 1e6:.0f} MB", file=sys.stderr)

    answers = [s.similar(q) for q in qs]
    truth = [exact_similar(shop, q) for q in qs]
    rec = [len(set(a) & set(t)) / max(1, len(t)) for a, t in zip(answers, truth)]
    bad = sum(1 for a, q in zip(answers, qs) if set(a) & q["seen"])
    rep["similar"] = {"recall10": sum(rec) / len(rec), "returned_seen": bad, "warm1": timed_passes(s.similar, qs, seconds), "warm4": timed_passes(s.similar, qs, seconds, 4), "answers": answers[:300]}
    print(f"{name}: similar p50 {rep['similar']['warm1']['p50_ms']:.2f} ms p99 {rep['similar']['warm1']['p99_ms']:.2f} ms, recall {rep['similar']['recall10']:.3f}, 4 threads {rep['similar']['warm4']['qps']:,.0f}/s", file=sys.stderr)
    try:
        feeds = [s.feed(q) for q in qs[:300]]
        rep["feed"] = {"warm1": timed_passes(s.feed, qs[:300], seconds), "answers": feeds}
        print(f"{name}: feed p50 {rep['feed']['warm1']['p50_ms']:.2f} ms p99 {rep['feed']['warm1']['p99_ms']:.2f} ms", file=sys.stderr)
    except NotImplementedError:
        rep["feed"] = None
    return rep


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--shop", required=True)
    ap.add_argument("--systems", default=",".join(SYSTEMS))
    ap.add_argument("--work", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--seconds", type=float, default=5.0)
    ap.add_argument("--users", type=int, default=2000)
    ap.add_argument("--limit-reviews", type=int, default=None)
    ap.add_argument("--one", action="store_true", help=argparse.SUPPRESS)
    a = ap.parse_args()
    systems = a.systems.split(",")
    if len(systems) > 1 and not a.one:
        for name in systems:
            args = [sys.executable, __file__, "--shop", a.shop, "--systems", name, "--work", a.work, "--out", a.out, "--seconds", str(a.seconds), "--users", str(a.users), "--one"]
            if a.limit_reviews:
                args += ["--limit-reviews", str(a.limit_reviews)]
            subprocess.run(args, check=False)
        return
    name = systems[0]
    work = Path(a.work) / name
    if work.exists():
        shutil.rmtree(work)
    work.mkdir(parents=True)
    rep = run(name, Path(a.shop), work, a.seconds, a.users, a.limit_reviews)
    rep["params"] = {"seconds": a.seconds, "users": a.users, "alpha": ALPHA, "eps": EPS, "depth": DEPTH}
    write_report(Path(a.out) / f"a3_{name}.json", rep)
    shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
