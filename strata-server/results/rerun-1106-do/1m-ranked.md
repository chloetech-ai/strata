# 1M: every engine from its newest run, ranked

`rank.py --no-load` on a directory holding each engine's three passes from its newest run: Strata and Manticore from `1m/` here (6 November), Elasticsearch and OpenSearch from [`bigger-items-do/1m`](../bigger-items-do/1m) (5 November, result caches off), the other ten from [`compare-msmarco-same-nodes-do/1m`](../compare-msmarco-same-nodes-do/1m) (4 November). Made with this directory's `composite.py`:

```bash
python3 composite.py /tmp/c ../compare-msmarco-same-nodes-do/1m strata=1m manticore=1m es=../bigger-items-do/1m os=../bigger-items-do/1m
python3 ../../deploy/compare/rank.py --no-load /tmp/c
```

## Places summed over 45 tests, on the median of the passes

| place | engine | points | average place | tests run | average place on those | first on |
|---|---|---|---|---|---|---|
| 1 | strata | 137 | 3.04 | 45 | 3.04 | prefix p50, facet p99, hybrid (RRF) p50, hybrid (RRF) q/s, hybrid (weighted) p50, hybrid (weighted) q/s, sql aggregate p50, sql aggregate p99, sql aggregate q/s |
| 2 | solr | 288 | 6.40 | 45 | 6.40 | prefix p99, prefix q/s, vector + filter recall, similar items recall |
| 3 | es | 292 | 6.49 | 45 | 6.49 | – |
| 4 | qdrant | 300 | 6.67 | 36 | 4.83 | bm25 p50, bm25 p99, bm25 q/s, bm25 + filter p50, bm25 + filter p99, bm25 + filter q/s, boosted (bm25 × decay) p50, boosted (bm25 × decay) p99, boosted (bm25 × decay) q/s, vector recall |
| 5 | os | 305 | 6.78 | 45 | 6.78 | facet q/s |
| 6 | milvus | 307 | 6.82 | 33 | 4.21 | phrase p99, hybrid (RRF) p99, hybrid (weighted) p99 |
| 7 | redis | 348 | 7.73 | 45 | 7.73 | phrase q/s, vector p50, vector p99, vector q/s, vector + filter p50, vector + filter p99, vector + filter q/s, similar items p50, similar items p99, similar items q/s, key lookup p50, key lookup p99, key lookup q/s |
| 8 | manticore | 364 | 8.09 | 42 | 7.67 | facet p50 |
| 9 | meili | 370 | 8.22 | 39 | 7.33 | typo p99, typo q/s |
| 10 | vespa | 384 | 8.53 | 39 | 7.69 | – |
| 11 | typesense | 401 | 8.91 | 39 | 8.13 | phrase p50, typo p50 |
| 12 | weaviate | 423 | 9.40 | 36 | 8.25 | – |
| 13 | postgres | 430 | 9.56 | 42 | 9.24 | – |
| 14 | paradedb | 441 | 9.80 | 45 | 9.80 | – |
