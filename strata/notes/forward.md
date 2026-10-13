# Forward index: columns, structured filters, facets, doc store

Component of DESIGN.md §5.2. Code: `strata/src/forward/` (`blocks.rs`
packed blocks and zone maps, `numeric.rs` INT/FLOAT columns and histograms,
`range.rs` range postings and strategy choice, `keyword.rs` dictionary,
codes, value lists and facets, `lists.rs` lazily parsed Pyramid list
tables, `store.rs` doc store), the zero-copy Pyramid in
`posting_bench/src/codecs/pyramid/`, tests in `strata/tests/forward_*.rs`,
experiment binary `strata/src/bin/strata-forward-bench.rs`, results in
`strata/results/forward/`.

## Public API

```rust
// strata::forward — building (called by segment::build::build_segment)
pub fn field_builder(field: &Field, opts: &BuildOptions) -> Result<Box<dyn FieldIndexBuilder>>; // KEYWORD, INT, FLOAT
pub fn build_doc_store(schema: &Schema, docs: &[Document], opts: &BuildOptions, out: &mut SegmentWriter) -> Result<()>;

// Numeric columns (INT, FLOAT). Bounds are inclusive; ±inf for open ends; NaN bounds match nothing.
pub struct NumericColumn;
impl NumericColumn {
    pub fn open(seg: &SegmentFile, field: &str) -> Result<Self>;
    pub fn name(&self) -> &str;
    pub fn len(&self) -> usize;                 // documents
    pub fn is_empty(&self) -> bool;
    pub fn n_present(&self) -> usize;           // documents with a value
    pub fn kind(&self) -> NumericKind;          // Int | Decimal(scale) | FloatBits
    pub fn histogram(&self) -> &Histogram;
    pub fn get(&self, d: DocId) -> Value;       // Int / Float / Null
    pub fn get_f64(&self, d: DocId) -> Option<f64>;
    pub fn get_i64(&self, d: DocId) -> Option<i64>;   // FLOAT: truncated toward zero
    pub fn sort_key(&self, d: DocId) -> Option<i64>;  // order-preserving key, for ORDER BY column
    pub fn min(&self) -> Option<f64>;
    pub fn max(&self) -> Option<f64>;
    pub fn key_range(&self, lo: f64, hi: f64) -> Option<KeyRange>;
    pub fn scan(&self, lo: f64, hi: f64) -> DocSet;   // zone-map pruned scan
    pub fn scan_count(&self, lo: f64, hi: f64) -> usize;
    pub fn probe(&self, d: DocId, lo: f64, hi: f64) -> bool;
    pub fn filter(&self, lo: f64, hi: f64) -> ColumnRangeFilter; // DocFilter, bounds converted once
    pub fn estimate(&self, lo: f64, hi: f64) -> f64;  // estimated matching documents (histogram)
    pub fn blocks_to_decode(&self, lo: f64, hi: f64) -> usize;
    pub fn scan_fractions(&self, r: KeyRange) -> (f64, f64); // sampled: blocks checked, decoded
    pub fn byte_sizes(&self) -> [usize; 5];     // zone maps, offsets, values, presence, histogram
    pub fn is_view(&self) -> bool;
}
pub enum NumericKind { Int, Decimal(u32), FloatBits } // decode(k) -> f64, key_range(lo, hi), is_float()
pub fn float_key(v: f64) -> i64;                // order-preserving key of a double
pub struct KeyRange { pub lo: i64, pub hi: i64 } // contains(k)
pub struct Histogram;                           // len(), bucket(i) -> (lo key, hi key, count), overlapping(KeyRange)
pub struct ColumnRangeFilter;                   // impl DocFilter

// Range postings: equi-depth histogram buckets as Pyramid lists.
pub struct RangeIndex;
impl RangeIndex {
    pub fn open(seg: &SegmentFile, field: &str) -> Result<Self>;
    pub fn open_if_present(seg: &SegmentFile, field: &str) -> Result<Option<Self>>;
    pub fn column(&self) -> &NumericColumn;
    pub fn len(&self) -> usize;                 // buckets
    pub fn bucket(&self, i: usize) -> &Pyramid;
    pub fn plan(&self, lo: f64, hi: f64) -> Option<RangePlan>; // covered / boundary buckets and their sizes
    pub fn eval(&self, lo: f64, hi: f64) -> DocSet;
    pub fn count(&self, lo: f64, hi: f64) -> usize;
    pub fn byte_len(&self) -> usize;
}
pub enum RangeStrategy { Auto, Scan, Postings }
pub fn eval_range(col: &NumericColumn, idx: Option<&RangeIndex>, lo: f64, hi: f64, strategy: RangeStrategy) -> DocSet;
pub fn count_range(col: &NumericColumn, idx: Option<&RangeIndex>, lo: f64, hi: f64, strategy: RangeStrategy) -> usize;
pub fn choose_range_strategy(col: &NumericColumn, idx: Option<&RangeIndex>, lo: f64, hi: f64, count_only: bool) -> RangeStrategy;
pub fn range_costs(col: &NumericColumn, idx: Option<&RangeIndex>, lo: f64, hi: f64, count_only: bool) -> Option<(f64, f64)>; // (scan, postings) ns

// Keyword columns.
pub struct KeywordColumn;
impl KeywordColumn {
    pub fn open(seg: &SegmentFile, field: &str) -> Result<Self>;
    pub fn name(&self) -> &str;
    pub fn len(&self) -> usize;
    pub fn n_values(&self) -> usize;
    pub fn code(&self, value: &str) -> Option<u32>;   // rank in the sorted dictionary
    pub fn value(&self, code: u32) -> &str;
    pub fn code_of_doc(&self, d: DocId) -> Option<u32>;
    pub fn value_of_doc(&self, d: DocId) -> Option<&str>;
    pub fn count(&self, value: &str) -> usize;        // O(1) after the lookup
    pub fn count_code(&self, code: u32) -> usize;
    pub fn docs(&self, value: &str) -> DocSet;
    pub fn docs_code(&self, code: u32) -> DocSet;
    pub fn docs_in(&self, values: &[&str]) -> DocSet; // IN (...): union of the lists
    pub fn filter(&self, value: &str) -> KeywordFilter;       // DocFilter: Pyramid probe
    pub fn filter_in(&self, values: &[&str]) -> CodeSetFilter; // DocFilter: code column + code bitset
    pub fn facet_counts(&self, docs: &DocSet, strategy: FacetStrategy) -> Vec<(String, u64)>; // count desc, then value
    pub fn facet_code_counts(&self, docs: &DocSet, strategy: FacetStrategy) -> Vec<u64>;     // per code
    pub fn choose_facet_strategy(&self, docs: &DocSet) -> FacetStrategy;
    pub fn byte_sizes(&self) -> KeywordSizes;
    pub fn is_view(&self) -> bool;
}
pub enum FacetStrategy { Auto, Codes, Postings }
pub enum KeywordFilter { List(Pyramid), Rare { .. } }  // impl DocFilter
pub struct CodeSetFilter;                               // impl DocFilter

// Doc store.
pub struct DocStore;
impl DocStore {
    pub fn open(seg: &SegmentFile, schema: &Schema) -> Result<Self>; // checks the stored fields' types
    pub fn get(&self, d: DocId) -> Document;                     // non-stored fields and vectors: Null
    pub fn get_fields(&self, d: DocId, fields: &[usize]) -> Vec<Value>; // schema field indices, in order
    pub fn len(&self) -> usize;
    pub fn n_blocks(&self) -> usize;
    pub fn byte_sizes(&self) -> (usize, usize);  // compressed, raw
    pub fn is_view(&self) -> bool;
}
```

`posting_bench::codecs::pyramid::Pyramid` gains:

```rust
pub fn write_to(&self, w: &mut ByteWriter);
pub fn to_bytes(&self) -> Vec<u8>;
pub fn read_from(r: &mut ByteReader) -> Result<Pyramid, String>; // zero-copy views; O(chunks) checks
pub fn from_bytes(bytes: OwnedBytes) -> Result<Pyramid, String>;
pub fn is_view(&self) -> bool;
pub fn heap_bytes(&self) -> usize;                 // bytes of the arrays (vs counted size_bits)
pub fn or_into_bits(&self, words: &mut [u64]);     // OR the list into a bitset
pub fn and_count_bits(&self, words: &[u64]) -> u64; // count-only intersection with a bitset
```

Build options: `forward.range_buckets` (default 64; 0 builds no bucket
lists, estimates then assume uniform values), `forward.kw_rare` (values with
fewer documents use a plain sorted docID array, default 64),
`forward.store_block` (uncompressed doc store block bytes, default 16384).

Sections: `col.<f>`, `col.<f>.hist`, `range.<f>` (numbers);
`kw.<f>.dict`, `kw.<f>.codes`, `kw.<f>.post` (keywords); `store.index`,
`store.data`. Every reader holds `Slice::Shared` views of its sections;
opening reads fixed headers and O(1) checks only. Pyramid lists are parsed
on first use (descriptors checked then) and cached, so a cold open touches
no list.

## Design decisions

### Numeric columns

* **Blocks of 128, frame of reference, two layouts.** A block stores
  `key - min` in `w` bits, `w` the width of its largest delta; its payload
  is exactly `2w` words, so the block offsets are a prefix sum and widths
  need no storage. Widths up to 32 use the `bitpacking::BitPacker4x`
  layout (already a posting_bench dependency): a block unpacks with SSE in
  a few ns, and a single value is still O(1) by lane arithmetic (lane
  `j % 4`, position `j / 4`, two loads at most). Wider blocks (keys
  spanning more than 2^32 inside one block) use plain u64 bit packing.
* **Scan kernel.** A decoded block is compared in the delta domain with
  one unsigned compare per lane (`x - lo <= span`, AVX-512 `vpcmpud` on 16
  lanes, scalar twin), producing the block's two bitset words directly
  (128 documents = 2 words), so a scan writes the result bitset without
  per-document work: ~34 ns per decoded block, 0.27 ns per document.
* **Zone maps.** Per block `(min, max)` as u32 offsets from the column
  minimum when the key range fits 32 bits (0.5 bits/doc), else i64.
  Uniform synthetic fields (year, rating) never prune, but correlated ones
  do: on `seq` (a timestamp-like column) a 0.1% range scans in 2-8 µs
  where unclustered columns take 40-220 µs.
* **Superblocks (32 blocks, 4096 documents).** Measured, then kept: before
  them a selective count on `seq` cost 4-6 µs on TREC-COVID, all of it the
  1339 per-block zone checks (~3.7 ns each, so ~15 µs for Quora's 4086
  blocks); with them it costs 0.2 µs (TREC-COVID) and 0.6 µs (Quora), for
  0.016 bits/doc. They
  cost nothing measurable on unclustered columns (one extra check per 32
  blocks).
* **Presence bitmap only with nulls**; a null slot holds the block minimum
  (so it does not widen the block) and scans AND the presence words.
* **Float encoding: detected decimal scale, else order-preserving bits.**
  Both give O(1) access and integer range checks on the encoded form. If
  every value is `m / 10^s` exactly for some `s <= 9` and `|m| <= 2^53`,
  keys are `m` (decoding `m as f64 / 10^s` reproduces each double; the
  build re-checks every key). Ratings with two decimals become 0..499: 9
  bits/doc packed (9.77 with zone maps and offsets) instead of ~60 bits for
  raw IEEE bits under frame of reference. Otherwise keys are the IEEE bits
  with negative magnitudes flipped (`-0.0` stored as `0.0`, NaN above
  +inf, matching no range). Double bounds map to the smallest and largest
  key whose decoded value is inside, by monotone threshold search
  (exactness tested against decoded values at random and on-value bounds).
* **Histogram.** Equi-depth, 64 buckets by default; equal keys never
  straddle buckets, a run that would more than double a bucket gets its
  own (heavy values). `estimate()` sums covered buckets and the covered
  fraction of cut ones (uniform within a bucket over keys, over values for
  IEEE keys); it is exact for bucket-aligned ranges.

### Range postings and the strategy choice

Bucket `i`'s documents are a Pyramid list. Covered buckets are disjoint, so
**counting them is a sum of stored cardinalities** (0.0-0.2 µs for any
year range against 40-140 µs for a scan) and materialising them needs no
deduplication: OR into a bitset (`Pyramid::or_into_bits`) when the result
is dense (> n/32), Pyramid's k-way union otherwise. Cut buckets (at most
two) are decoded and their members checked against the column: a random
column access each, ~9 ns per member, which dominates for continuous values
(rating: two cut buckets hold ~3% of the documents). Hence:

* year (35 values, a bucket per value): postings win up to ~15% for
  DocSets (1.3 vs 62 µs at 2.9% on TREC-COVID; 5.7 vs 216 µs on Quora),
  scans win at 50%; counts are always postings.
* rating (500 values, ~8 per bucket): postings win below ~1%; above,
  boundary checks cost more than the scan.
* seq (clustered): zone-mapped scans win everywhere.

`forward.range_buckets=256` makes rating ranges mostly bucket-aligned:
postings then take 0.4-9 µs (TREC-COVID) / 1-17 µs (Quora) up to 1%, and
counts ≤ 16 µs at any selectivity, for 14.5-17.3 instead of 10.6-11.2
bits/doc of bucket lists (`results/forward/range_buckets_256/`). 64 stays
the default (DESIGN), the knob is the size/latency trade.

`Auto` compares estimated costs: scan = superblocks + checked blocks (1 ns
per zone map) + decoded blocks (34 ns), with the checked and decoded
fractions estimated from 64 sampled zone maps (so clustered columns are
recognised), + 0.02 ns/doc for a result bitset; postings = cut-bucket
members x 9 ns + (materialising only) covered members x 0.6 ns + the
bitset. Constants are least-squares fits to the bench on both collections.
In the tables below Auto is within noise of the better strategy except
near crossovers, and for rating at 1% on Quora, where cut-bucket checks
cost ~15 ns per member (the larger column misses the cache more) and Auto
picks postings (242 µs) over the scan (176 µs).

### Keyword columns

* **Dictionary: offsets + bytes**, not front-coded, so `value(code)` can
  return `&str` without decoding (64 values: 452 bytes; ids: 12 bytes/doc).
  Codes are ranks, so code order is value order.
* **Codes** reuse the numeric blocks (FOR per 128 with zone maps), which
  costs ~0.77 bits/doc over plain fixed width but lets facet counting
  unpack whole blocks and makes clustered codes cheap (Quora's ids,
  numbered in docID order, pack in 9.1 bits/doc instead of 19).
* **Value lists: Pyramid** for values with at least `forward.kw_rare` (64)
  documents. Rarer values share one sorted docID array (4 bytes/doc plus a
  u32 offset per value; a serialised Pyramid has a fixed cost of a few
  hundred bytes). When every value has exactly one document (a key), the
  offsets are implied: `id` costs 32 bits/doc of postings instead of 64.
  Lists cost ~10 bits/doc for 32-64 values and 4 bits/doc for 4 values
  (Flat chunks).
* `docs(value)` decodes (sparse) or ORs into a bitset (dense; 1 µs for
  36k documents); `IN` ORs lists into a bitset when dense, k-way union
  otherwise.

### Probes

Per candidate (100k candidates, sorted or random order): a column range
probe costs ~7-11 ns, a Pyramid probe ~9-24 ns (Flat chunks ~9-12; Tiered
chunks pay the mask ranks and the directory select), a code-set probe
(`filter_in`, any number of values) 6-9 ns, a bitset DocSet 1.3-1.6 ns and
a sorted DocSet 15-17 ns.
So for per-candidate filtering the code column beats Pyramid's O(1) probe
on these lists; Pyramid's strengths are set operations and count-only
intersections. `filter(value)` remains the Pyramid probe as specified; the
planner should prefer `filter_in(&[value])` for long candidate streams, or
materialise to a bitset when the list is large.

### Facets

* **Codes**: read each result document's code into a counter array;
  for bitset results, blocks with at least 8 members are unpacked whole
  (one SSE unpack) instead of per-document lookups. ~1 ns per document
  plus ~6 ns per touched block.
* **Postings**: count-only intersection of each value's Pyramid list with
  the result as a bitset (`Pyramid::and_count_bits`): Flat chunks and
  dense blocks AND and popcount words, sparse blocks test up to 32 low
  bytes at once with `vpermb` (`simd::test_bits`), chunks of sparse blocks
  only are SIMD-decoded first. Cost is independent of the result size:
  ~75-105 µs (TREC-COVID) / ~200-310 µs (Quora) for 32-64 values, 3-7 /
  6-20 µs for 4 values. A first version tested sparse members one by one
  (470-670 µs on TREC-COVID, never winning); the vectorised one is 6-8x
  faster.
* **Crossover**: 64 values: postings win above ~40% of the documents
  (TREC-COVID ~68k of 171k, Quora ~209k of 523k); 32 values: above ~40%
  (TREC-COVID) to ~70% (Quora); 4 values: above ~0.5% (900 documents of
  171k, 2.7k of 523k), even for sorted results, which pay a bitset build.
* **Auto** compares codes (1 ns/doc + 6 ns per touched block) with
  postings (0.3 ns per bitset word for lists above 7% of the documents,
  0.6 ns per member for sparser ones, 1.5 ns per rare-array document, plus
  0.5 ns/word + 1 ns/doc to build a bitset from a sorted result).

### Doc store

Rows (presence bitmap over the stored fields, varint / zigzag / f64 /
length-prefixed UTF-8) in blocks of 16 KiB uncompressed, LZ4 per block,
with a per-block row-length table so a fetch decompresses one block and
skips to its row. Block index: first docID, compressed offset, raw size
(16 bytes per block). Measured trade-off on TREC-COVID (10 random
documents, warm):

| block | ratio | size | warm get (10 docs) |
|---:|---:|---:|---:|
| 4 KiB | 1.46 | 135.3 MB | 32 µs |
| 16 KiB (default) | 1.59 | 123.9 MB | 93 µs |
| 64 KiB | 1.69 | 116.5 MB | 316 µs |

Fetch time is the block's decompression (lz4_flex safe decoder ~2.4 GB/s;
its unchecked decoder measured only 15% faster, not worth a shared
dependency change). 16 KiB follows DESIGN; `forward.store_block=4096`
gives 3x faster point fetches for 9% more space. Cold fetches (segment
file mapped, evicted and unmapped before each query) read ~38 pages per
document with the kernel's read-ahead and 5-6 with `MADV_RANDOM` (the
block plus index pages); latency is 1.9-2.6 ms per 10 documents either
way, dominated by the device.

### Zero-copy Pyramid

* Arrays are `posting_bench::bytes::Slice`s. `Chunk` descriptors became a
  `#[repr(C)]` POD (flags in a u32 instead of three bools; still 28 bytes)
  so they are viewed in place. Built lists keep their meta and data arenas
  in the original 64-byte-aligned buffer (a `Slice::Shared` view of their
  own `Arena`): with plain `Vec<u64>` (16-byte aligned) count-only unions
  were ~20% slower (64-byte loads of Flat bitmaps split cache lines).
  Serialised lists align both arenas to 64 bytes relative to the buffer,
  so they sit on cache lines in a mapped segment.
* Each array caches its pointer and length next to the `Slice` (`Arr`):
  matching on the `Slice` variant at every access made probes 10% slower.
* `read_from` checks every descriptor offset against the array lengths
  (O(chunks), plus one mask line per chunk) so probes and set operations
  stay in bounds for any descriptor; payload bits (bitmaps, lows, `U`)
  are trusted (see open issues).
* Speed against the previous version, interleaved runs:

| operation (after / before) | posting-bench clustered, 300 queries (median of 3) | posting-bench dense, 200 queries (median of 2) | microbenchmark, min of 12 |
|---|---:|---:|---:|
| decode, ns/posting | 0.96 | 0.99 | 1.03 |
| `contains` (O(1) probe) | - | - | 0.96 |
| AND native | 0.88 | 0.98 | 0.97 |
| AND count only | 1.03 | 0.97 | 1.00 |
| OR native | 1.02 | 0.99 | 1.01 |
| OR count only | 1.00 | 1.13 (min of runs 1.01) | 0.99 |
| AND leapfrog (cursor `next_geq`) | 0.70 | 1.05 | 0.97 |

  Ratios are within the run-to-run noise of a shared machine (a single
  run varies by up to 40%); nothing is systematically slower. Before the
  two fixes above, the same comparison showed OR count-only +20% (arena
  alignment) and `contains` +10% (variant match).

### Experiment binary

`strata-forward-bench --collection {trec-covid,quora,...} [--limit N]
[--reps 5] [--only sizes,range,probe,keyword,facet,store] [--opt k=v]
[--out DIR] [--tmp DIR]` writes `<out>/<collection>.json`. It reads
`docs.jsonl` itself, without the vectors (`data::load` would hold Quora's
2 GB of vectors, twice during loading; the bench peaks at 366-543 MB), and
adds two synthetic fields to the BEIR table: `seq` (INT, increasing with
docID plus local disorder) and `tier` (KEYWORD, 4 values at 40/30/20/10%).
Timings are medians of 5 batches of about 10 ms each. Range windows are
random runs of the sorted values (5 per selectivity). All numbers below
were taken while four other builds and benchmarks shared the 4 cores (load
average 6-10): use them for relative decisions; absolute values will be
re-measured in isolation.

## Measurements

### Sizes (bits per document)

| field | trec-covid | quora |
|---|---:|---:|
| year column | 6.77 | 6.77 |
| year   packed values | 6.00 | 6.00 |
| year   zone maps (block + superblock) | 0.52 | 0.52 |
| year   block offsets | 0.25 | 0.25 |
| year range bucket lists | 10.29 | 9.96 |
| year kind | Int | Int |
| rating column | 9.77 | 9.77 |
| rating   packed values | 9.00 | 9.00 |
| rating   zone maps (block + superblock) | 0.52 | 0.52 |
| rating   block offsets | 0.25 | 0.25 |
| rating range bucket lists | 11.24 | 10.58 |
| rating kind | Decimal(2) | Decimal(2) |
| seq column | 12.25 | 12.22 |
| seq   packed values | 11.49 | 11.46 |
| seq   zone maps (block + superblock) | 0.52 | 0.52 |
| seq   block offsets | 0.25 | 0.25 |
| seq range bucket lists | 2.31 | 1.45 |
| seq kind | Int | Int |
| category values | 64 | 64 |
| category codes | 6.77 | 6.77 |
| category Pyramid lists + rare array | 10.57 + 0.01 | 9.80 + 0.00 |
| category dictionary bytes | 452 | 452 |
| topic values | 32 | 32 |
| topic codes | 5.77 | 5.77 |
| topic Pyramid lists + rare array | 10.01 + 0.01 | 9.81 + 0.00 |
| topic dictionary bytes | 228 | 228 |
| tier values | 4 | 4 |
| tier codes | 2.77 | 2.77 |
| tier Pyramid lists + rare array | 4.08 + 0.00 | 4.07 + 0.00 |
| tier dictionary bytes | 24 | 24 |
| id values | 171332 | 522931 |
| id codes | 18.77 | 9.11 |
| id Pyramid lists + rare array | 0.00 + 32.00 | 0.00 + 32.00 |
| id dictionary bytes | 2055988 | 5121635 |
| doc store raw / compressed MB (ratio) | 197.4 / 123.9 (1.59x) | 47.7 / 32.2 (1.48x) |

### Range filters, trec-covid (171332 docs): median us over 5 windows

| field | target | actual % | scan docset | postings docset | auto | scan count | postings count | auto choice (docset/count) |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| year | 0.1% | 2.86 | 62.0 | 1.3 | 1.8 | 46.8 | 0.0 | Postings/Postings x5 |
| year | 1% | 2.93 | 56.3 | 2.2 | 3.0 | 43.0 | 0.0 | Postings/Postings x5 |
| year | 10% | 14.32 | 42.1 | 17.7 | 17.4 | 41.8 | 0.1 | Postings/Postings x5 |
| year | 50% | 54.03 | 40.7 | 88.9 | 44.0 | 41.6 | 0.2 | Scan/Postings x5 |
| rating | 0.1% | 0.19 | 49.9 | 19.1 | 19.8 | 48.9 | 21.5 | Postings/Postings x5 |
| rating | 1% | 1.21 | 52.4 | 20.7 | 41.6 | 45.4 | 43.0 | Scan/Scan x1, Postings/Postings x4 |
| rating | 10% | 10.20 | 39.4 | 52.1 | 39.8 | 42.3 | 47.2 | Scan/Scan x3, Postings/Postings x2 |
| rating | 50% | 50.19 | 40.1 | 125.3 | 40.9 | 43.4 | 46.5 | Scan/Scan x4, Scan/Postings x1 |
| seq | 0.1% | 0.10 | 2.0 | 24.7 | 3.7 | 0.2 | 23.2 | Scan/Scan x5 |
| seq | 1% | 1.00 | 8.4 | 50.6 | 7.9 | 0.4 | 43.0 | Scan/Scan x5 |
| seq | 10% | 10.00 | 1.5 | 67.8 | 2.3 | 1.1 | 67.3 | Scan/Scan x5 |
| seq | 50% | 50.00 | 2.0 | 41.4 | 2.4 | 1.7 | 36.9 | Scan/Scan x5 |

### Range filters, quora (522931 docs): median us over 5 windows

| field | target | actual % | scan docset | postings docset | auto | scan count | postings count | auto choice (docset/count) |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| year | 0.1% | 2.86 | 216.4 | 5.7 | 6.0 | 138.9 | 0.0 | Postings/Postings x5 |
| year | 1% | 2.89 | 221.4 | 4.9 | 5.5 | 129.1 | 0.0 | Postings/Postings x5 |
| year | 10% | 11.47 | 131.5 | 51.7 | 47.2 | 129.9 | 0.0 | Postings/Postings x5 |
| year | 50% | 51.45 | 133.9 | 187.4 | 121.6 | 126.4 | 0.2 | Scan/Postings x5 |
| rating | 0.1% | 0.39 | 159.7 | 79.4 | 81.0 | 159.9 | 77.7 | Postings/Postings x5 |
| rating | 1% | 1.20 | 175.7 | 242.6 | 241.6 | 140.2 | 144.2 | Postings/Postings x4, Scan/Scan x1 |
| rating | 10% | 10.18 | 121.9 | 287.2 | 140.4 | 131.4 | 175.7 | Scan/Scan x5 |
| rating | 50% | 50.20 | 127.5 | 435.3 | 132.1 | 134.9 | 163.5 | Scan/Scan x3, Scan/Postings x2 |
| seq | 0.1% | 0.10 | 7.5 | 56.3 | 7.7 | 0.6 | 55.2 | Scan/Scan x5 |
| seq | 1% | 1.00 | 19.0 | 113.4 | 18.7 | 0.7 | 108.9 | Scan/Scan x5 |
| seq | 10% | 10.00 | 3.3 | 113.0 | 3.5 | 1.5 | 107.5 | Scan/Scan x5 |
| seq | 50% | 50.00 | 5.9 | 119.1 | 6.7 | 4.3 | 107.5 | Scan/Scan x5 |

### Probe cost per candidate (ns; 100k candidates)

| filter | trec-covid sorted | trec-covid random | quora sorted | quora random |
|---|---:|---:|---:|---:|
| column year in [2000, 2009] | 9.3 | 9.5 | 7.4 | 8.6 |
| column rating in [1, 2] | 9.8 | 10.6 | 10.9 | 11.2 |
| pyramid category = c00 | 10.2 | 13.7 | 9.1 | 8.7 |
| pyramid category = c15 | 18.3 | 17.4 | 16.4 | 18.5 |
| pyramid topic = t00 | 13.9 | 24.1 | 17.8 | 21.5 |
| pyramid tier = a (Flat chunks) | 8.6 | 12.4 | 9.3 | 9.1 |
| codes tier = a | 6.5 | 6.4 | 5.8 | 6.8 |
| codes category in 4 values | 7.6 | 7.6 | 7.6 | 8.8 |
| docset bits year in [2000, 2009] | 1.3 | 1.3 | 1.6 | 1.3 |
| docset sorted category = c15 | 15.2 | 17.3 | 17.1 | 17.3 |

### Keyword equality and IN, trec-covid (us)

| field | op | docs | DocSet | filter open |
|---|---|---:|---:|---:|
| category | = c00 | 35874 | 1.2 | 0.1 |
| category | = c17 | 2089 | 0.7 | 0.1 |
| category | = c32 | 1096 | 0.5 | 0.1 |
| category | = c62 | 558 | 0.4 | 0.1 |
| category | IN 2 most frequent | 53929 | 3.7 |  |
| category | IN 2 least frequent | 1119 | 6.9 |  |
| category | IN 4 most frequent | 74660 | 13.8 |  |
| category | IN 4 least frequent | 2277 | 10.9 |  |
| category | IN 8 most frequent | 97730 | 50.8 |  |
| category | IN 8 least frequent | 4735 | 15.1 |  |
| category | IN 16 most frequent | 121972 | 72.3 |  |
| category | IN 16 least frequent | 10234 | 17.8 |  |
| topic | = t19 | 10405 | 4.2 | 0.1 |
| topic | = t07 | 6501 | 6.7 | 0.1 |
| topic | = t08 | 5335 | 1.1 | 0.1 |
| topic | = t20 | 2405 | 0.8 | 0.1 |
| topic | IN 2 most frequent | 18796 | 10.6 |  |
| topic | IN 2 least frequent | 5248 | 10.8 |  |
| topic | IN 4 most frequent | 34882 | 27.7 |  |
| topic | IN 4 least frequent | 11390 | 9.9 |  |
| topic | IN 8 most frequent | 62316 | 41.3 |  |
| topic | IN 8 least frequent | 25667 | 22.0 |  |
| topic | IN 16 most frequent | 109549 | 97.0 |  |
| topic | IN 16 least frequent | 61783 | 52.5 |  |
| tier | = a | 68249 | 1.2 | 0.1 |
| tier | = b | 51369 | 1.1 | 0.1 |
| tier | = c | 34398 | 1.1 | 0.1 |
| tier | = d | 17316 | 3.0 | 0.1 |
| tier | IN 2 most frequent | 119618 | 2.1 |  |
| tier | IN 2 least frequent | 51714 | 4.0 |  |
| tier | IN 4 most frequent | 171332 | 5.7 |  |
| tier | IN 4 least frequent | 171332 | 5.9 |  |

### Keyword equality and IN, quora (us)

| field | op | docs | DocSet | filter open |
|---|---|---:|---:|---:|
| category | = c00 | 110275 | 4.0 | 0.1 |
| category | = c16 | 6528 | 1.6 | 0.1 |
| category | = c32 | 3287 | 1.2 | 0.1 |
| category | = c62 | 1712 | 1.0 | 0.1 |
| category | IN 2 most frequent | 165390 | 5.8 |  |
| category | IN 2 least frequent | 3462 | 14.6 |  |
| category | IN 4 most frequent | 229498 | 44.3 |  |
| category | IN 4 least frequent | 7019 | 17.3 |  |
| category | IN 8 most frequent | 298856 | 91.0 |  |
| category | IN 8 least frequent | 14557 | 24.0 |  |
| category | IN 16 most frequent | 372346 | 150.2 |  |
| category | IN 16 least frequent | 31380 | 34.4 |  |
| topic | = t17 | 29490 | 24.6 | 0.1 |
| topic | = t16 | 19941 | 17.5 | 0.1 |
| topic | = t31 | 16957 | 16.6 | 0.1 |
| topic | = t23 | 8070 | 3.1 | 0.1 |
| topic | IN 2 most frequent | 53634 | 44.2 |  |
| topic | IN 2 least frequent | 16182 | 23.7 |  |
| topic | IN 4 most frequent | 100832 | 79.9 |  |
| topic | IN 4 least frequent | 35396 | 25.9 |  |
| topic | IN 8 most frequent | 184734 | 137.3 |  |
| topic | IN 8 least frequent | 79664 | 57.8 |  |
| topic | IN 16 most frequent | 334115 | 259.5 |  |
| topic | IN 16 least frequent | 188816 | 142.4 |  |
| tier | = a | 209330 | 3.9 | 0.1 |
| tier | = b | 156366 | 3.9 | 0.1 |
| tier | = c | 104662 | 3.9 | 0.1 |
| tier | = d | 52573 | 3.6 | 0.1 |
| tier | IN 2 most frequent | 365696 | 5.4 |  |
| tier | IN 2 least frequent | 157235 | 5.7 |  |
| tier | IN 4 most frequent | 522931 | 9.1 |  |
| tier | IN 4 least frequent | 522931 | 8.9 |  |

### Facet counts, trec-covid (us)

| field | result docs | representation | codes | postings | auto (choice) |
|---|---:|---|---:|---:|---|
| category (64) | 16 | sorted | 0.1 | 79.9 | 0.2 (Codes) |
| category (64) | 158 | sorted | 1.1 | 78.7 | 1.2 (Codes) |
| category (64) | 845 | sorted | 6.3 | 76.9 | 6.4 (Codes) |
| category (64) | 1797 | sorted | 13.7 | 80.9 | 13.2 (Codes) |
| category (64) | 3510 | sorted | 26.0 | 77.4 | 25.4 (Codes) |
| category (64) | 5175 | sorted | 38.8 | 82.9 | 37.8 (Codes) |
| category (64) | 8641 | bits | 46.0 | 77.0 | 52.2 (Codes) |
| category (64) | 17170 | bits | 38.8 | 86.3 | 41.0 (Codes) |
| category (64) | 34436 | bits | 54.9 | 75.2 | 57.8 (Codes) |
| category (64) | 68406 | bits | 89.9 | 81.9 | 74.6 (Postings) |
| category (64) | 119857 | bits | 129.5 | 76.0 | 75.7 (Postings) |
| category (64) | 171332 | bits | 137.1 | 75.7 | 72.2 (Postings) |
| topic (32) | 21 | sorted | 0.2 | 77.4 | 0.2 (Codes) |
| topic (32) | 161 | sorted | 1.2 | 85.3 | 1.3 (Codes) |
| topic (32) | 910 | sorted | 6.9 | 90.2 | 7.1 (Codes) |
| topic (32) | 1740 | sorted | 15.2 | 80.3 | 12.9 (Codes) |
| topic (32) | 3435 | sorted | 26.7 | 91.7 | 30.3 (Codes) |
| topic (32) | 5184 | sorted | 52.1 | 83.3 | 45.5 (Codes) |
| topic (32) | 8648 | bits | 78.7 | 89.9 | 49.6 (Codes) |
| topic (32) | 16983 | bits | 48.7 | 106.1 | 61.9 (Codes) |
| topic (32) | 34312 | bits | 72.5 | 71.6 | 57.2 (Codes) |
| topic (32) | 68258 | bits | 87.6 | 80.8 | 84.7 (Codes) |
| topic (32) | 119573 | bits | 122.4 | 74.5 | 79.0 (Postings) |
| topic (32) | 171332 | bits | 144.5 | 81.3 | 93.8 (Postings) |
| tier (4) | 20 | sorted | 0.2 | 4.7 | 0.2 (Codes) |
| tier (4) | 175 | sorted | 1.8 | 5.0 | 1.2 (Codes) |
| tier (4) | 904 | sorted | 6.7 | 6.8 | 5.4 (Postings) |
| tier (4) | 1752 | sorted | 11.2 | 4.7 | 4.8 (Postings) |
| tier (4) | 3407 | sorted | 20.8 | 5.9 | 5.7 (Postings) |
| tier (4) | 5169 | sorted | 30.6 | 7.2 | 7.2 (Postings) |
| tier (4) | 8496 | bits | 50.3 | 3.3 | 3.4 (Postings) |
| tier (4) | 17001 | bits | 37.9 | 3.4 | 3.5 (Postings) |
| tier (4) | 34366 | bits | 56.9 | 3.6 | 3.5 (Postings) |
| tier (4) | 68620 | bits | 88.9 | 3.3 | 3.4 (Postings) |
| tier (4) | 120214 | bits | 140.2 | 3.4 | 3.5 (Postings) |
| tier (4) | 171332 | bits | 160.5 | 3.8 | 3.4 (Postings) |

### Facet counts, quora (us)

| field | result docs | representation | codes | postings | auto (choice) |
|---|---:|---|---:|---:|---|
| category (64) | 38 | sorted | 0.3 | 217.0 | 0.3 (Codes) |
| category (64) | 493 | sorted | 3.3 | 205.3 | 3.6 (Codes) |
| category (64) | 2603 | sorted | 18.6 | 207.0 | 17.1 (Codes) |
| category (64) | 5128 | sorted | 35.9 | 211.1 | 35.1 (Codes) |
| category (64) | 10244 | sorted | 75.3 | 231.1 | 76.9 (Codes) |
| category (64) | 15445 | sorted | 122.4 | 244.9 | 118.3 (Codes) |
| category (64) | 26047 | bits | 208.5 | 220.4 | 178.0 (Codes) |
| category (64) | 52804 | bits | 153.3 | 225.9 | 170.3 (Codes) |
| category (64) | 103794 | bits | 186.7 | 200.8 | 196.1 (Codes) |
| category (64) | 209294 | bits | 257.6 | 223.8 | 210.5 (Postings) |
| category (64) | 365937 | bits | 374.4 | 201.8 | 205.0 (Postings) |
| category (64) | 522931 | bits | 375.1 | 219.9 | 220.1 (Postings) |
| topic (32) | 53 | sorted | 0.3 | 252.6 | 0.4 (Codes) |
| topic (32) | 562 | sorted | 3.5 | 273.5 | 3.9 (Codes) |
| topic (32) | 2606 | sorted | 19.1 | 284.1 | 18.1 (Codes) |
| topic (32) | 5212 | sorted | 34.2 | 261.8 | 34.2 (Codes) |
| topic (32) | 10544 | sorted | 71.8 | 285.4 | 77.0 (Codes) |
| topic (32) | 15492 | sorted | 109.4 | 259.8 | 116.8 (Codes) |
| topic (32) | 25950 | bits | 192.6 | 288.1 | 212.2 (Codes) |
| topic (32) | 52341 | bits | 194.2 | 303.8 | 171.7 (Codes) |
| topic (32) | 104578 | bits | 202.7 | 312.9 | 193.0 (Codes) |
| topic (32) | 209097 | bits | 271.2 | 295.9 | 294.7 (Codes) |
| topic (32) | 365560 | bits | 415.4 | 276.1 | 262.7 (Postings) |
| topic (32) | 522931 | bits | 446.7 | 277.0 | 268.9 (Postings) |
| tier (4) | 57 | sorted | 0.8 | 11.4 | 0.7 (Codes) |
| tier (4) | 563 | sorted | 6.1 | 7.8 | 3.2 (Codes) |
| tier (4) | 2680 | sorted | 16.8 | 10.9 | 9.5 (Postings) |
| tier (4) | 5096 | sorted | 28.7 | 11.1 | 11.3 (Postings) |
| tier (4) | 10431 | sorted | 63.5 | 17.4 | 17.0 (Postings) |
| tier (4) | 15699 | sorted | 96.6 | 20.1 | 20.7 (Postings) |
| tier (4) | 26252 | bits | 188.3 | 8.0 | 8.0 (Postings) |
| tier (4) | 52460 | bits | 144.4 | 8.5 | 8.5 (Postings) |
| tier (4) | 104508 | bits | 179.3 | 8.3 | 8.1 (Postings) |
| tier (4) | 208964 | bits | 252.8 | 7.7 | 7.8 (Postings) |
| tier (4) | 365865 | bits | 400.1 | 8.4 | 7.6 (Postings) |
| tier (4) | 522931 | bits | 458.8 | 6.1 | 6.1 (Postings) |

### Doc store: 10 random documents (us per query)

| collection | blocks | warm get | warm get(id, title) | cold (read-ahead) | pages | cold (MADV_RANDOM) | pages |
|---|---:|---:|---:|---:|---:|---:|---:|
| trec-covid | 11417 | 93.3 | 86.2 | 1904.3 | 384.8 | 2631.0 | 61.5 |
| quora | 2866 | 80.4 | 77.3 | 2406.1 | 374.3 | 2137.3 | 48.7 |

Build seconds: trec-covid: category 0.05, id 0.12, rating 0.01, seq 0.01, store 0.98, tier 0.03, topic 0.03, year 0.01; quora: category 0.07, id 0.34, rating 0.04, seq 0.04, store 0.23, tier 0.07, topic 0.06, year 0.03
Peak RSS MB: trec-covid 521, quora 366

## Open issues

* **Trust model for corrupt segments.** Readers check headers, array
  lengths and Pyramid descriptors (O(1) or O(chunks)), so a truncated or
  mismatched section is an error at open. Payload bits (Pyramid bitmaps,
  `U` directories, block offsets of individual blocks, LZ4 blocks) are not
  validated: corruption there panics on a bounds check or returns wrong
  results, and a corrupted Pyramid `U` or offsets directory can make the
  codec read past a chunk's lows. A deep check costs a pass over the data,
  which would defeat lazy, proportional I/O; segments need checksums
  (storage layer) or an optional deep verification.
* **Corruption panics instead of returning errors** where the given API
  returns plain values (`DocStore::get`, lazily parsed lists).
* **INT bounds beyond 2^53**: bounds are doubles, so `x >= 2^53 + 1` cannot
  be expressed exactly; an i64 bound API (`KeyRange` is there) may be
  needed by the engine.
* **NULL facet group**: `facet_counts` skips documents without a value
  (SQL `GROUP BY` would report a NULL group).
* **Boundary buckets** dominate range postings on continuous values (one
  random column access, ~9 ns, per member of two cut buckets). Finer
  histograms fix it at a size cost (above); an alternative is to scan only
  the blocks holding cut-bucket members, or per-bucket sorted keys.
* **Pyramid probes vs code probes**: the code column probe (6-9 ns) beats
  the Pyramid probe (8-26 ns) for per-candidate checks; the planner should
  use `filter_in` for keyword probes. Pyramid's `View::new` recomputes four
  mask popcounts per probe; caching the chunk view for sorted candidate
  streams would close part of the gap.
* **Cost-model constants** were fitted on a machine shared with four other
  jobs (load 6-10); refit on an idle machine. The facet model's 7% density
  threshold for "dense" lists is a proxy for the Flat / dense-block
  decision inside Pyramid.
* **Doc store** has no block cache: fetching several documents of one block
  decompresses it each time; the query engine should group fetches by
  block or a small per-reader cache should be added.
* **`id` as an indexed KEYWORD** costs ~19 (TREC-COVID) / 9 (Quora) bits per
  document of codes, 32 bits of postings and ~10-12 bytes of dictionary,
  duplicating the primary-key index; the schema could mark it unindexed.
* Superblock and block zone checks are scalar; a SIMD pass over the zone
  arrays could cut the 1 ns per check further, which only matters for
  selective scans of clustered columns.

## Changes outside `strata/src/forward`

* `posting_bench/src/codecs/pyramid/` (in scope): zero-copy arrays,
  serialisation, `or_into_bits`, `and_count_bits`, tests.
* `strata/Cargo.toml`: added `bitpacking = "0.9.3"` (already a dependency of
  posting_bench, so no new crate in the build); `strata/Cargo.lock`
  updated accordingly.
