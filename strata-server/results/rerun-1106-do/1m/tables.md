## Load time and index size

| engine | load (s) | of which: send / build | searchable | merge to 1 segment | index size (GB) | documents |
|---|---|---|---|---|---|---|
| Strata | 519 | load 660 + merge 180 | (built) | (built as one) | 5.05 | 1,000,000 |
| Manticore (one node) | 1789 | send 568 | – | flush 4, merge 1217 | 5.03 | 1,000,000 |

## 1 client: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms | Manticore q/s | Manticore ms |
|---|---|---|---|---|
| bm25 | 438 | 2.03 / 3.13 / 5.94 | 144 | 5.32 / 15.73 / 29.84 |
| bm25 + filter | 376 | 2.35 / 3.66 / 7.22 | 163 | 6.37 / 10.37 / 16.13 |
| phrase | 586 | 1.66 / 1.92 / 2.89 | 459 | 1.80 / 3.88 / 8.27 |
| typo | 76 | 13.15 / 18.42 / 26.61 | 19 | 45.71 / 88.01 / 125.23 |
| prefix | 395 | 2.27 / 3.65 / 5.31 | 53 | 12.47 / 39.32 / 151.61 |
| vector | 423 | 2.30 / 2.62 / 3.55 | 617 | 1.55 / 2.17 / 2.64 |
| vector + filter | 411 | 2.37 / 2.69 / 3.68 | 98 | 10.18 / 11.47 / 12.67 |
| hybrid (RRF) | 173 | 5.50 / 7.37 / 11.27 | 45 | 14.42 / 55.26 / 114.13 |
| hybrid (weighted) | 171 | 5.55 / 7.41 / 11.99 | n/a | n/a |
| boosted (bm25 × decay) | 203 | 4.18 / 8.20 / 12.67 | 132 | 5.81 / 17.20 / 32.26 |
| similar items | 336 | 2.90 / 3.24 / 5.02 | 281 | 3.57 / 5.04 / 5.89 |
| facet | 513 | 1.92 / 2.29 / 3.27 | 407 | 1.86 / 4.73 / 8.26 |
| key lookup | 1,092 | 0.89 / 1.05 / 1.60 | 1,651 | 0.53 / 0.96 / 1.21 |
| sql aggregate | 120 | 8.35 / 10.77 / 13.32 | 30 | 32.62 / 48.46 / 57.65 |


## 4 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms | Manticore q/s | Manticore ms |
|---|---|---|---|---|
| bm25 | 1,194 | 2.98 / 4.97 / 9.20 | 184 | 11.63 / 53.75 / 137.15 |
| bm25 + filter | 1,043 | 3.41 / 5.72 / 10.83 | 245 | 11.72 / 35.09 / 75.62 |
| phrase | 1,798 | 2.13 / 2.77 / 4.00 | 893 | 2.64 / 9.91 / 23.41 |
| typo | 199 | 19.48 / 29.35 / 38.93 | 59 | 62.49 / 112.68 / 167.52 |
| prefix | 1,006 | 3.43 / 6.19 / 11.32 | 61 | 31.58 / 120.55 / 409.70 |
| vector | 1,177 | 3.26 / 4.27 / 5.80 | 2,629 | 1.47 / 1.89 / 2.66 |
| vector + filter | 1,133 | 3.36 / 4.46 / 6.71 | 338 | 11.49 / 14.76 / 16.97 |
| hybrid (RRF) | 454 | 8.25 / 12.23 / 19.23 | 141 | 17.37 / 68.08 / 151.70 |
| hybrid (weighted) | 448 | 8.35 / 12.33 / 19.60 | n/a | n/a |
| boosted (bm25 × decay) | 558 | 6.11 / 11.82 / 20.51 | 166 | 14.12 / 57.02 / 142.67 |
| similar items | 895 | 4.29 / 5.71 / 7.60 | 1,042 | 3.82 / 5.62 / 7.13 |
| facet | 1,556 | 2.48 / 3.31 / 4.55 | 1,009 | 2.13 / 8.76 / 28.27 |
| key lookup | 3,835 | 1.00 / 1.30 / 1.95 | 6,792 | 0.57 / 0.67 / 0.89 |
| sql aggregate | 295 | 13.21 / 19.01 / 24.03 | 34 | 85.11 / 259.64 / 346.28 |


## 16 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms | Manticore q/s | Manticore ms |
|---|---|---|---|---|
| bm25 | 1,990 | 7.29 / 12.44 / 21.37 | 177 | 39.50 / 323.62 / 746.89 |
| bm25 + filter | 1,711 | 8.31 / 15.00 / 25.97 | 239 | 40.83 / 79.12 / 1069.30 |
| phrase | 3,523 | 4.24 / 6.43 / 10.05 | 855 | 14.78 / 34.33 / 72.06 |
| typo | 249 | 62.23 / 96.38 / 143.19 | 78 | 183.88 / 312.88 / 552.94 |
| prefix | 1,901 | 7.79 / 13.12 / 19.13 | 65 | 80.17 / 550.13 / 2695.36 |
| vector | 2,030 | 7.44 / 11.43 / 16.70 | 5,207 | 2.66 / 3.25 / 16.00 |
| vector + filter | 1,954 | 7.71 / 11.95 / 16.86 | 489 | 30.33 / 42.21 / 47.36 |
| hybrid (RRF) | 642 | 22.71 / 38.26 / 67.01 | 172 | 77.41 / 158.35 / 338.48 |
| hybrid (weighted) | 648 | 22.97 / 37.72 / 59.86 | n/a | n/a |
| boosted (bm25 × decay) | 816 | 16.47 / 34.07 / 63.71 | 162 | 43.87 / 353.94 / 722.46 |
| similar items | 1,416 | 10.51 / 16.81 / 25.44 | 1,746 | 8.13 / 11.78 / 24.50 |
| facet | 2,957 | 5.08 / 7.75 / 11.51 | 1,211 | 7.73 / 24.31 / 53.05 |
| key lookup | 9,437 | 1.59 / 2.37 / 3.79 | 20,764 | 0.70 / 0.92 / 2.17 |
| sql aggregate | 344 | 45.24 / 72.50 / 91.94 | 39 | 395.90 / 616.51 / 772.77 |


Errors during the timed runs: Strata 0, Manticore 0

## Answers: recall@10 against an exact search, nDCG@10 against the collection's judgments

Unavailable kinds are n/a; recall is for the kinds an exact search can answer.

| kind | Strata recall | Strata nDCG | Manticore recall | Manticore nDCG |
|---|---|---|---|---|
| bm25 | – | – | – | – |
| bm25 + filter | – | – | – | – |
| phrase | – | – | – | – |
| typo | – | – | – | – |
| prefix | – | – | – | – |
| vector | 0.807 | – | 0.683 | – |
| vector + filter | 0.861 | – | 0.677 | – |
| hybrid (RRF) | – | – | – | – |
| hybrid (weighted) | – | – | n/a | n/a |
| boosted (bm25 × decay) | – | – | – | – |
| similar items | 0.994 | – | 0.875 | – |


## SQL aggregates equal to the true answer

| Strata | Manticore |
|---|---|
| 300 of 300 | 300 of 300 |

## Agreement with Strata's answers

| kind | Manticore |
|---|---|
| bm25 | 0.558 of the top 10 |
| bm25 + filter | 0.590 of the top 10 |
| phrase | 0.455 of the top 10 |
| typo | 0.065 of the top 10 |
| prefix | 0.424 of the top 10 |
| vector | 0.683 of the top 10 |
| vector + filter | 0.693 of the top 10 |
| hybrid (RRF) | 0.676 of the top 10 |
| hybrid (weighted) | n/a |
| boosted (bm25 × decay) | 0.547 of the top 10 |
| similar items | 0.877 of the top 10 |
| facet | 279 of 300 equal |
| key lookup | 300 of 300 equal |


## Writes: documents a second, and latency in ms (p50 / p99)

| phase | clients | Strata docs/s | Strata ms |
|---|---|---|---|
| insert one | 1 | 287 | 3.18 / 6.87 |
| insert one | 4 | 938 | 3.48 / 7.64 |
| insert one | 16 | 2,186 | 5.44 / 78.24 |
| insert batch | 1 | 1,301 | 44.89 / 191.47 |
| insert batch | 4 | 3,099 | 97.02 / 406.09 |
| insert batch | 16 | 4,947 | 284.38 / 670.93 |
| update one | 1 | 285 | 2.69 / 9.89 |
| update one | 4 | 1,217 | 2.89 / 7.92 |
| update one | 16 | 4,509 | 3.39 / 6.18 |

## How soon a written document is found (ms from the write's start)

| engine | searchable | write returned (p50) | found (p50) | found (p90) | found (max) |
|---|---|---|---|---|---|
| Strata | searchable when the write returns | 3.3 | 7.6 | 9.4 | 85.1 |

## Streams through Redpanda: documents written a second, and lag in ms from when a document was due (p50 / p99)

| phase | Strata docs/s | Strata lag |
|---|---|---|
| 250/s | 249 | 78 / 109 |
| 500/s | 499 | 77 / 143 |
| 1000/s | 996 | 79 / 252 |
| 2000/s | 1,994 | 98 / 652 |
| flat out | 3,368 | 3054 / 5127 |

Lag includes reading the topic (Redpanda's part, the same for every engine, is the 'read' column below).

| engine | what written means | read lag p50 (ms) | crash: events written again | documents lost |
|---|---|---|---|---|
| Strata | searchable (the server answers a batch once its rows are visible) | 32 | 0 (the consumer resumes after the offsets the server keeps with the rows (GET /v1/sources)) | 0 |

## CPU used by each engine's containers (cgroup cpu.stat)

| engine | phase | pod | CPU seconds | wall seconds | average cores | throttled (s) |
|---|---|---|---|---|---|---|
| Strata | timed run | strata-0 | 709 | 368 | 1.93 | 2 |
| Strata | timed run | strata-1 | 684 | 368 | 1.86 | 1 |
| Strata | writes | strata-0 | 83 | 70 | 1.18 | 1 |
| Strata | writes | strata-1 | 84 | 70 | 1.20 | 1 |
| Strata | stream | strata-0 | 139 | 152 | 0.92 | 1 |
| Strata | stream | strata-1 | 130 | 152 | 0.86 | 2 |
| Manticore | load | manticore-0 | 6845 | 1800 | 3.80 | 216 |
| Manticore | timed run | manticore-0 | 1794 | 384 | 4.67 | 30 |

## Memory of each engine's containers (cgroup memory.current, memory.stat, memory.peak; GB, all its pods)

| engine | after the load | of it anonymous | after the timed run | of it anonymous | of it files (page cache) | pods' peaks added |
|---|---|---|---|---|---|---|
| Strata | – | – | 1.06 | 0.94 | 0.10 | 3.77 |
| Manticore | 6.46 | 1.13 | 6.83 | 1.56 | 5.04 | 13.47 |

- Manticore, hybrid (weighted): unavailable: its hybrid search fuses by RRF only (fusion_method='rrf')

- Manticore, bm25: BM25 as the ranker expression bm25a(1.2, 0.75) with plain IDF (weight() is an integer: 10,000 x the score); Manticore's English stopwords

- Manticore, bm25 + filter: BM25 as the ranker expression bm25a(1.2, 0.75) with plain IDF (weight() is an integer: 10,000 x the score); Manticore's English stopwords

- Manticore, boosted (bm25 × decay): weight() x the decay, where weight() is bm25a(1.2, 0.75), which Manticore scales into 0 to 1 (not BM25's raw sum), so the decay weighs more than on Strata

- Manticore, hybrid (RRF): MATCH and knn fused by RRF (fusion_method='rrf', rank_constant 60, window 100)

- Manticore, phrase: BM25 as the ranker expression bm25a(1.2, 0.75) with plain IDF (weight() is an integer: 10,000 x the score); Manticore's English stopwords

- Manticore, prefix: BM25 as the ranker expression bm25a(1.2, 0.75) with plain IDF (weight() is an integer: 10,000 x the score); Manticore's English stopwords

- Manticore, typo: every word must match, each fuzzy (OPTION fuzzy=1, distance 2, preserve 1: fuzzy search takes no OR; it needs min_infix_len, set to 2)
