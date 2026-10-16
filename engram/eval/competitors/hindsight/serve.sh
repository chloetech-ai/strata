#!/bin/bash
# Hindsight 1.0.0 servers for scripts/competitor_contexts.py, Haiku through Hindsight's claude-code provider,
# observations off as AMB runs it, Postgres (pg0) on 5433.
#   serve.sh load    loading: database postgres, hash_embed.py on 8080, API on 8888
#   serve.sh final   answering: database hsfinal, tei_shim.py with bge-small-en-v1.5 (8082) and
#                    ms-marco-MiniLM-L-6-v2 (8081) from $MODELS, API on 8890; then migrate.py moves the banks
HERE=$(cd "$(dirname "$0")" && pwd)
PY=${HS_PY:-/home/claude/ext/hs-venv/bin/python}
MODELS=${MODELS:-/home/claude/data/models/hf}
RUN=${HS_RUN:-/home/claude/bench/hs}
mkdir -p $RUN/home && cd $RUN
api() {  # db port worker_port embed_url extra-env...
  local db=$1 port=$2 wport=$3 embed=$4; shift 4
  setsid nohup env -i PATH="$PATH" HOME=$RUN/home ANTHROPIC_BASE_URL="$ANTHROPIC_BASE_URL" \
    HINDSIGHT_API_DATABASE_URL=postgresql://postgres:postgres@127.0.0.1:5433/$db \
    HINDSIGHT_API_LLM_PROVIDER=claude-code HINDSIGHT_API_LLM_MODEL=haiku HINDSIGHT_API_LLM_MAX_CONCURRENT=${LLM_CONC:-8} \
    HINDSIGHT_API_EMBEDDINGS_PROVIDER=tei HINDSIGHT_API_EMBEDDINGS_TEI_URL=$embed \
    HINDSIGHT_API_ENABLE_OBSERVATIONS=false HINDSIGHT_API_HOST=127.0.0.1 HINDSIGHT_API_PORT=$port HINDSIGHT_API_WORKER_HTTP_PORT=$wport \
    "$@" /home/claude/ext/hs-venv/bin/hindsight-api > server_$db.log 2>&1 < /dev/null &
  for i in $(seq 1 90); do curl -sf 127.0.0.1:$port/health >/dev/null && break; sleep 2; done; curl -s 127.0.0.1:$port/health; echo
}
case $1 in
  load)
    curl -sf 127.0.0.1:8080/info >/dev/null || setsid nohup $PY -I $HERE/hash_embed.py 8080 > hash_embed.log 2>&1 < /dev/null &
    sleep 2
    # the cross-encoder is not used while loading
    api postgres 8888 8889 http://127.0.0.1:8080 HINDSIGHT_API_RERANKER_PROVIDER=rrf ;;
  final)
    curl -sf 127.0.0.1:8082/info >/dev/null || setsid nohup $PY -I $HERE/tei_shim.py embed --model $MODELS/bge-small-en-v1.5 --model-id BAAI/bge-small-en-v1.5 --port 8082 > tei_embed.log 2>&1 < /dev/null &
    curl -sf 127.0.0.1:8081/info >/dev/null || setsid nohup $PY -I $HERE/tei_shim.py rerank --model $MODELS/ms-marco-MiniLM-L-6-v2 --model-id cross-encoder/ms-marco-MiniLM-L-6-v2 --port 8081 > tei_rerank.log 2>&1 < /dev/null &
    for p in 8082 8081; do for i in $(seq 1 60); do curl -sf 127.0.0.1:$p/info >/dev/null && break; sleep 1; done; done
    # the shim scores two requests at a time: queue the rest in Hindsight rather than let them time out
    api hsfinal 8890 8891 http://127.0.0.1:8082 HINDSIGHT_API_RERANKER_PROVIDER=tei HINDSIGHT_API_RERANKER_TEI_URL=http://127.0.0.1:8081 \
      HINDSIGHT_API_RERANKER_TEI_MAX_CONCURRENT=2 HINDSIGHT_API_RERANKER_TEI_HTTP_TIMEOUT=300 ;;
  *) echo "usage: serve.sh load|final" >&2; exit 2 ;;
esac
