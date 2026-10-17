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

| | LongMemEval-S, 100 q | BEAM 100K, 400 q | context per question, LME / BEAM (characters) | model calls to store |
|---|---|---|---|---|
| Hindsight, as AMB runs it | **88.0** [81, 94]; +9.0 [+2, +16], p = 0.02 | **46.7** [42.7, 50.7]; +3.7 [−0.4, +7.8], p = 0.09 | 169,900 / 120,900 | 26,170 calls for 100 histories, 4,463 for 20 conversations: about $100 and $24 |
| Hindsight, with a context the size of Engram's | 78.0 [70, 86]; −1.0 [−10, +7], p = 1 | 39.8 [35.8, 43.7]; −3.3 [−7.3, +0.7], p = 0.11 | 4,607 / 4,340 | the same banks |
| Engram, query engine (shipped) | 79.0 [71, 87] | 43.1 [39.0, 47.1] | 4,510 / 1,604 | none |
| mem0 | 79.0 [71, 87]; ±0 [−8, +8], p = 1 | 41.9 [37.8, 46.0]; −1.2 [−5.3, +3.0], p = 0.58 | 4,421 / 4,875 | 4,752 calls, $12.78; 2,127 calls, $4.32 |
| mem0 with its search extras | 78.0 [70, 86]; −1.0 [−10, +8], p = 1 | 41.4 [37.4, 45.5]; −1.6 [−5.9, +2.6], p = 0.46 | 4,455 / 4,913 | the same memories |
| Engram, plain search | 69.0 [60, 78]; −10.0 [−18, −2], p = 0.03 | 36.6 [32.6, 40.8]; −6.4 [−10.4, −2.4], p = 0.0015 | 3,527 / 3,383 | none |
| for reference: the whole history in context | 87.0 [80, 93]; +8.0 [+1, +16], p = 0.06 | not read here; an earlier run, on 60 questions where it fits: 48 against the query engine's 41 ([results page](../../../../docs/engram/results.md)) | 497,625 / – | none |
| for reference: only the sessions that hold the answer | 88.0 [81, 94]; +9.0 [0, +18], p = 0.08 | – | 27,548 / – | none |

*95% bootstrap intervals; differences are paired against Engram's query
engine on the same questions, with an exact McNemar test (LongMemEval) or a
sign-flip permutation test (BEAM)
([`scripts/compare_systems.py`](../../../scripts/compare_systems.py), output in
`*/comparison.json`).*

Per question, Engram's query engine spends $0.0012 (LongMemEval) and $0.0014
(BEAM) on its planner and ranker, and reading its context costs about $0.001.
Hindsight's recall makes no model call, but reading what it returns at AMB's
budgets costs $0.015 a question on LongMemEval and $0.011 on BEAM (70,000 and
49,000 reader tokens), and a recall took about 30 seconds here.

**Hindsight is ahead because it hands the reader much more.** At AMB's budgets
the reader gets 170,000 characters a question on LongMemEval and 121,000 on
BEAM, 38 and 75 times what Engram's query engine passes. On LongMemEval that
is as good as reading gets with this reader: the whole history in context
scores 87.0, and only the sessions that hold the answer 88.0. With a context the
size of Engram's (640 tokens of facts and no source chunks, about 4,600
characters) Hindsight is level with the query engine on LongMemEval (78.0,
one question fewer) and 3.3 points below it on BEAM (39.8; p = 0.11). It also
pays for its facts up front: about a dollar of model calls per history, eight
times mem0's, where Engram stores without a model call.

## Hindsight

[Hindsight](https://github.com/vectorize-io/hindsight) 1.0.0, the open-source
server (`hindsight-api`, Postgres with pgvector), run by
[`scripts/competitor_contexts.py hindsight`](../../../scripts/competitor_contexts.py)
the way vectorize-io's [agent-memory-benchmark](https://github.com/vectorize-io/agent-memory-benchmark)
(AMB, at aef3702) runs it: one bank per history, observations off, AMB's
retain mission on BEAM, one document per session (BEAM: per message batch, in
AMB's chat format) with the session's date and AMB's context line, and one
recall per question with budget `high`, up to 32,768 tokens of facts and
16,384 of source chunks on LongMemEval (12,288 and 8,192 on BEAM), the
question's date as the query time, deduplicated and formatted as AMB does.
Hindsight's model is Haiku through its own `claude-code` provider; its search
models are its defaults, BAAI/bge-small-en-v1.5 for embeddings and
cross-encoder/ms-marco-MiniLM-L-6-v2 for reranking, run as ONNX on the CPU
behind Hindsight's TEI provider
([`eval/competitors/hindsight/tei_shim.py`](../../../eval/competitors/hindsight/tei_shim.py)).
The smaller context is the same recall with `--recall-max-tokens 640
--chunk-tokens 0`.

This container had no copy of those two models while the banks were loading,
so the banks were filled against a stand-in embedder
([`hash_embed.py`](../../../eval/competitors/hindsight/hash_embed.py)) and
then moved with Hindsight's own `hindsight-admin export-bank` and
`import-bank` into a second database whose server has the real models
([`migrate.py`](../../../eval/competitors/hindsight/migrate.py),
[`serve.sh`](../../../eval/competitors/hindsight/serve.sh)). Import replays
everything in the retain pipeline after the model's extraction: it embeds
every fact again, resolves entities and rebuilds the links, without a model
call. Every recall ran against the second database.

Hindsight's extraction is the expensive part. Its model calls, as Hindsight
logged them (the `claude-code` provider reports no token counts, so these are
Hindsight's own estimates from text length), priced at Haiku's list prices
before any prompt caching:

| | LongMemEval-S, 100 histories | BEAM 100K, 20 conversations |
|---|---|---|
| model calls to store | 26,170 (median 259 a history; 1,035 more hit the account's usage limit and were retried) | 4,463 |
| facts stored | 61,322 | 17,719 |
| estimated tokens in / out | 63.5M / 7.4M | 12.9M / 2.2M |
| estimated cost | about $100 ($1.00 a history) | about $24 ($1.20 a conversation) |
| mem0's cost for the same histories | $12.78 | $4.32 |

A recall took about 30 seconds on this container's four CPUs, shared with the
other runs, almost all of it in the cross-encoder scoring 300 candidates.

By question type, LongMemEval-S (percent of questions answered correctly):

| type | n | Engram QE | mem0 | mem0 + extras | Hindsight | Hindsight, small context | Engram search |
|---|---|---|---|---|---|---|---|
| abstention | 6 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 | 100.0 |
| knowledge update | 14 | 92.9 | 92.9 | 92.9 | **100.0** | 92.9 | 92.9 |
| multi session | 25 | 56.0 | 60.0 | 56.0 | **72.0** | 60.0 | 40.0 |
| single session assistant | 11 | 90.9 | 81.8 | 81.8 | **100.0** | 72.7 | 81.8 |
| single session preference | 6 | 50.0 | 66.7 | 66.7 | **83.3** | 50.0 | 50.0 |
| single session user | 13 | 92.3 | 92.3 | 92.3 | **100.0** | 92.3 | 92.3 |
| temporal reasoning | 25 | **84.0** | 80.0 | 80.0 | **84.0** | **84.0** | 64.0 |

By ability, BEAM 100K (rubric score):

| ability | n | Engram QE | mem0 | mem0 + extras | Hindsight | Hindsight, small context | Engram search |
|---|---|---|---|---|---|---|---|
| abstention | 40 | 73.8 | 57.5 | 61.3 | 62.5 | 71.2 | **83.8** |
| contradiction resolution | 40 | 25.0 | 19.4 | **27.2** | 23.1 | 19.4 | 21.6 |
| event ordering | 40 | 21.8 | 22.8 | 16.0 | **28.6** | 17.7 | 16.0 |
| information extraction | 40 | 64.1 | 59.7 | 59.7 | **67.3** | 52.1 | 51.1 |
| instruction following | 40 | 53.8 | 42.5 | 36.9 | **60.0** | 47.5 | 26.9 |
| knowledge update | 40 | 40.0 | **56.2** | 55.0 | 41.2 | 43.8 | 43.1 |
| multi session reasoning | 40 | 33.8 | 32.5 | 37.4 | **37.9** | 33.0 | 29.8 |
| preference following | 40 | 66.2 | 71.5 | 70.0 | **71.7** | 67.5 | 53.8 |
| summarization | 40 | 17.0 | 23.6 | 22.3 | **37.5** | 20.3 | 8.5 |
| temporal reasoning | 40 | 35.0 | 33.1 | 28.7 | **37.5** | 25.0 | 31.9 |

At AMB's budgets Hindsight gains most where the answer is spread over many
messages: LongMemEval's multi-session questions (72 against 56) and BEAM's
summaries (37.5 against 17.0), where the whole history in context also leads
retrieval ([results page](../../../../docs/engram/results.md)). It loses BEAM's
abstention (62.5 against 73.8): with that much text in front of it, the reader
answers questions it should decline. With the small context those gains are
gone (multi-session 60, summaries 20.3).

**Hindsight's search models matter at the small budget.** Before the real
models were in the container, every bank was also imported into a third
database whose server embedded with all-MiniLM-L6-v2 and fused its rankings
with reciprocal-rank fusion instead of the cross-encoder. At AMB's budgets
that changed little (LongMemEval 86.0, BEAM 45.3); at the small budget it
cost 11 points on LongMemEval (67.0 against 78.0) and 10 on BEAM (30.0 against
39.8). These rows were read and graded in the same pass, as `hindsight-prov`
and `hindsight-prov-small`, and are not the reported result.

## Mem0

[mem0](https://github.com/mem0ai/mem0) 2.2.1, the open-source library, run by
[`scripts/competitor_contexts.py mem0`](../../../scripts/competitor_contexts.py):
one local Qdrant store per history, Haiku through `claude -p`
([`eval/competitors/mem0/mem0_mcp.py`](../../../eval/competitors/mem0/mem0_mcp.py)),
all-MiniLM-L6-v2 embeddings, one `add()` per session (LongMemEval) or per BEAM
message group with the session's date, and `search()` for the top 20, as
AMB runs it. This is mem0's default install, which searches by vector alone.

**mem0 with its search extras** (`mem0ai[nlp]` and `[extras]`: spaCy lemmas
and entities, fastembed's BM25 with Qdrant/bm25) turns on its keyword scoring
and entity boosts (`scripts/competitor_contexts.py mem0-hybrid`). Each history
is replayed into a fresh store through mem0's own `add()`, session by session,
with the model call replaced by the memories that session produced in the
first run: mem0's `add()` looks up existing memories by vector search alone,
so its extraction sees the same input with or without the extras, and only the
steps after it differ. The extras change nothing measurable here: 78.0 on
LongMemEval-S, one question fewer than mem0's default, and 41.4 on BEAM, half a
point lower, with contexts of the same size.

| | mem0 | Engram |
|---|---|---|
| model calls to store, LongMemEval-S | 4,752 for 100 histories, **$12.78** (as `claude -p` reported it) | none |
| model calls to store, BEAM 100K | 2,127 for 20 conversations, **$4.32** | none |
| model calls per question | none | the query engine's planner and ranker: $0.0012 a question on LongMemEval, $0.0014 on BEAM ($0.58 for its 400) |

mem0 is better than the query engine on BEAM's knowledge updates (56 against
40), preferences (72 against 66) and summaries (24 against 17), the kinds of
question that facts rewritten at write time should help; the query engine is
better on abstention (74 against 58), instruction following (54 against 43)
and extraction, which depend more on the exact message.

Haiku refused a few of mem0's extraction calls on LongMemEval (4 of 4,752,
for the usage policy); mem0's retry got each one through.

## Files

Per benchmark directory:

- `mem0.contexts.jsonl.gz`, `mem0-hybrid.contexts.jsonl.gz`,
  `hindsight.contexts.jsonl.xz` and `hindsight-small.contexts.jsonl.gz`: what
  each system returned for each question (`hindsight.contexts.jsonl.xz` is
  17 MB uncompressed on LongMemEval and 50 MB on BEAM);
- `mem0.ingest.jsonl`, `mem0-hybrid.ingest.jsonl`, `hindsight.ingest.jsonl`:
  per history, what was stored and how long it took;
- `hindsight.load.jsonl`: per history, Hindsight's model calls, its token
  estimates and the facts it stored, from its own request log;
- `answers.jsonl.gz`: every reader answer, all systems;
- `grades.jsonl.gz` (BEAM) or `verdicts_small.jsonl` (LongMemEval): the
  judge's grades;
- `summary.json`: the reader scripts' totals, context sizes and costs;
- `comparison.json`: the paired comparison with Engram's query engine.

The provisional search models' contexts (`hindsight-prov*`) are not kept;
their answers and grades are. On LongMemEval the reference rows are configs
`full` and `oracle` of [`scripts/reader_eval.py`](../../../scripts/reader_eval.py).
