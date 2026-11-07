## Load time and index size

| engine | load (s) | of which: send / build | searchable | merge to 1 segment | index size (GB) | documents |
|---|---|---|---|---|---|---|
| Strata | 322 | load 591 + merge 30 | (built) | (built as one) | 4.99 | 1,000,000 |
| Qdrant (one node) | 3388 | send 333 | – | index 3055 | 8.58 | 1,000,000 |

## 1 client: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms | Qdrant q/s | Qdrant ms |
|---|---|---|---|---|
| bm25 | 385 | 2.38 / 3.53 / 6.15 | 705 | 1.36 / 2.08 / 2.82 |
| bm25 + filter | 356 | 2.55 / 3.90 / 6.96 | 600 | 1.59 / 2.30 / 3.68 |
| phrase | 478 | 2.00 / 2.58 / 3.28 | 409 | 1.14 / 4.85 / 14.32 |
| typo | 82 | 12.17 / 16.95 / 21.93 | n/a | n/a |
| prefix | 363 | 2.56 / 3.78 / 5.41 | n/a | n/a |
| vector | 392 | 2.48 / 3.06 / 3.65 | 522 | 1.84 / 2.27 / 2.73 |
| vector + filter | 387 | 2.50 / 3.03 / 3.80 | 301 | 3.23 / 4.03 / 4.71 |
| hybrid (RRF) | 181 | 5.26 / 6.98 / 11.05 | 115 | 8.17 / 12.27 / 14.13 |
| hybrid (weighted) | 184 | 5.17 / 6.90 / 10.12 | 114 | 8.31 / 12.22 / 14.33 |
| boosted (bm25 × decay) | 216 | 4.05 / 7.43 / 11.38 | 558 | 1.77 / 2.48 / 3.18 |
| similar items | 321 | 3.03 / 3.65 / 4.54 | 120 | 8.42 / 10.42 / 12.10 |
| facet | 439 | 2.19 / 2.89 / 3.72 | 54 | 13.70 / 40.11 / 51.41 |
| key lookup | 1,000 | 1.01 / 1.52 / 1.95 | 3,046 | 0.32 / 0.36 / 0.43 |
| sql aggregate | 122 | 8.14 / 10.78 / 13.74 | n/a | n/a |


## 4 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms | Qdrant q/s | Qdrant ms |
|---|---|---|---|---|
| bm25 | 1,259 | 2.82 / 4.66 / 8.53 | 1,853 | 2.08 / 3.07 / 4.21 |
| bm25 + filter | 1,114 | 3.20 / 5.26 / 10.23 | 1,667 | 2.29 / 3.30 / 4.99 |
| phrase | 1,928 | 2.01 / 2.45 / 3.30 | 1,342 | 1.63 / 5.27 / 16.24 |
| typo | 209 | 18.73 / 27.24 / 37.16 | n/a | n/a |
| prefix | 1,187 | 3.04 / 4.97 / 7.54 | n/a | n/a |
| vector | 1,328 | 2.91 / 3.70 / 4.95 | 1,238 | 3.12 / 4.00 / 5.12 |
| vector + filter | 1,302 | 2.96 / 3.76 / 4.91 | 727 | 5.35 / 6.66 / 8.51 |
| hybrid (RRF) | 507 | 7.40 / 10.87 / 17.19 | 307 | 11.90 / 18.57 / 22.42 |
| hybrid (weighted) | 511 | 7.38 / 10.65 / 16.46 | 302 | 12.04 / 18.71 / 22.77 |
| boosted (bm25 × decay) | 623 | 5.41 / 10.78 / 18.84 | 1,541 | 2.54 / 3.63 / 4.90 |
| similar items | 1,006 | 3.83 / 4.97 / 6.57 | 305 | 13.03 / 16.41 / 19.80 |
| facet | 1,701 | 2.28 / 2.93 / 3.94 | 64 | 61.48 / 109.08 / 146.50 |
| key lookup | 4,442 | 0.96 / 1.28 / 1.71 | 10,300 | 0.38 / 0.45 / 0.57 |
| sql aggregate | 320 | 12.36 / 17.34 / 22.25 | n/a | n/a |


## 16 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms | Qdrant q/s | Qdrant ms |
|---|---|---|---|---|
| bm25 | 2,196 | 6.58 / 11.33 / 18.69 | 2,431 | 6.26 / 9.25 / 14.46 |
| bm25 + filter | 1,954 | 7.38 / 12.67 / 21.82 | 2,091 | 7.16 / 10.98 / 17.43 |
| phrase | 4,089 | 3.77 / 5.00 / 7.33 | 1,719 | 8.28 / 14.78 / 28.22 |
| typo | 251 | 61.23 / 98.10 / 135.58 | n/a | n/a |
| prefix | 1,894 | 7.51 / 13.67 / 21.78 | n/a | n/a |
| vector | 2,442 | 6.29 / 8.83 / 12.26 | 1,417 | 10.61 / 15.35 / 22.80 |
| vector + filter | 2,354 | 6.49 / 9.25 / 13.09 | 744 | 20.38 / 28.42 / 34.49 |
| hybrid (RRF) | 727 | 20.65 / 31.88 / 49.11 | 323 | 47.23 / 65.28 / 79.69 |
| hybrid (weighted) | 731 | 20.81 / 31.65 / 46.59 | 321 | 47.85 / 66.67 / 79.74 |
| boosted (bm25 × decay) | 901 | 15.14 / 29.70 / 55.44 | 2,027 | 7.57 / 11.13 / 17.24 |
| similar items | 1,690 | 9.03 / 13.13 / 18.60 | 306 | 49.26 / 69.83 / 81.23 |
| facet | 3,400 | 4.49 / 6.29 / 9.52 | 64 | 247.92 / 331.01 / 401.44 |
| key lookup | 12,673 | 1.27 / 1.93 / 2.81 | 26,671 | 0.57 / 0.80 / 1.17 |
| sql aggregate | 367 | 41.54 / 67.77 / 91.30 | n/a | n/a |


Errors during the timed runs: Strata 0, Qdrant 0

## Answers: recall@10 against an exact search, nDCG@10 against the collection's judgments

Unavailable kinds are n/a; recall is for the kinds an exact search can answer.

| kind | Strata recall | Strata nDCG | Qdrant recall | Qdrant nDCG |
|---|---|---|---|---|
| bm25 | – | – | – | – |
| bm25 + filter | – | – | – | – |
| phrase | – | – | – | – |
| typo | – | – | n/a | n/a |
| prefix | – | – | n/a | n/a |
| vector | 0.809 | – | 0.905 | – |
| vector + filter | 0.861 | – | 0.871 | – |
| hybrid (RRF) | – | – | – | – |
| hybrid (weighted) | – | – | – | – |
| boosted (bm25 × decay) | – | – | – | – |
| similar items | 0.995 | – | 0.975 | – |


## SQL aggregates equal to the true answer

| Strata | Qdrant |
|---|---|
| 300 of 300 | n/a |

## Agreement with Strata's answers

| kind | Qdrant |
|---|---|
| bm25 | 0.943 of the top 10 |
| bm25 + filter | 0.932 of the top 10 |
| phrase | 0.640 of the top 10 |
| typo | n/a |
| prefix | n/a |
| vector | 0.828 of the top 10 |
| vector + filter | 0.842 of the top 10 |
| hybrid (RRF) | 0.860 of the top 10 |
| hybrid (weighted) | 0.703 of the top 10 |
| boosted (bm25 × decay) | 0.647 of the top 10 |
| similar items | 0.974 of the top 10 |
| facet | 274 of 300 equal |
| key lookup | 300 of 300 equal |


## Writes: documents a second, and latency in ms (p50 / p99)

| phase | clients | Strata docs/s | Strata ms |
|---|---|---|---|
| insert one | 1 | 307 | 3.07 / 5.03 |
| insert one | 4 | 1,018 | 3.34 / 6.61 |
| insert one | 16 | 2,412 | 4.57 / 45.38 |
| insert batch | 1 | 1,945 | 34.67 / 207.64 |
| insert batch | 4 | 3,848 | 55.80 / 737.87 |
| insert batch | 16 | 3,927 | 236.17 / 1224.42 |
| update one | 1 | 384 | 2.38 / 5.95 |
| update one | 4 | 1,523 | 2.54 / 4.25 |
| update one | 16 | 4,958 | 2.97 / 8.09 |

## How soon a written document is found (ms from the write's start)

| engine | searchable | write returned (p50) | found (p50) | found (p90) | found (max) |
|---|---|---|---|---|---|
| Strata | searchable when the write returns | 3.3 | 7.1 | 8.6 | 88.1 |

## Streams through Redpanda: documents written a second, and lag in ms from when a document was due (p50 / p99)

| phase | Strata docs/s | Strata lag |
|---|---|---|
| 250/s | 249 | 77 / 129 |
| 500/s | 498 | 78 / 158 |
| 1000/s | 997 | 80 / 286 |
| 2000/s | 1,993 | 89 / 470 |
| flat out | 4,037 | 1415 / 3891 |

Lag includes reading the topic (Redpanda's part, the same for every engine, is the 'read' column below).

| engine | what written means | read lag p50 (ms) | crash: events written again | documents lost |
|---|---|---|---|---|
| Strata | searchable (the server answers a batch once its rows are visible) | 33 | 0 (the consumer resumes after the offsets the server keeps with the rows (GET /v1/sources)) | 0 |

## CPU used by each engine's containers (cgroup cpu.stat)

| engine | phase | pod | CPU seconds | wall seconds | average cores | throttled (s) |
|---|---|---|---|---|---|---|
| Strata | timed run | strata-0 | 651 | 368 | 1.77 | 1 |
| Strata | timed run | strata-1 | 674 | 368 | 1.83 | 3 |
| Strata | writes | strata-0 | 74 | 69 | 1.07 | 2 |
| Strata | writes | strata-1 | 76 | 70 | 1.09 | 1 |
| Strata | stream | strata-0 | 114 | 154 | 0.74 | 2 |
| Strata | stream | strata-1 | 111 | 154 | 0.72 | 1 |
| Qdrant | load | qdrant-0 | 18847 | 3423 | 5.51 | 31 |
| Qdrant | timed run | qdrant-0 | 1385 | 301 | 4.60 | 12 |

## Memory of each engine's containers (cgroup memory.current, memory.stat, memory.peak; GB, all its pods)

| engine | after the load | of it anonymous | after the timed run | of it anonymous | of it files (page cache) | pods' peaks added |
|---|---|---|---|---|---|---|
| Strata | – | – | 1.02 | 0.90 | 0.10 | 4.18 |
| Qdrant | 9.61 | 3.50 | 9.52 | 3.55 | 4.58 | 20.15 |

- Qdrant, prefix: unavailable: prefixes need the prefix tokenizer, an index of its own that would replace the word index the facet and phrase kinds use on the field

- Qdrant, sql aggregate: unavailable: no grouped aggregates: its facet API counts values only

- Qdrant, typo: unavailable: no fuzzy matching: its text index matches whole words (or prefixes, with a prefix tokenizer)

- Qdrant, bm25: BM25 from the client: sparse vectors of Strata's analysis weighted tf (k1 + 1) / (tf + k1 (1 - b + b dl / avgdl)), Qdrant applying the IDF

- Qdrant, bm25 + filter: BM25 from the client: sparse vectors of Strata's analysis weighted tf (k1 + 1) / (tf + k1 (1 - b + b dl / avgdl)), Qdrant applying the IDF

- Qdrant, boosted (bm25 × decay): the BM25 top 100 re-ranked by a formula query: score x exp_decay(year, target 2024, scale 5, midpoint 0.5)

- Qdrant, hybrid (RRF): prefetch of the BM25 and vector top 100, fused by rrf (k 60)

- Qdrant, hybrid (weighted): prefetch of the BM25 and vector top 100, fused by dbsf (each normalised by its mean ± 3 standard deviations, summed unweighted: no weights)

- Qdrant, phrase: BM25 over the whole query, filtered by the text index's phrase match
