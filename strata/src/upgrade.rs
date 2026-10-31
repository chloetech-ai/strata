//! Copying a database written before field ids into a new one.
//!
//! A database Strata creates names each field by an id
//! ([`crate::schema::Field::id`], manifest feature
//! [`crate::storage::manifest::FIELD_IDS`]). One written by a version from
//! before field ids names fields by position and name, and opening it
//! fails; [`upgrade`] copies it into a database of this version in its
//! place, and keeps the old files beside it.
//!
//! The copy is a new database: the same tables with the same fields, rows
//! (with their vectors, and values written in place folded in) and rank
//! profiles, in the same layout, with a new database id. A database that
//! numbers its writes (replication) or keeps stream offsets (`_sources`)
//! is refused: its copy could not go on from its numbers, so its replicas
//! and streams start again from a database created anew.

use std::path::{Path, PathBuf};

use crate::db::Db;
use crate::error::{Error, Result};
use crate::storage::manifest::{self, SOURCES};
use crate::storage::{DbOptions, Layout};

/// What [`upgrade`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upgraded {
    /// Where the old database is kept: `<path>.before-field-ids`.
    pub old: PathBuf,
    /// The tables copied, and their rows in all.
    pub tables: usize,
    pub rows: u64,
}

/// Whether `path` holds a database written before field ids, which
/// [`upgrade`] copies. Reads its newest version's manifest without opening
/// it.
pub fn needs_upgrade(path: &Path) -> Result<bool> {
    Ok(manifest::read(path)?.is_legacy())
}

/// Where [`upgrade`] keeps the old database: `<path>.before-field-ids`.
pub fn old_path(path: &Path) -> PathBuf {
    suffixed(path, ".before-field-ids")
}

/// Where [`upgrade`] writes the copy before it takes `path`'s place.
fn new_path(path: &Path) -> PathBuf {
    suffixed(path, ".upgrading")
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Copies the database at `path`, written before field ids, into a new
/// database of this version, which takes its place; the old one is moved
/// to [`old_path`]. Returns `None`, doing nothing, when `path` needs no
/// upgrade ([`needs_upgrade`]).
///
/// The copy is written beside it (`<path>.upgrading`, removed first if an
/// earlier upgrade left one), flushed and closed before anything is
/// renamed; then the old database is moved aside and the copy moved in. A
/// crash between those two renames leaves no `path`: the next call
/// finishes the move. The database must not be open meanwhile.
///
/// Semantic-ID fields need their codebooks in the default models
/// directory ([`crate::embed::models_dir`]), as [`Db::create`] does.
pub fn upgrade(path: &Path) -> Result<Option<Upgraded>> {
    let (old, new) = (old_path(path), new_path(path));
    if !exists(path) && exists(&new) && exists(&old) {
        // A crash came between the renames: the copy is complete.
        move_database(&new, path)?;
        return Ok(Some(Upgraded { old, tables: 0, rows: 0 }));
    }
    if !needs_upgrade(path)? {
        return Ok(None);
    }
    if exists(&old) {
        return Err(Error::Schema(format!("{} exists: an earlier upgrade kept a database there; move it away to upgrade {}", old.display(), path.display())));
    }
    remove_database(&new)?;
    let layout = if path.is_dir() { Layout::Directory } else { Layout::SingleFile };
    let (tables, rows) = {
        let from = Db::open(path, DbOptions { open_old: true, ..DbOptions::default() })?;
        let copied = copy(&from, &new, layout);
        if copied.is_err() {
            let _ = remove_database(&new);
        }
        copied?
    };
    move_database(path, &old)?;
    move_database(&new, path)?;
    Ok(Some(Upgraded { old, tables, rows }))
}

/// Writes a copy of `from` at `to`: its tables, rows and rank profiles.
fn copy(from: &Db, to: &Path, layout: Layout) -> Result<(usize, u64)> {
    let storage = from.storage();
    if storage.seq_no().is_some() {
        return Err(Error::Schema(format!(
            "{} numbers its writes (replication), which a copy cannot go on from: create a new primary and copy the rows with load_from",
            storage.path().display()
        )));
    }
    let tables = storage.tables();
    if tables.iter().any(|t| t.table.eq_ignore_ascii_case(SOURCES)) {
        return Err(Error::Schema(format!("{} keeps stream offsets ({SOURCES}), which a copy cannot carry: create a new database and start its streams again", storage.path().display())));
    }
    let db = Db::create(to, tables[0].with_new_ids()?, DbOptions { layout, ..DbOptions::default() })?;
    for t in &tables[1..] {
        db.create_table(t.with_new_ids()?)?;
    }
    let mut rows = 0;
    for t in &tables {
        rows += db.load_table_from(&t.table, from, &t.table)?;
        for p in storage.rank_profiles(&t.table)? {
            db.storage().set_rank_profile(&t.table, p, false)?;
        }
    }
    db.storage().flush()?;
    Ok((tables.len(), rows))
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// The files of the single-file database at `path` beside it: its log,
/// sealed logs, damaged copies and merge record (`<name>-wal*`,
/// `<name>-merging`).
fn sidecars(path: &Path) -> Result<Vec<(PathBuf, String)>> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let Some(f) = e.file_name().to_str().map(String::from) else { continue };
        if let Some(rest) = f.strip_prefix(name) {
            if rest.starts_with("-wal") || rest == "-merging" {
                out.push((e.path(), rest.to_string()));
            }
        }
    }
    Ok(out)
}

/// Renames the database at `from` (a directory, or a file and its
/// [`sidecars`]) to `to`.
fn move_database(from: &Path, to: &Path) -> Result<()> {
    let side = if from.is_dir() { Vec::new() } else { sidecars(from)? };
    for (p, rest) in side {
        std::fs::rename(&p, suffixed(to, &rest))?;
    }
    std::fs::rename(from, to)?;
    if let Some(d) = to.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Ok(f) = std::fs::File::open(d) {
            let _ = f.sync_all();
        }
    }
    Ok(())
}

/// Removes the database at `path`, if there is one.
fn remove_database(path: &Path) -> Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)?;
        return Ok(());
    }
    for (p, _) in sidecars(path)? {
        std::fs::remove_file(p)?;
    }
    if exists(path) {
        std::fs::remove_file(path)?;
    }
    Ok(())
}
