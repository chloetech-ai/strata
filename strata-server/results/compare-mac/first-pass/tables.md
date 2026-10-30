Strata's answers at both CPU splits are identical.

## Load time and index size

| engine | load (s) | of which: send / build | searchable | merge to 1 segment | index size (GB) | documents |
|---|---|---|---|---|---|---|
| Strata | 225 | read 4 + build 222 | (built) | (built as one) | 2.82 | 522,931 |
| Elasticsearch | 521 | send 360 | 1 | 159 | 2.79 | 522,931 |
| Solr | 1083 | send 484 | 1 | 597 | 2.24 | 522,879 |

## 1 client: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms | Strata, 0.5-CPU coordinator q/s | Strata, 0.5-CPU coordinator ms | Elasticsearch q/s | Elasticsearch ms | Solr q/s | Solr ms |
|---|---|---|---|---|---|---|---|---|
| bm25 | 1,466 | 0.65 / – / 1.20 | 1,476 | 0.64 / – / 1.37 | 285 | 3.07 / – / 10.66 | 248 | 3.26 / – / 17.14 |
| bm25 + filter | 1,069 | 0.88 / – / 1.71 | 1,105 | 0.85 / – / 1.82 | 546 | 1.57 / – / 6.97 | 604 | 1.33 / – / 7.05 |
| vector | 1,600 | 0.61 / – / 0.90 | 1,742 | 0.56 / – / 0.80 | 499 | 1.81 / – / 5.37 | 351 | 2.54 / – / 9.97 |
| vector + filter | 1,224 | 0.79 / – / 1.43 | 1,320 | 0.74 / – / 1.21 | 508 | 1.87 / – / 3.74 | 176 | 5.10 / – / 12.73 |
| hybrid (RRF) | 543 | 1.79 / – / 2.87 | 566 | 1.72 / – / 2.61 | n/a | n/a | 247 | 3.51 / – / 12.51 |
| hybrid (weighted) | 537 | 1.81 / – / 2.82 | 238 | 2.27 / – / 46.93 | 412 | 2.19 / – / 5.77 | 234 | 3.70 / – / 12.83 |
| boosted (bm25 × decay) | 795 | 1.20 / – / 2.15 | 795 | 1.20 / – / 2.34 | 582 | 1.49 / – / 4.01 | 518 | 1.60 / – / 6.24 |
| facet | 2,290 | 0.43 / – / 0.75 | 1,956 | 0.45 / – / 1.08 | 1,173 | 0.74 / – / 2.95 | 1,004 | 0.82 / – / 3.90 |
| key lookup | 3,358 | 0.29 / – / 0.41 | 2,594 | 0.33 / – / 0.63 | 1,430 | 0.67 / – / 1.35 | 1,010 | 0.87 / – / 2.76 |

## 4 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms | Strata, 0.5-CPU coordinator q/s | Strata, 0.5-CPU coordinator ms | Elasticsearch q/s | Elasticsearch ms | Solr q/s | Solr ms |
|---|---|---|---|---|---|---|---|---|
| bm25 | 3,112 | 1.15 / – / 5.61 | 2,430 | 1.11 / – / 27.00 | 651 | 4.52 / – / 30.02 | 577 | 5.07 / – / 31.01 |
| bm25 + filter | 1,809 | 1.88 / – / 15.18 | 2,028 | 1.68 / – / 12.96 | 1,251 | 2.42 / – / 22.68 | 1,551 | 2.22 / – / 8.89 |
| vector | 3,088 | 1.23 / – / 3.31 | 2,552 | 1.14 / – / 25.65 | 1,027 | 3.12 / – / 20.92 | 528 | 6.53 / – / 22.48 |
| vector + filter | 2,022 | 1.69 / – / 14.10 | 2,411 | 1.51 / – / 6.41 | 964 | 3.74 / – / 12.77 | 259 | 13.54 / – / 38.89 |
| hybrid (RRF) | 895 | 3.92 / – / 18.10 | 942 | 3.74 / – / 17.34 | n/a | n/a | 414 | 8.58 / – / 27.27 |
| hybrid (weighted) | 837 | 4.11 / – / 19.21 | 724 | 4.75 / – / 20.65 | 722 | 4.77 / – / 18.46 | 355 | 9.67 / – / 34.40 |
| boosted (bm25 × decay) | 1,374 | 2.62 / – / 12.35 | 1,210 | 2.64 / – / 22.78 | 992 | 3.58 / – / 12.94 | 894 | 3.89 / – / 13.61 |
| facet | 5,022 | 0.77 / – / 1.46 | 3,128 | 0.71 / – / 41.98 | 3,990 | 0.83 / – / 4.33 | 2,351 | 1.45 / – / 5.92 |
| key lookup | 7,174 | 0.52 / – / 1.04 | 4,672 | 0.47 / – / 1.51 | 4,600 | 0.77 / – / 2.64 | 2,219 | 1.48 / – / 7.40 |

## 16 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms | Strata, 0.5-CPU coordinator q/s | Strata, 0.5-CPU coordinator ms | Elasticsearch q/s | Elasticsearch ms | Solr q/s | Solr ms |
|---|---|---|---|---|---|---|---|---|
| bm25 | 3,081 | 4.03 / – / 28.58 | 3,188 | 3.73 / – / 36.69 | 1,176 | 9.96 / – / 51.29 | 1,063 | 11.80 / – / 47.00 |
| bm25 + filter | 1,815 | 6.56 / – / 36.77 | 2,488 | 6.00 / – / 16.68 | 1,692 | 6.92 / – / 46.87 | 1,880 | 7.23 / – / 29.85 |
| vector | 3,368 | 4.17 / – / 20.64 | 3,076 | 3.77 / – / 34.81 | 1,322 | 9.23 / – / 43.50 | 599 | 23.54 / – / 72.18 |
| vector + filter | 2,070 | 5.61 / – / 36.33 | 2,807 | 4.93 / – / 23.76 | 1,146 | 11.79 / – / 38.28 | 272 | 53.62 / – / 147.01 |
| hybrid (RRF) | 908 | 13.41 / – / 45.52 | 1,039 | 12.37 / – / 41.28 | n/a | n/a | 457 | 31.70 / – / 91.24 |
| hybrid (weighted) | 864 | 13.97 / – / 47.92 | 761 | 15.84 / – / 66.33 | 911 | 14.38 / – / 44.03 | 394 | 36.32 / – / 109.09 |
| boosted (bm25 × decay) | 1,404 | 9.08 / – / 36.89 | 1,262 | 9.36 / – / 43.25 | 1,139 | 11.24 / – / 40.25 | 992 | 13.78 / – / 43.22 |
| facet | 6,156 | 2.55 / – / 4.23 | 3,772 | 2.24 / – / 50.30 | 5,585 | 2.27 / – / 14.96 | 3,219 | 4.24 / – / 19.47 |
| key lookup | 9,685 | 1.62 / – / 2.89 | 6,214 | 1.32 / – / 49.86 | 7,695 | 1.78 / – / 10.44 | 3,141 | 4.39 / – / 19.72 |

Errors during the timed runs: Strata 0, Strata, 0.5-CPU coordinator 0, Elasticsearch 0, Solr 0

## Answers: recall@10 against an exact search, nDCG@10 against Quora's judgments

| kind | Strata recall | Strata nDCG | Elasticsearch recall | Elasticsearch nDCG | Solr recall | Solr nDCG |
|---|---|---|---|---|---|---|
| bm25 | – | 0.790 | – | 0.788 | – | 0.795 |
| bm25 + filter | – | 0.171 | – | 0.171 | – | 0.172 |
| vector | 0.986 | 0.892 | 0.984 | 0.887 | 0.994 | 0.892 |
| vector + filter | 0.991 | 0.180 | 0.938 | 0.176 | 0.998 | 0.180 |
| hybrid (RRF) | – | 0.863 | n/a | n/a | – | 0.873 |
| hybrid (weighted) | – | 0.905 | – | 0.828 | – | 0.824 |
| boosted (bm25 × decay) | – | 0.192 | – | 0.177 | – | 0.176 |

## Agreement with Strata's answers

| kind | Elasticsearch | Solr |
|---|---|---|
| bm25 | 0.897 of the top 10 | 0.903 of the top 10 |
| bm25 + filter | 0.910 of the top 10 | 0.907 of the top 10 |
| vector | 0.977 of the top 10 | 0.988 of the top 10 |
| vector + filter | 0.939 of the top 10 | 0.990 of the top 10 |
| hybrid (RRF) | n/a | 0.672 of the top 10 |
| hybrid (weighted) | 0.582 of the top 10 | 0.556 of the top 10 |
| boosted (bm25 × decay) | 0.653 of the top 10 | 0.652 of the top 10 |
| facet | 296 of 300 equal | 262 of 300 equal |
| key lookup | 300 of 300 equal | 300 of 300 equal |

## CPU used by each engine's containers (cgroup cpu.stat)

| engine | phase | pod | CPU seconds | wall seconds | average cores | throttled (s) |
|---|---|---|---|---|---|---|
| Strata, 0.5-CPU coordinator | timed run | strata-0 (coord500m) | 128 | 175 | 0.73 | 0 |
| Strata, 0.5-CPU coordinator | timed run | strata-1 (coord500m) | 127 | 175 | 0.73 | 0 |
| Strata, 0.5-CPU coordinator | timed run | strata-coordinator (coord500m) | 78 | 175 | 0.44 | 29 |
| Elasticsearch | load | es-0 | 715 | 554 | 1.29 | 146 |
| Elasticsearch | load | es-1 | 713 | 554 | 1.29 | 167 |
| Elasticsearch | timed run | es-0 | 202 | 163 | 1.24 | 19 |
| Elasticsearch | timed run | es-1 | 200 | 163 | 1.22 | 16 |
| Solr | load | solr-0 | 1347 | 1 | 1346.93 | 305 |
| Solr | load | solr-1 | 1336 | 1 | 1335.58 | 285 |
| Solr | timed run | solr-0 | 223 | 182 | 1.22 | 16 |
| Solr | timed run | solr-1 | 224 | 182 | 1.23 | 15 |

- Elasticsearch, hybrid (RRF): unavailable: {"license.expired.feature":"Reciprocal Rank Fusion (RRF)","reason":"current license is non-compliant for [Reciprocal Rank Fusion (RRF)]","root_cause":[{"license.expired.feature":"Reciprocal Rank Fusion (RRF)","reason":"current license is non-compliant for [Reciprocal Rank Fusion (RRF)]","type":"secu

- Elasticsearch, hybrid (RRF): the rrf retriever

- Elasticsearch, hybrid (weighted): no min-max normalised fusion on the free licence (the linear retriever needs a paid one): the sum of 0.2 x the raw BM25 score and 0.8 x the vector score ((1 + dot) / 2) over the lexical matches and the knn top 100

- Solr, hybrid (RRF): the combined query handler, rrf, k 60, knn topK 100; the lexical query's depth is the request's limit

- Solr, hybrid (weighted): no min-max normalised fusion: the sum of 0.2 x the raw BM25 score (scaled by k1 + 1 = 2.2 as Elasticsearch's) and 0.8 x the vector score ((1 + dot) / 2) over the lexical matches and the knn top 100
