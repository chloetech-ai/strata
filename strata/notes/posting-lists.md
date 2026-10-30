# Posting lists (`strata::postings`)

Compressed posting-list representations, intersection and union algorithms,
and a benchmark testbed that compares them on synthetic and real collections.
This began as a separate crate, `posting_bench`; it is now the `postings`
module of Strata (`src/postings/`), whose text and keyword indexes use its Loom,
Pyramid and BP128 formats, and the `strata-posting-bench` binary.
Every query result of every algorithm is checked against a reference before
it is timed.

The full write-up, with formulas, diagrams and charts of the measurements, is
in [`report/postings/report.html`](../report/postings/report.html); the raw
numbers are in [`results/postings/`](../results/postings/). Three new structures designed from those results
(Pyramid, Loom and Entropic) are described and evaluated in
[`report/postings/designs.html`](../report/postings/designs.html), with raw
numbers in [`results/postings/designs/`](../results/postings/designs/).

## What is implemented

### Representations (`src/postings/codecs/`)

| Codec | Family | Idea | Traits |
|---|---|---|---|
| `Uncompressed` | baseline | sorted `u32` array, galloping `NextGEQ` | cursor |
| `VByte` | block | varint d-gaps, 128 per block + skip table | cursor |
| `BP128` | block | one bit width per 128 gaps, SIMD-BP128 vertical layout | cursor |
| `OptPFor` | block | patched frame of reference, optimal width per block | cursor |
| `Interpolative` | block | binary interpolative coding per block | cursor |
| `Elias-Fano` | succinct | low bits verbatim + unary high bits, `select0` skips | cursor |
| `PEF` | succinct | partitioned Elias-Fano, DP-chosen run / bitvector / EF partitions | cursor |
| `Roaring` | bitmap | 2^16 chunks with array / bitmap / run containers, SIMD kernels | cursor, set ops |
| `Hybrid` | bitmap | plain bitmaps for dense lists (u/n <= 8), BP128 otherwise | cursor, set ops |
| `Trie` | succinct | compact binary trie, 2 bits per node, rank navigation | cursor, set ops |
| `Run-Trie` | succinct | trie with full subtrees cut | cursor, set ops |
| `BP128 (bitpacking crate)` | baseline | Tantivy's `bitpacking` crate in the same block framework | cursor |
| `Roaring (roaring crate)` | baseline | the `roaring` crate | cursor, set ops |
| `Pyramid` | new | Roaring-like 2^16 chunks split into 256-docID blocks: dense blocks as 256-bit bitmaps, sparse ones as low bytes with a unary or `u16` directory; driven-chunk and block-join AND, touched-block OR | cursor, set ops |
| `Loom` | new | PEF on a 16-posting DP grid with word-aligned bitvectors and a bitvector bias; batched AND with per-partition merge / probe modes, windowed word-wise OR, SIMD EF decode | cursor, set ops |
| `Entropic` | new | 128-posting blocks of gap tokens coded with two interleaved context-switching tANS lanes (context = token two back), per-list or shared models, EF skip tables | cursor, set ops |

All codecs implement `PostingList` (build, size in bits, decode). `CursorList`
adds a cursor with `docid()`, `next()` and `next_geq(x)`; `SetOps` adds
native multi-list intersection and union.

### Algorithms (`src/postings/algorithms/`)

* **Two sorted arrays** (`pairwise`, `simd`): merge, branch-free merge,
  galloping, binary search, Baeza-Yates, SIMD block merge (SSE 4x4, AVX2 8x8
  and AVX-512 16x16 on x86-64; NEON 4x4 and 8x8 on aarch64), and an adaptive
  kernel (SIMD below a 64x size ratio, galloping above).
* **k sorted arrays** (`multiway`): SvS with any pairwise kernel, Small
  Adaptive (Demaine et al.), Sequential (Barbay & Kenyon).
* **k cursors** (`cursor`): rarest-first leapfrog AND (PISA, Lucene),
  round-robin AND (Leapfrog Triejoin), heap OR, min-scan OR (PISA), bitset
  window OR (Lucene `BooleanScorer`).
* **Union of arrays** (`union`): heap, pairwise fold, bitmap.

## Running

```sh
cargo test --release postings              # conformance suite for every codec, and the algorithms
cargo run --release --bin strata-posting-bench -- codecs   # list codecs
cargo run --release --bin strata-posting-bench -- stats    # structure of a collection and its query lists
cargo run --release --bin strata-posting-bench -- run      # default: clustered synthetic collection, 500 queries
cargo run --release --bin strata-posting-bench -- run --distribution uniform --codecs "pef,roaring,bp128" --ops and
cargo run --release --bin strata-posting-bench -- kernels --ratios 1,4,16,64,256,1024
```

Strata's `.cargo/config.toml` builds with `-C target-cpu=native`. The SIMD
paths of the codecs Strata's text index uses (Loom, BP128) and the
intersection kernels are chosen at run time (`strata::simd`), so other
builds use them too; the other codecs of the study (Pyramid, Roaring,
Entropic, the tries) select theirs at compile time. Every path has a
portable fallback.

### Data

* `--source synthetic` (default): Zipf list lengths; `--distribution
  clustered` mimics a reordered index (topic regions, runs of consecutive
  docIDs), `uniform` a random docID order.
* `--source ds2i --input coll.docs`: the binary format of ds2i / PISA
  (`[1, num_docs]` followed by `[len, docIDs...]` sequences of `u32`).
* `--source text --input docs.txt`: one document per line.
* Queries: `--queries file` (term ids, or words for text input), or
  generated: `--query-mode cooccur` picks terms that co-occur in a sampled
  document, `random` picks terms uniformly.
* `strata-posting-bench export` writes a synthetic collection and its queries in
  ds2i format, for use with other systems.

Results are printed as Markdown; `--json` / `--csv` save them.

### Reproducing the report

```sh
scripts/postings/prepare_wikitext.sh data           # WikiText-103 via Hugging Face (needs pyarrow)
scripts/postings/run_experiments.sh data/wikitext103.txt
scripts/postings/run_designs.sh data/wikitext103.txt  # new designs: three collections, selective/dense queries, 2-6 terms
python3 scripts/postings/build_report.py            # report/postings/report.html and designs.html
```

`run` also takes `--min-list-len` / `--max-list-len` to restrict query terms
by list length, and reports a Pareto frontier and the auxiliary memory each
codec builds at load time (Entropic's decode tables).

## Results (single thread, Apple M2 with NEON)

Measured on a MacBook Air with an Apple M2 (4 performance and 4 efficiency
cores, 24 GB, macOS 26.0.1), on mains power, with the NEON kernels. Two
independent runs of the same codecs agree to 0.3% at the median and within 3%
in all but one cell (12%). The study first ran on a 4-core Emerald Rapids Xeon
VM with AVX-512; its numbers are in the git history.

Bits per posting are measured on the lists that appear in queries; times are
the best algorithm per codec, in ms per query, with docIDs written out.
Clustered and uniform: 8M documents, 21M postings, 500 queries of 2-4
co-occurring terms. WikiText-103: 859,532 paragraphs, 57M postings, 500
queries.

| codec | clustered bits | clustered AND ms | clustered OR ms | uniform bits | WikiText bits | WikiText AND ms | WikiText OR ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| Uncompressed | 32.00 | 0.027 | 1.32 | 32.00 | 32.00 | 0.0089 | 0.496 |
| VByte | 9.61 | 0.092 | 2.25 | 10.91 | 8.91 | 0.037 | 0.885 |
| BP128 | 7.69 | 0.065 | 2.09 | 9.39 | 7.11 | 0.027 | 0.848 |
| BP128 (bitpacking crate) | 7.69 | 0.051 | 1.83 | 9.39 | 7.11 | 0.020 | 0.754 |
| OptPFor | 5.92 | 0.068 | 2.15 | 8.87 | 5.91 | 0.028 | 0.856 |
| Elias-Fano | 8.13 | 0.045 | 2.40 | 8.08 | 5.16 | 0.018 | 0.834 |
| PEF | 4.94 | 0.035 | 2.21 | 8.08 | 4.75 | 0.016 | 0.826 |
| Interpolative | 4.90 | 0.135 | 3.47 | 8.52 | 4.77 | 0.060 | 1.313 |
| Hybrid (bitmap + BP128) | 7.58 | 0.052 | 1.33 | 9.31 | 5.85 | 0.019 | 0.305 |
| Roaring | 9.32 | 0.018 | 0.38 | 13.70 | 8.47 | 0.0043 | 0.069 |
| Trie | 7.18 | 0.103 | 3.66 | 13.24 | 6.50 | 0.048 | 0.821 |
| Run-Trie | 6.67 | 0.109 | 3.86 | 13.24 | 6.23 | 0.049 | 0.866 |
| Roaring (roaring crate) | 9.32 | 0.053 | 2.50 | 13.70 | 8.47 | 0.018 | 0.805 |

* Roaring is the fastest for AND and OR on every collection (4.8-11x faster
  OR than the best compressed list codec) at 1.6-1.9x PEF's size.
* PEF is the smallest codec, or within 1% of it, everywhere, and the fastest
  compressed list codec for AND; uncompressed arrays are 1.3-2.1x faster still.
* Rarest-first leapfrog (`NextGEQ`) is 3.7-17x faster than decode-then-intersect.
* The NEON 8x8 block merge is 6.8x faster than a scalar merge on equal-size
  lists; galloping wins beyond a ~1:35 size ratio.
* DocID clustering shrinks PEF, interpolative, Roaring and tries by 32-50%.

Full analysis, formulas and diagrams: [`report/postings/report.html`](../report/postings/report.html).

### New designs

Same collections and query generator, rerun together with the existing codecs
(`results/postings/designs/`, so the reference rows differ slightly from the table above):

| codec | clustered bits | clustered AND ms | clustered OR ms | uniform bits | WikiText bits | WikiText AND ms | WikiText OR ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| PEF | 4.94 | 0.035 | 2.21 | 8.08 | 4.75 | 0.016 | 0.828 |
| Interpolative | 4.90 | 0.135 | 3.47 | 8.52 | 4.77 | 0.060 | 1.32 |
| Roaring | 9.32 | 0.018 | 0.384 | 13.70 | 8.47 | 0.0044 | 0.069 |
| **Pyramid** | 6.20 | 0.016 | 0.355 | 10.47 | 6.30 | 0.0081 | 0.078 |
| **Loom** | 4.90 | 0.013 | 0.514 | 8.01 | 4.74 | 0.0067 | 0.107 |
| **Entropic** | 4.22 | 0.066 | 0.895 | 7.60 | 4.18 | 0.028 | 0.279 |

* Loom is on all 10 size/time frontiers (AND and OR on five workloads), and on
  4 of them (AND and OR on the clustered collection and the dense queries)
  every point is a new design. Roaring has the fastest AND and OR on uniform
  and WikiText, and uncompressed arrays on the selective queries.
* Loom: PEF's size with 2.4-3.0x faster AND and 3.8-7.7x faster OR; on the
  clustered collection it also beats Roaring on AND at 53% of its size, but
  takes 1.34x Roaring's time on OR.
* Pyramid: 24-33% smaller than Roaring; OR is faster on clustered data (1.08x)
  and selective queries (1.31x) but 13-14% slower on uniform and WikiText; AND
  is 15% faster on clustered data but 42% and 87% slower on uniform and
  WikiText.
* Entropic: the smallest serialized index (6-15% below PEF, 11-14% below
  interpolative coding, 2.0-2.3x faster AND than the latter), but its decode
  tables add 1.3-22 MB at load time and it builds 7-10x slower than PEF.

Details: [`report/postings/designs.html`](../report/postings/designs.html).

