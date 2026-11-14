#!/usr/bin/env python3
"""What ChloeDB's queries return, in the shapes the other runners record
("value"), so their counts can be checked against ChloeDB's: runs each
query of ../mac-chloedb.jsonl once with the chloedb shell (read-only) on
DATA/NAME, and writes ../chloedb-values.jsonl. The match query is run
with only ts selected (bodies may hold the shell's column separator).
  chloedb_values.py DATA NAME...
"""
import json
import os
import subprocess
import sys

import common as c

CLI = os.path.join(c.RESULTS, "..", "..", "target", "release", "chloedb")


def sql(db, s):
    out = subprocess.check_output([CLI, "--read-only", db, s], text=True).splitlines()
    return [l.split(" | ") for l in out[1:-1]]  # header, rows, "(N rows, T ms)"


def main():
    data = sys.argv[1]
    with open(os.path.join(c.RESULTS, "chloedb-values.jsonl"), "w") as out:
        for name in sys.argv[2:]:
            db = os.path.join(data, name)
            for q, r in c.chloedb_runs(name).items():
                s = r["sql"]
                res = sql(db, s if q != "match_in_hour_newest" else s.replace("SELECT ts, host, body", "SELECT ts"))
                extra = {}
                if q.startswith("count"):
                    v = int(res[0][0])
                elif q == "hourly_7d":
                    v = c.digest([[c.parse_ts(a), int(n)] for a, n in res])
                elif q == "minutely_by_severity_24h":
                    v = c.digest([[c.parse_ts(a), sev, int(n)] for a, sev, n in res])
                elif q == "errors_top_hosts_24h":
                    v = [int(n) for _, n in res]
                elif q == "match_in_hour_newest":
                    v = [c.parse_ts(res[0][0]), c.parse_ts(res[-1][0])] if res else []
                    cnt = s.replace("SELECT ts, host, body", "SELECT COUNT(*)").split(" ORDER BY")[0]
                    extra["matches_in_window"] = int(sql(db, cnt)[0][0])
                elif q == "p99_hourly_by_service_24h":
                    v = c.digest([[c.parse_ts(a), svc, float(p)] for a, svc, p in res])
                    extra["groups"] = sorted([c.parse_ts(a), svc, float(p)] for a, svc, p in res)
                elif q == "p50_p95_p99_24h":
                    v = [float(x) for x in res[0]]
                line = json.dumps({"store": "chloedb", "db": name, "query": q, "rows": len(res), "value": v, **extra})
                print(line)
                out.write(line + "\n")


main()
