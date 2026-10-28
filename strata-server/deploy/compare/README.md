# Strata, Elasticsearch and Solr on the same cluster

Runs Strata, Elasticsearch and Solr one at a time on the kind cluster of
[`../kind-test.sh`](../kind-test.sh), on the same data, with the same number
of shards and the same CPU and memory, and asks each the query kinds of
`strata-server-bench`. Results of a run on a Mac are in
[`../../results/compare-mac/README.md`](../../results/compare-mac/README.md).

| file | what |
|---|---|
| `compare.sh` | builds and loads the image, starts and stops each engine, loads it, runs the client |
| `es.yaml`, `solr.yaml` | two data nodes of Elasticsearch 9.5.5 or Solr 10.1.0 (SolrCloud, ZooKeeper embedded in `solr-0`) |
| `strata.yaml`, `strata-coordinator.yaml` | two Strata shards and their coordinator |
| `analyze.py` | scores the answers and prints the tables |
| `../../src/bin/engine-compare.rs` | the client: `build` (a Strata shard), `load` (Elasticsearch or Solr) and `run` (time the kinds and record the answers) |

## What is the same for every engine

- **Data**: BEIR Quora as `strata/scripts/prepare_beir.py` prepares it:
  522,931 questions with Cohere embed-english-v3 vectors (1,024
  dimensions, unit length) and the synthetic `year`, `category`, `rating`
  and `topic` fields of `strata::data`.
- **Shards**: 2, no replicas, on the same two worker nodes (one each);
  the clients run on the third worker. Every index lives on its node's own
  disk.
- **CPU and memory**: 3 CPUs and 6 GiB per engine, as Kubernetes limits:
  Elasticsearch and Solr have two data nodes of 1.5 CPUs and 3 GiB (heap
  1.5 GB for Elasticsearch, as Elastic recommends; 1 GB for Solr, whose
  full-precision vectors need the page cache); Strata has two shards of 1.25
  CPUs and 2.5 GiB and a coordinator of 0.5 CPU and 1 GiB, since its
  shards do not also coordinate. Loading has the same budget: Elasticsearch
  and Solr take bulk requests on their two data nodes (from a loader pod on
  the third node, outside the budget), and Strata builds its shards one
  after the other in a job of 3 CPUs and 6 GiB on the shard's node (a
  shard's build holds its documents in memory; two at once in 3 GiB each
  ran out of memory).
- **Text analysis**: Strata's, rebuilt in the other two: words are runs of
  letters and digits, lowercased and folded to ASCII, minus Lucene's classic
  English stopwords, stemmed with Snowball English (Porter 2); ranked
  queries also drop Strata's query stopwords. BM25 with k1 1.2, b 0.75
  everywhere.
- **Vectors**: HNSW, m 16, ef_construction 200, dot product. Strata stores
  8-bit codes and re-scores the best 40 of an ef 64 search with the full
  vectors; Elasticsearch gets the same (`int8_hnsw`, `num_candidates` 64,
  `rescore_vector.oversample` 4). Solr's quantized field cannot re-score
  with full vectors, so it gets full-precision vectors (`DenseVectorField`)
  and `topK` 64.
- **One segment per shard**: Strata builds its shards as one segment each;
  Elasticsearch is force-merged and Solr optimized to one segment after
  loading.
- **No result caches**: the timed runs repeat 300 queries, so Solr's
  `queryResultCache` is off and Elasticsearch's shard request cache is
  bypassed (`request_cache=false`); filter caches stay on in all three.

## The query kinds

| kind | Strata (SQL) | Elasticsearch | Solr |
|---|---|---|---|
| bm25 | `ORDER BY bm25(body, :t)` | `match` | `{!lucene q.op=OR}` over the query's words |
| bm25 + filter | `WHERE year >= 2010 AND category IN (4 values)` | `bool` with `range` and `terms` filters | `fq` with a range and `{!terms}` |
| vector | `ORDER BY similarity(emb, :v)` | `knn` | `{!knn}` |
| vector + filter | `WHERE rating >= 4` | `knn` with `filter` | `{!knn}` with `fq` (a pre-filter) |
| hybrid (RRF) | `rrf(bm25, similarity)`, k 60, depth 100 | `rrf` retriever (needs a paid licence) | combined query handler (`combiner.algorithm` rrf) |
| hybrid (weighted) | `0.2 * bm25 + 0.8 * similarity`, each min-max normalised over the union of their top 100 | no normalised fusion on the free licence: `match` (boost 0.2) plus `knn` (k 100, boost 0.8), raw scores summed | `{!bool should=...}` of the two, boosted the same (0.44 for BM25, since Solr's BM25 leaves out the factor k1 + 1 = 2.2 that Strata's and Elasticsearch's keep), raw scores summed |
| boosted | `bm25(body, :t) * decay(year, 2024, 5)` | `function_score` with an `exp` decay, multiplied | `{!boost}` by `exp(ln(0.5) / 5 * abs(year - 2024))` |
| facet | `GROUP BY category` over `MATCH(body, :w)`, top 5 | `terms` aggregation, size 5 | JSON facet, `terms`, limit 5 |
| key lookup | `WHERE id = :k` | `term` query on `id` | `{!term f=id}` |

Elasticsearch searches return only `_id` and `_score` (`_source: false`)
and stop counting hits (`track_total_hits: false`); Solr's are given
`minExactCount` 10 for the same reason.

## Running it

With the cluster of `../kind-test.sh up` (its `/data` and `/beir`
mounts), from the repository root:

```bash
export STRATA_WORK=/path/to/dbs          # mounted at /data; reports go to $STRATA_WORK/compare
C=strata-server/deploy/compare/compare.sh
$C image
$C build;        $C up strata; $C run strata; $C down strata
$C up es;        $C load es;   $C run es;     $C down es
$C up solr;      $C load solr; $C run solr;   $C down solr
python3 strata-server/deploy/compare/analyze.py $STRATA_WORK/compare $STRATA_DATA/quora
```

The Docker VM must allow `vm.max_map_count` of at least 262144 for
Elasticsearch, and have room for one engine (6 GiB) at a time, plus
Kubernetes: stop the other engines, and scale down `kind-test.sh`'s own
Strata pods, first.
