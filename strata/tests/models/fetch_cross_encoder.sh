#!/usr/bin/env bash
# Downloads the cross-encoder of the relevance gate's cross-encoder variant
# (tests/relevance_gate.rs, feature bert), ms-marco-MiniLM-L-6-v2, into
# DIR/ms-marco-MiniLM-L-6-v2 at a pinned revision, and checks every file
# against its SHA-256. Files already there with the right hash are kept.
#
#   strata/tests/models/fetch_cross_encoder.sh DIR
#   STRATA_BEIR_DIR=... STRATA_MODELS=DIR cargo test --release --features bert --test relevance_gate cross_encoder
set -euo pipefail

dir=${1:?usage: fetch_cross_encoder.sh DIR}
repo=https://huggingface.co/cross-encoder/ms-marco-MiniLM-L6-v2/resolve/233902d25c440f23af6f7d6e94d2946bac0bee0a
out=$dir/ms-marco-MiniLM-L-6-v2

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

mkdir -p "$out"
while read -r file sha; do
    if [ -f "$out/$file" ] && [ "$(sha256 "$out/$file")" = "$sha" ]; then
        continue
    fi
    curl -fsSL --retry 5 --retry-delay 5 -o "$out/$file.tmp" "$repo/$file"
    got=$(sha256 "$out/$file.tmp")
    if [ "$got" != "$sha" ]; then
        echo "$out/$file: SHA-256 $got, expected $sha" >&2
        exit 1
    fi
    mv "$out/$file.tmp" "$out/$file"
    echo "fetched $out/$file"
done <<'FILES'
config.json 380e02c93f431831be65d99a4e7e5f67c133985bf2e77d9d4eba46847190bacc
tokenizer.json d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66
model.safetensors 821d1aa69520101d6e0737f78a042ae25b19e5cb9160701909d10434f4aeb0ae
FILES
