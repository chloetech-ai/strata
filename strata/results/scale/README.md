# Scale probe results

Raw steps: [`steps.jsonl`](steps.jsonl), one JSON line per `strata-scale`
step (load, merge, vacuum, query, stats), each naming the machine. The
tables below are generated from it by `scripts/scale_report.py`; the
write-up is [`notes/scale.md`](../../notes/scale.md). Run 2026-10-27,
06:10-08:10 UTC, with nothing else running in the container.

Machine: Intel(R) Xeon(R) Processor @ 2.80GHz, 4 cores, 15.7 GiB RAM, cloud container (Claude Code on the web).

## Loads

| step | rows loaded | segments of | seconds | docs/s | HNSW s | text s | doc store s | peak RSS MiB | segments after | live GiB | bytes/doc |
|---|---|---|---|---|---|---|---|---|---|---|---|
| growth 1000000 | 1,000,000 | 100,000 | 221 | 4,528 | 145 | 39 | 4 | 1,024 | 10 | 2.08 | 2,236 |
| growth 2000000 | 1,000,000 | 100,000 | 234 | 4,270 | 146 | 41 | 4 | 1,073 | 13 | 4.20 | 2,254 |
| segments 57 | 5,000,000 | 87,720 | 1173 | 4,261 | 754 | 224 | 21 | 1,333 | 57 | 10.49 | 2,253 |

## Vacuums

| step | seconds | GiB returned | file GiB before → after |
|---|---|---|---|
| segments 19 | 8.7 | 1.67 | 12.13 → 10.45 |
| segments 7 | 119.5 | 6.59 | 17.04 → 10.44 |
| segments 5 | 0.0 | 0.00 | 12.08 → 12.08 |

## Merges

Peak RSS counts the table's file pages that the step read through its mapping; peak heap is anonymous memory only.

| step | policy | merges | docs rewritten | seconds | rewritten docs/s | HNSW s | text s | reading inputs s | peak RSS MiB | peak heap MiB | segments before → after | file GiB after |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| growth 1000000 | factor 8, ≤ 2,000,000 | 1 | 800,000 | 251 | 3,192 | 190 | 33 | 22 | 7,171 | - | 10 → 3 | 3.74 |
| growth 2000000 | factor 8, ≤ 2,000,000 | 1 | 800,000 | 247 | 3,235 | 187 | 35 | 21 | 7,455 | - | 13 → 6 | 7.54 |
| segments 19 | factor 3, ≤ 270,000 | 19 | 5,000,000 | 1451 | 3,445 | 976 | 205 | 232 | 9,049 | - | 57 → 19 | 12.13 |
| segments 7 | factor 4, ≤ 1,100,000 | 4 | 4,210,520 | 1656 | 2,542 | 1165 | 223 | 229 | 11,269 | 8,934 | 19 → 7 | 17.04 |
| segments 5 | factor 3, ≤ 1,100,000 | 1 | 789,480 | 290 | 2,723 | 218 | 39 | 26 | 7,077 | 5,919 | 7 → 5 | 12.08 |

## Warm latency, one query at a time (p50 / p99 ms)

| step | docs | segments | bm25 | bm25_filtered | vector | vector_ef128 | vector_ef256 | vector_filtered | hybrid_rrf | hybrid_weighted |
|---|---|---|---|---|---|---|---|---|---|---|
| growth 1000000 | 1,000,000 | 3 | 0.52 / 4.71 | 0.98 / 5.59 | 0.32 / 0.52 | 0.50 / 1.30 | 0.82 / 1.85 | 0.56 / 1.75 | 1.52 / 6.98 | 1.36 / 6.88 |
| growth 2000000 | 2,000,000 | 6 | 0.94 / 7.68 | 2.15 / 12.49 | 0.69 / 1.02 | 1.06 / 2.61 | 1.76 / 2.41 | 2.23 / 5.57 | 2.73 / 13.04 | 2.85 / 13.61 |
| segments 57 | 5,000,000 | 57 | 5.34 / 28.46 | 8.30 / 37.00 | 6.53 / 9.35 | 10.65 / 14.10 | 16.90 / 22.15 | 9.20 / 19.19 | 18.94 / 70.13 | 18.21 / 63.76 |
| segments 19 | 5,000,000 | 19 | 3.57 / 25.56 | 6.82 / 32.54 | 2.70 / 4.45 | 4.44 / 6.20 | 7.55 / 12.05 | 6.53 / 15.60 | 9.64 / 42.26 | 10.98 / 42.50 |
| segments 7 | 5,000,000 | 7 | 2.37 / 18.41 | 4.82 / 26.07 | 1.19 / 1.58 | 1.77 / 2.52 | 2.98 / 4.37 | 4.21 / 9.67 | 5.50 / 30.19 | 5.60 / 30.16 |
| segments 5 | 5,000,000 | 5 | 2.16 / 18.28 | 5.56 / 24.83 | 0.90 / 1.51 | 1.20 / 1.57 | 2.12 / 4.03 | 3.40 / 10.26 | 4.66 / 28.89 | 4.78 / 29.07 |

## Vector recall@10 against exact search

| step | segments | ef 64 | ef 128 | ef 256 | filtered (vs prefilter scan) |
|---|---|---|---|---|---|
| growth 1000000 | 3 | 0.849 | 0.906 | 0.947 | 0.922 |
| growth 2000000 | 6 | 0.881 | 0.920 | 0.959 | 0.938 |
| segments 57 | 57 | 0.954 | 0.976 | 0.992 | 0.979 |
| segments 19 | 19 | 0.930 | 0.961 | 0.981 | 0.966 |
| segments 7 | 7 | 0.885 | 0.931 | 0.962 | 0.936 |
| segments 5 | 5 | 0.879 | 0.928 | 0.958 | 0.928 |

## Segment-parallel prototype (4 threads, segments split by size) vs serial, p50 / p99 ms

| step | segments | kind | serial | parallel | p50 speed-up | agreement with serial top-10 |
|---|---|---|---|---|---|---|
| growth 1000000 | 3 | bm25 | 0.52 / 4.71 | 0.37 / 3.26 | 1.42x | 0.999 |
| growth 1000000 | 3 | bm25_filtered | 0.98 / 5.59 | 0.90 / 4.36 | 1.09x | 0.999 |
| growth 1000000 | 3 | hybrid_rrf | 1.52 / 6.98 | 1.23 / 7.84 | 1.24x | 1.000 |
| growth 1000000 | 3 | vector | 0.32 / 0.52 | 0.23 / 0.36 | 1.40x | 1.000 |
| growth 1000000 | 3 | vector_filtered | 0.56 / 1.75 | 0.44 / 1.51 | 1.28x | 1.000 |
| growth 2000000 | 6 | bm25 | 0.94 / 7.68 | 0.74 / 6.13 | 1.26x | 0.999 |
| growth 2000000 | 6 | bm25_filtered | 2.15 / 12.49 | 1.24 / 6.44 | 1.73x | 0.999 |
| growth 2000000 | 6 | hybrid_rrf | 2.73 / 13.04 | 1.87 / 7.88 | 1.46x | 0.999 |
| growth 2000000 | 6 | vector | 0.69 / 1.02 | 0.57 / 1.27 | 1.22x | 1.000 |
| growth 2000000 | 6 | vector_filtered | 2.23 / 5.57 | 1.06 / 2.67 | 2.11x | 1.000 |
| segments 57 | 57 | bm25 | 5.34 / 28.46 | 1.59 / 13.82 | 3.35x | 1.000 |
| segments 57 | 57 | bm25_filtered | 8.30 / 37.00 | 3.61 / 14.14 | 2.30x | 0.999 |
| segments 57 | 57 | hybrid_rrf | 18.94 / 70.13 | 6.49 / 20.23 | 2.92x | 0.998 |
| segments 57 | 57 | vector | 6.53 / 9.35 | 1.79 / 3.73 | 3.64x | 1.000 |
| segments 57 | 57 | vector_filtered | 9.20 / 19.19 | 3.44 / 7.12 | 2.68x | 1.000 |
| segments 19 | 19 | bm25 | 3.57 / 25.56 | 1.76 / 12.41 | 2.03x | 1.000 |
| segments 19 | 19 | bm25_filtered | 6.82 / 32.54 | 3.02 / 13.04 | 2.26x | 0.999 |
| segments 19 | 19 | hybrid_rrf | 9.64 / 42.26 | 4.58 / 18.11 | 2.10x | 0.999 |
| segments 19 | 19 | vector | 2.70 / 4.45 | 0.93 / 2.64 | 2.90x | 1.000 |
| segments 19 | 19 | vector_filtered | 6.53 / 15.60 | 2.60 / 7.02 | 2.51x | 1.000 |
| segments 7 | 7 | bm25 | 2.37 / 18.41 | 1.34 / 10.27 | 1.76x | 1.000 |
| segments 7 | 7 | bm25_filtered | 4.82 / 26.07 | 2.38 / 9.90 | 2.03x | 0.999 |
| segments 7 | 7 | hybrid_rrf | 5.50 / 30.19 | 3.08 / 17.26 | 1.79x | 0.999 |
| segments 7 | 7 | vector | 1.19 / 1.58 | 0.70 / 2.43 | 1.71x | 1.000 |
| segments 7 | 7 | vector_filtered | 4.21 / 9.67 | 2.32 / 5.75 | 1.81x | 1.000 |
| segments 5 | 5 | bm25 | 2.16 / 18.28 | 1.48 / 10.45 | 1.46x | 1.000 |
| segments 5 | 5 | bm25_filtered | 5.56 / 24.83 | 2.73 / 12.11 | 2.04x | 0.999 |
| segments 5 | 5 | hybrid_rrf | 4.66 / 28.89 | 3.10 / 16.33 | 1.50x | 0.998 |
| segments 5 | 5 | vector | 0.90 / 1.51 | 0.58 / 1.16 | 1.53x | 1.000 |
| segments 5 | 5 | vector_filtered | 3.40 / 10.26 | 2.27 / 6.43 | 1.50x | 1.000 |

## Throughput with 4 concurrent callers (serial queries)

| step | segments | kind | queries/s | p50 ms | p99 ms |
|---|---|---|---|---|---|
| growth 1000000 | 3 | bm25 | 5,257 | 0.42 | 5.52 |
| growth 1000000 | 3 | hybrid_rrf | 2,141 | 1.40 | 7.46 |
| growth 1000000 | 3 | vector | 10,946 | 0.30 | 1.36 |
| growth 2000000 | 6 | bm25 | 2,734 | 0.84 | 8.95 |
| growth 2000000 | 6 | hybrid_rrf | 1,168 | 2.67 | 14.83 |
| growth 2000000 | 6 | vector | 6,306 | 0.62 | 0.96 |
| segments 57 | 57 | bm25 | 666 | 4.17 | 32.90 |
| segments 57 | 57 | hybrid_rrf | 196 | 17.82 | 62.04 |
| segments 57 | 57 | vector | 640 | 5.96 | 8.95 |
| segments 19 | 19 | bm25 | 894 | 2.72 | 32.75 |
| segments 19 | 19 | hybrid_rrf | 332 | 9.87 | 47.47 |
| segments 19 | 19 | vector | 1,553 | 2.53 | 3.50 |
| segments 7 | 7 | bm25 | 938 | 2.66 | 30.36 |
| segments 7 | 7 | hybrid_rrf | 485 | 6.06 | 36.25 |
| segments 7 | 7 | vector | 3,634 | 1.09 | 1.54 |
| segments 5 | 5 | bm25 | 1,411 | 1.58 | 18.26 |
| segments 5 | 5 | hybrid_rrf | 604 | 4.62 | 41.49 |
| segments 5 | 5 | vector | 5,340 | 0.73 | 1.13 |

## Bytes per passage by component (segments 5 final, 5,000,000 passages)

| component | bytes/passage | GiB |
|---|---|---|
| vector emb (f32) | 1,024 | 4.77 |
| doc store | 417 | 1.94 |
| vector emb (sq8) | 256 | 1.19 |
| text body (pos) | 145 | 0.67 |
| text body (post) | 129 | 0.60 |
| vector emb (hnsw) | 128 | 0.60 |
| pk | 67 | 0.31 |
| structured | 59 | 0.28 |
| text body (dict) | 7 | 0.03 |
| vector emb (hnsw_upper) | 5 | 0.02 |
| text body (norms) | 1 | 0.00 |

## 2M merge at the default cap (2026-11-01)

Raw steps: [`merge-2m-2026-11-01.jsonl`](merge-2m-2026-11-01.jsonl), from
`strata-scale` (`load`, then `merge --merge-factor 8 --floor-docs 2000000
--max-merged-docs 2000000`, 4 threads). The first 2M passages of the table
above, loaded into eight segments of 250,000 (4.5 GB of image, 64% of it
float32 rows and doc store), merged into one, once with main before merges
spilled (`5ca9864`) and once with merges that spill, from the same copy of
the loaded file. Run 2026-11-01, 05:15-06:05 UTC; the table was loaded
twice, and the first spilling merge ran while a `cargo` build used the
cores for about 110 of its seconds.

Machine: Intel(R) Xeon(R) Processor @ 2.10GHz, 4 cores, 15.7 GiB RAM, cloud container (Claude Code on the web).

| step | seconds | HNSW s | text s | peak heap MiB | peak RSS MiB |
|---|---|---|---|---|---|
| main, run 1 | 231 | 191 | 119 | 7,281 | 11,585 |
| main, run 2 | 277 | 195 | 115 | 7,217 | 11,536 |
| spilling, run 1 (build beside it) | 328 | 246 | 114 | 3,957 | 10,230 |
| spilling, run 2 | 240 | 196 | 112 | 3,862 | 10,086 |

Peak RSS counts the input file's pages read through its mapping (and, when
spilling, the mapped float32 rows); peak heap is anonymous memory only. The
October runs above put a merge at the 2M cap at 16-18 GB of heap, from
7.9-8.9 KB per passage measured at 0.8M-1.05M, before merges decoded their
inputs in batches. The merger estimates this merge at 6.95 GB with merges
that spill (14.2 GB without).
