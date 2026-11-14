"""Shared by the rollup runners: times, ChloeDB's harness lines, timing
and output."""
import datetime
import json
import os
import time

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.dirname(HERE)
OUT = os.path.join(RESULTS, "mac-rollups.jsonl")
RUNS = 7
MACHINE = "MacBook Air (Mac14,2), Apple M2, 8 cores (4P+4E), 24 GB"
QUERIES = ["hourly_7d", "by_severity_5m_24h", "mean_latency_hourly_by_service_24h", "max_latency_hourly_24h"]


def iso(us):
    d = datetime.datetime.fromtimestamp(us // 1_000_000, datetime.timezone.utc)
    return d.strftime("%Y-%m-%dT%H:%M:%S") + ".%06dZ" % (us % 1_000_000)


def parse_ts(s):
    s = s.strip().replace(" ", "T")
    if s.endswith("+00"):
        s = s[:-3] + "Z"
    d = datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))
    if d.tzinfo is None:
        d = d.replace(tzinfo=datetime.timezone.utc)
    return int(d.timestamp()) * 1_000_000 + d.microsecond


def harness_lines():
    if not os.path.exists(OUT):
        return []
    return [r for r in map(json.loads, open(OUT)) if r.get("store") == "chloedb" and r.get("mode") == "rollup"]


def timed(fn, runs=RUNS):
    ms, res = [], None
    for _ in range(runs):
        t = time.perf_counter()
        res = fn()
        ms.append((time.perf_counter() - t) * 1e3)
    s = sorted(ms)
    return round(s[runs // 2], 2), round(s[min(-(-runs * 95 // 100), runs) - 1], 2), round(ms[0], 2), res


def emit(rec):
    rec.setdefault("machine", MACHINE)
    line = json.dumps(rec)
    print(line, flush=True)
    with open(OUT, "a") as f:
        f.write(line + "\n")
