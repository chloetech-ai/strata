# Sparse keys: declared columns against a JSON field

`strata-json-bench` (`src/bin/strata-json-bench.rs`) keeps K attribute keys
that only a few rows hold as K declared columns (`A`: KEYWORD, INT or FLOAT
by key) or as the keys of one `JSON` field (`J`), over the same rows, and
measures the size, the load, the open, filters, facets, sorts and
aggregates on the keys, the `json_paths` listing and single-row `INSERT`s.

Each row holds 4 to 11 distinct keys drawn from a Zipf(1.0) popularity over
the K keys (7.5 a row on average, about 96 bytes of JSON). Of each 20 keys,
8 hold one of 20 strings, 4 a random 10-digit hex string, 5 an integer
below 100,000 and 3 a number with two decimals, and a numeric key holds the
string `"n/a"` 1% of the time, which a declared column cannot hold: `A`
drops those values (`values_dropped`). The seed is fixed, so every run
writes the same rows.

```text
cargo build --release --bin strata-json-bench
target/release/strata-json-bench --rows 1000000 --keys 1000 --out results/json/k1000.json
target/release/strata-json-bench --rows 1000000 --keys 100 --out results/json/k100.json
```

Measured on 2026-11-01 on a cloud VM (4-core Intel Xeon @ 2.80GHz, 16.9 GB,
Linux 6.18), the code of PR #189 (JSON fields as of phase 1 and text in
values). Times are the median of 7 runs after one untimed run; `peak_mb` is
the most heap the first run held beyond what was held before it. `J text`
is the JSON field with `OPTIONS(text = true)`, measured for its size only.

## 1,000,000 rows, 1,000 keys (`k1000.json`)

The most held key is in 66% of rows, the 10th in 10.5%, the 100th in 1.1%,
and 890 of the 1,000 keys are in fewer than 1% of rows.

| | declared columns (A) | JSON field (J) |
|---|---|---|
| database | 657 MB (657 B/row) | 251 MB (251 B/row) |
| load (100,000-row batches) | 75.9 s | 25.7 s |
| open | 44.7 ms | 6.4 ms |
| single-row INSERTs | 17 rows/s, 1,105 WAL B/row | 185 rows/s, 147 WAL B/row |

A segment of 100,000 rows by section, bytes a row: A 655 (columns 192,
keyword indexes 241, range indexes 93, doc store 61, key 31); J 249 (the
field's terms 147, doc store 71, key 31); J text 284 (the same and 35 of
text index).

| query | A ms | J ms |
|---|---|---|
| `= 'v3'` on the hottest key (66% of rows) | 0.27 | 0.14 |
| `= 'v3'` on a key in 1% of rows | 0.14 | 0.06 |
| `IN` of 4 values, hottest key | 0.83 | 0.38 |
| `LIKE 'ab%'`, hex key | 0.12 | 0.08 |
| `BETWEEN` on an integer key | 0.27 | 0.30 |
| `IS NOT NULL`, key in 1% of rows | 0.39 | 0.07 |
| two conditions (`AND`) | 1.10 | 0.60 |
| ids, `LIMIT 10` | 0.05 | 0.05 |
| whole rows, `LIMIT 100` | 34.3 | 1.43 |
| `GROUP BY` the hottest key (656,338 rows) | 2.55 | 1,905 (160 MB) |
| `GROUP BY` a key in 1% of rows | 0.38 | 95.4 |
| `GROUP BY` the hottest key of the rows `= 'v3'` on a 1% key | 0.10 | 8.5 |
| `AVG, MIN, MAX` of an integer key (82,208 rows) | 14.3 | 552 |
| `AVG, MIN, MAX` of an integer key in 1% of rows | 1.9 | 136 |
| `ORDER BY` an integer key `LIMIT 10` | 0.29 | 429 (54 MB) |
| `ORDER BY` an integer key `LIMIT 10` of the rows `= 'v3'` on the hottest key | 2.2 | 207 |
| `json_paths` (1,000 paths) | | 615 |
| `json_paths` of the rows `= 'v3'` on a 1% key (680 paths) | | 511 |

Every count and group is the same in A and J. `MAX` differs: J's is
`'n/a'`, since a path's values are compared by type (numbers, then
strings) and some rows hold that string; `MAX(json_extract(attrs, 'k12',
'number'))` reads the numbers only.

## 1,000,000 rows, 100 keys (`k100.json`)

Every key is in at least 1.8% of rows (the hottest in 80%), so A's columns
cost less here: A 283 MB (283 B/row), loaded in 14.7 s; J 222 MB (222
B/row), loaded in 26.1 s. Filters take 0.04 to 0.6 ms in A and 0.06 to 1.5
ms in J. `GROUP BY` the hottest key (804,755 rows) takes 2.9 ms in A and
2,442 ms in J, `AVG, MIN, MAX` of an integer key (130,188 rows) 41.9 ms
against 918 ms, and `ORDER BY` it `LIMIT 10` 0.26 ms against 729 ms.

## What it says

A JSON field is smaller than declared columns once most keys are sparse
(2.6 times at 1,000 keys), loads and opens faster, and filters on its paths
as fast, since every filter reads the field's terms. Reading a path outside
`WHERE` is where it loses: `GROUP BY`, aggregates and `ORDER BY` read each
row's stored object and parse it (`json_extract` over the rows found), 40
to 1,500 times slower than a declared column, and hold the rows in memory.
That is the next pull request's work.
