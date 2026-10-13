//! In-memory state of an open database: the current on-disk version with
//! its opened segments, the memtable, and deletes not yet flushed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use posting_bench::bytes::OwnedBytes;

use crate::error::{corrupt, Result};
use crate::schema::Schema;
use crate::segment::{DocId, SegmentFile};
use crate::storage::freelist::FreeList;
use crate::storage::live::{words_for, LiveDocs};
use crate::storage::manifest::{Manifest, SegmentMeta};
use crate::storage::pager::{Extent, Meta};
use crate::storage::pk::PkIndex;
use crate::storage::snapshot::{MemSeg, SnapInner};
use crate::storage::wal::Op;
use crate::value::{Document, Value};

/// A primary-key value usable as a hash-map key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    Int(i64),
    Str(Box<str>),
}

impl Key {
    pub fn of(v: &Value) -> Option<Key> {
        match v {
            Value::Int(x) => Some(Key::Int(*x)),
            Value::Str(s) => Some(Key::Str(s.as_str().into())),
            _ => None,
        }
    }
}

/// An on-disk segment, opened: zero-copy views into the mapping.
pub(crate) struct SegHandle {
    pub meta: SegmentMeta,
    pub file: SegmentFile,
    pub pk: Option<PkIndex>,
    /// The stored delete set, if any.
    pub base: Option<Arc<LiveDocs>>,
    /// The mapping the views point into (for cache eviction).
    pub map: OwnedBytes,
}

impl SegHandle {
    /// `random_hints`: advise the segment's random-access sections
    /// `MADV_RANDOM` ([`SegmentFile::advise_random_sections`]).
    pub fn open(map: &OwnedBytes, meta: &SegmentMeta, random_hints: bool) -> Result<SegHandle> {
        let within = |e: &Extent| e.range().end <= map.len();
        if !within(&meta.extent) || meta.deletes.as_ref().is_some_and(|e| !within(e)) {
            return Err(corrupt(format!("segment {} lies outside the file", meta.id)));
        }
        let file = SegmentFile::open(map.slice(meta.extent.range()))?;
        if random_hints {
            file.advise_random_sections();
        }
        let pk = PkIndex::open(&file)?;
        if file.n_docs() != meta.n_docs as usize || pk.as_ref().is_some_and(|p| p.len() != file.n_docs()) {
            return Err(corrupt(format!("segment {} document count disagrees with manifest", meta.id)));
        }
        let base = match meta.deletes {
            Some(e) => Some(Arc::new(LiveDocs::view(map.slice(e.range()), meta.n_docs, meta.n_deleted)?)),
            None => None,
        };
        Ok(SegHandle { meta: meta.clone(), file, pk, base, map: map.clone() })
    }
}

/// Committed documents not yet flushed to a segment, append-only between
/// flushes. Documents are kept in chunks of `chunk` documents; each chunk
/// is one in-memory segment of a snapshot, so a write invalidates only the
/// segment of the last (partial) chunk and a full chunk is indexed once.
pub(crate) struct Memtable {
    /// Bumped by every flush; identifies the contents of `chunks`.
    pub gen: u64,
    /// Documents per chunk, a multiple of 64 so that a chunk's slice of
    /// `deleted` starts on a word.
    pub chunk: usize,
    /// Shared with snapshot builders, which index a prefix of a chunk.
    pub chunks: Vec<Arc<Vec<Document>>>,
    pub len: usize,
    /// Deleted documents by memtable position.
    pub deleted: Vec<u64>,
    pub n_deleted: usize,
    pub chunk_deleted: Vec<usize>,
    /// Live documents by primary key, with their memtable position.
    pub keys: HashMap<Key, u32>,
}

impl Memtable {
    pub fn new(chunk: usize, gen: u64) -> Memtable {
        let chunk = chunk.clamp(1, 1 << 32).next_multiple_of(64);
        Memtable {
            gen,
            chunk,
            chunks: Vec::new(),
            len: 0,
            deleted: Vec::new(),
            n_deleted: 0,
            chunk_deleted: Vec::new(),
            keys: HashMap::new(),
        }
    }

    /// Empties the memtable (after a flush).
    pub fn clear(&mut self) {
        *self = Memtable::new(self.chunk, self.gen + 1);
    }

    /// Appends a document; returns its position.
    pub fn push(&mut self, doc: Document) -> u32 {
        let i = self.len;
        if i.is_multiple_of(self.chunk) {
            self.chunks.push(Arc::new(Vec::new()));
            self.chunk_deleted.push(0);
        }
        // Copies the chunk only if a snapshot is being built from it.
        Arc::make_mut(self.chunks.last_mut().unwrap()).push(doc);
        self.len += 1;
        self.deleted.resize(words_for(self.len as u32), 0);
        i as u32
    }

    pub fn delete(&mut self, i: u32) {
        self.deleted[i as usize / 64] |= 1 << (i % 64);
        self.n_deleted += 1;
        self.chunk_deleted[i as usize / self.chunk] += 1;
    }

    /// The deleted bits of chunk `k`'s documents.
    pub fn chunk_deleted_words(&self, k: usize) -> Vec<u64> {
        let first = k * self.chunk / 64;
        self.deleted[first..first + words_for(self.chunks[k].len() as u32)].to_vec()
    }
}

/// Where the live document with a key is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Loc {
    Mem(u32),
    Disk(u64, DocId),
}

pub(crate) struct State {
    pub manifest: Manifest,
    pub meta: Meta,
    /// Meta slot holding `meta`; the next commit writes the other one.
    pub slot: usize,
    pub freelist: FreeList,
    /// File length in pages (may exceed `freelist.page_count()` after a
    /// failed commit).
    pub file_pages: u64,
    pub map: OwnedBytes,
    /// Opened segments, aligned with `manifest.segments`.
    pub handles: Vec<Arc<SegHandle>>,
    /// Latest committed txn (WAL commits included).
    pub txn: u64,
    /// Bumped by every change; a snapshot is cached only if it is current.
    pub version: u64,
    pub mem: Memtable,
    /// Deletes of on-disk documents, applied to delete sets at flush.
    pub pending: BTreeMap<u64, BTreeSet<DocId>>,
    pub n_pending: usize,
    /// Stored delete set merged with pending deletes, by segment id, keyed
    /// by the number of pending deletes it includes.
    pub live_cache: HashMap<u64, (usize, Arc<LiveDocs>)>,
    /// Live filter of each memtable chunk, keyed by the chunk's length and
    /// deletes (emptied by a flush).
    pub mem_live: Vec<Option<(usize, usize, Arc<LiveDocs>)>>,
    /// The latest segment built for each memtable chunk (emptied by a flush).
    pub memsegs: Vec<Option<MemSeg>>,
    pub cached: Option<Arc<SnapInner>>,
}

impl State {
    /// The live document with primary key `v`.
    pub fn find_live(&self, v: &Value) -> Option<Loc> {
        let key = Key::of(v)?;
        if let Some(&i) = self.mem.keys.get(&key) {
            return Some(Loc::Mem(i));
        }
        for h in self.handles.iter().rev() {
            let Some(pk) = &h.pk else { continue };
            for d in pk.lookup_all(v) {
                if !self.is_deleted(h, d) {
                    return Some(Loc::Disk(h.meta.id, d));
                }
            }
        }
        None
    }

    /// Whether on-disk document `d` of `h` is deleted, stored or pending.
    pub fn is_deleted(&self, h: &SegHandle, d: DocId) -> bool {
        h.base.as_ref().is_some_and(|b| b.is_deleted(d)) || self.pending.get(&h.meta.id).is_some_and(|p| p.contains(&d))
    }

    /// Applies a committed operation (at commit and on WAL replay).
    pub fn apply(&mut self, schema: &Schema, op: Op) {
        match op {
            Op::Insert(doc) => {
                let key = schema.primary_key.and_then(|pk| Key::of(&doc[pk]));
                // The transaction owned the only reference.
                let i = self.mem.push(Arc::try_unwrap(doc).unwrap_or_else(|d| (*d).clone()));
                if let Some(k) = key {
                    self.mem.keys.insert(k, i);
                }
            }
            Op::Delete(v) => match self.find_live(&v) {
                Some(Loc::Mem(i)) => {
                    self.mem.delete(i);
                    self.mem.keys.remove(&Key::of(&v).unwrap());
                }
                Some(Loc::Disk(id, d)) => {
                    self.pending.entry(id).or_default().insert(d);
                    self.n_pending += 1;
                }
                None => {}
            },
        }
        self.changed();
    }

    /// Invalidates the cached snapshot.
    pub fn changed(&mut self) {
        self.version += 1;
        self.cached = None;
    }
}
