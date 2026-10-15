# Engram benchmark baseline: main at 787387d

These are Engram's committed benchmarks, rerun on the current `main` branch so that a changed checkout can be measured the same way later. Run on 2026-10-15 with `run_baseline.sh` from this directory.

- **Checkout:** `/home/claude/engram-main`, a worktree of github.com/rkonow/engram at `787387d` (Merge PR #8). It was clean and nothing in it was edited. The only writes were cargo's `target/` directories.
- **Machine:** a cloud VM with 4 cores (`Intel(R) Xeon(R) Processor @ 2.10GHz`), 15 GB RAM, Linux 6.18, rustc 1.97.0, Python 3.13, Claude Code CLI 2.1.300. Other agents shared the VM throughout, with their own `rustc` builds, `claude -p` sessions, bun and chroma-mcp. `logs/idle_<step>.log` records the CPU idle share every 30 s.
- **Data:**
  - `longmemeval_s.jsonl` and `longmemeval_oracle.jsonl` are the converted files the original runs used, supplied by the coordinator. huggingface.co is blocked by this environment's network policy, so they could not be downloaded here.
  - `locomo.jsonl` was converted here from `snap-research/locomo` at revision `3eb6f2c5`, fetched from GitHub with the repository's own converter functions (`tools/convert_locomo_only.py`). Its sha256 equals the supplied copy.
  - The embedder is potion-retrieval-32M, copied from the Mac. `model.safetensors` sha256 starts `07609e5bd33a`. `machine.txt` lists every input's hash.
- **Recorded:** "recorded" means the files committed under `engram/results/`. Their timings are from the M2 MacBook Air. "Recorded VM" means the cloud-VM timing files in git history (`1d55bd0`), copied to `recorded_vm/`.

## Summary

| step | reproduced? |
|---|---|
| 1. Retrieval, LongMemEval-S and LoCoMo, static embedder, 400-byte records | **Yes.** BM25 is identical to the last digit everywhere. Vectors are identical on LME and differ by ≤0.006 on LoCoMo (≤0.008 within a question type). Hybrid differs by ≤0.002 on LME (one LME preference question moves, 0.033 within that type of 30) and ≤0.007 on LoCoMo (≤0.011 within a type). Every reproduced value equals the docs' own MacBook rerun (`docs/reproduce.md`), so the committed `_c400` files are slightly older than the current code. This is not a machine effect. |
| 1. Gate statistics | **Yes**, within 0.009 (refit) and 0.002 (the gate in `recall.rs`). Refitting on the committed rows reproduces `gate_static_c400.json` exactly. |
| 2. Record structure (`engram-structure`, LME and LoCoMo) | **Yes, exactly.** All 888 metrics are bit-identical (rank R@1/5/10, MRR, evidence and answer in context per layout, presentation and budget). The reader's 4,000-character context file is identical for all 500 questions. |
| 3. Scale at 10k / 100k / 1M | **Quality yes, timings no.** Records, evidence records, bytes per record, needle recall@10 and pack4k are identical at all three sizes. Timings were measured under load and are not comparable. |
| 3. Hook latency (`engram-bench latency`) | **Not run.** The coordinator decided to rerun timed benchmarks on the Mac. |
| 4. Reader eval (none, Engram's 4,000-character context, oracle) on all 500 questions | **Yes, with a newer model.** "haiku" now resolves to a newer Haiku model than the recorded runs used. Judged by it, the numbers are 0.086 / 0.722 / 0.898. Re-judging the committed answers with the same judge gives 0.092 / 0.718 / 0.904, so the reader reproduces to within 0.006. The gap to the committed small-judge numbers (0.110 / 0.682 / 0.810) comes from the judge model. |
| 5. Unit tests | **Pass.** Strata: 216 passed, 10 ignored, 0 failed. Engram: 28 passed, 0 failed. The counts differ from the docs' 221/14 and 22, for reasons explained below. |

**Model spend:** $1.94 in total. The reader eval cost $1.76 ($1.58 reader, $0.18 judge), re-judging the committed answers cost $0.18, and a smoke test cost under $0.001. The reader eval used `env -i PATH HOME=/home/claude/bench/baseline/home ANTHROPIC_BASE_URL`.

**Wall time:** the baseline's steps took 85 min (`timings.tsv`): retrieval 2.3 min, structure 24 min, scale 12 min, reader 25 min, tests 21 min (including compiling the test targets). The session took about 1 h 50 min in all.

## 1. Retrieval

`engram-bench retrieval --embedder static --ctx request --tag c400`. LongMemEval-S scores 470 eligible questions and LoCoMo 1,536.

`engram-bench retrieval` reports no MRR. MRR comes from `engram-structure`, in section 2.

**By method.** Each cell is R@1 / R@5 / R@10 / R@20 / nDCG@10 / pack4k / pack8k.

| | recorded | reproduced |
|---|---|---|
| LongMemEval-S, BM25 | 0.311 / 0.751 / 0.851 / 0.913 / 0.522 / 0.768 / 0.862 | 0.311 / 0.751 / 0.851 / 0.913 / 0.522 / 0.768 / 0.862 |
| LongMemEval-S, vectors | 0.447 / 0.819 / 0.898 / 0.951 / 0.609 / 0.891 / 0.934 | 0.447 / 0.819 / 0.898 / 0.951 / 0.609 / 0.891 / 0.934 |
| LongMemEval-S, hybrid 0.3 + priors | 0.496 / 0.849 / **0.919** / 0.953 / **0.649** / 0.900 / 0.936 | 0.496 / 0.849 / **0.917** / 0.953 / **0.648** / 0.900 / 0.936 |
| LongMemEval-S, hybrid 0.5 + priors | 0.506 / 0.862 / 0.909 / 0.949 / 0.647 / 0.894 / 0.921 | 0.506 / 0.862 / 0.909 / 0.949 / 0.647 / 0.894 / 0.921 |
| LoCoMo, BM25 | 0.251 / 0.652 / 0.757 / 0.814 / 0.473 / 0.770 / 0.825 | 0.251 / 0.652 / 0.757 / 0.814 / 0.473 / 0.770 / 0.825 |
| LoCoMo, vectors | 0.150 / 0.303 / 0.391 / 0.481 / 0.238 / 0.410 / 0.510 | 0.150 / 0.304 / 0.391 / 0.485 / 0.238 / 0.411 / 0.516 |
| LoCoMo, hybrid 0.3 + priors | 0.236 / 0.443 / 0.550 / 0.656 / 0.350 / 0.571 / 0.688 | 0.236 / 0.450 / 0.557 / 0.660 / 0.353 / 0.577 / 0.692 |
| LoCoMo, hybrid 0.5 + priors | 0.320 / 0.615 / 0.710 / 0.794 / 0.468 / 0.727 / 0.810 | 0.322 / 0.616 / 0.711 / 0.796 / 0.470 / 0.728 / 0.809 |

The docs' MacBook rerun gives LME hybrid with priors 0.849 / 0.917 / 0.648 / 0.900, LoCoMo vectors 0.304 / 0.391 / 0.238 / 0.411 and LoCoMo hybrid with priors 0.450 / 0.557 / 0.353 / 0.577. These are exactly this run's numbers.

Wall time was 131 s for LME (recorded 178 s on the VM, 138 s on the M2) and 4 s for LoCoMo, under shared load.

**By question type.** Each cell is BM25 R@5 / R@10, vectors R@5 / R@10, hybrid 0.3 + priors R@5 / R@10.

| type (n) | recorded | reproduced |
|---|---|---|
| LME knowledge-update (72) | 0.889 / 0.958, 0.931 / 0.972, 0.958 / 0.972 | identical |
| LME multi-session (121) | 0.694 / 0.835, 0.860 / 0.942, 0.860 / 0.934 | identical |
| LME single-session-assistant (56) | 0.911 / 0.929, 0.821 / 0.893, 0.911 / 0.946 | identical |
| LME single-session-preference (30) | 0.300 / 0.467, 0.467 / 0.667, 0.567 / **0.767** | 0.300 / 0.467, 0.467 / 0.667, 0.567 / **0.733** (one question) |
| LME single-session-user (64) | 0.844 / 0.938, 0.844 / 0.891, 0.875 / 0.938 | identical |
| LME temporal-reasoning (127) | 0.717 / 0.819, 0.787 / 0.874, 0.803 / 0.890 | identical |
| LoCoMo multi-hop (282) | 0.496 / 0.656, 0.195 / 0.305, 0.326 / 0.468 | identical |
| LoCoMo open-domain (92) | 0.315 / 0.391, 0.163 / 0.228, 0.228 / 0.261 | identical |
| LoCoMo single-hop (841) | 0.742 / 0.839, 0.334 / 0.419, 0.493 / 0.603 | 0.742 / 0.839, 0.335 / 0.419, 0.503 / 0.612 |
| LoCoMo temporal (321) | 0.651 / 0.732, 0.355 / 0.439, 0.477 / 0.567 | 0.651 / 0.732, 0.358 / 0.442, 0.483 / 0.573 |

**Gate.** "Refit" is `scripts/fit_gate.py`, trained on the LME rows and tested on LoCoMo. "Code" is the default gate in `src/recall.rs`, applied to the same rows by `tools/gate_policy.py`. A changed checkout is measured with its own gate. Each cell is prompts injected / precision / evidence reaching the model / characters per prompt.

| | recorded | reproduced |
|---|---|---|
| LME, refit | 0.572 / 0.652 / 0.768 / 1,027 | 0.564 / 0.653 / 0.760 / 1,020 |
| LoCoMo, refit | 0.813 / 0.516 / 0.546 / 1,039 | 0.809 / 0.519 / 0.546 / 1,036 |
| LME, code gate | 0.572 / 0.653 / 0.770 / 1,029 | 0.571 / 0.655 / 0.770 / 1,030 |
| LoCoMo, code gate | 0.813 / 0.516 / 0.546 / 1,039 | 0.813 / 0.517 / 0.546 / 1,039 |
| always inject, LME and LoCoMo (precision / evidence) | 0.433 / 0.894, 0.560 / 0.727 | 0.433 / 0.894, 0.560 / 0.728 |

The docs' c400 claim is that evidence reaches the model for 77% of questions, 65% of injections hold evidence, and about 1,000 characters are injected per prompt. That claim holds.

Refitting on the committed rows (`extra/gate_static_c400_recomputed_from_committed_rows.json`) gives the same JSON content as `results/gate_static_c400.json`, every weight and rate.

## 2. Record structure

`engram-structure --data longmemeval_s.jsonl --dump "turn+req/snip600/4000,win400+req/vec-text/snip600/4000"`, and the same on `locomo.jsonl`. **All 888 metrics are identical at full precision.**

Each cell is hybrid R@10 / MRR / evidence in the 4,000-character context / answer in it (snip600). Recorded and reproduced are the same.

| layout | LongMemEval-S | LoCoMo |
|---|---|---|
| turn (1,500 B) | 0.923 / 0.688 / 0.911 / 0.850 | 0.645 / 0.423 / 0.717 / 0.737 |
| turn+req | 0.915 / 0.653 / 0.883 / 0.844 | 0.712 / 0.412 / 0.784 / 0.834 |
| win400+req (request in both keys) | 0.906 / 0.603 / 0.887 / 0.837 | 0.708 / 0.408 / 0.780 / 0.834 |
| win400+req/vec-text (Engram now) | 0.909 / 0.664 / 0.904 / 0.844 | 0.711 / 0.453 / 0.786 / 0.838 |
| session | 0.985 / 0.912 / 0.643 / 0.673 | 0.949 / 0.738 / 0.664 / 0.671 |

The docs' "MRR 0.664 → 0.603" and "R@10 0.985 against 64%" claims are reproduced. The full list is in `compare_recorded_full.md`.

Wall time was 1,252 s for LME (recorded 859 s on the VM) and 183 s for LoCoMo (recorded 89 s), under load.

## 3. Scale and latency

`engram-bench scale --sizes 10000,100000,1000000`. The 1M size took about 9 minutes and 3.1 GB of disk.

| | recorded (M2) | recorded VM (older 1,500 B records) | reproduced |
|---|---|---|---|
| records | 10,037 / 100,008 / 1,000,015 | 10,009 / 100,004 / 1,000,005 | 10,037 / 100,008 / 1,000,015 |
| needles' evidence records | 8,762 | 3,195 | 8,762 |
| needles in top 10 (recall10) | 0.798 / 0.649 / 0.436 | 0.777 / 0.521 / 0.447 | **0.798 / 0.649 / 0.436** |
| pack4k | 0.798 / 0.638 / 0.426 | 0.745 / 0.500 / 0.298 | **0.798 / 0.638 / 0.426** |
| bytes per record | 3,147 / 3,106 / 3,107 | 3,524 / 3,471 / 3,470 | **3,147 / 3,106 / 3,107** |
| recall query p50 (ms), *under load* | 1.27 / 1.67 / 5.71 | 3.14 / 4.02 / 11.5 | 2.54 / 3.29 / *100.9* |
| recall query p99 (ms), *under load* | 3.29 / 5.01 / 16.2 | 7.50 / 11.7 / 39.0 | 4.22 / 7.23 / *567* |
| full hook p50 / p99 (ms), *under load* | 14.4 / 17.2, 15.6 / 19.8, 26.1 / 42.7 | 9.4 / 15.4, 9.6 / 15.5, 17.2 / 67.7 | 7.4 / 17.4, 8.7 / 21.1, *43.4 / 134* |
| append p50 / p99 (ms), *under load* | 4.13 / 6.16, 4.18 / 5.62, 4.82 / 7.98 | 0.33 / 0.90, 0.36 / 1.47, 0.30 / 0.69 | 0.57 / 2.27, 0.34 / 2.27, 4.00 / 26.1 |
| static embedder (records/s), *under load* | 48,610 / 51,280 / 51,481 | 11,078 / 9,616 / 9,513 | 24,105 / 25,020 / 24,371 |

The quality rows are identical. **The timings were measured under load and are not comparable.** The 1M size ran with the CPU 0% idle, alongside other agents' `rustc` builds and `claude` sessions, so its query time is 100 ms instead of 6–11 ms. The 10k and 100k sizes started at 56–69% idle.

The hook latency study (`engram-bench latency`) was not run, as the coordinator directed. Its step is in `run_baseline.sh` as `latency`, but it is not in the default steps.

## 4. Reader eval

`scripts/reader_eval.py` ran on all 500 LongMemEval-S questions. The cost estimate was about $2–3, under the $10 limit, so no subset was used.

The configs were:
- **none:** no memory.
- **win400:** Engram's 4,000-character context, from this checkout's `engram-structure` dump. It is identical to the committed `context_longmemeval_s_win_dump_win400_req_vec-text_snip600_4000`.
- **oracle:** the evidence sessions.

Reader and judge are both `--model haiku`. "Recorded small" and "recorded large" are the committed `verdicts_small` and `verdicts_large` grades of the committed answers. The headline numbers in the docs (7%, 73%, 92%) are the large-judge ones.

| accuracy | recorded, small judge | recorded, large judge | reproduced (new answers, haiku judge) | committed answers re-judged by today's haiku |
|---|---|---|---|---|
| none | 0.110 | 0.066 | **0.086** | 0.092 |
| win400 (Engram's 4,000 characters) | 0.682 | 0.730 | **0.722** | 0.718 |
| oracle | 0.810 | 0.918 | **0.898** | 0.904 |

The reproduced accuracy by question type for win400 is:

| type | accuracy |
|---|---|
| abstention | 0.967 |
| knowledge-update | 0.861 |
| multi-session | 0.529 |
| single-session-assistant | 0.750 |
| single-session-preference | 0.833 |
| single-session-user | 0.844 |
| temporal-reasoning | 0.669 |

The full set is in `compare_recorded_full.md` under `reader.small.*.type.*`.

Mean reader tokens were 489 / 1,820 / 9,737, against 1,419 / 2,750 / 10,667 recorded. The constant gap of 930 tokens is the newer CLI's smaller prompt overhead. The reader cost $0.09 / $0.35 / $1.15, against $0.20 / $0.44 / $1.24 recorded.

`haiku` resolved to a newer Haiku model today. The model behind the original runs' "small" label is not recorded in the repository. That is why both the reader's new answers and the re-judged old answers are shown.

## 5. Unit tests

`cargo test --release`:

| crate | passed | failed | ignored |
|---|---|---|---|
| Strata | 216 | 0 | 10 |
| Engram | 28 | 0 | 0 |

The docs say Strata 221 with 14 ignored. Those counts are from aarch64, where 14 NEON-only tests (4 of them ignored) run and 4 AVX-only ones do not. Engram now has 28 tests, against 22 in the docs: tests were added after the docs' count.

## Docs numbers the current code cannot reproduce

The docs' headline LongMemEval-S retrieval figures come from `longmemeval_s_static_round.json`, made before records were cut at 400 bytes. These are 90% at R@10, 88% for BM25 and vectors, and 87% in the 4,000-character injection. The same is true of the LoCoMo figures (0.651, 0.382, 0.607, and BGE 0.678), the "0.760 → 0.835" and 0.50 by-type figures, and the first gate table (55% / 59% / 67%).

The current code always cuts at 400 bytes. `docs/reproduce.md` itself says the `_c400` files are the ones it reproduces, so the reproduced tables above compare against those.

## Files

| path | contents |
|---|---|
| `run_baseline.sh` | Reruns the same steps for any checkout: `./run_baseline.sh CHECKOUT OUT_DIR [STEPS]`. The default steps are `build,retrieval,structure,scale,reader,tests,compare`. Inputs are set with `DATA`, `ENGRAM_MODELS`, `SCALE_SIZES`, `READER_EVERY` and the other variables described in its header. It writes `compare_baseline_{key,full}.md` against this directory. |
| `tools/` | `metrics.py` (flatten and compare, `--key` for the headline rows), `gate_policy.py`, `context_diff.py`, `compare_retrieval.py`, `convert_locomo_only.py` |
| `retrieval/`, `gate/`, `structure/`, `scale/`, `reader/`, `tests/` | raw outputs (JSON, rows, context dumps, answers and verdicts, test logs) |
| `metrics.json`, `compare_recorded_key.md`, `compare_recorded_full.md` | every number flattened, and recorded against reproduced (key rows, then all 1,885) |
| `machine.txt`, `timings.tsv`, `logs/` | machine, checkout and input hashes; step wall times; step logs and CPU idle samples |
| `recorded_vm/` | the cloud-VM timing files from git history |
| `extra/` | the LoCoMo hash-embedder check run before the model arrived (BM25 identical), the 1M scale output under load, the re-judged committed answers, and the first Strata test run |

To measure the modified checkout against this baseline:

```sh
/home/claude/bench/baseline/run_baseline.sh /home/claude/engram /home/claude/bench/after
```

It builds `engram/target` there and writes `/home/claude/bench/after/compare_baseline_key.md`.

`SCALE_SIZES=10000,100000` skips the 1M store. On a busy machine the timings are not comparable between runs. The quality numbers are.
