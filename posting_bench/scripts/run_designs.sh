#!/usr/bin/env bash
# Evaluation of the three new designs (Pyramid, Loom, Entropic) against the
# existing codecs. Results land in results/designs/.
# Usage: scripts/run_designs.sh [WIKITEXT_TXT]
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release
bin=target/release/posting-bench
out=results/designs
mkdir -p "$out"
reps=${REPS:-3}
focus="uncompressed,bp128,optpfor,elias-fano,pef,interpolative,hybrid,roaring,pyramid,loom,entropic"

run() { # name, args...
  local name=$1; shift
  echo "== $name: $*" >&2
  "$bin" run --reps "$reps" --json "$out/$name.json" "$@" > "$out/$name.md"
}

# 1. The three collections, every codec.
run clustered --distribution clustered --num-queries 500
run uniform --distribution uniform --num-queries 500
if [ -n "${1:-}" ]; then
  run wikitext --source text --input "$1" --num-queries 500 --min-list-len 100
fi

# 2. Workload extremes on the clustered collection.
run selective --distribution clustered --num-queries 500 --max-list-len 20000 --codecs "$focus"
run dense --distribution clustered --num-queries 300 --min-list-len 200000 --codecs "$focus"

# 3. AND latency vs number of query terms.
for k in 2 3 4 5 6; do
  run "terms_$k" --distribution clustered --num-queries 300 --min-terms "$k" --max-terms "$k" --ops and --codecs "$focus"
done
echo "done" >&2
