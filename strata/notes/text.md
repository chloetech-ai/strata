# Full-text index (`strata/src/text`)

The text index of DESIGN.md §5.1: analyzer, front-coded term dictionary,
posting lists with frequencies and impact bounds in two formats (Loom and
BP128), positions, one-byte norms, boolean retrieval with phrases and
prefixes, and BM25 top-k by exhaustive scoring, MaxScore and Block-Max
WAND, with filters. Every reader structure is a zero-copy view of segment
sections.

Measurements: `strata-text-bench` (`src/bin/strata-text-bench.rs`), raw
numbers in `results/text/<collection>.json`. All runs shared the 4-core
machine with four other agents (load average 7-9), so absolute latencies
are noisy (repeat runs differ by up to ±20%); decisions below rest on
interleaved or same-run comparisons. Re-measure in isolation with:

```sh
cargo run --release --bin strata-text-bench -- \
    --collections scifact,fiqa,trec-covid,quora --max-queries 2000 --reps 3 \
    --cold-queries 100 --sweep-min-df 256,1024,2048
```

Options: `--limit N` (documents), `--formats`, `--algos`, `--filters`,
`--crossover`, `--crossover-queries`, `--no-crossover`, `--loom-min-df`,
`--k`, `--tmp` (cold-cache files), `--out`. Peak RSS: about 630 MB
(TREC-COVID); Quora alone about 220 MB. The four collections take about 25
minutes on the shared machine, most of it Quora's filter crossover.

## Public API

```rust
// strata::text
pub fn analyze(text: &str) -> Vec<String>;
pub fn analyze_with_positions(text: &str) -> Vec<(String, u32)>;
pub fn field_builder(field: &Field, opts: &BuildOptions) -> Result<Box<dyn FieldIndexBuilder>>;

pub enum BoolQuery { Term(String), Phrase(Vec<String>), Prefix(String), And(Vec<BoolQuery>), Or(Vec<BoolQuery>), Not(Box<BoolQuery>) }
impl BoolQuery { pub fn terms(&self) -> Vec<&str>; }
pub fn parse_match(q: &str) -> Result<BoolQuery>;

pub struct Bm25Params { pub k1: f32, pub b: f32 }               // Default: k1 = 0.9, b = 0.4
pub struct CollectionStats { pub n_docs: u64, pub total_len: u64, pub df: HashMap<String, u64> }
impl CollectionStats { pub fn add(&mut self, other: &CollectionStats); pub fn avgdl(&self) -> f32; pub fn idf(&self, df: u64) -> f32; }
pub enum TopKAlgo { Exhaustive, MaxScore, BlockMaxWand }
pub enum FilterStrategy { Auto, Drive, Probe }                   // extension: force a filter strategy
pub enum PostingsFormat { Loom, Bp128 }

impl TextFieldReader {
    pub fn open(seg: &SegmentFile, field: &str) -> Result<Self>;
    pub fn n_docs(&self) -> u32;
    pub fn total_len(&self) -> u64;
    pub fn doc_len(&self, doc: DocId) -> u32;                    // from the one-byte norm
    pub fn doc_freq(&self, term: &str) -> u32;
    pub fn expand_prefix(&self, prefix: &str, limit: usize) -> Vec<String>;
    pub fn postings(&self, term: &str) -> Option<PostingCursor<'_>>;
    pub fn eval(&self, q: &BoolQuery, filter: Option<&dyn DocFilter>) -> DocSet;
    pub fn eval_with(&self, q: &BoolQuery, filter: Option<&dyn DocFilter>, s: FilterStrategy) -> DocSet;
    pub fn eval_cost(&self, q: &BoolQuery) -> f64;               // postings a full evaluation reads
    pub fn estimate(&self, q: &BoolQuery) -> f64;
    pub fn stats(&self, terms: &[String]) -> CollectionStats;    // this segment's share
    pub fn top_k(&self, terms: &[String], stats: &CollectionStats, params: Bm25Params, k: usize,
                 filter: Option<&dyn DocFilter>, algo: TopKAlgo) -> Vec<(DocId, f32)>;
    pub fn top_k_with(/* same */, strategy: FilterStrategy) -> Vec<(DocId, f32)>;
    pub fn score(&self, terms: &[String], stats: &CollectionStats, params: Bm25Params, docs: &[DocId]) -> Vec<f32>;
    // Lower level: term_info, cursor(&TermInfo), decode, intersect_terms, union_terms,
    // phrase_docs, for_each_term, n_terms, norms, format, meta, sizes, list_sizes, is_zero_copy.
}

impl PostingCursor<'_> {                                         // END = u32::MAX when exhausted
    pub fn doc(&self) -> DocId;
    pub fn next(&mut self) -> DocId;
    pub fn seek(&mut self, target: DocId) -> DocId;              // NextGEQ, never backwards
    pub fn freq(&mut self) -> u32;
    pub fn positions(&mut self, out: &mut Vec<u32>);
    pub fn len(&self) -> u32;                                    // df in this segment
    pub fn rank(&mut self) -> usize;
    pub fn frontier(&self) -> &[(u32, u8)];                      // list impact frontier (tf, norm)
    pub fn block_bound(&mut self, target: DocId) -> (DocId, Impact);
    pub fn has_positions(&self) -> bool;
    pub fn is_loom(&self) -> bool;
}

// Building: TextFieldBuilder::new(field, positions, format, loom_min_df), add_text, write, write_as.
// Build options: text.postings = loom | bp128 (default loom), text.loom_min_df (default 512).

// posting_bench::codecs::loom (changed)
impl Loom {
    pub fn write_to(&self, w: &mut ByteWriter);
    pub fn to_bytes(&self) -> Vec<u8>;
    pub fn from_bytes(bytes: OwnedBytes) -> Result<Loom, String>;   // zero-copy view
    pub fn read_from(r: &mut ByteReader) -> Result<Loom, String>;
    pub fn is_shared(&self) -> bool;
    pub fn into_cursor(self) -> LoomCursor<Loom>;                    // a cursor owning its (view) list
}
pub struct LoomCursor<L: Borrow<Loom>>;                              // was LoomCursor<'a>; CursorList::Cursor<'a> = LoomCursor<&'a Loom>
impl<L: Borrow<Loom>> LoomCursor<L> { pub fn rank(&mut self) -> usize; }

// strata::segment::DocFilter (additive)
fn members(&self) -> Option<Box<dyn Iterator<Item = DocId> + '_>> { None }   // DocSet: Some(iter)
```

Sections of field `f`: `text.f.meta` (JSON: format, counts, size
breakdown), `text.f.dict`, `text.f.post`, `text.f.pos` (with positions),
`text.f.norms` (one byte per document).

## Design and the measurements behind it

### Analyzer (`analyzer.rs`)

Words are maximal runs of Unicode alphanumerics (combining accents
U+0300-036F stay inside the word); they are lowercased, Latin-1 and Latin
Extended-A letters are folded to ASCII (`é`->`e`, `ß`->`ss`, `œ`->`oe`),
Lucene's classic 33 English stopwords are dropped and the rest is stemmed
with Snowball English (`rust-stemmers`). Stopwords take no position, so
positions number the kept terms and `"quick the fox"` matches `quick fox`.
The builder caches folded word -> term id, so each distinct surface form is
stemmed once; analysis still dominates build time (TREC-COVID: 4.75
s of analysis, 0.55-0.72 s to write a format).

The ranking matches Pyserini's "BM25 flat" BEIR baselines (Lucene analyzer
with Porter stemming, title and text in one field, k1 = 0.9, b = 0.4):
SciFact 0.682 (Pyserini 0.679), FiQA 0.237 (0.236), TREC-COVID 0.588 (0.595), Quora 0.788 (0.789). BEIR's own numbers (0.665, 0.236, 0.656, 0.789) score title and
text as separate fields, which explains TREC-COVID's gap.

### Term dictionary (`dict.rs`)

Sorted terms in front-coded blocks of 32. Each entry: prefix length,
suffix, df, postings length, positions length (varints); offsets are sums
within the block, starting from the block's record. The block index is an
array of 32-byte records (8-byte big-endian key prefix of the block's first
term, block and first-term offsets, postings and positions offsets) plus
one key per 4 KiB page of records. A lookup binary-searches the page keys,
then the 128 records of one page (reading a first term only when keys tie,
i.e. terms sharing 8 bytes), then scans one block, comparing against the
target with the front-coding prefix lengths instead of rebuilding terms:
two to three pages per term from a cold cache (15-22 dictionary pages
for a cold BM25 query of 6-10 terms, table "Cold cache"), no allocation. An earlier layout with
separate arrays for first-term offsets, block offsets and the two base
offsets read 8.4 dictionary pages per 2-3-term AND query on SciFact; the
record layout reads 6.4. Prefix enumeration starts at the last block whose
first term is below the prefix and stops at the first term past the run.
About 9 bytes per term including the index.

### Postings: Loom vs BP128 (`postings.rs`, `cursor.rs`)

Both formats share the same metadata: the list's **impact frontier** (the
Pareto-optimal (tf, norm) pairs, an exact upper bound of the list's BM25
score under any k1, b and collection statistics) and, for lists of more
than one 128-posting block, a 12-byte block table entry per block (last
docID, offset, impact = max tf and min norm, a bound valid for any
parameters).

* **BP128**: classic 128-posting blocks, d-gaps and tf-1 bit-packed with
  posting_bench's SIMD-BP128 kernels (vertical layout; the partial last
  block packed horizontally), one bit width per block and stream. A cursor
  decodes a block's docIDs on entry and its frequencies on the first
  `freq()` there.
* **Loom**: docIDs are a posting_bench `Loom` list stored with
  `Loom::write_to` and read as a zero-copy view (`Loom::read_from`);
  frequencies are bit-packed per 128 postings *by rank* (horizontal, one
  width per block), and a frequency is read in O(1) from the cursor's rank
  without decoding its block, so MaxScore and Block-Max WAND pay only for
  the frequencies of documents they score. Lists shorter than
  `loom_min_df` (default 512) are BP128 lists, because a serialised Loom
  list has about 124 bytes of fixed overhead (48 bytes of header, 72 bytes
  of zero padding its SIMD decoders read past the payload, alignment).

The Loom format is the default. On the three larger collections it is
smaller (postings section -2.3% on FiQA, -3.8% on TREC-COVID, -3.7% on Quora; docIDs alone -3% to -5%), its native
AND (posting_bench's batched intersection) is 7.5x (FiQA), 10.4x (TREC-COVID) and 8.9x (Quora) faster and its
native OR (windowed union) 1.4x, 1.6x and 1.35x faster than cursor AND / OR over
BP128 lists, Block-Max WAND is 12-16% faster, and MaxScore is within the noise
(-6% to +14%). BP128 wins the exhaustive scan (sequential block decoding
beats per-posting, rank-based frequency reads), which no plan should pick.
Cold, AND reads fewer pages with Loom (FiQA 12.4 vs 14.9 pages, TREC-COVID 15.9 vs 25.1, Quora 19.5 vs 25.9, and about half the latency). On SciFact (5k
documents, few long lists) the formats are within 1.3% in size and a few
microseconds in latency.

The Loom threshold (`text.loom_min_df`) trades size for boolean speed
(table "Loom threshold sweep"): at 1024 the index is smallest; at 512 it is
0.3-0.6% larger on the three larger collections and AND / OR get faster on
FiQA (AND 11.2 -> 8.9 us, OR 46 -> 33) and Quora (OR 266 -> 239; 331 -> 226
in the decision run), unchanged within noise on TREC-COVID (40 boolean
queries); at 256, another 1-1.5% for a little more OR speed. 512 is the
default.

### Frequencies, impacts and norms

* Frequencies are stored as tf - 1 (most are 1): 1.1 (Quora's one-line questions) to 3.0 bits per
  posting.
* The frontier makes list bounds exact; a single (max tf, min norm) corner
  per list would be 4-12% looser on average. Block impacts are such
  corners: their bound exceeds the block's true best score by
  4-12% (p90 7-22%) on average, so storing per-block frontiers would gain
  little.
* Norms are one byte per document, Lucene's `SmallFloat.intToByte4`
  (lengths below 24 exact, then 4 significant bits, rounded down). Scoring
  uses a 256-entry table of `k1 (1 - b + b len / avgdl)` per query. nDCG@10
  with exact lengths differs by at most 0.0021 on the four collections
  (table "nDCG@10"), so exact u16 lengths (twice the bytes, random-read in
  cold queries) are not worth it. `doc_len` returns the quantised length.

### Positions

Per term, per posting: varint deltas (first absolute), with the byte offset
of every 128-posting block's positions at the front for lists of more than
one block. Reading the positions of a posting skips at most the 127
postings before it in its block, using their frequencies; reading postings
in order is one pass. 8.2-9.4 bits per position. Only phrase queries
read them.

### BM25 top-k (`bm25.rs`)

`score = Σ m_t · idf_t · tf (k1 + 1) / (tf + k1 (1 - b + b len / avgdl))`,
idf = `ln(1 + (N - df + 0.5) / (df + 0.5))`, with N, avgdl and df from
`CollectionStats` (summed over segments by the caller; a test checks that
two segments with summed statistics rank exactly like one).

* **Exhaustive**: document-at-a-time over all lists.
* **MaxScore**: lists sorted by bound; a prefix whose bounds sum to at most
  the threshold is non-essential and only probed (by `seek`) for
  candidates of the essential lists while the candidate can still qualify.
* **Block-Max WAND**: PISA's algorithm: pivot by list bounds, refine with
  the bounds of the blocks holding the pivot (cached per list until the
  block changes), evaluate with early exit, else skip the list with the
  largest bound past the smallest block end.

All three keep current docIDs in a dense array beside the (1.2 KB) cursors
(9-18% faster than dispatching to the cursors to find candidates). They
return the same top-k: the tests check them against a naive BM25 to 1e-4
relative on synthetic and SciFact data, with and without filters, in both
formats, and bounds are scaled by 1 + 1e-5 so that float rounding never
prunes a qualifying document.

**MaxScore is the fastest algorithm on every collection.** Mean latency
with Loom lists, MaxScore against Block-Max WAND: SciFact 50 vs 96 us, FiQA 337 vs 884, TREC-COVID 872 vs 2,206, Quora 371 vs 696; exhaustive
scoring is 3-13x slower than MaxScore. By query length (table "by distinct
query terms") the only exception is Quora's 18 queries of one or two
distinct terms (Block-Max WAND 233 us, MaxScore 257). Most queries are long
(5 or more distinct terms after stopwords): WAND-style pivoting then
advances one list at a time, and the 128-posting blocks of frequent terms
span few documents, so block skips are short. A variant of MaxScore that also checked block bounds before probing
non-essential lists was 26% slower on FiQA (465 vs 369 us, medians of three
interleaved runs) and was dropped. The planner should use MaxScore unless a
workload of 1-2-term queries shows otherwise.

### Filters

`top_k` and `eval` take any `DocFilter`. Two strategies:

* **Drive**: iterate the filter's members (via the new
  `DocFilter::members`) and score / check each by seeking the lists
  forward.
* **Probe**: run the query and skip candidates outside the filter
  (`contains`). With a selective filter the top-k threshold rises slowly,
  so pruning weakens: MaxScore with a 0.01% filter is as slow as
  exhaustive scoring.

`Auto` drives when `members * terms * c < cost`, with `cost` the total
length of the query's lists for top-k, and `eval_cost` for boolean queries
(a list costs its length, a union the sum of its parts, a conjunction its
cheapest positive part times the number of positive parts plus its negated
parts). The bench fits `c` per query family by minimising the total time
over every query at 8 selectivities from 0.01% to 30% (2,400 points per
family and collection; TREC-COVID 400). Fitted values on FiQA, TREC-COVID
and Quora: MaxScore 1.3-2.1, Block-Max WAND 0.5-1.3, AND 2.4-3.5, OR
1.1-1.6 (SciFact, whose lists are tiny, fits lower values). The code uses
values in the middle of these ranges: `drive_cost` = 1.8 (MaxScore), 0.9
(Block-Max WAND, exhaustive), and one `PROBE_COST` = 2.0 for boolean
queries. The total-time curves are flat near the optimum: with these
constants each collection's total stays within 8% of its own optimum (22% for Block-Max WAND on SciFact, whose lists are tiny), against 2-10x
for always driving or always probing (table "Filter strategy fits"). In
selectivity, MaxScore drives up to 3-10%, Block-Max WAND up to 3-30%,
AND up to 0.3-3%, OR up to 3-10% (table "Filter crossover rows").

### Zero-copy

`TextFieldReader::open` parses the JSON meta and the dictionary's array
headers; norms, dictionary index and blocks are `Slice::Shared` views, the
postings and positions sections are `OwnedBytes` slices, and a Loom list
is opened per cursor with `Loom::read_from` (four `Slice` views, i.e. Arc
clones). A counting global allocator (`tests/text_zero_copy.rs`) sees 180
bytes allocated by `open` over a 1.9 MB segment, and 172 bytes for a
cursor pass over a 15,828-posting list (its frontier vector), in both
formats.

### Loom refactor (posting_bench)

`Loom` keeps `ends`, `desc`, `words` and a new `ranks` array (rank of each
partition's first posting) in `Slice`s; `write_to` / `to_bytes` write them
with `ByteWriter` (n, size_bits, four arrays), `from_bytes` / `read_from`
check the array lengths and the last partition's rank and view them in
place. `LoomCursor` became generic over how it holds the list
(`L: Borrow<Loom>`: `&Loom` for `CursorList::cursor`, `Loom` for
`into_cursor`), so a cursor can own a view (no self-referential structs in
strata). `LoomCursor::rank` is O(1) in run and Elias-Fano partitions (the
EF cursor's index) and incremental in bitvector partitions (popcounts of
the words passed since the last call in the same partition), and costs
nothing unless called. `size_bits` excludes the ranks (32 bits per
partition) so posting_bench's numbers stay comparable. New tests:
serialised views decode, seek, rank and combine (AND/OR mixing views and
built lists) like built lists, malformed bytes are rejected, ranks match
after any mix of `next` / `next_geq` with both cursor kinds. All 93
posting_bench tests pass. Speed, `posting-bench run --codecs loom
--num-queries 300 --reps 5`, medians of three interleaved runs before /
after:

| workload | native AND ms | native OR ms | leapfrog AND ms | decode ns/posting |
|---|---|---|---|---|
| clustered | 0.0176 / 0.0167 | 0.498 / 0.508 | 0.0376 / 0.0339 | 0.430 / 0.468 |
| uniform | 0.0500 / 0.0419 | 0.735 / 0.714 | 0.0885 / 0.0846 | 0.473 / 0.438 |

All differences are within the run-to-run noise of the shared machine.

## Open issues

* **Block-Max WAND is slower than MaxScore** on all four collections
  (long queries; it wins only Quora's few one- and two-term queries). Variable-sized blocks or block-max MaxScore with windowed
  essential-list recomputation (as in Lucene's `MaxScoreBulkScorer`) may
  change that; a simple block-bound check inside MaxScore did not.
* **Per-list overhead of short lists.** A singleton list costs about 5
  bytes of metadata (frontier: count, tf, norm; block header: two widths)
  on top of its docID, i.e. 40 bits per posting on df = 1 lists, which are
  half the terms (table "Bits per posting by list length"). Inlining
  singletons in the dictionary entry ("pulsing") and deriving their bound
  from the posting would save about 3% of the postings section.
* **Loom's padding is per list.** The 72 zero bytes after a Loom list's
  payload exist only so that its SIMD decoders and probes may read past
  the end. Sharing them between consecutive lists would cut Loom's fixed
  cost to about 50 bytes and allow a lower `loom_min_df`; the code seems to
  need readable bytes there rather than zeros, but that is not verified.
* **Norms are random reads.** A cold BM25 query reads the norm page of
  every scored document: on Quora (523k documents, 128 norm pages) 126 of
  a MaxScore query's 188 pages are norms (table "Cold cache"). Storing the
  norm byte beside each frequency (8 bits per posting) would make them
  sequential; whether that pays depends on how often the index is cold.
* **Filtered Block-Max WAND** (probe strategy) degrades quickly with
  selectivity because pivots ignore the filter; `Auto` drives from the
  filter instead, but a probe-only filter (no `members`) has no
  alternative. A `next_member(from)` method on `DocFilter` would let both
  algorithms skip to the next allowed document.
* **One `PROBE_COST` for every boolean query shape**: the fitted values
  differ between AND (2.4-3.5) and OR (1.1-1.6); a per-operator constant in
  `eval_cost` would be more accurate.
* **Trust in the bytes.** `Dict::open` and `Loom::read_from` check array
  lengths only; corrupt varints in the dictionary panic and corrupt Loom
  descriptors are undefined behaviour. The storage layer's checksums are
  assumed (as the codec assumes its own build).
* **Prefixes are not stemmed** (`vaccination*` does not match the indexed
  `vaccin`): `parse_match` lowercases and folds them only, and they match
  the stemmed terms. Phrases on a field without positions fall back to
  the conjunction of their terms.
* `PostingCursor` is not `Send` (Loom cursors hold raw pointers into their
  view); `TextFieldReader` is `Send + Sync`, so open one cursor per thread.
* `CollectionStats.df` may omit terms: a missing term uses the segment's
  own df (convenient for one segment, wrong across segments).
* The bench's warm latencies come from a machine shared with four other
  agents; re-measure in isolation before quoting absolute numbers.

## Changes outside `strata/src/text`

* `posting_bench/src/codecs/loom/{mod,tests}.rs`: `Slice` storage,
  serialisation, ranks and `LoomCursor::rank`, generic cursor (in scope).
* `strata/src/segment/filter.rs`: `DocFilter::members` (provided method,
  default `None`) and its `DocSet` implementation, so that selective
  filters can drive `top_k` and `eval` instead of being probed. Additive;
  existing filters keep working unchanged (they are probed).
* `strata/Cargo.toml`: dependency `rust-stemmers = "1.2"` (Snowball
  English).

## Measurements

### Collections

| collection | docs | terms | postings | positions | avg len | analyse s | write s (loom / bp128) | peak RSS MB (process so far) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| scifact | 5,183 | 26,608 | 513,267 | 850,695 | 164.1 | 0.18 | 0.04 / 0.03 | 40 |
| fiqa | 57,638 | 57,245 | 3,773,703 | 5,485,744 | 95.2 | 1.00 | 0.17 / 0.14 | 173 |
| trec-covid | 171,332 | 171,963 | 13,298,409 | 21,127,017 | 123.3 | 4.75 | 0.72 / 0.55 | 629 |
| quora | 522,931 | 65,282 | 4,252,544 | 4,457,478 | 8.5 | 0.99 | 0.18 / 0.12 | 629 |

### Sizes (bits per posting; positions in bits per position)

| collection | format | docIDs | freqs | list meta | postings section | positions | dict B/term | segment MB |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| scifact | loom | 7.75 | 2.92 | 3.16 | 13.83 | 9.21 | 8.9 | 2.0 |
| scifact | bp128 | 7.57 | 2.92 | 3.17 | 13.66 | 9.21 | 8.9 | 2.0 |
| fiqa | loom | 7.39 | 2.85 | 1.50 | 11.74 | 9.15 | 9.2 | 11.8 |
| fiqa | bp128 | 7.63 | 2.85 | 1.54 | 12.02 | 9.15 | 9.2 | 12.0 |
| trec-covid | loom | 7.74 | 3.01 | 1.41 | 12.16 | 9.37 | 9.2 | 44.6 |
| trec-covid | bp128 | 8.18 | 3.01 | 1.46 | 12.65 | 9.37 | 9.2 | 45.3 |
| quora | loom | 10.09 | 1.14 | 1.43 | 12.65 | 8.22 | 8.8 | 11.8 |
| quora | bp128 | 10.53 | 1.14 | 1.47 | 13.13 | 8.22 | 8.8 | 12.1 |

### Bits per posting by list length (Loom format)

| collection | df | terms | postings | docIDs | freqs | meta |
|---|---|---:|---:|---:|---:|---:|
| scifact | 1-1 | 13,410 | 13,410 | 15.61 | 2.65 | 40.15 |
| scifact | 2-15 | 9,767 | 44,839 | 11.80 | 2.21 | 10.79 |
| scifact | 16-127 | 2,572 | 117,297 | 9.26 | 2.82 | 1.61 |
| scifact | 128-1023 | 802 | 254,870 | 7.03 | 3.02 | 1.30 |
| scifact | 1024-16383 | 57 | 82,851 | 4.36 | 3.20 | 0.93 |
| scifact | 16384-inf | 0 | 0 | 0.00 | 0.00 | 0.00 |
| fiqa | 1-1 | 31,600 | 31,600 | 15.95 | 0.81 | 40.64 |
| fiqa | 2-15 | 17,838 | 84,305 | 15.22 | 1.10 | 9.79 |
| fiqa | 16-127 | 5,128 | 235,913 | 12.70 | 2.18 | 1.47 |
| fiqa | 128-1023 | 1,935 | 730,628 | 10.28 | 2.61 | 1.22 |
| fiqa | 1024-16383 | 731 | 2,382,744 | 6.21 | 2.93 | 0.87 |
| fiqa | 16384-inf | 13 | 308,513 | 2.51 | 3.96 | 0.83 |
| trec-covid | 1-1 | 91,709 | 91,709 | 20.06 | 1.95 | 40.46 |
| trec-covid | 2-15 | 61,029 | 266,003 | 16.97 | 1.82 | 10.97 |
| trec-covid | 16-127 | 13,065 | 579,891 | 14.95 | 2.82 | 1.70 |
| trec-covid | 128-1023 | 4,168 | 1,497,708 | 12.20 | 3.09 | 1.27 |
| trec-covid | 1024-16383 | 1,858 | 7,143,178 | 7.39 | 2.95 | 0.86 |
| trec-covid | 16384-inf | 134 | 3,719,920 | 4.52 | 3.25 | 0.82 |
| quora | 1-1 | 31,362 | 31,362 | 23.15 | 0.12 | 40.57 |
| quora | 2-15 | 23,581 | 113,597 | 18.63 | 0.22 | 8.68 |
| quora | 16-127 | 7,141 | 321,112 | 16.14 | 0.79 | 1.10 |
| quora | 128-1023 | 2,557 | 910,398 | 13.66 | 1.10 | 1.15 |
| quora | 1024-16383 | 617 | 1,729,642 | 9.62 | 1.19 | 0.85 |
| quora | 16384-inf | 24 | 1,146,433 | 5.06 | 1.29 | 0.82 |

### BM25 top-10, warm, no filter (us: mean / p50 / p99)

| collection | format | exhaustive | MaxScore | Block-Max WAND |
|---|---|---:|---:|---:|
| scifact | loom | 148 / 129 / 441 | 50 / 38 / 195 | 96 / 70 / 384 |
| scifact | bp128 | 107 / 97 / 293 | 44 / 33 / 157 | 102 / 74 / 417 |
| fiqa | loom | 1,363 / 1,206 / 3,849 | 337 / 265 / 1,176 | 884 / 648 / 3,343 |
| fiqa | bp128 | 1,208 / 1,087 / 3,237 | 326 / 267 / 1,113 | 1,057 / 789 / 4,441 |
| trec-covid | loom | 5,097 / 4,486 / 16,192 | 872 / 655 / 3,179 | 2,206 / 1,579 / 15,224 |
| trec-covid | bp128 | 3,659 / 3,506 / 9,997 | 767 / 641 / 2,795 | 2,509 / 1,862 / 12,107 |
| quora | loom | 4,852 / 4,072 / 15,408 | 371 / 216 / 2,522 | 696 / 437 / 3,590 |
| quora | bp128 | 4,197 / 3,690 / 11,948 | 393 / 242 / 2,371 | 826 / 544 / 3,930 |

### BM25 top-10 by distinct query terms (Loom, warm, mean us)

| collection | terms | queries | exhaustive | MaxScore | BMW |
|---|---|---:|---:|---:|---:|
| scifact | 3-4 | 13 | 31 | 12 | 14 |
| scifact | 5-7 | 91 | 89 | 23 | 37 |
| scifact | 8+ | 196 | 182 | 65 | 129 |
| fiqa | 1-2 | 4 | 80 | 30 | 53 |
| fiqa | 3-4 | 53 | 304 | 91 | 163 |
| fiqa | 5-7 | 239 | 859 | 197 | 449 |
| fiqa | 8+ | 352 | 1,879 | 473 | 1,297 |
| trec-covid | 3-4 | 2 | 1,389 | 221 | 418 |
| trec-covid | 5-7 | 21 | 3,731 | 516 | 1,056 |
| trec-covid | 8+ | 27 | 6,434 | 1,198 | 3,232 |
| quora | 1-2 | 18 | 859 | 257 | 233 |
| quora | 3-4 | 292 | 2,065 | 109 | 184 |
| quora | 5-7 | 1074 | 4,608 | 249 | 494 |
| quora | 8+ | 616 | 6,717 | 711 | 1,306 |

### BM25 top-10 with random filters (Auto strategy, warm, mean us)

| collection | format | algo | none | 1% | 10% | 50% |
|---|---|---|---:|---:|---:|---:|
| scifact | loom | exhaustive | 148 | 10 | 60 | 149 |
| scifact | loom | maxscore | 50 | 10 | 50 | 48 |
| scifact | loom | bmw | 96 | 10 | 67 | 107 |
| scifact | bp128 | exhaustive | 107 | 14 | 72 | 114 |
| scifact | bp128 | maxscore | 44 | 13 | 55 | 51 |
| scifact | bp128 | bmw | 102 | 13 | 80 | 120 |
| fiqa | loom | exhaustive | 1,363 | 73 | 580 | 1,363 |
| fiqa | loom | maxscore | 337 | 77 | 345 | 376 |
| fiqa | loom | bmw | 884 | 80 | 616 | 902 |
| fiqa | bp128 | exhaustive | 1,208 | 127 | 736 | 1,256 |
| fiqa | bp128 | maxscore | 326 | 121 | 381 | 375 |
| fiqa | bp128 | bmw | 1,057 | 125 | 770 | 1,080 |
| trec-covid | loom | exhaustive | 5,097 | 258 | 1,775 | 4,587 |
| trec-covid | loom | maxscore | 872 | 395 | 1,105 | 870 |
| trec-covid | loom | bmw | 2,206 | 313 | 1,754 | 2,470 |
| trec-covid | bp128 | exhaustive | 3,659 | 381 | 2,127 | 4,187 |
| trec-covid | bp128 | maxscore | 767 | 484 | 959 | 842 |
| trec-covid | bp128 | bmw | 2,509 | 602 | 2,126 | 2,630 |
| quora | loom | exhaustive | 4,852 | 447 | 3,355 | 5,802 |
| quora | loom | maxscore | 371 | 454 | 475 | 470 |
| quora | loom | bmw | 696 | 404 | 1,414 | 803 |
| quora | bp128 | exhaustive | 4,197 | 645 | 3,509 | 5,224 |
| quora | bp128 | maxscore | 393 | 673 | 475 | 446 |
| quora | bp128 | bmw | 826 | 603 | 1,839 | 992 |

### Boolean retrieval, warm (us: mean / p50 / p99)

| collection | format | AND | OR | mean AND results | mean OR results |
|---|---|---:|---:|---:|---:|
| scifact | loom | 2.5 / 1.7 / 8.6 | 6.6 / 5.1 / 16.3 | 29 | 1,063 |
| scifact | bp128 | 4.2 / 2.3 / 22.2 | 4.3 / 3.7 / 11.4 | 29 | 1,063 |
| fiqa | loom | 8.9 / 8.0 / 29.8 | 33.3 / 26.0 / 145.8 | 544 | 12,895 |
| fiqa | bp128 | 66.8 / 36.0 / 449.3 | 46.6 / 36.5 / 165.3 | 544 | 12,895 |
| trec-covid | loom | 27.1 / 24.2 / 110.1 | 96.2 / 94.2 / 319.6 | 3,622 | 45,310 |
| trec-covid | bp128 | 282.1 / 199.3 / 1,332.2 | 158.2 / 130.8 / 349.1 | 3,622 | 45,310 |
| quora | loom | 17.2 / 7.6 / 124.5 | 238.9 / 151.9 / 1,139.1 | 1,274 | 79,820 |
| quora | bp128 | 152.6 / 29.6 / 2,362.2 | 322.7 / 289.1 / 989.2 | 1,274 | 79,820 |

### Cold cache (mean us; mean pages read: total = dict + postings + norms)

| collection | format | query | mean us | p50 us | pages | dict | post | norms |
|---|---|---|---:|---:|---:|---:|---:|---:|
| scifact | loom | bm25 maxscore | 1,200 | 1,089 | 27.2 | 14.8 | 10.3 | 2.0 |
| scifact | loom | bm25 bmw | 1,504 | 1,257 | 27.1 | 14.8 | 10.3 | 2.0 |
| scifact | loom | and | 484 | 381 | 8.8 | 6.1 | 2.7 | 0.0 |
| scifact | loom | or | 436 | 403 | 8.8 | 6.1 | 2.7 | 0.0 |
| scifact | bp128 | bm25 maxscore | 1,276 | 1,139 | 26.9 | 14.8 | 10.1 | 2.0 |
| scifact | bp128 | bm25 bmw | 1,414 | 1,276 | 27.0 | 14.8 | 10.2 | 2.0 |
| scifact | bp128 | and | 379 | 360 | 8.8 | 6.1 | 2.7 | 0.0 |
| scifact | bp128 | or | 373 | 356 | 8.9 | 6.1 | 2.8 | 0.0 |
| fiqa | loom | bm25 maxscore | 3,956 | 2,978 | 53.4 | 16.9 | 21.5 | 15.0 |
| fiqa | loom | bm25 bmw | 3,907 | 3,148 | 53.4 | 16.9 | 21.9 | 14.6 |
| fiqa | loom | and | 488 | 383 | 12.4 | 8.0 | 4.5 | 0.0 |
| fiqa | loom | or | 986 | 749 | 12.4 | 8.0 | 4.5 | 0.0 |
| fiqa | bp128 | bm25 maxscore | 3,903 | 3,222 | 55.2 | 16.9 | 23.2 | 15.0 |
| fiqa | bp128 | bm25 bmw | 4,237 | 3,748 | 55.2 | 16.9 | 23.7 | 14.6 |
| fiqa | bp128 | and | 923 | 720 | 14.9 | 8.0 | 7.0 | 0.0 |
| fiqa | bp128 | or | 1,307 | 794 | 15.0 | 8.0 | 7.1 | 0.0 |
| trec-covid | loom | bm25 maxscore | 9,715 | 8,733 | 118.1 | 21.9 | 54.2 | 42.0 |
| trec-covid | loom | bm25 bmw | 10,872 | 10,112 | 118.8 | 21.9 | 55.0 | 41.9 |
| trec-covid | loom | and | 1,090 | 709 | 15.9 | 7.6 | 8.3 | 0.0 |
| trec-covid | loom | or | 1,172 | 635 | 16.0 | 7.6 | 8.3 | 0.0 |
| trec-covid | bp128 | bm25 maxscore | 8,436 | 7,615 | 121.9 | 21.9 | 58.1 | 42.0 |
| trec-covid | bp128 | bm25 bmw | 11,248 | 10,022 | 124.8 | 21.9 | 61.0 | 41.9 |
| trec-covid | bp128 | and | 2,231 | 1,648 | 25.1 | 7.6 | 17.4 | 0.0 |
| trec-covid | bp128 | or | 2,260 | 1,685 | 25.3 | 7.6 | 17.6 | 0.0 |
| quora | loom | bm25 maxscore | 8,470 | 7,559 | 188.1 | 15.5 | 46.6 | 125.9 |
| quora | loom | bm25 bmw | 6,816 | 6,258 | 160.7 | 15.5 | 49.9 | 95.2 |
| quora | loom | and | 762 | 600 | 19.5 | 7.5 | 12.0 | 0.0 |
| quora | loom | or | 1,076 | 861 | 21.0 | 7.5 | 13.5 | 0.0 |
| quora | bp128 | bm25 maxscore | 7,602 | 7,417 | 190.0 | 15.5 | 48.5 | 125.9 |
| quora | bp128 | bm25 bmw | 6,840 | 6,486 | 167.6 | 15.5 | 56.9 | 95.2 |
| quora | bp128 | and | 1,125 | 880 | 25.9 | 7.5 | 18.4 | 0.0 |
| quora | bp128 | or | 1,601 | 1,362 | 29.7 | 7.5 | 22.2 | 0.0 |

### nDCG@10

| collection | k1=0.9 b=0.4 | same, exact lengths | k1=1.2 b=0.75 | same, exact lengths | judged queries |
|---|---:|---:|---:|---:|---:|
| scifact | 0.6821 | 0.6803 | 0.6851 | 0.6868 | 300 |
| fiqa | 0.2373 | 0.2381 | 0.2496 | 0.2495 | 648 |
| trec-covid | 0.5881 | 0.5897 | 0.6055 | 0.6034 | 50 |
| quora | 0.7882 | 0.7883 | 0.8086 | 0.8085 | 10000 |

### Bound tightness (k1=0.9, b=0.4; query terms' lists)

| collection | block bound / block max (mean, p90) | list corner / frontier bound (mean, p90) |
|---|---:|---:|
| scifact | 1.039, 1.070 | 1.036, 1.062 |
| fiqa | 1.093, 1.158 | 1.076, 1.115 |
| trec-covid | 1.066, 1.105 | 1.048, 1.070 |
| quora | 1.117, 1.222 | 1.116, 1.190 |

### Filter strategy fits (constant minimising total time; totals in ms)

| collection | family | fitted constant | in code | total fitted | total with code constant | always drive | always probe |
|---|---|---:|---:|---:|---:|---:|---:|
| scifact | and | 1.19 | 2.0 | 4.2 | 4.3 | 11.1 | 7.3 |
| scifact | bmw | 0.22 | 0.9 | 62.1 | 75.7 | 63.0 | 350.9 |
| scifact | maxscore | 0.73 | 1.8 | 45.6 | 49.2 | 62.7 | 208.0 |
| scifact | or | 0.98 | 2.0 | 10.0 | 10.7 | 18.1 | 32.0 |
| fiqa | and | 2.40 | 2.0 | 18.7 | 18.8 | 125.5 | 36.9 |
| fiqa | bmw | 0.46 | 0.9 | 549.2 | 591.0 | 576.9 | 3,533.2 |
| fiqa | maxscore | 1.97 | 1.8 | 315.9 | 317.1 | 570.5 | 1,676.7 |
| fiqa | or | 1.45 | 2.0 | 107.0 | 109.1 | 223.9 | 385.4 |
| trec-covid | and | 3.54 | 2.0 | 10.2 | 10.4 | 68.4 | 25.9 |
| trec-covid | bmw | 0.76 | 0.9 | 248.7 | 252.3 | 301.3 | 1,674.9 |
| trec-covid | maxscore | 2.14 | 1.8 | 137.2 | 141.3 | 308.2 | 792.5 |
| trec-covid | or | 1.61 | 2.0 | 53.9 | 54.9 | 117.2 | 239.2 |
| quora | and | 2.37 | 2.0 | 45.1 | 46.0 | 958.0 | 88.4 |
| quora | bmw | 1.34 | 0.9 | 1,146.8 | 1,210.6 | 2,966.5 | 7,767.1 |
| quora | maxscore | 1.34 | 1.8 | 818.8 | 825.1 | 2,898.7 | 4,937.7 |
| quora | or | 1.11 | 2.0 | 648.5 | 675.2 | 1,718.5 | 3,026.0 |

### Filter crossover rows (mean us: drive / probe / auto)


scifact:

| selectivity | members | MaxScore | BMW | AND | OR |
|---:|---:|---|---|---|---|
| 0.01% | 0 | 7 / 105 / 6 | 6 / 137 / 6 | 1 / 3 / 1 | 1 / 11 / 1 |
| 0.03% | 0 | 7 / 107 / 7 | 7 / 149 / 6 | 1 / 4 / 1 | 1 / 11 / 1 |
| 0.1% | 2 | 6 / 91 / 7 | 7 / 148 / 7 | 1 / 3 / 1 | 1 / 11 / 1 |
| 0.3% | 9 | 8 / 98 / 8 | 7 / 150 / 8 | 1 / 3 / 1 | 1 / 14 / 2 |
| 1% | 68 | 13 / 99 / 13 | 13 / 154 / 12 | 2 / 3 / 2 | 3 / 16 / 3 |
| 3% | 139 | 18 / 90 / 24 | 20 / 174 / 21 | 3 / 3 / 3 | 4 / 19 / 6 |
| 10% | 512 | 47 / 57 / 54 | 45 / 134 / 68 | 8 / 3 / 3 | 14 / 12 / 11 |
| 30% | 1,545 | 104 / 46 / 46 | 105 / 123 / 132 | 20 / 3 / 3 | 35 / 12 / 14 |

fiqa:

| selectivity | members | MaxScore | BMW | AND | OR |
|---:|---:|---|---|---|---|
| 0.01% | 6 | 7 / 1,075 / 6 | 7 / 1,657 / 6 | 1 / 13 / 1 | 1 / 114 / 2 |
| 0.03% | 17 | 8 / 1,076 / 8 | 9 / 1,734 / 8 | 1 / 14 / 2 | 2 / 138 / 2 |
| 0.1% | 57 | 13 / 838 / 12 | 11 / 1,720 / 13 | 2 / 15 / 2 | 3 / 146 / 5 |
| 0.3% | 175 | 30 / 721 / 28 | 29 / 1,658 / 23 | 4 / 15 / 4 | 6 / 177 / 7 |
| 1% | 593 | 76 / 629 / 108 | 88 / 1,495 / 96 | 12 / 19 / 10 | 21 / 214 / 23 |
| 3% | 1,748 | 197 / 557 / 229 | 215 / 1,386 / 195 | 31 / 19 / 16 | 60 / 237 / 76 |
| 10% | 5,760 | 469 / 335 / 339 | 470 / 1,065 / 606 | 108 / 13 / 13 | 198 / 107 / 98 |
| 30% | 17,402 | 1,103 / 356 / 369 | 1,094 / 1,062 / 1,002 | 260 / 14 / 15 | 455 / 152 / 135 |

trec-covid:

| selectivity | members | MaxScore | BMW | AND | OR |
|---:|---:|---|---|---|---|
| 0.01% | 15 | 14 / 3,483 / 9 | 9 / 5,535 / 9 | 1 / 54 / 2 | 2 / 504 / 4 |
| 0.03% | 41 | 19 / 3,487 / 12 | 11 / 5,381 / 11 | 2 / 52 / 2 | 2 / 439 / 3 |
| 0.1% | 161 | 29 / 2,481 / 27 | 26 / 5,012 / 28 | 3 / 60 / 4 | 5 / 572 / 6 |
| 0.3% | 534 | 92 / 1,867 / 84 | 83 / 4,896 / 88 | 11 / 79 / 10 | 18 / 667 / 20 |
| 1% | 1,675 | 261 / 1,564 / 240 | 227 / 3,975 / 236 | 37 / 78 / 29 | 62 / 810 / 83 |
| 3% | 5,150 | 646 / 1,310 / 624 | 583 / 3,323 / 593 | 104 / 96 / 62 | 168 / 993 / 212 |
| 10% | 17,167 | 1,474 / 902 / 862 | 1,419 / 2,776 / 1,633 | 339 / 43 / 42 | 539 / 353 / 364 |
| 30% | 51,545 | 3,628 / 757 / 830 | 3,668 / 2,599 / 3,020 | 871 / 58 / 58 | 1,549 / 446 / 468 |

quora:

| selectivity | members | MaxScore | BMW | AND | OR |
|---:|---:|---|---|---|---|
| 0.01% | 57 | 10 / 4,039 / 11 | 10 / 5,831 / 18 | 4 / 37 / 3 | 5 / 875 / 3 |
| 0.03% | 162 | 19 / 3,608 / 21 | 20 / 5,396 / 20 | 4 / 30 / 4 | 6 / 1,049 / 7 |
| 0.1% | 540 | 60 / 2,811 / 58 | 59 / 4,208 / 65 | 10 / 36 / 8 | 22 / 1,473 / 22 |
| 0.3% | 1,535 | 153 / 2,122 / 158 | 158 / 3,708 / 167 | 35 / 41 / 16 | 57 / 1,534 / 81 |
| 1% | 5,099 | 461 / 1,664 / 468 | 411 / 2,830 / 414 | 85 / 42 / 24 | 179 / 1,785 / 232 |
| 3% | 15,497 | 953 / 1,219 / 997 | 928 / 1,862 / 1,088 | 234 / 61 / 45 | 456 / 1,878 / 463 |
| 10% | 52,775 | 2,547 / 577 / 488 | 2,450 / 1,178 / 1,499 | 814 / 23 / 26 | 1,404 / 657 / 641 |
| 30% | 156,716 | 5,459 / 420 / 447 | 5,852 / 878 / 955 | 2,008 / 26 / 26 | 3,599 / 835 / 869 |

### Loom threshold sweep (loom_min_df)

| collection | loom_min_df | postings section bits/posting | docIDs | MaxScore us | BMW us | AND us | OR us |
|---|---:|---:|---:|---:|---:|---:|---:|
| scifact | 512 | 13.828 | 7.750 | 50 | 96 | 2.5 | 6.6 |
| scifact | bp128 | 13.656 | 7.566 | 44 | 102 | 4.2 | 4.3 |
| scifact | 256 | 14.257 | 8.179 | 55 | 132 | 2.0 | 5.7 |
| scifact | 1024 | 13.635 | 7.552 | 43 | 103 | 3.1 | 6.5 |
| scifact | 2048 | 13.621 | 7.533 | 46 | 107 | 4.1 | 5.2 |
| fiqa | 512 | 11.740 | 7.386 | 337 | 884 | 8.9 | 33.3 |
| fiqa | bp128 | 12.022 | 7.627 | 326 | 1,057 | 66.8 | 46.6 |
| fiqa | 256 | 11.897 | 7.544 | 314 | 816 | 9.0 | 31.1 |
| fiqa | 1024 | 11.665 | 7.309 | 314 | 801 | 11.2 | 46.3 |
| fiqa | 2048 | 11.669 | 7.308 | 335 | 922 | 19.4 | 55.0 |
| trec-covid | 512 | 12.162 | 7.741 | 872 | 2,206 | 27.1 | 96.2 |
| trec-covid | bp128 | 12.646 | 8.177 | 767 | 2,509 | 282.1 | 158.2 |
| trec-covid | 256 | 12.238 | 7.817 | 859 | 2,213 | 37.2 | 132.3 |
| trec-covid | 1024 | 12.128 | 7.706 | 808 | 2,149 | 23.9 | 91.5 |
| trec-covid | 2048 | 12.136 | 7.711 | 744 | 2,065 | 25.9 | 99.0 |
| quora | 512 | 12.652 | 10.089 | 371 | 696 | 17.2 | 238.9 |
| quora | bp128 | 13.131 | 10.527 | 393 | 826 | 152.6 | 322.7 |
| quora | 256 | 12.818 | 10.255 | 364 | 704 | 15.6 | 197.0 |
| quora | 1024 | 12.594 | 10.029 | 375 | 687 | 18.3 | 266.4 |
| quora | 2048 | 12.630 | 10.059 | 395 | 761 | 23.5 | 303.7 |
