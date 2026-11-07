## Load time and index size

| engine | load (s) | of which: send / build | searchable | merge to 1 segment | index size (GB) | documents |
|---|---|---|---|---|---|---|
| Strata | 6944 | load 6497 + merge 5838 | (built) | (built as one) | 49.71 | 10,000,000 |

## 1 client: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms |
|---|---|---|
| bm25 | 221 | 3.41 / 7.87 / 21.01 |
| bm25 + filter | 193 | 4.00 / 8.92 / 23.06 |
| phrase | 426 | 2.22 / 2.91 / 4.28 |
| typo | 21 | 45.10 / 75.50 / 92.05 |
| prefix | 150 | 5.35 / 12.26 / 21.18 |
| vector | 332 | 2.90 / 3.58 / 4.74 |
| vector + filter | 315 | 3.07 / 3.73 / 4.81 |
| hybrid (RRF) | 99 | 8.47 / 15.64 / 32.04 |
| hybrid (weighted) | 97 | 8.71 / 15.51 / 40.77 |
| boosted (bm25 × decay) | 94 | 8.89 / 19.11 / 37.66 |
| similar items | 241 | 4.01 / 4.95 / 6.26 |
| facet | 295 | 3.02 / 5.18 / 7.41 |
| key lookup | 1,034 | 0.99 / 1.31 / 1.78 |
| sql aggregate | 28 | 35.49 / 50.02 / 56.14 |


## 4 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms |
|---|---|---|
| bm25 | 506 | 5.63 / 14.94 / 43.39 |
| bm25 + filter | 437 | 6.49 / 17.06 / 45.26 |
| phrase | 1,377 | 2.69 / 3.95 / 5.85 |
| typo | 46 | 83.06 / 132.49 / 194.12 |
| prefix | 280 | 10.74 / 28.67 / 55.79 |
| vector | 857 | 4.50 / 5.93 / 7.70 |
| vector + filter | 783 | 4.92 / 6.57 / 9.08 |
| hybrid (RRF) | 217 | 14.93 / 30.50 / 77.38 |
| hybrid (weighted) | 210 | 15.11 / 30.81 / 84.75 |
| boosted (bm25 × decay) | 208 | 14.52 / 36.32 / 75.76 |
| similar items | 558 | 6.87 / 9.20 / 12.24 |
| facet | 898 | 4.01 / 7.02 / 10.92 |
| key lookup | 4,277 | 0.98 / 1.25 / 1.65 |
| sql aggregate | 41 | 95.36 / 141.11 / 163.55 |


## 16 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms |
|---|---|---|
| bm25 | 635 | 17.58 / 48.63 / 146.13 |
| bm25 + filter | 553 | 19.83 / 58.30 / 158.26 |
| phrase | 2,583 | 5.78 / 8.93 / 13.01 |
| typo | 47 | 319.01 / 573.13 / 801.21 |
| prefix | 342 | 32.45 / 100.59 / 190.71 |
| vector | 1,293 | 11.87 / 17.39 / 23.14 |
| vector + filter | 1,151 | 13.22 / 20.03 / 27.99 |
| hybrid (RRF) | 242 | 51.99 / 111.57 / 303.97 |
| hybrid (weighted) | 239 | 53.87 / 112.67 / 291.67 |
| boosted (bm25 × decay) | 228 | 49.43 / 146.13 / 340.07 |
| similar items | 762 | 20.29 / 29.81 / 39.99 |
| facet | 1,472 | 10.01 / 17.30 / 25.83 |
| key lookup | 11,993 | 1.32 / 1.99 / 2.94 |
| sql aggregate | 40 | 395.85 / 598.47 / 772.92 |


Errors during the timed runs: Strata 0

## Answers: recall@10 against an exact search, nDCG@10 against the collection's judgments

Unavailable kinds are n/a; recall is for the kinds an exact search can answer.

| kind | Strata recall | Strata nDCG |
|---|---|---|
| bm25 | – | – |
| bm25 + filter | – | – |
| phrase | – | – |
| typo | – | – |
| prefix | – | – |
| vector | 0.791 | – |
| vector + filter | 0.821 | – |
| hybrid (RRF) | – | – |
| hybrid (weighted) | – | – |
| boosted (bm25 × decay) | – | – |
| similar items | 0.980 | – |


## SQL aggregates equal to the true answer

| Strata |
|---|
| 300 of 300 |

## Agreement with Strata's answers


## Writes: documents a second, and latency in ms (p50 / p99)

| phase | clients | Strata docs/s | Strata ms |
|---|---|---|---|
| insert one | 1 | 289 | 3.24 / 6.80 |
| insert one | 4 | 1,025 | 3.31 / 6.83 |
| insert one | 16 | 2,332 | 4.69 / 64.44 |
| insert batch | 1 | 1,795 | 36.44 / 179.56 |
| insert batch | 4 | 3,307 | 74.97 / 514.74 |
| insert batch | 16 | 4,461 | 228.12 / 1498.79 |
| update one | 1 | 316 | 2.58 / 9.53 |
| update one | 4 | 1,477 | 2.60 / 5.09 |
| update one | 16 | 5,001 | 3.04 / 5.78 |

## How soon a written document is found (ms from the write's start)

| engine | searchable | write returned (p50) | found (p50) | found (p90) | found (max) |
|---|---|---|---|---|---|
| Strata | searchable when the write returns | 3.3 | 7.6 | 8.6 | 139.0 |

## Streams through Redpanda: documents written a second, and lag in ms from when a document was due (p50 / p99)

| phase | Strata docs/s | Strata lag |
|---|---|---|
| 250/s | 249 | 78 / 126 |
| 500/s | 498 | 78 / 152 |
| 1000/s | 997 | 79 / 195 |
| 2000/s | 1,994 | 91 / 555 |
| flat out | 4,087 | 2542 / 4207 |

Lag includes reading the topic (Redpanda's part, the same for every engine, is the 'read' column below).

| engine | what written means | read lag p50 (ms) | crash: events written again | documents lost |
|---|---|---|---|---|
| Strata | searchable (the server answers a batch once its rows are visible) | 34 | 0 (the consumer resumes after the offsets the server keeps with the rows (GET /v1/sources)) | 0 |

## CPU used by each engine's containers (cgroup cpu.stat)

| engine | phase | pod | CPU seconds | wall seconds | average cores | throttled (s) |
|---|---|---|---|---|---|---|
| Strata | timed run | strata-0 | 834 | 422 | 1.98 | 4 |
| Strata | timed run | strata-1 | 880 | 421 | 2.09 | 17 |
| Strata | writes | strata-0 | 70 | 69 | 1.02 | 1 |
| Strata | writes | strata-1 | 78 | 69 | 1.13 | 2 |
| Strata | stream | strata-0 | 117 | 149 | 0.78 | 2 |
| Strata | stream | strata-1 | 131 | 149 | 0.88 | 4 |

## Memory of each engine's containers (cgroup memory.current, memory.stat, memory.peak; GB, all its pods)

| engine | after the load | of it anonymous | after the timed run | of it anonymous | of it files (page cache) | pods' peaks added |
|---|---|---|---|---|---|---|
| Strata | – | – | 31.51 | 1.39 | 30.02 | 34.57 |
