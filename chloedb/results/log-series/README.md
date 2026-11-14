# Log tables over time

How a table of logs behaves as it grows and ages in ChloeDB: what merges
and retention cost while logs stream in, how the segments lie in time,
and how fast time-window counts, time buckets and percentiles answer.
Measured with `examples/log_series_bench.rs`:

```text
cargo build --release --example log_series_bench
log_series_bench stream DB gen:20000000:30 600 190    # 10 minutes, kept 190 s
log_series_bench load DB bgl.jsonl && log_series_bench query DB 7
log_series_bench ingest DB gen:20000000:30 4          # writers at once, background merges
log_series_bench rollup DB 7                          # a rollup of a loaded table
```

| file | what |
|---|---|
| `baseline.jsonl` | main at f46bfb7f (the harness at 3a3003c9) |
| `step1-windows.jsonl` | merges within windows of time for tables with `retain` (this branch) |
| `step6-rollups.jsonl` | ingest with 1, 2 and 4 writers; rollups (this branch, on a91f4601 with the rollup changes uncommitted) |
| `rollups-build-space.jsonl` | a rollup's first fold, refreshes and bytes on 20M lines, main at c08fd630 beside a day of buckets per query and integer keys (on c08fd630 with the change uncommitted), measured 2026-11-14 |
| `mac-chloedb.jsonl` | the query set on a MacBook Air, ChloeDB at 13bb25bd, with `"cores"` 8 and 4 (see below) |
| `mac-others.jsonl` | the same on ClickHouse, VictoriaLogs and OpenSearch, with `"store"` (`compare/`) |
| `chloedb-values.jsonl` | what ChloeDB's queries returned, to check the other stores against |
| `mac-rollups.jsonl` | rollups on a MacBook Air against ClickHouse materialized views and TimescaleDB continuous aggregates (`compare/rollups_*`) |
| `chloedb-rollup-values.jsonl` | what ChloeDB's eight rollup and table queries returned there, to check the other two against |
| `mac-rollups-fix.jsonl` | the same `rollup` run on that MacBook Air with a day of buckets per query and integer keys (8a0f4cbe) |

All measured on 2026-11-13 on a cloud container: 4-core Intel Xeon @
2.10GHz, 15 GB of RAM, release builds.

**Data.** Loghub's BGL (4.75M lines over about 7 months) and HDFS (11.2M
over 2 days), parsed by `../log-bytes/parse.py`, and a synthetic log
(`gen:LINES:DAYS`): lines spread over 30 days with a daily cycle, 500
hosts, 20 services, five severities, a log-normal latency (2 ms median)
and 2% of lines up to 10 minutes late.

## Streaming with retention

`stream` writes batches of 5,000 lines as OTLP requests come, each stamped
with the time it is written, for 10 minutes, into a table kept 190
seconds: thirty days of retention compressed into minutes. It flushes
every 10,000 rows and runs the merges the default policy selects after
each flush. 2% of lines are late by up to a tenth of the retention.

| | lines written | lines/s | write amplification | rows rewritten per row | merge seconds | rows past the cutoff still on disk | segments | overlap |
|---|---|---|---|---|---|---|---|---|
| main, no retention | 54.5M | 90,824 | 3.38 | 2.91 | 272 | — | 35 | 1.26 |
| main, `retain = '190 seconds'` | 51.8M | 86,393 | 3.44 | 2.99 | 274 | 964,164 | 14 | 2.00 |
| windows, `retain = '190 seconds'` | 61.5M | 102,457 | 2.58 | 1.91 | 194 | 408,711 | 60 | 6.08 |
| windows, also `cluster_by = 'ts'` | 59.7M | 99,498 | 2.40 | 1.70 | 182 | 338,048 | 139 | 13.14 |

Write amplification is bytes flushed and merged per byte flushed; overlap
is the sum of the segments' time spans over the span of all of them (1
when they tile it).

With windows a kept table rewrites each row about once less (1.91 times
against 2.99), spends 29% less time merging, writes 19% more lines in the
same 10 minutes, and keeps 58% fewer expired rows on disk. It holds more
segments (60 against 14), each of one window (6.3 s here, a thirtieth of
the retention). Overlap goes up because a segment's late rows reach back
into older windows: here they are late by up to three windows, which a
real table kept 30 days (windows of a day, lines late by minutes) does
not see. `cluster_by = 'ts'` saves a little more rewriting, but more than
doubles the segments, so `otel_logs` does not use it.

## Queries

`load` writes the lines with their own times, moved so the newest is now,
in the same batches and merges, without retention; `query` runs each
query 7 times (median and 95th percentile ms; the 95th is the first, cold
run). "Segments in window" counts the segments whose newest row is inside
the query's window.

Main at f46bfb7f:

| log | query | rows in window | median ms | p95 ms | segments in window |
|---|---|---|---|---|---|
| synthetic 100M | `COUNT(*)` last hour | ~140K | 0.07 | 13.85 | 3 of 58 |
| synthetic 100M | `COUNT(*)` last day | ~3.3M | 0.07 | 0.38 | 7 of 58 |
| synthetic 100M | `date_trunc('hour')` counts, 7 days | ~23M | 944 | 965 | 19 of 58 |
| synthetic 100M | `date_trunc('minute')` by severity, 24 h | ~3.3M | 156 | 207 | 7 of 58 |
| synthetic 100M | top 10 hosts by errors, 24 h | ~3.3M | 3.5 | 170 | 7 of 58 |
| synthetic 100M | `MATCH(body, 'error')`, last hour, newest 100 | ~140K | 0.55 | 359 | 3 of 58 |
| synthetic 100M | p99 latency per hour and service, 24 h (exact) | ~3.3M | 236 | 323 | 7 of 58 |
| synthetic 100M | p50, p95, p99 latency, 24 h (exact) | ~3.3M | 413 | 441 | 7 of 58 |
| HDFS 11.2M | `date_trunc('hour')` counts, 7 days | 11.2M | 437 | 466 | 18 of 18 |
| HDFS 11.2M | `date_trunc('minute')` by severity, 24 h | most of it | 340 | 416 | 17 of 18 |
| BGL 4.75M | `date_trunc('hour')` counts, 7 days | small | 0.47 | 2.53 | 1 of 13 |

Counts over a time window are answered from the range index in well under
a millisecond. A time-bucket `GROUP BY` reads every row of the window, at
about 40 ns a row, and exact percentiles about 70 to 120 ns a row: those
are the costs `date_bin` and approximate percentiles go after. Segments
written in arrival order already tile time (overlap 1.00 to 1.01 after
`load`), and a window's count costs well under a millisecond with 58
segments, the zone maps passing over the others' blocks, so skipping
segments by time up front (step 2 of the plan) has little to win on
local segments; it was not built.

## Ingest

`ingest` writes the 20M synthetic lines as a server does: writers sending
batches of 5,000 at once, the flusher and merger in the background.

| writers | lines/s, written | lines/s, merges settled | segments |
|---|---|---|---|
| 1 | 221,221 | 217,497 | 21 |
| 2 | 223,601 | 222,212 | 26 |
| 4 | 210,753 | 209,563 | 26 |

About 220,000 lines a second, the same with one writer as with four: the
writer applies one group of writes at a time, and one thread makes the
lines. `load`, which merges after each flush on its own thread, writes
80,000 a second (100M lines in 1,256 s); its slowness was the harness,
not the database.

## Rollups

`rollup` makes `CREATE ROLLUP logs_5m ON logs AS SELECT date_bin('5
minutes', ts), service, severity, COUNT(*), SUM(latency), MIN(latency),
MAX(latency)` on a loaded table, then queries it beside the same query of
the table, 7 runs each. The answers were equal wherever both ask the same
(by severity the table's query is by minute, the nearest it has without
`date_bin`).

| log | query | rollup ms | table ms |
|---|---|---|---|
| synthetic 100M | hourly counts, 7 days | 9.17 | 876 |
| synthetic 100M | counts by 5 minutes and severity, 24 h | 6.01 | 156 (by minute) |
| synthetic 100M | mean latency per hour and service, 24 h | 4.36 | 129 |
| synthetic 100M | greatest latency per hour, 24 h | 2.98 | 149 |
| HDFS 11.2M | hourly counts, 7 days | 0.43 | 396 |
| HDFS 11.2M | mean latency per hour and service, 24 h | 0.77 | 321 |

Keeping it, on the 100M lines (838,404 rows of rollup, 8,648 buckets):

| | seconds |
|---|---|
| first fold, `CREATE ROLLUP` | 251 |
| refresh, nothing new | 0.011 |
| refresh after 50,000 new lines in the last minute | 1.61 |
| full refresh, every bucket compared | 7.1 |

The first fold runs a query per bucket, about 29 ms each: folding many
buckets in one query waits for `date_bin` in the columnar GROUP BY. The
refresh after new lines is the first query over rows still in memory;
repeated by hand in the same container it took 18 ms.

### A day of buckets per query, and integer keys

The first fold above ran a query per bucket. Now the counts and the fold
each read a day of buckets (288 of 5 minutes) per query, through
`date_bin` in the columnar GROUP BY, and a new rollup keys its rows with
a number rather than text and keeps its columns unstored
(`stored = false`): its queries read the columns only. On the synthetic
20M lines over 30 days (748,995 rows of rollup, 8,644 buckets), the same
database, one after the other:

| | main at c08fd630 | this change |
|---|---|---|
| first fold, `CREATE ROLLUP`, seconds | 80.5 | 7.9 |
| refresh, nothing new, seconds | 0.004 | 0.005 |
| refresh after 50,000 new lines, seconds | 0.042 | 0.028 |
| full refresh, every bucket compared, seconds | 2.29 | 0.25 |
| rollup bytes, as flushed | 79.7 MB | 42.5 MB |
| rollup bytes, merged | 66.5 MB (89 B a row) | 30.3 MB (40 B a row) |

Of the 36 MB saved, most is the stored copy of each row; the rest is the
text key (the bucket's microseconds and the group's values joined, now one integer).
Compressing the rollup with zstd saved little more once rows were not
stored. Rollups made before keep their text keys.

## On a MacBook Air

The query set again, on a MacBook Air (Mac14,2): Apple M2, 8 cores (4
performance, 4 efficiency), 24 GB of RAM, macOS 26.0.1, measured on
2026-11-13. ChloeDB at 13bb25bd, release build, run natively. ClickHouse
25.8.33.6, VictoriaLogs v1.53.0 and OpenSearch 3.3.0 ran in Docker Desktop
(engine 24.0.5), whose VM has 4 CPUs and 8.7 GB, one store at a time,
with nothing else in the VM (a kind cluster there was stopped for the runs).
Scripts are in `compare/`: `chloedb.sh`, then `others.sh` (it starts each
container, runs `clickhouse.py`, `victorialogs.py`, `opensearch.py`, and
removes it), then `chloedb_values.py` and `check.py`, which prints the
tables below.

The data are the 4.75M lines of BGL, the 11.2M of HDFS and a synthetic log of
20M lines over 30 days (`log_series_bench gen syn20m.jsonl 20000000 30`;
20M rather than 100M so that every store fits in this laptop's memory).
Every store holds the same rows at the same times: each file's `ts` is
moved as `load` moved it for ChloeDB (by ChloeDB's newest row minus the
file's newest `ts`), so the windows, the hours and the minutes are the
same in all four.

- **ClickHouse**: the table the OpenTelemetry Collector's exporter creates
  (as in `../log-bytes/clickhouse.sh`: `ORDER BY (ServiceName,
  TimestampTime, Timestamp)`, partitioned by day, host in
  `ResourceAttributes`, pid and latency in `LogAttributes`, the token Bloom
  filter on `Body`), loaded with one `INSERT ... SELECT FROM file()`, then
  left as its background merges left it. Windows filter on `TimestampTime`
  and `Timestamp`, as the exporter's readers do. The newest lines with
  `error` use `hasTokenCaseInsensitive`.
- **VictoriaLogs**: a fresh store per file, the body as `_msg`, host and
  service as the stream, sent to `/insert/jsonline`, flushed and
  force-merged. Queries in LogsQL (`_time:>=T | stats count()`, `stats by
  (_time:1h)`, `i(error)`, `quantile`).
- **OpenSearch**: the full image with its security plugin off
  (`opensearch-min` is not on Docker Hub), a 4 GB heap, one shard, no
  replicas, `ts` a `date_nanos` so microseconds are kept. Bulk-loaded 5,000
  lines a request on 4 threads, then refreshed and force-merged to one
  segment. Queries use `range`, `date_histogram` (`min_doc_count` 1),
  `terms` and `percentiles`, with the request cache off.

Times are as the harness takes them: 7 runs, the median, and the 95th
percentile, which with 7 runs is the slowest (most often the first, cold
run; `ms_cold` in `mac-others.jsonl`). ChloeDB's are in process. The
others' are each request's wall time from a Python client on the Mac to
the container, so they include an HTTP round trip into the VM, about a
millisecond. A sub-millisecond time on their side cannot show for that
reason. Load is the time to write the file (ChloeDB: `load`, its merges
included; ClickHouse: the `INSERT`; VictoriaLogs: the POST and the flush;
OpenSearch: the bulk requests and a refresh). The others' force merges are
not counted. Bytes on disk are ChloeDB's database file, ClickHouse's active
parts, VictoriaLogs' data directory and OpenSearch's store once merged.

The tables compare ChloeDB limited to **4 cores**, the cores Docker's VM
gives the other three. ChloeDB sizes its threads from
`std::thread::available_parallelism` and reads no setting that overrides it
(and macOS has no CPU affinity), so the 4-core run loads `compare/ncpu.c`,
which makes `sysconf` report 4 cores. On all 8 cores (`"cores": 8` in
`mac-chloedb.jsonl`), every median of a millisecond or more is within 4% of the
4-core one, and the others within 0.04 ms. On this machine the queries gain
nothing from the other 4 cores (4 of the 8 are efficiency cores).

BGL 4.75M, ms (median / p95):

| query | rows | ChloeDB | ClickHouse | VictoriaLogs | OpenSearch |
|---|---|---|---|---|---|
| `COUNT(*)` last hour | 1 | 0.01 / 0.18 | 2.6 / 8.86 | 1.49 / 5.02 | 3.32 / 21.95 |
| `COUNT(*)` last day | 1 | 0.02 / 0.12 | 2.73 / 5.19 | 1.58 / 2.26 | 2.45 / 3.03 |
| `COUNT(*)` all | 1 | 0.01 / 0.02 | 1.49 / 1.9 | 114.72 / 133.1 | 1.93 / 2.86 |
| counts per hour, 7 days | 80 | 0.28 / 0.54 | 3.05 / 5.26 | 2.31 / 5.04 | 3.75 / 22.53 |
| counts per minute by severity, 24 h | 74 | 0.17 / 0.45 | 2.79 / 4.24 | 1.62 / 2.12 | 4.79 / 9.85 |
| top 10 hosts by errors, 24 h | 0 | 0.21 / 3.6 | 2.56 / 5.17 | 0.97 / 1.47 | 4.54 / 17.96 |
| newest 100 with `error`, last hour | 36 | 0.07 / 3.35 | 2.26 / 3.29 | 1.18 / 2.55 | 4.8 / 19.31 |
| load, bytes on disk | | 40.6 s, 188 MB | 4.9 s, 42 MB | 10.6 s, 316 MB | 31.7 s, 311 MB |

HDFS 11.2M, ms (median / p95):

| query | rows | ChloeDB | ClickHouse | VictoriaLogs | OpenSearch |
|---|---|---|---|---|---|
| `COUNT(*)` last hour | 1 | 0.02 / 0.32 | 5.24 / 7.39 | 2.3 / 7.89 | 4.37 / 12.52 |
| `COUNT(*)` last day | 1 | 0.03 / 0.16 | 16.31 / 19.52 | 2.37 / 3.98 | 2.8 / 3.21 |
| `COUNT(*)` all | 1 | 0.01 / 0.08 | 1.47 / 3.51 | 1.58 / 2.17 | 2.12 / 2.29 |
| counts per hour, 7 days | 39 | 271.11 / 274.85 | 32.51 / 40.1 | 27.1 / 69.65 | 3.24 / 15.27 |
| counts per minute by severity, 24 h | 2392 | 241.91 / 246.98 | 59.39 / 69.32 | 76.65 / 91.64 | 511.03 / 519.6 |
| top 10 hosts by errors, 24 h | 0 | 0.03 / 0.57 | 3.02 / 7.15 | 1.05 / 1.37 | 2.18 / 3.43 |
| newest 100 with `error`, last hour | 21 | 0.17 / 18.49 | 12.71 / 22.04 | 104.19 / 112.65 | 3.67 / 8.6 |
| load, bytes on disk | | 122.7 s, 600 MB | 11.2 s, 143 MB | 17.8 s, 146 MB | 69.9 s, 923 MB |

synthetic 20M, ms (median / p95):

| query | rows | ChloeDB | ClickHouse | VictoriaLogs | OpenSearch |
|---|---|---|---|---|---|
| `COUNT(*)` last hour | 1 | 0.02 / 0.42 | 5.66 / 11.19 | 6.86 / 12.27 | 2.92 / 28.51 |
| `COUNT(*)` last day | 1 | 0.02 / 0.12 | 9.19 / 11.55 | 8.9 / 13.35 | 2.4 / 2.67 |
| `COUNT(*)` all | 1 | 0.01 / 0.03 | 1.35 / 2.03 | 34.41 / 40.24 | 2.08 / 2.35 |
| counts per hour, 7 days | 169 | 107.4 / 111.48 | 31.63 / 40.33 | 105.83 / 125.49 | 98.06 / 211.4 |
| counts per minute by severity, 24 h | 7010 | 26.02 / 28.33 | 22.78 / 27.01 | 43.79 / 61.07 | 74.24 / 277.56 |
| top 10 hosts by errors, 24 h | 10 | 0.41 / 7.34 | 17.16 / 20.62 | 8.66 / 10.42 | 4.95 / 23.72 |
| newest 100 with `error`, last hour | 100 | 0.13 / 17.48 | 19.46 / 21.44 | 4.22 / 6.83 | 10.44 / 46.01 |
| p99 latency per hour and service, 24 h | 500 | 28.71 / 33.11 | 29.5 / 33.33 | 93.57 / 137.09 | 146.63 / 203.49 |
| p50, p95, p99 latency, 24 h | 1 | 33.24 / 34.54 | 25.65 / 26.65 | 119.2 / 136.05 | 94.47 / 95.77 |
| load, bytes on disk | | 168.9 s, 1497 MB | 28.9 s, 551 MB | 44.2 s, 647 MB | 130.7 s, 2408 MB |

**Counts.** Every count matches ChloeDB's exactly, in all three stores and
on all three files: the window counts, every hour and every minute-by-severity
bucket (compared as a hash of the sorted buckets), the top 10 hosts' error
counts, and the newest and oldest of the 100 newest lines with `error`,
along with how many lines in the hour have it. BGL and HDFS have no
`ERROR` lines in their last 24 hours (BGL's errors are `FATAL`, HDFS has
`INFO` and `WARN`), so their top-hosts query returns no rows in any store.

**Percentiles** (synthetic log only). ChloeDB's `PERCENTILE_CONT` is exact.
Each store used its own function:

| store | function | exact? |
|---|---|---|
| ClickHouse | `quantileExactInclusive` / `quantilesExactInclusive` (interpolated as `PERCENTILE_CONT`) | yes |
| VictoriaLogs | `quantile()`, its only percentile function | no |
| OpenSearch | `percentiles` aggregation, default t-digest (compression 100) | no |

ClickHouse's equal ChloeDB's to 1e-14 (the last bits of the interpolation).
The approximate ones, as relative error against the exact values over the
500 hour-and-service groups and over the three percentiles of the day:

- ClickHouse `p50_p95_p99_24h`: median 0.000%, largest 0.000%
- ClickHouse `p99_hourly_by_service_24h`: median 0.000%, largest 0.000%
- OpenSearch `p50_p95_p99_24h`: median 0.057%, largest 0.079%
- OpenSearch `p99_hourly_by_service_24h`: median 1.454%, largest 35.815%
- VictoriaLogs `p50_p95_p99_24h`: median 0.729%, largest 1.284%
- VictoriaLogs `p99_hourly_by_service_24h`: median 0.954%, largest 35.815%

The largest error is the same for both. It is the first, partial hour of
the window for `svc-14`: about 300 lines, whose p99 falls in a sparse tail.
p50, p95, p99 over the day: exact 1997, 14401, 32509.32; VictoriaLogs 1992,
14296, 32092; OpenSearch 1997.6, 14409.2, 32535.0.

On this laptop, ChloeDB answers the time-window counts and the top-hosts
and newest-match queries in well under a millisecond, from the range index
and the newest segments, where the others take a few milliseconds plus a
round trip. Time buckets that read every row of a large window are where
it is slowest. On HDFS, 11.2M lines with most of them in the last day,
counts per hour over 7 days take ChloeDB 271 ms against ClickHouse's 33,
VictoriaLogs' 27 and OpenSearch's 3.2, and counts per minute by severity
take 242 ms against 59, 77 and 511. On the synthetic log ChloeDB's hourly
counts (107 ms) are about VictoriaLogs' and OpenSearch's and three times
ClickHouse's. Its exact percentiles cost about what ClickHouse's exact ones
do (29 and 33 ms against 30 and 26), and a third to a fifth of what the
approximate ones cost in the other two stores. ChloeDB's load is the slowest of the four here (40 to 169 s,
against 5 to 29 s for ClickHouse). The harness writes in one thread, in
batches of 5,000, merging as it goes. Its database is 2.7 to 4.5 times
ClickHouse's on disk.
## Rollups on a MacBook Air

The `rollup` run again, on a MacBook Air (Mac14,2): Apple M2, 8 cores (4
performance, 4 efficiency), 24 GB of RAM, macOS 26.0.1, measured on
2026-11-14. It ran beside ClickHouse 25.8.33.6 with a materialized view
and TimescaleDB 2.32.0 (PostgreSQL 16.15) with a continuous aggregate. The
two ran in Docker Desktop (engine 24.0.5), whose VM has 4 CPUs and 8.7 GB,
one at a time with nothing else in the VM. ChloeDB at c08fd630 ran
natively, seeing 4 cores (`compare/ncpu.c`, as in the comparison of the
day before). The scripts are `compare/rollups_chloedb.sh`, then
`rollups_prep.py`, `rollups_others.sh` (which runs `rollups_clickhouse.py`
and `rollups_timescale.py` and removes each container), and
`rollups_check.py`, which prints these tables.

All three hold the same rows: the synthetic log `gen:20000000:30`, its
times shifted as ChloeDB's `load` shifted them, then the 50,000 lines
`rollup` writes over the last minute, with the same times. They answer
over the same windows, from the whole hour after the newest line less 7
days or 24 hours, as the harness's queries do.

- **ChloeDB**: `CREATE ROLLUP logs_5m ON logs AS SELECT date_bin('5
  minutes', ts), service, severity, COUNT(*), SUM(latency), MIN(latency),
  MAX(latency)`, then `REFRESH ROLLUP` after the new lines.
- **ClickHouse**: the logs table is the OpenTelemetry exporter's, as in the
  comparison before (latency in `LogAttributes`). The view is a
  materialized view `TO` an AggregatingMergeTree keyed by
  `toStartOfFiveMinutes`, service and severity, with
  `SimpleAggregateFunction` columns for count, sum, min and max. A view
  sees only inserts made after it exists, so it is filled with one
  `INSERT ... SELECT` from the table. Queries on it `GROUP BY` again with
  `sum`/`max`, since its parts merge later.
- **TimescaleDB**: a hypertable of day chunks (uncompressed), loaded with
  `COPY`. A continuous aggregate by `time_bucket('5 minutes')`, service and
  severity, materialized only (TimescaleDB's default), filled once with
  `refresh_continuous_aggregate` over everything, with a refresh policy
  every minute over the last hour.

Median and slowest of 7 runs, in ms. ChloeDB's are in process.
ClickHouse's are over HTTP from the Mac and TimescaleDB's are psql's
`\timing` from the Mac, both into Docker's VM, so each includes about a
millisecond of round trip. "Table" is the same question asked of the raw
table. By severity, the table's query is by minute, as the harness asks it.

| query | rows | ChloeDB rollup | ChloeDB table | ClickHouse rollup | ClickHouse table | TimescaleDB rollup | TimescaleDB table |
|---|---|---|---|---|---|---|---|
| counts per hour, 7 days | 168 | 2.22 / 5.23 | 36.27 / 46.3 | 4.56 / 33.54 | 32.44 / 42.32 | 27.27 / 40.8 | 1039.93 / 1162.63 |
| counts per 5 min by severity, 24 h | 1398 | 1.69 / 2.36 | 26.64 / 27.92 | 4.4 / 5.55 | 19.95 / 28.51 | 6.76 / 23.69 | 196.25 / 277.22 |
| mean latency per hour and service, 24 h | 480 | 1.54 / 3.31 | 19.69 / 19.81 | 4.45 / 13.94 | 25.03 / 31.95 | 7.1 / 19.54 | 197.42 / 257.92 |
| max latency per hour, 24 h | 24 | 0.55 / 1.96 | 10.28 / 11 | 2.98 / 9.42 | 23.78 / 25.95 | 5.36 / 16.55 | 181.62 / 238.58 |

| | ChloeDB | ClickHouse | TimescaleDB |
|---|---|---|---|
| load 20M lines | 186.1 s | 30.3 s | 66.7 s |
| build the rollup | 72.192 s | 0.886 s | 17.175 s |
| refresh, nothing new | 0.003 s | — | 0.037 s |
| 50,000 new lines in the rollup | 0.084 s | 0.065 s | 2.78 s |
| rollup rows | 749027 | 749027 | 749027 |
| rollup on disk | 82.0 MB (with the 50,000 new lines) | 11.5 MB | 99.9 MB |
| table on disk | 1361 MB | 551 MB | 3456 MB |

Each store's answers match ChloeDB's, on the rollup and on the table:
every hour's count and maximum exactly, and every mean to the 4 decimals
ChloeDB's shell prints. The per-minute query on the table matches in
number of rows. All three made the same 749,027 rollup rows.

- **Queries.** On the rollup, ChloeDB answers each query in 0.5 to 2.2 ms:
  2 to 5 times faster than ClickHouse's view (3 to 4.6 ms) and 4 to 12
  times faster than TimescaleDB's aggregate (5 to 27 ms), though both of
  those times include the round trip. Against its own raw table, a rollup
  is 13 to 19 times faster in ChloeDB, 4.5 to 8 in ClickHouse and 28 to 38
  in TimescaleDB. ChloeDB's table takes 10 to 36 ms here, about
  ClickHouse's 20 to 32, against 180 to 1,040 ms for TimescaleDB's
  uncompressed hypertable.
- **Building it.** ChloeDB's first fold is the slowest by far: 72 s,
  against 0.9 s for ClickHouse's `INSERT ... SELECT` and 17 s for
  TimescaleDB's first refresh. It folds one bucket per query, 8,643 buckets
  at about 8 ms each, as the section above says.
- **New lines.** ChloeDB's `REFRESH ROLLUP` takes the 50,000 lines in 0.08 s.
  ClickHouse's view has them as its `INSERT` returns (0.065 s). In
  TimescaleDB they are in the aggregate when the policy next runs: 2.6 s
  after the `COPY` here, but up to its minute.
- **Size.** ChloeDB's rollup takes 82 MB, the 50,000 new lines included:
  the segments a checkpoint wrote besides the table's. ClickHouse's takes
  11.5 MB and TimescaleDB's 99.9 MB.

### With a day of buckets per query, on the same MacBook Air

`log_series_bench rollup DB 7` again at 8a0f4cbe, which folds a day of
buckets per query and keys rollup rows with an integer
(`mac-rollups-fix.jsonl`). The machine and setup were the same: the
MacBook Air M2, with ChloeDB seeing 4 cores. It ran on a copy of the
database above, which already held the first run's 50,000 new lines, so
the rollup covers 20,050,000 lines before the next 50,000. ClickHouse and
TimescaleDB are as measured above. They did not run again.

| | ChloeDB at c08fd630 | ChloeDB at 8a0f4cbe | ClickHouse | TimescaleDB |
|---|---|---|---|---|
| build the rollup | 72.2 s | 8.6 s | 0.9 s | 17.2 s |
| refresh, nothing new | 0.003 s | 0.004 s | — | 0.037 s |
| 50,000 new lines in the rollup | 0.084 s | 0.021 s | 0.065 s | 2.8 s |
| full refresh, every bucket compared | 1.07 s | 0.20 s | — | — |
| rollup rows | 749,027 | 749,087 | 749,027 | 749,027 |
| rollup on disk | 82.0 MB* | 42.5 MB as flushed, 30.3 MB merged | 11.5 MB | 99.9 MB |

\* Measured another way: the segments a checkpoint wrote besides the
table's, so it includes the 50,000 new lines. The fix's figures are the
harness's own `rollup_bytes` and `rollup_bytes_merged`, for the rollup
alone. The 60 more rows are the first run's minute of new lines.

Medians of the rollup queries, in ms. The other two stores' times include
the round trip into Docker:

| query | ChloeDB at c08fd630 | ChloeDB at 8a0f4cbe | ClickHouse | TimescaleDB |
|---|---|---|---|---|
| counts per hour, 7 days | 2.22 | 1.95 | 4.56 | 27.27 |
| counts per 5 min by severity, 24 h | 1.69 | 1.43 | 4.4 | 6.76 |
| mean latency per hour and service, 24 h | 1.54 | 1.39 | 4.45 | 7.1 |
| max latency per hour, 24 h | 0.55 | 0.43 | 2.98 | 5.36 |

The windows of this run start about two hours later than the first run's,
since they end at the newest line. Its answers were checked against the
table, which the harness does on three of the queries, and not against the
other stores. ChloeDB's first fold is now twice as fast as TimescaleDB's
first refresh, and still about ten times ClickHouse's `INSERT ... SELECT`.
Merged, its rollup is 2.6 times ClickHouse's size and under a third of
TimescaleDB's.
