# Strata, Elasticsearch, OpenSearch, Solr and Vespa on the same cluster

Runs Strata, Elasticsearch, OpenSearch, Solr and Vespa one at a time on the
kind cluster of [`../kind-test.sh`](../kind-test.sh), on the same data, with
the same number of shards and the same CPU and memory, and asks each the
query kinds of `strata-server-bench` and five more (phrase, typo, prefix,
similar items, an SQL aggregate). Results of a run on a Mac (Strata,
Elasticsearch and Solr, 28 October 2026) are in
[`../../results/compare-mac/README.md`](../../results/compare-mac/README.md).

| file | what |
|---|---|
| `compare.sh` | builds and loads the image, starts and stops each engine, loads it, runs the client |
| `es.yaml`, `os.yaml`, `solr.yaml` | two data nodes of Elasticsearch 9.5.5, OpenSearch 3.9.0 or Solr 10.1.0 (SolrCloud, ZooKeeper embedded in `solr-0`) |
| `vespa.yaml`, `vespa/` | Vespa 8.778.45: `vespa-0` (config server, cluster controller and the container that answers queries) and two content nodes; `vespa/package.sh` writes the application package (`services-cluster.xml`, `schemas/doc.sd`, the stopword lists); `vespa/services.xml` is the same application on one node, for trying it in Docker |
| `strata.yaml`, `strata-coordinator.yaml` | two Strata shards and their coordinator |
| `analyze.py` | scores the answers and prints the tables |
| `../../src/bin/engine-compare.rs` | the client: `build` (a Strata shard), `load` (the other engines) and `run` (time the kinds and record the answers) |

## What is the same for every engine

- **Data**: BEIR Quora as `strata/scripts/prepare_beir.py` prepares it:
  522,931 questions with Cohere embed-english-v3 vectors (1,024
  dimensions, unit length) and the synthetic `year`, `category`, `rating`
  and `topic` fields of `strata::data`.
- **Shards**: 2, no replicas, on the same two worker nodes (one each);
  the clients run on the third worker. Every index lives on its node's own
  disk.
- **CPU and memory**: 3 CPUs and 6 GiB per engine, as Kubernetes limits:
  Elasticsearch, OpenSearch and Solr have two data nodes of 1.5 CPUs and 3
  GiB (heap 1.5 GB for Elasticsearch and OpenSearch, as Elastic recommends;
  1 GB for Solr, whose full-precision vectors need the page cache); Strata
  has two shards of 1 CPU and 2.5 GiB and a coordinator of 1 CPU and 1 GiB,
  since its shards do not also coordinate (`SHARD_CPU` and `COORD_CPU`
  split it otherwise; the results compare 1.25 + 1.25 + 0.5 too). Vespa is
  split as Strata is, since its content nodes do not answer queries either:
  `vespa-0` on the clients' node (config server, cluster controller and the
  container, 1 CPU and 2.25 GiB, container heap 768 MB) and two content
  nodes of 1 CPU and 1.875 GiB. Vespa asks for 4 GB per node and refuses to
  start below it, so its pods set `VESPA_IGNORE_NOT_ENOUGH_MEMORY`.
  Loading has the same budget: Elasticsearch, OpenSearch and Solr take bulk
  requests on their two data nodes and Vespa one document per request on
  its container (from a loader pod on the third node, outside the budget),
  and Strata builds its shards one after the other in a job of 3 CPUs and 6
  GiB on the shard's node (a shard's build holds its documents in memory;
  two at once in 3 GiB each ran out of memory).
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
- **One segment per shard**: Strata builds its shards as one segment each;
  Elasticsearch and OpenSearch are force-merged and Solr optimized to one
  segment after loading. Vespa has no segments to merge: a document is
  searchable once its put returns, and proton flushes and compacts on its
  own (compare.sh asks it to flush before measuring its size on disk).
- **No result caches**: the timed runs repeat 300 queries, so Solr's
  `queryResultCache` is off and Elasticsearch's and OpenSearch's shard
  request caches are bypassed (`request_cache=false`); filter caches stay
  on everywhere. Vespa keeps no result cache by default.

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
| key lookup | `WHERE id = :k` | `term` query on `id` | `{!term f=id}` |
| phrase | `bm25(body, '+"a b" words')`: two kept words of the query in a row are required, the rest rank | `match_phrase` in `must`, `match` in `should` | `+"a b" words` |
| typo | `bm25(body, :x, fuzzy => 'auto')`, the query with two letters of its longest word swapped | `match` with `fuzziness: AUTO` | each word `~1` or `~2`, as `AUTO` would |
| prefix | `WHERE MATCH(body, 'pre*')`: the first three letters of the query's last word, ranked by the words before it | `prefix` filter, `match` | `fq=body:pre*` |
| similar items | `WHERE rating >= 3 AND id NOT IN (20 keys) ORDER BY similarity(emb, :item)`: a recommender's "more like this item", minus what the user has seen | `knn` from the item's vector, filter `range` and `must_not ids` | `{!knn}` with `fq` |
| sql aggregate | `SELECT category, COUNT(*), AVG(rating) … WHERE year >= :y GROUP BY category HAVING COUNT(*) > 1000 ORDER BY 2 DESC LIMIT 10` | the same statement to `/_sql` | the same statement to `/sql` (Solr's parallel SQL) |

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

## Running it

With the cluster of `../kind-test.sh up` (its `/data` and `/beir`
mounts), from the repository root:

```bash
export STRATA_WORK=/path/to/dbs          # mounted at /data; reports go to $STRATA_WORK/compare
C=strata-server/deploy/compare/compare.sh
$C image
$C build;        $C up strata; $C run strata; $C down strata
$C up es;        $C load es;   $C run es
$C trial;        $C run es --kinds "hybrid (RRF),hybrid (weighted)" --paid --label es-trial; $C down es
$C up os;        $C load os;   $C run os;     $C down os
$C up solr;      $C load solr; $C run solr;   $C down solr
$C up vespa;     $C load vespa; $C run vespa; $C down vespa
python3 strata-server/deploy/compare/analyze.py $STRATA_WORK/compare $STRATA_DATA/quora
# Strata at another split of its 3 CPUs, kept beside the default run:
SHARD_CPU=1250m COORD_CPU=500m $C up strata; $C run strata --label strata-coord500m; $C down strata
```

The Docker VM must allow `vm.max_map_count` of at least 262144 for
Elasticsearch, and have room for one engine (6 GiB) at a time, plus
Kubernetes: stop the other engines, and scale down `kind-test.sh`'s own
Strata pods, first.
