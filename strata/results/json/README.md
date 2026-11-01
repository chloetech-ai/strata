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
Linux 6.18). `k1000.json` and `k100.json` hold the run of the code that reads
JSON paths from the field's terms (JSON fields as of #178, #186 and #189,
with `GROUP BY`, aggregates and `ORDER BY` of paths read from the index, on
main at 169d39d). The column "J before" is the same benchmark on the same
machine earlier that day on the code of #189, which read each row's object
for them. Times are the median of 7 runs after one untimed run; `peak_mb`
is the most heap the first run held beyond what was held before it. `J
text` is the JSON field with `OPTIONS(text = true)`, measured for its size
only.

## 1,000,000 rows, 1,000 keys (`k1000.json`)

The most held key is in 66% of rows, the 10th in 10.5%, the 100th in 1.1%,
and 890 of the 1,000 keys are in fewer than 1% of rows.

| | declared columns (A) | JSON field (J) | J before |
|---|---|---|---|
| database | 657 MB (657 B/row) | 251 MB (251 B/row) | 251 MB |
| load (100,000-row batches) | 83.5 s | 31.6 s | 25.7 s |
| open | 24.0 ms | 7.4 ms | 6.4 ms |
| single-row INSERTs | 15 rows/s, 1,105 WAL B/row | 143 rows/s, 147 WAL B/row | 185 rows/s |

A segment of 100,000 rows by section, bytes a row: A 655 (columns 192,
keyword indexes 241, range indexes 93, doc store 61, key 31); J 249 (the
field's terms 147, doc store 71, key 31); J text 284 (the same and 35 of
text index). The terms that mark the rows a path's terms do not give
exactly (objects, arrays, long strings, `5.0`) add 0.05 B/row here (the
field's terms 147.21 B/row before, 147.26 now).

| query | A ms | J ms | J before ms |
|---|---|---|---|
| `= 'v3'` on the hottest key (66% of rows) | 0.12 | 0.27 | 0.14 |
| `= 'v3'` on a key in 1% of rows | 0.04 | 0.11 | 0.06 |
| `IN` of 4 values, hottest key | 0.30 | 0.67 | 0.38 |
| `LIKE 'ab%'`, hex key | 0.05 | 0.12 | 0.08 |
| `BETWEEN` on an integer key | 0.08 | 0.49 | 0.30 |
| `IS NOT NULL`, key in 1% of rows | 0.24 | 0.14 | 0.07 |
| two conditions (`AND`) | 0.43 | 1.02 | 0.60 |
| ids, `LIMIT 10` | 0.03 | 0.07 | 0.05 |
| whole rows, `LIMIT 100` | 30.7 | 2.30 | 1.43 |
| `GROUP BY` the hottest key (656,338 rows) | 2.44 | 2.69 (0.2 MB) | 1,905 (160 MB) |
| `GROUP BY` a key in 1% of rows | 0.34 | 0.45 | 95.4 |
| `GROUP BY` the hottest key of the rows `= 'v3'` on a 1% key | 0.09 | 5.5 | 8.5 |
| `AVG, MIN, MAX` of an integer key (82,208 rows) | 13.6 | 21.9 | 552 |
| `AVG, MIN, MAX` of an integer key in 1% of rows | 1.9 | 3.5 | 136 |
| `ORDER BY` an integer key `LIMIT 10` | 0.30 | 11.3 (1.0 MB) | 429 (54 MB) |
| `ORDER BY` an integer key `LIMIT 10` of the rows `= 'v3'` on the hottest key | 1.7 | 3.2 | 207 |
| `json_paths` (1,000 paths) | | 639 | 615 |
| `json_paths` of the rows `= 'v3'` on a 1% key (680 paths) | | 516 | 511 |

Every count and group is the same in A and J. `MAX` differs: J's is
`'n/a'`, since a path's values are compared by type (numbers, then
strings) and some rows hold that string; `MAX(json_extract(attrs, 'k12',
'number'))` reads the numbers only.

The filters, which this change does not touch, read 1.5 to 2 times slower
in J here than before, in both runs of this code on main. A run of the same
change before it was rebased onto main measured them as before (`= 'v3'` on
the hottest key 0.13 ms), and the builds from before and after the rebase,
run side by side twice at 1,000,000 rows (with a smaller segment step and 5
runs a query), gave 0.14 and 0.16 ms against 0.18 and 0.14 ms, the other
filters alike: this VM's run conditions, not the code, as far as these runs
tell. Loads are about 10% slower: the code of #189 and this code side by
side, three times each, built a 100,000-row segment of J in 1.20 to 1.82 s
against 1.39 to 1.70 s, and loaded 200,000 rows in 4.9 to 5.4 s against 5.5
to 6.3 s, since each row's terms now check which paths need a mark.
Single-row `INSERT`s measured 134 to 143 rows/s in the three full runs of
this change against 185 before; side by side, 300 `INSERT`s into 200,000
rows gave 362 to 559 rows/s before and 344 to 524 after.

## 1,000,000 rows, 100 keys (`k100.json`)

Every key is in at least 1.8% of rows (the hottest in 80%), so A's columns
cost less here: A 283 MB (283 B/row), loaded in 16.7 s; J 222 MB (222
B/row), loaded in 31.1 s (26.1 s before). Filters take 0.03 to 0.6 ms in A
and 0.1 to 1.3 ms in J. `GROUP BY` the hottest key (804,755 rows) takes 4.7
ms in A and 4.6 ms in J (2,442 ms before), `AVG, MIN, MAX` of an integer
key (130,188 rows) 28.9 ms against 63.7 ms (918 ms before), and `ORDER BY`
it `LIMIT 10` 0.25 ms against 29.0 ms (729 ms before).

## What it says

A JSON field is smaller than declared columns once most keys are sparse
(2.6 times at 1,000 keys), loads and opens faster, and filters on its paths
in 0.07 to 1 ms where declared columns take 0.03 to 0.4 ms, since every
filter reads the field's terms. Reading a path outside `WHERE` now reads
the same terms, without reading a row's object or holding the rows in
memory. A path's counts come from its terms' docID lists, as a facet's do,
so `GROUP BY` a path over all rows takes about what a declared column takes
(2.7 against 2.4 ms on the hottest key); over the few rows a filter keeps
it takes longer (5.5 against 0.09 ms), since it goes through each value's
docID list rather than those rows. Aggregates take 1.6 to 3 times as long
as a declared column's. `ORDER BY` a path `LIMIT 10` stays the furthest
behind (11 against 0.3 ms): a declared number's column is read best block
first, by its zone maps, until the 10th row is found, while a path's sort
reads every value of the path among the rows searched.
