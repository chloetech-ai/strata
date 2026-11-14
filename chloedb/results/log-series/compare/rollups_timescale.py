#!/usr/bin/env python3
"""TimescaleDB's side of the rollup comparison: a hypertable logs (chunks
of a day) with the same rows (DATA/syn20m.shifted.tsv, rollups_prep.py)
loaded with COPY; a continuous aggregate by time_bucket('5 minutes'),
service and severity with count, sum, min and max of latency, materialized
in full once (refresh_continuous_aggregate over everything), with a
refresh policy every minute over the last hour; the harness's 50,000 new
lines, and how long until the policy has them in the aggregate; then the
four queries on the aggregate and on the hypertable. The aggregate is
materialized only (TimescaleDB's default), so it answers from what has
been refreshed.

Queries go through the Mac's psql (Homebrew postgresql@16) over TCP into
the container, as the other stores' through HTTP; each is timed by psql's
\\timing.

Container:
  docker run -d --name ls-ts -p 5433:5432 -v ls-ts:/var/lib/postgresql/data \
    -e POSTGRES_HOST_AUTH_METHOD=trust timescale/timescaledb:latest-pg16
rollups_timescale.py DATA
"""
import json
import os
import re
import subprocess
import sys
import time

import rollcommon as c

PSQL = ["psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", "-h", "127.0.0.1", "-p", "5433", "-U", "postgres", "-d", "postgres"]


def psql(sql, rows=True):
    out = subprocess.run(PSQL + ["-A", "-t", "-F", "\t", "-c", sql], check=True, capture_output=True, text=True).stdout
    return [l.split("\t") for l in out.splitlines() if l] if rows else None


def one(sql):
    return psql(sql)[0][0]


def timed_sql(sql, runs=c.RUNS):
    """Runs sql runs times in one psql session; psql's \\timing for each."""
    script = "\\timing on\n\\o /dev/null\n" + "".join(sql.rstrip(";") + ";\n" for _ in range(runs))
    out = subprocess.run(PSQL + ["-f", "-"], input=script, check=True, capture_output=True, text=True).stdout
    ms = [float(m) for m in re.findall(r"Time: ([0-9.]+) ms", out)]
    assert len(ms) == runs, out[:500]
    s = sorted(ms)
    return round(s[runs // 2], 2), round(s[min(-(-runs * 95 // 100), runs) - 1], 2), round(ms[0], 2)


def copy(path):
    t0 = time.time()
    subprocess.run(PSQL + ["-c", f"\\copy logs (ts, host, service, severity, pid, latency, body) FROM '{path}'"], check=True)
    return time.time() - t0


def main(data):
    meta = json.load(open(os.path.join(data, "rollup-meta.json")))
    version = one("SELECT extversion FROM pg_extension WHERE extname = 'timescaledb'")
    pg = one("SHOW server_version")
    base = {"store": "timescaledb", "version": f"TimescaleDB {version}, PostgreSQL {pg}", "db": "syn20m"}
    psql("SET timezone = 'UTC'", rows=False)
    psql("ALTER DATABASE postgres SET timezone TO 'UTC'", rows=False)
    psql("DROP MATERIALIZED VIEW IF EXISTS logs_5m CASCADE", rows=False)
    psql("DROP TABLE IF EXISTS logs CASCADE", rows=False)
    psql("CREATE TABLE logs (ts timestamptz NOT NULL, host text, service text, severity text, pid int, latency bigint, body text)", rows=False)
    psql("SELECT create_hypertable('logs', by_range('ts', INTERVAL '1 day'))", rows=False)
    load_s = copy(os.path.join(data, "syn20m.shifted.tsv"))
    psql("VACUUM ANALYZE logs", rows=False)
    lines = int(one("SELECT count(*) FROM logs"))
    tbytes = int(one("SELECT hypertable_size('logs')"))
    c.emit({**base, "mode": "load", "lines": lines, "seconds": round(load_s, 1), "bytes_on_disk": tbytes,
            "chunks": int(one("SELECT count(*) FROM show_chunks('logs')")), "note": "uncompressed hypertable"})

    psql("""CREATE MATERIALIZED VIEW logs_5m WITH (timescaledb.continuous, timescaledb.materialized_only = true) AS
      SELECT time_bucket('5 minutes', ts) AS bucket, service, severity, count(*) AS cnt, sum(latency) AS sum_latency,
        min(latency) AS min_latency, max(latency) AS max_latency
      FROM logs GROUP BY bucket, service, severity WITH NO DATA""", rows=False)
    t0 = time.time()
    psql("CALL refresh_continuous_aggregate('logs_5m', NULL, NULL)", rows=False)
    create_s = time.time() - t0
    mat = one("SELECT format('%I.%I', materialization_hypertable_schema, materialization_hypertable_name) FROM timescaledb_information.continuous_aggregates WHERE view_name = 'logs_5m'")
    vbytes = int(one(f"SELECT hypertable_size('{mat}')"))
    c.emit({**base, "mode": "rollup", "step": "create", "seconds": round(create_s, 3), "rollup_rows": int(one("SELECT count(*) FROM logs_5m")),
            "rollup_bytes_on_disk": vbytes, "table_bytes_on_disk": tbytes, "how": "refresh_continuous_aggregate over everything"})
    t0 = time.time()
    psql("CALL refresh_continuous_aggregate('logs_5m', NULL, NULL)", rows=False)
    c.emit({**base, "mode": "rollup", "step": "refresh_nothing_new", "seconds": round(time.time() - t0, 3)})

    job = one("SELECT add_continuous_aggregate_policy('logs_5m', start_offset => INTERVAL '1 hour', end_offset => NULL, schedule_interval => INTERVAL '1 minute')")
    before = int(one("SELECT coalesce(sum(cnt), 0) FROM logs_5m"))
    ins_s = copy(os.path.join(data, "fresh50k.tsv"))
    t_ins = time.time()
    while int(one("SELECT coalesce(sum(cnt), 0) FROM logs_5m")) < before + 50_000 and time.time() - t_ins < 300:
        time.sleep(0.5)
    waited = time.time() - t_ins
    run = psql(f"SELECT extract(epoch FROM last_run_duration) FROM timescaledb_information.job_stats WHERE job_id = {job}")
    c.emit({**base, "mode": "rollup", "step": "50k_new_visible", "seconds": round(ins_s + waited, 3), "insert_seconds": round(ins_s, 3),
            "waited_after_insert_seconds": round(waited, 3), "policy_last_run_seconds": float(run[0][0]) if run and run[0][0] else None,
            "visible": int(one("SELECT coalesce(sum(cnt), 0) FROM logs_5m")) >= before + 50_000,
            "how": "refresh policy every minute over the last hour"})
    psql(f"SELECT alter_job({job}, scheduled => false)", rows=False)
    t0 = time.time()
    psql("CALL refresh_continuous_aggregate('logs_5m', now() - INTERVAL '1 hour', NULL)", rows=False)
    c.emit({**base, "mode": "rollup", "step": "refresh_last_hour_by_hand", "seconds": round(time.time() - t0, 3), "note": "after the policy already took the new lines"})

    s = {k: c.iso(v) for k, v in meta["since_us"].items()}
    ep = lambda e: f"extract(epoch FROM {e})::bigint"
    hb, hts, mts = ep("time_bucket('1 hour', bucket)"), ep("date_trunc('hour', ts)"), ep("date_trunc('minute', ts)")
    pairs = {
        "hourly_7d": (f"SELECT {hb} AS h, sum(cnt) FROM logs_5m WHERE bucket >= '{s['hourly_7d']}' GROUP BY h",
                      f"SELECT {hts} AS h, count(*) FROM logs WHERE ts >= '{s['hourly_7d']}' GROUP BY h"),
        "by_severity_5m_24h": (f"SELECT {ep('bucket')} AS b, severity, sum(cnt) FROM logs_5m WHERE bucket >= '{s['by_severity_5m_24h']}' GROUP BY b, severity",
                               f"SELECT {mts} AS m, severity, count(*) FROM logs WHERE ts >= '{s['by_severity_5m_24h']}' GROUP BY m, severity"),
        "mean_latency_hourly_by_service_24h": (
            f"SELECT {hb} AS h, service, sum(sum_latency)::float8 / sum(cnt) FROM logs_5m WHERE bucket >= '{s['mean_latency_hourly_by_service_24h']}' GROUP BY h, service",
            f"SELECT {hts} AS h, service, avg(latency)::float8 FROM logs WHERE ts >= '{s['mean_latency_hourly_by_service_24h']}' GROUP BY h, service"),
        "max_latency_hourly_24h": (f"SELECT {hb} AS h, max(max_latency) FROM logs_5m WHERE bucket >= '{s['max_latency_hourly_24h']}' GROUP BY h",
                                   f"SELECT {hts} AS h, max(latency) FROM logs WHERE ts >= '{s['max_latency_hourly_24h']}' GROUP BY h"),
    }
    for name in c.QUERIES:
        on_v, on_t = pairs[name]
        vm, vp, vc = timed_sql(on_v)
        tm, tp, tc = timed_sql(on_t)
        vr, tr = psql(on_v), psql(on_t)
        c.emit({**base, "mode": "rollup", "query": name, "runs": c.RUNS, "rollup_ms_median": vm, "rollup_ms_p95": vp, "rollup_ms_cold": vc,
                "rollup_rows": len(vr), "table_ms_median": tm, "table_ms_p95": tp, "table_ms_cold": tc, "table_rows": len(tr),
                "sql_rollup": on_v, "sql_table": on_t, "answer_rollup": vr, "answer_table": tr if name != "by_severity_5m_24h" else None})


if __name__ == "__main__":
    main(sys.argv[1])
