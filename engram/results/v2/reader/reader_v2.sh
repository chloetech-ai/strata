#!/bin/bash
# Reader eval on LongMemEval-S: Engram main's context (win400) vs the query engine's picks.
# none/win400/oracle answers are reused from the baseline run (same reader and judge: haiku).
set -e
TAG=${1:-haiku}
OUT=/home/claude/bench/reader_v2
DEEP=/home/claude/bench/deep/longmemeval_s_deep_${TAG}.contexts.jsonl
WIN=/home/claude/bench/baseline/structure/context_longmemeval_s_win400_req_vec-text_snip600_4000.jsonl
LME=/home/claude/data/memory/longmemeval_s.jsonl
ORACLE=/home/claude/data/memory/longmemeval_oracle.jsonl
python3 -I - "$WIN" "$DEEP" "$OUT/win400_deep_${TAG}.jsonl" <<'PY'
import json, sys
win = {}
for l in open(sys.argv[1]):
    r = json.loads(l); win[r["qid"]] = r["context"]
with open(sys.argv[3], "w") as f:
    for l in open(sys.argv[2]):
        r = json.loads(l)
        extra = r["context"]
        ctx = win.get(r["qid"], "") + ("\n\nRecords a search over the whole memory picked for this question:\n\n" + extra if extra else "")
        f.write(json.dumps({"qid": r["qid"], "context": ctx}) + "\n")
PY
for pass in 1 2; do
  env -i PATH="$PATH" HOME="$OUT/home" ANTHROPIC_BASE_URL="${ANTHROPIC_BASE_URL:-}" READER_CWD="$OUT/cwd" \
    python3 -I /home/claude/engram/engram/scripts/reader_eval.py --data "$LME" --oracle "$ORACLE" \
    --config none=none --config "win400=$WIN" --config oracle=oracle \
    --config "deep_${TAG}=$DEEP" --config "deep_reading_${TAG}=$DEEP@reading" --config "win400_deep_${TAG}=$OUT/win400_deep_${TAG}.jsonl" \
    --reader haiku --judge haiku --reader-label small --judge-label small --workers 8 --out "$OUT"
done
