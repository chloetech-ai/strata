# Strata after the "best of the best" fixes, 7 November 2026, at 1M and 10M

One rerun of Strata on DigitalOcean (sfo2) with `fanout.sh`, after the
five changes of the plan's first round, and an ef sweep for Strata and Qdrant:

| PR | what it changes |
|---|---|
| #293 | slowdown 3: a merge starts each partition's vector graph from its inputs' graphs instead of rebuilding it |
| #294 | slowdown 4: the merger runs at nice 10 (`--merge-nice`), below the queries |
| #295 | slowdown 5: stream reads no longer wait on a publish of every write batch |
| #296 | the ef sweep: `engine-compare run --candidates N`, `fanout.sh` `EFS=128/256`, `ef.py` |
| #299 | key reads: one server per pod, a shard and a coordinator in one process, the coordinator calling its own shard in-process (`--coordinator-listen`, `--local-shard`; `compare.sh` `STRATA_COORD=process`, now the default) |

| | image |
|---|---|
| new | `ghcr.io/chloetech-ai/strata-server:861ab1d475d4b99707c3fe025e37525dcb5a2559`: main at 8e666228 with #293, #294, #295, #296, #299 and #291's harness (branch `claude/project-thread-50qgya-rerun-image`), `STRATA_COORD=process` |
| 6 Nov | `ghcr.io/chloetech-ai/strata-server:b022e39d6f73295afb2e4bbde8c9d99da262059b`, main on 6 November, as [that run](../rerun-1106-do/README.md) ran it: a shard and a coordinator per pod as two processes (`STRATA_COORD=shards`) |

Both ran in the same job, on the same nodes, one after the other (the
`strata-was` engine of `fanout.sh`): the 6 November image's build and three
passes, then the new image's build, three passes, the ef sweep, writes and
the stream. Data and queries are the files of 4 to 6 November
(`data/SHA256SUMS`, the same hashes). Strata's budget is unchanged: two pods
of 3.4 CPUs and 26 GiB.

| | nodes (shards; client) |
|---|---|
| 1M Strata, `1m/jobs/rerun-1m` | two m-4vcpu-32gb, Xeon Platinum 8168; client c-8, Platinum 8168 |
| 1M Qdrant, `1m/jobs/qdrant-ef` | one m-8vcpu-64gb, Platinum 8168; client c-8, Platinum 8168 |
| 10M Strata, `10m/jobs/rerun-10m` | two m3-4vcpu-32gb (300 GB disk), Platinum 8168; client c-8, Platinum 8168 (replaced at setup, before the first step) |

## 1M

`1m/`: both images' passes (`run-strata-b*` the 6 November image,
`run-strata*` the new one), the new image's ef sweep (`run-strata-ef128*`,
`run-strata-ef256*`), writes, stream and build; Qdrant's passes and ef
sweep. `tables.md` and `summary.json` are `analyze.py`'s (first passes),
`passes.md` every pass, `ef.md` is `ef.py`'s.

Medians of three passes on the same nodes, p50 at one client and queries a
second at 1, 4 and 16 clients:

| kind | p50 ms, 1 client: 6 Nov | new | q/s, 1 client: 6 Nov | new | q/s, 4: 6 Nov | new | q/s, 16: 6 Nov | new | change at 16 |
|---|---|---|---|---|---|---|---|---|---|
| bm25 | 2.31 | 2.38 | 392 | 385 | 1,223 | 1,247 | 2,049 | 2,196 | +7% |
| bm25 + filter | 2.55 | 2.56 | 354 | 356 | 1,063 | 1,110 | 1,814 | 1,954 | +8% |
| vector | 2.38 | 2.48 | 405 | 392 | 1,262 | 1,328 | 2,208 | 2,442 | +11% |
| vector + filter | 2.41 | 2.50 | 399 | 387 | 1,224 | 1,306 | 2,164 | 2,354 | +9% |
| hybrid (RRF) | 5.16 | 5.26 | 184 | 181 | 502 | 507 | 726 | 734 | +1% |
| hybrid (weighted) | 5.14 | 5.18 | 184 | 183 | 496 | 511 | 723 | 731 | +1% |
| boosted (bm25 × decay) | 4.11 | 4.00 | 213 | 216 | 586 | 617 | 841 | 901 | +7% |
| facet | 2.11 | 2.19 | 459 | 439 | 1,568 | 1,701 | 3,131 | 3,400 | +9% |
| key lookup | 1.08 | 1.03 | 881 | 969 | 3,577 | 4,435 | 9,210 | 12,673 | +38% |
| phrase | 1.96 | 1.96 | 490 | 489 | 1,768 | 1,928 | 3,614 | 4,089 | +13% |
| typo | 12.12 | 11.90 | 82 | 82 | 209 | 212 | 259 | 253 | -2% |
| prefix | 2.49 | 2.52 | 368 | 364 | 1,136 | 1,187 | 1,993 | 2,142 | +7% |
| similar items | 2.93 | 2.99 | 329 | 325 | 979 | 1,006 | 1,629 | 1,711 | +5% |
| sql aggregate | 8.00 | 8.13 | 125 | 123 | 310 | 313 | 357 | 367 | +3% |

Key lookups gain the most, as #299 meant: half of the keys are the
coordinator's own shard's and no longer cross gRPC. With one client the
median key is still one of the other half (1.03 ms; on 5 November, with the
coordinator on the client's node, it was 0.70 ms on other nodes). With 16
clients, one process per pod instead of two leaves more CPU to the queries:
5 to 13% more queries a second for most kinds. One client sees the same
latency within 0.1 ms.

Build on the same nodes (`jobs/rerun-1m/steps.tsv`): 470 s for the 6
November image, 336 s for the new one; shard 0's merge took 30 s
(`build-strata-s0.json`; 180 s in the 6 November run, on other nodes).

Writes and the stream ran for the new image only; against the 6 November
run (other nodes): one-document inserts at 16 clients 2,186 → 2,412 a
second, and the stream's flat-out phase lag p50 3,054 → 1,415 ms
(`writes-strata.json`, `stream-strata.json`).

### The ef sweep

`ef.md`: recall@10 against exact neighbours, p50 and p99 at one client, and
queries a second at 16 clients, median of three passes. Candidates 64 is
each engine's first set of passes, Strata's default ef and the harness's
setting for Qdrant.

| engine | kind | candidates | recall@10 | p50 ms | q/s, 16 clients |
|---|---|---|---|---|---|
| Strata | vector | 64 | 0.809 | 2.48 | 2,442 |
| Strata | vector | 128 | 0.875 | 2.60 | 2,081 |
| Strata | vector | 256 | 0.931 | 2.97 | 1,569 |
| Qdrant | vector | 64 | 0.905 | 1.84 | 1,416 |
| Qdrant | vector | 128 | 0.949 | 2.80 | 845 |
| Qdrant | vector | 256 | 0.972 | 4.49 | 475 |
| Strata | vector + filter | 64 | 0.861 | 2.50 | 2,354 |
| Strata | vector + filter | 128 | 0.913 | 2.70 | 2,010 |
| Strata | vector + filter | 256 | 0.952 | 3.04 | 1,557 |
| Qdrant | vector + filter | 64 | 0.871 | 3.31 | 734 |
| Qdrant | vector + filter | 128 | 0.945 | 5.23 | 432 |
| Qdrant | vector + filter | 256 | 0.981 | 9.05 | 243 |
| Strata | similar items | 64 | 0.995 | 2.99 | 1,711 |
| Qdrant | similar items | 64 | 0.975 | 8.42 | 305 |

## 10M

`10m/`: the same files as `1m/` for Strata (Qdrant was not rebuilt at
10M). Both images on the same nodes, as at 1M. The job went through a
restart of the machine driving it at 05:20: the 6 November image's build
jobs ran on in the cluster, and the driver went on from its second step on
the same nodes (`10m/jobs/rerun-10m/driver.log`).

Medians of three passes on the same nodes:

| kind | p50 ms, 1 client: 6 Nov | new | q/s, 1 client: 6 Nov | new | q/s, 4: 6 Nov | new | q/s, 16: 6 Nov | new | change at 16 |
|---|---|---|---|---|---|---|---|---|---|
| bm25 | 3.57 | 3.56 | 212 | 213 | 493 | 492 | 610 | 625 | +2% |
| bm25 + filter | 4.20 | 4.11 | 186 | 189 | 409 | 431 | 522 | 544 | +4% |
| vector | 2.95 | 2.93 | 328 | 329 | 818 | 825 | 1,201 | 1,247 | +4% |
| vector + filter | 3.15 | 3.27 | 307 | 293 | 735 | 766 | 1,041 | 1,137 | +9% |
| hybrid (RRF) | 8.67 | 8.64 | 97 | 97 | 211 | 212 | 238 | 239 | +0% |
| hybrid (weighted) | 8.76 | 8.71 | 96 | 96 | 213 | 210 | 238 | 241 | +1% |
| boosted (bm25 × decay) | 8.72 | 8.89 | 91 | 92 | 192 | 202 | 220 | 227 | +3% |
| facet | 3.00 | 3.07 | 296 | 291 | 837 | 884 | 1,364 | 1,441 | +6% |
| key lookup | 1.04 | 0.99 | 911 | 1,034 | 3,534 | 4,271 | 8,967 | 11,993 | +34% |
| phrase | 2.20 | 2.23 | 432 | 427 | 1,301 | 1,363 | 2,386 | 2,583 | +8% |
| typo | 46.44 | 45.82 | 21 | 21 | 46 | 46 | 48 | 47 | -2% |
| prefix | 5.45 | 5.35 | 145 | 148 | 269 | 278 | 322 | 342 | +6% |
| similar items | 3.99 | 4.00 | 242 | 241 | 539 | 558 | 725 | 753 | +4% |
| sql aggregate | 35.53 | 36.81 | 28 | 28 | 41 | 41 | 39 | 40 | +3% |

Key lookups gain the most again: 8,967 → 11,993 a second at 16 clients
(+34%), 1.04 → 0.99 ms at one client. The other kinds gain 0 to 9% at 16
clients, less than at 1M: at 10M the shards' own work is a larger part of
each query. One client sees the same latency within 4%.

Build on the same nodes (`build-strata-b-s*.json` the 6 November image,
`build-strata-s*.json` the new one), minutes:

| | load: 6 Nov | new | merge: 6 Nov | new | whole build: 6 Nov | new |
|---|---|---|---|---|---|---|
| shard 0 | 50.5 | 50.2 | 54.0 | 39.7 | 104.5 | 89.9 |
| shard 1 | 57.0 | 58.1 | 76.9 | 57.7 | 134.0 | 115.7 |

The merges take a quarter less time (#293: a merge starts each partition's
graph from its inputs' graphs); the shards' files are 24.9 GB instead of
25.2.

Writes and the stream ran for the new image only; against the 6 November
run (other nodes) they are within the spread between node sets: one-document
inserts at 16 clients 2,258 → 2,332 a second, at one client 328 → 289;
updates at 16 clients 5,408 → 5,001; the stream's flat-out lag p50
2,568 → 2,542 ms (at 1M it halved), its p99 at 2,000 a second
640 → 555 ms (`writes-strata.json`, `stream-strata.json`).

### The ef sweep at 10M

`10m/ef.md`, Strata only (Qdrant was not built at 10M); as at 1M, medians
of three passes, p50 at one client and queries a second at 16 clients:

| kind | candidates | recall@10 | p50 ms | q/s, 16 clients |
|---|---|---|---|---|
| vector | 64 | 0.791 | 2.93 | 1,247 |
| vector | 128 | 0.840 | 3.38 | 957 |
| vector | 256 | 0.903 | 4.09 | 630 |
| vector + filter | 64 | 0.821 | 3.27 | 1,137 |
| vector + filter | 128 | 0.865 | 3.69 | 832 |
| vector + filter | 256 | 0.916 | 4.52 | 555 |
| similar items | 64 | 0.980 | 4.00 | 753 |
| similar items | 128 | 0.981 | 4.15 | 684 |
| similar items | 256 | 0.990 | 4.87 | 505 |

At candidates 64, recall is the 6 November run's within 0.003 (vector
0.794, vector + filter 0.819, similar items 0.979 in
[that run's tables](../rerun-1106-do/10m/tables.md)): starting a merge's
graphs from its inputs' graphs (#293) costs no recall. At 10M the same
candidates give less recall than at 1M (0.903 at 256 against 0.931), and
each doubling costs a quarter to a third of the queries a second. All 300
SQL aggregates equal the true answer (`10m/tables.md`).
