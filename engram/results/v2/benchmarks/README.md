# Newer memory benchmarks (2026-10-15, cloud container)

MemoryAgentBench FactConsolidation, BEAM (100K, 500K, 1M) and LongMemEval-M,
converted by `scripts/convert_new_benchmarks.py` (LongMemEval-M by
`scripts/convert_memory_benchmarks.py`, every fifth question). Each history
is its own store, built by `engram-bench deep`, which also writes two reader
contexts per question: Engram's hybrid search, its top 10 records
(`@base10`), and the query engine's picks. A Haiku reader answers from each
(`scripts/bench_eval.py`; LongMemEval-M through `scripts/reader_eval.py`),
with "none" (no context) and, where the history fits in 600,000 characters,
"full" (the whole history, every second FactConsolidation question and every
fourth BEAM 100K question).

Scoring is each benchmark's own: FactConsolidation, an accepted answer
contained in the reply after normalisation (MemoryAgentBench's
`rag_agent` prompt for the reader); BEAM, its unified rubric judge, 0, 0.5
or 1 per rubric item, averaged (its `answer_generation_for_rag` prompt for
the reader); LongMemEval-M, LongMemEval's judge prompts.

Per directory: `query_engine.json` (evidence, cost, latency, tokens),
`query_engine.rows.jsonl.gz`, `query_engine.contexts.jsonl.gz` (both
contexts), `reader_summary.json` and the reader's answers and grades.
`longmemeval_m/same_questions_on_s/` has the same 100 questions over
LongMemEval-S's shorter histories.

Builds: the query engine ran with the intermediate build whose ranker prompt
was later cut back (tag `v3` in `results/v2/deep/VARIANTS.md`; on
LongMemEval-S it scored 79.4% with the recall against 81.8% for the shipped
prompt). FactConsolidation's run had prompt caching on, the others off. The
account's usage limit (15:47-15:50 UTC) failed 4 of BEAM 500K's 700 query
engine calls; LongMemEval-M lost 5 and was run again (`v3r`). Scripts:
`query_engine*.sh`, `read*.sh`.
