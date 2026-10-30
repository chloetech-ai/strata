#!/usr/bin/env bash
# Downloads the BEIR collections of the relevance gate (tests/relevance_gate.rs)
# into DIR, in BEIR's layout (corpus.jsonl, queries.jsonl, qrels/<split>.tsv),
# and checks every file against its SHA-256. The files come from the BeIR
# datasets on Hugging Face at pinned revisions; they are byte for byte those
# of the BEIR zips (public.ukp.informatik.tu-darmstadt.de). Files already
# there with the right hash are kept.
#
#   strata/tests/fixtures/beir/fetch.sh DIR
#   STRATA_BEIR_DIR=DIR cargo test --release --test relevance_gate
#
# Besides the test splits it fetches SciFact's train and NFCorpus's dev
# judgements, for tuning (strata-bm25-eval --split).
set -euo pipefail

dir=${1:?usage: fetch.sh DIR}
hf=https://huggingface.co/datasets/BeIR

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

# collection, file, repository and revision it comes from, SHA-256
while read -r name file repo rev sha; do
    out="$dir/$name/$file"
    if [ -f "$out" ] && [ "$(sha256 "$out")" = "$sha" ]; then
        continue
    fi
    mkdir -p "$(dirname "$out")"
    case "$file" in
        *.jsonl) curl -fsSL --retry 5 --retry-delay 5 "$hf/$repo/resolve/$rev/$file.gz" | gunzip > "$out.tmp" ;;
        *) curl -fsSL --retry 5 --retry-delay 5 -o "$out.tmp" "$hf/$repo/resolve/$rev/$(basename "$file")" ;;
    esac
    got=$(sha256 "$out.tmp")
    if [ "$got" != "$sha" ]; then
        echo "$out: SHA-256 $got, expected $sha" >&2
        exit 1
    fi
    mv "$out.tmp" "$out"
    echo "fetched $out"
done <<'TABLE'
scifact corpus.jsonl scifact 984eed826375f18d27936c4a32bf0f8491e3f414 dec31c8182f3d744c7d2c09423756fd1d17cbef75808db13ba01cc0aab4d1ac6
scifact queries.jsonl scifact 984eed826375f18d27936c4a32bf0f8491e3f414 8ff84a7c903f722981cd8d595c022660140c51867b27608a6d4910db86080313
scifact qrels/test.tsv scifact-qrels 2938d17dc3b09882fdb8c12bbbe2e2dc0e75a029 0864bb985e0ca2367ba217977e72004d549054b2b06666ed9d4825ac7c21284c
scifact qrels/train.tsv scifact-qrels 2938d17dc3b09882fdb8c12bbbe2e2dc0e75a029 a53f2114831916c096b6c37d9e54da68cef4efdcdbd5ed46533601af972acf1d
nfcorpus corpus.jsonl nfcorpus e28763e68d85db0fa71652ba3c4afabf8c5b3bb7 10cc83ef1826b1425e6a87090b5140b39b27755d5a27e48215a88611c899991f
nfcorpus queries.jsonl nfcorpus e28763e68d85db0fa71652ba3c4afabf8c5b3bb7 d024e6621b84925d485ae473d316a0c3af31c62c8068a59fb29d22f7613aef2a
nfcorpus qrels/test.tsv nfcorpus-qrels a451b3b26d3ae1358f259c1a3a4dd61fcea35a65 f8fba6ef3d4dd9c3a242a8ba4ae38276fc3622fce7dcbae764766d564542fd2a
nfcorpus qrels/dev.tsv nfcorpus-qrels a451b3b26d3ae1358f259c1a3a4dd61fcea35a65 b1d38b5e8f78c4a5820bce2b7ec2db54911d7690dc601e76811846b211180bd8
arguana corpus.jsonl arguana 900330faa8ca2828a468dfe795f9d3c3887c8cfc 0627982cec4ee896df6dc98604ef6b499c83fd87a19eaaeb3f23b88bbef6cbce
arguana queries.jsonl arguana 900330faa8ca2828a468dfe795f9d3c3887c8cfc f8b7c903f95d9a2d291de3915a8fac25085e9db679f19f59fea4145bfd1df618
arguana qrels/test.tsv arguana-qrels ae5468c6f1c198109a8af5f0d4dc58bd18b6fea7 0c47b481fa8b47fb9ca5e74bc8a017bdea14518c0e4a5d21345291180b3aca76
TABLE
