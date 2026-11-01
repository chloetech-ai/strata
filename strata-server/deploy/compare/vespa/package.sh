#!/usr/bin/env bash
# Writes Vespa's application package for the cluster of compare.sh as a zip:
#   package.sh OUT.zip CONTENT_NODES [REDUNDANCY] [CONTAINER_HEAP] [DOMAIN]
# vespa-0 runs the config server and the container; vespa-1 .. vespa-N the
# content nodes. DOMAIN is the pods' DNS suffix (the headless Service).
set -euo pipefail
out=$(cd "$(dirname "$1")" && pwd)/$(basename "$1") n=$2 redundancy=${3:-1} heap=${4:-512m} domain=${5:-vespa.compare.svc.cluster.local}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp -r "$here/schemas" "$here/linguistics" "$work/"
nodes="" hosts="    <host name=\"vespa-0.$domain\"><alias>config</alias></host>"$'\n'
for i in $(seq 1 "$n"); do
    nodes+="            <node hostalias=\"c$i\" distribution-key=\"$((i - 1))\"/>"$'\n'
    hosts+="    <host name=\"vespa-$i.$domain\"><alias>c$i</alias></host>"$'\n'
done
python3 - "$here/services-cluster.xml" "$work/services.xml" "$nodes" "$redundancy" "$heap" <<'PY'
import sys
src, dst, nodes, redundancy, heap = sys.argv[1:]
s = open(src).read().replace("__CONTENT_NODES__", nodes).replace("__REDUNDANCY__", redundancy).replace("__CONTAINER_HEAP__", heap)
open(dst, "w").write(s)
PY
printf '<?xml version="1.0" encoding="utf-8" ?>\n<hosts>\n%s</hosts>\n' "$hosts" >"$work/hosts.xml"
rm -f "$out"
(cd "$work" && python3 -m zipfile -c "$out" services.xml hosts.xml schemas linguistics)
