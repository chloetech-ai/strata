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

## Step 3: end-to-end benchmark (`strata-server-bench`, quora), reusing the databases

From `strata-server/` at a1f7070, with the first run's work directory:

    STRATA_DATA=/Users/rkonow/code2/bench-data/beir caffeinate -i cargo run --release \
        --bin strata-server-bench -- --collection quora --work /Users/rkonow/code2/bench-work/server-quora \
        --reuse --out results/bench/rerun 2> ../results/laptop/strata-server-bench-quora-rerun.log \
        | tee ../results/laptop/strata-server-bench-quora-rerun.md

`--reuse` reopened all four databases (8-bit, 1-bit, two shards), so nothing was
rebuilt. Same defaults as the first run (300 queries, clients 1/4/16, 8 s per
kind and client count, 2 shards). Started 19:21:41 UTC, load 4.72 (5- and
15-minute 8.91 and 12.42), finished 19:32:08 UTC, wall 627 s (the first run took
1243 s with the builds), exit 0, no errors in any phase, load 8.05 at the end.

- JSON: `strata-server/results/bench/rerun/quora.json`
- printed tables: `results/laptop/strata-server-bench-quora-rerun.md`
- progress lines as printed: `results/laptop/strata-server-bench-quora-rerun.log`

**Size.** Identical to the first run, since the files were reopened, not rebuilt:
8-bit 2,819.6 MB (5,392 B/doc, codes 535.5 MB), 1-bit 2,351.1 MB (codes 66.9 MB),
shards 1,411.7 + 1,410.6 MB. The build times in the table are the first run's,
read back from each database's `.build.json`.

**Vector search** (SQL, one thread), first run → re-run:

| search | recall@10 | p50 ms | p99 ms |
|---|---|---|---|
| 8-bit codes, HNSW ef 64 (default) | 0.990 → 0.990 | 0.103 → 0.100 | 0.149 → 0.130 |
| 8-bit codes, HNSW ef 320 | 0.996 → 0.996 | 0.330 → 0.318 | 0.474 → 0.390 |
| 8-bit codes, exact scan | 1.000 → 1.000 | 13.703 → 9.945 | 16.571 → 10.556 |
| 1-bit codes, HNSW (breadth 320) | 0.995 → 0.995 | 0.389 → 0.293 | 2.382 → 0.447 |
| 1-bit codes, exact scan | 1.000 → 1.000 | 22.623 → 9.160 | 23.450 → 9.340 |

The 1-bit NEON kernel shows here too: the 1-bit exact scan is 2.5× faster and
now a little faster than the 8-bit one, and the 1-bit HNSW p99 drops from 2.4 ms
to 0.45 ms. The 8-bit exact scan is also faster (13.7 → 9.9 ms) although its
kernel did not change; part of that may be run-to-run variation.

**Writes** (one database), first run → re-run:

| write | clients | per s | p50 ms | p99 ms |
|---|---:|---|---|---|
| in-place update (one per request) | 1 | 189 → 194 | 5.040 → 5.044 | 8.149 → 7.043 |
| in-place update (one per request) | 4 | 198 → 411 | 5.086 → 9.915 | 214.941 → 11.867 |
| in-place update (one per request) | 16 | 197 → 1657 | 5.759 → 9.729 | 711.558 → 12.286 |
| SQL UPDATE by key | 1 | 171 → 167 | 5.914 → 5.992 | 7.682 → 7.595 |
| SQL UPDATE by key | 4 | 188 → 182 | 5.442 → 21.991 | 181.833 → 25.362 |
| SQL UPDATE by key | 16 | 194 → 181 | 6.076 → 88.350 | 670.370 → 99.764 |
| insert, 100 documents per request (requests/s) | 1 | 12 → 69 | 82.648 → 14.541 | 153.144 → 17.765 |

- Group commit works for `/v1/write`: 1,657 in-place updates/s at 16 clients
  (8.4× the first run) with p99 12 ms instead of 712 ms. Commits still take about
  5 ms (`F_FULLFSYNC`), so at 4 clients (411/s) a commit carries about two
  requests and at 16 clients about eight; a request waits for the commit in
  flight, hence p50 near 10 ms.
- SQL UPDATE still commits alone (by design, per 9081bde), so it stays near
  180/s; the FIFO queue turns the long tail into an even wait: p99 670 → 100 ms at
  16 clients, but p50 6 → 88 ms.
- Batched inserts: 6,940 documents/s (first run 1,213, 5.7×), p50 per request
  83 → 14.5 ms, from the flat index in in-memory chunks (76970b5).

**Shard agreement** (2 shards against one database, 50 queries), first run → re-run:

| kind | same top 10 | same scores (ties may swap) | recall@10, one database | recall@10, shards |
|---|---|---|---|---|
| bm25 | 20 → 49 | 50 → 50 | exact | exact |
| bm25 + filter | 28 → 49 | 50 → 50 | exact | exact |
| vector | 44 → 46 | 46 → 46 | 0.990 → 0.990 | 0.992 → 0.992 |
| vector + filter | 39 → 39 | 40 → 39 | 0.982 → 0.984 | 0.990 → 0.992 |
| hybrid (RRF) | 20 → 42 | 20 → 42 | 0.972 → 0.972 | 0.942 → 0.968 |
| hybrid (weighted) | 45 → 50 | 45 → 50 | 0.998 → 0.998 | 0.998 → 0.998 |
| boosted (bm25 × decay) | 43 → 50 | 43 → 50 | exact | exact |
| facet | 50 → 50 | 50 → 50 | exact | exact |
| key lookup | 50 → 50 | 50 → 50 | exact | exact |

Ordering ties by key (4dfb665) fixes most of the disagreement: BM25 now matches on
49 of 50 queries and weighted and boosted on 50 of 50. Hybrid RRF goes from 20 to
42, and the shards' RRF recall rises from 0.942 to 0.968 (one database 0.972).
The vector kinds are unchanged: HNSW over two half-size graphs finds slightly
different neighbours, which is not a tie.

**HTTP queries**, 1 and 16 clients, first run → re-run (4-client rows are in the
.md):

| setup | kind | clients | per s first → re-run | p50 ms first → re-run | p99 ms first → re-run |
|---|---|---:|---|---|---|
| one database | bm25 | 1 | 4029 → 3233 | 0.208 → 0.247 | 0.977 → 1.292 |
| one database | bm25 | 16 | 17611 → 19642 | 0.712 → 0.634 | 3.950 → 3.403 |
| one database | bm25 + filter | 1 | 2708 → 2120 | 0.306 → 0.364 | 1.322 → 1.917 |
| one database | bm25 + filter | 16 | 12390 → 12652 | 1.045 → 1.043 | 4.967 → 4.412 |
| one database | vector | 1 | 3966 → 4125 | 0.249 → 0.241 | 0.376 → 0.276 |
| one database | vector | 16 | 14102 → 18530 | 0.949 → 0.769 | 4.469 → 2.835 |
| one database | vector + filter | 1 | 2887 → 2910 | 0.338 → 0.342 | 0.505 → 0.401 |
| one database | vector + filter | 16 | 8510 → 12400 | 1.565 → 1.052 | 7.881 → 5.734 |
| one database | hybrid (RRF) | 1 | 1672 → 1272 | 0.544 → 0.677 | 1.449 → 2.234 |
| one database | hybrid (RRF) | 16 | 5519 → 6903 | 2.214 → 1.990 | 14.875 → 6.643 |
| one database | hybrid (weighted) | 1 | 1575 → 1208 | 0.581 → 0.721 | 1.518 → 2.356 |
| one database | hybrid (weighted) | 16 | 5311 → 6460 | 2.375 → 2.143 | 13.572 → 6.831 |
| one database | boosted (bm25 × decay) | 1 | 2493 → 1730 | 0.342 → 0.462 | 1.271 → 2.096 |
| one database | boosted (bm25 × decay) | 16 | 11224 → 10056 | 1.190 → 1.348 | 4.965 → 5.034 |
| one database | facet | 1 | 11738 → 10201 | 0.074 → 0.089 | 0.233 → 0.228 |
| one database | facet | 16 | 34130 → 46424 | 0.426 → 0.324 | 1.357 → 0.792 |
| one database | key lookup | 1 | 15882 → 13192 | 0.060 → 0.075 | 0.097 → 0.090 |
| one database | key lookup | 16 | 43807 → 58630 | 0.344 → 0.265 | 0.804 → 0.484 |
| 1-bit codes | vector | 1 | 1786 → 2152 | 0.553 → 0.465 | 0.766 → 0.521 |
| 1-bit codes | vector | 16 | 6688 → 11304 | 2.050 → 1.112 | 8.617 → 6.518 |
| 1-bit codes | vector + filter | 1 | 819 → 1026 | 1.216 → 0.974 | 1.584 → 1.146 |
| 1-bit codes | vector + filter | 16 | 2820 → 5523 | 4.052 → 2.473 | 40.523 → 7.928 |
| 1-bit codes | hybrid (RRF) | 1 | 737 → 738 | 1.284 → 1.252 | 2.149 → 2.850 |
| 1-bit codes | hybrid (RRF) | 16 | 2662 → 3915 | 4.237 → 3.338 | 43.006 → 18.674 |
| 1-bit codes | hybrid (weighted) | 1 | 724 → 722 | 1.331 → 1.285 | 2.226 → 2.845 |
| 1-bit codes | hybrid (weighted) | 16 | 2491 → 3232 | 4.454 → 3.917 | 47.177 → 25.118 |
| 2 shards | bm25 | 1 | 2407 → 2202 | 0.388 → 0.414 | 0.843 → 1.014 |
| 2 shards | bm25 | 16 | 3828 → 4094 | 4.068 → 3.885 | 6.211 → 5.738 |
| 2 shards | bm25 + filter | 1 | 1261 → 1146 | 0.747 → 0.812 | 1.443 → 1.683 |
| 2 shards | bm25 + filter | 16 | 2255 → 2382 | 7.024 → 6.633 | 10.342 → 9.776 |
| 2 shards | vector | 1 | 2667 → 2858 | 0.361 → 0.348 | 0.621 → 0.396 |
| 2 shards | vector | 16 | 6141 → 7602 | 2.524 → 2.064 | 4.497 → 3.448 |
| 2 shards | vector + filter | 1 | 1851 → 1882 | 0.526 → 0.530 | 0.860 → 0.624 |
| 2 shards | vector + filter | 16 | 2772 → 5238 | 5.353 → 2.954 | 14.840 → 5.653 |
| 2 shards | hybrid (RRF) | 1 | 550 → 685 | 1.755 → 1.418 | 2.806 → 2.235 |
| 2 shards | hybrid (RRF) | 16 | 970 → 1820 | 15.674 → 8.605 | 33.071 → 13.667 |
| 2 shards | hybrid (weighted) | 1 | 533 → 607 | 1.806 → 1.602 | 2.835 → 2.427 |
| 2 shards | hybrid (weighted) | 16 | 888 → 1253 | 17.387 → 12.537 | 31.972 → 19.816 |
| 2 shards | boosted (bm25 × decay) | 1 | 725 → 717 | 1.308 → 1.335 | 2.292 → 2.354 |
| 2 shards | boosted (bm25 × decay) | 16 | 1380 → 1414 | 11.412 → 11.164 | 17.162 → 16.605 |
| 2 shards | facet | 1 | 4119 → 4711 | 0.229 → 0.207 | 0.493 → 0.353 |
| 2 shards | facet | 16 | 9480 → 11345 | 1.649 → 1.397 | 2.719 → 2.077 |
| 2 shards | key lookup | 1 | 6139 → 7521 | 0.152 → 0.131 | 0.323 → 0.160 |
| 2 shards | key lookup | 16 | 13861 → 17010 | 1.133 → 0.933 | 1.831 → 1.356 |

- At 16 clients almost every kind is as fast or faster. The biggest gains are in
  the vector kinds, especially 1-bit codes (1-bit vector + filter 2,820 → 5,523
  q/s, p99 40.5 → 7.9 ms; 1-bit hybrid p99 43–47 → 19–25 ms), and in hybrid RRF
  over shards (970 → 1,820 q/s).
- **Worth checking:** at 1 client, the kinds ranked by BM25 are slower on one
  database. Server-side p50 went bm25 0.119 → 0.154 ms, bm25 + filter 0.218 →
  0.272, hybrid RRF 0.403 → 0.528, hybrid weighted 0.438 → 0.568, boosted
  0.249 → 0.363 (+23% to +46%), while vector (0.126 → 0.122) and vector + filter
  (0.201 → 0.203) did not move, and facet and key lookup moved by 0.006 ms.
  4dfb665 changed exactly those rankings (reading to the end of the tie at the
  cut and ordering it by key), so this may be its cost on quora's many tied BM25
  scores. It is one run, though, and the same kinds are as fast or faster at 16
  clients (bm25 server p50 0.300 → 0.288 ms), so a repeat would be needed to
  confirm it.

# Third run, after the BM25 tie fix

Tree: `claude/laptop-report` at 5eaa2be, the merge of
`origin/claude/project-thread-2f1ge7` at 1017948 ("strata: keep BM25 ties in
one pass, and score a document the same everywhere") into the re-run's 8c4d8a9.
1017948 is the only change since the re-run: the text index keeps the tie at the
k-th score in one traversal instead of the executor searching again for twice as
many rows, and term scores are summed in f64. Same machine and toolchain as above.

## Step 1: tests (`cargo test --release --test text_topk --test cluster_split`)

In `strata/`, under `caffeinate -i`, 2026-10-27 20:28 UTC, load 2.70 at the start.
101 s wall including the release build of the changed crate. Exit 0.

| test binary | passed | failed | ignored |
|---|---:|---:|---:|
| `tests/text_topk.rs` | 5 | 0 | 0 |
| `tests/cluster_split.rs` | 4 | 0 | 0 |

`text_topk` includes the new `ties_kept_alike` test from 1017948. The full suites
were not re-run here (they pass in CI on macOS).

## Step 2: end-to-end benchmark (`strata-server-bench`, quora), reusing the databases again

From `strata-server/` at be5083c (code identical to the merge 5eaa2be), with the
same work directory as the first two runs:

    STRATA_DATA=/Users/rkonow/code2/bench-data/beir caffeinate -i cargo run --release \
        --bin strata-server-bench -- --collection quora --work /Users/rkonow/code2/bench-work/server-quora \
        --reuse --out results/bench/rerun2 2> ../results/laptop/strata-server-bench-quora-rerun2.log \
        | tee ../results/laptop/strata-server-bench-quora-rerun2.md

The build finished at 20:31:12 UTC with the 1-minute load at 11.22. The driver
waited for it to drop below 6 with no compiler or benchmark running, and started
at 20:32:12 UTC.

- **Load before:** 5.26 (5- and 15-minute 5.91 and 4.03)
- **Load after:** 8.43 (5- and 15-minute 11.11 and 8.53); the benchmark's own 16 clients and server threads account for most of that
- **Wall:** 629 s (re-run 627 s), exit 0, errors 0 in every row

`--reuse` reopened all four databases again, so sizes and build times are the
first run's (8-bit 2,819.6 MB, 1-bit 2,351.1 MB, shards 1,411.7 + 1,410.6 MB).

Output files:

- JSON: `strata-server/results/bench/rerun2/quora.json`
- printed tables: `results/laptop/strata-server-bench-quora-rerun2.md`
- progress lines as printed: `results/laptop/strata-server-bench-quora-rerun2.log`

The comparison tables below come from the three JSON files (`strata-server/results/bench/quora.json`, `rerun/quora.json` and `rerun2/quora.json`).

**HTTP queries: server-side p50 at 1 client and q/s at 16 clients**, first run / re-run / third run:

| setup | kind | server p50, 1 client: first / re-run / third | third vs first | third vs re-run | q/s, 16 clients: first / re-run / third | third vs first | third vs re-run |
|---|---|---|---:|---:|---|---:|---:|
| one database | bm25 | 0.119 / 0.154 / 0.123 | +3% | -20% | 17,611 / 19,642 / 23,395 | +33% | +19% |
| one database | bm25 + filter | 0.218 / 0.272 / 0.222 | +2% | -18% | 12,390 / 12,652 / 15,430 | +25% | +22% |
| one database | vector | 0.126 / 0.122 / 0.125 | -1% | +2% | 14,102 / 18,530 / 17,197 | +22% | -7% |
| one database | vector + filter | 0.201 / 0.203 / 0.212 | +5% | +5% | 8,510 / 12,400 / 11,436 | +34% | -8% |
| one database | hybrid (RRF) | 0.403 / 0.528 / 0.411 | +2% | -22% | 5,519 / 6,903 / 8,429 | +53% | +22% |
| one database | hybrid (weighted) | 0.438 / 0.568 / 0.452 | +3% | -20% | 5,311 / 6,460 / 7,641 | +44% | +18% |
| one database | boosted (bm25 × decay) | 0.249 / 0.363 / 0.255 | +3% | -30% | 11,224 / 10,056 / 14,012 | +25% | +39% |
| one database | facet | 0.038 / 0.044 / 0.045 | +18% | +2% | 34,130 / 46,424 / 44,743 | +31% | -4% |
| one database | key lookup | 0.027 / 0.033 / 0.033 | +25% | +2% | 43,807 / 58,630 / 55,422 | +27% | -5% |
| 1-bit codes | vector | 0.414 / 0.324 / 0.333 | -19% | +3% | 6,688 / 11,304 / 10,112 | +51% | -11% |
| 1-bit codes | vector + filter | 1.069 / 0.826 / 0.881 | -18% | +7% | 2,820 / 5,523 / 4,508 | +60% | -18% |
| 1-bit codes | hybrid (RRF) | 1.133 / 1.099 / 1.002 | -12% | -9% | 2,662 / 3,915 / 3,440 | +29% | -12% |
| 1-bit codes | hybrid (weighted) | 1.176 / 1.133 / 1.035 | -12% | -9% | 2,491 / 3,232 / 3,351 | +35% | +4% |
| 2 shards | bm25 | 0.331 / 0.360 / 0.331 | -0% | -8% | 3,828 / 4,094 / 3,972 | +4% | -3% |
| 2 shards | bm25 + filter | 0.658 / 0.726 / 0.653 | -1% | -10% | 2,255 / 2,382 / 2,319 | +3% | -3% |
| 2 shards | vector | 0.255 / 0.245 / 0.248 | -3% | +1% | 6,141 / 7,602 / 6,948 | +13% | -9% |
| 2 shards | vector + filter | 0.384 / 0.395 / 0.394 | +3% | -0% | 2,772 / 5,238 / 4,815 | +74% | -8% |
| 2 shards | hybrid (RRF) | 1.558 / 1.262 / 1.170 | -25% | -7% | 970 / 1,820 / 1,717 | +77% | -6% |
| 2 shards | hybrid (weighted) | 1.602 / 1.436 / 1.367 | -15% | -5% | 888 / 1,253 / 1,306 | +47% | +4% |
| 2 shards | boosted (bm25 × decay) | 1.184 / 1.224 / 1.036 | -12% | -15% | 1,380 / 1,414 / 1,439 | +4% | +2% |
| 2 shards | facet | 0.181 / 0.163 / 0.171 | -5% | +5% | 9,480 / 11,345 / 10,854 | +14% | -4% |
| 2 shards | key lookup | 0.108 / 0.094 / 0.097 | -10% | +3% | 13,861 / 17,010 / 16,320 | +18% | -4% |


- **The BM25 slowdown is gone.** At 1 client on one database, server-side p50
  is back within 2–3% of the first run for every BM25-ranked kind: bm25 0.119 /
  0.154 / 0.123 ms, bm25 + filter 0.218 / 0.272 / 0.222, hybrid RRF 0.403 / 0.528
  / 0.411, hybrid weighted 0.438 / 0.568 / 0.452, boosted 0.249 / 0.363 / 0.255.
  The re-run's +23–46% is down to +2–3%.
- At 16 clients the same kinds are the fastest of the three runs: bm25 23,395 q/s
  (+19% on the re-run), bm25 + filter 15,430 (+22%), hybrid RRF 8,429 (+22%),
  hybrid weighted 7,641 (+18%), boosted 14,012 (+39%, and now above the first
  run's 11,224 rather than below it).
- Over 2 shards the BM25-ranked kinds also come back to the first run's level or
  better at 1 client (bm25 0.331 / 0.360 / 0.331 ms, boosted 1.184 / 1.224 /
  1.036 ms), and the hybrid kinds on 1-bit codes drop 9% from the re-run.
- Kinds 1017948 does not touch: vector, facet and key lookup are within 0 to
  +7% of the re-run at 1 client. At 16 clients they are 4–18% below the re-run
  (1-bit vector + filter 5,523 → 4,508 q/s) but still above the first run. The
  8-bit exact scan below also moves 9.9 → 10.9 ms. These code paths did not
  change, so this is most likely run-to-run variation (the 5-minute load reached
  11 during this run). One run each, so differences of about 10% are not
  conclusive either way.

**Shard agreement** (2 shards against one database, 50 queries), first run / re-run / third run:

| kind | same top 10 | same scores (ties may swap) | recall@10, one database | recall@10, shards |
|---|---|---|---|---|
| bm25 | 20 / 49 / 50 | 50 / 50 / 50 | exact | exact |
| bm25 + filter | 28 / 49 / 50 | 50 / 50 / 50 | exact | exact |
| vector | 44 / 46 / 46 | 46 / 46 / 46 | 0.990 / 0.990 / 0.990 | 0.992 / 0.992 / 0.992 |
| vector + filter | 39 / 39 / 39 | 40 / 39 / 39 | 0.982 / 0.984 / 0.984 | 0.990 / 0.992 / 0.992 |
| hybrid (RRF) | 20 / 42 / 42 | 20 / 42 / 42 | 0.972 / 0.972 / 0.972 | 0.942 / 0.968 / 0.968 |
| hybrid (weighted) | 45 / 50 / 50 | 45 / 50 / 50 | 0.998 / 0.998 / 0.998 | 0.998 / 0.998 / 0.998 |
| boosted (bm25 × decay) | 43 / 50 / 50 | 43 / 50 / 50 | exact | exact |
| facet | 50 / 50 / 50 | 50 / 50 / 50 | exact | exact |
| key lookup | 50 / 50 / 50 | 50 / 50 / 50 | exact | exact |


BM25 and BM25 + filter now return the same top 10 on shards as on one database
in 50 of 50 queries (49 in the re-run), as expected from 1017948's f64 score sums
(a document scores the same bit for bit on a shard and on the whole database, so
ties fall alike). Hybrid RRF stays at 42 of 50 with shard recall 0.968 (one
database 0.972). The vector kinds are unchanged: HNSW over two half-size graphs
finds slightly different neighbours, which is not a tie.

**Writes** (one database), first run / re-run / third run:

| write | clients | per s | p50 ms | p99 ms | errors (third) |
|---|---:|---|---|---|---:|
| in-place update (one per request) | 1 | 189 / 194 / 195 | 5.040 / 5.044 / 5.073 | 8.149 / 7.043 / 7.022 | 0 |
| in-place update (one per request) | 4 | 198 / 411 / 406 | 5.086 / 9.915 / 9.842 | 214.941 / 11.867 / 12.370 | 0 |
| in-place update (one per request) | 16 | 197 / 1,657 / 1,591 | 5.759 / 9.729 / 9.982 | 711.558 / 12.286 / 12.486 | 0 |
| SQL UPDATE by key | 1 | 171 / 167 / 162 | 5.914 / 5.992 / 6.078 | 7.682 / 7.595 / 8.221 | 0 |
| SQL UPDATE by key | 4 | 188 / 182 / 181 | 5.442 / 21.991 / 21.923 | 181.833 / 25.362 / 27.609 | 0 |
| SQL UPDATE by key | 16 | 194 / 181 / 182 | 6.076 / 88.350 / 87.831 | 670.370 / 99.764 / 96.152 | 0 |
| insert (100 documents per request) | 1 | 12 / 69 / 69 (6,889 docs/s) | 82.648 / 14.541 / 14.927 | 153.144 / 17.765 / 17.203 | 0 |


Write throughput is within 4% of the re-run (1017948 does not touch writes):
group commit still gives about 1,600 in-place updates/s at 16 clients with p99
12.5 ms, SQL UPDATE stays near 180/s, and batched inserts run at 6,889 documents/s.

**Vector search** (SQL, one thread), first run / re-run / third run:

| search | recall@10 | p50 ms | p99 ms |
|---|---|---|---|
| 8-bit codes, HNSW ef 64 (default) | 0.990 / 0.990 / 0.990 | 0.103 / 0.100 / 0.104 | 0.149 / 0.130 / 0.161 |
| 8-bit codes, HNSW ef 320 | 0.996 / 0.996 / 0.996 | 0.330 / 0.318 / 0.328 | 0.474 / 0.390 / 0.454 |
| 8-bit codes, exact scan | 1.000 / 1.000 / 1.000 | 13.703 / 9.945 / 10.885 | 16.571 / 10.556 / 11.423 |
| 1-bit codes, HNSW (breadth 320) | 0.995 / 0.995 / 0.995 | 0.389 / 0.293 / 0.305 | 2.382 / 0.447 / 0.394 |
| 1-bit codes, exact scan | 1.000 / 1.000 / 1.000 | 22.623 / 9.160 / 9.391 | 23.450 / 9.340 / 9.618 |
