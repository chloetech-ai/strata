"""Part A2 of the laptop benchmark: full text, vector and hybrid search in
one file, Strata against SQLite (FTS5 and sqlite-vec), Tantivy, DuckDB (fts
and vss) and LanceDB, each driven from Python as an app would use it.

    python a2_search.py --data $STRATA_DATA --collections scifact,quora \\
        --workloads ../../../strata/results/workloads --work /tmp/a2 --out RESULTS_DIR

The query kinds are the shared workload's (strata/results/workloads/<name>.json,
which `strata-baselines --workload-only` writes: the same queries, filters and
words that strata-eval and strata-baselines time from Rust), plus phrase,
prefix, typo and a weighted hybrid made from the collection's queries.

Each kind is answered natively, by "app code" (the app fuses or filters
two results itself, as it would have to), or is "not supported", with the
reason. Every system gets the same documents and vectors; analysis is each
system's closest to Strata's (English stemming and stopwords). Per system
and collection it writes RESULTS_DIR/a2_<system>_<collection>.json: build
time and size, and per kind its support, warm latency at 1 and 4 threads,
nDCG@10 against the judgments and, for the vector kinds, recall@10 against
an exact search.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import sys
import time
from pathlib import Path
from typing import Any

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
from common import (  # noqa: E402
    CLASSIC_STOPWORDS,
    Collection,
    Unsupported,
    body,
    files_bytes,
    kept,
    ndcg10,
    phrase_of,
    prefix_of,
    rrf,
    timed_passes,
    typo_of,
    typo_word_of,
    weighted,
    write_report,
)

DEPTH = 100  # candidates each side of a hybrid
KEYWORDS = ("category", "topic")  # the other filter fields (year, rating) are ranges


def filter_parts(flt: list[dict] | None) -> list[tuple[str, Any]]:
    """[("year", (lo, hi)), ("category", [values]), ...]"""
    out = []
    for f in flt or []:
        for k, v in f.items():
            out.append((k, tuple(v) if k in ("year", "rating") else list(v)))
    return out


def mask_of(coll: Collection, flt: list[dict] | None) -> np.ndarray:
    m = np.ones(len(coll.docs), dtype=bool)
    for k, v in filter_parts(flt):
        if k in KEYWORDS:
            vals = set(v)
            m &= np.array([d[k] in vals for d in coll.docs])
        else:
            col = np.array([d[k] for d in coll.docs])
            m &= (col >= v[0]) & (col <= v[1])
    return m


# ---------------------------------------------------------------- the queries


def make_queries(coll: Collection, workload: dict) -> dict[str, list[dict]]:
    """The workload's queries by kind, each with what every system needs."""
    by: dict[str, list[dict]] = {}
    for w in workload["queries"]:
        q = {"kind": w["kind"], "filter": w.get("filter"), "words": w.get("words")}
        if "query" in w:
            qq = coll.queries[w["query"]]
            q.update(text=qq["text"], qid=qq["id"], vec=coll.qvecs[w["query"]])
        by.setdefault(w["kind"], []).append(q)
    # The extra kinds, from the first queries that make one.
    extra = {"phrase": [], "prefix": [], "typo": [], "typo_word": [], "hybrid_weighted": []}
    for i, qq in enumerate(coll.queries[:300]):
        base = {"text": qq["text"], "qid": qq["id"], "vec": coll.qvecs[i], "filter": None, "words": None}
        if (p := phrase_of(qq["text"])) is not None:
            extra["phrase"].append({**base, "kind": "phrase", "phrase": p})
        if (p := prefix_of(qq["text"])) is not None:
            extra["prefix"].append({**base, "kind": "prefix", "before": p[0], "pre": p[1]})
        if (t := typo_of(qq["text"])) is not None:
            extra["typo"].append({**base, "kind": "typo", "typo": t})
        if (t := typo_word_of(qq["text"])) is not None:
            extra["typo_word"].append({**base, "kind": "typo_word", "typo": t[1], "right": t[0]})
        extra["hybrid_weighted"].append({**base, "kind": "hybrid_weighted"})
    by.update(extra)
    return by


JUDGED = {"bm25", "vector", "hybrid", "hybrid_weighted", "phrase", "prefix", "typo"}
EXACT = {"vector", "filtered_vector"}


# ---------------------------------------------------------------- the systems


class System:
    name = ""
    version = ""
    # kind -> how it is answered when not natively, e.g. "app code: ..."
    how: dict[str, str] = {}

    def build(self, coll: Collection, path: Path) -> dict:
        raise NotImplementedError

    def answer(self, kind: str, q: dict) -> Any:
        raise NotImplementedError

    def files(self, path: Path) -> list[Path]:
        return [path]

    def close(self) -> None:
        pass


class StrataSys(System):
    name = "strata"

    def __init__(self) -> None:
        import strata

        self.strata = strata
        self.version = getattr(strata, "__version__", "")
        self.db = None

    def build(self, coll: Collection, path: Path) -> dict:
        schema = {
            "table": "docs",
            "key": "id",
            "fields": [
                {"name": "id", "type": "keyword"},
                {"name": "title", "type": "text", "indexed": False, "positions": False},
                {"name": "text", "type": "text", "indexed": False, "positions": False},
                {"name": "body", "type": "text", "from": ["title", "text"]},
                {"name": "year", "type": "int"},
                {"name": "category", "type": "keyword"},
                {"name": "rating", "type": "float"},
                {"name": "topic", "type": "keyword"},
                {"name": "emb", "type": "vector", "dim": coll.dim},
            ],
        }
        t0 = time.perf_counter()
        db = self.strata.open(str(path), schema=schema)
        for start in range(0, len(coll.docs), 2000):
            rows = []
            for i in range(start, min(start + 2000, len(coll.docs))):
                d = coll.docs[i]
                rows.append({**{k: d[k] for k in ("id", "title", "text", "year", "category", "rating", "topic")}, "emb": coll.vecs[i].tolist()})
            db.insert(rows)
        db.optimize()
        self.db = db
        return {"seconds": time.perf_counter() - t0}

    def files(self, path: Path) -> list[Path]:
        return [path, Path(str(path) + "-wal")]

    def _keys(self, sql: str, params: dict | None = None) -> list[str]:
        return list(self.db.query(sql, params or {}).keys or [])

    @staticmethod
    def _where(flt: list[dict] | None) -> str:
        parts = []
        for k, v in filter_parts(flt):
            if k in KEYWORDS:
                parts.append(f"{k} IN (" + ", ".join("'" + x.replace("'", "''") + "'" for x in v) + ")")
            else:
                parts.append(f"{k} BETWEEN {v[0]} AND {v[1]}")
        return ("WHERE " + " AND ".join(parts)) if parts else ""

    def answer(self, kind: str, q: dict) -> Any:
        w = self._where(q.get("filter"))
        db = self.db
        if kind in ("bm25", "filtered_bm25"):
            return self._keys(f"SELECT id FROM docs {w} ORDER BY bm25(body, :t) DESC LIMIT 10", {"t": q["text"]})
        if kind in ("and2", "and3", "or3"):
            op = " OR " if kind == "or3" else " AND "
            return self._keys("SELECT id FROM docs WHERE MATCH(body, :m) LIMIT 10", {"m": op.join(q["words"])})
        if kind == "filter":
            return self._keys(f"SELECT id FROM docs {w} ORDER BY rating DESC, id LIMIT 10")
        if kind in ("vector", "filtered_vector"):
            return self._keys(f"SELECT id FROM docs {w} ORDER BY similarity(emb, :v) DESC LIMIT 10", {"v": q["vec"].tolist()})
        if kind in ("hybrid", "filtered_hybrid"):
            return self._keys(
                f"SELECT id FROM docs {w} ORDER BY rrf(bm25(body, :t), similarity(emb, :v), depth => {DEPTH}) DESC LIMIT 10",
                {"t": q["text"], "v": q["vec"].tolist()},
            )
        if kind == "hybrid_weighted":
            return self._keys(
                f"SELECT id FROM docs ORDER BY 0.2 * minmax(bm25(body, :t), depth => {DEPTH}) + 0.8 * minmax(similarity(emb, :v), depth => {DEPTH}) DESC LIMIT 10",
                {"t": q["text"], "v": q["vec"].tolist()},
            )
        if kind == "facet":
            r = db.query("SELECT category, COUNT(*) FROM docs WHERE MATCH(body, :m) GROUP BY category ORDER BY 2 DESC, 1 LIMIT 5", {"m": " AND ".join(q["words"])})
            return [tuple(x) for x in r.rows]
        if kind == "phrase":
            a, b = q["phrase"]
            return self._keys("SELECT id FROM docs ORDER BY bm25(body, :p) DESC LIMIT 10", {"p": f'+"{a} {b}" {q["text"]}'.replace("'", " ")})
        if kind == "prefix":
            return self._keys("SELECT id FROM docs WHERE MATCH(body, :pre) ORDER BY bm25(body, :b, syntax => 'plain') DESC LIMIT 10", {"pre": q["pre"] + "*", "b": q["before"]})
        if kind in ("typo", "typo_word"):
            return self._keys("SELECT id FROM docs ORDER BY bm25(body, :x, syntax => 'plain', fuzzy => 'auto') DESC LIMIT 10", {"x": q["typo"]})
        raise Unsupported(kind)

    def close(self) -> None:
        if self.db is not None:
            self.db.close()


def fts5_quote(text: str) -> str:
    """The query's words as an FTS5 OR query, each quoted (FTS5's syntax has operators)."""
    return " OR ".join('"' + w + '"' for w in kept(text)) or '""'


class SqliteSys(System):
    """SQLite with FTS5 (porter unicode61) for text and sqlite-vec (an exact
    scan) for vectors; hybrids fused in the app."""

    name = "sqlite"
    how = {
        "hybrid": "app code: FTS5 and sqlite-vec top 100 each, fused by RRF in Python",
        "filtered_hybrid": "app code: both filtered, fused by RRF in Python",
        "hybrid_weighted": "app code: min-max weighted fusion in Python",
        "vector": "sqlite-vec: exact scan",
        "filtered_vector": "sqlite-vec: exact scan of the rows the filter keeps",
    }

    def __init__(self) -> None:
        import sqlite3

        import sqlite_vec

        self.sqlite3, self.sqlite_vec = sqlite3, sqlite_vec
        self.version = f"SQLite {sqlite3.sqlite_version}, sqlite-vec {getattr(sqlite_vec, '__version__', '')}"
        self.local = __import__("threading").local()
        self.path: Path | None = None

    def connect(self):
        c = self.sqlite3.connect(str(self.path), check_same_thread=False)
        c.enable_load_extension(True)
        self.sqlite_vec.load(c)
        c.enable_load_extension(False)
        c.execute("PRAGMA mmap_size = 1073741824")
        c.execute("PRAGMA cache_size = -262144")
        return c

    @property
    def con(self):
        c = getattr(self.local, "con", None)
        if c is None:
            c = self.local.con = self.connect()
        return c

    def build(self, coll: Collection, path: Path) -> dict:
        self.path = path
        t0 = time.perf_counter()
        c = self.connect()
        c.execute("PRAGMA journal_mode = WAL")
        c.executescript(
            f"""
            CREATE TABLE docs (rowid INTEGER PRIMARY KEY, id TEXT UNIQUE, title TEXT, text TEXT, year INT, category TEXT, rating REAL, topic TEXT);
            CREATE VIRTUAL TABLE fts USING fts5(body, content='', tokenize='porter unicode61 remove_diacritics 2');
            CREATE VIRTUAL TABLE vec USING vec0(emb float[{coll.dim}] distance_metric=cosine);
            """
        )
        with c:
            for i, d in enumerate(coll.docs):
                c.execute("INSERT INTO docs VALUES (?, ?, ?, ?, ?, ?, ?, ?)", (i + 1, d["id"], d["title"], d["text"], d["year"], d["category"], d["rating"], d["topic"]))
                c.execute("INSERT INTO fts (rowid, body) VALUES (?, ?)", (i + 1, body(d)))
                c.execute("INSERT INTO vec (rowid, emb) VALUES (?, ?)", (i + 1, coll.vecs[i].astype(np.float32).tobytes()))
        c.executescript("CREATE INDEX docs_year ON docs(year); CREATE INDEX docs_rating ON docs(rating); CREATE INDEX docs_category ON docs(category); CREATE INDEX docs_topic ON docs(topic);")
        c.execute("INSERT INTO fts(fts) VALUES ('optimize')")
        c.execute("ANALYZE")
        c.commit()
        c.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        c.close()
        return {"seconds": time.perf_counter() - t0}

    def files(self, path: Path) -> list[Path]:
        return [path, Path(str(path) + "-wal"), Path(str(path) + "-shm")]

    @staticmethod
    def _where(flt, alias="d") -> tuple[str, list]:
        parts, args = [], []
        for k, v in filter_parts(flt):
            if k in KEYWORDS:
                parts.append(f"{alias}.{k} IN ({', '.join('?' * len(v))})")
                args += v
            else:
                parts.append(f"{alias}.{k} BETWEEN ? AND ?")
                args += list(v)
        return " AND ".join(parts), args

    def _bm25(self, match: str, flt, k: int) -> list[tuple[str, float]]:
        w, args = self._where(flt)
        sql = f"SELECT d.id, bm25(fts) AS s FROM fts JOIN docs d ON d.rowid = fts.rowid WHERE fts MATCH ? {'AND ' + w if w else ''} ORDER BY s LIMIT {k}"
        return [(r[0], -r[1]) for r in self.con.execute(sql, [match, *args])]

    def _vec(self, v: np.ndarray, flt, k: int) -> list[tuple[str, float]]:
        blob = v.astype(np.float32).tobytes()
        if not flt:
            sql = f"SELECT d.id, v.distance FROM vec v JOIN docs d ON d.rowid = v.rowid WHERE v.emb MATCH ? AND k = {k} ORDER BY v.distance"
            return [(r[0], 1 - r[1]) for r in self.con.execute(sql, [blob])]
        # A filter: the rows it keeps, scored exactly.
        w, args = self._where(flt)
        sql = f"SELECT d.id, vec_distance_cosine(v.emb, ?) AS dist FROM docs d JOIN vec v ON v.rowid = d.rowid WHERE {w} ORDER BY dist LIMIT {k}"
        return [(r[0], 1 - r[1]) for r in self.con.execute(sql, [blob, *args])]

    def answer(self, kind: str, q: dict) -> Any:
        flt = q.get("filter")
        if kind in ("bm25", "filtered_bm25"):
            return [k for k, _ in self._bm25(fts5_quote(q["text"]), flt, 10)]
        if kind in ("and2", "and3", "or3"):
            op = " OR " if kind == "or3" else " AND "
            m = op.join('"' + w + '"' for w in q["words"])
            return [r[0] for r in self.con.execute("SELECT d.id FROM fts JOIN docs d ON d.rowid = fts.rowid WHERE fts MATCH ? LIMIT 10", [m])]
        if kind == "filter":
            w, args = self._where(flt)
            return [r[0] for r in self.con.execute(f"SELECT d.id FROM docs d WHERE {w} ORDER BY d.rating DESC, d.id LIMIT 10", args)]
        if kind in ("vector", "filtered_vector"):
            return [k for k, _ in self._vec(q["vec"], flt, 10)]
        if kind in ("hybrid", "filtered_hybrid"):
            return rrf([k for k, _ in self._bm25(fts5_quote(q["text"]), flt, DEPTH)], [k for k, _ in self._vec(q["vec"], flt, DEPTH)])[:10]
        if kind == "hybrid_weighted":
            return weighted(dict(self._bm25(fts5_quote(q["text"]), None, DEPTH)), dict(self._vec(q["vec"], None, DEPTH)))[:10]
        if kind == "facet":
            m = " AND ".join('"' + w + '"' for w in q["words"])
            sql = "SELECT d.category, COUNT(*) FROM fts JOIN docs d ON d.rowid = fts.rowid WHERE fts MATCH ? GROUP BY d.category ORDER BY 2 DESC, 1 LIMIT 5"
            return [tuple(r) for r in self.con.execute(sql, [m])]
        if kind == "phrase":
            a, b = q["phrase"]
            return [k for k, _ in self._bm25(f'"{a} {b}" AND ({fts5_quote(q["text"])})', None, 10)]
        if kind == "prefix":
            m = f'({fts5_quote(q["before"])}) AND {q["pre"]}*'
            return [k for k, _ in self._bm25(m, None, 10)]
        if kind in ("typo", "typo_word"):
            raise Unsupported("FTS5 has no typo-tolerant matching (the spellfix1 extension suggests words, it does not match them)")
        raise Unsupported(kind)


class TantivySys(System):
    """Tantivy (tantivy-py): text and filters, no vectors."""

    name = "tantivy"

    def __init__(self) -> None:
        import tantivy

        self.t = tantivy
        self.version = getattr(tantivy, "__version__", "")
        self.index = None

    def build(self, coll: Collection, path: Path) -> dict:
        t = self.t
        sb = t.SchemaBuilder()
        sb.add_text_field("id", stored=True, tokenizer_name="raw")
        sb.add_text_field("body", stored=False, tokenizer_name="en_stem", index_option="position")
        sb.add_integer_field("year", indexed=True, fast=True)
        sb.add_float_field("rating", indexed=True, fast=True)
        sb.add_text_field("category", stored=False, tokenizer_name="raw", fast=True)
        sb.add_text_field("topic", stored=False, tokenizer_name="raw", fast=True)
        self.schema = sb.build()
        path.mkdir(parents=True, exist_ok=True)
        t0 = time.perf_counter()
        index = t.Index(self.schema, path=str(path))
        w = index.writer(heap_size=512_000_000)
        for d in coll.docs:
            w.add_document(t.Document(id=d["id"], body=body(d), year=int(d["year"]), rating=float(d["rating"]), category=d["category"], topic=d["topic"]))
        w.commit()
        w.wait_merging_threads()
        index.reload()
        self.index = index
        self.searcher = index.searcher()
        return {"seconds": time.perf_counter() - t0}

    def _filter_query(self, flt) -> list:
        t = self.t
        qs = []
        for k, v in filter_parts(flt):
            if k in KEYWORDS:
                qs.append(t.Query.boolean_query([(t.Occur.Should, t.Query.term_query(self.schema, k, x)) for x in v]))
            elif k == "year":
                qs.append(t.Query.range_query(self.schema, "year", t.FieldType.Integer, int(v[0]), int(v[1])))
            else:
                qs.append(t.Query.range_query(self.schema, "rating", t.FieldType.Float, float(v[0]), float(v[1])))
        return qs

    def _ids(self, query, k=10) -> list[str]:
        hits = self.searcher.search(query, k).hits
        return [self.searcher.doc(addr)["id"][0] for _, addr in hits]

    def _text(self, text: str):
        ws = kept(text)
        return self.index.parse_query(" ".join(ws) if ws else "x", ["body"])

    def answer(self, kind: str, q: dict) -> Any:
        t = self.t
        if kind in ("bm25", "filtered_bm25"):
            fq = self._filter_query(q.get("filter"))
            query = self._text(q["text"])
            if fq:
                query = t.Query.boolean_query([(t.Occur.Must, query)] + [(t.Occur.Must, x) for x in fq])
            return self._ids(query)
        if kind in ("and2", "and3", "or3"):
            occ = t.Occur.Should if kind == "or3" else t.Occur.Must
            query = t.Query.boolean_query([(occ, self.index.parse_query(w, ["body"])) for w in q["words"]])
            return self._ids(query)
        if kind == "filter":
            fq = self._filter_query(q["filter"])
            query = t.Query.boolean_query([(t.Occur.Must, x) for x in fq])
            hits = self.searcher.search(query, 10, order_by_field="rating", order=t.Order.Desc).hits
            return [self.searcher.doc(addr)["id"][0] for _, addr in hits]
        if kind == "facet":
            raise Unsupported("tantivy-py has no terms aggregation over a query's matches (Tantivy's Rust API does)")
        if kind == "phrase":
            a, b = q["phrase"]
            query = t.Query.boolean_query([(t.Occur.Must, self.index.parse_query(f'"{a} {b}"', ["body"])), (t.Occur.Should, self._text(q["text"]))])
            return self._ids(query)
        if kind == "prefix":
            query = t.Query.boolean_query([(t.Occur.Must, t.Query.regex_query(self.schema, "body", re.escape(q["pre"]) + ".*")), (t.Occur.Should, self._text(q["before"]))])
            return self._ids(query)
        if kind in ("typo", "typo_word"):
            # The words as typed (BM25), and each word within its edit
            # distance (Tantivy scores a fuzzy match as a constant), as an
            # app would build it; distances as Elasticsearch's AUTO.
            parts = [(t.Occur.Should, self._text(q["typo"]))]
            for w in kept(q["typo"]):
                d = 0 if len(w) < 3 else (1 if len(w) < 6 else 2)
                parts.append((t.Occur.Should, t.Query.fuzzy_term_query(self.schema, "body", w, distance=d)))
            return self._ids(t.Query.boolean_query(parts))
        raise Unsupported("Tantivy has no vector search")


class DuckSys(System):
    """DuckDB with the fts extension (BM25) and vss (HNSW)."""

    name = "duckdb"
    how = {
        "hybrid": "app code: fts and vss top 100 each, fused by RRF in Python",
        "filtered_hybrid": "app code: both filtered, fused by RRF in Python",
        "hybrid_weighted": "app code: min-max weighted fusion in Python",
        "filtered_vector": "an exact scan of the rows the filter keeps (vss's HNSW index is not used with a WHERE)",
    }

    def __init__(self) -> None:
        import duckdb

        self.duckdb = duckdb
        self.version = duckdb.__version__
        self.local = __import__("threading").local()

    @property
    def con(self):
        c = getattr(self.local, "con", None)
        if c is None:
            c = self.local.con = self.root.cursor()
        return c

    def build(self, coll: Collection, path: Path) -> dict:
        import pyarrow as pa

        t0 = time.perf_counter()
        c = self.duckdb.connect(str(path))
        for e in ("fts", "vss"):
            c.execute(f"INSTALL {e}; LOAD {e}")
        c.execute("SET hnsw_enable_experimental_persistence = true")
        tbl = pa.table(
            {
                "id": [d["id"] for d in coll.docs],
                "body": [body(d) for d in coll.docs],
                "year": [d["year"] for d in coll.docs],
                "category": [d["category"] for d in coll.docs],
                "topic": [d["topic"] for d in coll.docs],
                "rating": [float(d["rating"]) for d in coll.docs],
                "emb": pa.FixedSizeListArray.from_arrays(pa.array(coll.vecs.reshape(-1), pa.float32()), coll.dim),
            }
        )
        c.register("src", tbl)
        c.execute("CREATE TABLE docs AS SELECT * FROM src")
        c.unregister("src")
        c.execute("PRAGMA create_fts_index('docs', 'id', 'body', stemmer = 'english', stopwords = 'english', lower = 1, strip_accents = 1)")
        c.execute("CREATE INDEX docs_emb ON docs USING HNSW (emb) WITH (metric = 'ip', M = 16, ef_construction = 200)")
        c.execute("CHECKPOINT")
        self.root = c
        self.dim = coll.dim
        return {"seconds": time.perf_counter() - t0}

    def files(self, path: Path) -> list[Path]:
        return [path, Path(str(path) + ".wal")]

    def _where(self, flt) -> tuple[str, list]:
        parts, args = [], []
        for k, v in filter_parts(flt):
            if k in KEYWORDS:
                parts.append(f"{k} IN ({', '.join('?' * len(v))})")
                args += v
            else:
                parts.append(f"{k} BETWEEN ? AND ?")
                args += list(v)
        return " AND ".join(parts), args

    def _bm25(self, text: str, flt, k: int, conjunctive: int = 0) -> list[tuple[str, float]]:
        w, args = self._where(flt)
        sql = f"""SELECT id, s FROM (SELECT id, fts_main_docs.match_bm25(id, ?, conjunctive := {conjunctive}) AS s, year, rating, category, topic FROM docs)
                  WHERE s IS NOT NULL {'AND ' + w if w else ''} ORDER BY s DESC LIMIT {k}"""
        return [tuple(r) for r in self.con.execute(sql, [text, *args]).fetchall()]

    def _vec(self, v: np.ndarray, flt, k: int) -> list[tuple[str, float]]:
        w, args = self._where(flt)
        sql = f"SELECT id, array_inner_product(emb, ?::FLOAT[{self.dim}]) AS s FROM docs {'WHERE ' + w if w else ''} ORDER BY array_negative_inner_product(emb, ?::FLOAT[{self.dim}]) LIMIT {k}"
        vl = v.tolist()
        return [tuple(r) for r in self.con.execute(sql, [vl, *args, vl]).fetchall()]

    def answer(self, kind: str, q: dict) -> Any:
        flt = q.get("filter")
        if kind in ("bm25", "filtered_bm25"):
            return [k for k, _ in self._bm25(q["text"], flt, 10)]
        if kind in ("and2", "and3"):
            return [k for k, _ in self._bm25(" ".join(q["words"]), None, 10, conjunctive=1)]
        if kind == "or3":
            return [k for k, _ in self._bm25(" ".join(q["words"]), None, 10)]
        if kind == "filter":
            w, args = self._where(flt)
            return [r[0] for r in self.con.execute(f"SELECT id FROM docs WHERE {w} ORDER BY rating DESC, id LIMIT 10", args).fetchall()]
        if kind in ("vector", "filtered_vector"):
            return [k for k, _ in self._vec(q["vec"], flt, 10)]
        if kind in ("hybrid", "filtered_hybrid"):
            return rrf([k for k, _ in self._bm25(q["text"], flt, DEPTH)], [k for k, _ in self._vec(q["vec"], flt, DEPTH)])[:10]
        if kind == "hybrid_weighted":
            return weighted(dict(self._bm25(q["text"], None, DEPTH)), dict(self._vec(q["vec"], None, DEPTH)))[:10]
        if kind == "facet":
            sql = """SELECT category, COUNT(*) FROM (SELECT category, fts_main_docs.match_bm25(id, ?, conjunctive := 1) AS s FROM docs)
                     WHERE s IS NOT NULL GROUP BY category ORDER BY 2 DESC, 1 LIMIT 5"""
            return [tuple(r) for r in self.con.execute(sql, [" ".join(q["words"])]).fetchall()]
        if kind == "phrase":
            raise Unsupported("DuckDB's fts extension has no phrase query (it keeps no positions)")
        if kind == "prefix":
            raise Unsupported("DuckDB's fts extension has no prefix query")
        if kind in ("typo", "typo_word"):
            raise Unsupported("DuckDB's fts extension has no typo-tolerant matching")
        raise Unsupported(kind)


class LanceSys(System):
    """LanceDB: its native full-text index (stemmed, stopwords, positions) and an IVF-HNSW-SQ vector index."""

    name = "lancedb"
    how = {
        "facet": "app code: the matches' categories counted in Python (LanceDB has no GROUP BY)",
        "filter": "app code: the rows the filter keeps, sorted in Python (LanceDB queries have no ORDER BY)",
        "hybrid": "LanceDB's hybrid query with its RRF reranker",
        "filtered_hybrid": "LanceDB's hybrid query with a prefilter and its RRF reranker",
        "hybrid_weighted": "LanceDB's hybrid query with its linear-combination reranker (weight 0.8 on the vector side)",
    }

    def __init__(self) -> None:
        import os

        os.environ.setdefault("LANCE_LOG", "error")
        import lancedb

        self.lancedb = lancedb
        self.version = lancedb.__version__

    def build(self, coll: Collection, path: Path) -> dict:
        import pyarrow as pa

        t0 = time.perf_counter()
        db = self.lancedb.connect(str(path))
        tbl = pa.table(
            {
                "id": [d["id"] for d in coll.docs],
                "body": [body(d) for d in coll.docs],
                "year": [d["year"] for d in coll.docs],
                "category": [d["category"] for d in coll.docs],
                "topic": [d["topic"] for d in coll.docs],
                "rating": [float(d["rating"]) for d in coll.docs],
                "vector": pa.FixedSizeListArray.from_arrays(pa.array(coll.vecs.reshape(-1), pa.float32()), coll.dim),
            }
        )
        t = db.create_table("docs", tbl, mode="overwrite")
        t.create_fts_index("body", with_position=True, stem=True, remove_stop_words=True, language="English", replace=True)
        if len(coll.docs) >= 10_000:
            t.create_index(metric="dot", index_type="IVF_HNSW_SQ", vector_column_name="vector", m=16, ef_construction=200)
        for col in ("year", "rating", "category", "topic"):
            t.create_scalar_index(col)
        self.t = t
        self.indexed = len(coll.docs) >= 10_000
        return {"seconds": time.perf_counter() - t0, "vector_index": "IVF_HNSW_SQ" if self.indexed else "none (fewer than 10,000 rows: an exact scan)"}

    @staticmethod
    def _where(flt) -> str | None:
        parts = []
        for k, v in filter_parts(flt):
            if k in KEYWORDS:
                parts.append(f"{k} IN (" + ", ".join("'" + x.replace("'", "''") + "'" for x in v) + ")")
            else:
                parts.append(f"{k} BETWEEN {v[0]} AND {v[1]}")
        return " AND ".join(parts) or None

    def _ids(self, q, k=10) -> list[str]:
        return q.limit(k).select(["id"]).to_arrow()["id"].to_pylist()

    def _fts(self, query, flt=None):
        from lancedb.query import MatchQuery

        s = self.t.search(query if not isinstance(query, str) else MatchQuery(query, "body"), query_type="fts")
        w = self._where(flt)
        return s.where(w, prefilter=True) if w else s

    def answer(self, kind: str, q: dict) -> Any:
        from lancedb.query import BooleanQuery, MatchQuery, Occur, PhraseQuery
        from lancedb.rerankers import LinearCombinationReranker, RRFReranker

        flt = q.get("filter")
        if kind in ("bm25", "filtered_bm25"):
            return self._ids(self._fts(" ".join(kept(q["text"])) or "x", flt))
        if kind in ("and2", "and3", "or3"):
            op = "OR" if kind == "or3" else "AND"
            return self._ids(self._fts(MatchQuery(" ".join(q["words"]), "body", operator=op)))
        if kind == "filter":
            rows = self.t.search().where(self._where(flt)).select(["id", "rating"]).limit(1_000_000).to_arrow()
            return rows.sort_by([("rating", "descending"), ("id", "ascending")])["id"][:10].to_pylist()
        if kind in ("vector", "filtered_vector"):
            s = self.t.search(q["vec"].tolist(), vector_column_name="vector").distance_type("dot")
            if self.indexed:
                s = s.ef(64)
            w = self._where(flt)
            if w:
                s = s.where(w, prefilter=True)
            return self._ids(s)
        if kind in ("hybrid", "filtered_hybrid", "hybrid_weighted"):
            s = (
                self.t.search(query_type="hybrid", vector_column_name="vector")
                .vector(q["vec"].tolist())
                .text(" ".join(kept(q["text"])) or "x")
                .distance_type("dot")
            )
            w = self._where(flt)
            if w:
                s = s.where(w, prefilter=True)
            s = s.rerank(RRFReranker(K=60) if kind != "hybrid_weighted" else LinearCombinationReranker(weight=0.8))
            return self._ids(s)
        if kind == "facet":
            ids = self._fts(MatchQuery(" ".join(q["words"]), "body", operator="AND")).select(["category"]).limit(1_000_000).to_arrow()["category"].to_pylist()
            counts: dict[str, int] = {}
            for c in ids:
                counts[c] = counts.get(c, 0) + 1
            return sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:5]
        if kind == "phrase":
            a, b = q["phrase"]
            query = BooleanQuery([(Occur.MUST, PhraseQuery(f"{a} {b}", "body")), (Occur.SHOULD, MatchQuery(" ".join(kept(q["text"])) or "x", "body"))])
            return self._ids(self._fts(query))
        if kind == "prefix":
            raise Unsupported("LanceDB's full-text index matches whole words; prefixes need a separate n-gram index")
        if kind in ("typo", "typo_word"):
            # The words as typed, and each word within its edit distance
            # (Elasticsearch's AUTO; LanceDB counts a transposition as 2).
            parts = [(Occur.SHOULD, MatchQuery(" ".join(kept(q["typo"])) or "x", "body"))]
            for w in kept(q["typo"]):
                d = 0 if len(w) < 3 else (1 if len(w) < 6 else 2)
                if d:
                    parts.append((Occur.SHOULD, MatchQuery(w, "body", fuzziness=d, max_expansions=50)))
            return self._ids(self._fts(BooleanQuery(parts)))
        raise Unsupported(kind)


SYSTEMS = {"strata": StrataSys, "sqlite": SqliteSys, "tantivy": TantivySys, "duckdb": DuckSys, "lancedb": LanceSys}


# ---------------------------------------------------------------- the run


def exact_truth(coll: Collection, qs: list[dict]) -> list[set[str]]:
    out = []
    for q in qs:
        m = mask_of(coll, q.get("filter")) if q.get("filter") else None
        s = coll.vecs @ q["vec"]
        if m is not None:
            s = np.where(m, s, -np.inf)
        top = np.argpartition(-s, 10)[:10]
        out.append({coll.docs[i]["id"] for i in top if np.isfinite(s[i])})
    return out


def run_system(name: str, coll: Collection, by_kind: dict[str, list[dict]], work: Path, seconds: float, threads: list[int], kinds: list[str] | None) -> dict:
    sys_ = SYSTEMS[name]()
    path = work / f"{name}_{coll.name}" / ("db" if name != "lancedb" else "lance")
    if path.parent.exists():
        shutil.rmtree(path.parent)
    path.parent.mkdir(parents=True)
    if name == "tantivy":
        path = path.parent / "index"
    built = sys_.build(coll, path)
    built["bytes"] = files_bytes(sys_.files(path)) if name not in ("lancedb", "tantivy") else files_bytes([path])
    print(f"{name:8} {coll.name}: built in {built['seconds']:.1f} s, {built['bytes'] / 1e6:.1f} MB", file=sys.stderr)
    report: dict[str, Any] = {"system": name, "version": sys_.version, "collection": coll.name, "n_docs": len(coll.docs), "build": built, "kinds": {}}
    for kind, qs in by_kind.items():
        if kinds and kind not in kinds:
            continue
        entry: dict[str, Any] = {"queries": len(qs)}
        try:
            answers = [sys_.answer(kind, q) for q in qs]
        except Unsupported as e:
            entry.update(support="not supported", why=str(e))
            if kind in ("typo", "typo_word"):
                try:
                    right = [sys_.answer("bm25", {"text": q.get("right", q["text"]), "filter": None}) for q in qs]
                    plain = [sys_.answer("bm25", {"text": q["typo"], "filter": None}) for q in qs]
                    entry["recovers10_without"] = sum(len(set(a) & set(r)) / max(1, len(r)) for a, r in zip(plain, right)) / len(qs)
                except Unsupported:
                    pass
            report["kinds"][kind] = entry
            print(f"{name:8} {kind:16} not supported: {e}", file=sys.stderr)
            continue
        entry["support"] = "app code" if sys_.how.get(kind, "").startswith("app code") else "native"
        if kind in sys_.how:
            entry["how"] = sys_.how[kind]
        for t in threads:
            entry[f"warm{t}"] = timed_passes(lambda q: sys_.answer(kind, q), qs, seconds, t)
        if kind in JUDGED:
            scores = [ndcg10(a, coll.qrels.get(q["qid"], {})) for a, q in zip(answers, qs)]
            scores = [s for s in scores if s is not None]
            entry["ndcg10"] = sum(scores) / len(scores) if scores else None
        if kind in EXACT:
            truth = exact_truth(coll, qs)
            entry["recall10"] = sum(len(set(a) & t) / max(1, len(t)) for a, t in zip(answers, truth)) / len(qs)
        if kind in ("typo", "typo_word"):
            # How much of the system's own answer to the query as written a
            # typo'd query gets back, with its typo tolerance and without.
            right = [sys_.answer("bm25", {"text": q.get("right", q["text"]), "filter": None}) for q in qs]
            plain = [sys_.answer("bm25", {"text": q["typo"], "filter": None}) for q in qs]
            entry["recovers10"] = sum(len(set(a) & set(r)) / max(1, len(r)) for a, r in zip(answers, right)) / len(qs)
            entry["recovers10_without"] = sum(len(set(a) & set(r)) / max(1, len(r)) for a, r in zip(plain, right)) / len(qs)
        if kind == "facet":
            entry["answers"] = [[list(r) for r in a] for a in answers]
        else:
            entry["answers"] = answers
        w1 = entry.get(f"warm{threads[0]}", {})
        print(
            f"{name:8} {kind:16} p50 {w1.get('p50_ms') or 0:8.3f} ms  p99 {w1.get('p99_ms') or 0:8.3f} ms"
            + (f"  nDCG {entry['ndcg10']:.3f}" if entry.get("ndcg10") is not None else "")
            + (f"  recall {entry['recall10']:.3f}" if "recall10" in entry else "")
            + (f"  recovers {entry['recovers10']:.3f} (without {entry['recovers10_without']:.3f})" if "recovers10" in entry else ""),
            file=sys.stderr,
        )
        report["kinds"][kind] = entry
    sys_.close()
    return report


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--data", required=True, help="BEIR collections as prepare_beir.py writes them ($STRATA_DATA)")
    ap.add_argument("--collections", default="scifact")
    ap.add_argument("--systems", default=",".join(SYSTEMS))
    ap.add_argument("--workloads", default=str(Path(__file__).resolve().parents[3] / "strata" / "results" / "workloads"))
    ap.add_argument("--kinds", default="", help="only these kinds, comma-separated")
    ap.add_argument("--work", required=True, help="where the databases are built (deleted at the end unless --keep)")
    ap.add_argument("--out", required=True)
    ap.add_argument("--seconds", type=float, default=5.0, help="timed seconds per kind and thread count")
    ap.add_argument("--threads", default="1,4")
    ap.add_argument("--keep", action="store_true")
    a = ap.parse_args()
    work = Path(a.work)
    threads = [int(x) for x in a.threads.split(",")]
    kinds = [k for k in a.kinds.split(",") if k] or None
    for name in a.collections.split(","):
        coll = Collection.load(a.data, name)
        wl = Path(a.workloads) / f"{name}.json"
        if not wl.exists():
            sys.exit(f"no workload {wl}: run strata-baselines --collections {name} --workload-only first")
        by_kind = make_queries(coll, json.loads(wl.read_text()))
        for s in a.systems.split(","):
            rep = run_system(s, coll, by_kind, work, a.seconds, threads, kinds)
            rep["params"] = {"seconds": a.seconds, "threads": threads, "hybrid_depth": DEPTH}
            write_report(Path(a.out) / f"a2_{s}_{name}.json", rep)
            if not a.keep:
                shutil.rmtree(work / f"{s}_{name}", ignore_errors=True)


if __name__ == "__main__":
    main()
