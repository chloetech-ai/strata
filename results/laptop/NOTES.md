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

## Step 4: end-to-end benchmark (`strata-server-bench`, quora)

Run from `strata-server/` at cb1e924 (this branch after merging 2360e07
"strata-server-bench: end-to-end benchmark on a BEIR collection"), exactly as
the file's header says for a laptop run:

    STRATA_DATA=/Users/rkonow/code2/bench-data/beir caffeinate -i cargo run --release \
        --bin strata-server-bench -- --collection quora --work /Users/rkonow/code2/bench-work/server-quora

Defaults: 300 queries, 4 build threads, clients 1/4/16, 8 s per kind and client
count, 2 shards. Started 17:49:37 UTC (load 4.52), finished 18:10:20 UTC, wall
1243 s (20.7 min), exit 0, no errors in any phase. The load average at the end
was 8.79 because the clients run in the same process and share the 8 cores with
the server. Working databases take 10 GB in `--work`, outside the repo.

- JSON: `strata-server/results/bench/quora.json`
- printed tables: `results/laptop/strata-server-bench-quora.md`
- progress lines as printed: `results/laptop/strata-server-bench-quora.log`

Things worth a look in the report:
- **Size.** The 8-bit database is 2,820 MB (5,392 B/doc) and the 1-bit one
  2,351 MB, because float32 vectors (2,142 MB) dominate both files. The 1-bit codes
  cut what a search keeps in memory from 536 MB to 67 MB, but the file shrinks
  only 17%.
- **Vector recall on real vectors.** 8-bit HNSW at ef 64 gives 0.990 at p50 0.10 ms.
  1-bit HNSW (breadth 320) gives 0.995 at p50 0.39 ms, but its p99 is 2.4 ms.
- **Writes are bound by fsync.** About 190 writes/s at every client count, with
  p50 about 5 ms, which fits macOS `F_FULLFSYNC`. p99 rises to 215 ms at 4 clients
  and 712 ms at 16, so concurrent writers queue rather than share a commit.
- **Two shards on one machine are slower than one database.** BM25 at 16
  clients: 3,828 q/s against 17,611, and hybrid RRF at 16 clients: 970 against
  5,519. Expected with gRPC over localhost and everything on 8 cores.
- **Shard agreement.** BM25 has the same top 10 in only 20 of 50 queries but the
  same scores in 50 of 50, so these are tie swaps. Hybrid RRF agrees in 20 of 50,
  and its recall@10 drops from 0.972 (one database) to 0.942 (shards).
- With 1-bit codes, the filtered-vector and hybrid p99 at 16 clients is
  40–47 ms, against 8–15 ms with 8-bit codes.

# Re-run after fixes

Tree: `claude/laptop-report` at 021f92b, the merge of
`origin/claude/project-thread-2f1ge7` at 76970b5 into the first run's 107b978.
New since the first run: a NEON kernel for 1-bit vector codes (bb44dd9), rows
tied on score cut and ordered by key (4dfb665), group commit for writes
(9081bde), flat vectors in in-memory chunks (76970b5). Same machine and
toolchain as above.

## Step 1: tests (`cargo test --release`)

One crate at a time, each under `caffeinate -i`, 2026-10-27 19:03–19:12 UTC.
Wall time includes the release build; the target directories were warm from the
first run, so only the changed crates recompiled.

| crate | passed | failed | ignored | test binaries | wall | of which compile | load at start |
|---|---:|---:|---:|---:|---:|---:|---:|
| strata | 258 | 0 | 14 | 37 | 353 s | 4 m 41 s | 2.08 |
| strata-server | 8 | 0 | 0 | 5 | 38 s | 35 s | 22.36 |
| engram | 42 | 0 | 0 | 6 | 129 s | 2 m 05 s | 23.89 |

All pass. Against the first run: strata gains one test (258 against 257, in
`strata/tests/cluster_split.rs`) and strata-server one test (8 against 7, in
`strata-server/tests/server.rs`). The extra strata-server test binary is the
`strata-server-bench` target, which the first test run (at 4f3f628) predates. The load average
rose to 22–34 while the tests ran (the build, test threads and macOS daemons), so
these wall times are rough.

## Step 2: 1-bit kernel timing (synthetic, 500,000 × 768, 1000 clusters, 200 queries)

In `strata/`, under `caffeinate -i`, after the build and once the 1-minute load
average was below 6:

    cargo run --release --bin strata-codes-bench -- --kinds flat,ivf,hnsw --out results/vector/m2-neon

Started 19:14:15 UTC at 9fd0d05, load 4.99 (5- and 15-minute averages 20.6 and
17.2, still falling from the tests), 376 s, load 6.20 at the end. Exit 0.
JSON: `strata/results/vector/m2-neon/codes-500000x768.json`.

Printed table (ef 64, as the first run's step 3 run 1):

```text
data: 500000 × 768 in 12.7 s
exact top-10 in 0.6 s
codes  index  rerank rescored   code MB    f32 MB build s recall@10   p50 ms   p99 ms
sq8    flat       10       10     384.0    1536.0     1.1     0.974    7.217    7.950
sq8    flat       20       20     384.0    1536.0     1.1     1.000    7.253    7.933
sq8    flat       40       40     384.0    1536.0     1.1     1.000    7.240    7.843
binary flat       10       80      48.0    1536.0     3.9     0.783    6.845    7.106
binary flat       20      160      48.0    1536.0     3.9     0.917    6.881    7.173
binary flat       40      320      48.0    1536.0     3.9     0.994    7.004    7.280
sq8    ivf        10       10     384.0    1536.0    40.4     0.974    0.239    0.332
sq8    ivf        20       20     384.0    1536.0    40.4     1.000    0.243    0.360
sq8    ivf        40       40     384.0    1536.0    40.4     1.000    0.246    0.348
binary ivf        10       80      48.0    1536.0    43.8     0.783    0.247    0.302
binary ivf        20      160      48.0    1536.0    43.8     0.917    0.251    0.331
binary ivf        40      320      48.0    1536.0    43.8     0.994    0.275    0.391
sq8    hnsw       10       10     384.0    1536.0   131.2     0.864    0.087    0.157
sq8    hnsw       20       20     384.0    1536.0   131.2     0.883    0.086    0.154
sq8    hnsw       40       40     384.0    1536.0   131.2     0.883    0.089    0.159
binary hnsw       10       80      48.0    1536.0   123.6     0.649    0.099    0.170
binary hnsw       20      160      48.0    1536.0   123.6     0.861    0.149    0.276
binary hnsw       40      320      48.0    1536.0   123.6     0.991    0.219    0.433
```

First run (no 1-bit NEON kernel) against this one, p50 / p99 in ms:

| codes | index | rerank | first run p50 | re-run p50 | first run p99 | re-run p99 | recall@10 first → re-run |
|---|---|---:|---:|---:|---:|---:|---|
| binary | flat | 10 | 14.926 | 6.845 | 17.017 | 7.106 | 0.783 → 0.783 |
| binary | flat | 20 | 14.995 | 6.881 | 16.011 | 7.173 | 0.917 → 0.917 |
| binary | flat | 40 | 15.052 | 7.004 | 29.871 | 7.280 | 0.994 → 0.994 |
| sq8 | flat | 10 | 7.552 | 7.217 | 8.747 | 7.950 | 0.974 → 0.974 |
| binary | ivf | 40 | 0.461 | 0.275 | 0.611 | 0.391 | 0.994 → 0.994 |
| sq8 | ivf | 40 | 0.250 | 0.246 | 0.440 | 0.348 | 1.000 → 1.000 |
| binary | hnsw | 40 | 0.298 | 0.219 | 0.639 | 0.433 | 0.982 → 0.991 |
| sq8 | hnsw | 40 | 0.096 | 0.089 | 0.178 | 0.159 | 0.858 → 0.883 |

- The 1-bit flat scan is now 2.2× faster (p50 15.0 → 6.8 ms) and slightly faster
  than the SQ8 flat scan (7.2 ms), where before it took twice as long. Its
  recall is unchanged, as expected from a kernel change.
- 1-bit IVF drops from 0.42–0.46 ms to 0.25–0.28 ms p50, close to SQ8 IVF.
- SQ8 rows move by 0–5%, within run-to-run noise; the SQ8 kernel is unchanged.
- HNSW recall differs between the runs for both code kinds (SQ8 0.838–0.858 →
  0.864–0.883; 1-bit at rerank 10 0.671 → 0.649, at rerank 40 0.982 → 0.991).
  Most likely the graph differs from run to run because it is built with 4
  threads (the tie-ordering change, 4dfb665, may also play a part); the build times (131 s and 124 s here, 136 s and 145 s before) differ too.
  Flat and IVF recall are identical to the first run.
