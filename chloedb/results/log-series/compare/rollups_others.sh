#!/bin/sh
# ClickHouse, then TimescaleDB, one at a time in Docker, after
# rollups_chloedb.sh and rollups_prep.py. Each container and its volume
# are removed when its run ends; the images are kept.
#   rollups_others.sh DATA [STORES]     STORES default: clickhouse timescaledb
set -e
data=$(cd "$1" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
alive() { [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null)" = true ] || { docker logs "$1" 2>&1 | tail -20; exit 1; }; }
for s in ${2:-clickhouse timescaledb}; do
  case $s in
  clickhouse)
    docker run -d --name ls-ch -p 8123:8123 -v ls-ch:/var/lib/clickhouse -v "$data:/data:ro" \
      -v "$here/ch_user_files.xml:/etc/clickhouse-server/config.d/user_files.xml:ro" \
      -e CLICKHOUSE_SKIP_USER_SETUP=1 -e CLICKHOUSE_DO_NOT_CHOWN=1 --ulimit nofile=262144:262144 clickhouse/clickhouse-server:25.8 >/dev/null
    until curl -sf http://127.0.0.1:8123/ping >/dev/null 2>&1; do alive ls-ch; sleep 1; done
    python3 "$here/rollups_clickhouse.py" "$data" || { docker rm -f ls-ch >/dev/null; docker volume rm ls-ch >/dev/null; exit 1; }
    docker rm -f ls-ch >/dev/null; docker volume rm ls-ch >/dev/null ;;
  timescaledb)
    docker run -d --name ls-ts -p 5433:5432 -v ls-ts:/var/lib/postgresql/data \
      -e POSTGRES_HOST_AUTH_METHOD=trust timescale/timescaledb:latest-pg16 >/dev/null
    # The image restarts Postgres once after its first setup: wait for a
    # query to work twice, 3 s apart.
    until psql -X -h 127.0.0.1 -p 5433 -U postgres -c 'SELECT 1' >/dev/null 2>&1 && sleep 3 && psql -X -h 127.0.0.1 -p 5433 -U postgres -c 'SELECT 1' >/dev/null 2>&1; do alive ls-ts; sleep 1; done
    python3 "$here/rollups_timescale.py" "$data" || { docker rm -f ls-ts >/dev/null; docker volume rm ls-ts >/dev/null; exit 1; }
    docker rm -f ls-ts >/dev/null; docker volume rm ls-ts >/dev/null ;;
  esac
done
