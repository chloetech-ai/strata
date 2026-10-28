Strata's answers at both CPU splits are identical.

## Load time and index size

| engine | load (s) | of which: send / build | searchable | merge to 1 segment | index size (GB) | documents |
|---|---|---|---|---|---|---|
| Strata | 225 | read 4 + build 222 | (built) | (built as one) | 2.82 | 522,931 |
| Elasticsearch | 549 | send 446 | 1 | 103 | 2.78 | 522,931 |
| Solr | 1087 | send 513 | 1 | 572 | 2.24 | 522,931 |

## 1 client: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms | Strata, 0.5-CPU coordinator q/s | Strata, 0.5-CPU coordinator ms | Elasticsearch q/s | Elasticsearch ms | Solr q/s | Solr ms |
|---|---|---|---|---|---|---|---|---|
| bm25 | 1,354 | 0.68 / 0.89 / 1.77 | 1,404 | 0.66 / 0.83 / 1.37 | 266 | 3.20 / 5.56 / 12.83 | 261 | 3.08 / 5.52 / 18.36 |
| bm25 + filter | 1,086 | 0.87 / 1.12 / 1.68 | 1,056 | 0.90 / 1.15 / 1.76 | 541 | 1.57 / 2.54 / 7.17 | 574 | 1.36 / 2.43 / 9.45 |
| vector | 1,683 | 0.58 / 0.63 / 0.88 | 1,629 | 0.60 / 0.67 / 0.95 | 391 | 2.12 / 3.62 / 9.39 | 355 | 2.54 / 3.41 / 7.48 |
| vector + filter | 1,306 | 0.75 / 0.82 / 1.23 | 1,204 | 0.78 / 0.90 / 1.58 | 506 | 1.86 / 2.39 / 4.21 | 186 | 4.92 / 7.01 / 12.80 |
| hybrid (RRF) | 536 | 1.82 / 2.09 / 2.65 | 549 | 1.76 / 2.06 / 2.83 | n/a | n/a | 263 | 3.32 / 5.23 / 11.29 |
| hybrid (weighted) | 529 | 1.83 / 2.18 / 2.94 | 547 | 1.77 / 2.09 / 2.88 | 362 | 2.42 / 3.77 / 8.07 | 261 | 3.47 / 4.84 / 10.01 |
| boosted (bm25 × decay) | 807 | 1.19 / 1.47 / 2.02 | 811 | 1.18 / 1.44 / 2.24 | 516 | 1.59 / 3.06 / 6.07 | 527 | 1.55 / 3.27 / 6.41 |
| facet | 2,397 | 0.41 / 0.46 / 0.73 | 2,066 | 0.43 / 0.48 / 0.86 | 1,001 | 0.82 / 1.44 / 3.73 | 973 | 0.78 / 1.51 / 5.04 |
| key lookup | 2,714 | 0.32 / 0.41 / 1.23 | 2,666 | 0.32 / 0.36 / 0.54 | 1,479 | 0.65 / 0.79 / 1.19 | 1,154 | 0.81 / 0.97 / 1.98 |

## 4 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms | Strata, 0.5-CPU coordinator q/s | Strata, 0.5-CPU coordinator ms | Elasticsearch q/s | Elasticsearch ms | Solr q/s | Solr ms |
|---|---|---|---|---|---|---|---|---|
| bm25 | 2,887 | 1.21 / 1.78 / 6.63 | 2,398 | 1.16 / 1.67 / 27.21 | 614 | 4.83 / 10.29 / 31.87 | 592 | 4.94 / 11.62 / 31.65 |
| bm25 + filter | 2,008 | 1.67 / 2.38 / 15.33 | 1,955 | 1.71 / 2.48 / 15.21 | 1,139 | 2.57 / 5.02 / 24.26 | 1,515 | 2.25 / 3.99 / 8.63 |
| vector | 3,172 | 1.22 / 1.63 / 2.33 | 2,330 | 1.26 / 1.79 / 23.93 | 968 | 3.29 / 5.54 / 23.36 | 530 | 6.51 / 12.10 / 22.83 |
| vector + filter | 1,947 | 1.80 / 2.54 / 12.06 | 2,236 | 1.64 / 2.29 / 6.79 | 1,098 | 3.36 / 4.72 / 10.44 | 243 | 14.71 / 27.40 / 45.60 |
| hybrid (RRF) | 774 | 4.51 / 6.40 / 19.09 | 885 | 3.83 / 5.41 / 20.22 | n/a | n/a | 399 | 8.77 / 16.24 / 28.57 |
| hybrid (weighted) | 735 | 4.77 / 6.84 / 19.70 | 860 | 4.02 / 5.55 / 19.59 | 597 | 5.35 / 10.35 / 29.16 | 315 | 10.61 / 21.64 / 40.02 |
| boosted (bm25 × decay) | 1,280 | 2.82 / 3.93 / 11.71 | 1,313 | 2.49 / 3.43 / 20.52 | 954 | 3.66 / 7.08 / 13.26 | 936 | 3.80 / 7.35 / 12.61 |
| facet | 5,558 | 0.70 / 0.92 / 1.24 | 3,229 | 0.68 / 0.92 / 41.57 | 3,630 | 0.85 / 1.49 / 6.01 | 2,512 | 1.31 / 2.39 / 5.90 |
| key lookup | 8,379 | 0.46 / 0.60 / 0.85 | 4,862 | 0.44 / 0.59 / 1.38 | 4,769 | 0.74 / 1.07 / 2.45 | 2,348 | 1.46 / 2.33 / 6.74 |

## 16 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms | Strata, 0.5-CPU coordinator q/s | Strata, 0.5-CPU coordinator ms | Elasticsearch q/s | Elasticsearch ms | Solr q/s | Solr ms |
|---|---|---|---|---|---|---|---|---|
| bm25 | 3,058 | 4.06 / 5.82 / 28.89 | 2,636 | 4.24 / 6.78 / 39.48 | 1,162 | 10.03 / 33.00 / 48.02 | 1,114 | 11.03 / 29.04 / 48.40 |
| bm25 + filter | 2,003 | 5.92 / 9.04 / 36.40 | 2,231 | 6.65 / 9.31 / 19.77 | 1,440 | 8.20 / 22.53 / 48.73 | 1,886 | 7.13 / 14.74 / 30.90 |
| vector | 3,366 | 4.45 / 6.07 / 14.88 | 2,704 | 4.30 / 6.52 / 34.58 | 1,256 | 9.44 / 26.20 / 47.96 | 584 | 24.38 / 47.76 / 71.45 |
| vector + filter | 2,087 | 5.94 / 9.44 / 33.82 | 2,519 | 5.54 / 8.25 / 23.13 | 1,148 | 11.51 / 27.19 / 37.29 | 281 | 51.24 / 92.62 / 148.12 |
| hybrid (RRF) | 849 | 14.45 / 40.05 / 47.33 | 943 | 13.45 / 35.65 / 43.42 | n/a | n/a | 469 | 31.54 / 56.50 / 87.40 |
| hybrid (weighted) | 759 | 15.80 / 42.54 / 50.69 | 922 | 14.01 / 35.59 / 43.18 | 707 | 18.45 / 40.10 / 60.26 | 419 | 35.40 / 63.20 / 97.20 |
| boosted (bm25 × decay) | 1,492 | 8.47 / 23.61 / 35.91 | 1,497 | 8.44 / 23.91 / 37.81 | 1,087 | 12.16 / 28.56 / 39.75 | 1,030 | 13.56 / 27.57 / 42.25 |
| facet | 6,158 | 2.50 / 3.39 / 4.76 | 3,927 | 2.14 / 2.95 / 50.64 | 6,272 | 2.09 / 3.71 / 14.38 | 3,180 | 4.17 / 8.58 / 20.64 |
| key lookup | 11,158 | 1.40 / 1.83 / 2.59 | 6,613 | 1.24 / 1.63 / 50.09 | 8,137 | 1.67 / 2.90 / 10.22 | 3,259 | 4.17 / 7.68 / 20.75 |

Errors during the timed runs: Strata 0, Strata, 0.5-CPU coordinator 0, Elasticsearch 0, Solr 0

## Answers: recall@10 against an exact search, nDCG@10 against Quora's judgments

| kind | Strata recall | Strata nDCG | Elasticsearch recall | Elasticsearch nDCG | Solr recall | Solr nDCG |
|---|---|---|---|---|---|---|
| bm25 | – | 0.790 | – | 0.788 | – | 0.793 |
| bm25 + filter | – | 0.171 | – | 0.172 | – | 0.172 |
| vector | 0.986 | 0.892 | 0.984 | 0.889 | 0.993 | 0.892 |
| vector + filter | 0.991 | 0.180 | 0.934 | 0.178 | 0.998 | 0.180 |
| hybrid (RRF) | – | 0.863 | n/a | n/a | – | 0.871 |
| hybrid (weighted) | – | 0.905 | – | 0.827 | – | 0.823 |
| boosted (bm25 × decay) | – | 0.192 | – | 0.177 | – | 0.176 |

## Agreement with Strata's answers

| kind | Elasticsearch | Solr |
|---|---|---|
| bm25 | 0.899 of the top 10 | 0.903 of the top 10 |
| bm25 + filter | 0.910 of the top 10 | 0.909 of the top 10 |
| vector | 0.980 of the top 10 | 0.986 of the top 10 |
| vector + filter | 0.935 of the top 10 | 0.991 of the top 10 |
| hybrid (RRF) | n/a | 0.669 of the top 10 |
| hybrid (weighted) | 0.580 of the top 10 | 0.555 of the top 10 |
| boosted (bm25 × decay) | 0.653 of the top 10 | 0.653 of the top 10 |
| facet | 300 of 300 equal | 291 of 300 equal |
| key lookup | 300 of 300 equal | 300 of 300 equal |

## CPU used by each engine's containers (cgroup cpu.stat)

| engine | phase | pod | CPU seconds | wall seconds | average cores | throttled (s) |
|---|---|---|---|---|---|---|
| Strata | timed run | strata-0 | 139 | 169 | 0.82 | 23 |
| Strata | timed run | strata-1 | 139 | 169 | 0.82 | 24 |
| Strata | timed run | strata-coordinator | 93 | 170 | 0.54 | 0 |
| Strata, 0.5-CPU coordinator | timed run | strata-0 (coord500m) | 127 | 184 | 0.69 | 0 |
| Strata, 0.5-CPU coordinator | timed run | strata-1 (coord500m) | 128 | 184 | 0.70 | 0 |
| Strata, 0.5-CPU coordinator | timed run | strata-coordinator (coord500m) | 79 | 184 | 0.43 | 29 |
| Elasticsearch | load | es-0 | 715 | 554 | 1.29 | 146 |
| Elasticsearch | load | es-1 | 713 | 554 | 1.29 | 167 |
| Elasticsearch | timed run | es-0 | 202 | 163 | 1.24 | 19 |
| Elasticsearch | timed run | es-1 | 200 | 163 | 1.22 | 16 |
| Solr | load | solr-0 | 1342 | 1094 | 1.23 | 386 |
| Solr | load | solr-1 | 1367 | 1094 | 1.25 | 349 |
| Solr | timed run | solr-0 | 224 | 183 | 1.22 | 20 |
| Solr | timed run | solr-1 | 219 | 182 | 1.20 | 12 |

- Elasticsearch, hybrid (RRF): unavailable: {"license.expired.feature":"Reciprocal Rank Fusion (RRF)","reason":"current license is non-compliant for [Reciprocal Rank Fusion (RRF)]","root_cause":[{"license.expired.feature":"Reciprocal Rank Fusion (RRF)","reason":"current license is non-compliant for [Reciprocal Rank Fusion (RRF)]","type":"secu

- Elasticsearch, hybrid (RRF): the rrf retriever

- Elasticsearch, hybrid (weighted): no min-max normalised fusion on the free licence (the linear retriever needs a paid one): the sum of 0.2 x the raw BM25 score and 0.8 x the vector score ((1 + dot) / 2) over the lexical matches and the knn top 100

- Solr, hybrid (RRF): the combined query handler, rrf, k 60, knn topK 100; the lexical query's depth is the request's limit

- Solr, hybrid (weighted): no min-max normalised fusion: the sum of 0.2 x the raw BM25 score (scaled by k1 + 1 = 2.2 as Elasticsearch's) and 0.8 x the vector score ((1 + dot) / 2) over the lexical matches and the knn top 100
