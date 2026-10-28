# Strata as shards on a simulated cluster (Mac + kind)

Measured on 2026-10-27 with [`deploy/kind-test.sh`](../../deploy/kind-test.sh)
(see [`deploy/README.md`](../../deploy/README.md) for how to run it). **These
are numbers from a simulation on one laptop, not from a cluster of
machines**: every Kubernetes node is a container in the same Docker VM, so
all pods share the same 4 CPUs, and the network between nodes is a Linux
bridge inside that VM.

| | |
|---|---|
| machine | MacBook Air, Apple M2 (4 performance + 4 efficiency cores), 24 GB, macOS 26.0.1 |
| Docker | Docker Desktop, engine 24.0.5, VM with 4 CPUs and 8.7 GB, LinuxKit kernel 5.15.49 |
| Kubernetes | kind 0.33, Kubernetes 1.37.0: 1 control plane and 3 workers |
| image | `deploy/Dockerfile`: strata-server and strata-server-bench on Debian bookworm-slim, 157 MB, arm64 |
| data | BEIR Quora: 522,931 questions, 1,024-dimension embeddings, 8-bit vector codes, HNSW (m 16, ef_construction 200), one segment per database; split by primary key into 2, 3 and 4 shards with `strata-server-bench --build-only` |
| storage | the database files stay on the Mac and are mounted into every node through Docker Desktop's file sharing (`hostPath` `/data`) |
| pods | `strata-single` (the whole collection, the reference), `strata-shards-N-i` (StatefulSet, spread over the 3 workers), `strata-coordinator` (Deployment) |
| client | `strata-server-bench --single ... --coordinator ...` in a pod (a Job): 300 Quora queries, 9 query kinds, 6 s per kind at 1 and 16 clients; failure runs use `--watch` with 4 clients sending all kinds in turn |
| load | `uptime` load average of the Mac before each run: 3.9 to 6.1 (logged in the runs) |

Reports: `quora-<N>-shards.json` (timings and agreement), `quora-watch-*.json`
(per-second timelines), `quora-<label>.json` for the network delay runs.

## Correctness: the coordinator answers as one database

Up to 50 queries of each kind sent to the unsplit database and to the
coordinator. BM25 scores use statistics summed over the shards, so ranked
text kinds must match exactly; vector search is approximate (HNSW) on one
database and on each shard alike, so those kinds are scored against an
exact scan instead (recall@10).

| kind | same top 10, 2 shards | 3 shards | 4 shards | recall@10, one database | 2 shards | 3 shards | 4 shards |
|---|---|---|---|---|---|---|---|
| bm25 | 50 of 50 | 50 of 50 | 50 of 50 | exact | exact | exact | exact |
| bm25 + filter | 50 of 50 | 50 of 50 | 50 of 50 | exact | exact | exact | exact |
| vector | 46 of 50 | 43 of 50 | 43 of 50 | 0.990 | 0.992 | 0.992 | 0.996 |
| vector + filter | 39 of 50 | 41 of 50 | 41 of 50 | 0.984 | 0.992 | 0.994 | 0.998 |
| hybrid (RRF) | 42 of 50 | 40 of 50 | 40 of 50 | 0.972 | 0.968 | 0.980 | 0.978 |
| hybrid (weighted) | 50 of 50 | 49 of 50 | 49 of 50 | 0.998 | 0.998 | 1.000 | 1.000 |
| boosted (bm25 × decay) | 50 of 50 | 50 of 50 | 50 of 50 | exact | exact | exact | exact |
| facet | 50 of 50 | 50 of 50 | 50 of 50 | exact | exact | exact | exact |
| key lookup | 50 of 50 | 50 of 50 | 50 of 50 | exact | exact | exact | exact |

Every BM25, filtered, boosted, facet and key-lookup answer matched one
database exactly at every shard count. Vector and hybrid search through the
shards find as many true neighbours as one database does, mostly a few more
(each shard searches a smaller graph with the same `ef`; the one exception,
hybrid RRF at 2 shards, is 0.004 lower), so their top 10 differ from one
database's but not for the worse.

## Scatter-gather latency and throughput

One client, median latency in ms:

| kind | one database | 2 shards | 3 shards | 4 shards |
|---|---|---|---|---|
| bm25 | 0.27 | 0.66 | 0.74 | 0.82 |
| bm25 + filter | 0.38 | 0.86 | 0.98 | 1.06 |
| vector | 0.39 | 0.62 | 0.68 | 0.88 |
| vector + filter | 0.54 | 0.78 | 0.87 | 1.27 |
| hybrid (RRF) | 0.74 | 1.85 | 2.06 | 2.92 |
| hybrid (weighted) | 0.78 | 1.76 | 2.04 | 2.94 |
| boosted (bm25 × decay) | 0.41 | 1.17 | 1.35 | 1.71 |
| facet | 0.18 | 0.40 | 0.44 | 0.50 |
| key lookup | 0.16 | 0.32 | 0.35 | 0.37 |

16 clients, queries per second:

| kind | one database | 2 shards | 3 shards | 4 shards |
|---|---|---|---|---|
| bm25 | 16,853 | 4,441 | 3,206 | 2,195 |
| bm25 + filter | 9,761 | 2,971 | 2,332 | 1,577 |
| vector | 10,855 | 4,196 | 3,203 | 2,050 |
| vector + filter | 6,971 | 2,935 | 2,133 | 1,318 |
| hybrid (RRF) | 4,793 | 1,190 | 856 | 540 |
| hybrid (weighted) | 4,452 | 1,211 | 764 | 545 |
| boosted (bm25 × decay) | 9,221 | 2,058 | 1,316 | 1,041 |
| facet | 39,473 | 9,325 | 5,319 | 4,425 |
| key lookup | 53,342 | 13,194 | 7,851 | 6,968 |

With one client, the coordinator adds 0.15 to 0.5 ms to most kinds at 2
shards (one gRPC round to every shard, then a merge; BM25 kinds take two
rounds, the first for the summed term statistics), and a little more for
each shard added. Hybrid (about +1.1 ms) and boosted (+0.8 ms) kinds pay
the most.

Throughput falls as shards are added, and that is the simulation, not
Strata: every shard and the coordinator share the same 4 CPUs, so each
query does more total work on the same hardware. On separate machines each
shard brings its own CPUs and memory; what carries over from these numbers
is the coordinator's fixed cost per query and how it grows with the number
of shards.

## Failures, with one copy of each shard

The coordinator over 2 shards, 4 clients sending every kind in turn
(about 2,000 queries a second), with one action at t = 10 s. A 40 s run
with no action had no errors.

| what happened to shard 1 | errors | back to full speed |
|---|---|---|
| crash: its server killed with SIGKILL; the kubelet restarts the container in place | 2,424 over about 2 s (fast "transport error", then "dns error" while it is not ready) | after about 10 s |
| graceful delete: `kubectl delete pod`; the StatefulSet starts it again | 225 over about 3 s | after about 10 s |
| its node frozen for 20 s (`docker pause`): connections stay open and nothing answers | only 20, but no answers at all for 19 s: each client waits the full 5 s query deadline and gets 504 | within 1 s of the node resuming |

- **One missing shard fails every query.** With one copy of each shard and
  no partial answers, a crashed or restarting shard is a full outage for
  the seconds it is gone, and a hung one is worse: every query waits out
  the deadline. Replicas fix this (next section).
- **A restarted shard warms up.** It answered at full speed only after about
  10 s here. Natively on the Mac, a freshly opened shard reaches full speed
  in about 2 s (190, then 2,044, then 12,000+ queries a second); in the
  cluster it most likely reads its file again through Docker's file
  sharing, which behaves like network storage.
- **Fixed on the way:** strata-server ignored SIGTERM as a container's first
  process (no handler, and the kernel drops such signals to PID 1), so
  `docker stop` took 11 s and killed it, and a pod deletion would wait out
  the 30 s grace period. It now shuts down gracefully at once.

## Failures, with two replicas of each shard

The coordinator now takes replicas of a shard (`--shards 'a0|a1,b0|b1'`):
a read asks one replica of each shard, in turn, and the next one when it
fails or has not answered after 100 ms (`--hedge-ms`); a replica that
failed, or was outrun, is asked last for 5 s. The manifests run each shard
as a StatefulSet of 2 pods on different nodes (`kind-test.sh shards 2 2`),
each pod with its own copy of the shard's file. The same test as above,
from a client on the coordinator's node, with each action on a replica of
shard 0 on another node:

| what happened | errors | slowest second after it | slowest query | coordinator |
|---|---|---|---|---|
| nothing (40 s) | 0 | | | |
| crash: one replica's server killed with SIGKILL | **0** (was 2,424) | 1,937 answers | 104 ms | 4 retries, 20 hedges |
| graceful delete of one replica's pod | **0** (was 225) | 1,680 answers | 107 ms | 22 hedges |
| a node frozen for 20 s (holding a replica of each shard) | **0** (was no answers for 19 s) | 1,755 answers | 107 ms | 28 hedges |

No query failed and none waited more than the 100 ms hedge plus an answer:
a dead replica fails fast and the next one is asked at once (a retry), and
one that does not answer, frozen or not yet reconnected, is outrun by the
next one after 100 ms (a hedge) and then avoided. Writes go to every replica
of their shard; a replica that is down when a write comes misses it and
must be rebuilt (there is no catch-up), so this is for read availability.
A coordinator whose shards are not answering yet now waits for them when it
starts, instead of exiting.

### Every replica of a shard gone: partial answers

With `--partial` (or `"partial": true` in a query) and `--shard-timeout-ms`,
a shard with no replica answering is left out, and the answer says so:
`"partial": {"missing_shards": [...]}`. Shard 1 scaled to 0 replicas for
20 s, then back to 2:

| | answers a second while shard 1 was gone | median latency | errors |
|---|---|---|---|
| first try: each query still asked the gone shard | about 170, all partial | 22 ms | 0 |
| now: a shard no replica answered is left out of the next second's queries without asking it | about 3,450, all partial | 1.0 ms | 0 |

The first try answered every query, but slowly: each one asked both of
the gone shard's replicas and waited for them to fail (their pods, and so
their names, no longer existed). The coordinator now skips such a shard for a
second at a time, so partial answers come at full speed (faster than whole
ones, with one shard fewer to ask). Within 3 s of shard 1 coming back,
answers were complete again; its two fresh replicas were then slow for
about 13 s (p99 up to 250 ms) while they read their files.

Without `--partial`, the same queries fail, as before, and a request a
shard rejects as wrong (400) fails the query either way.

## Adding shards

Shards are split by a hash of the primary key over the number of shards,
so going from N to N + 1 shards means splitting the data again (offline,
with `--build-only`) and moving the coordinator to the new set. Done
blue/green: the new set starts beside the old one, is warmed with queries
of its own, and the coordinator's Deployment is updated to point at it,
under the same 4-client load as above.

| move | errors | gap |
|---|---|---|
| 2 → 3 shards | 1,014 | no answers for about 9 s: requests went to the old coordinator's address after its pod was gone, and hung until the client's 10 s timeout |
| 3 → 4 shards, with a 5 s `preStop` delay on the coordinator | 1 in about 57,000 | none |

The `preStop` delay is now in `deploy/k8s/coordinator.yaml`: the old pod keeps
answering while the Service stops sending it new connections. Warming the new
set matters: the 4-shard set, read cold, served about 160 queries a second per
shard for the first 20 s and full speed (2,500 to 3,500) only after about 27 s.

## Network delay

The coordinator over 4 shards, one client, 100 queries of each kind for
4 s. `kind-test.sh delay` adds a fixed delay (Linux `netem`) to every packet
sent to a shard pod, on its node, which is what the coordinator's requests
to that shard cross; the client and the coordinator are not delayed.
Median latency in ms (p99 in brackets):

| kind | no delay | +1 ms to each shard | +5 ms to each shard | +20 ms to shard 0 only |
|---|---|---|---|---|
| bm25 | 0.99 (2.2) | 4.48 (8.6) | 14.72 (21.0) | 46.69 (50.7) |
| bm25 + filter | 1.12 (2.1) | 4.95 (9.6) | 15.66 (21.6) | 46.26 (51.6) |
| vector | 0.84 (1.6) | 2.18 (3.6) | 7.85 (12.5) | 24.13 (38.5) |
| vector + filter | 1.17 (2.2) | 2.69 (4.2) | 8.40 (12.7) | 23.90 (28.8) |
| hybrid (RRF) | 2.60 (4.0) | 6.05 (8.7) | 16.79 (23.9) | 48.54 (53.3) |
| hybrid (weighted) | 2.61 (5.3) | 6.51 (9.8) | 16.78 (25.2) | 47.84 (53.9) |
| boosted (bm25 × decay) | 1.51 (3.2) | 5.08 (9.4) | 15.51 (22.6) | 47.27 (53.9) |
| facet | 0.48 (1.1) | 2.02 (4.1) | 7.94 (12.9) | 23.79 (27.3) |
| key lookup | 0.38 (1.0) | 2.34 (5.6) | 7.30 (12.0) | 23.43 (26.9) |

- Each round to the shards costs a little more than the added delay (1.1
  to 1.8 times it here). Vector, facet and key-lookup queries take one
  round; BM25, hybrid and boosted queries take two (the summed term
  statistics first, then the query), so they pay it twice: +5 ms on every
  shard turned 1 ms into 15 ms.
- The slowest shard sets the latency of every query: one shard of four
  20 ms away made every vector query 23 ms slower and every BM25 query
  46 ms slower, while the other three answered at once.
- Between machines in one data centre (round trips of roughly 0.1 to
  0.5 ms) this should add well under a millisecond per round; across zones
  or regions, the second round of BM25 queries is worth avoiding, for
  instance by caching the summed statistics, which change only with
  writes.

## What to do next

- Catch-up for replicas: a replica that misses writes while down is not
  brought up to date; a write log the replicas replay, or rebuilding a
  replica from another, would make replicas safe for collections that take
  writes.
- Warm a replica before sending it traffic: a restarted replica here is
  slow for 10 s or more while it reads its file again, and only hedging
  hides that.
- Route key lookups: `WHERE id = :k` goes to every shard today, though the
  key names exactly one (as writes already use).
