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

**Sequence numbers and terms.** A database can number its writes, which
replication orders them by (`Database::start_seq_no`, opt-in and not
undone). Every write transaction then gets the next sequence number, from
1, under the database's term, 1 at first and raised by
`Database::begin_term` when a replica becomes primary; a transaction that
writes nothing takes none (`WriteTxn::commit_seq` returns it). The commit
record carries both, in a WAL whose header (`STRWAL02`) names the features
its records use, so a reader that lacks one refuses the log rather than
misread it; a database that does not number its writes logs in
`STRWAL01`, byte for byte as before. Within a log the numbers rise and the
term never falls, or replay treats it as damage. The manifest keeps the
term and a floor for the count (`seq_no`): every commit of a version
records at least the last number given, so after the WAL is truncated
opening counts on from the manifest, and otherwise from the log's last
commit. A failed commit uses no number. Without `sync` a crash can lose
the last transactions of the log, and their numbers are given again; with
it a number, once returned, is never given again. Starting flushes, so
the log it empties is restarted numbered after the commit that records
the feature; a crash between the two leaves an empty `STRWAL01` log,
which opening restarts numbered, while one holding transactions of the
other kind fails the open. Checkpoints keep the numbers and the term.
Bulk loads and attached segments take no number. The manifest also keeps
every term with the first number it gave (`Database::numbering`), so that
a replica back after a failover can find where its log left the new
primary's.

**Row versions.** In a numbered database each row keeps the number of the
write that last changed it (`Database::seq_of`; `WriteTxn::seq_of` reads
it under the writer lock, for a write conditional on it). The memtable
keeps it per document; a segment built from numbered documents keeps them
in section `seq` (`storage::rowseq`: a width, a base and one number per
docID); a value set in place keeps its document's new number beside it in
the segment's updates (as field `SEQ`, which no schema has), which a merge
folds into the merged segment's section and carries for values set while
it ran. Rows written before numbering, bulk loaded or attached have number
0: an attached image's section is dropped, since another database's
numbers could equal later ones here. A deleted row keeps no number; there
are no tombstones, which shipping the log in order and catching up from a
checkpoint do not need.

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

### 3.7 Tables

A database holds one or more tables, each with its own schema, segments,
primary keys, memtable and merges. They share everything that makes a
commit: the WAL, the writer lock, the meta slots and the manifest, which
lists the tables with their segments. So one transaction can write several
tables and commits or rolls back as a whole, and a snapshot sees every table
at the same version (`Snapshot::table`). Segment ids are unique across
tables, so file names, delete sets and reader caches keyed by segment id
need no table.

* **Formats.** A manifest with only the table the database was created
  with is written in format 1, field for field as before there were tables;
  a database that never gets a second table is unchanged on disk, and older
  versions of Strata open it. With more tables the manifest is format 2
  (`{format, ..., next_table_id, tables: [{id, schema, segments}]}`) and the
  meta slot (or, in the directory layout, `CURRENT`) says so. Older versions
  refuse such a file: a single file with "unsupported format version 2", a
  directory with an error about its manifest. Dropping back to the first
  table writes format 1 again, into both meta slots (older versions check
  both), and older versions open the file again.
* **Format 3 and features.** Format 3 is format 2 with the features the
  database uses: `{format: 3, features: {required: [...]}, ...}`. A version
  is written in format 3 only while a table uses a feature (a `BOOL` or
  `TIMESTAMP` field is `"bool"` or `"timestamp"`, a `NOT NULL` field
  `"not_null"`, a default `"default"`, a TEXT field's analyzer other than
  `english` `"analyzer:<name>:<version>"`; a database that numbers its
  writes `"seq_no"`, for good), and falls back to 2 or 1,
  in both meta slots, when the last one goes. A reader refuses a manifest
  that requires a feature it does not know, naming the feature, and ignores
  an `optional` list and other keys it does not know. Format 3 is the last
  format number: versions from before it refuse it as newer, and every
  later change that an older reader must not ignore is a feature.
* **Fixtures of each format.** `tests/fixtures` keeps databases that
  earlier versions wrote, in both layouts, byte for byte: format 1 from
  before tables, format 2 with two tables and format 3 with every feature,
  each with deletes, values set in place and a transaction left in the
  WAL. The tests open copies with the current code, check their rows and
  searches, write, flush and merge them and check the format they stay in.
  The writes behind formats 2 and 3 are deterministic, and a test writes
  them again and compares the bytes (but for the database id), so a change
  to what Strata writes fails until its pull request keeps the old set
  under a new name and adds one written by the new code
  (`tests/fixtures/README.md`).
* **BOOL and TIMESTAMP** are logical types over an `INT` column (1 and 0;
  microseconds since the epoch in UTC), so every index, zone map, merge and
  shard message handles them as integers. Values are typed only at the
  edges: writes turn booleans and ISO 8601 text into the integers
  (`Field::store`), and a query's answer turns the integers of its
  columns back (`Select::types`, applied by the executor and, for shards,
  by `cluster::merge` after merging, so shards exchange integers).
* **NOT NULL and DEFAULT** are field options (`not_null`, `default`, a JSON
  value; `"now"` on a `TIMESTAMP`). The engine enforces `NOT NULL` on every
  write (`value::validate`, and `set`), so no front end can skip it.
  Defaults belong to the front ends: SQL fills the columns an `INSERT`
  leaves out and the `DEFAULT` keyword, and `ingest` the keys and columns a
  JSON row or a file leaves out, each from `Schema::defaults(now)` with one
  `now` per statement, request or file, while the Rust API writes a row as
  given. A coordinator binds an `INSERT` once, so its shards store the
  same time. The key needs no `NOT NULL` (it is implied, and not a
  feature) and takes no default.
* **WAL.** Records carry no table while they write the first one; a `TABLE`
  record switches the table for the records after it in the transaction. A
  one-table WAL is therefore byte for byte what it was.
* **Tables have ids** that name them in the WAL and never change. The first
  table has id 0 and cannot be dropped, so methods that name no table
  (`WriteTxn::insert`, `Database::bulk_load`) write it, as before.
  `CREATE` commits a manifest with a new empty table. `DROP` flushes the
  memtables first, so no logged write outlives its table and replays into
  a later one with the same id, then commits a manifest without the table;
  its pages are freed once no snapshot reads them, as after a merge.
* **Flushes and merges.** A flush writes every table's memtable in one
  commit. A merge works on one table's segments and commits only that
  table's list.
* **References.** A KEYWORD or INT field may record the table whose primary
  keys it holds (`"references"` in the schema, SQL `REFERENCES`): a table
  with a primary key of the field's type, or the field's own table. It
  names the table, not its id, because a file that drops back to format 1
  and is reopened can give a new table a dropped table's id. References are
  not enforced on writes; dropping a referenced table needs the tables that
  reference it dropped too, or `CASCADE`, which removes their references in
  the same commit. An older version ignores the key and drops it when it
  rewrites the schema, which loses nothing it could use: a format 1 file
  has one table, which can only reference itself.
* **SQL.** A statement names its table. `Db::query` plans a query over
  the catalog of one snapshot: a query of one table binds against it, and
  a compound one's leaves each bind against the table they read, so a
  join, a subquery or a `WITH` may read another table (joins across
  tables). Each leaf runs on its table's view of the same snapshot. Join
  keys are of one kind, `KEYWORD`, or `INT` and `FLOAT` compared as
  numbers, and planning refuses two kinds, since a `KEYWORD` never equals
  an `INT`; an `EDGES` field links rows of its own table by their keys, so
  a join along links stays in one table. A write binds against the catalog, takes the
  writer lock, checks that its table was not dropped and created again
  meanwhile, and reads the rows it changes at that version. `CREATE TABLE`
  binds to the schema's short JSON form, so both take the same options.
  strata-server binds statements against the first table only.
* **Later format changes.** Readers refuse a newer format number in a meta
  slot, `CURRENT` or a manifest, but ignore keys they do not know inside a
  schema. So a change that an older version must not ignore (stable field
  ids, a schema id per segment, a field's analyzer, new value types) is a
  required feature of format 3, recorded only by files that use it, as
  format 2 is written only by files with several tables.

## 4. Segments

A segment is `n` documents with local docIDs `0..n`, stored as a header and a
directory of named sections (`text.body.terms`, `text.body.post`,
`col.year`, `vec.emb.ivf`, ...), each with its CRC32, which a query checks
the first time it reads the section. Sections are 64-byte aligned, and those over
one page are page-aligned, so any `&[u64]` view of a section is aligned and
an index structure can be used straight from the mapped file. Every index
component has a builder (documents in docID order to section bytes) and a
reader (sections to a zero-copy view). Global docIDs are
`(segment ordinal, local docID)`; operators run per segment and merge.

## 5. Index families

### 5.1 Full text (inverted index)

* **Analyzer.** Unicode word split, lowercase, ASCII folding, an English
  stopword list, a light English stemmer (`english`, version 1); `simple`
  (version 1) stops after folding. A field's schema records its analyzer
  by name and version (absent: `english`), and its queries (`MATCH`
  words, phrases, prefixes and typos, `bm25` terms, synonym rules) use
  the same one. Any other than `english` is a format 3 feature named
  after it (`analyzer:simple:1`), so a version of Strata that lacks it, or
  analyses differently, refuses the database rather than misread its
  terms; a change to an analyzer's output is a new version.
* **Term dictionary.** Sorted terms in blocks of 32, front-coded, with a
  sparse in-memory index over block first-terms. Each entry gives the
  document frequency and the posting list's location.
* **Postings.** DocIDs, term frequencies and per-block maximum scores, so that
  top-k BM25 can skip blocks (Block-Max WAND, MaxScore). DocIDs are Loom lists
  for terms in at least 512 documents (the posting-list study that preceded
  this design), with frequencies bit-packed in 128-posting blocks addressed by
  posting rank; rarer terms use BP128 blocks. Against an all-BP128 layout,
  Loom is 2–4% smaller with 7.6–11× faster conjunctions. MaxScore is the
  default top-k algorithm: on the BEIR queries it beats Block-Max WAND by
  1.9–2.6×. A filter either drives the scoring (its members
  are scored directly) or is probed per candidate, by a fitted crossover.
* **Positions** (optional per field) in a separate stream, read only by phrase
  queries.
* **Norms.** Document lengths quantised to one byte, in the forward index.
* **Scoring.** BM25 with `k1 = 1.2, b = 0.75` and collection-wide statistics
  summed over the snapshot's segments, so a document scores the same in any
  segment.
* **Learned sparse fields.** A `SPARSE` field holds a weight per term, as
  SPLADE, BGE-M3's sparse head or OpenSearch's neural sparse models produce
  (a JSON object, `{"tok": 1.25, "##en": 0.4}`). It is stored as the same
  inverted index, with each weight quantised to steps of 1/1024 in the
  frequency slot (`text/sparse.rs`), and ranked by the dot product with the
  query's weights: the term scorer runs in a linear mode (`w · tf` instead of
  BM25's saturation), so MaxScore, Block-Max WAND, the block impacts and the
  filter strategies all apply unchanged. Its scores need no collection
  statistics, so shards and segments agree without the stats round.

### 5.2 Structured fields: forward columns and filter postings

* **Columns.** Integers and floats in 128-document blocks, frame-of-reference
  bit-packed, with min/max zone maps per block. Keywords are dictionary
  encoded (sorted dictionary per segment) with bit-packed codes. A column
  answers "value of document d" in O(1) and scans at memory bandwidth.
* **Keyword postings.** Every keyword value also gets a docID list, so
  equality and `IN` filters become set operations. Pyramid is the encoding
  (plain sorted arrays below 64 documents): the fastest unions and count-only
  intersections for facet counts. Per-candidate probes use the code column
  and a bitset of accepted codes (4–5 ns), which beats probing the Pyramid
  lists (5–33 ns).
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
  products with AVX-512 VNNI on x86-64 or NEON `usdot` on aarch64, at
  10–14 ns per 1,024-dimension vector on the M2.
* **Binary codes.** A field declared with binary codes keeps one bit per
  dimension (the sign around the segment's per-dimension mean) instead of
  SQ8: 1/32 of the float32 vectors, so a 100M-vector index keeps its codes
  in memory and leaves the float32 rows on disk, read only for the
  candidates. The query keeps 8-bit weights; AVX-512 sums the weights of
  the set bits with masked loads and `vpsadbw`. Searches rescore 8 × `rerank`
  candidates (`notes/vector.md`).
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
power-law graph in four segments, personalized PageRank takes 3.5 ms where
the CSR takes 0.66 ms; on arXiv in two segments, 0.72 ms against 0.30 ms. A
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
  On a `SPARSE` field, `similarity(field, :q)` is the dot product with a
  JSON object of query term weights (`algo => ..` as for `bm25`).
* `rrf(s1, s2, ...)`: reciprocal rank fusion, `Σ 1/(60 + rank_i)`, where each
  ranking is over the rows passing `WHERE` (`k => ..`, `depth => ..`).
  Arithmetic on scores gives weighted fusion: candidates are the union of
  each input's top 100, re-scored exactly by every input and min-max
  normalised (`norm => 'none'` keeps raw scores, `exact => 1` scores every
  row passing `WHERE`).
* `decay(field, origin, scale, offset => 0, decay => 0.5, shape => 'exp')`:
  a numeric column's closeness to `origin`, as Elasticsearch's decay
  functions: `d = max(0, |x − origin| − offset)` maps to 1 at `d = 0` and
  to `decay` at `d = scale`, falling exponentially, as a Gaussian
  (`'gauss'`) or linearly (`'linear'`, reaching 0 at `scale / (1 −
  decay)`). Rows without a value score 0. `now()` as the origin is the
  current Unix time in seconds. Usable alone, in sums and in `rrf`.
* Products of scores boost relevance by a row's own values:
  `ORDER BY bm25(body, :t) * decay(published, now(), 2592000)` (half the boost at 30 days) or
  `similarity(emb, :q) * rating`. Candidates are the top `depth` (100)
  rows of each relevance factor (BM25, similarity, graph scores; of every
  factor when there is none), scored exactly by every factor; shards
  return each factor's raw scores and the coordinator multiplies them over
  the same candidates, so a split collection ranks as the whole one.
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

### 6.2 The plan IR

Every statement becomes one typed, versioned, serialisable plan
(`strata/src/query/ir.rs`): a tree of operators over expressions, which
SQL makes today and other front-ends (a Python builder, a coordinator
handing a query to its shards) are to make instead of writing SQL text.

* **Operators.** `Search` is the leaf: the rows of one table that a filter
  holds for, ranked by an order, which is what the executor (§7) answers.
  Above it, `Aggregate` groups rows, `Filter` keeps groups, `Sort` orders
  them, `Limit` cuts the rows and `Project` names the columns. Writes are
  operators too: `Insert` with its rows and what a live key does
  (`fail`, `replace`, `skip` or `update`), and `Update` and `Delete` over
  a `Search`.
* **Expressions.** Fields by id (their position in the table's schema,
  which never changes, since there is no `ALTER`), typed literals
  (`{"int": 5}`, `{"float": 5.0}`, `{"text": "5"}`, `{"timestamp": ...}`),
  `NOT`, `AND`, `OR`, `IS NULL`, comparisons, `BETWEEN` with open ends,
  `IN`, arithmetic (`+ - * / %`, negation), `||`, `LIKE` and `ILIKE`,
  `CASE`, the functions `lower`, `upper`, `length`, `abs`, `round` and
  `coalesce`, `MATCH`, the graph predicates, every score (BM25,
  similarity, sparse, decay, RRF, weighted sums, products, PageRank), and
  a subquery. A filter is in three-valued logic as the binder makes it: a
  negation over a field that can be null carries `x IS NOT NULL`.
* **Versions and features.** A plan carries its version (1) and the
  features it uses. A reader refuses a newer version, or a feature it
  does not know, by name, as a database refuses a format 3 feature; a
  later operator or expression is a new feature, not a new version.
* **What runs.** `Plan::of_select` and `Plan::of_write` make the plan of a
  bound statement, and `Plan::to_select` and `Plan::to_write` give back
  the statement for the shapes the executor runs: `Project [Limit]
  Search` and `Project [Limit] [Sort] [Filter] Aggregate Search`. A test
  takes every kind of statement the binder accepts through JSON and back
  to the same bound statement. A compound query's plan has operators
  over these shapes (below).
* **Every query runs as a plan.** `Db::query`, `Db::sql` and the server
  bind a statement into its plan and run the plan, whose `Search` the
  executor answers (each leaf of a compound query too). `Db::run` runs a
  plan made elsewhere, `POST /v1/plan` takes one as JSON, and Python has
  `Database.plan` and `run`. A plan made elsewhere is checked against the
  table's schema as the binder checks SQL. Going through the plan adds
  0.4 to 1.3 µs to a query whose binding takes 9 to 22 µs (cloud
  container, 2026-10-29). A coordinator runs a read's plan, and sends
  its shards a compound query's leaves as plans (below).
* **One evaluator.** `ir::eval` gives an expression's value over a row,
  in SQL's three-valued logic: NULL is UNKNOWN, `AND` and `OR` are
  Kleene's, `NOT` of UNKNOWN is UNKNOWN, and a row or group is kept only
  when its condition is TRUE. A comparison with a multi-valued field
  holds when one of its values does and is never UNKNOWN (a field with no
  values is an empty list), as the indexes answer it. Integers are exact,
  a result too big for one is a FLOAT, `/` gives a FLOAT and a zero
  divisor NULL. `HAVING` runs in it, and so do the conditions and values
  over joined and derived rows (`WITH`, `FROM (SELECT ...)`, joins),
  compiled into plan expressions once per query instead of walking the
  SQL for each row. Tests check that the evaluator and the indexes keep
  the same rows for 1,500 random conditions over rows with NULLs and
  lists, that a join keeps them too, and that `NOT` over an aggregate of
  no values keeps no group (`HAVING` was two-valued before, and joins
  compared a list with a value by its text, so `b.tags = 'x'` never
  held, and read a field with no values as NULL).
* **Writes as plans.** An `Update` (and an insert's `update` on a live
  key) sets each field to an expression of the row, which the evaluator
  computes from the row as it was (every right-hand side sees the old
  values) and stores as the field's type: a whole FLOAT fits an INT, one
  value a multi-valued field. In an insert's `update`, `excluded.f` is the
  row the statement proposed. A literal, `f + number` (a counter, where
  NULL counts as zero) and a bare `excluded.f` of the same type keep their
  faster forms; every other expression gives NULL for NULL, as in SQL, and
  may not search, score or traverse. A statement that fails on one row
  writes none. `UPDATE` and `DELETE` bind the statement's own `WHERE`,
  `ORDER BY` and `LIMIT`, and `ON CONFLICT` reads its live rows with a
  query it builds, where each used to print SQL text and parse it again:
  printed, `- -1` is the comment `--1`, and a column named `order` stopped
  `SELECT id, order, ...` from parsing.
* **Compound steps bind their own syntax trees.** Each step of a compound
  query (a side of a join, a subquery, a `WITH`) is built as a syntax
  tree and bound, where it used to be printed as SQL and parsed again,
  with the same two failures. A coordinator still sends its shards a
  step as SQL, printed so that it parses back to the step's plan (a
  column named like a keyword quoted, the operand of a sign that starts
  with a sign in parentheses), and a test checks that it does.
* **Operators.** A compound query (`WITH`, a subquery, a join,
  `SELECT DISTINCT` over rows in memory) plans into one tree, which
  `ops::run` runs: its leaves are single-table queries the executor
  answers, and above them `Join` (a hash join, inner or left, on key
  columns or along an `EDGES` field's links, with any other `ON`
  condition after), `With` and `Ref` (named results), `Distinct`,
  `KeyOrder` (rows in a `WITH` result's order) and `Project`, `Filter`,
  `Sort`, `Limit` and `Aggregate` over rows in memory, all through the one
  evaluator. A subquery is an expression of the condition it sits in
  (`Values`, `Keys`, `Exists`, `Scalar`) and runs before its leaf, whose
  filter then holds its values, so the leaf's indexes serve it. A join's
  second side reads only the rows whose join values the first side holds;
  under a `LIMIT` whose order is the first side's, scans of a growing
  number of its rows stop once enough rows join. Each part holds at most
  1,000,000 rows and counts against the query's memory budget. A plan
  with operators needs the feature `operators`, so an older reader
  refuses it by name. Rows sorted in memory put NULLs last unless
  `NULLS FIRST`, as the executor does; they came first in an ascending
  sort. A `SELECT DISTINCT` over rows of a `WITH` result kept every
  distinct value and dropped its `LIMIT`; it keeps the `LIMIT` now. A
  coordinator runs the operators and sends each leaf to its shards as its
  plan, where it sent SQL text printed to parse back to the leaf.
* **The shard protocol.** From protocol 2 a coordinator sends every read
  to its shards as a typed plan in protobuf (`QueryRequest.plan`, the same
  tree as the JSON form, with typed values and no SQL), where protocol 1
  sent SQL with parameters, or a leaf as a JSON plan. `Hello` answers a
  server's protocol versions, plan version and plan features. The
  coordinator asks each replica when it connects, every minute and after
  the replica fails, and speaks the oldest version any replica speaks, so
  a cluster upgrades one server at a time. A server refuses a protocol
  newer than its own, and reads a protobuf plan with the checks JSON's
  reading makes: version and features by name, every node and expression
  present, every number finite.
* **Query then fetch.** From protocol 3, a page of rows with stored
  columns whose shards would send at least 8 times its rows in one round
  (`FETCH_FROM`: a deep page, many shards, or a fused score's candidates)
  comes in two rounds. Each shard answers the query with its rows' keys
  alone (`cluster::ids`) and holds the snapshot it read (`hold`); the
  merge ranks the keys; the shards holding the page's rows answer
  `cluster::fetch`, the query's columns of those rows, from that snapshot,
  ending its lease (`release`; `Release` ends the others'); and
  `cluster::fill` puts them in the merged order with the merged scores. A
  statistics round's lease is ended by its query round too, where it
  lasted until its deadline and a second. Partial aggregates were in
  place already: shards send sums, counts, minimums, maximums and
  `COUNT(DISTINCT)`'s exact values, which the coordinator merges.
* **Slots.** From protocol 4, a key's shard is its slot's: one of 4,096,
  xxh3 of the key's type tag and bytes (an `INT` and a string of the same
  digits differ), where it was FNV-1a of the key's text modulo the shard
  count, which moved most keys when a shard was added. A versioned map
  assigns slots to shards (equal runs by default, or `--slot-map`). The
  coordinator gives each shard the map (`Slots`), which the shard keeps
  beside its database, and sends the map's epoch with every request; a
  shard refuses an older epoch, another map of its epoch, a write of a key
  another shard holds, and a map under which rows here are in other
  shards' slots. Online resharding is a map of a higher epoch once moved
  slots' rows are copied; the copy is not built. While a replica speaks
  an older protocol the coordinator routes by the old hash.
* **Sequence numbers.** Replication's order of writes is a database's
  own (§3.4): a numbered database gives each write transaction a term and
  a sequence number, logged in its commit record and kept across flushes,
  crashes, checkpoints and reopening, and each row the number of the
  write that last changed it. Nothing ships the log yet.
* **Conditional writes.** `seq_no()` is a column of a query on one table
  (plan feature `seq_no`, so an older shard refuses the plan by name), read
  from the row's segment, its updates or the memtable. strata-server's
  `--seq-no` numbers its database, a write's answer carries its number,
  and a write operation with `if_seq_no` applies only while its row's
  number is that, checked under the writer lock (`WriteTxn::seq_of`): a
  mismatch, a missing row or a row the request already wrote refuses the
  request with 409. Several requests share a group commit, so a conflict
  fails the group and each request is then applied alone, which leaves
  exactly one of the racing writers applied. A coordinator passes both on
  only while each shard has one replica, since replicas number their writes
  independently until the log is shipped.
* **`EXPLAIN`.** `EXPLAIN <statement>` gives the plan as indented lines,
  fields by name and expressions in SQL's words, without running it;
  `EXPLAIN FORMAT JSON` gives the JSON. `POST /v1/explain` returns both
  next to the executor's plan lines.

Next: the rest of replication on top of the shard protocol (WAL shipping,
catch-up, read-your-writes, promotion), and a coordinator and strata-server
over every table.

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
