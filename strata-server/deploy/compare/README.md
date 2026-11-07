# Strata and thirteen other engines on the same cluster

Runs Strata, Elasticsearch, OpenSearch, Solr and Vespa one at a time on the
kind cluster of [`../kind-test.sh`](../kind-test.sh), on the same data, with
the same number of shards and the same CPU and memory, and asks each the
query kinds of `strata-server-bench` and five more (phrase, typo, prefix,
similar items, an SQL aggregate). Nine more engines get the same data, the
same budget on one node, and the same kinds where they have them:
Meilisearch, Typesense, Manticore Search, Qdrant, Weaviate, Milvus,
ParadeDB, Postgres and Redis ([below](#nine-more-engines-on-one-node)). Results of a run on a Mac (Strata,
Elasticsearch and Solr, 28 October 2026) are in
[`../../results/compare-mac/README.md`](../../results/compare-mac/README.md).

| file | what |
|---|---|
| `compare.sh` | builds and loads the image, starts and stops each engine, loads it, runs the client |
| `es.yaml`, `os.yaml`, `solr.yaml` | two data nodes of Elasticsearch 9.5.5, OpenSearch 3.9.0 or Solr 10.1.0 (SolrCloud, ZooKeeper embedded in `solr-0`) |
| `vespa.yaml`, `vespa/` | Vespa 8.778.45: `vespa-0` (config server, cluster controller and the container that answers queries) and two content nodes; `vespa/package.sh` writes the application package (`services-cluster.xml`, `schemas/doc.sd`, the stopword lists); `vespa/services.xml` is the same application on one node, for trying it in Docker |
| `strata-coordinating.yaml` | two Strata shards, each with a coordinator in its process that calls the shard in-process (`--coordinator-listen`), or as a process of its own in the pod (`STRATA_COORD=shards`, as from 6 November 2026 until then) |
| `strata.yaml`, `strata-coordinator.yaml` | two Strata shards and a coordinator in a pod of its own (`STRATA_COORD=pod`, as before 6 November 2026) |
| `redpanda.yaml` | one Redpanda broker for the stream test |
| `more/*.yaml` | the nine one-node engines (`postgres.yaml` is ParadeDB's and Postgres's, as `paradedb-0` or `postgres-0`) |
| `analyze.py` | scores the answers and prints the tables |
| `profiles/*.toml` | traffic profiles for the [traffic simulator](#traffic-simulator) (copied into the image) |
| `../../src/bin/engine-compare/` | the client: `build` (a Strata shard), `load` (the other engines), `run` (time the kinds and record the answers), `plan` and `replay` (the traffic simulator, `sim/` and `replay.rs`); `more.rs` speaks to the nine one-node engines |

## What is the same for every engine

- **Data**: BEIR Quora as `strata/scripts/prepare_beir.py` prepares it:
  522,931 questions with Cohere embed-english-v3 vectors (1,024
  dimensions, unit length) and the synthetic `year`, `category`, `rating`
  and `topic` fields of `strata::data`. `COLLECTION=` picks another, such
  as `nq-1000k` from `strata/scripts/prepare_beir_local.py nq --docs
  1000000` (every judged document of BEIR NQ and the corpus's first ones,
  1,000,000 in all, with bge-small-en-v1.5 vectors of 384 dimensions).
- **Shards**: 2, no replicas, on the same two worker nodes (one each);
  the clients run on the third worker. Every index lives on its node's own
  disk.
- **CPU and memory**: 3 CPUs and 6 GiB per engine, as Kubernetes limits:
  Elasticsearch, OpenSearch and Solr have two data nodes of 1.5 CPUs and 3
  GiB (heap 1.5 GB for Elasticsearch and OpenSearch, as Elastic recommends;
  1 GB for Solr, whose full-precision vectors need the page cache); and
  as their data nodes also coordinate, Strata has two pods of a data
  node's CPU and memory (`DATA_CPU`, `DATA_MEM`), each with a shard and a
  coordinator over both shards, and the client sends its requests to both
  coordinators. Runs before 6 November 2026 gave Strata two shards of 1
  CPU and 2.5 GiB and a coordinator of 1 CPU and 1 GiB in a pod of its own
  on the clients' node, where it sat mostly idle while the shards ran
  short of CPU; `STRATA_COORD=pod` runs that split again (`SHARD_CPU` and
  `COORD_CPU` change it; those results compare 1.25 + 1.25 + 0.5 too). Vespa is
  split that way, since its content nodes do not answer queries:
  `vespa-0` on the clients' node (config server, cluster controller and the
  container, 1 CPU and 2.25 GiB, container heap 768 MB) and two content
  nodes of 1 CPU and 1.875 GiB. Vespa asks for 4 GB per node and refuses to
  start below it, so its pods set `VESPA_IGNORE_NOT_ENOUGH_MEMORY`.
  Loading has the same budget: Elasticsearch, OpenSearch and Solr take bulk
  requests on their two data nodes and Vespa one document per request on
  its container, 64 in flight from each of the 4 loading clients (256, what
  its container queues), a refused put sent again after a pause that grows
  to 5 s (from a loader pod on the third node, outside the budget; with
  every put of a 500-document batch at once, the first 1M NQ load failed
  at 850,000 with puts refused for 20 s on end),
  and Strata builds each shard in a job on the shard's node, reading the
  documents from the files as it builds a segment of each 500K of them
  (`build --segment-docs`; holding a shard's documents and its one segment
  at once took about 15 KB a document, 7.8 GB at 500K).
- **Text analysis**: Strata's, rebuilt in the others: words are runs of
  letters and digits, lowercased and folded to ASCII, minus Lucene's classic
  English stopwords, stemmed with Snowball English (Porter 2); ranked
  queries also drop Strata's query stopwords. BM25 with k1 1.2, b 0.75
  everywhere. Vespa gets the same chain from its Lucene linguistics, with
  one profile for documents and one for queries.
- **Vectors**: HNSW, m 16, ef_construction 200, dot product. Strata stores
  8-bit codes and re-scores the best 40 of an ef 64 search with the full
  vectors; Elasticsearch gets the same (`int8_hnsw`, `num_candidates` 64,
  `rescore_vector.oversample` 4), and OpenSearch its equivalent
  (`knn_vector` in `on_disk` mode with 4x compression, which is 8-bit
  codes on the Lucene engine, `ef_search` 64, `rescore.oversample_factor`
  4). Solr's quantized field cannot re-score with full vectors, so it gets
  full-precision vectors (`DenseVectorField`) and `topK` 64. Vespa gets
  full-precision vectors too, as a `paged` attribute so they may be read
  from disk when memory is short (Vespa warns that paged HNSW vectors are
  slow when they do not fit), with `targetHits` 10 and
  `exploreAdditionalHits` 54.
- **Filtered vectors**: Strata's vector field is partitioned by `rating`
  (`partition_by = 'rating', partition_bounds = '1, 2, 3, 4'`). Besides
  its graph of every row, each segment keeps a graph for each range of
  ratings, over the same codes. `vector + filter` (`rating >= 4`) and
  `similar items` (`rating >= 3`) walk the graphs of the ranges they
  cover, and test nothing in a range whose every row passes. Like an
  index on a column a workload filters by, this is a schema choice made
  for these two kinds. The other engines search their one graph with the
  filter applied during the search: Qdrant with a `float` payload index
  on `rating`, Weaviate with a range index, Milvus with an `STL_SORT`
  index. Runs before 6 November 2026 built Strata without the partitions.
- **Segments**: Strata builds its shards as one segment each
  up to 500K documents; a bigger shard is loaded in segments of 500K and
  then compacted as `optimize` does, into segments of up to Strata's cap of
  2M documents (a 5M shard at 10M: 2M, 2M and 1M), since merging into one
  would take more memory than the node has;
  Elasticsearch and OpenSearch are force-merged and Solr optimized to one
  segment after loading. Vespa has no segments to merge: a document is
  searchable once its put returns, and proton flushes and compacts on its
  own (compare.sh asks it to flush before measuring its size on disk).
- **Access**: Strata's servers listen beyond loopback only with a token,
  so its shards, coordinator and the client share a random one, made once
  per cluster by `compare.sh` and kept in the secret `strata-token`; the
  other engines run with their security off, as their images start
  (Typesense needs an API key, a fixed `compare`).
- **No result caches**: the timed runs repeat 300 queries, so no engine
  may answer a repeat from a cache of answers. Solr's `queryResultCache`
  is off (size 0); Elasticsearch's and OpenSearch's shard request caches
  are off for the index while it is timed
  (`index.requests.cache.enable: false`, which also covers their SQL
  endpoints); Manticore's query cache is off (`SET GLOBAL
  qcache_max_bytes=0`; it keeps queries slower than 3 s). Vespa, the
  other engines and Strata keep no result cache by default (Typesense
  only when a request asks with `use_cache`, which these do not). Page
  caches and filter caches stay on everywhere. Each `run-*.json` keeps
  the caches' counters from before and after the timed runs
  (`result_cache`), so a report can show that none answered. Runs
  before 5 November 2026 only passed `request_cache=false` on
  Elasticsearch's and OpenSearch's `_search` and left Manticore's cache
  on, so their SQL aggregates could be answered from the request cache.

## The query kinds

| kind | Strata (SQL) | Elasticsearch | Solr |
|---|---|---|---|
| bm25 | `ORDER BY bm25(body, :t)` | `match` | `{!lucene q.op=OR}` over the query's words |
| bm25 + filter | `WHERE year >= 2010 AND category IN (4 values)` | `bool` with `range` and `terms` filters | `fq` with a range and `{!terms}` |
| vector | `ORDER BY similarity(emb, :v)` | `knn` | `{!knn}` |
| vector + filter | `WHERE rating >= 4` | `knn` with `filter` | `{!knn}` with `fq` (a pre-filter) |
| hybrid (RRF) | `rrf(bm25, similarity)`, k 60, depth 100 | `rrf` retriever (needs a paid licence) | combined query handler (`combiner.algorithm` rrf) |
| hybrid (weighted) | `0.2 * minmax(bm25) + 0.8 * minmax(similarity)`, each min-max normalised over the union of their top 100 | no normalised fusion on the free licence: `match` (boost 0.2) plus `knn` (k 100, boost 0.8), raw scores summed | `{!bool should=...}` of the two, boosted the same (0.44 for BM25, since Solr's BM25 leaves out the factor k1 + 1 = 2.2 that Strata's and Elasticsearch's keep), raw scores summed |
| boosted | `bm25(body, :t) * decay(year, 2024, 5)` | `function_score` with an `exp` decay, multiplied | `{!boost}` by `exp(ln(0.5) / 5 * abs(year - 2024))` |
| facet | `GROUP BY category` over `MATCH(body, :w)`, top 5 | `terms` aggregation, size 5 | JSON facet, `terms`, limit 5 |
| key lookup | `POST /v1/kv/get` of the key, the read by key without SQL (the 1M run of 2026-11-03 sent `WHERE id = :k` to `/v1/query`) | `term` query on `id` | `{!term f=id}` |
| phrase | `bm25(body, '+"a b" words')`: two kept words of the query in a row are required, the rest rank | `match_phrase` in `must`, `match` in `should` | `+"a b" words` |
| typo | `bm25(body, :x, fuzzy => 'auto')`, the query with two letters of its longest word swapped | `match` with `fuzziness: AUTO` | each word `~1` or `~2`, as `AUTO` would |
| prefix | `WHERE MATCH(body, 'pre*')`: the first three letters of the query's last word, ranked by the words before it | `prefix` filter, `match` | `fq=body:pre*` |
| similar items | `WHERE rating >= 3 AND id NOT IN (20 keys) ORDER BY similarity(emb, :item)`: a recommender's "more like this item", minus what the user has seen | `knn` from the item's vector, filter `range` and `must_not ids` | `{!knn}` with `fq` |
| sql aggregate | `SELECT category, COUNT(*), AVG(rating) … WHERE year >= :y GROUP BY category HAVING COUNT(*) > 1000 ORDER BY 2 DESC LIMIT 10` | the same statement to `/_sql` | the same statement to `/sql` (Solr's parallel SQL, the `sql` module, which solr.yaml turns on), as a request parameter, with `` `year` `` in backticks and no column aliases (Solr's HAVING matches nothing with them) |

Elasticsearch searches return only `_id` and `_score` (`_source: false`)
and stop counting hits (`track_total_hits: false`); Solr's are given
`minExactCount` 10 for the same reason.

OpenSearch takes Elasticsearch's requests, except:

| kind | OpenSearch | Vespa |
|---|---|---|
| bm25 | `match` | `userQuery()` (`weakAnd`), rank profile `bm25` |
| bm25 + filter | as Elasticsearch | `and year >= 2010 and category in (…)` |
| vector, vector + filter | `knn` as a query, with `filter` | `nearestNeighbor`, rank profile `vector` |
| hybrid (RRF) | `hybrid` query through a search pipeline with the `score-ranker-processor` (RRF, rank constant 60), depth 100 | `userQuery() or nearestNeighbor` (100), global phase `reciprocal_rank_fusion`, 200 re-ranked |
| hybrid (weighted) | `hybrid` query through the `normalization-processor` (min-max, arithmetic mean, weights 0.2 and 0.8), on the free licence | the same retrieval, global phase `0.2 * normalize_linear(bm25) + 0.8 * normalize_linear(closeness)` |
| boosted | as Elasticsearch | rank profile `boosted`: `bm25(body) * exp(-ln 2 / 5 * abs(year - 2024))` |
| facet | as Elasticsearch | grouping `all(group(category) max(5) order(-count()) each(output(count())))` |
| key lookup | as Elasticsearch | `id contains :k`, unranked, summary of the three fields |
| phrase | as Elasticsearch | `and body contains phrase(a, b)` |
| typo | as Elasticsearch | unavailable: Vespa's fuzzy matching is for attributes, not indexed text |
| prefix | as Elasticsearch | unavailable: prefix matching is for attributes too |
| similar items | `knn` with the filter | `nearestNeighbor` from the item, `and rating >= 3 and !(id in (…))` |
| sql aggregate | the statement to `/_plugins/_sql` | grouping with `count()` and `avg(rating)`; `HAVING` applied to the answer |

Elasticsearch's free licence has no normalised fusion and no `rrf`
retriever. With `compare.sh trial` (Elastic's 30-day trial) a second run,
labelled `es-trial`, asks the two hybrid kinds of the `rrf` retriever and
the `linear` retriever with the `minmax` normalizer; `analyze.py` shows it
beside the free run.

## Writes

`compare.sh writes ENGINE` (`engine-compare writes`) times writes against
the loaded engine, through the same endpoints as the queries (Strata's
coordinator, Vespa's container), at 1, 4 and 16 clients for 6 s each:

| phase | Strata | Elasticsearch, OpenSearch | Solr | Vespa |
|---|---|---|---|---|
| insert one | `/v1/write`, one `insert` | `POST /quora/_doc/<id>` | `/update`, one document | `POST /document/v1/…/<id>` |
| insert batch (100) | one `/v1/write` of 100 inserts | `_bulk` of 100 | `/update` of 100 | 100 puts at once (`/document/v1` takes one document a request) |
| update one | `set` of `rating` in place | the whole document again | the whole document again | partial update (`assign`) of the `rating` attribute |

New documents are copies of the collection's first 20,000 under new keys;
an update gives a document of the first 20,000 a new rating. Elasticsearch's,
OpenSearch's and Solr's partial updates rebuild a document from its stored
source, which here leaves out `body` and `emb` (kept only in the index), so
they would lose them: they are sent the whole document instead, as an
application that owns its records would. "Visible" writes 30 documents one
at a time, each with a word no other has, and searches for the word every 5
ms until it is found: Strata and Vespa make a write searchable when it
returns; Elasticsearch and OpenSearch get their default refresh of 1 s
(loading turned it off) and Solr a soft commit every second. Durability is
each engine's default (Strata and Elasticsearch sync the log at every
write). Afterwards the written documents are deleted and the old ratings
written back, so the queries can be run again; the segments differ from
the loaded ones, so time the queries first.

## Streams

`compare.sh stream ENGINE` (`engine-compare stream`) starts one Redpanda
broker on the clients' node (`redpanda.yaml`: 1 CPU and 1 GiB, outside the
engine's budget, developer mode without fsync) and streams new documents
through a topic of 4 partitions into the engine. A document is about 10 KB of
JSON (its 1,024-number vector), keyed by a new id; a record's timestamp is
the time it was due.

- **Rates**: 250, 500, 1,000 and 2,000 documents a second for 30 s each,
  stopping after the first rate the engine falls behind at; then 20,000
  produced as fast as the producer can, for the consumer's top speed.
- **The consumer**: one task a partition, reading with a 10 ms wait and
  writing at most 500 documents a request, one request at a time: Strata
  through `/v1/stream` (each line with its source offset, kept by the
  server in the rows' transaction), Elasticsearch and OpenSearch `_bulk`,
  Solr `/update`, Vespa one put a document, 48 at once a partition. Pushback
  (429, 503) is retried a second later, so no event is dropped.
- **Lag**: from when a document was due to the engine's answer to the write
  holding it. Strata answers a batch once its rows are visible, and a Vespa
  put is visible when it returns; Elasticsearch, OpenSearch and Solr answer
  once a document is written, and it is searchable within their 1 s
  refresh or soft commit. The read lag (due to read from the topic) is
  reported beside it.
- **Crash**: a last stream of 10,000 documents at half the top speed, whose
  consumer crashes once half are written, right after a write and before
  it commits its offsets, and starts again: Strata's from the offsets the
  server has (`GET /v1/sources`), the others' from the offsets they
  committed (every second, as Kafka's consumers do). It counts the events
  written a second time (harmless here, since a document's id makes the
  write idempotent, but each would count twice in a counter) and the
  documents lost (none should be).

Afterwards the streamed documents are deleted and the topic too.

## Traffic simulator

`run` asks each kind's queries in turn, as fast as a fixed number of
clients can: a closed loop, which measures each engine at its best and
cannot say what a user would wait at a given load. The traffic simulator
sends a planned stream of requests instead, at the times it was planned for,
whatever the engine's answers: an open loop, so a slow answer delays nothing
behind it, and requests that would have queued are counted from when they
were due (as wrk2 does, against coordinated omission). It comes in two
steps, so that every engine gets the same requests at the same times:

```bash
B=strata-server/target/release/engine-compare    # or the image's engine-compare
P=strata-server/deploy/compare/profiles
$B --collection msmarco-1000k-v2-h5 plan --profile $P/search-app.toml --seed 1 --rate 200 --out search-app-1.jsonl.gz
$B --collection msmarco-1000k-v2-h5 replay --engine strata --url http://127.0.0.1:7700 --schedule search-app-1.jsonl.gz --out reports
# In the cluster: plans in the Job, then replays (PROFILE is a name under profiles/ or a path under /data).
$C replay strata search-app --rate 200;  $C replay es search-app --rate 200
$C replay strata search-app --shape ramp --label strata-ramp
```

**A profile** (`profiles/*.toml`, every value checked when it is read) holds
the distributions:

| section | what it draws |
|---|---|
| `mix` | the share of each kind of `KINDS` a session sets out to ask |
| `queries` | the collection's queries or a larger pool (`source = "pool"`); a Zipf head (`head`, `zipf_s`) that drifts (`drift_share` of it replaced every `drift_every_s`), a tail of queries asked once (`tail_share`), misspellings (`typo_share`: Strata's typo kind under `typos = "fuzzy"`, the text misspelt for every engine under `"plain"`) |
| `docs` | which documents key lookups and similar items ask for: Zipf over the collection (`popular`; `doc_popularity.f32` beside it, if present, gives the weights) or `run`'s strides (`legacy`) |
| `filters` | `run`'s fixed filters, or `drawn`: a target share of documents (log-uniform between `min_selectivity` and `max_selectivity`) met by a year and categories, or a rating, from the collection's own counts |
| `sessions` | follow-ups (`follow_up`, the weights of refine, open and similar), think times (log-normal), and typing: one prefix request a keystroke, a log-normal gap apart, before the query |
| `traffic` | the day (`day_s` long, `peak_to_trough`, `humps`, `peak_at`), bursts (a two-state Markov process: `factor` times the rate for spells of `busy_s` every `calm_s`), flash crowds (a few queries, `factor` times the rate, for `for_s`), the warm-up, the ramp's steps, the closed loop's concurrencies |
| `writes` | their share of requests, inserts, updates and deletes among them, batch sizes, `jobs` (bulk loads of `docs` in batches, beside the traffic), and every `mark_every`-th insert carrying a word no other document has |
| `slo` | the p99 a step must hold and the share of errors it may have |

`today.toml` is `run` again (the collection's queries in order, `run`'s
filters and documents): `replay --shape closed` with it sends the requests
`run` sends, at the same concurrencies, which a test checks for all 14
engines. `search-app.toml` is a search box in an app (typing, filters,
sessions, a 3:1 day with bursts and a flash crowd, 2% writes and a bulk
job); `rag.toml` is retrieval for a model (vector and hybrid, longer
thinking, no typing, 5% writes and an ingest job). Their starting values
and where they came from are in their comments.

**Shapes**: `--shape day --rate R` (R requests a second at the day's peak),
`--shape ramp` (the profile's steps of rising rates; a step holds when at
most 1% of its reads are over the SLO's p99, counting requests still in
flight at its end as late, and its errors stay under the SLO's share; two
steps that do not hold end the replay, and the report's
`capacity_at_slo` is the rate of the last step that held), `--shape closed`
(`run`'s loop). A warm-up (`warmup_s`, head queries only, no writes, planned
by a generator of its own so that it never shifts the timed requests) goes
first and is left out of every number.

**The schedule** is gzip-compressed JSON lines: a header (the profile, its
SHA-256, the seed, the collection, the steps), one line a request (its time,
kind, query, filters, session, write), and a trailer with what the plan
measured of itself (requests by kind, Zipf head share, first-time queries,
selectivity quantiles, peak rate). It names no engine: `replay` turns each
line into each engine's request as it sends it. The same profile, seed and
collection make the same file, byte for byte; its hash (`zcat FILE |
sha256sum`) goes into every report replayed from it.

**The report** (`OUT/replay-LABEL.json`) gives, for each kind, the response
time (from when the request was due) and the service time (from when it was
sent), from HDR histograms; the same by 10 s window, with whether the window
held the SLO; each ramp step's verdict; how soon a marked write was found (a
search for its word every 5 ms, up to 60 s); the driver's own lag behind the
schedule; and errors. A dispatcher thread sleeps until a request's time and
spins for the last 200 µs, so the driver's lag stays well under a
millisecond, and a test serving 4 ms answers one at a time at 75% load checks
the measured wait against M/D/1's. Afterwards the documents the schedule
inserted are deleted and the ratings it changed written back (`--no-reset`
keeps them).

**Caches**: `--cache-mode ranked` (the default) sends requests as `run` does,
with Elasticsearch's and OpenSearch's request cache off by request, so a
repeated head query is not answered from a cache of results that one engine
has and another has not; `shipped` sends them as each engine ships, for the
numbers a user of its defaults would see. Page and filter caches stay on in
both. Two result caches are as shipped in both modes until `run` turns them
off too: Elasticsearch's and OpenSearch's SQL endpoints (the SQL aggregate)
and Manticore's query cache. Report the mode with every number.

**Data**: a profile can ask for more than `prepare_msmarco_compare.py`'s
first collection holds:

- `--attrs v2` draws the fields as a catalogue's are (years growing, ratings
  skewed high, the category tied to the topic, or kept away from it with
  `--categories anti`); `--heldout 0.05` keeps every 20th passage out, in
  `heldout.jsonl` and `heldout_vecs.f32`, for inserts of documents no engine
  has seen (without them, inserts copy loaded documents under new keys).
  Either goes into a directory of its own (`msmarco-1000k-v2-h5`); the
  default collection is the one before, row for row.
- `strata/scripts/prepare_query_pool.py COLLECTION_DIR --n 1000000` writes a
  pool of queries beside it (ORCAS's and MS MARCO's train queries, without
  the collection's own, embedded with the same model) for `source = "pool"`.
- `strata/scripts/fit_traffic.py` fits a profile's values to data: `zipf` to
  a query log (ORCAS's clicks), `day` to Wikipedia's hourly page views.

The nine one-node engines replay writes too, each through its own API:

| engine | insert | rating update | delete |
|---|---|---|---|
| Meilisearch | add documents (a task) | add or update with the rating only | delete a batch of ids |
| Typesense | import, upsert | `PATCH` of the document | delete by `filter_by=id:[…]` |
| Qdrant | upsert points (`wait=true`) | set the payload's rating | delete points |
| Weaviate | batch of objects | `PATCH` of the object | delete each object |
| Milvus | insert | upsert of the whole document (no partial update) | delete by `key in […]` |
| Manticore | bulk `replace` | `/update` of the attribute | `DELETE … WHERE id IN (…)` |
| ParadeDB, Postgres | `INSERT` | `UPDATE … SET rating` | `DELETE … WHERE key IN (…)` |
| Redis | `HSET` | `HSET` of the rating | `DEL` |

Qdrant and Manticore number their documents: a loaded one by its row, a
replay's own from its key, above every row. The freshness prober searches
for the marked word with the engine's bm25 request and waits for the new
document's key in the answer (Meilisearch and Typesense forgive typos, so
another marked document can match the word too). After the reset, the
report's `reset.searchable_docs_after` is the number of documents the engine
says it holds, which should be the collection's again.

## Nine more engines, on one node

`compare.sh up|load|run|down ENGINE` takes `meili`, `typesense`,
`manticore`, `qdrant`, `weaviate`, `milvus`, `paradedb`, `postgres` and
`redis` too. Each is one pod (`more/<engine>.yaml`) with the whole budget of
the others' two data nodes, 3 CPUs and 6 GiB, on the first data node, its
data on that node's disk; the clients run on the third worker as before.
None of them is sharded across the two nodes here: several shard only in
their paid or cluster editions, and a one-node figure is what a small
deployment of each would see. `load` creates the index untimed, times the
bulk writes, then times what the engine does before every document is
searchable and indexed (its indexing tasks, index builds, flushes and
merges), and `compare.sh` measures the data directory with `du` (Redis: its
memory). Writes and streams are not measured for them yet.

| engine | image | text | vectors (HNSW, m 16, ef_construction 200, dot product) |
|---|---|---|---|
| Meilisearch | `getmeili/meilisearch:v1.56.0` | its ranking rules (not BM25), no stemming, Lucene's stopwords; 3 indexing threads, 3 GiB | a `userProvided` embedder |
| Typesense | `typesense/typesense:30.2` | its text match (not BM25), `stem: true`, Lucene's and Strata's query stopwords as a stopword set | `float[]` field, `ef` 64 |
| Manticore | `manticoresearch/manticore:29.9.0` | BM25 as `bm25a(1.2, 0.75)` with plain IDF, `stem_en`, its English stopwords | `float_vector`, 8-bit quantization, `ef` 64, rescore with oversampling 4 |
| Qdrant | `qdrant/qdrant:v1.20.0` | BM25 from the client: sparse vectors of Strata's analysis (tf weighted with k1 and b, average length from a pass over the documents), Qdrant's IDF modifier | int8 scalar quantization, `hnsw_ef` 64, rescore with oversampling 4 |
| Weaviate | `semitechnologies/weaviate:1.39.14` | its BM25 (k1 1.2, b 0.75), word tokenization, no stemming, its English stopwords | 8-bit rotational quantization, `ef` 64, rescore limit 40 |
| Milvus | `milvusdb/milvus:v2.6.26` (standalone, embedded etcd, local storage) | its BM25 function over the `english` analyzer | `HNSW_SQ` (SQ8) refined with FP32, refine_k 4, `ef` 64 |
| ParadeDB | `paradedb/paradedb:0.26.4-pg17` | `pg_search`'s BM25 over its `simple` tokenizer with the English stemmer, stopwords and ASCII folding | the vectors in the same index (`vector_ip_ops`) |
| Postgres | the same image, without `pg_search` | `ts_rank` (no IDF: not BM25) over a generated `tsvector` with a GIN index | `pgvector` HNSW, `ef_search` 64, `iterative_scan` strict_order |
| Redis | `redis:8.8.4` (Redis Query Engine; no persistence, 5.5 GB maxmemory) | `BM25STD` with its English stemmer and Lucene's stopwords | `HNSW` `FLOAT16` (with `FLOAT32` a million 384-dimension vectors, held in the hash and again in the index, and the text do not fit in 6 GiB: the first 1M NQ load ran out of memory at about 93%), `EF_RUNTIME` 64 |

Each kind is the engine's closest native request, and its report says
where it has none (`unavailable`) or where the request means something
else than Strata's (`differs`); `analyze.py` prints both under the tables.
In short:

| engine | unavailable | means something else |
|---|---|---|
| Meilisearch | boosted (no function scores), sql aggregate | text kinds ranked by its rules, typos and a prefix last word on; hybrids by `semanticRatio` 0.5 and 0.8, not RRF or min-max |
| Typesense | boosted (decays only sort), sql aggregate | text kinds ranked by its text match; hybrids by its rank fusion, `alpha` 0.5 and 0.8 |
| Manticore | hybrid (weighted): its fusion is RRF only | typo: every word must match (its fuzzy search takes no OR) |
| Qdrant | typo, prefix, sql aggregate | phrase: BM25 filtered by the text index's phrase match; weighted hybrid by `dbsf` (no weights); boosted re-ranks the BM25 top 100 by a formula |
| Weaviate | boosted, phrase, typo | prefix as a `Like` filter; facet and sql aggregate as grouped `Aggregate` queries; hybrids by `rankedFusion` and `relativeScoreFusion` |
| Milvus | facet, typo, prefix, sql aggregate | weighted hybrid normalised by arctan, not min-max; boosted by its decay ranker over the BM25 top 100 |
| ParadeDB | – | hybrids in SQL over each top 100 (ParadeDB's own pattern); prefix as a regex on the terms |
| Postgres | typo (full-text search has no fuzzy matching) | `ts_rank`, not BM25; hybrids in SQL over each top 100 |
| Redis | – | weighted hybrid sums raw BM25STD and vector scores (FT.HYBRID does not normalise them); typo with `%word%` and `%%word%%` |

Postgres and Redis are spoken to over their own protocols (the simple query
protocol, with trust authentication, and RESP2), a few lines each in
`more.rs`, so the client needs no library for them.

## Recommender kinds

`engine-compare recsys-load` and `recsys-run` (`src/bin/engine-compare/recsys.rs`)
time what a recommender asks of its store, on Strata, Elasticsearch,
OpenSearch, Vespa, Qdrant, Milvus, Redis and Postgres (pgvector). The data
is `strata/results/recsys/serving_data.py`'s, made from Yambda-50M: the
most played tracks, each with the vector the sequence model learned (64
numbers), its artist, the regions it may be played in (synthetic: Yambda
has none), a popularity and twelve ranking features; users with their
vector at the start of the test day, their region and the tracks they
played; and a LightGBM ranker of 300 trees over the features and the
user-track dot product (`f12`). Each kind is sent for each of `--users`
users in turn:

| kind | what | Strata |
|---|---|---|
| r1 user to items | the 500 nearest tracks in the user's region, minus the (up to) 500 they played | `WHERE regions = :r AND id NOT IN (...) ORDER BY similarity(emb, :u, ef => 1000, rerank => 1000) DESC LIMIT 500` |
| r2 candidates (RRF) | three sources under r1's filters, 1,000 deep: nearest the user, nearest their last track, most popular; fused by RRF (k 60), the best 1,000 | `rrf(similarity(emb, :u), similarity(emb, :l), pop, depth => 1000)` |
| r3 features | the twelve features of 1,000 tracks by key | `POST /v1/kv/get` with `columns` (the twelve) |
| r4 GBDT rerank | those 1,000 by the ranker; the best 50 | a rerank phase: `WITH c AS (... ORDER BY ranked(:c) ...) SELECT ... ORDER BY gbdt('ranker', ..., dot(emb, :u))` |
| r5 diversify | r1's ranking with at most 2 tracks an artist; the best 20 | `... MAX 2 PER artist LIMIT 20` |
| r6 events while serving | r1 while plays are written one a request at `--event-rate`, and how soon a play is in a query of the user's last 5 | `/v1/write`, then `SELECT id FROM events WHERE usr = :u ORDER BY ts DESC LIMIT 5` |

`run` sends each kind once and scores the answers against exact ones it
computes from the same data (recall for r1, r2 and r5, rows found for r3,
and for r4 the share of the answer in the model's own top 50, ties
counted), then times them closed-loop at each `--concurrency` after a
2-second warm-up. Where an engine has no request that does a kind's whole
job, the client does the rest and the report says so (`differs`):

| engine | r2 | r4 | r5 |
|---|---|---|---|
| Elasticsearch | the `rrf` retriever (two `knn`, one `function_score` of pop), on the trial licence | `_mget`, the trees scored in the client | the `knn` query, 200, collapsed by artist, `inner_hits` 2 |
| OpenSearch | a `hybrid` query through a search pipeline's `score-ranker-processor`, rrf, k 60 | `_mget`, scored in the client | `knn` 200 collapsed by artist, `inner_hits` 2 |
| Vespa | three queries at once (two `nearestNeighbor`, one of the region `order by pop desc`), fused in the client | `lightgbm("ranker.json")` in a rank profile | `nearestNeighbor` 200 grouped by artist |
| Qdrant | the query API's `prefetch` (two vectors, one `order_by` pop) with `rrf` k 60 | points by id, scored in the client | `query/groups` by artist, group size 2 |
| Milvus | the two vector searches at once, fused in the client with the region's most popular, which the client keeps (Milvus ranks by vectors only) | entities by id, scored in the client | a search of 200 grouped by artist, group size 2 |
| Redis | three `FT.SEARCH` at once (two KNN, one `SORTBY pop`), fused in the client | 1,000 pipelined `HMGET`, scored in the client | KNN 200 with the artist, capped in the client |
| Postgres | one SQL statement: three subqueries ranked by `row_number()`, summed as RRF | `WHERE id = ANY(...)`, scored in the client | the nearest 200 in a subquery, `row_number()` over each artist |

The client scores the trees with Strata's own GBDT evaluator, so a
client-side r4 costs what Strata's trees cost plus the read. Vectors are
each engine's HNSW of full-precision floats (m 16, ef_construction 200, dot
product), searched with 1,000 candidates for r1 and twice the depth for the
others (pgvector's `ef_search` at most 1,000); Strata walks its graph over
its 8-bit codes, as it always does, with the same `ef` and every candidate
re-scored with the floats (`similarity(emb, :u, ef => 1000, rerank =>
1000)`), and its database is merged into one segment after the load, as
Elasticsearch's and OpenSearch's are. Qdrant indexes a segment
once it holds 10 MB of vectors (its default `indexing_threshold`), and
searches smaller segments exactly: at 100,000 tracks of 64 numbers it
indexed 81,500 in one of its two segments (`recsys-load` records it). Vespa's
package is `vespa-recsys/` (`package.sh` adds the ranker); Strata's server
starts with `recsys/strata-items.json` as its schema and the serving data
as its `--models`.

Four things each engine's documentation leaves easy to miss.
Elasticsearch's `rrf` retriever needs a paid or trial licence, so
`local.sh` starts it on the 30-day trial. Elasticsearch's collapse runs its
`inner_hits` as the request's `query` within each group, so r5 sends the
`knn` query; with a top-level `knn` the inner hits are any two tracks of
the artist. Milvus serves a collection created with its indexes from
growing segments through an interim index until the sealed segments are
handed over, a fraction of the HNSW's recall (0.35 at r1 in a test), so
`recsys-load` releases and loads each collection once the index is built.
Vespa's `range(pop)` with a `hitLimit` takes the most popular before the
query's filters, and a query that matches the whole region computes
`closeness` for every document it matches, not only for the neighbours, so
no one query keeps the region's 1,000 most popular beside the two
neighbour lists; r2 sends three.

On one machine, one engine at a time in Docker (each capped at `CPUS` and
`MEM`; Strata's server runs outside Docker, on the first `CPUS` cores):

```bash
python3 strata/results/recsys/serving_data.py --work ~/data/yambda/work --data ~/data/yambda --out ~/data/recsys-100k --items 100000
(cd strata-server && cargo build --profile ci --bins)
DATA=~/data/recsys-100k OUT=~/data/recsys-out strata-server/deploy/compare/recsys/local.sh strata es os vespa qdrant milvus redis postgres
```

## Running it

With the cluster of `../kind-test.sh up` (its `/data` and `/beir`
mounts), from the repository root:

```bash
export STRATA_WORK=/path/to/dbs          # mounted at /data; reports go to $STRATA_WORK/compare
C=strata-server/deploy/compare/compare.sh
$C image
$C build;        $C up strata; $C run strata; $C writes strata; $C stream strata; $C down strata
$C up es;        $C load es;   $C run es;     $C writes es;    $C stream es
$C trial;        $C run es --kinds "hybrid (RRF),hybrid (weighted)" --paid --label es-trial; $C down es
$C up os;        $C load os;   $C run os;     $C writes os;    $C stream os;    $C down os
$C up solr;      $C load solr; $C run solr;   $C writes solr;  $C stream solr;  $C down solr
$C up vespa;     $C load vespa; $C run vespa; $C writes vespa; $C stream vespa; $C down vespa
python3 strata-server/deploy/compare/analyze.py $STRATA_WORK/compare $STRATA_DATA/quora
# Strata with its coordinator in a pod of its own, as before 6 November
# 2026, at the old split and another, kept beside the default run:
export STRATA_COORD=pod
$C up strata; $C run strata --label strata-coord-pod; $C down strata
SHARD_CPU=1250m COORD_CPU=500m $C up strata; $C run strata --label strata-coord500m; $C down strata
unset STRATA_COORD
# The nine one-node engines, one at a time:
for e in meili typesense manticore qdrant weaviate milvus paradedb postgres redis; do
    $C up $e; $C load $e; $C run $e; $C down $e
done
```

`COLLECTION=nq-1000k` before any of these runs them on another collection
(the vector size comes from its `meta.json`; Vespa's schema is written for
it); give `analyze.py` that collection's directory.

The Docker VM must allow `vm.max_map_count` of at least 262144 for
Elasticsearch, and have room for one engine (6 GiB) at a time, plus
Kubernetes: stop the other engines, and scale down `kind-test.sh`'s own
Strata pods, first.
