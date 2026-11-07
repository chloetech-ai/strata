## Load time and index size

| engine | load (s) | of which: send / build | searchable | merge to 1 segment | index size (GB) | documents |
|---|---|---|---|---|---|---|
| Strata | 7136 | load 6124 + merge 7188 | (built) | (built as one) | 50.29 | 10,000,000 |

## 1 client: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms |
|---|---|---|
| bm25 | 221 | 3.34 / 7.81 / 21.40 |
| bm25 + filter | 189 | 4.01 / 8.94 / 22.43 |
| phrase | 443 | 2.14 / 2.75 / 3.84 |
| typo | 22 | 44.86 / 69.73 / 93.15 |
| prefix | 151 | 5.24 / 12.49 / 23.74 |
| vector | 343 | 2.83 / 3.34 / 4.49 |
| vector + filter | 323 | 2.98 / 3.56 / 5.42 |
| hybrid (RRF) | 99 | 8.48 / 15.81 / 33.87 |
| hybrid (weighted) | 98 | 8.42 / 15.45 / 37.18 |
| boosted (bm25 × decay) | 93 | 8.55 / 19.40 / 38.49 |
| similar items | 254 | 3.84 / 4.54 / 6.00 |
| facet | 309 | 2.83 / 4.94 / 7.26 |
| key lookup | 954 | 1.02 / 1.26 / 1.75 |
| sql aggregate | 27 | 36.95 / 51.92 / 59.34 |


## 4 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms |
|---|---|---|
| bm25 | 486 | 5.86 / 15.35 / 44.87 |
| bm25 + filter | 428 | 6.79 / 17.35 / 45.19 |
| phrase | 1,343 | 2.80 / 3.94 / 5.70 |
| typo | 46 | 80.45 / 131.88 / 191.90 |
| prefix | 282 | 10.78 / 28.26 / 52.25 |
| vector | 870 | 4.44 / 5.80 / 7.70 |
| vector + filter | 779 | 4.96 / 6.55 / 8.47 |
| hybrid (RRF) | 216 | 15.08 / 29.55 / 77.85 |
| hybrid (weighted) | 210 | 15.30 / 30.81 / 78.88 |
| boosted (bm25 × decay) | 198 | 15.37 / 38.07 / 84.87 |
| similar items | 568 | 6.77 / 9.06 / 11.51 |
| facet | 874 | 4.04 / 7.27 / 11.31 |
| key lookup | 3,542 | 1.10 / 1.40 / 1.92 |
| sql aggregate | 39 | 103.83 / 149.07 / 165.17 |


## 16 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms |
|---|---|---|
| bm25 | 604 | 18.38 / 53.52 / 154.39 |
| bm25 + filter | 522 | 21.14 / 59.28 / 170.68 |
| phrase | 2,430 | 6.17 / 9.68 / 14.11 |
| typo | 46 | 327.75 / 539.85 / 834.42 |
| prefix | 312 | 37.29 / 107.23 / 197.21 |
| vector | 1,289 | 11.79 / 17.95 / 24.83 |
| vector + filter | 1,117 | 13.57 / 20.94 / 29.22 |
| hybrid (RRF) | 242 | 52.42 / 114.99 / 284.20 |
| hybrid (weighted) | 240 | 51.80 / 112.74 / 275.05 |
| boosted (bm25 × decay) | 219 | 51.17 / 143.77 / 389.81 |
| similar items | 782 | 19.62 / 29.82 / 40.32 |
| facet | 1,364 | 10.65 / 19.10 / 29.68 |
| key lookup | 9,159 | 1.66 / 2.36 / 3.74 |
| sql aggregate | 37 | 412.73 / 651.22 / 733.89 |


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
| vector | 0.794 | – |
| vector + filter | 0.819 | – |
| hybrid (RRF) | – | – |
| hybrid (weighted) | – | – |
| boosted (bm25 × decay) | – | – |
| similar items | 0.979 | – |


## SQL aggregates equal to the true answer

| Strata |
|---|
| 300 of 300 |

## Agreement with Strata's answers


## Writes: documents a second, and latency in ms (p50 / p99)

| phase | clients | Strata docs/s | Strata ms |
|---|---|---|---|
| insert one | 1 | 328 | 2.81 / 6.01 |
| insert one | 4 | 1,096 | 3.08 / 7.39 |
| insert one | 16 | 2,258 | 4.85 / 71.56 |
| insert batch | 1 | 1,639 | 38.52 / 232.01 |
| insert batch | 4 | 3,247 | 87.30 / 535.08 |
| insert batch | 16 | 4,659 | 262.63 / 967.52 |
| update one | 1 | 367 | 2.35 / 9.37 |
| update one | 4 | 1,774 | 2.18 / 4.35 |
| update one | 16 | 5,408 | 2.86 / 5.85 |

## How soon a written document is found (ms from the write's start)

| engine | searchable | write returned (p50) | found (p50) | found (p90) | found (max) |
|---|---|---|---|---|---|
| Strata | searchable when the write returns | 2.7 | 7.1 | 8.1 | 131.3 |

## Streams through Redpanda: documents written a second, and lag in ms from when a document was due (p50 / p99)

| phase | Strata docs/s | Strata lag |
|---|---|---|
| 250/s | 249 | 78 / 110 |
| 500/s | 499 | 78 / 242 |
| 1000/s | 996 | 79 / 240 |
| 2000/s | 1,978 | 92 / 640 |
| flat out | 4,157 | 2568 / 4047 |

Lag includes reading the topic (Redpanda's part, the same for every engine, is the 'read' column below).

| engine | what written means | read lag p50 (ms) | crash: events written again | documents lost |
|---|---|---|---|---|
| Strata | searchable (the server answers a batch once its rows are visible) | 35 | 0 (the consumer resumes after the offsets the server keeps with the rows (GET /v1/sources)) | 0 |

## CPU used by each engine's containers (cgroup cpu.stat)

| engine | phase | pod | CPU seconds | wall seconds | average cores | throttled (s) |
|---|---|---|---|---|---|---|
| Strata | timed run | strata-0 | 885 | 425 | 2.08 | 8 |
| Strata | timed run | strata-1 | 865 | 426 | 2.03 | 7 |
| Strata | writes | strata-0 | 81 | 70 | 1.16 | 1 |
| Strata | writes | strata-1 | 87 | 70 | 1.24 | 2 |
| Strata | stream | strata-0 | 125 | 150 | 0.83 | 2 |
| Strata | stream | strata-1 | 124 | 150 | 0.82 | 3 |

## Memory of each engine's containers (cgroup memory.current, memory.stat, memory.peak; GB, all its pods)

| engine | after the load | of it anonymous | after the timed run | of it anonymous | of it files (page cache) | pods' peaks added |
|---|---|---|---|---|---|---|
| Strata | – | – | 30.89 | 1.35 | 29.44 | 33.65 |
