"""Shared by the comparison runners: the windows ChloeDB's harness used,
timing, and one JSON line per query in the harness's shape plus "store".

Every store gets the same rows at the same times: each file's timestamps
are shifted by (ChloeDB's newest row - the file's newest ts), which is
the shift `log_series_bench load` applied (ts + (now - max ts), "now"
being the moment ChloeDB loaded it). ChloeDB's newest row is read back
from the `count_last_hour` SQL the harness printed (its window starts
an hour before it). So windows, hour and minute buckets line up exactly.
"""
import datetime
import hashlib
import json
import os
import re
import time

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.dirname(HERE)
HOUR = 3_600_000_000
DAY = 24 * HOUR
RUNS = 7
NAMES = {"bgl.jsonl": "bgl", "hdfs.jsonl": "hdfs", "syn20m.jsonl": "syn20m"}


def parse_ts(s):
    d = datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))
    if d.tzinfo is None:
        d = d.replace(tzinfo=datetime.timezone.utc)
    return int(d.timestamp()) * 1_000_000 + d.microsecond


def iso(us):
    d = datetime.datetime.fromtimestamp(us // 1_000_000, datetime.timezone.utc)
    return d.strftime("%Y-%m-%dT%H:%M:%S") + ".%06dZ" % (us % 1_000_000)


def chloedb_runs(name):
    """ChloeDB's 8-core query lines for database NAME (e.g. bgl)."""
    out = {}
    with open(os.path.join(RESULTS, "mac-chloedb.jsonl")) as f:
        for line in f:
            r = json.loads(line)
            if r.get("mode") == "query" and r.get("db") == name and r.get("cores", 8) == 8:
                out[r["query"]] = r
    return out


def newest(name):
    sql = chloedb_runs(name)["count_last_hour"]["sql"]
    return parse_ts(sql.split("ts >= '")[1].split("'")[0]) + HOUR


def file_max_ts(path):
    """The file's newest ts, cached beside it."""
    cache = path + ".maxts"
    if os.path.exists(cache):
        return int(open(cache).read())
    ts = re.compile(rb'"ts":(-?\d+)')
    m = max(int(ts.search(line).group(1)) for line in open(path, "rb"))
    open(cache, "w").write(str(m))
    return m


def shift(path, name):
    return newest(name) - file_max_ts(path)


def timed(fn, runs=RUNS):
    """Runs fn RUNS times; the first is the cold run. Returns (median, p95,
    cold, last result) in ms as the harness takes them."""
    ms, res = [], None
    for _ in range(runs):
        t = time.perf_counter()
        res = fn()
        ms.append((time.perf_counter() - t) * 1e3)
    cold = ms[0]
    s = sorted(ms)
    p95 = s[min(-(-runs * 95 // 100), runs) - 1]
    return round(s[runs // 2], 2), round(p95, 2), round(cold, 2), res


def digest(groups):
    """A short hash of sorted (key..., count) tuples, to compare bucketings."""
    h = hashlib.sha1(json.dumps(sorted(groups), separators=(",", ":")).encode()).hexdigest()
    return h[:12]


def emit(out, store, name, lines, query, sql, med, p95, cold, rows, value, **extra):
    r = {"mode": "query", "store": store, "db": name, "lines": lines, "query": query, "sql": sql,
         "runs": RUNS, "ms_median": med, "ms_p95": p95, "ms_cold": cold, "rows": rows, "value": value}
    r.update(extra)
    line = json.dumps(r)
    print(line, flush=True)
    out.write(line + "\n")
    out.flush()
