#!/usr/bin/env python3
"""After rollups_chloedb.sh: reads back from ChloeDB's database what the
other stores need to hold the same rows and answer over the same windows,
and what ChloeDB answered, to check them against.

- DATA/rollup-meta.json: the shift `load` applied to the synthetic log
  (ChloeDB's newest loaded row minus the file's newest ts), the `now` the
  harness's 50,000 new lines were stamped from, and the windows' starts as
  the harness's rollup queries give them.
- DATA/fresh50k.jsonl: those 50,000 lines, made as the harness makes them.
- DATA/syn20m.shifted.tsv and DATA/fresh50k.tsv: the same rows for
  TimescaleDB's COPY (ts as RFC 3339 with microseconds).
- ../chloedb-rollup-values.jsonl: the rows of ChloeDB's eight queries
  (four on the rollup, four on the table).
  rollups_prep.py DATA
"""
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.dirname(HERE)
CLI = os.path.join(RESULTS, "..", "..", "target", "release", "chloedb")
sys.path.insert(0, HERE)
from rollcommon import iso, parse_ts, harness_lines  # noqa: E402

data = sys.argv[1]
db = os.path.join(data, "syn20m")


def sql(s):
    out = subprocess.check_output([CLI, "--read-only", db, s], text=True).splitlines()
    return [l.split(" | ") for l in out[1:-1]]


orig_newest = parse_ts(sql("SELECT MAX(ts) FROM logs WHERE body <> 'request served ok'")[0][0])
fresh_newest = parse_ts(sql("SELECT MAX(ts) FROM logs WHERE body = 'request served ok'")[0][0])
fresh_now = fresh_newest + 1200  # the harness stamps line i at now - 60 s + i * 1.2 ms
tsre = re.compile(rb'"ts":(-?\d+)')
file_max = max(int(tsre.search(l).group(1)) for l in open(os.path.join(data, "syn20m.jsonl"), "rb"))
shift = orig_newest - file_max

queries = {r["query"]: r for r in harness_lines() if "sql_rollup" in r}
since = {}
for q, r in queries.items():
    since[q] = parse_ts(r["sql_rollup"].split("bucket >= '")[1].split("'")[0])
meta = {"shift_us": shift, "fresh_now_us": fresh_now, "since_us": since, "orig_newest_us": orig_newest}
json.dump(meta, open(os.path.join(data, "rollup-meta.json"), "w"), indent=1)

fresh = []
for i in range(50_000):
    fresh.append({"ts": fresh_now - 60_000_000 + i * 1200, "host": "host-%03d" % (i % 500), "service": "svc-%02d" % (i % 20),
                  "severity": ["INFO", "WARN", "ERROR"][i % 3], "pid": 1000 + i % 4000, "latency": i % 9000, "body": "request served ok"})
with open(os.path.join(data, "fresh50k.jsonl"), "w") as f:
    for r in fresh:
        f.write(json.dumps(r) + "\n")


def tsv(r, sh):
    esc = lambda v: "\\N" if v is None else str(v).replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n")
    return "\t".join([iso(r["ts"] + sh), esc(r.get("host")), esc(r.get("service")), esc(r.get("severity")), esc(r.get("pid")), esc(r.get("latency")), esc(r.get("body"))]) + "\n"


with open(os.path.join(data, "fresh50k.tsv"), "w") as f:
    for r in fresh:
        f.write(tsv(r, 0))
with open(os.path.join(data, "syn20m.jsonl")) as src, open(os.path.join(data, "syn20m.shifted.tsv"), "w") as f:
    for line in src:
        f.write(tsv(json.loads(line), shift))

with open(os.path.join(RESULTS, "chloedb-rollup-values.jsonl"), "w") as out:
    for q, r in queries.items():
        for on in ("rollup", "table"):
            rows = sql(r["sql_" + on])
            rec = {"store": "chloedb", "query": q, "on": on, "rows": rows}
            if q == "by_severity_5m_24h" and on == "table":
                rec = {**rec, "rows": None, "n_rows": len(rows)}  # by minute: only its number of rows is compared
            out.write(json.dumps(rec) + "\n")
print(json.dumps(meta))
