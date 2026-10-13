//! Term dictionary: sorted terms in front-coded blocks of [`DICT_BLOCK`].
//!
//! ```text
//! u64 n_terms
//! array<u64> index   per block a 4-word record, then a sentinel record:
//!                    key (first 8 bytes of the block's first term,
//!                    big-endian, zero-padded), block offset | first-term
//!                    offset << 32, postings offset and positions offset of
//!                    the block's first term
//! array<u64> top     the key of every TOP_EVERY-th block
//! array<u8>  firsts  every block's first term, concatenated
//! array<u8>  blocks  entries: [prefix len, suffix len, suffix] (not for
//!                    the first entry), df, postings length, positions
//!                    length (varints)
//! ```
//!
//! A lookup binary-searches `top` (one page), then the records of one
//! index page (128 records), comparing keys and reading a first term only
//! when keys tie, then scans one block: about three pages from a cold
//! cache. Nothing is built at open. A term's postings start where the
//! previous term's end, so entries store lengths and offsets are summed
//! within the block.

use std::cmp::Ordering;

use posting_bench::bytes::{ByteReader, ByteWriter, OwnedBytes, Slice};

use super::codec::{get_varint, put_varint};
use crate::error::{corrupt, Result};

/// Terms per front-coded block.
pub const DICT_BLOCK: usize = 32;

/// Words per index record.
const REC: usize = 4;

/// Blocks per `top` key: the records of one 4 KiB page.
const TOP_EVERY: usize = 4096 / (8 * REC);

/// What the dictionary knows about a term.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermInfo {
    /// Rank of the term in sorted order.
    pub ord: u32,
    /// Document frequency (length of the posting list).
    pub df: u32,
    /// Byte range of the posting list in the postings section.
    pub post_off: u64,
    pub post_len: u64,
    /// Byte range of the positions in the positions section (empty without
    /// positions).
    pub pos_off: u64,
    pub pos_len: u64,
}

/// The first 8 bytes of `t`, big-endian and zero-padded: keys order like
/// their terms (analysed terms hold no zero bytes), up to ties.
#[inline]
fn key(t: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    let n = t.len().min(8);
    b[..n].copy_from_slice(&t[..n]);
    u64::from_be_bytes(b)
}

/// Writes a dictionary from terms added in increasing byte order.
#[derive(Default)]
pub struct DictBuilder {
    n: usize,
    index: Vec<u64>,
    firsts: Vec<u8>,
    blocks: Vec<u8>,
    prev: Vec<u8>,
    post_off: u64,
    pos_off: u64,
}

impl DictBuilder {
    /// An empty dictionary.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the next term (strictly greater than the previous one) whose
    /// postings and positions occupy the next `post_len` and `pos_len` bytes.
    pub fn add(&mut self, term: &[u8], df: u32, post_len: u64, pos_len: u64) {
        assert!(self.n == 0 || term > &self.prev[..], "terms must be added in increasing order");
        assert!(!term.contains(&0), "terms must not contain zero bytes");
        if self.n.is_multiple_of(DICT_BLOCK) {
            self.push_record(key(term));
            self.firsts.extend_from_slice(term);
        } else {
            let prefix = term.iter().zip(&self.prev).take_while(|(a, b)| a == b).count();
            put_varint(&mut self.blocks, prefix as u64);
            put_varint(&mut self.blocks, (term.len() - prefix) as u64);
            self.blocks.extend_from_slice(&term[prefix..]);
        }
        put_varint(&mut self.blocks, df as u64);
        put_varint(&mut self.blocks, post_len);
        put_varint(&mut self.blocks, pos_len);
        self.prev.clear();
        self.prev.extend_from_slice(term);
        self.post_off += post_len;
        self.pos_off += pos_len;
        self.n += 1;
    }

    fn push_record(&mut self, key: u64) {
        assert!(self.blocks.len() < u32::MAX as usize && self.firsts.len() < u32::MAX as usize, "dictionary too large");
        self.index.extend([key, self.blocks.len() as u64 | (self.firsts.len() as u64) << 32, self.post_off, self.pos_off]);
    }

    /// The dictionary's bytes.
    pub fn finish(mut self) -> Vec<u8> {
        // The sentinel record ends the last block.
        self.push_record(u64::MAX);
        let top: Vec<u64> = self.index.chunks(REC).take(self.n.div_ceil(DICT_BLOCK)).step_by(TOP_EVERY).map(|r| r[0]).collect();
        let mut w = ByteWriter::new();
        w.u64(self.n as u64);
        w.array(&self.index);
        w.array(&top);
        w.array(&self.firsts);
        // Entries are read without bounds checks on varint boundaries; the
        // trailing padding of `array` keeps reads of a truncated entry
        // inside the buffer.
        w.array(&self.blocks);
        w.finish()
    }
}

/// Length of the common prefix of `a` and `b`.
#[inline]
fn lcp(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// A zero-copy view of a dictionary written by [`DictBuilder`].
#[derive(Clone, Debug)]
pub struct Dict {
    n_terms: u32,
    n_blocks: usize,
    index: Slice<u64>,
    top: Slice<u64>,
    firsts: Slice<u8>,
    blocks: Slice<u8>,
}

impl Dict {
    /// Views a dictionary written by [`DictBuilder::finish`]; checks its
    /// array lengths only.
    pub fn open(bytes: OwnedBytes) -> Result<Self> {
        let mut r = ByteReader::new(bytes);
        let n_terms = r.u64().map_err(corrupt)?;
        let n_blocks = (n_terms as usize).div_ceil(DICT_BLOCK);
        let d = Dict {
            n_terms: u32::try_from(n_terms).map_err(corrupt)?,
            n_blocks,
            index: r.array().map_err(corrupt)?,
            top: r.array().map_err(corrupt)?,
            firsts: r.array().map_err(corrupt)?,
            blocks: r.array().map_err(corrupt)?,
        };
        let ok = d.index.len() == REC * (n_blocks + 1)
            && d.top.len() == n_blocks.div_ceil(TOP_EVERY)
            && d.block_off(n_blocks) == d.blocks.len()
            && d.first_off(n_blocks) == d.firsts.len();
        if !ok {
            return Err(corrupt("inconsistent term dictionary"));
        }
        Ok(d)
    }

    /// Number of terms.
    pub fn n_terms(&self) -> u32 {
        self.n_terms
    }

    #[inline]
    fn block_off(&self, b: usize) -> usize {
        self.index[REC * b + 1] as u32 as usize
    }

    #[inline]
    fn first_off(&self, b: usize) -> usize {
        (self.index[REC * b + 1] >> 32) as usize
    }

    fn first(&self, b: usize) -> &[u8] {
        &self.firsts[self.first_off(b)..self.first_off(b + 1)]
    }

    /// Orders block `b`'s first term against `t` (key `k`).
    #[inline]
    fn cmp_first(&self, b: usize, t: &[u8], k: u64) -> Ordering {
        match self.index[REC * b].cmp(&k) {
            Ordering::Equal => self.first(b).cmp(t),
            o => o,
        }
    }

    /// Number of leading blocks whose first term is below `t` (`or_equal`:
    /// at most `t`).
    fn blocks_below(&self, t: &[u8], or_equal: bool) -> usize {
        let k = key(t);
        let below = |o: Ordering| o == Ordering::Less || (or_equal && o == Ordering::Equal);
        // Index pages whose first key is below k hold only such blocks
        // (keys tie only for terms sharing 8 bytes); the answer is in the
        // pages from the last of them to the first key above k.
        let lo_page = self.top.partition_point(|&x| x < k).saturating_sub(1);
        let hi_page = self.top.partition_point(|&x| x <= k).max(lo_page + 1);
        let (mut lo, mut hi) = (lo_page * TOP_EVERY, (hi_page * TOP_EVERY).min(self.n_blocks));
        while lo < hi {
            let mid = (lo + hi) / 2;
            if below(self.cmp_first(mid, t, k)) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// The term's entry, if present.
    ///
    /// Scans the block without rebuilding its terms: with `m` the common
    /// prefix of the current term (which is below `t`) and `t`, an entry
    /// sharing fewer than `m` bytes with its predecessor is above `t`, one
    /// sharing more is still below it, and only one sharing exactly `m`
    /// needs its suffix compared.
    pub fn get(&self, term: &str) -> Option<TermInfo> {
        let t = term.as_bytes();
        // Last block whose first term is <= t.
        let b = self.blocks_below(t, true).checked_sub(1)?;
        let data = &self.blocks[..self.block_off(b + 1)];
        let mut pos = self.block_off(b);
        let (mut post_off, mut pos_off) = (self.index[REC * b + 2], self.index[REC * b + 3]);
        let first = self.first(b);
        let mut m = lcp(first, t);
        let mut equal = m == first.len() && m == t.len();
        let end = ((b + 1) * DICT_BLOCK).min(self.n_terms as usize);
        for ord in b * DICT_BLOCK..end {
            if !ord.is_multiple_of(DICT_BLOCK) {
                let p = get_varint(data, &mut pos) as usize;
                let len = get_varint(data, &mut pos) as usize;
                let suffix = &data[pos..pos + len];
                pos += len;
                match p.cmp(&m) {
                    Ordering::Less => return None,
                    Ordering::Greater => {}
                    Ordering::Equal => {
                        let rest = &t[m..];
                        let l = lcp(suffix, rest);
                        if l < suffix.len() && (l == rest.len() || suffix[l] > rest[l]) {
                            return None;
                        }
                        m += l;
                        equal = l == suffix.len() && l == rest.len();
                    }
                }
            }
            let df = get_varint(data, &mut pos) as u32;
            let post_len = get_varint(data, &mut pos);
            let pos_len = get_varint(data, &mut pos);
            if equal {
                return Some(TermInfo { ord: ord as u32, df, post_off, post_len, pos_off, pos_len });
            }
            post_off += post_len;
            pos_off += pos_len;
        }
        None
    }

    /// Up to `limit` terms starting with `prefix`, in order.
    pub fn prefix(&self, prefix: &str, limit: usize) -> Vec<(String, TermInfo)> {
        let mut out = Vec::new();
        if limit == 0 {
            return out;
        }
        self.for_each_prefix(prefix, |t, info| {
            out.push((String::from_utf8_lossy(t).into_owned(), info));
            out.len() < limit
        });
        out
    }

    /// Calls `f` with every term starting with `prefix`, in order, until it
    /// returns false.
    pub fn for_each_prefix(&self, prefix: &str, mut f: impl FnMut(&[u8], TermInfo) -> bool) {
        let p = prefix.as_bytes();
        // Terms >= prefix start in the last block whose first term is < prefix.
        let b0 = self.blocks_below(p, false).saturating_sub(1);
        for b in b0..self.n_blocks {
            let mut more = true;
            self.scan_block(b, |term, info| {
                more = if term.starts_with(p) {
                    f(term, info)
                } else {
                    // Before the matching run: keep going; after it: stop.
                    term < p
                };
                more
            });
            if !more {
                break;
            }
        }
    }

    /// Every term with its entry, in order.
    pub fn for_each(&self, mut f: impl FnMut(&[u8], TermInfo)) {
        for b in 0..self.n_blocks {
            self.scan_block(b, |t, info| {
                f(t, info);
                true
            });
        }
    }

    /// Decodes block `b` in order, calling `f(term, info)` until it returns
    /// false.
    fn scan_block(&self, b: usize, mut f: impl FnMut(&[u8], TermInfo) -> bool) {
        let data = &self.blocks[..self.block_off(b + 1)];
        let mut pos = self.block_off(b);
        let mut term: Vec<u8> = self.first(b).to_vec();
        let (mut post_off, mut pos_off) = (self.index[REC * b + 2], self.index[REC * b + 3]);
        let end = ((b + 1) * DICT_BLOCK).min(self.n_terms as usize);
        for ord in b * DICT_BLOCK..end {
            if !ord.is_multiple_of(DICT_BLOCK) {
                let prefix = get_varint(data, &mut pos) as usize;
                let suffix = get_varint(data, &mut pos) as usize;
                term.truncate(prefix);
                term.extend_from_slice(&data[pos..pos + suffix]);
                pos += suffix;
            }
            let df = get_varint(data, &mut pos) as u32;
            let post_len = get_varint(data, &mut pos);
            let pos_len = get_varint(data, &mut pos);
            if !f(&term, TermInfo { ord: ord as u32, df, post_off, post_len, pos_off, pos_len }) {
                return;
            }
            post_off += post_len;
            pos_off += pos_len;
        }
    }

    /// Bytes of the dictionary's arrays (index and blocks).
    pub fn byte_len(&self) -> usize {
        self.index.byte_len() + self.top.byte_len() + self.firsts.byte_len() + self.blocks.byte_len()
    }

    /// Whether every array is a view into the section (no copies).
    pub fn is_shared(&self) -> bool {
        matches!(self.index, Slice::Shared { .. })
            && matches!(self.top, Slice::Shared { .. })
            && matches!(self.firsts, Slice::Shared { .. })
            && matches!(self.blocks, Slice::Shared { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lookups and prefixes across blocks and index pages, with terms that
    /// tie on their 8-byte keys.
    #[test]
    fn lookups_with_key_ties() {
        let mut terms: Vec<String> = (0..20_000).map(|i| format!("t{i:06}")).collect();
        // Long runs sharing 8 bytes, spanning several index pages.
        terms.extend((0..9000).map(|i| format!("internationalisation{i:05}")));
        terms.extend(["a", "ab", "abc", "zz", "zzzzzzzz", "zzzzzzzza", "zzzzzzzzb"].map(String::from));
        terms.sort();
        let mut b = DictBuilder::new();
        for (i, t) in terms.iter().enumerate() {
            b.add(t.as_bytes(), i as u32 + 1, 3, 1);
        }
        let d = Dict::open(OwnedBytes::new(b.finish())).unwrap();
        assert_eq!(d.n_terms() as usize, terms.len());
        for (i, t) in terms.iter().enumerate() {
            let info = d.get(t).unwrap_or_else(|| panic!("{t}"));
            assert_eq!((info.ord as usize, info.df, info.post_off, info.pos_off), (i, i as u32 + 1, 3 * i as u64, i as u64));
        }
        for absent in ["", "0", "aa", "abd", "internationalisation", "internationalisation99999", "t", "t1", "zzz", "zzzzzzzzz", "~"] {
            assert_eq!(d.get(absent), None, "{absent}");
        }
        for p in ["", "a", "internationalisation0", "internationalisation088", "t01", "zzzzzzzz", "x"] {
            let want: Vec<&String> = terms.iter().filter(|t| t.starts_with(p)).collect();
            let got = d.prefix(p, usize::MAX);
            assert!(got.iter().map(|(t, _)| t).eq(want), "prefix {p:?}");
        }
        let mut n = 0;
        d.for_each(|t, info| {
            assert_eq!(t, terms[n].as_bytes());
            assert_eq!(info.ord as usize, n);
            n += 1;
        });
        assert_eq!(n, terms.len());
    }
}
