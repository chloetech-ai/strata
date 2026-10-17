#!/bin/bash
# Reader eval on LongMemEval-S for query-engine variants: picks alone and Engram main's context (win400) + picks.
# Reuses none/win400/oracle answers in reader_v2 (same reader and judge: haiku).
set -e
OUT=/home/claude/bench/reader_v2
WIN=/home/claude/bench/baseline/structure/context_longmemeval_s_win400_req_vec-text_snip600_4000.jsonl
LME=/home/claude/data/memory/longmemeval_s.jsonl
ORACLE=/home/claude/data/memory/longmemeval_oracle.jsonl
CFG=()
for TAG in "$@"; do
  DEEP=/home/claude/bench/v3/longmemeval_s_deep_${TAG}.contexts.jsonl
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
  CFG+=(--config "deep_${TAG}=$DEEP" --config "win400_deep_${TAG}=$OUT/win400_deep_${TAG}.jsonl")
done
for pass in 1 2; do
  env -i PATH="$PATH" HOME="$OUT/home" ANTHROPIC_BASE_URL="${ANTHROPIC_BASE_URL:-}" READER_CWD="$OUT/cwd" \
    python3 -I /home/claude/engram/engram/scripts/reader_eval.py --data "$LME" --oracle "$ORACLE" \
    --config none=none --config "win400=$WIN" --config oracle=oracle --config "win400_deep_haiku=$OUT/win400_deep_haiku.jsonl" "${CFG[@]}" \
    --reader haiku --judge haiku --reader-label small --judge-label small --workers 8 --out "$OUT"
done
