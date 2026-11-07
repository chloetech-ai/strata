#!/usr/bin/env bash
# The engine comparison of compare.sh on DigitalOcean, every engine on node
# pools of its own and all of them at once, driven from here over the
# Kubernetes API (nothing needs SSH). One DOKS cluster (REGION, default
# sfo2) holds a base node (the data's file server, where the data is
# prepared and the reports analysed, and a timer that deletes the cluster
# after HOURS even if this script never runs `down`) and, per job, its own
# pools: two SHARDED_SIZE nodes (Strata, Elasticsearch, OpenSearch, Solr,
# Vespa) or one ONE_SIZE node (the nine one-node engines), and a CLIENT_SIZE
# node for the client, Redpanda and Vespa's container.
# A job's pools are labelled and tainted for it, and deleted as soon as its
# reports are back. Everything is tagged strata-bench; nothing else in the
# account is touched.
#
#   fanout.sh up [HOURS]       # (token in DIGITALOCEAN_TOKEN or digitalocean) cluster, base node, timer (default 8 h)
#   fanout.sh extend HOURS     # move the timer HOURS later
#   fanout.sh prep [DOCS]      # prepare msmarco-<DOCS/1000>k on the base node (default 1,000,000), with SHA-256 sums
#   fanout.sh run [JOBS]       # start the jobs of JOBS (default fanout-jobs.tsv) under MAX_NODES and BUDGET, until all end; resumable
#   fanout.sh status           # every job's state and step, nodes up, dollars spent and committed
#   fanout.sh requeue NAME     # run a failed job again at the next `run` (RETRIES, default 1, are automatic)
#   fanout.sh analyze DIR      # every job's reports into DIR, and analyze.py's tables (run on the base node, next to the data)
#   fanout.sh down             # delete the cluster and check that nothing tagged strata-bench is left
#
# IMAGE (required for run) is the strata-server image every job uses, e.g.
# ghcr.io/chloetech-ai/strata-server:<full sha of a main commit>.
# MAX_NODES (default 22) counts every node of the cluster, the base node
# too: the account allows 25 droplets and strata-e2e uses 2. BUDGET (default
# 50, dollars at list price) stops new jobs once what has been spent plus
# what the running jobs could still spend (to their timeouts) would pass it.
#
# Each job: its nodes' CPU, kernel and a fixed CPU and disk calibration (a
# node more than 5% slower than the median of its size is replaced once),
# the data copied to the nodes that read it and checked against its SHA-256
# sums, then compare.sh's steps for its engine with three passes of `run`,
# and loads capped at LOAD_HOURS (default 3). PASSES (default 3) is how
# many passes of `run` each job makes. COPY_JOBS (default 0, no limit)
# holds back new jobs while that many started ones are still copying the
# data (the base node's disk and network are shared, so copies at once only
# keep each other's nodes waiting, billed). STATE (default ~/.strata-fanout,
# mode 700) keeps the kubeconfig, the ledger of pools and each job's log,
# steps and reports.
#
# A job's line may end with a fifth column of KEY=VALUE settings, comma
# separated, that hold for that job only: its node sizes (ONE_SIZE,
# SHARDED_SIZE, CLIENT_SIZE) and compare.sh's budgets (ONE_MEM,
# REDIS_MAXMEM, ...), e.g. a node with more memory for an engine that keeps
# everything in it, or STRATA_COORD=pod for Strata's coordinator in a pod
# of its own on the client node (compare.sh), as before 6 November 2026.
#
# The engine strata-ab runs two Strata images on the same nodes, IMAGE and
# the job's IMAGE_B, passes alternating, for a before and after that the
# nodes cannot sway (KINDS in the environment picks the kinds):
#   strata-ab  strata-ab  sharded  1  IMAGE_B=ghcr.io/chloetech-ai/strata-server:<sha>
# The engine strata-was runs IMAGE_B as an earlier run ran it: shards built
# by IMAGE_B itself (an older image may not read a newer one's segments),
# served in STRATA_COORD_B's layout (default pod, as before 6 November
# 2026), its passes labelled strata-b. Before a rerun of Strata alone, on
# the same nodes, it shows how far these nodes are from that run's:
#   rerun-1m  strata-was+strata  sharded  2  IMAGE_B=ghcr.io/chloetech-ai/strata-server:<sha of that run>
# Engines joined by + run one after another on the same nodes, each with its
# own steps (engines of the job's one shape):
#   same-nodes  strata+es+os+solr+vespa  sharded  5
# In such a list strata-first and strata-last split Strata's steps around
# the others: strata-first builds and runs its passes; strata-last runs them
# again on the same shard files, labelled strata-b (the drift of the nodes
# over the hours between), then Strata's writes and stream, which change the
# shards. An engine whose step fails in such a list has its other steps
# skipped (its down still runs) and the next engine goes on; its failures
# are in the job's reason and failures/<engine>/, and the job ends done.
#   same-nodes-1m  strata-first+es+os+solr+vespa+strata-last  sharded  7
set -euo pipefail

API=https://api.digitalocean.com/v2
DIGITALOCEAN_TOKEN=${DIGITALOCEAN_TOKEN:-${digitalocean:-${DIGITALOCEAN:-}}}
NAME=${NAME:-strata-fanout}
REGION=${REGION:-sfo2}
# The base node: 16 GB, as preparing 10M MS MARCO (a Parquet shard's row
# groups of vectors at a time) took more than an 8 GB node had left.
BASE_SIZE=${BASE_SIZE:-s-8vcpu-16gb}
SHARDED_SIZE=${SHARDED_SIZE:-m-4vcpu-32gb}
ONE_SIZE=${ONE_SIZE:-m-8vcpu-64gb}
CLIENT_SIZE=${CLIENT_SIZE:-c-8}
MAX_NODES=${MAX_NODES:-22}
RETRIES=${RETRIES:-1}
BUDGET=${BUDGET:-50}
LOAD_HOURS=${LOAD_HOURS:-3}
PASSES=${PASSES:-3}
COPY_JOBS=${COPY_JOBS:-0}
STATE=${STATE:-$HOME/.strata-fanout}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=${FANOUT_REPO:-$(cd "$HERE/../../.." && pwd)}
# up, prep and run go on for hours: they run from a copy of this directory
# in STATE, so that editing these files meanwhile changes nothing under them
# (bash reads a script as it goes), and every job runs the same compare.sh.
case ${1:-} in
up | prep | run)
    if [ -z "${FANOUT_COPY:-}" ]; then
        # mktemp: two calls in the same second (a run and a prep started
        # together) must not share a copy, or the second cp fails.
        mkdir -p "$STATE" && chmod 700 "$STATE"
        copy=$(mktemp -d "$STATE/code-$(date +%Y%m%dT%H%M%S)-XXXX") && cp -r "$HERE/." "$copy"
        FANOUT_COPY=$copy FANOUT_REPO=$REPO exec "$copy/fanout.sh" "$@"
    fi
    ;;
esac
KUBECONFIG=$STATE/kubeconfig
export KUBECONFIG
K="kubectl -n fanout"
FILES=http://files.fanout.svc.cluster.local:8000

# Every engine's budget: 6.8 CPUs and 52 GiB in all, split among its
# pods as compare.sh splits 3 CPUs and 6 GiB on the Mac, and what each
# engine is told of it. A sharded engine has 2 x 4 vCPUs and 32 GB, a
# one-node engine 8 vCPUs and 64 GB. A 4-vCPU DOKS node can give pods
# 3,890m CPU and 28 GiB, of which the cilium and do-node-agent
# DaemonSets take 412m and 412 MiB: 3,400m and 26 GiB fit, 3,500m does
# not (a pod bound by nodeName is then refused, not left pending).
export DATA_CPU=${DATA_CPU:-3400m} DATA_MEM=${DATA_MEM:-26Gi} ES_HEAP=${ES_HEAP:-13g} OS_HEAP=${OS_HEAP:-13g} SOLR_HEAP=${SOLR_HEAP:-8g}
export BUILD_CPU=${BUILD_CPU:-3400m} BUILD_MEM=${BUILD_MEM:-26Gi} BUILD_THREADS=${BUILD_THREADS:-4} BUILD_PARALLEL=${BUILD_PARALLEL:-1}
# Strata's pods have DATA_CPU and DATA_MEM; these split it only with STRATA_COORD=pod.
export SHARD_CPU=${SHARD_CPU:-2900m} SHARD_MEM=${SHARD_MEM:-24Gi} COORD_CPU=${COORD_CPU:-1} COORD_MEM=${COORD_MEM:-4Gi}
export VESPA_CONFIG_CPU=${VESPA_CONFIG_CPU:-1} VESPA_CONFIG_MEM=${VESPA_CONFIG_MEM:-6Gi} VESPA_CONTAINER_HEAP=${VESPA_CONTAINER_HEAP:-2g}
export VESPA_CONTENT_CPU=${VESPA_CONTENT_CPU:-2900m} VESPA_CONTENT_MEM=${VESPA_CONTENT_MEM:-23Gi}
export ONE_CPU=${ONE_CPU:-6800m} ONE_MEM=${ONE_MEM:-52Gi} ONE_THREADS=${ONE_THREADS:-7} TS_THREADS=${TS_THREADS:-14} MEILI_MEM=${MEILI_MEM:-26 GiB}
export REDIS_MAXMEM=${REDIS_MAXMEM:-47000mb} GOMEMLIMIT_ONE=${GOMEMLIMIT_ONE:-43GiB} PG_SHARED=${PG_SHARED:-13GB} PG_CACHE=${PG_CACHE:-35GB}
export PG_MAINT=${PG_MAINT:-21GB} PG_PAR=${PG_PAR:-6} PG_WORKERS=${PG_WORKERS:-16} LOAD_CLIENTS=${LOAD_CLIENTS:-8}

do_api() { # METHOD PATH [JSON]
    curl -sS -f -X "$1" -H "Authorization: Bearer ${DIGITALOCEAN_TOKEN:?set DIGITALOCEAN_TOKEN}" \
        -H "Content-Type: application/json" ${3:+--data "$3"} "$API$2"
}
cluster_id() { cat "$STATE/cluster_id"; }
now() { date +%s; }
log() { echo "$(date -u +%H:%M:%S) $*"; }
jq_() { python3 -c "import json, sys; d = json.load(sys.stdin); print($1)"; }

# The ledger: one line per pool created (id, job, size, count, time) and
# per pool deleted (id, -, time); written under a lock by the scheduler and
# the jobs, read to price the run.
ledger() { (flock 9; IFS=$'\t'; printf '%s\n' "$*" >>"$STATE/ledger.tsv") 9>"$STATE/ledger.lock"; }
pool_gone() { grep -qP "^$1\t-\t" "$STATE/ledger.tsv" 2>/dev/null; }

# cost: dollars spent so far, and the most the run can come
# to if every running pool lasts until its job's timeout and the base node
# until the timer: "spent bound".
cost() {
    python3 - "$STATE" <<'PY'
import json, math, os, sys, time
state = sys.argv[1]
prices = json.load(open(os.path.join(state, "prices.json")))
now = time.time()
pools, deleted = {}, {}
for line in open(os.path.join(state, "ledger.tsv")) if os.path.exists(os.path.join(state, "ledger.tsv")) else []:
    f = line.rstrip("\n").split("\t")
    if f[1] == "-":
        deleted[f[0]] = float(f[2])
    else:
        pools[f[0]] = (f[1], f[2], int(f[3]), float(f[4]))
timeouts = {}
for name in os.listdir(os.path.join(state, "jobs")) if os.path.isdir(os.path.join(state, "jobs")) else []:
    p = os.path.join(state, "jobs", name, "timeout_h")
    if os.path.exists(p):
        timeouts[name] = float(open(p).read())
spent = bound = 0.0
for pid, (job, size, count, t0) in pools.items():
    rate = prices[size] * count
    end = deleted.get(pid, now)
    spent += rate * math.ceil(max(end - t0, 1) / 3600)
    if pid in deleted:
        bound += rate * math.ceil(max(end - t0, 1) / 3600)
    else:
        bound += rate * math.ceil(max(now - t0, (timeouts.get(job, 4) + 0.5) * 3600) / 3600)
base = json.load(open(os.path.join(state, "base.json")))
rate = prices[base["size"]]
spent += rate * math.ceil(max(now - base["created"], 1) / 3600)
bound += rate * math.ceil((max(now, float(open(os.path.join(state, "deadline")).read())) - base["created"]) / 3600)
print(f"{spent:.2f} {bound:.2f}")
PY
}

# The timer: a pod that deletes the cluster and what it made at DEADLINE
# (seconds since the epoch), from inside it.
timer() { # DEADLINE
    $K apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: {name: teardown-$1, labels: {app: teardown}}
spec:
  restartPolicy: OnFailure
  containers:
    - name: teardown
      image: curlimages/curl:8.17.0
      env:
        - {name: TOKEN, valueFrom: {secretKeyRef: {name: do-token, key: token}}}
      command: [sh, -c, "while [ \$(date +%s) -lt $1 ]; do sleep 60; done; until curl -sS -f -X DELETE -H \"Authorization: Bearer \$TOKEN\" $API/kubernetes/clusters/$(cluster_id)/destroy_with_associated_resources/dangerous; do sleep 60; done"]
      resources: {requests: {cpu: 10m, memory: 16Mi}}
EOF
    $K wait --for=condition=Ready "pod/teardown-$1" --timeout=300s >/dev/null
    echo "$1" >"$STATE/deadline"
}

in_base() { $K exec base -- bash -c "$1"; }

# Jobs: name, engine, shape (sharded or one), timeout in hours, and its own
# settings (KEY=VALUE,...; - for none). Order is the order they start in.
jobs_of() { grep -v '^#' "$1" | awk 'NF >= 4 {print $1, $2, $3, $4, (NF >= 5 ? $5 : "-")}'; }
# A job's own settings, exported into this (sub)shell.
apply_env() { # ENVS
    [ "$1" = - ] && return 0
    local kv
    for kv in ${1//,/ }; do export "${kv?}"; done
}
nodes_of() { [ "$1" = sharded ] && echo 3 || echo 2; }
# The most a job's pools can cost: to its timeout, and half an hour more.
job_cost() { # SHAPE HOURS
    python3 - "$STATE/prices.json" "$1" "$2" "$SHARDED_SIZE" "$ONE_SIZE" "$CLIENT_SIZE" <<'PY'
import json, math, sys
p = json.load(open(sys.argv[1]))
shape, hours, sharded, one, client = sys.argv[2], float(sys.argv[3]), *sys.argv[4:]
rate = (2 * p[sharded] if shape == "sharded" else p[one]) + p[client]
print(round(rate * math.ceil(hours + 0.5), 2))
PY
}
# Nodes still up for a job that ended (a pool whose delete failed).
pools_live() { # NAME
    local n=0 pool pname size count
    [ -f "$STATE/jobs/$1/pools" ] || { echo 0; return; }
    while read -r pool pname size count; do pool_gone "$pool" || n=$((n + count)); done <"$STATE/jobs/$1/pools"
    echo $n
}
jstate() { cat "$STATE/jobs/$1/state" 2>/dev/null || echo queued; }
# Nodes in every pool of the cluster, the base and another run's included.
nodes_up() { do_api GET "/kubernetes/clusters/$(cluster_id)/node_pools" | python3 -c 'import json, sys; print(sum(p["count"] for p in json.load(sys.stdin)["node_pools"]))'; }
# Attempts a job may make: one, RETRIES more, and one per `requeue`.
max_attempts() { echo $((1 + RETRIES + $(cat "$STATE/jobs/$1/requeued" 2>/dev/null || echo 0))); }
setstate() { mkdir -p "$STATE/jobs/$1" && echo "$2" >"$STATE/jobs/$1/state"; }

# A one-shot pod on NODE (any taint tolerated) running SCRIPT in IMAGE with
# the node's /beir, /data and /var/local/compare, privileged (it sets
# vm.max_map_count for Elasticsearch and OpenSearch); its log on stdout.
node_pod() { # NS NAME NODE IMAGE SCRIPT
    local ns=$1 name=$2 node=$3 image=$4 script=$5
    kubectl -n "$ns" delete pod "$name" --ignore-not-found >/dev/null
    kubectl -n "$ns" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: {name: $name}
spec:
  restartPolicy: Never
  nodeName: $node
  tolerations: [{operator: Exists}]
  containers:
    - name: c
      image: $image
      imagePullPolicy: IfNotPresent
      command: [bash, -c, $(python3 -c 'import json, sys; print(json.dumps(sys.argv[1]))' "$script")]
      securityContext: {privileged: true}
      volumeMounts:
        - {name: beir, mountPath: /beir}
        - {name: data, mountPath: /data}
        - {name: local, mountPath: /local}
  volumes:
    - {name: beir, hostPath: {path: /beir, type: DirectoryOrCreate}}
    - {name: data, hostPath: {path: /data, type: DirectoryOrCreate}}
    - {name: local, hostPath: {path: /var/local/compare, type: DirectoryOrCreate}}
EOF
    local t0 phase
    t0=$(now)
    while :; do
        phase=$(kubectl -n "$ns" get pod "$name" -o jsonpath='{.status.phase}' 2>/dev/null || true)
        case $phase in Succeeded | Failed) break ;; esac
        [ $(($(now) - t0)) -gt 2400 ] && { echo "pod $name on $node: still $phase after 40 min" >&2; return 1; }
        sleep 5
    done
    kubectl -n "$ns" logs "$name"
    kubectl -n "$ns" delete pod "$name" --wait=false >/dev/null
    [ "$phase" = Succeeded ]
}

# What a node is, a fixed calibration, and (COPY=1) the data: one JSON line.
setup_script() { # NODE COPY
    cat <<EOF
set -e
echo 262144 >/proc/sys/vm/max_map_count
mkdir -p /data/compare /beir
s1=\$(date +%s%N); head -c 4G /dev/zero | sha256sum >/dev/null; s2=\$(date +%s%N)
n=\$(nproc); for i in \$(seq \$n); do head -c 1G /dev/zero | sha256sum >/dev/null & done; wait; s3=\$(date +%s%N)
dd if=/dev/zero of=/beir/.calib bs=4M count=512 oflag=direct conv=fsync 2>/dev/null; s4=\$(date +%s%N); rm -f /beir/.calib
copy=0
if [ "$2" = 1 ]; then
    c0=\$(date +%s%N)
    mkdir -p /beir/$COLLECTION && cd /beir/$COLLECTION
    curl -sSf --retry 5 -o SHA256SUMS $FILES/$COLLECTION/SHA256SUMS
    if ! sha256sum -c --quiet SHA256SUMS 2>/dev/null; then
        for f in \$(awk '{print \$2}' SHA256SUMS); do curl -sSf --retry 5 -o "\$f" "$FILES/$COLLECTION/\$f"; done
        sha256sum -c --quiet SHA256SUMS
    fi
    copy=\$(( (\$(date +%s%N) - c0) / 1000000 ))
fi
model=\$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ *//')
mem=\$(awk '/MemTotal/ {print \$2}' /proc/meminfo)
echo "{\"node\": \"$1\", \"cpu\": \"\$model\", \"vcpus\": \$n, \"mem_kb\": \$mem, \"kernel\": \"\$(uname -r)\", \"sha256_4g_s\": \$(( (s2 - s1) / 1000000 ))e-3, \"sha256_1g_x_vcpus_s\": \$(( (s3 - s2) / 1000000 ))e-3, \"disk_write_2g_s\": \$(( (s4 - s3) / 1000000 ))e-3, \"copy_ms\": \$copy, \"time\": \$(date +%s)}"
EOF
}

# A node's calibration against the median of every node of its size so far:
# prints "slow" when its single-core time is more than 5% over it (with at
# least 3 others to compare with).
judge() { # NODE_JSON SIZE
    python3 - "$STATE" "$1" "$2" <<'PY'
import glob, json, statistics, sys
state, line, size = sys.argv[1], json.loads(sys.argv[2]), sys.argv[3]
xs = []
for p in glob.glob(f"{state}/jobs/*/nodes.jsonl"):
    for l in open(p):
        d = json.loads(l)
        if d.get("size") == size and d["node"] != line["node"]:
            xs.append(d["sha256_4g_s"])
print("slow" if len(xs) >= 3 and line["sha256_4g_s"] > 1.05 * statistics.median(xs) else "ok")
PY
}

# The PASSES passes of `run` for ENGINE (labelled LABEL, default ENGINE).
runs() { # ENGINE [LABEL]
    local i l=${2:-$1}
    [ "$l" = "$1" ] && echo "run $1" || echo "run $1 --label $l"
    for ((i = 2; i <= PASSES; i++)); do echo "run $1 --label $l-pass$i"; done
}
# A step without the settings it may start with (IMAGE=REF STRATA_COORD=...).
bare_step() { # STEP
    local s=$1
    while [[ $s =~ ^(IMAGE|STRATA_COORD)=[^\ ]*\ (.*)$ ]]; do s=${BASH_REMATCH[2]}; done
    echo "$s"
}
# The engine a step is for: compare.sh's argument after the command.
step_engine() { # STEP
    local c e
    read -r c e _ <<<"$(bare_step "$1")"
    case $c in build) echo strata ;; trial) echo es ;; *) echo "$e" ;; esac
}
steps_of() { # ENGINE: compare.sh's steps, one per line
    case $1 in
    # Engines joined by + (strata+es+os) run one after another on the same
    # nodes, each with its own steps, so that the nodes cannot make the
    # difference between them.
    *+*)
        local e
        for e in ${1//+/ }; do steps_of "$e"; done
        ;;
    strata) printf '%s\n' "build" "up strata" "$(runs strata)" "writes strata" "stream strata" "down strata" ;;
    # Strata's steps split around the other engines of a + list (above).
    strata-first) printf '%s\n' "build" "up strata" "$(runs strata)" "down strata" ;;
    strata-last) printf '%s\n' "up strata" "$(runs strata strata-b)" "writes strata" "stream strata" "down strata" ;;
    # The trial licence's run before the writes, on the index as loaded (at
    # 1M it ran after them, on segments the writes and the stream had churned).
    es) printf '%s\n' "up es" "load es" "$(runs es)" "trial" \
        "run es --kinds hybrid (RRF),hybrid (weighted) --paid --label es-trial" "writes es" "stream es" "down es" ;;
    # Two Strata images on the same nodes, so that the nodes cannot make the
    # difference: IMAGE's runs labelled strata-a, IMAGE_B's (the job's fifth
    # column) strata-b, PASSES of each, their order swapped every pass (a b,
    # b a, a b, ...), on shard files built once by IMAGE. KINDS (comma
    # separated, in the environment of `fanout.sh run`) picks the kinds.
    strata-ab)
        local i x img sfx
        echo build
        for ((i = 1; i <= PASSES; i++)); do
            sfx=""
            [ "$i" = 1 ] || sfx=-pass$i
            for x in $([ $((i % 2)) = 1 ] && echo a b || echo b a); do
                img=$IMAGE
                [ "$x" = a ] || img=${IMAGE_B:?strata-ab needs IMAGE_B}
                printf '%s\n' "IMAGE=$img up strata" "run strata --label strata-$x$sfx${KINDS:+ --kinds $KINDS}" "down strata"
            done
        done
        ;;
    # IMAGE_B as an earlier run ran it (see the top of this file): its own
    # build, client and layout; the passes labelled strata-b. The shards are
    # its own, so it goes before the job's other Strata steps, which build
    # theirs again.
    strata-was)
        local b="IMAGE=${IMAGE_B:?strata-was needs IMAGE_B} STRATA_COORD=${STRATA_COORD_B:-pod}"
        printf '%s\n' "$b build" "$b up strata"
        runs strata strata-b | sed "s|^|$b |"
        printf '%s\n' "$b down strata"
        ;;
    os | solr | vespa) printf '%s\n' "up $1" "load $1" "$(runs "$1")" "writes $1" "stream $1" "down $1" ;;
    *) printf '%s\n' "up $1" "load $1" "$(runs "$1")" "down $1" ;;
    esac
}

# The job NAME, on its pools (in STATE/jobs/NAME/pools: id, name, size, count).
job_main() { # NAME ENGINE SHAPE TIMEOUT_H
    local name=$1 engine=$2 shape=$3 hours=$4 dir=$STATE/jobs/$1 t0 ns
    t0=$(now)
    ns=$name
    mkdir -p "$dir/work/compare" "$dir/failures"
    echo "$IMAGE" >"$dir/image"
    setstate "$name" running
    log "$name: waiting for its nodes"
    local dnodes=() cnode="" pool pname psize pcount ready
    while read -r pool pname psize pcount; do
        until ready=$(kubectl get nodes -l "doks.digitalocean.com/node-pool=$pname" -o json 2>/dev/null | python3 -c '
import json, sys
n = [x for x in json.load(sys.stdin)["items"] if any(c["type"] == "Ready" and c["status"] == "True" for c in x["status"]["conditions"])]
print(" ".join(sorted(x["metadata"]["name"] for x in n)))') && [ "$(wc -w <<<"$ready")" -ge "$pcount" ]; do
            [ $(($(now) - t0)) -gt 1800 ] && { log "$name: pool $pname not ready after 30 min"; return 1; }
            sleep 15
        done
        if [[ $pname == *-c-* ]]; then cnode=$ready; else read -r -a dnodes <<<"$ready"; fi
        echo "$pname $psize $ready" >>"$dir/nodes.txt"
    done <"$dir/pools"
    [ ${#dnodes[@]} = 1 ] && dnodes+=("${dnodes[0]}")
    log "$name: nodes ${dnodes[*]} and $cnode"
    kubectl create namespace "$ns" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
    # Every node: what it is, its calibration, and the data where it is read
    # (the client's node; Strata's data nodes too, where its shards build).
    local n copy size line verdict uniq
    read -r -a uniq <<<"$(printf '%s\n' "${dnodes[@]}" "$cnode" | awk '!seen[$0]++' | tr '\n' ' ')"
    for n in "${uniq[@]}"; do
        [ -s "$dir/setup-$n.json" ] && continue
        copy=0
        [ "$n" = "$cnode" ] || [[ +$engine+ == *+strata* ]] && copy=1
        size=$CLIENT_SIZE
        [ "$n" = "$cnode" ] || { [ "$shape" = sharded ] && size=$SHARDED_SIZE || size=$ONE_SIZE; }
        (
            line=$(node_pod "$ns" "setup-$(echo "$n" | tail -c 6)" "$n" "$IMAGE" "$(setup_script "$n" "$copy")" </dev/null | tail -1)
            line=$(python3 -c 'import json, sys; d = json.loads(sys.argv[1]); d["size"] = sys.argv[2]; print(json.dumps(d))' "$line" "$size")
            echo "$line" >"$dir/setup-$n.json"
        ) &
    done
    wait
    local new
    for n in "${uniq[@]}"; do
        [ -s "$dir/setup-$n.json" ] || { log "$name: setup failed on $n"; return 1; }
        line=$(cat "$dir/setup-$n.json")
        size=$(jq_ 'd["size"]' <<<"$line")
        verdict=$(judge "$line" "$size")
        echo "$line" >>"$dir/nodes.jsonl"
        [ "$verdict" = slow ] || continue
        # Once per node: a new droplet in its place, set up the same way.
        log "$name: node $n is more than 5% slower than the median of $size: replacing it"
        new=$(replace_node "$name" "$n") || { log "$name: could not replace $n: kept"; continue; }
        copy=0
        [ "$n" = "$cnode" ] || [[ +$engine+ == *+strata* ]] && copy=1
        line=$(node_pod "$ns" "setup-$(echo "$new" | tail -c 6)" "$new" "$IMAGE" "$(setup_script "$new" "$copy")" </dev/null | tail -1)
        line=$(python3 -c 'import json, sys; d = json.loads(sys.argv[1]); d["size"] = sys.argv[2]; d["replaced"] = sys.argv[3]; print(json.dumps(d))' "$line" "$size" "$n")
        echo "$line" >"$dir/setup-$new.json"
        echo "$line" >>"$dir/nodes.jsonl"
        if [ "$n" = "$cnode" ]; then cnode=$new; else
            for i in "${!dnodes[@]}"; do [ "${dnodes[$i]}" = "$n" ] && dnodes[$i]=$new; done
        fi
        log "$name: $n replaced by $new"
    done
    # Its data is copied: the scheduler may start the next job (COPY_JOBS).
    touch "$dir/ready"
    local step i=0 st rc multi=0 skip=" " se why fdir
    [[ $engine == *+* ]] && multi=1
    # A + list taken up again: the engines that failed before stay skipped.
    if [ $multi = 1 ] && [ -f "$dir/steps.tsv" ]; then
        local res prev
        while IFS=$'\t' read -r _ res _ _ prev; do
            [[ $res == fail:* ]] && skip+="$(step_engine "$prev") "
        done <"$dir/steps.tsv"
    fi
    while IFS= read -r step; do
        i=$((i + 1))
        grep -qP "^$i\t(ok|skipped)\t" "$dir/steps.tsv" 2>/dev/null && continue
        if [ $(($(now) - t0)) -gt $((${hours%.*} * 3600)) ]; then
            log "$name: past its timeout of $hours h before step $i ($step)"
            echo "timeout before: $step" >>"$dir/reason"
            return 1
        fi
        # A step may start with IMAGE=REF, its own image (strata-ab), and
        # STRATA_COORD=..., its own layout (strata-was); its --kinds runs to
        # the next option as one argument (kinds have spaces).
        local s=$step simage=$IMAGE scoord=${STRATA_COORD:-} kinds rest
        while [[ $s =~ ^(IMAGE|STRATA_COORD)=([^\ ]*)\ (.*)$ ]]; do
            case ${BASH_REMATCH[1]} in
            IMAGE) simage=${BASH_REMATCH[2]} ;;
            STRATA_COORD) scoord=${BASH_REMATCH[2]} ;;
            esac
            s=${BASH_REMATCH[3]}
        done
        se=$(step_engine "$s")
        if [[ $skip == *" $se "* ]] && [[ $s != down* ]]; then
            printf '%s\t%s\t%s\t%s\t%s\n' "$i" skipped "$(now)" "$(now)" "$step" >>"$dir/steps.tsv"
            continue
        fi
        log "$name: step $i: compare.sh $step"
        st=$(now)
        # A load stops at LOAD_HOURS, or sooner at the job's own timeout (which
        # the budget counts on).
        local cap="" left=$((${hours%.*} * 3600 - ($(now) - t0)))
        [[ $step == load* ]] && cap="timeout $((left < LOAD_HOURS * 3600 ? left : LOAD_HOURS * 3600))s"
        rc=0
        # shellcheck disable=SC2086
        (
            export CONTEXT NS=$ns NODES="${dnodes[*]}" CLIENT_NODE=$cnode IMAGE=$simage STRATA_COORD=$scoord COLLECTION STRATA_WORK=$dir/work STRATA_DATA=$STATE/data
            if [[ $s == *" --kinds "* ]]; then
                rest=${s#* --kinds }
                kinds=${rest%% --*}
                $cap "$HERE/compare.sh" ${s%% --kinds *} --kinds "$kinds" ${rest#"$kinds"}
            else
                $cap "$HERE/compare.sh" $s
            fi
        ) </dev/null >>"$dir/log" 2>&1 || rc=$?
        printf '%s\t%s\t%s\t%s\t%s\n' "$i" "$([ $rc = 0 ] && echo ok || echo "fail:$rc")" "$st" "$(now)" "$step" >>"$dir/steps.tsv"
        if [ $rc != 0 ]; then
            log "$name: step $i ($step) failed ($rc)"
            if [[ $s == load* ]] && [ $rc = 124 ]; then
                why="load timed out after $((($(now) - st) / 60)) min (LOAD_HOURS $LOAD_HOURS, job timeout $hours h)"
            else
                why="$step: exit $rc"
            fi
            fdir=$dir/failures
            [ $multi = 1 ] && fdir=$dir/failures/$se && mkdir -p "$fdir"
            kubectl -n "$ns" get pods -o wide >"$fdir/pods.txt" 2>&1 || true
            kubectl -n "$ns" describe pods >"$fdir/describe.txt" 2>&1 || true
            for p in $(kubectl -n "$ns" get pods -o name 2>/dev/null); do
                kubectl -n "$ns" logs "$p" --all-containers --tail=300 >"$fdir/$(basename "$p").log" 2>&1 || true
            done
            if [ $multi = 1 ]; then
                echo "$why" >>"$dir/reason"
                skip+="$se "
                log "$name: $se's other steps are skipped (its down runs); the next engine goes on"
                continue
            fi
            echo "$why" >"$dir/reason"
            return 1
        fi
    done < <(steps_of "$engine")
    return 0
}

# replace_node NAME NODE: deletes NODE's droplet and has DOKS make a new one
# in its pool; prints the new node's name once it is Ready.
replace_node() {
    local name=$1 node=$2 pool pname size count nid="" t0 ready n
    while read -r pool pname size count; do
        nid=$(do_api GET "/kubernetes/clusters/$(cluster_id)/node_pools/$pool" | python3 -c '
import json, sys
print(next((n["id"] for n in json.load(sys.stdin)["node_pool"]["nodes"] if n["name"] == sys.argv[1]), ""))' "$node")
        [ -n "$nid" ] && break
    done <"$STATE/jobs/$name/pools"
    [ -n "$nid" ] || return 1
    do_api DELETE "/kubernetes/clusters/$(cluster_id)/node_pools/$pool/nodes/$nid?replace=1" >/dev/null || return 1
    t0=$(now)
    while :; do
        ready=$(kubectl get nodes -l "doks.digitalocean.com/node-pool=$pname" -o json 2>/dev/null | python3 -c '
import json, sys
n = [x["metadata"]["name"] for x in json.load(sys.stdin)["items"] if any(c["type"] == "Ready" and c["status"] == "True" for c in x["status"]["conditions"])]
print(" ".join(x for x in sorted(n) if x != sys.argv[1]))' "$node")
        for n in $ready; do
            grep -qw -- "$n" "$STATE/jobs/$name/nodes.txt" || { echo "$pname $size $n (replaces $node)" >>"$STATE/jobs/$name/nodes.txt"; echo "$n"; return 0; }
        done
        [ $(($(now) - t0)) -gt 1200 ] && return 1
        sleep 15
    done
}

# The reports on the client's node into STATE/jobs/NAME/work/compare.
fetch_reports() { # NAME
    local name=$1 dir=$STATE/jobs/$1 cnode
    cnode=$(awk '$1 ~ /-c-/ {n = $3} END {print n}' "$dir/nodes.txt")
    [ -n "$cnode" ] || return 0
    kubectl -n "$name" delete pod fetch --ignore-not-found >/dev/null 2>&1 || true
    kubectl -n "$name" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: {name: fetch}
spec:
  nodeName: $cnode
  tolerations: [{operator: Exists}]
  containers:
    - {name: c, image: $IMAGE, command: [sleep, "3600"], volumeMounts: [{name: data, mountPath: /data}]}
  volumes:
    - {name: data, hostPath: {path: /data, type: DirectoryOrCreate}}
EOF
    local rc=0
    { kubectl -n "$name" wait --for=condition=Ready pod/fetch --timeout=600s >/dev/null &&
        kubectl -n "$name" exec fetch -- tar -C /data -c compare | tar -x -C "$dir/work" &&
        log "$name: reports fetched ($(ls "$dir/work/compare" | wc -l) files)"; } || rc=1
    kubectl -n "$name" delete pod fetch --wait=false >/dev/null 2>&1 || true
    return $rc
}

delete_pools() { # NAME
    local name=$1 pool rest
    [ -f "$STATE/jobs/$name/pools" ] || return 0
    while read -r pool rest; do
        pool_gone "$pool" && continue
        if do_api DELETE "/kubernetes/clusters/$(cluster_id)/node_pools/$pool" >/dev/null 2>&1 ||
            ! do_api GET "/kubernetes/clusters/$(cluster_id)/node_pools/$pool" >/dev/null 2>&1; then
            ledger "$pool" - "$(now)"
        else
            log "$name: could not delete pool $pool (retried by the scheduler)"
        fi
    done <"$STATE/jobs/$name/pools"
}

create_pools() { # NAME SHAPE ATTEMPT
    local name=$1 shape=$2 a=$3 dsize dcount out pid pname
    [ "$shape" = sharded ] && dsize=$SHARDED_SIZE dcount=2 || dsize=$ONE_SIZE dcount=1
    : >"$STATE/jobs/$name/pools"
    for spec in "d $dsize $dcount" "c $CLIENT_SIZE 1"; do
        read -r kind size count <<<"$spec"
        pname="$name-$kind-a$a"
        out=$(do_api POST "/kubernetes/clusters/$(cluster_id)/node_pools" "$(python3 - "$pname" "$size" "$count" "$name" <<'PY'
import json, sys
pname, size, count, job = sys.argv[1:]
print(json.dumps({"name": pname, "size": size, "count": int(count), "tags": ["strata-bench"],
                  "labels": {"fanout-job": job}, "taints": [{"key": "fanout-job", "value": job, "effect": "NoSchedule"}]}))
PY
)") || { log "$name: could not create pool $pname"; return 1; }
        pid=$(jq_ 'd["node_pool"]["id"]' <<<"$out")
        ledger "$pid" "$name" "$size" "$count" "$(now)"
        echo "$pid $pname $size $count" >>"$STATE/jobs/$name/pools"
    done
}

# One job from its pools to its reports, then its pools deleted.
job() { # NAME ENGINE SHAPE TIMEOUT_H
    local rc=0
    job_main "$@" || rc=$?
    local i
    for i in 1 2 3; do
        fetch_reports "$1" && break
        log "$1: fetching its reports failed (try $i of 3)"
        sleep 30
    done
    delete_pools "$1"
    kubectl delete namespace "$1" --wait=false >/dev/null 2>&1 || true
    if [ $rc = 0 ]; then setstate "$1" done; else setstate "$1" failed; fi
    log "$1: $(jstate "$1") after $(($(now) - $(stat -c %Y "$STATE/jobs/$1/pools"))) s"
}

cmd=${1:-}
shift || true
case "$cmd" in
up)
    hours=${1:-8}
    deadline=$(($(now) + hours * 3600))
    mkdir -p "$STATE/jobs" "$STATE/data" && chmod 700 "$STATE"
    if [ -f "$STATE/cluster_id" ]; then
        # An `up` that stopped part way: carry on with that cluster.
        deadline=$(cat "$STATE/deadline")
        hours=$(((deadline - $(now)) / 3600))
        log "cluster $(cluster_id) is there already: carrying on with it"
    else
        # Every page of sizes (there are more than 200; a page past the last
        # has no "sizes").
        for page in 1 2 3; do do_api GET "/sizes?per_page=200&page=$page"; echo; done | python3 -c '
import json, sys
print(json.dumps({s["slug"]: s["price_hourly"] for l in sys.stdin if l.strip() for s in json.loads(l).get("sizes", [])}))' >"$STATE/prices.json"
        version=$(do_api GET /kubernetes/options | jq_ 'd["options"]["versions"][0]["slug"]')
        body=$(python3 - "$NAME" "$REGION" "$version" "$BASE_SIZE" <<'PY'
import json, sys
name, region, version, size = sys.argv[1:]
print(json.dumps({"name": name, "region": region, "version": version, "tags": ["strata-bench"],
                  "node_pools": [{"name": "base", "size": size, "count": 1, "tags": ["strata-bench"]}]}))
PY
)
        do_api POST /kubernetes/clusters "$body" | jq_ 'd["kubernetes_cluster"]["id"]' >"$STATE/cluster_id"
        echo "{\"size\": \"$BASE_SIZE\", \"created\": $(now)}" >"$STATE/base.json"
        echo "$deadline" >"$STATE/deadline"
        log "cluster $(cluster_id) ($version, base $BASE_SIZE in $REGION): waiting for it to run"
    fi
    until [ "$(do_api GET "/kubernetes/clusters/$(cluster_id)" | jq_ 'd["kubernetes_cluster"]["status"]["state"]')" = running ]; do sleep 15; done
    do_api GET "/kubernetes/clusters/$(cluster_id)/kubeconfig" >"$KUBECONFIG"
    chmod 600 "$KUBECONFIG"
    kubectl create namespace fanout --dry-run=client -o yaml | kubectl apply -f - >/dev/null
    $K create secret generic do-token --from-literal=token="$DIGITALOCEAN_TOKEN" --dry-run=client -o yaml | $K apply -f - >/dev/null
    timer "$deadline"
    # The base pod: the file server over the node's own disk, where the data
    # is prepared and the reports analysed.
    $K apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: {name: base, labels: {app: base}}
spec:
  containers:
    - name: base
      image: python:3.12-bookworm
      command: [bash, -c, "mkdir -p /srv/data && exec python3 -m http.server 8000 -d /srv/data"]
      ports: [{containerPort: 8000}]
      resources: {requests: {cpu: "2", memory: 4Gi}}
      volumeMounts: [{name: srv, mountPath: /srv}]
  volumes:
    - {name: srv, hostPath: {path: /var/lib/fanout, type: DirectoryOrCreate}}
---
apiVersion: v1
kind: Service
metadata: {name: files}
spec:
  selector: {app: base}
  ports: [{port: 8000, targetPort: 8000}]
EOF
    $K wait --for=condition=Ready pod/base --timeout=600s >/dev/null
    log "up: teardown at $(date -u -d "@$deadline" +%Y-%m-%dT%H:%MZ) ($hours h)"
    ;;
extend)
    hours=${1:?extend HOURS}
    old=$(cat "$STATE/deadline")
    new=$((old + hours * 3600))
    timer "$new"
    for p in $($K get pods -l app=teardown -o name); do
        [ "$p" = "pod/teardown-$new" ] || $K delete "$p" --wait=false >/dev/null
    done
    log "teardown moved from $(date -u -d "@$old" +%Y-%m-%dT%H:%MZ) to $(date -u -d "@$new" +%Y-%m-%dT%H:%MZ)"
    ;;
prep)
    docs=${1:-1000000}
    coll=msmarco-$((docs / 1000))k
    in_base "mkdir -p /srv/scripts /srv/logs"
    (cd "$REPO" && tar -c strata/scripts/prepare_msmarco.py strata/scripts/prepare_msmarco_compare.py strata/scripts/embed_queries.py \
        strata-server/deploy/compare/analyze.py) | $K exec -i base -- tar -x -C /srv/scripts
    in_base "pip install -q numpy pyarrow onnxruntime tokenizers >/srv/logs/pip.log 2>&1
        nohup bash -c 'set -e; python3 /srv/scripts/strata/scripts/prepare_msmarco_compare.py /srv/data --docs $docs
            cd /srv/data/$coll && sha256sum docs.jsonl doc_vecs.f32 queries.jsonl query_vecs.f32 qrels.tsv meta.json >SHA256SUMS && echo PREP DONE' \
            >/srv/logs/prep-$coll.log 2>&1 &"
    log "prep of $coll started: its log is /srv/logs/prep-$coll.log on the base pod"
    until in_base "grep -q 'PREP DONE' /srv/logs/prep-$coll.log"; do
        if ! in_base "pgrep -f prepare_msmarco_compare >/dev/null || grep -q 'PREP DONE' /srv/logs/prep-$coll.log"; then
            in_base "tail -20 /srv/logs/prep-$coll.log"
            echo "prep failed" >&2
            exit 1
        fi
        sleep 30
    done
    in_base "tail -3 /srv/logs/prep-$coll.log"
    mkdir -p "$STATE/data/$coll"
    for f in meta.json SHA256SUMS; do $K exec base -- cat "/srv/data/$coll/$f" >"$STATE/data/$coll/$f"; done
    log "prep done: $coll"
    ;;
run)
    jobs=${1:-$HERE/fanout-jobs.tsv}
    : "${IMAGE:?set IMAGE to a strata-server image, e.g. ghcr.io/chloetech-ai/strata-server:<sha>}"
    COLLECTION=${COLLECTION:-msmarco-1000k}
    [ -f "$STATE/data/$COLLECTION/meta.json" ] || { echo "no $COLLECTION prepared (fanout.sh prep)"; exit 1; }
    CONTEXT=$(kubectl config current-context)
    export IMAGE COLLECTION CONTEXT
    while :; do
        up=1 left=0
        # Running jobs: nodes in use; a job whose process is gone (this
        # script stopped) is retried on new pools.
        while read -r name engine shape hours envs; do
            s=$(jstate "$name")
            case $s in
            done | failed | skipped | queued)
                live=$(pools_live "$name")
                if [ "$live" -gt 0 ]; then
                    delete_pools "$name"
                    up=$((up + $(pools_live "$name")))
                fi
                ;;
            starting | running)
                pid=$(cat "$STATE/jobs/$name/pid" 2>/dev/null || echo 0)
                if kill -0 "$pid" 2>/dev/null; then
                    up=$((up + $(nodes_of "$shape")))
                else
                    log "$name: its process is gone ($s): its pools go, and it runs again"
                    delete_pools "$name"
                    kubectl delete namespace "$name" --wait=false >/dev/null 2>&1 || true
                    setstate "$name" queued
                fi
                ;;
            esac
        done < <(jobs_of "$jobs")
        # What the cluster has, so that two runs at once share MAX_NODES.
        if n=$(nodes_up 2>/dev/null) && [ -n "$n" ] && [ "$n" -gt "$up" ]; then up=$n; fi
        # Started jobs still copying their data (COPY_JOBS).
        copying=0
        for d in "$STATE"/jobs/*/; do
            n=$(basename "$d")
            [[ $n == *.attempt* ]] && continue
            case $(jstate "$n") in starting | running) [ -f "$d/ready" ] || copying=$((copying + 1)) ;; esac
        done
        while read -r name engine shape hours envs; do
            s=$(jstate "$name")
            case $s in done | failed | skipped) continue ;; esac
            left=$((left + 1))
            [ "$s" = queued ] || continue
            need=$(nodes_of "$shape")
            [ $((up + need)) -le "$MAX_NODES" ] || continue
            [ "$COPY_JOBS" = 0 ] || [ "$copying" -lt "$COPY_JOBS" ] || continue
            mkdir -p "$STATE/jobs/$name"
            echo "$hours" >"$STATE/jobs/$name/timeout_h"
            read -r spent bound <<<"$(cost)"
            add=$(apply_env "$envs" && job_cost "$shape" "$hours")
            if python3 -c "import sys; sys.exit(0 if $bound + $add <= $BUDGET else 1)"; then
                a=$(($(cat "$STATE/jobs/$name/attempts" 2>/dev/null || echo 0) + 1))
                if [ $a -gt "$(max_attempts "$name")" ]; then setstate "$name" failed; continue; fi
                echo $a >"$STATE/jobs/$name/attempts"
                [ $a -gt 1 ] && [ -d "$STATE/jobs/$name/work" ] && mv "$STATE/jobs/$name" "$STATE/jobs/$name.attempt$((a - 1))" &&
                    mkdir -p "$STATE/jobs/$name" && echo $a >"$STATE/jobs/$name/attempts" && echo "$hours" >"$STATE/jobs/$name/timeout_h" &&
                    { cp "$STATE/jobs/$name.attempt$((a - 1))/requeued" "$STATE/jobs/$name/" 2>/dev/null || true; }
                (apply_env "$envs" && create_pools "$name" "$shape" "$a") || { delete_pools "$name"; continue; }
                setstate "$name" starting
                (apply_env "$envs" && exec setsid nohup "$0" _job "$name" "$engine" "$shape" "$hours") </dev/null >>"$STATE/jobs/$name/driver.log" 2>&1 &
                echo $! >"$STATE/jobs/$name/pid"
                up=$((up + need)) copying=$((copying + 1))
                log "$name: started (attempt $a, $need nodes; $up up; spent \$$spent, at most \$$bound before it, +\$$add)"
            elif [ "$up" -le 1 ]; then
                log "$name: skipped: \$$bound + \$$add would pass the budget of \$$BUDGET"
                setstate "$name" skipped
            fi
        done < <(jobs_of "$jobs")
        # Failed jobs run once more.
        for f in "$STATE"/jobs/*/state; do
            name=$(basename "$(dirname "$f")")
            [[ $name == *.attempt* ]] && continue
            if [ "$(cat "$f")" = failed ] && [ "$(cat "$STATE/jobs/$name/attempts" 2>/dev/null || echo 99)" -lt "$(max_attempts "$name")" ] &&
                ! grep -q "load timed out" "$STATE/jobs/$name/reason" 2>/dev/null; then
                log "$name: failed ($(cat "$STATE/jobs/$name/reason" 2>/dev/null || echo ?)); it runs once more"
                setstate "$name" queued
            fi
        done
        [ "$left" = 0 ] && break
        sleep 30
    done
    read -r spent bound <<<"$(cost)"
    log "all jobs ended; spent \$$spent so far (list price)"
    ;;
requeue)
    # requeue NAME: the job runs again at the next `run` (its last attempt kept).
    name=${1:?requeue NAME}
    echo $(($(cat "$STATE/jobs/$name/requeued" 2>/dev/null || echo 0) + 1)) >"$STATE/jobs/$name/requeued"
    setstate "$name" queued
    log "$name: queued again"
    ;;
_job)
    COLLECTION=${COLLECTION:-msmarco-1000k}
    export IMAGE COLLECTION CONTEXT
    job "$@"
    ;;
status)
    jobs=${1:-$HERE/fanout-jobs.tsv}
    read -r spent bound <<<"$(cost)"
    printf '%-12s %-9s %-8s %s\n' JOB STATE MINUTES STEP
    while read -r name engine shape hours envs; do
        s=$(jstate "$name")
        d=$STATE/jobs/$name
        m=""
        [ -f "$d/pools" ] && m=$((($(now) - $(stat -c %Y "$d/pools")) / 60))
        step=$(tail -1 "$d/steps.tsv" 2>/dev/null | awk -F'\t' '{print $1 " " $2 " " $5}' || true)
        last=$(grep 'step [0-9]*:' "$d/driver.log" 2>/dev/null | tail -1 | sed 's/.*: step/step/' || true)
        printf '%-12s %-9s %-8s %s\n' "$name" "$s" "$m" "${last:-$step}"
    done < <(jobs_of "$jobs")
    echo "nodes: $(kubectl get nodes --no-headers 2>/dev/null | wc -l); spent \$$spent so far, at most \$$bound with the running jobs to their timeouts (budget \$$BUDGET)"
    ;;
analyze)
    out=${1:?analyze DIR}
    COLLECTION=${COLLECTION:-msmarco-1000k}
    mkdir -p "$out/all" "$out/strata-b" "$out/jobs"
    for d in "$STATE"/jobs/*/; do
        name=$(basename "$d")
        mkdir -p "$out/jobs/$name"
        cp -r "$d"/{nodes.jsonl,steps.tsv,log,reason,failures} "$out/jobs/$name/" 2>/dev/null || true
        [ -d "$d/work/compare" ] || continue
        case $name in
        *.attempt*) ;;
        strata-b) cp -r "$d/work/compare/." "$out/strata-b/" ;;
        *)
            for f in "$d"/work/compare/*; do
                case $(basename "$f") in
                cpu.jsonl) cat "$f" >>"$out/all/cpu.jsonl" ;;
                *) cp -r "$f" "$out/all/" ;;
                esac
            done
            ;;
        esac
    done
    for set in all strata-b; do
        [ -n "$(ls "$out/$set")" ] || continue
        in_base "rm -rf /srv/reports/$set && mkdir -p /srv/reports/$set"
        tar -C "$out/$set" -c . | $K exec -i base -- tar -x -C "/srv/reports/$set"
        $K exec base -- bash -c "python3 /srv/scripts/strata-server/deploy/compare/analyze.py /srv/reports/$set /srv/data/$COLLECTION" >"$out/$set/tables.md"
        $K exec base -- cat "/srv/reports/$set/summary.json" >"$out/$set/summary.json" 2>/dev/null || true
        log "analyze: $out/$set/tables.md"
    done
    ;;
down)
    do_api DELETE "/kubernetes/clusters/$(cluster_id)/destroy_with_associated_resources/dangerous" >/dev/null || true
    sleep 60
    left=$(do_api GET "/droplets?tag_name=strata-bench" | jq_ 'len(d["droplets"])')
    clusters=$(do_api GET /kubernetes/clusters | jq_ "sum(c['name'] == '$NAME' for c in d['kubernetes_clusters'])")
    e2e=$(do_api GET "/droplets?per_page=200" | jq_ "sorted(x['name'] for x in d['droplets'] if 'strata-bench' not in x.get('tags', []))")
    echo "left: $left droplets tagged strata-bench, $clusters clusters named $NAME (droplets take a minute or two to go); others, untouched: $e2e"
    [ "$left" = 0 ] && [ "$clusters" = 0 ] && mv "$STATE/cluster_id" "$STATE/cluster_id.deleted-$(now)"
    ;;
*)
    sed -n '2,37p' "$0"
    exit 1
    ;;
esac
