#!/bin/sh
# ChloeDB's side of the rollup comparison, seeing 4 cores (ncpu.c), as
# Docker's VM has: log_series_bench load DB gen:20000000:30, then rollup
# DB 7 (create the 5-minute rollup, refresh, 50,000 new lines, refresh,
# the four queries on the rollup and on the table). Also writes the
# synthetic log to a file, for the other stores, and the bytes the
# database takes before and after the rollup.
#   rollups_chloedb.sh DATA_DIR     (appends to ../mac-rollups.jsonl)
set -e
data=$1
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
bench=$root/target/release/examples/log_series_bench
out=$here/../mac-rollups.jsonl
cc -dynamiclib -o "$data/ncpu.dylib" "$here/ncpu.c"
tag() { python3 -c 'import json,sys
for l in sys.stdin:
    r = json.loads(l); r.update(store="chloedb", cores=4, machine=sys.argv[1]); print(json.dumps(r), flush=True)' "$MACHINE"; }
MACHINE="MacBook Air (Mac14,2), Apple M2, 8 cores (4P+4E), 24 GB, macOS $(sw_vers -productVersion)"
export MACHINE
run4() { DYLD_INSERT_LIBRARIES="$data/ncpu.dylib" NCPU=4 "$@"; }
"$bench" gen "$data/syn20m.jsonl" 20000000 30
run4 "$bench" load "$data/syn20m" gen:20000000:30 | tag | tee -a "$out"
run4 "$bench" rollup "$data/syn20m" 7 | tag | tee -a "$out"
# Bytes: a checkpoint to a directory writes each segment as a file. The
# table's segments are the largest ones, as many as load left and their
# bytes what it reported; the rest are the rollup and the 50,000 new lines.
rm -rf "$data/ckpt"
echo ".checkpoint $data/ckpt dir" | "$root/target/release/chloedb" "$data/syn20m" >/dev/null
python3 - "$data/ckpt" "$out" <<'PY'
import json, os, sys
ck, out = sys.argv[1:3]
load = [json.loads(l) for l in open(out) if '"mode": "load"' in l and '"store": "chloedb"' in l][-1]
sizes = sorted((os.path.getsize(os.path.join(ck, f)) for f in os.listdir(ck) if f.startswith("seg-")), reverse=True)
table = sum(sizes[:load["segments"]])
assert table == load["segment_bytes"], (table, load["segment_bytes"])
r = {"mode": "disk", "store": "chloedb", "db": "syn20m", "table_bytes": table, "rollup_bytes": sum(sizes) - table, "checkpoint_bytes": sum(sizes),
     "how": "the table's segments are the largest, as load reported them; the rest are the rollup and the 50,000 new lines"}
print(json.dumps(r)); open(out, "a").write(json.dumps(r) + "\n")
PY
rm -rf "$data/ckpt"
