#!/usr/bin/env python3
"""Scores and tabulates the reports of compare.sh.

    python3 analyze.py REPORTS_DIR BEIR_DIR/quora > tables.md

Reads run-<label>.json (timings and answers), writes-<engine>.json, stream-<engine>.json, load-<engine>.json,
build-strata-s<i>.json and disk-vespa.jsonl (load time and index size) and
cpu.jsonl from REPORTS_DIR, and the collection from BEIR_DIR/quora, and
prints Markdown tables: load time and index size; latency and throughput per
kind; and the quality of each engine's answers:

- recall@10 of the vector kinds against an exact search (dot product over
  every document, with the filter for "vector + filter", and for "similar
  items" the item's vector, rated 3 or more, minus the keys it was seen with);
- nDCG@10 against Quora's relevance judgments, for every kind ranked for a
  query's text;
- how many SQL aggregates equal the true answer, computed from the documents;
- overlap@10 with Strata's answers, for every ranked kind, and how many
  facet and key-lookup answers equal Strata's (facets up to the order of
  values with the same count).

Writes REPORTS_DIR/summary.json with the same numbers. Needs numpy.
"""

import gzip
import json
import math
import sys
from pathlib import Path

import numpy as np

# Reports by label (run-<label>.json): Strata at the default split of its
# 3 CPUs and, if present, at another (compare.sh run with SHARD_CPU and
# COORD_CPU set, the report renamed run-strata-<split>.json and its CPU lines
# moved to cpu-strata-<split>.jsonl).
# es-trial: Elasticsearch's hybrid retrievers under its trial licence
# (compare.sh trial, then run es --paid --label es-trial).
RUNS = [
    ("strata", "Strata"),
    ("strata-coord500m", "Strata, 0.5-CPU coordinator"),
    ("es", "Elasticsearch"),
    ("es-trial", "Elasticsearch, trial licence"),
    ("os", "OpenSearch"),
    ("solr", "Solr"),
    ("vespa", "Vespa"),
]
ENGINES = [k for k, _ in RUNS]
NAMES = dict(RUNS)
# Ranked for a query's text: scored against Quora's judgments.
TEXT_RANKED = [
    "bm25",
    "bm25 + filter",
    "phrase",
    "typo",
    "prefix",
    "vector",
    "vector + filter",
    "hybrid (RRF)",
    "hybrid (weighted)",
    "boosted (bm25 × decay)",
]
RANKED = TEXT_RANKED + ["similar items"]
KINDS = RANKED + ["facet", "key lookup", "sql aggregate"]


def load_json(p):
    p = Path(p)
    if p.exists():
        return json.loads(p.read_text())
    gz = p.with_name(p.name + ".gz")
    return json.loads(gzip.decompress(gz.read_bytes())) if gz.exists() else None


def exact_top10(beir, qids, filtered):
    meta = json.loads((beir / "meta.json").read_text())
    n, dim = meta["docs"], meta["dim"]
    docs = np.memmap(beir / "doc_vecs.f32", dtype=np.float32, mode="r", shape=(n, dim))
    ids, ratings = [], np.empty(n, dtype=np.float32)
    with open(beir / "docs.jsonl") as f:
        for i, line in enumerate(f):
            d = json.loads(line)
            ids.append(d["id"])
            ratings[i] = d["rating"]
    row = {}
    with open(beir / "queries.jsonl") as f:
        for i, line in enumerate(f):
            row[json.loads(line)["id"]] = i
    qv = np.memmap(beir / "query_vecs.f32", dtype=np.float32, mode="r", shape=(meta["queries"], dim))
    q = np.stack([qv[row[i]] for i in qids])
    best_s = np.full((len(qids), 10), -np.inf, dtype=np.float32)
    best_i = np.zeros((len(qids), 10), dtype=np.int64)
    step = 65536
    for start in range(0, n, step):
        s = q @ np.asarray(docs[start : start + step]).T
        if filtered:
            s[:, ratings[start : start + step] < 4] = -np.inf
        cand_s = np.concatenate([best_s, s], axis=1)
        cand_i = np.concatenate([best_i, np.arange(start, start + s.shape[1])[None, :].repeat(len(qids), 0)], axis=1)
        top = np.argpartition(-cand_s, 10, axis=1)[:, :10]
        best_s = np.take_along_axis(cand_s, top, 1)
        best_i = np.take_along_axis(cand_i, top, 1)
    return {qid: {ids[j] for j in best_i[k]} for k, qid in enumerate(qids)}


def exact_similar(beir, probes):
    """The 10 documents nearest each probe's item, rated 3 or more, minus its seen keys."""
    meta = json.loads((beir / "meta.json").read_text())
    n, dim = meta["docs"], meta["dim"]
    docs = np.memmap(beir / "doc_vecs.f32", dtype=np.float32, mode="r", shape=(n, dim))
    ids, ratings = [], np.empty(n, dtype=np.float32)
    with open(beir / "docs.jsonl") as f:
        for i, line in enumerate(f):
            d = json.loads(line)
            ids.append(d["id"])
            ratings[i] = d["rating"]
    row = {k: i for i, k in enumerate(ids)}
    q = np.stack([np.asarray(docs[row[p["item"]]]) for p in probes])
    step = 65536
    best_s = np.full((len(probes), 10), -np.inf, dtype=np.float32)
    best_i = np.zeros((len(probes), 10), dtype=np.int64)
    seen = [np.array([row[k] for k in p["seen"]]) for p in probes]
    for start in range(0, n, step):
        s = q @ np.asarray(docs[start : start + step]).T
        s[:, ratings[start : start + step] < 3] = -np.inf
        for k, sk in enumerate(seen):
            s[k, sk[(sk >= start) & (sk < start + s.shape[1])] - start] = -np.inf
        cand_s = np.concatenate([best_s, s], axis=1)
        cand_i = np.concatenate([best_i, np.arange(start, start + s.shape[1])[None, :].repeat(len(probes), 0)], axis=1)
        top = np.argpartition(-cand_s, 10, axis=1)[:, :10]
        best_s = np.take_along_axis(cand_s, top, 1)
        best_i = np.take_along_axis(cand_i, top, 1)
    return [{ids[j] for j, sc in zip(best_i[k], best_s[k]) if sc > -np.inf} for k in range(len(probes))]


def true_aggregates(beir, years):
    """SELECT category, COUNT(*), AVG(rating) FROM docs WHERE year >= :y GROUP BY
    category HAVING COUNT(*) > 1000 ORDER BY 2 DESC LIMIT 10, for each year:
    the ten rows, and the rows that tie with the tenth's count."""
    rows = []
    with open(beir / "docs.jsonl") as f:
        for line in f:
            d = json.loads(line)
            rows.append((d["year"], d["category"], d["rating"]))
    out = {}
    for y in set(years):
        g = {}
        for yr, c, r in rows:
            if yr >= y:
                n, t = g.get(c, (0, 0.0))
                g[c] = (n + 1, t + r)
        top = sorted(((c, n, t / n) for c, (n, t) in g.items() if n > 1000), key=lambda x: -x[1])
        cut = top[9][1] if len(top) > 10 else None
        out[y] = (top[:10], [x for x in top if cut is not None and x[1] == cut])
    return out


def same_aggregate(rows, truth):
    """Whether an engine's rows are the true answer: the same counts in order, each
    category with its count (any of the tied ones at the tenth), averages within 1e-5
    (engines that keep ratings as 32-bit floats average them in other last digits)."""
    top, ties = truth
    got = []
    for r in rows:
        if len(r) < 3:
            return False
        got.append((str(r[0]), int(r[1]), float(r[2])))
    if [n for _, n, _ in got] != [n for _, n, _ in top]:
        return False
    allowed = {c: (n, a) for c, n, a in top + ties}
    if len({c for c, _, _ in got}) != len(got):
        return False
    return all(c in allowed and allowed[c][0] == n and abs(allowed[c][1] - a) <= 1e-5 * max(1.0, abs(a)) for c, n, a in got)


def same_facet(a, b):
    """Whether two facets' (value, count) rows are the same answer: the same
    counts in order, and the same values above the last count (values that
    tie may come in any order, and those tied at the cut may differ)."""
    if [r[1] for r in a] != [r[1] for r in b]:
        return False
    if not a:
        return True
    cut = a[-1][1]
    return {(r[0], r[1]) for r in a if r[1] > cut} == {(r[0], r[1]) for r in b if r[1] > cut}


def qrels(beir):
    out = {}
    for line in open(beir / "qrels.tsv"):
        parts = line.rstrip("\n").split("\t")
        if len(parts) >= 3 and parts[2].strip().isdigit() and int(parts[2]) > 0:
            out.setdefault(parts[0], set()).add(parts[1])
    return out


def ndcg10(keys, rel):
    dcg = sum(1 / math.log2(i + 2) for i, k in enumerate(keys[:10]) if k in rel)
    ideal = sum(1 / math.log2(i + 2) for i in range(min(len(rel), 10)))
    return dcg / ideal if ideal else None


def rows_of(a):
    rows = a.get("rows") or []
    return [list(r.values()) if isinstance(r, dict) else list(r) for r in rows]


def fmt(x, digits=2):
    if x is None:
        return "–"
    if isinstance(x, float) and x >= 1000:
        return f"{x:,.0f}"
    return f"{x:.{digits}f}"


def main():
    rep, beir = Path(sys.argv[1]), Path(sys.argv[2])
    runs = {e: load_json(rep / f"run-{e}.json") for e in ENGINES}
    runs = {e: r for e, r in runs.items() if r}
    # Strata's answers do not depend on how its CPUs are split: score one.
    scored = [e for e in runs if e != "strata-coord500m"]
    if "strata-coord500m" in runs and "strata" in runs:
        same = runs["strata-coord500m"]["answers"] == runs["strata"]["answers"]
        print(f"Strata's answers at both CPU splits are {'identical' if same else 'DIFFERENT'}.\n")
    rel = qrels(beir)
    summary = {"engines": list(runs), "load": {}, "http": {}, "quality": {}, "unavailable": {}, "differs": {}, "cpu": {}}

    # Load time and index size.
    print("## Load time and index size\n")
    print("| engine | load (s) | of which: send / build | searchable | merge to 1 segment | index size (GB) | documents |")
    print("|---|---|---|---|---|---|---|")
    builds = [load_json(rep / f"build-strata-s{i}.json") for i in (0, 1)]
    if all(builds):
        # compare.sh builds the shards one after the other.
        total = sum(b["total_s"] for b in builds)
        size = sum(b["file_bytes"] for b in builds)
        docs = sum(b["docs"] for b in builds)
        summary["load"]["strata"] = {"total_s": total, "index_bytes": size, "docs": docs, "shards": builds}
        print(f"| Strata | {total:.0f} | read {sum(b['read_s'] for b in builds):.0f} + build {sum(b['build_s'] for b in builds):.0f} | (built) | (built as one) | {size / 1e9:.2f} | {docs:,} |")
    for e in ["es", "os", "solr", "vespa"]:
        l = load_json(rep / f"load-{e}.json")
        if l:
            if e == "vespa":
                # compare.sh: du of each content node's documents directory, once proton had flushed.
                disk = [json.loads(x) for x in open(rep / "disk-vespa.jsonl")] if (rep / "disk-vespa.jsonl").exists() else []
                l["index_bytes"] = sum(d["documents_bytes"] for d in disk) if disk else None
                l["tls_bytes"] = sum(d["tls_bytes"] for d in disk) if disk else None
                l["indexed_docs"] = l.get("searchable_docs")
            summary["load"][e] = {k: l.get(k) for k in ["total_s", "ingest_s", "refresh_s", "merge_s", "index_bytes", "tls_bytes", "indexed_docs", "segments", "json_bytes"]}
            size = f"{l['index_bytes'] / 1e9:.2f}" if l.get("index_bytes") is not None else "–"
            if l.get("tls_bytes"):
                size += f" (+ {l['tls_bytes'] / 1e9:.2f} transaction log)"
            searchable, merged = (f"{l['refresh_s']:.0f}", f"{l['merge_s']:.0f}") if e != "vespa" else ("on write", "(on its own)")
            docs = f"{l['indexed_docs']:,}" if l.get("indexed_docs") is not None else "–"
            print(f"| {NAMES[e]} | {l['total_s']:.0f} | send {l['ingest_s']:.0f} | {searchable} | {merged} | {size} | {docs} |")

    # Latency and throughput.
    def http(e, kind, c):
        for h in runs[e]["http"]:
            if h["kind"] == kind and h["concurrency"] == c:
                return h
        return None

    for e, r in runs.items():
        summary["unavailable"][e] = r.get("unavailable", {})
        summary["differs"][e] = r.get("differs", {})
        summary["http"][e] = r["http"]
    concs = sorted({h["concurrency"] for r in runs.values() for h in r["http"]})

    def pct(h):
        # p90 is in reports from runs after it was added.
        return " / ".join(f"{h[k]:.2f}" if k in h else "–" for k in ["p50_ms", "p90_ms", "p99_ms"])

    for c in concs:
        print(f"\n## {c} client{'s' if c > 1 else ''}: queries per second, and latency in ms (p50 / p90 / p99)\n")
        print("| kind | " + " | ".join(f"{NAMES[e]} q/s | {NAMES[e]} ms" for e in runs) + " |")
        print("|---|" + "---|---|" * len(runs))
        for kind in KINDS:
            cells = []
            for e in runs:
                h = http(e, kind, c)
                cells += [fmt(h["qps"], 0), pct(h)] if h else ["n/a", "n/a"]
            print(f"| {kind} | " + " | ".join(cells) + " |")
    # Every percentile, for the appendix.
    with open(rep / "latency-full.md", "w") as f:
        f.write("| engine | kind | clients | q/s | p50 | p90 | p95 | p99 | p99.9 | max | mean | errors |\n")
        f.write("|---|---|---|---|---|---|---|---|---|---|---|---|\n")
        for e, r in runs.items():
            for h in r["http"]:
                ms = " | ".join(f"{h[k]:.2f}" if k in h else "–" for k in ["p50_ms", "p90_ms", "p95_ms", "p99_ms", "p999_ms", "max_ms", "mean_ms"])
                f.write(f"| {NAMES[e]} | {h['kind']} | {h['concurrency']} | {h['qps']:,.0f} | {ms} | {h['errors']} |\n")
    errors = {e: sum(h["errors"] for h in r["http"]) for e, r in runs.items()}
    print("\nErrors during the timed runs: " + ", ".join(f"{NAMES[e]} {n}" for e, n in errors.items()))

    # Quality.
    vec_qids = next((r["query_ids"]["vector"] for r in runs.values() if "vector" in r["query_ids"]), None)
    truth = {"vector": exact_top10(beir, vec_qids, False), "vector + filter": exact_top10(beir, vec_qids, True)} if vec_qids else {}
    # Every engine is sent the same items and years (they depend on the query's position only).
    probes = next((r["probes"]["similar items"] for r in runs.values() if "similar items" in r.get("probes", {})), None)
    similar = exact_similar(beir, probes) if probes else None
    years = next((r["probes"]["sql aggregate"] for r in runs.values() if "sql aggregate" in r.get("probes", {})), None)
    aggregates = true_aggregates(beir, [p["year"] for p in years]) if years else None
    strata = runs.get("strata")
    quality = {}
    for e, r in ((e, runs[e]) for e in scored):
        quality[e] = {}
        for kind in KINDS:
            if kind not in r["answers"]:
                continue
            ans, qids = r["answers"][kind], r["query_ids"][kind]
            q = {"n": len(ans)}
            if kind in truth:
                found = [len(set(a["keys"] or []) & truth[kind][qid]) for a, qid in zip(ans, qids)]
                q["recall_at_10"] = sum(found) / (10 * len(found))
            if kind == "similar items" and similar:
                mine = r["probes"]["similar items"]
                assert [p["item"] for p in mine] == [p["item"] for p in probes], f"{e}: other items than the first run's"
                found = [len(set(a["keys"] or []) & t) / max(1, min(10, len(t))) for a, t in zip(ans, similar)]
                q["recall_at_10"] = sum(found) / len(found)
            if kind == "sql aggregate" and aggregates:
                mine = r["probes"]["sql aggregate"]
                q["true_answers"] = sum(same_aggregate(rows_of(a), aggregates[p["year"]]) for a, p in zip(ans, mine))
                q["of"] = len(ans)
            if kind in TEXT_RANKED:
                scores = [ndcg10(a["keys"] or [], rel[qid]) for a, qid in zip(ans, qids) if qid in rel]
                scores = [x for x in scores if x is not None]
                q["ndcg_at_10"] = sum(scores) / len(scores) if scores else None
            if strata and e != "strata" and kind in strata["answers"]:
                ref = dict(zip(strata["query_ids"][kind], strata["answers"][kind]))
                pairs = [(a, ref[qid]) for a, qid in zip(ans, qids) if qid in ref]
                if kind in RANKED:
                    ov = [len(set(a["keys"] or []) & set(b["keys"] or [])) / max(1, len(b["keys"] or [])) for a, b in pairs]
                    q["overlap_with_strata"] = sum(ov) / len(ov) if ov else None
                elif kind == "sql aggregate":
                    # Scored against the true answer above (averages differ in their last digits between engines).
                    pass
                elif kind == "facet":
                    q["same_as_strata"] = sum(same_facet(rows_of(a), rows_of(b)) for a, b in pairs)
                    q["of"] = len(pairs)
                else:
                    q["same_as_strata"] = sum((a["keys"] or []) == (b["keys"] or []) for a, b in pairs)
                    q["of"] = len(pairs)
            quality[e][kind] = q
    summary["quality"] = quality

    print("\n## Answers: recall@10 against an exact search, nDCG@10 against Quora's judgments\n")
    print("Unavailable kinds are n/a; recall is for the kinds an exact search can answer.\n")
    print("| kind | " + " | ".join(f"{NAMES[e]} recall | {NAMES[e]} nDCG" for e in scored) + " |")
    print("|---|" + "---|---|" * len(scored))
    for kind in RANKED:
        cells = []
        for e in scored:
            q = quality[e].get(kind)
            cells += [fmt(q.get("recall_at_10"), 3) if q else "n/a", fmt(q.get("ndcg_at_10"), 3) if q else "n/a"]
        print(f"| {kind} | " + " | ".join(cells) + " |")
    if aggregates:
        print("\n## SQL aggregates equal to the true answer\n")
        print("| " + " | ".join(NAMES[e] for e in scored) + " |")
        print("|" + "---|" * len(scored))
        cells = []
        for e in scored:
            q = quality[e].get("sql aggregate")
            cells.append(f"{q['true_answers']} of {q['of']}" if q and "true_answers" in q else "n/a")
        print("| " + " | ".join(cells) + " |")
    if strata:
        others = [e for e in scored if e != "strata"]
        print("\n## Agreement with Strata's answers\n")
        print("| kind | " + " | ".join(NAMES[e] for e in others) + " |")
        print("|---|" + "---|" * len(others))
        for kind in (k for k in KINDS if k != "sql aggregate"):
            cells = []
            for e in others:
                q = quality[e].get(kind)
                if not q:
                    cells.append("n/a")
                elif "overlap_with_strata" in q:
                    cells.append(f"{q['overlap_with_strata']:.3f} of the top 10")
                else:
                    cells.append(f"{q.get('same_as_strata')} of {q.get('of')} equal")
            print(f"| {kind} | " + " | ".join(cells) + " |")

    # Writes (engine-compare writes): documents a second and latency per phase and clients, and how soon a write is found.
    wr = {e: load_json(rep / f"writes-{e}.json") for e in ENGINES}
    wr = {e: w for e, w in wr.items() if w}
    if wr:
        summary["writes"] = wr
        print("\n## Writes: documents a second, and latency in ms (p50 / p99)\n")
        print("| phase | clients | " + " | ".join(f"{NAMES[e]} docs/s | {NAMES[e]} ms" for e in wr) + " |")
        print("|---|---|" + "---|---|" * len(wr))
        keys = []
        for w in wr.values():
            for row in w["writes"]:
                if (row["phase"], row["concurrency"]) not in keys:
                    keys.append((row["phase"], row["concurrency"]))
        for phase, c in keys:
            cells = []
            for w in wr.values():
                row = next((r for r in w["writes"] if (r["phase"], r["concurrency"]) == (phase, c)), None)
                if row:
                    err = f" ({row['errors']} errors)" if row["errors"] else ""
                    cells += [fmt(row["docs_per_s"], 0), f"{row['p50_ms']:.2f} / {row['p99_ms']:.2f}{err}"]
                else:
                    cells += ["n/a", "n/a"]
            print(f"| {phase} | {c} | " + " | ".join(cells) + " |")
        print("\n## How soon a written document is found (ms from the write's start)\n")
        print("| engine | searchable | write returned (p50) | found (p50) | found (p90) | found (max) |")
        print("|---|---|---|---|---|---|")
        for e, w in wr.items():
            v = w["visible"]
            print(f"| {NAMES[e]} | {w['params']['searchable']} | {v['ack_p50_ms']:.1f} | {v['visible_p50_ms']:.1f} | {v['visible_p90_ms']:.1f} | {v['visible_max_ms']:.1f} |")

    # Streams (engine-compare stream): each rate's lag, the top speed, and the crash test.
    st = {e: load_json(rep / f"stream-{e}.json") for e in ENGINES}
    st = {e: x for e, x in st.items() if x}
    if st:
        summary["stream"] = st
        print("\n## Streams through Redpanda: documents written a second, and lag in ms from when a document was due (p50 / p99)\n")
        print("| phase | " + " | ".join(f"{NAMES[e]} docs/s | {NAMES[e]} lag" for e in st) + " |")
        print("|---|" + "---|---|" * len(st))
        names = []
        for x in st.values():
            for row in x["phases"]:
                if row["phase"] not in names and row["phase"] != "before the crash":
                    names.append(row["phase"])
        for name in names:
            cells = []
            for x in st.values():
                row = next((r for r in x["phases"] if r["phase"] == name), None)
                if row:
                    behind = "" if row["kept_up"] or name == "flat out" else " (fell behind)"
                    cells += [fmt(row["written_per_s"], 0), f"{row['lag_p50_ms']:.0f} / {row['lag_p99_ms']:.0f}{behind}"]
                else:
                    cells += ["not run", "not run"]
            print(f"| {name} | " + " | ".join(cells) + " |")
        print("\nLag includes reading the topic (Redpanda's part, the same for every engine, is the 'read' column below).\n")
        print("| engine | what written means | read lag p50 (ms) | crash: events written again | documents lost |")
        print("|---|---|---|---|---|")
        for e, x in st.items():
            first = x["phases"][0]
            r = x["restart"]
            print(f"| {NAMES[e]} | {x['params']['written_means']} | {fmt(first.get('fetch_lag_p50_ms'), 0)} | {r['written_again']} ({r['how']}) | {r['lost']} |")

    # CPU used during each run, from the containers' cgroups.
    cpu_lines = [json.loads(l) for l in open(rep / "cpu.jsonl")] if (rep / "cpu.jsonl").exists() else []
    for e in ENGINES:
        extra = rep / f"cpu-{e}.jsonl"
        if e.startswith("strata-") and extra.exists():
            cpu_lines += [dict(json.loads(l), engine=e, label=e, pod=f"{json.loads(l)['pod']} ({e[len('strata-'):]})") for l in open(extra)]
    if cpu_lines:
        print("\n## CPU used by each engine's containers (cgroup cpu.stat)\n")
        print("| engine | phase | pod | CPU seconds | wall seconds | average cores | throttled (s) |")
        print("|---|---|---|---|---|---|---|")
        for e in ENGINES:
            for phase, (tb, ta) in [("load", ("load-before", "load-after")), ("timed run", ("before", "after")), ("writes", ("writes-before", "writes-after")), ("stream", ("stream-before", "stream-after"))]:
                # Lines written before runs had labels name the run by its engine.
                mine = [l for l in cpu_lines if l.get("label", l["engine"]) == e]
                for pod in sorted({l["pod"] for l in mine}):
                    b = [l for l in mine if l["pod"] == pod and l["tag"] == tb]
                    a = [l for l in mine if l["pod"] == pod and l["tag"] == ta]
                    if not a or not b:
                        continue
                    b, a = b[-1], a[-1]
                    secs = (a["usage_usec"] - b["usage_usec"]) / 1e6
                    wall = max(1, a["time"] - b["time"])
                    thr = (a.get("throttled_usec", 0) - b.get("throttled_usec", 0)) / 1e6
                    summary["cpu"].setdefault(e, {}).setdefault(phase, {})[pod] = {"cpu_s": secs, "wall_s": wall, "throttled_s": thr}
                    print(f"| {NAMES[e]} | {phase} | {pod} | {secs:.0f} | {wall} | {secs / wall:.2f} | {thr:.0f} |")

    for e in runs:
        for kind, why in runs[e].get("unavailable", {}).items():
            print(f"\n- {NAMES[e]}, {kind}: unavailable: {why[:300]}")
        for kind, why in runs[e].get("differs", {}).items():
            print(f"\n- {NAMES[e]}, {kind}: {why}")
    (rep / "summary.json").write_text(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
