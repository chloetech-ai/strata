#!/usr/bin/env bash
# Strata, Elasticsearch and Solr on the kind cluster of deploy/kind-test.sh,
# one engine at a time, each with 2 shards on the same two nodes and the
# same CPU and memory in all. See README.md.
#
#   compare.sh image                  # build the image and load it and the engines' images into the nodes
#   compare.sh build                  # Strata: build both shards, one after the other, each on its node (timed)
#   compare.sh up strata|es|solr      # start an engine
#   compare.sh load es|solr           # load the collection into it (timed)
#   compare.sh run strata|es|solr     # time the query kinds and record answers; CPU use around it
#   compare.sh down strata|es|solr    # stop it (Strata's shard files stay on the nodes)
#
# Reports land in STRATA_WORK/compare (/data/compare in the cluster; OUT=/data/x for another).
set -euo pipefail

CLUSTER=${CLUSTER:-strata}
IMAGE=strata-server:compare
ES_IMAGE=elasticsearch:9.5.5
SOLR_IMAGE=solr:10.1.0
NS=compare
HERE=$(cd "$(dirname "$0")" && pwd)
K="kubectl --context kind-$CLUSTER -n $NS"
# Data pods on the first two workers; clients (and Strata's coordinator) on the third.
NODES=(strata-worker strata-worker2)
CLIENT_NODE=strata-worker3
# Every engine: 3 CPUs and 6 GiB in all. Elasticsearch and Solr: two data
# nodes of 1.5 CPUs and 3 GiB. Strata: two shards of 1 CPU and 2.5 GiB and a
# coordinator of 1 CPU and 1 GiB. Loading gets the whole budget
# too: Elasticsearch and Solr on their two data nodes, Strata one shard's
# build job at a time.
# SHARD_CPU and COORD_CPU may be set to split Strata's 3 CPUs otherwise.
DATA_CPU=1500m DATA_MEM=3Gi
BUILD_CPU=3 BUILD_MEM=6Gi
SHARD_CPU=${SHARD_CPU:-1} SHARD_MEM=2560Mi
COORD_CPU=${COORD_CPU:-1} COORD_MEM=1Gi
OUT=${OUT:-/data/compare}

# render FILE I NODE CPU MEM [ZK]: a manifest for node I.
render() {
    sed -e "s/__I__/$2/g" -e "s/__NODE__/$3/g" -e "s/__CPU__/$4/g" -e "s/__MEM__/$5/g" -e "s|__ZK__|${6:-}|g" "$HERE/$1"
}

# job NAME NODE CPU MEM ARGS...: engine-compare as a Job on NODE (CPU and
# MEM empty: no limits), following its log until it ends.
job() {
    local name=$1 node=$2 cpu=$3 mem=$4
    shift 4
    local args res=""
    args=$(python3 -c 'import json, sys; print(json.dumps(sys.argv[1:]))' "$@")
    [ -n "$cpu" ] && res="resources: {requests: {cpu: $cpu, memory: $mem}, limits: {cpu: $cpu, memory: $mem}}"
    $K delete job "$name" --ignore-not-found >/dev/null
    $K apply -f - <<EOF
apiVersion: batch/v1
kind: Job
metadata:
  name: $name
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      nodeName: $node
      containers:
        - name: compare
          image: $IMAGE
          imagePullPolicy: IfNotPresent
          command: ["engine-compare"]
          args: $args
          $res
          env:
            - {name: STRATA_DATA, value: /beir}
          volumeMounts:
            - {name: beir, mountPath: /beir, readOnly: true}
            - {name: data, mountPath: /data}
            - {name: local, mountPath: /local}
      volumes:
        - name: beir
          hostPath: {path: /beir, type: Directory}
        - name: data
          hostPath: {path: /data, type: Directory}
        - name: local
          hostPath: {path: /var/local/compare, type: DirectoryOrCreate}
EOF
    until phase=$($K get pod -l "job-name=$name" -o jsonpath='{.items[0].status.phase}' 2>/dev/null) && [ -n "$phase" ] && [ "$phase" != Pending ]; do sleep 1; done
    $K logs -f "job/$name"
    $K wait --for=condition=complete "job/$name" --timeout=60s >/dev/null
}

urls() {
    case $1 in
    strata) echo http://strata-coordinator:7700 ;;
    es) echo http://es-0.es:9200,http://es-1.es:9200 ;;
    solr) echo http://solr-0.solr:8983,http://solr-1.solr:8983 ;;
    esac
}

# cpu ENGINE TAG: each of the engine's containers' CPU time so far (cgroup
# cpu.stat), one JSON line per pod in $STRATA_WORK/compare/cpu.jsonl (for
# another OUT, the directory of that name).
cpu() {
    for pod in $($K get pods -l "engine=$1" -o jsonpath='{.items[*].metadata.name}'); do
        stat=$($K exec "$pod" -- cat /sys/fs/cgroup/cpu.stat | awk '{printf "\"%s\": %s, ", $1, $2}')
        echo "{\"engine\": \"$1\", \"tag\": \"$2\", \"pod\": \"$pod\", ${stat}\"time\": $(date +%s)}" >>"${STRATA_WORK:?set STRATA_WORK}/${OUT#/data/}/cpu.jsonl"
    done
}

cmd=${1:-}
shift || true
case "$cmd" in
image)
    docker build -f "$HERE/../Dockerfile" -t "$IMAGE" "$HERE/../../.."
    nodes=$(IFS=,; echo "${NODES[*]},$CLIENT_NODE")
    kind load docker-image "$IMAGE" --name "$CLUSTER" --nodes "$nodes"
    kind load docker-image "$ES_IMAGE" "$SOLR_IMAGE" --name "$CLUSTER" --nodes "$(IFS=,; echo "${NODES[*]}")"
    kubectl --context "kind-$CLUSTER" create namespace "$NS" --dry-run=client -o yaml | kubectl --context "kind-$CLUSTER" apply -f -
    ;;
build)
    # One shard after the other, each on the node that will serve it, with
    # the engine's whole budget: a shard's build holds its documents in
    # memory, and two at once in 3 GiB each were killed for lack of memory.
    for i in 0 1; do
        job "build-s$i" "${NODES[$i]}" "$BUILD_CPU" "$BUILD_MEM" build --shard "$i" --of 2 --out "/local/quora.shard${i}of2.strata" --threads 3
        docker exec "${NODES[$i]}" cat "/var/local/compare/quora.shard${i}of2.strata.build.json" >"${STRATA_WORK:?}/${OUT#/data/}/build-strata-s$i.json"
    done
    ;;
up)
    case ${1:?up strata|es|solr} in
    strata)
        render strata.yaml 0 "${NODES[0]}" "$SHARD_CPU" "$SHARD_MEM" | $K apply -f -
        render strata.yaml 1 "${NODES[1]}" "$SHARD_CPU" "$SHARD_MEM" | awk 'f {print} /^---$/ {f = 1}' | $K apply -f -
        render strata-coordinator.yaml c "$CLIENT_NODE" "$COORD_CPU" "$COORD_MEM" | $K apply -f -
        $K wait --for=condition=Ready pod/strata-0 pod/strata-1 pod/strata-coordinator --timeout=300s
        ;;
    es)
        render es.yaml 0 "${NODES[0]}" "$DATA_CPU" "$DATA_MEM" | $K apply -f -
        render es.yaml 1 "${NODES[1]}" "$DATA_CPU" "$DATA_MEM" | awk 'f {print} /^---$/ {f = 1}' | $K apply -f -
        $K wait --for=condition=Ready pod/es-0 pod/es-1 --timeout=300s
        ;;
    solr)
        render solr.yaml 0 "${NODES[0]}" "$DATA_CPU" "$DATA_MEM" "" | $K apply -f -
        $K wait --for=condition=Ready pod/solr-0 --timeout=300s
        render solr.yaml 1 "${NODES[1]}" "$DATA_CPU" "$DATA_MEM" "solr-0.solr.$NS.svc.cluster.local:9983" | awk 'f {print} /^---$/ {f = 1}' | $K apply -f -
        $K wait --for=condition=Ready pod/solr-1 --timeout=300s
        ;;
    esac
    ;;
load)
    e=${1:?load es|solr}
    shift
    cpu "$e" load-before
    job "load-$e" "$CLIENT_NODE" "" "" load --engine "$e" --url "$(urls "$e")" --out "$OUT" "$@"
    cpu "$e" load-after
    ;;
run)
    e=${1:?run strata|es|solr}
    shift
    cpu "$e" before
    job "run-$e" "$CLIENT_NODE" "" "" run --engine "$e" --url "$(urls "$e")" --out "$OUT" "$@"
    cpu "$e" after
    ;;
down)
    case ${1:?down strata|es|solr} in
    strata) $K delete pod strata-0 strata-1 strata-coordinator --ignore-not-found ;;
    es) $K delete pod es-0 es-1 --ignore-not-found ;;
    solr) $K delete pod solr-0 solr-1 --ignore-not-found ;;
    esac
    ;;
*)
    sed -n '2,13p' "$0"
    exit 2
    ;;
esac
