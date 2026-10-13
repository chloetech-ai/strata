#!/usr/bin/env bash
# Download WikiText-103 (raw, train split) from Hugging Face and convert it
# to one paragraph per line for `posting-bench run --source text`.
# Usage: scripts/prepare_wikitext.sh OUT_DIR   (needs python3 + pyarrow)
set -euo pipefail
out=${1:?usage: prepare_wikitext.sh OUT_DIR}
mkdir -p "$out"
base=https://huggingface.co/datasets/Salesforce/wikitext/resolve/main/wikitext-103-raw-v1
for i in 0 1; do
  f="$out/train-$i.parquet"
  [ -s "$f" ] || curl -sSL -o "$f" "$base/train-0000$i-of-00002.parquet"
done
python3 "$(dirname "$0")/wikitext_to_lines.py" "$out/wikitext103.txt" "$out/train-0.parquet" "$out/train-1.parquet"
