# The price tracker, before and after the fixes it asked for (6 November 2026)

An outside report built an eBay price tracker on embedded Strata (Python):
a simulated market of 10,000 live listings crawled every 4 hours for 90
days, 5.4M price snapshots, an `optimize()` once a day, then seven trend
queries. The report's code (`tracker.py`, `run.py`, `ebay_sim.py`,
`sqlite_baseline.py`) is not in this repository: it is in the project's
files as `strata-ebay/`. The fixes it led to are #267, #268, #270, #271,
#272 and #284; #252 and #264 (aggregates from the columns) and #253
(filters kept between queries) also touch its queries.

Both Strata builds and SQLite ran here, one after another, on one cloud
container (4 cores, Intel Xeon @ 2.10GHz, 15 GB, Python 3.13.18):

- before: strata-py at 4aa2bd0 (main on 5 November, before any of those
  fixes), built with `maturin build --release`;
- now: strata-py at b022e39 (main on 6 November), built the same way;
- SQLite 3.53.1 (Python's `sqlite3`), WAL, the same rows and indexes, FTS5
  for the title search (`sqlite_baseline.py`).

```bash
python run.py --db DB --listings 10000 --days 90 --every 4 --optimize-every 6 [--compact] --out OUT.json
python sqlite_baseline.py --db prices.sqlite --listings 10000 --days 90 --every 4
```

"text" is the report's schema (snapshots keyed by a text id such as
`v1|100078926272|0@20261129T2000`); "compact" (`--compact`) gives the
append-only snapshots table no primary key.

| | before, text | now, text | before, compact | now, compact | SQLite |
|---|---|---|---|---|---|
| size after the last `optimize()` | 790.5 MB | 757.5 MB | 214.5 MB | 214.6 MB | 1,890.9 MB (after `VACUUM`) |
| ingest, 540 crawls | 830 s | 667 s | 549 s | 451 s | 452 s |
| daily `optimize()`, median (max) | 6.4 s (12.8) | 4.5 s (10.0) | 3.0 s (6.8) | 1.9 s (4.4) | – |
| item_history | 0.1 ms | 0.3 ms | 0.0 ms | 0.1 ms | 0.0 ms |
| product_daily | 9.7 ms | 1.1 ms | 12.1 ms | 0.9 ms | 89.1 ms |
| trends_used | 368.9 ms | 22.2 ms | 350.2 ms | 21.1 ms | 2,333.1 ms |
| deals | 173.1 ms | 20.9 ms | 172.8 ms | 20.9 ms | 9,286.8 ms |
| price_drops | 73.5 ms | 73.8 ms | 70.4 ms | 73.8 ms | 254.4 ms |
| auction_closes | 17.9 ms | 17.2 ms | 18.1 ms | 16.9 ms | – |
| search_stats | 3.6 ms | 2.7 ms | 2.6 ms | 2.6 ms | 3.4 ms |

Each query time is the median of 5 runs (`run.py`, `sqlite_baseline.py`);
every row count is the same in all five columns. `sqlite_baseline.py` has
no `auction_closes`.

- The three aggregating trend queries are 8 to 17 times faster: daily
  averages per product 9.7 to 1.1 ms, the 7- and 30-day moving averages 369
  to 22 ms, and the deals join 173 to 21 ms. They now group from the
  columns (#252, #264). `price_drops` (a LAG over the last day) and the
  others did not change.
- Ingest is 18 to 20% faster and the daily `optimize()` a third quicker.
  One run per build, one after another on a shared container: read these
  two as a direction, not a measurement to a few per cent.
- Size did not change: the compact table takes 39.7 bytes a snapshot, a
  ninth of SQLite's 350; the text key still costs about 100 bytes a row
  (the report's last pain point, not fixed).

`strata-<build>-<schema>.json` holds each run's numbers, the crawl and
optimize times and the first rows of every query's answer; the `.log` files
are the runs' output.
