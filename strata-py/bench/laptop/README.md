# The laptop benchmark: Strata in one file

Part A of the laptop benchmark (Part B, the cluster, is
`strata-server/deploy/compare`): Strata as an app's database, search box and
recommender in one file, against the libraries apps use for each today, all
driven from Python as an app would drive them. Part C carries the same app
queries from the file to a sharded cluster.

| script | what | against |
|---|---|---|
| `shop_prep.py` | the Shop data: Amazon Reviews 2023 Video Games (137K products, 4.6M reviews), cleaned, and a bge-small-en-v1.5 vector per product | |
| `a1_app.py` | replacing SQLite: load, open, lookups, inserts, updates, deletes, pages, aggregates, joins, 4 readers and a writer, footprint | SQLite 3 (WAL, synchronous FULL with fullfsync, and NORMAL), DuckDB |
| `a2_search.py` | full text, vector and hybrid search on BEIR, the 11 kinds of `strata/results/workloads` plus phrase, prefix, typos (whole query and one word) and weighted fusion | SQLite FTS5 + sqlite-vec, Tantivy (tantivy-py), DuckDB fts + vss, LanceDB |
| `a_writes.py` | writes into a file with text and vector indexes: inserts of 1 to 10,000 per transaction, six kinds of update, a stream of review events at 1K, 10K, 50K a second and flat out while 4 threads query | SQLite FTS5 + sqlite-vec (FULL and NORMAL), DuckDB, LanceDB |
| `a3_recsys.py` | similar items (filtered, history left out) and a home feed (two sources, rules, at most 2 a brand) | LanceDB, sqlite-vec, FAISS HNSW + SQLite |
| `part_c.py`, `part_c.sh` | the Shop's 12 app queries in the app, on one server, on a cluster of 1 then 3 shards while slots move, and from `git clone` to a 3-shard cluster | Strata's own answers in the app |
| `common.py` | timing, the machine, BEIR collections, reports | |

Next items (the sequence encoder, walks and their fusion on Amazon 2014
Beauty, Sports and Toys) run with `strata/results/generative/tiger.py
--stage prep`, `--stage a8`, then `--stage eval --no-decoder --tag=-laptop`.

Every report names the machine it was measured on and the versions of
each library. Each system runs in its own process, so its memory is its own.
A kind a system cannot do is marked `not supported` with the reason; one the
app has to do itself (fusing two lists, sorting, counting) is run that way
and marked `app code`.

## Setup

```sh
python3 -m venv ~/bench-venv && . ~/bench-venv/bin/activate
pip install maturin numpy pyarrow duckdb lancedb tantivy sqlite-vec faiss-cpu fastembed psutil
cd strata-py && maturin develop --release && cd -
```

On macOS use a Python whose `sqlite3` module can load extensions (Homebrew's
or python.org's; Apple's system Python cannot), for sqlite-vec.

## Run

```sh
B=strata-py/bench/laptop; OUT=strata/results/laptop-2026-11/part-a
python $B/shop_prep.py --meta meta_Video_Games.jsonl --reviews Video_Games.jsonl --out ~/data/shop
python $B/a1_app.py --shop ~/data/shop --work ~/bench-work/a1 --out $OUT
python $B/a2_search.py --data $STRATA_DATA --collections scifact,nfcorpus,fiqa,trec-covid,quora --work ~/bench-work/a2 --out $OUT
python $B/a_writes.py --shop ~/data/shop --work ~/bench-work/aw --out $OUT
python $B/a3_recsys.py --shop ~/data/shop --work ~/bench-work/a3 --out $OUT
STRATA_WORK=~/bench-work $B/part_c.sh file ~/data/shop && $B/part_c.sh app && $B/part_c.sh server
$B/part_c.sh cluster && $B/part_c.sh devloop && $B/part_c.sh compare
```

`a2_search.py` needs a collection's workload, `strata-baselines --collections
NAME --workload-only` (nfcorpus has none committed).
