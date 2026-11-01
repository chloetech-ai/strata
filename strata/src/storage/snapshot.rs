//! Snapshots: immutable, reference-counted views of one committed version.
//!
//! For each table, a snapshot lists the on-disk segments of its version
//! with their live filters (stored delete set plus deletes not yet
//! flushed), then the table's memtable as in-memory segments built with
//! `build_segment`, one per memtable chunk. Building those is the only
//! expensive step: it runs outside the state lock, a full chunk's segment
//! is built once, and the last chunk's is rebuilt only after it grows.
//! While a snapshot is alive it *pins* its txn, so pages it can reach are
//! not reused.
//!
//! A [`Snapshot`] value looks at one table, the database's first unless
//! [`Snapshot::table`] picks another; every table it reaches is at the same
//! version.

use std::alloc::Layout;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::postings::bytes::OwnedBytes;

use crate::error::{Error, Result};
use crate::schema::Schema;
use crate::segment::build::BuildOptions;
use crate::segment::{DocFilter, DocId, SegmentFile};
use crate::storage::live::LiveDocs;
use crate::storage::lock;
use crate::storage::manifest::RankProfile;
use crate::storage::pk::PkIndex;
use crate::storage::rowseq;
use crate::storage::state::{SegHandle, State};
use crate::storage::updates::{Updates, SEQ};
use crate::value::{Document, Value};

/// One segment as seen by a snapshot.
#[derive(Clone)]
pub struct SegmentView {
    /// Segment id; in-memory (memtable) segments have the top bit set and a
    /// fresh id every time they are rebuilt, so ids can key caches.
    pub id: u64,
    pub file: SegmentFile,
    /// Live documents; `None` when nothing is deleted.
    pub live: Option<Arc<dyn DocFilter>>,
    pub n_live: u32,
    pub in_memory: bool,
    /// Numeric fields updated in place; `None` when there are none.
    pub updates: Option<Arc<Updates>>,
}

/// A consistent, immutable view of the database, looking at one of its
/// tables. Cheap to clone.
#[derive(Clone)]
pub struct Snapshot {
    pub(crate) inner: Arc<SnapInner>,
    /// The table this value looks at: a position in `inner.tables`.
    pub(crate) table: usize,
}

pub(crate) struct SnapInner {
    txn: u64,
    /// Whether the database numbers its writes (`seq_no()`).
    numbered: bool,
    /// The number of the last transaction it holds (0 unnumbered).
    seq: u64,
    tables: Vec<TableView>,
    /// Mappings the on-disk segments point into.
    maps: Vec<OwnedBytes>,
    path: PathBuf,
    _pin: Pin,
}

/// One table as a snapshot sees it.
#[derive(Clone)]
struct TableView {
    id: u32,
    schema: Arc<Schema>,
    profiles: Arc<Vec<RankProfile>>,
    segments: Vec<SegmentView>,
    pks: Vec<Option<PkIndex>>,
    n_live: u64,
}

impl Snapshot {
    pub(crate) fn of(inner: Arc<SnapInner>) -> Snapshot {
        Snapshot { inner, table: 0 }
    }

    /// Whether the database numbers its writes, so that each row has a
    /// sequence number (`seq_no()`).
    pub fn numbers_writes(&self) -> bool {
        self.inner.numbered
    }

    fn view(&self) -> &TableView {
        &self.inner.tables[self.table]
    }

    /// The txn this snapshot reflects.
    pub fn txn(&self) -> u64 {
        self.inner.txn
    }

    /// The sequence number of the last transaction this snapshot holds,
    /// in a database that numbers its writes (0 before the first).
    pub fn seq_no(&self) -> Option<u64> {
        self.inner.numbered.then_some(self.inner.seq)
    }

    /// Calls `f` with every live row of the table this snapshot looks at:
    /// its primary key (`None` in a table without one) and the number of
    /// the write that last changed it ([`rowseq`]; 0 for rows from before
    /// numbering, loaded or attached), segment by segment. Replicas compare
    /// their rows with their primary's by these. Refused for a database that
    /// does not number its writes.
    pub fn row_versions(&self, mut f: impl FnMut(Option<&Value>, u64)) -> Result<()> {
        if !self.numbers_writes() {
            return Err(Error::Schema("the database does not number its writes".into()));
        }
        let v = self.view();
        let keyed = v.schema.primary_key.is_some();
        for (seg, pk) in v.segments.iter().zip(&v.pks) {
            let pk = match pk {
                Some(pk) => Some(pk),
                None if keyed => return Err(Error::Corrupt(format!("table {}: segment {} has no primary-key index", v.schema.table, seg.id))),
                None => None,
            };
            let built = rowseq::Seqs::of(&seg.file)?;
            for d in 0..seg.file.n_docs() as DocId {
                if seg.live.as_ref().is_some_and(|l| !l.contains(d)) {
                    continue;
                }
                f(pk.map(|pk| pk.key(d)).as_ref(), row_seq(seg, built.as_ref(), d));
            }
        }
        Ok(())
    }

    /// The schema of the table this snapshot looks at.
    pub fn schema(&self) -> &Schema {
        &self.view().schema
    }

    /// [`Snapshot::schema`] as the table holds it, shared by snapshots
    /// until the table's schema changes.
    pub(crate) fn shared_schema(&self) -> &Arc<Schema> {
        &self.view().schema
    }

    /// The rank profiles of the table this snapshot looks at, by name.
    pub fn rank_profiles(&self) -> &[RankProfile] {
        &self.view().profiles
    }

    /// Its rank profile named `name` (any case).
    pub fn rank_profile(&self, name: &str) -> Option<&RankProfile> {
        self.rank_profiles().iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// The same version, looking at the table named `name` (any case).
    pub fn table(&self, name: &str) -> Option<Snapshot> {
        let t = self.inner.tables.iter().position(|t| t.schema.table.eq_ignore_ascii_case(name))?;
        Some(Snapshot { inner: self.inner.clone(), table: t })
    }

    /// The same version, looking at the table with this id.
    pub(crate) fn table_by_id(&self, id: u32) -> Option<Snapshot> {
        let t = self.inner.tables.iter().position(|t| t.id == id)?;
        Some(Snapshot { inner: self.inner.clone(), table: t })
    }

    /// The schemas of every table at this version, in creation order.
    pub fn schemas(&self) -> impl Iterator<Item = &Schema> {
        self.inner.tables.iter().map(|t| &*t.schema)
    }

    /// The segments of every table at this version.
    pub(crate) fn all_segments(&self) -> impl Iterator<Item = &SegmentView> {
        self.inner.tables.iter().flat_map(|t| &t.segments)
    }

    /// On-disk segments (oldest first), then the memtable's in-memory
    /// segments (one per chunk of `DbOptions::memtable_chunk_docs`).
    pub fn segments(&self) -> &[SegmentView] {
        &self.view().segments
    }

    pub fn n_live_docs(&self) -> u64 {
        self.view().n_live
    }

    /// The primary-key index of segment `seg` (`None` without a primary key).
    pub fn pk(&self, seg: usize) -> Option<&PkIndex> {
        self.view().pks[seg].as_ref()
    }

    /// Segment ordinal and docID of the live document with primary key `key`.
    pub fn lookup(&self, key: &Value) -> Option<(usize, DocId)> {
        let v = self.view();
        let segs = &v.segments;
        // Newest first: recently written keys are found sooner.
        (0..segs.len()).rev().find_map(|i| {
            let pk = v.pks[i].as_ref()?;
            let mut docs = pk.lookup_all(key);
            docs.find(|&d| segs[i].live.as_ref().is_none_or(|l| l.contains(d))).map(|d| (i, d))
        })
    }

    /// Like [`lookup`](Self::lookup), for a borrowed key (graph traversals
    /// resolve edge targets stored as keys).
    pub fn lookup_key(&self, key: crate::storage::KeyRef) -> Option<(usize, DocId)> {
        let v = self.view();
        let segs = &v.segments;
        let h = crate::storage::key_hash(key);
        (0..segs.len()).rev().find_map(|i| {
            let pk = v.pks[i].as_ref()?;
            let mut docs = pk.lookup_hashed(Some(key), h);
            docs.find(|&d| segs[i].live.as_ref().is_none_or(|l| l.contains(d))).map(|d| (i, d))
        })
    }

    /// The primary key of document `doc` of segment `seg`.
    pub fn key(&self, seg: usize, doc: DocId) -> Option<Value> {
        Some(self.pk(seg)?.key(doc))
    }

    /// The same version, with the rows `mask` hides left out of every
    /// table: queries, counts and lookups through it do not see them. The
    /// rows stay in the database (a write by key still replaces one), and
    /// what is hidden in each segment is worked out once per mask and
    /// kept ([`Mask`]). Statistics read from the indexes themselves, such
    /// as a term's document frequency, still count them, as they count
    /// deleted rows until a merge drops them.
    pub fn masked(&self, mask: &Mask) -> Result<Snapshot> {
        let mut tables = Vec::with_capacity(self.inner.tables.len());
        let mut present = std::collections::HashSet::new();
        for t in &self.inner.tables {
            let mut t = t.clone();
            let place = t.schema.shard_by_field().or(t.schema.primary_key);
            if let Some(place) = place {
                for (seg, pk) in t.segments.iter_mut().zip(&t.pks) {
                    present.insert((t.id, seg.id));
                    let Some(hidden) = mask.hidden(t.id, &t.schema, place, seg, pk.as_ref())? else { continue };
                    let n_hidden = match &seg.live {
                        None => hidden.len(),
                        Some(l) => hidden.iter().filter(|&d| l.contains(d)).count(),
                    } as u32;
                    if n_hidden == 0 {
                        continue;
                    }
                    let n_live = seg.n_live.saturating_sub(n_hidden);
                    seg.live = Some(Arc::new(Unhidden { live: seg.live.take(), hidden, n: n_live as usize }));
                    seg.n_live = n_live;
                }
                t.n_live = t.segments.iter().map(|s| s.n_live as u64).sum();
            }
            tables.push(t);
        }
        mask.forget_except(&present);
        let i = &self.inner;
        let inner = SnapInner { txn: i.txn, numbered: i.numbered, seq: i.seq, tables, maps: i.maps.clone(), path: i.path.clone(), _pin: i._pin.pins.pin(i._pin.txn) };
        Ok(Snapshot { inner: Arc::new(inner), table: self.table })
    }

    /// The live rows of the table this snapshot looks at that `mask`
    /// hides, as (segment ordinal, docID), in that order; with `after`,
    /// only those a write numbered above `after` last changed (refused
    /// unless the database numbers its writes). A shard moving rows to
    /// another picks them so, by a mask of the slots that move, and reads
    /// them with [`crate::db::Db::documents_of`]. A table placed by neither
    /// a primary key nor shard_by has none.
    pub fn covered(&self, mask: &Mask, after: Option<u64>) -> Result<Vec<(usize, DocId)>> {
        if after.is_some() && !self.numbers_writes() {
            return Err(Error::Schema("the database does not number its writes".into()));
        }
        let v = self.view();
        let Some(place) = v.schema.shard_by_field().or(v.schema.primary_key) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for (i, (seg, pk)) in v.segments.iter().zip(&v.pks).enumerate() {
            let Some(hidden) = mask.hidden(v.id, &v.schema, place, seg, pk.as_ref())? else { continue };
            let built = match after {
                Some(_) => rowseq::Seqs::of(&seg.file)?,
                None => None,
            };
            for d in hidden.iter() {
                if seg.live.as_ref().is_some_and(|l| !l.contains(d)) {
                    continue;
                }
                if after.is_some_and(|a| row_seq(seg, built.as_ref(), d) <= a) {
                    continue;
                }
                out.push((i, d));
            }
        }
        Ok(out)
    }

    /// The rows `mask` hides in `prev`, an earlier snapshot of the same
    /// table, that this one no longer holds where `mask` hides them:
    /// deleted since, or replaced by a row of the same key that `mask`
    /// does not hide (another shard_by value). Each as its primary key and
    /// the value that placed it in `prev`; none in a table without a
    /// primary key. In a database that numbers its writes, a row this
    /// snapshot holds unchanged since `prev` is passed over by its number.
    pub fn covered_gone(&self, prev: &Snapshot, mask: &Mask) -> Result<Vec<(Value, Value)>> {
        let (pv, nv) = (prev.view(), self.view());
        if pv.schema.table != nv.schema.table {
            return Err(Error::Query(format!("rows of {} compared with rows of {}", pv.schema.table, nv.schema.table)));
        }
        if pv.schema.primary_key.is_none() {
            return Ok(Vec::new());
        }
        let by = pv.schema.shard_by_field();
        let numbered = self.numbers_writes() && prev.numbers_writes();
        let mut seqs: std::collections::HashMap<usize, Option<rowseq::Seqs>> = Default::default();
        let mut stores: std::collections::HashMap<(bool, usize), crate::forward::DocStore> = Default::default();
        let mut field_of = |now: bool, s: usize, d: DocId, f: usize| -> Result<Value> {
            let (view, s) = (if now { nv } else { pv }, s);
            let store = match stores.entry((now, s)) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => e.insert(crate::forward::DocStore::open(&view.segments[s].file, &view.schema)?),
            };
            Ok(store.get_fields(d, &[f]).pop().unwrap_or(Value::Null))
        };
        let mut out = Vec::new();
        for (s, d) in prev.covered(mask, None)? {
            let key = pv.pks[s].as_ref().ok_or_else(|| Error::Corrupt(format!("table {}: segment {} has no primary-key index", pv.schema.table, pv.segments[s].id)))?.key(d);
            let gone = match (self.lookup(&key), by) {
                (None, _) => true,
                // Placed by its key, which it keeps.
                (Some(_), None) => false,
                (Some((ns, nd)), Some(f)) => {
                    let seg = &nv.segments[ns];
                    let unchanged = numbered && {
                        let built = match seqs.entry(ns) {
                            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                            std::collections::hash_map::Entry::Vacant(e) => e.insert(rowseq::Seqs::of(&seg.file)?),
                        };
                        row_seq(seg, built.as_ref(), nd) <= prev.inner.seq
                    };
                    !unchanged && !(mask.hide)(&nv.schema, &field_of(true, ns, nd, f)?)
                }
            };
            if gone {
                let place = match by {
                    None => key.clone(),
                    Some(f) => field_of(false, s, d, f)?,
                };
                out.push((key, place));
            }
        }
        Ok(out)
    }

    /// Drops this snapshot's file pages from memory and the page cache, so
    /// the next access reads from the device (cold-cache experiments).
    pub fn evict_from_cache(&self) -> Result<()> {
        for m in &self.inner.maps {
            if !m.is_empty() {
                // SAFETY: a page-aligned, read-only shared file mapping that
                // nobody writes through; its pages are re-read on access.
                unsafe { crate::iostat::drop_mapping(m.as_ptr(), m.len())? };
            }
        }
        if self.inner.path.is_dir() {
            for e in std::fs::read_dir(&self.inner.path)? {
                let e = e?;
                if e.file_name().to_string_lossy().starts_with("seg-") {
                    crate::iostat::evict(&e.path())?;
                }
            }
        } else {
            crate::iostat::evict(&self.inner.path)?;
        }
        Ok(())
    }
}

/// Live snapshots by txn, so commits know which freed pages are still
/// reachable.
#[derive(Default)]
pub(crate) struct Pins(Mutex<BTreeMap<u64, usize>>);

impl Pins {
    pub fn pin(self: &Arc<Self>, txn: u64) -> Pin {
        *lock(&self.0).entry(txn).or_default() += 1;
        Pin { pins: self.clone(), txn }
    }

    /// The oldest pinned txn.
    pub fn min(&self) -> Option<u64> {
        lock(&self.0).keys().next().copied()
    }
}

pub(crate) struct Pin {
    pins: Arc<Pins>,
    txn: u64,
}

impl Drop for Pin {
    fn drop(&mut self) {
        let mut m = lock(&self.pins.0);
        if let Some(c) = m.get_mut(&self.txn) {
            *c -= 1;
            if *c == 0 {
                m.remove(&self.txn);
            }
        }
    }
}

/// Rows to leave out of what a snapshot shows ([`Snapshot::masked`]): those
/// whose placement, a table's `shard_by` value or else its primary key,
/// `hide` names. A sharded server hides the rows of slots it does not hold
/// while rows move between shards. Each segment's hidden rows are worked
/// out once and kept, by table and segment id; a new mask starts again.
pub struct Mask {
    hide: Box<dyn Fn(&Schema, &Value) -> bool + Send + Sync>,
    built: Mutex<std::collections::HashMap<(u32, u64), Option<Arc<crate::segment::DocSet>>>>,
}

impl Mask {
    pub fn new(hide: impl Fn(&Schema, &Value) -> bool + Send + Sync + 'static) -> Mask {
        Mask { hide: Box::new(hide), built: Mutex::new(Default::default()) }
    }

    /// The rows of `seg` (of table `table`) to hide, placed by field
    /// `place`; `None` when there are none.
    fn hidden(&self, table: u32, schema: &Schema, place: usize, seg: &SegmentView, pk: Option<&PkIndex>) -> Result<Option<Arc<crate::segment::DocSet>>> {
        if let Some(h) = lock(&self.built).get(&(table, seg.id)) {
            return Ok(h.clone());
        }
        let n = seg.file.n_docs() as DocId;
        let mut docs = Vec::new();
        if Some(place) == schema.primary_key && schema.shard_by_field().is_none() {
            let pk = pk.ok_or_else(|| Error::Corrupt(format!("table {}: segment {} has no primary-key index", schema.table, seg.id)))?;
            docs.extend((0..n).filter(|&d| (self.hide)(schema, &pk.key(d))));
        } else {
            let values = crate::forward::DocStore::open(&seg.file, schema)?.field_values(place);
            docs.extend((0..n).filter(|&d| (self.hide)(schema, &values[d as usize])));
        }
        let h = (!docs.is_empty()).then(|| Arc::new(crate::segment::DocSet::from_sorted(docs, n)));
        lock(&self.built).insert((table, seg.id), h.clone());
        Ok(h)
    }

    /// Drops what it kept of segments other than `present`, once it holds
    /// more than twice as many.
    fn forget_except(&self, present: &std::collections::HashSet<(u32, u64)>) {
        let mut b = lock(&self.built);
        if b.len() > 2 * present.len() + 16 {
            b.retain(|k, _| present.contains(k));
        }
    }
}

/// The number of the write that last changed document `d` of `seg`
/// ([`rowseq`]; 0 for rows from before numbering): an update's, else the
/// segment's as built (`built`).
fn row_seq(seg: &SegmentView, built: Option<&rowseq::Seqs>, d: DocId) -> u64 {
    match seg.updates.as_ref().and_then(|u| u.get(SEQ, d)) {
        Some(x) => x.as_i64().unwrap_or(0) as u64,
        None => built.map_or(0, |s| s.get(d)),
    }
}

impl std::fmt::Debug for Mask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mask").field("segments", &lock(&self.built).len()).finish()
    }
}

/// A segment's live rows less those a [`Mask`] hides.
struct Unhidden {
    live: Option<Arc<dyn DocFilter>>,
    hidden: Arc<crate::segment::DocSet>,
    n: usize,
}

impl DocFilter for Unhidden {
    fn contains(&self, doc: DocId) -> bool {
        !self.hidden.contains(doc) && self.live.as_ref().is_none_or(|l| l.contains(doc))
    }
    fn cardinality(&self) -> usize {
        self.n
    }
}

/// A built segment of the first `len` documents of memtable chunk `chunk`
/// of generation `gen`.
#[derive(Clone)]
pub(crate) struct MemSeg {
    pub gen: u64,
    pub chunk: usize,
    pub len: usize,
    pub id: u64,
    pub file: SegmentFile,
    pub pk: Option<PkIndex>,
}

/// A memtable chunk a snapshot needs as a segment.
pub(crate) struct MemPart {
    /// The id of the table whose memtable holds the chunk.
    pub table: u32,
    pub gen: u64,
    pub chunk: usize,
    pub len: usize,
    pub docs: Arc<Vec<Document>>,
    pub live: Option<Arc<LiveDocs>>,
    pub n_live: u32,
    /// Already built, if the cache held this prefix.
    pub seg: Option<MemSeg>,
    pub updates: Option<Arc<Updates>>,
    /// The documents' insert numbers, when the database numbers its
    /// writes and the segment is still to build.
    pub seqs: Option<Vec<u64>>,
}

/// What a snapshot needs of one table.
pub(crate) struct TableCapture {
    id: u32,
    pub schema: Arc<Schema>,
    profiles: Arc<Vec<RankProfile>>,
    disk: Vec<(Arc<SegHandle>, Option<Arc<LiveDocs>>, u32, Option<Arc<Updates>>)>,
    pub mem: Vec<MemPart>,
}

/// Everything a snapshot needs, taken under the state lock.
pub(crate) struct Capture {
    pub version: u64,
    txn: u64,
    numbered: bool,
    seq: u64,
    pin: Pin,
    pub tables: Vec<TableCapture>,
}

impl State {
    /// Takes what a snapshot of the current version needs (without the
    /// memtables unless `with_mem`).
    pub(crate) fn capture(&mut self, pins: &Arc<Pins>, with_mem: bool) -> Capture {
        let mut tables = Vec::with_capacity(self.tables.len());
        let numbered = self.manifest.seq_no.is_some();
        for t in 0..self.tables.len() {
            tables.push(self.capture_table(t, with_mem, numbered));
        }
        Capture { version: self.version, txn: self.txn, numbered, seq: self.seq, pin: pins.pin(self.txn), tables }
    }

    fn capture_table(&mut self, t: usize, with_mem: bool, numbered: bool) -> TableCapture {
        let handles = self.tables[t].handles.clone();
        let mut disk = Vec::with_capacity(handles.len());
        for h in &handles {
            let id = h.meta.id;
            let live = match self.pending.get(&id) {
                None => h.base.clone(),
                Some(p) => {
                    let fresh = self.live_cache.get(&id).filter(|(n, _)| *n == p.len()).map(|(_, l)| l.clone());
                    Some(fresh.unwrap_or_else(|| {
                        let l = Arc::new(LiveDocs::with_deleted(h.base.as_deref(), h.meta.n_docs, p.iter().copied()));
                        self.live_cache.insert(id, (p.len(), l.clone()));
                        l
                    }))
                }
            };
            let n_live = live.as_ref().map_or(h.meta.n_docs, |l| l.n_live());
            let updates = self.updates_of(h);
            disk.push((h.clone(), live, n_live, updates));
        }
        let table = &mut self.tables[t];
        let gen = table.mem.gen;
        let n_chunks = if with_mem { table.mem.chunks.len() } else { 0 };
        if table.mem_live.len() < n_chunks {
            table.mem_live.resize(n_chunks, None);
        }
        let mut mem = Vec::new();
        for k in 0..n_chunks {
            let (len, n_deleted) = (table.mem.chunks[k].len(), table.mem.chunk_deleted[k]);
            if len == n_deleted {
                continue;
            }
            let mut live = None;
            if n_deleted > 0 {
                let hit = table.mem_live[k].as_ref().filter(|c| (c.0, c.1) == (len, n_deleted)).map(|c| c.2.clone());
                live = Some(hit.unwrap_or_else(|| {
                    let f = Arc::new(LiveDocs::from_deleted(table.mem.chunk_deleted_words(k), len as u32));
                    table.mem_live[k] = Some((len, n_deleted, f.clone()));
                    f
                }));
            }
            let seg = table.memsegs.get(k).and_then(|s| s.as_ref()).filter(|s| (s.gen, s.len) == (gen, len)).cloned();
            let docs = table.mem.chunks[k].clone();
            let first = k * table.mem.chunk;
            let updates = Some(table.mem.updates.slice(first as DocId, len as u32)).filter(|u| !u.is_empty()).map(Arc::new);
            let seqs = (numbered && seg.is_none()).then(|| (first..first + len).map(|p| table.mem.seqs.get(p).copied().unwrap_or(0)).collect());
            mem.push(MemPart { table: table.id, gen, chunk: k, len, docs, live, n_live: (len - n_deleted) as u32, seg, updates, seqs });
        }
        TableCapture { id: table.id, schema: table.schema.clone(), profiles: table.profiles.clone(), disk, mem }
    }

    /// The cached segment of the first `len` documents of chunk `k` of
    /// table `table`'s memtable.
    pub(crate) fn memseg(&self, table: u32, gen: u64, k: usize, len: usize) -> Option<MemSeg> {
        let t = &self.tables[self.table_index(table)?];
        t.memsegs.get(k)?.as_ref().filter(|s| (s.gen, s.len) == (gen, len)).cloned()
    }

    /// Caches a built memtable segment unless the memtable moved past it
    /// (or the table was dropped).
    pub(crate) fn keep_memseg(&mut self, table: u32, seg: &MemSeg) {
        let Some(t) = self.table_index(table) else { return };
        let t = &mut self.tables[t];
        if seg.gen != t.mem.gen {
            return;
        }
        if t.memsegs.len() <= seg.chunk {
            t.memsegs.resize(seg.chunk + 1, None);
        }
        let slot = &mut t.memsegs[seg.chunk];
        if slot.as_ref().is_none_or(|s| s.len < seg.len) {
            *slot = Some(seg.clone());
        }
    }
}

/// `image` copied to page-aligned memory, as a segment opened from it
/// needs for its sections to be viewed in place.
pub(crate) fn page_aligned(image: &[u8]) -> OwnedBytes {
    OwnedBytes::new(PageAligned::new(image))
}

/// Builds the segment of a memtable chunk's first `len` documents.
pub(crate) fn build_memseg(schema: &Schema, opts: &BuildOptions, part: &MemPart, id: u64) -> MemSeg {
    // The documents were validated when they were written, so a failure
    // here is a bug in an index builder.
    let image = rowseq::build(schema, &part.docs[..part.len], part.seqs.as_deref(), opts).expect("building a memtable segment");
    let file = SegmentFile::open(OwnedBytes::new(PageAligned::new(&image))).expect("memtable segment opens");
    let pk = PkIndex::open(&file).expect("memtable pk index");
    MemSeg { gen: part.gen, chunk: part.chunk, len: part.len, id, file, pk }
}

impl Capture {
    pub(crate) fn assemble(self, path: PathBuf) -> SnapInner {
        let mut maps: Vec<OwnedBytes> = Vec::new();
        let mut tables = Vec::with_capacity(self.tables.len());
        for t in self.tables {
            let mut segments = Vec::with_capacity(t.disk.len() + t.mem.len());
            let mut pks = Vec::with_capacity(t.disk.len() + t.mem.len());
            for (h, live, n_live, updates) in t.disk {
                if !maps.iter().any(|m| m.as_ptr() == h.map.as_ptr()) {
                    maps.push(h.map.clone());
                }
                pks.push(h.pk.clone());
                segments.push(SegmentView {
                    id: h.meta.id,
                    file: h.file.clone(),
                    live: live.map(|l| l as Arc<dyn DocFilter>),
                    n_live,
                    in_memory: false,
                    updates,
                });
            }
            for m in t.mem {
                let seg = m.seg.expect("memtable segment built before assembly");
                pks.push(seg.pk);
                segments.push(SegmentView {
                    id: seg.id,
                    file: seg.file,
                    live: m.live.map(|l| l as Arc<dyn DocFilter>),
                    n_live: m.n_live,
                    in_memory: true,
                    updates: m.updates,
                });
            }
            let n_live = segments.iter().map(|s| s.n_live as u64).sum();
            tables.push(TableView { id: t.id, schema: t.schema, profiles: t.profiles, segments, pks, n_live });
        }
        SnapInner { txn: self.txn, numbered: self.numbered, seq: self.seq, tables, maps, path, _pin: self.pin }
    }
}

/// A page-aligned heap copy of a segment image, so that an in-memory
/// segment has the same alignment as one mapped from the file.
struct PageAligned {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the buffer is owned and never mutated after construction.
unsafe impl Send for PageAligned {}
unsafe impl Sync for PageAligned {}

impl PageAligned {
    fn layout(len: usize) -> Layout {
        Layout::from_size_align(len.max(1), 4096).expect("segment size fits a layout")
    }

    fn new(bytes: &[u8]) -> PageAligned {
        let layout = Self::layout(bytes.len());
        // SAFETY: the layout has non-zero size; the copy stays in bounds.
        unsafe {
            let ptr = std::alloc::alloc(layout);
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            PageAligned { ptr, len: bytes.len() }
        }
    }
}

impl AsRef<[u8]> for PageAligned {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: `ptr` holds `len` initialised bytes for our lifetime.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for PageAligned {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { std::alloc::dealloc(self.ptr, Self::layout(self.len)) }
    }
}
