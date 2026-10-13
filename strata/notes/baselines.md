# Baselines: shared workloads, SQLite and Tantivy

This component defines the query workloads that Strata, SQLite and Tantivy all
run (DESIGN.md §8), the measurement harness, and the two baselines.

* `src/workload.rs` (always compiled): workload types, the deterministic
  generator, SQL in Strata's dialect, shared text helpers.
* `src/workload/run.rs` (always compiled): the `System` trait and the
  harness (warm/cold latency, pages read, nDCG@10).
* `src/baselines/` (feature `baselines`): `Sqlite` and `Tantivy`.
* `src/bin/strata-baselines.rs` (feature `baselines`): the experiment binary.
* `tests/workload_gen.rs`, `tests/workload_baselines.rs` (the latter needs the
  feature).
* Results: `results/workloads/<collection>.json`,
  `results/baselines/{sqlite,tantivy}_<collection>.json`,
  `results/baselines/truth_<collection>.json`,
  `results/baselines/compare_<collection>.json`.

Dependencies added (optional, feature `baselines`): `rusqlite 0.40.2`
(features `bundled`, `functions`; bundles SQLite 3.53.2 via
`libsqlite3-sys 0.38.2`) and `tantivy 0.26.2`. Changes outside the component's
files: the two `pub mod` lines in `lib.rs` (plus three lines of its module
docs), `[features]`, the optional dependencies and the `[[bin]]` entry in
`Cargo.toml`, and the corresponding `Cargo.lock` entries. The harness lives in
the submodule file `src/workload/run.rs` next to `src/workload.rs`.

## API

### Workloads (`strata::workload`)

```rust
let coll = strata::data::load("fiqa", None)?;
let w: Workload = workload::generate(&coll, &WorkloadConfig::default());
w.save(Path::new("results/workloads/fiqa.json"))?;      // or Workload::load
for i in w.of_kind(Kind::FilteredVector) {
    let q: &WorkloadQuery = &w.queries[i];
    // q.sql        SQL in Strata's dialect; bind :q = coll.queries[q.query?].vector
    // q.count_sql  COUNT(*) form, for kinds that report a count
    // q.query      index into coll.queries (text and/or vector)
    // q.words      boolean / facet words
    // q.filter     Vec<Pred>: Year(lo, hi) | Rating(lo, hi) | Category(vals) | Topic(vals), ANDed, inclusive
    // q.shape, q.target, q.matches   filter shape, target selectivity, exact match count
}
```

* `Kind`: `bm25, and2, and3, or3, filter, filtered_bm25, vector,
  filtered_vector, hybrid, filtered_hybrid, facet` (`Kind::ALL`, `name`,
  `parse`, `judged`, `uses_vector`).
* `Shape`: `year, rating, category, year_rating, year_category,
  rating_category` (query independent) and `topic_near`, `topic_far`.
* Helpers every system should use for identical semantics:
  `query_terms(text)` (the BM25 terms the baselines search: lowercase
  alphanumeric tokens minus `STOPWORDS`), `tokens`, `is_stopword`,
  `filter_matches`, `filter_sql`, `rrf(rankings, k)` (Σ 1/(60 + rank), ties by
  id), constants `TOP_K = 10`, `FACET_K = 5`, `RRF_K = 60`,
  `RRF_DEPTH = 100`.

### Harness (`strata::workload::run`)

```rust
pub trait System {
    fn name(&self) -> &str;
    fn supports(&self, kind: Kind) -> bool;
    fn run(&mut self, q: &WorkloadQuery, coll: &Collection) -> Result<QueryOutput>;
    fn evict_caches(&mut self) -> Result<()>;   // close, iostat::evict the files, reopen
    fn files(&self) -> Vec<PathBuf>;            // every file queries read
}
pub struct QueryOutput { pub ids: Vec<String>, pub count: Option<u64>, pub facets: Option<Vec<(String, u64)>> }

let run: KindRun = run_kind(&mut sys, &w, &coll, Kind::Bm25, &RunOptions::default())?;
run.report   // KindReport: queries, reps, warm/cold Latency {n, mean_ms, p50_ms, p99_ms},
             // cold_pages, open_pages (Pages {mean, p50, p99, max}), ndcg10, mean_results,
             // by_target, by_shape
run.outputs  // Vec<(workload index, QueryOutput)> from the untimed warm-up pass
```

Strata's evaluation driver implements `System` for the engine and calls
`run_kind` per kind; `BuildReport {seconds, phases, bytes, components}` is the
shared shape for load reports.

Measurement protocol (`run_kind`):

* **Warm**: one untimed pass over the kind's queries (fills caches, records
  outputs), then up to `reps` (3) timed passes, fewer if the untimed pass
  shows they would exceed `warm_budget_secs` (60 s; at least one). A query's
  latency is the median of its repetitions; mean/p50/p99 are over queries.
* **Cold**: up to `cold_sample` (100) queries, evenly spaced over the kind and
  run in shuffled order until `cold_budget_secs` (60 s) are spent (at least
  5). Before each: `evict_caches` (close the connection or drop the reader,
  `posix_fadvise(DONTNEED)` every file, reopen and load what an application
  loads at startup). Pages read = increase of the files' resident pages
  (`mincore`) across the query; `open_pages` = pages resident after the
  reopen, before the query.
* `max_per_kind` / `max_vector` cap the queries per kind (evenly spaced), the
  latter only for kinds that use vectors (SQLite scans every vector).
* `ndcg10`: mean `metrics::ndcg_at_k(ids, qrels, 10)` over judged queries of
  `bm25`, `vector`, `hybrid` (no BEIR identical-id removal; FiQA has 55
  query ids that collide with document ids).

### Baselines (`strata::baselines`, feature `baselines`)

```rust
let (mut sqlite, report) = Sqlite::build(Path::new(".../fiqa.sqlite"), &coll)?;   // or Sqlite::open
let (mut tantivy, report) = Tantivy::build(Path::new(".../fiqa.tantivy"), &coll, &TantivyOptions::default())?;
sqlite.explain(&q, &coll)?;     // EXPLAIN QUERY PLAN of the statements answering q
Sqlite::version(); Tantivy::version(); tantivy.segments(); Sqlite::remove(path)?;
```

### Experiment binary

```text
cargo run --release --features baselines --bin strata-baselines -- \
    --collections scifact,fiqa,trec-covid [--systems sqlite,tantivy] [--limit N] \
    [--kinds bm25,filter,...] [--reps 3] [--warm-budget 60] [--cold-sample 100] \
    [--cold-budget 60] [--max-per-kind N] [--max-vector N] \
    [--db-dir /home/user/data/baselines] [--results-dir DIR] [--keep] [--reuse] \
    [--regen-workload] [--workload-only] [--tantivy-threads 1] [--tantivy-memory-mb 512]
```

Per collection it loads the data, loads (or generates) the workload, builds
each system in `--db-dir`, drops the documents from memory, runs every kind
warm and cold, and writes `<system>_<tag>.json` (`tag` = collection, plus
`_<limit>` with `--limit`), `truth_<tag>.json` (SQLite's exact vector and
hybrid top-10 per workload query: the reference for Strata's recall) and
`compare_<tag>.json` (per kind: identical ids, mean top-10 overlap, identical
counts, identical facets). Databases are deleted afterwards unless `--keep`;
`--reuse` reopens kept ones (and their saved build reports).

## Workload definition

Generated by `workload::generate` with `WorkloadConfig::default()`:
seed 20261012, `max_text_queries` 2000, `max_boolean` 300,
`filter_instances` 5, `per_target` 30, targets 0.1%, 1%, 10%, 50%,
tolerance 25% (relative).

| kind | queries | SQL (Strata dialect) |
|---|---|---|
| `bm25` | every judged query whose text has a non-stopword term (Quora: a seeded 2,000-query sample) | `SELECT id FROM docs ORDER BY bm25(body, '<text>') DESC LIMIT 10` |
| `and2`, `and3`, `or3` | ≤ 300 queries with enough content words; 2 or 3 of them at random | `SELECT id FROM docs WHERE MATCH(body, 'a AND b') LIMIT 10` + `SELECT COUNT(*) ...` |
| `filter` | 5 per (shape, target) where the shape can reach the target | `SELECT id FROM docs WHERE <f> ORDER BY rating DESC, id LIMIT 10` + `COUNT(*)` |
| `filtered_bm25` | 30 per target, random structured shape | `... WHERE <f> ORDER BY bm25(body, '<text>') DESC LIMIT 10` |
| `vector` | the query sample | `SELECT id FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 10` |
| `filtered_vector` | 30 per target × {structured, topic_near, topic_far} | `... WHERE <f> ORDER BY similarity(emb, :q) DESC LIMIT 10` |
| `hybrid` | as `bm25` | `... ORDER BY rrf(bm25(body, '<text>'), similarity(emb, :q), depth => 100) DESC LIMIT 10` |
| `filtered_hybrid` | as `filtered_vector` | same with `WHERE <f>` |
| `facet` | ≤ 300 queries, 1 or 2 content words | `SELECT category, COUNT(*) FROM docs WHERE MATCH(body, 'a AND b') GROUP BY category ORDER BY 2 DESC, 1 LIMIT 5` |

Details:

* **Content words** (boolean, facet): ASCII-lowercase alphabetic tokens of
  the query text, length ≥ 3, not in `STOPWORDS` (NLTK's English list without
  apostrophe forms; 153 words), with document frequency ≥ 1 under the simple
  tokenizer (lowercased alphanumeric runs) over `body`.
* **Filters** are exact: the structured fields are coded over their sorted
  distinct values; a single-field shape is a random-start window (year,
  rating) or a random-order greedy subset (category) whose exact count is
  closest to the target; a two-field shape takes about √target on the first
  field and tunes the second on the exact joint counts. A filter is kept only
  if its exact count is within 25% of the target (up to 16 random restarts),
  so shapes that cannot reach a target are absent there: year alone has 35
  values (≈2.9% each) and misses 0.1% and 1%; category alone (Zipf, smallest
  ≈0.33%) misses 0.1%; rating has a 0.01 grid (≈0.2% per value) and reaches
  0.1% only at its half-weight end points.
* **Topic filters** (vector and hybrid kinds) are correlated with the query:
  topics are ordered by the similarity of their centroid (mean document
  vector) to the query vector; `topic_near` starts from the closest topic,
  `topic_far` from the farthest, adds topics in that order while the count
  gets closer to the target, and, when one topic is larger than the target
  (always at 0.1% and 1%: topics hold 1.3–7% of the documents), narrows it
  with a rating range.
* **Semantics shared by all systems**: BM25 ranks only documents containing
  at least one query term (a filtered BM25 query may return fewer than 10);
  boolean results are the first 10 matches in load order; filter results are
  ordered by `rating DESC, id`; facets by count, then value; RRF fuses the
  top-100 of BM25 and of vector search, both restricted to the filter, ties
  by id.
* Sizes: scifact 3,042 queries (0.8 MB), fiqa 4,084 (0.9 MB), trec-covid
  1,288 (0.4 MB); a Quora 100k prefix gives 8,140 queries (1.5 MB).
  Generation takes 0.1–2 s. `tests/workload_gen.rs` checks that
  `results/workloads/scifact.json` matches the generator.

## System configurations

### SQLite

* SQLite 3.53.2 bundled by rusqlite 0.40.2 (compile options include
  `ENABLE_FTS5`, `ENABLE_DBSTAT_VTAB`, `ENABLE_STAT4`; the build checks FTS5).
* Schema: `docs(id TEXT PRIMARY KEY, title TEXT, text TEXT, year INTEGER,
  category TEXT, rating REAL, topic TEXT, emb BLOB)` (rowid table, rowid =
  load order + 1; vectors as 4096-byte little-endian float32 blobs);
  `CREATE INDEX` on year, category, rating, topic.
* FTS5: `CREATE VIRTUAL TABLE fts USING fts5(body, content='',
  contentless_delete=1, tokenize='porter unicode61')`, rowid = docs rowid.
  Contentless because the body (title + "\n" + text) is not a `docs` column:
  external content would need a duplicate stored column or a view, while a
  contentless table stores only the index and the per-document lengths that
  `bm25()` needs; `contentless_delete=1` keeps it updatable. Default
  `detail=full` (positions). Tokenizer: unicode61 (Unicode word split, case
  folding, diacritics removed) + Porter stemmer; no stopwords.
* Load: `journal_mode=WAL`; one transaction with all inserts (docs and fts
  rows), then the four `CREATE INDEX`, then `INSERT INTO fts(fts)
  VALUES('optimize')` (one FTS5 segment); `COMMIT`; `ANALYZE`;
  `PRAGMA wal_checkpoint(TRUNCATE)`. Load time covers all of it.
* Pragmas: defaults except WAL: `page_size` 4096, `cache_size` -2000
  (2000 KiB page cache), `mmap_size` 0, `synchronous` FULL, `temp_store`
  default (file). One connection, prepared statements
  (`prepare_cached`, capacity 128).
* Queries:
  * BM25: `SELECT docs.id FROM (SELECT rowid AS r, rank AS s FROM fts WHERE
    fts MATCH ? ORDER BY rank LIMIT 10) AS m JOIN docs ON docs.rowid = m.r
    ORDER BY m.s`, with `MATCH '"t1" OR "t2" ...'` over `query_terms`.
    FTS5's `bm25()` uses k1 = 1.2, b = 0.75 (fixed).
  * Filtered BM25: `SELECT docs.id FROM fts JOIN docs ON docs.rowid =
    fts.rowid WHERE fts MATCH ? AND <filter> ORDER BY fts.rank LIMIT 10`
    (plan: FTS5 scan, rowid lookup per match, filter, sort).
  * Boolean: `SELECT count(*) FROM fts WHERE fts MATCH '"a" AND "b"'` and
    the first 10 rowids joined to `docs`.
  * Filter: `SELECT count(*) FROM docs WHERE <filter>` and `SELECT id FROM
    docs WHERE <filter> ORDER BY rating DESC, id LIMIT 10` (B-tree indexes
    chosen by the planner with STAT4).
  * Vectors: `vdot(blob, blob)` registered with `create_scalar_function`
    (16-lane f32 dot product over the little-endian bytes), `SELECT id FROM
    docs [WHERE <filter>] ORDER BY vdot(emb, ?) DESC LIMIT 10`: a full scan
    (or an index range scan plus row lookups when filtered). This is what
    SQLite offers without an extension; the sqlite-vec extension (`vec0`
    virtual table) is the alternative and is also a brute-force scan.
  * Hybrid: the BM25 and vector statements with `LIMIT 100`, fused in the
    application by `workload::rrf`.
  * Facets: `SELECT docs.category, count(*) FROM fts JOIN docs ON docs.rowid
    = fts.rowid WHERE fts MATCH ? GROUP BY docs.category ORDER BY 2 DESC, 1
    LIMIT 5`.
  * `EXPLAIN QUERY PLAN` of one query per (kind, shape) is in
    `sqlite_<collection>.json` under `extra.plans`.

### Tantivy

* Tantivy 0.26.2, MMAP directory, one index directory per collection.
* Schema: `id` STRING|STORED|FAST; `title`, `text` STORED; `body` TEXT with
  tokenizer `en_stem` (SimpleTokenizer, RemoveLongFilter(40), LowerCaser,
  Snowball English stemmer; no stopwords), `WithFreqsAndPositions`; `year`
  i64 INDEXED|FAST; `rating` f64 INDEXED|FAST; `category`, `topic`
  STRING|FAST; `seq` u64 FAST (load order).
* Load: `writer_with_num_threads(1, 512 MiB)` (single thread like SQLite;
  `--tantivy-threads`), `NoMergePolicy`, one commit, then one explicit merge
  of all segments into one and garbage collection. Load time includes the
  merge. Doc store: LZ4, 16 KiB blocks (defaults).
* Reader: `ReloadPolicy::Manual`, single-threaded search. Result ids are read
  from the `id` fast column (dictionary-encoded), not the doc store.
* Queries:
  * BM25: `BooleanQuery` of `Should` `TermQuery`s (`WithFreqs`) over the
    `en_stem` analysis of `query_terms`, `TopDocs::with_limit(10)
    .order_by_score()` (Block-WAND). BM25 k1 = 1.2, b = 0.75 (fixed).
  * Filtered BM25: `BooleanQuery[Must(text disjunction),
    Must(ConstScoreQuery(pred, 0))...]` (an intersection; no Block-WAND).
  * Boolean: `Must`/`Should` `TermQuery`s (`Basic`), collected by
    `(Count, TopDocs.order_by_u64_field("seq", Asc))`.
  * Filter: `Must` clauses: `RangeQuery` on year/rating (Tantivy evaluates it
    by scanning the fast column because the fields are FAST), `TermQuery` /
    `TermSetQuery` on category/topic (postings); collected by `(Count,
    TopDocs.order_by((rating Desc, id Asc)))` with Tantivy's sort-key tuple
    API, so ties break by id exactly like SQL.
  * Facets: a custom collector counting category ordinals of the matches in
    the fast column, merged across segments by value.
  * Vector and hybrid kinds: unsupported (Tantivy has no vector search).

## Results (validation runs on the shared machine)

Measured on 2026-10-12 with `strata-baselines --collections scifact,fiqa,trec-covid`
(defaults: `--reps 3 --warm-budget 60 --cold-sample 100 --cold-budget 60`)
while other agents were compiling and benchmarking on the same 4 cores, so
latencies are noisy (repeated SciFact and FiQA runs differed by up to ~50%
on sub-millisecond kinds and by a few per cent on long ones); quality
numbers and agreement are exact and were identical across runs. Full results: `results/baselines/`.

| | SciFact | FiQA | TREC-COVID |
|---|---:|---:|---:|
| documents | 5,183 | 57,638 | 171,332 |
| SQLite load / size | 0.8 s / 36 MiB | 4.2 s / 311 MiB | 30.9 s / 1,014 MiB |
| Tantivy load / size | 0.7 s / 7.3 MiB | 4.1 s / 47 MiB | 21.3 s / 180 MiB |
| nDCG@10 BM25, SQLite / Tantivy | 0.684 / 0.687 | 0.254 / 0.251 | 0.641 / 0.667 |
| nDCG@10 exact vector (SQLite) | 0.718 | 0.421 | 0.818 |
| nDCG@10 RRF hybrid (SQLite) | 0.733 | 0.364 | 0.833 |
| BM25 warm p50, SQLite / Tantivy | 3.4 / 0.38 ms | 20 / 0.71 ms | 147 / 1.2 ms |
| filter warm p50, SQLite / Tantivy | 0.87 / 0.33 ms | 12 / 0.66 ms | 36 / 1.1 ms |
| exact vector warm p50 (SQLite) | 14 ms | 144 ms | 517 ms |

Observations:

* The BM25 nDCG values match published BM25 numbers for these collections
  (SciFact ≈0.67, FiQA ≈0.24, TREC-COVID ≈0.66 in BEIR) and the dense numbers
  match the published ones for these embeddings, so both pipelines rank
  correctly. RRF helps where BM25 is competitive (SciFact, TREC-COVID) and
  hurts on FiQA, where BM25 is much weaker than the vectors.
* SQLite's size is dominated by the vectors (4 KiB per row, 56–73% of the
  file); FTS5 alone is 2.8 / 19 / 68 MiB against Tantivy's postings,
  positions and terms of 2.6 / 16.8 / 59 MiB. Tantivy's LZ4 doc store is
  about 2× smaller than SQLite's rows without the vectors.
* FTS5 scores every match (no pruning), so its BM25 latency grows with the
  posting lists: 9× (SciFact) to 120× (TREC-COVID) slower than Tantivy's
  Block-WAND. Short boolean queries are comparable (SQLite from 2× faster to
  3.5× slower).
* SQLite's exact vector scan costs ≈2.5–3 µs per row warm (reading each
  row's overflow chain dominates) and reads the whole table cold (240k pages
  on TREC-COVID). Filtered vector search through a B-tree index is cheap at
  0.1–1%; at 50% it costs 60–75% of a full scan for half the rows (random
  row lookups).
* Cold queries on SQLite that touch rows (filters, filtered BM25, facets)
  read thousands of pages because the filter columns sit after the text in
  rows that spill into overflow pages; Tantivy reads its columns
  sequentially from the fast-field file.

### Build

| collection | system | load s | phases (s) | size MiB | largest components (MiB) |
|---|---|---:|---|---:|---|
| scifact (5,183) | sqlite | 0.8 | insert 0.4, indexes 0.1, fts_optimize 0.1, commit 0.2, analyze 0.0, checkpoint 0.0 | 36.0 | docs 30.0, fts_data 2.8, sqlite_autoindex_docs_1 0.1, docs_rating 0.1, fts_docsize 0.1 |
| scifact (5,183) | tantivy | 0.7 | index 0.7, merge_1_segments 0.0 | 7.3 | store 4.6, pos 1.3, idx 1.0, term 0.3, fast 0.1 |
| fiqa (57,638) | sqlite | 4.2 | insert 2.3, indexes 0.2, fts_optimize 0.3, commit 1.2, analyze 0.0, checkpoint 0.2 | 310.5 | docs 281.5, fts_data 19.1, docs_rating 0.9, sqlite_autoindex_docs_1 0.9, fts_docsize 0.7 |
| fiqa (57,638) | tantivy | 4.1 | index 4.1, merge_1_segments 0.0 | 47.4 | store 29.6, pos 9.0, idx 6.8, term 1.0, fast 0.7 |
| trec-covid (171,332) | sqlite | 30.9 | insert 20.0, indexes 1.3, fts_optimize 1.3, commit 7.5, analyze 0.2, checkpoint 0.6 | 1014.1 | docs 926.6, fts_data 67.8, sqlite_autoindex_docs_1 3.0, docs_rating 2.8, fts_docsize 2.1 |
| trec-covid (171,332) | tantivy | 21.3 | index 21.3, merge_1_segments 0.0 | 179.7 | store 117.0, pos 31.5, idx 23.2, term 4.3, fast 2.7 |

### scifact

Latency in ms (warm: median of repetitions per query; cold: one run after eviction); pages are 4 KiB.

| kind | system | n | warm mean | warm p50 | warm p99 | cold n | cold p50 | cold p99 | pages p50 | pages p99 | nDCG@10 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| bm25 | sqlite | 300 | 3.73 | 3.38 | 9.87 | 100 | 11.1 | 21.3 | 73 | 105 | 0.6842 |
| bm25 | tantivy | 300 | 0.399 | 0.375 | 0.709 | 100 | 1.63 | 5.55 | 237 | 288 | 0.6866 |
| and2 | sqlite | 300 | 0.158 | 0.129 | 0.388 | 100 | 1.1 | 8.52 | 34 | 67 |  |
| and2 | tantivy | 300 | 0.199 | 0.262 | 0.416 | 100 | 1.06 | 6.19 | 153 | 167 |  |
| and3 | sqlite | 299 | 0.11 | 0.104 | 0.202 | 100 | 0.954 | 6.91 | 31 | 52 |  |
| and3 | tantivy | 299 | 0.11 | 0.0584 | 0.428 | 100 | 0.679 | 3.82 | 166 | 197 |  |
| or3 | sqlite | 299 | 0.134 | 0.124 | 0.261 | 100 | 1.06 | 5.02 | 36 | 70 |  |
| or3 | tantivy | 299 | 0.271 | 0.27 | 0.307 | 100 | 1 | 2.01 | 170 | 199 |  |
| filter | sqlite | 104 | 1.98 | 0.869 | 7.11 | 100 | 24.3 | 102 | 528 | 5038 |  |
| filter | tantivy | 104 | 0.32 | 0.328 | 0.481 | 100 | 0.419 | 4.41 | 1 | 1 |  |
| filtered_bm25 | sqlite | 120 | 5.3 | 4.19 | 15 | 100 | 19.9 | 107 | 221 | 2343 |  |
| filtered_bm25 | tantivy | 120 | 0.345 | 0.384 | 0.543 | 100 | 1.44 | 5.02 | 222 | 292 |  |
| vector | sqlite | 300 | 13.3 | 13.9 | 17.2 | 100 | 31.8 | 52.7 | 7864 | 7864 | 0.7181 |
| filtered_vector | sqlite | 360 | 2.63 | 1.31 | 10.2 | 100 | 34.4 | 203 | 1581 | 7624 |  |
| hybrid | sqlite | 300 | 17 | 16.8 | 24.9 | 100 | 38.4 | 52.9 | 7876 | 7906 | 0.7332 |
| filtered_hybrid | sqlite | 360 | 8.37 | 8.37 | 16.3 | 100 | 84.4 | 159 | 2415 | 7632 |  |
| facet | sqlite | 300 | 0.423 | 0.132 | 2.91 | 100 | 5.01 | 96.2 | 73 | 3534 |  |
| facet | tantivy | 300 | 0.0173 | 0.0144 | 0.0413 | 100 | 0.618 | 0.915 | 130 | 166 |  |

Pages resident after reopening (p50): sqlite 25, tantivy 95. Peak RSS of the run: 92 MiB.

SQLite vs Tantivy: and2 overlap 0.972, same count 0.89; and3 overlap 0.983, same count 0.96; bm25 overlap 0.923; facet overlap 1.000, same facets 0.94; filter overlap 1.000, same count 1.00; filtered_bm25 overlap 0.953; or3 overlap 0.988, same count 0.80.

### fiqa

Latency in ms (warm: median of repetitions per query; cold: one run after eviction); pages are 4 KiB.

| kind | system | n | warm mean | warm p50 | warm p99 | cold n | cold p50 | cold p99 | pages p50 | pages p99 | nDCG@10 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| bm25 | sqlite | 648 | 22.1 | 20.2 | 59 | 100 | 47.1 | 119 | 253 | 311 | 0.2537 |
| bm25 | tantivy | 648 | 0.83 | 0.711 | 2.06 | 100 | 2.63 | 4.93 | 479 | 642 | 0.2513 |
| and2 | sqlite | 300 | 0.308 | 0.24 | 1.1 | 100 | 1.5 | 3.53 | 32 | 68 |  |
| and2 | tantivy | 300 | 0.339 | 0.358 | 0.536 | 100 | 1.42 | 4.18 | 264 | 303 |  |
| and3 | sqlite | 300 | 0.332 | 0.321 | 0.783 | 100 | 1.73 | 5.58 | 41 | 76 |  |
| and3 | tantivy | 300 | 0.3 | 0.365 | 0.609 | 100 | 1.41 | 1.91 | 313 | 346 |  |
| or3 | sqlite | 300 | 0.745 | 0.659 | 2.04 | 100 | 3.53 | 9.63 | 34 | 67 |  |
| or3 | tantivy | 300 | 0.478 | 0.466 | 0.706 | 100 | 1.85 | 4.79 | 311 | 346 |  |
| filter | sqlite | 100 | 16.7 | 12.4 | 52.2 | 100 | 251 | 856 | 5177 | 23672 |  |
| filter | tantivy | 100 | 0.841 | 0.664 | 1.78 | 100 | 1.21 | 2.38 | 146 | 148 |  |
| filtered_bm25 | sqlite | 120 | 23.7 | 20.2 | 60 | 100 | 57.9 | 479 | 702 | 9225 |  |
| filtered_bm25 | tantivy | 120 | 0.892 | 0.764 | 1.77 | 100 | 3.4 | 9.39 | 537 | 715 |  |
| vector | sqlite | 648 | 148 | 144 | 211 | 100 | 411 | 827 | 72814 | 72814 | 0.4214 |
| filtered_vector | sqlite | 360 | 35.6 | 17.1 | 135 | 100 | 298 | 1.76e+03 | 16019 | 71143 |  |
| hybrid | sqlite | 648 | 178 | 173 | 252 | 98 | 594 | 769 | 72871 | 72927 | 0.3636 |
| filtered_hybrid | sqlite | 360 | 80.9 | 67.9 | 197 | 87 | 549 | 1.83e+03 | 18327 | 70869 |  |
| facet | sqlite | 300 | 3.81 | 1.45 | 22.4 | 100 | 30.4 | 647 | 700 | 13436 |  |
| facet | tantivy | 300 | 0.0767 | 0.0531 | 0.205 | 100 | 0.991 | 5.14 | 212 | 289 |  |

Pages resident after reopening (p50): sqlite 18, tantivy 136. Peak RSS of the run: 478 MiB.

SQLite vs Tantivy: and2 overlap 0.980, same count 0.87; and3 overlap 0.974, same count 0.88; bm25 overlap 0.918; facet overlap 1.000, same facets 0.88; filter overlap 1.000, same count 1.00; filtered_bm25 overlap 0.940; or3 overlap 0.985, same count 0.65.

### trec-covid

Latency in ms (warm: median of repetitions per query; cold: one run after eviction); pages are 4 KiB.

| kind | system | n | warm mean | warm p50 | warm p99 | cold n | cold p50 | cold p99 | pages p50 | pages p99 | nDCG@10 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| bm25 | sqlite | 50 | 144 | 147 | 332 | 50 | 183 | 353 | 860 | 1130 | 0.6407 |
| bm25 | tantivy | 50 | 1.58 | 1.2 | 9.55 | 50 | 4.13 | 16.3 | 878 | 1314 | 0.6668 |
| and2 | sqlite | 50 | 0.904 | 0.734 | 3.32 | 50 | 3.86 | 10.9 | 68 | 200 |  |
| and2 | tantivy | 50 | 0.502 | 0.426 | 1.46 | 50 | 1.77 | 3.64 | 462 | 537 |  |
| and3 | sqlite | 49 | 1.18 | 1.04 | 2.76 | 49 | 6.49 | 14.1 | 109 | 258 |  |
| and3 | tantivy | 49 | 0.41 | 0.392 | 0.874 | 49 | 2.24 | 8.06 | 554 | 621 |  |
| or3 | sqlite | 49 | 3.21 | 3.23 | 6.47 | 49 | 6.76 | 15.5 | 97 | 214 |  |
| or3 | tantivy | 49 | 0.85 | 0.911 | 1.24 | 49 | 3.12 | 7.82 | 524 | 627 |  |
| filter | sqlite | 100 | 71 | 36.1 | 226 | 37 | 1.2e+03 | 4.11e+03 | 16258 | 114560 |  |
| filter | tantivy | 100 | 1.51 | 1.07 | 4.23 | 100 | 3.19 | 9.6 | 552 | 643 |  |
| filtered_bm25 | sqlite | 120 | 130 | 131 | 258 | 100 | 233 | 916 | 1454 | 13370 |  |
| filtered_bm25 | tantivy | 120 | 2.55 | 1.98 | 5.97 | 100 | 8 | 17.1 | 1120 | 1669 |  |
| vector | sqlite | 50 | 527 | 517 | 687 | 21 | 2.73e+03 | 3.97e+03 | 239497 | 239497 | 0.8178 |
| filtered_vector | sqlite | 360 | 117 | 54 | 476 | 39 | 943 | 5.63e+03 | 50868 | 222825 |  |
| hybrid | sqlite | 50 | 737 | 746 | 1.07e+03 | 20 | 2.94e+03 | 3.86e+03 | 239795 | 239903 | 0.8331 |
| filtered_hybrid | sqlite | 360 | 345 | 322 | 702 | 24 | 2.56e+03 | 4.89e+03 | 61528 | 224216 |  |
| facet | sqlite | 50 | 19.7 | 9.03 | 74 | 50 | 189 | 1.87e+03 | 4688 | 42323 |  |
| facet | tantivy | 50 | 0.186 | 0.123 | 0.845 | 50 | 1.2 | 7.3 | 311 | 407 |  |

Pages resident after reopening (p50): sqlite 18, tantivy 168. Peak RSS of the run: 1351 MiB.

SQLite vs Tantivy: and2 overlap 0.970, same count 0.78; and3 overlap 0.982, same count 0.84; bm25 overlap 0.876; facet overlap 1.000, same facets 0.74; filter overlap 1.000, same count 1.00; filtered_bm25 overlap 0.878; or3 overlap 0.992, same count 0.37.

### Filtered kinds by target selectivity (warm p50, ms)

| collection | system | kind | 0.1% | 1% | 10% | 50% |
|---|---|---|---:|---:|---:|---:|
| scifact | sqlite | filter | 0.247 | 0.845 | 2.04 | 4.4 |
| scifact | sqlite | filtered_bm25 | 6.65 | 6.04 | 2.98 | 3.63 |
| scifact | sqlite | filtered_vector | 0.168 | 0.268 | 1.69 | 7.59 |
| scifact | sqlite | filtered_hybrid | 7.53 | 7.36 | 6.44 | 12.3 |
| scifact | tantivy | filter | 0.185 | 0.319 | 0.353 | 0.364 |
| scifact | tantivy | filtered_bm25 | 0.159 | 0.384 | 0.403 | 0.461 |
| fiqa | sqlite | filter | 3.8 | 9.48 | 23.6 | 26.9 |
| fiqa | sqlite | filtered_bm25 | 37.2 | 20.2 | 17.3 | 16.4 |
| fiqa | sqlite | filtered_vector | 2.83 | 4.55 | 21.8 | 105 |
| fiqa | sqlite | filtered_hybrid | 64.2 | 45.6 | 60.6 | 135 |
| fiqa | tantivy | filter | 0.546 | 0.579 | 0.909 | 1.46 |
| fiqa | tantivy | filtered_bm25 | 0.622 | 0.731 | 1.04 | 1.15 |
| trec-covid | sqlite | filter | 18.3 | 34.4 | 95.4 | 127 |
| trec-covid | sqlite | filtered_bm25 | 156 | 137 | 130 | 122 |
| trec-covid | sqlite | filtered_vector | 12.5 | 14.1 | 75.1 | 326 |
| trec-covid | sqlite | filtered_hybrid | 385 | 191 | 225 | 531 |
| trec-covid | tantivy | filter | 0.734 | 0.715 | 1.59 | 3.01 |
| trec-covid | tantivy | filtered_bm25 | 1.37 | 1.52 | 2.65 | 4.33 |

### Quora

Not run in full here. A validation run on a 100,000-document prefix
(`--limit 100000 --max-vector 100 --cold-sample 30 --cold-budget 30
--warm-budget 30`, results not committed) completed in about 6 minutes:
2,000-query sample, SQLite 4.2 s / 455 MiB, Tantivy 1.3 s / 10 MiB, filter
counts and top-10s identical, SQLite exact vector search 212 ms per query
(2.1 µs per row). (nDCG on a prefix is meaningless: most judged documents
lie outside it.) Peak RSS was 2.4 GiB, because `data::load` reads the whole
`doc_vecs.f32` (2.1 GB for Quora) before keeping the prefix; a full Quora run
should peak around 4.5 GiB (the file buffer plus the vectors), which fits an
isolated run but not the shared 3.5 GiB budget.

At ≈1.1 s per full-scan query on 523k documents, SQLite's vector kinds need a
cap. Recommended final runs (one at a time, on an idle machine):

```text
cargo build --release --features baselines
target/release/strata-baselines --collections scifact,fiqa,trec-covid     # ≈ 2 + 16 + 17 min here
target/release/strata-baselines --collections quora --max-vector 300      # ≈ 45 min; ≈ 5 GB disk at peak (database + WAL)
```

The workload files are deterministic, so a re-run regenerates
`results/workloads/quora.json` identically on any machine (it is not
committed yet). Databases are deleted after each run unless `--keep`.


## Fairness caveats

* **BM25 differs by design.** FTS5 and Tantivy fix k1 = 1.2, b = 0.75 and use
  different IDF formulas (FTS5: `ln((N - n + 0.5)/(n + 0.5))` floored at
  1e-6; Tantivy: `ln(1 + (N - n + 0.5)/(n + 0.5))`); Strata uses k1 = 0.9,
  b = 0.4. Stemmers differ (FTS5 original Porter, Tantivy Snowball English),
  so boolean counts agree on 37–99% of queries (an OR of three frequent
  words is the most sensitive) and top-5 facets on 74–97%, while filter
  counts and top-10s agree exactly. Typical cases: Porter stems `news` to
  `new` and `use` to `us` (FiQA `behind AND news`: 146 vs 33 matches), and
  `bonus` to `bonu` but `bonuses` to `bonus` (`find AND bonus`: 42 vs 60);
  FTS5's unicode61 also folds diacritics. `compare_*.json` lists examples. Both baselines index stopwords and drop them from queries
  (`query_terms`); Strata drops them from its index too, so its index is
  smaller for that reason alone.
* **SQLite's schema is the one specified**, with the vector in the `docs`
  row after the text. Rows exceed a page, so the record spills into overflow
  pages; evaluating a predicate on a column that is not in the chosen index
  (e.g. `year` when the plan uses the rating index, or any filter column in
  filtered BM25) can read overflow pages. A tuned SQLite schema would move
  `emb` (and the text) to separate tables or put the small columns first.
  The B-tree indexes are single-column; composite or covering indexes would
  speed up the conjunctions.
* **FTS5 has no dynamic pruning**: `ORDER BY rank LIMIT 10` scores every
  matching document. Tantivy uses Block-WAND for pure disjunctions but not
  under filters.
* **Vectors**: SQLite's exact scan is the realistic baseline without an
  extension, not a tuned one; its latency is dominated by reading each row's
  overflow chain into a buffer (≈2 µs per row warm). It doubles as the exact
  reference (`truth_*.json`); scores are f32 sums in a fixed order, so
  near-ties may order differently from another exact implementation.
* **Caches**: SQLite runs with its default 2000 KiB page cache, so warm runs
  rely on the OS page cache (syscall + copy per page); Tantivy and Strata read
  through `mmap`. Cold runs reopen the system first; the reopen's own reads
  are excluded from the query's pages and latency and reported as
  `open_pages`. On a small index the reopen's readahead can bring in a
  whole small file (Tantivy's SciFact fast fields: the filter query then
  reads 1 more page).
* **Pages read include kernel readahead.** SQLite's `pread` of a random page
  pulls a small readahead window; Tantivy's `mmap` faults read around the
  faulting page (up to 32 pages by default). Strata maps its file too, so its
  page counts behave like Tantivy's unless it advises `MADV_RANDOM`.
* **Tantivy** indexes with one thread (its default would use all cores);
  ids come from the fast column (cheaper than fetching stored documents);
  numeric ranges scan the fast column rather than postings because the fields
  are FAST (Tantivy's default choice).
* **Noise**: these numbers were measured while four other builds and
  benchmarks shared the 4-core machine; they are for validation. The final
  numbers are to be re-measured in isolation with the same binary.
