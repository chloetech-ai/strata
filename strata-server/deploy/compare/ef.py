#!/usr/bin/env python3
"""Recall and latency at each HNSW candidates value of a sweep (fanout.sh EFS).

    python3 ef.py REPORTS_DIR BEIR_DIR > ef.md

Reads run-<engine>.json (the benchmark's 64 candidates) and the sweep's
run-<engine>-ef<N>.json, each with its -pass2 and -pass3 (or .json.gz),
and prints, per engine, kind and candidates value: recall@10 against an
exact search (the kinds analyze.py scores the same way: vector, vector +
filter, similar items; the hybrids have no exact answer and show latency
only), then the median of the passes' p50 and p99 at one client and their
qps at the most clients the run timed.
"""
import re
import statistics
import sys
from pathlib import Path

from analyze import NAMES, exact_similar, exact_top10, load_json

KINDS = ["vector", "vector + filter", "similar items", "hybrid (RRF)", "hybrid (weighted)"]


def passes(d, label):
    return [r for r in (load_json(d / f"run-{label}{s}.json") for s in ["", "-pass2", "-pass3"]) if r]


def med(xs, digits):
    return f"{statistics.median(xs):.{digits}f}" if xs else "n/a"


def main():
    d, beir = Path(sys.argv[1]), Path(sys.argv[2])
    sweeps = {}
    for p in d.glob("run-*-ef*.json*"):
        m = re.fullmatch(r"run-(.+)-ef(\d+)\.json(\.gz)?", p.name)
        if m:
            sweeps.setdefault(m[1], set()).add(int(m[2]))
    if not sweeps:
        sys.exit(f"no run-<engine>-ef<N>.json in {d}")
    truth, similar = {}, None
    print("## Recall@10 and latency per HNSW candidates value\n")
    print("Latency is the median of the passes: p50 and p99 at one client, qps at the most clients timed.\n")
    print("| engine | kind | candidates | recall@10 | p50 ms | p99 ms | qps | passes |")
    print("|---|---|---|---|---|---|---|---|")
    for e in sorted(sweeps):
        labels = [(e, None)] + [(f"{e}-ef{n}", n) for n in sorted(sweeps[e])]
        for kind in KINDS:
            for label, n in labels:
                runs = passes(d, label)
                if not runs:
                    continue
                r = runs[0]
                n = n or r.get("params", {}).get("candidates", 64)
                if kind not in r.get("answers", {}):
                    continue
                ans, qids = r["answers"][kind], r["query_ids"][kind]
                recall = None
                if kind in ("vector", "vector + filter"):
                    if kind not in truth:
                        truth[kind] = exact_top10(beir, qids, kind == "vector + filter")
                    found = [len(set(a["keys"] or []) & truth[kind][q]) for a, q in zip(ans, qids)]
                    recall = sum(found) / (10 * len(found))
                elif kind == "similar items":
                    probes = r["probes"]["similar items"]
                    if similar is None:
                        similar = (probes, exact_similar(beir, probes))
                    assert [p["item"] for p in probes] == [p["item"] for p in similar[0]], f"{label}: other items than the first run's"
                    found = [len(set(a["keys"] or []) & t) / max(1, min(10, len(t))) for a, t in zip(ans, similar[1])]
                    recall = sum(found) / len(found)
                hs = [h for x in runs for h in x["http"] if h["kind"] == kind and h["requests"] > 0]
                one = [h for h in hs if h["concurrency"] == 1]
                top = max((h["concurrency"] for h in hs), default=None)
                most = [h for h in hs if h["concurrency"] == top]
                print(f"| {NAMES.get(e, e)} | {kind} | {n} | {'n/a' if recall is None else f'{recall:.3f}'} | "
                      f"{med([h['p50_ms'] for h in one], 2)} | {med([h['p99_ms'] for h in one], 2)} | "
                      f"{med([h['qps'] for h in most], 0)}{f' ({top} clients)' if top else ''} | {len(runs)} |")


if __name__ == "__main__":
    main()
