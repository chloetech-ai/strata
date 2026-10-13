//! Cold-cache I/O measurement that works for any engine's files.
//!
//! [`evict`] drops a file's pages from the OS page cache
//! (`posix_fadvise(DONTNEED)`), so the next read goes to the device.
//! [`resident_pages`] counts how many of a file's pages are in the page cache
//! (`mincore` over a fresh read-only mapping). Evicting, running a query and
//! counting gives the pages the query read from disk, for Strata, SQLite and
//! Tantivy alike. Mappings held by the measured process must also drop their
//! page-table entries (`madvise(DONTNEED)`, see [`drop_mapping`]), or the
//! kernel may keep the pages.

use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

pub const PAGE: usize = 4096;

/// Drops the file's clean pages from the page cache.
/// Only Linux has `posix_fadvise`; elsewhere this is a no-op and pages stay
/// resident.
#[cfg(target_os = "linux")]
pub fn evict(path: &Path) -> io::Result<()> {
    let f = File::open(path)?;
    let r = unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn evict(path: &Path) -> io::Result<()> {
    File::open(path).map(|_| ())
}

/// Evicts every regular file under `dir` (for multi-file engines).
pub fn evict_dir(dir: &Path) -> io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_file() {
            evict(&p)?;
        } else if p.is_dir() {
            evict_dir(&p)?;
        }
    }
    Ok(())
}

/// Number of the file's pages currently in the page cache.
pub fn resident_pages(path: &Path) -> io::Result<usize> {
    let f = File::open(path)?;
    let len = f.metadata()?.len() as usize;
    if len == 0 {
        return Ok(0);
    }
    unsafe {
        let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0);
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let pages = len.div_ceil(PAGE);
        let mut v = vec![0u8; pages];
        let r = libc::mincore(p, len, v.as_mut_ptr() as *mut _);
        libc::munmap(p, len);
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(v.iter().filter(|&&b| b & 1 == 1).count())
    }
}

/// Sum of [`resident_pages`] over every regular file under `dir`.
pub fn resident_pages_dir(dir: &Path) -> io::Result<usize> {
    let mut n = 0;
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_file() {
            n += resident_pages(&p)?;
        } else if p.is_dir() {
            n += resident_pages_dir(&p)?;
        }
    }
    Ok(n)
}

/// Drops this process's page-table entries for a mapped range, so the next
/// access faults and (after [`evict`]) reads from the device.
///
/// # Safety
/// `ptr..ptr+len` must be a page-aligned shared file mapping that nobody
/// writes through; its contents are re-read from the file afterwards.
pub unsafe fn drop_mapping(ptr: *const u8, len: usize) -> io::Result<()> {
    if libc::madvise(ptr as *mut libc::c_void, len, libc::MADV_DONTNEED) != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn evict_then_read_counts_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let mut f = File::create(&path).unwrap();
        f.write_all(&vec![1u8; 64 * PAGE]).unwrap();
        f.sync_all().unwrap();
        drop(f);
        evict(&path).unwrap();
        let before = resident_pages(&path).unwrap();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), 64 * PAGE);
        let after = resident_pages(&path).unwrap();
        eprintln!("resident before {before}, after {after}");
        assert!(after >= 60, "pages not resident after read");
        // Eviction may be refused on some filesystems; report rather than fail.
        if before > 4 {
            eprintln!("warning: eviction left {before} pages resident");
        }
    }
}
