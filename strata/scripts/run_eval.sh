#!/usr/bin/env bash
# The full evaluation of Strata and its baselines, one step at a time on an
# otherwise idle machine (DESIGN.md §8). Logs go to results/logs/.
# Usage: scripts/run_eval.sh [step ...]   (default: every step, in order)
set -uo pipefail
cd "$(dirname "$0")/.."
cargo build --release --features baselines --bins 2>&1 | tail -1
B=target/release
log=results/logs
mkdir -p "$log"

declare -A STEPS=(
  [strata_small]="$B/strata-eval --collections scifact,fiqa,trec-covid --keep"
  [strata_quora]="$B/strata-eval --collections quora --keep"
  [strata_ivf]="$B/strata-eval --collections trec-covid,quora --index ivf --studies workload,planner --kinds vector,filtered_vector,hybrid,filtered_hybrid"
  [baselines_small]="$B/strata-baselines --collections scifact,fiqa,trec-covid"
  [baselines_quora]="$B/strata-baselines --collections quora --max-vector 300"
  [vector_fiqa]="$B/strata-vector-bench --dataset fiqa --queries 648 --threads 4"
  [vector_trec]="$B/strata-vector-bench --dataset trec-covid --queries 50 --filter-queries 50 --threads 4"
  [vector_quora]="$B/strata-vector-bench --dataset quora --queries 1000 --cold-queries 10 --rerank 10,20,40 --nprobe 1,2,4,8,16,32,64,128,256 --budget-secs 10 --threads 4"
  [text]="$B/strata-text-bench --collections scifact,fiqa,trec-covid,quora --max-queries 2000 --reps 3 --cold-queries 100 --sweep-min-df 256,1024,2048"
  [forward_trec]="$B/strata-forward-bench --collection trec-covid"
  [forward_quora]="$B/strata-forward-bench --collection quora"
  [storage]="$B/strata-storage-bench --dataset fiqa --reps 5"
  # Cold-cache ablation on the databases kept above: read-ahead on every
  # section, and read-ahead off everywhere (the default advises only the
  # random-access sections MADV_RANDOM).
  [ablation_readahead]="$B/strata-eval --collections trec-covid,quora --reuse --keep --no-random-hints --studies workload"
  [ablation_noreadahead]="$B/strata-eval --collections trec-covid,quora --reuse --keep --no-read-ahead --studies workload"
  # BM25 quality with and without query stopwords, through the `strata` shell.
  [query_stopwords]="python3 scripts/query_stopwords.py"
)
ORDER=(strata_small strata_quora strata_ivf ablation_readahead ablation_noreadahead query_stopwords baselines_small baselines_quora vector_fiqa vector_trec vector_quora text forward_trec forward_quora storage)

for step in "${@:-${ORDER[@]}}"; do
  echo "== $step $(date +%T): ${STEPS[$step]}" | tee -a "$log/progress.txt"
  start=$SECONDS
  if ${STEPS[$step]} > "$log/$step.log" 2>&1; then status=ok; else status="FAILED ($?)"; fi
  echo "   $step $status after $((SECONDS - start)) s" | tee -a "$log/progress.txt"
done
