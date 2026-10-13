# Strata

An embedded, single-file search database in the spirit of SQLite: one file,
ACID transactions, SQL. Every table is indexed three ways at once (posting
lists, a columnar forward index and vector ANN indexes), rows can link to
other rows (graph edges), and a cost-based planner combines all of it in one
statement, hybrid ranking and graph traversal included.

```sql
SELECT id, title, score() FROM docs
WHERE MATCH(body, 'vaccine AND (mrna OR "messenger rna") -animal')
  AND year BETWEEN 2019 AND 2021 AND category IN ('c01', 'c02')
ORDER BY 0.2 * bm25(body, :text) + 0.8 * similarity(emb, :q) DESC
LIMIT 10;
```

The design is in [DESIGN.md](DESIGN.md). The report, with formulas,
diagrams and results, is built into `report/strata.html` by
`scripts/build_report.py`.

## Layout

| path | what |
|---|---|
| `src/storage/` | file format, meta slots and shadow-paging commits, WAL, memtable, snapshots, primary keys, merges, vacuum |
| `src/segment/` | immutable segments of named, aligned sections, read zero-copy through `mmap` |
| `src/text/` | analyser, Loom and BP128 postings, positions, BM25 with MaxScore and Block-Max WAND |
| `src/forward/` | FOR-packed columns with zone maps, keyword dictionaries and Pyramid lists, range histograms, facets, LZ4 doc store |
| `src/vector/` | float32 and SQ8 (VNNI) vectors, flat, IVF and HNSW, filtered search |
| `src/graph/` | EDGES fields: out-edge CSR and reverse index per segment; breadth-first search, shortest paths, personalized PageRank |
| `src/query/` | SQL binder (sqlparser), logical plan, cost-based planner, executor, RRF and weighted fusion, graph predicates and scores |
| `src/db.rs` | `Db`: open, load, query |
| `src/baselines/` | SQLite + FTS5 and Tantivy baselines (feature `baselines`) |
| `src/bin/` | `strata` shell, `strata-eval`, component benchmarks, `strata-baselines`, `strata-graph-bench` |

## Using it

```sh
cargo build --release
target/release/strata db.strata "SELECT category, COUNT(*) FROM docs WHERE MATCH(body, 'india') GROUP BY category ORDER BY 2 DESC LIMIT 5" --plan
target/release/strata db.strata --param t='rapid covid tests' < queries.sql
```

SQL covers single-table `SELECT`: `MATCH` with phrases, prefixes, `AND`,
`OR` and `-negation`; comparisons, `BETWEEN`, `IN`, `AND`, `OR` and `NOT`
on structured fields; `ORDER BY` a column, `bm25(...)`,
`similarity(...)`/`distance(...)`, `rrf(...)` or a weighted sum of scores;
`GROUP BY` for facet counts; `COUNT(*)`; graph traversal with
`REACHABLE(...)`, `PATH(...)`, `hops()` and `ppr(...)`, seeded by keys or by
any subquery (a vector search, for instance):

```sql
SELECT id, hops() FROM papers
WHERE REACHABLE(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT 5), hops => 2, dir => 'both')
  AND year >= 2018
ORDER BY similarity(emb, :q) DESC LIMIT 20;
```

Functions take named options, for
example `similarity(emb, :q, nprobe => 32, strategy => 'prefilter')`,
`rrf(a, b, k => 60, depth => 100)` or `bm25(body, :t, stopwords => 0)`.
Writes go through the Rust API (`Database::begin`, `WriteTxn::insert`,
`delete`, `commit`).

## Evaluation

```sh
python3 scripts/prepare_beir.py /home/user/data/beir   # BEIR + embeddings (about 4.8 GB)
cargo test --release                                   # components, crash recovery, end-to-end SQL
scripts/run_eval.sh                                    # every experiment, in order (about 4 h)
python3 scripts/build_report.py                        # report/strata.html
```

Graphs (ogbn-arxiv, about 150 MB; vectors from Engram's static embedder):

```sh
python3 -I scripts/prepare_arxiv.py /home/user/data/graph/arxiv      # after downloading arxiv.zip and titleabs.tsv.gz there
engram-bench embed --input /home/user/data/graph/arxiv/arxiv.jsonl --out /home/user/data/graph/arxiv/arxiv.f32
cargo build --release --features baselines --bin strata-graph-bench
target/release/strata-graph-bench load
target/release/strata-graph-bench latency --sqlite /home/user/data/graph/arxiv/edges.sqlite
target/release/strata-graph-bench quality --seeds 3,5,10,20,50
python3 scripts/graph_quality_stats.py results/graph/quality_arxiv.rows.jsonl.gz --out results/graph/quality_stats.json
target/release/strata-graph-bench synthetic --nodes 1000000 --sqlite /home/user/data/graph/synthetic.sqlite
```

Results are in `results/` as JSON, one file per system, study and
collection; logs of the runs that produced them are in `results/logs/`.
