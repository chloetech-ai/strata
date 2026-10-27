## quora on Apple M2

522931 documents; raw text 32.5 MB, raw float32 vectors 2141.9 MB.

| database | docs | file MB | bytes/doc | vector codes MB | build s |
|---|---|---|---|---|---|
| 8-bit codes | 522931 | 2819.6 | 5392 | 535.5 | 200.5 |
| 1-bit codes | 522931 | 2351.1 | 4496 | 66.9 | 201.4 |
| shard 0 of 2 | 261563 | 1411.7 | 5397 | 267.8 | 103.5 |
| shard 1 of 2 | 261368 | 1410.6 | 5397 | 267.6 | 103.1 |

| component | 8-bit MB | 1-bit MB |
|---|---|---|
| doc store | 32.3 | 32.3 |
| pk | 15.8 | 15.8 |
| structured columns | 12.3 | 12.3 |
| text index (body) | 12.4 | 12.4 |
| vectors (f32) | 2141.9 | 2141.9 |
| vectors (hnsw) | 66.9 | 66.9 |
| vectors (hnsw_upper) | 2.4 | 2.4 |
| vectors (meta) | 0.0 | 0.0 |
| vectors (sq8) | 535.5 | 0.0 |
| vectors (sq8p) | 0.0 | 0.0 |

| vector search (SQL, one thread) | recall@10 | p50 ms | p99 ms |
|---|---|---|---|
| 8-bit codes, HNSW ef 64 (default) | 0.990 | 0.100 | 0.130 |
| 8-bit codes, HNSW ef 320 | 0.996 | 0.318 | 0.390 |
| 8-bit codes, exact scan | 1.000 | 9.945 | 10.556 |
| 1-bit codes, HNSW (breadth 320) | 0.995 | 0.293 | 0.447 |
| 1-bit codes, exact scan | 1.000 | 9.160 | 9.340 |

| setup | kind | clients | per s | p50 ms | p99 ms | server p50 ms | errors |
|---|---|---|---|---|---|---|---|
| one database | bm25 | 1 | 3233 | 0.247 | 1.292 | 0.154 | 0 |
| one database | bm25 | 4 | 12859 | 0.239 | 1.455 | 0.177 | 0 |
| one database | bm25 | 16 | 19642 | 0.634 | 3.403 | 0.288 | 0 |
| one database | bm25 + filter | 1 | 2120 | 0.364 | 1.917 | 0.272 | 0 |
| one database | bm25 + filter | 4 | 8183 | 0.376 | 2.032 | 0.293 | 0 |
| one database | bm25 + filter | 16 | 12652 | 1.043 | 4.412 | 0.695 | 0 |
| one database | vector | 1 | 4125 | 0.241 | 0.276 | 0.122 | 0 |
| one database | vector | 4 | 14793 | 0.266 | 0.359 | 0.154 | 0 |
| one database | vector | 16 | 18530 | 0.769 | 2.835 | 0.210 | 0 |
| one database | vector + filter | 1 | 2910 | 0.342 | 0.401 | 0.203 | 0 |
| one database | vector + filter | 4 | 10574 | 0.373 | 0.525 | 0.261 | 0 |
| one database | vector + filter | 16 | 12400 | 1.052 | 5.734 | 0.407 | 0 |
| one database | hybrid (RRF) | 1 | 1272 | 0.677 | 2.234 | 0.528 | 0 |
| one database | hybrid (RRF) | 4 | 4729 | 0.719 | 2.455 | 0.583 | 0 |
| one database | hybrid (RRF) | 16 | 6903 | 1.990 | 6.643 | 1.379 | 0 |
| one database | hybrid (weighted) | 1 | 1208 | 0.721 | 2.356 | 0.568 | 0 |
| one database | hybrid (weighted) | 4 | 4523 | 0.768 | 2.500 | 0.630 | 0 |
| one database | hybrid (weighted) | 16 | 6460 | 2.143 | 6.831 | 1.509 | 0 |
| one database | boosted (bm25 × decay) | 1 | 1730 | 0.462 | 2.096 | 0.363 | 0 |
| one database | boosted (bm25 × decay) | 4 | 6570 | 0.481 | 2.231 | 0.397 | 0 |
| one database | boosted (bm25 × decay) | 16 | 10056 | 1.348 | 5.034 | 0.957 | 0 |
| one database | facet | 1 | 10201 | 0.089 | 0.228 | 0.044 | 0 |
| one database | facet | 4 | 32499 | 0.112 | 0.281 | 0.052 | 0 |
| one database | facet | 16 | 46424 | 0.324 | 0.792 | 0.085 | 0 |
| one database | key lookup | 1 | 13192 | 0.075 | 0.090 | 0.033 | 0 |
| one database | key lookup | 4 | 40768 | 0.093 | 0.168 | 0.035 | 0 |
| one database | key lookup | 16 | 58630 | 0.265 | 0.484 | 0.058 | 0 |
| 1-bit codes | vector | 1 | 2152 | 0.465 | 0.521 | 0.324 | 0 |
| 1-bit codes | vector | 4 | 8309 | 0.478 | 0.589 | 0.370 | 0 |
| 1-bit codes | vector | 16 | 11304 | 1.112 | 6.518 | 0.504 | 0 |
| 1-bit codes | vector + filter | 1 | 1026 | 0.974 | 1.146 | 0.826 | 0 |
| 1-bit codes | vector + filter | 4 | 3863 | 1.033 | 1.315 | 0.901 | 0 |
| 1-bit codes | vector + filter | 16 | 5523 | 2.473 | 7.928 | 1.916 | 0 |
| 1-bit codes | hybrid (RRF) | 1 | 738 | 1.252 | 2.850 | 1.099 | 0 |
| 1-bit codes | hybrid (RRF) | 4 | 2754 | 1.341 | 3.051 | 1.199 | 0 |
| 1-bit codes | hybrid (RRF) | 16 | 3915 | 3.338 | 18.674 | 2.590 | 0 |
| 1-bit codes | hybrid (weighted) | 1 | 722 | 1.285 | 2.845 | 1.133 | 0 |
| 1-bit codes | hybrid (weighted) | 4 | 2617 | 1.412 | 3.154 | 1.269 | 0 |
| 1-bit codes | hybrid (weighted) | 16 | 3232 | 3.917 | 25.118 | 3.034 | 0 |
| 2 shards | bm25 | 1 | 2202 | 0.414 | 1.014 | 0.360 | 0 |
| 2 shards | bm25 | 4 | 4581 | 0.840 | 1.571 | 0.754 | 0 |
| 2 shards | bm25 | 16 | 4094 | 3.885 | 5.738 | 3.282 | 0 |
| 2 shards | bm25 + filter | 1 | 1146 | 0.812 | 1.683 | 0.726 | 0 |
| 2 shards | bm25 + filter | 4 | 2766 | 1.350 | 2.752 | 1.259 | 0 |
| 2 shards | bm25 + filter | 16 | 2382 | 6.633 | 9.776 | 5.637 | 0 |
| 2 shards | vector | 1 | 2858 | 0.348 | 0.396 | 0.245 | 0 |
| 2 shards | vector | 4 | 6397 | 0.622 | 0.873 | 0.474 | 0 |
| 2 shards | vector | 16 | 7602 | 2.064 | 3.448 | 1.524 | 0 |
| 2 shards | vector + filter | 1 | 1882 | 0.530 | 0.624 | 0.395 | 0 |
| 2 shards | vector + filter | 4 | 4601 | 0.867 | 1.221 | 0.702 | 0 |
| 2 shards | vector + filter | 16 | 5238 | 2.954 | 5.653 | 2.330 | 0 |
| 2 shards | hybrid (RRF) | 1 | 685 | 1.418 | 2.235 | 1.262 | 0 |
| 2 shards | hybrid (RRF) | 4 | 1777 | 2.208 | 3.390 | 2.031 | 0 |
| 2 shards | hybrid (RRF) | 16 | 1820 | 8.605 | 13.667 | 7.414 | 0 |
| 2 shards | hybrid (weighted) | 1 | 607 | 1.602 | 2.427 | 1.436 | 0 |
| 2 shards | hybrid (weighted) | 4 | 1384 | 2.857 | 4.111 | 2.634 | 0 |
| 2 shards | hybrid (weighted) | 16 | 1253 | 12.537 | 19.816 | 10.761 | 0 |
| 2 shards | boosted (bm25 × decay) | 1 | 717 | 1.335 | 2.354 | 1.224 | 0 |
| 2 shards | boosted (bm25 × decay) | 4 | 1190 | 3.344 | 5.079 | 3.169 | 0 |
| 2 shards | boosted (bm25 × decay) | 16 | 1414 | 11.164 | 16.605 | 9.545 | 0 |
| 2 shards | facet | 1 | 4711 | 0.207 | 0.353 | 0.163 | 0 |
| 2 shards | facet | 4 | 8146 | 0.482 | 0.738 | 0.384 | 0 |
| 2 shards | facet | 16 | 11345 | 1.397 | 2.077 | 1.011 | 0 |
| 2 shards | key lookup | 1 | 7521 | 0.131 | 0.160 | 0.094 | 0 |
| 2 shards | key lookup | 4 | 12748 | 0.308 | 0.461 | 0.230 | 0 |
| 2 shards | key lookup | 16 | 17010 | 0.933 | 1.356 | 0.658 | 0 |
| one database | in-place update (one per request) | 1 | 194 | 5.044 | 7.043 | 4.748 | 0 |
| one database | in-place update (one per request) | 4 | 411 | 9.915 | 11.867 | 9.485 | 0 |
| one database | in-place update (one per request) | 16 | 1657 | 9.729 | 12.286 | 9.026 | 0 |
| one database | SQL UPDATE by key | 1 | 167 | 5.992 | 7.595 | 5.598 | 0 |
| one database | SQL UPDATE by key | 4 | 182 | 21.991 | 25.362 | 21.486 | 0 |
| one database | SQL UPDATE by key | 16 | 181 | 88.350 | 99.764 | 87.850 | 0 |
| one database | insert (100 documents per request) | 1 | 69 | 14.541 | 17.765 | 9.605 | 0 |

| kind | same top 10 as one database | same scores (ties may swap) | recall@10, one database | recall@10, shards |
|---|---|---|---|---|
| bm25 | 49 of 50 | 50 of 50 | exact | exact |
| bm25 + filter | 49 of 50 | 50 of 50 | exact | exact |
| vector | 46 of 50 | 46 of 50 | 0.990 | 0.992 |
| vector + filter | 39 of 50 | 39 of 50 | 0.984 | 0.992 |
| hybrid (RRF) | 42 of 50 | 42 of 50 | 0.972 | 0.968 |
| hybrid (weighted) | 50 of 50 | 50 of 50 | 0.998 | 0.998 |
| boosted (bm25 × decay) | 50 of 50 | 50 of 50 | exact | exact |
| facet | 50 of 50 | 50 of 50 | exact | exact |
| key lookup | 50 of 50 | 50 of 50 | exact | exact |

