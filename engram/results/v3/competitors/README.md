# Other memory systems on LongMemEval-S and BEAM 100K

Measured on 2026-10-16 in the cloud container (4 CPUs, 16 GB), with Haiku as
every system's memory model, as the reader and as the judge. Each system gets
the same questions:

- **LongMemEval-S**, every fifth question (100 questions, one history of about
  50 sessions each), judged with LongMemEval's prompts by
  [`scripts/reader_eval.py`](../../../scripts/reader_eval.py);
- **BEAM 100K**, all 20 conversations and their 400 questions, graded against
  BEAM's rubric (0, 0.5 or 1 per item, averaged) by
  [`scripts/bench_eval.py`](../../../scripts/bench_eval.py).

Engram's contexts are the ones already committed (BEAM:
[`v2/benchmarks/beam_100k/query_engine.contexts.jsonl.gz`](../../v2/benchmarks/beam_100k/),
its `@base10` column for plain search; LongMemEval-S: `win400` and
`win400_deep_v6pref`, built by [`v2/reader/reader_v2.sh`](../../v2/reader/reader_v2.sh)).
They were read and graded again in the same run as the other systems, so every
row of a table below shares one reader and judge pass.

## Mem0

[mem0](https://github.com/mem0ai/mem0) 2.2.1, the open-source library, run by
[`scripts/competitor_contexts.py mem0`](../../../scripts/competitor_contexts.py):
one local Qdrant store per history, Haiku through `claude -p`
([`eval/competitors/mem0/mem0_mcp.py`](../../../eval/competitors/mem0/mem0_mcp.py)),
all-MiniLM-L6-v2 embeddings, one `add()` per session (LongMemEval) or per BEAM
message group with the session's date, and `search()` for the top 20, as
vectorize-io's agent-memory-benchmark runs it. This is mem0's default install,
which searches by vector alone: its keyword search and entity boosts come with
optional extras whose model files this container cannot download.

| | LongMemEval-S, 100 q | BEAM 100K, 400 q |
|---|---|---|
| Engram, query engine (shipped) | **79.0** [71.0, 87.0] | **43.1** [39.0, 47.1] |
| mem0 | **79.0** [71.0, 87.0]; same as Engram on 82, better on 9, worse on 9 (McNemar p = 1) | 41.9 [37.8, 46.0]; −1.2 [−5.3, +2.9], p = 0.58 |
| Engram, plain search | 69.0 [60.0, 78.0] | 36.6 [32.5, 40.7] |

*95% bootstrap intervals; differences are paired, on the same questions
([`scripts/compare_systems.py`](../../../scripts/compare_systems.py), output in
`*/comparison.json`).*

| | mem0 | Engram |
|---|---|---|
| model calls to store, LongMemEval-S | 4,752 for 100 histories, **$12.78** (as `claude -p` reported it) | none |
| model calls to store, BEAM 100K | 2,127 for 20 conversations, **$4.32** | none |
| model calls per question | none | the query engine's planner and ranker: $0.0012 a question on LongMemEval, $0.0014 on BEAM ($0.58 for its 400) |
| context per question, LongMemEval-S / BEAM | 4,421 / 4,875 characters | 4,510 / 1,604 characters |

By question type, BEAM 100K (Engram query engine / mem0 / Engram plain search):

| ability | Engram QE | mem0 | Engram search |
|---|---|---|---|
| abstention | 73.8 | 57.5 | **83.8** |
| contradiction resolution | **25.0** | 19.4 | 21.6 |
| event ordering | 21.8 | **22.8** | 16.0 |
| information extraction | **64.1** | 59.7 | 51.1 |
| instruction following | **53.8** | 42.5 | 26.9 |
| knowledge update | 40.0 | **56.2** | 43.1 |
| multi-session reasoning | **33.8** | 32.5 | 29.8 |
| preference following | 66.2 | **71.5** | 53.8 |
| summarization | 17.0 | **23.6** | 8.5 |
| temporal reasoning | **35.0** | 33.1 | 31.9 |

Haiku refused a few of mem0's extraction calls on LongMemEval (4 of 4,752,
for the usage policy); mem0's retry got each one through.

## Files

Per benchmark directory: `mem0.contexts.jsonl.gz` (what mem0 returned for each
question), `mem0.ingest.jsonl` (adds and time per history), `answers.jsonl.gz`
(every reader answer, all systems), `grades.jsonl.gz` or `verdicts_small.jsonl`
(the judge's grades), `summary.json` (the reader scripts' totals and costs) and
`comparison.json` (paired comparison with Engram's query engine).
