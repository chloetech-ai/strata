//! Cold segments: segment images kept on object storage (S3 and stores
//! that speak its API, or a directory) instead of in the database's
//! directory, and read through a local cache (directory layout only).
//!
//! A segment goes cold ([`crate::storage::Database::offload`]) in three
//! steps: its image and a table of the crc32 of each of its 256 KiB parts
//! ([`ChunkTable`]) are written to the store, under keys only this
//! database uses (`<db id>/seg-<id>.strata` and `.crc`); its header and
//! keys (the `pk` section) are copied into a cache file; then a commit
//! marks the segment cold in the manifest ([`ColdRef`], feature
//! `cold_segments`) and its local file goes, as a merged-away segment's
//! does. Nothing changes in the segment: the same bytes, the same id.
//!
//! A cold segment opens over its cache file, a sparse file of the image's
//! length mapped as a local image is. The header and keys are always
//! there, so opening the segment, writes (which look up keys) and reads by
//! key never wait on the store, and never fail because it is down. Every
//! other part is fetched the first time a query reads a section that
//! holds it (a whole section, as [`crate::segment::SegmentFile::section`]
//! hands out; the doc store a block at a time), checked against its crc32
//! and written into the cache file, which the mapping then shows. A
//! section is handed out only once all of it has arrived, so a query
//! reads fetched bytes or fails; it never reads a hole. The cache file
//! survives the process: the parts already in it are found by their
//! checksums when next read, and a damaged one is fetched again.
//!
//! The cache keeps at most [`ColdOptions::cache_bytes`] of fetched parts
//! besides the headers and keys; past it, [`crate::storage::Database::evict_cold`]
//! drops the least recently read segments' parts. It cannot punch holes
//! under a query that is reading them, so it opens the segment again over
//! a new cache file holding only its header and keys, swaps it in, and
//! unlinks the old file: snapshots taken before keep reading the old one,
//! whose space returns when the last of them ends.
//!
//! Cold segments are not merged (their rows would have to be fetched):
//! they are the large ones a tiered merge policy has stopped merging
//! anyway. One whose rows are all deleted or past the table's retention is
//! dropped as any other is. A segment the database no longer names has its
//! objects deleted once no snapshot reads it; uploads and deletes are
//! recorded first in `PENDING` in the cache directory, so that objects a
//! crash left behind are deleted when the database next opens. A
//! checkpoint fetches its cold segments whole: the copy holds them as
//! local files and shares nothing with the source.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{corrupt, Error, Result};
use crate::segment::Fetch;
use crate::storage::io::sync_dir;
use crate::storage::lock;
use crate::storage::manifest::{ColdRef, Manifest};

/// The size of the parts a cold image is fetched and checked in.
pub const CHUNK: u32 = 256 << 10;
/// The most one request fetches: a run of parts that are next to each
/// other, read at once.
const RUN: u64 = 8 << 20;
/// Requests at once while one section is fetched.
const PARALLEL: usize = 8;

/// Where cold segments are kept: S3 or a store that speaks its API
/// (feature `s3`, [`crate::objects::Bucket`]), or a directory
/// ([`DirStore`]). Keys are relative to the store's own prefix.
pub trait ObjectStore: Send + Sync + std::fmt::Debug {
    /// Writes the file at `path` as object `key`, replacing one there.
    fn put_file(&self, key: &str, path: &Path) -> Result<()>;
    /// Writes `bytes` as object `key`, replacing one there.
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()>;
    /// Bytes `range` of object `key`; an error if it is missing or shorter.
    fn get_range(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>>;
    /// Deletes object `key`; deleting one that is not there succeeds.
    fn delete(&self, key: &str) -> Result<()>;
    /// Where the store is, for messages (`s3://bucket/prefix/`, a path).
    fn describe(&self) -> String;

    /// The whole of object `key` and its version; `None` when it is
    /// missing. A version names one write of the object: no two writes of
    /// different bytes under one key have the same version.
    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        Err(unconditional(self.describe(), key))
    }
    /// Writes `bytes` as object `key` only if no object has that key, and
    /// answers its version; `None`, having written nothing, when one does.
    /// Of several callers at once, at most one is answered a version.
    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<Option<String>> {
        let _ = bytes;
        Err(unconditional(self.describe(), key))
    }
    /// Replaces object `key` with `bytes` only if it is still at
    /// `version`, and answers its new version; `None`, having written
    /// nothing, when the object changed since or is gone. Of several
    /// callers at once holding one version, at most one is answered.
    fn put_if_version(&self, key: &str, bytes: &[u8], version: &str) -> Result<Option<String>> {
        let _ = (bytes, version);
        Err(unconditional(self.describe(), key))
    }
    /// The keys that start with `prefix`, in no order. A store may answer
    /// from a moment ago: a key just written can be missing.
    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        Err(unconditional(self.describe(), prefix))
    }
    /// The names directly under `prefix` (empty, or ending in `/`) that
    /// keys continue past, as directories of a file system, in order. A
    /// store may name one that no key is under any more.
    fn list_dirs(&self, prefix: &str) -> Result<Vec<String>> {
        let mut names: Vec<String> = self.list(prefix)?.iter().filter_map(|k| k.get(prefix.len()..)?.split_once('/').map(|(d, _)| d.to_string())).collect();
        names.sort();
        names.dedup();
        Ok(names)
    }
    /// Writes the whole of object `key` to the file at `dest`, replacing
    /// one there; an error if the object is missing.
    fn get_file(&self, key: &str, dest: &Path) -> Result<()> {
        let (bytes, _) = self.get(key)?.ok_or_else(|| Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("{}: object {key} is missing", self.describe()))))?;
        Ok(std::fs::write(dest, bytes)?)
    }
}

/// The error of a store that has no conditional writes, which a database
/// kept in an object store commits by ([`crate::tenant`]).
fn unconditional(store: String, key: &str) -> Error {
    Error::Schema(format!("{store}: this object store has no conditional writes or listing ({key})"))
}

/// The store `url` names: an `s3://bucket/prefix` URL (feature `s3`), or a
/// directory, created if missing.
pub fn open_store(url: &str) -> Result<Arc<dyn ObjectStore>> {
    #[cfg(feature = "s3")]
    if crate::objects::is_url(url) {
        return Ok(Arc::new(crate::objects::Bucket::open(url)?));
    }
    #[cfg(not(feature = "s3"))]
    if url.to_ascii_lowercase().starts_with("s3") && url.contains("://") {
        return Err(Error::Schema(format!("{url}: this build of ChloeDB has no S3 support (feature s3)")));
    }
    Ok(Arc::new(DirStore::new(url)?))
}

/// A directory as an object store: object `a/b` is file `<root>/a/b`. For
/// tests, and for a file system mounted from elsewhere (NFS, EFS, a disk
/// of another class).
#[derive(Debug)]
pub struct DirStore {
    root: PathBuf,
}

impl DirStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<DirStore> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(DirStore { root })
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        let p = Path::new(key);
        if key.is_empty() || !p.components().all(|c| matches!(c, std::path::Component::Normal(_))) {
            return Err(Error::Schema(format!("object key {key:?} is not a relative path")));
        }
        Ok(self.root.join(p))
    }

    /// Makes `dir` and any of its parents under the root that are missing,
    /// syncing the directory each new one was made in, so that a directory
    /// an object is renamed into survives a power failure too.
    fn make_dirs(&self, dir: &Path) -> std::io::Result<()> {
        if dir.is_dir() {
            return Ok(());
        }
        let mut missing = Vec::new();
        let mut d = dir;
        while !d.is_dir() && d != self.root {
            missing.push(d);
            match d.parent() {
                Some(p) => d = p,
                None => break,
            }
        }
        for m in missing.into_iter().rev() {
            match std::fs::create_dir(m) {
                Ok(()) => {}
                // Another writer made it first: it syncs its own parent.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
            if let Some(parent) = m.parent() {
                sync_dir(parent)?;
            }
        }
        Ok(())
    }

    /// Writes an object under a temporary name, syncs it, renames it into
    /// place and syncs the directory, so that once this returns the object
    /// survives a power failure (tenants rely on the store for that: their
    /// local file is not synced).
    fn write_with(&self, key: &str, write: impl FnOnce(&Path) -> std::io::Result<()>) -> Result<()> {
        let p = self.path(key)?;
        let parent = p.parent().unwrap_or(&self.root).to_path_buf();
        self.make_dirs(&parent)?;
        static N: AtomicU64 = AtomicU64::new(0);
        let tmp = p.with_extension(format!("tmp-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let done = write(&tmp).and_then(|()| File::open(&tmp)?.sync_all()).and_then(|()| std::fs::rename(&tmp, &p));
        if done.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        done?;
        sync_dir(&parent)?;
        Ok(())
    }
}

impl ObjectStore for DirStore {
    fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        self.write_with(key, |tmp| std::fs::copy(path, tmp).map(|_| ()))
    }

    fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        self.write_with(key, |tmp| std::fs::write(tmp, bytes))
    }

    fn get_range(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>> {
        let f = File::open(self.path(key)?).map_err(|e| missing(self, key, e))?;
        let mut buf = vec![0u8; (range.end - range.start) as usize];
        f.read_exact_at(&mut buf, range.start).map_err(|e| missing(self, key, e))?;
        Ok(buf)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let path = self.path(key)?;
        // Under the key's lock, as its conditional writes are: one of
        // them that found the object as it expected does not put it back
        // after the delete.
        let _held = self.lock_key(key)?;
        let done = match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        };
        // The lock's file goes with the object, while the lock is held:
        // who waits for it finds that it is no longer the key's
        // ([`DirStore::lock_key`]).
        let _ = std::fs::remove_file(self.lock_path(key));
        done
    }

    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        match std::fs::read(self.path(key)?) {
            Ok(b) => {
                let v = version_of(&b);
                Ok(Some((b, v)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            // A key under a file (`a/b` where `a` is an object) names none.
            Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<Option<String>> {
        let _held = self.lock_key(key)?;
        if self.path(key)?.exists() {
            return Ok(None);
        }
        self.put(key, bytes)?;
        Ok(Some(version_of(bytes)))
    }

    fn put_if_version(&self, key: &str, bytes: &[u8], version: &str) -> Result<Option<String>> {
        let _held = self.lock_key(key)?;
        match self.get(key)? {
            Some((_, v)) if v == version => {}
            _ => return Ok(None),
        }
        self.put(key, bytes)?;
        Ok(Some(version_of(bytes)))
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        // Start from the deepest directory the prefix names.
        let dir = match prefix.rfind('/') {
            Some(i) => self.path(&prefix[..i])?,
            None => self.root.clone(),
        };
        let mut out = Vec::new();
        let mut todo = vec![dir];
        while let Some(d) = todo.pop() {
            let entries = match std::fs::read_dir(&d) {
                Ok(e) => e,
                Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => continue,
                Err(e) => return Err(e.into()),
            };
            for entry in entries {
                let entry = entry?;
                let p = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // The locks' directory, and objects still being written.
                if name == LOCKS || name.contains(".tmp-") {
                    continue;
                }
                if entry.file_type()?.is_dir() {
                    todo.push(p);
                } else if let Some(key) = p.strip_prefix(&self.root).ok().and_then(|k| k.to_str()) {
                    if key.starts_with(prefix) {
                        out.push(key.to_string());
                    }
                }
            }
        }
        Ok(out)
    }

    fn get_file(&self, key: &str, dest: &Path) -> Result<()> {
        std::fs::copy(self.path(key)?, dest).map_err(|e| missing(self, key, e))?;
        Ok(())
    }

    fn list_dirs(&self, prefix: &str) -> Result<Vec<String>> {
        let dir = match prefix.trim_end_matches('/') {
            "" => self.root.clone(),
            p => self.path(p)?,
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != LOCKS && entry.file_type()?.is_dir() {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }
}

/// Where a [`DirStore`] keeps the locks of its conditional writes.
const LOCKS: &str = ".locks";

/// The version a [`DirStore`] gives an object: a hash of its bytes, so the
/// same bytes written again have the version they had (as S3's entity tag
/// of a small object does).
fn version_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)[..16].iter().map(|b| format!("{b:02x}")).collect()
}

impl DirStore {
    fn lock_path(&self, key: &str) -> PathBuf {
        self.root.join(LOCKS).join(version_of(key.as_bytes()))
    }

    /// Holds key `key`'s lock until the file returned is dropped: an
    /// exclusive `flock` on a file of its own, so that conditional writes
    /// of one key and its deletes exclude each other, between processes
    /// too. A delete removes the lock's file while it holds the lock, so
    /// the lock taken is the key's only when its file is still the one at
    /// its path: otherwise this takes the lock of the file that is.
    fn lock_key(&self, key: &str) -> Result<File> {
        use std::os::unix::fs::MetadataExt;
        self.path(key)?;
        let path = self.lock_path(key);
        std::fs::create_dir_all(self.root.join(LOCKS))?;
        loop {
            let f = OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
            // SAFETY: `f` is an open file, whose descriptor the call only locks.
            if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&f), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mine = f.metadata()?;
            match std::fs::metadata(&path) {
                Ok(now) if now.ino() == mine.ino() && now.dev() == mine.dev() => return Ok(f),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

fn missing(store: &dyn ObjectStore, key: &str, e: std::io::Error) -> Error {
    Error::Io(std::io::Error::new(e.kind(), format!("{}: object {key}: {e}", store.describe())))
}

/// The crc32 of each part of a cold image, kept beside it as `<key>.crc`
/// and in the cache directory: `STRCOLD1`, the part size (u32), the
/// image's length (u64), the number of parts (u32), their crc32s, all
/// little-endian. The manifest keeps the table's own crc32.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkTable {
    pub chunk: u32,
    pub len: u64,
    pub crcs: Vec<u32>,
}

const TABLE_MAGIC: &[u8; 8] = b"STRCOLD1";

impl ChunkTable {
    /// The table of `image` in parts of `chunk` bytes.
    pub fn of(image: &[u8], chunk: u32) -> ChunkTable {
        ChunkTable { chunk, len: image.len() as u64, crcs: image.chunks(chunk as usize).map(crc32fast::hash).collect() }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(24 + 4 * self.crcs.len());
        b.extend_from_slice(TABLE_MAGIC);
        b.extend_from_slice(&self.chunk.to_le_bytes());
        b.extend_from_slice(&self.len.to_le_bytes());
        b.extend_from_slice(&(self.crcs.len() as u32).to_le_bytes());
        for c in &self.crcs {
            b.extend_from_slice(&c.to_le_bytes());
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<ChunkTable> {
        if b.len() < 24 || &b[..8] != TABLE_MAGIC {
            return Err(corrupt("cold segment part table: bad header"));
        }
        let chunk = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let len = u64::from_le_bytes(b[12..20].try_into().unwrap());
        let n = u32::from_le_bytes(b[20..24].try_into().unwrap()) as usize;
        if chunk == 0 || b.len() != 24 + 4 * n || len.div_ceil(chunk as u64) != n as u64 {
            return Err(corrupt("cold segment part table: sizes disagree"));
        }
        Ok(ChunkTable { chunk, len, crcs: b[24..].chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect() })
    }

    /// Bytes of part `c` within the image.
    pub fn range(&self, c: usize) -> Range<u64> {
        let start = c as u64 * self.chunk as u64;
        start..(start + self.chunk as u64).min(self.len)
    }

    /// The parts that bytes `r` of the image touch.
    fn parts(&self, r: &Range<usize>) -> Range<usize> {
        let chunk = self.chunk as usize;
        r.start / chunk..(r.end - 1) / chunk + 1
    }
}

/// How a database keeps segments cold ([`crate::storage::DbOptions::cold`]).
#[derive(Clone, Debug)]
pub struct ColdOptions {
    /// Where the images go.
    pub store: Arc<dyn ObjectStore>,
    /// The local cache: by default `cold` in the database's directory.
    pub cache_dir: Option<PathBuf>,
    /// Bytes of fetched parts the cache keeps, besides the segments' headers
    /// and keys, which it always keeps ([`crate::storage::Database::evict_cold`]).
    pub cache_bytes: u64,
    /// What [`crate::storage::Database::offload`] leaves local: the newest
    /// segments up to this many bytes of images; the older ones it makes
    /// cold, oldest first. `None`: it makes none cold
    /// ([`crate::storage::Database::offload_segment`] still does).
    pub keep_local_bytes: Option<u64>,
    /// Segments smaller than this stay local, as merges are likely to join
    /// them.
    pub min_segment_bytes: u64,
}

impl ColdOptions {
    /// Cold segments in `store`, with a cache of 8 GiB, offloading by hand.
    pub fn new(store: Arc<dyn ObjectStore>) -> ColdOptions {
        ColdOptions { store, cache_dir: None, cache_bytes: 8 << 30, keep_local_bytes: None, min_segment_bytes: 16 << 20 }
    }
}

/// What the cold tier holds and has done since the database opened.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ColdStats {
    /// Where the images are.
    pub store: String,
    pub segments: usize,
    /// Bytes of the cold segments' images.
    pub bytes: u64,
    /// Bytes of them in the cache: the headers and keys, and fetched parts.
    pub cached_bytes: u64,
    /// Of `cached_bytes`, the headers and keys.
    pub pinned_bytes: u64,
    /// [`ColdOptions::cache_bytes`].
    pub cache_limit: u64,
    /// Requests to the store for parts, and the bytes they brought.
    pub fetches: u64,
    pub fetched_bytes: u64,
    /// Parts found in the cache file from before the database opened.
    pub found_parts: u64,
    /// Segments whose fetched parts the cache dropped.
    pub evictions: u64,
    pub segments_offloaded: u64,
    pub objects_deleted: u64,
}

/// Uploads begun and deletes owed, kept in `PENDING` so that a crash
/// leaves no object behind for good ([`Cold::recover`]).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Pending {
    #[serde(default)]
    uploading: Vec<String>,
    #[serde(default)]
    deleting: Vec<String>,
}

/// An open database's cold tier.
#[derive(Debug)]
pub(crate) struct Cold {
    pub opts: ColdOptions,
    /// This database's keys start with it: its id, then `/`.
    pub prefix: String,
    pub dir: PathBuf,
    /// Fetched parts held by the cache files segments read now, headers and
    /// keys apart.
    used: AtomicU64,
    /// The next cache file's generation.
    gen: AtomicU64,
    /// Stamps the last read of each image, for eviction.
    clock: AtomicU64,
    objects: Mutex<HashMap<u64, Weak<ColdObject>>>,
    /// Each cold segment's current image.
    images: Mutex<HashMap<u64, Weak<ColdImage>>>,
    pending: Mutex<Pending>,
    fetches: AtomicU64,
    fetched_bytes: AtomicU64,
    found_parts: AtomicU64,
    pub(crate) evictions: AtomicU64,
    pub(crate) offloaded: AtomicU64,
    deleted: Arc<AtomicU64>,
    /// Keys [`Cold::delete_later`] has yet to delete: an upload under one
    /// of them waits until it is gone, so that a delete of a failed upload's
    /// objects never takes the next one's.
    deleting_now: Arc<(Mutex<std::collections::HashSet<String>>, Condvar)>,
    /// Wakes the cold tier's thread ([`crate::storage::Database::spawn_cold`]).
    pub(crate) signal: Signal,
}

/// Wakes the cold tier's thread when the cache outgrows its limit, and
/// tells it to stop.
#[derive(Default)]
pub(crate) struct Signal {
    /// Due, closed.
    state: Mutex<(bool, bool)>,
    cv: Condvar,
}

impl std::fmt::Debug for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Signal")
    }
}

impl Signal {
    pub fn wake(&self) {
        lock(&self.state).0 = true;
        self.cv.notify_all();
    }

    pub fn close(&self) {
        lock(&self.state).1 = true;
        self.cv.notify_all();
    }

    /// Waits until woken or `timeout` passes; false once closed.
    pub fn wait(&self, timeout: Duration) -> bool {
        let mut st = lock(&self.state);
        if !st.0 && !st.1 {
            st = self.cv.wait_timeout(st, timeout).unwrap_or_else(|e| e.into_inner()).0;
        }
        st.0 = false;
        !st.1
    }
}

pub const PENDING: &str = "PENDING";

impl Cold {
    /// The cold tier of the database with id `db_id` in directory `db_dir`.
    pub fn new(opts: ColdOptions, db_dir: &Path, db_id: u64) -> Result<Arc<Cold>> {
        // A cache directory given is shared by databases: each has its own
        // under it, named by its id, since files are named by segment id.
        let dir = match &opts.cache_dir {
            Some(d) => d.join(format!("{db_id:016x}")),
            None => db_dir.join("cold"),
        };
        std::fs::create_dir_all(&dir)?;
        let pending = match std::fs::read(dir.join(PENDING)) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| corrupt(format!("{}: {e}", dir.join(PENDING).display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Pending::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Arc::new(Cold {
            opts,
            prefix: format!("{db_id:016x}/"),
            dir,
            used: AtomicU64::new(0),
            gen: AtomicU64::new(1),
            clock: AtomicU64::new(0),
            objects: Mutex::new(HashMap::new()),
            images: Mutex::new(HashMap::new()),
            pending: Mutex::new(pending),
            fetches: AtomicU64::new(0),
            fetched_bytes: AtomicU64::new(0),
            found_parts: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            offloaded: AtomicU64::new(0),
            deleted: Arc::new(AtomicU64::new(0)),
            deleting_now: Arc::default(),
            signal: Signal::default(),
        }))
    }

    /// The key segment `id`'s image gets.
    pub fn key_of(&self, id: u64) -> String {
        format!("{}{}", self.prefix, crate::storage::dir::segment_name(id))
    }

    /// Deletes what a crash left: objects whose upload or delete `PENDING`
    /// records and the manifest does not name, and cache files of segments
    /// it does not name cold (or of an older generation).
    pub fn recover(self: &Arc<Self>, manifest: &Manifest) -> Result<()> {
        let named: HashMap<u64, &ColdRef> = manifest.segments().filter_map(|s| Some((s.id, s.cold.as_ref()?))).collect();
        let keys: std::collections::HashSet<&str> = named.values().map(|c| c.key.as_str()).collect();
        let stale: Vec<String> = {
            let mut p = lock(&self.pending);
            let p = &mut *p;
            let all: Vec<String> = p.uploading.drain(..).chain(p.deleting.drain(..)).collect();
            all.into_iter().filter(|k| !keys.contains(k.trim_end_matches(".crc"))).collect()
        };
        self.store_pending()?;
        self.delete_later(stale);
        // Cache files: seg-<id>.crc, seg-<id>-<gen>.cache.
        let mut newest: HashMap<u64, u64> = HashMap::new();
        let mut files = Vec::new();
        for e in std::fs::read_dir(&self.dir)? {
            let name = e?.file_name().to_string_lossy().into_owned();
            if let Some((id, gen)) = parse_cache_name(&name) {
                files.push((name, id, gen));
                if gen.is_some_and(|g| newest.get(&id).is_none_or(|&n| g > n)) {
                    newest.insert(id, gen.unwrap());
                }
            }
        }
        let top = newest.values().copied().max().unwrap_or(0);
        self.gen.store(top + 1, Ordering::Relaxed);
        for (name, id, gen) in files {
            let keep = named.contains_key(&id) && gen.is_none_or(|g| newest.get(&id) == Some(&g));
            if !keep {
                let _ = std::fs::remove_file(self.dir.join(name));
            }
        }
        Ok(())
    }

    fn store_pending(&self) -> Result<()> {
        // Held while the file is written: threads writing it at once would
        // share `PENDING.tmp` (one's rename then finds no file), and an
        // older record could replace a newer one.
        let p = lock(&self.pending);
        let path = self.dir.join(PENDING);
        if p.uploading.is_empty() && p.deleting.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            };
        }
        let tmp = self.dir.join("PENDING.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&*p).expect("pending serialises"))?;
        File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Records that `key` (and its table) are being written, before they
    /// are.
    pub fn uploading(&self, key: &str) -> Result<()> {
        lock(&self.pending).uploading.extend([key.to_string(), format!("{key}.crc")]);
        self.store_pending()
    }

    /// The commit that names `key` is done (or `key` is owed a delete).
    pub fn uploaded(&self, key: &str) {
        lock(&self.pending).uploading.retain(|k| k.trim_end_matches(".crc") != key);
        let _ = self.store_pending();
    }

    /// Records that `keys` are to be deleted: a crash before they go leaves
    /// them to [`Cold::recover`].
    fn owe_delete(&self, keys: &[String]) {
        {
            let mut p = lock(&self.pending);
            for k in keys {
                if !p.deleting.contains(k) {
                    p.deleting.push(k.clone());
                }
            }
        }
        if let Err(e) = self.store_pending() {
            tracing::warn!(error = %e, "could not record cold objects to delete; a crash now would leave them on the store");
        }
    }

    /// Deletes `keys` from the store on a thread of its own, recording them
    /// first ([`Cold::owe_delete`]).
    pub fn delete_later(self: &Arc<Self>, keys: Vec<String>) {
        if keys.is_empty() {
            return;
        }
        self.owe_delete(&keys);
        lock(&self.deleting_now.0).extend(keys.iter().cloned());
        let cold = Arc::downgrade(self);
        let store = self.opts.store.clone();
        let deleted = self.deleted.clone();
        let now = self.deleting_now.clone();
        let done = move |k: &str| {
            lock(&now.0).remove(k);
            now.1.notify_all();
        };
        let spawned = std::thread::Builder::new().name("chloedb-cold-delete".into()).spawn({
            let keys = keys.clone();
            let done = done.clone();
            move || {
                for k in keys {
                    match store.delete(&k) {
                        Ok(()) => {
                            deleted.fetch_add(1, Ordering::Relaxed);
                            if let Some(c) = cold.upgrade() {
                                lock(&c.pending).deleting.retain(|x| *x != k);
                                let _ = c.store_pending();
                            }
                        }
                        Err(e) => tracing::warn!(key = %k, error = %e, "could not delete a cold segment's object; the next open tries again"),
                    }
                    done(&k);
                }
            }
        });
        if spawned.is_err() {
            // Left to the next open, as a failed delete is.
            keys.iter().for_each(|k| done(k));
        }
    }

    /// Waits until no delete of `key` or its part table is left to run
    /// ([`Cold::deleting_now`]).
    fn wait_for_deletes(&self, key: &str) {
        let (set, cv) = &*self.deleting_now;
        let crc = format!("{key}.crc");
        let mut s = lock(set);
        while s.contains(key) || s.contains(&crc) {
            s = cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Uploads segment `id`'s image, the file at `path` whose bytes are
    /// `image`, and its part table, recording the upload first: a crash
    /// before a version names them leaves them to [`Cold::recover`]. Returns
    /// the manifest's reference to them and the object, whose image the
    /// caller makes ([`Cold::fresh_image`]). On a failure, what reached the
    /// store is deleted.
    pub fn upload(self: &Arc<Self>, id: u64, path: &Path, image: &[u8]) -> Result<(ColdRef, Arc<ColdObject>)> {
        let key = self.key_of(id);
        let table = ChunkTable::of(image, CHUNK);
        let bytes = table.encode();
        self.wait_for_deletes(&key);
        self.uploading(&key)?;
        let object = Arc::new(ColdObject { cold: self.clone(), id, key: key.clone(), table, obsolete: AtomicBool::new(false) });
        let sent = (|| -> Result<()> {
            self.opts.store.put_file(&key, path)?;
            self.opts.store.put(&format!("{key}.crc"), &bytes)?;
            std::fs::write(self.dir.join(table_name(id)), &bytes)?;
            Ok(())
        })();
        if let Err(e) = sent {
            object.retire();
            // Its drop records the delete before the upload's record goes.
            drop(object);
            self.uploaded(&key);
            return Err(e);
        }
        lock(&self.objects).insert(id, Arc::downgrade(&object));
        Ok((ColdRef { key, chunk: CHUNK, table_crc: crc32fast::hash(&bytes) }, object))
    }

    /// The object of cold segment `id`, with its part table: the one open
    /// already, else read from the cache directory or, if it is not there
    /// or fails its checksum, from the store.
    pub fn object(self: &Arc<Self>, id: u64, cref: &ColdRef, len: u64) -> Result<Arc<ColdObject>> {
        let mut objects = lock(&self.objects);
        if let Some(o) = objects.get(&id).and_then(Weak::upgrade) {
            if o.key == cref.key {
                return Ok(o);
            }
        }
        let local = self.dir.join(table_name(id));
        let table = match std::fs::read(&local) {
            Ok(b) if crc32fast::hash(&b) == cref.table_crc => ChunkTable::decode(&b)?,
            _ => {
                // The table's length follows from the image's.
                let key = format!("{}.crc", cref.key);
                let n = len.div_ceil(cref.chunk.max(1) as u64);
                let b = self.opts.store.get_range(&key, 0..24 + 4 * n)?;
                if crc32fast::hash(&b) != cref.table_crc {
                    return Err(corrupt(format!("cold segment {id}: {key} fails its checksum")));
                }
                std::fs::write(&local, &b)?;
                ChunkTable::decode(&b)?
            }
        };
        if table.len != len || table.chunk != cref.chunk {
            return Err(corrupt(format!("cold segment {id}: its part table describes another image")));
        }
        let o = Arc::new(ColdObject { cold: self.clone(), id, key: cref.key.clone(), table, obsolete: AtomicBool::new(false) });
        objects.insert(id, Arc::downgrade(&o));
        Ok(o)
    }

    /// Segment `id`'s current image: the one open, else its newest cache
    /// file, else a new one. Its header and keys are fetched now, if they
    /// are not in the file.
    pub fn image(self: &Arc<Self>, object: &Arc<ColdObject>) -> Result<Arc<ColdImage>> {
        if let Some(i) = lock(&self.images).get(&object.id).and_then(Weak::upgrade) {
            if Arc::ptr_eq(&i.object, object) {
                return Ok(i);
            }
        }
        let prefix = format!("seg-{:010}-", object.id);
        let found = std::fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let (id, gen) = parse_cache_name(&name)?;
                (id == object.id && name.starts_with(&prefix)).then_some((gen?, name))
            })
            .max();
        let path = match found {
            Some((_, name)) => self.dir.join(name),
            None => self.dir.join(cache_name(object.id, self.gen.fetch_add(1, Ordering::Relaxed))),
        };
        let image = ColdImage::open(object.clone(), path)?;
        image.pin_header_and_keys()?;
        lock(&self.images).insert(object.id, Arc::downgrade(&image));
        Ok(image)
    }

    /// A new image of `object` over a new cache file holding what `from`
    /// had pinned (its header and keys), copied from `image` if given (a
    /// local image being made cold) or from `from`'s file; registered as
    /// the segment's current image.
    pub fn fresh_image(self: &Arc<Self>, object: &Arc<ColdObject>, pinned: &[Range<u64>], image: Option<&[u8]>, from: Option<&ColdImage>) -> Result<Arc<ColdImage>> {
        let path = self.dir.join(cache_name(object.id, self.gen.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_file(&path);
        let fresh = ColdImage::open(object.clone(), path)?;
        for r in pinned {
            let bytes = match (image, from) {
                (Some(img), _) => img[r.start as usize..r.end as usize].to_vec(),
                (None, Some(f)) => {
                    let mut b = vec![0u8; (r.end - r.start) as usize];
                    f.file.read_exact_at(&mut b, r.start)?;
                    b
                }
                (None, None) => break,
            };
            fresh.file.write_all_at(&bytes, r.start)?;
        }
        fresh.pin_header_and_keys()?;
        lock(&self.images).insert(object.id, Arc::downgrade(&fresh));
        Ok(fresh)
    }

    /// The live images, least recently read first.
    pub fn images_by_age(&self) -> Vec<Arc<ColdImage>> {
        let mut all: Vec<Arc<ColdImage>> = lock(&self.images).values().filter_map(Weak::upgrade).collect();
        all.sort_by_key(|i| i.last_use.load(Ordering::Relaxed));
        all
    }

    /// Fetched parts held, headers and keys apart.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> ColdStats {
        let images: Vec<Arc<ColdImage>> = lock(&self.images).values().filter_map(Weak::upgrade).collect();
        ColdStats {
            store: self.opts.store.describe(),
            segments: images.len(),
            bytes: images.iter().map(|i| i.object.table.len).sum(),
            cached_bytes: images.iter().map(|i| i.held.load(Ordering::Relaxed) + i.pinned_bytes()).sum(),
            pinned_bytes: images.iter().map(|i| i.pinned_bytes()).sum(),
            cache_limit: self.opts.cache_bytes,
            fetches: self.fetches.load(Ordering::Relaxed),
            fetched_bytes: self.fetched_bytes.load(Ordering::Relaxed),
            found_parts: self.found_parts.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            segments_offloaded: self.offloaded.load(Ordering::Relaxed),
            objects_deleted: self.deleted.load(Ordering::Relaxed),
        }
    }
}

/// The byte ranges, whole parts of `t`, that a cold segment's cache
/// always keeps: the header, the key index (section `pk`, and the keyword
/// column it reads its keys from, if it does), and `fields`. `read` reads
/// bytes of the image.
pub fn pinned_of(t: &ChunkTable, read: &dyn Fn(Range<usize>) -> Result<Vec<u8>>) -> Result<Vec<Range<u64>>> {
    let len = t.len as usize;
    let header = crate::segment::header_len_from(&read(0..24.min(len))?)?;
    if header > len {
        return Err(corrupt("the segment's directory lies outside its image"));
    }
    let (_, sections) = crate::segment::directory(&read(0..header)?)?;
    let inside = |name: &str, r: &Range<usize>| match r.end <= len {
        true => Ok(()),
        false => Err(corrupt(format!("section {name} lies outside the image"))),
    };
    let mut keep = vec![crate::storage::pk::SECTION.to_string(), crate::segment::FIELDS.to_string()];
    if let Some((name, r)) = sections.iter().find(|(n, r)| n == crate::storage::pk::SECTION && !r.is_empty()) {
        inside(name, r)?;
        if let Some(f) = crate::storage::pk::key_column(crate::postings::bytes::OwnedBytes::new(read(r.clone())?))? {
            keep.extend(["dict", "codes", "post"].map(|p| format!("kw.{f}.{p}")));
        }
    }
    let whole = |r: &Range<usize>| {
        let p = t.parts(r);
        t.range(p.start).start..t.range(p.end - 1).end
    };
    let mut out = vec![whole(&(0..header))];
    for (name, r) in &sections {
        if keep.contains(name) && !r.is_empty() {
            inside(name, r)?;
            out.push(whole(r));
        }
    }
    Ok(out)
}

fn table_name(id: u64) -> String {
    format!("seg-{id:010}.crc")
}

fn cache_name(id: u64, gen: u64) -> String {
    format!("seg-{id:010}-{gen:08}.cache")
}

/// The segment id of a cache directory file, with the generation of a
/// cache file (`None` for a part table).
fn parse_cache_name(name: &str) -> Option<(u64, Option<u64>)> {
    let rest = name.strip_prefix("seg-")?;
    if let Some(id) = rest.strip_suffix(".crc") {
        return Some((id.parse().ok()?, None));
    }
    let (id, gen) = rest.strip_suffix(".cache")?.split_once('-')?;
    Some((id.parse().ok()?, Some(gen.parse().ok()?)))
}

/// A cold segment's image on the store, shared by the images of it the
/// cache has opened. Once the database no longer names it
/// ([`ColdObject::retire`]), the last of them to go deletes it.
#[derive(Debug)]
pub(crate) struct ColdObject {
    cold: Arc<Cold>,
    pub id: u64,
    pub key: String,
    pub table: ChunkTable,
    obsolete: AtomicBool,
}

impl ColdObject {
    /// The database no longer names the segment: its objects and cache
    /// files go once nothing reads it. Recorded now, so that a crash before
    /// then leaves them to [`Cold::recover`].
    pub fn retire(&self) {
        if !self.obsolete.swap(true, Ordering::AcqRel) {
            self.cold.owe_delete(&[self.key.clone(), format!("{}.crc", self.key)]);
        }
    }
}

impl Drop for ColdObject {
    fn drop(&mut self) {
        if self.obsolete.load(Ordering::Acquire) {
            let _ = std::fs::remove_file(self.cold.dir.join(table_name(self.id)));
            self.cold.delete_later(vec![self.key.clone(), format!("{}.crc", self.key)]);
        }
    }
}

/// One cache file of a cold segment: a sparse file of the image's length,
/// mapped as a local image is, whose parts are fetched as they are first
/// read ([`Fetch`]).
#[derive(Debug)]
pub(crate) struct ColdImage {
    pub object: Arc<ColdObject>,
    file: File,
    path: PathBuf,
    /// Parts known to be in the file.
    present: Box<[AtomicBool]>,
    /// Parts kept for as long as the segment is cold: its header and keys.
    pinned: Mutex<Vec<bool>>,
    /// One fetch at a time.
    fetching: Mutex<()>,
    last_use: AtomicU64,
    /// Bytes of fetched parts not pinned.
    held: AtomicU64,
    /// Evicted: another image took its place and its file is unlinked.
    retired: AtomicBool,
}

impl ColdImage {
    fn open(object: Arc<ColdObject>, path: PathBuf) -> Result<Arc<ColdImage>> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
        if file.metadata()?.len() != object.table.len {
            file.set_len(object.table.len)?;
        }
        let n = object.table.crcs.len();
        Ok(Arc::new(ColdImage {
            object,
            file,
            path,
            present: (0..n).map(|_| AtomicBool::new(false)).collect(),
            pinned: Mutex::new(vec![false; n]),
            fetching: Mutex::new(()),
            last_use: AtomicU64::new(0),
            held: AtomicU64::new(0),
            retired: AtomicBool::new(false),
        }))
    }

    /// The file, to map.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Fetches the header and the keys (the `pk` and `fields` sections)
    /// and pins them, so that they stay for as long as the segment is cold.
    fn pin_header_and_keys(&self) -> Result<()> {
        let pinned = self.pinned_ranges()?;
        for r in &pinned {
            self.ensure(r.start as usize..r.end as usize)?;
        }
        let t = &self.object.table;
        let mut p = lock(&self.pinned);
        for r in &pinned {
            for c in t.parts(&(r.start as usize..r.end as usize)) {
                if !p[c] && self.present[c].load(Ordering::Acquire) {
                    let len = t.range(c).end - t.range(c).start;
                    self.held.fetch_sub(len, Ordering::Relaxed);
                    if !self.retired.load(Ordering::Acquire) {
                        self.object.cold.used.fetch_sub(len, Ordering::Relaxed);
                    }
                }
                p[c] = true;
            }
        }
        Ok(())
    }

    /// The byte ranges of the image's header and keys, whole parts
    /// ([`pinned_of`]).
    pub fn pinned_ranges(&self) -> Result<Vec<Range<u64>>> {
        let read = |r: Range<usize>| -> Result<Vec<u8>> {
            self.ensure(r.clone())?;
            let mut b = vec![0u8; r.len()];
            self.file.read_exact_at(&mut b, r.start as u64)?;
            Ok(b)
        };
        pinned_of(&self.object.table, &read).map_err(|e| match e {
            Error::Corrupt(m) => corrupt(format!("cold segment {}: {m}", self.object.id)),
            e => e,
        })
    }

    fn pinned_bytes(&self) -> u64 {
        let t = &self.object.table;
        lock(&self.pinned).iter().enumerate().filter(|(_, &p)| p).map(|(c, _)| t.range(c).end - t.range(c).start).sum()
    }

    /// Bytes of fetched parts it holds, pinned ones apart.
    pub fn held(&self) -> u64 {
        self.held.load(Ordering::Relaxed)
    }

    /// Another image took its place: its file is unlinked (the mappings of
    /// it keep its bytes until they go) and its parts no longer count.
    pub fn retire(&self) {
        if !self.retired.swap(true, Ordering::AcqRel) {
            self.object.cold.used.fetch_sub(self.held.load(Ordering::Relaxed), Ordering::Relaxed);
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Checks the parts in the file against their checksums, fetching
    /// a damaged one again.
    pub fn verify_present(&self) -> Result<()> {
        let t = &self.object.table;
        let _one = lock(&self.fetching);
        let mut damaged = Vec::new();
        let mut buf = Vec::new();
        for c in 0..t.crcs.len() {
            if !self.present[c].load(Ordering::Acquire) {
                continue;
            }
            let r = t.range(c);
            buf.resize((r.end - r.start) as usize, 0);
            if self.file.read_exact_at(&mut buf, r.start).is_err() || crc32fast::hash(&buf) != t.crcs[c] {
                tracing::warn!(segment = self.object.id, part = c, "a part of a cold segment in the cache is damaged; fetching it again");
                damaged.push(c);
            }
        }
        if damaged.is_empty() {
            return Ok(());
        }
        self.fetch(&damaged)
    }

    /// Part `c` arrived (or was found in the file).
    fn arrived(&self, c: usize) {
        if !self.present[c].swap(true, Ordering::AcqRel) && !lock(&self.pinned)[c] {
            let r = self.object.table.range(c);
            self.held.fetch_add(r.end - r.start, Ordering::Relaxed);
            if !self.retired.load(Ordering::Acquire) {
                let cold = &self.object.cold;
                let used = cold.used.fetch_add(r.end - r.start, Ordering::Relaxed) + (r.end - r.start);
                if used > cold.opts.cache_bytes {
                    cold.signal.wake();
                }
            }
        }
    }

    /// Fetches `parts` (sorted) in runs of neighbours, [`PARALLEL`]
    /// requests at once, checks each part and writes it into the file.
    fn fetch(&self, parts: &[usize]) -> Result<()> {
        let t = &self.object.table;
        let mut runs: Vec<Range<usize>> = Vec::new();
        for &c in parts {
            match runs.last_mut() {
                Some(r) if r.end == c && t.range(c).end - t.range(r.start).start <= RUN => r.end = c + 1,
                _ => runs.push(c..c + 1),
            }
        }
        let cold = &self.object.cold;
        let one = |r: &Range<usize>| -> Result<()> {
            let bytes = t.range(r.start).start..t.range(r.end - 1).end;
            let got = cold.opts.store.get_range(&self.object.key, bytes.clone())?;
            cold.fetches.fetch_add(1, Ordering::Relaxed);
            cold.fetched_bytes.fetch_add(got.len() as u64, Ordering::Relaxed);
            if got.len() as u64 != bytes.end - bytes.start {
                return Err(corrupt(format!("cold segment {}: {} is shorter than its image", self.object.id, self.object.key)));
            }
            for c in r.clone() {
                let pr = t.range(c);
                let part = &got[(pr.start - bytes.start) as usize..(pr.end - bytes.start) as usize];
                if crc32fast::hash(part) != t.crcs[c] {
                    return Err(corrupt(format!("cold segment {}: part {c} of {} fails its checksum", self.object.id, self.object.key)));
                }
            }
            self.file.write_all_at(&got, bytes.start)?;
            for c in r.clone() {
                self.arrived(c);
            }
            Ok(())
        };
        if runs.len() == 1 {
            return one(&runs[0]);
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let failed: Mutex<Option<Error>> = Mutex::new(None);
        std::thread::scope(|s| {
            for _ in 0..PARALLEL.min(runs.len()) {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= runs.len() || lock(&failed).is_some() {
                        break;
                    }
                    if let Err(e) = one(&runs[i]) {
                        lock(&failed).get_or_insert(e);
                    }
                });
            }
        });
        match failed.into_inner().unwrap_or_else(|e| e.into_inner()) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Fetch for ColdImage {
    fn ensure(&self, range: Range<usize>) -> Result<()> {
        if range.is_empty() {
            return Ok(());
        }
        let t = &self.object.table;
        if range.end as u64 > t.len {
            return Err(corrupt(format!("cold segment {}: a read past the end of its image", self.object.id)));
        }
        let cold = &self.object.cold;
        self.last_use.store(cold.clock.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        let parts = t.parts(&range);
        if parts.clone().all(|c| self.present[c].load(Ordering::Acquire)) {
            return Ok(());
        }
        let _one = lock(&self.fetching);
        // Parts in the file from before this process opened it are found
        // by their checksums; a damaged one is fetched again.
        let mut missing = Vec::new();
        let mut buf = Vec::new();
        for c in parts {
            if self.present[c].load(Ordering::Acquire) {
                continue;
            }
            let r = t.range(c);
            buf.resize((r.end - r.start) as usize, 0);
            if self.file.read_exact_at(&mut buf, r.start).is_ok() && crc32fast::hash(&buf) == t.crcs[c] {
                cold.found_parts.fetch_add(1, Ordering::Relaxed);
                self.arrived(c);
            } else {
                missing.push(c);
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        self.fetch(&missing).map_err(|e| match e {
            Error::Io(io) => Error::Io(std::io::Error::new(io.kind(), format!("cold segment {}: fetching from {}: {io}", self.object.id, cold.opts.store.describe()))),
            e => e,
        })
    }
}

impl Drop for ColdImage {
    fn drop(&mut self) {
        if self.retired.load(Ordering::Acquire) {
            return;
        }
        self.object.cold.used.fetch_sub(self.held.load(Ordering::Relaxed), Ordering::Relaxed);
        if self.object.obsolete.load(Ordering::Acquire) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// X-A7: an object is durable only once the directory holding its new
    /// name is synced, and each directory made for it once its parent is.
    #[cfg(feature = "fault-injection")]
    #[test]
    fn a_directory_store_syncs_the_directories_it_writes_into() {
        use crate::storage::io;
        let dir = std::env::temp_dir().join(format!("chloedb-dirstore-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = DirStore::new(&dir).unwrap();
        io::arm(1);
        s.put("a/b/HEAD", b"one").unwrap();
        // `a` (synced in the root), `a/b` (in `a`), then `a/b` after the rename.
        assert_eq!(io::calls(), 3);
        s.put("a/b/HEAD", b"two").unwrap();
        assert_eq!(io::calls(), 4, "an existing directory is synced once, after the rename");
        let _ = io::crash();
        // The last sync failing fails the write: it was not made durable.
        io::arm(1);
        io::fail_at(io::Fault { at: 1, errno: libc::EIO, sticky: false, crash: false });
        assert!(s.put_if_version("a/b/HEAD", b"three", &version_of(b"two")).is_err());
        let _ = io::crash();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_store_writes_under_conditions() {
        let dir = std::env::temp_dir().join(format!("chloedb-dirstore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Arc::new(DirStore::new(&dir).unwrap());
        assert_eq!(s.get("t/HEAD").unwrap(), None);
        assert_eq!(s.put_if_version("t/HEAD", b"x", "any").unwrap(), None, "nothing to replace");
        let v1 = s.put_if_absent("t/HEAD", b"one").unwrap().expect("created");
        assert_eq!(s.put_if_absent("t/HEAD", b"other").unwrap(), None);
        assert_eq!(s.get("t/HEAD").unwrap(), Some((b"one".to_vec(), v1.clone())));
        let v2 = s.put_if_version("t/HEAD", b"two", &v1).unwrap().expect("replaced");
        assert_ne!(v1, v2);
        assert_eq!(s.put_if_version("t/HEAD", b"three", &v1).unwrap(), None, "a stale version");
        assert_eq!(s.get("t/HEAD").unwrap().unwrap().0, b"two");
        assert_eq!(s.get("t/HEAD/under").unwrap(), None);

        // Many at once replacing one version: one of them does.
        let wins: usize = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8u8).map(|n| scope.spawn({
                let (s, v2) = (s.clone(), v2.clone());
                move || usize::from(s.put_if_version("t/HEAD", &[n], &v2).unwrap().is_some())
            })).collect();
            threads.into_iter().map(|t| t.join().unwrap()).sum()
        });
        assert_eq!(wins, 1);

        s.put("t/i/log/1", b"l").unwrap();
        s.put("t/i/base/1", b"b").unwrap();
        s.put("t2/HEAD", b"h").unwrap();
        let mut keys = s.list("t/").unwrap();
        keys.sort();
        assert_eq!(keys, ["t/HEAD", "t/i/base/1", "t/i/log/1"]);
        assert_eq!(s.list("t/i/l").unwrap(), ["t/i/log/1"]);
        assert!(s.list("nowhere/").unwrap().is_empty());
        assert_eq!(s.list_dirs("").unwrap(), ["t", "t2"]);
        assert_eq!(s.list_dirs("t/i/").unwrap(), ["base", "log"]);
        assert!(s.list_dirs("nowhere/").unwrap().is_empty());
        let dest = dir.join("copy");
        s.get_file("t/i/base/1", &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"b");
        assert!(s.get_file("t/i/base/2", &dest).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunk_tables_round_trip_and_refuse_damage() {
        let image: Vec<u8> = (0..(3 * CHUNK as usize + 17)).map(|i| (i * 7 % 251) as u8).collect();
        let t = ChunkTable::of(&image, CHUNK);
        assert_eq!(t.crcs.len(), 4);
        assert_eq!(t.range(3), 3 * CHUNK as u64..image.len() as u64);
        let b = t.encode();
        assert_eq!(ChunkTable::decode(&b).unwrap(), t);
        assert!(ChunkTable::decode(&b[..b.len() - 4]).is_err());
        let mut bad = b.clone();
        bad[0] ^= 1;
        assert!(ChunkTable::decode(&bad).is_err());
        assert_eq!(t.parts(&(0..1)), 0..1);
        assert_eq!(t.parts(&(CHUNK as usize - 1..CHUNK as usize + 1)), 0..2);
    }

    #[test]
    fn cache_names() {
        assert_eq!(parse_cache_name(&cache_name(42, 7)), Some((42, Some(7))));
        assert_eq!(parse_cache_name(&table_name(42)), Some((42, None)));
        assert_eq!(parse_cache_name("PENDING"), None);
        assert_eq!(parse_cache_name("seg-x.cache"), None);
    }

    /// Threads that record uploads and deletes at once (an offload, and the
    /// thread deleting a failed upload's objects) each write the record
    /// whole, and the last one written holds every change.
    #[test]
    fn the_pending_record_takes_writes_from_threads_at_once() {
        let d = std::env::temp_dir().join(format!("strata-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let store: Arc<dyn ObjectStore> = Arc::new(DirStore::new(d.join("store")).unwrap());
        let cold = Cold::new(ColdOptions::new(store), &d, 7).unwrap();
        std::thread::scope(|s| {
            for t in 0..8 {
                let cold = &cold;
                s.spawn(move || {
                    for i in 0..200 {
                        let key = format!("t{t}-{i}");
                        cold.uploading(&key).unwrap();
                        if i % 2 == 0 {
                            cold.uploaded(&key);
                        }
                    }
                });
            }
        });
        let on_disk: Pending = serde_json::from_slice(&std::fs::read(cold.dir.join(PENDING)).unwrap()).unwrap();
        assert_eq!(on_disk.uploading.len(), 8 * 100 * 2);
        assert_eq!(on_disk.uploading, lock(&cold.pending).uploading);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A directory store whose first part table upload fails, and whose
    /// deletes take a while, as a store's requests can.
    #[derive(Debug)]
    struct SlowDeletes {
        inner: DirStore,
        fail_next_table: AtomicBool,
    }

    impl ObjectStore for SlowDeletes {
        fn put_file(&self, key: &str, path: &Path) -> Result<()> {
            self.inner.put_file(key, path)
        }
        fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
            if key.ends_with(".crc") && self.fail_next_table.swap(false, Ordering::AcqRel) {
                return Err(Error::Io(std::io::Error::other("the store refused the table")));
            }
            self.inner.put(key, bytes)
        }
        fn get_range(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>> {
            self.inner.get_range(key, range)
        }
        fn delete(&self, key: &str) -> Result<()> {
            std::thread::sleep(Duration::from_millis(100));
            self.inner.delete(key)
        }
        fn describe(&self) -> String {
            self.inner.describe()
        }
    }

    /// An upload that fails leaves its objects to be deleted; one of the
    /// same segment right after it, under the same keys, keeps its own.
    #[test]
    fn a_failed_uploads_deletes_leave_the_next_uploads_objects() {
        let d = std::env::temp_dir().join(format!("strata-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let store = Arc::new(SlowDeletes { inner: DirStore::new(d.join("store")).unwrap(), fail_next_table: AtomicBool::new(true) });
        let cold = Cold::new(ColdOptions::new(store.clone()), &d, 7).unwrap();
        let image: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let path = d.join("seg-0000000003.strata");
        std::fs::write(&path, &image).unwrap();
        assert!(cold.upload(3, &path, &image).is_err());
        let (cref, object) = cold.upload(3, &path, &image).unwrap();
        // Long after the failed upload's deletes would have run.
        std::thread::sleep(Duration::from_millis(500));
        for key in [cref.key.clone(), format!("{}.crc", cref.key)] {
            assert!(store.get_range(&key, 0..1).is_ok(), "{key} was deleted");
        }
        drop(object);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn dir_store_keys_stay_inside() {
        let d = std::env::temp_dir().join(format!("strata-dirstore-{}", std::process::id()));
        let s = DirStore::new(&d).unwrap();
        s.put("a/b", b"hello").unwrap();
        assert_eq!(s.get_range("a/b", 1..4).unwrap(), b"ell");
        assert!(s.get_range("a/b", 3..9).is_err());
        assert!(s.get_range("a/missing", 0..1).is_err());
        assert!(s.put("../x", b"").is_err() && s.put("/etc/x", b"").is_err());
        s.delete("a/b").unwrap();
        s.delete("a/b").unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }
}
