#!/usr/bin/env bash
# Run every experiment of the report; results land in results/.
# Usage: scripts/run_experiments.sh [WIKITEXT_TXT]
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release
bin=target/release/posting-bench
mkdir -p results
reps=${REPS:-3}

run() { # name, args...
  local name=$1; shift
  echo "== $name: $*" >&2
  "$bin" run --reps "$reps" --json "results/$name.json" "$@" > "results/$name.md"
}

# 1-2. Synthetic collections: clustered (reordered-like) vs uniform docIDs.
run clustered --distribution clustered --num-queries 500
run uniform --distribution uniform --num-queries 500

# 3. Pairwise kernels across size ratios.
echo "== kernels" >&2
"$bin" kernels --small 10000 --json results/kernels.json > results/kernels.md

# 4. AND latency vs number of query terms (clustered collection).
for k in 2 3 4 5 6; do
  run "terms_$k" --distribution clustered --num-queries 300 --min-terms "$k" --max-terms "$k" --ops and \
    --codecs "uncompressed,bp128,optpfor,pef,roaring,trie,hybrid"
done

# 5. Real text: WikiText-103, one paragraph per document.
if [ -n "${1:-}" ]; then
  run wikitext --source text --input "$1" --num-queries 500 --min-list-len 100
fi
echo "done" >&2
