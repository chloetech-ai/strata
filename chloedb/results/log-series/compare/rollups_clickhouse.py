#!/usr/bin/env python3
"""ClickHouse 25.8's side of the rollup comparison. The logs table is the
OpenTelemetry exporter's, as in this morning's comparison (partitioned by
day, host in ResourceAttributes, pid and latency in LogAttributes), loaded
from DATA/syn20m.jsonl with ChloeDB's shift. Then a materialized view
into an AggregatingMergeTree by toStartOfFiveMinutes, service and
severity with count, sum, min and max of latency (SimpleAggregateFunction
columns), backfilled with INSERT ... SELECT; then the harness's 50,000
new lines; then the four queries on the view and on the table.

Container:
  docker run -d --name ls-ch -p 8123:8123 -v ls-ch:/var/lib/clickhouse -v DATA:/data:ro \
    -v compare/ch_user_files.xml:/etc/clickhouse-server/config.d/user_files.xml:ro \
    -e CLICKHOUSE_SKIP_USER_SETUP=1 -e CLICKHOUSE_DO_NOT_CHOWN=1 clickhouse/clickhouse-server:25.8
rollups_clickhouse.py DATA
"""
import json
import os
import sys
import time
import urllib.parse
import urllib.request

import rollcommon as c

URL = "http://127.0.0.1:8123/"
T, V = "otel_logs", "logs_5m"


def q(sql, fmt="JSONCompact"):
    body = (sql + (f" FORMAT {fmt}" if fmt else "")).encode()
    params = urllib.parse.urlencode({"max_partitions_per_insert_block": 100000, "use_query_cache": 0})
    with urllib.request.urlopen(urllib.request.Request(URL + "?" + params, data=body), timeout=7200) as r:
        out = r.read()
    return json.loads(out)["data"] if fmt else None


def wait_merges():
    while int(q("SELECT count() FROM system.merges")[0][0]) > 0:
        time.sleep(1)


def size(table):
    r = q(f"SELECT sum(rows), sum(bytes_on_disk), count() FROM system.parts WHERE active AND table = '{table}'")[0]
    return int(r[0]), int(r[1]), int(r[2])


def insert_from(file, shift):
    q(f"""INSERT INTO {T} (Timestamp, SeverityText, ServiceName, Body, ResourceAttributes, LogAttributes)
      SELECT fromUnixTimestamp64Micro(ts + {shift}), severity, service, body, map('host.name', host),
        map('process.pid', toString(pid), 'latency', toString(latency))
      FROM file('{file}', JSONEachRow, 'ts Int64, host String, service String, severity String, pid Int64, latency Int64, body String')""", None)


def main(data):
    meta = json.load(open(os.path.join(data, "rollup-meta.json")))
    version = q("SELECT version()")[0][0]
    base = {"store": "clickhouse", "version": version, "db": "syn20m"}
    for t in (V + "_mv", V, T):
        q(f"DROP TABLE IF EXISTS {t} SYNC", None)
    q(f"""CREATE TABLE {T} (
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
    insert_from("syn20m.jsonl", meta["shift_us"])
    load_s = time.time() - t0
    wait_merges()
    rows, b, parts = size(T)
    c.emit({**base, "mode": "load", "lines": rows, "seconds": round(load_s, 1), "bytes_on_disk": b, "parts": parts})

    lat = "toInt64(LogAttributes['latency'])"
    select = f"""SELECT toStartOfFiveMinutes(Timestamp) AS bucket, ServiceName AS service, SeverityText AS severity,
        count() AS cnt, sum({lat}) AS sum_latency, min({lat}) AS min_latency, max({lat}) AS max_latency
      FROM {T} GROUP BY bucket, service, severity"""
    q(f"""CREATE TABLE {V} (bucket DateTime, service LowCardinality(String), severity LowCardinality(String),
        cnt SimpleAggregateFunction(sum, UInt64), sum_latency SimpleAggregateFunction(sum, Int64),
        min_latency SimpleAggregateFunction(min, Int64), max_latency SimpleAggregateFunction(max, Int64))
      ENGINE = AggregatingMergeTree PARTITION BY toDate(bucket) ORDER BY (bucket, service, severity)""", None)
    t0 = time.time()
    q(f"CREATE MATERIALIZED VIEW {V}_mv TO {V} AS {select}", None)
    create_s = time.time() - t0
    t0 = time.time()
    q(f"INSERT INTO {V} {select}", None)  # the backfill: a view only sees inserts made after it exists
    backfill_s = time.time() - t0
    wait_merges()
    vrows, vb, vparts = size(V)
    c.emit({**base, "mode": "rollup", "step": "create", "seconds": round(create_s + backfill_s, 3), "create_view_seconds": round(create_s, 3),
            "backfill_seconds": round(backfill_s, 3), "rollup_rows": vrows, "rollup_bytes_on_disk": vb, "rollup_parts": vparts,
            "table_bytes_on_disk": b})

    # The harness's 50,000 new lines (rollups_prep.py), in one insert: the
    # view is updated in the same insert, so they are in it once it returns.
    before = int(q(f"SELECT sum(cnt) FROM {V}")[0][0])
    t0 = time.time()
    insert_from("fresh50k.jsonl", 0)
    ins_s = time.time() - t0
    waited = 0.0
    while int(q(f"SELECT sum(cnt) FROM {V}")[0][0]) < before + 50_000:
        time.sleep(0.05)
        waited = time.time() - t0 - ins_s
    c.emit({**base, "mode": "rollup", "step": "50k_new_visible", "seconds": round(time.time() - t0, 3), "insert_seconds": round(ins_s, 3),
            "waited_after_insert_seconds": round(waited, 3), "how": "materialized view, updated within the INSERT"})

    s = meta["since_us"]
    w = lambda q_, col="bucket": f"{col} >= toDateTime({s[q_] // 1_000_000})"
    tw = lambda q_: f"TimestampTime >= toDateTime({s[q_] // 1_000_000}) AND Timestamp >= fromUnixTimestamp64Micro({s[q_]})"
    pairs = {
        "hourly_7d": (f"SELECT toUnixTimestamp(toStartOfHour(bucket)) AS h, sum(cnt) FROM {V} WHERE {w('hourly_7d')} GROUP BY h",
                      f"SELECT toUnixTimestamp(toStartOfHour(Timestamp)) AS h, count() FROM {T} WHERE {tw('hourly_7d')} GROUP BY h"),
        "by_severity_5m_24h": (f"SELECT toUnixTimestamp(bucket) AS b, severity, sum(cnt) FROM {V} WHERE {w('by_severity_5m_24h')} GROUP BY b, severity",
                               f"SELECT toUnixTimestamp(toStartOfMinute(Timestamp)) AS m, SeverityText, count() FROM {T} WHERE {tw('by_severity_5m_24h')} GROUP BY m, SeverityText"),
        "mean_latency_hourly_by_service_24h": (
            f"SELECT toUnixTimestamp(toStartOfHour(bucket)) AS h, service, sum(sum_latency) / sum(cnt) FROM {V} WHERE {w('mean_latency_hourly_by_service_24h')} GROUP BY h, service",
            f"SELECT toUnixTimestamp(toStartOfHour(Timestamp)) AS h, ServiceName, avg({lat}) FROM {T} WHERE {tw('mean_latency_hourly_by_service_24h')} GROUP BY h, ServiceName"),
        "max_latency_hourly_24h": (f"SELECT toUnixTimestamp(toStartOfHour(bucket)) AS h, max(max_latency) FROM {V} WHERE {w('max_latency_hourly_24h')} GROUP BY h",
                                   f"SELECT toUnixTimestamp(toStartOfHour(Timestamp)) AS h, max({lat}) FROM {T} WHERE {tw('max_latency_hourly_24h')} GROUP BY h"),
    }
    for name in c.QUERIES:
        on_v, on_t = pairs[name]
        vm, vp, vc, vr = c.timed(lambda: q(on_v))
        tm, tp, tc, tr = c.timed(lambda: q(on_t))
        c.emit({**base, "mode": "rollup", "query": name, "runs": c.RUNS, "rollup_ms_median": vm, "rollup_ms_p95": vp, "rollup_ms_cold": vc,
                "rollup_rows": len(vr), "table_ms_median": tm, "table_ms_p95": tp, "table_ms_cold": tc, "table_rows": len(tr),
                "sql_rollup": on_v, "sql_table": on_t, "answer_rollup": vr, "answer_table": tr if name != "by_severity_5m_24h" else None})


if __name__ == "__main__":
    main(sys.argv[1])
