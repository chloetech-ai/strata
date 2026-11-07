# Strata and four graph databases on the arXiv citation graph (Mac, Docker)

**One laptop, one Docker VM.** Measured on 2026-11-03 between 16:40 and 16:51
UTC with [`deploy/graph-compare/compare.sh`](../../deploy/graph-compare/compare.sh)
(data and exact answers from [`prep.py`](../../deploy/graph-compare/prep.py),
queries in [`run.py`](../../deploy/graph-compare/run.py), tables from
[`analyze.py`](../../deploy/graph-compare/analyze.py)). Every engine ran alone
in the same Docker VM with the same limits, loaded the same graph and answered
the same 200 queries of each kind from one client on the same Docker network.
The engines and the client share the VM's 4 CPUs, so the numbers compare the
engines with each other, not what any of them does on a server. Strata was
queried both over gRPC and over HTTP JSON. Two earlier passes that morning,
over HTTP JSON only and on a busier Mac, are kept in `pass2/` and `pass3/`
([tables-morning.md](tables-morning.md)). What went wrong on the way is in
[NOTES.md](NOTES.md).

| | |
|---|---|
| machine | MacBook Air (Mac14,2), Apple M2 (4 performance + 4 efficiency cores), 24 GB, macOS 26.0.1 (25A362) |
| Docker | Docker Desktop, engine 24.0.5, VM with 4 CPUs and 8.7 GiB, LinuxKit kernel 5.15.49 |
| data | ogbn-arxiv citations: 169,343 papers, 1,166,243 citations (no self-citations or repeats). Directed as cited; "both ways" and paths treat it as undirected, a pair of papers citing each other being two edges |
| budget | each engine `--cpus 3 --memory 6g` (no swap); the client `--cpus 1 --memory 2g` |
| client | Python 3.12 in a container on the engines' Docker network: grpcio 1.85.1 with stubs from `strata-server/proto/strata.proto` (gRPC `Query`: Strata), httpx 0.28.1 (HTTP JSON: Strata's loading and its HTTP runs, ArangoDB), neo4j 6.4.1 (Bolt: Neo4j, Memgraph), redis 8.1.0 (RESP: FalkorDB). One query at a time |
| queries | 200 seeds and 200 pairs (seed 7). Each kind: up to 20 queries untimed, then all 200 timed. Client time is send to whole answer; the engine's own time is what each reports (below) |
| runs | two passes (`pass4`, `pass5`), one engine at a time, every engine in each. The tables are pass 5; the last table puts both passes side by side |
| quiet | the kind cluster's containers stopped; the Mac's 1-minute load average was 2.2 to 4.6 before each engine |

| engine | image | graph as |
|---|---|---|
| Strata (EDGES field) | `strata-server:graph`, built from main f7096030 | `papers (id INT PRIMARY KEY, cites EDGES)`: a paper's citations are a field of its row |
| Strata (edge table) | the same | `papers (id)` and `links (id, src, dst) OPTIONS(edges = 'src, dst')`: a citation is a row |
| Neo4j | `neo4j:2026.10.1-community`, Graph Data Science 2026.10.1 | `(:Paper {id})-[:CITES]->(:Paper)`, unique `id`; GDS projection `g` of all of it, undirected |
| Memgraph | `memgraph/memgraph-mage:3.14.0` | the same, index on `id`, in-memory transactional storage |
| FalkorDB | `falkordb/falkordb:v4.22.3` | the same, index on `id` |
| ArangoDB | `arangodb/arangodb:3.12.4.3` | `papers` documents keyed by id, `cites` edge collection |

## Results

Client p50 / p95 in ms, pass 5; best in bold. "n/a": the engine has no such
query (below). Strata's edge table over HTTP JSON is in [tables.md](tables.md).

| query | Strata (field), gRPC | Strata (field), HTTP | Strata (edge table), gRPC | Neo4j | Memgraph | FalkorDB | ArangoDB |
|---|---:|---:|---:|---:|---:|---:|---:|
| 1 hop, both ways (9 nodes on average) | 0.35 / 0.40 | 0.46 / 0.54 | 0.37 / 0.40 | 1.00 / 1.22 | **0.18 / 0.22** | 0.20 / 0.22 | 0.36 / 0.59 |
| 2 hops, both ways (2,821 on average, up to 30,012) | 0.41 / 1.95 | 0.52 / **1.90** | 0.47 / 4.93 | 0.94 / 6.01 | 0.24 / 4.54 | **0.23** / 5.24 | 0.55 / 4.68 |
| 3 hops out (171 on average) | 0.38 / 0.75 | 0.48 / 0.85 | 0.40 / 0.98 | 0.78 / 1.28 | 0.19 / **0.44** | **0.18** / 0.72 | 0.41 / 1.12 |
| shortest path, at most 10 hops | **0.52 / 1.26** | 0.57 / 1.29 | 1.41 / 5.96 | 1.33 / 6.11 | 0.90 / 24.8 | n/a | 2.15 / 8.46 |
| personalized PageRank, top 20 | **1.42 / 1.91** (eps 1e-4) | 1.47 / 1.94 | 3.05 / 4.20 | 93.1 / 141 (GDS defaults) | n/a | n/a | n/a |
| the same, finer | 7.80 / 10.7 (eps 1e-5) | **7.70 / 10.2** | 17.0 / 24.0 | 429 / 460 (100 iterations) | n/a | n/a | n/a |
| the same, finest | 52.6 / 75.8 (eps 1e-6) | **50.9 / 73.0** | 77.2 / 108 | | | | |

Every engine answered every neighbour and path query exactly as the reference
(`prep.py`, NumPy and SciPy) did, over both of Strata's protocols. PageRank's
answers are scored as recall@20 against the exact top 20 (power iteration to
1e-12); they do not depend on the protocol:

| setting | Strata (field) | Strata (edge table) | Neo4j GDS |
|---|---:|---:|---:|
| coarse (eps 1e-4; GDS defaults: 20 iterations, tolerance 1e-7) | 0.807 | 0.809 | 0.988 |
| fine (eps 1e-5; GDS: 100 iterations, tolerance 1e-9) | 0.895 | 0.895 | 0.999 |
| finest (eps 1e-6) | 0.955 | 0.955 | |

Load and footprint (pass 5): the time to load the graph through each engine's
own client API until it answers, the container's memory after the queries
(`docker stats`) and the size of its data directory. Strata loads over HTTP in
both of its runs.

| engine | load (s) | memory | data on disk |
|---|---:|---:|---:|
| Strata (field), gRPC run | 1.27 | 160 MiB | 68 MiB |
| Strata (field), HTTP run | **1.06** | **159 MiB** | 67 MiB |
| Strata (edge table), gRPC run | 6.22 | 202 MiB | 239 MiB |
| Neo4j | 16.1 (+ 1.05 for the GDS projection) | 3.56 GiB (3 GiB of it the heap it was given) | 836 MiB |
| Memgraph | 6.96 | 801 MiB | 387 MiB (snapshot and WAL; the graph is in memory) |
| FalkorDB | 26.9 | 319 MiB | 0 (in memory; nothing saved during the run) |
| ArangoDB | 13.6 | 681 MiB | 537 MiB |

Full tables, including each engine's own time, are in [tables.md](tables.md).

Ranked by the sum of places over 16 tests (client p50 and p95 for six query
kinds, PageRank's recall at two settings, load and memory; a test a setup
cannot run counts as 6th), with Strata over gRPC: Strata (field) 31, Strata
(edge table) 46, Memgraph 60, Neo4j 67, FalkorDB 70, ArangoDB 80. Without the
six PageRank tests, Strata has 21 and Memgraph 24.

## What the numbers say

- **Neighbours: Memgraph and FalkorDB answer fastest at the median, Strata has
  the shortest tail on 2 hops.** Their medians are 0.18 to 0.24 ms against
  Strata's 0.35 to 0.41 ms over gRPC. Strata's own time is 0.10 to 0.15 ms,
  against Memgraph's 0.02 to 0.06 ms and FalkorDB's 0.05 to 0.09 ms; the rest
  is the client. On 2 hops both ways, which reach up to 30,012 papers,
  Strata's p95 is 1.90 to 1.95 ms against 4.5 to 6.0 ms for every other engine.
- **gRPC against HTTP JSON.** Over gRPC, Strata's neighbour medians are 0.10
  to 0.11 ms lower and its path median 0.06 ms lower; PageRank does not change
  beyond noise. On 1 hop this Python client spends 0.25 ms on a gRPC call and
  0.37 ms on an HTTP JSON one, against 0.13 ms on RESP (FalkorDB) and 0.17 ms
  on Bolt (Memgraph).
- **Shortest paths: Strata is fastest at the median and the tail.** It takes
  0.52 / 1.26 ms against Memgraph's 0.90 / 24.8 ms, Neo4j's 1.33 / 6.11 ms and
  ArangoDB's 2.15 / 8.46 ms. FalkorDB has no undirected shortest path: its
  `shortestPath()` is directed only, and `algo.SPpaths` with
  `relDirection: 'both'` timed out after 24.6 s on the first pair and ran
  for over 8 minutes on the second before it was stopped, so it was not run.
- **Personalized PageRank: only Strata and Neo4j have it. Strata is 55 to 66
  times faster setting for setting, but less exact.** Neo4j's GDS iterates
  over the whole projected graph for every query: 93 ms with its defaults
  (recall@20 0.988) and 429 ms at 100 iterations (0.999). Strata's forward
  push touches only the seed's neighbourhood: 1.42 ms at eps 1e-4 (recall
  0.807), 7.80 ms at 1e-5 (0.895), 53 ms at 1e-6 (0.955). Its finest setting
  is still almost twice as fast as GDS's defaults, but no setting tried
  reached GDS's 0.99. Memgraph's MAGE has only a global PageRank, plus
  NetworkX's on the directed graph with the seed marked by a node property.
  FalkorDB's `algo.pageRank` is global, and ArangoDB 3.12 removed Pregel.
- **Load and size: Strata's EDGES field is the smallest and quickest to
  load.** It takes 1.06 to 1.27 s and 67 MiB, because a paper's citations are
  one value of its row. The edge table form, one row per citation, takes
  6.22 s and 239 MiB, like Memgraph (6.96 s) and well under ArangoDB, Neo4j
  and FalkorDB (13.6 to 26.9 s).
- **Strata's edge table walks more slowly than its EDGES field.** Paths are
  2.7 times slower and PageRank 1.5 to 2.2 times slower. A hop over an edge
  table finds the key in the edge table's index, then resolves each
  neighbour's key to its row of `papers` through the primary-key index. The
  EDGES field reads a CSR slice whose targets carry the row's docID when it
  is in the same segment, with no key lookup. Key resolution is also where
  the A3 co-purchase feed's PageRank spends its time
  ([part-a-do](../../../strata/results/laptop-2026-11/part-a-do/README.md)),
  so this is likely the same cost.

## Pass to pass

Client p50 in ms, pass 5 / pass 4:

| query | Strata (field), HTTP | Strata (field), gRPC | Strata (edge table), HTTP | Strata (edge table), gRPC | Neo4j | Memgraph | FalkorDB | ArangoDB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 hop, both ways | 0.46 / 0.46 | 0.35 / 0.35 | 0.44 / 0.45 | 0.37 / 0.36 | 1.00 / 1.02 | 0.18 / 0.19 | 0.20 / 0.20 | 0.36 / 0.37 |
| 2 hops, both ways | 0.52 / 0.52 | 0.41 / 0.40 | 0.56 / 0.56 | 0.47 / 0.47 | 0.94 / 0.90 | 0.24 / 0.23 | 0.23 / 0.22 | 0.55 / 0.55 |
| 3 hops out | 0.48 / 0.50 | 0.38 / 0.40 | 0.48 / 0.50 | 0.40 / 0.40 | 0.78 / 0.75 | 0.19 / 0.20 | 0.18 / 0.19 | 0.41 / 0.40 |
| shortest path | 0.57 / 0.56 | 0.52 / 0.54 | 1.51 / 1.42 | 1.41 / 1.44 | 1.33 / 1.30 | 0.90 / 0.89 | | 2.15 / 2.23 |
| PageRank, coarse | 1.47 / 1.44 | 1.42 / 1.39 | 3.01 / 3.03 | 3.05 / 2.99 | 93.1 / 97.1 | | | |
| PageRank, fine | 7.70 / 7.52 | 7.80 / 7.82 | 17.3 / 17.1 | 17.0 / 17.1 | 429 / 446 | | | |
| PageRank, finest | 50.9 / 52.4 | 52.6 / 49.2 | 76.4 / 75.2 | 77.2 / 75.6 | | | | |
| load, total (s) | 1.06 / 1.05 | 1.27 / 1.03 | 6.71 / 6.92 | 6.22 / 6.67 | 16.1 / 16.1 | 6.96 / 6.85 | 26.9 / 27.0 | 13.6 / 13.5 |

Every median moves by at most 7% between the passes, and every load by at
most 7% except Strata's own 1-second one.

The morning's passes ([tables-morning.md](tables-morning.md)) ran on a busier
Mac (load 4.4 to 7.5, macOS's `mediaanalysisd` using about one core) and
queried Strata over HTTP JSON only. Every engine's medians were slower then,
from 3% (Memgraph on 1 hop) to 67% (ArangoDB on 2 hops), and Neo4j's coarse
PageRank took 143 ms against 93 ms (pass 3 against pass 5). Paths and PageRank
keep the same order. Among the neighbour queries the order shifts between
close values: ArangoDB, which gained the most, moved ahead of Strata over HTTP
JSON on the 1- and 3-hop medians.

## How each engine was asked

| query | Strata | Neo4j | Memgraph | FalkorDB | ArangoDB |
|---|---|---|---|---|---|
| hops | `SELECT COUNT(*) FROM papers WHERE REACHABLE(cites, :s, hops => k, dir => 'both', max_nodes => 1000000)` | `MATCH (s:Paper {id: $s})-[:CITES*1..k]-(n) WHERE n <> s RETURN count(DISTINCT n)` | `-[:CITES *BFS ..k]-` in the same pattern | as Neo4j | `FOR v IN 1..k ANY @s cites OPTIONS {order: 'bfs', uniqueVertices: 'global'} COLLECT WITH COUNT INTO c RETURN c` |
| path | `SELECT id FROM papers WHERE PATH(cites, :a, :b, dir => 'both', max_hops => 10, max_nodes => 1000000) ORDER BY hops()` | `MATCH (a ...), (b ...) MATCH p = shortestPath((a)-[:CITES*..10]-(b)) RETURN length(p)` | `MATCH (a ...), (b ...) WITH a, b MATCH p = (a)-[:CITES *BFS ..10]-(b)` | (none) | `FOR v IN ANY SHORTEST_PATH @a TO @b cites`; longer than 10 counts as none |
| PageRank | `SELECT id FROM papers ORDER BY ppr(cites, :s, eps => e) DESC LIMIT 20` (restart 0.15, both ways) | `gds.pageRank.stream('g', {sourceNodes: [s], dampingFactor: 0.85, ...})`, top 20 | (none) | (none) | (none) |

Strata gets the same SQL over both protocols: `POST /v1/sql` with JSON params,
or gRPC `strata.v1.Strata/Query` with the SQL and protobuf `Value` params,
the server started with `--grpc 0.0.0.0:7701`. The edge-table form names
`links` where the field form names `cites`. Loading: Strata gets
`POST /v1/import` of NDJSON in requests of 50,000 rows. Neo4j, Memgraph and
FalkorDB get `UNWIND` batches of 10,000 nodes, then 10,000 edges, matched by
the indexed `id`. ArangoDB gets `/_api/import` in batches of 50,000. None used
an offline bulk importer such as `neo4j-admin database import`, which would
load faster.

The engine's own time is Strata's `took_ms` (in both protocols' answers),
Neo4j's `result_available_after + result_consumed_after` (whole
milliseconds), Memgraph's parsing, planning and execution times, FalkorDB's
"internal execution time" and ArangoDB's `executionTime`.

## Not covered

One graph of 1.17M edges with no properties, one client, and only reads.
LDBC SNB's typed, property-rich graph, concurrent clients, updates during
traversal, and PageRank fused with vector search (the A3 feed) are left for a
follow-up.
