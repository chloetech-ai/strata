#!/usr/bin/env bash
# The Strata scale probe (notes/scale.md): MS MARCO v2.1 passages with Arctic
# Embed M v1.5 vectors cut to 256-d, loaded, merged and queried at 1M, 2M and
# 5M passages, then 5M at 57, 19, 7 and 3 segments.
#
# Usage, from strata/: scripts/scale_probe.sh [phase ...]
#   phases: gt growth seg-load seg-merge (default: all, in that order;
#   seg-merge needs only meta.json and centroids.f32 of $DATA, so the
#   passages can be removed after seg-load where disk is short)
# Inputs (prepared first):
#   $DATA     scripts/prepare_msmarco.py OUT_DIR --shards 0,1,2
#   $QUERIES  scripts/embed_queries.py MODEL_DIR passv2_dev_queries.tsv OUT_DIR --n 1000
# Every step appends a JSON line to $OUT; databases go to $DB_DIR.
set -euo pipefail

DATA=${DATA:-/home/user/data/msmarco/prep}
QUERIES=${QUERIES:-/home/user/data/msmarco/queries}
DB_DIR=${DB_DIR:-/home/user/data/scale}
OUT=${OUT:-results/scale/steps.jsonl}
PY=${PY:-python3}
BIN=target/release/strata-scale
THREADS=${THREADS:-4}

cargo build --release --bin strata-scale
mkdir -p "$DB_DIR" "$(dirname "$OUT")"

say() { echo "[$(date -u +%H:%M:%S)] $*"; df -h "$DB_DIR" | tail -1; }
q() { # query DB ROWS LABEL
  $BIN query --db "$1" --data "$DATA" --queries "$QUERIES" --gt "$DB_DIR/gt_$2.json" \
    --n 1000 --n-slow 300 --parallel "$THREADS" --clients "$THREADS" --out "$OUT" --label "$3"
}

phases=("$@")
[ ${#phases[@]} -eq 0 ] && phases=(gt growth seg-load seg-merge)

for phase in "${phases[@]}"; do
  case $phase in
  gt)
    for n in 1000000 2000000 5000000; do
      say "exact top-100 over $n rows"
      [ -s "$DB_DIR/gt_$n.json" ] || $PY scripts/exact_topk.py "$DATA" "$n" "$QUERIES" "$DB_DIR/gt_$n.json" --n 1000
    done
    ;;
  growth)
    # Default sizes: bulk loads of 100K-document segments, the default
    # tiered merge policy (factor 8, at most 2M documents per merge).
    db=$DB_DIR/growth.strata
    rm -f "$db" "$db-wal"
    prev=0
    for n in 1000000 2000000; do
      say "growth: load $prev..$n"
      $BIN load --data "$DATA" --db "$db" --start $prev --end $n --segment-docs 100000 --threads "$THREADS" --out "$OUT" --label "growth $n"
      say "growth: merge at $n"
      $BIN merge --db "$db" --threads "$THREADS" --out "$OUT" --label "growth $n"
      say "growth: queries at $n"
      q "$db" $n "growth $n"
      prev=$n
    done
    $BIN stats --db "$db" --out "$OUT" --label "growth 2000000 final"
    rm -f "$db" "$db-wal"
    ;;
  seg-load)
    # 5M passages in 57 segments of 87,720 (113.5M / 2M-document segments =
    # 57), then (seg-merge) merged down to 19, 7 and 3 segments.
    db=$DB_DIR/seg.strata
    rm -f "$db" "$db-wal"
    say "segments: load 0..5000000 in 57 segments"
    $BIN load --data "$DATA" --db "$db" --start 0 --end 5000000 --segment-docs 87720 --threads "$THREADS" --out "$OUT" --label "segments 57"
    say "segments: queries at 57 segments"
    q "$db" 5000000 "segments 57"
    ;;
  seg-merge)
    db=$DB_DIR/seg.strata
    say "segments: merge to 19 (factor 3, at most 270K per merge)"
    $BIN merge --db "$db" --merge-factor 3 --max-merged-docs 270000 --threads "$THREADS" --out "$OUT" --label "segments 19"
    q "$db" 5000000 "segments 19"
    say "segments: merge to 7 (default policy: factor 8, at most 2M)"
    $BIN merge --db "$db" --threads "$THREADS" --out "$OUT" --label "segments 7"
    q "$db" 5000000 "segments 7"
    say "segments: merge to 3 (factor 5, at most 2M)"
    $BIN merge --db "$db" --merge-factor 5 --threads "$THREADS" --out "$OUT" --label "segments 3"
    q "$db" 5000000 "segments 3"
    $BIN stats --db "$db" --out "$OUT" --label "segments 3 final"
    ;;
  *) echo "unknown phase $phase" >&2; exit 2 ;;
  esac
done
say done
