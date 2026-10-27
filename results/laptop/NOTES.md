# Strata Phase 1 on the MacBook Air (Apple M2)

Machine: MacBook Air, Apple M2 (4 performance + 4 efficiency cores), 24 GB,
macOS 26.0.1 (25A362), on AC power. Toolchain: rustc 1.92.0 / cargo 1.92.0
(stable-aarch64-apple-darwin). Tree: `claude/laptop-report`, branched from
`origin/claude/project-thread-2f1ge7` at 4f3f628 (main after PR #13, plus the
strata-server target-cpu fix).

## Step 1: build and test (`cargo test --release`)

Run one crate at a time, each under `caffeinate -i`, 2026-10-27 17:22–17:33 UTC.
Wall time includes the release build (the `Finished` line is the compile part).

| crate | passed | failed | ignored | test binaries | wall | of which compile |
|---|---:|---:|---:|---:|---:|---:|
| strata | 257 | 0 | 14 | 37 | 362 s | 4 m 52 s |
| engram | 42 | 0 | 0 | 6 | 150 s | 2 m 27 s |
| strata-server | 7 | 0 | 0 | 4 | 89 s | 1 m 24 s |

strata-server compiles and passes on macOS with the target-cpu fix.

Load average at the start of each crate: 3.45, 27.5, 105.7. During the engram and
strata-server runs, macOS background daemons (mobileassetd at about 115% CPU,
triald, logd, BiomeAgent, fseventsd) pushed the load average far above 8 with no
build of mine running, so treat these wall times as rough.

## Step 2: BEIR data

`STRATA_DATA=/Users/rkonow/code2/bench-data/beir`. All four collections were
already prepared there, so `prepare_beir.py` was not run:

| collection | docs | queries | dim | size on disk |
|---|---:|---:|---:|---:|
| quora | 522,931 | 10,000 | 1024 | 3.1 GB |
| trec-covid | 171,332 | 50 | 1024 | 1.3 GB |
| fiqa | 57,638 | 648 | 1024 | 411 MB |
| scifact | 5,183 | 300 | 1024 | 44 MB |

quora, fiqa and trec-covid already hold a `vec_gt_top100.bin` ground-truth cache
from earlier runs; scifact does not.

## Step 3: binary-code benchmark (synthetic, 500,000 × 768, 1000 clusters, 200 queries)

Same commands as the cloud run, in `strata/`, each under `caffeinate -i`:

    cargo run --release --bin strata-codes-bench -- --kinds flat,ivf,hnsw --out results/vector/m2
    cargo run --release --bin strata-codes-bench -- --kinds hnsw --ef 320 --rerank 40 --out results/vector/m2-ef320

Started once the 1-minute load average fell below 6 (it took about 2 minutes).
Run 1: 17:36:17 UTC, 415 s, load 5.46 → 6.04. Run 2: 17:43:12 UTC, 302 s, load
6.04 → 6.10. The SQ8 kernel is `neon-usdot`; builds use 4 threads, searches 1.
JSON: `strata/results/vector/m2/codes-500000x768.json` and
`strata/results/vector/m2-ef320/codes-500000x768.json`.

Printed table, run 1 (`--kinds flat,ivf,hnsw`, ef 64):

```text
data: 500000 × 768 in 12.9 s
exact top-10 in 0.6 s
codes  index  rerank rescored   code MB    f32 MB build s recall@10   p50 ms   p99 ms
sq8    flat       10       10     384.0    1536.0     2.4     0.974    7.552    8.747
sq8    flat       20       20     384.0    1536.0     2.4     1.000    7.697    9.264
sq8    flat       40       40     384.0    1536.0     2.4     1.000    7.662    9.345
binary flat       10       80      48.0    1536.0     3.3     0.783   14.926   17.017
binary flat       20      160      48.0    1536.0     3.3     0.917   14.995   16.011
binary flat       40      320      48.0    1536.0     3.3     0.994   15.052   29.871
sq8    ivf        10       10     384.0    1536.0    40.0     0.974    0.241    0.423
sq8    ivf        20       20     384.0    1536.0    40.0     1.000    0.243    0.398
sq8    ivf        40       40     384.0    1536.0    40.0     1.000    0.250    0.440
binary ivf        10       80      48.0    1536.0    44.8     0.783    0.423    0.556
binary ivf        20      160      48.0    1536.0    44.8     0.917    0.433    0.563
binary ivf        40      320      48.0    1536.0    44.8     0.994    0.461    0.611
sq8    hnsw       10       10     384.0    1536.0   135.9     0.838    0.092    0.171
sq8    hnsw       20       20     384.0    1536.0   135.9     0.858    0.092    0.189
sq8    hnsw       40       40     384.0    1536.0   135.9     0.858    0.096    0.178
binary hnsw       10       80      48.0    1536.0   144.5     0.671    0.153    0.297
binary hnsw       20      160      48.0    1536.0   144.5     0.876    0.216    0.392
binary hnsw       40      320      48.0    1536.0   144.5     0.982    0.298    0.639
```

Printed table, run 2 (`--kinds hnsw --ef 320 --rerank 40`):

```text
data: 500000 × 768 in 12.8 s
exact top-10 in 0.8 s
codes  index  rerank rescored   code MB    f32 MB build s recall@10   p50 ms   p99 ms
sq8    hnsw       40       40     384.0    1536.0   140.6     0.994    0.200    0.458
binary hnsw       40      320      48.0    1536.0   146.1     0.991    0.327    0.754
```

Against the cloud run (Intel Xeon @ 2.80 GHz, 4 cores, `vnni` kernel; the JSON in
`strata/results/vector/codes-500000x768*.json`). Code size is the same on both
(SQ8 384 MB, binary 48 MB, float32 1536 MB):

| codes | index | ef | rerank | build s cloud | build s M2 | recall@10 cloud | recall@10 M2 | p50 ms cloud | p50 ms M2 | p99 ms cloud | p99 ms M2 |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| sq8 | flat | – | 10 | 8.4 | 2.4 | 0.974 | 0.974 | 59.372 | 7.552 | 79.135 | 8.747 |
| sq8 | flat | – | 20 | 8.4 | 2.4 | 1.000 | 1.000 | 58.962 | 7.697 | 86.226 | 9.264 |
| sq8 | flat | – | 40 | 8.4 | 2.4 | 1.000 | 1.000 | 60.014 | 7.662 | 81.524 | 9.345 |
| binary | flat | – | 10 | 8.0 | 3.3 | 0.783 | 0.783 | 13.391 | 14.926 | 19.919 | 17.017 |
| binary | flat | – | 20 | 8.0 | 3.3 | 0.917 | 0.917 | 14.152 | 14.995 | 25.224 | 16.011 |
| binary | flat | – | 40 | 8.0 | 3.3 | 0.994 | 0.994 | 17.231 | 15.052 | 24.871 | 29.871 |
| sq8 | ivf | – | 10 | 75.5 | 40.0 | 0.974 | 0.974 | 1.711 | 0.241 | 3.555 | 0.423 |
| sq8 | ivf | – | 20 | 75.5 | 40.0 | 1.000 | 1.000 | 1.534 | 0.243 | 2.506 | 0.398 |
| sq8 | ivf | – | 40 | 75.5 | 40.0 | 1.000 | 1.000 | 1.803 | 0.250 | 4.058 | 0.440 |
| binary | ivf | – | 10 | 75.3 | 44.8 | 0.783 | 0.783 | 0.751 | 0.423 | 1.151 | 0.556 |
| binary | ivf | – | 20 | 75.3 | 44.8 | 0.917 | 0.917 | 0.664 | 0.433 | 1.181 | 0.563 |
| binary | ivf | – | 40 | 75.3 | 44.8 | 0.994 | 0.994 | 0.887 | 0.461 | 1.470 | 0.611 |
| sq8 | hnsw | ef 64 | 10 | 366.3 | 135.9 | 0.850 | 0.838 | 0.500 | 0.092 | 0.986 | 0.171 |
| sq8 | hnsw | ef 64 | 20 | 366.3 | 135.9 | 0.868 | 0.858 | 0.460 | 0.092 | 1.171 | 0.189 |
| sq8 | hnsw | ef 64 | 40 | 366.3 | 135.9 | 0.868 | 0.858 | 0.542 | 0.096 | 0.978 | 0.178 |
| binary | hnsw | ef 64 | 10 | 361.5 | 144.5 | 0.670 | 0.671 | 0.446 | 0.153 | 0.835 | 0.297 |
| binary | hnsw | ef 64 | 20 | 361.5 | 144.5 | 0.871 | 0.876 | 0.728 | 0.216 | 1.433 | 0.392 |
| binary | hnsw | ef 64 | 40 | 361.5 | 144.5 | 0.991 | 0.982 | 0.953 | 0.298 | 1.837 | 0.639 |
| sq8 | hnsw | ef 320 | 40 | 364.6 | 140.6 | 0.993 | 0.994 | 0.864 | 0.200 | 2.165 | 0.458 |
| binary | hnsw | ef 320 | 40 | 377.4 | 146.1 | 0.985 | 0.991 | 0.873 | 0.327 | 1.629 | 0.754 |

Observations:
- Recall matches the cloud run to within 0.01 everywhere. The data is seeded, so
  only the HNSW graphs differ, and they are built with 4 threads.
- On the M2, the SQ8 flat scan (7.6 ms) is about 2× *faster* than the binary flat
  scan (15 ms). On the cloud it is the other way round (59 ms against 13–17 ms).
  The binary scan on the M2 is about as fast as on the Xeon, while every other
  row is 1.5–8× faster, so the aarch64 Hamming-scan path may be worth a look.
- HNSW builds take 136–146 s on the M2 against 362–377 s on the cloud. HNSW
  queries are 3–5× faster at p50.
