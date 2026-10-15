# Query engine variants (2026-10-15, cloud container)

`engram-bench deep` over every LongMemEval-S question and every fourth LoCoMo
question, with Haiku for every model call, run from a clean environment
(`env -i`, so no inherited memory stores). Each `<tag>.json` has the evidence
numbers, cost, latency and tokens; `.rows.jsonl.gz` one row per question;
`.contexts.jsonl.gz` the picks given to the reader
(`results/v2/reader/summary_variants.json` has the answers).

| tag | build | planner | ranker | prompt caching |
|---|---|---|---|---|
| `haiku` | first build (PR #9) | model | model, first prompt | on; calls inherited ~7,000 tokens of the session's memory |
| `v2clean` | first build | model | model, first prompt | on |
| `v6old` | final code, first prompt | model | model, first prompt | off |
| `v6pref` | final code | model | model, first prompt + the preferences sentence (the default) | off |
| `v3` | intermediate | model | model, + preferences, no age warnings, advice reading (dropped) | on |
| `rankonly` | intermediate (`v3` prompt) | none | model | on |
| `local0`, `local5` | intermediate | none | local: a threshold (`local0`), the top 5 (`local5`, the shipped setting) | – |
| `v7rank`, `v7_q4`, `v7rank_q4` | final build | as named | final prompt | off |

`variants.sh` and `variants_prompt.sh` are the scripts that ran them. With
caching on, each one-shot prompt is written to the cache and never read back;
the cost columns of those runs are higher than the same calls cost now.
