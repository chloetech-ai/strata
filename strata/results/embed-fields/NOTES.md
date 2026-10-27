# Which fields to embed: SciFact and TREC-COVID

`strata-embed-eval` loads a BEIR collection into a Strata table with three
embedded vector fields made by one model, from the title, the text, and the
title + text, and scores each (exact, flat vector search) against the
collection's relevance judgments, with BM25 over title + text, RRF of BM25 and
the title + text vector, and the collection's own Cohere embed-english-v3
vectors as references.

## Machine

- Apple M2, 8 cores, 24 GB, macOS 26.0.1 (25A362)
- Embedding on the CPU (candle for the BERT models, no GPU)

## Code measured

- `0c0ee30` (origin/claude/project-thread-2f1ge7, PR #15): "docs: embedded
  vector fields, choosing which fields to embed"
- Built with `cargo build --release --features bert --bin strata-embed-eval`
  in `strata/`, and `cargo build --release --features bert` in
  `strata-server/` (for `fetch-model`)

## Models

`strata-server fetch-model NAME` with `STRATA_MODELS=/Users/rkonow/code2/strata-models`:

| model | kind | dimensions | model.safetensors |
|---|---|---|---|
| potion-retrieval-32M | Model2Vec (static) | 512 | 129.2 MB |
| bge-small-en-v1.5 | BERT | 384 | 133.5 MB |
| snowflake-arctic-embed-m-v1.5 | BERT | 768 | 435.6 MB |

## Data

`STRATA_DATA=/Users/rkonow/code2/bench-data/beir` (meta.json, docs.jsonl,
doc_vecs.f32, queries.jsonl, query_vecs.f32, qrels.tsv per collection):

- scifact: 5,183 documents, 300 judged queries
- trec-covid: 171,332 documents, 50 judged queries

## Commands

From `strata/`, one run at a time, each through
`/Users/rkonow/code2/bench-work/scripts/embed-run.sh D M`, which waits (up to
10 minutes) for the 1-minute load average to drop below 6, then runs:

```bash
export STRATA_MODELS=/Users/rkonow/code2/strata-models
export STRATA_DATA=/Users/rkonow/code2/bench-data/beir
caffeinate -i target/release/strata-embed-eval --dataset D --model M \
  --out results/embed-fields/D-M.json 2>&1 | tee results/embed-fields/D-M.txt
```

`runs.log` has each run's start and end time, load averages and wall time.

## Run times

| dataset | model | load + embed 3 fields | wall |
|---|---|---|---|
| scifact | potion-retrieval-32M | 1.2 s | 2 s |
