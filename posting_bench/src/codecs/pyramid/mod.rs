//! Pyramid: a two-level partition of the docID space in which every
//! membership probe is O(1) and every set operation is block-local, at any
//! density.
//!
//! The docID space is cut into 2^16-docID *chunks* keyed by the high 16 bits
//! (as in Roaring), and each chunk into 256 *blocks* of 256 docIDs. Each
//! chunk takes the smallest of three forms (with a speed bias toward Flat):
//!
//! * **Small**: the sorted low 16 bits as `u16` (at most [`SMALL_MAX`]).
//!   Probed with one or two masked AVX-512 compares.
//! * **Tiered**: two 256-bit block masks and two leaf pools.
//!   * `present`: block holds at least one value.
//!   * `dense`: block is stored as a 256-bit bitmap (stored only when the
//!     chunk has a dense block). A block is dense when it holds at least
//!     [`DENSE_MIN`] values, i.e. when its bitmap is no larger than its
//!     sparse form (about 9 bits per value).
//!   * dense bitmaps, `[u64; 4]` each, in block order (index = rank in
//!     `dense`);
//!   * the *sparse* blocks (present, not dense) keep the low byte of each
//!     value, grouped by block, plus a directory locating the lows of sparse
//!     block `r` (its rank among the sparse blocks), whichever is smaller:
//!     - *unary* ("byte Elias-Fano"): a bit vector `U` with one bit per
//!       sparse value, set on the first value of each block, a terminating
//!       set bit, and a `u16` sample of every [`SAMPLE`]-th block start. The
//!       lows of block `r` lie between the `r`-th and `(r+1)`-th set bits of
//!       `U`, found from the sample with one `pdep` ([`simd::select2`]).
//!       Wins when blocks hold few values (about 1 + 2/8 bits per value).
//!     - *offsets*: `u16` prefix offsets, one 32-bit load per lookup. Wins
//!       when sparse blocks average more than about 14 values.
//! * **Flat**: the `present` mask plus all 256 block bitmaps (a 65536-bit
//!   bitmap), i.e. a Tiered chunk whose dense mask is implicitly all ones,
//!   so every generic path works unchanged while probes and unions can
//!   address bits directly. Chosen when it is below [`FLAT_BIAS_PCT`]% of
//!   the Tiered size.
//!
//! A probe `contains(x)` is: present bit, dense bit, then either one bitmap
//! bit test, or a rank, one directory lookup (`pdep` select or one load)
//! and one masked byte compare; on a Flat chunk, a single bit test.
//!
//! Memory layout: per list, the sorted keys, a descriptor per chunk, the
//! Small values, a *meta* arena holding, per Tiered chunk, the masks (one
//! 64-byte line) and the directory, and a *data* arena holding the dense
//! bitmaps (32-byte aligned) and the lows. Queries visit chunks in key
//! order, so descriptors, Small values and meta are read nearly
//! sequentially (and prefetched ahead); leaf probes are the random accesses.
//!
//! Set operations (details in `and.rs` and `or.rs`):
//! * **Intersection**: lists are ordered by cardinality and the chunk keys
//!   of the smallest are galloped in the others. In each common chunk the
//!   values of the smallest chunk are probed in the others (SvS within the
//!   chunk), the probes into the first probed list being located and
//!   prefetched one chunk ahead; when that chunk is large and its blocks
//!   dense, a *block join* runs instead over the AND of the `present`
//!   masks: blocks dense everywhere are 4-word ANDs, other blocks filter a
//!   sparse block's low bytes 32 at a time against the others (`vpermb` bit
//!   lookups for dense blocks, byte compares for sparse ones).
//! * **Union**: k-way merge of the chunk keys; a chunk held by one list is
//!   decoded directly. Dense groups are ORed into a 65536-bit accumulator
//!   (touching only present blocks) and extracted range by range; sparse
//!   groups are merged block by block: runs of blocks held by one input are
//!   decoded straight from it, blocks held by several are ORed into a
//!   256-bit accumulator.
//! * [`Pyramid::and_count`] / [`Pyramid::or_count`] run the same algorithms
//!   counting instead of writing docIDs (unions add chunk cardinalities and
//!   only inspect blocks held by several inputs).
//! * Decoding (and union runs) expand consecutive sparse blocks 16 values at
//!   a time ([`simd::expand_sparse`]: block of each value from a prefix
//!   popcount of `U`) and extract bitmaps with `vpcompressd` (`vpcompressb`
//!   where AVX512-VBMI2 exists).
//!
//! Space ([`PostingList::size_bits`]) counts, per list, a 32-bit chunk
//! count; per chunk a 64-bit header (16-bit key, 16-bit cardinality - 1,
//! 32-bit data offset whose top two bits are the chunk kind and directory
//! kind); per Small chunk 16 bits per value; per Tiered chunk 32 bits of
//! counts (dense blocks, sparse values), the masks, the bitmaps, the lows
//! and the directory; per Flat chunk the counts, `present` and 65536 bits,
//! all at their exact bit sizes. In memory the descriptors also cache
//! derived offsets, the arenas align chunks to cache lines, and a few words
//! of padding allow wide loads past the end; none of this is counted, as it
//! is recomputable from the counted fields.

mod and;
mod or;
pub mod simd;
#[cfg(all(
    test,
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512vl"
))]
mod timing;

use super::{Cursor, CursorList, PostingList, SetOps};
use crate::bytes::{ByteReader, ByteWriter, OwnedBytes, Pod, Slice};
use crate::codecs::roaring::simd::gallop;
use crate::END;
use simd::SLACK;

/// Largest Small chunk (stack buffers are sized by it).
pub const SMALL_MAX: usize = 48;
/// Small is chosen when its size is at most the Tiered size plus this many
/// bits (a speed bias toward the one-compare probe).
pub const SMALL_BIAS_BITS: u64 = 0;
/// Blocks with at least this many values are stored as bitmaps.
pub const DENSE_MIN: u32 = 29;
/// Every this many sparse blocks, the start of the next one is sampled.
pub const SAMPLE: u32 = 8;
/// Intersection drives from the values of the smallest chunk when it holds
/// at most this many; otherwise it joins block by block.
pub const DRIVE_MAX: usize = 1024;
/// ... but beyond 64 values only if they average at most this many per
/// present block.
pub const DRIVE_VPB: u32 = 8;
/// Chunks of look-ahead for metadata prefetching in intersections.
const PREFETCH: usize = 3;

const _: () = assert!(SMALL_MAX <= 64 && 64 <= DRIVE_MAX);
const _: () = assert!(DENSE_MIN <= 32);

/// Prefetches the cache line holding `p` into all levels.
#[inline(always)]
fn prefetch<T>(p: *const T) {
    #[cfg(all(target_arch = "x86_64", target_feature = "sse"))]
    // SAFETY: prefetching never faults.
    unsafe {
        std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(p as *const i8);
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "sse")))]
    let _ = p;
}

/// A 64-byte cache line of the arenas.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C, align(64))]
struct Line([u64; 8]);

/// A growable array of `u64` whose start is 64-byte aligned: the meta and
/// data arenas while building, then the owner of their words.
#[derive(Default)]
struct Arena {
    lines: Vec<Line>,
    len: usize,
}

impl Arena {
    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    fn words_mut(&mut self) -> &mut [u64] {
        // SAFETY: `lines` holds 8 * lines.len() initialised words.
        unsafe { std::slice::from_raw_parts_mut(self.lines.as_mut_ptr() as *mut u64, 8 * self.lines.len()) }
    }

    /// Grows to `n` words (new words are zero).
    fn grow(&mut self, n: usize) {
        debug_assert!(n >= self.len);
        self.lines.resize(n.div_ceil(8), Line::default());
        self.len = n;
    }

    /// Pads with zero words to a multiple of `align` words.
    fn align(&mut self, align: usize) {
        self.grow(self.len.next_multiple_of(align));
    }

    fn extend(&mut self, it: impl IntoIterator<Item = u64>) {
        for x in it {
            let n = self.len;
            self.grow(n + 1);
            self.words_mut()[n] = x;
        }
    }

    /// The words as a [`Slice`] viewing the arena itself. A built list so
    /// keeps its masks and bitmaps on cache lines, as a loaded one does in
    /// a mapped file: a `Vec<u64>` is only 16-byte aligned, and 64-byte
    /// loads of Flat bitmaps that split cache lines made count-only unions
    /// about 20% slower.
    fn into_slice(mut self) -> Slice<u64> {
        self.lines.shrink_to_fit();
        Slice::view(OwnedBytes::new(self)).expect("arena words are aligned")
    }
}

impl AsRef<[u8]> for Arena {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: the first `len` words of `lines` are initialised.
        unsafe { std::slice::from_raw_parts(self.lines.as_ptr() as *const u8, 8 * self.len) }
    }
}

/// Chunk descriptor. `card`, the kind and the data offset are the counted
/// 64-bit header (with the key); the other fields are derived offsets.
/// Plain integers in C layout, so that descriptors can be viewed in place.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
struct Chunk {
    /// Number of values (1..=65536).
    card: u32,
    /// Small: index of the first value in `small`. Tiered and Flat: word
    /// index of the `present` mask in `meta` (followed by `dense` if
    /// `nd > 0`, then the directory: samples and `U`, or offsets).
    off: u32,
    /// Tiered: word index of `U` in `meta` (after the offsets, unused, when
    /// the directory is offsets).
    u: u32,
    /// Tiered: word index of the first dense bitmap in `data`.
    bm: u32,
    /// Tiered: byte index of the lows in `data`.
    lows: u32,
    /// Tiered: number of dense blocks (Flat: present blocks).
    nd: u16,
    /// Tiered: number of sparse values.
    ns: u16,
    /// [`Chunk::SMALL`], [`Chunk::FLAT`], [`Chunk::OFFS`].
    flags: u32,
}

// SAFETY: `repr(C)` integers without padding (5 x 4 + 2 x 2 + 4 bytes).
unsafe impl Pod for Chunk {}
const _: () = assert!(std::mem::size_of::<Chunk>() == 28);

impl Chunk {
    /// A Small chunk.
    const SMALL: u32 = 1;
    /// Flat: a Tiered chunk whose dense mask is implicitly all ones (all 256
    /// block bitmaps stored, absent ones zero).
    const FLAT: u32 = 2;
    /// Tiered: the sparse directory is `u16` offsets (else unary `U`).
    const OFFS: u32 = 4;

    #[inline(always)]
    fn small(&self) -> bool {
        self.flags & Chunk::SMALL != 0
    }

    #[inline(always)]
    fn flat(&self) -> bool {
        self.flags & Chunk::FLAT != 0
    }

    #[inline(always)]
    fn offs(&self) -> bool {
        self.flags & Chunk::OFFS != 0
    }
}

/// A [`Slice`] with its pointer and length cached, so that reading it needs
/// no match on the variant (a probe reads four arrays; matching on each
/// made probes 10% slower).
struct Arr<T: Pod> {
    ptr: *const T,
    len: usize,
    /// Owns the elements; never mutated, so `ptr` stays valid (an owned
    /// `Vec`'s buffer does not move with it).
    own: Slice<T>,
}

// SAFETY: `ptr` points into `own`, which is immutable and Send + Sync.
unsafe impl<T: Pod> Send for Arr<T> {}
unsafe impl<T: Pod> Sync for Arr<T> {}

impl<T: Pod> From<Slice<T>> for Arr<T> {
    fn from(own: Slice<T>) -> Self {
        Arr { ptr: own.as_ptr(), len: own.len(), own }
    }
}

impl<T: Pod> std::ops::Deref for Arr<T> {
    type Target = [T];
    #[inline(always)]
    fn deref(&self) -> &[T] {
        // SAFETY: the elements of `own` (see the field).
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl<T: Pod> Clone for Arr<T> {
    fn clone(&self) -> Self {
        Arr::from(self.own.clone())
    }
}

impl<T: Pod> Default for Arr<T> {
    fn default() -> Self {
        Arr::from(Slice::default())
    }
}

impl<T: Pod + std::fmt::Debug> std::fmt::Debug for Arr<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.own.fmt(f)
    }
}

/// A Pyramid posting list. Its arrays are [`Slice`]s: owned when built (the
/// two arenas as views of their own cache-line aligned buffer), views into
/// shared bytes when loaded with [`Pyramid::from_bytes`].
#[derive(Clone, Debug, Default)]
pub struct Pyramid {
    keys: Arr<u16>,
    chunks: Arr<Chunk>,
    /// Values of Small chunks, plus 64 padding values.
    small: Arr<u16>,
    /// Masks and directories of Tiered chunks, plus two padding words.
    meta: Arr<u64>,
    /// Dense bitmaps and lows of Tiered chunks.
    data: Arr<u64>,
    card: u64,
    bits: u64,
}

/// A Pyramid list under construction.
#[derive(Default)]
struct Builder {
    keys: Vec<u16>,
    chunks: Vec<Chunk>,
    small: Vec<u16>,
    meta: Arena,
    data: Arena,
    card: u64,
    bits: u64,
}

/// Flat is chosen when its size is below this percentage of the Tiered size
/// (a speed bias: Flat probes and unions need no ranks).
pub const FLAT_BIAS_PCT: u64 = 160;

/// Bits of a Flat chunk's payload: counts, `present`, 256 bitmaps.
const FLAT_BITS: u64 = 32 + 256 + 65536;

/// Bits of the unary sparse directory (`U` plus samples) and of the
/// offsets directory (`u16` per sparse block; `offs[0] = 0` and
/// `offs[ms] = ns` are implied by the header) for `ns` values in `ms` blocks.
#[inline]
fn dir_bits(ns: u64, ms: u64) -> (u64, u64) {
    let samples = ms.saturating_sub(1) / SAMPLE as u64;
    ((ns + 1) + 16 * samples, 16 * ms.saturating_sub(1))
}

/// Bits of a Tiered chunk's payload (without the 64-bit chunk header), with
/// the smaller sparse directory.
#[inline]
fn tiered_bits(nd: u64, ns: u64, ms: u64) -> u64 {
    let (u, o) = dir_bits(ns, ms);
    32 + 256 + if nd > 0 { 256 + 256 * nd } else { 0 } + u.min(o) + 8 * ns
}

/// Sparse block ids of a chunk (see [`View::sparse_ids`]).
#[derive(Clone)]
struct SparseIds([u32; 1 + 256 + 48]);

impl SparseIds {
    #[inline(always)]
    fn as_ptr(&self) -> *const u32 {
        self.0.as_ptr()
    }
}

/// Per-chunk query state of a Tiered chunk: masks, per-word cumulative
/// ranks and pointers to the pools.
#[derive(Clone, Copy)]
struct View {
    present: [u64; 4],
    dense: [u64; 4],
    /// Dense blocks before word `w` of the masks.
    dcum: [u32; 4],
    /// Sparse blocks before word `w`.
    scum: [u32; 4],
    bitmaps: *const u64,
    u: *const u64,
    samples: *const u16,
    lows: *const u8,
    /// Number of sparse blocks and of sparse values.
    ms: u32,
    ns: u32,
    /// A Flat chunk: bit `v` of `bitmaps` is value `v` (absent blocks are
    /// zero), so a probe needs no mask or rank.
    flat: bool,
    /// The sparse directory is `ms + 1` `u16` prefix offsets at `samples`
    /// (else `U` and its samples).
    offs: bool,
}

impl View {
    const EMPTY: View = View {
        present: [0; 4],
        dense: [0; 4],
        dcum: [0; 4],
        scum: [0; 4],
        bitmaps: std::ptr::null(),
        u: std::ptr::null(),
        samples: std::ptr::null(),
        lows: std::ptr::null(),
        ms: 0,
        ns: 0,
        flat: false,
        offs: false,
    };

    /// # Safety
    /// `c` must be a Tiered chunk of `p`.
    #[inline(always)]
    unsafe fn new(p: &Pyramid, c: &Chunk) -> View {
        debug_assert!(!c.small());
        let meta = p.meta.as_ptr();
        let data = p.data.as_ptr();
        let w = meta.add(c.off as usize);
        let present = [*w, *w.add(1), *w.add(2), *w.add(3)];
        let dense = if c.flat() {
            [!0; 4]
        } else if c.nd > 0 {
            [*w.add(4), *w.add(5), *w.add(6), *w.add(7)]
        } else {
            [0; 4]
        };
        let mut dcum = [0u32; 4];
        let mut scum = [0u32; 4];
        for i in 1..4 {
            dcum[i] = dcum[i - 1] + dense[i - 1].count_ones();
            scum[i] = scum[i - 1] + (present[i - 1] & !dense[i - 1]).count_ones();
        }
        View {
            present,
            dense,
            dcum,
            scum,
            bitmaps: data.add(c.bm as usize),
            u: meta.add(c.u as usize),
            samples: w.add(if c.nd > 0 { 8 } else { 4 }) as *const u16,
            lows: (data as *const u8).add(c.lows as usize),
            ms: scum[3] + (present[3] & !dense[3]).count_ones(),
            ns: c.ns as u32,
            flat: c.flat(),
            offs: c.offs(),
        }
    }

    /// The bitmap of the dense block `64 w + tz(bit)`.
    #[inline(always)]
    unsafe fn bitmap(&self, w: usize, bit: u64) -> *const u64 {
        let d = self.dcum[w] + (self.dense[w] & (bit - 1)).count_ones();
        self.bitmaps.add(4 * d as usize)
    }

    /// Rank among the sparse blocks of the sparse block `64 w + tz(bit)`.
    #[inline(always)]
    fn sparse_rank(&self, w: usize, bit: u64) -> u32 {
        self.scum[w] + (self.present[w] & !self.dense[w] & (bit - 1)).count_ones()
    }

    /// `[start, end)` of the lows of sparse block `r`.
    #[inline(always)]
    unsafe fn sparse_range(&self, r: u32) -> (u32, u32) {
        if self.offs {
            let x = (self.samples.add(r as usize) as *const u32).read_unaligned();
            return (x & 0xFFFF, x >> 16);
        }
        sparse_range(self.u, self.samples, r)
    }

    /// End of the lows of sparse block `r`, which start at `so` (for
    /// sequential walks: one load, or a scan of `U`).
    #[inline(always)]
    unsafe fn sparse_end(&self, r: u32, so: u32) -> u32 {
        if self.offs {
            *self.samples.add(r as usize + 1) as u32
        } else {
            next_one(self.u, so + 1)
        }
    }

    /// Start of the lows of sparse block `r` (`ns` for `r == ms`).
    #[inline(always)]
    unsafe fn sparse_start(&self, r: u32) -> u32 {
        if self.offs {
            *self.samples.add(r as usize) as u32
        } else if r < self.ms {
            self.sparse_range(r).0
        } else {
            self.ns
        }
    }

    /// The block ids of the sparse blocks, in order, at index `1..=ms`
    /// (index 0 and the tail are padding for [`simd::expand_sparse`]).
    #[inline(always)]
    unsafe fn sparse_ids(&self) -> SparseIds {
        let mut ids = SparseIds([0; 1 + 256 + 48]);
        let s = [
            self.present[0] & !self.dense[0],
            self.present[1] & !self.dense[1],
            self.present[2] & !self.dense[2],
            self.present[3] & !self.dense[3],
        ];
        if s != [0; 4] {
            simd::extract256(&s, 0, ids.0.as_mut_ptr().add(1));
        }
        ids
    }

    /// Writes the values of the present blocks `run` of mask word `w`, which
    /// must be consecutive among the chunk's present blocks, at `dst`;
    /// returns how many. Consecutive dense blocks are extracted one by one;
    /// consecutive sparse blocks are decoded together by
    /// [`simd::expand_sparse`]. `ids` points at index 1 of
    /// [`View::sparse_ids`].
    ///
    /// # Safety
    /// `dst` has room for the values plus [`SLACK`].
    #[inline(always)]
    unsafe fn decode_run(&self, w: usize, run: u64, ids: *const u32, base: u32, dst: *mut u32) -> usize {
        debug_assert!(run != 0 && run & !self.present[w] == 0);
        let dw = self.dense[w];
        let first = run & run.wrapping_neg();
        let mut sr = self.sparse_rank(w, first);
        let mut so = self.sparse_start(sr);
        let mut k = 0usize;
        let mut m = run;
        while m != 0 {
            let bit = m & m.wrapping_neg();
            if dw & bit != 0 {
                // Dense blocks up to the next sparse one.
                let sp = m & !dw;
                let mut d = m & if sp == 0 { !0 } else { (sp & sp.wrapping_neg()) - 1 };
                m &= !d;
                while d != 0 {
                    let b = (w as u32) << 6 | d.trailing_zeros();
                    // By rank: Flat chunks also count absent blocks.
                    let bm = self.bitmap(w, d & d.wrapping_neg());
                    d &= d - 1;
                    k += simd::extract256(&*(bm as *const [u64; 4]), base | b << 8, dst.add(k));
                }
            } else {
                // Sparse blocks up to the next dense one.
                let dn = m & dw;
                let sb = m & if dn == 0 { !0 } else { (dn & dn.wrapping_neg()) - 1 };
                m &= !sb;
                let nb = sb.count_ones();
                if self.offs {
                    // Large sparse blocks: widen each.
                    let mut x = sb;
                    while x != 0 {
                        let b = (w as u32) << 6 | x.trailing_zeros();
                        x &= x - 1;
                        let (s, e) = self.sparse_range(sr);
                        simd::widen_bytes(self.lows.add(s as usize), (e - s) as usize, base | b << 8, dst.add(k));
                        k += (e - s) as usize;
                        sr += 1;
                    }
                    so = self.sparse_start(sr);
                    continue;
                }
                let e = self.sparse_start(sr + nb);
                simd::expand_sparse(self.lows.add(so as usize), self.u, so, (e - so) as usize, ids.add(sr as usize), base, dst.add(k));
                k += (e - so) as usize;
                sr += nb;
                so = e;
            }
        }
        k
    }

    /// Whether the chunk holds the low 16 bits `v`.
    #[inline(always)]
    unsafe fn contains(&self, v: u16) -> bool {
        if self.flat {
            return (*self.bitmaps.add((v >> 6) as usize) >> (v & 63)) & 1 != 0;
        }
        let b = (v >> 8) as u32;
        let l = v as u8;
        let w = (b >> 6) as usize;
        let bit = 1u64 << (b & 63);
        let p = *self.present.get_unchecked(w);
        if p & bit == 0 {
            return false;
        }
        if *self.dense.get_unchecked(w) & bit != 0 {
            let bm = self.bitmap(w, bit);
            return (*bm.add((l >> 6) as usize) >> (l & 63)) & 1 != 0;
        }
        let (s, e) = self.sparse_range(self.sparse_rank(w, bit));
        simd::bytes_contain(self.lows.add(s as usize), (e - s) as usize, l)
    }
}

/// `[start, end)` of sparse block `r`: the `r`-th and `(r+1)`-th set bits of
/// `U`, starting from the sample of block `32 * (r / 32)`.
///
/// # Safety
/// `r` must be a sparse block of the chunk whose `U` and samples are given.
#[inline(always)]
unsafe fn sparse_range(u: *const u64, samples: *const u16, r: u32) -> (u32, u32) {
    let s = r / SAMPLE;
    let pos = if s == 0 { 0 } else { *samples.add(s as usize - 1) as u32 };
    select_from(u, pos, r % SAMPLE)
}

/// Positions of the `k`-th and `(k+1)`-th set bits of `U` counting from
/// `pos` (whose own bit is the 0-th).
///
/// # Safety
/// Both bits must exist (`U` ends with a terminator bit).
#[inline(always)]
unsafe fn select_from(u: *const u64, pos: u32, k: u32) -> (u32, u32) {
    // 128 bits of U from `pos` in two words (the two words after U are
    // always readable; bits past the terminator are never selected).
    let wi = (pos >> 6) as usize;
    let sh = pos & 63;
    let (a0, a1, a2) = (*u.add(wi), *u.add(wi + 1), *u.add(wi + 2));
    let w0 = (a0 >> sh) | ((a1 << 1) << (63 - sh));
    let c0 = w0.count_ones();
    if c0 >= k + 2 {
        let (a, b) = simd::select2(w0, k);
        return (pos + a, pos + b);
    }
    let w1 = (a1 >> sh) | ((a2 << 1) << (63 - sh));
    if c0 == k + 1 {
        if w1 != 0 {
            return (pos + simd::select1(w0, k), pos + 64 + w1.trailing_zeros());
        }
    } else if w1.count_ones() >= k - c0 + 2 {
        let (a, b) = simd::select2(w1, k - c0);
        return (pos + 64 + a, pos + 64 + b);
    }
    sparse_range_slow(u, pos, k)
}

#[cold]
#[inline(never)]
unsafe fn sparse_range_slow(u: *const u64, pos: u32, k: u32) -> (u32, u32) {
    // Skip k set bits from `pos` (whose own bit is set), then find two more.
    let mut wi = (pos >> 6) as usize;
    let mut w = *u.add(wi) & (!0u64 << (pos & 63));
    let mut skip = k;
    loop {
        let c = w.count_ones();
        if c > skip {
            break;
        }
        skip -= c;
        wi += 1;
        w = *u.add(wi);
    }
    let start = (wi as u32) << 6 | simd::select1(w, skip);
    (start, next_one(u, start + 1))
}

/// First set bit of `U` at or after `pos` (the terminator guarantees one).
#[inline(always)]
unsafe fn next_one(u: *const u64, pos: u32) -> u32 {
    let mut wi = (pos >> 6) as usize;
    let mut w = *u.add(wi) & (!0u64 << (pos & 63));
    while w == 0 {
        wi += 1;
        w = *u.add(wi);
    }
    (wi as u32) << 6 | w.trailing_zeros()
}

/// First set bit of a 256-bit mask at or after `b` (`None` past the end).
#[inline(always)]
fn first_at_or_after(mask: &[u64; 4], b: u32) -> Option<u32> {
    if b >= 256 {
        return None;
    }
    let mut w = (b >> 6) as usize;
    let mut m = mask[w] & (!0u64 << (b & 63));
    loop {
        if m != 0 {
            return Some((w as u32) << 6 | m.trailing_zeros());
        }
        w += 1;
        if w == 4 {
            return None;
        }
        m = mask[w];
    }
}

// ------------------------------------------------------------- building

impl Pyramid {
    /// Number of docIDs.
    #[inline]
    pub fn cardinality(&self) -> u64 {
        self.card
    }

    /// Number of Small, Tiered and Flat chunks.
    pub fn chunk_counts(&self) -> [usize; 3] {
        let s = self.chunks.iter().filter(|c| c.small()).count();
        let f = self.chunks.iter().filter(|c| c.flat()).count();
        [s, self.chunks.len() - s - f, f]
    }

    /// Number of dense and sparse blocks over all Tiered chunks.
    pub fn block_counts(&self) -> [usize; 2] {
        let mut n = [0; 2];
        for c in self.chunks.iter().filter(|c| !c.small()) {
            // SAFETY: Tiered chunk of self.
            let v = unsafe { View::new(self, c) };
            n[0] += c.nd as usize;
            n[1] += v.ms as usize;
        }
        n
    }
}

impl Builder {
    fn push_chunk(&mut self, key: u16, vals: &[u32]) {
        let n = vals.len();
        let mut counts = [0u32; 256];
        for &v in vals {
            counts[((v >> 8) & 255) as usize] += 1;
        }
        let (mut nd, mut ns, mut ms) = (0u64, 0u64, 0u64);
        for &c in &counts {
            if c >= DENSE_MIN {
                nd += 1;
            } else if c > 0 {
                ms += 1;
                ns += c as u64;
            }
        }
        let tiered = tiered_bits(nd, ns, ms);
        let (u_dir, o_dir) = dir_bits(ns, ms);
        let offs = o_dir < u_dir;
        let small = 16 * n as u64;
        self.keys.push(key);
        self.card += n as u64;
        if n <= SMALL_MAX && small <= tiered + SMALL_BIAS_BITS {
            self.chunks.push(Chunk { card: n as u32, off: self.small.len() as u32, flags: Chunk::SMALL, ..Chunk::default() });
            self.small.extend(vals.iter().map(|&v| v as u16));
            self.bits += 64 + small;
            return;
        }
        if FLAT_BITS * 100 < tiered * FLAT_BIAS_PCT {
            self.push_flat(n, &counts, vals);
            return;
        }
        self.bits += 64 + tiered;

        let mut present = [0u64; 4];
        let mut dense = [0u64; 4];
        for (b, &c) in counts.iter().enumerate() {
            if c > 0 {
                present[b >> 6] |= 1 << (b & 63);
            }
            if c >= DENSE_MIN {
                dense[b >> 6] |= 1 << (b & 63);
            }
        }
        // Sparse lows, block starts and samples.
        let mut starts = Vec::with_capacity(ms as usize);
        let mut lows = Vec::with_capacity(ns as usize);
        let mut i = 0;
        for &c in &counts {
            let c = c as usize;
            if c > 0 && (c as u32) < DENSE_MIN {
                starts.push(lows.len() as u16);
                lows.extend(vals[i..i + c].iter().map(|&v| v as u8));
            }
            i += c;
        }
        let samples: Vec<u16> = starts.iter().copied().skip(SAMPLE as usize).step_by(SAMPLE as usize).collect();
        debug_assert_eq!(samples.len() as u64, ms.saturating_sub(1) / SAMPLE as u64);

        // Meta: masks on their own cache line, samples, U.
        let meta = &mut self.meta;
        meta.align(8);
        let off = meta.len();
        meta.extend(present);
        if nd > 0 {
            meta.extend(dense);
        }
        let pack = |s: &[u16]| s.iter().enumerate().fold(0u64, |w, (j, &x)| w | (x as u64) << (16 * j));
        let u = if offs {
            // Prefix offsets: the block starts and the end.
            let mut o = starts.clone();
            o.push(ns as u16);
            meta.extend(o.chunks(4).map(pack));
            meta.len()
        } else {
            meta.extend(samples.chunks(4).map(pack));
            let u = meta.len();
            meta.grow(u + (ns as usize + 1).div_ceil(64));
            let words = meta.words_mut();
            for &s in &starts {
                words[u + s as usize / 64] |= 1 << (s % 64);
            }
            words[u + ns as usize / 64] |= 1 << (ns % 64);
            u
        };

        // Data: dense bitmaps (32-byte aligned) in block order, then lows.
        let data = &mut self.data;
        data.align(4);
        let bm = data.len();
        data.grow(bm + 4 * nd as usize);
        let words = data.words_mut();
        let (mut d, mut i) = (0, 0);
        for &c in &counts {
            let c = c as usize;
            if c as u32 >= DENSE_MIN {
                for &v in &vals[i..i + c] {
                    let l = v & 255;
                    words[bm + 4 * d + (l >> 6) as usize] |= 1 << (l & 63);
                }
                d += 1;
            }
            i += c;
        }
        let lo = data.len();
        data.extend(lows.chunks(8).map(|s| s.iter().enumerate().fold(0u64, |w, (j, &x)| w | (x as u64) << (8 * j))));
        self.chunks.push(Chunk {
            card: n as u32,
            off: off as u32,
            u: u as u32,
            bm: bm as u32,
            lows: 8 * lo as u32,
            nd: nd as u16,
            ns: ns as u16,
            flags: if offs { Chunk::OFFS } else { 0 },
        });
    }

    /// A Flat chunk: the `present` mask and a 65536-bit bitmap.
    fn push_flat(&mut self, n: usize, counts: &[u32; 256], vals: &[u32]) {
        self.bits += 64 + FLAT_BITS;
        let mut present = [0u64; 4];
        for (b, &c) in counts.iter().enumerate() {
            if c > 0 {
                present[b >> 6] |= 1 << (b & 63);
            }
        }
        let meta = &mut self.meta;
        meta.align(8);
        let off = meta.len();
        meta.extend(present);
        let data = &mut self.data;
        data.align(8);
        let bm = data.len();
        data.grow(bm + 1024);
        let words = data.words_mut();
        for &v in vals {
            let l = v & 0xFFFF;
            words[bm + (l >> 6) as usize] |= 1 << (l & 63);
        }
        let nd: u32 = present.iter().map(|w| w.count_ones()).sum();
        self.chunks.push(Chunk {
            card: n as u32,
            off: off as u32,
            u: off as u32,
            bm: bm as u32,
            lows: 8 * data.len() as u32,
            nd: nd as u16,
            ns: 0,
            flags: Chunk::FLAT,
        });
    }

    fn finish(mut self) -> Pyramid {
        // Padding: `U` windows may read two words past the last chunk, and
        // intersections copy 64 values from a Small chunk's start.
        self.meta.extend([0, 0]);
        self.small.extend([0; 64]);
        self.keys.shrink_to_fit();
        self.chunks.shrink_to_fit();
        self.small.shrink_to_fit();
        Pyramid {
            keys: Slice::Owned(self.keys).into(),
            chunks: Slice::Owned(self.chunks).into(),
            small: Slice::Owned(self.small).into(),
            meta: self.meta.into_slice().into(),
            data: self.data.into_slice().into(),
            card: self.card,
            bits: self.bits,
        }
    }
}

impl Pyramid {
    #[inline(always)]
    fn small_vals(&self, c: &Chunk) -> &[u16] {
        debug_assert!(c.small());
        // SAFETY: Small chunks index valid ranges of `small`.
        unsafe { self.small.get_unchecked(c.off as usize..(c.off + c.card) as usize) }
    }

    /// Prefetches the descriptor-adjacent metadata of chunk `ci`.
    #[inline(always)]
    fn prefetch_chunk(&self, ci: usize) {
        // SAFETY: ci indexes a chunk; offsets stay inside the arenas.
        unsafe {
            let c = self.chunks.get_unchecked(ci);
            if c.small() {
                prefetch(self.small.as_ptr().add(c.off as usize));
            } else {
                prefetch(self.meta.as_ptr().add(c.off as usize));
                prefetch(self.meta.as_ptr().add(c.u as usize));
            }
        }
    }

    /// Whether `x` is in the list (chunk binary search, then an O(1) probe).
    pub fn contains(&self, x: u32) -> bool {
        let Ok(ci) = self.keys.binary_search(&((x >> 16) as u16)) else {
            return false;
        };
        let c = &self.chunks[ci];
        if c.small() {
            let v = self.small_vals(c);
            // SAFETY: `v` is a valid slice of at most SMALL_MAX values.
            unsafe { simd::u16s_contain(v.as_ptr(), v.len(), x as u16) }
        } else {
            // SAFETY: Tiered chunk of self.
            unsafe { View::new(self, c).contains(x as u16) }
        }
    }

    /// Appends the values of chunk `c` (`base` = key << 16) to `out`.
    #[inline]
    fn decode_chunk(&self, c: &Chunk, base: u32, out: &mut Vec<u32>) {
        if c.small() {
            out.extend(self.small_vals(c).iter().map(|&v| base | v as u32));
            return;
        }
        out.reserve(c.card as usize + SLACK);
        // SAFETY: Tiered chunk of self; `card + SLACK` slots are reserved.
        unsafe {
            let len = out.len();
            let n = self.decode_tiered(c, base, out.as_mut_ptr().add(len));
            out.set_len(len + n);
        }
    }

    /// Writes the values of the Tiered chunk `c` at `dst`; returns `c.card`.
    ///
    /// # Safety
    /// `c` is a Tiered chunk of `self`; `dst` has room for `c.card + SLACK`
    /// values (the kernels write at most SLACK past the chunk's values).
    #[inline]
    unsafe fn decode_tiered(&self, c: &Chunk, base: u32, dst: *mut u32) -> usize {
        let v = View::new(self, c);
        let ids = v.sparse_ids();
        let mut k = 0usize;
        for w in 0..4 {
            if v.present[w] != 0 {
                k += v.decode_run(w, v.present[w], ids.as_ptr().add(1), base, dst.add(k));
            }
        }
        debug_assert_eq!(k, c.card as usize);
        k
    }

    /// Cardinality of `lists[0] ∩ lists[1] ∩ ...` (0 for no lists).
    pub fn and_count(lists: &[&Pyramid]) -> u64 {
        match lists {
            [] => 0,
            [l] => l.card,
            _ => {
                let mut sink = CountSink(0);
                and::and_lists(lists, &mut sink);
                sink.0
            }
        }
    }

    /// Cardinality of `lists[0] ∪ lists[1] ∪ ...` (0 for no lists).
    pub fn or_count(lists: &[&Pyramid]) -> u64 {
        match lists {
            [] => 0,
            [l] => l.card,
            _ => {
                let mut sink = CountSink(0);
                or::or_lists(lists, &mut sink);
                sink.0
            }
        }
    }

    /// Sets the bit of every docID of the list in `words`, a bitset over
    /// the docID space (bit `d % 64` of word `d / 64`) that must cover the
    /// largest docID. Flat chunks and dense blocks are ORed word by word;
    /// chunks of sparse blocks only are decoded with the SIMD kernels first
    /// (per-block directory lookups cost more than the values when blocks
    /// hold a few values each).
    pub fn or_into_bits(&self, words: &mut [u64]) {
        let mut buf = Vec::new();
        for (&k, c) in self.keys.iter().zip(self.chunks.iter()) {
            let base = (k as usize) << 10;
            if c.small() {
                for &v in self.small_vals(c) {
                    words[base + (v >> 6) as usize] |= 1 << (v & 63);
                }
                continue;
            }
            if c.nd == 0 {
                buf.clear();
                self.decode_chunk(c, (k as u32) << 16, &mut buf);
                let last = buf.last().map_or(0, |&x| x as usize >> 6);
                assert!(last < words.len(), "bitset too short for docID {}", buf[buf.len() - 1]);
                for &x in &buf {
                    // SAFETY: x <= the last (largest) value, checked above.
                    unsafe { *words.get_unchecked_mut(x as usize >> 6) |= 1 << (x & 63) };
                }
                continue;
            }
            // SAFETY: Tiered chunk of self.
            let v = unsafe { View::new(self, c) };
            if c.flat() {
                // A partial last chunk: the bitmap is zero past the bitset.
                let n = 1024.min(words.len().saturating_sub(base));
                // SAFETY: a Flat chunk holds 1024 bitmap words.
                let bm = unsafe { std::slice::from_raw_parts(v.bitmaps, n) };
                for (w, &b) in words[base..base + n].iter_mut().zip(bm) {
                    *w |= b;
                }
                continue;
            }
            let (mut d, mut so, mut sr) = (0, 0, 0);
            for w in 0..4 {
                let mut m = v.present[w];
                while m != 0 {
                    let b = w << 6 | m.trailing_zeros() as usize;
                    let bit = m & m.wrapping_neg();
                    m &= m - 1;
                    let dst = base + 4 * b;
                    if v.dense[w] & bit != 0 {
                        // SAFETY: dense block `d` of the chunk.
                        let bm = unsafe { std::slice::from_raw_parts(v.bitmaps.add(4 * d), 4) };
                        // The bitset may end inside the last block.
                        let len = words.len();
                        for (x, &y) in words[dst.min(len)..(dst + 4).min(len)].iter_mut().zip(bm) {
                            *x |= y;
                        }
                        d += 1;
                    } else {
                        // SAFETY: sparse block `sr`, whose lows start at `so`.
                        let e = unsafe { v.sparse_end(sr, so) };
                        for i in so..e {
                            // SAFETY: lows of the sparse block.
                            let l = unsafe { *v.lows.add(i as usize) };
                            words[dst + (l >> 6) as usize] |= 1 << (l & 63);
                        }
                        so = e;
                        sr += 1;
                    }
                }
            }
        }
    }

    /// Number of docIDs of the list whose bit is set in `words` (a bitset as
    /// for [`Pyramid::or_into_bits`]; docIDs past its end count as absent):
    /// a count-only intersection with a bitset. Flat chunks and dense
    /// blocks AND and count words, sparse blocks test up to 32 low bytes at
    /// once ([`simd::test_bits`]), chunks of sparse blocks only are decoded
    /// first.
    pub fn and_count_bits(&self, words: &[u64]) -> u64 {
        let limit = words.len() << 6;
        let test = |x: usize| words.get(x >> 6).is_some_and(|w| w >> (x & 63) & 1 != 0) as u64;
        let mut buf = Vec::new();
        let mut count = 0u64;
        for (&k, c) in self.keys.iter().zip(self.chunks.iter()) {
            let base = (k as usize) << 10;
            if base >= words.len() {
                break;
            }
            if c.small() {
                count += self.small_vals(c).iter().map(|&v| test(base << 6 | v as usize)).sum::<u64>();
                continue;
            }
            if c.nd == 0 {
                buf.clear();
                self.decode_chunk(c, (k as u32) << 16, &mut buf);
                let end = buf.partition_point(|&x| (x as usize) < limit);
                for &x in &buf[..end] {
                    // SAFETY: x < limit = 64 * words.len().
                    count += unsafe { *words.get_unchecked(x as usize >> 6) } >> (x & 63) & 1;
                }
                continue;
            }
            // SAFETY: Tiered chunk of self.
            let v = unsafe { View::new(self, c) };
            if c.flat() {
                let n = 1024.min(words.len() - base);
                // SAFETY: a Flat chunk holds 1024 bitmap words.
                let bm = unsafe { std::slice::from_raw_parts(v.bitmaps, n) };
                count += words[base..base + n].iter().zip(bm).map(|(&x, &y)| (x & y).count_ones() as u64).sum::<u64>();
                continue;
            }
            let (mut d, mut so, mut sr) = (0, 0, 0);
            for w in 0..4 {
                let mut m = v.present[w];
                while m != 0 {
                    let b = w << 6 | m.trailing_zeros() as usize;
                    let bit = m & m.wrapping_neg();
                    m &= m - 1;
                    let dst = base + 4 * b;
                    let inside = dst + 4 <= words.len();
                    if v.dense[w] & bit != 0 {
                        for i in 0..4 {
                            // SAFETY: dense block `d` of the chunk.
                            let y = unsafe { *v.bitmaps.add(4 * d + i) };
                            count += (words.get(dst + i).copied().unwrap_or(0) & y).count_ones() as u64;
                        }
                        d += 1;
                    } else {
                        // SAFETY: sparse block `sr`, whose lows start at `so`.
                        let e = unsafe { v.sparse_end(sr, so) };
                        let n = ((e - so) as usize).min(32);
                        if inside {
                            // SAFETY: the block's lows, and its 4 words of the bitset.
                            let hits = unsafe {
                                let lows = simd::load_bytes32(v.lows.add(so as usize), n);
                                simd::test_bits(&lows, words.as_ptr().add(dst))
                            };
                            count += (hits & simd::low_mask32(n)).count_ones() as u64;
                        } else {
                            for i in so..so + n as u32 {
                                // SAFETY: lows of the sparse block.
                                let l = unsafe { *v.lows.add(i as usize) } as usize;
                                count += test((base << 6) + (b << 8) + l);
                            }
                        }
                        so = e;
                        sr += 1;
                    }
                }
            }
        }
        count
    }
}

// --------------------------------------------------------- serialisation

/// Tag of the serialised form: "PYRAMID" and a format version.
const MAGIC: u64 = u64::from_le_bytes(*b"PYRAMID\x01");

/// Pads with zero words so that the next [`ByteWriter::array`]'s elements
/// start on a 64-byte boundary of the buffer (masks and bitmaps stay on
/// cache lines when the buffer is line-aligned, e.g. in a mapped file).
fn align_line(w: &mut ByteWriter) {
    w.pad();
    while (w.buf.len() + 8) % 64 != 0 {
        w.u64(0);
    }
}

/// Skips the padding written by [`align_line`].
fn skip_line(r: &mut ByteReader) -> Result<(), String> {
    while (r.position() + 8) % 64 != 0 {
        r.u64()?;
    }
    Ok(())
}

impl Pyramid {
    /// Appends the list to `w`: a tag, the counts, and every array
    /// (`meta` and `data` 64-byte aligned relative to the buffer start).
    /// Read back with [`Pyramid::read_from`].
    pub fn write_to(&self, w: &mut ByteWriter) {
        w.u64(MAGIC);
        w.u64(self.card);
        w.u64(self.bits);
        w.array(&self.keys);
        w.array(&self.chunks);
        w.array(&self.small);
        align_line(w);
        w.array(&self.meta);
        align_line(w);
        w.array(&self.data);
    }

    /// The serialised list (see [`Pyramid::write_to`]).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = ByteWriter::new();
        self.write_to(&mut w);
        w.finish()
    }

    /// Reads a list written by [`Pyramid::write_to`] as zero-copy views into
    /// the reader's bytes. Checks the descriptors (O(chunks)): every offset
    /// they hold is in bounds, so probes and set operations stay inside the
    /// arrays; the payload bits themselves are trusted.
    pub fn read_from(r: &mut ByteReader) -> Result<Pyramid, String> {
        if r.u64()? != MAGIC {
            return Err("not a Pyramid list".into());
        }
        let card = r.u64()?;
        let bits = r.u64()?;
        let keys = r.array()?.into();
        let chunks = r.array()?.into();
        let small = r.array()?.into();
        skip_line(r)?;
        let meta = r.array()?.into();
        skip_line(r)?;
        let data = r.array()?.into();
        let p = Pyramid { keys, chunks, small, meta, data, card, bits };
        p.validate()?;
        Ok(p)
    }

    /// [`Pyramid::read_from`] over `bytes`.
    pub fn from_bytes(bytes: OwnedBytes) -> Result<Pyramid, String> {
        Pyramid::read_from(&mut ByteReader::new(bytes))
    }

    /// Whether every array is a view into shared bytes (a loaded list).
    pub fn is_view(&self) -> bool {
        fn shared<T: Pod>(s: &Arr<T>) -> bool {
            matches!(s.own, Slice::Shared { .. })
        }
        shared(&self.keys) && shared(&self.chunks) && shared(&self.small) && shared(&self.meta) && shared(&self.data)
    }

    /// Bytes held by the arrays (descriptors and padding included), i.e.
    /// the in-memory and serialised size, as opposed to the counted
    /// [`PostingList::size_bits`].
    pub fn heap_bytes(&self) -> usize {
        self.keys.own.byte_len() + self.chunks.own.byte_len() + self.small.own.byte_len() + self.meta.own.byte_len() + self.data.own.byte_len()
    }

    /// Bounds checks of [`Pyramid::read_from`].
    fn validate(&self) -> Result<(), String> {
        let bad = |what: &str, i: usize| Err(format!("Pyramid chunk {i}: {what}"));
        if self.keys.len() != self.chunks.len() || !self.keys.windows(2).all(|w| w[0] < w[1]) {
            return Err("Pyramid keys are not strictly increasing, one per chunk".into());
        }
        if self.small.len() < 64 || self.meta.len() < 2 {
            return Err("Pyramid padding missing".into());
        }
        let (meta, data) = (self.meta.len(), self.data.len());
        let mut total = 0u64;
        for (i, c) in self.chunks.iter().enumerate() {
            if c.card == 0 || c.card > 65_536 || c.flags > 7 {
                return bad("bad header", i);
            }
            total += c.card as u64;
            if c.small() {
                if c.card as usize > SMALL_MAX || c.off as usize + 64 > self.small.len() {
                    return bad("Small values out of bounds", i);
                }
                continue;
            }
            let off = c.off as usize;
            let masks = if c.nd > 0 && !c.flat() { 8 } else { 4 };
            if off + masks > meta {
                return bad("masks out of bounds", i);
            }
            if c.flat() {
                if c.bm as usize + 1024 > data {
                    return bad("Flat bitmap out of bounds", i);
                }
                continue;
            }
            let present = &self.meta[off..off + 4];
            let dense = if masks == 8 { &self.meta[off + 4..off + 8] } else { &[0; 4][..] };
            let nd: u32 = dense.iter().map(|w| w.count_ones()).sum();
            let ms = present.iter().zip(dense).map(|(p, d)| (p & !d).count_ones() as usize).sum::<usize>();
            let ns = c.ns as usize;
            if nd != c.nd as u32 || dense.iter().zip(present).any(|(d, p)| d & !p != 0) || ns < ms || nd as usize + ms == 0 {
                return bad("inconsistent masks", i);
            }
            if c.bm as usize + 4 * nd as usize > data || c.lows as usize + ns > 8 * data {
                return bad("leaves out of bounds", i);
            }
            // The `u16` directory entry `j` (offsets or samples).
            let dir = off + masks;
            let entry = |j: usize| (self.meta[dir + j / 4] >> (16 * (j % 4))) as u16 as usize;
            if c.offs() {
                // `ms + 1` prefix offsets, ending with `ns`.
                if dir + (ms + 1).div_ceil(4) > meta || entry(ms) != ns {
                    return bad("bad offsets", i);
                }
            } else if ms > 0 {
                // Samples, then `U` plus the two words select windows may
                // read; the terminator at bit `ns` bounds every scan of `U`
                // that starts at a sample.
                let samples = (ms - 1) / SAMPLE as usize;
                let u = c.u as usize;
                if u < dir + samples.div_ceil(4) || u + (ns + 1).div_ceil(64) + 2 > meta {
                    return bad("directory out of bounds", i);
                }
                if self.meta[u + ns / 64] >> (ns % 64) & 1 == 0 || self.meta[u] & 1 == 0 || (0..samples).any(|j| entry(j) >= ns) {
                    return bad("bad directory", i);
                }
            }
        }
        if total != self.card {
            return Err("Pyramid cardinality mismatch".into());
        }
        Ok(())
    }
}

impl PostingList for Pyramid {
    const NAME: &'static str = "Pyramid";

    fn build(docs: &[u32], _universe: u32) -> Self {
        let mut b = Builder { bits: 32, ..Builder::default() };
        let mut i = 0;
        while i < docs.len() {
            let key = docs[i] >> 16;
            let end = i + docs[i..].partition_point(|&d| d >> 16 == key);
            b.push_chunk(key as u16, &docs[i..end]);
            i = end;
        }
        b.finish()
    }

    fn len(&self) -> usize {
        self.card as usize
    }

    fn size_bits(&self) -> u64 {
        self.bits
    }

    fn decode_into(&self, out: &mut Vec<u32>) {
        out.reserve(self.card as usize + SLACK);
        for (&k, c) in self.keys.iter().zip(self.chunks.iter()) {
            self.decode_chunk(c, (k as u32) << 16, out);
        }
    }
}

// ---------------------------------------------------------------- sinks

/// Where set operations put their results: written docIDs or a count.
trait Sink {
    /// Counting only (no docIDs are written).
    const COUNT: bool;
    /// Count mode: adds `n` (possibly negative) to the count.
    fn add(&mut self, n: i64);
    /// The values `base | v` (in order).
    fn small(&mut self, vals: &[u16], base: u32);
    /// `base + d[j]` for the set bits `j` of `hits`.
    fn lanes(&mut self, d: &[u8; 32], hits: u32, base: u32);
    /// `base + i` for the set bits `i` of `w`.
    fn bitmap(&mut self, w: &[u64; 4], base: u32);
    /// `base + i` for the set bits `i` of the bitmap `w` (any length).
    fn words(&mut self, w: &[u64], base: u32);
    /// The whole chunk `c` of `p`.
    fn chunk(&mut self, p: &Pyramid, c: &Chunk, base: u32);
    /// The present blocks `run` of mask word `w` of the Tiered chunk `v`
    /// (consecutive among its present blocks); `ids` points at index 1 of
    /// its [`View::sparse_ids`].
    ///
    /// # Safety
    /// `v` is a live view and `run` a subset of its present blocks.
    unsafe fn run(&mut self, v: &View, w: usize, run: u64, ids: *const u32, base: u32);
}

struct VecSink<'a>(&'a mut Vec<u32>);

impl VecSink<'_> {
    /// Room for `n` values plus the kernels' slack.
    #[inline(always)]
    fn room(&mut self, n: usize) -> *mut u32 {
        let v = &mut *self.0;
        if v.capacity() - v.len() < n + SLACK {
            v.reserve(n + SLACK);
        }
        // SAFETY: within the allocation (len <= capacity).
        unsafe { v.as_mut_ptr().add(v.len()) }
    }

    #[inline(always)]
    unsafe fn advance(&mut self, n: usize) {
        let len = self.0.len();
        self.0.set_len(len + n);
    }
}

impl Sink for VecSink<'_> {
    const COUNT: bool = false;

    #[inline(always)]
    fn add(&mut self, _n: i64) {}

    #[inline(always)]
    fn small(&mut self, vals: &[u16], base: u32) {
        self.0.extend(vals.iter().map(|&v| base | v as u32));
    }

    #[inline(always)]
    fn lanes(&mut self, d: &[u8; 32], hits: u32, base: u32) {
        let dst = self.room(32);
        // SAFETY: room for 32 + SLACK values.
        unsafe {
            let n = simd::emit_lanes(d, hits, base, dst);
            self.advance(n);
        }
    }

    #[inline(always)]
    fn bitmap(&mut self, w: &[u64; 4], base: u32) {
        let dst = self.room(256);
        // SAFETY: room for 256 + SLACK values.
        unsafe {
            let n = simd::extract256(w, base, dst);
            self.advance(n);
        }
    }

    #[inline(always)]
    fn words(&mut self, w: &[u64], base: u32) {
        let dst = self.room(64 * w.len());
        // SAFETY: room for every bit plus SLACK values.
        unsafe {
            let n = simd::extract_words(w, base, dst);
            self.advance(n);
        }
    }

    #[inline(always)]
    fn chunk(&mut self, p: &Pyramid, c: &Chunk, base: u32) {
        p.decode_chunk(c, base, self.0);
    }

    #[inline(always)]
    unsafe fn run(&mut self, v: &View, w: usize, run: u64, ids: *const u32, base: u32) {
        // At most 64 blocks of 256 values.
        let dst = self.room(64 * 256);
        let n = v.decode_run(w, run, ids, base, dst);
        self.advance(n);
    }
}

struct CountSink(u64);

impl Sink for CountSink {
    const COUNT: bool = true;

    #[inline(always)]
    fn add(&mut self, n: i64) {
        self.0 = self.0.wrapping_add(n as u64);
    }

    #[inline(always)]
    fn small(&mut self, vals: &[u16], _base: u32) {
        self.0 += vals.len() as u64;
    }

    #[inline(always)]
    fn lanes(&mut self, _d: &[u8; 32], hits: u32, _base: u32) {
        self.0 += hits.count_ones() as u64;
    }

    #[inline(always)]
    fn bitmap(&mut self, w: &[u64; 4], _base: u32) {
        self.0 += w.iter().map(|x| x.count_ones() as u64).sum::<u64>();
    }

    #[inline(always)]
    fn words(&mut self, w: &[u64], _base: u32) {
        self.0 += w.iter().map(|x| x.count_ones() as u64).sum::<u64>();
    }

    #[inline(always)]
    fn chunk(&mut self, _p: &Pyramid, c: &Chunk, _base: u32) {
        self.0 += c.card as u64;
    }

    #[inline(always)]
    unsafe fn run(&mut self, _v: &View, _w: usize, _run: u64, _ids: *const u32, _base: u32) {
        unreachable!("count-only unions skip single runs")
    }
}

impl SetOps for Pyramid {
    fn intersect(lists: &[&Self], out: &mut Vec<u32>) {
        match lists {
            [] => {}
            [l] => l.decode_into(out),
            _ => and::and_lists(lists, &mut VecSink(out)),
        }
    }

    fn union(lists: &[&Self], out: &mut Vec<u32>) {
        match lists {
            [] => {}
            [l] => l.decode_into(out),
            _ => {
                out.reserve(lists.iter().map(|l| l.card).max().unwrap_or(0) as usize + SLACK);
                or::or_lists(lists, &mut VecSink(out))
            }
        }
    }
}

// ---------------------------------------------------------------- cursor

/// Position inside the current chunk.
#[derive(Clone, Copy)]
enum St {
    Small { p: *const u16, n: u32, i: u32 },
    /// The current block is dense; its bitmap.
    Dense { bm: *const u64 },
    /// The current block is sparse; its lows and the current index.
    Sparse { lows: *const u8, i: u32, n: u32 },
    Done,
}

pub struct PyramidCursor<'a> {
    list: &'a Pyramid,
    ci: usize,
    cur: u32,
    /// Key << 16 of the current chunk.
    base: u32,
    st: St,
    /// Tiered chunk state.
    view: View,
    /// Current block.
    blk: u32,
    /// Sparse blocks before the current block.
    sr: u32,
    /// Start of the next sparse block's lows (end of the current one when
    /// in a sparse block).
    so: u32,
}

impl<'a> PyramidCursor<'a> {
    fn new(list: &'a Pyramid) -> Self {
        let mut c =
            PyramidCursor { list, ci: 0, cur: END, base: 0, st: St::Done, view: View::EMPTY, blk: 0, sr: 0, so: 0 };
        c.load(0);
        c
    }

    /// Positions on the first value of chunk `ci` (or END).
    #[inline]
    fn load(&mut self, ci: usize) {
        self.ci = ci;
        let Some(c) = self.list.chunks.get(ci) else {
            self.cur = END;
            self.st = St::Done;
            return;
        };
        self.base = (self.list.keys[ci] as u32) << 16;
        if c.small() {
            let v = self.list.small_vals(c);
            self.st = St::Small { p: v.as_ptr(), n: v.len() as u32, i: 0 };
            self.cur = self.base | v[0] as u32;
            return;
        }
        // SAFETY: Tiered chunk of the list.
        self.view = unsafe { View::new(self.list, c) };
        self.sr = 0;
        self.so = 0;
        let b = first_at_or_after(&self.view.present, 0).unwrap();
        // SAFETY: b is present; sr/so describe the first block.
        unsafe { self.enter(b, 0) };
    }

    /// Enters present block `b` (with `sr` and `so` describing it: the
    /// sparse blocks before it and the start of the next sparse block's
    /// lows), positioned on
    /// its first value `>= low`. Returns false (without moving `cur`) if the
    /// block has none.
    #[inline]
    unsafe fn enter(&mut self, b: u32, low: u32) -> bool {
        self.blk = b;
        let w = (b >> 6) as usize;
        let bit = 1u64 << (b & 63);
        let bbase = self.base | b << 8;
        if self.view.dense[w] & bit != 0 {
            let bm = self.view.bitmap(w, bit);
            self.st = St::Dense { bm };
            match first_set(bm, low) {
                Some(l) => {
                    self.cur = bbase | l;
                    true
                }
                None => false,
            }
        } else {
            let e = self.view.sparse_end(self.sr, self.so);
            let lows = self.view.lows.add(self.so as usize);
            let n = e - self.so;
            self.so = e;
            let i = if low == 0 { 0 } else { simd::bytes_count_lt(lows, n as usize, low as u8) as u32 };
            self.st = St::Sparse { lows, i, n };
            if i < n {
                self.cur = bbase | *lows.add(i as usize) as u32;
                true
            } else {
                false
            }
        }
    }

    /// Leaves the current block for the next present one (or chunk).
    #[inline]
    fn next_block(&mut self) {
        if let St::Sparse { .. } = self.st {
            self.sr += 1;
        }
        match first_at_or_after(&self.view.present, self.blk + 1) {
            // SAFETY: b is present; sr/so were advanced past the old block.
            Some(b) => unsafe {
                self.enter(b, 0);
            },
            None => self.load(self.ci + 1),
        }
    }
}

/// First set bit `>= low` of the 256-bit bitmap at `bm`.
#[inline(always)]
unsafe fn first_set(bm: *const u64, low: u32) -> Option<u32> {
    if low >= 256 {
        return None;
    }
    let mut w = (low >> 6) as usize;
    let mut m = *bm.add(w) & (!0u64 << (low & 63));
    loop {
        if m != 0 {
            return Some((w as u32) << 6 | m.trailing_zeros());
        }
        w += 1;
        if w == 4 {
            return None;
        }
        m = *bm.add(w);
    }
}

impl Cursor for PyramidCursor<'_> {
    #[inline]
    fn docid(&self) -> u32 {
        self.cur
    }

    #[inline]
    fn next(&mut self) -> u32 {
        match &mut self.st {
            St::Small { p, n, i } => {
                *i += 1;
                if *i < *n {
                    // SAFETY: i < n.
                    self.cur = self.base | unsafe { *p.add(*i as usize) } as u32;
                } else {
                    self.load(self.ci + 1);
                }
            }
            St::Dense { bm } => {
                // SAFETY: bm is a dense block bitmap.
                match unsafe { first_set(*bm, (self.cur & 255) + 1) } {
                    Some(l) => self.cur = (self.cur & !255) | l,
                    None => self.next_block(),
                }
            }
            St::Sparse { lows, i, n } => {
                *i += 1;
                if *i < *n {
                    // SAFETY: i < n.
                    self.cur = (self.cur & !255) | unsafe { *lows.add(*i as usize) } as u32;
                } else {
                    self.next_block();
                }
            }
            St::Done => {}
        }
        self.cur
    }

    #[inline]
    fn next_geq(&mut self, target: u32) -> u32 {
        if target <= self.cur {
            return self.cur; // also covers END
        }
        if target >> 16 != self.base >> 16 {
            let ci = gallop(&self.list.keys, self.ci + 1, (target >> 16) as u16);
            self.load(ci);
            if target <= self.cur {
                return self.cur;
            }
        }
        // Same chunk as `target`, current value below it.
        let low16 = target & 0xFFFF;
        match &mut self.st {
            St::Small { p, n, i } => {
                // SAFETY: p..p + n is the chunk's value slice.
                let v = unsafe { std::slice::from_raw_parts(*p, *n as usize) };
                *i = gallop(v, *i as usize + 1, low16 as u16) as u32;
                if *i < *n {
                    self.cur = self.base | v[*i as usize] as u32;
                } else {
                    self.load(self.ci + 1);
                }
            }
            St::Done => {}
            _ => {
                let tb = low16 >> 8;
                let tl = low16 & 255;
                if tb == self.blk {
                    // Within the current block.
                    match &mut self.st {
                        St::Dense { bm } => {
                            // SAFETY: the current block's bitmap.
                            if let Some(l) = unsafe { first_set(*bm, tl) } {
                                self.cur = (self.cur & !255) | l;
                                return self.cur;
                            }
                        }
                        St::Sparse { lows, i, n } => {
                            // SAFETY: the current block's lows.
                            unsafe {
                                let rest = simd::bytes_count_lt(lows.add(*i as usize), (*n - *i) as usize, tl as u8);
                                *i += rest as u32;
                                if *i < *n {
                                    self.cur = (self.cur & !255) | *lows.add(*i as usize) as u32;
                                    return self.cur;
                                }
                            }
                        }
                        _ => unreachable!(),
                    }
                    self.next_block();
                    return self.cur;
                }
                // A later block: random access by rank and select.
                let Some(b) = first_at_or_after(&self.view.present, tb) else {
                    self.load(self.ci + 1);
                    return self.cur;
                };
                let w = (b >> 6) as usize;
                let bit = 1u64 << (b & 63);
                self.sr = self.view.sparse_rank(w, bit);
                // SAFETY: b is present; sr/so describe it.
                unsafe {
                    self.so = self.view.sparse_start(self.sr);
                    if !self.enter(b, if b == tb { tl } else { 0 }) {
                        self.next_block();
                    }
                }
            }
        }
        self.cur
    }

    fn size(&self) -> usize {
        self.list.len()
    }
}

impl CursorList for Pyramid {
    type Cursor<'a> = PyramidCursor<'a>;

    fn cursor(&self) -> PyramidCursor<'_> {
        PyramidCursor::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::conformance;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use std::collections::BTreeSet;

    #[test]
    fn conformance() {
        conformance::check_roundtrip::<Pyramid>();
        conformance::check_cursor::<Pyramid>();
        conformance::check_setops::<Pyramid>();
    }

    fn reference_and(lists: &[&Vec<u32>]) -> Vec<u32> {
        let mut acc = lists[0].clone();
        for l in &lists[1..] {
            acc.retain(|x| l.binary_search(x).is_ok());
        }
        acc
    }

    fn reference_or(lists: &[&Vec<u32>]) -> Vec<u32> {
        lists.iter().flat_map(|l| l.iter().copied()).collect::<BTreeSet<_>>().into_iter().collect()
    }

    /// Lists over 10 chunks where each chunk is, per list, empty, Small,
    /// all-sparse Tiered, all-dense Tiered, mixed Tiered (including blocks
    /// just below and at the dense threshold and long sparse runs that
    /// exercise the slow select path), or full.
    fn mixed_lists(rng: &mut StdRng, count: usize) -> Vec<Vec<u32>> {
        (0..count)
            .map(|_| {
                let mut docs = BTreeSet::new();
                for key in 0..10u32 {
                    let base = key << 16;
                    match rng.gen_range(0..8) {
                        0 => {
                            for _ in 0..rng.gen_range(1..=SMALL_MAX) {
                                docs.insert(base + rng.gen_range(0..65_536));
                            }
                        }
                        1 => {
                            for _ in 0..rng.gen_range(100..3000) {
                                docs.insert(base + rng.gen_range(0..65_536));
                            }
                        }
                        2 => {
                            for b in 0..256u32 {
                                if rng.gen_bool(0.3) {
                                    let lo = rng.gen_range(0..200);
                                    docs.extend(base + (b << 8) + lo..base + (b << 8) + lo + rng.gen_range(29..57));
                                }
                            }
                        }
                        3 => {
                            for b in 0..256u32 {
                                let c = match rng.gen_range(0..5) {
                                    0 => 0,
                                    1 => 1,
                                    2 => DENSE_MIN - 1,
                                    3 => DENSE_MIN,
                                    _ => rng.gen_range(2..200),
                                };
                                let mut lows: Vec<u32> = (0..256).collect();
                                for i in 0..c as usize {
                                    let j = rng.gen_range(i..256);
                                    lows.swap(i, j);
                                }
                                docs.extend(lows[..c as usize].iter().map(|&l| base + (b << 8) + l));
                            }
                        }
                        4 => {
                            // 70 sparse blocks of 28 values in a row.
                            for b in 0..70u32 {
                                docs.extend((0..DENSE_MIN - 1).map(|i| base + (b << 8) + 3 * i));
                            }
                        }
                        5 => docs.extend(base..base + 65_536),
                        6 => {
                            // Dense random with gaps: a Flat chunk.
                            for b in 0..256u32 {
                                if rng.gen_bool(0.9) {
                                    docs.extend((0..256).filter(|_| rng.gen_bool(0.4)).map(|l| base + (b << 8) + l));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                docs.into_iter().collect()
            })
            .collect()
    }

    #[test]
    fn mixed_chunk_kinds() {
        let mut rng = StdRng::seed_from_u64(7);
        let lists = mixed_lists(&mut rng, 10);
        let universe = 10 << 16;
        let built: Vec<Pyramid> = lists.iter().map(|l| Pyramid::build(l, universe)).collect();
        let kinds = built.iter().fold([0; 3], |t, p| {
            let c = p.chunk_counts();
            [t[0] + c[0], t[1] + c[1], t[2] + c[2]]
        });
        let blocks = built.iter().fold([0; 2], |t, p| [t[0] + p.block_counts()[0], t[1] + p.block_counts()[1]]);
        assert!(kinds.iter().all(|&n| n > 3), "chunk mix {kinds:?}");
        assert!(blocks.iter().all(|&n| n > 100), "block mix {blocks:?}");

        for (l, p) in lists.iter().zip(&built) {
            let mut out = Vec::new();
            p.decode_into(&mut out);
            assert!(&out == l);
            for _ in 0..2000 {
                let x = if rng.gen_bool(0.5) && !l.is_empty() { l[rng.gen_range(0..l.len())] } else { rng.gen_range(0..universe) };
                assert_eq!(p.contains(x), l.binary_search(&x).is_ok(), "contains({x})");
            }
        }

        for _ in 0..300 {
            let k = rng.gen_range(1..=5);
            let idx: Vec<usize> = (0..k).map(|_| rng.gen_range(0..lists.len())).collect();
            let refs: Vec<&Vec<u32>> = idx.iter().map(|&i| &lists[i]).collect();
            let prefs: Vec<&Pyramid> = idx.iter().map(|&i| &built[i]).collect();
            let want_and = reference_and(&refs);
            let want_or = reference_or(&refs);
            let mut got = Vec::new();
            Pyramid::intersect(&prefs, &mut got);
            assert!(got == want_and, "intersect {idx:?}");
            got.clear();
            Pyramid::union(&prefs, &mut got);
            assert!(got == want_or, "union {idx:?}");
            assert_eq!(Pyramid::and_count(&prefs), want_and.len() as u64, "and_count {idx:?}");
            assert_eq!(Pyramid::or_count(&prefs), want_or.len() as u64, "or_count {idx:?}");
        }

        // Cursors: next_geq at random strides (crossing blocks and chunks),
        // interleaved with next().
        for (l, p) in lists.iter().zip(&built) {
            for stride in [3u32, 40, 700, 5000] {
                let mut cur = p.cursor();
                let mut t = 0u32;
                let mut pos = 0usize;
                while t < universe {
                    pos = pos.max(l.partition_point(|&x| x < t));
                    let want = l.get(pos).copied().unwrap_or(END);
                    assert_eq!(cur.next_geq(t), want, "next_geq({t})");
                    if want == END {
                        break;
                    }
                    if rng.gen_bool(0.3) {
                        pos += 1;
                        assert_eq!(cur.next(), l.get(pos).copied().unwrap_or(END), "next after {want}");
                    }
                    t = cur.docid().max(t) + rng.gen_range(1..=stride);
                }
            }
        }
    }

    #[test]
    fn chunk_choice_and_size() {
        // One Small chunk of 3 values: list header + chunk header + 3 x 16.
        let p = Pyramid::build(&[1, 5, 9], 100);
        assert_eq!(p.chunk_counts(), [1, 0, 0]);
        assert_eq!(p.size_bits(), 32 + 64 + 48);
        // 100 singletons in distinct blocks: Tiered, all sparse.
        let docs: Vec<u32> = (0..100).map(|i| i * 256 + 7).collect();
        let p = Pyramid::build(&docs, 1 << 16);
        assert_eq!(p.chunk_counts(), [0, 1, 0]);
        assert_eq!(p.block_counts(), [0, 100]);
        // 32 + 64 + counts 32 + present 256 + U 101 + samples + lows 800.
        let samples = 16 * (99 / SAMPLE as u64);
        assert_eq!(p.size_bits(), 32 + 64 + 32 + 256 + 101 + samples + 800);
        // A full chunk: Flat (smaller than Tiered with 256 dense blocks).
        let p = Pyramid::build(&(0..65_536).collect::<Vec<_>>(), 1 << 16);
        assert_eq!(p.chunk_counts(), [0, 0, 1]);
        assert_eq!(p.block_counts(), [256, 0]);
        assert_eq!(p.size_bits(), 32 + 64 + 32 + 256 + 256 * 256);
        // Dense random chunk with an absent block: Flat (its 256 bitmaps are
        // smaller than 255 dense blocks plus masks); a sparse one: Tiered.
        let mut rng = StdRng::seed_from_u64(3);
        let docs: Vec<u32> = (0..65_536).filter(|&x| x >> 8 != 7 && rng.gen_bool(0.3)).collect();
        let p = Pyramid::build(&docs, 1 << 16);
        assert_eq!(p.chunk_counts(), [0, 0, 1]);
        assert_eq!(p.block_counts(), [255, 0]);
        assert_eq!(p.size_bits(), 32 + 64 + FLAT_BITS);
        let docs: Vec<u32> = (0..65_536).filter(|_| rng.gen_bool(0.03)).collect();
        assert_eq!(Pyramid::build(&docs, 1 << 16).chunk_counts(), [0, 1, 0]);
        // 10 sparse blocks of 20 values: offsets (9 x 16 bits) beat U (201
        // bits plus a sample).
        let docs: Vec<u32> = (0..10).flat_map(|b| (0..20).map(move |i| b * 256 + 3 * i)).collect();
        let p = Pyramid::build(&docs, 1 << 16);
        assert!(p.chunks[0].offs() && !p.chunks[0].small());
        assert_eq!(p.size_bits(), 32 + 64 + 32 + 256 + 9 * 16 + 200 * 8);
        let mut out = Vec::new();
        p.decode_into(&mut out);
        assert_eq!(out, docs);
        // Threshold: DENSE_MIN - 1 values stay sparse, DENSE_MIN are dense.
        let mut docs: Vec<u32> = (0..DENSE_MIN - 1).collect();
        docs.extend((0..DENSE_MIN).map(|i| 256 + i));
        docs.extend((0..40).map(|i| 1024 + 256 * i));
        let p = Pyramid::build(&docs, 1 << 20);
        assert_eq!(p.block_counts(), [1, 41]);
        // Exactly SMALL_MAX + 1 singletons is never Small.
        let docs: Vec<u32> = (0..SMALL_MAX as u32 + 1).map(|i| i * 256).collect();
        assert_eq!(Pyramid::build(&docs, 1 << 16).chunk_counts(), [0, 1, 0]);
    }

    #[test]
    fn sparse_directory_select() {
        // Sparse blocks with every count 1..DENSE_MIN - 1 in varied order,
        // so selects cross words, samples and the slow path.
        let mut rng = StdRng::seed_from_u64(9);
        for _ in 0..50 {
            let mut docs = Vec::new();
            for b in 0..256u32 {
                if rng.gen_bool(0.8) {
                    let c = rng.gen_range(1..DENSE_MIN);
                    docs.extend((0..c).map(|i| (b << 8) + 7 * i));
                }
            }
            let p = Pyramid::build(&docs, 1 << 16);
            let c = &p.chunks[0];
            assert!(!c.small());
            // SAFETY: Tiered chunk of p.
            let v = unsafe { View::new(&p, c) };
            let mut start = 0u32;
            let mut r = 0;
            for b in 0..256u32 {
                let n = docs.iter().filter(|&&d| d >> 8 == b).count() as u32;
                if n == 0 {
                    continue;
                }
                let bit = 1u64 << (b & 63);
                assert_eq!(v.sparse_rank((b >> 6) as usize, bit), r);
                assert_eq!(unsafe { v.sparse_range(r) }, (start, start + n), "block {b} rank {r}");
                start += n;
                r += 1;
            }
            assert_eq!(unsafe { v.sparse_start(r) }, start);
        }
    }

    #[test]
    fn flat_chunks_with_gaps() {
        // Nearly full blocks with absent blocks in between: Flat chunks whose
        // bitmap index is the block id, not the rank among present blocks.
        let mut rng = StdRng::seed_from_u64(13);
        let universe = 8 << 16;
        let mut lists: Vec<Vec<u32>> = (0..5)
            .map(|_| {
                let mut docs = Vec::new();
                for key in 0..8u32 {
                    let gaps: BTreeSet<u32> = (0..rng.gen_range(1..40)).map(|_| rng.gen_range(0..256)).collect();
                    for b in (0..256u32).filter(|b| !gaps.contains(b)) {
                        docs.extend((0..256u32).filter(|_| rng.gen_bool(0.97)).map(|l| (key << 16) + (b << 8) + l));
                    }
                }
                docs
            })
            .collect();
        lists.push(conformance::random_sorted(&mut rng, 3000, universe));
        lists.push(conformance::clustered(&mut rng, 40_000, universe));
        lists.push(conformance::random_sorted(&mut rng, 200_000, universe));
        let built: Vec<Pyramid> = lists.iter().map(|l| Pyramid::build(l, universe)).collect();
        if FLAT_BIAS_PCT >= 110 {
            assert!(built[..5].iter().all(|p| p.chunk_counts()[2] == 8), "expected Flat chunks");
        }
        for (l, p) in lists.iter().zip(&built) {
            let mut out = Vec::new();
            p.decode_into(&mut out);
            assert!(&out == l, "decode");
            for _ in 0..3000 {
                let x = rng.gen_range(0..universe);
                assert_eq!(p.contains(x), l.binary_search(&x).is_ok(), "contains({x})");
            }
            let mut cur = p.cursor();
            let (mut t, mut pos) = (0u32, 0usize);
            while t < universe {
                pos = pos.max(l.partition_point(|&x| x < t));
                let want = l.get(pos).copied().unwrap_or(END);
                assert_eq!(cur.next_geq(t), want, "next_geq({t})");
                if want == END {
                    break;
                }
                if rng.gen_bool(0.5) {
                    pos += 1;
                    assert_eq!(cur.next(), l.get(pos).copied().unwrap_or(END));
                }
                t = cur.docid().max(t) + rng.gen_range(1..2000);
            }
        }
        for _ in 0..200 {
            let k = rng.gen_range(2..=4);
            let idx: Vec<usize> = (0..k).map(|_| rng.gen_range(0..lists.len())).collect();
            let refs: Vec<&Vec<u32>> = idx.iter().map(|&i| &lists[i]).collect();
            let prefs: Vec<&Pyramid> = idx.iter().map(|&i| &built[i]).collect();
            let (want_and, want_or) = (reference_and(&refs), reference_or(&refs));
            let mut got = Vec::new();
            Pyramid::intersect(&prefs, &mut got);
            assert!(got == want_and, "intersect {idx:?}");
            got.clear();
            Pyramid::union(&prefs, &mut got);
            assert!(got == want_or, "union {idx:?}");
            assert_eq!(Pyramid::and_count(&prefs), want_and.len() as u64);
            assert_eq!(Pyramid::or_count(&prefs), want_or.len() as u64);
        }
    }

    #[test]
    fn serialised_views_match_built_lists() {
        let mut rng = StdRng::seed_from_u64(17);
        let universe = 10 << 16;
        let mut lists = mixed_lists(&mut rng, 8);
        lists.push(Vec::new());
        lists.push(vec![universe - 1]);
        let built: Vec<Pyramid> = lists.iter().map(|l| Pyramid::build(l, universe)).collect();
        // All lists in one buffer, as in a segment section.
        let mut w = ByteWriter::new();
        for p in &built {
            w.u64(7);
            p.write_to(&mut w);
        }
        let bytes = OwnedBytes::new(w.finish());
        let base = bytes.as_ptr() as usize;
        let mut r = ByteReader::new(bytes);
        let loaded: Vec<Pyramid> = built
            .iter()
            .map(|_| {
                assert_eq!(r.u64().unwrap(), 7);
                Pyramid::read_from(&mut r).unwrap()
            })
            .collect();
        for ((l, p), q) in lists.iter().zip(&built).zip(&loaded) {
            assert!(q.is_view() && !p.is_view());
            assert_eq!(q.cardinality(), p.cardinality());
            assert_eq!(q.size_bits(), p.size_bits());
            assert_eq!(q.heap_bytes(), p.heap_bytes());
            assert_eq!((q.meta.as_ptr() as usize - base) % 64, 0);
            assert_eq!((q.data.as_ptr() as usize - base) % 64, 0);
            let mut out = Vec::new();
            q.decode_into(&mut out);
            assert!(&out == l);
            for _ in 0..1000 {
                let x = rng.gen_range(0..universe);
                assert_eq!(q.contains(x), l.binary_search(&x).is_ok());
            }
            let mut cur = q.cursor();
            for &x in l.iter().step_by(97) {
                assert_eq!(cur.next_geq(x), x);
            }
        }
        for _ in 0..100 {
            let k = rng.gen_range(2..=4);
            let idx: Vec<usize> = (0..k).map(|_| rng.gen_range(0..lists.len())).collect();
            let refs: Vec<&Vec<u32>> = idx.iter().map(|&i| &lists[i]).collect();
            // Mix built and loaded lists.
            let prefs: Vec<&Pyramid> = idx.iter().enumerate().map(|(j, &i)| if j % 2 == 0 { &loaded[i] } else { &built[i] }).collect();
            let mut got = Vec::new();
            Pyramid::intersect(&prefs, &mut got);
            assert!(got == reference_and(&refs));
            got.clear();
            Pyramid::union(&prefs, &mut got);
            assert!(got == reference_or(&refs));
        }
        // A copy of a loaded list shares its bytes.
        assert!(loaded[0].clone().is_view());
    }

    #[test]
    fn corrupt_bytes_are_rejected() {
        let mut rng = StdRng::seed_from_u64(19);
        let lists = mixed_lists(&mut rng, 4);
        for l in &lists {
            let p = Pyramid::build(l, 10 << 16);
            let bytes = p.to_bytes();
            assert!(Pyramid::from_bytes(OwnedBytes::new(bytes.clone())).is_ok());
            for cut in [8, bytes.len() / 2, bytes.len() - 8] {
                assert!(Pyramid::from_bytes(OwnedBytes::new(bytes[..cut].to_vec())).is_err(), "truncated at {cut}");
            }
            // The tag, the total cardinality, the first key (the keys then
            // collide or reorder) and the first chunk's cardinality.
            let first_chunk = 32 + (2 * p.keys.len()).next_multiple_of(8) + 8;
            assert!(p.keys.len() >= 2);
            for (at, what) in [(0, "tag"), (8, "card"), (32, "key"), (first_chunk, "chunk card")] {
                let mut b = bytes.clone();
                b[at] ^= if at == 32 { p.keys[1] as u8 ^ p.keys[0] as u8 } else { 1 };
                assert!(Pyramid::from_bytes(OwnedBytes::new(b)).is_err(), "{what}");
            }
        }
    }

    #[test]
    fn bitset_operations() {
        let mut rng = StdRng::seed_from_u64(23);
        let universe = 10 << 16;
        let lists = mixed_lists(&mut rng, 10);
        for l in &lists {
            let p = Pyramid::build(l, universe);
            // Just long enough for the last docID (ending inside a block).
            let n_words = l.last().map_or(0, |&x| x as usize / 64 + 1);
            let mut words = vec![0u64; n_words];
            p.or_into_bits(&mut words);
            let mut want = vec![0u64; n_words];
            for &x in l {
                want[x as usize / 64] |= 1 << (x % 64);
            }
            assert!(words == want);
            // Count against random bitsets of several densities, including
            // ones shorter than the list's range.
            for density in [0.001, 0.1, 0.5, 0.99] {
                let len = if rng.gen_bool(0.3) { rng.gen_range(0..=n_words) } else { n_words };
                let bits: Vec<u64> = (0..len)
                    .map(|_| (0..64).fold(0u64, |w, b| w | (rng.gen_bool(density) as u64) << b))
                    .collect();
                let want = l.iter().filter(|&&x| bits.get(x as usize / 64).is_some_and(|w| w >> (x % 64) & 1 == 1)).count();
                assert_eq!(p.and_count_bits(&bits), want as u64);
            }
        }
    }

    #[test]
    fn many_lists() {
        // More lists than any fixed-size buffer would assume.
        let mut rng = StdRng::seed_from_u64(5);
        let universe = 1 << 20;
        let lists: Vec<Vec<u32>> = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    (0..universe).step_by(1 + i).collect()
                } else {
                    conformance::clustered(&mut rng, 200_000, universe)
                }
            })
            .collect();
        let built: Vec<Pyramid> = lists.iter().map(|l| Pyramid::build(l, universe)).collect();
        let refs: Vec<&Vec<u32>> = lists.iter().collect();
        let prefs: Vec<&Pyramid> = built.iter().collect();
        let mut got = Vec::new();
        Pyramid::intersect(&prefs, &mut got);
        assert!(got == reference_and(&refs));
        got.clear();
        Pyramid::union(&prefs, &mut got);
        assert!(got == reference_or(&refs));
        assert_eq!(Pyramid::or_count(&prefs), got.len() as u64);
    }
}
