# Graph engine comparison

Strata against Neo4j (with Graph Data Science), Memgraph (MAGE), FalkorDB and
ArangoDB on one machine's Docker: neighbours within 1 to 3 hops, shortest
paths and personalized PageRank on the arXiv citation graph, every answer
checked against an exact one. Results from a run on a MacBook Air are in
[`results/graph-compare-mac`](../../results/graph-compare-mac/README.md).

```bash
# The graph, the queries and their exact answers (NumPy, SciPy).
python prep.py --arxiv arxiv.jsonl --out DATA
printf '{"id": %d}\n' $(seq 0 169342) > DATA/papers-ids.ndjson   # rows for the edge-table form

# Each engine alone, loaded and queried from a client container.
# Strata's -grpc setups query over gRPC: PYLIB_GRPC holds grpcio and the
# stubs from strata-server/proto/strata.proto, built for CLIENT_IMAGE.
DATA=DATA OUT=OUT CLIENT_IMAGE=python:3.12-slim PYLIB=DIR_WITH_httpx_neo4j_redis PYLIB_GRPC=DIR \
  ./compare.sh strata strata-grpc strata-edges strata-edges-grpc neo4j memgraph arangodb
SKIP=path DATA=DATA OUT=OUT CLIENT_IMAGE=... PYLIB=... ./compare.sh falkordb

python analyze.py OUT > OUT/tables.md
```

`arxiv.jsonl` is ogbn-arxiv, one paper a line with `id` and `cites`, as
`strata-graph-bench load` reads it. `compare.sh` needs the engines' images
already pulled (it runs with `--pull never`) and `strata-server:graph` built
from `deploy/Dockerfile`. FalkorDB runs without `path`: it has no undirected
shortest path that finishes on this graph (see the results' NOTES.md).
