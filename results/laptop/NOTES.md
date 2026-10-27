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
