//! The database: one file plus a `-wal` sidecar (or a directory of files,
//! [`Layout::Directory`]), one writer, any number of snapshot readers.
//!
//! * Write transactions append to the WAL and `fsync` at commit; their
//!   documents and deletes land in the memtable, visible to the next
//!   snapshot. Deletes of on-disk documents stay *pending* (an overlay on
//!   the segment's delete set) until a flush.
//! * A flush, a bulk load and a merge each write a new on-disk version with
//!   the shadow-paging commit ([`VersionWriter`]): new extents into free
//!   pages, `fsync`, then the other meta slot, `fsync`.
//! * Readers never take the writer lock; the state lock is held only to
//!   copy pointers, never across I/O or index builds.
//! * With [`DbOptions::publish_snapshots`] the writer also builds each new
//!   version's snapshot before its commit returns, and readers only clone
//!   the published one: they never build a memtable segment or wait for
//!   one to be built.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, TryLockError};
use std::time::Instant;

use serde::Serialize;

use crate::error::{corrupt, Error, Result};
use crate::schema::{FieldType, Schema};
use crate::segment::build::{build_segment, BuildOptions};
use crate::segment::{DocId, SegmentFile};
use crate::storage::freelist::FreeList;
use crate::storage::dir::{self, Current};
use crate::storage::live::LiveDocs;
use crate::storage::lock;
use crate::storage::manifest::{Manifest, SegmentMeta};
use crate::storage::merge::{MergePolicy, MergeStats, SegmentInfo};
use crate::storage::pager::{self, Extent, Meta, META_PAGES, PAGE};
use crate::postings::bytes::OwnedBytes;
use crate::storage::snapshot::{build_memseg, Pins, Snapshot};
use crate::storage::state::{Key, Loc, Memtable, SegHandle, State};
use crate::storage::updates::Updates;
use crate::storage::wal::{Op, Wal};
use crate::value::{validate, Document, Value};

/// Ids of in-memory segments have this bit set.
const MEM_ID: u64 = 1 << 63;

/// Rebuilds full documents from a segment, for merges. Provided by the
/// layer that owns the doc store and vector sections.
pub trait DocumentSource: Send + Sync {
    /// The documents `docs` (increasing docIDs) of `seg`, in that order.
    fn documents(&self, schema: &Schema, seg: &SegmentFile, docs: &[DocId]) -> Result<Vec<Document>>;
}

/// Options of an open database (not stored in the file).
#[derive(Clone)]
pub struct DbOptions {
    /// Options of every segment build (memtable, flush, bulk load, merge).
    pub build: BuildOptions,
    /// Options of memtable segment builds instead of `build`. A memtable
    /// chunk is rebuilt by the first snapshot after each write, so an index
    /// that is slow to build but fast to search (HNSW) costs more there
    /// than a scan of a few hundred vectors (`vector.index = flat`).
    pub mem_build: Option<BuildOptions>,
    /// Flush when the memtable's documents plus pending deletes exceed this.
    pub memtable_max_docs: usize,
    /// Documents per memtable chunk (rounded up to a multiple of 64). Each
    /// chunk is one in-memory segment of a snapshot; the first snapshot
    /// after a write rebuilds only the last chunk's.
    pub memtable_chunk_docs: usize,
    /// Largest segment written by flushes and bulk loads.
    pub segment_max_docs: usize,
    /// `fsync` at every commit. Without it a process crash still loses
    /// nothing, but a power failure may lose recent commits.
    pub sync: bool,
    /// Needed by [`Database::merge`].
    pub doc_source: Option<Arc<dyn DocumentSource>>,
    /// Kernel read-ahead on the file mapping. With it off (`MADV_RANDOM`) a
    /// page fault reads one page instead of up to 32, which suits random
    /// access (dictionary probes, graph hops, doc store fetches) when the
    /// data is not cached.
    pub read_ahead: bool,
    /// With read-ahead on, advise the sections that are read at random
    /// positions (graph, float32 vectors, norms, dictionaries, doc store,
    /// keys) `MADV_RANDOM`, keeping read-ahead for scanned sections.
    pub random_hints: bool,
    /// Build each version's snapshot when it is made, in the writer (a
    /// commit, flush, bulk load, merge or vacuum returns once readers can
    /// see it), and give readers that one. A reader then never builds a
    /// memtable segment or waits for another reader's build; each write
    /// pays for the build instead of the first query after it.
    pub publish_snapshots: bool,
    /// How a database created with these options is laid out on disk (an
    /// existing one opens in the layout it has).
    pub layout: Layout,
}

/// How a database is laid out on disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum Layout {
    /// One file of pages plus a `-wal` sidecar: extents, two meta slots and
    /// the shadow-paging commit.
    #[default]
    SingleFile,
    /// A directory with one file per segment and delete set, and versioned
    /// manifests named by `CURRENT` ([`crate::storage::dir`]).
    Directory,
}

impl Default for DbOptions {
    fn default() -> Self {
        DbOptions {
            build: BuildOptions::default(),
            mem_build: None,
            memtable_max_docs: 10_000,
            memtable_chunk_docs: 1024,
            segment_max_docs: 100_000,
            sync: true,
            doc_source: None,
            read_ahead: true,
            random_hints: true,
            publish_snapshots: false,
            layout: Layout::SingleFile,
        }
    }
}

/// Space use of the file and the WAL.
#[derive(Clone, Debug, Serialize)]
pub struct FileStats {
    pub file_bytes: u64,
    pub page_count: u64,
    /// Pages reachable from the current version (meta slots included).
    pub live_pages: u64,
    pub free_pages: u64,
    /// Freed pages not yet reusable (held for a snapshot or the older slot).
    pub pending_pages: u64,
    pub wal_bytes: u64,
    pub memtable_docs: usize,
    pub pending_deletes: usize,
    pub segments: Vec<SegmentStats>,
}

/// Size of one on-disk segment.
#[derive(Clone, Debug, Serialize)]
pub struct SegmentStats {
    pub id: u64,
    pub n_docs: u32,
    pub n_deleted: u32,
    pub bytes: u64,
    pub delete_set_bytes: u64,
}

/// An open database. `Send + Sync`: share it (e.g. in an `Arc`) between a
/// writer thread, readers and a background merger.
pub struct Database {
    path: PathBuf,
    wal_path: PathBuf,
    opts: DbOptions,
    schema: Arc<Schema>,
    /// The database file, or the directory layout's `LOCK` file.
    file: File,
    /// The directory, in the directory layout.
    dir: Option<PathBuf>,
    /// The single-writer lock; it owns the WAL.
    writer: Mutex<Wal>,
    state: Mutex<State>,
    /// Serialises memtable segment builds, so concurrent readers build once.
    build_lock: Mutex<()>,
    /// The snapshot readers get, with `DbOptions::publish_snapshots`.
    published: RwLock<Option<Snapshot>>,
    merge_lock: Mutex<()>,
    pins: Arc<Pins>,
    mem_ids: AtomicU64,
    /// Set when a write failed in a way that leaves memory and disk out of
    /// step; every later write fails until the database is reopened.
    poisoned: AtomicBool,
}

impl Database {
    /// Creates a new database (the path must not exist) in the layout
    /// `opts.layout` names, and its WAL.
    pub fn create(path: &Path, schema: Schema, opts: DbOptions) -> Result<Database> {
        if opts.layout == Layout::Directory {
            return Database::create_dir(path, schema, opts);
        }
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        lock_exclusive(&file)?;
        let db_id: u64 = rand::random();
        let manifest = Manifest { schema, txn: 1, wal_txn: 0, next_segment_id: 1, segments: Vec::new() };
        let mut fl = FreeList::new(META_PAGES);
        let mbytes = manifest.encode();
        let mext = Extent { page: fl.alloc(pages_for(mbytes.len())), len: mbytes.len() as u64 };
        let (fext, fbytes) = place_freelist(&mut fl);
        file.set_len(fl.page_count() * PAGE)?;
        file.write_all_at(&mbytes, mext.page * PAGE)?;
        file.write_all_at(&fbytes, fext.page * PAGE)?;
        let meta = Meta {
            db_id,
            txn: 1,
            manifest: mext,
            manifest_crc: crc32fast::hash(&mbytes),
            freelist: fext,
            freelist_crc: crc32fast::hash(&fbytes),
            page_count: fl.page_count(),
        };
        pager::write_meta(&file, 0, &meta)?;
        file.sync_all()?;
        Wal::create(&wal_path(path), db_id, true)?;
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        File::open(dir)?.sync_all()?;
        Database::load(path, file, opts)
    }

    /// Opens an existing database, recovering from a crash: the newest valid
    /// meta slot (or `CURRENT`) names the on-disk version, and committed WAL
    /// transactions not yet in it are replayed into the memtable.
    pub fn open(path: &Path, opts: DbOptions) -> Result<Database> {
        if path.is_dir() {
            let lockf = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path.join(dir::LOCK))?;
            lock_exclusive(&lockf)?;
            return Database::load_dir(path, lockf, opts);
        }
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        lock_exclusive(&file)?;
        Database::load(path, file, opts)
    }

    fn create_dir(path: &Path, schema: Schema, opts: DbOptions) -> Result<Database> {
        std::fs::create_dir(path)?;
        let lockf = OpenOptions::new().read(true).write(true).create_new(true).open(path.join(dir::LOCK))?;
        lock_exclusive(&lockf)?;
        let db_id: u64 = rand::random();
        let manifest = Manifest { schema, txn: 1, wal_txn: 0, next_segment_id: 1, segments: Vec::new() };
        let mbytes = manifest.encode();
        let name = dir::manifest_name(1);
        dir::write_file(path, &name, &mbytes, true)?;
        dir::write_current(path, &Current { db_id, txn: 1, manifest: name, crc: crc32fast::hash(&mbytes) }, true)?;
        Wal::create(&path.join(dir::WAL), db_id, true)?;
        dir::sync_dir(path)?;
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        File::open(parent)?.sync_all()?;
        Database::load_dir(path, lockf, opts)
    }

    fn load_dir(path: &Path, lockf: File, opts: DbOptions) -> Result<Database> {
        let (cur, manifest) = dir::read_current(path)?;
        // Files a crash left behind: written for a commit that did not
        // finish, or no longer named after one that did.
        for (f, _) in dir::orphans(path, &dir::referenced(&cur, &manifest))? {
            std::fs::remove_file(f)?;
        }
        let handles = manifest
            .segments
            .iter()
            .map(|m| SegHandle::open_in(path, m, opts.read_ahead, opts.read_ahead && opts.random_hints).map(Arc::new))
            .collect::<Result<_>>()?;
        let meta = dir_meta(&cur);
        Database::assemble(path, path.join(dir::WAL), Some(path.to_path_buf()), lockf, opts, manifest, meta, 0, FreeList::new(META_PAGES), OwnedBytes::empty(), handles)
    }

    fn load(path: &Path, file: File, opts: DbOptions) -> Result<Database> {
        let (meta, slot) = pager::read_meta(&file)?;
        let manifest = Manifest::decode(&pager::read_extent(&file, meta.manifest, meta.manifest_crc)?)?;
        let freelist = FreeList::decode(&pager::read_extent(&file, meta.freelist, meta.freelist_crc)?, meta.page_count)?;
        let len = file.metadata()?.len();
        if len < meta.page_count * PAGE {
            return Err(corrupt("file is shorter than its page count"));
        }
        if len > meta.page_count * PAGE {
            // Left by a commit that did not finish; nothing valid lives there.
            file.set_len(meta.page_count * PAGE)?;
        }
        let map = pager::map_file(&file, opts.read_ahead)?;
        let handles = manifest.segments.iter().map(|m| SegHandle::open(&map, m, opts.read_ahead && opts.random_hints).map(Arc::new)).collect::<Result<_>>()?;
        Database::assemble(path, wal_path(path), None, file, opts, manifest, meta, slot, freelist, map, handles)
    }

    /// The open database around an on-disk version: replays the WAL into
    /// the memtable.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        path: &Path,
        wal_path: PathBuf,
        dir: Option<PathBuf>,
        file: File,
        opts: DbOptions,
        manifest: Manifest,
        meta: Meta,
        slot: usize,
        freelist: FreeList,
        map: OwnedBytes,
        handles: Vec<Arc<SegHandle>>,
    ) -> Result<Database> {
        let (mut wal, txns) = Wal::open(&wal_path, meta.db_id, opts.sync)?;
        let schema = Arc::new(manifest.schema.clone());
        let mut st = State {
            manifest,
            meta,
            slot,
            freelist,
            file_pages: meta.page_count,
            map,
            handles,
            txn: meta.txn,
            version: 0,
            mem: Memtable::new(opts.memtable_chunk_docs, 0),
            pending: Default::default(),
            pending_updates: Default::default(),
            n_pending: 0,
            live_cache: HashMap::new(),
            updates_cache: HashMap::new(),
            mem_live: Vec::new(),
            memsegs: Vec::new(),
            cached: None,
        };
        let mut flushed = false;
        for t in txns {
            if t.txn <= st.manifest.wal_txn {
                // Flushed before a crash cut the WAL truncation short.
                flushed = true;
                continue;
            }
            for op in t.ops {
                st.apply(&schema, op);
            }
            st.txn = st.txn.max(t.txn);
        }
        if flushed && st.mem.len == 0 && st.pending.is_empty() {
            wal.reset(opts.sync)?;
        }
        let db = Database {
            path: path.to_path_buf(),
            wal_path,
            opts,
            schema,
            file,
            dir,
            writer: Mutex::new(wal),
            state: Mutex::new(st),
            build_lock: Mutex::new(()),
            published: RwLock::new(None),
            merge_lock: Mutex::new(()),
            pins: Arc::new(Pins::default()),
            mem_ids: AtomicU64::new(0),
            poisoned: AtomicBool::new(false),
        };
        db.republish();
        Ok(db)
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn options(&self) -> &DbOptions {
        &self.opts
    }

    /// Starts the write transaction; `Error::Busy` while another one (or a
    /// flush, bulk load or merge commit) is in progress.
    pub fn begin(&self) -> Result<WriteTxn<'_>> {
        let wal = self.try_writer()?;
        self.check_poisoned()?;
        Ok(WriteTxn { db: self, wal, ops: Vec::new(), touched: HashMap::new(), written: HashMap::new(), sets: HashMap::new() })
    }

    /// The current version. Never waits for a writer; the first snapshot
    /// after a write rebuilds the last memtable chunk's segment, unless
    /// `DbOptions::publish_snapshots` has the writer build it.
    pub fn snapshot(&self) -> Snapshot {
        if let Some(s) = &*self.published.read().unwrap_or_else(|e| e.into_inner()) {
            return s.clone();
        }
        self.snapshot_inner(true)
    }

    /// With `publish_snapshots`, builds the current version's snapshot and
    /// hands it to readers from now on. Callers hold the writer lock (or
    /// are opening the database), so versions are published in order.
    fn republish(&self) {
        if self.opts.publish_snapshots {
            let s = self.snapshot_inner(true);
            *self.published.write().unwrap_or_else(|e| e.into_inner()) = Some(s);
        }
    }

    /// A snapshot without the memtable (merges read only on-disk segments).
    fn snapshot_inner(&self, with_mem: bool) -> Snapshot {
        let mut cap = {
            let mut st = lock(&self.state);
            if let (true, Some(s)) = (with_mem, &st.cached) {
                return Snapshot { inner: s.clone() };
            }
            st.capture(&self.pins, with_mem)
        };
        if cap.mem.iter().any(|p| p.seg.is_none()) {
            let _b = lock(&self.build_lock);
            for part in cap.mem.iter_mut().filter(|p| p.seg.is_none()) {
                // Another reader may have built it while we waited.
                let built = lock(&self.state).memseg(part.gen, part.chunk, part.len);
                part.seg = Some(built.unwrap_or_else(|| {
                    let id = MEM_ID | self.mem_ids.fetch_add(1, Ordering::Relaxed);
                    let seg = build_memseg(&self.schema, self.opts.mem_build.as_ref().unwrap_or(&self.opts.build), part, id);
                    lock(&self.state).keep_memseg(&seg);
                    seg
                }));
            }
        }
        let version = cap.version;
        let inner = Arc::new(cap.assemble(self.schema.clone(), self.path.clone()));
        if with_mem {
            let mut st = lock(&self.state);
            if st.version == version {
                st.cached = Some(inner.clone());
            }
        }
        Snapshot { inner }
    }

    /// Writes the memtable as segments and pending deletes into delete sets,
    /// then empties the WAL.
    pub fn flush(&self) -> Result<()> {
        let mut wal = self.try_writer()?;
        self.flush_locked(&mut wal)
    }

    fn flush_locked(&self, wal: &mut Wal) -> Result<()> {
        self.check_poisoned()?;
        let (chunks, deleted, pending, mem_updates, pending_updates, txn) = {
            let st = lock(&self.state);
            if st.mem.len == 0 && st.pending.is_empty() && st.pending_updates.is_empty() {
                return Ok(());
            }
            (st.mem.chunks.clone(), st.mem.deleted.clone(), st.pending.clone(), st.mem.updates.clone(), st.pending_updates.clone(), st.txn)
        };
        let live: Vec<Document> = chunks
            .iter()
            .flat_map(|c| c.iter())
            .enumerate()
            .filter(|(i, _)| deleted[i / 64] >> (i % 64) & 1 == 0)
            .map(|(i, d)| {
                let mut d = d.clone();
                mem_updates.apply(i as DocId, &mut d);
                d
            })
            .collect();
        drop(chunks);
        let mut vw = VersionWriter::begin(self);
        vw.manifest.wal_txn = txn;
        for chunk in live.chunks(self.opts.segment_max_docs.max(1)) {
            let image = build_segment(&self.schema, chunk, &self.opts.build)?;
            vw.add_segment(None, &image, chunk.len() as u32, None, None)?;
        }
        for (&id, docs) in &pending {
            vw.add_deletes(id, docs)?;
        }
        for (&id, (_, ups)) in &pending_updates {
            vw.add_updates(id, ups)?;
        }
        let c = vw.commit()?;
        self.publish(c, |st| {
            st.mem.clear();
            st.mem_live.clear();
            st.memsegs.clear();
            st.pending.clear();
            st.pending_updates.clear();
            st.updates_cache.clear();
            st.n_pending = 0;
        })?;
        wal.reset(self.opts.sync).map_err(|e| self.poison(e))
    }

    /// Loads documents straight into segments of `segment_max_docs`, in one
    /// commit and without the WAL. Keys must be new (as with `insert`).
    pub fn bulk_load(&self, docs: impl IntoIterator<Item = Document>) -> Result<()> {
        let mut wal = self.try_writer()?;
        // WAL deletes are replayed against the on-disk version; segments
        // written behind the WAL's back must not hold keys it touches.
        self.flush_locked(&mut wal)?;
        let mut vw = VersionWriter::begin(self);
        let pk = self.schema.primary_key;
        let check_existing = !lock(&self.state).handles.is_empty();
        let mut seen = HashSet::new();
        let mut chunk = Vec::new();
        let max = self.opts.segment_max_docs.max(1);
        for doc in docs {
            validate(&self.schema, &doc)?;
            if let Some(pk) = pk {
                let dup = !seen.insert(Key::of(&doc[pk]).expect("validated key"))
                    || (check_existing && lock(&self.state).find_live(&doc[pk]).is_some());
                if dup {
                    return Err(Error::Schema(format!("duplicate primary key {:?}", doc[pk])));
                }
            }
            chunk.push(doc);
            if chunk.len() == max {
                let image = build_segment(&self.schema, &chunk, &self.opts.build)?;
                vw.add_segment(None, &image, chunk.len() as u32, None, None)?;
                chunk.clear();
            }
        }
        if !chunk.is_empty() {
            let image = build_segment(&self.schema, &chunk, &self.opts.build)?;
            vw.add_segment(None, &image, chunk.len() as u32, None, None)?;
        }
        if vw.segments_added == 0 {
            return Ok(());
        }
        let c = vw.commit()?;
        self.publish(c, |_| {})
    }

    /// Writes segment `id`'s image to `dest` (a new file), for another
    /// database to attach ([`Database::attach_segments`]): the same bytes in
    /// either layout. An image carries no delete set, so a segment with
    /// deleted documents must be merged first.
    pub fn export_segment(&self, id: u64, dest: &Path) -> Result<()> {
        let (h, pending) = {
            let st = lock(&self.state);
            let h = st.handles.iter().find(|h| h.meta.id == id).cloned().ok_or_else(|| Error::Schema(format!("no segment {id}")))?;
            (h, st.pending.get(&id).map_or(0, |p| p.len()) + st.pending_updates.get(&id).map_or(0, |p| p.1.len()))
        };
        if h.meta.n_deleted > 0 || h.updates.is_some() || pending > 0 {
            return Err(Error::Schema(format!("segment {id} has deleted or updated documents; merge it first")));
        }
        let image = if self.dir.is_some() { h.map.clone() } else { h.map.slice(h.meta.extent.range()) };
        let mut f = OpenOptions::new().write(true).create_new(true).open(dest)?;
        std::io::Write::write_all(&mut f, &image)?;
        f.sync_data()?;
        Ok(())
    }

    /// Adds segment images built elsewhere (by [`Database::export_segment`]
    /// or [`crate::segment::build::build_segment`] with this schema) in one
    /// commit, after flushing the memtable. Their primary keys must be new.
    /// Returns the new segments' ids.
    pub fn attach_segments(&self, images: &[PathBuf]) -> Result<Vec<u64>> {
        let mut wal = self.try_writer()?;
        // As for bulk loads: WAL deletes are replayed against the on-disk
        // version, so segments added behind the WAL's back must follow it.
        self.flush_locked(&mut wal)?;
        let pk = self.schema.primary_key;
        let mut seen = HashSet::new();
        let mut loaded = Vec::with_capacity(images.len());
        for path in images {
            let bytes = std::fs::read(path)?;
            let file = SegmentFile::open(OwnedBytes::new(bytes.clone())).map_err(|e| Error::Schema(format!("{}: {e}", path.display())))?;
            let keys = crate::storage::pk::PkIndex::open(&file)?;
            if pk.is_some() != keys.is_some() {
                return Err(Error::Schema(format!("{}: primary key index does not match the table", path.display())));
            }
            if let Some(keys) = keys {
                let st = lock(&self.state);
                for d in 0..file.n_docs() as DocId {
                    let k = keys.key(d);
                    if !seen.insert(Key::of(&k).ok_or_else(|| corrupt("bad key"))?) || st.find_live(&k).is_some() {
                        return Err(Error::Schema(format!("{}: duplicate primary key {k:?}", path.display())));
                    }
                }
            }
            loaded.push((bytes, file.n_docs() as u32));
        }
        let mut vw = VersionWriter::begin(self);
        let first = vw.manifest.next_segment_id;
        for (bytes, n) in &loaded {
            vw.add_segment(None, bytes, *n, None, None)?;
        }
        let ids: Vec<u64> = (first..vw.manifest.next_segment_id).collect();
        if ids.is_empty() {
            return Ok(ids);
        }
        let c = vw.commit()?;
        self.publish(c, |_| {})?;
        Ok(ids)
    }

    /// Merges on-disk segments as `policy` selects until it selects nothing.
    /// Inputs are rebuilt from `DbOptions::doc_source` without blocking
    /// writers; the commit then marks documents deleted in the meantime as
    /// deleted in the merged segment. Must not be called by a thread that
    /// holds a [`WriteTxn`] (the commit waits for the writer lock).
    pub fn merge(&self, policy: &MergePolicy) -> Result<MergeStats> {
        let _m = lock(&self.merge_lock);
        let source = self.opts.doc_source.clone().ok_or_else(|| Error::Schema("merging needs DbOptions::doc_source".into()))?;
        let t0 = Instant::now();
        let mut stats = MergeStats::default();
        // Each merge removes a segment, except rewrites of a single segment
        // to drop deletes; the bound stops those from repeating forever
        // under a concurrent stream of deletes.
        let mut rounds = 2 * lock(&self.state).handles.len() + 1;
        while rounds > 0 {
            rounds -= 1;
            let snap = self.snapshot_inner(false);
            let infos: Vec<SegmentInfo> =
                snap.segments().iter().map(|v| SegmentInfo { id: v.id, n_docs: v.file.n_docs() as u32, n_live: v.n_live }).collect();
            let Some(mut group) = policy.select(&infos) else { break };
            // Oldest first, so the merged segment keeps documents in age order.
            group.sort_by_key(|id| infos.iter().position(|s| s.id == *id));
            self.merge_group(&snap, &group, source.as_ref(), &mut stats)?;
        }
        stats.seconds = t0.elapsed().as_secs_f64();
        Ok(stats)
    }

    fn merge_group(&self, snap: &Snapshot, ids: &[u64], source: &dyn DocumentSource, stats: &mut MergeStats) -> Result<()> {
        let mut inputs: Vec<(u64, Vec<DocId>)> = Vec::new();
        let mut docs: Vec<Document> = Vec::new();
        for &id in ids {
            let v = snap.segments().iter().find(|v| v.id == id).expect("selected from this snapshot");
            let n = v.file.n_docs() as DocId;
            let live: Vec<DocId> = (0..n).filter(|&d| v.live.as_ref().is_none_or(|l| l.contains(d))).collect();
            let mut got = source.documents(&self.schema, &v.file, &live)?;
            if got.len() != live.len() {
                return Err(corrupt(format!("document source returned {} of {} documents", got.len(), live.len())));
            }
            // The merged segment holds the values written in place.
            if let Some(u) = &v.updates {
                for (doc, &d) in got.iter_mut().zip(&live) {
                    u.apply(d, doc);
                }
            }
            stats.docs_dropped += (n as usize - live.len()) as u64;
            docs.extend(got);
            inputs.push((id, live));
        }
        let n_new = docs.len() as u32;
        let image = if docs.is_empty() { None } else { Some(build_segment(&self.schema, &docs, &self.opts.build)?) };
        drop(docs);

        let _wal = lock(&self.writer);
        self.check_poisoned()?;
        // Documents of the inputs deleted after the snapshot, as docIDs of
        // the merged segment (inputs are concatenated in order), and values
        // written in place after it.
        let mut deleted = Vec::new();
        let mut updated = Updates::default();
        {
            let mut st = lock(&self.state);
            let mut base = 0u32;
            for (id, live) in &inputs {
                let h = st.handles.iter().find(|h| h.meta.id == *id).cloned().ok_or_else(|| corrupt("merge input vanished"))?;
                deleted.extend(live.iter().enumerate().filter(|&(_, &d)| st.is_deleted(&h, d)).map(|(j, _)| base + j as u32));
                let then = snap.segments().iter().find(|v| v.id == *id).and_then(|v| v.updates.clone());
                if let Some(now) = st.updates_of(&h) {
                    for (f, d, v) in now.iter() {
                        if then.as_ref().and_then(|t| t.get(f, d)) != Some(v) {
                            if let Ok(j) = live.binary_search(&d) {
                                updated.set(f, base + j as u32, v.clone());
                            }
                        }
                    }
                }
                base += live.len() as u32;
            }
        }
        let mut vw = VersionWriter::begin(self);
        // The merged segment takes the place of the oldest input.
        let pos = vw.manifest.segments.iter().position(|s| ids.contains(&s.id)).expect("inputs present");
        for &id in ids {
            vw.remove_segment(id);
        }
        if let Some(image) = image {
            let dels = (!deleted.is_empty()).then(|| LiveDocs::with_deleted(None, n_new, deleted.iter().copied()));
            vw.add_segment(Some(pos), &image, n_new, dels.as_ref(), Some(&updated).filter(|u| !u.is_empty()))?;
        }
        let c = vw.commit()?;
        stats.bytes_written += c.bytes_written;
        // Pending deletes of the inputs are now either dropped documents or
        // stored in the merged segment's delete set.
        self.publish(c, |st| {
            for id in ids {
                if let Some(p) = st.pending.remove(id) {
                    st.n_pending -= p.len();
                }
                if let Some((_, u)) = st.pending_updates.remove(id) {
                    st.n_pending -= u.len();
                }
                st.updates_cache.remove(id);
            }
        })?;
        stats.merges += 1;
        stats.segments_in += ids.len();
        stats.docs_merged += (n_new as usize - deleted.len()) as u64;
        Ok(())
    }

    /// Returns space to the file system: moves segments and delete sets from
    /// the end of the file into free runs below them and truncates the free
    /// tail, committing until nothing moves (pages freed by a commit become
    /// reusable two commits later). Space a live snapshot can still reach
    /// stays. Returns the number of bytes the file shrank by.
    pub fn vacuum(&self) -> Result<u64> {
        let _wal = self.try_writer()?;
        self.check_poisoned()?;
        if let Some(d) = &self.dir {
            // Each segment is its own file, so there is nothing to compact:
            // only files a crash or a failed unlink left behind.
            let keep = {
                let st = lock(&self.state);
                dir::referenced(&Current { db_id: st.meta.db_id, txn: st.meta.txn, manifest: dir::manifest_name(st.manifest.txn), crc: st.meta.manifest_crc }, &st.manifest)
            };
            let mut freed = 0;
            for (f, len) in dir::orphans(d, &keep)? {
                std::fs::remove_file(f)?;
                freed += len;
            }
            return Ok(freed);
        }
        let before = lock(&self.state).meta.page_count;
        let mut idle = 0;
        while idle < 2 {
            let mut vw = VersionWriter::begin(self);
            let moved = vw.relocate_down()?;
            let c = vw.commit()?;
            let shrank = c.meta.page_count < lock(&self.state).meta.page_count;
            self.publish(c, |_| {})?;
            idle = if moved > 0 || shrank { 0 } else { idle + 1 };
        }
        Ok(before.saturating_sub(lock(&self.state).meta.page_count) * PAGE)
    }

    pub fn file_stats(&self) -> FileStats {
        let st = lock(&self.state);
        if let Some(d) = &self.dir {
            return FileStats {
                file_bytes: dir::size(d),
                page_count: 0,
                live_pages: 0,
                free_pages: 0,
                pending_pages: 0,
                wal_bytes: std::fs::metadata(&self.wal_path).map_or(0, |m| m.len()),
                memtable_docs: st.mem.len - st.mem.n_deleted,
                pending_deletes: st.n_pending,
                segments: st
                    .manifest
                    .segments
                    .iter()
                    .map(|s| SegmentStats { id: s.id, n_docs: s.n_docs, n_deleted: s.n_deleted, bytes: s.extent.len, delete_set_bytes: s.deletes.map_or(0, |e| e.len) })
                    .collect(),
            };
        }
        let segments: Vec<SegmentStats> = st
            .manifest
            .segments
            .iter()
            .map(|s| SegmentStats {
                id: s.id,
                n_docs: s.n_docs,
                n_deleted: s.n_deleted,
                bytes: s.extent.len,
                delete_set_bytes: s.deletes.map_or(0, |e| e.len),
            })
            .collect();
        let seg_pages: u64 =
            st.manifest.segments.iter().map(|s| s.extent.pages() + s.deletes.map_or(0, |e| e.pages()) + s.updates.map_or(0, |e| e.pages())).sum();
        FileStats {
            file_bytes: self.file.metadata().map_or(st.file_pages * PAGE, |m| m.len()),
            page_count: st.meta.page_count,
            live_pages: META_PAGES + st.meta.manifest.pages() + st.meta.freelist.pages() + seg_pages,
            free_pages: st.freelist.free_pages(),
            pending_pages: st.freelist.pending_pages(),
            wal_bytes: std::fs::metadata(&self.wal_path).map_or(0, |m| m.len()),
            memtable_docs: st.mem.len - st.mem.n_deleted,
            pending_deletes: st.n_pending,
            segments,
        }
    }

    /// Verifies the file: the meta slot and manifest on disk match the open
    /// version, every segment and delete set matches its checksum, every
    /// key is found by its segment's index, and every page is used exactly
    /// once (meta slot, extent, free run or pending free). `Error::Busy`
    /// while a write is in progress.
    pub fn check(&self) -> Result<()> {
        let _w = self.try_writer()?;
        if let Some(d) = &self.dir {
            return self.check_dir(d);
        }
        // The writer lock keeps the version still; copy it so readers are
        // not held up while every byte is checksummed.
        let (st_meta, st_manifest, freelist, handles, map) = {
            let st = lock(&self.state);
            (st.meta, st.manifest.clone(), st.freelist.clone(), st.handles.clone(), st.map.clone())
        };
        let (meta, _) = pager::read_meta(&self.file)?;
        let manifest = Manifest::decode(&pager::read_extent(&self.file, meta.manifest, meta.manifest_crc)?)?;
        if meta != st_meta || manifest != st_manifest {
            return Err(corrupt("on-disk version differs from the open one"));
        }
        let mut used = vec![false; meta.page_count as usize];
        let mut claim = |what: &str, page: u64, pages: u64| -> Result<()> {
            for p in page..page + pages {
                match used.get_mut(p as usize) {
                    Some(u) if !*u => *u = true,
                    _ => return Err(corrupt(format!("page {p} ({what}) is out of range or used twice"))),
                }
            }
            Ok(())
        };
        claim("meta", 0, META_PAGES)?;
        claim("manifest", meta.manifest.page, meta.manifest.pages())?;
        claim("freelist", meta.freelist.page, meta.freelist.pages())?;
        for (s, h) in manifest.segments.iter().zip(&handles) {
            claim("segment", s.extent.page, s.extent.pages())?;
            if crc32fast::hash(&map[s.extent.range()]) != s.crc {
                return Err(corrupt(format!("segment {} fails its checksum", s.id)));
            }
            if let Some(e) = s.deletes {
                claim("delete set", e.page, e.pages())?;
                if crc32fast::hash(&map[e.range()]) != s.deletes_crc {
                    return Err(corrupt(format!("delete set of segment {} fails its checksum", s.id)));
                }
                let n: u32 = h.base.as_ref().map_or(0, |b| b.deleted_words().iter().map(|w| w.count_ones()).sum());
                if n != s.n_deleted {
                    return Err(corrupt(format!("segment {} has {n} deletes, manifest says {}", s.id, s.n_deleted)));
                }
            }
            if let Some(e) = s.updates {
                claim("updates", e.page, e.pages())?;
                if crc32fast::hash(&map[e.range()]) != s.updates_crc {
                    return Err(corrupt(format!("updates of segment {} fail their checksum", s.id)));
                }
            }
            if let Some(pk) = &h.pk {
                for d in 0..s.n_docs {
                    if !pk.lookup_all(&pk.key(d)).any(|x| x == d) {
                        return Err(corrupt(format!("segment {} key of document {d} not found", s.id)));
                    }
                }
            }
        }
        for (start, len) in freelist.runs() {
            claim("free run", start, len)?;
        }
        for p in freelist.pending() {
            claim("pending free", p.page, p.pages)?;
        }
        match used.iter().position(|u| !u) {
            Some(p) => Err(corrupt(format!("page {p} is neither used nor free"))),
            None => Ok(()),
        }
    }

    /// [`Database::check`] in the directory layout: `CURRENT` names the open
    /// version, every file matches its checksum, every key is found, and no
    /// file is left over.
    fn check_dir(&self, d: &Path) -> Result<()> {
        let (manifest, handles, meta) = {
            let st = lock(&self.state);
            (st.manifest.clone(), st.handles.clone(), st.meta)
        };
        let (cur, on_disk) = dir::read_current(d)?;
        if on_disk != manifest || cur.txn != meta.txn {
            return Err(corrupt("on-disk version differs from the open one"));
        }
        for (s, h) in manifest.segments.iter().zip(&handles) {
            let name = s.file.as_deref().ok_or_else(|| corrupt(format!("segment {} has no file", s.id)))?;
            if crc32fast::hash(&std::fs::read(d.join(name))?) != s.crc {
                return Err(corrupt(format!("{name} fails its checksum")));
            }
            if let Some(df) = &s.deletes_file {
                if crc32fast::hash(&std::fs::read(d.join(df))?) != s.deletes_crc {
                    return Err(corrupt(format!("{df} fails its checksum")));
                }
                let n: u32 = h.base.as_ref().map_or(0, |b| b.deleted_words().iter().map(|w| w.count_ones()).sum());
                if n != s.n_deleted {
                    return Err(corrupt(format!("segment {} has {n} deletes, manifest says {}", s.id, s.n_deleted)));
                }
            }
            if let Some(uf) = &s.updates_file {
                if crc32fast::hash(&std::fs::read(d.join(uf))?) != s.updates_crc {
                    return Err(corrupt(format!("{uf} fails its checksum")));
                }
            }
            if let Some(pk) = &h.pk {
                for doc in 0..s.n_docs {
                    if !pk.lookup_all(&pk.key(doc)).any(|x| x == doc) {
                        return Err(corrupt(format!("segment {} key of document {doc} not found", s.id)));
                    }
                }
            }
        }
        match dir::orphans(d, &dir::referenced(&cur, &manifest))?.first() {
            Some((f, _)) => Err(corrupt(format!("{} is named by no version", f.display()))),
            None => Ok(()),
        }
    }

    fn try_writer(&self) -> Result<MutexGuard<'_, Wal>> {
        match self.writer.try_lock() {
            Ok(g) => Ok(g),
            Err(TryLockError::Poisoned(p)) => Ok(p.into_inner()),
            Err(TryLockError::WouldBlock) => Err(Error::Busy("another write is in progress".into())),
        }
    }

    fn check_poisoned(&self) -> Result<()> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(Error::Io(std::io::Error::other("an earlier write failed; reopen the database")));
        }
        Ok(())
    }

    fn poison(&self, e: Error) -> Error {
        self.poisoned.store(true, Ordering::Release);
        e
    }

    /// Makes a committed version the current one.
    fn publish(&self, c: Committed, update: impl FnOnce(&mut State)) -> Result<()> {
        let (old, map) = {
            let st = lock(&self.state);
            (st.handles.clone(), st.map.clone())
        };
        let grown = self.dir.is_none() && c.meta.page_count * PAGE > map.len() as u64;
        let map = if grown { pager::map_file(&self.file, self.opts.read_ahead).map_err(|e| self.poison(e))? } else { map };
        let hints = self.opts.read_ahead && self.opts.random_hints;
        let handles = c
            .manifest
            .segments
            .iter()
            .map(|m| match (old.iter().find(|h| h.meta == *m), &self.dir) {
                (Some(h), _) => Ok(h.clone()),
                (None, Some(d)) => SegHandle::open_in(d, m, self.opts.read_ahead, hints).map(Arc::new),
                (None, None) => SegHandle::open(&map, m, hints).map(Arc::new),
            })
            .collect::<Result<Vec<_>>>()
            .map_err(|e| self.poison(e))?;
        let obsolete = c.obsolete;
        {
            let mut st = lock(&self.state);
            st.txn = c.meta.txn;
            st.manifest = c.manifest;
            st.meta = c.meta;
            st.slot = c.slot;
            st.freelist = c.fl;
            st.file_pages = c.file_pages;
            st.map = map;
            st.handles = handles;
            st.live_cache.clear();
            update(&mut st);
            st.changed();
        }
        self.republish();
        // Directory layout: files the new version no longer names. Open
        // snapshots keep reading them through their mappings.
        if let Some(d) = &self.dir {
            for f in obsolete {
                let _ = std::fs::remove_file(d.join(f));
            }
        }
        Ok(())
    }

    fn primary_key(&self) -> Result<usize> {
        self.schema.primary_key.ok_or_else(|| Error::Schema("table has no primary key".into()))
    }

    /// INT field `f` of the live document with primary key `key`: the
    /// value written in place if any, else the document's.
    fn live_int(&self, key: &Value, f: usize) -> Result<Option<i64>> {
        let st = lock(&self.state);
        let (h, d) = match st.find_live(key) {
            None => return Ok(None),
            Some(Loc::Mem(i)) => {
                return Ok(match st.mem.updates.get(f, i) {
                    Some(v) => v.as_i64(),
                    None => st.mem.chunks[i as usize / st.mem.chunk][i as usize % st.mem.chunk][f].as_i64(),
                })
            }
            Some(Loc::Disk(id, d)) => (st.handles.iter().find(|h| h.meta.id == id).cloned().expect("segment of a live key"), d),
        };
        let written = st.pending_updates.get(&h.meta.id).and_then(|(_, u)| u.get(f, d)).or_else(|| h.updates.as_ref().and_then(|u| u.get(f, d)));
        if let Some(v) = written {
            return Ok(v.as_i64());
        }
        drop(st);
        let field = &self.schema.fields[f];
        Ok(if field.indexed {
            crate::forward::NumericColumn::open(&h.file, &field.name)?.get(d).as_i64()
        } else {
            crate::forward::DocStore::open(&h.file, &self.schema)?.get_fields(d, &[f]).pop().and_then(|v| v.as_i64())
        })
    }

    /// Checks that `key` has the primary key's type.
    fn key_of(&self, key: &Value) -> Result<Key> {
        let pk = self.primary_key()?;
        match (&self.schema.fields[pk].ty, key) {
            (FieldType::Keyword, Value::Str(_)) | (FieldType::Int, Value::Int(_)) => Ok(Key::of(key).unwrap()),
            _ => Err(Error::Schema(format!("{key:?} is not a value of primary key {}", self.schema.fields[pk].name))),
        }
    }

    fn commit_ops(&self, wal: &mut Wal, ops: Vec<Op>) -> Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        self.check_poisoned()?;
        let txn = lock(&self.state).txn + 1;
        // A failed append is cut off the log, so the transaction is not
        // committed; but the page cache may be in doubt after a failed
        // fsync, so stop writing.
        wal.append(txn, &ops, self.opts.sync).map_err(|e| self.poison(e))?;
        let full = {
            let mut st = lock(&self.state);
            for op in ops {
                st.apply(&self.schema, op);
            }
            st.txn = txn;
            st.mem.len + st.n_pending > self.opts.memtable_max_docs
        };
        if full {
            // The flush publishes its version; if it fails, the transaction
            // is committed all the same and must be visible.
            let r = self.flush_locked(wal);
            if r.is_err() {
                self.republish();
            }
            return r;
        }
        self.republish();
        Ok(())
    }
}

/// The single write transaction. Operations are buffered and logged at
/// commit; dropping the transaction rolls it back.
pub struct WriteTxn<'a> {
    db: &'a Database,
    wal: MutexGuard<'a, Wal>,
    ops: Vec<Op>,
    /// Keys this transaction made live (true) or deleted (false).
    touched: HashMap<Key, bool>,
    /// Documents this transaction wrote, by key, as they now stand.
    written: HashMap<Key, Arc<Document>>,
    /// Fields this transaction set in place on documents it did not write.
    sets: HashMap<(Key, usize), Value>,
}

impl WriteTxn<'_> {
    /// Inserts a document; its primary key must not be live.
    pub fn insert(&mut self, doc: Document) -> Result<()> {
        validate(&self.db.schema, &doc)?;
        if let Some(pk) = self.db.schema.primary_key {
            let key = Key::of(&doc[pk]).expect("validated key");
            if self.is_live(&key, &doc[pk]) {
                return Err(Error::Schema(format!("duplicate primary key {:?}", doc[pk])));
            }
            self.touched.insert(key.clone(), true);
            let doc = Arc::new(doc);
            self.written.insert(key, doc.clone());
            self.ops.push(Op::Insert(doc));
            return Ok(());
        }
        self.ops.push(Op::Insert(Arc::new(doc)));
        Ok(())
    }

    /// Inserts a document, replacing the live one with the same key.
    pub fn upsert(&mut self, doc: Document) -> Result<()> {
        validate(&self.db.schema, &doc)?;
        let pk = self.db.primary_key()?;
        self.delete(&doc[pk].clone())?;
        let key = Key::of(&doc[pk]).expect("validated key");
        self.touched.insert(key.clone(), true);
        let doc = Arc::new(doc);
        self.written.insert(key, doc.clone());
        self.ops.push(Op::Insert(doc));
        Ok(())
    }

    /// Upserts `doc` only if its INT field `version` is greater than the
    /// live document's, or no document has its key: external versions, so
    /// that writers whose updates arrive out of order keep the newest.
    /// Returns whether it was written.
    pub fn upsert_if_newer(&mut self, doc: Document, version: &str) -> Result<bool> {
        validate(&self.db.schema, &doc)?;
        let pk = self.db.primary_key()?;
        let vf = self.db.schema.fields.iter().position(|f| f.name == version).ok_or_else(|| Error::Schema(format!("no field {version}")))?;
        if self.db.schema.fields[vf].ty != FieldType::Int || self.db.schema.fields[vf].multi {
            return Err(Error::Schema(format!("version field {version} must be a single-valued INT")));
        }
        let new = doc[vf].as_i64().ok_or_else(|| Error::Schema(format!("document has no {version}")))?;
        let key = Key::of(&doc[pk]).expect("validated key");
        let old = match (self.touched.get(&key), self.written.get(&key)) {
            (Some(true), Some(d)) => d[vf].as_i64(),
            (Some(false), _) => None,
            _ => match self.sets.get(&(key.clone(), vf)) {
                Some(v) => v.as_i64(),
                None => self.db.live_int(&doc[pk], vf)?,
            },
        };
        if old.is_some_and(|o| o >= new) {
            return Ok(false);
        }
        self.upsert(doc)?;
        Ok(true)
    }

    /// Deletes the live document with this primary key; false if none.
    pub fn delete(&mut self, key: &Value) -> Result<bool> {
        let k = self.db.key_of(key)?;
        if !self.is_live(&k, key) {
            return Ok(false);
        }
        self.touched.insert(k.clone(), false);
        self.written.remove(&k);
        self.ops.push(Op::Delete(key.clone()));
        Ok(true)
    }

    /// Sets a numeric field of the live document with this primary key in
    /// place, without rewriting the document: its text and vectors stay as
    /// they are. The field must be a single-valued INT or FLOAT. False if no
    /// document has the key.
    pub fn set(&mut self, key: &Value, field: &str, value: Value) -> Result<bool> {
        let k = self.db.key_of(key)?;
        let i = self.db.schema.fields.iter().position(|f| f.name == field).ok_or_else(|| Error::Schema(format!("no field {field}")))?;
        let f = &self.db.schema.fields[i];
        let value = match (&f.ty, value) {
            _ if f.multi || Some(i) == self.db.schema.primary_key => {
                return Err(Error::Schema(format!("{field} cannot be set in place: it is multi-valued or the key")))
            }
            (FieldType::Int, v @ (Value::Int(_) | Value::Null)) => v,
            (FieldType::Float, Value::Int(x)) => Value::Float(x as f64),
            (FieldType::Float, v @ (Value::Float(_) | Value::Null)) => v,
            (FieldType::Int | FieldType::Float, v) => return Err(Error::Schema(format!("bad value for {field}: {v:?}"))),
            _ => return Err(Error::Schema(format!("{field} is not a numeric field; only INT and FLOAT are set in place"))),
        };
        if !self.is_live(&k, key) {
            return Ok(false);
        }
        match self.written.get_mut(&k) {
            Some(d) => Arc::make_mut(d)[i] = value.clone(),
            None => {
                self.sets.insert((k, i), value.clone());
            }
        }
        self.ops.push(Op::Set { key: key.clone(), field: i as u16, value });
        Ok(true)
    }

    /// Operations buffered so far.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Logs the transaction (durable on return when `DbOptions::sync`) and
    /// applies it. If the memtable is then full it is flushed; an error from
    /// that flush leaves the transaction committed.
    pub fn commit(self) -> Result<()> {
        let WriteTxn { db, mut wal, ops, .. } = self;
        db.commit_ops(&mut wal, ops)
    }

    pub fn rollback(self) {}

    fn is_live(&self, k: &Key, v: &Value) -> bool {
        match self.touched.get(k) {
            Some(&live) => live,
            None => lock(&self.db.state).find_live(v).is_some(),
        }
    }
}

/// The outcome of a [`VersionWriter`] commit, to be published.
struct Committed {
    meta: Meta,
    slot: usize,
    manifest: Manifest,
    fl: FreeList,
    file_pages: u64,
    bytes_written: u64,
    /// Directory layout: files to unlink once the version is published.
    obsolete: Vec<String>,
}

/// Builds the next on-disk version (shadow paging). Holds working copies
/// of the manifest and freelist; nothing changes in memory until the commit
/// is published, and nothing on disk is reachable until the meta slot is
/// written. Callers hold the writer lock.
struct VersionWriter<'a> {
    db: &'a Database,
    txn: u64,
    manifest: Manifest,
    fl: FreeList,
    meta: Meta,
    slot: usize,
    file_pages: u64,
    bytes_written: u64,
    segments_added: usize,
    /// Directory layout: files the new version stops naming.
    obsolete: Vec<String>,
}

impl<'a> VersionWriter<'a> {
    fn begin(db: &'a Database) -> VersionWriter<'a> {
        let st = lock(&db.state);
        let mut fl = st.freelist.clone();
        // Reusable: freed before the newest on-disk commit (so the older
        // meta slot cannot reach them) and by a commit no live snapshot
        // predates.
        fl.release((st.meta.txn - 1).min(db.pins.min().unwrap_or(u64::MAX)));
        VersionWriter {
            db,
            txn: st.txn + 1,
            manifest: st.manifest.clone(),
            fl,
            meta: st.meta,
            slot: st.slot,
            file_pages: st.file_pages,
            bytes_written: 0,
            segments_added: 0,
            obsolete: Vec::new(),
        }
    }

    /// Writes `bytes` as file `name` in the directory layout, or into a new
    /// extent of the single file.
    fn write_as(&mut self, bytes: &[u8], name: String) -> Result<(Extent, Option<String>)> {
        match &self.db.dir {
            Some(d) => {
                dir::write_file(d, &name, bytes, self.db.opts.sync)?;
                self.bytes_written += bytes.len() as u64;
                Ok((Extent { page: 0, len: bytes.len() as u64 }, Some(name)))
            }
            None => Ok((self.write(bytes)?, None)),
        }
    }

    /// Allocates an extent for `bytes` and writes them.
    fn write(&mut self, bytes: &[u8]) -> Result<Extent> {
        debug_assert!(!bytes.is_empty());
        let page = self.fl.alloc(pages_for(bytes.len()));
        self.grow()?;
        self.db.file.write_all_at(bytes, page * PAGE)?;
        self.bytes_written += bytes.len() as u64;
        Ok(Extent { page, len: bytes.len() as u64 })
    }

    fn grow(&mut self) -> Result<()> {
        if self.fl.page_count() > self.file_pages {
            self.db.file.set_len(self.fl.page_count() * PAGE)?;
            self.file_pages = self.fl.page_count();
        }
        Ok(())
    }

    /// Adds a segment at position `pos` (default: last).
    fn add_segment(&mut self, pos: Option<usize>, image: &[u8], n_docs: u32, deleted: Option<&LiveDocs>, updates: Option<&Updates>) -> Result<()> {
        let id = self.manifest.next_segment_id;
        self.manifest.next_segment_id += 1;
        let (extent, file) = self.write_as(image, dir::segment_name(id))?;
        let (mut deletes, mut deletes_crc, mut n_deleted, mut deletes_file) = (None, 0, 0, None);
        if let Some(l) = deleted.filter(|l| l.n_deleted() > 0) {
            let b = l.encode();
            let (e, f) = self.write_as(&b, dir::deletes_name(id, self.txn))?;
            (deletes, deletes_file) = (Some(e), f);
            deletes_crc = crc32fast::hash(&b);
            n_deleted = l.n_deleted();
        }
        let (mut upd, mut updates_crc, mut updates_file) = (None, 0, None);
        if let Some(u) = updates {
            let b = u.encode();
            let (e, f) = self.write_as(&b, dir::updates_name(id, self.txn))?;
            (upd, updates_file, updates_crc) = (Some(e), f, crc32fast::hash(&b));
        }
        let meta = SegmentMeta {
            id,
            extent,
            crc: crc32fast::hash(image),
            n_docs,
            n_deleted,
            deletes,
            deletes_crc,
            txn: self.txn,
            file,
            deletes_file,
            updates: upd,
            updates_file,
            updates_crc,
        };
        let segs = &mut self.manifest.segments;
        segs.insert(pos.unwrap_or(segs.len()), meta);
        self.segments_added += 1;
        Ok(())
    }

    /// Replaces a segment's delete set with its current one plus `docs`.
    fn add_deletes(&mut self, id: u64, docs: &BTreeSet<DocId>) -> Result<()> {
        let h = lock(&self.db.state).handles.iter().find(|h| h.meta.id == id).cloned().expect("segment of a pending delete");
        let live = LiveDocs::with_deleted(h.base.as_deref(), h.meta.n_docs, docs.iter().copied());
        let b = live.encode();
        let (e, file) = self.write_as(&b, dir::deletes_name(id, self.txn))?;
        let s = self.manifest.segments.iter_mut().find(|s| s.id == id).expect("segment in manifest");
        match (s.deletes, s.deletes_file.take()) {
            (_, Some(old)) => self.obsolete.push(old),
            (Some(old), None) => self.fl.defer(self.txn, old),
            _ => {}
        }
        s.deletes_file = file;
        s.deletes = Some(e);
        s.deletes_crc = crc32fast::hash(&b);
        s.n_deleted = live.n_deleted();
        Ok(())
    }

    /// Replaces a segment's in-place updates with its current ones plus
    /// `newer`.
    fn add_updates(&mut self, id: u64, newer: &Updates) -> Result<()> {
        let h = lock(&self.db.state).handles.iter().find(|h| h.meta.id == id).cloned().expect("segment of a pending update");
        let b = Updates::merged(h.updates.as_deref(), newer).encode();
        let (e, file) = self.write_as(&b, dir::updates_name(id, self.txn))?;
        let s = self.manifest.segments.iter_mut().find(|s| s.id == id).expect("segment in manifest");
        match (s.updates, s.updates_file.take()) {
            (_, Some(old)) => self.obsolete.push(old),
            (Some(old), None) => self.fl.defer(self.txn, old),
            _ => {}
        }
        s.updates_file = file;
        s.updates = Some(e);
        s.updates_crc = crc32fast::hash(&b);
        Ok(())
    }

    /// Copies extents into free runs below them, highest first. Returns how
    /// many moved.
    fn relocate_down(&mut self) -> Result<usize> {
        if self.db.dir.is_some() {
            return Ok(0);
        }
        let map = lock(&self.db.state).map.clone();
        // (extent, segment, what): 0 the image, 1 the delete set, 2 updates.
        let mut extents: Vec<(Extent, usize, u8)> = Vec::new();
        for (i, s) in self.manifest.segments.iter().enumerate() {
            extents.push((s.extent, i, 0));
            extents.extend(s.deletes.map(|e| (e, i, 1)));
            extents.extend(s.updates.map(|e| (e, i, 2)));
        }
        extents.sort_by_key(|&(e, _, _)| std::cmp::Reverse(e.page));
        let mut moved = 0;
        for (e, i, what) in extents {
            let Some(page) = self.fl.alloc_below(e.pages(), e.page) else { continue };
            // Copy out of the mapping first: on macOS, a pwrite whose source
            // is a mapping of the same file can deadlock in the kernel.
            let bytes = map[e.range()].to_vec();
            self.db.file.write_all_at(&bytes, page * PAGE)?;
            self.bytes_written += e.len;
            self.fl.defer(self.txn, e);
            let to = Extent { page, len: e.len };
            let s = &mut self.manifest.segments[i];
            if what == 2 {
                s.updates = Some(to);
            } else if what == 1 {
                s.deletes = Some(to);
            } else {
                s.extent = to;
            }
            moved += 1;
        }
        Ok(moved)
    }

    fn remove_segment(&mut self, id: u64) {
        let i = self.manifest.segments.iter().position(|s| s.id == id).expect("segment in manifest");
        let s = self.manifest.segments.remove(i);
        if self.db.dir.is_some() {
            self.obsolete.extend(s.file);
            self.obsolete.extend(s.deletes_file);
            self.obsolete.extend(s.updates_file);
            return;
        }
        self.fl.defer(self.txn, s.extent);
        if let Some(u) = s.updates {
            self.fl.defer(self.txn, u);
        }
        if let Some(d) = s.deletes {
            self.fl.defer(self.txn, d);
        }
    }

    /// Writes the manifest and freelist, syncs, then writes the other meta
    /// slot and syncs again.
    fn commit(mut self) -> Result<Committed> {
        if let Some(d) = self.db.dir.clone() {
            return self.commit_dir(&d);
        }
        let db = self.db;
        let c = self.txn;
        self.fl.defer(c, self.meta.manifest);
        self.fl.defer(c, self.meta.freelist);
        self.fl.trim_tail();
        self.manifest.txn = c;
        let mbytes = self.manifest.encode();
        let manifest = self.write(&mbytes)?;
        let (freelist, fbytes) = place_freelist(&mut self.fl);
        self.grow()?;
        db.file.write_all_at(&fbytes, freelist.page * PAGE)?;
        self.bytes_written += fbytes.len() as u64;
        if db.opts.sync {
            db.file.sync_data()?;
        }
        let meta = Meta {
            db_id: self.meta.db_id,
            txn: c,
            manifest,
            manifest_crc: crc32fast::hash(&mbytes),
            freelist,
            freelist_crc: crc32fast::hash(&fbytes),
            page_count: self.fl.page_count(),
        };
        let slot = 1 - self.slot;
        // Past this point the on-disk version may already be the new one.
        pager::write_meta(&db.file, slot, &meta).map_err(|e| db.poison(e))?;
        if db.opts.sync {
            db.file.sync_data().map_err(|e| db.poison(e.into()))?;
        }
        // Free pages at the end are reachable from neither slot. A failed
        // truncation only leaves the file longer than needed.
        if self.fl.page_count() < self.file_pages && db.file.set_len(self.fl.page_count() * PAGE).is_ok() {
            self.file_pages = self.fl.page_count();
        }
        Ok(Committed { meta, slot, manifest: self.manifest, fl: self.fl, file_pages: self.file_pages, bytes_written: self.bytes_written, obsolete: Vec::new() })
    }

    /// Directory layout: writes the manifest file, then points `CURRENT` at
    /// it with an atomic rename.
    fn commit_dir(mut self, d: &Path) -> Result<Committed> {
        let db = self.db;
        let c = self.txn;
        let previous = dir::manifest_name(lock(&db.state).manifest.txn);
        self.manifest.txn = c;
        let mbytes = self.manifest.encode();
        let name = dir::manifest_name(c);
        dir::write_file(d, &name, &mbytes, db.opts.sync)?;
        self.bytes_written += mbytes.len() as u64;
        let cur = Current { db_id: self.meta.db_id, txn: c, manifest: name, crc: crc32fast::hash(&mbytes) };
        // Past the rename the on-disk version may already be the new one.
        dir::write_current(d, &cur, db.opts.sync).map_err(|e| db.poison(e))?;
        self.obsolete.push(previous);
        Ok(Committed {
            meta: dir_meta(&cur),
            slot: self.slot,
            manifest: self.manifest,
            fl: self.fl,
            file_pages: 0,
            bytes_written: self.bytes_written,
            obsolete: self.obsolete,
        })
    }
}

/// The meta of a directory-layout version (it has no pages).
fn dir_meta(cur: &Current) -> Meta {
    let none = Extent { page: 0, len: 0 };
    Meta { db_id: cur.db_id, txn: cur.txn, manifest: none, manifest_crc: cur.crc, freelist: none, freelist_crc: 0, page_count: 0 }
}

/// Allocates the freelist's own extent and encodes the freelist after that
/// allocation, padded to the size measured before it (allocating never adds
/// a run, so the encoding only shrinks).
fn place_freelist(fl: &mut FreeList) -> (Extent, Vec<u8>) {
    let bound = fl.encoded_len();
    let page = fl.alloc(pages_for(bound));
    let mut bytes = fl.encode();
    bytes.resize(bound, 0);
    (Extent { page, len: bound as u64 }, bytes)
}

fn pages_for(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(PAGE).max(1)
}

fn wal_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push("-wal");
    PathBuf::from(s)
}

/// One open `Database` per file: an exclusive `flock`, released when the
/// file is closed (also by a crash).
fn lock_exclusive(file: &File) -> Result<()> {
    // SAFETY: flock on a valid descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(Error::Busy("the database is open elsewhere".into()));
        }
        return Err(e.into());
    }
    Ok(())
}
