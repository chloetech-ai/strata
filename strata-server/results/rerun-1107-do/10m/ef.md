## Recall@10 and latency per HNSW candidates value

Latency is the median of the passes: p50 and p99 at one client, qps at the most clients timed.

| engine | kind | candidates | recall@10 | p50 ms | p99 ms | qps | passes |
|---|---|---|---|---|---|---|---|
| Strata | vector | 64 | 0.791 | 2.93 | 4.89 | 1247 (16 clients) | 3 |
| Strata | vector | 128 | 0.840 | 3.38 | 5.69 | 957 (16 clients) | 3 |
| Strata | vector | 256 | 0.903 | 4.09 | 7.53 | 630 (16 clients) | 3 |
| Strata | vector + filter | 64 | 0.821 | 3.27 | 5.73 | 1137 (16 clients) | 3 |
| Strata | vector + filter | 128 | 0.865 | 3.69 | 6.47 | 832 (16 clients) | 3 |
| Strata | vector + filter | 256 | 0.916 | 4.52 | 7.86 | 555 (16 clients) | 3 |
| Strata | similar items | 64 | 0.980 | 4.00 | 6.53 | 753 (16 clients) | 3 |
| Strata | similar items | 128 | 0.981 | 4.15 | 7.39 | 684 (16 clients) | 3 |
| Strata | similar items | 256 | 0.990 | 4.87 | 8.37 | 505 (16 clients) | 3 |
| Strata | hybrid (RRF) | 64 | n/a | 8.64 | 34.43 | 239 (16 clients) | 3 |
| Strata | hybrid (RRF) | 128 | n/a | 8.82 | 34.61 | 233 (16 clients) | 3 |
| Strata | hybrid (RRF) | 256 | n/a | 9.64 | 36.66 | 209 (16 clients) | 3 |
| Strata | hybrid (weighted) | 64 | n/a | 8.71 | 35.14 | 241 (16 clients) | 3 |
| Strata | hybrid (weighted) | 128 | n/a | 9.03 | 36.90 | 234 (16 clients) | 3 |
| Strata | hybrid (weighted) | 256 | n/a | 9.65 | 34.39 | 212 (16 clients) | 3 |
