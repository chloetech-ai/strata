//! Entropic: context-modelled entropy coding of d-gaps in skippable blocks.
//!
//! Reordered posting lists have low-entropy gap structure (runs, then
//! bursts, then jumps) that bit packing, Elias-Fano and interpolative coding
//! do not model. Entropic codes every gap with table-based asymmetric
//! numeral systems (tANS) under a small context model fitted per list,
//! keeps 128-posting blocks independently decodable, and pays for skipping
//! with a compressed skip table.
//!
//! # Format
//!
//! * **Blocks.** A list is cut into blocks of [`BLOCK`] postings. A block
//!   codes the gaps `g = d[i] - d[i-1] - 1` of its first `n - 1` postings
//!   (the first relative to the previous block's maximum); the last posting
//!   is the block's maximum, which the skip table holds anyway.
//! * **Symbols** ([`model`]). A gap is a token (64 classes: the exact values
//!   0..=3, then bit width plus the bit below the leading one) and the
//!   remaining low bits, stored raw in the same bit stream.
//! * **Contexts** ([`model`]). Token `i` is coded in the context of token
//!   `i - 2`: one to four contexts, each a contiguous range of the token
//!   axis, chosen per list by dynamic programming. Conditioning on `i - 2`
//!   rather than `i - 1` costs about 0.02 bits per posting, and lets two
//!   tANS states code alternate tokens as independent dependency chains.
//! * **Models** ([`model`]), chosen per list by total size behind a 1–2 bit
//!   selector: an *own* model (5-bit log-scale frequency levels over the
//!   tokens each context uses), a *shared* parametric model (zero-inflated
//!   geometric per context, 20-bit index; decode tables built once per
//!   process), or the parameter-free *auto* model (geometric with the
//!   list's mean gap). The tANS state size, 7 to 10 bits, is chosen per list
//!   too.
//! * **Coder** ([`tans`]). Context-switching tANS: all contexts share the
//!   state space, and each 8-byte decode-table entry folds in the next
//!   context, so a symbol costs one load, one `bzhi` and one `or` on the
//!   critical path. Two lanes (states) per block; a branch-free bit reader;
//!   the lanes' final states carry the low raw bits of their last symbols
//!   ("bits-back"), so the initial states cost little.
//! * **Skip table** ([`skip`]). An Elias-Fano sequence of block maxima
//!   (with a select0 directory for long jumps) and one of the blocks' bit
//!   offsets (with a select1 directory), about 20 bits per block in all;
//!   none for single-block lists.
//!
//! `size_bits` counts the streams, the model (selector plus index or own
//! model) and the skip table with its directories. As for every codec here,
//! the list length is lexicon metadata and not counted. The decode tables
//! are derived from the counted model bits when a list is built (as zstd
//! or FSE decoders do per frame): own tables are kept with the list, shared
//! ones once per process.
//!
//! # Algorithms
//!
//! * **Cursor.** `next_geq` seeks the block in the Elias-Fano maxima, then
//!   decodes lazily up to the target, two symbols per bit-buffer refill.
//! * **Bulk decoding** (`decode_into`, `union`, the intersection's driver).
//!   With AVX-512, eight blocks decode at once: lane `j` of a register runs
//!   block `j`, table lookups and refills are gathers; the remaining blocks
//!   decode two at a time (four interleaved chains).
//! * **`SetOps::intersect`**: SvS. The shortest list is decoded in bulk;
//!   each other list, shortest first, filters the candidates through a
//!   cursor (one lazy block decode serves every candidate in the block),
//!   stopping early when no candidate is left.
//! * **`SetOps::union`**: every list sets bits in a bitmap over the result's
//!   docID range straight from the decoder (the eight-block decoder's
//!   interleaved output avoids read-modify-write stalls), then the bitmap is
//!   extracted with [`BitPos::extract`] (AVX-512 VBMI2 when available).

pub mod model;
pub mod skip;
pub mod tans;

use super::roaring::simd::{BitPos, EXTRACT_SLACK};
use super::{Cursor, CursorList, PostingList, SetOps};
use crate::bits::BitVec;
use crate::END;
use model::{fit_own, token, Model, OwnModel, SharedParams, Stats, LANES, SHARED_INDEX_BITS};
use skip::Ef;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
use tans::{run8, SIMD_BLOCKS};
use tans::{decode_table, run2, Decoder, Encoder, PADDING, PAIR_MAX_BITS};

/// Postings per block.
pub const BLOCK: usize = 128;

/// Lists with fewer coded gaps than this never use an own model...
const OWN_MIN_SYMBOLS: u64 = 1024;
/// ...and with fewer than this, only the parameter-free one.
const SHARED_MIN_SYMBOLS: u64 = 16;

/// Decode tables of the shared models, built once per process.
static SHARED: OnceLock<Mutex<HashMap<SharedParams, &'static [u64]>>> = OnceLock::new();

fn shared_table(p: SharedParams) -> &'static [u64] {
    let mut map = SHARED.get_or_init(Default::default).lock().unwrap();
    *map.entry(p).or_insert_with(|| Box::leak(decode_table(&p.model())))
}

/// Number and total bytes of the shared-model decode tables built so far
/// in this process (memory, not part of any list's size).
pub fn shared_tables() -> (usize, usize) {
    SHARED.get().map_or((0, 0), |m| {
        let m = m.lock().unwrap();
        (m.len(), m.values().map(|t| 8 * t.len()).sum())
    })
}

enum Table {
    None,
    Own(Box<[u64]>),
    Shared(&'static [u64]),
}

/// An Entropic posting list.
pub struct Entropic {
    n: usize,
    nblocks: usize,
    /// Last docID of the list.
    last: u32,
    /// Skip table (`None` for single-block lists, whose only maximum is
    /// `last` and whose stream starts at bit 0).
    skip: Option<Box<Skip>>,
    /// Block streams, plus [`PADDING`] bytes (empty if there are none).
    data: Vec<u8>,
    data_bits: u64,
    model_bits: u64,
    table: Table,
    start_ctx: u32,
    /// tANS state bits of the model.
    r: u32,
    /// Every symbol takes at most [`PAIR_MAX_BITS`] bits, so the decoder
    /// refills once per two symbols.
    pair: bool,
}

/// Skip table of a list with more than one block.
struct Skip {
    /// Last docID of every block.
    maxes: Ef,
    /// Bit offset in `data` of every block after the first with at least
    /// two postings (all blocks but the first and possibly the last).
    offsets: Ef,
}

static NO_SKIP: Skip = Skip { maxes: Ef::EMPTY, offsets: Ef::EMPTY };

/// Sequential walk over the blocks of a list: `(block, base, max, offset)`.
struct Blocks<'a> {
    list: &'a Entropic,
    b: usize,
    mpos: usize,
    opos: usize,
    base: u32,
}

impl Iterator for Blocks<'_> {
    type Item = (usize, u32, u32, usize);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let l = self.list;
        if self.b >= l.nblocks {
            return None;
        }
        let b = self.b;
        if l.nblocks == 1 {
            self.b = 1;
            return Some((0, 0, l.last, 0));
        }
        self.mpos = l.skip().maxes.next_one(self.mpos);
        let max = l.skip().maxes.value_at(b, self.mpos);
        let off = if b > 0 && b <= l.skip().offsets.len() {
            self.opos = l.skip().offsets.next_one(self.opos);
            let o = l.skip().offsets.value_at(b - 1, self.opos) as usize;
            self.opos += 1;
            o
        } else {
            0
        };
        self.mpos += 1;
        let base = self.base;
        self.base = max.wrapping_add(1);
        self.b += 1;
        Some((b, base, max, off))
    }
}

impl Entropic {
    #[inline(always)]
    fn skip(&self) -> &Skip {
        self.skip.as_deref().unwrap_or(&NO_SKIP)
    }

    #[inline]
    fn block_len(&self, b: usize) -> usize {
        if b + 1 < self.nblocks {
            BLOCK
        } else {
            self.n - b * BLOCK
        }
    }

    #[inline]
    fn table(&self) -> &[u64] {
        match &self.table {
            Table::None => &[],
            Table::Own(t) => t,
            Table::Shared(t) => t,
        }
    }

    #[inline]
    fn blocks(&self) -> Blocks<'_> {
        Blocks { list: self, b: 0, mpos: 0, opos: 0, base: 0 }
    }

    /// Decoder at bit `offset`, the start of a block of `n >= 2` postings.
    #[inline(always)]
    fn decoder(&self, offset: usize, n: usize) -> Decoder {
        // SAFETY: `offset` starts a block stream of n - 1 gaps encoded with
        // this list's model; data is padded.
        unsafe { Decoder::new(self.table(), self.data.as_ptr(), offset, self.start_ctx, n - 1, self.r) }
    }

    /// Decode all blocks into `out[..n]`.
    fn decode_all(&self, out: &mut [u32]) {
        if self.pair {
            self.decode_all_impl::<true>(out)
        } else {
            self.decode_all_impl::<false>(out)
        }
    }

    /// Decode all blocks, two at a time so that the decoders' four lanes
    /// overlap.
    #[inline(always)]
    fn decode_all_impl<const PAIR: bool>(&self, out: &mut [u32]) {
        let mut it = self.blocks();
        // Groups of eight full blocks with the AVX-512 decoder.
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
        if PAIR {
            let mut tmp = [[0u32; SIMD_BLOCKS]; BLOCK - 1];
            for g in 0..self.full_blocks() / SIMD_BLOCKS {
                let maxs = self.simd_group(&mut it, &mut tmp);
                let dst = &mut out[g * SIMD_BLOCKS * BLOCK..(g + 1) * SIMD_BLOCKS * BLOCK];
                transpose_group(&tmp, &maxs, dst);
            }
        }
        while let Some((b, base, max, off)) = it.next() {
            let n = self.block_len(b);
            if n == BLOCK && b + 1 < self.nblocks && self.block_len(b + 1) == BLOCK {
                let (b1, base1, max1, off1) = it.next().unwrap();
                let mut d0 = self.decoder(off, BLOCK);
                let mut d1 = self.decoder(off1, BLOCK);
                let (o0, o1) = out[b * BLOCK..(b1 + 1) * BLOCK].split_at_mut(BLOCK);
                // SAFETY: both blocks have BLOCK - 1 coded gaps.
                unsafe {
                    run2::<PAIR>(
                        &mut d0,
                        &mut d1,
                        base.wrapping_sub(1),
                        base1.wrapping_sub(1),
                        &mut o0[..BLOCK - 1],
                        &mut o1[..BLOCK - 1],
                    )
                };
                o0[BLOCK - 1] = max;
                o1[BLOCK - 1] = max1;
            } else {
                let o = &mut out[b * BLOCK..b * BLOCK + n];
                if n > 1 {
                    let mut d = self.decoder(off, n);
                    // SAFETY: the block has n - 1 coded gaps.
                    unsafe { d.block::<PAIR>(base.wrapping_sub(1), &mut o[..n - 1]) };
                }
                o[n - 1] = max;
            }
        }
    }

    /// Number of blocks with [`BLOCK`] postings (all but possibly the last).
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    #[inline]
    fn full_blocks(&self) -> usize {
        self.n / BLOCK
    }

    /// Decode the next [`SIMD_BLOCKS`] blocks of `it` (all full) into
    /// `tmp` (row `s`: the `s`-th docID of every block); returns their
    /// maxima. The list must be pair-safe.
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    #[inline(always)]
    fn simd_group(&self, it: &mut Blocks<'_>, tmp: &mut [[u32; SIMD_BLOCKS]; BLOCK - 1]) -> [u32; SIMD_BLOCKS] {
        let mut decs = [Decoder::empty(); SIMD_BLOCKS];
        let mut prev = [0u32; SIMD_BLOCKS];
        let mut maxs = [0u32; SIMD_BLOCKS];
        for j in 0..SIMD_BLOCKS {
            let (b, base, max, off) = it.next().unwrap();
            debug_assert_eq!(self.block_len(b), BLOCK);
            decs[j] = self.decoder(off, BLOCK);
            prev[j] = base.wrapping_sub(1);
            maxs[j] = max;
        }
        // SAFETY: every block has BLOCK - 1 gaps; all decoders read this
        // list's padded data with its table; the list is pair-safe.
        unsafe { run8::<true>(&mut decs, &prev, self.data.as_ptr(), tmp) };
        maxs
    }

    /// Set the bit of every posting in `words` (bit `d - 64 · lo_word`).
    fn set_bits(&self, words: &mut [u64], lo_word: usize) {
        #[allow(unused_mut)] // only the AVX-512 path advances it here
        let mut it = self.blocks();
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
        if self.pair {
            let mut tmp = [[0u32; SIMD_BLOCKS]; BLOCK - 1];
            for _ in 0..self.full_blocks() / SIMD_BLOCKS {
                let maxs = self.simd_group(&mut it, &mut tmp);
                // Consecutive updates hit different blocks, so they do not
                // wait on each other's read-modify-write.
                for row in tmp.iter() {
                    for &d in row {
                        set_bit(words, lo_word, d);
                    }
                }
                for &d in &maxs {
                    set_bit(words, lo_word, d);
                }
            }
        }
        if self.pair {
            self.set_bits_rest::<true>(it, words, lo_word)
        } else {
            self.set_bits_rest::<false>(it, words, lo_word)
        }
    }

    /// [`Entropic::set_bits`] for the blocks left in `it`, two at a time.
    #[inline(always)]
    fn set_bits_rest<const PAIR: bool>(&self, mut it: Blocks<'_>, words: &mut [u64], lo_word: usize) {
        let mut buf = [0u32; 2 * BLOCK];
        while let Some((b, base, max, off)) = it.next() {
            let n = self.block_len(b);
            if n == BLOCK && b + 1 < self.nblocks && self.block_len(b + 1) == BLOCK {
                let (_, base1, max1, off1) = it.next().unwrap();
                let mut d0 = self.decoder(off, BLOCK);
                let mut d1 = self.decoder(off1, BLOCK);
                let (o0, o1) = buf.split_at_mut(BLOCK);
                // SAFETY: both blocks have BLOCK - 1 coded gaps.
                unsafe {
                    run2::<PAIR>(
                        &mut d0,
                        &mut d1,
                        base.wrapping_sub(1),
                        base1.wrapping_sub(1),
                        &mut o0[..BLOCK - 1],
                        &mut o1[..BLOCK - 1],
                    )
                };
                o0[BLOCK - 1] = max;
                o1[BLOCK - 1] = max1;
                for i in 0..BLOCK {
                    set_bit(words, lo_word, buf[i]);
                    set_bit(words, lo_word, buf[BLOCK + i]);
                }
            } else {
                if n > 1 {
                    let mut d = self.decoder(off, n);
                    // SAFETY: the block has n - 1 coded gaps.
                    unsafe { d.block::<PAIR>(base.wrapping_sub(1), &mut buf[..n - 1]) };
                }
                buf[n - 1] = max;
                for &d in &buf[..n] {
                    set_bit(words, lo_word, d);
                }
            }
        }
    }

    /// Bits of the model (shared index or own model), included in `size_bits`.
    pub fn model_bits(&self) -> u64 {
        self.model_bits
    }

    /// Bits of the skip table, included in `size_bits`.
    pub fn skip_bits(&self) -> u64 {
        if self.nblocks == 1 {
            // A one-element Elias-Fano sequence (not materialised: the
            // list's last docID is the only maximum).
            skip::ef_size_bits(1, self.last as u64 + 1)
        } else {
            self.skip().maxes.size_bits() + self.skip().offsets.size_bits()
        }
    }

    /// Number of contexts of the model (0 if none).
    pub fn num_contexts(&self) -> usize {
        self.table().len() >> self.r
    }

    /// Bytes of the list's own decode table (memory derived from the model
    /// bits, not part of `size_bits`; 0 for shared models).
    pub fn own_table_bytes(&self) -> usize {
        match &self.table {
            Table::Own(t) => 8 * t.len(),
            _ => 0,
        }
    }

    /// Whether the list uses its own (fitted) model.
    pub fn has_own_model(&self) -> bool {
        matches!(self.table, Table::Own(_))
    }
}

impl PostingList for Entropic {
    const NAME: &'static str = "Entropic";

    fn build(docs: &[u32], universe: u32) -> Self {
        let n = docs.len();
        let nblocks = n.div_ceil(BLOCK);
        let mut list = Entropic {
            n,
            nblocks,
            last: docs.last().copied().unwrap_or(0),
            skip: None,
            data: Vec::new(),
            data_bits: 0,
            model_bits: 0,
            table: Table::None,
            start_ctx: 0,
            r: model::R_MAX,
            pair: false,
        };
        if n == 0 {
            return list;
        }
        // Gaps of every block but its last posting.
        let mut gaps: Vec<u32> = Vec::with_capacity(n);
        let mut maxes = Vec::with_capacity(nblocks);
        let mut stats = Stats::new();
        let mut toks: Vec<u8> = Vec::with_capacity(BLOCK);
        let mut base = 0u32;
        for chunk in docs.chunks(BLOCK) {
            let mut prev = base.wrapping_sub(1);
            toks.clear();
            for &d in &chunk[..chunk.len() - 1] {
                let g = d.wrapping_sub(prev).wrapping_sub(1);
                gaps.push(g);
                toks.push(token(g).0 as u8);
                prev = d;
            }
            stats.add_block(&toks);
            let last = *chunk.last().unwrap();
            maxes.push(last);
            base = last.wrapping_add(1);
        }

        let mut bits = BitVec::new();
        let mut offsets = Vec::with_capacity(nblocks);
        if stats.symbols > 0 {
            let max_tok = stats.max_token() as u8;
            debug_assert!(max_tok as usize <= token(universe.max(1) - 1).0.max(token(list.last).0));
            // Model selector: '0' auto (parameter-free), '10' shared
            // (fitted parameters), '11' own.
            let auto = SharedParams::auto(universe, n);
            let auto_cost = auto.cost(&stats) + 1.0;
            // A 22-bit index does not pay off on a handful of gaps.
            let (shared, shared_cost) = if stats.symbols >= SHARED_MIN_SYMBOLS {
                let (p, c) = SharedParams::fit(&stats, max_tok);
                (p, c + (2 + SHARED_INDEX_BITS) as f64)
            } else {
                (auto, f64::INFINITY)
            };
            let own = (stats.symbols >= OWN_MIN_SYMBOLS).then(|| fit_own(&stats));
            let own_cost = own.as_ref().map_or(f64::INFINITY, |o| o.1 + 2.0);
            let model: Model = if own_cost < shared_cost.min(auto_cost) {
                // Store the model, then rebuild it from its serialised form
                // so that coder and size agree exactly.
                let own = own.unwrap().0;
                let mut mb = BitVec::new();
                own.write(&mut mb);
                let (back, used) = OwnModel::read(mb.words(), 0);
                debug_assert_eq!(back, own);
                list.model_bits = 2 + used as u64;
                let m = back.model();
                list.table = Table::Own(decode_table(&m));
                list.pair = model::NRAW[max_tok as usize] + m.r <= PAIR_MAX_BITS;
                m
            } else {
                let (p, bits) = if auto_cost <= shared_cost { (auto, 1) } else { (shared, 2 + SHARED_INDEX_BITS) };
                list.model_bits = bits;
                list.table = Table::Shared(shared_table(p));
                list.pair = model::NRAW[p.max_tok as usize] + p.r as u32 <= PAIR_MAX_BITS;
                p.model()
            };
            list.start_ctx = model.start_ctx as u32;
            list.r = model.r;
            let enc = Encoder::new(&model);
            let mut scratch = Vec::new();
            let mut start = 0usize;
            for b in 0..nblocks {
                let len = if b + 1 < nblocks { BLOCK } else { n - b * BLOCK };
                if len > 1 {
                    if b > 0 {
                        offsets.push(u32::try_from(bits.len()).expect("posting list stream larger than 512 MiB"));
                    }
                    enc.encode_block(&gaps[start..start + len - 1], &mut bits, &mut scratch);
                }
                start += len - 1;
            }
        }
        if nblocks > 1 {
            list.skip = Some(Box::new(Skip {
                maxes: Ef::new(&maxes, list.last as u64 + 1, false, true),
                offsets: Ef::new(&offsets, bits.len() as u64 + 1, true, false),
            }));
        }
        list.data_bits = bits.len() as u64;
        if !bits.is_empty() {
            let bytes = bits.len().div_ceil(8);
            list.data = bits.words().iter().flat_map(|w| w.to_le_bytes()).take(bytes).collect();
            list.data.resize(bytes + PADDING, 0);
            list.data.shrink_to_fit();
        }
        list
    }

    fn len(&self) -> usize {
        self.n
    }

    fn size_bits(&self) -> u64 {
        self.data_bits + self.model_bits + self.skip_bits()
    }

    fn decode_into(&self, out: &mut Vec<u32>) {
        let start = out.len();
        out.resize(start + self.n, 0);
        self.decode_all(&mut out[start..]);
    }

    fn aux_memory_bytes(&self) -> u64 {
        self.own_table_bytes() as u64
    }

    fn shared_aux_memory_bytes() -> u64 {
        shared_tables().1 as u64
    }
}

/// Cursor over an [`Entropic`] list: decodes the current block lazily, as
/// far as the targets require (two gaps per bit-buffer refill when the
/// list allows it).
pub struct EntropicCursor<'a> {
    list: &'a Entropic,
    cur: u32,
    /// Postings of the block after `cur` (the last one is implicit).
    left: u32,
    block: usize,
    /// Position of the block's one bit in the maxima's high bits.
    mpos: usize,
    /// Last docID of the block.
    max: u32,
    dec: Decoder,
}

impl<'a> EntropicCursor<'a> {
    fn new(list: &'a Entropic) -> Self {
        let mut c = Self { list, cur: END, left: 0, block: 0, mpos: 0, max: END, dec: Decoder::empty() };
        if list.n > 0 {
            if list.nblocks == 1 {
                c.load(0, 0, list.last, 0);
            } else {
                let maxes = &list.skip().maxes;
                let mpos = maxes.next_one(0);
                c.load(0, mpos, maxes.value_at(0, mpos), 0);
            }
            c.step();
        }
        c
    }

    /// Position before the first posting of block `b`.
    #[inline(always)]
    fn load(&mut self, b: usize, mpos: usize, max: u32, base: u32) {
        self.block = b;
        self.mpos = mpos;
        self.max = max;
        let n = self.list.block_len(b);
        self.left = n as u32;
        self.cur = base.wrapping_sub(1);
        if n > 1 {
            let off = if b == 0 { 0 } else { self.list.skip().offsets.get(b - 1) as usize };
            self.dec = self.list.decoder(off, n);
        }
    }

    /// Advance within the block (`left >= 1`).
    #[inline(always)]
    fn step(&mut self) -> u32 {
        debug_assert!(self.left >= 1);
        self.left -= 1;
        if self.left == 0 {
            self.cur = self.max;
        } else {
            // SAFETY: `left` coded gaps remain; the last LANES of them end
            // their lanes.
            let g = unsafe { if self.left as usize > LANES { self.dec.gap() } else { self.dec.gap_last() } };
            self.cur = self.cur.wrapping_add(g).wrapping_add(1);
        }
        self.cur
    }

    #[inline]
    fn exhaust(&mut self) -> u32 {
        self.block = self.list.nblocks.saturating_sub(1);
        self.left = 0;
        self.cur = END;
        self.max = END;
        END
    }

    /// Move to the first block whose maximum is `>= target` (which must be
    /// beyond the current block and at most the list's last docID).
    #[inline(never)]
    fn seek_block(&mut self, target: u32) {
        let maxes = &self.list.skip().maxes;
        let (b, p, max) = maxes.seek(self.block, self.mpos, target);
        let base = if b == self.block + 1 {
            self.max.wrapping_add(1)
        } else {
            maxes.value_at(b - 1, maxes.prev_one(p)).wrapping_add(1)
        };
        self.load(b, p, max, base);
    }

    /// Decode forward in the block to the first posting `>= target`
    /// (the block's maximum is `>= target`).
    #[inline(always)]
    fn scan<const PAIR: bool>(&mut self, target: u32) -> u32 {
        // Decode in locals so that the decoder state stays in registers.
        let (mut left, mut cur, mut dec) = (self.left, self.cur, self.dec);
        macro_rules! gap {
            ($g:expr) => {
                left -= 1;
                cur = cur.wrapping_add($g).wrapping_add(1);
                if cur >= target {
                    (self.left, self.cur, self.dec) = (left, cur, dec);
                    return cur;
                }
            };
        }
        // SAFETY: `left - 1` coded gaps remain, the last LANES of which end
        // their lanes; each decode below keeps `left >= 1`.
        const L1: u32 = LANES as u32 + 1;
        unsafe {
            if PAIR {
                while left > L1 + 1 {
                    dec.refill();
                    gap!(dec.gap_buffered());
                    gap!(dec.gap_buffered());
                }
            }
            while left > L1 {
                gap!(dec.gap());
            }
            while left > 1 {
                gap!(dec.gap_last());
            }
        }
        self.left = 0;
        self.cur = self.max;
        self.cur
    }
}

impl Cursor for EntropicCursor<'_> {
    #[inline]
    fn docid(&self) -> u32 {
        self.cur
    }

    #[inline]
    fn next(&mut self) -> u32 {
        if self.left == 0 {
            if self.cur == END || self.block + 1 >= self.list.nblocks {
                return self.exhaust();
            }
            let b = self.block + 1;
            let maxes = &self.list.skip().maxes;
            let p = maxes.next_one(self.mpos + 1);
            let base = self.max.wrapping_add(1);
            self.load(b, p, maxes.value_at(b, p), base);
        }
        self.step()
    }

    #[inline]
    fn next_geq(&mut self, target: u32) -> u32 {
        if self.cur >= target {
            return self.cur;
        }
        if target > self.max {
            if target > self.list.last {
                return self.exhaust();
            }
            self.seek_block(target);
        }
        if self.list.pair {
            self.scan::<true>(target)
        } else {
            self.scan::<false>(target)
        }
    }

    fn size(&self) -> usize {
        self.list.n
    }
}

impl CursorList for Entropic {
    type Cursor<'a> = EntropicCursor<'a>;

    fn cursor(&self) -> EntropicCursor<'_> {
        EntropicCursor::new(self)
    }
}

/// Set bit `d - 64 · lo_word` of `words`.
#[inline(always)]
fn set_bit(words: &mut [u64], lo_word: usize, d: u32) {
    words[(d >> 6) as usize - lo_word] |= 1u64 << (d & 63);
}

/// Write a group decoded by [`Entropic::simd_group`] (transposed, plus the
/// blocks' maxima) to `dst`, block after block.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline(always)]
fn transpose_group(tmp: &[[u32; SIMD_BLOCKS]; BLOCK - 1], maxs: &[u32; SIMD_BLOCKS], dst: &mut [u32]) {
    for (j, blk) in dst.chunks_exact_mut(BLOCK).enumerate() {
        for (x, row) in blk.iter_mut().zip(tmp.iter()) {
            *x = row[j];
        }
        blk[BLOCK - 1] = maxs[j];
    }
}

thread_local! {
    static CANDIDATES: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    static BITMAP: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// Run `f` with a thread-local buffer (a fresh one if it is already in use).
fn with_buf<T: Default, R>(key: &'static std::thread::LocalKey<RefCell<T>>, f: impl FnOnce(&mut T) -> R) -> R {
    key.with(|cell| match cell.try_borrow_mut() {
        Ok(mut v) => f(&mut v),
        Err(_) => f(&mut T::default()),
    })
}

impl SetOps for Entropic {
    fn intersect(lists: &[&Self], out: &mut Vec<u32>) {
        let mut order: Vec<&Self> = lists.to_vec();
        order.sort_by_key(|l| l.n);
        if order[0].n == 0 {
            return;
        }
        if order.len() == 1 {
            return order[0].decode_into(out);
        }
        with_buf(&CANDIDATES, |cand| {
            cand.clear();
            order[0].decode_into(cand);
            for l in &order[1..] {
                let mut c = l.cursor();
                cand.retain(|&x| c.next_geq(x) == x);
                if cand.is_empty() {
                    break;
                }
            }
            out.extend_from_slice(cand);
        });
    }

    fn union(lists: &[&Self], out: &mut Vec<u32>) {
        let nonempty: Vec<&Self> = lists.iter().copied().filter(|l| l.n > 0).collect();
        match nonempty.len() {
            0 => return,
            1 => return nonempty[0].decode_into(out),
            _ => {}
        }
        let lo = nonempty.iter().map(|l| l.cursor().docid()).min().unwrap();
        let hi = nonempty.iter().map(|l| l.last).max().unwrap();
        let lo_word = (lo >> 6) as usize;
        let nwords = (hi >> 6) as usize - lo_word + 1;
        let total: usize = nonempty.iter().map(|l| l.n).sum();
        with_buf(&BITMAP, |words| {
            words.clear();
            words.resize(nwords, 0);
            for l in &nonempty {
                l.set_bits(words, lo_word);
            }
            out.reserve(total + EXTRACT_SLACK);
            let start = out.len();
            // SAFETY: room for every set bit (at most `total`) plus the slack.
            unsafe {
                let dst = out.as_mut_ptr().add(start);
                let end = <u32 as BitPos>::extract(0..nwords, |i| *words.get_unchecked(i), (lo_word as u32) << 6, dst);
                out.set_len(start + end.offset_from(dst) as usize);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::conformance;

    use rand::{rngs::StdRng, Rng, SeedableRng};

    #[test]
    fn conformance() {
        conformance::check_roundtrip::<Entropic>();
        conformance::check_cursor::<Entropic>();
        conformance::check_setops::<Entropic>();
    }

    /// Lists exercising every decode path: the eight-block AVX-512 decoder
    /// (pair-safe lists with at least eight full blocks), the two-block and
    /// single-block scalar decoders, lists that need a refill per symbol
    /// (gaps of 2^20 and more), and all three model kinds.
    fn path_lists() -> Vec<(Vec<u32>, u32)> {
        let mut rng = StdRng::seed_from_u64(77);
        let mut out = Vec::new();
        for n in [1usize, 2, 3, 127, 128, 129, 255, 1000, 1024 + 5, 8 * 128, 8 * 128 + 1, 17 * 128 + 3, 60_000] {
            // Runs and short jumps (clustered), uniform, and huge gaps.
            let mut d = 0u32;
            let runs: Vec<u32> = (0..n)
                .map(|_| {
                    d += if rng.gen_bool(0.7) { 1 } else { rng.gen_range(2..200) };
                    d
                })
                .collect();
            out.push((runs, d + 1));
            let u = (n as u32).saturating_mul(50).max(64);
            out.push((conformance::random_sorted(&mut rng, n, u), u));
            if n <= 2000 {
                let mut d = 0u32;
                let huge: Vec<u32> = (0..n)
                    .map(|_| {
                        d += rng.gen_range(1..(u32::MAX - 1) / (n as u32 + 1));
                        d
                    })
                    .collect();
                out.push((huge, u32::MAX));
            }
        }
        out
    }

    #[test]
    fn decode_paths_agree() {
        for (docs, universe) in path_lists() {
            let l = Entropic::build(&docs, universe);
            let mut out = vec![1, 2, 3];
            l.decode_into(&mut out);
            assert_eq!(&out[..3], &[1, 2, 3]);
            assert!(out[3..] == docs[..], "decode_into, n={}", docs.len());
            // Cursor traversal (gap-by-gap decoding).
            let mut c = l.cursor();
            let mut got = Vec::new();
            while c.docid() != END {
                got.push(c.docid());
                c.next();
            }
            assert!(got == docs, "cursor, n={}", docs.len());
            // Union with itself (bitmap path) and with an empty list.
            let empty = Entropic::build(&[], universe);
            let mut u = Vec::new();
            Entropic::union(&[&l, &empty, &l], &mut u);
            assert!(u == docs, "union, n={}", docs.len());
            // Intersection with every other posting.
            let half: Vec<u32> = docs.iter().copied().step_by(2).collect();
            let h = Entropic::build(&half, universe);
            let mut i = Vec::new();
            Entropic::intersect(&[&l, &h], &mut i);
            assert!(i == half, "intersect, n={}", docs.len());
        }
    }

    #[test]
    fn model_selection() {
        // Tiny lists take the parameter-free model (a 1-bit selector).
        let tiny = Entropic::build(&[3, 17, 90], 1000);
        assert_eq!(tiny.model_bits(), 1);
        // A long clustered list pays for its own model.
        let mut d = 0u32;
        let long: Vec<u32> = (0..100_000u32)
            .map(|i| {
                d += if i % 7 < 5 { 1 } else { 1 + (i * 7919) % 300 };
                d
            })
            .collect();
        let l = Entropic::build(&long, d + 1);
        assert!(l.has_own_model() && l.num_contexts() >= 1);
        // Size accounting: every part is counted.
        assert_eq!(l.size_bits(), l.data_bits + l.model_bits() + l.skip_bits());
        // Empty and single-posting lists.
        assert_eq!(Entropic::build(&[], 10).size_bits(), 0);
        let one = Entropic::build(&[u32::MAX - 1], u32::MAX);
        assert_eq!(one.model_bits(), 0);
        assert!(one.size_bits() > 0 && one.size_bits() <= 40);
    }
}
