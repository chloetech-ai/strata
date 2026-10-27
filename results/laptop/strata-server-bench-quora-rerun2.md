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
| 8-bit codes, HNSW ef 64 (default) | 0.990 | 0.104 | 0.161 |
| 8-bit codes, HNSW ef 320 | 0.996 | 0.328 | 0.454 |
| 8-bit codes, exact scan | 1.000 | 10.885 | 11.423 |
| 1-bit codes, HNSW (breadth 320) | 0.995 | 0.305 | 0.394 |
| 1-bit codes, exact scan | 1.000 | 9.391 | 9.618 |

| setup | kind | clients | per s | p50 ms | p99 ms | server p50 ms | errors |
|---|---|---|---|---|---|---|---|
| one database | bm25 | 1 | 3985 | 0.210 | 1.005 | 0.123 | 0 |
| one database | bm25 | 4 | 15814 | 0.205 | 1.107 | 0.141 | 0 |
| one database | bm25 | 16 | 23395 | 0.541 | 2.954 | 0.213 | 0 |
| one database | bm25 + filter | 1 | 2695 | 0.307 | 1.330 | 0.222 | 0 |
| one database | bm25 + filter | 4 | 10521 | 0.308 | 1.487 | 0.247 | 0 |
| one database | bm25 + filter | 16 | 15430 | 0.775 | 4.780 | 0.391 | 0 |
| one database | vector | 1 | 4058 | 0.243 | 0.328 | 0.125 | 0 |
| one database | vector | 4 | 13347 | 0.284 | 0.525 | 0.163 | 0 |
| one database | vector | 16 | 17197 | 0.816 | 3.171 | 0.231 | 0 |
| one database | vector + filter | 1 | 2719 | 0.358 | 0.479 | 0.212 | 0 |
| one database | vector + filter | 4 | 9462 | 0.399 | 0.743 | 0.277 | 0 |
| one database | vector + filter | 16 | 11436 | 1.139 | 6.252 | 0.453 | 0 |
| one database | hybrid (RRF) | 1 | 1652 | 0.551 | 1.413 | 0.411 | 0 |
| one database | hybrid (RRF) | 4 | 5900 | 0.622 | 1.551 | 0.462 | 0 |
| one database | hybrid (RRF) | 16 | 8429 | 1.611 | 6.217 | 1.039 | 0 |
| one database | hybrid (weighted) | 1 | 1548 | 0.593 | 1.499 | 0.452 | 0 |
| one database | hybrid (weighted) | 4 | 5440 | 0.678 | 1.664 | 0.517 | 0 |
| one database | hybrid (weighted) | 16 | 7641 | 1.799 | 6.661 | 1.186 | 0 |
| one database | boosted (bm25 × decay) | 1 | 2517 | 0.344 | 1.249 | 0.255 | 0 |
| one database | boosted (bm25 × decay) | 4 | 9744 | 0.348 | 1.381 | 0.286 | 0 |
| one database | boosted (bm25 × decay) | 16 | 14012 | 0.884 | 5.199 | 0.473 | 0 |
| one database | facet | 1 | 10043 | 0.089 | 0.235 | 0.045 | 0 |
| one database | facet | 4 | 31968 | 0.112 | 0.302 | 0.053 | 0 |
| one database | facet | 16 | 44743 | 0.332 | 0.935 | 0.088 | 0 |
| one database | key lookup | 1 | 12968 | 0.076 | 0.108 | 0.033 | 0 |
| one database | key lookup | 4 | 40431 | 0.093 | 0.188 | 0.035 | 0 |
| one database | key lookup | 16 | 55422 | 0.277 | 0.571 | 0.059 | 0 |
| 1-bit codes | vector | 1 | 2114 | 0.467 | 0.598 | 0.333 | 0 |
| 1-bit codes | vector | 4 | 6965 | 0.523 | 1.111 | 0.397 | 0 |
| 1-bit codes | vector | 16 | 10112 | 1.331 | 5.780 | 0.815 | 0 |
| 1-bit codes | vector + filter | 1 | 933 | 1.030 | 1.945 | 0.881 | 0 |
| 1-bit codes | vector + filter | 4 | 3480 | 1.094 | 2.060 | 0.947 | 0 |
| 1-bit codes | vector + filter | 16 | 4508 | 2.877 | 14.209 | 2.115 | 0 |
| 1-bit codes | hybrid (RRF) | 1 | 811 | 1.156 | 2.384 | 1.002 | 0 |
| 1-bit codes | hybrid (RRF) | 4 | 2601 | 1.409 | 3.237 | 1.211 | 0 |
| 1-bit codes | hybrid (RRF) | 16 | 3440 | 3.479 | 27.696 | 2.565 | 0 |
| 1-bit codes | hybrid (weighted) | 1 | 778 | 1.183 | 2.682 | 1.035 | 0 |
| 1-bit codes | hybrid (weighted) | 4 | 2478 | 1.525 | 3.119 | 1.341 | 0 |
| 1-bit codes | hybrid (weighted) | 16 | 3351 | 3.771 | 23.913 | 2.811 | 0 |
| 2 shards | bm25 | 1 | 2437 | 0.386 | 0.789 | 0.331 | 0 |
| 2 shards | bm25 | 4 | 4711 | 0.823 | 1.444 | 0.735 | 0 |
| 2 shards | bm25 | 16 | 3972 | 3.976 | 5.884 | 3.356 | 0 |
| 2 shards | bm25 + filter | 1 | 1291 | 0.741 | 1.317 | 0.653 | 0 |
| 2 shards | bm25 + filter | 4 | 2615 | 1.413 | 2.854 | 1.308 | 0 |
| 2 shards | bm25 + filter | 16 | 2319 | 6.813 | 10.042 | 5.765 | 0 |
| 2 shards | vector | 1 | 2752 | 0.355 | 0.562 | 0.248 | 0 |
| 2 shards | vector | 4 | 5896 | 0.665 | 1.030 | 0.507 | 0 |
| 2 shards | vector | 16 | 6948 | 2.236 | 4.004 | 1.661 | 0 |
| 2 shards | vector + filter | 1 | 1879 | 0.527 | 0.703 | 0.394 | 0 |
| 2 shards | vector + filter | 4 | 4206 | 0.938 | 1.418 | 0.759 | 0 |
| 2 shards | vector + filter | 16 | 4815 | 3.185 | 6.558 | 2.512 | 0 |
| 2 shards | hybrid (RRF) | 1 | 740 | 1.322 | 1.914 | 1.170 | 0 |
| 2 shards | hybrid (RRF) | 4 | 1670 | 2.270 | 5.330 | 2.069 | 0 |
| 2 shards | hybrid (RRF) | 16 | 1717 | 9.038 | 16.262 | 7.753 | 0 |
| 2 shards | hybrid (weighted) | 1 | 641 | 1.532 | 2.146 | 1.367 | 0 |
| 2 shards | hybrid (weighted) | 4 | 1359 | 2.875 | 4.464 | 2.651 | 0 |
| 2 shards | hybrid (weighted) | 16 | 1306 | 11.589 | 26.772 | 9.926 | 0 |
| 2 shards | boosted (bm25 × decay) | 1 | 841 | 1.144 | 1.892 | 1.036 | 0 |
| 2 shards | boosted (bm25 × decay) | 4 | 1325 | 2.915 | 4.731 | 2.750 | 0 |
| 2 shards | boosted (bm25 × decay) | 16 | 1439 | 10.831 | 16.412 | 9.237 | 0 |
| 2 shards | facet | 1 | 4456 | 0.218 | 0.396 | 0.171 | 0 |
| 2 shards | facet | 4 | 7085 | 0.538 | 0.935 | 0.426 | 0 |
| 2 shards | facet | 16 | 10854 | 1.458 | 2.196 | 1.055 | 0 |
| 2 shards | key lookup | 1 | 7270 | 0.136 | 0.164 | 0.097 | 0 |
| 2 shards | key lookup | 4 | 12213 | 0.322 | 0.476 | 0.241 | 0 |
| 2 shards | key lookup | 16 | 16320 | 0.973 | 1.407 | 0.687 | 0 |
| one database | in-place update (one per request) | 1 | 195 | 5.073 | 7.022 | 4.761 | 0 |
| one database | in-place update (one per request) | 4 | 406 | 9.842 | 12.370 | 9.447 | 0 |
| one database | in-place update (one per request) | 16 | 1591 | 9.982 | 12.486 | 9.306 | 0 |
| one database | SQL UPDATE by key | 1 | 162 | 6.078 | 8.221 | 5.656 | 0 |
| one database | SQL UPDATE by key | 4 | 181 | 21.923 | 27.609 | 21.419 | 0 |
| one database | SQL UPDATE by key | 16 | 182 | 87.831 | 96.152 | 87.324 | 0 |
| one database | insert (100 documents per request) | 1 | 69 | 14.927 | 17.203 | 9.851 | 0 |

| kind | same top 10 as one database | same scores (ties may swap) | recall@10, one database | recall@10, shards |
|---|---|---|---|---|
| bm25 | 50 of 50 | 50 of 50 | exact | exact |
| bm25 + filter | 50 of 50 | 50 of 50 | exact | exact |
| vector | 46 of 50 | 46 of 50 | 0.990 | 0.992 |
| vector + filter | 39 of 50 | 39 of 50 | 0.984 | 0.992 |
| hybrid (RRF) | 42 of 50 | 42 of 50 | 0.972 | 0.968 |
| hybrid (weighted) | 50 of 50 | 50 of 50 | 0.998 | 0.998 |
| boosted (bm25 × decay) | 50 of 50 | 50 of 50 | exact | exact |
| facet | 50 of 50 | 50 of 50 | exact | exact |
| key lookup | 50 of 50 | 50 of 50 | exact | exact |

