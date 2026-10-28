#!/usr/bin/env python3
"""Scores and tabulates the reports of compare.sh.

    python3 analyze.py REPORTS_DIR BEIR_DIR/quora > tables.md

Reads run-<engine>.json (timings and answers), load-<engine>.json and
build-strata-s<i>.json (load time and index size) and cpu.jsonl from
REPORTS_DIR, and the collection from BEIR_DIR/quora, and prints Markdown
tables: load time and index size; latency and throughput per kind; and the
quality of each engine's answers:

- recall@10 of the vector kinds against an exact search (dot product over
  every document, with the filter for "vector + filter");
- nDCG@10 against Quora's relevance judgments, for every ranked kind;
- overlap@10 with Strata's answers, for every ranked kind, and how many
  facet and key-lookup answers equal Strata's.

Writes REPORTS_DIR/summary.json with the same numbers. Needs numpy.
"""

import json
import math
import sys
from pathlib import Path

import numpy as np

# Reports by label (run-<label>.json): Strata at the default split of its
# 3 CPUs and, if present, at another (compare.sh run with SHARD_CPU and
# COORD_CPU set, the report renamed run-strata-<split>.json and its CPU lines
# moved to cpu-strata-<split>.jsonl).
RUNS = [
    ("strata", "Strata"),
    ("strata-coord500m", "Strata, 0.5-CPU coordinator"),
    ("es", "Elasticsearch"),
    ("solr", "Solr"),
]
ENGINES = [k for k, _ in RUNS]
NAMES = dict(RUNS)
RANKED = ["bm25", "bm25 + filter", "vector", "vector + filter", "hybrid (RRF)", "hybrid (weighted)", "boosted (bm25 × decay)"]
KINDS = RANKED + ["facet", "key lookup"]


def load_json(p):
    return json.loads(Path(p).read_text()) if Path(p).exists() else None


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
        total = max(b["total_s"] for b in builds)
        size = sum(b["file_bytes"] for b in builds)
        docs = sum(b["docs"] for b in builds)
        summary["load"]["strata"] = {"total_s": total, "index_bytes": size, "docs": docs, "shards": builds}
        print(f"| Strata | {total:.0f} | read {max(b['read_s'] for b in builds):.0f} + build {max(b['build_s'] for b in builds):.0f} | (built) | (built as one) | {size / 1e9:.2f} | {docs:,} |")
    for e in ["es", "solr"]:
        l = load_json(rep / f"load-{e}.json")
        if l:
            summary["load"][e] = {k: l.get(k) for k in ["total_s", "ingest_s", "refresh_s", "merge_s", "index_bytes", "indexed_docs", "segments", "json_bytes"]}
            print(f"| {NAMES[e]} | {l['total_s']:.0f} | send {l['ingest_s']:.0f} | {l['refresh_s']:.0f} | {l['merge_s']:.0f} | {l['index_bytes'] / 1e9:.2f} | {l['indexed_docs']:,} |")

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
    print("\n## One client: median latency in ms (p99)\n")
    print("| kind | " + " | ".join(NAMES[e] for e in runs) + " |")
    print("|---|" + "---|" * len(runs))
    for kind in KINDS:
        cells = []
        for e in runs:
            h = http(e, kind, 1)
            cells.append(f"{h['p50_ms']:.2f} ({h['p99_ms']:.1f})" if h else ("n/a" if kind in runs[e].get("unavailable", {}) else "–"))
        print(f"| {kind} | " + " | ".join(cells) + " |")
    concs = sorted({h["concurrency"] for r in runs.values() for h in r["http"]})
    print("\n## Queries per second (" + " / ".join(f"{c} clients" for c in concs) + ")\n")
    print("| kind | " + " | ".join(NAMES[e] for e in runs) + " |")
    print("|---|" + "---|" * len(runs))
    for kind in KINDS:
        cells = []
        for e in runs:
            hs = [http(e, kind, c) for c in concs]
            cells.append(" / ".join(fmt(h["qps"], 0) if h else "–" for h in hs) if any(hs) else "n/a")
        print(f"| {kind} | " + " | ".join(cells) + " |")
    print(f"\n## {concs[-1]} clients: median latency in ms (p99)\n")
    print("| kind | " + " | ".join(NAMES[e] for e in runs) + " |")
    print("|---|" + "---|" * len(runs))
    for kind in KINDS:
        cells = []
        for e in runs:
            h = http(e, kind, concs[-1])
            cells.append(f"{h['p50_ms']:.2f} ({h['p99_ms']:.1f})" if h else "n/a")
        print(f"| {kind} | " + " | ".join(cells) + " |")
    errors = {e: sum(h["errors"] for h in r["http"]) for e, r in runs.items()}
    print("\nErrors during the timed runs: " + ", ".join(f"{NAMES[e]} {n}" for e, n in errors.items()))

    # Quality.
    vec_qids = runs[next(iter(runs))]["query_ids"]["vector"]
    truth = {"vector": exact_top10(beir, vec_qids, False), "vector + filter": exact_top10(beir, vec_qids, True)}
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
            if kind in RANKED:
                scores = [ndcg10(a["keys"] or [], rel[qid]) for a, qid in zip(ans, qids) if qid in rel]
                scores = [x for x in scores if x is not None]
                q["ndcg_at_10"] = sum(scores) / len(scores) if scores else None
            if strata and e != "strata" and kind in strata["answers"]:
                ref = dict(zip(strata["query_ids"][kind], strata["answers"][kind]))
                pairs = [(a, ref[qid]) for a, qid in zip(ans, qids) if qid in ref]
                if kind in RANKED:
                    ov = [len(set(a["keys"] or []) & set(b["keys"] or [])) / max(1, len(b["keys"] or [])) for a, b in pairs]
                    q["overlap_with_strata"] = sum(ov) / len(ov) if ov else None
                elif kind == "facet":
                    q["same_as_strata"] = sum(rows_of(a) == rows_of(b) for a, b in pairs)
                    q["of"] = len(pairs)
                else:
                    q["same_as_strata"] = sum((a["keys"] or []) == (b["keys"] or []) for a, b in pairs)
                    q["of"] = len(pairs)
            quality[e][kind] = q
    summary["quality"] = quality

    print("\n## Answers: recall@10 against an exact search, nDCG@10 against Quora's judgments\n")
    print("| kind | " + " | ".join(f"{NAMES[e]} recall | {NAMES[e]} nDCG" for e in scored) + " |")
    print("|---|" + "---|---|" * len(scored))
    for kind in RANKED:
        cells = []
        for e in scored:
            q = quality[e].get(kind)
            cells += [fmt(q.get("recall_at_10"), 3) if q else "n/a", fmt(q.get("ndcg_at_10"), 3) if q else "n/a"]
        print(f"| {kind} | " + " | ".join(cells) + " |")
    if strata:
        others = [e for e in scored if e != "strata"]
        print("\n## Agreement with Strata's answers\n")
        print("| kind | " + " | ".join(NAMES[e] for e in others) + " |")
        print("|---|" + "---|" * len(others))
        for kind in KINDS:
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

    # CPU used during each run, from the containers' cgroups.
    cpu_lines = [json.loads(l) for l in open(rep / "cpu.jsonl")] if (rep / "cpu.jsonl").exists() else []
    for e in ENGINES:
        extra = rep / f"cpu-{e}.jsonl"
        if e.startswith("strata-") and extra.exists():
            cpu_lines += [dict(json.loads(l), engine=e, pod=f"{json.loads(l)['pod']} ({e[len('strata-'):]})") for l in open(extra)]
    if cpu_lines:
        print("\n## CPU used by each engine's containers (cgroup cpu.stat)\n")
        print("| engine | phase | pod | CPU seconds | wall seconds | average cores | throttled (s) |")
        print("|---|---|---|---|---|---|---|")
        for e in ENGINES:
            for phase, (tb, ta) in [("load", ("load-before", "load-after")), ("timed run", ("before", "after"))]:
                for pod in sorted({l["pod"] for l in cpu_lines if l["engine"] == e}):
                    b = [l for l in cpu_lines if l["pod"] == pod and l["tag"] == tb]
                    a = [l for l in cpu_lines if l["pod"] == pod and l["tag"] == ta]
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
