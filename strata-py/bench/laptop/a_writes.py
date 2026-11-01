"""Part A of the laptop benchmark, writes: inserts, updates and a stream of
events into one file that has text and vector indexes, Strata against
SQLite (FTS5 + sqlite-vec, at synchronous FULL and NORMAL), DuckDB (vss
HNSW; its fts index does not follow writes) and LanceDB.

    python a_writes.py --shop SHOP_DIR --work /tmp/aw --out RESULTS_DIR [--systems ...]

The file starts with the Shop's products (title and description searched,
a 384-number vector, price, stock, a counter), then:

  insert      1 product per durable transaction, then 100, 1,000 and 10,000 per transaction
  update      price and stock by id; a counter + 1; the title (re-indexed); the vector;
              an upsert of the whole row; a delete
  stream      review events (real review texts) appended at 1,000, 10,000 and 50,000 a
              second, then flat out, in a transaction per 50 ms, while 4 threads query
              (a product's latest reviews, and a word in the reviews' text)

For each: rows per second, p50 and p99 per transaction, and whether a query
straight after the commit sees the write (by its new word for a title, by
nearest neighbour for a vector, gone for a delete); when it does not, how
long until it does. A stream phase reports the rate it kept, the lag from
an event's creation to its commit, and the readers' latency. A phase that
falls behind its rate by more than 10% ends the stream (the next rates
would too).

Each system runs in its own process; RESULTS_DIR/writes_<system>.json.
"""

from __future__ import annotations

import argparse
import json
import random
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from common import files_bytes, summary, timed_once, timed_passes, write_report  # noqa: E402

DIM = 384
TICK = 0.05


def marker(i: int) -> str:
    """A word no product has: 'zqx' and i in consonants."""
    c = "bcdfghjklm"
    return "zqx" + "".join(c[int(d)] for d in str(i)) + "q"


def load_products(shop: Path, limit: int | None) -> tuple[list[dict], np.ndarray]:
    rows = []
    for line in open(shop / "products.jsonl"):
        p = json.loads(line)
        rows.append({"id": p["id"], "title": p["title"], "description": p["description"][:2000], "brand": p["brand"], "category": p["category"], "price": p["price"], "stock": p["stock"], "rating_count": p["rating_count"]})
        if limit and len(rows) >= limit:
            break
    vf = shop / "product_vecs.f32"
    if vf.exists() and vf.stat().st_size >= len(rows) * DIM * 4:
        vecs = np.fromfile(vf, dtype=np.float32, count=len(rows) * DIM).reshape(len(rows), DIM)
    else:  # harness check before the vectors exist
        vecs = np.random.default_rng(0).standard_normal((len(rows), DIM)).astype(np.float32)
        vecs /= np.linalg.norm(vecs, axis=1, keepdims=True)
    return rows, vecs


def load_events(shop: Path, n: int) -> list[dict]:
    out = []
    for line in open(shop / "reviews.jsonl"):
        r = json.loads(line)
        if r["text"]:
            out.append({"user_id": r["user_id"], "product_id": r["product_id"], "rating": r["rating"], "text": r["text"][:1000]})
        if len(out) >= n:
            break
    return out


# ---------------------------------------------------------------- the systems
#
# Each has: create(rows, vecs); insert(rows, vecs) in one transaction;
# set(id, **cols); incr(id); vector(id, v); upsert(row, v); delete(id);
# by_word(word) -> ids; nearest(v) -> id; get(id) -> row or None;
# events: ev_create(seed); ev_insert(rows); ev_latest(product); ev_word(word).


class StrataW:
    name = "strata"

    def __init__(self, work: Path) -> None:
        import strata

        self.strata, self.path = strata, work / "w.strata"

    def files(self):
        return [self.path, Path(str(self.path) + "-wal")]

    def create(self, rows, vecs) -> None:
        self.db = self.strata.open(
            str(self.path),
            schema={
                "table": "products",
                "key": "id",
                "fields": [
                    {"name": "id", "type": "keyword"},
                    {"name": "title", "type": "text", "indexed": False, "positions": False},
                    {"name": "description", "type": "text", "indexed": False, "positions": False},
                    {"name": "body", "type": "text", "from": ["title", "description"]},
                    {"name": "brand", "type": "keyword"},
                    {"name": "category", "type": "keyword"},
                    {"name": "price", "type": "float"},
                    {"name": "stock", "type": "int"},
                    {"name": "rating_count", "type": "int"},
                    {"name": "emb", "type": "vector", "dim": DIM},
                ],
            },
        )
        for i in range(0, len(rows), 10_000):
            self.insert(rows[i : i + 10_000], vecs[i : i + 10_000])
        self.db.optimize()

    def insert(self, rows, vecs) -> None:
        self.db.insert([{**r, "emb": v} for r, v in zip(rows, vecs)])

    def set(self, key, **cols) -> None:
        self.db.sql("UPDATE products SET " + ", ".join(f"{k} = :{k}" for k in cols) + " WHERE id = :id", {**cols, "id": key})

    def incr(self, key) -> None:
        self.db.sql("UPDATE products SET rating_count = rating_count + 1 WHERE id = :id", {"id": key})

    def vector(self, key, v) -> None:
        self.db.sql("UPDATE products SET emb = :v WHERE id = :id", {"v": v, "id": key})

    def upsert(self, row, v) -> None:
        self.db.upsert({**row, "emb": v})

    def delete(self, key) -> None:
        self.db.delete(key)

    def by_word(self, w):
        return list(self.db.query("SELECT id FROM products WHERE MATCH(body, :w) LIMIT 10", {"w": w}).keys)

    def nearest(self, v):
        r = self.db.query("SELECT id FROM products ORDER BY similarity(emb, :v) DESC LIMIT 1", {"v": v}).keys
        return r[0] if r else None

    def get(self, key):
        r = self.db.query("SELECT id, title, price, stock, rating_count FROM products WHERE id = :id", {"id": key}).rows
        return r[0] if r else None

    def ev_create(self, seed) -> None:
        self.db.sql("CREATE TABLE events (id KEYWORD PRIMARY KEY, user_id KEYWORD, product_id KEYWORD, rating INT, text TEXT OPTIONS(positions = false), ts INT)")
        self.ev_insert(seed)

    def ev_insert(self, rows) -> None:
        self.db.insert(rows, table="events")

    def ev_latest(self, p):
        return self.db.query("SELECT id FROM events WHERE product_id = :p ORDER BY ts DESC LIMIT 10", {"p": p}).rows

    def ev_word(self, w):
        return self.db.query("SELECT id FROM events ORDER BY bm25(text, :w, syntax => 'plain') DESC LIMIT 10", {"w": w}).rows

    def ev_count(self) -> int:
        return self.db.query("SELECT COUNT(*) FROM events").rows[0][0]

    def close(self) -> None:
        self.db.close()


class SqliteW:
    def __init__(self, work: Path, sync: str) -> None:
        import sqlite3

        import sqlite_vec

        self.sqlite3, self.sqlite_vec, self.sync = sqlite3, sqlite_vec, sync
        self.name = f"sqlite-{sync.lower()}"
        self.path = work / f"w-{sync.lower()}.sqlite"
        self.local = threading.local()
        self.lock = threading.Lock()  # one writer connection

    def files(self):
        return [self.path, Path(str(self.path) + "-wal"), Path(str(self.path) + "-shm")]

    def connect(self):
        c = self.sqlite3.connect(str(self.path), isolation_level=None, check_same_thread=False)
        c.enable_load_extension(True)
        self.sqlite_vec.load(c)
        c.enable_load_extension(False)
        c.execute("PRAGMA journal_mode = WAL")
        c.execute(f"PRAGMA synchronous = {self.sync}")
        if self.sync == "FULL":
            c.execute("PRAGMA fullfsync = ON")
            c.execute("PRAGMA checkpoint_fullfsync = ON")
        c.execute("PRAGMA cache_size = -262144")
        c.execute("PRAGMA busy_timeout = 10000")
        return c

    @property
    def con(self):
        c = getattr(self.local, "con", None)
        if c is None:
            c = self.local.con = self.connect()
        return c

    def tx(self, fn) -> None:
        c = self.con
        c.execute("BEGIN IMMEDIATE")
        try:
            fn(c)
            c.execute("COMMIT")
        except BaseException:
            c.execute("ROLLBACK")
            raise

    def create(self, rows, vecs) -> None:
        self.con.executescript(
            f"""
            CREATE TABLE products (rowid INTEGER PRIMARY KEY, id TEXT UNIQUE, title TEXT, description TEXT, brand TEXT, category TEXT,
                                   price REAL, stock INT, rating_count INT);
            CREATE VIRTUAL TABLE products_fts USING fts5(body, content='', contentless_delete=1, tokenize='porter unicode61 remove_diacritics 2');
            CREATE VIRTUAL TABLE products_vec USING vec0(emb float[{DIM}] distance_metric=cosine);
            """
        )
        self.next = 1
        for i in range(0, len(rows), 10_000):
            self.insert(rows[i : i + 10_000], vecs[i : i + 10_000])

    def _ins(self, c, rows, vecs) -> None:
        start = self.next
        self.next += len(rows)
        c.executemany(
            "INSERT INTO products VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            [(start + i, r["id"], r["title"], r["description"], r["brand"], r["category"], r["price"], r["stock"], r["rating_count"]) for i, r in enumerate(rows)],
        )
        c.executemany("INSERT INTO products_fts (rowid, body) VALUES (?, ?)", [(start + i, r["title"] + " " + r["description"]) for i, r in enumerate(rows)])
        c.executemany("INSERT INTO products_vec (rowid, emb) VALUES (?, ?)", [(start + i, np.asarray(v, dtype=np.float32).tobytes()) for i, v in enumerate(vecs)])

    def insert(self, rows, vecs) -> None:
        with self.lock:
            self.tx(lambda c: self._ins(c, rows, vecs))

    def _rowid(self, c, key):
        r = c.execute("SELECT rowid, title, description FROM products WHERE id = ?", (key,)).fetchone()
        return r

    def set(self, key, **cols) -> None:
        def f(c):
            c.execute("UPDATE products SET " + ", ".join(f"{k} = ?" for k in cols) + " WHERE id = ?", (*cols.values(), key))
            if "title" in cols or "description" in cols:
                rid, t, d = self._rowid(c, key)
                c.execute("DELETE FROM products_fts WHERE rowid = ?", (rid,))
                c.execute("INSERT INTO products_fts (rowid, body) VALUES (?, ?)", (rid, t + " " + d))

        with self.lock:
            self.tx(f)

    def incr(self, key) -> None:
        with self.lock:
            self.con.execute("UPDATE products SET rating_count = rating_count + 1 WHERE id = ?", (key,))

    def vector(self, key, v) -> None:
        def f(c):
            rid = self._rowid(c, key)[0]
            c.execute("UPDATE products_vec SET emb = ? WHERE rowid = ?", (np.asarray(v, dtype=np.float32).tobytes(), rid))

        with self.lock:
            self.tx(f)

    def upsert(self, row, v) -> None:
        def f(c):
            r = self._rowid(c, row["id"])
            if r is not None:
                rid = r[0]
                c.execute("DELETE FROM products WHERE rowid = ?", (rid,))
                c.execute("DELETE FROM products_fts WHERE rowid = ?", (rid,))
                c.execute("DELETE FROM products_vec WHERE rowid = ?", (rid,))
            self._ins(c, [row], [v])

        with self.lock:
            self.tx(f)

    def delete(self, key) -> None:
        def f(c):
            rid = self._rowid(c, key)[0]
            for t in ("products", "products_fts", "products_vec"):
                c.execute(f"DELETE FROM {t} WHERE rowid = ?", (rid,))

        with self.lock:
            self.tx(f)

    def by_word(self, w):
        return [r[0] for r in self.con.execute("SELECT p.id FROM products_fts f JOIN products p ON p.rowid = f.rowid WHERE products_fts MATCH ? LIMIT 10", ('"' + w + '"',))]

    def nearest(self, v):
        r = self.con.execute("SELECT p.id FROM products_vec v JOIN products p ON p.rowid = v.rowid WHERE v.emb MATCH ? AND k = 1", (np.asarray(v, dtype=np.float32).tobytes(),)).fetchone()
        return r[0] if r else None

    def get(self, key):
        return self.con.execute("SELECT id, title, price, stock, rating_count FROM products WHERE id = ?", (key,)).fetchone()

    def ev_create(self, seed) -> None:
        self.con.executescript(
            """
            CREATE TABLE events (rowid INTEGER PRIMARY KEY, id TEXT UNIQUE, user_id TEXT, product_id TEXT, rating INT, text TEXT, ts INT);
            CREATE INDEX events_product_ts ON events(product_id, ts);
            CREATE VIRTUAL TABLE events_fts USING fts5(text, content='', tokenize='porter unicode61 remove_diacritics 2');
            """
        )
        self.ev_next = 1
        self.ev_insert(seed)

    def ev_insert(self, rows) -> None:
        def f(c):
            start = self.ev_next
            self.ev_next += len(rows)
            c.executemany("INSERT INTO events VALUES (?, ?, ?, ?, ?, ?, ?)", [(start + i, r["id"], r["user_id"], r["product_id"], r["rating"], r["text"], r["ts"]) for i, r in enumerate(rows)])
            c.executemany("INSERT INTO events_fts (rowid, text) VALUES (?, ?)", [(start + i, r["text"]) for i, r in enumerate(rows)])

        with self.lock:
            self.tx(f)

    def ev_latest(self, p):
        return self.con.execute("SELECT id FROM events WHERE product_id = ? ORDER BY ts DESC LIMIT 10", (p,)).fetchall()

    def ev_word(self, w):
        return self.con.execute("SELECT e.id FROM events_fts f JOIN events e ON e.rowid = f.rowid WHERE events_fts MATCH ? ORDER BY bm25(events_fts) LIMIT 10", ('"' + w + '"',)).fetchall()

    def ev_count(self) -> int:
        return self.con.execute("SELECT COUNT(*) FROM events").fetchone()[0]

    def close(self) -> None:
        pass


class DuckW:
    name = "duckdb"
    notes = {
        "text": "DuckDB's fts index does not follow writes (it is rebuilt with PRAGMA create_fts_index): searches by a new word use an ILIKE scan, as an app would",
    }

    def __init__(self, work: Path) -> None:
        import duckdb

        self.duckdb, self.path = duckdb, work / "w.duckdb"
        self.local = threading.local()

    def files(self):
        return [self.path, Path(str(self.path) + ".wal")]

    @property
    def con(self):
        c = getattr(self.local, "con", None)
        if c is None:
            c = self.local.con = self.root.cursor()
        return c

    def create(self, rows, vecs) -> None:
        self.root = self.duckdb.connect(str(self.path))
        self.root.execute("INSTALL vss; LOAD vss; INSTALL fts; LOAD fts; SET hnsw_enable_experimental_persistence = true")
        self.root.execute(
            f"CREATE TABLE products (id VARCHAR PRIMARY KEY, title VARCHAR, description VARCHAR, brand VARCHAR, category VARCHAR, price DOUBLE, stock INTEGER, rating_count INTEGER, emb FLOAT[{DIM}])"
        )
        for i in range(0, len(rows), 10_000):
            self.insert(rows[i : i + 10_000], vecs[i : i + 10_000])
        self.root.execute("CREATE INDEX products_emb ON products USING HNSW (emb) WITH (metric = 'cosine')")
        self.root.execute("PRAGMA create_fts_index('products', 'id', 'title', 'description', stemmer = 'english')")
        self.root.execute("CHECKPOINT")

    def insert(self, rows, vecs) -> None:
        import pyarrow as pa

        cols = ["id", "title", "description", "brand", "category", "price", "stock", "rating_count"]
        t = pa.table({**{k: [r[k] for r in rows] for k in cols}, "emb": pa.FixedSizeListArray.from_arrays(pa.array(np.asarray(vecs, dtype=np.float32).reshape(-1)), DIM)})
        c = self.con
        c.register("batch", t)
        c.execute("INSERT INTO products SELECT * FROM batch").fetchall()
        c.unregister("batch")

    def set(self, key, **cols) -> None:
        self.con.execute("UPDATE products SET " + ", ".join(f"{k} = ?" for k in cols) + " WHERE id = ?", [*cols.values(), key]).fetchall()

    def incr(self, key) -> None:
        self.con.execute("UPDATE products SET rating_count = rating_count + 1 WHERE id = ?", [key]).fetchall()

    def vector(self, key, v) -> None:
        self.con.execute(f"UPDATE products SET emb = ?::FLOAT[{DIM}] WHERE id = ?", [list(map(float, v)), key]).fetchall()

    def upsert(self, row, v) -> None:
        c = self.con
        c.execute("BEGIN").fetchall()
        c.execute("DELETE FROM products WHERE id = ?", [row["id"]]).fetchall()
        c.execute(
            f"INSERT INTO products VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?::FLOAT[{DIM}])",
            [row["id"], row["title"], row["description"], row["brand"], row["category"], row["price"], row["stock"], row["rating_count"], list(map(float, v))],
        ).fetchall()
        c.execute("COMMIT").fetchall()

    def delete(self, key) -> None:
        self.con.execute("DELETE FROM products WHERE id = ?", [key]).fetchall()

    def by_word(self, w):
        return [r[0] for r in self.con.execute("SELECT id FROM products WHERE title ILIKE ? OR description ILIKE ? LIMIT 10", [f"%{w}%", f"%{w}%"]).fetchall()]

    def nearest(self, v):
        r = self.con.execute(f"SELECT id FROM products ORDER BY array_cosine_distance(emb, ?::FLOAT[{DIM}]) LIMIT 1", [list(map(float, v))]).fetchall()
        return r[0][0] if r else None

    def get(self, key):
        r = self.con.execute("SELECT id, title, price, stock, rating_count FROM products WHERE id = ?", [key]).fetchall()
        return r[0] if r else None

    def ev_create(self, seed) -> None:
        self.root.execute("CREATE TABLE events (id VARCHAR PRIMARY KEY, user_id VARCHAR, product_id VARCHAR, rating INTEGER, text VARCHAR, ts BIGINT)")
        self.ev_insert(seed)

    def ev_insert(self, rows) -> None:
        import pyarrow as pa

        t = pa.table({k: [r[k] for r in rows] for k in ("id", "user_id", "product_id", "rating", "text", "ts")})
        c = self.con
        c.register("ev", t)
        c.execute("INSERT INTO events SELECT * FROM ev").fetchall()
        c.unregister("ev")

    def ev_latest(self, p):
        return self.con.execute("SELECT id FROM events WHERE product_id = ? ORDER BY ts DESC LIMIT 10", [p]).fetchall()

    def ev_word(self, w):
        return self.con.execute("SELECT id FROM events WHERE text ILIKE ? LIMIT 10", [f"%{w}%"]).fetchall()

    def ev_count(self) -> int:
        return self.con.execute("SELECT COUNT(*) FROM events").fetchall()[0][0]

    def close(self) -> None:
        self.root.close()


class LanceW:
    name = "lancedb"
    notes = {"layout": "every write adds a fragment and a version; nothing compacts during the run (LanceDB's optimize() would)"}

    def __init__(self, work: Path) -> None:
        import os

        os.environ.setdefault("LANCE_LOG", "error")
        import lancedb

        self.lancedb, self.path = lancedb, work / "w.lance"
        self.lock = threading.Lock()

    def files(self):
        return [self.path]

    def _arrow(self, rows, vecs):
        import pyarrow as pa

        cols = ["id", "title", "description", "brand", "category", "price", "stock", "rating_count"]
        return pa.table(
            {
                **{k: pa.array([r[k] for r in rows], type=pa.float64() if k == "price" else (pa.int64() if k in ("stock", "rating_count") else pa.string())) for k in cols},
                "vector": pa.FixedSizeListArray.from_arrays(pa.array(np.asarray(vecs, dtype=np.float32).reshape(-1)), DIM),
            }
        )

    def create(self, rows, vecs) -> None:
        from lancedb.index import FTS

        self.db = self.lancedb.connect(str(self.path))
        self.t = self.db.create_table("products", self._arrow(rows[:10_000], vecs[:10_000]), mode="overwrite")
        for i in range(10_000, len(rows), 10_000):
            self.t.add(self._arrow(rows[i : i + 10_000], vecs[i : i + 10_000]))
        self.t.create_index(metric="cosine", index_type="IVF_HNSW_SQ", vector_column_name="vector")
        self.t.create_index("title", config=FTS(with_position=False, stem=True, remove_stop_words=True))
        self.t.create_scalar_index("id")
        self.t.optimize()

    def insert(self, rows, vecs) -> None:
        with self.lock:
            self.t.add(self._arrow(rows, vecs))

    @staticmethod
    def _q(v):
        return "'" + str(v).replace("'", "''") + "'"

    def set(self, key, **cols) -> None:
        with self.lock:
            self.t.update(where=f"id = {self._q(key)}", values=cols)

    def incr(self, key) -> None:
        with self.lock:
            self.t.update(where=f"id = {self._q(key)}", values_sql={"rating_count": "rating_count + 1"})

    def vector(self, key, v) -> None:
        with self.lock:
            self.t.update(where=f"id = {self._q(key)}", values={"vector": list(map(float, v))})

    def upsert(self, row, v) -> None:
        with self.lock:
            self.t.merge_insert("id").when_matched_update_all().when_not_matched_insert_all().execute(self._arrow([row], [v]))

    def delete(self, key) -> None:
        with self.lock:
            self.t.delete(f"id = {self._q(key)}")

    def by_word(self, w):
        return self.t.search(w, query_type="fts", fts_columns="title").limit(10).select(["id"]).to_arrow()["id"].to_pylist()

    def nearest(self, v):
        r = self.t.search(list(map(float, v)), vector_column_name="vector").distance_type("cosine").limit(1).select(["id"]).to_arrow()["id"].to_pylist()
        return r[0] if r else None

    def get(self, key):
        r = self.t.search().where(f"id = {self._q(key)}").select(["id", "title", "price", "stock", "rating_count"]).limit(1).to_arrow().to_pylist()
        return tuple(r[0].values()) if r else None

    def ev_create(self, seed) -> None:
        import pyarrow as pa

        from lancedb.index import FTS

        self.ev = self.db.create_table("events", pa.Table.from_pylist(seed), mode="overwrite")
        self.ev.create_index("text", config=FTS(with_position=False, stem=True, remove_stop_words=True))
        self.ev.create_scalar_index("product_id")

    def ev_insert(self, rows) -> None:
        import pyarrow as pa

        with self.lock:
            self.ev.add(pa.Table.from_pylist(rows))

    def ev_latest(self, p):
        r = self.ev.search().where(f"product_id = {self._q(p)}").select(["id", "ts"]).limit(100_000).to_arrow()
        return r.sort_by([("ts", "descending")])["id"][:10].to_pylist()

    def ev_word(self, w):
        return self.ev.search(w, query_type="fts").limit(10).select(["id"]).to_arrow()["id"].to_pylist()

    def ev_count(self) -> int:
        return self.ev.count_rows()

    def close(self) -> None:
        pass


def make(name: str, work: Path):
    work.mkdir(parents=True, exist_ok=True)
    return {
        "strata": lambda: StrataW(work),
        "sqlite-full": lambda: SqliteW(work, "FULL"),
        "sqlite-normal": lambda: SqliteW(work, "NORMAL"),
        "duckdb": lambda: DuckW(work),
        "lancedb": lambda: LanceW(work),
    }[name]()


# ---------------------------------------------------------------- the run


def visible(check, timeout: float = 5.0) -> float | None:
    """0 when `check()` holds straight away, else seconds until it does (None: not within `timeout`)."""
    if check():
        return 0.0
    t0 = time.perf_counter()
    while time.perf_counter() - t0 < timeout:
        time.sleep(0.01)
        if check():
            return time.perf_counter() - t0
    return None


def vis_summary(lags: list[float | None]) -> dict:
    seen = [x for x in lags if x is not None]
    return {"checked": len(lags), "at_once": sum(1 for x in lags if x == 0.0), "later": sum(1 for x in lags if x), "never_within_5s": sum(1 for x in lags if x is None), "max_s": max(seen) if seen else None}


def run(name: str, shop: Path, work: Path, seconds: float, limit: int | None, rates: list[int], stream_seconds: float) -> dict:
    s = make(name, work)
    rows, vecs = load_products(shop, limit)
    rng = random.Random(3)
    rep: dict[str, Any] = {"system": name, "products": len(rows), "notes": getattr(s, "notes", {})}
    t0 = time.perf_counter()
    s.create(rows, vecs)
    rep["load"] = {"seconds": time.perf_counter() - t0, "bytes": files_bytes(s.files())}
    print(f"{name}: loaded {len(rows)} products with text and vector indexes in {rep['load']['seconds']:.1f} s", file=sys.stderr)

    nid = iter(range(10**9))

    def fresh(k: int):
        out_r, out_v = [], []
        for _ in range(k):
            i = next(nid)
            src = rows[rng.randrange(len(rows))]
            v = vecs[rng.randrange(len(rows))] + np.float32(0.05) * np.random.default_rng(i).standard_normal(DIM).astype(np.float32)
            out_r.append({**src, "id": f"n{i}", "title": src["title"] + " " + marker(i)})
            out_v.append(v / np.linalg.norm(v))
        return out_r, out_v

    # inserts
    ins: dict[str, Any] = {}
    one = timed_once(lambda _: s.insert(*fresh(1)), range(100_000), seconds)
    ins["1"] = {**one, "rows_per_s": 1e3 / one["mean_ms"]}
    for k in (100, 1000, 10000):
        times = []
        t_end = time.perf_counter() + seconds
        while len(times) < 3 or (time.perf_counter() < t_end and len(times) < 50):
            r, v = fresh(k)
            t0 = time.perf_counter()
            s.insert(r, v)
            times.append((time.perf_counter() - t0) * 1e3)
            if k == 10000 and len(times) >= 3:
                break
        ins[str(k)] = {**summary(times), "rows_per_s": k * len(times) / (sum(times) / 1e3)}
    lags = []
    for _ in range(20):
        r, v = fresh(1)
        s.insert(r, v)
        w = marker(int(r[0]["id"][1:]))
        lags.append(visible(lambda: r[0]["id"] in s.by_word(w) and s.nearest(v[0]) == r[0]["id"]))
    ins["visible"] = vis_summary(lags)
    rep["insert"] = ins
    print(f"{name}: insert 1/txn {ins['1']['rows_per_s']:,.0f}/s p50 {ins['1']['p50_ms']:.2f} ms; 100 {ins['100']['rows_per_s']:,.0f}/s; 1000 {ins['1000']['rows_per_s']:,.0f}/s; 10000 {ins['10000']['rows_per_s']:,.0f}/s; visible {ins['visible']}", file=sys.stderr)

    # updates
    keys = [r["id"] for r in rng.sample(rows, 3000)]
    up: dict[str, Any] = {}
    x = timed_passes(lambda k: s.set(k, price=round(rng.uniform(5, 80), 2), stock=rng.randrange(200)), keys, seconds)
    up["price_stock"] = {**x, "rows_per_s": 1e3 / x["mean_ms"]}
    ck = keys[0]
    before = s.get(ck)[4]
    x = timed_passes(lambda _: s.incr(ck), [None], seconds)
    up["counter"] = {**x, "rows_per_s": 1e3 / x["mean_ms"], "counted_right": s.get(ck)[4] - before == x["n"]}
    ti = iter(range(10**9))
    tkeys = keys[1:1000]
    x = timed_once(lambda k: s.set(k, title="retitled " + marker(10**8 + next(ti))), tkeys, seconds)
    up["title"] = {**x, "rows_per_s": 1e3 / x["mean_ms"]}
    lags = []
    for k in keys[1000:1020]:
        w = marker(10**8 + next(ti))
        s.set(k, title="retitled " + w)
        lags.append(visible(lambda: k in s.by_word(w)))
    up["title"]["visible"] = vis_summary(lags)
    vkeys = keys[1020:2000]
    x = timed_once(lambda k: s.vector(k, fresh_vec(rng)), vkeys, seconds)
    up["vector"] = {**x, "rows_per_s": 1e3 / x["mean_ms"]}
    lags = []
    for k in keys[2000:2020]:
        v = fresh_vec(rng)
        s.vector(k, v)
        lags.append(visible(lambda: s.nearest(v) == k))
    up["vector"]["visible"] = vis_summary(lags)
    by_id = {r["id"]: r for r in rows}
    x = timed_once(lambda k: s.upsert({**by_id[k], "price": round(rng.uniform(5, 80), 2)}, fresh_vec(rng)), keys[2020:2500], seconds)
    up["upsert"] = {**x, "rows_per_s": 1e3 / x["mean_ms"]}
    x = timed_once(s.delete, keys[2500:2980], seconds)
    up["delete"] = {**x, "rows_per_s": 1e3 / x["mean_ms"]}
    lags = []
    for k in keys[2980:3000]:
        s.delete(k)
        lags.append(visible(lambda: s.get(k) is None))
    up["delete"]["visible"] = vis_summary(lags)
    rep["update"] = up
    errs = {k: v.get("first_error") for k, v in {**{f"insert {k}": v for k, v in ins.items() if isinstance(v, dict)}, **up}.items() if v.get("errors")}
    if errs:
        print(f"{name}: ERRORS {errs}", file=sys.stderr)
    print(
        f"{name}: price {up['price_stock']['rows_per_s']:,.0f}/s, counter {up['counter']['rows_per_s']:,.0f}/s (right {up['counter']['counted_right']}), title {up['title']['rows_per_s']:,.0f}/s, vector {up['vector']['rows_per_s']:,.0f}/s, upsert {up['upsert']['rows_per_s']:,.0f}/s, delete {up['delete']['rows_per_s']:,.0f}/s",
        file=sys.stderr,
    )

    # stream
    events = load_events(shop, 600_000)
    words = [w for w in "great love broken fun graphics controller battery story refund gift".split()]
    seq = iter(range(10**12))

    def ev_rows(k: int) -> list[dict]:
        now = int(time.time() * 1000)
        return [{**events[(i := next(seq)) % len(events)], "id": f"e{i}", "ts": now} for _ in range(k)]

    s.ev_create(ev_rows(10_000))
    pids = [r["id"] for r in rows[:20000]]
    phases = []
    for rate in [*rates, 0]:
        stop = threading.Event()
        reads: list[list[float]] = [[] for _ in range(4)]
        errors: list[str] = []

        def reader(t: int) -> None:
            r = random.Random(t)
            while not stop.is_set():
                t0 = time.perf_counter()
                try:
                    if r.random() < 0.5:
                        s.ev_latest(r.choice(pids))
                    else:
                        s.ev_word(r.choice(words))
                except Exception as e:  # noqa: BLE001
                    errors.append(repr(e)[:200])
                reads[t].append((time.perf_counter() - t0) * 1e3)

        ths = [threading.Thread(target=reader, args=(t,)) for t in range(4)]
        for t in ths:
            t.start()
        commits, lagv, written = [], [], 0
        t_start = time.perf_counter()
        behind = False
        while time.perf_counter() - t_start < stream_seconds:
            tick0 = time.perf_counter()
            k = int(rate * TICK) if rate else 5000
            batch = ev_rows(k)
            created = time.time()
            s.ev_insert(batch)
            done = time.time()
            commits.append((time.perf_counter() - tick0) * 1e3)
            lagv.append((done - created) * 1e3)
            written += k
            if rate:
                due = t_start + (len(commits)) * TICK
                if (slack := due - time.perf_counter()) > 0:
                    time.sleep(slack)
                else:
                    lagv[-1] += -slack * 1e3  # events created on schedule wait for the writer
        wall = time.perf_counter() - t_start
        stop.set()
        for t in ths:
            t.join()
        got = written / wall
        ph = {"rate": rate or "flat out", "seconds": wall, "written": written, "events_per_s": got, "commit": summary(commits), "lag": summary(lagv), "reads": summary([x for l in reads for x in l], wall), "read_errors": len(errors), "first_error": errors[0] if errors else None}
        phases.append(ph)
        print(f"{name}: stream {ph['rate']}/s -> {got:,.0f}/s, commit p50 {ph['commit']['p50_ms']:.1f} ms, lag p99 {ph['lag']['p99_ms']:.0f} ms, reads {ph['reads']['qps']:,.0f}/s p99 {ph['reads']['p99_ms']:.1f} ms", file=sys.stderr)
        if rate and got < 0.9 * rate:
            behind = True
            break
    rep["stream"] = {"phases": phases, "fell_behind": behind, "events_total": s.ev_count(), "tick_s": TICK}
    rep["bytes_end"] = files_bytes(s.files())
    s.close()
    return rep


def fresh_vec(rng: random.Random) -> np.ndarray:
    v = np.random.default_rng(rng.randrange(2**32)).standard_normal(DIM).astype(np.float32)
    return v / np.linalg.norm(v)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--shop", required=True)
    ap.add_argument("--systems", default="strata,sqlite-full,sqlite-normal,duckdb,lancedb")
    ap.add_argument("--work", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--seconds", type=float, default=5.0)
    ap.add_argument("--stream-seconds", type=float, default=15.0)
    ap.add_argument("--rates", default="1000,10000,50000")
    ap.add_argument("--limit-products", type=int, default=None)
    ap.add_argument("--one", action="store_true", help=argparse.SUPPRESS)
    a = ap.parse_args()
    systems = a.systems.split(",")
    if len(systems) > 1 and not a.one:
        for name in systems:
            args = [sys.executable, __file__, "--shop", a.shop, "--systems", name, "--work", a.work, "--out", a.out, "--seconds", str(a.seconds), "--stream-seconds", str(a.stream_seconds), "--rates", a.rates, "--one"]
            if a.limit_products:
                args += ["--limit-products", str(a.limit_products)]
            subprocess.run(args, check=False)
        return
    name = systems[0]
    work = Path(a.work) / name
    if work.exists():
        shutil.rmtree(work)
    rep = run(name, Path(a.shop), work, a.seconds, a.limit_products, [int(x) for x in a.rates.split(",") if x], a.stream_seconds)
    rep["params"] = {"seconds": a.seconds, "stream_seconds": a.stream_seconds}
    write_report(Path(a.out) / f"writes_{name}.json", rep)
    shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
