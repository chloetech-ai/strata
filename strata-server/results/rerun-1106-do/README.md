# Strata on main, 6 November 2026, at 1M and 10M

The engine comparison run again for the engines whose code or settings
changed since their last run, on DigitalOcean (sfo2) with `fanout.sh`:

| engine | image or version | why it ran again |
|---|---|---|
| Strata | `ghcr.io/chloetech-ai/strata-server:b022e39d6f73295afb2e4bbde8c9d99da262059b` (main on 6 November) | everything merged after 18fb4bc, the image of [5 November's run](../bigger-items-do/README.md): #263 (filtered vector search re-ranks at full depth), #278 (a vector graph per value of a column; the harness now builds `emb` partitioned by `rating`, 7d081da), #284 (MEDIAN and percentiles), #285 (the filter cache keeps a table's own keys out), #287 (a coordinator in each shard's pod, the harness's default since 54c2892) |
| Manticore, 1M only | 29.9.0 | #262 turned its query cache off (`qcache_max_bytes = 0`); it last ran with it on, on 4 November. At 10M it did not finish loading in 8 hours, which the cache cannot change |

Not run again, because nothing of theirs changed since their newest run:
Elasticsearch and OpenSearch (5 November, result caches off since #262,
[`bigger-items-do`](../bigger-items-do/README.md)), and the other engines
(4 November at 1M, [`compare-msmarco-same-nodes-do`](../compare-msmarco-same-nodes-do/README.md);
3 November at 10M, [`compare-msmarco-10m-do`](../compare-msmarco-10m-do/README.md)).

Every engine had the budget of the earlier runs: 6.8 CPUs and 52 GiB, data
and queries checked against `data/SHA256SUMS` (the same files as on 4 and 5
November). Strata's budget is now two pods of 3.4 CPUs and 26 GiB, each a
shard and a coordinator (`STRATA_COORD=shards`, #287); until 5 November it
was two shards of 2.9 CPUs and a 1-CPU coordinator on the client's node
(`STRATA_COORD=pod`).

## The node check

DigitalOcean hands out several Xeon models, and the same image moves on
other nodes (up to 39% on BM25 at 16 clients, [the BM25 study](../bm25-do-1m)).
So each job first ran the image of 5 November (18fb4bc, `STRATA_COORD=pod`,
as it ran then; the `strata-was` engine of `fanout.sh`, 98ba19c) for three
passes, then Strata on main for three passes, writes and the stream, on the
same nodes. `check/` holds the first, renamed `strata` so the tools read it.

| | nodes (shards; client) |
|---|---|
| 1M, `jobs/rerun-1m` | two m-4vcpu-32gb, Xeon Platinum 8168; client c-8, 8168 |
| 10M, `jobs/rerun-10m` | two m3-4vcpu-32gb, Xeon Gold 6248 and Platinum 8168; client c-8, 8168 |

## 1M

`1m/`: Strata on main (`run-strata*.json.gz`, writes, stream, builds),
Manticore (`run-manticore*`, load), `check/`, `jobs/`. `tables.md` and
`summary.json` are `analyze.py`'s, `passes.md` the passes side by side.
Every engine from its newest run, ranked: [`1m-ranked.md`](1m-ranked.md)
(Strata first with 137 points, Solr 288, Elasticsearch 292).

Medians of three passes, ms at one client and queries a second at 4 and 16:

| kind | p50 main | p50 check | p50 5 Nov | q/s 4 main | q/s 4 check | q/s 16 main | q/s 16 check | q/s 16 5 Nov |
|---|---|---|---|---|---|---|---|---|
| bm25 | 2.03 | 1.64 | 1.66 | 1,194 | 1,273 | 1,982 | 1,888 | 2,023 |
| bm25 + filter | 2.37 | 1.91 | 1.84 | 1,039 | 1,134 | 1,728 | 1,571 | 1,808 |
| phrase | 1.66 | 1.34 | 1.34 | 1,818 | 1,605 | 3,551 | 1,862 | 2,188 |
| prefix | 2.25 | 1.83 | 1.79 | 1,114 | 1,202 | 1,922 | 1,760 | 1,661 |
| typo | 13.15 | 12.69 | 11.17 | 200 | 210 | 252 | 221 | 216 |
| boosted (bm25 × decay) | 4.18 | 3.62 | 3.28 | 558 | 634 | 820 | 830 | 845 |
| facet | 1.92 | 1.53 | 1.48 | 1,556 | 1,404 | 2,959 | 1,646 | 1,954 |
| key lookup | 0.89 | 0.70 | 0.77 | 3,904 | 3,992 | 9,437 | 5,509 | 6,304 |
| sql aggregate | 8.29 | 7.60 | 7.42 | 295 | 306 | 347 | 296 | 282 |
| vector | 2.28 | 1.82 | 1.74 | 1,177 | 1,210 | 2,029 | 1,456 | 1,666 |
| vector + filter | 2.35 | 2.13 | 2.04 | 1,145 | 1,117 | 1,954 | 1,289 | 1,557 |
| similar items | 2.90 | 2.25 | 2.15 | 905 | 1,012 | 1,501 | 1,121 | 1,396 |
| hybrid (RRF) | 5.50 | 4.54 | 4.29 | 454 | 466 | 648 | 596 | 692 |
| hybrid (weighted) | 5.55 | 4.55 | 4.25 | 448 | 479 | 651 | 613 | 673 |

- **The layout trades one client's latency for throughput under load.**
  Main is 4 to 29% slower than the check at the median on one client
  (median of the kinds 24%): the client now reaches a coordinator across the
  network, and that coordinator shares its pod's CPUs with a shard. At 4
  clients it is 12% slower to 13% faster; at 16 clients it serves up to 1.9
  times as many queries (phrase 1,862 to 3,551, key lookups 5,509 to 9,437,
  facets 1,646 to 2,959), since both coordinators work and no CPU sits idle
  in a coordinator pod. Pods averaged 1.86 to 1.94 cores of their 3.4,
  throttled in 5.5 to 11.6% of periods; the check's shards 1.2 cores and its
  coordinator 0.65 (`cpu.jsonl`).
- **Vector + filter recall@10** 0.861, against 0.83 (#263, #278).
- **The nodes.** The check against 5 November's own run of the same image:
  0.91 to 1.14 at the median, 0.80 to 1.06 at 16 clients, slower on most
  kinds. So main's numbers here are, if anything, a little low against the
  engines that ran on 4 and 5 November.
- **Writes, from one client, are slower than on 4 November** (287 inserts a
  second against 417, batches of 100 1,301 against 2,127, updates 285
  against 547; a write searchable in 7.6 ms at the median, against 5.4). At
  16 clients they are level or faster (inserts 2,186 against 2,153, batches
  4,947 against 3,955) except updates (4,509 against 5,495). The check ran
  queries only, so other nodes, the coordinator across the network and the
  partitioned vectors are not separated.
- **The build** took 533 s a shard against 261 (merges 180 s against 29):
  the shards now build a vector graph per rating (7d081da). Shown, not
  ranked, as before.
- **Manticore** with its query cache off: 0.88 to 1.16 of its 4 November
  median time, 0.89 to 1.18 of its queries a second at 4 clients, on another
  node (8280, as then); 8th with 364 points, against 9th with 370.

## 10M

`10m/`: the same files as `1m/`, without Manticore. Every data file hashes
as 5 November's (`data/SHA256SUMS`). Every engine from its newest run,
ranked: [`10m-ranked.md`](10m-ranked.md). Strata is first with 88 points,
OpenSearch 180, Elasticsearch 186 (nine engines; 133, 159 and 179 on 3
November), and fastest on 26 of the 45 tests. Head to head with
Elasticsearch and OpenSearch of 5 November: 55, 107 and 108, and fastest on
37 tests (Elasticsearch on typos, OpenSearch on BM25 under load and key
lookups).

Medians of three passes, ms at one client and queries a second at 4 and 16;
Elasticsearch and OpenSearch from 5 November's nodes:

| kind | p50 main | p50 check | p50 5 Nov | q/s 4 main | q/s 4 check | q/s 16 main | q/s 16 check | q/s 16 5 Nov | q/s 16 ES | q/s 16 OS |
|---|---|---|---|---|---|---|---|---|---|---|
| bm25 | 3.49 | 2.97 | 2.90 | 486 | 549 | 604 | 584 | 591 | 683 | 720 |
| bm25 + filter | 4.01 | 3.72 | 3.59 | 422 | 473 | 522 | 499 | 502 | 435 | 428 |
| phrase | 2.13 | 1.69 | 1.51 | 1,343 | 1,253 | 2,404 | 1,516 | 2,028 | 468 | 472 |
| prefix | 5.51 | 5.08 | 5.18 | 273 | 300 | 314 | 296 | 295 | 298 | 224 |
| typo | 43.89 | 45.75 | 48.75 | 46 | 43 | 46 | 39 | 41 | 71 | 51 |
| boosted (bm25 × decay) | 8.55 | 8.03 | 8.39 | 195 | 213 | 219 | 196 | 200 | 42 | 41 |
| facet | 2.83 | 2.41 | 2.27 | 874 | 898 | 1,383 | 1,253 | 1,585 | 715 | 665 |
| key lookup | 1.02 | 0.80 | 0.72 | 3,542 | 3,886 | 9,159 | 5,529 | 6,756 | 9,050 | 12,620 |
| sql aggregate | 36.66 | 43.40 | 43.49 | 39 | 32 | 37 | 30 | 31 | 5 | 4 |
| vector | 2.83 | 2.43 | 2.25 | 849 | 972 | 1,248 | 1,231 | 1,460 | 1,287 | 934 |
| vector + filter | 3.01 | 3.24 | 3.09 | 774 | 703 | 1,117 | 898 | 818 | 267 | 381 |
| similar items | 3.84 | 4.46 | 4.33 | 567 | 491 | 782 | 551 | 504 | 88 | 72 |
| hybrid (RRF) | 8.50 | 7.98 | 7.55 | 213 | 237 | 242 | 238 | 236 | – | 193 |
| hybrid (weighted) | 8.49 | 8.25 | 7.53 | 213 | 234 | 240 | 236 | 238 | 115 | 195 |

- **The layout, as at 1M but smaller.** Against the check, main takes 0.84
  to 1.26 of its time at the median on one client (1.07 over the kinds:
  BM25 1.17, phrase and key lookups 1.26), and serves 0.87 to 1.22 of its
  queries a second at 4 clients (0.91 over the kinds) and 1.01 to 1.66 at 16
  (1.11: phrase 1.59, key lookups 1.66).
- **#285 fixed 5 November's one regression.** Similar items take 3.84 ms
  and serve 782 queries a second at 16 clients, against the check's 4.46
  and 551 (and 4.33 and 504 on 5 November; 3.78 and 624 before #253).
- **Faster on main besides the layout:** vector + filter, 3.01 ms against
  3.24 and 1,117 queries a second against 898 at 16 clients (#263, #278),
  and the SQL aggregate, 36.7 ms against 43.4 (not traced to a pull
  request).
- **The nodes.** The check ran at 0.94 to 1.12 of its 5 November median
  time, but at 16 clients served only 0.75 to 0.84 of 5 November's queries
  a second on phrase, facets, key lookups and vectors (1,516 against 2,028
  on phrase): these nodes (a Gold 6248 among them) were slower under load.
  Elasticsearch and OpenSearch ran on 5 November's, so where they lead main
  at 16 clients, part of it may be the machines.
- **Writes**, against 4 November's run on other nodes: a write searchable in
  7.1 ms at the median (6.5 then; Elasticsearch 820, OpenSearch 674 on 5
  November). From one client 328 inserts a second against 405, batches of
  100 1,639 against 2,496, updates 367 against 564; at 16 clients inserts
  2,258 against 2,282, batches 4,659 against 4,297, updates 5,408 against
  6,042. As at 1M, not separated from the nodes.
- **The build** took 6,176 and 7,136 s a shard against the check's 3,052
  and 4,064 (merges 3,089 and 4,099 s against 1,268 and 2,303), for the
  vector graph per rating.

### BM25 at 16 clients with the coordinators in the shard pods

The bigger-items plan's item 5 asked whether Strata's BM25 under load at
10M (591 queries a second on 5 November, against Elasticsearch's 683 and
OpenSearch's 720) was the harness's CPU split: two shards of 2.9 CPUs and a
1-CPU coordinator that sat mostly idle, against two data pods of 3.4. #287
gives each shard pod 3.4 CPUs and a coordinator, and the estimate was 10 to
15% more.

Measured on the same nodes: **604 against the check's 584, 3% more.** The
gap to Elasticsearch and OpenSearch (12 and 16%) stays, and it is not the
machines: on BM25 the check served as many queries here as on 5 November's
nodes (584 against 591). `cpu.jsonl` (cgroup `cpu.stat` before and after each pass; cores =
CPU time over wall time, throttled = throttled periods over periods), whole
passes:

| | pod | cores | throttled |
|---|---|---|---|
| main | strata-0, strata-1 (3.4 CPUs each) | 2.08 to 2.17 | 8.7 to 16.7% |
| check | strata-0, strata-1 (2.9 CPUs each) | 1.63 to 1.74 | 10.1 to 18.7% |
| check | coordinator (1 CPU) | 0.40 to 0.41 | 0.0% |

Both layouts hit their CPU limits in bursts, and BM25 at 16 clients is where
the shards are busiest; giving them the coordinator's CPU moved BM25 by 3%,
key lookups and phrase queries (coordinator-heavy, short on the shards) by
59 to 66%. What is left for BM25 under load is the shards' own work.
Per data CPU, main serves 89 BM25 queries a second at 16 clients
(604 / 6.8), Elasticsearch 100, OpenSearch 106.

## Cost

The cluster `efc0d554` (sfo2) ran from 08:50 to 14:48 UTC on 6 November:
$6.70 at list price as `fanout.sh status` counted it before `down` (the
base node and the three jobs' pools; the 1M jobs ended at 10:07, the 10M job at
13:37, after a 2-hour shard build). `down` left no droplet tagged
`strata-bench` and no cluster but `strata-e2e`. With the no-server run's
node in nyc3 ($1.51), the rerun cost $8.21.
