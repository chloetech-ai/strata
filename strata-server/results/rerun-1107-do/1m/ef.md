## Recall@10 and latency per HNSW candidates value

Latency is the median of the passes: p50 and p99 at one client, qps at the most clients timed.

| engine | kind | candidates | recall@10 | p50 ms | p99 ms | qps | passes |
|---|---|---|---|---|---|---|---|
| Qdrant | vector | 64 | 0.905 | 1.84 | 2.73 | 1416 (16 clients) | 3 |
| Qdrant | vector | 128 | 0.949 | 2.80 | 4.24 | 845 (16 clients) | 3 |
| Qdrant | vector | 256 | 0.972 | 4.49 | 6.96 | 475 (16 clients) | 3 |
| Qdrant | vector + filter | 64 | 0.871 | 3.31 | 4.94 | 734 (16 clients) | 3 |
| Qdrant | vector + filter | 128 | 0.945 | 5.23 | 7.94 | 432 (16 clients) | 3 |
| Qdrant | vector + filter | 256 | 0.981 | 9.05 | 13.58 | 243 (16 clients) | 3 |
| Qdrant | similar items | 64 | 0.975 | 8.42 | 12.71 | 305 (16 clients) | 3 |
| Qdrant | similar items | 128 | 0.990 | 14.10 | 21.15 | 173 (16 clients) | 3 |
| Qdrant | similar items | 256 | 0.996 | 23.09 | 34.34 | 100 (16 clients) | 3 |
| Qdrant | hybrid (RRF) | 64 | n/a | 8.17 | 14.35 | 321 (16 clients) | 3 |
| Qdrant | hybrid (RRF) | 128 | n/a | 10.19 | 13.25 | 252 (16 clients) | 3 |
| Qdrant | hybrid (RRF) | 256 | n/a | 9.62 | 13.32 | 253 (16 clients) | 3 |
| Qdrant | hybrid (weighted) | 64 | n/a | 8.34 | 14.51 | 321 (16 clients) | 3 |
| Qdrant | hybrid (weighted) | 128 | n/a | 10.26 | 13.35 | 253 (16 clients) | 3 |
| Qdrant | hybrid (weighted) | 256 | n/a | 9.83 | 13.27 | 251 (16 clients) | 3 |
| Strata | vector | 64 | 0.809 | 2.48 | 3.93 | 2442 (16 clients) | 3 |
| Strata | vector | 128 | 0.875 | 2.60 | 4.12 | 2081 (16 clients) | 3 |
| Strata | vector | 256 | 0.931 | 2.97 | 4.57 | 1569 (16 clients) | 3 |
| Strata | vector + filter | 64 | 0.861 | 2.50 | 3.80 | 2354 (16 clients) | 3 |
| Strata | vector + filter | 128 | 0.913 | 2.70 | 4.26 | 2010 (16 clients) | 3 |
| Strata | vector + filter | 256 | 0.952 | 3.04 | 4.75 | 1557 (16 clients) | 3 |
| Strata | similar items | 64 | 0.995 | 2.99 | 4.69 | 1711 (16 clients) | 3 |
| Strata | similar items | 128 | 0.997 | 3.14 | 4.73 | 1660 (16 clients) | 3 |
| Strata | similar items | 256 | 0.999 | 3.44 | 5.38 | 1354 (16 clients) | 3 |
| Strata | hybrid (RRF) | 64 | n/a | 5.26 | 11.05 | 734 (16 clients) | 3 |
| Strata | hybrid (RRF) | 128 | n/a | 5.23 | 10.97 | 722 (16 clients) | 3 |
| Strata | hybrid (RRF) | 256 | n/a | 5.61 | 11.64 | 650 (16 clients) | 3 |
| Strata | hybrid (weighted) | 64 | n/a | 5.18 | 11.06 | 731 (16 clients) | 3 |
| Strata | hybrid (weighted) | 128 | n/a | 5.28 | 11.34 | 724 (16 clients) | 3 |
| Strata | hybrid (weighted) | 256 | n/a | 5.58 | 11.14 | 659 (16 clients) | 3 |
