#!/usr/bin/env python3
"""Checks ClickHouse's and TimescaleDB's answers (../mac-rollups.jsonl)
against ChloeDB's (../chloedb-rollup-values.jsonl): counts and maxima
exactly, means to the 4 decimals ChloeDB's shell prints; the table's per-minute query by its
number of rows. Prints the README's tables.
  rollups_check.py
"""
import json
import os

import rollcommon as c

LABEL = {"chloedb": "ChloeDB", "clickhouse": "ClickHouse", "timescaledb": "TimescaleDB"}
QL = {"hourly_7d": "counts per hour, 7 days", "by_severity_5m_24h": "counts per 5 min by severity, 24 h",
      "mean_latency_hourly_by_service_24h": "mean latency per hour and service, 24 h", "max_latency_hourly_24h": "max latency per hour, 24 h"}


def norm(rows):
    out = {}
    for r in rows:
        k0 = r[0]
        k0 = int(k0) // 1 if str(k0).lstrip("-").isdigit() else c.parse_ts(k0) // 1_000_000
        out[(k0, *[str(x) for x in r[1:-1]])] = float(r[-1])
    return out


def same(a, b, q):
    if a.keys() != b.keys():
        return f"groups differ ({len(a)} against {len(b)})"
    # ChloeDB's shell prints a float with 4 decimals: means match to 5e-5.
    tol = 5e-5 if q.startswith("mean") else 0
    bad = [k for k in a if abs(a[k] - b[k]) > tol]
    return f"{len(bad)} of {len(a)} values differ, e.g. {bad[0]}: {a[bad[0]]} against {b[bad[0]]}" if bad else None


def main():
    vals = [json.loads(l) for l in open(os.path.join(c.RESULTS, "chloedb-rollup-values.jsonl"))]
    truth = {(r["query"], r["on"]): r["rows"] for r in vals}
    # The per-minute table query's rows are kept as their number only.
    n_rows = {(r["query"], r["on"]): r.get("n_rows", len(r["rows"] or [])) for r in vals}
    recs = [json.loads(l) for l in open(c.OUT)]
    q = {(r["store"], r["query"]): r for r in recs if r.get("query")}
    steps = {(r["store"], r["step"]): r for r in recs if r.get("step")}
    loads = {r["store"]: r for r in recs if r.get("mode") == "load"}
    disk = {r["store"]: r for r in recs if r.get("mode") == "disk"}
    mism = []
    for (store, name), r in q.items():
        if store == "chloedb":
            continue
        d = same(norm(r["answer_rollup"]), norm(truth[(name, "rollup")]), name)
        if d:
            mism.append(f"- {LABEL[store]}, `{name}` on the rollup: {d}")
        if r.get("answer_table") is not None:
            d = same(norm(r["answer_table"]), norm(truth[(name, "table")]), name)
            if d:
                mism.append(f"- {LABEL[store]}, `{name}` on the table: {d}")
        elif r["table_rows"] != n_rows[(name, "table")]:
            mism.append(f"- {LABEL[store]}, `{name}` on the table: {r['table_rows']} rows (ChloeDB {n_rows[(name, 'table')]})")
    stores = ["chloedb", "clickhouse", "timescaledb"]
    print("| query | rows | " + " | ".join(f"{LABEL[s]} rollup | {LABEL[s]} table" for s in stores) + " |")
    print("|---|---|" + "---|---|" * len(stores))
    for name in c.QUERIES:
        cells = []
        for s in stores:
            r = q.get((s, name))
            cells += [f"{r['rollup_ms_median']:g} / {r['rollup_ms_p95']:g}", f"{r['table_ms_median']:g} / {r['table_ms_p95']:g}"] if r else ["—", "—"]
        print(f"| {QL[name]} | {len(truth[(name, 'rollup')])} | " + " | ".join(cells) + " |")
    print()
    print("| | " + " | ".join(LABEL[s] for s in stores) + " |")
    print("|---|" + "---|" * len(stores))
    row = lambda label, f: print(f"| {label} | " + " | ".join(f(s) for s in stores) + " |")
    g = lambda s, step, k="seconds": (lambda r: f"{r[k]:g} s" if r and r.get(k) is not None else "—")(steps.get((s, step)))
    row("load 20M lines", lambda s: f"{loads[s]['seconds']:g} s" if s in loads else "—")
    row("build the rollup", lambda s: g(s, "create"))
    row("refresh, nothing new", lambda s: g(s, "refresh_nothing_new"))
    row("50,000 new lines in the rollup", lambda s: g(s, "refresh_50k_new") if s == "chloedb" else g(s, "50k_new_visible"))
    row("rollup rows", lambda s: str(steps.get((s, "create"), {}).get("rollup_rows", "—")))
    def rb(s):
        if s == "chloedb":
            d = disk.get("chloedb")
            return f"{d['rollup_bytes'] / 1e6:.1f} MB (with the 50,000 new lines)" if d else "—"
        r = steps.get((s, "create"))
        return f"{r['rollup_bytes_on_disk'] / 1e6:.1f} MB" if r else "—"
    row("rollup on disk", rb)
    row("table on disk", lambda s: (f"{disk['chloedb']['table_bytes'] / 1e6:.0f} MB" if s == "chloedb" and "chloedb" in disk else
                                    f"{loads[s]['bytes_on_disk'] / 1e6:.0f} MB" if s in loads and "bytes_on_disk" in loads[s] else "—"))
    print("\nMismatches:\n")
    print("\n".join(mism) or "none")


main()
