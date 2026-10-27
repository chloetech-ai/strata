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
| 8-bit codes, HNSW ef 64 (default) | 0.990 | 0.103 | 0.149 |
| 8-bit codes, HNSW ef 320 | 0.996 | 0.330 | 0.474 |
| 8-bit codes, exact scan | 1.000 | 13.703 | 16.571 |
| 1-bit codes, HNSW (breadth 320) | 0.995 | 0.389 | 2.382 |
| 1-bit codes, exact scan | 1.000 | 22.623 | 23.450 |

| setup | kind | clients | per s | p50 ms | p99 ms | server p50 ms | errors |
|---|---|---|---|---|---|---|---|
| one database | bm25 | 1 | 4029 | 0.208 | 0.977 | 0.119 | 0 |
| one database | bm25 | 4 | 12472 | 0.259 | 1.358 | 0.178 | 0 |
| one database | bm25 | 16 | 17611 | 0.712 | 3.950 | 0.300 | 0 |
| one database | bm25 + filter | 1 | 2708 | 0.306 | 1.322 | 0.218 | 0 |
| one database | bm25 + filter | 4 | 8289 | 0.391 | 1.857 | 0.311 | 0 |
| one database | bm25 + filter | 16 | 12390 | 1.045 | 4.967 | 0.621 | 0 |
| one database | vector | 1 | 3966 | 0.249 | 0.376 | 0.126 | 0 |
| one database | vector | 4 | 10545 | 0.355 | 0.719 | 0.194 | 0 |
| one database | vector | 16 | 14102 | 0.949 | 4.469 | 0.284 | 0 |
| one database | vector + filter | 1 | 2887 | 0.338 | 0.505 | 0.201 | 0 |
| one database | vector + filter | 4 | 7230 | 0.525 | 1.004 | 0.339 | 0 |
| one database | vector + filter | 16 | 8510 | 1.565 | 7.881 | 0.667 | 0 |
| one database | hybrid (RRF) | 1 | 1672 | 0.544 | 1.449 | 0.403 | 0 |
| one database | hybrid (RRF) | 4 | 4007 | 0.899 | 2.486 | 0.670 | 0 |
| one database | hybrid (RRF) | 16 | 5519 | 2.214 | 14.875 | 1.184 | 0 |
| one database | hybrid (weighted) | 1 | 1575 | 0.581 | 1.518 | 0.438 | 0 |
| one database | hybrid (weighted) | 4 | 3791 | 0.955 | 2.614 | 0.724 | 0 |
| one database | hybrid (weighted) | 16 | 5311 | 2.375 | 13.572 | 1.359 | 0 |
| one database | boosted (bm25 × decay) | 1 | 2493 | 0.342 | 1.271 | 0.249 | 0 |
| one database | boosted (bm25 × decay) | 4 | 6951 | 0.487 | 1.881 | 0.374 | 0 |
| one database | boosted (bm25 × decay) | 16 | 11224 | 1.190 | 4.965 | 0.719 | 0 |
| one database | facet | 1 | 11738 | 0.074 | 0.233 | 0.038 | 0 |
| one database | facet | 4 | 23188 | 0.151 | 0.432 | 0.071 | 0 |
| one database | facet | 16 | 34130 | 0.426 | 1.357 | 0.111 | 0 |
| one database | key lookup | 1 | 15882 | 0.060 | 0.097 | 0.027 | 0 |
| one database | key lookup | 4 | 30960 | 0.121 | 0.255 | 0.047 | 0 |
| one database | key lookup | 16 | 43807 | 0.344 | 0.804 | 0.072 | 0 |
| 1-bit codes | vector | 1 | 1786 | 0.553 | 0.766 | 0.414 | 0 |
| 1-bit codes | vector | 4 | 4809 | 0.807 | 1.349 | 0.611 | 0 |
| 1-bit codes | vector | 16 | 6688 | 2.050 | 8.617 | 1.210 | 0 |
| 1-bit codes | vector + filter | 1 | 819 | 1.216 | 1.584 | 1.069 | 0 |
| 1-bit codes | vector + filter | 4 | 2021 | 1.941 | 3.081 | 1.724 | 0 |
| 1-bit codes | vector + filter | 16 | 2820 | 4.052 | 40.523 | 3.172 | 0 |
| 1-bit codes | hybrid (RRF) | 1 | 737 | 1.284 | 2.149 | 1.133 | 0 |
| 1-bit codes | hybrid (RRF) | 4 | 1988 | 1.932 | 3.423 | 1.716 | 0 |
| 1-bit codes | hybrid (RRF) | 16 | 2662 | 4.237 | 43.006 | 3.317 | 0 |
| 1-bit codes | hybrid (weighted) | 1 | 724 | 1.331 | 2.226 | 1.176 | 0 |
| 1-bit codes | hybrid (weighted) | 4 | 1838 | 2.071 | 3.681 | 1.849 | 0 |
| 1-bit codes | hybrid (weighted) | 16 | 2491 | 4.454 | 47.177 | 3.494 | 0 |
| 2 shards | bm25 | 1 | 2407 | 0.388 | 0.843 | 0.331 | 0 |
| 2 shards | bm25 | 4 | 4660 | 0.825 | 1.500 | 0.737 | 0 |
| 2 shards | bm25 | 16 | 3828 | 4.068 | 6.211 | 3.427 | 0 |
| 2 shards | bm25 + filter | 1 | 1261 | 0.747 | 1.443 | 0.658 | 0 |
| 2 shards | bm25 + filter | 4 | 2617 | 1.384 | 3.392 | 1.280 | 0 |
| 2 shards | bm25 + filter | 16 | 2255 | 7.024 | 10.342 | 5.946 | 0 |
| 2 shards | vector | 1 | 2667 | 0.361 | 0.621 | 0.255 | 0 |
| 2 shards | vector | 4 | 5484 | 0.708 | 1.173 | 0.536 | 0 |
| 2 shards | vector | 16 | 6141 | 2.524 | 4.497 | 1.855 | 0 |
| 2 shards | vector + filter | 1 | 1851 | 0.526 | 0.860 | 0.384 | 0 |
| 2 shards | vector + filter | 4 | 3011 | 1.308 | 2.029 | 1.006 | 0 |
| 2 shards | vector + filter | 16 | 2772 | 5.353 | 14.840 | 4.242 | 0 |
| 2 shards | hybrid (RRF) | 1 | 550 | 1.755 | 2.806 | 1.558 | 0 |
| 2 shards | hybrid (RRF) | 4 | 973 | 4.018 | 6.628 | 3.573 | 0 |
| 2 shards | hybrid (RRF) | 16 | 970 | 15.674 | 33.071 | 13.489 | 0 |
| 2 shards | hybrid (weighted) | 1 | 533 | 1.806 | 2.835 | 1.602 | 0 |
| 2 shards | hybrid (weighted) | 4 | 841 | 4.586 | 7.237 | 4.167 | 0 |
| 2 shards | hybrid (weighted) | 16 | 888 | 17.387 | 31.972 | 14.872 | 0 |
| 2 shards | boosted (bm25 × decay) | 1 | 725 | 1.308 | 2.292 | 1.184 | 0 |
| 2 shards | boosted (bm25 × decay) | 4 | 1190 | 3.336 | 4.873 | 3.129 | 0 |
| 2 shards | boosted (bm25 × decay) | 16 | 1380 | 11.412 | 17.162 | 9.715 | 0 |
| 2 shards | facet | 1 | 4119 | 0.229 | 0.493 | 0.181 | 0 |
| 2 shards | facet | 4 | 6964 | 0.558 | 0.950 | 0.443 | 0 |
| 2 shards | facet | 16 | 9480 | 1.649 | 2.719 | 1.200 | 0 |
| 2 shards | key lookup | 1 | 6139 | 0.152 | 0.323 | 0.108 | 0 |
| 2 shards | key lookup | 4 | 10320 | 0.375 | 0.640 | 0.279 | 0 |
| 2 shards | key lookup | 16 | 13861 | 1.133 | 1.831 | 0.799 | 0 |
| one database | in-place update (one per request) | 1 | 189 | 5.040 | 8.149 | 4.774 | 0 |
| one database | in-place update (one per request) | 4 | 198 | 5.086 | 214.941 | 4.838 | 0 |
| one database | in-place update (one per request) | 16 | 197 | 5.759 | 711.558 | 5.307 | 0 |
| one database | SQL UPDATE by key | 1 | 171 | 5.914 | 7.682 | 5.539 | 0 |
| one database | SQL UPDATE by key | 4 | 188 | 5.442 | 181.833 | 5.060 | 0 |
| one database | SQL UPDATE by key | 16 | 194 | 6.076 | 670.370 | 5.802 | 0 |
| one database | insert (100 documents per request) | 1 | 12 | 82.648 | 153.144 | 77.157 | 0 |

| kind | same top 10 as one database | same scores (ties may swap) | recall@10, one database | recall@10, shards |
|---|---|---|---|---|
| bm25 | 20 of 50 | 50 of 50 | exact | exact |
| bm25 + filter | 28 of 50 | 50 of 50 | exact | exact |
| vector | 44 of 50 | 46 of 50 | 0.990 | 0.992 |
| vector + filter | 39 of 50 | 40 of 50 | 0.982 | 0.990 |
| hybrid (RRF) | 20 of 50 | 20 of 50 | 0.972 | 0.942 |
| hybrid (weighted) | 45 of 50 | 45 of 50 | 0.998 | 0.998 |
| boosted (bm25 × decay) | 43 of 50 | 43 of 50 | exact | exact |
| facet | 50 of 50 | 50 of 50 | exact | exact |
| key lookup | 50 of 50 | 50 of 50 | exact | exact |

