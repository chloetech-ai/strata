# Notes on the graph comparison run (2026-11-03)

What happened on the way to [README.md](README.md)'s numbers, in order.

## Setup

- Images were pulled on 2026-11-03 (digests in `passN/image-*.txt`). Strata's
  image was built from main f7096030 with `strata-server/deploy/Dockerfile`,
  its `rust:1-bookworm` base pinned to the digest already in the build cache
  (`sha256:9181b56c...`), image id `6777de583f1d`. Neo4j fetched its Graph
  Data Science plugin (2026.10.1, 69.8 MB) on first start through
  `NEO4J_PLUGINS`, into a volume kept between runs.
- Engine settings beyond the limits: Neo4j heap 3 GiB (initial and maximum),
  page cache 1 GiB, no auth; Memgraph `--memory-limit=5000`; ArangoDB told
  its memory and cores (`ARANGODB_OVERRIDE_DETECTED_TOTAL_MEMORY=6g`,
  `..._NUMBER_OF_CORES=3`), no auth; FalkorDB and Strata as they come
  (Strata with a random token).
- The kind cluster's four node containers (up since 2026-11-02 19:03 UTC)
  were stopped at 07:15 UTC for the runs.
- macOS's `mediaanalysisd` used about one core of the Mac throughout. It was
  not paused.
- The client is `doable/base-python:3.12-slim` (Python 3.12.13, already on the
  Mac) with pure-Python copies of httpx 0.28.1, neo4j 6.4.1 and redis 8.1.0.

## The client moved into Docker

The first smoke run timed Strata from macOS through Docker Desktop's port
forwarding: 1 hop both ways took 1.385 ms at p50 on the client against
0.098 ms in the engine. From a container on the engines' network the same
query took 0.487 ms, so every number reported comes from there.

## Query forms changed after smoke runs

- **Memgraph's shortest path.** With `b` matched inside the path pattern
  (`MATCH p = (a {id})-[*BFS ..10]-(b {id})`), Memgraph plans a one-sided
  `BFSExpand` and filters `b` at the end: 251 ms at p50 on 40 pairs. Matching
  both ends first (`MATCH (a ...), (b ...) WITH a, b MATCH p = (a)-[*BFS
  ..10]-(b)`) plans `STShortestPath`: 1.19 ms, same answers. The reported runs
  use the second form.
- **FalkorDB's shortest path.** `shortestPath()` is accepted only in `WITH`
  or `RETURN` ("FalkorDB currently only supports shortestPaths in WITH or
  RETURN clauses"), and there only directed ("FalkorDB does not currently
  support undirected shortestPath traversals"). `algo.SPpaths` takes
  `relDirection: 'both'`, but in a probe (3 CPUs, the full graph):
  - The first pair (path of 3) answered "Query timed out" after 24.6 s with
    `maxLen` 4, 6 and 10 alike.
  - The second pair (path of 5) found no path with `maxLen` 4 in 0.1 s, which
    is right. With `maxLen` 6 it ran 518.9 s until the probe was stopped.

  FalkorDB's path query was therefore left out (`SKIP=path`).
- **ArangoDB's shortest path** has no depth limit. The graph is one
  component, and the 4 pairs farther apart than 10 hops are searched to the
  end, then counted as no path.

## Personalized PageRank where it exists

- Neo4j: `gds.pageRank.stream` with `sourceNodes`, over a projection of the
  whole graph made once after loading (1.1 s).
- Memgraph MAGE 3.14.0 lists `pagerank.get` and `igraphalg.pagerank`, both
  global, and `nxalg.pagerank`. That last one runs NetworkX's PageRank over a
  directed view of the graph, with the personalization given as the name of a
  node property. It needs a write per query and answers a different
  (directed) question, so it was not run.
- FalkorDB's `algo.pageRank` is global. ArangoDB 3.12 no longer has Pregel.

## The shakedown pass

Pass 1 (07:53 to 07:59 UTC) ran every engine once. It is not reported, for
two reasons: the engines without PageRank had their PageRank recorded as 200
failed queries instead of "n/a", and Strata's eps 1e-6 setting was added
after it. Its numbers are not used.

## Neo4j's GDS progress log

In both passes Neo4j first ran with GDS's default progress logging. Each
run wrote 1,815,952 lines (about 200 MB) to the container's log, about 4,000
lines per PageRank query on average. Its PageRank p50 was then 182 / 199 ms (coarse) and
622 / 686 ms (fine) in passes 2 / 3. Neo4j was then run again in both passes
with `logProgress: false`: 109 / 143 ms and 539 / 557 ms. The re-runs are the
ones reported. The logged runs' reports and the last 30 lines of their logs
are in `passN/neo4j-progress-logs/`. Even with `logProgress: false`, GDS
writes a start and finish line per iteration: 72,874 lines (7.6 MB) a run,
not committed (`logs-neo4j.txt` is the only container log left out).

## The afternoon passes, with Strata over gRPC

Roberto asked why Strata was queried over HTTP JSON when the other engines
used their binary protocols, and asked for a re-run over gRPC.

- The client image got grpcio 1.85.1, grpcio-tools and protobuf 7.36.2 from
  PyPI (installed with pip inside `doable/base-python:3.12-slim`), and stubs
  generated from `strata-server/proto/strata.proto`. The new setups
  `strata-grpc` and `strata-edges-grpc` start the server with
  `--grpc 0.0.0.0:7701`, load over HTTP as before, and send every query to
  gRPC `Query` with the same SQL and protobuf `Value` params.
- The kind cluster's containers, stopped at 07:15 UTC, had started again at
  08:33:21 UTC, three minutes after the morning's last run. `docker events`
  shows the start but not what caused it; nothing on the cluster was running
  but Redpanda. They were stopped again at 16:40 UTC.
- Passes 4 and 5 (16:40 to 16:51 UTC) ran every engine again, Strata in all
  four setups, with GDS progress logging off. The Mac was quieter than in the
  morning (load 2.2 to 4.6, `mediaanalysisd` idle), and every engine's
  medians were faster than in passes 2 and 3. The README quotes pass 5.
- `logs-neo4j.txt` is again left out of both passes (7.6 MB each).

## Files

- `pass4/`, `pass5/` (afternoon, reported) and `pass2/`, `pass3/` (morning):
  `run-<engine>.json` (load, every query kind's client and engine
  statistics, wrong answers if any), `mem-*.json` (`docker stats` after the
  queries), `disk-*.txt` (`du -sk` of the data directory), `image-*.txt`,
  `logs-*.txt` (container logs under 1 MB), `run.log`.
- `queries.json`: the seeds and pairs. `reference.json.gz`: the exact
  answers (`prep.py`).
- `tables.md`: `analyze.py pass5 pass4`. `tables-morning.md`:
  `analyze.py pass3 pass2`.
