#!/usr/bin/env python3
"""Write docs/site/assets/bench-data.js, every number on the site's benchmark pages.

    python3 docs/site/scripts/bench_data.py            # from the repository root

Nothing here is typed by hand: each value is read from a run's committed
results and only rounded. The reports it reads:

  cloud-10m      strata-server/results/compare-msmarco-10m-do/  (fanout.sh, one pass, every engine on its own nodes)
  cloud-10m-h2h  strata-server/results/compare-msmarco-same-nodes-do/10m/  (Strata, Elasticsearch and
                 OpenSearch on the same nodes on latest main, three passes)
  cloud-1m       strata-server/results/compare-msmarco-same-nodes-do/1m/   (14 engines on latest main,
                 each lane's engines on the same nodes, three passes)
  cloud-1m-first strata-server/results/compare-msmarco-1m-do/   (the first 1M run, three passes)
  laptop     strata-server/results/compare-nq-mac/          (compare.sh on kind, one pass)
  embedded   strata/results/laptop-2026-11/part-a-do/       (Part A, no server)
  graph      strata-server/results/graph-compare-mac/README.md
  bm25-load  strata-server/results/bm25-stats-cache-container/
  bm25-do    strata-server/results/bm25-do-1m/ (the same before and after on DigitalOcean)
  strata-1m-since  Strata's own medians in cloud-1m-first and cloud-1m, kind by kind

A results directory that is not on this checkout is read from the git ref
named for it below (a branch whose pull request is still open), and the page
says so. A report missing from both is left out, and its page says it is
pending.

Medians: a cluster comparison's latency and throughput are the median of its
passes (deploy/compare/rank.py's rule), never the first pass alone. Places:
each query kind at three measures (p50 and p99 at 1 client, queries a second
at 4) and recall@10 for the three vector kinds; an engine that cannot run a
kind takes last place on it. Load time and size are left out of the ranking,
since Strata's shards are built offline from local files while the others
take their documents over HTTP.
"""
import gzip
import json
import re
import statistics
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / "docs/site/assets/bench-data.js"
GH = "https://github.com/chloetech-ai/strata/"

KINDS = [
    ("bm25", "BM25", "text"), ("bm25 + filter", "BM25 + filter", "text"), ("phrase", "Phrase", "text"),
    ("prefix", "Prefix", "text"), ("typo", "Typo-tolerant", "text"), ("boosted (bm25 × decay)", "BM25 × recency", "text"),
    ("facet", "Facet counts", "cols"), ("key lookup", "Key lookup", "cols"), ("sql aggregate", "SQL aggregate", "cols"),
    ("vector", "Vector", "vec"), ("vector + filter", "Vector + filter", "vec"), ("similar items", "Similar items", "vec"),
    ("hybrid (RRF)", "Hybrid, RRF", "hyb"), ("hybrid (weighted)", "Hybrid, weighted", "hyb"),
]
RECALL = ["vector", "vector + filter", "similar items"]
ENGINES = {
    "strata": ("Strata", ""), "es": ("Elasticsearch", "9.5.5"), "os": ("OpenSearch", "3.9.0"), "solr": ("Solr", "10.1.0"),
    "vespa": ("Vespa", "8.778.45"), "meili": ("Meilisearch", "1.56.0"), "typesense": ("Typesense", "30.2"),
    "manticore": ("Manticore", "29.9.0"), "qdrant": ("Qdrant", "1.20.0"), "weaviate": ("Weaviate", "1.39.14"),
    "milvus": ("Milvus", "2.6.26"), "paradedb": ("ParadeDB", "0.26.4"), "postgres": ("Postgres", "17, pgvector"),
    "redis": ("Redis", "8.8.4"),
}
SHARDED = {"strata", "es", "os", "solr", "vespa"}


def git_show(ref, path):
    try:
        return subprocess.run(["git", "-C", str(ROOT), "show", f"{ref}:{path}"], check=True,
                              capture_output=True).stdout
    except subprocess.CalledProcessError:
        return None


class Source:
    """A results directory on this checkout, or the same path on a git ref."""

    def __init__(self, path, ref=None):
        self.path, self.ref = path, None
        if not (ROOT / path).exists() and ref and git_show(ref, path + "/README.md") is not None:
            self.ref = ref
        self.ok = (ROOT / path).exists() or self.ref is not None

    def bytes(self, name):
        if self.ref:
            return git_show(self.ref, f"{self.path}/{name}")
        p = ROOT / self.path / name
        return p.read_bytes() if p.exists() else None

    def json(self, name):
        b = self.bytes(name)
        if b is None:
            b = self.bytes(name + ".gz")
            b = gzip.decompress(b) if b is not None else None
        return json.loads(b) if b is not None else None

    def url(self, name=""):
        return f"{GH}{'tree' if not name else 'blob'}/{self.ref.removeprefix('origin/') if self.ref else 'main'}/{self.path}/{name}".rstrip("/")


def r3(x):
    """Three significant figures, so the file holds what the pages show."""
    if x is None:
        return None
    if x == 0:
        return 0
    a = abs(x)
    if a >= 1000:
        return round(x)
    digits = 3 - len(str(int(a))) if a >= 1 else 3 - int(f"{a:e}".split("e")[1]) - 1
    return round(x, max(digits, 0))


# ----------------------------------------------------------------- clusters

def medians(src, e, passes):
    rows = {}
    for s in passes:
        r = src.json(f"run-{e}{s}.json")
        for h in (r or {}).get("http", []):
            if h["requests"] > 0:
                rows.setdefault((h["kind"], h["concurrency"]), []).append(h)
    return {k: {m: statistics.median(h[m] for h in hs) for m in ("p50_ms", "p99_ms", "qps")} | {"n": len(hs)}
            for k, hs in rows.items()}


def cluster(src, rid, engines=None, extra_runs=None, ranked=None, lanes=None, take=None):
    """One comparison. `take` maps an engine to a later run (a Source) that
    measured it again, after a change to that engine or to its settings: its
    numbers, recall, load, writes and streams come from there, and the other
    engines' from `src`. Elasticsearch's trial-licence run comes with it."""
    s = src.json("summary.json")
    take = dict(take or {})
    if "es" in take:
        take.setdefault("es-trial", take["es"])
    for e, t in take.items():
        ts = t.json("summary.json")
        if e not in s["engines"]:
            s["engines"].append(e)
        for key in ("quality", "unavailable", "load", "writes", "stream"):
            s.setdefault(key, {})
            if (ts.get(key) or {}).get(e) is not None:
                s[key][e] = ts[key][e]
            else:
                s[key].pop(e, None)
    all_engines = [e for e in s["engines"] if e != "es-trial"]
    engines = [e for e in (engines or all_engines) if e in all_engines]
    passes = ["", "-pass2", "-pass3"]
    H = {e: medians(take.get(e, src), e, passes) for e in all_engines + ["es-trial"]}
    for e, name in (extra_runs or {}).items():  # a kind re-run on its own (Solr's SQL on the Mac)
        for k, v in medians(src, name, [""]).items():
            H[e][k] = v
    npass = max((v["n"] for e in engines for v in H[e].values()), default=1)

    def get(e, k, c):
        v = H[e].get((k, c))
        if v is None and e == "es":
            v = H["es-trial"].get((k, c))
        return v

    Q, U = s["quality"], s.get("unavailable", {})
    m = {"p50": {}, "p99": {}, "qps1": {}, "qps4": {}, "qps16": {}, "recall": {}, "ndcg": {}}
    raw = {key: {} for key in m}  # unrounded, for places: rounding would make ties rank.py does not see
    na, notes = {}, {}
    for e in engines:
        for key in m:
            m[key][e] = {}
            raw[key][e] = {}
        def put(key, k, v, rounding=r3):
            raw[key][e][k] = v
            m[key][e][k] = rounding(v)
        for k, _, _ in KINDS:
            one, four, sixteen = get(e, k, 1), get(e, k, 4), get(e, k, 16)
            if one:
                put("p50", k, one["p50_ms"])
                put("p99", k, one["p99_ms"])
                put("qps1", k, one["qps"])
            if four:
                put("qps4", k, four["qps"])
            if sixteen:
                put("qps16", k, sixteen["qps"])
            q = (Q.get(e) or {}).get(k) or {}
            if q.get("recall_at_10") is not None and k in RECALL:
                put("recall", k, q["recall_at_10"], lambda v: round(v, 3))
            if q.get("ndcg_at_10") is not None:
                put("ndcg", k, q["ndcg_at_10"], lambda v: round(v, 3))
            if not one:
                why = (U.get(e) or {}).get(k)
                if why and why.startswith("{"):
                    why = "needs a paid licence"
                elif why and "statement timeout" in why:
                    why = "over the 20 s statement timeout set during the run, so that the vector kinds could finish"
                elif why and why.startswith("error sending request"):
                    why = "the request failed with a connection error at this size"
                elif why and "SEARCH_TIMEOUT" in why:
                    why = "hit its own search timeout"
                na.setdefault(e, {})[k] = why or "no answer in time, or the run failed"
            elif e == "es" and H["es"].get((k, 1)) is None:
                notes.setdefault(e, {})[k] = "on Elasticsearch's 30-day trial licence: a paid feature"
    # places
    rank_engines = [e for e in (ranked or engines)]
    tests = []
    for k, _, _ in KINDS:
        tests.append(("p50", k, False))
        tests.append(("p99", k, False))
        tests.append(("qps4", k, True))
    for k in RECALL:
        tests.append(("recall", k, True))
    n = len(rank_engines)
    score = {e: 0 for e in rank_engines}
    ran = {e: 0 for e in rank_engines}
    own = {e: 0 for e in rank_engines}
    firsts = {e: [] for e in rank_engines}
    places = {e: {} for e in rank_engines}
    for metric, k, high in tests:
        vals = {e: raw[metric][e][k] for e in rank_engines if k in raw[metric][e]}
        for e in rank_engines:
            if e in vals:
                place = 1 + sum(1 for v in vals.values() if (v > vals[e] if high else v < vals[e]))
                ran[e] += 1
                own[e] += place
                if place == 1:
                    firsts[e].append([metric, k])
            else:
                place = n
            score[e] += place
            places[e].setdefault(metric, {})[k] = place if e in vals else None
    rank = sorted(rank_engines, key=lambda e: score[e])
    def placed(e):
        return [p for met in places[e].values() for p in met.values() if p is not None]
    rank = [{"e": e, "points": score[e], "ran": ran[e], "own": round(own[e] / max(ran[e], 1), 2),
             "firsts": firsts[e], "half": sum(1 for p in placed(e) if p <= n // 2),
             "worst": max(placed(e), default=None),
             "cant": sum(1 for k, _, _ in KINDS if k not in m["p50"][e])} for e in rank]
    # the same ranking under other rules, to show how much it depends on them
    def summed(tests_, tie=0.0, missing_last=True):
        sc, cnt = {e: 0 for e in rank_engines}, {e: 0 for e in rank_engines}
        for metric, k, high in tests_:
            vals = {e: raw[metric][e][k] for e in rank_engines if k in raw[metric][e]}
            for e in rank_engines:
                if e in vals:
                    v = vals[e]
                    sc[e] += 1 + sum(1 for o in vals.values() if (o > v * (1 + tie) if high else o * (1 + tie) < v))
                    cnt[e] += 1
                elif missing_last:
                    sc[e] += n
        if missing_last:
            out = sorted(rank_engines, key=lambda e: sc[e])
            return [[e, sc[e]] for e in out]
        avg = {e: sc[e] / cnt[e] for e in rank_engines if cnt[e]}
        return [[e, round(avg[e], 2)] for e in sorted(avg, key=avg.get)]
    common = [k for k, _, _ in KINDS if all(k in m["p50"][e] for e in rank_engines)]
    variants = [
        {"rule": "As ranked above", "rank": summed(tests)},
        {"rule": "Throughput at 16 clients instead of 4", "rank": summed([("qps16" if t[0] == "qps4" else t[0], t[1], t[2]) for t in tests])},
        {"rule": "Gaps under 15% counted as ties", "rank": summed(tests, tie=0.15)},
        {"rule": f"Only the {len(common)} kinds every engine runs", "rank": summed([t for t in tests if t[1] in common]), "kinds": common},
        {"rule": "Average place on only the tests each engine ran", "rank": summed(tests, missing_last=False), "avg": True},
    ]
    # load, writes, freshness, streams
    L = s.get("load", {})
    load = {}
    for e in engines:
        l = L.get(e)
        if not l:
            continue
        shards = l.get("shards") or []
        secs = max(x["total_s"] for x in shards) if shards else l["total_s"]
        load[e] = {"s": round(secs), "gb": r3((l.get("index_bytes") or l.get("memory_bytes") or 0) / 1e9) or None,
                   "mem": bool(l.get("memory_bytes") and not l.get("index_bytes"))}
    writes = {}
    for e, w in (s.get("writes") or {}).items():
        if e not in engines or not w:
            continue
        ph = {(x["phase"], x["concurrency"]): x for x in w.get("writes", [])}
        vis = w.get("visible") or {}
        writes[e] = {
            "insert1": r3((ph.get(("insert one", 16)) or {}).get("docs_per_s")),
            "batch16": r3((ph.get(("insert batch", 16)) or {}).get("docs_per_s")),
            "update16": r3((ph.get(("update one", 16)) or {}).get("docs_per_s")),
            "update1_p50": r3((ph.get(("update one", 1)) or {}).get("p50_ms")),
            "visible_p50": r3(vis.get("visible_p50_ms")),
            "visible_p90": r3(vis.get("visible_p90_ms")),
        }
    stream = {}
    for e, st in (s.get("stream") or {}).items():
        if e not in engines or not st:
            continue
        phases = st.get("phases") or []
        kept = [p for p in phases if re.fullmatch(r"\d+/s", p.get("phase", "")) and p.get("kept_up")]
        flat = next((p for p in phases if p.get("phase") == "flat out"), {})
        at1k = next((p for p in phases if p.get("rate") == 1000), {})
        rs = st.get("restart") or {}
        stream[e] = {"kept": max((round(p["rate"]) for p in kept), default=None), "flat": r3(flat.get("written_per_s")),
                     "lag1k": r3(at1k.get("lag_p50_ms")), "again": rs.get("written_again"), "lost": rs.get("lost")}
    return {
        "id": rid, "type": "cluster", "passes": npass, "source": src.url(), "branch": src.ref,
        "engines": [{"id": e, "name": ENGINES[e][0], "ver": ENGINES[e][1], "sharded": e in SHARDED}
                    | ({"lane": lanes[e]} if lanes and e in lanes else {})
                    | ({"from": take[e].url()} if e in take else {}) for e in engines],
        "kinds": [{"id": k, "name": n, "fam": f} for k, n, f in KINDS],
        "m": {k: v for k, v in m.items() if any(v[e] for e in v)}, "na": na, "notes": notes,
        "rank": rank, "tests": len(tests), "ranked": rank_engines, "places": places, "variants": variants,
        "load": load, "writes": writes, "stream": stream,
    }


# ----------------------------------------------------------------- Part A

def part_a(src):
    def j(name):
        return src.json(name)
    # Strata from the rerun of 6 November (main, and 2 November's build as the
    # node check, both on one node); the other systems from 2 November.
    new, was = ("part-a-1106", "part-a-1106-check") if j("part-a-1106/a1_strata.json") else ("part-a", None)

    def sj(part, name):  # a Strata report: "strata" from the rerun, "strata-was" its check
        return j(f"{new}/{part}_strata{name}.json"), (j(f"{was}/{part}_strata{name}.json") if was else None)

    a1 = {s: j(f"part-a/a1_{s}.json") for s in ["sqlite-full", "sqlite-normal", "duckdb"]}
    a1["strata"], a1["strata-was"] = sj("a1", "")
    a1t = {s: j(f"part-a-tantivy/a1_{s}.json") for s in ["strata", "tantivy"]}
    machine = a1["strata"]["machine"]
    machine_t = a1t["strata"]["machine"]

    def g(d, *p):
        for k in p:
            d = (d or {}).get(k)
        return d

    def us(x):  # ms -> µs
        return r3(x * 1000) if x is not None else None

    scols = [{"name": "Strata", "ver": "main, 6 Nov", "self": True}] + (
        [{"name": "Strata", "ver": "2 Nov build, rerun 6 Nov", "self": True}] if was else [])
    ss = ["strata"] + (["strata-was"] if was else [])
    cols_a1 = scols + [{"name": "SQLite", "ver": "3.53, FULL"},
                       {"name": "SQLite", "ver": "3.53, NORMAL"}, {"name": "DuckDB", "ver": "2.0"}]
    sys_a1 = ss + ["sqlite-full", "sqlite-normal", "duckdb"]

    def row(label, f, unit=None, better=None, systems=sys_a1, data=a1):
        cells = []
        for s in systems:
            v = f(data[s]) if data.get(s) else None
            cells.append(r3(v) if isinstance(v, (int, float)) else (v if v is not None else "na:not run"))
        r = {"label": label, "cells": cells}
        if unit is not None:
            r["unit"] = unit
        if better:
            r["better"] = better
        return r

    charts = []
    charts.append({
        "id": "emb-a1-reads", "title": "An app's reads", "sub": "Median at one client, lower is better",
        "unit": "ms", "better": "lower", "scale": "row", "run": "embedded", "columns": cols_a1,
        "rows": [
            row("a product by id (SQL)", lambda d: us(g(d, "get", "product", "p50_ms")), "µs"),
            row("a product by key (no SQL)", lambda d: us(g(d, "get", "product_by_key", "p50_ms")) if g(d, "get", "product_by_key") else "na:no key API", "µs"),
            row("a user's reviews", lambda d: us(g(d, "get", "user_reviews", "p50_ms")), "µs"),
            row("list page 1: category, price range, by rating", lambda d: g(d, "page", "0", "p50_ms")),
            row("the same page at offset 10,000", lambda d: g(d, "page", "10000", "p50_ms")),
            row("reviews per brand", lambda d: g(d, "aggregate", "brand", "p50_ms")),
            row("reviews per month", lambda d: g(d, "aggregate", "month", "p50_ms")),
            row("join: top products with review counts", lambda d: g(d, "join", "top", "p50_ms")),
            row("join: a user's reviewed products", lambda d: us(g(d, "join", "user", "p50_ms")), "µs"),
        ]})
    charts.append({
        "id": "emb-a1-writes", "title": "An app's writes", "sub": "Rows a second, higher is better",
        "unit": "rows/s", "better": "higher", "scale": "row", "run": "embedded", "columns": cols_a1,
        "rows": [
            row("load 4.6M reviews", lambda d: g(d, "load", "reviews", "rows_per_s")),
            row("insert, one row a commit", lambda d: g(d, "insert", "one_per_txn", "rows_per_s")),
            row("insert, 1,000 rows a commit", lambda d: g(d, "insert", "1000_per_txn", "rows_per_s")),
            row("update a price", lambda d: g(d, "update", "price_stock", "rows_per_s")),
            row("delete by id", lambda d: g(d, "delete", "by_id", "rows_per_s")),
            row("reads beside a writer (2 readers)", lambda d: g(d, "mixed", "reads", "qps"), "q/s"),
            row("writes beside readers (1 writer)", lambda d: g(d, "mixed", "writes", "qps"), "rows/s"),
        ]})
    # A2: one chart per kind, rows = collections, columns = systems
    colls = ["scifact", "nfcorpus", "fiqa", "trec-covid", "quora"]
    a2sys = ss + ["strata-1t", "sqlite", "tantivy", "duckdb", "lancedb"]
    a2 = {(s, c): j(f"part-a/a2_{s}_{c}.json") for s in a2sys if not s.startswith("strata") for c in colls}
    a2.update({("strata-1t", c): j(f"part-a-1thread/a2_strata_{c}.json") for c in colls})
    for c in colls:
        a2[("strata", c)], a2[("strata-was", c)] = sj("a2", f"_{c}")
    a2cols = scols + [{"name": "Strata", "ver": "1 thread, older build", "self": True},
                      {"name": "SQLite", "ver": "FTS5, sqlite-vec (exact)"}, {"name": "Tantivy", "ver": "0.27"},
                      {"name": "DuckDB", "ver": "fts, vss"}, {"name": "LanceDB", "ver": "defaults"}]
    a2note = ("Strata searches a query's segments on up to 8 threads by default; its one-thread column is the older build "
              "128459b, not run on quora. SQLite's vectors are sqlite-vec's exact scan (it has no ANN index); LanceDB is "
              "at its default search settings." + (
                  " Strata on main and its 2 November build ran on 6 November on one node, the rest on 2 November on another of the same "
                  "type, on which the 2 November build was about 5% slower at search." if was else ""))
    ndocs = {c: a2[("strata", c)]["n_docs"] for c in colls}

    def a2row(c, kind, field="p50_ms"):
        cells = []
        for s in a2sys:
            if a2[(s, c)] is None:
                cells.append("skip:not run on this build")
                continue
            k = g(a2[(s, c)], "kinds", kind)
            if not k or k.get("support") == "not supported" or not k.get("warm1"):
                cells.append("na:no " + ("vector search" if kind != "bm25" else "BM25"))
                continue
            v = k["warm1"][field] if field != "qps4" else g(k, "warm4", "qps")
            note = None
            if k.get("ndcg10") is not None:
                note = f"nDCG {k['ndcg10']:.3f}" + (f" · recall {k['recall10']:.3f}" if k.get("recall10") is not None else "")
            cell = {"v": r3(v * 1000) if field == "p50_ms" else r3(v)}
            if note:
                cell["q"] = note
            cells.append(cell)
        return {"label": f"{c} ({ndocs[c]:,} docs)", "cells": cells}

    for kind, title in [("bm25", "BM25 top 10"), ("vector", "Vector top 10"), ("hybrid", "Hybrid: BM25 and vectors by RRF"),
                        ("filtered_hybrid", "Hybrid under a filter")]:
        charts.append({"id": f"emb-a2-{kind.replace('_', '-')}", "title": title,
                       "sub": "Median at one client in µs, log scale, lower is better", "unit": "µs", "better": "lower",
                       "scale": "log", "run": "embedded", "columns": a2cols, "note": a2note,
                       "rows": [a2row(c, kind) for c in colls]})
    charts.append({"id": "emb-a2-hybrid4", "title": "Hybrid at four client threads", "sub": "Queries a second, higher is better",
                   "unit": "q/s", "better": "higher", "scale": "log", "run": "embedded", "columns": a2cols, "note": a2note,
                   "rows": [a2row(c, "hybrid", "qps4") for c in colls]})
    # writes and streams with indexes
    wsys = sys_a1 + ["lancedb"]
    W = {s: j(f"part-a/writes_{s}.json") for s in wsys if not s.startswith("strata")}
    W["strata"], W["strata-was"] = sj("writes", "")
    wcols = cols_a1 + [{"name": "LanceDB"}]
    charts.append({
        "id": "emb-writes", "title": "Writes with a text and a vector index", "sub": "137,260 products; rows a second, higher is better",
        "unit": "rows/s", "better": "higher", "scale": "row", "run": "embedded", "columns": wcols,
        "rows": [row(lbl, f, systems=wsys, data=W) for lbl, f in [
            ("insert, one row a commit", lambda d: g(d, "insert", "1", "rows_per_s")),
            ("insert, 100 rows a commit", lambda d: g(d, "insert", "100", "rows_per_s")),
            ("insert, 10,000 rows a commit", lambda d: g(d, "insert", "10000", "rows_per_s")),
            ("update a price", lambda d: g(d, "update", "price_stock", "rows_per_s")),
            ("update a title (re-index text)", lambda d: g(d, "update", "title", "rows_per_s")),
            ("update a vector", lambda d: g(d, "update", "vector", "rows_per_s") or "na:no vector index"),
            ("delete", lambda d: g(d, "update", "delete", "rows_per_s")),
        ]]})
    # stream: reads a second while 10,000 rows/s arrive
    def phase(d, rate):
        return next((p for p in g(d, "stream", "phases") or [] if p.get("rate") == rate), None)
    charts.append({
        "id": "emb-stream", "title": "Reads while a stream arrives", "sub": "Queries a second answered while rows stream in, higher is better",
        "unit": "q/s", "better": "higher", "scale": "row", "run": "embedded", "columns": wcols,
        "rows": [
            row("at 1,000 rows/s", lambda d: g(phase(d, 1000), "reads", "qps"), systems=wsys, data=W),
            row("at 10,000 rows/s", lambda d: g(phase(d, 10000), "reads", "qps"), systems=wsys, data=W),
            row("lag p99 at 10,000 rows/s", lambda d: g(phase(d, 10000), "lag", "p99_ms"), "ms", "lower", systems=wsys, data=W),
        ]})
    # A3
    a3sys = ss + ["lancedb", "sqlite-vec", "faiss-sqlite"]
    A3 = {s: j(f"part-a/a3_{s}.json") for s in a3sys if not s.startswith("strata")}
    A3["strata"], A3["strata-was"] = sj("a3", "")
    a3cols = scols + [{"name": "LanceDB"}, {"name": "sqlite-vec"}, {"name": "FAISS", "ver": "+ SQLite"}]
    charts.append({
        "id": "emb-a3", "title": "Recommendations", "sub": "Similar products under filters, and a feed fusing vectors with a walk over products reviewed together",
        "unit": "ms", "better": "lower", "scale": "row", "run": "embedded", "columns": a3cols,
        "rows": [
            row("similar items, median", lambda d: g(d, "similar", "warm1", "p50_ms"), "ms", systems=a3sys, data=A3),
            row("similar items, p99", lambda d: g(d, "similar", "warm1", "p99_ms"), "ms", systems=a3sys, data=A3),
            row("similar items, 4 threads", lambda d: g(d, "similar", "warm4", "qps"), "q/s", "higher", systems=a3sys, data=A3),
            row("recall@10 of similar items", lambda d: g(d, "similar", "recall10"), "", "higher", systems=a3sys, data=A3),
            row("the feed (vectors + PageRank walk)", lambda d: g(d, "feed", "warm1", "p50_ms") if g(d, "feed") else "na:no graph walk", "ms", systems=a3sys, data=A3),
        ]})
    # Tantivy as the app's store
    tcols = [{"name": "Strata", "self": True}, {"name": "Tantivy", "ver": "0.27, tantivy-py"}]
    tsys = ["strata", "tantivy"]
    Wt = {s: j(f"part-a-tantivy-2/writes_{s}.json") for s in tsys}
    charts.append({
        "id": "emb-tantivy", "title": "Tantivy as the app's store", "sub": "The same node for both; lower is better unless marked",
        "unit": "µs", "better": "lower", "scale": "row", "run": "embedded-tantivy", "columns": tcols,
        "rows": [
            row("a product by id", lambda d: us(g(d, "get", "product", "p50_ms")), "µs", systems=tsys, data=a1t),
            row("a user's reviews", lambda d: us(g(d, "get", "user_reviews", "p50_ms")), "µs", systems=tsys, data=a1t),
            row("list page 1", lambda d: us(g(d, "page", "0", "p50_ms")), "µs", systems=tsys, data=a1t),
            row("the same page at offset 10,000", lambda d: us(g(d, "page", "10000", "p50_ms")), "µs", systems=tsys, data=a1t),
            row("reviews per month", lambda d: g(d, "aggregate", "month", "p50_ms"), "ms", systems=tsys, data=a1t),
            row("join: top products", lambda d: g(d, "join", "top", "p50_ms"), "ms", systems=tsys, data=a1t),
            row("insert, one row a commit", lambda d: g(d, "insert", "1", "rows_per_s"), "rows/s", "higher", systems=tsys, data=Wt),
            row("update a price", lambda d: g(d, "update", "price_stock", "rows_per_s"), "rows/s", "higher", systems=tsys, data=Wt),
            row("insert, 10,000 rows a commit", lambda d: g(d, "insert", "10000", "rows_per_s"), "rows/s", "higher", systems=tsys, data=Wt),
        ]})
    tb = []
    for c in colls:
        cells = []
        for s in tsys:
            d = j(f"part-a-tantivy-2/a2_{s}_{c}.json")
            k = g(d, "kinds", "bm25")
            cells.append({"v": r3(k["warm1"]["p50_ms"] * 1000), "q": f"nDCG {k['ndcg10']:.3f}"})
        tb.append({"label": f"{c} ({ndocs[c]:,} docs)", "cells": cells})
    charts.append({"id": "emb-tantivy-bm25", "title": "BM25 beside Tantivy", "sub": "Median at one client in µs, lower is better",
                   "unit": "µs", "better": "lower", "scale": "row", "run": "embedded-tantivy", "columns": tcols, "rows": tb,
                   "note": "Strata searches a query's segments on up to 8 threads by default; tantivy-py searches on one."})
    m = machine
    mt = machine_t
    return charts, (f"DigitalOcean {m['node_size']} ({m['node_region']}): {m['cpu'].replace('(R)', '')}, {m['cpus']} vCPU, "
                    f"{m['memory_bytes'] / 2**30:.0f} GiB, Python {m['python']}"), (
        f"DigitalOcean {mt['node_size']}: {mt['cpu'].replace('(R)', '')}, {mt['cpus']} vCPU, Python {mt['python']}")


# ----------------------------------------------------------------- graph

def md_tables(text):
    """Every pipe table of a markdown text: (heading line above it, rows)."""
    out, cur, before = [], None, ""
    for line in text.splitlines():
        if line.startswith("|"):
            cells = [c.strip() for c in line.strip().strip("|").split("|")]
            if cur is None:
                cur = {"before": before, "rows": []}
            if not all(set(c) <= set("-: ") for c in cells):
                cur["rows"].append(cells)
        else:
            if cur:
                out.append(cur)
                cur = None
            if line.strip():
                before = line.strip()
    if cur:
        out.append(cur)
    return out


def num(cell):
    m = re.search(r"[\d,]*\.?\d+", cell.replace("**", ""))
    return float(m.group(0).replace(",", "")) if m else None


def graph(src):
    text = src.bytes("README.md").decode()
    tables = md_tables(text)
    lat = next(t for t in tables if t["rows"][0][0] == "query" and "Neo4j" in t["rows"][0] and "Strata (field), gRPC" in t["rows"][0])
    head = lat["rows"][0][1:]
    cols = []
    for h in head:
        cols.append({"name": h.split(",")[0].replace("Strata (field)", "Strata").replace("Strata (edge table)", "Strata, edge table"),
                     "ver": (h.split(", ", 1)[1] if ", " in h else ""), "self": h.startswith("Strata")})
    p50_rows, p95_rows = [], []
    for r in lat["rows"][1:]:
        label = r[0].split(" (")[0]
        c50, c95 = [], []
        for cell in r[1:]:
            if cell in ("", "n/a"):
                c50.append("na:no such query" if cell == "n/a" else "skip:not in this setting")
                c95.append(c50[-1])
                continue
            parts = cell.replace("**", "").split("/")
            note = cell.split("(", 1)[1].rstrip(")") if "(" in cell else None
            v50 = {"v": num(parts[0])}
            v95 = {"v": num(parts[1])} if len(parts) > 1 else None
            if note:
                v50["note"] = note
            c50.append(v50)
            c95.append(v95 or "skip:")
        p50_rows.append({"label": label, "cells": c50})
        p95_rows.append({"label": label, "cells": c95})
    rec = next(t for t in tables if t["rows"][0][0] == "setting")
    load = next(t for t in tables if t["rows"][0][0] == "engine" and "load (s)" in t["rows"][0])
    ranking = re.search(r"Ranked by the sum of places over (\d+) tests.*?: (.*?)\. Without the\s+six PageRank tests, Strata has (\d+) and Memgraph (\d+)\.", text, re.S)
    rank = []
    if ranking:
        body = " ".join(ranking.group(2).split()).split(": ", 1)[-1]
        for part in body.split(", "):
            name, pts = part.rsplit(" ", 1)
            rank.append((name, int(pts)))
    when = re.search(r"Measured on (\d{4}-\d{2}-\d{2})", text)
    charts = [
        {"id": "graph-p50", "title": "Median latency", "sub": "Client time in ms, one query at a time, lower is better",
         "unit": "ms", "better": "lower", "scale": "row", "run": "graph", "columns": cols, "rows": p50_rows},
        {"id": "graph-p95", "title": "95th percentile", "sub": "Client time in ms, lower is better",
         "unit": "ms", "better": "lower", "scale": "row", "run": "graph", "columns": cols, "rows": p95_rows},
    ]
    rh = rec["rows"][0][1:]
    charts.append({"id": "graph-ppr-recall", "title": "Personalized PageRank: how exact", "sub": "recall@20 against the exact top 20, higher is better",
                   "unit": "", "better": "higher", "scale": "unit", "digits": 3, "run": "graph",
                   "columns": [{"name": h.replace("Strata (field)", "Strata").replace("Strata (edge table)", "Strata, edge table"), "self": h.startswith("Strata")} for h in rh],
                   "rows": [{"label": r[0].split(" (")[0], "note": r[0], "cells": [num(c) if c else "skip:not in this setting" for c in r[1:]]} for r in rec["rows"][1:]]})
    lrows = load["rows"][1:]
    charts.append({"id": "graph-load", "title": "Load and footprint", "sub": "Lower is better",
                   "unit": "s", "better": "lower", "scale": "row", "run": "graph",
                   "columns": [{"name": r[0].replace("Strata (field), gRPC run", "Strata").replace("Strata (field), HTTP run", "Strata, HTTP").replace("Strata (edge table), gRPC run", "Strata, edge table"), "self": r[0].startswith("Strata")} for r in lrows],
                   "rows": [
                       {"label": "load the graph", "unit": "s", "cells": [num(r[1]) for r in lrows]},
                       {"label": "memory after the queries", "unit": "MiB", "cells": [num(r[2]) * (1024 if "GiB" in r[2].split("(")[0] else 1) for r in lrows]},
                       {"label": "data on disk", "unit": "MiB", "cells": [num(r[3]) or "na:in memory, nothing saved" for r in lrows]},
                   ]})
    return charts, rank, (int(ranking.group(1)) if ranking else None), (int(ranking.group(3)), int(ranking.group(4))) if ranking else None, when.group(1) if when else None


# ----------------------------------------------------------------- BM25 under load

def bm25_load(src):
    text = src.bytes("README.md").decode()
    t = next(t for t in md_tables(text) if t["rows"][0][:3] == ["kind", "clients", "q/s before"])
    rows = [{"kind": r[0], "clients": int(r[1]), "before": num(r[2]), "after": num(r[3]), "p50_before": num(r[5]), "p50_after": num(r[6])} for r in t["rows"][1:]]
    return rows


# ----------------------------------------------------------------- Strata, then and now

PASSES = ["", "-pass2", "-pass3"]


def since(before, after, rows):
    """Strata's medians in two runs, (kind, clients) by (kind, clients): q/s and p50 at those clients."""
    B, A = medians(before, "strata", PASSES), medians(after, "strata", PASSES)
    out = []
    for k, c in rows:
        b, a = B.get((k, c)), A.get((k, c))
        if b and a:
            out.append({"kind": k, "clients": c, "before": r3(b["qps"]), "after": r3(a["qps"]),
                        "p50_before": r3(b["p50_ms"]), "p50_after": r3(a["p50_ms"])})
    return out


# ----------------------------------------------------------------- main

def main():
    runs, reports, suites, missing = {}, {}, [], []
    c10 = Source("strata-server/results/compare-msmarco-10m-do")
    c10h = Source("strata-server/results/compare-msmarco-same-nodes-do/10m")
    c1 = Source("strata-server/results/compare-msmarco-same-nodes-do/1m")
    c1f = Source("strata-server/results/compare-msmarco-1m-do")
    mac = Source("strata-server/results/compare-nq-mac")
    pa = Source("strata/results/laptop-2026-11/part-a-do")
    gr = Source("strata-server/results/graph-compare-mac", "origin/claude/graph-compare-mac")
    bl = Source("strata-server/results/bm25-stats-cache-container")
    bd = Source("strata-server/results/bm25-do-1m")
    bi1 = Source("strata-server/results/bigger-items-do/1m")
    bi10 = Source("strata-server/results/bigger-items-do/10m")
    r1 = Source("strata-server/results/rerun-1106-do/1m")
    r10 = Source("strata-server/results/rerun-1106-do/10m")

    if c10.ok:
        ranked = ["strata", "os", "es", "solr", "vespa", "qdrant", "weaviate", "paradedb", "postgres"]
        # Milvus (out of memory at 52 GiB) and Redis (all in memory) ran on a 128 GB
        # node with the same CPUs: shown, never ranked (the run's README)
        # Strata from the 6 November rerun, Elasticsearch and OpenSearch from 5
        # November's (result caches off, #262); the others did not change
        take10 = {"strata": r10, "es": bi10, "os": bi10} if r10.ok else {}
        reports["cloud-10m"] = cluster(c10, "cloud-10m", ranked=ranked, take=take10,
                                       lanes={"milvus": "128 GB node, not ranked", "redis": "128 GB node, not ranked"} | (
                                           {"strata": "6 Nov", "es": "9.5.5, 5 Nov", "os": "3.9.0, 5 Nov"} if take10 else {}))
        if take10:
            reports["cloud-10m"]["passes_note"] = "medians of 3 passes for Strata, Elasticsearch and OpenSearch, one pass for the rest"
    else:
        missing.append("cloud-10m")
    if r10.ok:
        # 5 November's head to head with Strata from 6 November's rerun
        reports["cloud-10m-h2h"] = cluster(bi10, "cloud-10m-h2h", take={"strata": r10}, lanes={"strata": "6 Nov", "es": "5 Nov", "os": "5 Nov"})
        c10h = bi10
    else:
        reports["cloud-10m-h2h"] = cluster(c10h, "cloud-10m-h2h")
    # Milvus from its run alone on a fresh node: last in its lane it was still
    # compacting during its first two passes (the run's README)
    # Engines whose code or settings changed since that run, from the runs that
    # measured them again: Strata on 6 November (main b022e39), Manticore then
    # too (its query cache off, #262), Elasticsearch and OpenSearch on 5
    # November (result caches off, #262). The others' code and settings did
    # not change. Every run's README names its nodes.
    take1 = {"strata": r1, "manticore": r1, "es": bi1, "os": bi1} if r1.ok else {}
    reports["cloud-1m"] = cluster(c1, "cloud-1m", take=take1, lanes={
        "milvus": "2.6.26, run alone on a fresh node"} | ({
        "strata": "6 Nov", "manticore": "29.9.0, 6 Nov", "es": "9.5.5, 5 Nov", "os": "3.9.0, 5 Nov"} if take1 else {}))
    reports["cloud-1m-first"] = cluster(c1f, "cloud-1m-first")
    reports["strata-1m-since"] = {"id": "strata-1m-since", "type": "ab", "source": (r1 if r1.ok else c1).url(), "rows": since(c1f, r1 if r1.ok else c1, [
        ("bm25", 1), ("bm25", 16), ("bm25 + filter", 16), ("phrase", 16), ("prefix", 16),
        ("typo", 1), ("typo", 16), ("boosted (bm25 × decay)", 16), ("hybrid (RRF)", 16), ("sql aggregate", 1)])}
    reports["laptop"] = cluster(mac, "laptop", extra_runs={"solr": "solr+sql"})
    for rid, src in [("cloud-10m", c10), ("cloud-10m-h2h", c10h), ("cloud-1m", c1), ("cloud-1m-first", c1f), ("laptop", mac)]:
        if rid in reports:
            reports[rid]["source"] = src.url()
    charts, machine, machine_t = part_a(pa)
    runs["embedded"] = {"label": "No server, 2 and 6 November 2026", "machine": machine, "source": pa.path + "/part-a/ and part-a-1106*/", "url": pa.url()}
    runs["embedded-tantivy"] = {"label": "No server, 3 November 2026", "machine": machine_t, "source": pa.path + "/part-a-tantivy*/", "url": pa.url()}
    suites.append({"id": "embedded", "charts": charts})
    if gr.ok:
        gcharts, grank, gtests, gwithout, gwhen = graph(gr)
        runs["graph"] = {"label": "Graph, " + (gwhen or ""), "machine": "MacBook Air, Apple M2, 24 GB; Docker Desktop VM with 4 CPUs; each engine 3 CPUs and 6 GB",
                         "source": gr.path + "/", "url": gr.url(), "branch": gr.ref}
        suites.append({"id": "graph", "charts": gcharts})
        reports["graph"] = {"id": "graph", "type": "graph", "rank": [{"name": n, "points": p} for n, p in grank], "tests": gtests,
                            "without_ppr": gwithout, "source": gr.url(), "branch": gr.ref}
    else:
        missing.append("graph")
    reports["bm25-load"] = {"id": "bm25-load", "type": "ab", "rows": bm25_load(bl), "source": bl.url()}
    if bd.ok:
        reports["bm25-do"] = {"id": "bm25-do", "type": "ab", "rows": bm25_load(bd), "source": bd.url()}
    else:
        missing.append("bm25-do")

    data = {
        "generated_by": "docs/site/scripts/bench_data.py",
        "missing": missing,
        "runs": runs,
        "reports": reports,
        "suites": suites,
    }
    js = ("/* Generated by docs/site/scripts/bench_data.py from the committed results; do not edit.\n"
          " * Re-run it after a benchmark's results change. */\n"
          "window.STRATA_BENCH = " + json.dumps(data, ensure_ascii=False, separators=(",", ":")) + ";\n")
    OUT.write_text(js)
    print(f"wrote {OUT.relative_to(ROOT)} ({len(js):,} bytes); missing: {', '.join(missing) or 'none'}", file=sys.stderr)


if __name__ == "__main__":
    main()
