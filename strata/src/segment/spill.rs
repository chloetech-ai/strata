//! A segment build that writes as it goes: the sections it finishes, and
//! the buffers that grow with every document it adds (a vector field's
//! float32 rows, the doc store's compressed blocks), go to unnamed
//! temporary files beside the database instead of staying in memory until
//! the image is written. A merge builds this way, so it holds its
//! builders' working state and not its output; [`SegmentWriter`] then
//! copies each spilled section into the image, which is the same bytes as
//! one built in memory.
//!
//! Each file is removed from its directory as soon as it is created, so a
//! process that dies mid-merge leaves nothing behind (opening a database
//! removes the name of one created by a process that died in between,
//! [`remove_leftovers`]). The files are never synced: they need to outlive
//! the merge, not the process.
//!
//! [`SegmentWriter`]: crate::segment::SegmentWriter

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::storage::io as sio;
use crate::storage::merge::Pacer;

/// Whether a build that spills writes section `name` out as it adds
/// documents rather than holding what it is made of until the end: a
/// vector field's float32 rows and the doc store's blocks. A merge's
/// memory estimate leaves their bytes out
/// ([`crate::storage::merge::merge_memory`]).
pub fn streams(name: &str) -> bool {
    name.ends_with(".f32") || name == "store.data"
}

/// Sections shorter than this stay in memory: writing them out saves
/// nothing worth a write.
pub const MIN_SPILLED: usize = 64 << 10;

/// Spilled bytes are written in pieces of this size, each paced.
const CHUNK: usize = 1 << 20;

const PAGE: u64 = crate::segment::PAGE as u64;

/// Where one segment build spills: a directory, a name for its files, and
/// the pace its writes keep ([`Pacer`], shared with the image's).
pub struct Spill {
    dir: PathBuf,
    prefix: String,
    pacer: Option<Arc<Pacer>>,
    /// Finished sections, each at a page-aligned offset, and the end of
    /// the last one.
    sections: Arc<File>,
    end: Mutex<u64>,
    /// Bytes written, to this file and the streams.
    written: AtomicU64,
}

/// A piece of a spill file: a section's bytes.
#[derive(Clone)]
pub struct Spilled {
    file: Arc<File>,
    off: u64,
    len: u64,
}

impl Spilled {
    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Writes the bytes to `w`, a piece at a time.
    pub fn copy_to(&self, w: &mut impl io::Write) -> io::Result<()> {
        let mut buf = vec![0u8; CHUNK.min(self.len as usize)];
        let mut at = 0u64;
        while at < self.len {
            let n = (self.len - at).min(CHUNK as u64) as usize;
            self.file.read_exact_at(&mut buf[..n], self.off + at)?;
            w.write_all(&buf[..n])?;
            at += n as u64;
        }
        Ok(())
    }

    /// Gives the bytes' disk space back, once they are copied (Linux; a
    /// stream's file goes when its last piece does).
    pub fn release(&self) {
        punch_hole(&self.file, self.off, self.len);
    }
}

#[cfg(target_os = "linux")]
fn punch_hole(f: &File, off: u64, len: u64) {
    use std::os::fd::AsRawFd;
    if len == 0 {
        return;
    }
    // SAFETY: fallocate only changes the file's allocation; a failure (a
    // file system without holes) leaves the space in use until the file
    // is closed.
    unsafe {
        libc::fallocate(f.as_raw_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, off as libc::off_t, len as libc::off_t);
    }
}

#[cfg(not(target_os = "linux"))]
fn punch_hole(_f: &File, _off: u64, _len: u64) {}

/// A new spill file's name: `prefix`, the process and a number.
fn name(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("{prefix}spill-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Removes the spill files of the database at `dir`/`prefix` that a
/// process left named (it died between creating one and removing its name).
pub fn remove_leftovers(dir: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let start = format!("{prefix}spill-");
    for e in entries.flatten() {
        if e.file_name().to_str().is_some_and(|n| n.starts_with(&start)) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

impl Spill {
    /// A spill into files named `prefix` and a number in `dir`.
    pub fn new(dir: &Path, prefix: &str, pacer: Option<Arc<Pacer>>) -> io::Result<Arc<Spill>> {
        let sections = Arc::new(temp_file(dir, prefix)?);
        Ok(Arc::new(Spill { dir: dir.to_path_buf(), prefix: prefix.to_string(), pacer, sections, end: Mutex::new(0), written: AtomicU64::new(0) }))
    }

    /// Bytes written so far.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// Writes `data` at `off` in `f`, paced, a piece at a time.
    fn write(&self, f: &File, data: &[u8], off: u64) -> io::Result<()> {
        for (k, piece) in data.chunks(CHUNK).enumerate() {
            if let Some(p) = &self.pacer {
                p.pace(piece.len());
            }
            sio::write_at(f, piece, off + (k * CHUNK) as u64)?;
            self.written.fetch_add(piece.len() as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Writes a finished section's bytes.
    pub fn put(&self, data: &[u8]) -> io::Result<Spilled> {
        let off = {
            let mut end = self.end.lock().unwrap_or_else(|e| e.into_inner());
            let off = *end;
            *end = (off + data.len() as u64).next_multiple_of(PAGE);
            off
        };
        self.write(&self.sections, data, off)?;
        Ok(Spilled { file: self.sections.clone(), off, len: data.len() as u64 })
    }

    /// A buffer written out as it grows, in a file of its own.
    pub fn stream(self: &Arc<Self>) -> io::Result<SpillStream> {
        let file = Arc::new(temp_file(&self.dir, &self.prefix)?);
        Ok(SpillStream { spill: self.clone(), file, len: 0, buf: Vec::new(), crc: crc32fast::Hasher::new(), failed: None })
    }
}

/// A new file in `dir`, already removed from it.
fn temp_file(dir: &Path, prefix: &str) -> io::Result<File> {
    let path = dir.join(name(prefix));
    let f = OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
    // Gone already only if another opening of the database removed it.
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(f),
    }
}

/// A section that grows with every document a build adds, written out a
/// piece at a time ([`Spill::stream`]); [`map`](Self::map) reads it back
/// whole. A failed write is kept and returned by [`map`](Self::map) and
/// [`finish`](Self::finish), so that adding a document stays infallible.
pub struct SpillStream {
    spill: Arc<Spill>,
    file: Arc<File>,
    /// Bytes written to the file, and those buffered after them.
    len: u64,
    buf: Vec<u8>,
    crc: crc32fast::Hasher,
    failed: Option<io::Error>,
}

impl SpillStream {
    /// The stream's length, buffered bytes included.
    pub fn len(&self) -> usize {
        self.len as usize + self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn extend_from_slice(&mut self, data: &[u8]) {
        self.crc.update(data);
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            self.drain();
        }
    }

    /// Appends `n` zero bytes.
    pub fn zeros(&mut self, n: usize) {
        const Z: [u8; 256] = [0; 256];
        let mut left = n;
        while left > 0 {
            let k = left.min(Z.len());
            self.extend_from_slice(&Z[..k]);
            left -= k;
        }
    }

    fn drain(&mut self) {
        if self.buf.is_empty() || self.failed.is_some() {
            self.buf.clear();
            return;
        }
        match self.spill.write(&self.file, &self.buf, self.len) {
            Ok(()) => self.len += self.buf.len() as u64,
            Err(e) => self.failed = Some(e),
        }
        self.buf.clear();
    }

    fn check(&mut self) -> io::Result<()> {
        self.drain();
        self.buf = Vec::new();
        match self.failed.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The stream's bytes so far, mapped from its file (page-aligned);
    /// `None` when there are none.
    pub fn map(&mut self) -> io::Result<Option<memmap2::Mmap>> {
        self.check()?;
        if self.len == 0 {
            return Ok(None);
        }
        // SAFETY: the file has no name, so only this stream changes it,
        // and it is not written while the mapping is read.
        let map = unsafe { memmap2::MmapOptions::new().len(self.len as usize).map(&*self.file)? };
        Ok(Some(map))
    }

    /// The stream's bytes as a section, and their crc32.
    pub fn finish(mut self) -> io::Result<(Spilled, u32)> {
        self.check()?;
        Ok((Spilled { file: self.file.clone(), off: 0, len: self.len }, self.crc.clone().finalize()))
    }
}
