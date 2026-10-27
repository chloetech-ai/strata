#!/usr/bin/env bash
# A local multi-node Kubernetes cluster (kind, in Docker) running Strata as
# shards behind a coordinator, beside one unsplit database to compare with.
# See deploy/README.md for the whole test.
#
#   STRATA_WORK=/path/to/dbs STRATA_DATA=/path/to/beir deploy/kind-test.sh up
#   deploy/kind-test.sh image                 # build the image and load it into the nodes
#   deploy/kind-test.sh single                # the unsplit database
#   deploy/kind-test.sh shards 2              # shards 0..1 of /data/quora.shard<i>of2.strata
#   deploy/kind-test.sh coordinator 2         # point the coordinator at that set
#   deploy/kind-test.sh bench NAME ARGS...    # strata-server-bench as a Job in the cluster
#   deploy/kind-test.sh crash POD             # SIGKILL POD's server; the kubelet restarts it
#   deploy/kind-test.sh delete POD            # stop POD gracefully; its controller replaces it
#   deploy/kind-test.sh pause POD SECONDS     # freeze POD's node for SECONDS
#   deploy/kind-test.sh delay POD MS          # add MS ms to every packet sent to POD (0 removes it)
#   deploy/kind-test.sh down
#
# STRATA_WORK holds the databases (strata-server-bench --build-only writes
# them) and is mounted at /data on every node; reports land in
# STRATA_WORK/results. STRATA_DATA holds the prepared BEIR collections the
# client reads its queries from, mounted read-only at /beir.
set -euo pipefail

CLUSTER=${CLUSTER:-strata}
WORKERS=${WORKERS:-3}
IMAGE=strata-server:dev
NS=strata
HERE=$(cd "$(dirname "$0")" && pwd)
K="kubectl --context kind-$CLUSTER -n $NS"

cmd=${1:-}
shift || true
case "$cmd" in
up)
    : "${STRATA_WORK:?set STRATA_WORK to the directory of the databases}"
    : "${STRATA_DATA:?set STRATA_DATA to the directory of the BEIR collections}"
    mkdir -p "$STRATA_WORK/results"
    cfg=$(mktemp)
    {
        echo "kind: Cluster"
        echo "apiVersion: kind.x-k8s.io/v1alpha4"
        echo "nodes:"
        echo "  - role: control-plane"
        for _ in $(seq "$WORKERS"); do
            echo "  - role: worker"
            echo "    extraMounts:"
            echo "      - {hostPath: \"$STRATA_WORK\", containerPath: /data}"
            echo "      - {hostPath: \"$STRATA_DATA\", containerPath: /beir, readOnly: true}"
        done
    } >"$cfg"
    kind create cluster --name "$CLUSTER" --config "$cfg"
    rm -f "$cfg"
    kubectl --context "kind-$CLUSTER" apply -f "$HERE/k8s/namespace.yaml"
    ;;
image)
    docker build -f "$HERE/Dockerfile" -t "$IMAGE" "$HERE/../.."
    kind load docker-image "$IMAGE" --name "$CLUSTER"
    ;;
single)
    $K apply -f "$HERE/k8s/single.yaml"
    $K rollout status deploy/strata-single --timeout=300s
    ;;
shards)
    n=${1:?shards N}
    sed -e "s/strata-shards-2/strata-shards-$n/g" -e "s/shards: \"2\"/shards: \"$n\"/g" \
        -e "s/replicas: 2/replicas: $n/" -e "s/value: \"2\"/value: \"$n\"/" "$HERE/k8s/shards.yaml" | $K apply -f -
    $K rollout status "statefulset/strata-shards-$n" --timeout=300s
    ;;
coordinator)
    n=${1:?coordinator N}
    urls=$(for i in $(seq 0 $((n - 1))); do printf 'http://strata-shards-%s-%s.strata-shards-%s:7701,' "$n" "$i" "$n"; done)
    sed -e "s|--shards=.*|--shards=${urls%,}|" "$HERE/k8s/coordinator.yaml" | $K apply -f -
    $K rollout status deploy/strata-coordinator --timeout=300s
    ;;
bench)
    name=${1:?bench NAME ARGS...}
    shift
    args=$(python3 -c 'import json, sys; print(json.dumps(sys.argv[1:]))' "$@")
    $K delete job "bench-$name" --ignore-not-found >/dev/null
    # BENCH_NODE pins the client to a node (so a test can freeze another).
    node=${BENCH_NODE:+nodeName: $BENCH_NODE}
    $K apply -f - <<EOF
apiVersion: batch/v1
kind: Job
metadata:
  name: bench-$name
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      $node
      containers:
        - name: bench
          image: $IMAGE
          imagePullPolicy: IfNotPresent
          command: ["strata-server-bench"]
          args: $args
          env:
            - {name: STRATA_DATA, value: /beir}
          volumeMounts:
            - {name: beir, mountPath: /beir, readOnly: true}
            - {name: data, mountPath: /data}
      volumes:
        - name: beir
          hostPath: {path: /beir, type: Directory}
        - name: data
          hostPath: {path: /data, type: Directory}
EOF
    until phase=$($K get pod -l "job-name=bench-$name" -o jsonpath='{.items[0].status.phase}' 2>/dev/null) && [ -n "$phase" ] && [ "$phase" != Pending ]; do sleep 1; done
    $K logs -f "job/bench-$name"
    $K wait --for=condition=complete "job/bench-$name" --timeout=60s >/dev/null
    ;;
crash)
    pod=${1:?crash POD}
    # (jsonpath prints no newline, so `read` would fail under set -e.)
    set -- $($K get pod "$pod" -o jsonpath='{.spec.nodeName} {.status.containerStatuses[0].containerID}')
    node=$1 id=$2
    # SIGKILL from the node, as a crash would: the kubelet restarts the
    # container in place (same pod, same address).
    docker exec "$node" sh -c "kill -9 \$(crictl inspect --output go-template --template '{{.info.pid}}' ${id#containerd://})"
    echo "$pod: killed on $node"
    ;;
delete)
    pod=${1:?delete POD}
    # Stops the pod gracefully (SIGTERM); its controller starts a new one.
    $K delete pod "$pod" --wait=false
    ;;
pause)
    pod=${1:?pause POD SECONDS}
    seconds=${2:?pause POD SECONDS}
    node=$($K get pod "$pod" -o jsonpath='{.spec.nodeName}')
    # Freezes the pod's whole node, as a hung machine or a partition would:
    # its connections stay open and nothing answers.
    docker pause "$node" >/dev/null
    echo "$node (with $pod) paused for $seconds s"
    sleep "$seconds"
    docker unpause "$node" >/dev/null
    echo "$node unpaused"
    ;;
delay)
    pod=${1:?delay POD MS}
    ms=${2:?delay POD MS}
    set -- $($K get pod "$pod" -o jsonpath='{.spec.nodeName} {.status.podIP}')
    node=$1 ip=$2
    # The pod's veth on its node: netem there delays every packet to the pod.
    dev=$(docker exec "$node" ip route get "$ip" | grep -o 'dev [^ ]*' | cut -d' ' -f2)
    if [ "$ms" = 0 ]; then
        docker exec "$node" tc qdisc del dev "$dev" root 2>/dev/null || true
    else
        docker exec "$node" tc qdisc replace dev "$dev" root netem delay "${ms}ms"
    fi
    echo "$pod ($ip on $node, $dev): +${ms} ms"
    ;;
down)
    kind delete cluster --name "$CLUSTER"
    ;;
*)
    sed -n '2,20p' "$0"
    exit 2
    ;;
esac
