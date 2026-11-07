# 10M: every engine from its newest run, ranked

`rank.py --no-load` on a directory holding each engine's passes from its
newest run: Strata from `10m/` here (6 November, three passes), Elasticsearch
and OpenSearch from [`bigger-items-do/10m`](../bigger-items-do/10m) (5
November, result caches off, three passes), the others from
[`compare-msmarco-10m-do`](../compare-msmarco-10m-do) (3 November, one pass).
Milvus and Redis ran there on 128 GB nodes and are left out
(`--without milvus,redis`), as on 3 November. Made with this directory's
`composite.py`:

```bash
python3 composite.py /tmp/c ../compare-msmarco-10m-do strata=10m es=../bigger-items-do/10m os=../bigger-items-do/10m
python3 ../../deploy/compare/rank.py --without milvus,redis --no-load /tmp/c
python3 composite.py /tmp/h ../bigger-items-do/10m strata=10m
python3 ../../deploy/compare/rank.py --no-load /tmp/h
```

Strata's and the Lucene engines' nodes differ (two m3-4vcpu-32gb each; see
the README's node check).

## All nine ranked engines

### Places summed over 45 tests, on the median of the passes

| place | engine | points | average place | tests run | average place on those | first on |
|---|---|---|---|---|---|---|
| 1 | strata | 88 | 1.96 | 45 | 1.96 | bm25 p50, bm25 + filter p50, bm25 + filter q/s, phrase p50, phrase p99, phrase q/s, prefix p50, prefix p99, prefix q/s, facet p50, facet p99, facet q/s, vector p50, vector p99, vector + filter p50, vector + filter p99, hybrid (RRF) p50, hybrid (RRF) p99, hybrid (RRF) q/s, hybrid (weighted) p50, hybrid (weighted) p99, hybrid (weighted) q/s, similar items p99, sql aggregate p50, sql aggregate p99, sql aggregate q/s |
| 2 | os | 180 | 4.00 | 45 | 4.00 | bm25 q/s |
| 3 | es | 186 | 4.13 | 45 | 4.13 | typo p50, typo p99, typo q/s |
| 4 | vespa | 242 | 5.38 | 39 | 4.82 | boosted (bm25 × decay) p50, boosted (bm25 × decay) q/s |
| 5 | qdrant | 244 | 5.42 | 36 | 4.53 | bm25 p99, bm25 + filter p99, boosted (bm25 × decay) p99, vector recall |
| 6 | paradedb | 258 | 5.73 | 45 | 5.73 | similar items recall |
| 7 | solr | 275 | 6.11 | 45 | 6.11 | vector + filter recall |
| 8 | weaviate | 294 | 6.53 | 30 | 5.30 | key lookup q/s |
| 9 | postgres | 297 | 6.60 | 18 | 3.00 | vector q/s, vector + filter q/s, similar items p50, similar items q/s, key lookup p50, key lookup p99 |

## Head to head: Strata, Elasticsearch, OpenSearch

### Places summed over 45 tests, on the median of the passes

| place | engine | points | average place | tests run | average place on those | first on |
|---|---|---|---|---|---|---|
| 1 | strata | 55 | 1.22 | 45 | 1.22 | bm25 p50, bm25 + filter p50, bm25 + filter p99, bm25 + filter q/s, phrase p50, phrase p99, phrase q/s, prefix p50, prefix p99, prefix q/s, boosted (bm25 × decay) p50, boosted (bm25 × decay) p99, boosted (bm25 × decay) q/s, facet p50, facet p99, facet q/s, vector p50, vector p99, vector q/s, vector + filter p50, vector + filter p99, vector + filter q/s, hybrid (RRF) p50, hybrid (RRF) p99, hybrid (RRF) q/s, hybrid (weighted) p50, hybrid (weighted) p99, hybrid (weighted) q/s, similar items p50, similar items p99, similar items q/s, key lookup p99, sql aggregate p50, sql aggregate p99, sql aggregate q/s, vector recall, similar items recall |
| 2 | es | 107 | 2.38 | 45 | 2.38 | typo p50, typo p99, typo q/s |
| 3 | os | 108 | 2.40 | 45 | 2.40 | bm25 p99, bm25 q/s, key lookup p50, key lookup q/s, vector + filter recall |
