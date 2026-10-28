//! The database: one file plus a `-wal` sidecar (or a directory of files,
//! [`Layout::Directory`]), one writer, any number of snapshot readers.
//!
//! A database holds one or more tables ([`Database::create_table`]), each
//! with its own schema, segments and memtable. They share the WAL, the
//! writer lock and the commit: one transaction can write several tables,
//! and a snapshot sees every table at the same version. The table a
//! database was created with is its first; the methods that take no table
//! name ([`WriteTxn::insert`], [`Database::bulk_load`], ...) act on it.
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
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::{corrupt, Error, Result};
use crate::schema::{FieldType, Schema};
use crate::segment::build::{build_segment, BuildOptions};
use crate::segment::{DocId, SegmentFile};
use crate::storage::freelist::FreeList;
use crate::storage::dir::{self, Current};
use crate::storage::live::LiveDocs;
use crate::storage::lock;
use crate::storage::manifest::{Manifest, SegmentMeta, TableMeta};
use crate::storage::merge::{MergePolicy, MergeStats, SegmentInfo};
use crate::storage::pager::{self, Extent, Meta, META_PAGES, PAGE};
use crate::postings::bytes::OwnedBytes;
use crate::storage::snapshot::{build_memseg, Pins, Snapshot};
use crate::storage::state::{Key, Loc, SegHandle, State, TableState};
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
    /// The table the segment belongs to.
    pub table: String,
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
    /// The first table's schema (that table cannot be dropped).
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
    /// Why the last flush failed, until one succeeds.
    flush_error: Mutex<Option<String>>,
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
        let manifest = Manifest::new(schema);
        let mut fl = FreeList::new(META_PAGES);
        let mbytes = manifest.encode();
        let mext = Extent { page: fl.alloc(pages_for(mbytes.len())), len: mbytes.len() as u64 };
        let (fext, fbytes) = place_freelist(&mut fl);
        file.set_len(fl.page_count() * PAGE)?;
        file.write_all_at(&mbytes, mext.page * PAGE)?;
        file.write_all_at(&fbytes, fext.page * PAGE)?;
        let meta = Meta {
            format: 1,
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
        let manifest = Manifest::new(schema);
        let mbytes = manifest.encode();
        let name = dir::manifest_name(1);
        dir::write_file(path, &name, &mbytes, true)?;
        dir::write_current(path, &Current { db_id, txn: 1, manifest: name, crc: crc32fast::hash(&mbytes), format: 1 }, true)?;
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
            .tables
            .iter()
            .map(|t| t.segments.iter().map(|m| SegHandle::open_in(path, m, opts.read_ahead, opts.read_ahead && opts.random_hints).map(Arc::new)).collect())
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
        let handles = manifest
            .tables
            .iter()
            .map(|t| t.segments.iter().map(|m| SegHandle::open(&map, m, opts.read_ahead && opts.random_hints).map(Arc::new)).collect())
            .collect::<Result<_>>()?;
        Database::assemble(path, wal_path(path), None, file, opts, manifest, meta, slot, freelist, map, handles)
    }

    /// The open database around an on-disk version (`handles`: each table's
    /// opened segments): replays the WAL into the memtables.
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
        handles: Vec<Vec<Arc<SegHandle>>>,
    ) -> Result<Database> {
        let (mut wal, txns) = Wal::open(&wal_path, meta.db_id, opts.sync)?;
        let tables: Vec<TableState> = manifest
            .tables
            .iter()
            .zip(handles)
            .map(|(t, h)| TableState::new(t.id, Arc::new(t.schema.clone()), h, opts.memtable_chunk_docs, 0))
            .collect();
        let schema = tables[0].schema.clone();
        let mut st = State {
            manifest,
            meta,
            slot,
            freelist,
            file_pages: meta.page_count,
            map,
            tables,
            txn: meta.txn,
            version: 0,
            pending: Default::default(),
            pending_updates: Default::default(),
            n_pending: 0,
            live_cache: HashMap::new(),
            updates_cache: HashMap::new(),
            cached: None,
        };
        let mut flushed = false;
        for t in txns {
            if t.txn <= st.manifest.wal_txn {
                // Flushed before a crash cut the WAL truncation short.
                flushed = true;
                continue;
            }
            for (table, op) in t.ops {
                // A dropped table's writes were flushed before the drop,
                // so none can follow it here; skip them all the same.
                if let Some(i) = st.table_index(table) {
                    st.apply(i, op);
                }
            }
            st.txn = st.txn.max(t.txn);
        }
        if flushed && st.mem_len() == 0 && st.pending.is_empty() {
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
            flush_error: Mutex::new(None),
        };
        db.republish();
        Ok(db)
    }

    /// The schema of the database's first table.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The schemas of the database's tables, in creation order (the first
    /// is the table the database was created with).
    pub fn tables(&self) -> Vec<Arc<Schema>> {
        lock(&self.state).tables.iter().map(|t| t.schema.clone()).collect()
    }

    /// The schema of the table named `name` (any case).
    pub fn table(&self, name: &str) -> Option<Arc<Schema>> {
        lock(&self.state).tables.iter().find(|t| t.schema.table.eq_ignore_ascii_case(name)).map(|t| t.schema.clone())
    }

    /// Adds an empty table with this schema, in one commit. Its name must
    /// be a new one (in any case) and a plain identifier: letters, digits
    /// and underscores, not starting with a digit.
    ///
    /// A database with more than one table is written in format 2, which
    /// versions of Strata before tables cannot open; dropping back to the
    /// first table alone writes format 1 again.
    pub fn create_table(&self, schema: Schema) -> Result<()> {
        let _wal = self.try_writer()?;
        self.check_poisoned()?;
        check_table_name(&schema.table)?;
        if self.table(&schema.table).is_some() {
            return Err(Error::Schema(format!("table {} already exists", schema.table)));
        }
        let mut vw = VersionWriter::begin(self);
        let id = vw.manifest.next_table_id;
        vw.manifest.next_table_id += 1;
        vw.manifest.tables.push(TableMeta { id, schema, segments: Vec::new() });
        let c = vw.commit()?;
        self.publish(c, |_| {})
    }

    /// Drops the table named `name` (any case) and its rows, in one commit.
    /// The first table (the one the database was created with) cannot be
    /// dropped. The memtables are flushed first, so that no logged write
    /// outlives its table. Its pages are freed once no snapshot reads them.
    pub fn drop_table(&self, name: &str) -> Result<()> {
        let mut wal = self.try_writer()?;
        let id = {
            let st = lock(&self.state);
            st.tables.iter().find(|t| t.schema.table.eq_ignore_ascii_case(name)).map(|t| t.id)
        };
        let id = id.ok_or_else(|| Error::Schema(format!("no table {name}")))?;
        if id == 0 {
            return Err(Error::Schema(format!("{name} is the database's first table, which cannot be dropped")));
        }
        self.flush_locked(&mut wal)?;
        if !wal.is_empty() {
            // Only transactions the segments already hold can be left.
            wal.reset(self.opts.sync).map_err(|e| self.poison(e))?;
        }
        let mut vw = VersionWriter::begin(self);
        vw.drop_table(id);
        let c = vw.commit()?;
        self.publish(c, |st| {
            st.live_cache.clear();
            st.updates_cache.clear();
        })?;
        if self.dir.is_none() && lock(&self.state).manifest.format() == 1 {
            // The other meta slot still names a version with several
            // tables, and versions of Strata from before tables refuse a
            // file with such a slot even when it is the older one. Commit
            // again so that both slots hold format 1.
            let c = VersionWriter::begin(self).commit()?;
            self.publish(c, |_| {})?;
        }
        Ok(())
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
        Ok(WriteTxn { db: self, wal, ops: Vec::new(), keys: Vec::new() })
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
                return Snapshot::of(s.clone());
            }
            st.capture(&self.pins, with_mem)
        };
        if cap.tables.iter().any(|t| t.mem.iter().any(|p| p.seg.is_none())) {
            let _b = lock(&self.build_lock);
            let opts = self.opts.mem_build.as_ref().unwrap_or(&self.opts.build);
            for t in &mut cap.tables {
                for part in t.mem.iter_mut().filter(|p| p.seg.is_none()) {
                    // Another reader may have built it while we waited.
                    let built = lock(&self.state).memseg(part.table, part.gen, part.chunk, part.len);
                    part.seg = Some(built.unwrap_or_else(|| {
                        let id = MEM_ID | self.mem_ids.fetch_add(1, Ordering::Relaxed);
                        let seg = build_memseg(&t.schema, opts, part, id);
                        lock(&self.state).keep_memseg(part.table, &seg);
                        seg
                    }));
                }
            }
        }
        let version = cap.version;
        let inner = Arc::new(cap.assemble(self.path.clone()));
        if with_mem {
            let mut st = lock(&self.state);
            if st.version == version {
                st.cached = Some(inner.clone());
            }
        }
        Snapshot::of(inner)
    }

    /// Writes the memtables as segments and pending deletes into delete
    /// sets, all tables in one commit, then empties the WAL.
    pub fn flush(&self) -> Result<()> {
        let mut wal = self.try_writer()?;
        self.flush_locked(&mut wal)
    }

    /// Why the last flush failed, if no flush has succeeded since. Writes
    /// still commit meanwhile (the WAL holds them) until the memtable
    /// reaches twice its limit.
    pub fn flush_error(&self) -> Option<String> {
        lock(&self.flush_error).clone()
    }

    /// Whether a write failed partway (a WAL append, a meta page), after
    /// which every write and merge fails until the database is reopened.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    fn flush_locked(&self, wal: &mut Wal) -> Result<()> {
        let r = self.flush_inner(wal);
        *lock(&self.flush_error) = r.as_ref().err().map(|e| e.to_string());
        r
    }

    fn flush_inner(&self, wal: &mut Wal) -> Result<()> {
        self.check_poisoned()?;
        let (mems, pending, pending_updates, txn) = {
            let st = lock(&self.state);
            if st.mem_len() == 0 && st.pending.is_empty() && st.pending_updates.is_empty() {
                return Ok(());
            }
            let mems: Vec<_> = st
                .tables
                .iter()
                .filter(|t| t.mem.len > 0)
                .map(|t| (t.id, t.schema.clone(), t.mem.chunks.clone(), t.mem.deleted.clone(), t.mem.updates.clone()))
                .collect();
            (mems, st.pending.clone(), st.pending_updates.clone(), st.txn)
        };
        let mut vw = VersionWriter::begin(self);
        vw.manifest.wal_txn = txn;
        for (table, schema, chunks, deleted, mem_updates) in mems {
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
            for chunk in live.chunks(self.opts.segment_max_docs.max(1)) {
                let image = build_segment(&schema, chunk, &self.opts.build)?;
                vw.add_segment(table, None, &image, chunk.len() as u32, None, None)?;
            }
        }
        for (&id, docs) in &pending {
            vw.add_deletes(id, docs)?;
        }
        for (&id, (_, ups)) in &pending_updates {
            vw.add_updates(id, ups)?;
        }
        let c = vw.commit()?;
        self.publish(c, |st| {
            for t in &mut st.tables {
                t.clear_mem();
            }
            st.pending.clear();
            st.pending_updates.clear();
            st.updates_cache.clear();
            st.n_pending = 0;
        })?;
        wal.reset(self.opts.sync).map_err(|e| self.poison(e))
    }

    /// Loads documents into the first table straight into segments of
    /// `segment_max_docs`, in one commit and without the WAL. Keys must be
    /// new (as with `insert`).
    pub fn bulk_load(&self, docs: impl IntoIterator<Item = Document>) -> Result<()> {
        self.try_bulk_load(docs.into_iter().map(Ok))
    }

    /// [`Database::bulk_load`] of documents that may fail to arrive: the
    /// first error ends the load, and nothing is committed.
    pub fn try_bulk_load(&self, docs: impl IntoIterator<Item = Result<Document>>) -> Result<()> {
        self.bulk_load_table(None, docs)
    }

    /// [`Database::bulk_load`] into the table named `table`.
    pub fn bulk_load_into(&self, table: &str, docs: impl IntoIterator<Item = Document>) -> Result<()> {
        self.bulk_load_table(Some(table), docs.into_iter().map(Ok))
    }

    /// [`Database::try_bulk_load`] into the table named `table`.
    pub fn try_bulk_load_into(&self, table: &str, docs: impl IntoIterator<Item = Result<Document>>) -> Result<()> {
        self.bulk_load_table(Some(table), docs)
    }

    fn bulk_load_table(&self, table: Option<&str>, docs: impl IntoIterator<Item = Result<Document>>) -> Result<()> {
        let mut wal = self.try_writer()?;
        let t = self.table_ref(table)?;
        // WAL deletes are replayed against the on-disk version; segments
        // written behind the WAL's back must not hold keys it touches.
        self.flush_locked(&mut wal)?;
        let mut vw = VersionWriter::begin(self);
        let schema = &t.schema;
        let pk = schema.primary_key;
        let check_existing = !lock(&self.state).tables[t.pos].handles.is_empty();
        let mut seen = HashSet::new();
        let mut chunk = Vec::new();
        let max = self.opts.segment_max_docs.max(1);
        for doc in docs {
            let doc = doc?;
            validate(schema, &doc)?;
            if let Some(pk) = pk {
                let dup = !seen.insert(Key::of(&doc[pk]).expect("validated key"))
                    || (check_existing && lock(&self.state).find_live(t.pos, &doc[pk]).is_some());
                if dup {
                    return Err(Error::Schema(format!("duplicate primary key {:?}", doc[pk])));
                }
            }
            chunk.push(doc);
            if chunk.len() == max {
                let image = build_segment(schema, &chunk, &self.opts.build)?;
                vw.add_segment(t.id, None, &image, chunk.len() as u32, None, None)?;
                chunk.clear();
            }
        }
        if !chunk.is_empty() {
            let image = build_segment(schema, &chunk, &self.opts.build)?;
            vw.add_segment(t.id, None, &image, chunk.len() as u32, None, None)?;
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
            let h = st.handle(id).cloned().ok_or_else(|| Error::Schema(format!("no segment {id}")))?;
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
    /// or [`crate::segment::build::build_segment`] with the first table's
    /// schema) to the first table in one commit, after flushing the
    /// memtables. Their primary keys must be new. Returns the new segments'
    /// ids.
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
                    if !seen.insert(Key::of(&k).ok_or_else(|| corrupt("bad key"))?) || st.find_live(0, &k).is_some() {
                        return Err(Error::Schema(format!("{}: duplicate primary key {k:?}", path.display())));
                    }
                }
            }
            loaded.push((bytes, file.n_docs() as u32));
        }
        let mut vw = VersionWriter::begin(self);
        let first = vw.manifest.next_segment_id;
        for (bytes, n) in &loaded {
            vw.add_segment(0, None, bytes, *n, None, None)?;
        }
        let ids: Vec<u64> = (first..vw.manifest.next_segment_id).collect();
        if ids.is_empty() {
            return Ok(ids);
        }
        let c = vw.commit()?;
        self.publish(c, |_| {})?;
        Ok(ids)
    }

    /// Merges each table's on-disk segments as `policy` selects until it
    /// selects nothing. Inputs are rebuilt from `DbOptions::doc_source`
    /// without blocking writers; the commit then marks documents deleted in
    /// the meantime as deleted in the merged segment. Must not be called by
    /// a thread that holds a [`WriteTxn`] (the commit waits for the writer
    /// lock).
    pub fn merge(&self, policy: &MergePolicy) -> Result<MergeStats> {
        let _m = lock(&self.merge_lock);
        let source = self.opts.doc_source.clone().ok_or_else(|| Error::Schema("merging needs DbOptions::doc_source".into()))?;
        // A database that cannot commit would build the merged segment
        // only to throw it away.
        self.check_poisoned()?;
        let t0 = Instant::now();
        let mut stats = MergeStats::default();
        let tables: Vec<(u32, usize)> = lock(&self.state).tables.iter().map(|t| (t.id, t.handles.len())).collect();
        for (table, n) in tables {
            // Each merge removes a segment, except rewrites of a single
            // segment to drop deletes; the bound stops those from repeating
            // forever under a concurrent stream of deletes.
            let mut rounds = 2 * n + 1;
            while rounds > 0 {
                rounds -= 1;
                // The table may have been dropped meanwhile.
                let Some(snap) = self.snapshot_inner(false).table_by_id(table) else { break };
                let infos: Vec<SegmentInfo> =
                    snap.segments().iter().map(|v| SegmentInfo { id: v.id, n_docs: v.file.n_docs() as u32, n_live: v.n_live }).collect();
                let Some(mut group) = policy.select(&infos) else { break };
                // Oldest first, so the merged segment keeps documents in age order.
                group.sort_by_key(|id| infos.iter().position(|s| s.id == *id));
                self.merge_group(table, &snap, &group, source.as_ref(), &mut stats)?;
            }
        }
        stats.seconds = t0.elapsed().as_secs_f64();
        Ok(stats)
    }

    fn merge_group(&self, table: u32, snap: &Snapshot, ids: &[u64], source: &dyn DocumentSource, stats: &mut MergeStats) -> Result<()> {
        let mut inputs: Vec<(u64, Vec<DocId>)> = Vec::new();
        let mut docs: Vec<Document> = Vec::new();
        for &id in ids {
            let v = snap.segments().iter().find(|v| v.id == id).expect("selected from this snapshot");
            let n = v.file.n_docs() as DocId;
            let live: Vec<DocId> = (0..n).filter(|&d| v.live.as_ref().is_none_or(|l| l.contains(d))).collect();
            let mut got = source.documents(snap.schema(), &v.file, &live)?;
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
        let image = if docs.is_empty() { None } else { Some(build_segment(snap.schema(), &docs, &self.opts.build)?) };
        drop(docs);

        let _wal = lock(&self.writer);
        self.check_poisoned()?;
        if lock(&self.state).table_index(table).is_none() {
            // Dropped while the merge was building.
            return Ok(());
        }
        // Documents of the inputs deleted after the snapshot, as docIDs of
        // the merged segment (inputs are concatenated in order), and values
        // written in place after it.
        let mut deleted = Vec::new();
        let mut updated = Updates::default();
        {
            let mut st = lock(&self.state);
            let mut base = 0u32;
            for (id, live) in &inputs {
                let h = st.handle(*id).cloned().ok_or_else(|| corrupt("merge input vanished"))?;
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
        let pos = vw.table_mut(table).segments.iter().position(|s| ids.contains(&s.id)).expect("inputs present");
        for &id in ids {
            vw.remove_segment(id);
        }
        if let Some(image) = image {
            let dels = (!deleted.is_empty()).then(|| LiveDocs::with_deleted(None, n_new, deleted.iter().copied()));
            vw.add_segment(table, Some(pos), &image, n_new, dels.as_ref(), Some(&updated).filter(|u| !u.is_empty()))?;
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
                let cur = Current {
                    db_id: st.meta.db_id,
                    txn: st.meta.txn,
                    manifest: dir::manifest_name(st.manifest.txn),
                    crc: st.meta.manifest_crc,
                    format: st.meta.format,
                };
                dir::referenced(&cur, &st.manifest)
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
                memtable_docs: st.mem_live_docs(),
                pending_deletes: st.n_pending,
                segments: segment_stats(&st.manifest),
            };
        }
        let segments = segment_stats(&st.manifest);
        let seg_pages: u64 =
            st.manifest.segments().map(|s| s.extent.pages() + s.deletes.map_or(0, |e| e.pages()) + s.updates.map_or(0, |e| e.pages())).sum();
        FileStats {
            file_bytes: self.file.metadata().map_or(st.file_pages * PAGE, |m| m.len()),
            page_count: st.meta.page_count,
            live_pages: META_PAGES + st.meta.manifest.pages() + st.meta.freelist.pages() + seg_pages,
            free_pages: st.freelist.free_pages(),
            pending_pages: st.freelist.pending_pages(),
            wal_bytes: std::fs::metadata(&self.wal_path).map_or(0, |m| m.len()),
            memtable_docs: st.mem_live_docs(),
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
            (st.meta, st.manifest.clone(), st.freelist.clone(), st.handles().cloned().collect::<Vec<_>>(), st.map.clone())
        };
        let (meta, _) = pager::read_meta(&self.file)?;
        let manifest = Manifest::decode(&pager::read_extent(&self.file, meta.manifest, meta.manifest_crc)?)?;
        // Compared as written: a format-1 manifest does not hold the next
        // table id, which the open one keeps counting.
        if meta != st_meta || manifest.encode() != st_manifest.encode() {
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
        for (s, h) in manifest.segments().zip(&handles) {
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
            (st.manifest.clone(), st.handles().cloned().collect::<Vec<_>>(), st.meta)
        };
        let (cur, on_disk) = dir::read_current(d)?;
        if on_disk.encode() != manifest.encode() || cur.txn != meta.txn {
            return Err(corrupt("on-disk version differs from the open one"));
        }
        for (s, h) in manifest.segments().zip(&handles) {
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
            (st.handles().cloned().collect::<Vec<_>>(), st.map.clone())
        };
        let grown = self.dir.is_none() && c.meta.page_count * PAGE > map.len() as u64;
        let map = if grown { pager::map_file(&self.file, self.opts.read_ahead).map_err(|e| self.poison(e))? } else { map };
        let hints = self.opts.read_ahead && self.opts.random_hints;
        let handles = c
            .manifest
            .tables
            .iter()
            .map(|t| {
                t.segments
                    .iter()
                    .map(|m| match (old.iter().find(|h| h.meta == *m), &self.dir) {
                        (Some(h), _) => Ok(h.clone()),
                        (None, Some(d)) => SegHandle::open_in(d, m, self.opts.read_ahead, hints).map(Arc::new),
                        (None, None) => SegHandle::open(&map, m, hints).map(Arc::new),
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()
            .map_err(|e| self.poison(e))?;
        let obsolete = c.obsolete;
        {
            let mut st = lock(&self.state);
            // Tables keep their memtables; created ones start empty and
            // dropped ones go.
            let mut old_tables = std::mem::take(&mut st.tables);
            st.tables = c
                .manifest
                .tables
                .iter()
                .zip(handles)
                .map(|(t, h)| match old_tables.iter().position(|o| o.id == t.id) {
                    Some(i) => {
                        let mut o = old_tables.swap_remove(i);
                        o.handles = h;
                        o
                    }
                    None => TableState::new(t.id, Arc::new(t.schema.clone()), h, self.opts.memtable_chunk_docs, 0),
                })
                .collect();
            st.txn = c.meta.txn;
            st.manifest = c.manifest;
            st.meta = c.meta;
            st.slot = c.slot;
            st.freelist = c.fl;
            st.file_pages = c.file_pages;
            st.map = map;
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

    /// The table named `name` (the first table for `None`). Callers hold
    /// the writer lock, so it stays where it is until they release it.
    fn table_ref(&self, name: Option<&str>) -> Result<TableRef> {
        let Some(n) = name else { return Ok(self.first_table()) };
        let st = lock(&self.state);
        let pos = st.tables.iter().position(|t| t.schema.table.eq_ignore_ascii_case(n)).ok_or_else(|| Error::Schema(format!("no table {n}")))?;
        let t = &st.tables[pos];
        Ok(TableRef { id: t.id, pos, schema: t.schema.clone() })
    }

    /// The first table: id 0, always first since it cannot be dropped.
    fn first_table(&self) -> TableRef {
        TableRef { id: 0, pos: 0, schema: self.schema.clone() }
    }

    /// INT field `f` of the live document of table `t` with primary key
    /// `key`: the value written in place if any, else the document's.
    fn live_int(&self, t: &TableRef, key: &Value, f: usize) -> Result<Option<i64>> {
        Ok(self.live_value(t, key, f)?.as_i64())
    }

    /// Numeric field `f` of the live document of table `t` with primary
    /// key `key` (NULL without one): the value written in place if any,
    /// else the document's.
    fn live_value(&self, t: &TableRef, key: &Value, f: usize) -> Result<Value> {
        let st = lock(&self.state);
        let (h, d) = match st.find_live(t.pos, key) {
            None => return Ok(Value::Null),
            Some(Loc::Mem(i)) => {
                let mem = &st.tables[t.pos].mem;
                return Ok(match mem.updates.get(f, i) {
                    Some(v) => v.clone(),
                    None => mem.chunks[i as usize / mem.chunk][i as usize % mem.chunk][f].clone(),
                });
            }
            Some(Loc::Disk(id, d)) => (st.handle(id).cloned().expect("segment of a live key"), d),
        };
        let written = st.pending_updates.get(&h.meta.id).and_then(|(_, u)| u.get(f, d)).or_else(|| h.updates.as_ref().and_then(|u| u.get(f, d)));
        if let Some(v) = written {
            return Ok(v.clone());
        }
        drop(st);
        let field = &t.schema.fields[f];
        Ok(if field.indexed {
            crate::forward::NumericColumn::open(&h.file, &field.name)?.get(d)
        } else {
            crate::forward::DocStore::open(&h.file, &t.schema)?.get_fields(d, &[f]).pop().unwrap_or(Value::Null)
        })
    }

    fn commit_ops(&self, wal: &mut Wal, ops: Vec<(u32, Op)>) -> Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        self.check_poisoned()?;
        // While flushes fail, the memtable grows; past twice its limit,
        // writes wait for a flush to succeed, refused before they are logged.
        let over = {
            let st = lock(&self.state);
            st.mem_len() + st.n_pending > 2 * self.opts.memtable_max_docs.max(1)
        };
        if over && self.flush_error().is_some() {
            self.flush_locked(wal).map_err(|e| Error::Io(std::io::Error::other(format!("writes wait for a flush to succeed: {e}"))))?;
        }
        let txn = lock(&self.state).txn + 1;
        // A failed append is cut off the log, so the transaction is not
        // committed; but the page cache may be in doubt after a failed
        // fsync, so stop writing.
        wal.append(txn, &ops, self.opts.sync).map_err(|e| self.poison(e))?;
        let full = {
            let mut st = lock(&self.state);
            for (table, op) in ops {
                // The writer lock kept every table of the transaction.
                let t = st.table_index(table).expect("table of a logged write");
                st.apply(t, op);
            }
            st.txn = txn;
            st.mem_len() + st.n_pending > self.opts.memtable_max_docs
        };
        // From here the transaction is durable, and its result is Ok: an
        // error would make callers retry a write that already happened (an
        // `add` applied twice). A failed flush is kept in `flush_error`
        // and tried again by the next commit.
        if full && self.flush_locked(wal).is_ok() {
            // The flush published its version.
            return Ok(());
        }
        self.republish();
        Ok(())
    }
}

/// The single write transaction. Operations are buffered and logged at
/// commit; dropping the transaction rolls it back. Its methods write the
/// first table; [`WriteTxn::table`] writes another, in the same
/// transaction.
pub struct WriteTxn<'a> {
    db: &'a Database,
    wal: MutexGuard<'a, Wal>,
    ops: Vec<(u32, Op)>,
    /// What the transaction did to each table it wrote, by table id.
    keys: Vec<(u32, TxnKeys)>,
}

/// What a transaction did to one table's keys.
#[derive(Default)]
struct TxnKeys {
    /// Keys this transaction made live (true) or deleted (false).
    touched: HashMap<Key, bool>,
    /// Documents this transaction wrote, by key, as they now stand.
    written: HashMap<Key, Arc<Document>>,
    /// Fields this transaction set in place on documents it did not write.
    sets: HashMap<(Key, usize), Value>,
}

/// A table resolved under the writer lock, which keeps it from being
/// dropped or moved until the lock is released.
#[derive(Clone)]
struct TableRef {
    id: u32,
    /// Its position in `State::tables`.
    pos: usize,
    schema: Arc<Schema>,
}

impl TableRef {
    fn primary_key(&self) -> Result<usize> {
        self.schema.primary_key.ok_or_else(|| Error::Schema(format!("table {} has no primary key", self.schema.table)))
    }

    /// Checks that `key` has the primary key's type.
    fn key_of(&self, key: &Value) -> Result<Key> {
        let pk = self.primary_key()?;
        match (&self.schema.fields[pk].ty, key) {
            (FieldType::Keyword, Value::Str(_)) | (FieldType::Int, Value::Int(_)) => Ok(Key::of(key).unwrap()),
            _ => Err(Error::Schema(format!("{key:?} is not a value of primary key {}", self.schema.fields[pk].name))),
        }
    }
}

/// Writes to one table of a [`WriteTxn`], from [`WriteTxn::table`].
pub struct TableTxn<'t, 'a> {
    txn: &'t mut WriteTxn<'a>,
    t: TableRef,
}

impl TableTxn<'_, '_> {
    /// [`WriteTxn::insert`] into this table.
    pub fn insert(&mut self, doc: Document) -> Result<()> {
        self.txn.insert_in(&self.t, doc)
    }

    /// [`WriteTxn::upsert`] into this table.
    pub fn upsert(&mut self, doc: Document) -> Result<()> {
        self.txn.upsert_in(&self.t, doc)
    }

    /// [`WriteTxn::upsert_if_newer`] into this table.
    pub fn upsert_if_newer(&mut self, doc: Document, version: &str) -> Result<bool> {
        self.txn.upsert_if_newer_in(&self.t, doc, version)
    }

    /// [`WriteTxn::delete`] from this table.
    pub fn delete(&mut self, key: &Value) -> Result<bool> {
        self.txn.delete_in(&self.t, key)
    }

    /// [`WriteTxn::set`] in this table.
    pub fn set(&mut self, key: &Value, field: &str, value: Value) -> Result<bool> {
        self.txn.set_in(&self.t, key, field, value)
    }

    /// [`WriteTxn::add`] in this table.
    pub fn add(&mut self, key: &Value, field: &str, delta: &Value) -> Result<bool> {
        self.txn.add_in(&self.t, key, field, delta)
    }

    pub fn schema(&self) -> &Schema {
        &self.t.schema
    }
}

impl<'a> WriteTxn<'a> {
    /// Writes to the table named `name` (any case) in this transaction.
    pub fn table(&mut self, name: &str) -> Result<TableTxn<'_, 'a>> {
        let t = self.db.table_ref(Some(name))?;
        Ok(TableTxn { txn: self, t })
    }

    /// Inserts a document; its primary key must not be live.
    pub fn insert(&mut self, doc: Document) -> Result<()> {
        let t = self.db.first_table();
        self.insert_in(&t, doc)
    }

    /// Inserts a document, replacing the live one with the same key.
    pub fn upsert(&mut self, doc: Document) -> Result<()> {
        let t = self.db.first_table();
        self.upsert_in(&t, doc)
    }

    /// Upserts `doc` only if its INT field `version` is greater than the
    /// live document's, or no document has its key: external versions, so
    /// that writers whose updates arrive out of order keep the newest.
    /// Returns whether it was written.
    pub fn upsert_if_newer(&mut self, doc: Document, version: &str) -> Result<bool> {
        let t = self.db.first_table();
        self.upsert_if_newer_in(&t, doc, version)
    }

    /// Deletes the live document with this primary key; false if none.
    pub fn delete(&mut self, key: &Value) -> Result<bool> {
        let t = self.db.first_table();
        self.delete_in(&t, key)
    }

    /// Sets a numeric field of the live document with this primary key in
    /// place, without rewriting the document: its text and vectors stay as
    /// they are. The field must be a single-valued INT or FLOAT. False if no
    /// document has the key.
    pub fn set(&mut self, key: &Value, field: &str, value: Value) -> Result<bool> {
        let t = self.db.first_table();
        self.set_in(&t, key, field, value)
    }

    /// Adds `delta` to a numeric field of the live document with this
    /// primary key, in place as [`WriteTxn::set`] does. The base is the
    /// value as this transaction sees it, its own writes included (NULL
    /// counts as 0). An INT field takes an integer and refuses a sum that
    /// overflows; a FLOAT sum must be finite. False if no document has the
    /// key.
    pub fn add(&mut self, key: &Value, field: &str, delta: &Value) -> Result<bool> {
        let t = self.db.first_table();
        self.add_in(&t, key, field, delta)
    }

    fn insert_in(&mut self, t: &TableRef, doc: Document) -> Result<()> {
        validate(&t.schema, &doc)?;
        if let Some(pk) = t.schema.primary_key {
            let key = Key::of(&doc[pk]).expect("validated key");
            if self.is_live(t, &key, &doc[pk]) {
                return Err(Error::Schema(format!("duplicate primary key {:?}", doc[pk])));
            }
            let doc = Arc::new(doc);
            let k = self.keys(t.id);
            k.touched.insert(key.clone(), true);
            k.written.insert(key, doc.clone());
            self.ops.push((t.id, Op::Insert(doc)));
            return Ok(());
        }
        self.ops.push((t.id, Op::Insert(Arc::new(doc))));
        Ok(())
    }

    fn upsert_in(&mut self, t: &TableRef, doc: Document) -> Result<()> {
        validate(&t.schema, &doc)?;
        let pk = t.primary_key()?;
        self.delete_in(t, &doc[pk].clone())?;
        let key = Key::of(&doc[pk]).expect("validated key");
        let doc = Arc::new(doc);
        let k = self.keys(t.id);
        k.touched.insert(key.clone(), true);
        k.written.insert(key, doc.clone());
        self.ops.push((t.id, Op::Insert(doc)));
        Ok(())
    }

    fn upsert_if_newer_in(&mut self, t: &TableRef, doc: Document, version: &str) -> Result<bool> {
        validate(&t.schema, &doc)?;
        let pk = t.primary_key()?;
        let vf = t.schema.fields.iter().position(|f| f.name == version).ok_or_else(|| Error::Schema(format!("no field {version}")))?;
        if t.schema.fields[vf].ty != FieldType::Int || t.schema.fields[vf].multi {
            return Err(Error::Schema(format!("version field {version} must be a single-valued INT")));
        }
        let new = doc[vf].as_i64().ok_or_else(|| Error::Schema(format!("document has no {version}")))?;
        let key = Key::of(&doc[pk]).expect("validated key");
        let k = self.keys(t.id);
        let old = match (k.touched.get(&key), k.written.get(&key)) {
            (Some(true), Some(d)) => d[vf].as_i64(),
            (Some(false), _) => None,
            _ => match k.sets.get(&(key.clone(), vf)) {
                Some(v) => v.as_i64(),
                None => self.db.live_int(t, &doc[pk], vf)?,
            },
        };
        if old.is_some_and(|o| o >= new) {
            return Ok(false);
        }
        self.upsert_in(t, doc)?;
        Ok(true)
    }

    fn delete_in(&mut self, t: &TableRef, key: &Value) -> Result<bool> {
        let k = t.key_of(key)?;
        if !self.is_live(t, &k, key) {
            return Ok(false);
        }
        let keys = self.keys(t.id);
        keys.touched.insert(k.clone(), false);
        keys.written.remove(&k);
        self.ops.push((t.id, Op::Delete(key.clone())));
        Ok(true)
    }

    fn set_in(&mut self, t: &TableRef, key: &Value, field: &str, value: Value) -> Result<bool> {
        let k = t.key_of(key)?;
        let i = t.schema.fields.iter().position(|f| f.name == field).ok_or_else(|| Error::Schema(format!("no field {field}")))?;
        let f = &t.schema.fields[i];
        let value = match (&f.ty, value) {
            _ if f.multi || Some(i) == t.schema.primary_key => {
                return Err(Error::Schema(format!("{field} cannot be set in place: it is multi-valued or the key")))
            }
            (FieldType::Int, v @ (Value::Int(_) | Value::Null)) => v,
            (FieldType::Float, Value::Int(x)) => Value::Float(x as f64),
            (FieldType::Float, Value::Float(x)) if !x.is_finite() => return Err(Error::Schema(format!("bad value for {field}: {x} (FLOAT values are finite)"))),
            (FieldType::Float, v @ (Value::Float(_) | Value::Null)) => v,
            (FieldType::Int | FieldType::Float, v) => return Err(Error::Schema(format!("bad value for {field}: {v:?}"))),
            _ => return Err(Error::Schema(format!("{field} is not a numeric field; only INT and FLOAT are set in place"))),
        };
        if !self.is_live(t, &k, key) {
            return Ok(false);
        }
        let keys = self.keys(t.id);
        match keys.written.get_mut(&k) {
            Some(d) => Arc::make_mut(d)[i] = value.clone(),
            None => {
                keys.sets.insert((k, i), value.clone());
            }
        }
        self.ops.push((t.id, Op::Set { key: key.clone(), field: i as u16, value }));
        Ok(true)
    }

    fn add_in(&mut self, t: &TableRef, key: &Value, field: &str, delta: &Value) -> Result<bool> {
        let k = t.key_of(key)?;
        let i = t.schema.fields.iter().position(|f| f.name == field).ok_or_else(|| Error::Schema(format!("no field {field}")))?;
        if !self.is_live(t, &k, key) {
            return Ok(false);
        }
        let keys = self.keys(t.id);
        let base = match (keys.touched.get(&k), keys.written.get(&k)) {
            (Some(true), Some(d)) => d[i].clone(),
            _ => match keys.sets.get(&(k.clone(), i)) {
                Some(v) => v.clone(),
                None => self.db.live_value(t, key, i)?,
            },
        };
        let overflow = || Error::Schema(format!("{field} + {delta:?} overflows"));
        let sum = match (&t.schema.fields[i].ty, delta) {
            (FieldType::Int, Value::Int(x)) => Value::Int(base.as_i64().unwrap_or(0).checked_add(*x).ok_or_else(overflow)?),
            (FieldType::Int, _) => return Err(Error::Schema(format!("{field} is INT: add an integer"))),
            (FieldType::Float, d @ (Value::Int(_) | Value::Float(_))) => {
                let y = base.as_f64().unwrap_or(0.0) + d.as_f64().unwrap_or(0.0);
                if !y.is_finite() {
                    return Err(overflow());
                }
                Value::Float(y)
            }
            (FieldType::Float, d) => return Err(Error::Schema(format!("cannot add {d:?} to {field}"))),
            _ => return Err(Error::Schema(format!("{field} is not a numeric field"))),
        };
        self.set_in(t, key, field, sum)
    }

    /// Operations buffered so far.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Logs the transaction (durable on return when `DbOptions::sync`) and
    /// applies it. `Ok` means it committed and `Err` that it did not, so a
    /// failed commit can be retried. If the memtables are then full they
    /// are flushed; a failed flush does not fail the commit, and shows in
    /// [`Database::flush_error`].
    pub fn commit(self) -> Result<()> {
        let WriteTxn { db, mut wal, ops, .. } = self;
        db.commit_ops(&mut wal, ops)
    }

    pub fn rollback(self) {}

    fn keys(&mut self, table: u32) -> &mut TxnKeys {
        let i = match self.keys.iter().position(|(id, _)| *id == table) {
            Some(i) => i,
            None => {
                self.keys.push((table, TxnKeys::default()));
                self.keys.len() - 1
            }
        };
        &mut self.keys[i].1
    }

    fn is_live(&self, t: &TableRef, k: &Key, v: &Value) -> bool {
        match self.keys.iter().find(|(id, _)| *id == t.id).and_then(|(_, keys)| keys.touched.get(k)) {
            Some(&live) => live,
            None => lock(&self.db.state).find_live(t.pos, v).is_some(),
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

    /// Adds a segment to table `table` at position `pos` (default: last).
    #[allow(clippy::too_many_arguments)]
    fn add_segment(
        &mut self,
        table: u32,
        pos: Option<usize>,
        image: &[u8],
        n_docs: u32,
        deleted: Option<&LiveDocs>,
        updates: Option<&Updates>,
    ) -> Result<()> {
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
        let segs = &mut self.table_mut(table).segments;
        segs.insert(pos.unwrap_or(segs.len()), meta);
        self.segments_added += 1;
        Ok(())
    }

    fn table_mut(&mut self, id: u32) -> &mut TableMeta {
        self.manifest.table_mut(id).expect("table in manifest")
    }

    /// Replaces a segment's delete set with its current one plus `docs`.
    fn add_deletes(&mut self, id: u64, docs: &BTreeSet<DocId>) -> Result<()> {
        let h = lock(&self.db.state).handle(id).cloned().expect("segment of a pending delete");
        let live = LiveDocs::with_deleted(h.base.as_deref(), h.meta.n_docs, docs.iter().copied());
        let b = live.encode();
        let (e, file) = self.write_as(&b, dir::deletes_name(id, self.txn))?;
        let s = self.manifest.segment_mut(id).expect("segment in manifest");
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
        let h = lock(&self.db.state).handle(id).cloned().expect("segment of a pending update");
        let b = Updates::merged(h.updates.as_deref(), newer).encode();
        let (e, file) = self.write_as(&b, dir::updates_name(id, self.txn))?;
        let s = self.manifest.segment_mut(id).expect("segment in manifest");
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
        // (extent, (table, segment), what): 0 the image, 1 the delete set,
        // 2 updates.
        let mut extents: Vec<(Extent, (usize, usize), u8)> = Vec::new();
        for (t, table) in self.manifest.tables.iter().enumerate() {
            for (i, s) in table.segments.iter().enumerate() {
                extents.push((s.extent, (t, i), 0));
                extents.extend(s.deletes.map(|e| (e, (t, i), 1)));
                extents.extend(s.updates.map(|e| (e, (t, i), 2)));
            }
        }
        extents.sort_by_key(|&(e, _, _)| std::cmp::Reverse(e.page));
        let mut moved = 0;
        for (e, (t, i), what) in extents {
            let Some(page) = self.fl.alloc_below(e.pages(), e.page) else { continue };
            // Copy out of the mapping first: on macOS, a pwrite whose source
            // is a mapping of the same file can deadlock in the kernel.
            let bytes = map[e.range()].to_vec();
            self.db.file.write_all_at(&bytes, page * PAGE)?;
            self.bytes_written += e.len;
            self.fl.defer(self.txn, e);
            let to = Extent { page, len: e.len };
            let s = &mut self.manifest.tables[t].segments[i];
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
        let (t, i) = self
            .manifest
            .tables
            .iter()
            .enumerate()
            .find_map(|(t, table)| Some((t, table.segments.iter().position(|s| s.id == id)?)))
            .expect("segment in manifest");
        let s = self.manifest.tables[t].segments.remove(i);
        self.free_segment(s);
    }

    /// Drops table `id` and frees its segments.
    fn drop_table(&mut self, id: u32) {
        let i = self.manifest.tables.iter().position(|t| t.id == id).expect("table in manifest");
        for s in self.manifest.tables.remove(i).segments {
            self.free_segment(s);
        }
    }

    /// Frees the pages (or names the files) of a segment the new version
    /// no longer holds.
    fn free_segment(&mut self, s: SegmentMeta) {
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
            format: self.manifest.format(),
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
        let cur = Current { db_id: self.meta.db_id, txn: c, manifest: name, crc: crc32fast::hash(&mbytes), format: self.manifest.format() };
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
    Meta { format: cur.format, db_id: cur.db_id, txn: cur.txn, manifest: none, manifest_crc: cur.crc, freelist: none, freelist_crc: 0, page_count: 0 }
}

/// Every segment's size, by table.
fn segment_stats(m: &Manifest) -> Vec<SegmentStats> {
    m.tables
        .iter()
        .flat_map(|t| {
            t.segments.iter().map(|s| SegmentStats {
                table: t.schema.table.clone(),
                id: s.id,
                n_docs: s.n_docs,
                n_deleted: s.n_deleted,
                bytes: s.extent.len,
                delete_set_bytes: s.deletes.map_or(0, |e| e.len),
            })
        })
        .collect()
}

/// Table names are plain identifiers, so SQL can name them unquoted.
fn check_table_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(Error::Schema(format!("bad table name {name:?}: use letters, digits and underscores, not starting with a digit")))
    }
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

/// How long an open waits for a lock that is held.
pub const LOCK_WAIT: Duration = Duration::from_millis(500);

/// One open `Database` per file: an exclusive `flock`, released when the
/// file is closed (also by a crash). A child process holds a copy of every
/// descriptor of its parent from its fork until its exec closes them, so a
/// database that a process closes and opens again while it starts a child
/// can find its own lock still held for a moment: a held lock is retried
/// for [`LOCK_WAIT`] before the open fails.
fn lock_exclusive(file: &File) -> Result<()> {
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        // SAFETY: flock on a valid descriptor.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(e.into());
        }
        if Instant::now() >= deadline {
            return Err(Error::Busy("the database is open elsewhere".into()));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
