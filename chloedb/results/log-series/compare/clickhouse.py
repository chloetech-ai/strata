#!/usr/bin/env python3
"""ClickHouse 25.8's side: the table the OpenTelemetry Collector's
ClickHouse exporter creates for logs (as ../../log-bytes/clickhouse.sh,
partitioned by day, host in ResourceAttributes, pid and latency in
LogAttributes), loaded from DATA/NAME.jsonl with every ts shifted as
ChloeDB's load shifted it, then the harness's queries over HTTP.

Container (no published ports but 8123, a named volume, the JSON lines at /data):
  docker run -d --name ls-ch -p 8123:8123 -v ls-ch:/var/lib/clickhouse -v DATA:/data:ro \
    -v compare/ch_user_files.xml:/etc/clickhouse-server/config.d/user_files.xml:ro \
    -e CLICKHOUSE_SKIP_USER_SETUP=1 clickhouse/clickhouse-server:25.8
clickhouse.py DATA NAME...   (appends to ../mac-others.jsonl)
"""
import json
import os
import sys
import time
import urllib.parse
import urllib.request

import common as c

URL = "http://127.0.0.1:8123/"


def q(sql, fmt="JSONCompact"):
    body = (sql + (f" FORMAT {fmt}" if fmt else "")).encode()
    params = urllib.parse.urlencode({"max_partitions_per_insert_block": 100000, "use_query_cache": 0})
    with urllib.request.urlopen(urllib.request.Request(URL + "?" + params, data=body), timeout=7200) as r:
        out = r.read()
    return json.loads(out) if fmt else out


def rows(sql):
    return q(sql)["data"]


def wait_merges():
    while int(rows("SELECT count() FROM system.merges")[0][0]) > 0:
        time.sleep(1)


def load(data, name, t):
    sh = c.shift(os.path.join(data, name + ".jsonl"), name)
    q(f"DROP TABLE IF EXISTS {t} SYNC", None)
    q(f"""CREATE TABLE {t} (
      Timestamp DateTime64(9) CODEC(Delta(8), ZSTD(1)),
      TimestampTime DateTime DEFAULT toDateTime(Timestamp),
      SeverityText LowCardinality(String) CODEC(ZSTD(1)),
      SeverityNumber UInt8,
      ServiceName LowCardinality(String) CODEC(ZSTD(1)),
      Body String CODEC(ZSTD(1)),
      ResourceAttributes Map(LowCardinality(String), String) CODEC(ZSTD(1)),
      LogAttributes Map(LowCardinality(String), String) CODEC(ZSTD(1)),
      INDEX idx_res_attr_key mapKeys(ResourceAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
      INDEX idx_res_attr_value mapValues(ResourceAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
      INDEX idx_log_attr_key mapKeys(LogAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
      INDEX idx_log_attr_value mapValues(LogAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
      INDEX idx_body Body TYPE tokenbf_v1(32768, 3, 0) GRANULARITY 8
    ) ENGINE = MergeTree PARTITION BY toDate(TimestampTime)
    ORDER BY (ServiceName, TimestampTime, Timestamp) SETTINGS index_granularity = 8192, ttl_only_drop_parts = 1""", None)
    t0 = time.time()
    q(f"""INSERT INTO {t} (Timestamp, SeverityText, ServiceName, Body, ResourceAttributes, LogAttributes)
      SELECT fromUnixTimestamp64Micro(ts + {sh}), severity, service, body, map('host.name', host),
        mapFilter((k, v) -> v != '', map('process.pid', ifNull(toString(pid), ''), 'latency', ifNull(toString(latency), '')))
      FROM file('{name}.jsonl', JSONEachRow, 'ts Int64, host String, service String, severity String, pid Nullable(Int64), latency Nullable(Int64), body String')""", None)
    insert_s = time.time() - t0
    wait_merges()
    settle_s = time.time() - t0 - insert_s
    lines, b, parts = rows(f"SELECT sum(rows), sum(bytes_on_disk), count() FROM system.parts WHERE active AND table = '{t}'")[0]
    r = {"mode": "load", "store": "clickhouse", "input": name + ".jsonl", "lines": int(lines), "seconds": round(insert_s, 1),
         "lines_per_second": round(int(lines) / insert_s), "background_merge_seconds_after": round(settle_s, 1),
         "parts": int(parts), "bytes_on_disk": int(b), "shift_us": sh, "version": rows("SELECT version()")[0][0]}
    return r, int(lines)


def run(data, name, out):
    t = "otel_logs_" + name
    r, lines = load(data, name, t)
    print(json.dumps(r), flush=True)
    out.write(json.dumps(r) + "\n")
    new = c.newest(name)
    since = lambda w: f"TimestampTime >= toDateTime({(new - w) // 1_000_000}) AND Timestamp >= fromUnixTimestamp64Micro({new - w})"
    host = "ResourceAttributes['host.name']"
    lat = "toInt64(LogAttributes['latency'])"
    us = lambda col: f"toUnixTimestamp64Micro(toDateTime64({col}, 6))"
    count = lambda res: int(res[0][0])
    groups = lambda res: c.digest([[int(x) if str(x).lstrip('-').isdigit() else x for x in row] for row in res])
    qs = [
        ("count_last_hour", f"SELECT count() FROM {t} WHERE {since(c.HOUR)}", count),
        ("count_last_day", f"SELECT count() FROM {t} WHERE {since(c.DAY)}", count),
        ("count_all", f"SELECT count() FROM {t}", count),
        ("hourly_7d", f"SELECT {us('toStartOfHour(Timestamp)')} AS h, count() FROM {t} WHERE {since(7 * c.DAY)} GROUP BY h", groups),
        ("minutely_by_severity_24h", f"SELECT {us('toStartOfMinute(Timestamp)')} AS m, SeverityText, count() FROM {t} WHERE {since(c.DAY)} GROUP BY m, SeverityText", groups),
        ("errors_top_hosts_24h", f"SELECT {host} AS host, count() AS n FROM {t} WHERE {since(c.DAY)} AND SeverityText = 'ERROR' GROUP BY host ORDER BY n DESC LIMIT 10", lambda res: [int(x[1]) for x in res]),
        ("match_in_hour_newest", f"SELECT {us('Timestamp')}, {host}, Body FROM {t} WHERE hasTokenCaseInsensitive(Body, 'error') AND {since(c.HOUR)} ORDER BY Timestamp DESC LIMIT 100", lambda res: [int(res[0][0]), int(res[-1][0])] if res else []),
    ]
    if name == "syn20m":
        qs += [
            ("p99_hourly_by_service_24h", f"SELECT {us('toStartOfHour(Timestamp)')} AS h, ServiceName, quantileExactInclusive(0.99)({lat}) FROM {t} WHERE {since(c.DAY)} GROUP BY h, ServiceName",
             lambda res: c.digest([[int(x[0]), x[1], float(x[2])] for x in res])),
            ("p50_p95_p99_24h", f"SELECT quantilesExactInclusive(0.5, 0.95, 0.99)({lat}) FROM {t} WHERE {since(c.DAY)}", lambda res: [float(x) for x in res[0][0]]),
        ]
    for query, sql, val in qs:
        med, p95, cold, res = c.timed(lambda: rows(sql))
        extra = {}
        if query.startswith("p"):
            extra["percentile"] = "quantileExactInclusive (exact, interpolated like PERCENTILE_CONT)"
        if query == "p99_hourly_by_service_24h":
            extra["groups"] = sorted([int(x[0]), x[1], float(x[2])] for x in res)
        if query == "match_in_hour_newest":
            extra["matches_in_window"] = count(rows(f"SELECT count() FROM {t} WHERE hasTokenCaseInsensitive(Body, 'error') AND {since(c.HOUR)}"))
        c.emit(out, "clickhouse", name, lines, query, sql, med, p95, cold, len(res), val(res), **extra)
    q(f"DROP TABLE {t} SYNC", None)


if __name__ == "__main__":
    data = sys.argv[1]
    with open(os.path.join(c.RESULTS, "mac-others.jsonl"), "a") as out:
        for n in sys.argv[2:]:
            run(data, n, out)
