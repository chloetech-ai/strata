---
title: Posting lists
nav_order: 4
---

# Posting lists: formats and algorithms
{: .no_toc }

Strata's `postings` module is a study of compressed posting lists and the algorithms that intersect and unite them. It compares thirteen existing formats on synthetic and real collections, then three new designs built from what the comparison showed: **Pyramid**, **Loom** and **Entropic**. Loom has the size of partitioned Elias-Fano with 2.4–3.0× faster conjunctions and 3.8–7.7× faster unions. Strata's text index stores its long lists as Loom, and its keyword filters as Pyramid.
{: .fs-5 .fw-300 }

1. TOC
{:toc}

## The problem

An inverted index maps each term to the sorted list of documents that contain it. A boolean query is a set operation over those lists: `AND` intersects them, `OR` unites them. Lists are compressed, so the format decides both the index's size and how fast a query can skip through it. Conjunctions spend most of their time in one operation, `NextGEQ(x)`, which moves a cursor to the first docID ≥ *x*; unions spend it writing docIDs out.

Formats fall into three families:

- **Block codecs** (VByte, BP128, OptPFor, binary interpolative coding) encode d-gaps in blocks with a skip table.
- **Succinct codecs** (Elias-Fano, partitioned Elias-Fano, compact tries) answer `NextGEQ` with rank and select instead of decoding. Elias-Fano stores *n* docIDs below *u* in about

  $$
  n\Bigl(2 + \Bigl\lceil \log_2 \frac{u}{n} \Bigr\rceil\Bigr) \text{ bits},
  $$

  and partitioned Elias-Fano (PEF) cuts each list into partitions chosen by dynamic programming, each a run, a bitvector or an Elias-Fano sequence.
- **Bitmaps** (Roaring, a bitmap-and-BP128 hybrid) split the docID space into chunks of 2<sup>16</sup> and store each as an array, a bitmap or runs, so set operations become word operations.

## The testbed

Every format implements the same traits (build, size, decode; a cursor with `docid()`, `next()` and `next_geq(x)`; optionally native multi-list set operations), and every query result of every algorithm is checked against a reference before it is timed.

| family | codecs |
|---|---|
| baseline | uncompressed `u32` arrays; BP128 and Roaring from the `bitpacking` and `roaring` crates |
| block | VByte, BP128, OptPFor, binary interpolative |
| succinct | Elias-Fano, PEF, compact trie, run-trie |
| bitmap | Roaring (with SIMD kernels), hybrid bitmap + BP128 |
| new | Pyramid, Loom, Entropic |

Algorithms: for two sorted arrays, merge, branch-free merge, galloping, binary search, Baeza-Yates and SIMD block merges; for *k* arrays, SvS, Small Adaptive and Sequential; for *k* cursors, rarest-first leapfrog AND, round-robin AND, heap OR, min-scan OR and a bitset-window OR; for unions of arrays, heap, pairwise fold and bitmap.

**Collections.** A *clustered* synthetic collection that mimics a reordered index (topic regions, runs of consecutive docIDs) and a *uniform* one with random docID order, each 8M documents and 21M postings with Zipf list lengths; and **WikiText-103**, 859,532 paragraphs and 57M postings. Each has 500 queries of 2–4 co-occurring terms. Times are single-threaded, best algorithm per codec, with docIDs written out; sizes are bits per posting over the lists the queries use.

## What the existing formats showed

| codec | clustered bits | clustered AND ms | clustered OR ms | WikiText bits | WikiText AND ms | WikiText OR ms |
|---|---:|---:|---:|---:|---:|---:|
| Uncompressed | 32.00 | 0.027 | 1.32 | 32.00 | 0.0089 | 0.496 |
| BP128 | 7.69 | 0.065 | 2.09 | 7.11 | 0.027 | 0.848 |
| OptPFor | 5.92 | 0.068 | 2.15 | 5.91 | 0.028 | 0.856 |
| Elias-Fano | 8.13 | 0.045 | 2.40 | 5.16 | 0.018 | 0.834 |
| PEF | 4.94 | 0.035 | 2.21 | 4.75 | 0.016 | 0.826 |
| Interpolative | 4.90 | 0.135 | 3.47 | 4.77 | 0.060 | 1.31 |
| Roaring | 9.32 | 0.018 | 0.382 | 8.47 | 0.0043 | 0.069 |

*Source: `strata/results/postings/`; the full table, with every codec and the uniform collection, is in [Strata's posting-list notes](https://github.com/rkonow/engram/blob/main/strata/notes/posting-lists.md).*

- **The skip structure matters more than the gap coder.** Conjunctions spend their time in `NextGEQ`. Elias-Fano's select-based jumps made PEF the fastest compressed list codec for AND on every collection, though it decodes 1.3–1.5× slower than BP128. Uncompressed arrays, galloping through plain memory, are still 1.3–2.1× faster than PEF on the M2 (on the Xeon VM that first ran this study, PEF matched them).
- **PEF is the balanced default:** the smallest or within 1% of it everywhere, at 2.0–3.8× Roaring's AND time.
- **Roaring is the speed choice,** especially for unions: the fastest AND on every collection and 4.8–11× faster OR than the best compressed list codec, because ORing words beats merging docIDs one at a time. It costs 1.6–1.9× PEF's space.
- **SIMD only where sizes are similar.** A NEON 8×8 block-compare kernel is 6.8× faster than a scalar merge on equal-size lists, but galloping wins beyond about a 1:35 size ratio, and real conjunctions are skewed.
- **Reorder docIDs.** Clustering cut PEF, interpolative coding, Roaring and the tries by 32–50% and roughly halved conjunction time for list codecs.
- **Implementation quality changes the ranking.** Two Roaring implementations with byte-identical containers differ by 3.0× on AND and 6.5× on OR.

That left a gap: nothing was both as small as PEF and as fast as Roaring. Each new design aims at one corner of it.

## Three new designs

```mermaid
flowchart LR
    R["Roaring<br/>fast, 1.6–1.9× PEF's size"] --> PY["<b>Pyramid</b><br/>speed first:<br/>256-docID blocks,<br/>bitmaps or bytes"]
    P["PEF<br/>smallest, slow OR"] --> LO["<b>Loom</b><br/>balanced:<br/>word-aligned partitions,<br/>batched AND, windowed OR"]
    I["interpolative coding<br/>small, slow"] --> EN["<b>Entropic</b><br/>space first:<br/>tANS over gap tokens"]
```

### Pyramid

*Speed first: make every membership test O(1) at every density, and every set operation local to a 256-docID block.*

Roaring's two costs on these workloads are the mid-density chunk (either a 16-bit-per-value array or an 8 KiB bitmap, with nothing in between) and the skewed probe into a large array chunk. Pyramid adds one level. Each 2<sup>16</sup> chunk is split into 256 blocks of 256 docIDs and stored as one of three kinds: *Small* (a sorted `u16` array of at most 48 values), *Tiered* (present and dense masks, a 256-bit bitmap per dense block, and the low bytes of the other values grouped by block) or *Flat* (all 256 block bitmaps). A block becomes a bitmap when that is no larger than its sparse form at about 9 bits per value:

$$
\text{dense}(B) \iff 256 \le 9\,\lvert B \rvert \iff \lvert B \rvert \ge 29
$$

A probe reads one mask bit, then either one bitmap bit or a handful of bytes. Conjunctions drive small chunks value by value and join larger ones block by block (dense blocks with a 4-word AND, sparse blocks with byte compares); unions accumulate dense groups in a 65,536-bit buffer that touches only present blocks.

### Loom

*Balanced: keep PEF's size, but line dense partitions up on machine words and replace posting-by-posting cursor merging with partition-native algorithms.*

Bit *i* of a bitvector partition with base *b* is docID 64⌊*b*/64⌋ + *i*, so the bitvectors of any two lists share the docID word grid and can be ANDed or ORed word by word. The partitioning dynamic program runs on a 16-posting grid (PEF uses 32) and divides bitvector cost by λ = 5/4:

$$
C(j) = \min_{\substack{i < j \\ j - i \le 4096}} \Bigl[ C(i) + 64 + \min\Bigl\{ \tfrac{1}{\lambda}\,\mathrm{BV}(i,j),\ \mathrm{EF}(i,j) + 12,\ \mathrm{RUN}(i,j) \Bigr\} \Bigr]
$$

The finer grid saves about 1%, and the bias spends it again on bitvectors, which probe in O(1) and OR word-wise, so the total stays at or just below PEF's size.

A conjunction decodes the shortest list in batches of 256 candidates, and every other list filters the batch in place, rarest first, stopping once it is empty. A bitvector partition filters with a branch-free bit test, a run with a range check, and an Elias-Fano partition by a mode chosen from ρ, its postings per candidate over the candidates' range:

$$
\text{mode}(\rho) =
\begin{cases}
\text{merge: decode the range, SIMD } 16{\times}16 \text{ block intersection} & \rho \le 12 \\
\text{far probes: an independent select per candidate} & \rho \ge 64 \\
\text{forward probes: 2–4 interleaved branch-free streams} & \text{otherwise}
\end{cases}
$$

A union walks the docID space in windows of 2<sup>16</sup>, ORs bitvectors word-wise, fills runs, decodes Elias-Fano postings with SIMD and sets them as bits, then extracts the window.

### Entropic

*Space first: model the gap stream instead of bounding it.*

Each block of 128 postings codes its gaps as one of 64 tokens (gaps 0–3 exactly; larger gaps as their bit width plus the bit below the leading one, with the remaining bits raw). Token *t*<sub>i</sub> is coded with context-switching tANS in the context of *t*<sub>i−2</sub>, so the size of a list approaches the conditional entropy of its token stream:

$$
\lvert \mathrm{enc}(\mathcal{L}) \rvert \approx \sum_i -\log_2 p_{c(t_{i-2})}(t_i) + \sum_i \max(0, w_i - 2) + \lvert \text{model} \rvert + \lvert \text{skip} \rvert
$$

Conditioning on the token two back instead of one costs about 0.02 bits per posting but splits the stream into two independent chains, which two interleaved tANS lanes decode in parallel. Models are per list, shared and parametric, or a zero-cost geometric fit, whichever is smallest; an Elias-Fano skip table over block maxima keeps blocks skippable.

## Results

Same collections, query generator and harness as the first round, rerun together with the existing codecs:

| codec | clustered bits | clustered AND ms | clustered OR ms | uniform bits | WikiText bits | WikiText AND ms | WikiText OR ms |
|---|---:|---:|---:|---:|---:|---:|---:|
| PEF | 4.94 | 0.035 | 2.21 | 8.08 | 4.75 | 0.016 | 0.828 |
| Interpolative | 4.90 | 0.135 | 3.47 | 8.52 | 4.77 | 0.060 | 1.32 |
| Roaring | 9.32 | 0.018 | 0.384 | 13.70 | 8.47 | **0.0044** | **0.069** |
| **Pyramid** | 6.20 | 0.016 | **0.355** | 10.47 | 6.30 | 0.0081 | 0.078 |
| **Loom** | 4.90 | **0.013** | 0.514 | 8.01 | 4.74 | 0.0067 | 0.107 |
| **Entropic** | **4.22** | 0.066 | 0.895 | **7.60** | **4.18** | 0.028 | 0.279 |

*Source: `strata/results/postings/designs/`.*

Loom is on all ten size/time frontiers (AND and OR on five workloads), and on four of them, AND and OR on the clustered and dense collections, every point is a new design. Roaring holds the fast end of both AND and OR on uniform and WikiText, and on the selective queries, which take microseconds, uncompressed arrays are fastest.

![AND on the clustered collection: bits per posting against time; the Pareto frontier is Entropic then Loom, with Pyramid close](assets/img/chart-postings-and-clustered.png)

On the clustered collection the old codecs drop off both frontiers. Loom answers conjunctions faster than Roaring (0.013 against 0.018 ms) at 53% of its size, the result the first round said was out of reach for a list codec; its unions take 1.34× Roaring's time (0.514 against 0.384 ms), and Pyramid's are the fastest (0.355 ms).

![AND on WikiText-103: Entropic, Loom and Roaring on the frontier; Loom at 0.0067 ms, Roaring at 0.0044 ms](assets/img/chart-postings-and-wikitext.png)

![OR on WikiText-103: Entropic, Loom, Pyramid and Roaring on the frontier; Roaring fastest at 0.069 ms](assets/img/chart-postings-or-wikitext.png)

On WikiText-103 the lists are denser and queries more skewed (the shortest list has a median of 288 postings). Loom sits at PEF's size with 2.4× faster conjunctions and 7.7× faster unions; Roaring has both the fastest conjunction and the fastest union, and Pyramid unites in 1.13× Roaring's time with 26% less space.

### Verdict

| design | met | missed |
|---|---|---|
| **Pyramid** vs Roaring | 24–33% smaller; faster unions on the clustered collection (1.08×) and on selective queries (1.31×), level on dense ones; faster conjunctions on clustered and selective queries | unions on uniform and WikiText (13–14% more time); conjunctions on uniform (42% more), WikiText (87% more) and dense queries (24% more), where probes into mid-density sparse blocks cost more than Roaring's gallop through one array |
| **Loom** vs PEF | PEF's size with 2.4–3.0× faster conjunctions and 3.8–7.7× faster unions; on all ten frontiers; fastest conjunction for three- to six-term queries | the stretch goal of beating Roaring everywhere: on uniform and WikiText it takes 21–54% more time on AND, and on all three collections 34–74% more on OR, while Roaring is 1.7–1.9× its size |
| **Entropic** vs interpolative and PEF | the smallest serialized index on every collection, 11–14% below interpolative coding and 6–15% below PEF, 2.0–4.7× faster than interpolative coding | not smaller in memory: decode tables add 1.3–22 MB at load time; conjunctions 1.7–1.9× slower than PEF's; builds 7–10× slower |

**Which one to use.** Loom as the default: it replaces PEF outright and takes at most 1.54× Roaring's conjunction time at 53–58% of its size. Pyramid where unions over clustered docIDs dominate or memory is less tight, as in facets, filters or OR-heavy candidate generation; on uniform and WikiText lists Roaring now unites faster. Entropic for cold or storage-bound tiers, once its decode tables are built on demand.

## In Strata

```mermaid
flowchart TB
    T["text term"] --> Q1{"in ≥ 512 documents?"}
    Q1 -- yes --> LO["Loom docIDs<br/>+ frequencies in 128-posting blocks"]
    Q1 -- no --> BP["BP128 blocks"]
    K["keyword value"] --> Q2{"in ≥ 64 documents?"}
    Q2 -- yes --> PY["Pyramid list"]
    Q2 -- no --> AR["sorted array"]
```

Strata's [text index](strata/index.md#full-text) keeps Loom for long lists, adding term frequencies addressed by posting rank and per-block maximum scores for top-k. On BEIR, against BP128 for every list, it is 2–4% smaller with 7.6–12× faster conjunctions ([Strata results](strata/results.md#posting-formats-and-top-k)). Keyword values use Pyramid, whose unions and count-only intersections serve `IN` filters and facet counts.

## Limitations

- **One machine.** A MacBook Air with an Apple M2 (4 performance and 4 efficiency cores, 24 GB, macOS 26.0.1), on mains power. Two independent runs of the same codecs agree to 0.3% at the median and within 3% in all but one cell (12%), so differences under 5% are not significant. The study first ran on a 4-core cloud VM with an Emerald Rapids Xeon; [Reproducing the results](reproduce.md#what-changed-on-the-laptop) compares the two.
- **NEON kernels.** Every SIMD kernel has an AVX-512 version for x86-64, a NEON version for aarch64 and a portable twin; these numbers time the NEON builds. NEON has 128-bit registers and no compress or gather instructions, so some kernels take a different route than their AVX-512 versions (Entropic's decoder runs four blocks' state machines in scalar registers), and a few stay scalar where NEON was not faster.
- **Tuned on these data, on the Xeon.** Thresholds such as 29 values per dense block, Pyramid's bias toward Flat chunks and Loom's λ = 5/4 were chosen while profiling these collections on the VM and were kept for the M2. The 64× switch from SIMD merging to galloping is one that no longer fits: with NEON, galloping wins from about 1:35.
- **Boolean, static, single-threaded.** No scoring or top-k inside the study, no updates, one core. Queries come from a generator of co-occurring terms rather than a real query log.

## Reports

- [Posting lists: formats and algorithms](reports/posting-lists.html), the first round with every codec and algorithm.
- [The three new designs](reports/posting-list-designs.html), with layouts, diagrams and every frontier.
- [Reproducing the results](reproduce.md#posting-lists) has the commands.
