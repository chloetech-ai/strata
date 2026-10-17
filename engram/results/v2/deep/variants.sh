#!/bin/bash
# Query engine variants, clean environment (no inherited memory stores).
O=/home/claude/bench/v3
D=/home/claude/data/memory
run() { local B=$1; shift; echo "== $(date -u +%FT%TZ) $(basename $B) $*" >> $O/deep.log;
  env -i PATH="$PATH" HOME=$O/home ANTHROPIC_BASE_URL="$ANTHROPIC_BASE_URL" ENGRAM_MODELS=/home/claude/data/models "$B" deep "$@" >> $O/deep.log 2>&1 || echo "FAILED $*" >> $O/deep.log; }
OLD=/home/claude/bench/bin/engram-bench-deep
NEW=/home/claude/bench/bin/engram-bench-v3
run $OLD --data $D/longmemeval_s.jsonl --threads 8 --out $O --tag v2clean
run $NEW --data $D/longmemeval_s.jsonl --threads 8 --out $O --planner none --ranker model --tag rankonly
run $NEW --data $D/longmemeval_s.jsonl --threads 8 --out $O --planner model --ranker model --tag v3
run $OLD --data $D/locomo.jsonl --qevery 4 --threads 8 --out $O --tag v2clean_q4
run $NEW --data $D/locomo.jsonl --qevery 4 --threads 8 --out $O --planner none --ranker model --tag rankonly_q4
run $NEW --data $D/locomo.jsonl --qevery 4 --threads 8 --out $O --planner model --ranker model --tag v3_q4
echo "ALLDONE $(date -u +%FT%TZ)" >> $O/deep.log
