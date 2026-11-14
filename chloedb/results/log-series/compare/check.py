#!/usr/bin/env python3
"""Checks each store's query results (../mac-others.jsonl, "value",
"rows", "matches_in_window") against ChloeDB's (../chloedb-values.jsonl)
and prints the tables for the README: per file, per query, median and
p95 ms for each store, ChloeDB's at CORES (default 4, the cores the
Docker VM has).
  check.py [CORES]
"""
import json
import os
import sys

import common as c

STORES = ["chloedb", "clickhouse", "victorialogs", "opensearch"]
LABEL = {"chloedb": "ChloeDB", "clickhouse": "ClickHouse", "victorialogs": "VictoriaLogs", "opensearch": "OpenSearch"}
QUERIES = {
    "count_last_hour": "`COUNT(*)` last hour", "count_last_day": "`COUNT(*)` last day", "count_all": "`COUNT(*)` all",
    "hourly_7d": "counts per hour, 7 days", "minutely_by_severity_24h": "counts per minute by severity, 24 h",
    "errors_top_hosts_24h": "top 10 hosts by errors, 24 h", "match_in_hour_newest": "newest 100 with `error`, last hour",
    "p99_hourly_by_service_24h": "p99 latency per hour and service, 24 h", "p50_p95_p99_24h": "p50, p95, p99 latency, 24 h",
}
FILES = [("bgl", "BGL 4.75M"), ("hdfs", "HDFS 11.2M"), ("syn20m", "synthetic 20M")]


def lines(p):
    return [json.loads(l) for l in open(os.path.join(c.RESULTS, p)) if l.strip()]


def main():
    cores = int(sys.argv[1]) if len(sys.argv) > 1 else 4
    truth = {(r["db"], r["query"]): r for r in lines("chloedb-values.jsonl")}
    runs = {}
    for r in lines("mac-chloedb.jsonl"):
        if r.get("mode") == "query" and r.get("cores") == cores:
            runs[("chloedb", r["db"], r["query"])] = r
    loads = {}
    for r in lines("mac-chloedb.jsonl"):
        if r.get("mode") == "load":
            loads[("chloedb", r["input"][:-6])] = {"seconds": r["seconds"]}
        if r.get("mode") == "disk":
            loads[("chloedb", r["db"])]["bytes_on_disk"] = r["bytes_on_disk"]
    mism = []
    pctl = {}
    for r in lines("mac-others.jsonl"):
        if r.get("mode") == "load":
            loads[(r["store"], r["input"][:-6])] = r
            continue
        runs[(r["store"], r["db"], r["query"])] = r
        t = truth[(r["db"], r["query"])]
        diffs = []
        if r["query"].startswith("p"):
            # Percentiles: equal to 1e-9 relative is a match (exact methods
            # differ in the last bits of the interpolation); otherwise the
            # largest relative error against ChloeDB's exact value.
            if r["query"] == "p50_p95_p99_24h":
                pairs = list(zip(r["value"], t["value"]))
            else:
                mine = {(g[0], g[1]): g[2] for g in r.get("groups", [])}
                pairs = [(mine.get((g[0], g[1])), g[2]) for g in t["groups"]]
            if any(a is None for a, _ in pairs) or len(r.get("groups", t.get("groups", []))) != len(t.get("groups", [])):
                diffs.append("groups differ")
            else:
                errs = sorted(abs(a - b) / abs(b) for a, b in pairs if b)
                err = errs[-1]
                pctl[(r["store"], r["query"])] = (errs[len(errs) // 2], err)
                if err > 1e-9:
                    diffs.append(f"approximate: largest relative error {err:.2%}" + (f", values {[round(x, 1) for x in r['value']]} (exact {t['value']})" if r["query"] == "p50_p95_p99_24h" else ""))
        elif r["value"] != t["value"]:
            diffs.append(f"value {r['value']} (ChloeDB {t['value']})")
        if r["rows"] != t["rows"]:
            diffs.append(f"rows {r['rows']} (ChloeDB {t['rows']})")
        if "matches_in_window" in t and r.get("matches_in_window") != t["matches_in_window"]:
            diffs.append(f"matches {r.get('matches_in_window')} (ChloeDB {t['matches_in_window']})")
        if diffs:
            mism.append(f"- {LABEL[r['store']]}, {r['db']}, `{r['query']}`: " + "; ".join(diffs))
    for db, title in FILES:
        print(f"\n{title}, ms (median / p95):\n")
        print("| query | rows | " + " | ".join(LABEL[s] for s in STORES) + " |")
        print("|---|---|" + "---|" * len(STORES))
        for q, ql in QUERIES.items():
            if (db, q) not in truth:
                continue
            cells = []
            for s in STORES:
                r = runs.get((s, db, q))
                cells.append(f"{r['ms_median']:g} / {r['ms_p95']:g}" if r else "—")
            print(f"| {ql} | {truth[(db, q)]['rows']} | " + " | ".join(cells) + " |")
        cells = []
        for s in STORES:
            l = loads.get((s, db))
            cells.append(f"{l['seconds']:g} s, {l['bytes_on_disk'] / 1e6:.0f} MB" if l else "—")
        print("| load, bytes on disk | | " + " | ".join(cells) + " |")
    print("\nMismatches:\n")
    print("\n".join(mism) or "none")
    print("\nPercentile relative error against ChloeDB's exact:\n")
    for k, v in sorted(pctl.items()):
        print(f"- {LABEL[k[0]]} `{k[1]}`: median {v[0]:.3%}, largest {v[1]:.3%}")


main()
