#!/bin/sh
# Runs ClickHouse, VictoriaLogs and OpenSearch one at a time in Docker
# (each container stopped and removed before the next starts), on
# DATA/{bgl,hdfs,syn20m}.jsonl, after chloedb.sh has run (the shifts come
# from its output). Results go to ../mac-others.jsonl. Named volumes
# ls-ch, ls-vl, ls-os are removed at the end; the images are kept.
#   others.sh DATA [STORES]     STORES default: clickhouse victorialogs opensearch
set -e
data=$(cd "$1" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
names=${NAMES:-bgl hdfs syn20m}
wait_http() {  # URL CONTAINER
  until curl -sf "$1" >/dev/null 2>&1; do
    [ "$(docker inspect -f {{.State.Running}} "$2")" = true ] || { docker logs "$2" 2>&1 | tail -20; exit 1; }
    sleep 1
  done
}
for s in ${2:-clickhouse victorialogs opensearch}; do
  case $s in
  clickhouse)
    docker run -d --name ls-ch -p 8123:8123 -v ls-ch:/var/lib/clickhouse -v "$data:/data:ro" \
      -v "$here/ch_user_files.xml:/etc/clickhouse-server/config.d/user_files.xml:ro" \
      -e CLICKHOUSE_SKIP_USER_SETUP=1 -e CLICKHOUSE_DO_NOT_CHOWN=1 --ulimit nofile=262144:262144 clickhouse/clickhouse-server:25.8 >/dev/null
    wait_http http://127.0.0.1:8123/ping ls-ch
    python3 "$here/clickhouse.py" "$data" $names
    docker rm -f ls-ch >/dev/null; docker volume rm ls-ch >/dev/null ;;
  victorialogs)
    python3 "$here/shift.py" "$data" $names
    for n in $names; do   # a fresh store per file, so its bytes are that file's
      docker run -d --name ls-vl -p 9428:9428 -v ls-vl:/vl victoriametrics/victoria-logs:v1.53.0 \
        -storageDataPath=/vl -retentionPeriod=100y -futureRetention=100y >/dev/null
      wait_http http://127.0.0.1:9428/health ls-vl
      python3 "$here/victorialogs.py" "$data" $n
      docker rm -f ls-vl >/dev/null; docker volume rm ls-vl >/dev/null
    done ;;
  opensearch)
    python3 "$here/shift.py" "$data" $names
    docker run -d --name ls-os -p 9200:9200 -v ls-os:/usr/share/opensearch/data \
      -e discovery.type=single-node -e OPENSEARCH_JAVA_OPTS="-Xms4g -Xmx4g" \
      -e DISABLE_SECURITY_PLUGIN=true -e DISABLE_INSTALL_DEMO_CONFIG=true \
      --ulimit nofile=65536:65536 --ulimit memlock=-1:-1 opensearchproject/opensearch:3.3.0 >/dev/null
    wait_http 'http://127.0.0.1:9200/_cluster/health?wait_for_status=green&timeout=5s' ls-os
    python3 "$here/opensearch.py" "$data" $names
    docker rm -f ls-os >/dev/null; docker volume rm ls-os >/dev/null ;;
  esac
done
