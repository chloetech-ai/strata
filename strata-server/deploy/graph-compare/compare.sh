#!/bin/bash
# Graph engine comparison on one machine's Docker: each engine alone, with
# the same CPU and memory limits, loaded and queried by run.py from a client
# container on the same Docker network (so that the host's port forwarding,
# about a millisecond a request on Docker Desktop, is not in the times).
#
#   compare.sh ENGINE...      strata strata-edges strata-grpc strata-edges-grpc
#                             neo4j memgraph falkordb arangodb
#
# Environment: DATA (prep.py's output), OUT (reports), CLIENT_IMAGE (any
# image with python3), PYLIB (a directory holding httpx, neo4j and redis for
# it), PYLIB_GRPC (for strata-grpc and strata-edges-grpc: grpcio and the
# stubs from strata-server/proto/strata.proto, built for that image), CPUS
# (engine, 3), MEM (engine, 6g), LOAD_GATE (wait for the host's 1-minute
# load average to drop below it, 5), SKIP (query kinds to leave out).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
: "${DATA:?prep.py output dir}" "${OUT:?reports dir}" "${CLIENT_IMAGE:?image with python3}" "${PYLIB:?client packages}"
CPUS=${CPUS:-3}
MEM=${MEM:-6g}
LOAD_GATE=${LOAD_GATE:-5}
STRATA_IMAGE=${STRATA_IMAGE:-strata-server:graph}
NEO4J_IMAGE=${NEO4J_IMAGE:-neo4j:2026.10.1-community}
MEMGRAPH_IMAGE=${MEMGRAPH_IMAGE:-memgraph/memgraph-mage:3.14.0}
FALKOR_IMAGE=${FALKOR_IMAGE:-falkordb/falkordb:v4.22.3}
ARANGO_IMAGE=${ARANGO_IMAGE:-arangodb/arangodb:3.12.4.3}
NET=gc-net
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
docker network inspect $NET >/dev/null 2>&1 || docker network create $NET >/dev/null

log() { echo "$(date -u +%FT%TZ) $*" | tee -a "$OUT/run.log" >&2; }

quiet() {
    local t0=$SECONDS l
    while :; do
        l=$(sysctl -n vm.loadavg | awk '{print $2}')
        if awk -v l="$l" -v g="$LOAD_GATE" 'BEGIN{exit !(l < g)}'; then log "load $l"; return; fi
        if (( SECONDS - t0 > 900 )); then log "load still $l after 15 min, going on"; return; fi
        sleep 15
    done
}

run_one() {
    local engine=$1 name=gc-$1 port image data_dir token="" args=()
    docker rm -f "$name" >/dev/null 2>&1 || true
    docker volume rm "$name-data" >/dev/null 2>&1 || true
    local limits=(--name "$name" --network $NET --cpus "$CPUS" --memory "$MEM" --memory-swap "$MEM" --pull never)
    case $engine in
        strata | strata-edges | strata-grpc | strata-edges-grpc)
            port=7700 image=$STRATA_IMAGE data_dir=/data
            token=$(openssl rand -hex 16)
            local grpc=off
            case $engine in *-grpc) grpc=0.0.0.0:7701 ;; esac
            if [ "$engine" = strata ] || [ "$engine" = strata-grpc ]; then
                schema='{"table":"papers","key":"id","fields":[{"name":"id","type":"int"},{"name":"cites","type":"edges"}]}'
            else
                schema='{"table":"papers","key":"id","fields":[{"name":"id","type":"int"}]}'
            fi
            docker run -d "${limits[@]}" -v "$name-data:/data" -e STRATA_TOKEN="$token" --entrypoint sh "$image" \
                -c "echo '$schema' > /data/schema.json && exec strata-server --db /data/graph.strata --schema /data/schema.json --listen 0.0.0.0:7700 --grpc $grpc --allow-host $name" >/dev/null
            args=(--token "$token")
            ;;
        neo4j)
            port=7687 image=$NEO4J_IMAGE data_dir=/data
            docker run -d "${limits[@]}" -v "$name-data:/data" -v gc-neo4j-plugins:/plugins \
                -e NEO4J_AUTH=none -e NEO4J_PLUGINS='["graph-data-science"]' \
                -e NEO4J_server_memory_heap_initial__size=3g -e NEO4J_server_memory_heap_max__size=3g \
                -e NEO4J_server_memory_pagecache_size=1g "$image" >/dev/null
            ;;
        memgraph)
            port=7687 image=$MEMGRAPH_IMAGE data_dir=/var/lib/memgraph
            docker run -d "${limits[@]}" -v "$name-data:/var/lib/memgraph" "$image" --memory-limit=5000 --telemetry-enabled=false >/dev/null
            ;;
        falkordb)
            port=6379 image=$FALKOR_IMAGE data_dir=/var/lib/falkordb/data
            docker run -d "${limits[@]}" -v "$name-data:/var/lib/falkordb/data" "$image" >/dev/null
            ;;
        arangodb)
            port=8529 image=$ARANGO_IMAGE data_dir=/var/lib/arangodb3
            docker run -d "${limits[@]}" -v "$name-data:/var/lib/arangodb3" -e ARANGO_NO_AUTH=1 \
                -e ARANGODB_OVERRIDE_DETECTED_TOTAL_MEMORY="$MEM" -e ARANGODB_OVERRIDE_DETECTED_NUMBER_OF_CORES="$CPUS" "$image" >/dev/null
            ;;
        *) echo "unknown engine $engine" >&2; return 2 ;;
    esac
    log "$engine: started $image"
    docker image inspect "$image" --format '{{.Id}} {{.RepoDigests}}' >"$OUT/image-$engine.txt"
    local rc=0
    docker run --rm --name gc-client --network $NET --cpus 1 --memory 2g --pull never \
        -v "$HERE:/app:ro" -v "$DATA:/in:ro" -v "$OUT:/out" -v "$PYLIB:/pylib:ro" ${PYLIB_GRPC:+-v "$PYLIB_GRPC:/pylib-grpc:ro"} -e PYTHONPATH=/pylib${PYLIB_GRPC:+:/pylib-grpc} \
        --entrypoint python3 "$CLIENT_IMAGE" /app/run.py "$engine" --host "$name" --port "$port" ${args[@]+"${args[@]}"} \
        --data /in --out "/out/run-$engine.json" ${SKIP:+--skip "$SKIP"} 2>&1 | tee -a "$OUT/run.log" || rc=$?
    docker stats --no-stream --format '{"mem": "{{.MemUsage}}", "cpu": "{{.CPUPerc}}"}' "$name" >"$OUT/mem-$engine.json" || true
    docker exec "$name" du -sk "$data_dir" >"$OUT/disk-$engine.txt" 2>&1 || true
    docker logs "$name" >"$OUT/logs-$engine.txt" 2>&1 || true
    docker rm -f "$name" >/dev/null
    docker volume rm "$name-data" >/dev/null 2>&1 || true
    log "$engine: done (rc $rc)"
    return $rc
}

for e in "$@"; do
    quiet
    run_one "$e" || log "$e FAILED"
done
