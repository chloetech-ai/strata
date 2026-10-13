# Strata: an embedded, single-file search database

Strata is to search what SQLite is to relational data: a library that keeps a
whole database in one file, needs no server or configuration, gives ACID
transactions, and is queried with SQL. Its tables are indexed three ways at
once (posting lists, a columnar forward index and vector ANN indexes), and one
query can combine all three. A cost-based planner picks how.

This document is the design. `report/` holds the evaluation of the prototype.

## 1. Goals

* **Embedded and single-file.** One `.strata` file plus a `-wal` sidecar, like
  SQLite in WAL mode. Opening a database maps the file; nothing is loaded
  eagerly, so databases larger than RAM work and a cold open is O(1).
* **ACID with snapshot isolation.** One writer, any number of readers; readers
  never block and see a consistent snapshot. Crash at any point leaves the
  last committed state.
* **SQL with search extensions.** `SELECT ... WHERE ... ORDER BY ... LIMIT`,
  `GROUP BY` for facets, plus `MATCH` (boolean full text), `bm25()`,
  `similarity()` / `distance()` (vectors) and `rrf()` (rank fusion).
* **Three index families, freely combined.** Full text (BM25 ranking, boolean
  and phrase queries), structured fields (equality, `IN`, ranges, facets) and
  vectors (approximate and exact nearest neighbours), in any combination:
  filtered ANN, filtered BM25, hybrid text + vector ranking, faceting over any
  result.
* **Disk-aware.** Every structure is read in place through `mmap` with no
  deserialisation. Layout keeps the I/O of a query proportional to the work it
  does, and sequential where possible.

* **Graphs over the same rows.** A row can hold typed, weighted edges to
  other rows. Queries traverse them (reachability, shortest paths,
  personalized PageRank) and combine the result with text, structured and
  vector search in one statement (§5.4, §6.1).

Non-goals for the prototype: joins, multi-process writers (the file lock and
`-shm` reader table of SQLite are designed but not built), distribution.

## 2. Architecture

```
 SQL text ──► parser (sqlparser) ──► binder (schema, MATCH grammar) ──► logical plan
                                                                          │
                     statistics (df, histograms, list lengths, ANN params) ▼
                                                     cost-based optimizer ──► physical plan
                                                                          │
   per segment:  postings iterators │ column scans / probes │ ANN searchers
                 (Loom, Pyramid)    │ (FOR blocks, zone maps)│ (IVF, HNSW, flat)
                                     ▼
                    segment = named, aligned sections (zero-copy views)
                                     ▼
          single file: meta slots A/B │ immutable extents │ freelist      + WAL sidecar
```

## 3. Storage engine

### 3.1 Why immutable segments, not a B-tree

SQLite stores everything in B-trees that are updated in place. Compressed
posting lists, bit-packed columns and ANN graphs do not survive in-place
updates: inserting one docID into a Loom partition or a BP128 block means
re-encoding it, and the best encodings (partitioned Elias-Fano, Entropic) are
chosen globally per list. SQLite's own FTS5 already stores its index as
immutable segments merged in the background; Lucene, Tantivy and PISA do the
same. Strata adopts that log-structured design for the whole table: every
index structure lives in immutable segments, writes go to a WAL and an
in-memory memtable, and segments are merged in the background.

### 3.2 File layout

```
page 0      meta slot A   ┐ magic, format version, txn id, manifest extent,
page 1      meta slot B   ┘ freelist extent, page count, crc32c
page 2..    extents: segment data, manifests, delete sets, freelists
```

Pages are 4 KiB. An extent is a run of pages. Segments are written once,
never modified, and freed only when no snapshot references them.

The file is read through one `mmap`. Sections read at random positions are
advised `MADV_RANDOM`, so a cold page fault there reads one page rather than
the kernel's default read-around of up to 32: the HNSW graph and, in
segments that have one, the SQ8 codes its traversal scores; float32 vectors
used for re-ranking; dictionaries; the doc store; the key index. Sections
read a run at a time keep read-ahead: postings, document lengths, columns,
IVF clusters. Readers open lazily per field, so a query touches only the
headers of the fields it uses.

### 3.3 Commit protocol (shadow paging)

1. Write the new extents (segment data, new delete sets, the new manifest and
   freelist) into free pages or at the end of the file. `fsync`.
2. Write the meta slot `(txn + 1) mod 2` with the new manifest location and a
   checksum. `fsync`.

Recovery reads both slots and takes the valid one with the larger txn id. A
torn meta write fails its checksum, and the other slot still names the
previous, fully written state. Nothing reachable from a valid meta slot is
ever overwritten (the LMDB discipline), so there is no rollback journal.

### 3.4 WAL and memtable

Small transactions would otherwise write a segment per commit. Instead,
`INSERT`/`DELETE` append checksummed records to the `-wal` sidecar and
`fsync` at commit (group commit). Uncommitted records are ignored on
recovery. The memtable holds the WAL's documents; a snapshot that needs them
builds small in-memory segments from them, one per chunk of 1,024 documents
(cached; a write rebuilds only the last chunk), so queries see committed
writes immediately and the memtable needs no index code of its own. When the memtable exceeds a threshold it is flushed into a
segment by the shadow-paging commit and the WAL is truncated. Bulk loads
bypass the WAL and write segments directly.

### 3.5 Deletes, updates, snapshots

Documents have a primary key. Each segment has a primary-key index (sorted
64-bit key hashes with docIDs) and a delete set, stored as its own small
extent and replaced copy-on-write when it changes. An update is a delete plus
an insert. A snapshot is an immutable manifest version (list of segments,
their delete sets, collection statistics) held by reference count; readers
pin one, writers publish the next.

### 3.6 Merges

A tiered policy merges segments of similar size. A merge reads the live
documents of its inputs back (stored fields from the doc store, vectors and
unstored structured fields from their indexes, generated columns recomputed)
and rebuilds every index for the merged segment, which also drops deleted
documents and re-optimises the encodings for the larger lists. `vacuum`
moves data down and shrinks the file. Large, rarely updated segments may use the
space-optimised codecs (Entropic) instead of the speed-optimised ones.

## 4. Segments

A segment is `n` documents with local docIDs `0..n`, stored as a header and a
directory of named sections (`text.body.terms`, `text.body.post`,
`col.year`, `vec.emb.ivf`, ...). Sections are 64-byte aligned, and those over
one page are page-aligned, so any `&[u64]` view of a section is aligned and
an index structure can be used straight from the mapped file. Every index
component has a builder (documents in docID order to section bytes) and a
reader (sections to a zero-copy view). Global docIDs are
`(segment ordinal, local docID)`; operators run per segment and merge.

## 5. Index families

### 5.1 Full text (inverted index)

* **Analyzer.** Unicode word split, lowercase, ASCII folding, an English
  stopword list, a light English stemmer. Queries use the same analyzer.
* **Term dictionary.** Sorted terms in blocks of 32, front-coded, with a
  sparse in-memory index over block first-terms. Each entry gives the
  document frequency and the posting list's location.
* **Postings.** DocIDs, term frequencies and per-block maximum scores, so that
  top-k BM25 can skip blocks (Block-Max WAND, MaxScore). DocIDs are Loom lists
  for terms in at least 512 documents (the posting-list study that preceded
  this design), with frequencies bit-packed in 128-posting blocks addressed by
  posting rank; rarer terms use BP128 blocks. Against an all-BP128 layout,
  Loom is 2–4% smaller with 7.6–12× faster conjunctions. MaxScore is the
  default top-k algorithm: on the BEIR queries it beats Block-Max WAND by
  1.9–2.6×. A filter either drives the scoring (its members
  are scored directly) or is probed per candidate, by a fitted crossover.
* **Positions** (optional per field) in a separate stream, read only by phrase
  queries.
* **Norms.** Document lengths quantised to one byte, in the forward index.
* **Scoring.** BM25 with `k1 = 1.2, b = 0.75` and collection-wide statistics
  summed over the snapshot's segments, so a document scores the same in any
  segment.

### 5.2 Structured fields: forward columns and filter postings

* **Columns.** Integers and floats in 128-document blocks, frame-of-reference
  bit-packed, with min/max zone maps per block. Keywords are dictionary
  encoded (sorted dictionary per segment) with bit-packed codes. A column
  answers "value of document d" in O(1) and scans at memory bandwidth.
* **Keyword postings.** Every keyword value also gets a docID list, so
  equality and `IN` filters become set operations. Pyramid is the encoding
  (plain sorted arrays below 64 documents): the fastest unions and count-only
  intersections for facet counts. Per-candidate probes use the code column
  and a bitset of accepted codes (6–9 ns), which beats probing the Pyramid
  lists (9–24 ns).
* **Range postings.** Numeric fields get an equi-depth histogram (64
  buckets) whose buckets are docID lists. A range is the union of the buckets
  it covers, plus the boundary buckets filtered through the column. The
  histogram doubles as the selectivity estimator.
* **Doc store.** Documents as rows in LZ4-compressed 16 KiB blocks with a
  block index; it serves projections and is the source for merges.

### 5.3 Vectors

* **Forward vectors.** Every vector field stores float32 vectors (for exact
  re-ranking) and 8-bit scalar-quantised codes (SQ8, per-dimension min/max)
  for scanning: 4× less I/O, and with the query quantised too, integer dot
  products with AVX-512 VNNI at 10–15 ns per 1,024-dimension vector.
* **Flat.** Exact scan of SQ8 codes plus float32 re-ranking. The right answer
  for small segments and very selective filters.
* **IVF.** k-means centroids; each cluster's members are stored contiguously
  as a docID list (a posting list) followed by their codes. Probing a cluster
  is one sequential read, which suits disk. A filter is applied by
  intersecting a cluster's docID list with the filter's list, the same
  operation as a text conjunction.
* **HNSW.** A navigable small-world graph with fixed-degree adjacency lists
  in the segment. Fastest in memory; random I/O on disk.
* **Filtered search.** Three strategies the planner chooses between:
  *pre-filter* (evaluate the filter, then scan exactly), *in-filter* (search
  the index and skip non-matching documents during traversal or probing) and
  *post-filter* (search with a larger k and drop non-matching results).

### 5.4 Graphs

Graph databases keep nodes and edges in their own stores. Strata keeps its
one table: a row is a node, and an `EDGES` field holds its outgoing edges as
a JSON array of targets, each a primary key of the same table, optionally
with a label and a positive weight:

```text
["p2", ["cites", "p7"], ["extends", "p9", 0.5], {"to": "p11", "label": "refutes"}]
```

Edges are part of their source row, so they are written, updated, deleted,
logged, merged and snapshotted with it: an update to a row's edges is an
update to the row, and nothing else changes. Because targets are keys and
not docIDs, an edge never needs rewriting when its target moves to another
segment in a merge, is updated (a new version in a newer segment) or is
deleted (the edge then dangles and traversals skip it, as a foreign key
without a constraint).

* **Out-edges.** Per segment, a CSR over local docIDs: offsets `[u64; n+1]`,
  then per edge its source docID, its target key (`i64`, or offsets and
  bytes for KEYWORD keys), a label code (`u16`, when any edge has a label)
  and a weight (`f32`, when any differs from 1).
* **Reverse index.** Edges grouped by target key, the groups ordered by the
  key's 64-bit hash under the radix directory of the primary-key index, with
  each edge's source docID stored in group order: "which rows point at key
  k" is one directory read, one stored-key comparison per group (colliding
  keys form separate groups; integer hashes are a bijection and need none),
  and a sequential read of the sources.
* **Same-segment hints.** Per edge, the docID its target had in this
  segment at build time, if any. A key has at most one live row in a
  snapshot, so a hinted row that is still live is the target.
* **Resolution.** Otherwise a hop from row `u` to its edges' targets looks
  each target key up in the snapshot's primary-key indexes (the lookup of
  `WHERE id = ...`) and keeps the live row, remembering the answer for the
  rest of the query. A hop against the edges looks `u`'s own key up in every
  segment's reverse index and keeps the live sources. Sections are read in
  place through `mmap`, advised `MADV_RANDOM`.

The graph costs about 27 bytes per edge with integer keys (1.15M arXiv
citations: 31 MB, next to 453 MB of vectors for the same rows). The memtable
needs no graph code: its in-memory segments are built by the same builder.

Keys instead of docIDs are what let edges survive updates, deletes and
merges untouched, and they are also the cost: a cross-segment hop is a hash
probe per segment where an in-memory CSR reads an array. On a million-row
power-law graph in four segments, personalized PageRank takes 5.8 ms where
the CSR takes 1.3 ms; on arXiv in two segments, 1.4 ms against 0.5 ms. A
merge into fewer segments narrows the gap; a snapshot-wide resolved CSR,
rebuilt when segments change, would close it at the price of memory and
rebuild time.

**Vector graph nodes.** Rows carry vectors as well as edges, so a traversal
can start from a vector search (seeds from a subquery, §6.1) and its result
can be ranked by similarity, fused with BM25, or filtered. A VECTOR field
can also be traversed as if it were an edge field: a row's neighbours are
its `k` most similar rows (one ANN search per expanded row). This
similarity graph is implicit, needs no storage and is always current, but it
carries no information the vector search does not already have; §8.9
measures both.

## 6. Query language

Standard SQL `SELECT` over one table, with search functions:

```sql
-- boolean full text + structured filters, ranked by BM25
SELECT id, title, score() FROM papers
WHERE MATCH(body, 'vaccine AND (mrna OR "messenger rna") NOT animal')
  AND year BETWEEN 2019 AND 2021 AND category IN ('virology', 'immunology')
ORDER BY bm25(body, 'mrna vaccine efficacy') DESC LIMIT 10;

-- filtered vector search
SELECT id FROM papers WHERE rating >= 4
ORDER BY similarity(emb, :q) DESC LIMIT 10;

-- hybrid: reciprocal rank fusion, or a weighted sum of scores
SELECT id FROM papers WHERE year >= 2015
ORDER BY rrf(bm25(body, :text), similarity(emb, :q)) DESC LIMIT 10;

-- facets over any result
SELECT category, COUNT(*) FROM papers WHERE MATCH(body, 'vaccine')
GROUP BY category ORDER BY 2 DESC LIMIT 5;
```

* `MATCH(field, q)`: boolean predicate. `q` has terms, `"phrases"`, `AND`,
  `OR`, `NOT`, `-term`, parentheses and `prefix*`; juxtaposition means AND,
  as in FTS5.
* `bm25(field, text)`: BM25 of the analysed terms of `text` (disjunctive: a
  document scores if it contains any term). Ranked queries drop English
  function words (NLTK's 153-word list) and count each term once.
  `k1 => .., b => .., algo => ..` override the defaults; `stopwords => 0`
  keeps the function words (better for short questions: +0.055 nDCG@10 on
  Quora, -0.055 on TREC-COVID).
* `similarity(field, vector)` (dot product, cosine for unit vectors) and
  `distance(field, vector)`. Vectors are bound parameters or JSON arrays.
* `rrf(s1, s2, ...)`: reciprocal rank fusion, `Σ 1/(60 + rank_i)`, where each
  ranking is over the rows passing `WHERE` (`k => ..`, `depth => ..`).
  Arithmetic on scores gives weighted fusion: candidates are the union of
  each input's top 100, re-scored exactly by every input and min-max
  normalised (`norm => 'none'` keeps raw scores, `exact => 1` scores every
  row passing `WHERE`).
* Options as named arguments, e.g. `similarity(emb, :q, nprobe => 32)`,
  `bm25(body, :t, k1 => 0.9, stopwords => 0)`.

### 6.1 Graph queries

```sql
-- papers within two citations of p1, nearest first
SELECT id, title, hops() FROM papers
WHERE REACHABLE(cites, 'p1', hops => 2, dir => 'both') ORDER BY hops() LIMIT 20;

-- one shortest path, through surveys only
SELECT id, hops() FROM papers
WHERE PATH(cites, 'p1', 'p9', max_hops => 6, through => kind = 'survey') ORDER BY hops();

-- vector search as the entry point, the citation neighbourhood ranked by similarity
SELECT id FROM papers
WHERE REACHABLE(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT 5))
ORDER BY similarity(emb, :q) DESC LIMIT 20;

-- personalized PageRank from the same seeds, fused with BM25
SELECT id FROM papers
ORDER BY rrf(bm25(body, :t), ppr(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT 5))) DESC LIMIT 20;
```

* `REACHABLE(field, seeds, hops => 1, min_hops => 1, dir => 'out', label =>
  'a,b', through => <predicate>, max_nodes => 100000)`: rows within
  `min_hops..hops` of a seed. `dir` is `out` (along edges), `in` (against
  them) or `both`. `through` restricts the rows a traversal may pass
  through (seeds and end points are exempt); end points are filtered by the
  rest of `WHERE`. `hops => 0, min_hops => 0` is the seed set itself.
* `PATH(field, from, to, max_hops => 6, ...)`: the rows of one shortest
  path (unweighted), by bidirectional breadth-first search.
* `hops()`: a row's distance from the seeds (or position on the path), in
  `SELECT` and `ORDER BY` (nearest first; `DESC` for farthest first).
* `ppr(field, seeds, alpha => 0.15, eps => 1e-4, dir => 'both', label =>
  .., weighted => 0)`: personalized PageRank, usable wherever a score is,
  including `rrf` and weighted sums. `weighted => 1` uses the seed rows'
  scores from a subquery as restart weights.
* Seeds are a key, a parameter, a tuple of keys `('a', 'b')` or a subquery
  over the same table, whose ranking and filters are those of any query.
* With a VECTOR field as the first argument, `REACHABLE` and `PATH` follow
  similarity edges: `k => 10` neighbours per row, `min_sim => ..`.

## 7. Planning and execution

The binder turns the `WHERE` clause into a predicate tree over three kinds of
leaves: text (`MATCH`), structured (comparisons, `IN`) and the always-true
leaf. `ORDER BY` becomes a scorer: BM25, vector, fusion, a column, or none.

**Statistics.** Exact list lengths (document frequencies, keyword list sizes),
histogram bucket counts for ranges, per-segment document counts, and ANN
parameters. Conjunctions assume independence; disjunctions use
inclusion-exclusion.

**Filter evaluation.** A predicate is either *materialised* (a docID set from
postings) or *probed* (a per-candidate check against the forward column).
Materialising costs work proportional to the lists involved; probing costs
work proportional to the candidates. The planner materialises when the
candidate stream is long relative to the lists, and probes otherwise.

**Ranked retrieval.**

| ORDER BY | strategies |
|---|---|
| none / column | iterate filter; top-k heap or column sort |
| `bm25` | MaxScore or Block-Max WAND over postings, with the filter as a skip constraint |
| `similarity` | pre-filter exact scan, IVF with in-filter intersection, HNSW in-filter, post-filter with oversampling |
| fusion | top-k′ from each retriever, union, exact re-scoring of both scores on the union; or exact scan |

The vector strategies are chosen by estimated cost against an estimated
filter selectivity `s`: pre-filter costs `s·n` distance computations; IVF
needs about `nprobe/√s` probes' worth of clusters to collect enough matching
candidates; HNSW in-filter visits about `1/s` times more nodes; post-filter
needs `k/s` candidates. What matters for the index strategies is the
selectivity *near the query*, which differs from the global one when the
filter correlates with the query's topic. When the global estimate picks an
index strategy, the planner checks it with a 32-result unfiltered search:
the fraction of those results that pass the filter (smoothed) replaces `s`
for in-index search, and bounds it from above for post-filtering.

**Graphs.** Graph predicates and scores are computed before planning,
across segments, because a traversal does not respect segment boundaries:
seeds are resolved (a seed subquery runs as its own query on the same
snapshot), the traversal runs, and its result becomes per segment a
materialised document set with exact cardinality, plus hop distances and
PageRank scores by row. The planner then treats `REACHABLE` like any other
leaf, only with an exact estimate: a vector search over a reachable set of a
few hundred rows is a pre-filtered exact scan, and a text filter on a large
neighbourhood is probed per row. The traversals:

* breadth-first search, frontier by frontier, stopping at `max_nodes`;
* shortest paths by bidirectional search, expanding the smaller frontier;
* personalized PageRank by forward push (Andersen, Chung and Lang 2006).
  Each row has an estimate \(p\) and a residual \(r\), initially the restart
  distribution over the seeds. Pushing \(u\) moves \(\alpha\,r(u)\) into
  \(p(u)\) and spreads \((1-\alpha)\,r(u)\) over its neighbours in proportion
  to edge weight; rows are pushed while \(r(u) \ge \varepsilon\,\deg(u)\), so
  each estimate is within \(\varepsilon\,\deg(u)\) of the exact value and
  the work is independent of the graph's size (at most
  \(1/(\alpha\varepsilon)\) pushes). A row is queued only once its residual
  reaches \(\varepsilon\), the least any threshold can be, so rows that
  merely received a little mass never have their neighbours resolved.
  Neighbour lists are resolved once per query and reused.

**Facets.** `GROUP BY` a keyword over a result set either reads the
dictionary codes of the result's documents (small results) or intersects the
result set with each value's Pyramid list using count-only intersection
(large results, few values).

**Segments.** Each segment runs the same physical plan with its own
statistics; per-segment top-k lists are merged, and collection-wide BM25
statistics keep scores comparable.

**Generated columns.** A TEXT field may be declared as generated from other
stored text fields (`body` = title + text in the evaluation table, like
SQLite's `GENERATED ALWAYS AS`). It is indexed but not stored; merges
recompute it.

## 8. Evaluation plan

Data: four BEIR collections (SciFact 5k, FiQA 58k, TREC-COVID 171k, Quora
523k documents) with Cohere embed-english-v3 vectors (1024 dimensions) for
documents and queries, relevance judgments, and synthetic structured fields
(`year`, `category`, `rating`) plus a content-correlated `topic`.

1. **Build and size**: load time and file size by component, against SQLite
   (FTS5 + B-tree indexes + vector blobs) and Tantivy.
2. **Full text**: BM25 top-10 and boolean query latency, warm and cold cache;
   nDCG@10 as a check that ranking is right.
3. **Structured filters**: range and keyword filters, column scan with zone
   maps vs range postings vs SQLite B-tree indexes.
4. **Vectors**: flat, IVF, HNSW: recall@10 against exact search, latency,
   pages read, build time, size.
5. **Filtered vectors**: every strategy across filter selectivities from 0.1%
   to 100%, and the planner's choice.
6. **Hybrid**: nDCG@10 of BM25, vectors, RRF and weighted fusion; latency and
   recall of the hybrid execution strategies against exact hybrid ranking.
7. **Planner**: parse and plan overhead, and regret (chosen plan against the
   best plan in hindsight) over a randomised query workload.
8. **Transactions**: insert throughput with commits, crash recovery, reads
   during writes.
9. **Graphs**: ogbn-arxiv (169,343 papers with titles, abstracts and 1.17M
   citations; vectors from a static embedder) and a power-law graph of a
   million rows: graph index size and build time; k-hop, path and PageRank
   latency against an in-memory CSR and SQLite recursive CTEs; and whether
   the graph helps search: finding a held-out paper's references from its
   text with BM25, vectors and hybrid search, alone and with graph
   expansion or PageRank from vector seeds (`strata-graph-bench`).
