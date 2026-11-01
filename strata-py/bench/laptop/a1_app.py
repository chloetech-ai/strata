"""Part A1 of the laptop benchmark: Strata as an app's database in one
file, in place of SQLite (WAL, at synchronous FULL and NORMAL) and DuckDB,
each driven from Python as an app would use it.

    python a1_app.py --shop SHOP_DIR --work /tmp/a1 --out RESULTS_DIR [--systems strata,sqlite-full,sqlite-normal,duckdb]

SHOP_DIR is what shop_prep.py writes. Two tables: products (13 columns,
details as JSON) and reviews (10 columns). No search indexes here (A2 has
them): text is stored, and each system indexes the columns the queries
filter on (SQLite: B-trees; Strata: every keyword and number column, its
default; DuckDB: its primary keys and zone maps).

Each system runs in its own process (so memory is its own) through these
steps; RESULTS_DIR/a1_<system>.json gets every number and the machine:

  load        products, then reviews, 10,000 rows per transaction: rows/s, file size
  open        close, then open and answer one lookup: ms
  get         a product by id; a user's latest 50 reviews: p50/p99
  insert      1 review per durable transaction; 1,000 per transaction: rows/s
  update      price and stock by id; a counter + 1; 1 per transaction: rows/s
  delete      by id, 1 per transaction; by a time range (about 1,000 rows)
  page        category and price range ORDER BY rating LIMIT 20 at offsets 0 to 10,000
  aggregate   average rating and count per brand; reviews per month
  join        top 10 products by reviews with their titles; a user's reviewed products
  mixed       4 reader threads and 1 writer thread for 10 s: q/s and p99 of each
  footprint   file size and memory after the load and after 10% of rows updated

Durability is each system's as configured: Strata syncs every commit (on
macOS with F_FULLFSYNC, through Rust's File::sync_all), sqlite-full is
synchronous=FULL with fullfsync=ON (the same guarantee on macOS),
sqlite-normal is synchronous=NORMAL (a power cut may lose the last
commits), DuckDB syncs its WAL at each commit.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any, Iterator

sys.path.insert(0, str(Path(__file__).parent))
from common import files_bytes, summary, timed_passes, write_report  # noqa: E402

BATCH = 10_000
PRODUCT_COLS = ["id", "title", "brand", "platform", "category", "price", "rating", "rating_count", "stock", "year", "rated", "description", "attrs"]
REVIEW_COLS = ["id", "user_id", "product_id", "rating", "title", "text", "ts", "month", "helpful", "verified"]


def current_rss() -> int:
    try:
        import psutil

        return psutil.Process().memory_info().rss
    except ImportError:
        pass
    try:
        with open("/proc/self/statm") as f:
            return int(f.read().split()[1]) * os.sysconf("SC_PAGE_SIZE")
    except OSError:
        import resource

        r = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        return r if sys.platform == "darwin" else r * 1024


def month_of(ts_ms: int) -> int:
    t = time.gmtime(ts_ms / 1000)
    return t.tm_year * 100 + t.tm_mon


def product_row(p: dict) -> dict:
    r = {k: p.get(k) for k in PRODUCT_COLS}
    r["attrs"] = {k: v for k, v in (r["attrs"] or {}).items() if k.strip()}  # as shop_prep.py now does
    return r


def review_row(r: dict) -> dict:
    return {**{k: r.get(k) for k in REVIEW_COLS if k not in ("month", "verified")}, "month": month_of(r["ts"]), "verified": int(bool(r.get("verified")))}


def batches(path: Path, conv, n: int = BATCH, limit: int | None = None) -> Iterator[list[dict]]:
    out, seen = [], 0
    with open(path) as f:
        for line in f:
            out.append(conv(json.loads(line)))
            seen += 1
            if len(out) >= n:
                yield out
                out = []
            if limit is not None and seen >= limit:
                break
    if out:
        yield out


# ---------------------------------------------------------------- the systems


class Strata:
    name = "strata"

    def __init__(self, path: Path) -> None:
        import strata

        self.strata, self.path = strata, path
        self.version = getattr(strata, "__version__", "")

    def create(self) -> None:
        self.db = self.strata.open(
            str(self.path),
            schema={
                "table": "products",
                "key": "id",
                "fields": [
                    {"name": "id", "type": "keyword"},
                    {"name": "title", "type": "text", "indexed": False},
                    {"name": "brand", "type": "keyword"},
                    {"name": "platform", "type": "keyword"},
                    {"name": "category", "type": "keyword"},
                    {"name": "price", "type": "float"},
                    {"name": "rating", "type": "float"},
                    {"name": "rating_count", "type": "int"},
                    {"name": "stock", "type": "int"},
                    {"name": "year", "type": "int"},
                    {"name": "rated", "type": "keyword"},
                    {"name": "description", "type": "text", "indexed": False},
                    {"name": "attrs", "type": "json"},
                ],
            },
        )
        self.db.sql(
            "CREATE TABLE reviews (id KEYWORD PRIMARY KEY, user_id KEYWORD, product_id KEYWORD, rating INT, "
            "title TEXT OPTIONS(indexed = false), text TEXT OPTIONS(indexed = false), ts INT, month INT, helpful INT, verified INT)"
        )

    def files(self) -> list[Path]:
        return [self.path, Path(str(self.path) + "-wal")]

    def reopen(self) -> None:
        self.db = self.strata.Database.open(str(self.path))

    def close(self) -> None:
        self.db.close()

    def insert(self, table: str, rows: list[dict]) -> None:
        self.db.insert(rows, table=table)

    def q(self, sql: str, params: dict | None = None) -> list:
        return self.db.query(sql, params or {}).rows

    def x(self, sql: str, params: dict | None = None) -> int:
        return self.db.sql(sql, params or {})

    def delete_one(self, table: str, key: str) -> None:
        self.db.delete(key, table=table)

    def optimize(self) -> None:
        self.db.optimize()


class Sqlite:
    def __init__(self, path: Path, sync: str) -> None:
        import sqlite3

        self.sqlite3, self.path, self.sync = sqlite3, path, sync
        self.name = f"sqlite-{sync.lower()}"
        self.version = sqlite3.sqlite_version
        self.local = threading.local()

    def connect(self):
        c = self.sqlite3.connect(str(self.path), isolation_level=None, check_same_thread=False)
        c.execute("PRAGMA journal_mode = WAL")
        c.execute(f"PRAGMA synchronous = {self.sync}")
        if self.sync == "FULL":
            c.execute("PRAGMA fullfsync = ON")  # macOS: flush the drive's cache, as F_FULLFSYNC does
            c.execute("PRAGMA checkpoint_fullfsync = ON")
        c.execute("PRAGMA cache_size = -262144")
        c.execute("PRAGMA mmap_size = 1073741824")
        return c

    @property
    def con(self):
        c = getattr(self.local, "con", None)
        if c is None:
            c = self.local.con = self.connect()
        return c

    def create(self) -> None:
        self.con.executescript(
            """
            CREATE TABLE products (id TEXT PRIMARY KEY, title TEXT, brand TEXT, platform TEXT, category TEXT, price REAL, rating REAL,
                                   rating_count INT, stock INT, year INT, rated TEXT, description TEXT, attrs TEXT);
            CREATE TABLE reviews (id TEXT PRIMARY KEY, user_id TEXT, product_id TEXT, rating INT, title TEXT, text TEXT, ts INT,
                                  month INT, helpful INT, verified INT);
            CREATE INDEX products_category_price ON products(category, price);
            CREATE INDEX products_brand ON products(brand);
            CREATE INDEX reviews_user_ts ON reviews(user_id, ts);
            CREATE INDEX reviews_product ON reviews(product_id);
            CREATE INDEX reviews_ts ON reviews(ts);
            """
        )

    def files(self) -> list[Path]:
        return [self.path, Path(str(self.path) + "-wal"), Path(str(self.path) + "-shm")]

    def reopen(self) -> None:
        self.local = threading.local()

    def close(self) -> None:
        c = getattr(self.local, "con", None)
        if c is not None:
            c.close()
        self.local = threading.local()

    def insert(self, table: str, rows: list[dict]) -> None:
        cols = PRODUCT_COLS if table == "products" else REVIEW_COLS
        conv = (lambda r: [json.dumps(r["attrs"]) if k == "attrs" else r[k] for k in cols]) if table == "products" else (lambda r: [r[k] for k in cols])
        c = self.con
        c.execute("BEGIN")
        c.executemany(f"INSERT INTO {table} ({', '.join(cols)}) VALUES ({', '.join('?' * len(cols))})", [conv(r) for r in rows])
        c.execute("COMMIT")

    def q(self, sql: str, params: dict | None = None) -> list:
        return self.con.execute(sql, params or {}).fetchall()

    def x(self, sql: str, params: dict | None = None) -> int:
        return self.con.execute(sql, params or {}).rowcount

    def delete_one(self, table: str, key: str) -> None:
        self.con.execute(f"DELETE FROM {table} WHERE id = ?", (key,))

    def optimize(self) -> None:
        self.con.execute("ANALYZE")
        self.con.execute("PRAGMA wal_checkpoint(TRUNCATE)")


class Duck:
    name = "duckdb"

    def __init__(self, path: Path) -> None:
        import duckdb

        self.duckdb, self.path = duckdb, path
        self.version = duckdb.__version__
        self.local = threading.local()

    @property
    def con(self):
        c = getattr(self.local, "con", None)
        if c is None:
            c = self.local.con = self.root.cursor()
        return c

    def create(self) -> None:
        self.root = self.duckdb.connect(str(self.path))
        self.root.execute(
            """
            CREATE TABLE products (id VARCHAR PRIMARY KEY, title VARCHAR, brand VARCHAR, platform VARCHAR, category VARCHAR, price DOUBLE,
                                   rating DOUBLE, rating_count INTEGER, stock INTEGER, year INTEGER, rated VARCHAR, description VARCHAR, attrs JSON);
            CREATE TABLE reviews (id VARCHAR PRIMARY KEY, user_id VARCHAR, product_id VARCHAR, rating INTEGER, title VARCHAR, text VARCHAR,
                                  ts BIGINT, month INTEGER, helpful INTEGER, verified INTEGER);
            """
        )

    def files(self) -> list[Path]:
        return [self.path, Path(str(self.path) + ".wal")]

    def reopen(self) -> None:
        self.root = self.duckdb.connect(str(self.path))
        self.local = threading.local()

    def close(self) -> None:
        self.root.close()
        self.local = threading.local()

    def insert(self, table: str, rows: list[dict]) -> None:
        import pyarrow as pa

        cols = PRODUCT_COLS if table == "products" else REVIEW_COLS
        data = {k: [json.dumps(r[k]) if k == "attrs" else r[k] for r in rows] for k in cols}
        t = pa.table(data)
        c = self.con
        c.register("batch", t)
        c.execute(f"INSERT INTO {table} SELECT * FROM batch").fetchall()
        c.unregister("batch")

    def q(self, sql: str, params: dict | None = None) -> list:
        return self.con.execute(sql.replace(":", "$") if params else sql, params or {}).fetchall()

    def x(self, sql: str, params: dict | None = None) -> int:
        r = self.con.execute(sql.replace(":", "$") if params else sql, params or {}).fetchall()
        return r[0][0] if r else 0

    def delete_one(self, table: str, key: str) -> None:
        self.con.execute(f"DELETE FROM {table} WHERE id = ?", [key]).fetchall()

    def optimize(self) -> None:
        self.root.execute("CHECKPOINT")


def make(name: str, work: Path):
    work.mkdir(parents=True, exist_ok=True)
    if name == "strata":
        return Strata(work / "shop.strata")
    if name == "sqlite-full":
        return Sqlite(work / "shop-full.sqlite", "FULL")
    if name == "sqlite-normal":
        return Sqlite(work / "shop-normal.sqlite", "NORMAL")
    if name == "duckdb":
        return Duck(work / "shop.duckdb")
    raise SystemExit(f"unknown system {name}")


# ---------------------------------------------------------------- the queries

GET_PRODUCT = "SELECT id, title, brand, price, rating, stock FROM products WHERE id = :id"
USER_REVIEWS = "SELECT id, product_id, rating, ts FROM reviews WHERE user_id = :u ORDER BY ts DESC LIMIT 50"
PAGE = "SELECT id, title, price, rating FROM products WHERE category = :c AND price BETWEEN :lo AND :hi ORDER BY rating DESC, id LIMIT 20 OFFSET {off}"
AGG_BRAND = "SELECT brand, AVG(rating) AS avg_rating, COUNT(*) AS n FROM products WHERE brand IS NOT NULL GROUP BY brand HAVING COUNT(*) >= 20 ORDER BY n DESC, brand LIMIT 20"
AGG_MONTH = "SELECT month, COUNT(*) AS n, AVG(rating) AS avg_rating FROM reviews GROUP BY month ORDER BY month"
JOIN_TOP = "SELECT p.id, p.title, COUNT(*) AS n FROM reviews r JOIN products p ON r.product_id = p.id GROUP BY p.id, p.title ORDER BY n DESC, p.id LIMIT 10"
JOIN_USER = "SELECT p.id, p.title, r.rating, r.ts FROM reviews r JOIN products p ON p.id = r.product_id WHERE r.user_id = :u ORDER BY r.ts DESC, p.id LIMIT 50"
UPDATE_PRICE = "UPDATE products SET price = :p, stock = :s WHERE id = :id"
UPDATE_COUNTER = "UPDATE products SET rating_count = rating_count + 1 WHERE id = :id"
DELETE_RANGE = "DELETE FROM reviews WHERE ts BETWEEN :lo AND :hi"


def canon(rows: list) -> list:
    """Rows as lists, floats rounded, for comparing answers across systems."""
    return [[round(v, 6) if isinstance(v, float) else v for v in r] for r in rows]


def run(name: str, shop: Path, work: Path, seconds: float, limit_reviews: int | None) -> dict:
    meta = json.loads((shop / "meta.json").read_text()) if (shop / "meta.json").exists() else {"products": None, "reviews": None}
    s = make(name, work)
    for p in s.files():
        if p.exists():
            p.unlink()
    rss0 = current_rss()
    rep: dict[str, Any] = {"system": name, "version": s.version, "dataset": {"products": meta["products"], "reviews": meta["reviews"]}, "limit_reviews": limit_reviews}
    rng = random.Random(7)

    # load
    s.create()
    t0 = time.perf_counter()
    n_p = 0
    product_ids, cats, prices = [], {}, []
    for b in batches(shop / "products.jsonl", product_row):
        s.insert("products", b)
        n_p += len(b)
        product_ids += [r["id"] for r in b]
        for r in b:
            if r["category"] and r["price"] is not None:
                cats[r["category"]] = cats.get(r["category"], 0) + 1
                prices.append(r["price"])
    t_p = time.perf_counter() - t0
    t0 = time.perf_counter()
    n_r = 0
    users: dict[str, int] = {}
    ts_sample: list[int] = []
    for b in batches(shop / "reviews.jsonl", review_row, limit=limit_reviews):
        s.insert("reviews", b)
        n_r += len(b)
        for r in b:
            users[r["user_id"]] = users.get(r["user_id"], 0) + 1
        ts_sample += [r["ts"] for r in b[:: max(1, len(b) // 50)]]
    t_r = time.perf_counter() - t0
    t0 = time.perf_counter()
    s.optimize()
    t_o = time.perf_counter() - t0
    rep["load"] = {
        "products": {"rows": n_p, "seconds": t_p, "rows_per_s": n_p / t_p},
        "reviews": {"rows": n_r, "seconds": t_r, "rows_per_s": n_r / t_r},
        "finish_seconds": t_o,
        "bytes": files_bytes(s.files()),
        "rss_bytes": current_rss() - rss0,
    }
    print(f"{name}: loaded {n_p} products at {n_p / t_p:,.0f}/s, {n_r} reviews at {n_r / t_r:,.0f}/s, {rep['load']['bytes'] / 1e6:,.0f} MB", file=sys.stderr)

    # open
    s.close()
    pid = product_ids[len(product_ids) // 2]
    t0 = time.perf_counter()
    s.reopen()
    first = s.q(GET_PRODUCT, {"id": pid})
    rep["open"] = {"ms": (time.perf_counter() - t0) * 1e3, "found": len(first)}

    # get
    pids = rng.sample(product_ids, 2000)
    heavy = [u for u, n in users.items() if n >= 3]
    uids = rng.sample(heavy, min(2000, len(heavy)))
    rep["get"] = {
        "product": timed_passes(lambda k: s.q(GET_PRODUCT, {"id": k}), pids, seconds),
        "user_reviews": timed_passes(lambda u: s.q(USER_REVIEWS, {"u": u}), uids, seconds),
    }
    for t in (4,):
        rep["get"][f"product_{t}threads"] = timed_passes(lambda k: s.q(GET_PRODUCT, {"id": k}), pids, seconds, t)
    print(f"{name}: get p50 {rep['get']['product']['p50_ms'] * 1e3:.0f} µs, user reviews p50 {rep['get']['user_reviews']['p50_ms']:.3f} ms", file=sys.stderr)

    # answers, compared across systems by the analysis
    page_cat = max(cats, key=cats.get)
    prices.sort()
    lo, hi = prices[len(prices) // 10], prices[len(prices) * 9 // 10]
    rep["answers"] = {
        "get": canon(s.q(GET_PRODUCT, {"id": pids[0]})),
        "user_reviews": canon(s.q(USER_REVIEWS, {"u": uids[0]})),
        "page_0": canon(s.q(PAGE.format(off=0), {"c": page_cat, "lo": lo, "hi": hi})),
        "agg_brand": canon(s.q(AGG_BRAND)),
        "agg_month": canon(s.q(AGG_MONTH)),
        "join_top": canon(s.q(JOIN_TOP)),
        "join_user": canon(s.q(JOIN_USER, {"u": uids[0]})),
    }

    # page
    rep["page"] = {"category": page_cat, "price": [lo, hi]}
    for off in (0, 100, 1000, 10000):
        rep["page"][str(off)] = timed_passes(lambda _: s.q(PAGE.format(off=off), {"c": page_cat, "lo": lo, "hi": hi}), [None], seconds)

    # aggregate, join
    rep["aggregate"] = {"brand": timed_passes(lambda _: s.q(AGG_BRAND), [None], seconds), "month": timed_passes(lambda _: s.q(AGG_MONTH), [None], seconds)}
    rep["join"] = {"top": timed_passes(lambda _: s.q(JOIN_TOP), [None], seconds), "user": timed_passes(lambda u: s.q(JOIN_USER, {"u": u}), uids, seconds)}
    print(f"{name}: page@0 {rep['page']['0']['p50_ms']:.2f} ms, @10000 {rep['page']['10000']['p50_ms']:.2f} ms, brands {rep['aggregate']['brand']['p50_ms']:.1f} ms, months {rep['aggregate']['month']['p50_ms']:.1f} ms, join top {rep['join']['top']['p50_ms']:.1f} ms", file=sys.stderr)

    # insert
    def new_review(i: int) -> dict:
        return {"id": f"new{i}", "user_id": rng.choice(uids), "product_id": rng.choice(pids), "rating": 1 + i % 5, "title": "new", "text": "a review written during the benchmark", "ts": 1_800_000_000_000 + i, "month": 202701, "helpful": 0, "verified": 1}

    counter = iter(range(10**9))
    ins = timed_passes(lambda _: s.insert("reviews", [new_review(next(counter))]), list(range(200)), seconds)
    t0 = time.perf_counter()
    n_k = 0
    while n_k < 20_000 and time.perf_counter() - t0 < seconds * 2:
        s.insert("reviews", [new_review(next(counter)) for _ in range(1000)])
        n_k += 1000
    rep["insert"] = {"one_per_txn": {**ins, "rows_per_s": ins["n"] / (ins["n"] * ins["mean_ms"] / 1e3)}, "1000_per_txn": {"rows": n_k, "rows_per_s": n_k / (time.perf_counter() - t0)}}
    written = next(counter)
    rep["insert"]["visible"] = s.q("SELECT COUNT(*) FROM reviews WHERE ts >= 1800000000000")[0][0] == written
    print(f"{name}: insert one {rep['insert']['one_per_txn']['rows_per_s']:,.0f}/s (p50 {ins['p50_ms']:.3f} ms), 1000 per txn {rep['insert']['1000_per_txn']['rows_per_s']:,.0f}/s", file=sys.stderr)

    # update
    up = timed_passes(lambda k: s.x(UPDATE_PRICE, {"p": round(rng.uniform(5, 80), 2), "s": rng.randrange(0, 200), "id": k}), pids, seconds)
    cnt_id = pids[1]
    before = s.q("SELECT rating_count FROM products WHERE id = :id", {"id": cnt_id})[0][0]
    cn = timed_passes(lambda _: s.x(UPDATE_COUNTER, {"id": cnt_id}), [None], seconds)
    after = s.q("SELECT rating_count FROM products WHERE id = :id", {"id": cnt_id})[0][0]
    rep["update"] = {"price_stock": {**up, "rows_per_s": 1e3 / up["mean_ms"]}, "counter": {**cn, "rows_per_s": 1e3 / cn["mean_ms"], "counted_right": after - before == cn["n"]}}

    # delete
    keys = iter([f"new{i}" for i in range(written)])
    de = timed_passes(lambda _: s.delete_one("reviews", next(keys)), list(range(200)), min(seconds, 3))
    ts_sample.sort()
    step = max(1, len(ts_sample) * 1000 // max(1, n_r))
    ranges = [(ts_sample[i], ts_sample[i + step]) for i in range(0, len(ts_sample) - step, max(1, len(ts_sample) // 12))][:10]
    t_ranges, n_ranges = [], 0
    for a_, b_ in ranges:
        t0 = time.perf_counter()
        n_ranges += s.x(DELETE_RANGE, {"lo": a_, "hi": b_}) or 0
        t_ranges.append((time.perf_counter() - t0) * 1e3)
    rep["delete"] = {"by_id": {**de, "rows_per_s": 1e3 / de["mean_ms"]}, "by_range": {**summary(t_ranges), "rows": n_ranges, "ranges": len(ranges)}}
    print(f"{name}: update {rep['update']['price_stock']['rows_per_s']:,.0f}/s, counter {rep['update']['counter']['rows_per_s']:,.0f}/s, delete {rep['delete']['by_id']['rows_per_s']:,.0f}/s, range p50 {rep['delete']['by_range']['p50_ms']:.1f} ms", file=sys.stderr)

    # mixed: 4 readers, 1 writer
    stop = threading.Event()
    reads: list[list[float]] = [[] for _ in range(4)]
    writes: list[float] = []
    errors: list[str] = []

    def reader(t: int) -> None:
        r = random.Random(t)
        while not stop.is_set():
            op = r.random()
            t0 = time.perf_counter()
            try:
                if op < 0.5:
                    s.q(GET_PRODUCT, {"id": r.choice(pids)})
                elif op < 0.8:
                    s.q(USER_REVIEWS, {"u": r.choice(uids)})
                else:
                    s.q(PAGE.format(off=0), {"c": page_cat, "lo": lo, "hi": hi})
            except Exception as e:  # noqa: BLE001
                errors.append(repr(e)[:200])
            reads[t].append((time.perf_counter() - t0) * 1e3)

    def writer() -> None:
        r = random.Random(99)
        while not stop.is_set():
            t0 = time.perf_counter()
            try:
                s.x(UPDATE_PRICE, {"p": round(r.uniform(5, 80), 2), "s": r.randrange(0, 200), "id": r.choice(pids)})
            except Exception as e:  # noqa: BLE001
                errors.append(repr(e)[:200])
            writes.append((time.perf_counter() - t0) * 1e3)

    ths = [threading.Thread(target=reader, args=(t,)) for t in range(4)] + [threading.Thread(target=writer)]
    t0 = time.perf_counter()
    for t in ths:
        t.start()
    time.sleep(seconds * 2)
    stop.set()
    for t in ths:
        t.join()
    wall = time.perf_counter() - t0
    rep["mixed"] = {"reads": summary([x for l in reads for x in l], wall), "writes": summary(writes, wall), "errors": len(errors), "first_error": errors[0] if errors else None}
    print(f"{name}: mixed reads {rep['mixed']['reads']['qps']:,.0f}/s p99 {rep['mixed']['reads']['p99_ms']:.2f} ms, writes {rep['mixed']['writes']['qps']:,.0f}/s p99 {rep['mixed']['writes']['p99_ms']:.2f} ms, {len(errors)} errors", file=sys.stderr)

    # footprint after 10% of rows updated
    s.optimize()
    after_load = files_bytes(s.files())
    touched = rng.sample(product_ids, len(product_ids) // 10)
    t0 = time.perf_counter()
    for i in range(0, len(touched), 1000):
        chunk = touched[i : i + 1000]
        if name == "strata":
            # One transaction of 1,000 updates, as the others do.
            s.db.sql("UPDATE products SET stock = stock + 1 WHERE id IN (" + ", ".join(f"'{k}'" for k in chunk) + ")")
        elif name == "duckdb":
            s.con.execute("UPDATE products SET stock = stock + 1 WHERE id IN (SELECT unnest(?))", [chunk])
        else:
            c = s.con
            c.execute("BEGIN")
            c.executemany("UPDATE products SET stock = stock + 1 WHERE id = ?", [(k,) for k in chunk])
            c.execute("COMMIT")
    t_up = time.perf_counter() - t0
    s.optimize()
    rep["footprint"] = {"bytes_after_load": rep["load"]["bytes"], "bytes_after_writes": after_load, "bytes_after_10pct_updated": files_bytes(s.files()), "update_10pct_seconds": t_up, "rss_bytes": current_rss() - rss0}
    s.close()
    return rep


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--shop", required=True)
    ap.add_argument("--systems", default="strata,sqlite-full,sqlite-normal,duckdb")
    ap.add_argument("--work", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--seconds", type=float, default=5.0)
    ap.add_argument("--limit-reviews", type=int, default=None)
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--one", action="store_true", help=argparse.SUPPRESS)
    a = ap.parse_args()
    systems = a.systems.split(",")
    if len(systems) > 1 and not a.one:
        # Each system in a process of its own, so memory is its own.
        for name in systems:
            args = [sys.executable, __file__, "--shop", a.shop, "--systems", name, "--work", a.work, "--out", a.out, "--seconds", str(a.seconds), "--one"]
            if a.limit_reviews:
                args += ["--limit-reviews", str(a.limit_reviews)]
            if a.keep:
                args.append("--keep")
            subprocess.run(args, check=True)
        return
    name = systems[0]
    work = Path(a.work) / name
    if work.exists():
        shutil.rmtree(work)
    rep = run(name, Path(a.shop), work, a.seconds, a.limit_reviews)
    rep["params"] = {"seconds": a.seconds, "batch": BATCH}
    write_report(Path(a.out) / f"a1_{name}.json", rep)
    if not a.keep:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
