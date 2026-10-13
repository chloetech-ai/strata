//! Primary-key index: section `pk` of every segment of a table with a
//! primary key.
//!
//! ```text
//! kind: u64 (1 = KEYWORD, 2 = INT) | n: u64 | dir_bits: u64
//! dir:     [u32; 2^dir_bits + 1]  first entry whose hash has each top-bit prefix
//! entries: [u64; 2n]              (hash, docID) pairs, sorted by hash, then docID
//! KEYWORD: offsets [u64; n + 1], bytes [u8]   the key of each document, in docID order
//! INT:     keys [i64; n]
//! ```
//!
//! Key hashes are uniform, so a radix directory over their top bits (about
//! four entries per slot) turns a lookup into one directory read and a short
//! scan: a cold lookup touches a constant number of pages (directory, entry,
//! stored key) where a binary search over the entries touches about
//! `log2(n) - 9`. Hash collisions are resolved by comparing the stored key.
//! (hash, docID) pairs are interleaved so that a probe reads one cache line.

use posting_bench::bytes::{ByteReader, ByteWriter, OwnedBytes, Slice};

use crate::error::{corrupt, Error, Result};
use crate::schema::{FieldType, Schema};
use crate::segment::{DocId, SegmentFile};
use crate::value::{Document, Value};

/// Section name of the primary-key index.
pub const SECTION: &str = "pk";

const KIND_STR: u64 = 1;
const KIND_INT: u64 = 2;

/// A primary-key value, borrowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyRef<'a> {
    Int(i64),
    Str(&'a [u8]),
}

impl<'a> KeyRef<'a> {
    /// The key of a value, if it can be one (KEYWORD or INT).
    pub fn of(v: &'a Value) -> Option<KeyRef<'a>> {
        match v {
            Value::Int(x) => Some(KeyRef::Int(*x)),
            Value::Str(s) => Some(KeyRef::Str(s.as_bytes())),
            _ => None,
        }
    }
}

/// The 64-bit hash of a key. It is part of the file format: changing it
/// invalidates every stored `pk` section.
pub fn key_hash(k: KeyRef) -> u64 {
    match k {
        KeyRef::Int(v) => hash_int(v),
        KeyRef::Str(s) => hash_bytes(s),
    }
}

/// Hash function type, overridable in tests to force collisions.
pub(crate) type HashFn = fn(KeyRef) -> u64;

const P0: u64 = 0xa076_1d64_78bd_642f;
const P1: u64 = 0xe703_7ed1_a0b4_28db;
const P2: u64 = 0x8ebc_6af0_9c88_c6e3;

#[inline]
fn mum(a: u64, b: u64) -> u64 {
    let r = (a as u128) * (b as u128);
    (r as u64) ^ ((r >> 64) as u64)
}

/// Multiply-fold hash over 16-byte blocks (in the style of wyhash).
fn hash_bytes(s: &[u8]) -> u64 {
    let mut h = P0 ^ (s.len() as u64).wrapping_mul(P2);
    let mut blocks = s.chunks_exact(16);
    for c in &mut blocks {
        let a = u64::from_le_bytes(c[..8].try_into().unwrap());
        let b = u64::from_le_bytes(c[8..].try_into().unwrap());
        h = mum(a ^ P1, b ^ h);
    }
    let rest = blocks.remainder();
    let mut tail = [0u8; 16];
    tail[..rest.len()].copy_from_slice(rest);
    let a = u64::from_le_bytes(tail[..8].try_into().unwrap());
    let b = u64::from_le_bytes(tail[8..].try_into().unwrap());
    h = mum(a ^ P1, b ^ h);
    mum(h ^ P2, (s.len() as u64) ^ P1)
}

/// The splitmix64 finaliser: a bijection, so distinct integers never collide.
fn hash_int(v: i64) -> u64 {
    let mut z = (v as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Directory size: about four entries per slot, at most 2^24 slots.
fn dir_bits(n: usize) -> u32 {
    if n < 8 {
        0
    } else {
        (n.ilog2() - 2).min(24)
    }
}

#[inline]
fn bucket(h: u64, bits: u32) -> usize {
    h.checked_shr(64 - bits).unwrap_or(0) as usize
}

/// Writes the primary-key index section of a segment (nothing if the table
/// has no primary key).
pub fn build_pk_index(schema: &Schema, docs: &[Document], out: &mut crate::segment::SegmentWriter) -> Result<()> {
    let Some(pk) = schema.primary_key else { return Ok(()) };
    let keys: Vec<&Value> = docs.iter().map(|d| &d[pk]).collect();
    out.put(SECTION, build(&schema.fields[pk].ty, &keys, key_hash)?);
    Ok(())
}

/// Encodes the index of `keys` (key of document `i` at position `i`).
pub(crate) fn build(ty: &FieldType, keys: &[&Value], hash: HashFn) -> Result<Vec<u8>> {
    let n = keys.len();
    let kind = match ty {
        FieldType::Keyword => KIND_STR,
        FieldType::Int => KIND_INT,
        _ => return Err(Error::Schema("primary key must be KEYWORD or INT".into())),
    };
    let mut pairs: Vec<(u64, u32)> = Vec::with_capacity(n);
    for (d, v) in keys.iter().enumerate() {
        let k = match (kind, v) {
            (KIND_STR, Value::Str(s)) => KeyRef::Str(s.as_bytes()),
            (KIND_INT, Value::Int(x)) => KeyRef::Int(*x),
            _ => return Err(Error::Schema(format!("bad primary key value {v:?}"))),
        };
        pairs.push((hash(k), d as u32));
    }
    pairs.sort_unstable();
    let bits = dir_bits(n);
    let mut dir = vec![0u32; (1usize << bits) + 1];
    let mut i = 0;
    for (b, slot) in dir.iter_mut().enumerate() {
        while i < n && bucket(pairs[i].0, bits) < b {
            i += 1;
        }
        *slot = i as u32;
    }
    let entries: Vec<u64> = pairs.iter().flat_map(|&(h, d)| [h, d as u64]).collect();
    let mut w = ByteWriter::new();
    w.u64(kind);
    w.u64(n as u64);
    w.u64(bits as u64);
    w.array(&dir);
    w.array(&entries);
    if kind == KIND_STR {
        let mut offsets = Vec::with_capacity(n + 1);
        let mut bytes = Vec::new();
        offsets.push(0u64);
        for v in keys {
            bytes.extend_from_slice(v.as_str().unwrap().as_bytes());
            offsets.push(bytes.len() as u64);
        }
        w.array(&offsets);
        w.array(&bytes);
    } else {
        let ints: Vec<i64> = keys.iter().map(|v| v.as_i64().unwrap()).collect();
        w.array(&ints);
    }
    Ok(w.finish())
}

/// Zero-copy reader of a `pk` section.
#[derive(Clone, Debug)]
pub struct PkIndex {
    str_keys: bool,
    n: usize,
    bits: u32,
    dir: Slice<u32>,
    entries: Slice<u64>,
    offsets: Slice<u64>,
    bytes: Slice<u8>,
    ints: Slice<i64>,
    hash: HashFn,
    /// `hash` is [`key_hash`], whose integer hash is a bijection, so an
    /// integer key with an equal hash needs no comparison.
    bijective_ints: bool,
}

impl PkIndex {
    /// The index of a segment, or `None` if it has no `pk` section.
    pub fn open(seg: &SegmentFile) -> Result<Option<PkIndex>> {
        seg.section(SECTION).map(PkIndex::from_section).transpose()
    }

    pub fn from_section(bytes: OwnedBytes) -> Result<PkIndex> {
        let mut idx = PkIndex::from_section_with(bytes, key_hash)?;
        idx.bijective_ints = true;
        Ok(idx)
    }

    pub(crate) fn from_section_with(bytes: OwnedBytes, hash: HashFn) -> Result<PkIndex> {
        let mut r = ByteReader::new(bytes);
        let kind = r.u64().map_err(corrupt)?;
        let n = r.u64().map_err(corrupt)? as usize;
        let bits = r.u64().map_err(corrupt)? as u32;
        if !(kind == KIND_STR || kind == KIND_INT) || bits > 24 {
            return Err(corrupt("bad pk section header"));
        }
        let dir: Slice<u32> = r.array().map_err(corrupt)?;
        let entries: Slice<u64> = r.array().map_err(corrupt)?;
        if dir.len() != (1 << bits) + 1 || entries.len() != 2 * n || dir[dir.len() - 1] as usize != n {
            return Err(corrupt("pk section sizes disagree"));
        }
        let (mut offsets, mut bytes, mut ints) = (Slice::default(), Slice::default(), Slice::default());
        if kind == KIND_STR {
            offsets = r.array().map_err(corrupt)?;
            bytes = r.array().map_err(corrupt)?;
            if offsets.len() != n + 1 || offsets[n] as usize != bytes.len() {
                return Err(corrupt("pk key offsets disagree"));
            }
        } else {
            ints = r.array().map_err(corrupt)?;
            if ints.len() != n {
                return Err(corrupt("pk key array size"));
            }
        }
        Ok(PkIndex { str_keys: kind == KIND_STR, n, bits, dir, entries, offsets, bytes, ints, hash, bijective_ints: false })
    }

    /// Number of documents (keys) in the segment.
    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The document holding `key`. A segment holds each key at most once,
    /// except an in-memory memtable segment, which keeps deleted versions
    /// (use [`lookup_all`](Self::lookup_all) with the live filter there).
    pub fn lookup(&self, key: &Value) -> Option<DocId> {
        self.lookup_all(key).next()
    }

    /// Every document holding `key`, in increasing docID order.
    pub fn lookup_all<'a>(&'a self, key: &'a Value) -> impl Iterator<Item = DocId> + 'a {
        self.lookup_key(KeyRef::of(key))
    }

    /// Every document holding the borrowed key `k`, in increasing docID order.
    pub fn lookup_key<'a>(&'a self, k: Option<KeyRef<'a>>) -> impl Iterator<Item = DocId> + 'a {
        let h = k.map_or(0, |k| (self.hash)(k));
        self.lookup_hashed(k, h)
    }

    /// Like [`lookup_key`](Self::lookup_key) with the key's hash already
    /// computed (`h` must be this index's hash of `k`), so that a lookup over
    /// many segments hashes once.
    pub fn lookup_hashed<'a>(&'a self, k: Option<KeyRef<'a>>, h: u64) -> impl Iterator<Item = DocId> + 'a {
        let k = k.filter(|k| matches!(k, KeyRef::Str(_)) == self.str_keys);
        let range = match k {
            Some(_) => {
                let b = bucket(h, self.bits);
                self.dir[b] as usize..self.dir[b + 1] as usize
            }
            None => 0..0,
        };
        // The integer hash is a bijection: an equal hash is an equal key.
        let verify = self.str_keys || !self.bijective_ints;
        range
            .map(move |i| (self.entries[2 * i], self.entries[2 * i + 1] as DocId))
            .skip_while(move |&(eh, _)| eh < h)
            .take_while(move |&(eh, _)| eh == h)
            .filter(move |&(_, d)| !verify || Some(self.key_ref(d)) == k)
            .map(|(_, d)| d)
    }

    /// The key of document `doc`.
    pub fn key_ref(&self, doc: DocId) -> KeyRef<'_> {
        let d = doc as usize;
        if self.str_keys {
            KeyRef::Str(&self.bytes[self.offsets[d] as usize..self.offsets[d + 1] as usize])
        } else {
            KeyRef::Int(self.ints[d])
        }
    }

    /// The key of document `doc` as a value.
    pub fn key(&self, doc: DocId) -> Value {
        match self.key_ref(doc) {
            KeyRef::Int(v) => Value::Int(v),
            KeyRef::Str(b) => Value::Str(String::from_utf8_lossy(b).into_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Field;
    use crate::segment::SegmentWriter;
    use rand::{Rng, SeedableRng};

    fn index(ty: FieldType, keys: &[Value], hash: HashFn) -> PkIndex {
        let refs: Vec<&Value> = keys.iter().collect();
        PkIndex::from_section_with(OwnedBytes::new(build(&ty, &refs, hash).unwrap()), hash).unwrap()
    }

    #[test]
    fn random_string_and_int_keys_found() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        for n in [0usize, 1, 7, 8, 100, 5000] {
            let keys: Vec<Value> = (0..n).map(|i| Value::Str(format!("doc-{i}-{}", rng.gen::<u32>()))).collect();
            let pk = index(FieldType::Keyword, &keys, key_hash);
            assert_eq!(pk.len(), n);
            for (d, k) in keys.iter().enumerate() {
                assert_eq!(pk.lookup(k), Some(d as u32));
                assert_eq!(&pk.key(d as u32), k);
            }
            assert_eq!(pk.lookup(&Value::Str("absent".into())), None);
            assert_eq!(pk.lookup(&Value::Int(3)), None);

            let ints: Vec<Value> = (0..n).map(|_| Value::Int(rng.gen_range(-1_000_000_000..1_000_000_000))).collect();
            let mut seen = std::collections::HashSet::new();
            let ints: Vec<Value> = ints.into_iter().filter(|v| seen.insert(v.as_i64().unwrap())).collect();
            let pk = index(FieldType::Int, &ints, key_hash);
            for (d, k) in ints.iter().enumerate() {
                assert_eq!(pk.lookup(k), Some(d as u32));
                assert_eq!(&pk.key(d as u32), k);
            }
            assert_eq!(pk.lookup(&Value::Str("1".into())), None);
        }
    }

    #[test]
    fn collisions_resolved_by_stored_key() {
        // Every key hashes to one of three values.
        fn bad(k: KeyRef) -> u64 {
            match k {
                KeyRef::Str(s) => s.len() as u64 % 3,
                KeyRef::Int(v) => v.rem_euclid(3) as u64,
            }
        }
        let keys: Vec<Value> = (0..300).map(|i| Value::Str(format!("k{i}"))).collect();
        let pk = index(FieldType::Keyword, &keys, bad);
        for (d, k) in keys.iter().enumerate() {
            assert_eq!(pk.lookup(k), Some(d as u32));
        }
        assert_eq!(pk.lookup(&Value::Str("k300".into())), None);
        // Duplicates (the memtable segment keeps deleted versions) all come back.
        let dup: Vec<Value> = ["a", "b", "a", "c", "a"].iter().map(|s| Value::Str(s.to_string())).collect();
        let pk = index(FieldType::Keyword, &dup, bad);
        assert_eq!(pk.lookup_all(&dup[0]).collect::<Vec<_>>(), vec![0, 2, 4]);
    }

    #[test]
    fn hash_spreads_over_directory() {
        let n = 1 << 16;
        let keys: Vec<Value> = (0..n).map(|i| Value::Str(format!("{i}"))).collect();
        let refs: Vec<&Value> = keys.iter().collect();
        let pk = PkIndex::from_section(OwnedBytes::new(build(&FieldType::Keyword, &refs, key_hash).unwrap())).unwrap();
        let max_bucket = pk.dir.windows(2).map(|w| w[1] - w[0]).max().unwrap();
        assert!(max_bucket < 24, "largest bucket {max_bucket}");
        let mut hashes: Vec<u64> = keys.iter().map(|k| key_hash(KeyRef::of(k).unwrap())).collect();
        hashes.sort();
        hashes.dedup();
        assert_eq!(hashes.len(), n);
    }

    #[test]
    fn section_written_only_with_primary_key() {
        let schema = Schema::new("t", vec![Field::new("id", FieldType::Int)], Some("id")).unwrap();
        let docs = vec![vec![Value::Int(5)], vec![Value::Int(-2)]];
        let mut w = SegmentWriter::new(2);
        build_pk_index(&schema, &docs, &mut w).unwrap();
        let seg = SegmentFile::from_vec(w.finish()).unwrap();
        let pk = PkIndex::open(&seg).unwrap().unwrap();
        assert_eq!(pk.lookup(&Value::Int(-2)), Some(1));
        let schema = Schema::new("t", vec![Field::new("id", FieldType::Int)], None).unwrap();
        let mut w = SegmentWriter::new(2);
        build_pk_index(&schema, &docs, &mut w).unwrap();
        assert!(PkIndex::open(&SegmentFile::from_vec(w.finish()).unwrap()).unwrap().is_none());
    }
}
