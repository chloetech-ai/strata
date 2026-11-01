//! Every change the storage engine makes to its files, in one place, so
//! that tests can see what a power loss or a failing disk does to a
//! database. Built normally, each function is the plain system call.
//!
//! With the `fault-injection` feature a thread can [`arm`] a recorder. For
//! each file the thread changes through here it keeps the content last
//! made durable (by a sync) and the writes and truncations since. Then:
//!
//! * [`fail_at`] makes the `n`-th call from arming fail (a write may land
//!   partly first), and every later one when the fault is sticky, as a
//!   full disk or a dead one does;
//! * a sync that fails loses the file's unsynced writes, each 512-byte
//!   sector kept or lost at random, as Linux does (it marks the pages
//!   clean whether they reached the disk or not);
//! * [`crash`] stops recording and returns what a power loss could leave
//!   of each file ([`Crash::image`]): the durable content plus each later
//!   change kept or lost at random, sector by sector. A fault that
//!   [`Fault::crash`]es freezes that state at the failing call.
//!
//! Directory entries (creating, renaming and removing a file) count as
//! durable at once, so the directory layout's crash images do not question
//! them; its calls can still be failed. A file whose last name is removed
//! or renamed over through here is forgotten, since ext4 gives its inode to
//! the next file created. Reads are not intercepted: the engine reads the
//! real file, which holds every write.

use std::fs::File;
use std::io;
use std::path::Path;

/// Writes all of `buf` at `off`.
pub fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    imp::write_at(f, buf, off)
}

/// Writes all of `buf` at `off` in a scratch file: one whose name is
/// removed when it is created and that holds nothing a reopen reads (a
/// merge's spill, [`crate::segment::spill`]). The call counts, and fails,
/// as any other; the recorder does not keep the file, whose inode the
/// next file created may take once it is closed.
pub fn scratch_write_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    imp::call()?;
    std::os::unix::fs::FileExt::write_all_at(f, buf, off)
}

/// Sets the file's length.
pub fn set_len(f: &File, len: u64) -> io::Result<()> {
    imp::set_len(f, len)
}

/// `fdatasync`.
pub fn sync(f: &File) -> io::Result<()> {
    imp::sync(f, false)
}

/// `fsync`.
pub fn sync_all(f: &File) -> io::Result<()> {
    imp::sync(f, true)
}

/// `fsync` of a directory, which makes the entries in it durable.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    imp::call()?;
    File::open(dir)?.sync_all()
}

pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    imp::call()?;
    let replaced = imp::last_name(to).filter(|&k| imp::last_name(from) != Some(k));
    std::fs::rename(from, to)?;
    imp::forget(replaced);
    Ok(())
}

pub fn remove_file(p: &Path) -> io::Result<()> {
    imp::call()?;
    let removed = imp::last_name(p);
    std::fs::remove_file(p)?;
    imp::forget(removed);
    Ok(())
}

#[cfg(feature = "fault-injection")]
pub use imp::{arm, calls, crash, fail_at, Crash, Fault};

#[cfg(not(feature = "fault-injection"))]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::FileExt;
    use std::path::Path;

    #[inline]
    pub fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
        f.write_all_at(buf, off)
    }

    #[inline]
    pub fn set_len(f: &File, len: u64) -> io::Result<()> {
        f.set_len(len)
    }

    #[inline]
    pub fn sync(f: &File, all: bool) -> io::Result<()> {
        if all {
            f.sync_all()
        } else {
            f.sync_data()
        }
    }

    #[inline]
    pub fn call() -> io::Result<()> {
        Ok(())
    }

    #[inline]
    pub fn last_name(_: &Path) -> Option<()> {
        None
    }

    #[inline]
    pub fn forget(_: Option<()>) {}
}

#[cfg(feature = "fault-injection")]
mod imp {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::{FileExt, MetadataExt};
    use std::path::Path;

    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    const SECTOR: u64 = 512;

    /// A file as the recorder knows it.
    #[derive(Clone, Default)]
    struct Tracked {
        /// The content a power loss keeps for certain.
        durable: Vec<u8>,
        /// Changes since, in order.
        pending: Vec<Change>,
    }

    #[derive(Clone)]
    enum Change {
        Write { off: u64, data: Vec<u8> },
        SetLen(u64),
    }

    /// When and how a call fails.
    #[derive(Clone, Copy, Debug)]
    pub struct Fault {
        /// The call that fails, counted from 1 after [`arm`].
        pub at: u64,
        /// Its error (`libc::ENOSPC`, `libc::EIO`, ...).
        pub errno: i32,
        /// Every later call fails too.
        pub sticky: bool,
        /// The power is lost at that call: [`crash`] reports the files as
        /// they were just before it.
        pub crash: bool,
    }

    struct Recorder {
        files: HashMap<(u64, u64), Tracked>,
        calls: u64,
        fault: Option<Fault>,
        frozen: Option<HashMap<(u64, u64), Tracked>>,
        rng: StdRng,
    }

    thread_local! {
        static REC: RefCell<Option<Recorder>> = const { RefCell::new(None) };
    }

    /// Starts recording this thread's file changes (anew), with `seed`
    /// for every random choice.
    pub fn arm(seed: u64) {
        REC.with(|r| *r.borrow_mut() = Some(Recorder { files: HashMap::new(), calls: 0, fault: None, frozen: None, rng: StdRng::seed_from_u64(seed) }));
    }

    /// Fails calls from now on as `fault` says (replacing an earlier one).
    pub fn fail_at(fault: Fault) {
        REC.with(|r| r.borrow_mut().as_mut().expect("io::arm first").fault = Some(fault));
    }

    /// Calls made since [`arm`].
    pub fn calls() -> u64 {
        REC.with(|r| r.borrow().as_ref().map_or(0, |r| r.calls))
    }

    /// Stops recording; what a power loss now (or at a crashing fault)
    /// could leave of each file.
    pub fn crash() -> Crash {
        let rec = REC.with(|r| r.borrow_mut().take()).expect("io::arm first");
        Crash { files: rec.frozen.unwrap_or(rec.files), rng: rec.rng }
    }

    /// The files a power loss hit.
    pub struct Crash {
        files: HashMap<(u64, u64), Tracked>,
        rng: StdRng,
    }

    impl Crash {
        /// One content the file at `path` could have after the power loss
        /// (each call draws again). A file the recorder never saw changed
        /// is read as it is.
        pub fn image(&mut self, path: &Path) -> io::Result<Vec<u8>> {
            match self.files.get(&key(&std::fs::metadata(path)?)) {
                Some(t) => Ok(draw(t, &mut self.rng)),
                None => std::fs::read(path),
            }
        }

        /// Writes an image of `from` to `to`.
        pub fn copy(&mut self, from: &Path, to: &Path) -> io::Result<()> {
            let img = self.image(from)?;
            std::fs::write(to, img)
        }
    }

    /// The durable content of `t` with each later change kept or lost at
    /// random, sector by sector.
    fn draw(t: &Tracked, rng: &mut StdRng) -> Vec<u8> {
        apply(t, || rng.gen_bool(0.5))
    }

    /// The durable content of `t` with the parts of later changes `keep`
    /// says reached the disk: each 512-byte sector of a write, a write's
    /// new length (which can arrive without its data), each truncation.
    fn apply(t: &Tracked, mut keep: impl FnMut() -> bool) -> Vec<u8> {
        let mut img = t.durable.clone();
        for c in &t.pending {
            match c {
                Change::Write { off, data } => {
                    let end = off + data.len() as u64;
                    let mut s = *off;
                    while s < end {
                        let e = ((s / SECTOR + 1) * SECTOR).min(end);
                        if keep() {
                            if img.len() < e as usize {
                                img.resize(e as usize, 0);
                            }
                            img[s as usize..e as usize].copy_from_slice(&data[(s - off) as usize..(e - off) as usize]);
                        }
                        s = e;
                    }
                    if img.len() < end as usize && keep() {
                        img.resize(end as usize, 0);
                    }
                }
                Change::SetLen(len) => {
                    if keep() {
                        img.resize(*len as usize, 0);
                    }
                }
            }
        }
        img
    }

    fn key(m: &std::fs::Metadata) -> (u64, u64) {
        (m.dev(), m.ino())
    }

    /// Counts a call and fails it if the fault says so; `Ok(None)` when
    /// not recording.
    fn with<R>(f: impl FnOnce(&mut Recorder) -> io::Result<R>) -> io::Result<Option<R>> {
        REC.with(|r| match r.borrow_mut().as_mut() {
            Some(rec) => f(rec).map(Some),
            None => Ok(None),
        })
    }

    impl Recorder {
        fn count(&mut self) -> io::Result<()> {
            self.calls += 1;
            let Some(f) = self.fault else { return Ok(()) };
            if self.calls == f.at && f.crash && self.frozen.is_none() {
                self.frozen = Some(self.files.clone());
            }
            if self.calls == f.at || (f.sticky && self.calls > f.at) {
                return Err(io::Error::from_raw_os_error(f.errno));
            }
            Ok(())
        }

        fn tracked(&mut self, f: &File) -> io::Result<&mut Tracked> {
            let k = key(&f.metadata()?);
            if !self.files.contains_key(&k) {
                let durable = read_all(f)?;
                self.files.insert(k, Tracked { durable, pending: Vec::new() });
            }
            Ok(self.files.get_mut(&k).unwrap())
        }
    }

    /// The file's content when the recorder first sees it (none for a
    /// file just created, which may be open for writing only).
    fn read_all(f: &File) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; f.metadata()?.len() as usize];
        if !buf.is_empty() {
            f.read_exact_at(&mut buf, 0)?;
        }
        Ok(buf)
    }

    pub fn call() -> io::Result<()> {
        with(|r| r.count()).map(|_| ())
    }

    /// The recorder's key for the file at `p` when that is its only name,
    /// so that removing or replacing the name lets the inode go; `None`
    /// when not recording.
    pub fn last_name(p: &Path) -> Option<(u64, u64)> {
        REC.with(|r| r.borrow().is_some()).then_some(())?;
        let m = std::fs::symlink_metadata(p).ok()?;
        (m.is_file() && m.nlink() == 1).then(|| key(&m))
    }

    /// Forgets a file whose last name is gone. Linux's ext4 gives its inode
    /// to the next file created (once the file is closed), which is a new
    /// file, not this one.
    pub fn forget(k: Option<(u64, u64)>) {
        let Some(k) = k else { return };
        REC.with(|r| {
            if let Some(rec) = r.borrow_mut().as_mut() {
                rec.files.remove(&k);
            }
        });
    }

    pub fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
        let done = with(|r| {
            r.tracked(f)?;
            if let Err(e) = r.count() {
                // Part of the write may land before the error.
                let n = r.rng.gen_range(0..=buf.len());
                f.write_all_at(&buf[..n], off)?;
                r.tracked(f)?.pending.push(Change::Write { off, data: buf[..n].to_vec() });
                return Err(e);
            }
            f.write_all_at(buf, off)?;
            r.tracked(f)?.pending.push(Change::Write { off, data: buf.to_vec() });
            Ok(())
        })?;
        match done {
            Some(()) => Ok(()),
            None => f.write_all_at(buf, off),
        }
    }

    pub fn set_len(f: &File, len: u64) -> io::Result<()> {
        let done = with(|r| {
            r.tracked(f)?;
            r.count()?;
            f.set_len(len)?;
            r.tracked(f)?.pending.push(Change::SetLen(len));
            Ok(())
        })?;
        match done {
            Some(()) => Ok(()),
            None => f.set_len(len),
        }
    }

    pub fn sync(f: &File, all: bool) -> io::Result<()> {
        let done = with(|r| {
            r.tracked(f)?;
            if let Err(e) = r.count() {
                // The unsynced writes are lost or kept at random, and count
                // as written: a later sync does not retry them.
                let Recorder { files, rng, .. } = r;
                let t = files.get_mut(&key(&f.metadata()?)).unwrap();
                let img = draw(t, rng);
                // Written over, then cut, so that the file never gets
                // shorter than the mapped version while it is rewritten.
                f.write_all_at(&img, 0)?;
                f.set_len(img.len() as u64)?;
                *t = Tracked { durable: img, pending: Vec::new() };
                return Err(e);
            }
            if all {
                f.sync_all()?
            } else {
                f.sync_data()?
            }
            // What the file holds now, without reading it (the engine
            // opens some files write-only).
            let t = r.tracked(f)?;
            t.durable = apply(t, || true);
            t.pending.clear();
            Ok(())
        })?;
        match done {
            Some(()) => Ok(()),
            None if all => f.sync_all(),
            None => f.sync_data(),
        }
    }
}
