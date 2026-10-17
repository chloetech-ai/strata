#!/bin/bash
# New benchmarks: query engine (default: planner + ranker) over each history; contexts for the reader.
O=/home/claude/bench/v3/new
D=/home/claude/data/memory
B=/home/claude/bench/bin/engram-bench-v4
run() { echo "== $(date -u +%FT%TZ) $*" >> $O/deep.log;
  env -i PATH="$PATH" HOME=/home/claude/bench/v3/home ANTHROPIC_BASE_URL="$ANTHROPIC_BASE_URL" ENGRAM_MODELS=/home/claude/data/models "$B" deep "$@" >> $O/deep.log 2>&1 || echo "FAILED $*" >> $O/deep.log; }
until [ -f $O/mab_factconsolidation_deep_v3.json ]; do sleep 20; done
run --data $D/beam_100k.jsonl --threads 4 --out $O --tag v3
run --data $D/longmemeval_m_every5.jsonl --threads 4 --out $O --tag v3
run --data $D/beam_500k.jsonl --threads 4 --out $O --tag v3
run --data $D/beam_1m.jsonl --threads 4 --out $O --tag v3
echo "ALLDONE $(date -u +%FT%TZ)" >> $O/deep.log
