//! Snapshots: immutable, reference-counted views of one committed version.
//!
//! A snapshot lists the on-disk segments of its version with their live
//! filters (stored delete set plus deletes not yet flushed), then the
//! memtable as in-memory segments built with `build_segment`, one per
//! memtable chunk. Building those is the only expensive step: it runs
//! outside the state lock, a full chunk's segment is built once, and the
//! last chunk's is rebuilt only after it grows. While a snapshot is alive it
//! *pins* its txn, so pages it can reach are not reused.

use std::alloc::Layout;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use posting_bench::bytes::OwnedBytes;

use crate::error::Result;
use crate::schema::Schema;
use crate::segment::build::{build_segment, BuildOptions};
use crate::segment::{DocFilter, DocId, SegmentFile};
use crate::storage::live::LiveDocs;
use crate::storage::lock;
use crate::storage::pk::PkIndex;
use crate::storage::state::{SegHandle, State};
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
}

/// A consistent, immutable view of the database. Cheap to clone.
#[derive(Clone)]
pub struct Snapshot {
    pub(crate) inner: Arc<SnapInner>,
}

pub(crate) struct SnapInner {
    txn: u64,
    schema: Arc<Schema>,
    segments: Vec<SegmentView>,
    pks: Vec<Option<PkIndex>>,
    n_live: u64,
    /// Mappings the on-disk segments point into.
    maps: Vec<OwnedBytes>,
    path: PathBuf,
    _pin: Pin,
}

impl Snapshot {
    /// The txn this snapshot reflects.
    pub fn txn(&self) -> u64 {
        self.inner.txn
    }

    pub fn schema(&self) -> &Schema {
        &self.inner.schema
    }

    /// On-disk segments (oldest first), then the memtable's in-memory
    /// segments (one per chunk of `DbOptions::memtable_chunk_docs`).
    pub fn segments(&self) -> &[SegmentView] {
        &self.inner.segments
    }

    pub fn n_live_docs(&self) -> u64 {
        self.inner.n_live
    }

    /// The primary-key index of segment `seg` (`None` without a primary key).
    pub fn pk(&self, seg: usize) -> Option<&PkIndex> {
        self.inner.pks[seg].as_ref()
    }

    /// Segment ordinal and docID of the live document with primary key `key`.
    pub fn lookup(&self, key: &Value) -> Option<(usize, DocId)> {
        let segs = &self.inner.segments;
        // Newest first: recently written keys are found sooner.
        (0..segs.len()).rev().find_map(|i| {
            let pk = self.inner.pks[i].as_ref()?;
            let mut docs = pk.lookup_all(key);
            docs.find(|&d| segs[i].live.as_ref().is_none_or(|l| l.contains(d))).map(|d| (i, d))
        })
    }

    /// Like [`lookup`](Self::lookup), for a borrowed key (graph traversals
    /// resolve edge targets stored as keys).
    pub fn lookup_key(&self, key: crate::storage::KeyRef) -> Option<(usize, DocId)> {
        let segs = &self.inner.segments;
        let h = crate::storage::key_hash(key);
        (0..segs.len()).rev().find_map(|i| {
            let pk = self.inner.pks[i].as_ref()?;
            let mut docs = pk.lookup_hashed(Some(key), h);
            docs.find(|&d| segs[i].live.as_ref().is_none_or(|l| l.contains(d))).map(|d| (i, d))
        })
    }

    /// The primary key of document `doc` of segment `seg`.
    pub fn key(&self, seg: usize, doc: DocId) -> Option<Value> {
        Some(self.pk(seg)?.key(doc))
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
        crate::iostat::evict(&self.inner.path)?;
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
    pub gen: u64,
    pub chunk: usize,
    pub len: usize,
    pub docs: Arc<Vec<Document>>,
    pub live: Option<Arc<LiveDocs>>,
    pub n_live: u32,
    /// Already built, if the cache held this prefix.
    pub seg: Option<MemSeg>,
}

/// Everything a snapshot needs, taken under the state lock.
pub(crate) struct Capture {
    pub version: u64,
    txn: u64,
    pin: Pin,
    disk: Vec<(Arc<SegHandle>, Option<Arc<LiveDocs>>, u32)>,
    pub mem: Vec<MemPart>,
}

impl State {
    /// Takes what a snapshot of the current version needs (without the
    /// memtable unless `with_mem`).
    pub(crate) fn capture(&mut self, pins: &Arc<Pins>, with_mem: bool) -> Capture {
        let mut disk = Vec::with_capacity(self.handles.len());
        for h in &self.handles {
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
            disk.push((h.clone(), live, n_live));
        }
        let gen = self.mem.gen;
        let n_chunks = if with_mem { self.mem.chunks.len() } else { 0 };
        if self.mem_live.len() < n_chunks {
            self.mem_live.resize(n_chunks, None);
        }
        let mut mem = Vec::new();
        for k in 0..n_chunks {
            let (len, n_deleted) = (self.mem.chunks[k].len(), self.mem.chunk_deleted[k]);
            if len == n_deleted {
                continue;
            }
            let mut live = None;
            if n_deleted > 0 {
                let hit = self.mem_live[k].as_ref().filter(|c| (c.0, c.1) == (len, n_deleted)).map(|c| c.2.clone());
                live = Some(hit.unwrap_or_else(|| {
                    let f = Arc::new(LiveDocs::from_deleted(self.mem.chunk_deleted_words(k), len as u32));
                    self.mem_live[k] = Some((len, n_deleted, f.clone()));
                    f
                }));
            }
            let seg = self.memseg(gen, k, len);
            let docs = self.mem.chunks[k].clone();
            mem.push(MemPart { gen, chunk: k, len, docs, live, n_live: (len - n_deleted) as u32, seg });
        }
        Capture { version: self.version, txn: self.txn, pin: pins.pin(self.txn), disk, mem }
    }

    /// The cached segment of the first `len` documents of chunk `k`.
    pub(crate) fn memseg(&self, gen: u64, k: usize, len: usize) -> Option<MemSeg> {
        self.memsegs.get(k)?.as_ref().filter(|s| (s.gen, s.len) == (gen, len)).cloned()
    }

    /// Caches a built memtable segment unless the memtable moved past it.
    pub(crate) fn keep_memseg(&mut self, seg: &MemSeg) {
        if seg.gen != self.mem.gen {
            return;
        }
        if self.memsegs.len() <= seg.chunk {
            self.memsegs.resize(seg.chunk + 1, None);
        }
        let slot = &mut self.memsegs[seg.chunk];
        if slot.as_ref().is_none_or(|s| s.len < seg.len) {
            *slot = Some(seg.clone());
        }
    }
}

/// Builds the segment of a memtable chunk's first `len` documents.
pub(crate) fn build_memseg(schema: &Schema, opts: &BuildOptions, part: &MemPart, id: u64) -> MemSeg {
    // The documents were validated when they were written, so a failure
    // here is a bug in an index builder.
    let image = build_segment(schema, &part.docs[..part.len], opts).expect("building a memtable segment");
    let file = SegmentFile::open(OwnedBytes::new(PageAligned::new(&image))).expect("memtable segment opens");
    let pk = PkIndex::open(&file).expect("memtable pk index");
    MemSeg { gen: part.gen, chunk: part.chunk, len: part.len, id, file, pk }
}

impl Capture {
    pub(crate) fn assemble(self, schema: Arc<Schema>, path: PathBuf) -> SnapInner {
        let mut segments = Vec::with_capacity(self.disk.len() + 1);
        let mut pks = Vec::with_capacity(self.disk.len() + 1);
        let mut maps: Vec<OwnedBytes> = Vec::new();
        for (h, live, n_live) in self.disk {
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
            });
        }
        for m in self.mem {
            let seg = m.seg.expect("memtable segment built before assembly");
            pks.push(seg.pk);
            segments.push(SegmentView {
                id: seg.id,
                file: seg.file,
                live: m.live.map(|l| l as Arc<dyn DocFilter>),
                n_live: m.n_live,
                in_memory: true,
            });
        }
        let n_live = segments.iter().map(|s| s.n_live as u64).sum();
        SnapInner { txn: self.txn, schema, segments, pks, n_live, maps, path, _pin: self.pin }
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
