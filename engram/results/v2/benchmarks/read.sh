#!/bin/bash
# Reader + each benchmark's own scoring for the new benchmarks, as each query-engine run finishes.
O=/home/claude/bench/v3/new
D=/home/claude/data/memory
S=/home/claude/engram/engram/scripts/bench_eval.py
ev() { local bench=$1 ds=$2; shift 2
  until [ -f $O/${ds}_deep_v3.json ]; do sleep 30; done
  echo "== $(date -u +%FT%TZ) $bench $ds $*" >> $O/read.log
  for pass in 1 2; do
    env -i PATH="$PATH" HOME=/home/claude/bench/v3/home ANTHROPIC_BASE_URL="$ANTHROPIC_BASE_URL" READER_CWD=/tmp \
      python3 -I $S --bench $bench --data $D/$ds.jsonl --config none=none --config hybrid=$O/${ds}_deep_v3.contexts.jsonl@base10 \
      --config engine=$O/${ds}_deep_v3.contexts.jsonl "$@" --reader haiku --judge haiku --workers 6 --out $O/read_$ds >> $O/read.log 2>&1
  done
}
ev mab mab_factconsolidation --config full=full --full-every 2
ev beam beam_100k --config full=full --full-every 4
ev beam beam_500k
ev beam beam_1m
echo "ALLDONE $(date -u +%FT%TZ)" >> $O/read.log
