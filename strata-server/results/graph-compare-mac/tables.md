## Load

| engine | nodes (s) | edges (s) | total (s) | memory after the queries | data on disk |
|---|---:|---:|---:|---:|---:|
| Strata (EDGES field) | – | – | 1.06 | 159MiB | 67 MiB |
| Strata (EDGES field, gRPC) | – | – | 1.27 | 159.9MiB | 67 MiB |
| Strata (edge table) | 0.52 | 6.19 | 6.71 | 219.1MiB | 234 MiB |
| Strata (edge table, gRPC) | 0.49 | 5.73 | 6.22 | 202.2MiB | 239 MiB |
| Neo4j | 2.14 | 14.0 | 16.1 (+ 1.0 s GDS projection) | 3.563GiB | 836 MiB |
| Memgraph | 0.32 | 6.64 | 6.96 | 800.6MiB | 387 MiB |
| FalkorDB | 0.28 | 26.7 | 26.9 | 318.6MiB | 0 MiB |
| ArangoDB | 0.67 | 12.9 | 13.6 | 681.3MiB | 537 MiB |

## Latency, client: p50 / p95 in ms

| query | Strata (EDGES field) | Strata (EDGES field, gRPC) | Strata (edge table) | Strata (edge table, gRPC) | Neo4j | Memgraph | FalkorDB | ArangoDB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 hop, both ways | 0.46 / 0.54 | 0.35 / 0.40 | 0.44 / 0.52 | 0.37 / 0.40 | 1.00 / 1.22 | 0.18 / 0.22 | 0.20 / 0.22 | 0.36 / 0.59 |
| 2 hops, both ways | 0.52 / 1.90 | 0.41 / 1.95 | 0.56 / 5.55 | 0.47 / 4.93 | 0.94 / 6.01 | 0.24 / 4.54 | 0.23 / 5.24 | 0.55 / 4.68 |
| 3 hops out | 0.48 / 0.85 | 0.38 / 0.75 | 0.48 / 1.02 | 0.40 / 0.98 | 0.78 / 1.28 | 0.19 / 0.44 | 0.18 / 0.72 | 0.41 / 1.12 |
| shortest path (≤ 10) | 0.57 / 1.29 | 0.52 / 1.26 | 1.51 / 5.76 | 1.41 / 5.96 | 1.33 / 6.11 | 0.90 / 24.8 | not run | 2.15 / 8.46 |
| PPR top 20, eps 1e-4 (GDS: defaults) | 1.47 / 1.94 | 1.42 / 1.91 | 3.01 / 4.15 | 3.05 / 4.20 | 93.1 / 141 | n/a | n/a | n/a |
| PPR top 20, eps 1e-5 (GDS: 100 iterations) | 7.70 / 10.2 | 7.80 / 10.7 | 17.3 / 23.0 | 17.0 / 24.0 | 429 / 460 | n/a | n/a | n/a |
| PPR top 20, eps 1e-6 | 50.9 / 73.0 | 52.6 / 75.8 | 76.4 / 108 | 77.2 / 108 | n/a | n/a | n/a | n/a |

## Latency, engine's own time: p50 / p95 in ms

| query | Strata (EDGES field) | Strata (EDGES field, gRPC) | Strata (edge table) | Strata (edge table, gRPC) | Neo4j | Memgraph | FalkorDB | ArangoDB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 hop, both ways | 0.09 / 0.11 | 0.10 / 0.12 | 0.11 / 0.12 | 0.12 / 0.14 | 0.00 / 1.00 | 0.02 / 0.03 | 0.07 / 0.09 | 0.06 / 0.08 |
| 2 hops, both ways | 0.14 / 1.50 | 0.15 / 1.60 | 0.19 / 5.16 | 0.20 / 4.65 | 1.00 / 6.00 | 0.06 / 4.04 | 0.09 / 5.09 | 0.19 / 4.02 |
| 3 hops out | 0.10 / 0.45 | 0.11 / 0.47 | 0.13 / 0.66 | 0.15 / 0.71 | 0.00 / 1.00 | 0.03 / 0.26 | 0.05 / 0.57 | 0.07 / 0.75 |
| shortest path (≤ 10) | 0.26 / 0.98 | 0.27 / 1.00 | 1.18 / 5.39 | 1.16 / 5.66 | 1.00 / 6.00 | 0.65 / 24.3 | not run | 1.72 / 8.07 |
| PPR top 20, eps 1e-4 (GDS: defaults) | 1.14 / 1.58 | 1.16 / 1.64 | 2.64 / 3.75 | 2.78 / 3.88 | 91.0 / 140 | n/a | n/a | n/a |
| PPR top 20, eps 1e-5 (GDS: 100 iterations) | 7.24 / 9.80 | 7.49 / 10.2 | 16.8 / 22.4 | 16.5 / 23.5 | 427 / 459 | n/a | n/a | n/a |
| PPR top 20, eps 1e-6 | 50.2 / 72.4 | 52.0 / 75.3 | 75.7 / 107 | 76.7 / 107 | n/a | n/a | n/a | n/a |

## Answers

| query | Strata (EDGES field) | Strata (EDGES field, gRPC) | Strata (edge table) | Strata (edge table, gRPC) | Neo4j | Memgraph | FalkorDB | ArangoDB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 hop, both ways | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right |
| 2 hops, both ways | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right |
| 3 hops out | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right |
| shortest path (≤ 10) | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | – | 200/200 right |
| PPR top 20, eps 1e-4 (GDS: defaults) | recall@20 0.807 | recall@20 0.807 | recall@20 0.809 | recall@20 0.809 | recall@20 0.988 | – | – | – |
| PPR top 20, eps 1e-5 (GDS: 100 iterations) | recall@20 0.895 | recall@20 0.895 | recall@20 0.895 | recall@20 0.895 | recall@20 0.999 | – | – | – |
| PPR top 20, eps 1e-6 | recall@20 0.955 | recall@20 0.955 | recall@20 0.955 | recall@20 0.955 | – | – | – | – |

## Client p50 in each pass (pass5, pass4), ms

| query | Strata (EDGES field) | Strata (EDGES field, gRPC) | Strata (edge table) | Strata (edge table, gRPC) | Neo4j | Memgraph | FalkorDB | ArangoDB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 hop, both ways | 0.46 / 0.46 | 0.35 / 0.35 | 0.44 / 0.45 | 0.37 / 0.36 | 1.00 / 1.02 | 0.18 / 0.19 | 0.20 / 0.20 | 0.36 / 0.37 |
| 2 hops, both ways | 0.52 / 0.52 | 0.41 / 0.40 | 0.56 / 0.56 | 0.47 / 0.47 | 0.94 / 0.90 | 0.24 / 0.23 | 0.23 / 0.22 | 0.55 / 0.55 |
| 3 hops out | 0.48 / 0.50 | 0.38 / 0.40 | 0.48 / 0.50 | 0.40 / 0.40 | 0.78 / 0.75 | 0.19 / 0.20 | 0.18 / 0.19 | 0.41 / 0.40 |
| shortest path (≤ 10) | 0.57 / 0.56 | 0.52 / 0.54 | 1.51 / 1.42 | 1.41 / 1.44 | 1.33 / 1.30 | 0.90 / 0.89 | – / – | 2.15 / 2.23 |
| PPR top 20, eps 1e-4 (GDS: defaults) | 1.47 / 1.44 | 1.42 / 1.39 | 3.01 / 3.03 | 3.05 / 2.99 | 93.1 / 97.1 | – / – | – / – | – / – |
| PPR top 20, eps 1e-5 (GDS: 100 iterations) | 7.70 / 7.52 | 7.80 / 7.82 | 17.3 / 17.1 | 17.0 / 17.1 | 429 / 446 | – / – | – / – | – / – |
| PPR top 20, eps 1e-6 | 50.9 / 52.4 | 52.6 / 49.2 | 76.4 / 75.2 | 77.2 / 75.6 | – / – | – / – | – / – | – / – |

| load, total (s) | 1.06 / 1.05 | 1.27 / 1.03 | 6.71 / 6.92 | 6.22 / 6.67 | 16.1 / 16.1 | 6.96 / 6.85 | 26.9 / 27.0 | 13.6 / 13.5 |
