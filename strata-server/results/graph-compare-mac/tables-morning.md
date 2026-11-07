## Load

| engine | nodes (s) | edges (s) | total (s) | memory after the queries | data on disk |
|---|---:|---:|---:|---:|---:|
| Strata (EDGES field) | – | – | 1.45 | 166.9MiB | 68 MiB |
| Strata (edge table) | 0.53 | 6.96 | 7.48 | 286.2MiB | 237 MiB |
| Neo4j | 3.34 | 18.0 | 21.3 (+ 1.1 s GDS projection) | 3.562GiB | 836 MiB |
| Memgraph | 0.34 | 7.18 | 7.52 | 783.6MiB | 387 MiB |
| FalkorDB | 0.32 | 27.5 | 27.8 | 251.1MiB | 0 MiB |
| ArangoDB | 0.73 | 17.8 | 18.5 | 684.9MiB | 537 MiB |

## Latency, client: p50 / p95 in ms

| query | Strata (EDGES field) | Strata (edge table) | Neo4j | Memgraph | FalkorDB | ArangoDB |
|---|---:|---:|---:|---:|---:|---:|
| 1 hop, both ways | 0.51 / 0.73 | 0.46 / 0.55 | 1.39 / 1.79 | 0.19 / 0.25 | 0.21 / 0.29 | 0.59 / 1.37 |
| 2 hops, both ways | 0.63 / 2.40 | 0.63 / 6.07 | 1.31 / 7.81 | 0.27 / 5.54 | 0.32 / 6.93 | 0.92 / 5.85 |
| 3 hops out | 0.49 / 0.77 | 0.56 / 1.51 | 0.97 / 1.49 | 0.21 / 0.51 | 0.21 / 0.76 | 0.49 / 1.47 |
| shortest path (≤ 10) | 0.64 / 1.52 | 1.77 / 7.28 | 1.75 / 8.05 | 1.15 / 33.2 | not run | 2.71 / 10.5 |
| PPR top 20, eps 1e-4 (GDS: defaults) | 1.77 / 2.50 | 3.65 / 5.55 | 143 / 245 | n/a | n/a | n/a |
| PPR top 20, eps 1e-5 (GDS: 100 iterations) | 10.8 / 14.4 | 23.0 / 41.6 | 557 / 652 | n/a | n/a | n/a |
| PPR top 20, eps 1e-6 | 65.0 / 91.3 | 96.8 / 138 | n/a | n/a | n/a | n/a |

## Latency, engine's own time: p50 / p95 in ms

| query | Strata (EDGES field) | Strata (edge table) | Neo4j | Memgraph | FalkorDB | ArangoDB |
|---|---:|---:|---:|---:|---:|---:|
| 1 hop, both ways | 0.10 / 0.14 | 0.11 / 0.13 | 1.00 / 1.00 | 0.02 / 0.03 | 0.07 / 0.10 | 0.08 / 0.21 |
| 2 hops, both ways | 0.16 / 1.74 | 0.21 / 5.57 | 1.00 / 7.00 | 0.07 / 4.95 | 0.13 / 6.71 | 0.31 / 4.91 |
| 3 hops out | 0.10 / 0.40 | 0.16 / 0.96 | 0.00 / 1.00 | 0.03 / 0.31 | 0.06 / 0.62 | 0.10 / 1.03 |
| shortest path (≤ 10) | 0.30 / 1.19 | 1.34 / 6.70 | 1.00 / 7.00 | 0.81 / 32.6 | not run | 2.23 / 9.93 |
| PPR top 20, eps 1e-4 (GDS: defaults) | 1.38 / 2.04 | 3.20 / 4.75 | 141 / 244 | n/a | n/a | n/a |
| PPR top 20, eps 1e-5 (GDS: 100 iterations) | 9.94 / 13.3 | 21.9 / 40.6 | 555 / 650 | n/a | n/a | n/a |
| PPR top 20, eps 1e-6 | 64.1 / 90.4 | 95.8 / 137 | n/a | n/a | n/a | n/a |

## Answers

| query | Strata (EDGES field) | Strata (edge table) | Neo4j | Memgraph | FalkorDB | ArangoDB |
|---|---:|---:|---:|---:|---:|---:|
| 1 hop, both ways | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right |
| 2 hops, both ways | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right |
| 3 hops out | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right | 200/200 right |
| shortest path (≤ 10) | 200/200 right | 200/200 right | 200/200 right | 200/200 right | – | 200/200 right |
| PPR top 20, eps 1e-4 (GDS: defaults) | recall@20 0.807 | recall@20 0.809 | recall@20 0.987 | – | – | – |
| PPR top 20, eps 1e-5 (GDS: 100 iterations) | recall@20 0.895 | recall@20 0.895 | recall@20 0.999 | – | – | – |
| PPR top 20, eps 1e-6 | recall@20 0.955 | recall@20 0.955 | – | – | – | – |

## Client p50 in each pass (pass3, pass2), ms

| query | Strata (EDGES field) | Strata (edge table) | Neo4j | Memgraph | FalkorDB | ArangoDB |
|---|---:|---:|---:|---:|---:|---:|
| 1 hop, both ways | 0.51 / 0.51 | 0.46 / 0.48 | 1.39 / 1.18 | 0.19 / 0.20 | 0.21 / 0.20 | 0.59 / 0.55 |
| 2 hops, both ways | 0.63 / 0.64 | 0.63 / 0.75 | 1.31 / 1.39 | 0.27 / 0.33 | 0.32 / 0.26 | 0.92 / 1.14 |
| 3 hops out | 0.49 / 0.51 | 0.56 / 0.54 | 0.97 / 0.98 | 0.21 / 0.23 | 0.21 / 0.19 | 0.49 / 0.65 |
| shortest path (≤ 10) | 0.64 / 0.72 | 1.77 / 1.70 | 1.75 / 1.68 | 1.15 / 1.12 | – / – | 2.71 / 5.03 |
| PPR top 20, eps 1e-4 (GDS: defaults) | 1.77 / 1.75 | 3.65 / 3.74 | 143 / 109 | – / – | – / – | – / – |
| PPR top 20, eps 1e-5 (GDS: 100 iterations) | 10.8 / 10.4 | 23.0 / 23.6 | 557 / 539 | – / – | – / – | – / – |
| PPR top 20, eps 1e-6 | 65.0 / 69.1 | 96.8 / 113 | – / – | – / – | – / – | – / – |

| load, total (s) | 1.45 / 1.28 | 7.48 / 7.64 | 21.3 / 19.5 | 7.52 / 8.05 | 27.8 / 54.0 | 18.5 / 25.2 |
