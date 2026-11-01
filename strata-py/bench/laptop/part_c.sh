#!/usr/bin/env bash
# Part C of the laptop benchmark: the Shop app's twelve queries, unchanged,
# at four stages, every answer checked against stage 1's (part_c.py).
#
#   part_c.sh file SHOP_DIR       build shop.strata from the Shop data (once)
#   part_c.sh app                 stage 1: Python opens the file in-process
#   part_c.sh server              stage 2: strata-server serves a copy of it on 127.0.0.1:7790
#   part_c.sh cluster             stage 3: on the kind cluster of compare.sh, the copy is shard 0
#                                 behind a coordinator; two empty shards join and the slots
#                                 rebalance onto them while a client queries; then the queries again
#   part_c.sh devloop             stage 4: git clone to a running 3-shard cluster (operator) on a
#                                 new kind cluster, each step timed; then it is deleted
#   part_c.sh compare             the table of answers against stage 1
#   part_c.sh down                remove stage 3's pods
#
# Environment: STRATA_WORK (default ~/strata-bench), where shop.strata and the
# results (STRATA_WORK/part-c) go; PY (default python3), a Python with strata-py;
# SERVER (default strata-server on PATH, else this checkout's release build);
# CLUSTER (default strata, compare.sh's kind cluster); REPO_URL for devloop.
# Every number names the machine: part_c.py writes it in each report.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
WORK=${STRATA_WORK:-$HOME/strata-bench}
OUT=$WORK/part-c
PY=${PY:-python3}
SERVER=${SERVER:-$(command -v strata-server || echo "$ROOT/strata-server/target/release/strata-server")}
CLUSTER=${CLUSTER:-strata}
NS=stages
K="kubectl --context kind-$CLUSTER -n $NS"
IMAGE=strata-server:compare
REPO_URL=${REPO_URL:-https://github.com/chloetech-ai/strata}
mkdir -p "$OUT"

now() { $PY -c 'import time; print(time.time())'; }
since() { $PY -c "import time; print(round(time.time() - $1, 1))"; }
log() { echo "[$(date +%H:%M:%S)] $*" >&2; }

# shard I on NODE: an empty server of the shop's first table (or, for 0, the
# file), its gRPC port 7701; CPU and memory as one Strata shard of compare.sh.
shard() {
    local i=$1 node=$2 db=$3 extra=${4:-}
    cat <<EOF | $K apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: stage-$i
  labels: {app: stage-shard, engine: stage}
spec:
  hostname: stage-$i
  subdomain: stage
  nodeName: $node
  containers:
    - name: strata
      image: $IMAGE
      imagePullPolicy: IfNotPresent
      args: [--db, /local/stages/$db, --listen, "0.0.0.0:7700", --grpc, "0.0.0.0:7701"$extra]
      readinessProbe:
        httpGet: {path: /v1/health, port: 7700}
        periodSeconds: 2
      resources:
        requests: {cpu: "1", memory: 2Gi}
        limits: {cpu: "1", memory: 2Gi}
      volumeMounts:
        - {name: local, mountPath: /local}
  volumes:
    - name: local
      hostPath: {path: /var/local/compare, type: Directory}
EOF
}

case "${1:-}" in
file)
    $PY "$HERE/part_c.py" build --shop "${2:?SHOP_DIR}" --out "$WORK/shop.strata"
    ;;
app)
    $PY "$HERE/part_c.py" run --db "$WORK/shop.strata" --label app --out "$OUT"
    ;;
server)
    rm -rf "$WORK/stage2" && mkdir -p "$WORK/stage2"
    cp "$WORK/shop.strata" "$WORK/stage2/shop.strata"
    t0=$(now)
    "$SERVER" --db "$WORK/stage2/shop.strata" --listen 127.0.0.1:7790 --grpc off >"$WORK/stage2/serve.log" 2>&1 &
    pid=$!
    trap 'kill $pid 2>/dev/null || true' EXIT
    until curl -sf http://127.0.0.1:7790/v1/health >/dev/null; do sleep 0.2; done
    echo "{\"stage\": \"server\", \"ready_seconds\": $(since "$t0")}" >"$OUT/setup-server.json"
    $PY "$HERE/part_c.py" run --url http://127.0.0.1:7790 --label server --out "$OUT"
    ps -o rss= -p $pid | awk '{print "{\"rss_bytes\": " $1 * 1024 "}"}' >"$OUT/memory-server.json"
    ;;
cluster)
    kubectl --context "kind-$CLUSTER" create namespace $NS --dry-run=client -o yaml | kubectl --context "kind-$CLUSTER" apply -f -
    cat <<EOF | $K apply -f -
apiVersion: v1
kind: Service
metadata: {name: stage}
spec:
  clusterIP: None
  publishNotReadyAddresses: true
  selector: {app: stage-shard}
  ports: [{name: http, port: 7700}, {name: grpc, port: 7701}]
EOF
    t0=$(now)
    log "copying shop.strata to strata-worker as shard 0"
    docker exec "$CLUSTER-worker" mkdir -p /var/local/compare/stages
    docker cp "$WORK/shop.strata" "$CLUSTER-worker:/var/local/compare/stages/shop.strata"
    for n in "$CLUSTER-worker2" "$CLUSTER-worker3"; do docker exec "$n" mkdir -p /var/local/compare/stages; done
    shard 0 "$CLUSTER-worker" shop.strata
    $K wait --for=condition=Ready pod/stage-0 --timeout=600s
    cat <<EOF | $K apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: stage-coordinator
  labels: {app: stage-coordinator, engine: stage}
spec:
  nodeName: $CLUSTER-worker3
  containers:
    - name: strata
      image: $IMAGE
      imagePullPolicy: IfNotPresent
      args: [--listen=0.0.0.0:7700, --grpc=off, --shards=http://stage-0.stage:7701]
      readinessProbe:
        httpGet: {path: /v1/health, port: 7700}
        periodSeconds: 2
      resources:
        requests: {cpu: "1", memory: 2Gi}
        limits: {cpu: "1", memory: 2Gi}
EOF
    $K wait --for=condition=Ready pod/stage-coordinator --timeout=300s
    $K port-forward pod/stage-coordinator 7791:7700 >"$OUT/port-forward.log" 2>&1 &
    pf=$!
    trap 'kill $pf 2>/dev/null || true' EXIT
    until curl -sf http://127.0.0.1:7791/v1/health >/dev/null; do sleep 0.5; done
    echo "{\"stage\": \"one shard\", \"ready_seconds\": $(since "$t0")}" >"$OUT/setup-cluster1.json"
    log "stage 3a: one shard behind the coordinator"
    $PY "$HERE/part_c.py" run --url http://127.0.0.1:7791 --label cluster-1-shard --out "$OUT"

    log "two empty shards join (the products schema), then the slots rebalance while a client queries"
    docker cp "$WORK/shop.schema.json" "$CLUSTER-worker2:/var/local/compare/stages/shop.schema.json"
    docker cp "$WORK/shop.schema.json" "$CLUSTER-worker3:/var/local/compare/stages/shop.schema.json"
    shard 1 "$CLUSTER-worker2" shard1.strata ', --schema, /local/stages/shop.schema.json'
    shard 2 "$CLUSTER-worker3" shard2.strata ', --schema, /local/stages/shop.schema.json'
    $K wait --for=condition=Ready pod/stage-1 pod/stage-2 --timeout=300s
    t1=$(now)
    for i in 1 2; do
        curl -sf -X POST http://127.0.0.1:7791/v1/admin/shards -H 'content-type: application/json' \
            -d "{\"servers\": [\"http://stage-$i.stage:7701\"]}" >"$OUT/add-shard-$i.json"
    done
    $PY "$HERE/part_c.py" run --url http://127.0.0.1:7791 --label moving --loop 900 --out "$OUT" &
    client=$!
    curl -sf -X POST http://127.0.0.1:7791/v1/admin/rebalance -H 'content-type: application/json' -d '{}' >"$OUT/rebalance.json"
    while :; do
        curl -sf http://127.0.0.1:7791/v1/admin/moves >"$OUT/moves.json"
        state=$($PY -c "import json; j = json.load(open('$OUT/moves.json')); print((j.get('job') or {}).get('state', ''))")
        log "moves: $state"
        case "$state" in done | failed | stopped) break ;; esac
        sleep 5
    done
    echo "{\"stage\": \"three shards\", \"rebalance_seconds\": $(since "$t1"), \"state\": \"$state\"}" >"$OUT/setup-cluster3.json"
    sleep 20 # the client keeps querying a little after the last move
    kill $client 2>/dev/null || true
    wait $client 2>/dev/null || true
    curl -sf http://127.0.0.1:7791/v1/cluster >"$OUT/cluster.json"
    log "stage 3b: three shards"
    $PY "$HERE/part_c.py" run --url http://127.0.0.1:7791 --label cluster-3-shards --out "$OUT"
    for p in stage-0 stage-1 stage-2 stage-coordinator; do
        $K exec $p -- sh -c 'cat /sys/fs/cgroup/memory.current' | awk -v p=$p '{print "{\"pod\": \"" p "\", \"memory_bytes\": " $1 "}"}'
    done >"$OUT/memory-cluster.jsonl"
    for n in "$CLUSTER-worker" "$CLUSTER-worker2" "$CLUSTER-worker3"; do
        docker exec "$n" du -sb /var/local/compare/stages | awk -v n=$n '{print "{\"node\": \"" n "\", \"bytes\": " $1 "}"}'
    done >"$OUT/disk-cluster.jsonl"
    ;;
devloop)
    dev=stages-dev
    d=$(mktemp -d)
    steps=()
    step() {
        local name=$1
        shift
        local t
        t=$(now)
        log "devloop: $name"
        "$@"
        steps+=("{\"step\": \"$name\", \"seconds\": $(since "$t")}")
    }
    t0=$(now)
    step clone git clone --depth 1 "$REPO_URL" "$d/strata"
    step image docker build -f "$d/strata/strata-server/deploy/Dockerfile" -t strata-server:dev "$d/strata"
    step kind kind create cluster --name $dev --wait 120s
    step load kind load docker-image strata-server:dev --name $dev
    sed 's#image: .*strata-server.*#image: strata-server:dev#' "$d/strata/strata-server/deploy/k8s/operator/operator.yaml" >"$d/operator.yaml"
    sed -e 's#image: .*strata-server.*#image: strata-server:dev#' -e 's#shards: [0-9]*#shards: 3#' -e 's#replicas: [0-9]*#replicas: 1#' "$d/strata/strata-server/deploy/k8s/operator/cluster.yaml" >"$d/cluster.yaml"
    step operator kubectl --context kind-$dev apply -f "$d/operator.yaml"
    step cluster kubectl --context kind-$dev apply -f "$d/cluster.yaml"
    wait_ready() {
        until kubectl --context kind-$dev -n strata get strata -o jsonpath='{.items[0].status.phase}' 2>/dev/null | grep -qx Ready; do sleep 3; done
    }
    step ready wait_ready
    total=$(since "$t0")
    (IFS=,; echo "{\"stage\": \"devloop\", \"total_seconds\": $total, \"steps\": [${steps[*]}]}") >"$OUT/devloop.json"
    kubectl --context kind-$dev -n strata get strata -o json >"$OUT/devloop-cluster.json" || true
    kind delete cluster --name $dev
    rm -rf "$d"
    ;;
compare)
    $PY "$HERE/part_c.py" compare --out "$OUT"
    ;;
down)
    kubectl --context "kind-$CLUSTER" delete namespace $NS --wait=true
    for n in "$CLUSTER-worker" "$CLUSTER-worker2" "$CLUSTER-worker3"; do docker exec "$n" rm -rf /var/local/compare/stages; done
    ;;
*)
    sed -n '2,22p' "$0"
    exit 1
    ;;
esac
