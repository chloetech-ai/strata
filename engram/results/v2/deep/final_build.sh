#!/bin/bash
# Final build (ranker prompt = v6pref, planner SQL fix): evidence and cost on LongMemEval-S (ranker only)
# and LoCoMo every 4th question (default and ranker only). Prompt caching off.
O=/home/claude/bench/v3
D=/home/claude/data/memory
B=/home/claude/bench/bin/engram-bench-v7
until [ -x $B ]; do sleep 20; done
run() { echo "== $(date -u +%FT%TZ) $(basename $B) $*" >> $O/deep.log;
  env -i PATH="$PATH" HOME=$O/home ANTHROPIC_BASE_URL="$ANTHROPIC_BASE_URL" ENGRAM_MODELS=/home/claude/data/models "$B" deep "$@" >> $O/deep.log 2>&1 || echo "FAILED $*" >> $O/deep.log; }
run --data $D/longmemeval_s.jsonl --threads 6 --out $O --planner none --ranker model --tag v7rank
run --data $D/locomo.jsonl --qevery 4 --threads 6 --out $O --tag v7_q4
run --data $D/locomo.jsonl --qevery 4 --threads 6 --out $O --planner none --ranker model --tag v7rank_q4
echo "ALLDONE $(date -u +%FT%TZ)" >> $O/v7.log
