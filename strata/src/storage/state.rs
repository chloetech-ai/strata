//! In-memory state of an open database: the current on-disk version with
//! each table's opened segments and memtable, and deletes not yet flushed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use crate::postings::bytes::OwnedBytes;

use crate::error::{corrupt, Result};
use crate::schema::Schema;
use crate::segment::{DocId, SegmentFile};
use crate::storage::db::Reserved;
use crate::storage::freelist::FreeList;
use crate::storage::live::{words_for, LiveDocs};
use crate::storage::manifest::{Manifest, SegmentMeta};
use crate::storage::pager::{Extent, Meta};
use crate::storage::pk::PkIndex;
use crate::storage::rowseq;
use crate::storage::ship::ShipLog;
use crate::storage::snapshot::{MemSeg, SnapInner};
use crate::storage::updates::{Updates, SEQ};
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
    /// The stored in-place updates, if any.
    pub updates: Option<Arc<Updates>>,
    /// The mapping the views point into (for cache eviction).
    pub map: OwnedBytes,
}

/// Checks a segment's image and, if any, its delete set and updates
/// against the checksums of its manifest entry.
fn verify_segment(meta: &SegmentMeta, image: Option<&[u8]>, deletes: Option<&[u8]>, updates: Option<&[u8]>) -> Result<()> {
    for (what, bytes, crc) in [("image", image, meta.crc), ("delete set", deletes, meta.deletes_crc), ("updates", updates, meta.updates_crc)] {
        if bytes.is_some_and(|b| crc32fast::hash(b) != crc) {
            return Err(corrupt(format!("the {what} of segment {} fails its checksum", meta.id)));
        }
    }
    Ok(())
}

/// When an opened segment's image is checked against its checksums.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    /// Whole, as it opens.
    Open,
    /// Each section the first time it is read
    /// ([`SegmentFile::checked_on_read`]).
    Read,
    /// Not (only as far as its structure checks go).
    Never,
}

impl SegHandle {
    /// `random_hints`: advise the segment's random-access sections
    /// `MADV_RANDOM` ([`SegmentFile::advise_random_sections`]). The delete
    /// set and updates are checked against their checksums (they are small
    /// and read whole); the image as `check` says.
    pub fn open(map: &OwnedBytes, meta: &SegmentMeta, read_ahead: bool, random_hints: bool, check: Check) -> Result<SegHandle> {
        let within = |e: &Extent| e.range().end <= map.len();
        if !within(&meta.extent) || [meta.deletes, meta.updates].iter().flatten().any(|e| !within(e)) {
            return Err(corrupt(format!("segment {} lies outside the file", meta.id)));
        }
        let deletes = meta.deletes.map(|e| map.slice(e.range()));
        let updates = meta.updates.map(|e| map.slice(e.range()));
        SegHandle::from_bytes(map.slice(meta.extent.range()), deletes, updates, map.clone(), meta, read_ahead, random_hints, check)
    }

    /// Opens a segment of the directory layout from its own files (see
    /// [`SegHandle::open`]).
    pub fn open_in(dir: &Path, meta: &SegmentMeta, read_ahead: bool, random_hints: bool, check: Check) -> Result<SegHandle> {
        let name = meta.file.as_deref().ok_or_else(|| corrupt(format!("segment {} has no file", meta.id)))?;
        let map = crate::storage::dir::map(dir, name, read_ahead)?;
        if map.len() as u64 != meta.extent.len {
            return Err(corrupt(format!("{name} has {} bytes, the manifest says {}", map.len(), meta.extent.len)));
        }
        let deletes = match &meta.deletes_file {
            Some(d) => Some(crate::storage::dir::map(dir, d, read_ahead)?),
            None => None,
        };
        let updates = match &meta.updates_file {
            Some(u) => Some(OwnedBytes::new(std::fs::read(dir.join(u))?)),
            None => None,
        };
        SegHandle::from_bytes(map.clone(), deletes, updates, map, meta, read_ahead, random_hints, check)
    }

    #[allow(clippy::too_many_arguments)]
    fn from_bytes(
        image: OwnedBytes,
        deletes: Option<OwnedBytes>,
        updates: Option<OwnedBytes>,
        map: OwnedBytes,
        meta: &SegmentMeta,
        read_ahead: bool,
        random_hints: bool,
        check: Check,
    ) -> Result<SegHandle> {
        verify_segment(meta, (check == Check::Open).then_some(&image[..]), deletes.as_deref(), updates.as_deref())?;
        let mut file = SegmentFile::open(image)?;
        if check == Check::Read {
            file = file.checked_on_read();
        }
        if !read_ahead {
            file = file.mapped_random();
        }
        if random_hints {
            file.advise_random_sections();
        }
        let pk = PkIndex::open(&file)?;
        if file.n_docs() != meta.n_docs as usize || pk.as_ref().is_some_and(|p| p.len() != file.n_docs()) {
            return Err(corrupt(format!("segment {} document count disagrees with manifest", meta.id)));
        }
        let base = match deletes {
            Some(b) => Some(Arc::new(LiveDocs::view(b, meta.n_docs, meta.n_deleted)?)),
            None => None,
        };
        let updates = match updates {
            Some(b) => Some(Arc::new(Updates::decode(&b)?)),
            None => None,
        };
        Ok(SegHandle { meta: meta.clone(), file, pk, base, updates, map })
    }

    /// Checks the segment's image, delete set and updates against their
    /// checksums; `dir` is the directory of the directory layout.
    pub fn verify(&self, dir: Option<&Path>) -> Result<()> {
        let m = &self.meta;
        match dir {
            None => {
                let at = |e: Option<Extent>| e.map(|e| &self.map[e.range()]);
                verify_segment(m, Some(&self.map[m.extent.range()]), at(m.deletes), at(m.updates))
            }
            Some(d) => {
                let read = |f: &Option<String>| f.as_ref().map(|f| std::fs::read(d.join(f))).transpose();
                let (deletes, updates) = (read(&m.deletes_file)?, read(&m.updates_file)?);
                verify_segment(m, Some(&self.map), deletes.as_deref(), updates.as_deref())
            }
        }
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
    /// In-place updates by memtable position, applied at flush.
    pub updates: Updates,
    /// In a database that numbers its writes, the sequence number of the
    /// write that inserted each document, by memtable position (one that
    /// set a value in place since is in `updates`); empty otherwise.
    pub seqs: Vec<u64>,
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
            updates: Updates::default(),
            seqs: Vec::new(),
        }
    }

    /// Empties the memtable (after a flush).
    pub fn clear(&mut self) {
        *self = Memtable::new(self.chunk, self.gen + 1);
    }

    /// Appends a document written by the transaction numbered `seq` (0 if
    /// the database does not number its writes); returns its position.
    pub fn push(&mut self, doc: Document, seq: u64) -> u32 {
        let i = self.len;
        if seq > 0 {
            self.seqs.resize(i, 0);
            self.seqs.push(seq);
        }
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

    /// The sequence number of the write that last changed document `i`.
    pub fn seq(&self, i: u32) -> u64 {
        mem_seq(&self.updates, &self.seqs, i)
    }

    /// The deleted bits of chunk `k`'s documents.
    pub fn chunk_deleted_words(&self, k: usize) -> Vec<u64> {
        let first = k * self.chunk / 64;
        self.deleted[first..first + words_for(self.chunks[k].len() as u32)].to_vec()
    }
}

/// The sequence number of the write that last changed memtable document
/// `i`, from the memtable's (or a copy's) updates and insert numbers.
pub(crate) fn mem_seq(updates: &Updates, seqs: &[u64], i: u32) -> u64 {
    match updates.get(SEQ, i) {
        Some(v) => v.as_i64().unwrap_or(0) as u64,
        None => seqs.get(i as usize).copied().unwrap_or(0),
    }
}

/// Where the live document with a key is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Loc {
    Mem(u32),
    Disk(u64, DocId),
}

/// One table of the open database.
pub(crate) struct TableState {
    /// The table's id ([`crate::storage::manifest::TableMeta::id`]).
    pub id: u32,
    pub schema: Arc<Schema>,
    /// Opened segments, aligned with the table's segments in the manifest.
    pub handles: Vec<Arc<SegHandle>>,
    pub mem: Memtable,
    /// Live filter of each memtable chunk, keyed by the chunk's length and
    /// deletes (emptied by a flush).
    pub mem_live: Vec<Option<(usize, usize, Arc<LiveDocs>)>>,
    /// The latest segment built for each memtable chunk (emptied by a flush).
    pub memsegs: Vec<Option<MemSeg>>,
    /// The ties its semantic-ID fields have given ([`crate::sid::Ties`]),
    /// once a write needed them; kept up to date by every insert.
    pub ties: Option<crate::sid::Ties>,
}

impl TableState {
    pub fn new(id: u32, schema: Arc<Schema>, handles: Vec<Arc<SegHandle>>, chunk: usize, gen: u64) -> TableState {
        TableState { id, schema, handles, mem: Memtable::new(chunk, gen), mem_live: Vec::new(), memsegs: Vec::new(), ties: None }
    }

    /// Empties the memtable (after a flush).
    pub fn clear_mem(&mut self) {
        self.mem.clear();
        self.mem_live.clear();
        self.memsegs.clear();
    }

    /// Drops the memtable's first `n` documents (after a background flush
    /// wrote them to segments); the others stay, as a memtable of the next
    /// generation. When `n` ends a chunk, the chunks after it stay as they
    /// are, with their live filters and segments.
    pub fn split_mem(&mut self, n: usize) {
        let next = Memtable::new(self.mem.chunk, self.mem.gen + 1);
        let old = std::mem::replace(&mut self.mem, next);
        let mem = &mut self.mem;
        if n.is_multiple_of(old.chunk) {
            let k = n / old.chunk;
            mem.chunks = old.chunks[k..].to_vec();
            mem.len = old.len - n;
            mem.deleted = old.deleted[n / 64..].to_vec();
            mem.chunk_deleted = old.chunk_deleted[k..].to_vec();
            mem.n_deleted = mem.chunk_deleted.iter().sum();
            let gen = mem.gen;
            self.memsegs = self
                .memsegs
                .drain(..)
                .skip(k)
                .map(|s| {
                    s.map(|mut s| {
                        s.gen = gen;
                        s.chunk -= k;
                        s
                    })
                })
                .collect();
            self.mem_live = self.mem_live.drain(..).skip(k).collect();
        } else {
            for p in n..old.len {
                let i = mem.push(old.chunks[p / old.chunk][p % old.chunk].clone(), 0);
                if old.deleted[p / 64] >> (p % 64) & 1 == 1 {
                    mem.delete(i);
                }
            }
            self.memsegs.clear();
            self.mem_live.clear();
        }
        mem.seqs = old.seqs.get(n..).unwrap_or_default().to_vec();
        let n32 = n as u32;
        mem.keys = old.keys.into_iter().filter(|&(_, p)| p >= n32).map(|(k, p)| (k, p - n32)).collect();
        for (f, p, v) in old.updates.iter().filter(|&(_, p, _)| p >= n32) {
            mem.updates.set(f, p - n32, v.clone());
        }
    }
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
    /// The tables, aligned with `manifest.tables`.
    pub tables: Vec<TableState>,
    /// Latest committed txn (WAL commits included).
    pub txn: u64,
    /// The last sequence number given to a write, when the database
    /// numbers its writes (`manifest.seq_no`); 0 otherwise.
    pub seq: u64,
    /// Bumped by every change; a snapshot is cached only if it is current.
    pub version: u64,
    /// Deletes of on-disk documents by segment id (ids are unique across
    /// tables), applied to delete sets at flush.
    pub pending: BTreeMap<u64, BTreeSet<DocId>>,
    /// In-place updates of on-disk documents by segment id, written beside
    /// the segments at flush, each with a count of the sets applied to it.
    pub pending_updates: BTreeMap<u64, (u64, Updates)>,
    /// Pending deletes and updates (a flush is due when there are many).
    pub n_pending: usize,
    /// Stored updates merged with pending ones, by segment id, keyed by
    /// the count of sets they include.
    pub updates_cache: HashMap<u64, (u64, Arc<Updates>)>,
    /// Stored delete set merged with pending deletes, by segment id, keyed
    /// by the number of pending deletes it includes.
    pub live_cache: HashMap<u64, (usize, Arc<LiveDocs>)>,
    pub cached: Option<Arc<SnapInner>>,
    /// Space reserved for merged images being written, which no commit may
    /// hand out until they are installed or given back.
    pub reserved: Vec<Reserved>,
    /// Segment ids below this are taken (by reservations), even when the
    /// manifest's `next_segment_id` is lower.
    pub next_segment_id: u64,
    /// The most recent numbered transactions, for replicas
    /// ([`crate::storage::ship`]); updated with `seq`.
    pub ship: ShipLog,
}

impl State {
    /// Position in `tables` of the table with this id.
    pub fn table_index(&self, id: u32) -> Option<usize> {
        self.tables.iter().position(|t| t.id == id)
    }

    /// The opened on-disk segment with this id, in any table.
    pub fn handle(&self, id: u64) -> Option<&Arc<SegHandle>> {
        self.tables.iter().flat_map(|t| &t.handles).find(|h| h.meta.id == id)
    }

    /// Every opened on-disk segment, table by table.
    pub fn handles(&self) -> impl Iterator<Item = &Arc<SegHandle>> {
        self.tables.iter().flat_map(|t| &t.handles)
    }

    /// Documents in the memtables, deleted ones included.
    pub fn mem_len(&self) -> usize {
        self.tables.iter().map(|t| t.mem.len).sum()
    }

    /// Live documents in the memtables.
    pub fn mem_live_docs(&self) -> usize {
        self.tables.iter().map(|t| t.mem.len - t.mem.n_deleted).sum()
    }

    /// The live document with primary key `v` in table `t` (a position in
    /// `tables`).
    pub fn find_live(&self, t: usize, v: &Value) -> Option<Loc> {
        let key = Key::of(v)?;
        let table = &self.tables[t];
        if let Some(&i) = table.mem.keys.get(&key) {
            return Some(Loc::Mem(i));
        }
        for h in table.handles.iter().rev() {
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

    /// Applies a committed operation on table `t` (a position in `tables`),
    /// at commit and on WAL replay, of the transaction numbered `seq` (0 if
    /// the database does not number its writes).
    pub fn apply(&mut self, t: usize, op: Op, seq: u64) {
        match op {
            Op::Insert(doc) => {
                let table = &mut self.tables[t];
                if let Some(ties) = &mut table.ties {
                    ties.saw_doc(&table.schema, &doc);
                }
                let key = table.schema.primary_key.and_then(|pk| Key::of(&doc[pk]));
                // The transaction owned the only reference.
                let i = table.mem.push(Arc::try_unwrap(doc).unwrap_or_else(|d| (*d).clone()), seq);
                if let Some(k) = key {
                    table.mem.keys.insert(k, i);
                }
            }
            Op::Delete(v) => match self.find_live(t, &v) {
                Some(Loc::Mem(i)) => {
                    let mem = &mut self.tables[t].mem;
                    mem.delete(i);
                    mem.keys.remove(&Key::of(&v).unwrap());
                }
                Some(Loc::Disk(id, d)) => {
                    self.pending.entry(id).or_default().insert(d);
                    self.n_pending += 1;
                }
                None => {}
            },
            Op::Set { key, field, value } => match self.find_live(t, &key) {
                Some(Loc::Mem(i)) => {
                    let u = &mut self.tables[t].mem.updates;
                    u.set(field as usize, i, value);
                    if seq > 0 {
                        u.set(SEQ, i, Value::Int(seq as i64));
                    }
                }
                Some(Loc::Disk(id, d)) => {
                    let (n, u) = self.pending_updates.entry(id).or_default();
                    *n += 1;
                    if u.set(field as usize, d, value) {
                        self.n_pending += 1;
                    }
                    if seq > 0 {
                        u.set(SEQ, d, Value::Int(seq as i64));
                    }
                }
                None => {}
            },
        }
        self.changed();
    }

    /// The sequence number of the write that last changed the document at
    /// `loc` of table `t` (a position in `tables`).
    pub fn seq_at(&self, t: usize, loc: Loc) -> Result<u64> {
        match loc {
            Loc::Mem(i) => Ok(self.tables[t].mem.seq(i)),
            Loc::Disk(id, d) => {
                let h = self.handle(id).ok_or_else(|| corrupt(format!("segment {id} is not open")))?;
                let written = self.pending_updates.get(&id).and_then(|(_, u)| u.get(SEQ, d)).or_else(|| h.updates.as_ref().and_then(|u| u.get(SEQ, d)));
                match written {
                    Some(v) => Ok(v.as_i64().unwrap_or(0) as u64),
                    None => rowseq::of(&h.file, d),
                }
            }
        }
    }

    /// Segment `h`'s stored updates with its pending ones written over.
    pub fn updates_of(&mut self, h: &SegHandle) -> Option<Arc<Updates>> {
        let id = h.meta.id;
        let Some((n, p)) = self.pending_updates.get(&id) else { return h.updates.clone() };
        if let Some((m, u)) = self.updates_cache.get(&id) {
            if m == n {
                return Some(u.clone());
            }
        }
        let u = Arc::new(Updates::merged(h.updates.as_deref(), p));
        self.updates_cache.insert(id, (*n, u.clone()));
        Some(u)
    }

    /// Invalidates the cached snapshot.
    pub fn changed(&mut self) {
        self.version += 1;
        self.cached = None;
    }
}
