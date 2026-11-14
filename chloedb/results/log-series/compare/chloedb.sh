#!/bin/sh
# ChloeDB's side of the comparison: for each file, log_series_bench load
# then query 7, once on all cores and once seeing 4 cores (as Docker's VM
# has), each output line tagged with "cores"; then the results of each
# query (chloedb_values.py), so the other stores can be checked against them.
#   chloedb.sh DATA_DIR      (reads DATA_DIR/{bgl,hdfs,syn20m}.jsonl, writes DATA_DIR/NAME db files)
# The 4-core run loads ncpu.dylib, which makes sysconf report NCPU cores to
# std::thread::available_parallelism, the only thread count ChloeDB reads.
set -e
data=$1
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)        # the chloedb crate
bench=$root/target/release/examples/log_series_bench
out=$here/../mac-chloedb.jsonl
cc -dynamiclib -o "$data/ncpu.dylib" "$here/ncpu.c"
tag() { python3 -c 'import json,sys
for l in sys.stdin:
    r = json.loads(l); r["cores"] = int(sys.argv[1]); print(json.dumps(r), flush=True)' "$1"; }
for n in ${NAMES:-bgl hdfs syn20m}; do
  "$bench" load "$data/$n" "$data/$n.jsonl" | tag 8 | tee -a "$out"
  du -sk "$data/$n" | awk -v n=$n '{print "{\"mode\":\"disk\",\"db\":\"" n "\",\"bytes_on_disk\":" $1 * 1024 "}"}' | tee -a "$out"
  "$bench" query "$data/$n" 7 | tag 8 | tee -a "$out"
  DYLD_INSERT_LIBRARIES="$data/ncpu.dylib" NCPU=4 "$bench" query "$data/$n" 7 | tag 4 | tee -a "$out"
done
