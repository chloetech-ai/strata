# Scale probe: MS MARCO v2.1 passages at 1M, 2M and 5M

Phase 1 of the web-scale plan starts with a scale probe on the 113.5M
passages of MS MARCO v2.1 (segmented). Before renting a large machine, this
probe ran the same measurements at 1M, 2M and 5M passages in a cloud
container, to decide which comes first: parallel segment search (W3) or
work on the vector index.

All numbers are from one machine: an Intel Xeon @ 2.80 GHz with 4 cores and
15.7 GiB RAM, a Claude Code on the web cloud container, on 2026-10-27,
06:10-08:10 UTC. Raw steps are in `results/scale/steps.jsonl` and every table
is in `results/scale/README.md`. Cloud containers drift from day to day (see
`results/concurrency/`), so compare runs from the same day.

## Setup

* **Passages**: the first 5,649,026 rows of MS MARCO v2.1 segmented (shards
  00-02 of 60). Vectors come from
  `huggingface.co/datasets/Snowflake/msmarco-v2.1-snowflake-arctic-embed-m-v1.5`
  (Arctic Embed M v1.5, Apache 2.0). Its Parquet files hold only `doc_id` and
  the 768-d embedding. The text comes from `msmarco_v2.1_doc_segmented.tar`
  on `msmarco.z22.web.core.windows.net`, one member per shard, fetched by
  HTTP range request (`scripts/prepare_msmarco.py`, which checks that every
  id matches). Each vector is cut to its first 256 dimensions (Matryoshka)
  and renormalised. The dataset embeds `title + "\n" + segment`: embedding 64
  passages locally with the ONNX model gave cosine 1.0000 with the dataset's
  vectors.
* **Queries**: the first 1,000 MS MARCO v2 passage dev queries
  (`passv2_dev_queries.tsv`), embedded locally on the CPU with the model
  card's query prefix, CLS pooling and the same cut to 256 dimensions
  (`scripts/embed_queries.py`).
* **Table**: `data::beir_schema(256, HNSW m 16, ef_construction 200)`. `body`
  is title + segment, indexed with positions. `year`, `category` and
  `rating` are synthetic and seeded. `topic` is the k-means cluster (32) of
  the vector, as in the BEIR runs.
* **Filters**: three shapes in turn. The first is broad (`year BETWEEN 2015
  AND 2024`, 29%). The second is selective and unrelated to the query
  (`category = 'c10'`, about 2%). The third is selective and correlated with
  the query (`topic` = the query's own cluster `AND year >= 2000`).
* **Recall**: vector recall@10 is measured against an exact float32 top-k
  over the same rows (`scripts/exact_topk.py`). Filtered vector recall is
  measured against Strata's prefilter scan with rerank 100.
* **Latency**: warm. Each query runs once to warm up, then once timed. 1,000
  queries are used for BM25 and vector; 300 for filtered, hybrid, parallel
  and throughput runs.
* **Builds**: 4 threads for HNSW construction. Peak RSS (`ru_maxrss`)
  includes the file pages a step reads through its mapping, so heap use
  (`RssAnon`, sampled every 100 ms) is reported separately where it was
  sampled.

Two runs:

1. **Growth**: bulk loads into 100K-document segments (the default) with
   the default merge policy (factor 8, at most 2M documents per merge),
   queried at 1M and 2M.
2. **Segments**: 5M passages loaded into 57 segments of 87,720 documents.
   113.5M passages in segments of 2M (the default `max_merged_docs`) would
   be 57 segments. The segments were then merged down to 19 (factor 3, at
   most 270K per merge), 7 (factor 4, at most 1.1M) and 5, with a vacuum and
   the full query set after each step. Merges were kept near 1M documents
   because a 2M merge does not fit in RAM here (see Merges below).

## Results

### Loading

| | passages/s | HNSW share of the load | bytes/passage |
|---|---|---|---|
| 1M in 100K segments | 4,528 | 66% (145 of 221 s) | 2,236 |
| +1M in 100K segments | 4,270 | 62% | 2,254 |
| 5M in 87,720-document segments | 4,261 | 64% (754 of 1,173 s) | 2,253 |

The load rate stays flat from 1M to 5M, because each bulk-loaded segment is
built on its own. Peak RSS was 1.0-1.3 GiB.

### Merges

| merge | passages rewritten | seconds | passages/s | HNSW | text | reading inputs | peak heap |
|---|---|---|---|---|---|---|---|
| 8 × 100K into 800K (1M) | 800,000 | 251 | 3,192 | 190 s (76%) | 33 s | 22 s | - |
| 8 × 100K into 800K (2M) | 800,000 | 247 | 3,235 | 187 s (76%) | 35 s | 21 s | - |
| 57 → 19 (3 × 88K each) | 5,000,000 | 1,451 | 3,445 | 976 s (67%) | 205 s | 232 s | - |
| 19 → 7 (4 × 263K each) | 4,210,520 | 1,656 | 2,542 | 1,165 s (70%) | 223 s | 229 s | 8.7 GiB |
| 3 × 263K into 789K | 789,480 | 290 | 2,723 | 218 s (75%) | 39 s | 26 s | 5.8 GiB |

* **HNSW construction is 67-76% of merge time.** A merge rebuilds the graph
  from scratch from the merged rows. HNSW build speed falls as graphs grow:
  about 6,600-6,900 vectors/s in 88K-100K segments, 5,100 at 263K, and
  3,600-4,300 at 0.8M-1.05M (4 threads). The two 800K-class merges of the
  5M run were slower per vector than the growth run's (3,600 against
  4,200/s), with 10 GiB of table competing for the page cache; that spread
  is the run-to-run noise to expect here.
* **A merge holds everything in memory**: the rebuilt documents, every
  builder and the segment image. Peak heap was 5.8 GiB for 789K passages and
  8.7 GiB for 1.05M, about 7.9-8.9 KB per passage. A merge at the default 2M
  cap would need about 16-18 GB, more than this machine has.
* **The file grows past the live data.** Pages a commit frees can be reused
  only two commits later, and a segment needs one contiguous extent. After
  the 2M growth merge the file was 7.54 GiB for 4.19 GiB of live data
  (1.8x). After the four 1.05M merges it was 17.04 GiB for 10.43 GiB
  (1.6x). Vacuum returned 6.59 GiB in 120 s.

### Queries as the table grows (growth run, default layout)

| | segments | BM25 | filtered BM25 | vector ef 64 (recall) | ef 256 (recall) | filtered vector | hybrid RRF |
|---|---|---|---|---|---|---|---|
| 1M | 3 | 0.52 / 4.71 | 0.98 / 5.59 | 0.32 / 0.52 (0.849) | 0.82 / 1.85 (0.947) | 0.56 / 1.75 | 1.52 / 6.98 |
| 2M | 6 | 0.94 / 7.68 | 2.15 / 12.49 | 0.69 / 1.02 (0.881) | 1.76 / 2.41 (0.959) | 2.23 / 5.57 | 2.73 / 13.04 |

The cells are p50 / p99 in ms, one query at a time.

### Queries at 5M by segment count

| segments | BM25 | filtered BM25 | vector ef 64 (recall) | ef 128 (recall) | ef 256 (recall) | filtered vector | hybrid RRF |
|---|---|---|---|---|---|---|---|
| 57 | 5.34 / 28.46 | 8.30 / 37.00 | 6.53 / 9.35 (0.954) | 10.65 / 14.10 (0.976) | 16.90 / 22.15 (0.992) | 9.20 / 19.19 | 18.94 / 70.13 |
| 19 | 3.57 / 25.56 | 6.82 / 32.54 | 2.70 / 4.45 (0.930) | 4.44 / 6.20 (0.961) | 7.55 / 12.05 (0.981) | 6.53 / 15.60 | 9.64 / 42.26 |
| 7 | 2.37 / 18.41 | 4.82 / 26.07 | 1.19 / 1.58 (0.885) | 1.77 / 2.52 (0.931) | 2.98 / 4.37 (0.962) | 4.21 / 9.67 | 5.50 / 30.19 |
| 5 | 2.16 / 18.28 | 5.56 / 24.83 | 0.90 / 1.51 (0.879) | 1.20 / 1.57 (0.928) | 2.12 / 4.03 (0.958) | 3.40 / 10.26 | 4.66 / 28.89 |

* **Segment count matters most for vector search.** Every segment runs its
  own HNSW search, so recall rises and cost rises with the number of
  segments. At equal recall (about 0.96), 57 segments cost 6.5 ms (ef 64),
  19 cost 4.4 ms (ef 128), 7 cost 3.0 ms and 5 cost 2.1 ms (ef 256): 3.1x
  from 57 to 5. BM25 changes less, 2.5x from 57 to 5 segments, and its p99
  stays at 18-28 ms, set by long queries.
* **These web queries are hard for HNSW.** With few large segments, ef 64
  gives 0.88 recall, and ef 256 is needed to reach 0.96. faiss
  `IndexHNSWFlat` (M 16, efConstruction 200) on the first 200K passages
  gave 0.824 at ef 64 and 0.939 at ef 256 with the same queries, so this
  comes from the data and the queries, not from Strata's HNSW. Filtered
  vector recall against the prefilter scan is 0.92-0.98.

### Segment-parallel prototype

`ExecOptions::segments` restricts a query to a subset of the snapshot's
segments, while BM25 statistics are still summed over all of them. The
probe splits the segments into 4 groups of about equal documents, runs one
group per worker thread and merges the top-k lists by score. For hybrid
RRF it fetches the BM25 and vector top-100 that way and fuses them in the
harness. In every configuration the results matched serial execution:
0.998-1.000 of the serial top-10 ids, with the differences coming from
score ties at the cut.

| segments | BM25 serial → parallel | vector | filtered vector | hybrid RRF |
|---|---|---|---|---|
| 57 | 5.34 → 1.59 ms (3.35x) | 6.53 → 1.79 (3.64x) | 9.20 → 3.44 (2.68x) | 18.94 → 6.49 (2.92x) |
| 19 | 3.57 → 1.76 (2.03x) | 2.70 → 0.93 (2.90x) | 6.53 → 2.60 (2.51x) | 9.64 → 4.58 (2.10x) |
| 7 | 2.37 → 1.34 (1.76x) | 1.19 → 0.70 (1.71x) | 4.21 → 2.32 (1.81x) | 5.50 → 3.08 (1.79x) |
| 5 | 2.16 → 1.48 (1.46x) | 0.90 → 0.58 (1.55x) | 3.40 → 2.27 (1.50x) | 4.66 → 3.10 (1.50x) |

All cells are p50, on 4 threads. The speed-up is near-linear with many
segments. It shrinks as segments grow, because a query cannot be split
finer than its largest segment.

### Throughput with 4 concurrent callers (serial queries)

| segments | BM25 queries/s | vector queries/s | hybrid RRF queries/s |
|---|---|---|---|
| 57 | 666 | 640 | 196 |
| 19 | 894 | 1,553 | 332 |
| 7 | 938 | 3,634 | 485 |
| 5 | 1,411 | 5,340 | 604 |

Segment-parallel search does not add throughput. Four callers already keep
the 4 cores busy: at 57 segments, one caller on 4 threads does about 630
BM25 queries/s (1/1.59 ms), against 666 for four serial callers. Throughput
comes from doing less work per query, and that means fewer segments,
especially for vectors (8.3x from 57 to 5 segments at ef 64, which also
lowers recall from 0.954 to 0.879).

### Space

5M passages take 10.42 GiB of live pages, or 2,253 bytes per passage:

| component | bytes/passage |
|---|---|
| vectors: float32 rows | 1,024 |
| doc store (title + segment, LZ4) | 417 |
| vectors: SQ8 codes | 256 |
| text: positions | 145 |
| text: postings | 129 |
| vectors: HNSW graph | 133 |
| primary keys | 67 |
| structured fields | 59 |
| text: dictionary, norms | 9 |

Vectors are 63% of the bytes (1,413 B/passage). The float32 rows alone are
45%.

## A bug found on the way: multi-threaded HNSW builds

On the first 20K passages, an HNSW graph built with 4 threads had recall@10
of 0.769 at ef 64, against 0.859 when built with 1 thread. faiss reached
0.883. Construction inserted documents in docID order, so the 4 threads
always inserted neighbouring docIDs at the same moment. In MS MARCO,
neighbouring docIDs are consecutive passages of one document, which are
each other's nearest neighbours. Concurrent inserts cannot see each other,
so those pairs never got linked. The fix in `vector/hnsw.rs`: with more
than one thread, insert in a seeded random order. On the same 20K
passages, 4 threads then reach 0.871 at ef 64 and 0.983 at ef 256 (1
thread in docID order: 0.859 and 0.975).

A shuffled order also helps a single thread (0.878 at ef 64), but
single-threaded builds were left in docID order. engram builds with one
thread, and its results stay as measured. The BEIR runs in
`notes/vector.md` were built with 2 and 4 threads on collections whose
order carries little locality, and reached 0.98-0.99 recall at ef 64. They
are probably affected only slightly, but they were not re-run.

## What this says for Phase 1

**Do W3 (parallel segment search) first, and start the vector-index work
right after it.** W3 fixes latency. The vector-index work fixes build cost,
merge memory and throughput.

* **W3 is cheap and works.** The prototype is one executor option plus a
  worker pool in the harness, and it gives 3.4-3.6x on 4 threads at 57
  segments with the same results. The production version (per-segment
  tasks inside the executor, a top-k merge, fusion over the merged lists)
  looks like 1-2 engineer-weeks; that is an estimate, not a measurement.
  Without W3, latency at 113.5M grows with the data. A linear extrapolation
  from the 5-segment layout gives about 50 ms serial p50 for BM25 and 11-20
  ms for vector search at ef 64 (57 segments of 2M, or 113 of 1M) on this
  CPU. That is an estimate, and per-core speed on the probe machine will
  differ.
* **W3 does not raise capacity.** Under concurrent load, queries/s depends
  on the total work per query. That work grows with the number of
  segments, mostly through vector search.
* **Vector-index work is where the build cost and the memory are.** HNSW
  construction is 62-66% of load time and 67-76% of merge time. The graph
  is rebuilt from scratch at every merge. Merges need 7.9-8.9 KB of heap
  per passage, so the default 2M cap needs about 16-18 GB per merge.
  Vectors are 63% of the bytes on disk. In priority order, the measured
  costs point to:
  1. merges that keep the largest input's graph and insert the other
     inputs, instead of rebuilding;
  2. merges with bounded memory (stream documents and sections instead of
     materialising the whole merge);
  3. building and searching on SQ8, with the float32 rows used only for
     reranking (they could live outside the mapped hot set).
* **W1 (segments as files) is supported by the space numbers.** The file
  reached 1.6-1.8x the live data during merges, and a vacuum needed 120 s
  to return 6.6 GiB. With one file per segment, a merged-away segment is
  deleted and its space returns at once.

### Rough sizing for 113.5M passages

These are linear extrapolations from the measurements above, not
measurements.

* Disk: 2,253 B/passage gives about 256 GB live, and up to 1.6-1.8x that
  while merging, unless vacuumed or until W1 lands.
* Build on this 4-core container: about 7.4 h to load. Merging up to 1-2M
  segments rewrites every passage about twice, at 2,500-3,400 passages/s,
  which is 19-25 h. On a 32-vCPU machine, divide by the speed-up of the
  HNSW build, which was not measured above 4 threads.
* Merge memory at the 2M cap: about 16-18 GB per concurrent merge.

## Reproduce

From `strata/`, with Python 3 and `pyarrow numpy onnxruntime tokenizers` (a
venv works):

```sh
python3 scripts/prepare_msmarco.py /home/user/data/msmarco/prep --shards 0,1,2   # ~17 GB downloaded, ~7 GB kept
# model.onnx + tokenizer.json from huggingface.co/Snowflake/snowflake-arctic-embed-m-v1.5
python3 scripts/embed_queries.py MODEL_DIR passv2_dev_queries.tsv /home/user/data/msmarco/queries --n 1000
PY=python3 scripts/scale_probe.sh            # gt, growth, seg-load, seg-merge (~2 h on 4 cores)
python3 scripts/scale_report.py > results/scale/README.md
```

The 5M table needs about 11 GB on disk, plus up to about 7 GB more while
merging. On a 30 GB disk, the passages beyond the first 1M can be deleted
after `seg-load` (`seg-merge` needs only `meta.json` and `centroids.f32`).
That was done in this run.
