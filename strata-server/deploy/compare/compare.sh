#!/usr/bin/env bash
# Strata, Elasticsearch, OpenSearch, Solr and Vespa on the kind cluster of
# deploy/kind-test.sh, one engine at a time, each with 2 shards on the same
# two nodes and the same CPU and memory in all; and nine more engines, each
# one node with the same CPU and memory (meili, typesense, manticore,
# qdrant, weaviate, milvus, paradedb, postgres, redis). See README.md.
#
#   compare.sh image                  # build the image and load it and the engines' images into the nodes
#   compare.sh build                  # Strata: build both shards, one after the other, each on its node (timed)
#   compare.sh up ENGINE              # start an engine: strata, es, os, solr, vespa or one of the nine
#   compare.sh load ENGINE            # load the collection into it (timed), then its size on disk (any engine but strata)
#   compare.sh run ENGINE [ARGS]      # time the query kinds and record answers; CPU use around it
#   compare.sh writes ENGINE [ARGS]   # time writes, updates and how soon a write is found (after run: it changes the index, then puts it back)
#   compare.sh stream ENGINE [ARGS]   # stream documents through Redpanda into it: rates, flat out, a consumer crash (after run)
#   compare.sh replay ENGINE PROFILE [ARGS]  # replay a traffic profile's schedule into it, open loop (after run: its writes are put back)
#   compare.sh trial                  # Elasticsearch: start the 30-day trial licence (hybrid retrievers)
#   compare.sh down ENGINE            # stop it (Strata's shard files stay on the nodes)
#
# The trial licence's own run: compare.sh trial, then
#   compare.sh run es --kinds "hybrid (RRF),hybrid (weighted)" --paid --label es-trial
# Reports land in STRATA_WORK/compare (/data/compare in the cluster; OUT=/data/x for another).
# COLLECTION=nq-1000k (default quora) picks the collection under STRATA_DATA for every step.
#
# Another cluster than kind's (fanout.sh runs every engine on DigitalOcean
# this way): CONTEXT (the kubectl context, default kind-$CLUSTER), NS, NODES
# (the two data nodes, space-separated), CLIENT_NODE, IMAGE (pulled, not
# built: `image` then only makes the namespace) and the budgets below can
# all be set in the environment. Each node needs /beir/$COLLECTION; reports
# land on the client node's /data, and STRATA_WORK is then a directory here
# that gets the CPU and disk lines.
set -euo pipefail

CLUSTER=${CLUSTER:-strata}
CONTEXT=${CONTEXT:-kind-$CLUSTER}
KIND=0
[ "$CONTEXT" = "kind-$CLUSTER" ] && KIND=1
IMAGE=${IMAGE:-strata-server:compare}
ES_IMAGE=elasticsearch:9.5.5
SOLR_IMAGE=solr:10.1.0
OS_IMAGE=opensearchproject/opensearch:3.9.0
VESPA_IMAGE=vespaengine/vespa:8.778.45
REDPANDA_IMAGE=redpandadata/redpanda:v25.2.9
# The nine one-node engines (more/*.yaml; paradedb and postgres share postgres.yaml).
MORE_IMAGES=(getmeili/meilisearch:v1.56.0 typesense/typesense:30.2 manticoresearch/manticore:29.9.0 qdrant/qdrant:v1.20.0
    semitechnologies/weaviate:1.39.14 milvusdb/milvus:v2.6.26 paradedb/paradedb:0.26.4-pg17 redis:8.8.4)
MORE="meili typesense manticore qdrant weaviate milvus paradedb postgres redis"
NS=${NS:-compare}
HERE=$(cd "$(dirname "$0")" && pwd)
K="kubectl --context $CONTEXT -n $NS"
# Data pods on the first two workers; clients on the third.
read -r -a NODES <<<"${NODES:-strata-worker strata-worker2}"
CLIENT_NODE=${CLIENT_NODE:-strata-worker3}
# Every engine: 3 CPUs and 6 GiB in all. Elasticsearch, OpenSearch and
# Solr: two data nodes of 1.5 CPUs and 3 GiB, which also coordinate.
# Strata: as they, two pods of a data node's DATA_CPU and DATA_MEM, each
# with a shard and a coordinator over both shards (strata-coordinating.yaml);
# the client sends its requests to both coordinators. Loading gets the
# whole budget too: Elasticsearch and Solr on their two data nodes, Strata
# one shard's build job at a time.
# By default each pod runs one server, the shard with its coordinator in
# its process, which calls that shard in-process (--coordinator-listen).
# STRATA_COORD=shards runs them as two processes in the pod, as from 6
# November 2026 until then (and as images without --coordinator-listen
# must run). STRATA_COORD=pod runs Strata as the runs before 6 November
# 2026 did: two shards of SHARD_CPU and SHARD_MEM (1 CPU and 2.5 GiB) and a
# coordinator of COORD_CPU and COORD_MEM (1 CPU and 1 GiB) in a pod of its
# own on the clients' node (strata.yaml, strata-coordinator.yaml).
# BUILD_PARALLEL=1 builds both shards at once, each with its node's share
# (a node then holds BUILD_CPU and BUILD_MEM, not the whole budget).
DATA_CPU=${DATA_CPU:-1500m} DATA_MEM=${DATA_MEM:-3Gi}
BUILD_CPU=${BUILD_CPU:-3} BUILD_MEM=${BUILD_MEM:-6Gi} BUILD_THREADS=${BUILD_THREADS:-3} BUILD_PARALLEL=${BUILD_PARALLEL:-0}
SHARD_CPU=${SHARD_CPU:-1} SHARD_MEM=${SHARD_MEM:-2560Mi}
COORD_CPU=${COORD_CPU:-1} COORD_MEM=${COORD_MEM:-1Gi}
STRATA_COORD=${STRATA_COORD:-process}
case $STRATA_COORD in process | pod | shards) ;; *) echo "STRATA_COORD is process (a coordinator in each shard's process, the default), shards (in each shard's pod, a process of its own) or pod (one in its own pod)" >&2 && exit 2 ;; esac
# Vespa: vespa-0 (config server, cluster controller and the container that
# answers queries and takes documents) on the clients' node, as Strata's
# coordinator, and a content node on each data node; 1 CPU each, 6 GiB in all.
VESPA_CONFIG_CPU=${VESPA_CONFIG_CPU:-1} VESPA_CONFIG_MEM=${VESPA_CONFIG_MEM:-2304Mi} VESPA_CONTAINER_HEAP=${VESPA_CONTAINER_HEAP:-768m}
VESPA_CONTENT_CPU=${VESPA_CONTENT_CPU:-1} VESPA_CONTENT_MEM=${VESPA_CONTENT_MEM:-1920Mi}
# The one-node engines: the whole budget in one pod, on the first data node.
ONE_CPU=${ONE_CPU:-3} ONE_MEM=${ONE_MEM:-6Gi}
# What the engines are told of that budget (heaps, threads, memory caps),
# written for 3 CPUs and 6 GiB; another budget sets them to match.
ES_HEAP=${ES_HEAP:-1536m} OS_HEAP=${OS_HEAP:-1536m} SOLR_HEAP=${SOLR_HEAP:-1g}
ONE_THREADS=${ONE_THREADS:-3} TS_THREADS=${TS_THREADS:-6} MEILI_MEM=${MEILI_MEM:-3 GiB} REDIS_MAXMEM=${REDIS_MAXMEM:-5500mb}
GOMEMLIMIT_ONE=${GOMEMLIMIT_ONE:-5GiB} PG_SHARED=${PG_SHARED:-1536MB} PG_CACHE=${PG_CACHE:-4GB} PG_MAINT=${PG_MAINT:-2560MB}
PG_PAR=${PG_PAR:-2} PG_WORKERS=${PG_WORKERS:-8}
# engine-compare load's bulk requests in flight (its default is 4).
LOAD_CLIENTS=${LOAD_CLIENTS:-4}
OUT=${OUT:-/data/compare}
# The collection under STRATA_DATA (prepare_beir.py or prepare_beir_local.py), e.g. nq-1000k.
COLLECTION=${COLLECTION:-quora}

# render FILE I NODE CPU MEM [ZK|ROLE]: a manifest for node I.
render() {
    sed -e "s/__I__/$2/g" -e "s/__NODE__/$3/g" -e "s/__CPU__/$4/g" -e "s/__MEM__/$5/g" -e "s|__ZK__|${6:-}|g" -e "s|__ROLE__|${6:-}|g" -e "s|__COLL__|$COLLECTION|g" \
        -e "s|__NS__|$NS|g" -e "s|__IMAGE__|$IMAGE|g" -e "s|__IN_PROCESS__|$([ "$STRATA_COORD" = process ] && echo 1)|g" -e "s|__HEAP__|${HEAP:-}|g" -e "s|__THREADS__|$ONE_THREADS|g" -e "s|__TS_THREADS__|$TS_THREADS|g" \
        -e "s|__MEILI_MEM__|$MEILI_MEM|g" -e "s|__REDIS_MAXMEM__|$REDIS_MAXMEM|g" -e "s|__GOMEMLIMIT__|$GOMEMLIMIT_ONE|g" -e "s|__PG_SHARED__|$PG_SHARED|g" \
        -e "s|__PG_CACHE__|$PG_CACHE|g" -e "s|__PG_MAINT__|$PG_MAINT|g" -e "s|__PG_PAR__|$PG_PAR|g" -e "s|__PG_WORKERS__|$PG_WORKERS|g" "$HERE/$1"
}

# Strata's token: random, made once per cluster and kept in the secret
# strata-token, which its pods and the client read (nothing fixed in the repo).
token_secret() {
    # Two jobs at once (BUILD_PARALLEL) may both find it missing: the one
    # that loses the race finds the other's.
    $K get secret strata-token >/dev/null 2>&1 ||
        $K create secret generic strata-token --from-literal=token="$(python3 -c 'import secrets; print(secrets.token_hex(16))')" >/dev/null 2>&1 ||
        $K get secret strata-token >/dev/null
}

# job NAME NODE CPU MEM ARGS...: engine-compare as a Job on NODE (CPU and
# MEM empty: no limits), following its log until it ends.
job() {
    local name=$1 node=$2 cpu=$3 mem=$4
    shift 4
    local args res=""
    token_secret
    args=$(python3 -c 'import json, sys; print(json.dumps(sys.argv[1:]))' "$@" --collection "$COLLECTION")
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
      tolerations: [{operator: Exists}]
      containers:
        - name: compare
          image: $IMAGE
          imagePullPolicy: IfNotPresent
          command: ["engine-compare"]
          args: $args
          $res
          env:
            - {name: STRATA_DATA, value: /beir}
            - {name: STRATA_TOKEN, valueFrom: {secretKeyRef: {name: strata-token, key: token, optional: true}}}
          volumeMounts:
            - {name: beir, mountPath: /beir, readOnly: true}
            - {name: data, mountPath: /data}
            - {name: local, mountPath: /local}
      volumes:
        - name: beir
          hostPath: {path: /beir, type: Directory}
        - name: data
          hostPath: {path: /data, type: DirectoryOrCreate}
        - name: local
          hostPath: {path: /var/local/compare, type: DirectoryOrCreate}
EOF
    until phase=$($K get pod -l "job-name=$name" -o jsonpath='{.items[0].status.phase}' 2>/dev/null) && [ -n "$phase" ] && [ "$phase" != Pending ]; do sleep 1; done
    # A remote API server may cut a log followed for hours: follow it again
    # from where it stopped until the Job has succeeded or failed.
    local since="" s
    while :; do
        $K logs -f "job/$name" ${since:+--since-time="$since"} || true
        since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
        s=$($K get job "$name" -o jsonpath='{.status.succeeded}/{.status.failed}' 2>/dev/null || echo "?")
        case $s in
        1/*) return 0 ;;
        */[1-9]*) echo "job $name failed" >&2; return 1 ;;
        esac
        sleep 5
    done
}

# node_cat NODE PATH: a file on a node's own disk, by docker on kind and by
# a short pod (the image already on every node) elsewhere.
node_cat() {
    if [ $KIND = 1 ]; then
        docker exec "$1" cat "$2"
        return
    fi
    local name="cat-$(date +%s%N | tail -c 9)"
    $K apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: {name: $name}
spec:
  nodeName: $1
  tolerations: [{operator: Exists}]
  containers:
    - {name: c, image: $IMAGE, imagePullPolicy: IfNotPresent, command: [sleep, "600"], volumeMounts: [{name: f, mountPath: /f, readOnly: true}]}
  volumes:
    - {name: f, hostPath: {path: $(dirname "$2")}}
EOF
    $K wait --for=condition=Ready "pod/$name" --timeout=300s >/dev/null
    $K exec "$name" -- cat "/f/$(basename "$2")"
    $K delete pod "$name" --wait=false >/dev/null
}

urls() {
    case $1 in
    strata)
        if [ "$STRATA_COORD" != pod ]; then
            echo http://strata-0.strata:7710,http://strata-1.strata:7710
        else
            echo http://strata-coordinator:7700
        fi
        ;;
    es) echo http://es-0.es:9200,http://es-1.es:9200 ;;
    os) echo http://os-0.os:9200,http://os-1.os:9200 ;;
    vespa) echo http://vespa-0.vespa:8080 ;;
    solr) echo http://solr-0.solr:8983,http://solr-1.solr:8983 ;;
    meili) echo http://meili-0.meili:7700 ;;
    typesense) echo http://typesense-0.typesense:8108 ;;
    manticore) echo http://manticore-0.manticore:9308 ;;
    qdrant) echo http://qdrant-0.qdrant:6333 ;;
    weaviate) echo http://weaviate-0.weaviate:8080 ;;
    milvus) echo http://milvus-0.milvus:19530 ;;
    paradedb | postgres) echo "postgres://$1-0.$1:5432" ;;
    redis) echo redis://redis-0.redis:6379 ;;
    esac
}

# A one-node engine's data directory (what compare.sh load measures on disk).
datadir() {
    case $1 in
    meili) echo /meili_data ;;
    typesense) echo /data ;;
    manticore) echo /var/lib/manticore ;;
    qdrant) echo /qdrant/storage ;;
    weaviate) echo /var/lib/weaviate ;;
    milvus) echo /var/lib/milvus ;;
    paradedb | postgres) echo /var/lib/postgresql/data/pg ;;
    esac
}

is_more() {
    case " $MORE " in *" $1 "*) return 0 ;; esac
    return 1
}

# cpu ENGINE TAG [LABEL]: each of the engine's containers' CPU time so far
# (cgroup cpu.stat) and memory (memory.current and memory.peak, and of the
# current its anonymous and file pages from memory.stat, as mem_*), one JSON
# line per pod in $STRATA_WORK/compare/cpu.jsonl (for another OUT, the
# directory of that name). LABEL names the run, as engine-compare's --label
# does (the engine's name if none).
cpu() {
    for pod in $($K get pods -l "engine=$1" -o jsonpath='{.items[*].metadata.name}'); do
        stat=$({ $K exec "$pod" -- cat /sys/fs/cgroup/cpu.stat /sys/fs/cgroup/memory.current /sys/fs/cgroup/memory.peak /sys/fs/cgroup/memory.stat 2>/dev/null || true; } |
            awk 'NF == 1 { n++; printf "\"%s\": %s, ", (n == 1 ? "mem_current" : "mem_peak"), $1; next }
                 n == 0 { printf "\"%s\": %s, ", $1, $2; next }
                 $1 == "anon" || $1 == "file" { printf "\"mem_%s\": %s, ", $1, $2 }')
        echo "{\"engine\": \"$1\", \"label\": \"${3:-$1}\", \"tag\": \"$2\", \"pod\": \"$pod\", ${stat}\"time\": $(date +%s)}" >>"${STRATA_WORK:?set STRATA_WORK}/${OUT#/data/}/cpu.jsonl"
    done
}

cmd=${1:-}
shift || true
case "$cmd" in
image)
    if [ $KIND = 0 ]; then
        # Another cluster pulls IMAGE and the engines' pinned images itself.
        kubectl --context "$CONTEXT" create namespace "$NS" --dry-run=client -o yaml | kubectl --context "$CONTEXT" apply -f -
        exit 0
    fi
    docker build -f "$HERE/../Dockerfile" -t "$IMAGE" "$HERE/../../.."
    nodes=$(IFS=,; echo "${NODES[*]},$CLIENT_NODE")
    kind load docker-image "$IMAGE" --name "$CLUSTER" --nodes "$nodes"
    kind load docker-image "$ES_IMAGE" "$OS_IMAGE" "$SOLR_IMAGE" --name "$CLUSTER" --nodes "$(IFS=,; echo "${NODES[*]}")"
    kind load docker-image "$VESPA_IMAGE" --name "$CLUSTER" --nodes "$nodes"
    kind load docker-image "$REDPANDA_IMAGE" --name "$CLUSTER" --nodes "$CLIENT_NODE"
    kind load docker-image "${MORE_IMAGES[@]}" --name "$CLUSTER" --nodes "${NODES[0]}"
    kubectl --context "$CONTEXT" create namespace "$NS" --dry-run=client -o yaml | kubectl --context "$CONTEXT" apply -f -
    ;;
build)
    # One shard after the other, each on the node that will serve it, with
    # the engine's whole budget: a shard's build holds a segment's documents in
    # memory, and two at once in 3 GiB each were killed for lack of memory.
    # With BUILD_PARALLEL=1 both at once, each with its node's share.
    build_shard() {
        job "build-s$1" "${NODES[$1]}" "$BUILD_CPU" "$BUILD_MEM" build --shard "$1" --of 2 --out "/local/$COLLECTION.shard${1}of2.strata" --threads "$BUILD_THREADS"
        node_cat "${NODES[$1]}" "/var/local/compare/$COLLECTION.shard${1}of2.strata.build.json" >"${STRATA_WORK:?}/${OUT#/data/}/build-strata-s$1.json"
    }
    if [ "$BUILD_PARALLEL" = 1 ]; then
        token_secret
        build_shard 0 >"${STRATA_WORK:?}/${OUT#/data/}/build-s0.log" 2>&1 &
        b0=$!
        build_shard 1 >"${STRATA_WORK:?}/${OUT#/data/}/build-s1.log" 2>&1 &
        b1=$!
        # Both are waited for, so that a failed shard does not leave the
        # other running after this step has ended.
        rc=0
        wait $b0 || rc=$?
        wait $b1 || rc=$?
        [ $rc = 0 ] || { tail -5 "${STRATA_WORK:?}/${OUT#/data/}"/build-s[01].log >&2; exit $rc; }
    else
        for i in 0 1; do build_shard $i; done
    fi
    ;;
up)
    if is_more "${1:?up ENGINE}"; then
        f=$1
        case $1 in paradedb | postgres) f=postgres ;; esac
        render "more/$f.yaml" 0 "${NODES[0]}" "$ONE_CPU" "$ONE_MEM" "$1" | $K apply -f -
        $K wait --for=condition=Ready "pod/$1-0" --timeout=600s
        exit 0
    fi
    case $1 in
    strata)
        token_secret
        if [ "$STRATA_COORD" != pod ]; then
            render strata-coordinating.yaml 0 "${NODES[0]}" "$DATA_CPU" "$DATA_MEM" | $K apply -f -
            render strata-coordinating.yaml 1 "${NODES[1]}" "$DATA_CPU" "$DATA_MEM" | awk 'f {print} /^---$/ {f = 1}' | $K apply -f -
            $K wait --for=condition=Ready pod/strata-0 pod/strata-1 --timeout=300s
        else
            render strata.yaml 0 "${NODES[0]}" "$SHARD_CPU" "$SHARD_MEM" | $K apply -f -
            render strata.yaml 1 "${NODES[1]}" "$SHARD_CPU" "$SHARD_MEM" | awk 'f {print} /^---$/ {f = 1}' | $K apply -f -
            render strata-coordinator.yaml c "$CLIENT_NODE" "$COORD_CPU" "$COORD_MEM" | $K apply -f -
            $K wait --for=condition=Ready pod/strata-0 pod/strata-1 pod/strata-coordinator --timeout=300s
        fi
        ;;
    es)
        HEAP=$ES_HEAP render es.yaml 0 "${NODES[0]}" "$DATA_CPU" "$DATA_MEM" | $K apply -f -
        HEAP=$ES_HEAP render es.yaml 1 "${NODES[1]}" "$DATA_CPU" "$DATA_MEM" | awk 'f {print} /^---$/ {f = 1}' | $K apply -f -
        $K wait --for=condition=Ready pod/es-0 pod/es-1 --timeout=300s
        ;;
    os)
        HEAP=$OS_HEAP render os.yaml 0 "${NODES[0]}" "$DATA_CPU" "$DATA_MEM" | $K apply -f -
        HEAP=$OS_HEAP render os.yaml 1 "${NODES[1]}" "$DATA_CPU" "$DATA_MEM" | awk 'f {print} /^---$/ {f = 1}' | $K apply -f -
        $K wait --for=condition=Ready pod/os-0 pod/os-1 --timeout=300s
        ;;
    vespa)
        render vespa.yaml 0 "$CLIENT_NODE" "$VESPA_CONFIG_CPU" "$VESPA_CONFIG_MEM" configserver,services | $K apply -f -
        for i in 1 2; do
            render vespa.yaml $i "${NODES[$((i - 1))]}" "$VESPA_CONTENT_CPU" "$VESPA_CONTENT_MEM" services | awk 'f {print} /^---$/ {f = 1}' | $K apply -f -
        done
        $K wait --for=condition=Ready pod/vespa-0 pod/vespa-1 pod/vespa-2 --timeout=300s
        # The pods are up once their processes are; the application comes
        # from the config server, which takes a minute or two to start.
        until $K exec vespa-0 -- curl -sf http://localhost:19071/state/v1/health >/dev/null 2>&1; do sleep 2; done
        zip=$(mktemp -d)/app.zip
        DIM=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["dim"])' "${STRATA_DATA:?set STRATA_DATA}/$COLLECTION/meta.json") \
            "$HERE/vespa/package.sh" "$zip" 2 1 "$VESPA_CONTAINER_HEAP" "vespa.$NS.svc.cluster.local"
        $K exec -i vespa-0 -- curl -sf -H Content-Type:application/zip --data-binary @- \
            http://localhost:19071/application/v2/tenant/default/prepareandactivate <"$zip" >/dev/null
        rm -rf "$(dirname "$zip")"
        until $K exec vespa-0 -- curl -sf http://localhost:8080/state/v1/health 2>/dev/null | grep -q '"code" : "up"'; do sleep 2; done
        ;;
    solr)
        HEAP=$SOLR_HEAP render solr.yaml 0 "${NODES[0]}" "$DATA_CPU" "$DATA_MEM" "" | $K apply -f -
        $K wait --for=condition=Ready pod/solr-0 --timeout=300s
        HEAP=$SOLR_HEAP render solr.yaml 1 "${NODES[1]}" "$DATA_CPU" "$DATA_MEM" "solr-0.solr.$NS.svc.cluster.local:9983" | awk 'f {print} /^---$/ {f = 1}' | $K apply -f -
        $K wait --for=condition=Ready pod/solr-1 --timeout=300s
        ;;
    esac
    ;;
load)
    e=${1:?load ENGINE}
    shift
    cpu "$e" load-before
    job "load-$e" "$CLIENT_NODE" "" "" load --engine "$e" --url "$(urls "$e")" --clients "$LOAD_CLIENTS" --out "$OUT" "$@"
    cpu "$e" load-after
    # The one-node engines' data directories on disk (Redis keeps everything in memory: load-redis.json has its memory).
    if is_more "$e" && [ "$e" != redis ]; then
        kb=$($K exec "$e-0" -- du -sk "$(datadir "$e")" | awk '{print $1}')
        echo "{\"pod\": \"$e-0\", \"dir\": \"$(datadir "$e")\", \"bytes\": $((kb * 1024))}" >"${STRATA_WORK:?}/${OUT#/data/}/disk-$e.json"
    fi
    # Vespa reports no index size: each content node's documents directory
    # (indexes, attributes and document store) and transaction log, as du
    # sees them once proton has flushed what it holds in memory and the
    # sizes have stopped moving.
    if [ "$e" = vespa ]; then
        for i in 1 2; do
            $K exec vespa-$i -- vespa-proton-cmd --local triggerFlush >/dev/null 2>&1
            d=/opt/vespa/var/db/vespa/search/cluster.docs/n$((i - 1))
            last="" same=0
            while [ $same -lt 3 ]; do
                sleep 5
                now=$($K exec vespa-$i -- du -sb "$d/documents" "$d/tls" | awk '{printf "%s ", $1}')
                if [ "$now" = "$last" ]; then same=$((same + 1)); else same=0 last=$now; fi
            done
            echo "$now" | awk -v p=vespa-$i '{print "{\"pod\": \"" p "\", \"documents_bytes\": " $1 ", \"tls_bytes\": " $2 "}"}'
        done >"${STRATA_WORK:?}/${OUT#/data/}/disk-vespa.jsonl"
    fi
    ;;
run)
    e=${1:?run ENGINE}
    shift
    label=$e prev=""
    for a in "$@"; do
        [ "$prev" = --label ] && label=$a
        prev=$a
    done
    cpu "$e" before "$label"
    # A Job is named in lowercase letters, digits and -: a label such as solr+sql runs as run-solr-sql.
    job "run-${label//+/-}" "$CLIENT_NODE" "" "" run --engine "$e" --url "$(urls "$e")" --out "$OUT" "$@"
    cpu "$e" after "$label"
    ;;
writes)
    e=${1:?writes strata|es|os|solr|vespa}
    shift
    cpu "$e" writes-before
    job "writes-$e" "$CLIENT_NODE" "" "" writes --engine "$e" --url "$(urls "$e")" --out "$OUT" "$@"
    cpu "$e" writes-after
    ;;
stream)
    # One broker beside the clients (1 CPU, 1 GiB, outside the engine's
    # budget), started for the first stream and left for the next.
    e=${1:?stream strata|es|os|solr|vespa}
    shift
    if ! $K get pod redpanda >/dev/null 2>&1; then
        render redpanda.yaml 0 "$CLIENT_NODE" 1 1Gi | $K apply -f -
    fi
    $K wait --for=condition=Ready pod/redpanda --timeout=300s
    cpu "$e" stream-before
    job "stream-$e" "$CLIENT_NODE" "" "" stream --engine "$e" --url "$(urls "$e")" --brokers redpanda.redpanda:9092 --out "$OUT" "$@"
    cpu "$e" stream-after
    ;;
replay)
    # PROFILE is a name under deploy/compare/profiles (in the image) or a
    # path under /data. The schedule is planned in the Job, beside the
    # report (OUT/schedule-PROFILE-SEED.jsonl.gz), and kept.
    e=${1:?replay ENGINE PROFILE}
    p=${2:?replay ENGINE PROFILE}
    shift 2
    case $p in
    */*) ;;
    *) p=/usr/local/share/engine-compare/profiles/${p%.toml}.toml ;;
    esac
    label="" prev=""
    for a in "$@"; do
        [ "$prev" = --label ] && label=$a
        prev=$a
    done
    # Without --label, the run is ENGINE-PROFILE.
    if [ -z "$label" ]; then
        label=$e-$(basename "${p%.toml}")
        set -- "$@" --label "$label"
    fi
    cpu "$e" replay-before "$label"
    jl=$(echo "$label" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9\n' '-')
    job "replay-$jl" "$CLIENT_NODE" "" "" replay --engine "$e" --url "$(urls "$e")" --profile "$p" --out "$OUT" "$@"
    cpu "$e" replay-after "$label"
    ;;
trial)
    $K exec es-0 -- curl -sf -X POST 'http://localhost:9200/_license/start_trial?acknowledge=true'
    echo
    ;;
down)
    case ${1:?down ENGINE} in
    strata) $K delete pod strata-0 strata-1 strata-coordinator --ignore-not-found ;;
    es) $K delete pod es-0 es-1 --ignore-not-found ;;
    os) $K delete pod os-0 os-1 --ignore-not-found ;;
    solr) $K delete pod solr-0 solr-1 --ignore-not-found ;;
    vespa) $K delete pod vespa-0 vespa-1 vespa-2 --ignore-not-found ;;
    *) is_more "$1" && $K delete pod "$1-0" --ignore-not-found ;;
    esac
    ;;
*)
    sed -n '2,18p' "$0"
    exit 2
    ;;
esac
