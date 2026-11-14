#!/usr/bin/env python3
"""VictoriaLogs' side: DATA/NAME.shifted.jsonl (shift.py) ingested as
../../log-bytes/victorialogs.sh does (body as _msg, host and service the
stream, the rest fields), flushed and merged, then the harness's queries
in LogsQL over HTTP.

Container:
  docker run -d --name ls-vl -p 9428:9428 -v ls-vl:/vl victoriametrics/victoria-logs:v1.53.0 \
    -storageDataPath=/vl -retentionPeriod=100y -futureRetention=100y
victorialogs.py DATA NAME   (one file per fresh container; appends to ../mac-others.jsonl)
"""
import http.client
import json
import os
import subprocess
import sys
import time
import urllib.parse

import common as c

HOST, PORT = "127.0.0.1", 9428


def call(method, path, body=None, headers=None):
    conn = http.client.HTTPConnection(HOST, PORT, timeout=7200)
    conn.request(method, path, body=body, headers=headers or {})
    r = conn.getresponse()
    out = r.read()
    if r.status >= 300:
        raise SystemExit(f"{path}: {r.status} {out[:500]}")
    return out


def query(lq):
    out = call("POST", "/select/logsql/query", urllib.parse.urlencode({"query": lq}).encode(), {"Content-Type": "application/x-www-form-urlencoded"})
    return [json.loads(l) for l in out.splitlines() if l.strip()]


def active_merges():
    m = call("GET", "/metrics").decode()
    return sum(float(l.split()[-1]) for l in m.splitlines() if l.startswith("vl_active_merges"))


def load(data, name):
    path = os.path.join(data, name + ".shifted.jsonl")
    t0 = time.time()
    with open(path, "rb") as f:
        conn = http.client.HTTPConnection(HOST, PORT, timeout=7200)
        conn.request("POST", "/insert/jsonline?_time_field=ts&_msg_field=body&_stream_fields=host,service", body=f,
                     headers={"Content-Type": "application/stream+json", "Content-Length": str(os.path.getsize(path))})
        r = conn.getresponse()
        r.read()
        assert r.status < 300, r.status
    call("POST", "/internal/force_flush")
    insert_s = time.time() - t0
    call("POST", "/internal/force_merge")
    quiet = 0
    while quiet < 10:
        quiet = quiet + 1 if active_merges() == 0 else 0
        time.sleep(1)
    merge_s = time.time() - t0 - insert_s - 10
    lines = int(query("* | stats count() n")[0]["n"])
    b = vol_bytes()
    return {"mode": "load", "store": "victorialogs", "input": name + ".jsonl", "lines": lines, "seconds": round(insert_s, 1),
            "lines_per_second": round(lines / insert_s), "force_merge_seconds": round(merge_s, 1), "bytes_on_disk": b,
            "version": "v1.53.0"}, lines


def vol_bytes():
    # The image has no shell tools; read the volume from another container.
    out = subprocess.check_output(["docker", "run", "--rm", "-v", "ls-vl:/vl:ro", "--entrypoint", "du",
                                   "clickhouse/clickhouse-server:25.8", "-sb", "/vl"])
    return int(out.split()[0])


def run(data, name, out):
    r, lines = load(data, name)
    print(json.dumps(r), flush=True)
    out.write(json.dumps(r) + "\n")
    new = c.newest(name)
    since = lambda w: f"_time:>={c.iso(new - w)}"
    count = lambda res: int(res[0]["n"])
    ts = lambda s: c.parse_ts(s)
    groups = lambda keys: (lambda res: c.digest([[ts(x["_time"])] + [x[k] for k in keys] + [int(x["n"])] for x in res]))
    qs = [
        ("count_last_hour", f"{since(c.HOUR)} | stats count() n", count),
        ("count_last_day", f"{since(c.DAY)} | stats count() n", count),
        ("count_all", "* | stats count() n", count),
        ("hourly_7d", f"{since(7 * c.DAY)} | stats by (_time:1h) count() n", groups([])),
        ("minutely_by_severity_24h", f"{since(c.DAY)} | stats by (_time:1m, severity) count() n", groups(["severity"])),
        ("errors_top_hosts_24h", f"{since(c.DAY)} severity:=ERROR | stats by (host) count() n | sort by (n desc) limit 10", lambda res: [int(x["n"]) for x in res]),
        ("match_in_hour_newest", f"{since(c.HOUR)} i(error) | sort by (_time desc) limit 100 | fields _time, host, _msg", lambda res: [ts(res[0]["_time"]), ts(res[-1]["_time"])] if res else []),
    ]
    if name == "syn20m":
        qs += [
            ("p99_hourly_by_service_24h", f"{since(c.DAY)} | stats by (_time:1h, service) quantile(0.99, latency) n",
             lambda res: c.digest([[ts(x["_time"]), x["service"], float(x["n"])] for x in res])),
            ("p50_p95_p99_24h", f"{since(c.DAY)} | stats quantile(0.5, latency) p50, quantile(0.95, latency) p95, quantile(0.99, latency) p99",
             lambda res: [float(res[0][k]) for k in ("p50", "p95", "p99")]),
        ]
    for q, lq, val in qs:
        med, p95, cold, res = c.timed(lambda: query(lq))
        extra = {}
        if q.startswith("p"):
            extra["percentile"] = "quantile() (VictoriaLogs' only percentile function)"
        if q == "p99_hourly_by_service_24h":
            extra["groups"] = sorted([ts(x["_time"]), x["service"], float(x["n"])] for x in res)
        if q == "match_in_hour_newest":
            extra["matches_in_window"] = count(query(f"{since(c.HOUR)} i(error) | stats count() n"))
        c.emit(out, "victorialogs", name, lines, q, lq, med, p95, cold, len(res), val(res), **extra)


if __name__ == "__main__":
    with open(os.path.join(c.RESULTS, "mac-others.jsonl"), "a") as out:
        run(sys.argv[1], sys.argv[2], out)
