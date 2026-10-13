# Graph recall: evaluation plan and how to run it

Work in progress on `claude/determined-clarke-idilbk`. This note lists what
the branch has, what is still to run, and the commands, for a session on
another machine (macOS included: every SIMD path in Strata has a portable
fallback, so it builds on Apple Silicon).

## In the branch

- Records have a `links` EDGES field (`src/record.rs`). Entities are records
  of kind `entity`, with ids `e:<normalised name>` (`entity_id`) or
  `f:<path>` for files. A store made before links is rewritten on open
  (`store.rs`, `migrate`; the old file is kept as `<name>.v1`).
- `Store::append` turns a draft's entities and files into links and creates
  the entity records it has not seen. Entities are never search results.
- The distiller asks for the entities of each note (`src/distill.rs`); a
  note links `about` its entities, `from` the turn it was written from, and
  `supersedes` the note it replaces.
- Graph recall (`recall.rs`, `GraphParams`, `expand`, `merge_graph`): the
  top hits and the entities nearest the prompt start a walk over the links
  (`Store::reach`, a Strata `REACHABLE` query through entities and notes);
  what it reaches, ranked by similarity, is fused into the hybrid ranking by
  reciprocal rank. It is off until the evaluations show it helps:
  `ENGRAM_GRAPH=1` (or `rank.graph.enabled` in the config) turns it on.
- `engram-structure` has a `turn+facts+graph` layout. Facts link to their
  turn and to their entities, and each graph variant in `graph_cfgs` is
  scored as `turn+facts+graph:<variant>`.
- `scripts/extract_facts.py` writes entities per fact (`--model` is required).
- `scripts/convert_memory_benchmarks.py --lme-m-every 5` streams
  LongMemEval_M (1.5M-token histories) into a jsonl of every fifth question.
- `scripts/long_session_ab.py`: one long Claude Code session per lane,
  compacted with `/compact` after each block of work. Every task forks it
  (`--resume ID --fork-session`). The lanes are `compact`,
  `compact+engram`, `compact+engram+graph` and `full` (never compacted).

## Still to run

1. `cargo test --release` in `engram/` (new tests: `entities_link_records_across_sessions`,
   `stores_without_links_are_migrated`, `entity_ids_meet`, `links_round_trip`).
2. Facts with entities for all LongMemEval_S sessions (19,208 unique) and
   LoCoMo (272): about $46 with a small model and 3 hours with 32 workers.
3. Retrieval: `turn`, `turn+facts` and `turn+facts+graph` with every graph
   variant on LongMemEval_S (500) and LoCoMo. Pick the variant on one half
   of the questions (md5(qid) parity) and report it on the other
   (`scripts/graph_recall_stats.py`). Facts are matched to sessions by
   content hash, since LoCoMo reuses session ids across conversations.
4. Reader accuracy, with a small reader and a large judge: none, Engram
   today, +facts, +facts+graph, oracle, and full history on 100 questions.
   The earlier answers in `results/reader*` are cached by config and question.
5. LongMemEval_M (every fifth question): Engram without facts. A full
   history does not fit in any context window.
6. The long-session A/B: 2 scenarios × 4 lanes × 10 tasks × 3 repeats.
7. The report.

## Commands

```sh
cd engram && cargo build --release
target/release/engram fetch-model                     # static embedder
export ENGRAM_MODELS=~/.engram/models                 # where fetch-model put it
D=~/data/memory
python3 scripts/download_memory_benchmarks.py --root $D && python3 scripts/convert_memory_benchmarks.py --root $D
python3 scripts/convert_memory_benchmarks.py --root $D --lme-m-every 5

python3 scripts/extract_facts.py --data $D/longmemeval_s.jsonl --every 1 --model <small model> --workers 16 --out $D/facts_entities.jsonl
python3 scripts/extract_facts.py --data $D/locomo.jsonl --every 1 --model <small model> --workers 4 --out $D/facts_entities_locomo.jsonl

target/release/engram-structure --data $D/longmemeval_s.jsonl --facts $D/facts_entities.jsonl \
  --layouts turn,turn+facts,turn+facts+graph --tag graph --out results/graph_recall \
  --dump turn/snip600/4000,turn+facts/snip600/4000,turn+facts+graph:g/snip600/4000

python3 scripts/reader_eval.py --data $D/longmemeval_s.jsonl --oracle $D/longmemeval_oracle.jsonl \
  --reader <small model> --judge <large model> --out results/reader_graph \
  --config none=none --config oracle=oracle \
  --config facts=results/graph_recall/context_longmemeval_s_graph_turn_facts_snip600_4000.jsonl \
  --config graph=results/graph_recall/context_longmemeval_s_graph_turn_facts_graph:g_snip600_4000.jsonl

python3 scripts/long_session_ab.py --model <small model> --work ~/long --repeats 3
```

Scripts default to the cloud machine's paths (`/home/user/...`); pass
`--root`, `--work`, `--oracle` and `ENGRAM_MODELS` as above.
