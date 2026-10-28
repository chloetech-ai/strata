| engine | kind | clients | q/s | p50 | p90 | p95 | p99 | p99.9 | max | mean | errors |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Strata | bm25 | 1 | 1,466 | 0.65 | – | 0.91 | 1.20 | – | – | 0.68 | 0 |
| Strata | bm25 | 4 | 3,112 | 1.15 | – | 1.86 | 5.61 | – | – | 1.28 | 0 |
| Strata | bm25 | 16 | 3,081 | 4.03 | – | 18.01 | 28.58 | – | – | 5.19 | 0 |
| Strata | bm25 + filter | 1 | 1,069 | 0.88 | – | 1.29 | 1.71 | – | – | 0.94 | 0 |
| Strata | bm25 + filter | 4 | 1,809 | 1.88 | – | 3.08 | 15.18 | – | – | 2.21 | 0 |
| Strata | bm25 + filter | 16 | 1,815 | 6.56 | – | 32.31 | 36.77 | – | – | 8.80 | 0 |
| Strata | vector | 1 | 1,600 | 0.61 | – | 0.71 | 0.90 | – | – | 0.62 | 0 |
| Strata | vector | 4 | 3,088 | 1.23 | – | 1.83 | 3.31 | – | – | 1.30 | 0 |
| Strata | vector | 16 | 3,368 | 4.17 | – | 7.38 | 20.64 | – | – | 4.74 | 0 |
| Strata | vector + filter | 1 | 1,224 | 0.79 | – | 0.97 | 1.43 | – | – | 0.82 | 0 |
| Strata | vector + filter | 4 | 2,022 | 1.69 | – | 2.74 | 14.10 | – | – | 1.98 | 0 |
| Strata | vector + filter | 16 | 2,070 | 5.61 | – | 30.57 | 36.33 | – | – | 7.73 | 0 |
| Strata | hybrid (RRF) | 1 | 543 | 1.79 | – | 2.26 | 2.87 | – | – | 1.84 | 0 |
| Strata | hybrid (RRF) | 4 | 895 | 3.92 | – | 6.88 | 18.10 | – | – | 4.47 | 0 |
| Strata | hybrid (RRF) | 16 | 908 | 13.41 | – | 41.44 | 45.52 | – | – | 17.60 | 0 |
| Strata | hybrid (weighted) | 1 | 537 | 1.81 | – | 2.29 | 2.82 | – | – | 1.86 | 0 |
| Strata | hybrid (weighted) | 4 | 837 | 4.11 | – | 7.73 | 19.21 | – | – | 4.78 | 0 |
| Strata | hybrid (weighted) | 16 | 864 | 13.97 | – | 43.69 | 47.92 | – | – | 18.49 | 0 |
| Strata | boosted (bm25 × decay) | 1 | 795 | 1.20 | – | 1.64 | 2.15 | – | – | 1.26 | 0 |
| Strata | boosted (bm25 × decay) | 4 | 1,374 | 2.62 | – | 4.17 | 12.35 | – | – | 2.91 | 0 |
| Strata | boosted (bm25 × decay) | 16 | 1,404 | 9.08 | – | 31.34 | 36.89 | – | – | 11.39 | 0 |
| Strata | facet | 1 | 2,290 | 0.43 | – | 0.52 | 0.75 | – | – | 0.44 | 0 |
| Strata | facet | 4 | 5,022 | 0.77 | – | 1.13 | 1.46 | – | – | 0.80 | 0 |
| Strata | facet | 16 | 6,156 | 2.55 | – | 3.56 | 4.23 | – | – | 2.60 | 0 |
| Strata | key lookup | 1 | 3,358 | 0.29 | – | 0.34 | 0.41 | – | – | 0.30 | 0 |
| Strata | key lookup | 4 | 7,174 | 0.52 | – | 0.76 | 1.04 | – | – | 0.56 | 0 |
| Strata | key lookup | 16 | 9,685 | 1.62 | – | 2.29 | 2.89 | – | – | 1.65 | 0 |
| Strata, 0.5-CPU coordinator | bm25 | 1 | 1,476 | 0.64 | – | 0.92 | 1.37 | – | – | 0.68 | 0 |
| Strata, 0.5-CPU coordinator | bm25 | 4 | 2,430 | 1.11 | – | 2.00 | 27.00 | – | – | 1.65 | 0 |
| Strata, 0.5-CPU coordinator | bm25 | 16 | 3,188 | 3.73 | – | 15.72 | 36.69 | – | – | 5.02 | 0 |
| Strata, 0.5-CPU coordinator | bm25 + filter | 1 | 1,105 | 0.85 | – | 1.26 | 1.82 | – | – | 0.90 | 0 |
| Strata, 0.5-CPU coordinator | bm25 + filter | 4 | 2,028 | 1.68 | – | 2.84 | 12.96 | – | – | 1.97 | 0 |
| Strata, 0.5-CPU coordinator | bm25 + filter | 16 | 2,488 | 6.00 | – | 10.53 | 16.68 | – | – | 6.43 | 0 |
| Strata, 0.5-CPU coordinator | vector | 1 | 1,742 | 0.56 | – | 0.63 | 0.80 | – | – | 0.57 | 0 |
| Strata, 0.5-CPU coordinator | vector | 4 | 2,552 | 1.14 | – | 1.77 | 25.65 | – | – | 1.57 | 0 |
| Strata, 0.5-CPU coordinator | vector | 16 | 3,076 | 3.77 | – | 24.62 | 34.81 | – | – | 5.20 | 0 |
| Strata, 0.5-CPU coordinator | vector + filter | 1 | 1,320 | 0.74 | – | 0.86 | 1.21 | – | – | 0.76 | 0 |
| Strata, 0.5-CPU coordinator | vector + filter | 4 | 2,411 | 1.51 | – | 2.40 | 6.41 | – | – | 1.66 | 0 |
| Strata, 0.5-CPU coordinator | vector + filter | 16 | 2,807 | 4.93 | – | 15.09 | 23.76 | – | – | 5.70 | 0 |
| Strata, 0.5-CPU coordinator | hybrid (RRF) | 1 | 566 | 1.72 | – | 2.16 | 2.61 | – | – | 1.77 | 0 |
| Strata, 0.5-CPU coordinator | hybrid (RRF) | 4 | 942 | 3.74 | – | 6.46 | 17.34 | – | – | 4.24 | 0 |
| Strata, 0.5-CPU coordinator | hybrid (RRF) | 16 | 1,039 | 12.37 | – | 36.84 | 41.28 | – | – | 15.39 | 0 |
| Strata, 0.5-CPU coordinator | hybrid (weighted) | 1 | 238 | 2.27 | – | 8.56 | 46.93 | – | – | 4.21 | 0 |
| Strata, 0.5-CPU coordinator | hybrid (weighted) | 4 | 724 | 4.75 | – | 12.04 | 20.65 | – | – | 5.53 | 0 |
| Strata, 0.5-CPU coordinator | hybrid (weighted) | 16 | 761 | 15.84 | – | 44.82 | 66.33 | – | – | 21.00 | 0 |
| Strata, 0.5-CPU coordinator | boosted (bm25 × decay) | 1 | 795 | 1.20 | – | 1.65 | 2.34 | – | – | 1.26 | 0 |
| Strata, 0.5-CPU coordinator | boosted (bm25 × decay) | 4 | 1,210 | 2.64 | – | 4.51 | 22.78 | – | – | 3.31 | 0 |
| Strata, 0.5-CPU coordinator | boosted (bm25 × decay) | 16 | 1,262 | 9.36 | – | 38.38 | 43.25 | – | – | 12.67 | 0 |
| Strata, 0.5-CPU coordinator | facet | 1 | 1,956 | 0.45 | – | 0.55 | 1.08 | – | – | 0.51 | 0 |
| Strata, 0.5-CPU coordinator | facet | 4 | 3,128 | 0.71 | – | 1.10 | 41.98 | – | – | 1.28 | 0 |
| Strata, 0.5-CPU coordinator | facet | 16 | 3,772 | 2.24 | – | 3.94 | 50.30 | – | – | 4.24 | 0 |
| Strata, 0.5-CPU coordinator | key lookup | 1 | 2,594 | 0.33 | – | 0.39 | 0.63 | – | – | 0.39 | 0 |
| Strata, 0.5-CPU coordinator | key lookup | 4 | 4,672 | 0.47 | – | 0.70 | 1.51 | – | – | 0.86 | 0 |
| Strata, 0.5-CPU coordinator | key lookup | 16 | 6,214 | 1.32 | – | 2.07 | 49.86 | – | – | 2.57 | 0 |
| Elasticsearch | bm25 | 1 | 285 | 3.07 | – | 6.42 | 10.66 | – | – | 3.51 | 0 |
| Elasticsearch | bm25 | 4 | 651 | 4.52 | – | 18.49 | 30.02 | – | – | 6.14 | 0 |
| Elasticsearch | bm25 | 16 | 1,176 | 9.96 | – | 39.61 | 51.29 | – | – | 13.60 | 0 |
| Elasticsearch | bm25 + filter | 1 | 546 | 1.57 | – | 3.00 | 6.97 | – | – | 1.83 | 0 |
| Elasticsearch | bm25 + filter | 4 | 1,251 | 2.42 | – | 6.75 | 22.68 | – | – | 3.20 | 0 |
| Elasticsearch | bm25 + filter | 16 | 1,692 | 6.92 | – | 27.98 | 46.87 | – | – | 9.45 | 0 |
| Elasticsearch | vector | 1 | 499 | 1.81 | – | 2.83 | 5.37 | – | – | 2.00 | 0 |
| Elasticsearch | vector | 4 | 1,027 | 3.12 | – | 7.31 | 20.92 | – | – | 3.89 | 0 |
| Elasticsearch | vector | 16 | 1,322 | 9.23 | – | 31.63 | 43.50 | – | – | 12.10 | 0 |
| Elasticsearch | vector + filter | 1 | 508 | 1.87 | – | 2.52 | 3.74 | – | – | 1.97 | 0 |
| Elasticsearch | vector + filter | 4 | 964 | 3.74 | – | 6.83 | 12.77 | – | – | 4.15 | 0 |
| Elasticsearch | vector + filter | 16 | 1,146 | 11.79 | – | 30.42 | 38.28 | – | – | 13.95 | 0 |
| Elasticsearch | hybrid (weighted) | 1 | 412 | 2.19 | – | 3.50 | 5.77 | – | – | 2.43 | 0 |
| Elasticsearch | hybrid (weighted) | 4 | 722 | 4.77 | – | 10.41 | 18.46 | – | – | 5.54 | 0 |
| Elasticsearch | hybrid (weighted) | 16 | 911 | 14.38 | – | 37.58 | 44.03 | – | – | 17.56 | 0 |
| Elasticsearch | boosted (bm25 × decay) | 1 | 582 | 1.49 | – | 3.17 | 4.01 | – | – | 1.72 | 0 |
| Elasticsearch | boosted (bm25 × decay) | 4 | 992 | 3.58 | – | 7.99 | 12.94 | – | – | 4.03 | 0 |
| Elasticsearch | boosted (bm25 × decay) | 16 | 1,139 | 11.24 | – | 34.41 | 40.25 | – | – | 14.04 | 0 |
| Elasticsearch | facet | 1 | 1,173 | 0.74 | – | 1.38 | 2.95 | – | – | 0.85 | 0 |
| Elasticsearch | facet | 4 | 3,990 | 0.83 | – | 1.58 | 4.33 | – | – | 1.00 | 0 |
| Elasticsearch | facet | 16 | 5,585 | 2.27 | – | 6.46 | 14.96 | – | – | 2.86 | 0 |
| Elasticsearch | key lookup | 1 | 1,430 | 0.67 | – | 0.91 | 1.35 | – | – | 0.70 | 0 |
| Elasticsearch | key lookup | 4 | 4,600 | 0.77 | – | 1.27 | 2.64 | – | – | 0.87 | 0 |
| Elasticsearch | key lookup | 16 | 7,695 | 1.78 | – | 3.67 | 10.44 | – | – | 2.08 | 0 |
| Solr | bm25 | 1 | 248 | 3.26 | – | 8.44 | 17.14 | – | – | 4.04 | 0 |
| Solr | bm25 | 4 | 577 | 5.07 | – | 20.04 | 31.01 | – | – | 6.93 | 0 |
| Solr | bm25 | 16 | 1,063 | 11.80 | – | 36.25 | 47.00 | – | – | 15.05 | 0 |
| Solr | bm25 + filter | 1 | 604 | 1.33 | – | 2.94 | 7.05 | – | – | 1.65 | 0 |
| Solr | bm25 + filter | 4 | 1,551 | 2.22 | – | 4.83 | 8.89 | – | – | 2.58 | 0 |
| Solr | bm25 + filter | 16 | 1,880 | 7.23 | – | 18.15 | 29.85 | – | – | 8.51 | 0 |
| Solr | vector | 1 | 351 | 2.54 | – | 4.09 | 9.97 | – | – | 2.85 | 0 |
| Solr | vector | 4 | 528 | 6.53 | – | 15.47 | 22.48 | – | – | 7.57 | 0 |
| Solr | vector | 16 | 599 | 23.54 | – | 54.07 | 72.18 | – | – | 26.70 | 0 |
| Solr | vector + filter | 1 | 176 | 5.10 | – | 9.03 | 12.73 | – | – | 5.69 | 0 |
| Solr | vector + filter | 4 | 259 | 13.54 | – | 30.12 | 38.89 | – | – | 15.43 | 0 |
| Solr | vector + filter | 16 | 272 | 53.62 | – | 107.03 | 147.01 | – | – | 58.74 | 0 |
| Solr | hybrid (RRF) | 1 | 247 | 3.51 | – | 7.07 | 12.51 | – | – | 4.05 | 0 |
| Solr | hybrid (RRF) | 4 | 414 | 8.58 | – | 18.86 | 27.27 | – | – | 9.66 | 0 |
| Solr | hybrid (RRF) | 16 | 457 | 31.70 | – | 68.69 | 91.24 | – | – | 34.96 | 0 |
| Solr | hybrid (weighted) | 1 | 234 | 3.70 | – | 7.79 | 12.83 | – | – | 4.28 | 0 |
| Solr | hybrid (weighted) | 4 | 355 | 9.67 | – | 22.67 | 34.40 | – | – | 11.25 | 0 |
| Solr | hybrid (weighted) | 16 | 394 | 36.32 | – | 82.02 | 109.09 | – | – | 40.56 | 0 |
| Solr | boosted (bm25 × decay) | 1 | 518 | 1.60 | – | 3.93 | 6.24 | – | – | 1.93 | 0 |
| Solr | boosted (bm25 × decay) | 4 | 894 | 3.89 | – | 9.14 | 13.61 | – | – | 4.47 | 0 |
| Solr | boosted (bm25 × decay) | 16 | 992 | 13.78 | – | 34.05 | 43.22 | – | – | 16.11 | 0 |
| Solr | facet | 1 | 1,004 | 0.82 | – | 1.86 | 3.90 | – | – | 1.00 | 0 |
| Solr | facet | 4 | 2,351 | 1.45 | – | 3.15 | 5.92 | – | – | 1.70 | 0 |
| Solr | facet | 16 | 3,219 | 4.24 | – | 10.31 | 19.47 | – | – | 4.97 | 0 |
| Solr | key lookup | 1 | 1,010 | 0.87 | – | 1.47 | 2.76 | – | – | 0.99 | 0 |
| Solr | key lookup | 4 | 2,219 | 1.48 | – | 3.29 | 7.40 | – | – | 1.80 | 0 |
| Solr | key lookup | 16 | 3,141 | 4.39 | – | 10.28 | 19.72 | – | – | 5.09 | 0 |
