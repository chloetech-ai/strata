#!/bin/bash
# Ranker prompt ablation on LongMemEval-S, current code (prompt caching off):
# v6old = the PR #9 ranker prompt; v6pref = that prompt plus only the preferences sentence.
O=/home/claude/bench/v3
D=/home/claude/data/memory
run() { local B=$1; shift; echo "== $(date -u +%FT%TZ) $(basename $B) $*" >> $O/deep.log;
  env -i PATH="$PATH" HOME=$O/home ANTHROPIC_BASE_URL="$ANTHROPIC_BASE_URL" ENGRAM_MODELS=/home/claude/data/models "$B" deep "$@" >> $O/deep.log 2>&1 || echo "FAILED $*" >> $O/deep.log; }
run /home/claude/bench/bin/engram-bench-v6old --data $D/longmemeval_s.jsonl --threads 6 --out $O --tag v6old
run /home/claude/bench/bin/engram-bench-v6pref --data $D/longmemeval_s.jsonl --threads 6 --out $O --tag v6pref
/home/claude/bench/v3/reader.sh v6old v6pref >> $O/reader_v6.log 2>&1
echo "ALLDONE $(date -u +%FT%TZ)" >> $O/reader_v6.log
