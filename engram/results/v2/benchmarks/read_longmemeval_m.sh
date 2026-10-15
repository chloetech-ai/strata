#!/bin/bash
# LongMemEval-M (100 questions, ~500 sessions each) against the same questions on LongMemEval-S.
O=/home/claude/bench/v3/new
# Re-run of the query engine on LongMemEval-M: the first run (tag v3) lost 5 questions to the account usage limit at 15:47-15:50 UTC.
D=/home/claude/data/memory
env -i PATH="$PATH" HOME=/home/claude/bench/v3/home ANTHROPIC_BASE_URL="$ANTHROPIC_BASE_URL" ENGRAM_MODELS=/home/claude/data/models /home/claude/bench/bin/engram-bench-v4 deep --data $D/longmemeval_m_every5.jsonl --threads 4 --out $O --tag v3r >> $O/deep_lme.log 2>&1
for pass in 1 2; do
  env -i PATH="$PATH" HOME=/home/claude/bench/v3/home ANTHROPIC_BASE_URL="$ANTHROPIC_BASE_URL" READER_CWD=/tmp \
    python3 -I /home/claude/engram/engram/scripts/reader_eval.py --data $D/longmemeval_m_every5.jsonl --oracle $D/longmemeval_oracle.jsonl \
    --config none=none --config oracle=oracle --config hybrid_m=$O/longmemeval_m_deep_v3r.contexts.jsonl@base10 --config engine_m=$O/longmemeval_m_deep_v3r.contexts.jsonl \
    --reader haiku --judge haiku --reader-label small --judge-label small --workers 6 --out $O/read_lme_m >> $O/read_lme.log 2>&1
  env -i PATH="$PATH" HOME=/home/claude/bench/v3/home ANTHROPIC_BASE_URL="$ANTHROPIC_BASE_URL" READER_CWD=/tmp \
    python3 -I /home/claude/engram/engram/scripts/reader_eval.py --data $D/longmemeval_s_mq.jsonl --oracle $D/longmemeval_oracle.jsonl \
    --config hybrid_s=/home/claude/bench/v3/longmemeval_s_deep_v3.contexts.jsonl@base10 --config engine_s=/home/claude/bench/v3/longmemeval_s_deep_v3.contexts.jsonl \
    --reader haiku --judge haiku --reader-label small --judge-label small --workers 6 --out $O/read_lme_s100 >> $O/read_lme.log 2>&1
done
echo "ALLDONE $(date -u +%FT%TZ)" >> $O/read_lme.log
