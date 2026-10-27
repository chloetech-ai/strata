//! The directory layout: a database as a directory of files instead of one
//! file, so that segments can be built on one machine and shipped to
//! another, stored on object storage, or copied with ordinary tools.
//!
//! ```text
//! <dir>/LOCK                          held with flock by the open database
//! <dir>/CURRENT                       {db_id, txn, manifest, crc} as JSON
//! <dir>/MANIFEST-<txn>                the manifest CURRENT names (JSON)
//! <dir>/seg-<id>.strata               one segment image
//! <dir>/del-<id>-<txn>.bits           a segment's delete set
//! <dir>/upd-<id>-<txn>.bin            a segment's in-place updates
//! <dir>/wal                           the write-ahead log
//! ```
//!
//! A commit writes its new segment and delete-set files and its manifest,
//! syncs them, then replaces `CURRENT` with an atomic rename and syncs the
//! directory. Files the new version no longer names are unlinked after the
//! commit: snapshots keep reading them through their mappings, which outlive
//! the names. Files a crash left unnamed are removed when the database opens.
//! Segment images are the same bytes as in the single-file layout.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{corrupt, Result};
use crate::postings::bytes::OwnedBytes;
use crate::storage::manifest::Manifest;

pub const CURRENT: &str = "CURRENT";
pub const LOCK: &str = "LOCK";
pub const WAL: &str = "wal";

/// The contents of `CURRENT`: which manifest is the database.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Current {
    pub db_id: u64,
    pub txn: u64,
    pub manifest: String,
    /// crc32 of the manifest file.
    pub crc: u32,
}

pub fn manifest_name(txn: u64) -> String {
    format!("MANIFEST-{txn:020}")
}

pub fn segment_name(id: u64) -> String {
    format!("seg-{id:010}.strata")
}

pub fn deletes_name(id: u64, txn: u64) -> String {
    format!("del-{id:010}-{txn:020}.bits")
}

pub fn updates_name(id: u64, txn: u64) -> String {
    format!("upd-{id:010}-{txn:020}.bin")
}

/// Writes a new file (it must not exist) and syncs it when `sync`.
pub fn write_file(dir: &Path, name: &str, bytes: &[u8], sync: bool) -> Result<()> {
    let mut f = OpenOptions::new().write(true).create_new(true).open(dir.join(name))?;
    f.write_all(bytes)?;
    if sync {
        f.sync_data()?;
    }
    Ok(())
}

pub fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Replaces `CURRENT` atomically (write a temporary file, rename it over).
pub fn write_current(dir: &Path, cur: &Current, sync: bool) -> Result<()> {
    let tmp = dir.join("CURRENT.tmp");
    let _ = std::fs::remove_file(&tmp);
    write_file(dir, "CURRENT.tmp", &serde_json::to_vec(cur).expect("current serialises"), sync)?;
    std::fs::rename(&tmp, dir.join(CURRENT))?;
    if sync {
        sync_dir(dir)?;
    }
    Ok(())
}

/// Reads `CURRENT` and the manifest it names.
pub fn read_current(dir: &Path) -> Result<(Current, Manifest)> {
    let cur: Current = serde_json::from_slice(&std::fs::read(dir.join(CURRENT))?).map_err(|e| corrupt(format!("CURRENT: {e}")))?;
    let bytes = std::fs::read(dir.join(&cur.manifest))?;
    if crc32fast::hash(&bytes) != cur.crc {
        return Err(corrupt(format!("{} fails its checksum", cur.manifest)));
    }
    Ok((cur.clone(), Manifest::decode(&bytes)?))
}

/// Maps a file of the directory read-only.
pub fn map(dir: &Path, name: &str, read_ahead: bool) -> Result<OwnedBytes> {
    let f = File::open(dir.join(name))?;
    crate::storage::pager::map_file(&f, read_ahead)
}

/// The files a version needs: its manifest, segments and delete sets.
pub fn referenced(cur: &Current, manifest: &Manifest) -> HashSet<String> {
    let mut keep: HashSet<String> = [CURRENT.to_string(), LOCK.to_string(), WAL.to_string(), cur.manifest.clone()].into();
    for s in &manifest.segments {
        keep.extend(s.file.iter().cloned());
        keep.extend(s.deletes_file.iter().cloned());
        keep.extend(s.updates_file.iter().cloned());
    }
    keep
}

/// Files of the layout's kinds that `keep` does not name, with their sizes.
pub fn orphans(dir: &Path, keep: &HashSet<String>) -> Result<Vec<(PathBuf, u64)>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let ours = ["seg-", "del-", "upd-", "MANIFEST-"].iter().any(|p| name.starts_with(p)) || name == "CURRENT.tmp";
        if ours && !keep.contains(&name) {
            out.push((e.path(), e.metadata().map_or(0, |m| m.len())));
        }
    }
    Ok(out)
}

/// Total size of the directory's files (the WAL excluded).
pub fn size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|it| it.filter_map(|e| e.ok()).filter(|e| e.file_name() != WAL).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
        .unwrap_or(0)
}
