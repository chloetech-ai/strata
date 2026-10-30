#!/usr/bin/env bash
# Reruns Engram's benchmarks for one engram checkout, the same steps as the
# baseline in /home/claude/bench/baseline (REPORT.md there):
#
#   run_baseline.sh ENGRAM_CHECKOUT OUT_DIR [STEPS]
#
#   ENGRAM_CHECKOUT  a clone of github.com/rkonow/strata (the directory holding engram/ and strata/)
#   OUT_DIR          where every output goes (created; existing reader answers there are reused)
#   STEPS            comma list, default: build,retrieval,structure,scale,reader,tests,compare
#                    (the steps of the baseline; `latency` is available but was not run there)
#
# Steps:
#   build      cargo build --release --bins in engram/ (strata is built through its path dependency)
#   retrieval  engram-bench retrieval, static embedder, --ctx request, 400-byte records (tag c400), on
#              LongMemEval-S (cleaned) and LoCoMo; then scripts/fit_gate.py (refit on the LME rows, tested
#              on LoCoMo) and tools/gate_policy.py (the checkout's own default gate on the same rows)
#   structure  engram-structure on LongMemEval-S (all layouts, plus the two --dump context files the
#              reader uses) and on LoCoMo
#   scale      engram-bench scale at $SCALE_SIZES (needles in filler; hooks through the real binary)
#   latency    engram-bench latency at $LATENCY_SIZES (phases, write-then-query stall, hook latency)
#   reader     scripts/reader_eval.py on LongMemEval-S: none, win400 (Engram's 4,000-character context
#              from this checkout's engram-structure dump) and oracle; reader and judge $READER_MODEL /
#              $JUDGE_MODEL through an isolated `claude` (env -i PATH HOME ANTHROPIC_BASE_URL)
#   tests      cargo test --release in strata/ and engram/ (counts in tests/counts.json)
#   compare    tools/metrics.py: this run against the checkout's committed results, and against
#              $BASELINE when that is another run directory
#
# Environment (defaults in brackets):
#   DATA [/home/claude/data/memory]         longmemeval_s.jsonl, longmemeval_oracle.jsonl, locomo.jsonl
#   ENGRAM_MODELS [/home/claude/data/models] potion-retrieval-32M/{config.json,tokenizer.json,model.safetensors}
#   SCALE_SIZES, LATENCY_SIZES [10000,100000,1000000]
#   READER_MODEL, JUDGE_MODEL [haiku]   READER_EVERY [1] (2 = every other question)   READER_WORKERS [8]
#   READER_HOME [$OUT_DIR/home]          HOME for the isolated claude CLI
#   BASELINE [/home/claude/bench/baseline]  run directory to compare against in the compare step
#   IDLE_PCT [55], IDLE_MAX_WAIT [300]      scale and latency wait (up to IDLE_MAX_WAIT s) for no compiler
#                                           and the CPU at least IDLE_PCT% idle on three checks in a row
#
# Quality numbers (recall, nDCG, gate, structure, scale recall, reader accuracy) do not depend on
# the machine or its load. Timings do: on a shared container they are not comparable between runs
# (the baseline's were measured under load); compare them only from runs on an idle machine.
#
# Each step logs to OUT_DIR/logs/<step>.log, its wall time to OUT_DIR/timings.tsv, and the CPU idle
# share every 30 s while it runs to OUT_DIR/logs/idle_<step>.log. Nothing here writes outside OUT_DIR
# except cargo's target/ directories inside the checkout.
set -uo pipefail

if [ $# -lt 2 ]; then
    sed -n '2,40p' "$0"
    exit 2
fi
CHECKOUT=$(realpath "$1")
OUT=$(realpath -m "$2")
STEPS=${3:-build,retrieval,structure,scale,reader,tests,compare}
TOOLS=$(dirname "$(realpath "$0")")/tools

DATA=${DATA:-/home/claude/data/memory}
export ENGRAM_MODELS=${ENGRAM_MODELS:-/home/claude/data/models}
SCALE_SIZES=${SCALE_SIZES:-10000,100000,1000000}
LATENCY_SIZES=${LATENCY_SIZES:-10000,100000,1000000}
READER_MODEL=${READER_MODEL:-haiku}
JUDGE_MODEL=${JUDGE_MODEL:-haiku}
READER_EVERY=${READER_EVERY:-1}
READER_WORKERS=${READER_WORKERS:-8}
READER_HOME=${READER_HOME:-$OUT/home}
BASELINE=${BASELINE:-/home/claude/bench/baseline}
IDLE_PCT=${IDLE_PCT:-55}
IDLE_MAX_WAIT=${IDLE_MAX_WAIT:-300}

ENG=$CHECKOUT/engram
BIN=$ENG/target/release
LME=$DATA/longmemeval_s.jsonl
ORACLE=$DATA/longmemeval_oracle.jsonl
LOCOMO=$DATA/locomo.jsonl

mkdir -p "$OUT"/{logs,tmp,retrieval,gate,structure,scale,latency,reader,tests,engram_home}
# Temporary stores (engram-bench, engram-structure) go under OUT, not /tmp.
export TMPDIR=$OUT/tmp
export ENGRAM_HOME=$OUT/engram_home

has() { case ",$STEPS," in *",$1,"*) return 0 ;; *) return 1 ;; esac; }
say() { echo "[$(date '+%F %T')] $*" | tee -a "$OUT/logs/run.log"; }

[ -d "$ENG" ] && [ -d "$CHECKOUT/strata" ] || { echo "not an engram checkout: $CHECKOUT" >&2; exit 2; }

# Machine and checkout; a later invocation into the same OUT_DIR appends.
[ -f "$OUT/machine.txt" ] && echo "--- invoked again" >> "$OUT/machine.txt"
{
    echo "date: $(date -u '+%FT%TZ')"
    echo "host: $(hostname)"
    echo "cpu: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"
    echo "cores: $(nproc)"
    echo "memory: $(free -g | awk '/Mem:/ {print $2 " GB"}')"
    echo "kernel: $(uname -sr)"
    echo "rustc: $(rustc --version 2>/dev/null)"
    echo "python: $(python3 --version 2>&1)"
    echo "claude: $(claude --version 2>/dev/null)"
    echo "checkout: $CHECKOUT"
    echo "commit: $(git -C "$CHECKOUT" rev-parse HEAD 2>/dev/null)"
    echo "branch: $(git -C "$CHECKOUT" rev-parse --abbrev-ref HEAD 2>/dev/null)"
    echo "uncommitted changes (git status --short):"
    git -C "$CHECKOUT" status --short 2>/dev/null | sed 's/^/  /'
    echo "diff against HEAD, sha256: $(git -C "$CHECKOUT" diff HEAD 2>/dev/null | sha256sum | cut -c1-16)"
    echo "data:"
    for f in "$LME" "$ORACLE" "$LOCOMO" "$ENGRAM_MODELS"/potion-retrieval-32M/*; do
        [ -f "$f" ] && echo "  $(sha256sum "$f" | cut -c1-16)  $(stat -c %s "$f")  $f"
    done
    echo "steps: $STEPS"
    echo "env: SCALE_SIZES=$SCALE_SIZES LATENCY_SIZES=$LATENCY_SIZES READER_MODEL=$READER_MODEL JUDGE_MODEL=$JUDGE_MODEL READER_EVERY=$READER_EVERY"
} >> "$OUT/machine.txt"
[ -f "$OUT/timings.tsv" ] || printf 'step\tstart\tseconds\texit\n' > "$OUT/timings.tsv"

# Logs the CPU idle share (from /proc/stat) and the busiest processes every 30 s.
sample_idle() {
    local log=$1 a b
    read -r _ a1 a2 a3 a4 a5 a6 a7 a8 _ < /proc/stat
    while sleep 30; do
        read -r _ b1 b2 b3 b4 b5 b6 b7 b8 _ < /proc/stat
        local tot=$(( (b1+b2+b3+b4+b5+b6+b7+b8) - (a1+a2+a3+a4+a5+a6+a7+a8) ))
        local idle=$(( (b4+b5) - (a4+a5) ))
        [ "$tot" -gt 0 ] && echo "$(date '+%T') idle $(( 100 * idle / tot ))% load $(cut -d' ' -f1-3 /proc/loadavg) | $(ps -eo pcpu=,comm= --sort=-pcpu | head -3 | awk '{printf "%s %s%%  ", $2, $1}')" >> "$log"
        a1=$b1 a2=$b2 a3=$b3 a4=$b4 a5=$b5 a6=$b6 a7=$b7 a8=$b8
    done
}

# Runs a step: output to logs/<name>.log, wall time and exit code to timings.tsv.
step() {
    local name=$1; shift
    say "start $name"
    local t0; t0=$(date +%s.%N)
    sample_idle "$OUT/logs/idle_$name.log" & local sampler=$!
    "$@" >> "$OUT/logs/$name.log" 2>&1
    local rc=$?
    kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null
    local s; s=$(echo "$(date +%s.%N) - $t0" | bc)
    printf '%s\t%s\t%.1f\t%s\n' "$name" "$(date -d @"${t0%.*}" '+%F %T')" "$s" "$rc" >> "$OUT/timings.tsv"
    say "end $name: exit $rc in $(printf '%.0f' "$s") s"
    return $rc
}

# Timing benchmarks wait for the machine to be quiet: no compiler running and
# the CPU at least IDLE_PCT% idle on three checks in a row, 10 s apart (the
# rule the committed runs used, with 55%). After IDLE_MAX_WAIT s they run anyway
# and say so; logs/idle_<step>.log shows what the CPU did during the step.
cpu_idle_pct() {
    local a b
    read -r _ a1 a2 a3 a4 a5 a6 a7 a8 _ < /proc/stat
    sleep "${1:-10}"
    read -r _ b1 b2 b3 b4 b5 b6 b7 b8 _ < /proc/stat
    local tot=$(( (b1+b2+b3+b4+b5+b6+b7+b8) - (a1+a2+a3+a4+a5+a6+a7+a8) ))
    local idle=$(( (b4+b5) - (a4+a5) ))
    echo $(( tot > 0 ? 100 * idle / tot : 0 ))
}
wait_idle() {
    local waited=0 ok=0 busy idle
    while :; do
        busy=$(pgrep -c -x 'rustc|cargo|cc|cc1|cc1plus|ld|lld' || true)
        idle=$(cpu_idle_pct 10); waited=$((waited + 10))
        if [ "${busy:-0}" -eq 0 ] && [ "$idle" -ge "$IDLE_PCT" ]; then ok=$((ok + 1)); else ok=0; fi
        if [ "$ok" -ge 3 ]; then
            say "idle: no compiler, CPU ${idle}% idle three checks in a row (waited ${waited} s)"
            return 0
        fi
        if [ "$waited" -ge "$IDLE_MAX_WAIT" ]; then
            say "WARNING: machine still busy after ${waited} s (compilers ${busy:-0}, CPU ${idle}% idle); running anyway"
            ps -eo pcpu=,comm=,args= --sort=-pcpu | head -6 | cut -c1-160 >> "$OUT/logs/run.log"
            return 0
        fi
    done
}

need() { for f in "$@"; do [ -e "$f" ] || { say "missing $f"; return 1; }; done; }

# ---------------------------------------------------------------- build
do_build() { (cd "$ENG" && cargo build --release --bins); }

# ---------------------------------------------------------------- 1. retrieval and the gate
do_retrieval() {
    need "$LME" "$LOCOMO" "$ENGRAM_MODELS/potion-retrieval-32M/model.safetensors" || return 1
    "$BIN/engram-bench" retrieval --data "$LME" --embedder static --ctx request --tag c400 --out "$OUT/retrieval" || return 1
    "$BIN/engram-bench" retrieval --data "$LOCOMO" --embedder static --ctx request --tag c400 --out "$OUT/retrieval" || return 1
    local rl=$OUT/retrieval/longmemeval_s_static_request_c400.rows.jsonl rc=$OUT/retrieval/locomo_static_request_c400.rows.jsonl
    python3 -I "$ENG/scripts/fit_gate.py" "$rl" "$rc" --out "$OUT/gate/gate_static_c400.json" | tee "$OUT/gate/fit_gate.txt" || return 1
    python3 -I "$TOOLS/gate_policy.py" "$ENG" "$rl" "$rc" --out "$OUT/gate/code_gate_policy.json" | tee "$OUT/gate/code_gate_policy.txt"
}

# ---------------------------------------------------------------- 2. record structure
do_structure() {
    need "$LME" "$LOCOMO" || return 1
    "$BIN/engram-structure" --data "$LME" --out "$OUT/structure" \
        --dump "turn+req/snip600/4000,win400+req/vec-text/snip600/4000" || return 1
    "$BIN/engram-structure" --data "$LOCOMO" --out "$OUT/structure" || return 1
    # Is the reader's context the one committed with the checkout?
    python3 -I "$TOOLS/context_diff.py" "$ENG/results/structure/context_longmemeval_s_win_dump_win400_req_vec-text_snip600_4000.jsonl.gz" \
        "$OUT/structure/context_longmemeval_s_win400_req_vec-text_snip600_4000.jsonl" | tee "$OUT/structure/context_diff_win400.txt"
}

# ---------------------------------------------------------------- 3. scale and latency
do_scale() {
    need "$LME" || return 1
    wait_idle
    "$BIN/engram-bench" scale --data "$LME" --sizes "$SCALE_SIZES" --out "$OUT/scale"
}
do_latency() {
    need "$LME" || return 1
    wait_idle
    "$BIN/engram-bench" latency --data "$LME" --sizes "$LATENCY_SIZES" --out "$OUT/latency"
}

# ---------------------------------------------------------------- 4. reader
do_reader() {
    local ctx=$OUT/structure/context_longmemeval_s_win400_req_vec-text_snip600_4000.jsonl
    need "$LME" "$ORACLE" "$ctx" || return 1
    mkdir -p "$READER_HOME" "$OUT/reader_cwd"
    local pass
    # A second pass retries any call that failed in the first (answers and verdicts are cached).
    for pass in 1 2; do
        echo "== pass $pass"
        env -i PATH="$PATH" HOME="$READER_HOME" ANTHROPIC_BASE_URL="${ANTHROPIC_BASE_URL:-}" READER_CWD="$OUT/reader_cwd" \
            python3 -I "$ENG/scripts/reader_eval.py" --data "$LME" --oracle "$ORACLE" \
            --config none=none --config "win400=$ctx" --config oracle=oracle \
            --reader "$READER_MODEL" --judge "$JUDGE_MODEL" --reader-label small --judge-label small \
            --every "$READER_EVERY" --workers "$READER_WORKERS" --out "$OUT/reader" || return 1
    done
    python3 -I - "$OUT/reader" <<'EOF'
import json, sys
d = sys.argv[1]
a = [json.loads(l) for l in open(f"{d}/answers.jsonl")]
v = [json.loads(l) for l in open(f"{d}/verdicts_small.jsonl")]
r, j = sum(x.get("cost_reader", 0) for x in a), sum(x.get("cost_judge", 0) for x in v)
json.dump({"answers": len(a), "verdicts": len(v), "reader_usd": r, "judge_usd": j, "total_usd": r + j}, open(f"{d}/cost.json", "w"), indent=1)
print(f"model spend: reader ${r:.2f}, judge ${j:.2f}, total ${r + j:.2f}")
EOF
}

# ---------------------------------------------------------------- 5. unit tests
do_tests() {
    (cd "$CHECKOUT/strata" && cargo test --release) > "$OUT/tests/strata.log" 2>&1; local a=$?
    (cd "$ENG" && cargo test --release) > "$OUT/tests/engram.log" 2>&1; local b=$?
    python3 -I - "$OUT/tests" <<'EOF'
import json, re, sys
d = sys.argv[1]
out = {}
for crate in ("strata", "engram"):
    c = {"passed": 0, "failed": 0, "ignored": 0, "suites": 0}
    for m in re.finditer(r"test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored", open(f"{d}/{crate}.log", errors="replace").read()):
        c["suites"] += 1; c["passed"] += int(m.group(2)); c["failed"] += int(m.group(3)); c["ignored"] += int(m.group(4))
    out[crate] = c
json.dump(out, open(f"{d}/counts.json", "w"), indent=1)
print(json.dumps(out))
EOF
    tail -3 "$OUT/tests/strata.log" "$OUT/tests/engram.log"
    [ $a -eq 0 ] && [ $b -eq 0 ]
}

# ---------------------------------------------------------------- compare
do_compare() {
    python3 -I "$TOOLS/metrics.py" flatten "$OUT" --json "$OUT/metrics.json" > /dev/null
    python3 -I "$TOOLS/metrics.py" compare "recorded:$CHECKOUT" "$OUT" --key --label-a recorded --label-b "this run" --md "$OUT/compare_recorded_key.md" > /dev/null
    python3 -I "$TOOLS/metrics.py" compare "recorded:$CHECKOUT" "$OUT" --label-a recorded --label-b "this run" --md "$OUT/compare_recorded_full.md" > /dev/null
    if [ -d "$BASELINE" ] && [ "$(realpath "$BASELINE")" != "$OUT" ] && [ -f "$BASELINE/metrics.json" ]; then
        python3 -I "$TOOLS/metrics.py" compare "$BASELINE" "$OUT" --key --label-a baseline --label-b "this run" --md "$OUT/compare_baseline_key.md" > /dev/null
        python3 -I "$TOOLS/metrics.py" compare "$BASELINE" "$OUT" --label-a baseline --label-b "this run" --md "$OUT/compare_baseline_full.md" > /dev/null
        echo "wrote $OUT/compare_baseline_key.md and compare_baseline_full.md"
    fi
    echo "wrote $OUT/metrics.json, compare_recorded_key.md, compare_recorded_full.md"
}

say "run_baseline: $CHECKOUT -> $OUT ($STEPS)"
T0=$(date +%s)
fail=0
has build && { step build do_build || { say "build failed; stopping"; exit 1; }; }
has retrieval && { step retrieval do_retrieval || fail=1; }
has structure && { step structure do_structure || fail=1; }
has scale && { step scale do_scale || fail=1; }
has latency && { step latency do_latency || fail=1; }
has reader && { step reader do_reader || fail=1; }
has tests && { step tests do_tests || fail=1; }
has compare && { step compare do_compare || fail=1; }
rm -rf "$OUT/tmp"
say "done in $(( $(date +%s) - T0 )) s; failures: $fail (see timings.tsv)"
exit $fail
