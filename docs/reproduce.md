---
title: Reproducing the results
nav_order: 5
---

# Reproducing the results
{: .no_toc }

Every number on these pages comes from a file committed under `engram/results/`, `strata/results/` or `posting_bench/results/`, produced on a 4-core cloud VM with an Emerald Rapids Xeon (AVX-512, Linux). For this release some of them were run again: on a cloud VM by the session that migrated the code, and on a MacBook Air. This page lists what was re-run, where, and what came out, then how to run everything else.
{: .fs-5 .fw-300 }

1. TOC
{:toc}

## What was re-run for this release

Two machines ran things again. Numbers from the second are labelled **MacBook Air** throughout and appear only on this page; every other page quotes the committed results.

- **Cloud VM:** the Linux x86-64 VM of the session that migrated the code.
- **MacBook Air:** Roberto's laptop, an Apple M2 with 8 cores (4 performance, 4 efficiency) and 24 GB, macOS 26.0.1, Rust 1.92.0.

### Test suites

`cargo test --release` in each crate:

| crate | cloud VM | MacBook Air |
|---|---|---|
| `posting_bench` | 96 passed | 94 passed, 7 ignored |
| `strata` | not reported | 116 passed |
| `engram` | not reported | 22 passed |

- **posting_bench** runs fewer tests on Apple silicon because two SIMD conformance tests (`sse42_matches_scalar`, `avx512_matches_scalar`) and two ignored timing tests are compiled only for x86-64. Every codec's scalar path passes the shared conformance suite.
- **strata** did not compile on macOS at first: the `strata-text-bench` binary passed a `*mut u8` to `libc::mincore`, which takes `*mut c_uchar` on Linux and `*mut c_char` on macOS. It now casts the pointer, as `strata/src/iostat.rs` already did. Two tests then failed on macOS, and both exposed real portability bugs, now fixed:
  - `iostat::tests::evict_then_read_counts_pages` counted resident pages assuming 4 KiB pages, but Apple silicon pages are 16 KiB, so `mincore` reported a quarter of the expected count. `resident_pages` now sizes its vector by the OS page size and still returns 4 KiB units.
  - `random_operations_match_model` hung inside `vacuum`: `relocate_down` called `pwrite` with a source buffer that was a mapping of the same file, which deadlocks in the macOS kernel. It now copies the extent out of the mapping before writing it back.

### Engram retrieval

`engram-bench retrieval` with the current code, which cuts records at 400 characters (the `_c400` result files). BM25 does not depend on the embedder or the CPU, and its rows came out identical everywhere.

**Cloud VM, LoCoMo, BM25** (run with the model-free hash embedder, so only the BM25 row is comparable): R@5 0.652, R@10 0.757, nDCG@10 0.473, injection at 4,000 characters 0.770, identical to `engram/results/retrieval/locomo_static_request_c400.json`.

**MacBook Air, LongMemEval-S and LoCoMo, static embedder** (`--embedder static --ctx request`, 470 and 1,536 eligible questions):

| | committed (VM) | MacBook Air |
|---|---|---|
| LongMemEval-S, BM25: R@5 / R@10 / nDCG@10 / 4k pack | 0.751 / 0.851 / 0.522 / 0.768 | 0.751 / 0.851 / 0.522 / 0.768 |
| LongMemEval-S, vectors | 0.819 / 0.898 / 0.609 / 0.891 | 0.819 / 0.898 / 0.609 / 0.891 |
| LongMemEval-S, hybrid with priors | 0.849 / 0.919 / 0.649 / 0.900 | 0.849 / 0.917 / 0.648 / 0.900 |
| LoCoMo, BM25 | 0.652 / 0.757 / 0.473 / 0.770 | 0.652 / 0.757 / 0.473 / 0.770 |
| LoCoMo, vectors | 0.303 / 0.391 / 0.238 / 0.410 | 0.304 / 0.391 / 0.238 / 0.411 |
| LoCoMo, hybrid with priors | 0.443 / 0.550 / 0.350 / 0.571 | 0.450 / 0.557 / 0.353 / 0.577 |

The vector and hybrid rows move by at most 0.007, while the BM25 rows are identical. That pattern fits floating-point rounding: the static embedder and the vector scores are sums of floats, which can round differently on the M2 than on the Xeon and reorder near-tied candidates. The whole LongMemEval-S run took 138 s on the laptop against 178 s on the VM.

The headline LongMemEval-S numbers on the [Engram results](engram/results.md#retrieval-on-long-conversation-benchmarks) page (R@10 0.90, 87% in the 4,000-character injection) come from `longmemeval_s_static_round.json`. That file was produced before records were cut at 400 characters; today's code always cuts at 400, so the `_c400` files are the ones it reproduces.

### Statistics recomputed from committed raw results

These need no model calls or downloads, only the per-run and per-query files in the repository.

- **Claude Code with and without Engram.** `engram/scripts/ab_stats.py` over the held-out projects' `runs.jsonl` gives the same counts (100/100 against 60/100; 60/60 against 20/60 on conversation-only facts), the same Fisher p-values (1.34 × 10<sup>−14</sup> pooled) and the same permutation p-values (< 1/20,001). The bootstrap intervals differ in the second decimal, because the committed `stats.json` also covers the Tally runs, which shifts the resampling's random stream.
- **Does the graph help search?** `strata/scripts/graph_quality_stats.py` over `quality_arxiv.rows.jsonl.gz` reproduces `strata/results/graph/quality_stats.json` exactly, including nDCG@20 0.253 against 0.123 and its interval and p-value.

### Strata graphs at a million rows

`strata-graph-bench synthetic --nodes 1000000` with the SQLite baseline, on the MacBook Air, written to a scratch directory and compared with `strata/results/graph/synthetic_1000000.json`:

| query (p50, ms) | Strata, VM | Strata, Mac | SQLite, VM | SQLite, Mac |
|---|---:|---:|---:|---:|
| REACHABLE out, 1 hop | 0.023 | 0.026 | 0.022 | 0.017 |
| REACHABLE out, 2 hops | 0.048 | 0.052 | 0.106 | 0.096 |
| REACHABLE out, 3 hops | 0.166 | 0.163 | 1.034 | 0.969 |
| REACHABLE in, 1 hop | 0.027 | 0.021 | 0.022 | 0.015 |
| REACHABLE both, 2 hops | 0.172 | 0.193 | 0.823 | 0.919 |
| shortest path, both directions | 0.134 | 0.160 | – | – |
| personalized PageRank, ε = 10⁻⁴ | 5.813 | 6.658 | – | – |
| personalized PageRank, ε = 10⁻⁵ | 44.6 | 51.8 | – | – |

The graph is identical on both machines: 9,998,469 edges, a 307 MB Strata file. Loading took 4.7 s on the Mac against 7.6 s on the VM. Latencies agree within about 20%, and the comparison with SQLite holds: at three hops Strata answers in 0.16 ms where SQLite takes 0.97 ms.

### Everything else

Every other number (posting-list sizes and times, Strata's BEIR evaluation, Engram's probes, scale and latency studies, the A/B runs themselves and the reader study) is taken from the committed result files, which were produced on the 4-core VM and are listed on each results page.

{: .note }
The first long-session A/B (`engram/results/long/*_v1_leaky`) is kept for the record only, and its own `NOTE.md` marks it invalid: Claude Code's auto-memory was not reset between runs, so runs within a lane were not independent.

## Requirements

- Rust (stable; 1.92 on the laptop above) and Python 3.
- For Engram's live tests, A/B runs and distillation: the [Claude Code](https://claude.com/claude-code) CLI. Those runs call a model and cost money; retrieval, scale and latency studies do not.
- Several scripts default to data under `/home/user/data/…` and some use `bash` 4 features (`strata/scripts/run_eval.sh` uses `declare -A`); pass paths explicitly and use a recent `bash` on macOS.
- Benchmarks are single-threaded unless a flag says otherwise. Run them on an otherwise idle machine.

## posting_bench

```sh
cd posting_bench
cargo test --release                               # conformance suite for every codec, and the algorithms
scripts/prepare_wikitext.sh data                   # WikiText-103 via Hugging Face (needs pyarrow)
scripts/run_experiments.sh data/wikitext103.txt    # first round: every codec and algorithm
scripts/run_designs.sh data/wikitext103.txt        # Pyramid, Loom and Entropic against the existing codecs
python3 scripts/build_report.py                    # report/report.html and report/designs.html
```

`.cargo/config.toml` builds with `-C target-cpu=native`. Every SIMD kernel has a portable fallback, but only the AVX-512 builds were timed, so timings on other CPUs are not comparable with the committed ones.

## Strata

The BEIR evaluation (about 4.8 GB of data with embeddings, about 4 hours):

```sh
cd strata
python3 scripts/prepare_beir.py /home/user/data/beir    # SciFact, FiQA, TREC-COVID, Quora + Cohere embeddings
cargo test --release                                    # components, crash recovery, end-to-end SQL
scripts/run_eval.sh                                     # every experiment in order; or name steps, e.g. strata_small text
python3 scripts/build_report.py                         # report/strata.html
```

`run_eval.sh` builds with `--features baselines` (SQLite with FTS5 and Tantivy) and writes JSON to `strata/results/` and logs to `strata/results/logs/`.

### Strata graphs

```sh
cd strata
# ogbn-arxiv and the papers' titles and abstracts, into /home/user/data/graph/arxiv
curl -O https://snap.stanford.edu/ogb/data/nodeproppred/arxiv.zip
curl -O https://snap.stanford.edu/ogb/data/misc/ogbn_arxiv/titleabs.tsv.gz
python3 -I scripts/prepare_arxiv.py /home/user/data/graph/arxiv
../engram/target/release/engram-bench embed --input /home/user/data/graph/arxiv/arxiv.jsonl --out /home/user/data/graph/arxiv/arxiv.f32
cargo build --release --features baselines --bin strata-graph-bench
target/release/strata-graph-bench load                  # holds out 1,000 papers of OGB's test split
target/release/strata-graph-bench latency --sqlite /home/user/data/graph/arxiv/edges.sqlite
target/release/strata-graph-bench quality --seeds 1,2,3,5,10,20,50
python3 scripts/graph_quality_stats.py results/graph/quality_arxiv.rows.jsonl.gz --out results/graph/quality_stats.json
target/release/strata-graph-bench synthetic --nodes 1000000 --db /tmp/synthetic.strata --sqlite /tmp/synthetic.sqlite --out /tmp/graph
python3 scripts/build_graph_report.py                   # report/graph.html
```

The synthetic graph needs no download. Without `--out`, results overwrite the committed files in `results/graph/`.

## Engram

```sh
cd engram
cargo build --release
target/release/engram fetch-model                        # static embedder, 129 MB
python3 scripts/download_memory_benchmarks.py --out data --max-m-bytes 0 --skip-original-s
python3 scripts/convert_memory_benchmarks.py --root data
```

The download needs `huggingface_hub` and `requests`; `--max-m-bytes 0` skips the 2.7 GB LongMemEval-M file, which no result here uses.

**Retrieval** (minutes, no model calls):

```sh
target/release/engram-bench retrieval --data data/longmemeval_s.jsonl --embedder static --ctx request --tag c400 --out /tmp/retrieval
target/release/engram-bench retrieval --data data/locomo.jsonl --embedder static --ctx request --tag c400 --out /tmp/retrieval
python3 scripts/fit_gate.py results/retrieval/longmemeval_s_static_request_c400.rows.jsonl --out /tmp/gate_static_c400.json
```

**Scale and latency** (a million records take about 3.5 GB):

```sh
target/release/engram-bench scale --data data/longmemeval_s.jsonl --sizes 10000,100000,1000000
target/release/engram-bench latency --data data/longmemeval_s.jsonl --sizes 10000,100000,1000000
```

**Probes on a real session** need a Claude Code transcript of your own; the probe files in `engram/probes/` were written for this repository's 26-hour session and only make sense against it:

```sh
target/release/engram-bench probes --transcript <session.jsonl> --probes probes/lost_probes.json --points 1952:4632,4633:5392
```

**Record structure and answers** (`engram-structure`, `scripts/extract_facts.py`, `scripts/reader_eval.py`) and **Claude Code with and without Engram** call a model; see the [Engram README](https://github.com/rkonow/engram/blob/main/engram/README.md#evaluation) for the full commands. The A/B statistics alone need nothing but the committed runs:

```sh
python3 scripts/ab_stats.py stockroom=results/ab/stockroom/runs.jsonl slotwise=results/ab/slotwise/runs.jsonl --out /tmp/ab_stats.json
```

## Building these pages

The pages are a Jekyll site with the [just-the-docs](https://just-the-docs.com/) theme, under `docs/`. GitHub Pages can publish it from that folder; the long-form reports in `docs/reports/` are copies of the HTML reports each crate builds.
