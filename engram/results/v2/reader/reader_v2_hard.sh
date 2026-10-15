#!/bin/bash
set -e
OUT=/home/claude/bench/reader_v2_hard
T=haiku_sonnet_hard
DEEP=/home/claude/bench/deep/longmemeval_s_deep_${T}.contexts.jsonl
WIN=/home/claude/bench/baseline/structure/context_longmemeval_s_win400_req_vec-text_snip600_4000.jsonl
python3 -I - "$WIN" "$DEEP" "$OUT/win400_deep_${T}.jsonl" <<'PY'
import json, sys
win = {json.loads(l)["qid"]: json.loads(l)["context"] for l in open(sys.argv[1])}
with open(sys.argv[3], "w") as f:
    for l in open(sys.argv[2]):
        r = json.loads(l)
        extra = r["context"]
        ctx = win.get(r["qid"], "") + ("\n\nRecords a search over the whole memory picked for this question:\n\n" + extra if extra else "")
        f.write(json.dumps({"qid": r["qid"], "context": ctx}) + "\n")
PY
for pass in 1 2; do
  env -i PATH="$PATH" HOME="$OUT/home" ANTHROPIC_BASE_URL="${ANTHROPIC_BASE_URL:-}" READER_CWD="$OUT/cwd" \
    python3 -I /home/claude/engram/engram/scripts/reader_eval.py --data /home/claude/data/memory/longmemeval_s.jsonl --oracle /home/claude/data/memory/longmemeval_oracle.jsonl \
    --config none=none --config "win400=$WIN" --config "deep_haiku=/home/claude/bench/deep/longmemeval_s_deep_haiku.contexts.jsonl" \
    --config "win400_deep_haiku=/home/claude/bench/reader_v2/win400_deep_haiku.jsonl" \
    --config "deep_${T}=$DEEP" --config "win400_deep_${T}=$OUT/win400_deep_${T}.jsonl" \
    --reader haiku --judge haiku --reader-label small --judge-label small --every 5 --workers 8 --out "$OUT"
done
